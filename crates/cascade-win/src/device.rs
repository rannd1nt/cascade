//! `DeviceProfile` collection for `cascade doctor` and the S5 `assess()`
//! (design-m4 §5.1).
//!
//! Every collector is best-effort: an unreadable value becomes `None` (or an
//! absent list entry), never an error — `doctor` must print something on
//! every machine, including one whose driver hides half these keys. This
//! module is CLI-only; the daemon never calls it.

use std::mem::size_of;

use cascade_core::device::{
    subtype_label, CpuInfo, DecodeCapability, DecodeInput, DeviceProfile, GpuAdapter,
    HardwareDecoder, MonitorProfile, RamInfo,
};
use windows::core::{Interface, GUID};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11VideoDevice, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
    DXGI_COLOR_SPACE_TYPE,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput6, DXGI_ADAPTER_FLAG_SOFTWARE,
};
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFTransform, MFShutdown, MFStartup, MFTEnumEx, MFT_FRIENDLY_NAME_Attribute,
    MFSTARTUP_FULL, MFT_CATEGORY_VIDEO_DECODER, MFT_ENUM_FLAG_HARDWARE,
    MFT_ENUM_FLAG_SORTANDFILTER, MF_MT_MPEG2_LEVEL, MF_MT_MPEG2_PROFILE, MF_MT_SUBTYPE,
    MF_TRANSFORM_ASYNC_UNLOCK, MF_VERSION,
};
use windows::Win32::System::Com::{
    CoInitializeEx, CoTaskMemFree, CoUninitialize, COINIT_MULTITHREADED,
};
use windows_sys::Win32::Graphics::Gdi::{
    EnumDisplaySettingsW, DEVMODEW, DM_DISPLAYFREQUENCY, ENUM_CURRENT_SETTINGS, HMONITOR,
};
use windows_sys::Win32::System::Registry::HKEY_LOCAL_MACHINE;
use windows_sys::Win32::System::SystemInformation::{
    GetLogicalProcessorInformationEx, GlobalMemoryStatusEx, RelationProcessorCore, MEMORYSTATUSEX,
};
use windows_sys::Win32::System::Threading::{GetActiveProcessorCount, ALL_PROCESSOR_GROUPS};
use windows_sys::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};

use crate::monitor;
use crate::registry;
use crate::wide;

/// Display adapter class key (`HKLM\SYSTEM\...\Control\Class\{4d36e968-...}`),
/// opened read-only (design-m4 §5.1). `DriverVersion` exists there on this
/// machine (measured: `30.0.101.1960`, 2026-11-08); when it does not, the
/// version stays `None` instead of being guessed.
const DISPLAY_CLASS_KEY: &str =
    r"SYSTEM\CurrentControlSet\Control\Class\{4d36e968-e325-11ce-bfc1-08002be10318}";
const CPU_NAME_KEY: &str = r"HARDWARE\DESCRIPTION\System\CentralProcessor\0";

/// Collect the whole profile. Best-effort per section — see module docs.
pub fn collect() -> DeviceProfile {
    DeviceProfile {
        monitors: monitor_profiles(),
        gpus: gpus(),
        cpu: cpu(),
        ram: ram(),
        decode: decode(),
    }
}

// --- monitors -----------------------------------------------------------------

/// DXGI per-output extras, keyed by device name (`\\.\DISPLAY1`).
struct OutputExtras {
    hdr: bool,
    bits_per_color: u32,
    color_space: String,
}

fn monitor_profiles() -> Vec<MonitorProfile> {
    let extras = output_extras();
    monitor::enumerate()
        .into_iter()
        .map(|m| {
            let refresh_hz = refresh_hz(&m.id);
            let dpi = monitor_dpi(m.handle);
            let extra = extras.get(&m.id);
            MonitorProfile {
                id: m.id,
                rect: m.rect,
                work: m.work,
                primary: m.primary,
                refresh_hz,
                dpi,
                hdr: extra.map(|e| e.hdr),
                bits_per_color: extra.map(|e| e.bits_per_color),
                color_space: extra.map(|e| e.color_space.clone()),
            }
        })
        .collect()
}

/// Current refresh rate from `EnumDisplaySettingsW`, `None` when the mode
/// does not report one (`0`/`1` mean "hardware default", not a rate).
fn refresh_hz(device: &str) -> Option<u32> {
    let name = wide::to_wide(device);
    let mut mode: DEVMODEW = unsafe { std::mem::zeroed() };
    mode.dmSize = size_of::<DEVMODEW>() as u16;
    // SAFETY: `name` is a live NUL-terminated buffer and `mode` is a sized
    // local the API fills; failure just leaves the struct untouched.
    let ok = unsafe { EnumDisplaySettingsW(name.as_ptr(), ENUM_CURRENT_SETTINGS, &mut mode) };
    if ok == 0 || mode.dmFields & DM_DISPLAYFREQUENCY == 0 {
        return None;
    }
    match mode.dmDisplayFrequency {
        0 | 1 => None,
        hz => Some(hz),
    }
}

/// Effective DPI of one monitor. Requires per-monitor awareness for exact
/// per-monitor values; an unaware `doctor` still reports what the API returns
/// (the awareness line above is printed honestly either way).
fn monitor_dpi(handle: isize) -> Option<u32> {
    let mut x = 0u32;
    let mut y = 0u32;
    // SAFETY: out-params are live locals; the handle comes from the monitor
    // enumeration. `GetDpiForMonitor` is in shcore via the HiDpi feature.
    let hr = unsafe { GetDpiForMonitor(handle as HMONITOR, MDT_EFFECTIVE_DPI, &mut x, &mut y) };
    if hr == 0 {
        Some(x)
    } else {
        None
    }
}

/// All DXGI outputs' HDR/bits/color-space extras, keyed by device name.
fn output_extras() -> std::collections::BTreeMap<String, OutputExtras> {
    let mut map = std::collections::BTreeMap::new();
    // SAFETY: flat COM factory creation; failure simply yields no extras.
    let Ok(factory) = (unsafe { CreateDXGIFactory1::<IDXGIFactory1>() }) else {
        return map;
    };
    let mut adapter_index = 0u32;
    // SAFETY: enumeration calls return Err past the last adapter/output and
    // hand back owning interface pointers on success.
    while let Ok(adapter) = unsafe { factory.EnumAdapters1(adapter_index) } {
        let mut output_index = 0u32;
        while let Ok(output) = unsafe { adapter.EnumOutputs(output_index) } {
            if let Ok(output6) = output.cast::<IDXGIOutput6>() {
                if let Ok(desc) = unsafe { output6.GetDesc1() } {
                    map.insert(
                        wide::from_wide(&desc.DeviceName),
                        OutputExtras {
                            hdr: desc.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020,
                            bits_per_color: desc.BitsPerColor,
                            color_space: color_space_label(desc.ColorSpace).to_string(),
                        },
                    );
                }
            }
            output_index += 1;
        }
        adapter_index += 1;
    }
    map
}

/// Label for a `DXGI_COLOR_SPACE_TYPE` (pure; unit-tested). Comparisons
/// instead of a `match`: the type is a newtype over `i32`, so const patterns
/// are not structural.
fn color_space_label(color_space: DXGI_COLOR_SPACE_TYPE) -> &'static str {
    if color_space == DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709 {
        "SDR (BT.709)"
    } else if color_space == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020 {
        "HDR10 (BT.2020 PQ)"
    } else {
        "unknown"
    }
}

// --- GPU ----------------------------------------------------------------------

/// Hardware DXGI adapters (software/GPU-preference mirrors are skipped) with
/// their driver version matched from the display class key by `DriverDesc`.
fn gpus() -> Vec<GpuAdapter> {
    let drivers = display_drivers();
    let mut out = Vec::new();
    // SAFETY: same factory call as in `output_extras`.
    let Ok(factory) = (unsafe { CreateDXGIFactory1::<IDXGIFactory1>() }) else {
        return out;
    };
    let mut index = 0u32;
    // SAFETY: Err past the last adapter; success owns the returned pointer.
    while let Ok(adapter) = unsafe { factory.EnumAdapters1(index) } {
        if let Ok(desc) = unsafe { adapter.GetDesc1() } {
            // `Flags` is a plain u32; the const is a newtype over i32.
            if desc.Flags & (DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32) != 0 {
                index += 1;
                continue;
            }
            let name = wide::from_wide(&desc.Description);
            out.push(GpuAdapter {
                driver_version: match_driver(&drivers, &name),
                vendor_id: desc.VendorId,
                dedicated_mb: desc.DedicatedVideoMemory as u64 / (1024 * 1024),
                shared_mb: desc.SharedSystemMemory as u64 / (1024 * 1024),
                name,
            });
        }
        index += 1;
    }
    out
}

/// Match a GPU to its driver version: `DriverDesc` equals the DXGI adapter
/// name, case-insensitively (measured equal on the target machine).
fn match_driver(drivers: &[(String, String)], gpu_name: &str) -> Option<String> {
    drivers
        .iter()
        .find(|(desc, _)| desc.eq_ignore_ascii_case(gpu_name))
        .map(|(_, version)| version.clone())
}

/// `(DriverDesc, DriverVersion)` for every `0xxx` subkey of the display
/// class key that reports both. Read-only; a subkey the OS refuses to read
/// is skipped.
fn display_drivers() -> Vec<(String, String)> {
    registry::enum_subkeys(HKEY_LOCAL_MACHINE, DISPLAY_CLASS_KEY)
        .into_iter()
        .filter(|name| name.len() == 4 && name.bytes().all(|b| b.is_ascii_digit()))
        .filter_map(|name| {
            let sub = format!(r"{DISPLAY_CLASS_KEY}\{name}");
            let desc = registry::read_string(HKEY_LOCAL_MACHINE, &sub, "DriverDesc")?;
            let version = registry::read_string(HKEY_LOCAL_MACHINE, &sub, "DriverVersion")?;
            Some((desc, version))
        })
        .collect()
}

// --- CPU / RAM ----------------------------------------------------------------

/// CPU name from the registry; physical cores from
/// `GetLogicalProcessorInformationEx(RelationProcessorCore)` (one record per
/// core); threads from `GetActiveProcessorCount` across all groups.
fn cpu() -> Option<CpuInfo> {
    let cores = processor_cores()?;
    let threads = unsafe { GetActiveProcessorCount(ALL_PROCESSOR_GROUPS) };
    Some(CpuInfo {
        name: registry::read_string(HKEY_LOCAL_MACHINE, CPU_NAME_KEY, "ProcessorNameString"),
        cores,
        threads: if threads == 0 { cores } else { threads },
    })
}

fn processor_cores() -> Option<u32> {
    let mut len = 0u32;
    // SAFETY: size probe — null buffer is the documented first call; it is
    // expected to fail with ERROR_INSUFFICIENT_BUFFER while reporting `len`.
    let ok = unsafe {
        GetLogicalProcessorInformationEx(RelationProcessorCore, std::ptr::null_mut(), &mut len)
    };
    if ok != 0 || len == 0 {
        return None;
    }
    let mut buf = vec![0u8; len as usize];
    // SAFETY: `buf` is exactly `len` bytes, the size the API just reported.
    let ok = unsafe {
        GetLogicalProcessorInformationEx(
            RelationProcessorCore,
            buf.as_mut_ptr().cast::<windows_sys::Win32::System::SystemInformation::SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX>(),
            &mut len,
        )
    };
    if ok == 0 {
        return None;
    }
    // Walk the variable-size records by their (Relationship, Size) header.
    // Unaligned reads: the Vec<u8> carries no alignment guarantee.
    let mut cores = 0u32;
    let mut offset = 0usize;
    while offset + 8 <= buf.len() {
        let relationship = i32::from_le_bytes(buf[offset..offset + 4].try_into().ok()?);
        let size = u32::from_le_bytes(buf[offset + 4..offset + 8].try_into().ok()?) as usize;
        if size < 8 || offset + size > buf.len() {
            break;
        }
        if relationship == RelationProcessorCore {
            cores += 1;
        }
        offset += size;
    }
    if cores == 0 {
        None
    } else {
        Some(cores)
    }
}

/// Total/available RAM in MB from `GlobalMemoryStatusEx`.
fn ram() -> Option<RamInfo> {
    let mut status: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    status.dwLength = size_of::<MEMORYSTATUSEX>() as u32;
    // SAFETY: `status` is a correctly sized local, as the API requires.
    if unsafe { GlobalMemoryStatusEx(&mut status) } == 0 {
        return None;
    }
    Some(RamInfo {
        total_mb: status.ullTotalPhys / (1024 * 1024),
        available_mb: status.ullAvailPhys / (1024 * 1024),
    })
}

// --- decode capability --------------------------------------------------------

/// Enumerate hardware video-decoder MFTs and their advertised input types,
/// unioned with the GPU's D3D11 decoder profiles.
///
/// Returns `None` when COM/MF startup or `MFTEnumEx` fails (doctor prints
/// "unavailable"); `Some` with an empty `decoders` vector means MF ran fine
/// and the machine has no hardware decoder MFT. The D3D11 query runs first
/// and independently — it is the source that stays representative on
/// machines where vendors register no decoder MFT at all (see
/// `DecodeCapability`'s doc for the measured case).
fn decode() -> Option<DecodeCapability> {
    let gpu_codecs = gpu_decode_codecs();
    // `MFTEnumEx` blocks indefinitely without COM + MF started on the calling
    // thread (observed 2026-11-07 in the cascade-media probe) — same order.
    let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    // RPC_E_CHANGED_MODE: the thread already has another apartment. Then this
    // call did not initialize anything, so it must not be uninitialized.
    let com_owned = com.is_ok();
    // SAFETY: exact `MF_VERSION`, balancing the MFShutdown below.
    let mf = unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) };
    if mf.is_err() {
        if com_owned {
            // SAFETY: balances the successful CoInitializeEx above.
            unsafe { CoUninitialize() };
        }
        return None;
    }
    let mut capability = enumerate_decoders();
    if let Some(capability) = capability.as_mut() {
        capability.gpu_codecs = gpu_codecs;
    }
    // Shutdown order: MF first, then COM (design-m2 §3.3).
    let _ = unsafe { MFShutdown() };
    if com_owned {
        // SAFETY: balances the successful CoInitializeEx above.
        unsafe { CoUninitialize() };
    }
    capability
}

/// Codec families every hardware DXGI adapter reports through the documented
/// `ID3D11VideoDevice::GetVideoDecoderProfile` API — vendor-generic (Intel,
/// NVIDIA, AMD, APU alike), unlike the Media Foundation hardware-MFT flag,
/// which vendors may not populate for decoders they still accelerate (see
/// `DecodeCapability`'s doc). Deduplicated and sorted across adapters.
fn gpu_decode_codecs() -> Vec<String> {
    let mut codecs = std::collections::BTreeSet::new();
    // SAFETY: flat COM factory creation; failure simply yields no codecs.
    let Ok(factory) = (unsafe { CreateDXGIFactory1::<IDXGIFactory1>() }) else {
        return Vec::new();
    };
    let mut index = 0u32;
    // SAFETY: Err past the last adapter; success owns the returned pointer.
    while let Ok(adapter) = unsafe { factory.EnumAdapters1(index) } {
        // Same skip rule as `gpus()`: software adapters (WARP/mirrors) are
        // not hardware decoders — and a desc we cannot read is not one we
        // can trust either, so both are skipped instead of guessed.
        // SAFETY: `adapter` is live; the desc out-param is handled inside.
        let hardware = unsafe { adapter.GetDesc1() }
            .map(|desc| desc.Flags & (DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32) == 0)
            .unwrap_or(false);
        if hardware {
            codecs.extend(adapter_decode_codecs(&adapter));
        }
        index += 1;
    }
    codecs.into_iter().map(String::from).collect()
}

/// One adapter's decoder profiles as fourcc labels. Best-effort: a device
/// that cannot be created or a missing video interface yields no labels —
/// a failed query is never reported as a capability, nor as its absence.
fn adapter_decode_codecs(adapter: &IDXGIAdapter1) -> Vec<&'static str> {
    let mut out = Vec::new();
    let mut device: Option<ID3D11Device> = None;
    // SAFETY: out-params point at locals that outlive the call, `adapter` is
    // live, and `Software` is NULL as required for non-software drivers.
    // With a non-NULL adapter the driver type must be `UNKNOWN`, not
    // `HARDWARE` — `HARDWARE` returns E_INVALIDARG (0x80070057, observed
    // 2026-11-09, documented return value):
    // https://learn.microsoft.com/en-us/windows/win32/api/d3d11/nf-d3d11-d3d11createdevice
    let created = unsafe {
        D3D11CreateDevice(
            adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )
    };
    if created.is_err() {
        return out;
    }
    let Some(device) = device else {
        return out;
    };
    // `cast` queries the device's own interface IID and owns its reference.
    let Ok(video) = device.cast::<ID3D11VideoDevice>() else {
        return out;
    };
    // SAFETY: plain accessor; count then indexed reads below.
    let count = unsafe { video.GetVideoDecoderProfileCount() };
    for index in 0..count {
        // SAFETY: out-param handled inside the typed helper; `index` < count.
        if let Ok(guid) = unsafe { video.GetVideoDecoderProfile(index) } {
            // GUIDs outside the tracked families (vendor-private profiles
            // exist; the target machine reports 58 of them, 2026-11-09) are
            // skipped — an unknown GUID is never invented into a name.
            if let Some(label) = d3d11_codec_label(&guid) {
                out.push(label);
            }
        }
    }
    out
}

/// Fourcc label for one `D3D11_DECODER_PROFILE_*` GUID, `None` for a codec
/// family this product does not track. Labels match `subtype_label`'s fourcc
/// style so an MFT row and a GPU row for the same codec merge into one row.
fn d3d11_codec_label(guid: &GUID) -> Option<&'static str> {
    use windows::Win32::Graphics::Direct3D11 as d3d;
    const H264: [GUID; 10] = [
        d3d::D3D11_DECODER_PROFILE_H264_IDCT_FGT,
        d3d::D3D11_DECODER_PROFILE_H264_IDCT_NOFGT,
        d3d::D3D11_DECODER_PROFILE_H264_MOCOMP_FGT,
        d3d::D3D11_DECODER_PROFILE_H264_MOCOMP_NOFGT,
        d3d::D3D11_DECODER_PROFILE_H264_VLD_FGT,
        d3d::D3D11_DECODER_PROFILE_H264_VLD_MULTIVIEW_NOFGT,
        d3d::D3D11_DECODER_PROFILE_H264_VLD_NOFGT,
        d3d::D3D11_DECODER_PROFILE_H264_VLD_STEREO_NOFGT,
        d3d::D3D11_DECODER_PROFILE_H264_VLD_STEREO_PROGRESSIVE_NOFGT,
        d3d::D3D11_DECODER_PROFILE_H264_VLD_WITHFMOASO_NOFGT,
    ];
    const HEVC: [GUID; 2] = [
        d3d::D3D11_DECODER_PROFILE_HEVC_VLD_MAIN,
        d3d::D3D11_DECODER_PROFILE_HEVC_VLD_MAIN10,
    ];
    const AV1: [GUID; 5] = [
        d3d::D3D11_DECODER_PROFILE_AV1_VLD_12BIT_PROFILE2,
        d3d::D3D11_DECODER_PROFILE_AV1_VLD_12BIT_PROFILE2_420,
        d3d::D3D11_DECODER_PROFILE_AV1_VLD_PROFILE0,
        d3d::D3D11_DECODER_PROFILE_AV1_VLD_PROFILE1,
        d3d::D3D11_DECODER_PROFILE_AV1_VLD_PROFILE2,
    ];
    const VP9: [GUID; 2] = [
        d3d::D3D11_DECODER_PROFILE_VP9_VLD_10BIT_PROFILE2,
        d3d::D3D11_DECODER_PROFILE_VP9_VLD_PROFILE0,
    ];
    const MPEG2: [GUID; 4] = [
        d3d::D3D11_DECODER_PROFILE_MPEG2_IDCT,
        d3d::D3D11_DECODER_PROFILE_MPEG2_MOCOMP,
        d3d::D3D11_DECODER_PROFILE_MPEG2_VLD,
        d3d::D3D11_DECODER_PROFILE_MPEG2and1_VLD, // combined MPEG-1/2 VLD
    ];
    const MPEG4: [GUID; 3] = [
        d3d::D3D11_DECODER_PROFILE_MPEG4PT2_VLD_ADVSIMPLE_GMC,
        d3d::D3D11_DECODER_PROFILE_MPEG4PT2_VLD_ADVSIMPLE_NOGMC,
        d3d::D3D11_DECODER_PROFILE_MPEG4PT2_VLD_SIMPLE,
    ];
    const VC1: [GUID; 5] = [
        d3d::D3D11_DECODER_PROFILE_VC1_D2010,
        d3d::D3D11_DECODER_PROFILE_VC1_IDCT,
        d3d::D3D11_DECODER_PROFILE_VC1_MOCOMP,
        d3d::D3D11_DECODER_PROFILE_VC1_POSTPROC,
        d3d::D3D11_DECODER_PROFILE_VC1_VLD,
    ];
    const WMV8: [GUID; 2] = [
        d3d::D3D11_DECODER_PROFILE_WMV8_MOCOMP,
        d3d::D3D11_DECODER_PROFILE_WMV8_POSTPROC,
    ];
    const WMV9: [GUID; 3] = [
        d3d::D3D11_DECODER_PROFILE_WMV9_IDCT,
        d3d::D3D11_DECODER_PROFILE_WMV9_MOCOMP,
        d3d::D3D11_DECODER_PROFILE_WMV9_POSTPROC,
    ];
    if H264.contains(guid) {
        Some("h264")
    } else if HEVC.contains(guid) {
        Some("hevc")
    } else if AV1.contains(guid) {
        Some("av01")
    } else if VP9.contains(guid) {
        Some("vp90")
    } else if guid == &d3d::D3D11_DECODER_PROFILE_VP8_VLD {
        Some("vp08")
    } else if MPEG2.contains(guid) {
        Some("mpeg2")
    } else if guid == &d3d::D3D11_DECODER_PROFILE_MPEG1_VLD {
        Some("mpeg1")
    } else if MPEG4.contains(guid) {
        Some("mp4v")
    } else if VC1.contains(guid) {
        Some("vc1")
    } else if WMV8.contains(guid) {
        Some("wmv8")
    } else if WMV9.contains(guid) {
        Some("wmv9")
    } else {
        None
    }
}

fn enumerate_decoders() -> Option<DecodeCapability> {
    let mut activate_array: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    // SAFETY: flat API with local out-params. The array out-param must be a
    // *pointer to* a null variable, not a null pointer — passing null made
    // `MFTEnumEx` fail with E_POINTER (fixed in the cascade-media probe,
    // 2026-11-07).
    let hr = unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_DECODER,
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
            None,
            None,
            &mut activate_array,
            &mut count,
        )
    };
    if hr.is_err() {
        return None;
    }
    if activate_array.is_null() {
        // count == 0: MF ran, nothing hardware is registered.
        return Some(DecodeCapability::default());
    }

    let mut decoders = Vec::new();
    for index in 0..count as usize {
        // SAFETY: `activate_array` has `count` entries written by MFTEnumEx;
        // `ptr::read` moves each interface pointer out exactly once (the drop
        // at the end of this scope releases it; the raw array memory is freed
        // below without dropping it again).
        let activate = unsafe { std::ptr::read(activate_array.add(index)) };
        if let Some(activate) = activate {
            let mut decoder = describe_decoder(&activate);
            if decoder.name.is_empty() {
                decoder.name = format!("hardware decoder #{index}");
            }
            decoders.push(decoder);
        }
    }
    // SAFETY: the array came from MFTEnumEx's CoTaskMemAlloc (null is valid).
    unsafe { CoTaskMemFree(Some(activate_array.cast())) };
    Some(DecodeCapability {
        decoders,
        gpu_codecs: Vec::new(),
    })
}

fn describe_decoder(activate: &IMFActivate) -> HardwareDecoder {
    let name = friendly_name(activate);
    let mut inputs = Vec::new();
    // SAFETY: typed helper passes the transform's own IID; the returned
    // interface owns its reference.
    if let Ok(transform) = unsafe { activate.ActivateObject::<IMFTransform>() } {
        // Hardware decoders are async MFTs: calling GetInputAvailableType
        // without the unlock returns MF_E_TRANSFORM_ASYNC_LOCKED
        // (0xC00D6D77, "caller does not appear to support this transform's
        // asynchronous capabilities", observed on the target machine's
        // Intel decoders 2026-11-09). The attribute lives on the MFT's
        // attribute store (`GetAttributes`), not on `IMFTransform` itself.
        // Best-effort: a MFT that refuses the unlock or has no store just
        // yields no input rows below.
        if let Ok(attributes) = unsafe { transform.GetAttributes() } {
            // SAFETY: SetUINT32 only reads the receiver and copies a u32.
            let _ = unsafe { attributes.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1) };
        }
        // Any error ends the walk — the documented end is
        // MF_E_NO_MORE_TYPES, but a driver that errors early simply reports
        // no input types instead of hanging.
        let mut type_index = 0u32;
        // SAFETY: out-params are handled inside the typed helper.
        while let Ok(media_type) = unsafe { transform.GetInputAvailableType(0, type_index) } {
            // SAFETY: attribute getters only read the media type.
            if let Ok(subtype) = unsafe { media_type.GetGUID(&MF_MT_SUBTYPE) } {
                let profile = unsafe { media_type.GetUINT32(&MF_MT_MPEG2_PROFILE) }.ok();
                let level = unsafe { media_type.GetUINT32(&MF_MT_MPEG2_LEVEL) }.ok();
                inputs.push(DecodeInput {
                    subtype: subtype_label(
                        subtype.data1,
                        subtype.data2,
                        subtype.data3,
                        subtype.data4,
                    ),
                    profile,
                    level,
                });
            }
            type_index += 1;
        }
    }
    HardwareDecoder { name, inputs }
}

/// `MFT_FRIENDLY_NAME_Attribute`, or empty when the MFT does not report one.
fn friendly_name(activate: &IMFActivate) -> String {
    let mut name_ptr = windows::core::PWSTR::null();
    let mut length = 0u32;
    // SAFETY: out-params are locals; on success the string is CoTaskMem-
    // allocated and freed below.
    let result = unsafe {
        activate.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut name_ptr, &mut length)
    };
    if result.is_err() || name_ptr.is_null() {
        return String::new();
    }
    // SAFETY: non-null on the success path; NUL-terminated by the API.
    let name = unsafe { wide::from_wide_ptr(name_ptr.0) };
    // SAFETY: allocated by GetAllocatedString, so CoTaskMemFree is correct.
    unsafe { CoTaskMemFree(Some(name_ptr.0.cast())) };
    name
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_space_label_covers_sdr_hdr_and_unknown() {
        assert_eq!(
            color_space_label(DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709),
            "SDR (BT.709)"
        );
        assert_eq!(
            color_space_label(DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020),
            "HDR10 (BT.2020 PQ)"
        );
        assert_eq!(color_space_label(DXGI_COLOR_SPACE_TYPE(99)), "unknown");
    }

    #[test]
    fn d3d11_codec_label_covers_the_tracked_families() {
        use windows::Win32::Graphics::Direct3D11 as d3d;
        assert_eq!(
            d3d11_codec_label(&d3d::D3D11_DECODER_PROFILE_H264_VLD_NOFGT),
            Some("h264")
        );
        assert_eq!(
            d3d11_codec_label(&d3d::D3D11_DECODER_PROFILE_HEVC_VLD_MAIN10),
            Some("hevc")
        );
        assert_eq!(
            d3d11_codec_label(&d3d::D3D11_DECODER_PROFILE_VP9_VLD_PROFILE0),
            Some("vp90")
        );
        assert_eq!(
            d3d11_codec_label(&d3d::D3D11_DECODER_PROFILE_AV1_VLD_PROFILE0),
            Some("av01")
        );
        // A profile outside the tracked families is not renamed into one.
        assert_eq!(
            d3d11_codec_label(&d3d::D3D11_DECODER_PROFILE_VC1_VLD),
            Some("vc1")
        );
    }

    #[test]
    fn driver_match_is_case_insensitive_and_absent_when_unrelated() {
        let drivers = vec![(
            "Intel(R) UHD Graphics".to_string(),
            "30.0.101.1960".to_string(),
        )];
        assert_eq!(
            match_driver(&drivers, "intel(r) uhd graphics"),
            Some("30.0.101.1960".to_string())
        );
        // A different adapter name gets no version — printed as absent.
        assert_eq!(match_driver(&drivers, "Some Other GPU"), None);
        assert_eq!(match_driver(&drivers, "NVIDIA GeForce RTX 4060"), None);
    }
}
