//! Tier 2 media backend: `IMFSourceReader` + own DXGI flip presenter.
//!
//! Avoids the Media Engine pipeline (scheduler clock, EVR composer, audio
//! session) that made slice 2 ~5x heavier than mpv on CPU. Instead:
//! SourceReader decodes H.264 → NV12 frames → CPU convert to BGR8 into a
//! staging texture → `CopyResource` + `Present` on our own DXGI flip
//! swapchain on the host HWND.
//!
//! experiments (`CASCADE_TIER2_E3*`, off by default): attach MF's DXGI
//! device manager so decode (DXVA) runs on the GPU.
//! (`CASCADE_TIER2_E3=1`) also had MF do YUV→RGB + resize (XVP) — measured
//! crashing the Intel driver, kept for the record.
//! (`CASCADE_TIER2_E3B=1`) negotiates NV12 and does the convert + scale
//! OURSELVES via `ID3D11VideoProcessor` (mpv-style, plain Microsoft APIs),
//! never touching MF's XVP. Any setup failure falls back to the NV12
//! CPU path.
//!
//! COM + SourceReader + D3D11 all live on the playback thread (started on
//! `attach_surface`, stopped on `detach_surface`), so no COM interfaces
//! cross thread boundaries.
//!
//! Dependency direction: `cascade-media` does NOT depend on `cascade-win`.
//! The swapchain is created here directly via DXGI.
//!
//! Design notes ():
//! The swap chain back buffer is a `D3D11_USAGE_DEFAULT` texture and
//!   cannot be `Map()`ped; the converter writes into a `STAGING` texture
//!   which is copied to the back buffer.
//! The host surface size (1366x768) usually differs from the video size
//!   (1920x1080); the converter samples the frame with nearest-neighbour
//!   scaling into the surface rectangle.
//! `Present(1, 0)` blocks on vsync, which is the frame pacing. There is
//!   no `sleep(16)` after a successful present anymore.

use std::mem::ManuallyDrop;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crate::colour::{colour_tags, ColourTags, VideoMatrix, VideoRange};
use crate::fit::fit_rects;
use cascade_core::{
    BackendError, BackendResult, BackendStats, Fit, HostSurface, MediaBackend, PlaybackOptions,
};
use windows::core::{Interface, BOOL};
use windows::Win32::Foundation::{HMODULE, HWND};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, ID3D11VideoContext,
    ID3D11VideoDevice, ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator,
    ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView, D3D11_CPU_ACCESS_WRITE,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_MAPPED_SUBRESOURCE,
    D3D11_MAP_WRITE, D3D11_SDK_VERSION, D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_STAGING, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_CAPS,
    D3D11_VIDEO_PROCESSOR_COLOR_SPACE, D3D11_VIDEO_PROCESSOR_CONTENT_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
    D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_IGNORE, DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_TYPELESS, DXGI_FORMAT_B8G8R8A8_UNORM,
    DXGI_FORMAT_B8G8R8A8_UNORM_SRGB, DXGI_FORMAT_B8G8R8X8_TYPELESS, DXGI_FORMAT_B8G8R8X8_UNORM,
    DXGI_FORMAT_B8G8R8X8_UNORM_SRGB, DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    IDXGIAdapter, IDXGIDevice, IDXGIFactory2, IDXGIOutput, IDXGISwapChain1, DXGI_PRESENT,
    DXGI_SCALING_STRETCH, DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_EFFECT_FLIP_DISCARD,
    DXGI_USAGE_RENDER_TARGET_OUTPUT,
};
use windows::Win32::Media::MediaFoundation::{
    IMF2DBuffer, IMFActivate, IMFAttributes, IMFDXGIBuffer, IMFDXGIDeviceManager, IMFMediaBuffer,
    IMFMediaType, IMFSample, IMFSourceReader, MFCreateAttributes, MFCreateDXGIDeviceManager,
    MFCreateMediaType, MFCreateSourceReaderFromURL, MFMediaType_Video, MFShutdown, MFStartup,
    MFTEnumEx, MFVideoFormat_H264, MFVideoFormat_NV12, MFVideoFormat_RGB32, MFSTARTUP_FULL,
    MFT_CATEGORY_VIDEO_DECODER, MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER,
    MFT_REGISTER_TYPE_INFO, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE,
    MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, MF_SOURCE_READER_D3D_MANAGER,
    MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, MF_VERSION,
};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Variant::VT_I8;

// Stream index constants not exposed by windows 0.62 as named items.
// `pub(crate)`: the library probe (`probe.rs`) opens its own reader.
pub(crate) const MF_SOURCE_READER_ALL_STREAMS: u32 = 0xFFFFFFFF;
pub(crate) const MF_SOURCE_READER_FIRST_VIDEO_STREAM: u32 = 0xFFFFFFFC;

// Stream flags returned by ReadSample, not exposed as named items.
// `pub(crate)`: the thumbnail reader (thumbnail.rs) checks the same flags.
pub(crate) const MF_SOURCE_READER_FLAG_END_OF_STREAM: u32 = 0x2;
pub(crate) const MF_SOURCE_READER_FLAG_ERROR: u32 = 0x8;

// H.264 / AVC is the only codec Cascade ships support for in v1. Everything
// else is rejected with a clear, actionable message. The probe below answers
// two *different* questions, and keeps them separate because a bool
// cannot express "we could not tell".

/// What the H.264 probe could actually prove about this machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum H264Evidence {
    /// A hardware MFT explicitly registered `MFVideoFormat_H264` as input.
    MftRegistered,
    /// Hardware decode may still be active, but no H.264 MFT registration
    /// proves it. Measured: Intel UHD registers only an M-JPEG MFT while
    /// GPU VideoDecode handles H.264. `stats()` must report `None`, never
    /// `Some(false)` — an unobservable capability is not an absent one.
    Unobservable,
}

impl H264Evidence {
    /// Map evidence onto the honest `Option<bool>` wire answer.
    fn as_backend_report(self) -> Option<bool> {
        match self {
            Self::MftRegistered => Some(true),
            Self::Unobservable => None,
        }
    }
}

/// Enumerate hardware video decoder MFTs, optionally restricted to one input
/// subtype. The caller owns the returned array and must release it exactly
/// once with [`release_activate_array`], including when `count == 0`.
///
/// SAFETY: `MFTEnumEx` is a flat Win32 API. The array out-param must be a
/// *pointer to* a null variable, not a null pointer: passing null made
/// `MFTEnumEx` fail with E_POINTER (0x80004003) on every call, so the probe
/// rejected every file (fixed 2026-11-07).
unsafe fn enum_hw_decoders(
    input: Option<&MFT_REGISTER_TYPE_INFO>,
) -> BackendResult<(*mut Option<IMFActivate>, u32)> {
    let mut activate_array: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count: u32 = 0;
    MFTEnumEx(
        MFT_CATEGORY_VIDEO_DECODER,
        MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
        input.map(|value| value as *const _),
        None,
        &mut activate_array,
        &mut count,
    )
    .map_err(|err| {
        BackendError::Unsupported(format!(
            "cannot enumerate video decoder MFTs: 0x{:08X}",
            err.code().0
        ))
    })?;
    Ok((activate_array, count))
}

/// Release an `IMFActivate` array returned by [`enum_hw_decoders`].
///
/// SAFETY: `array` must come from a successful `enum_hw_decoders` call and
/// must not have been released before. `ptr::read` moves each interface
/// pointer out exactly once; the raw array memory is then freed without
/// dropping it again, so no element is released twice.
unsafe fn release_activate_array(array: *mut Option<IMFActivate>, count: u32) {
    if array.is_null() {
        return;
    }
    for i in 0..count as usize {
        drop(unsafe { std::ptr::read(array.add(i)) });
    }
    // SAFETY: the array came from MFTEnumEx's CoTaskMemAlloc.
    unsafe {
        windows::Win32::System::Com::CoTaskMemFree(Some(array as *const _));
    }
}

/// Probe the machine's hardware decoder support before `load()`.
///
/// Two questions are answered, and they must not be confused:
///
/// 1. **Load acceptance** (the `Result`): does *any* hardware decoder MFT
///    exist? This stays unfiltered by subtype because measured case has
///    an Intel UHD that registers only M-JPEG while GPU decode is active;
///    filtering acceptance by H.264 would reject every video there.
/// 2. **Evidence** (the returned [`H264Evidence`]): did a hardware MFT
///    explicitly register H.264? Only that permits `hwdec: true` on the wire.
///
/// A failure of the second, filtered query is `Unobservable`, not a load
/// error: the unfiltered call already proved MF works (design note ).
fn probe_hw_decoder() -> BackendResult<H264Evidence> {
    // SAFETY: initialised out-params, released below on every success path.
    let (array, count) = unsafe { enum_hw_decoders(None)? };
    if count == 0 {
        unsafe { release_activate_array(array, count) };
        return Err(BackendError::Unsupported(
            "no hardware video decoder MFT found on this machine".to_string(),
        ));
    }
    if array.is_null() {
        // Defensive: the API returns an array with a non-zero count. Refuse
        // instead of dereferencing null below.
        return Err(BackendError::Unsupported(
            "MFTEnumEx reported decoders but returned no array".to_string(),
        ));
    }
    unsafe { release_activate_array(array, count) };

    let h264_input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };
    let evidence = match unsafe { enum_hw_decoders(Some(&h264_input)) } {
        Ok((array, count)) => {
            let registered = count > 0 && !array.is_null();
            unsafe { release_activate_array(array, count) };
            if registered {
                H264Evidence::MftRegistered
            } else {
                H264Evidence::Unobservable
            }
        }
        Err(_) => H264Evidence::Unobservable,
    };
    Ok(evidence)
}

/// Log the DXGI adapter driving the D3D11 device. Useful for diagnosing
/// cross-GPU behaviour (Intel iGPU vs NVIDIA/AMD dGPU).
fn log_adapter(device: &ID3D11Device) {
    let Ok(dxgi_device) = device.cast::<IDXGIDevice>() else {
        return;
    };
    let Ok(adapter) = (unsafe { dxgi_device.GetAdapter() }) else {
        return;
    };
    let desc = match unsafe { adapter.GetDesc() } {
        Ok(d) => d,
        Err(_) => return,
    };
    let name = String::from_utf16_lossy(&desc.Description);
    let vram = desc.DedicatedVideoMemory as f64 / (1024.0 * 1024.0 * 1024.0);
    eprintln!("cascade: SourceReader: D3D11 adapter={name}  vram={vram:.1} GB");
}

/// One-shot guard: log the GPU texture description exactly once.
static E3_GPU_DESC_LOGGED: AtomicBool = AtomicBool::new(false);

/// Configuration handed to the playback thread.
struct PlaybackConfig {
    source: PathBuf,
    hwnd: isize,
    width: u32,
    height: u32,
    want_playing: Arc<AtomicBool>,
    looping: Arc<AtomicBool>,
    frames_read: Arc<AtomicU64>,
    frames_presented: Arc<AtomicU64>,
    measured_fps: Arc<AtomicU32>,
    /// Optional FPS cap from PlaybackOptions::max_fps. When set, the frame
    // loop sleeps between frames to stay at or below this rate instead of
    // running uncapped at display refresh.
    max_fps: Option<u32>,
}

/// One backend instance.
pub struct SourceReaderBackend {
    source: Option<PathBuf>,
    #[allow(dead_code)]
    opts: PlaybackOptions,
    /// What `probe_hw_decoder()` could prove in `load()`. `None` before load;
    /// `Unobservable` still maps to `None` on the wire (never `Some(false)`).
    h264_evidence: Option<H264Evidence>,
    /// Measured video FPS, updated on each presented frame for `stats()`.
    measured_fps: Arc<AtomicU32>,
    /// Command channel to the playback thread.
    cmd_tx: Option<mpsc::Sender<PlaybackCmd>>,
    /// Playback thread handle.
    thread: Option<thread::JoinHandle<()>>,
    want_playing: Arc<AtomicBool>,
    looping: Arc<AtomicBool>,
    frames_read: Arc<AtomicU64>,
    frames_presented: Arc<AtomicU64>,
    width: AtomicU32,
    height: AtomicU32,
    /// True when `new()` initialized COM on this thread. Only
    /// then does `Drop` call `CoUninitialize`.
    com_ours: bool,
}

/// Command sent from the daemon thread to the playback thread.
enum PlaybackCmd {
    Play,
    Pause,
    SetLooping(bool),
    Stop,
}

impl SourceReaderBackend {
    pub fn new() -> BackendResult<Self> {
        // COM first, then MF — the same order as `MediaEngineBackend::new`:
        // `MFTEnumEx` in `load()` creates COM objects and needs an apartment
        // on the calling thread : apartment-initialized per
        // thread). Without either prerequisite the probe can block forever.
        // SAFETY: null reserved pointer is the documented value; `Drop` only
        // uninitializes when this call is the one that initialized COM.
        let hr = unsafe {
            windows::Win32::System::Com::CoInitializeEx(
                None,
                windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
            )
        };
        let com_ours = if hr.is_ok() {
            // S_OK and S_FALSE both mean "initialized"; S_FALSE still needs
            // a balancing CoUninitialize.
            true
        } else if hr == windows::Win32::Foundation::RPC_E_CHANGED_MODE {
            // Already initialized in another apartment: leave it alone.
            false
        } else {
            return Err(BackendError::Mf(format!(
                "CoInitializeEx failed: 0x{:08X}",
                hr.0
            )));
        };
        // MF must be started with the exact `MF_VERSION`; a wrong version is
        // rejected with MF_E_INVALIDVERSION and every later MF call fails.
        // SAFETY: balances the MFShutdown in `Drop`.
        let mf = unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) };
        if let Err(err) = mf {
            if com_ours {
                // SAFETY: balances the CoInitializeEx above on this thread.
                unsafe { windows::Win32::System::Com::CoUninitialize() };
            }
            return Err(BackendError::Mf(format!("MFStartup: {}", err.message())));
        }
        Ok(Self {
            source: None,
            opts: PlaybackOptions::default(),
            h264_evidence: None,
            measured_fps: Arc::new(AtomicU32::new(0)),
            cmd_tx: None,
            thread: None,
            want_playing: Arc::new(AtomicBool::new(true)),
            looping: Arc::new(AtomicBool::new(true)),
            frames_read: Arc::new(AtomicU64::new(0)),
            frames_presented: Arc::new(AtomicU64::new(0)),
            width: AtomicU32::new(0),
            height: AtomicU32::new(0),
            com_ours,
        })
    }

    /// Playback thread entry point. Everything COM/D3D11 happens on this
    /// thread — nothing crosses thread boundaries.
    ///
    /// Every failure path logs to stderr immediately: the daemon copies this
    /// into its log file, and a silent early return is what made the first
    /// Tier 2 build show only the host's teal clear colour.
    fn playback_thread(config: PlaybackConfig, cmd_rx: mpsc::Receiver<PlaybackCmd>) {
        // SAFETY: COM must be initialized on this thread.
        let hr = unsafe {
            windows::Win32::System::Com::CoInitializeEx(
                None,
                windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
            )
        };
        if hr.is_err() {
            eprintln!("cascade: SourceReader error: CoInitializeEx failed ({hr:?})");
            return;
        }

        // SAFETY: balances the MFShutdown below; MFStartup is per-thread.
        if let Err(err) = unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) } {
            eprintln!("cascade: SourceReader error: MFStartup: {}", err.message());
            unsafe {
                windows::Win32::System::Com::CoUninitialize();
            }
            return;
        }

        // experiments:
        // (CASCADE_TIER2_E3): MF does decode + YUV→RGB + resize on the
        //   GPU via its video processor (XVP). Measured dead on this machine
        //   (Intel driver crash, 4/4 — Appendix D).
        // (CASCADE_TIER2_E3B): MF only DECODES on the GPU (NV12 out);
        //   convert + scale are ours via ID3D11VideoProcessor, bypassing the
        //   crashing XVP entirely.
        // Any setup failure falls back to the NV12 CPU path.
        let e3b = std::env::var_os("CASCADE_TIER2_E3B").is_some();
        let e3_requested = std::env::var_os("CASCADE_TIER2_E3").is_some() || e3b;
        // Diagnostic knobs (see E3Request): native size = XVP without
        // scaling; no-adv = classic converter MFT instead of XVP.
        let e3_native = std::env::var_os("CASCADE_TIER2_E3_NATIVE").is_some();
        if e3_native && e3_requested {
            eprintln!("cascade: SourceReader: diagnosis CASCADE_TIER2_E3_NATIVE=1 active (no XVP scaling)");
        }
        let e3_no_adv = std::env::var_os("CASCADE_TIER2_E3_NO_ADV").is_some();
        if e3_no_adv && e3_requested && !e3b {
            eprintln!("cascade: SourceReader: diagnosis CASCADE_TIER2_E3_NO_ADV=1 active (no advanced video processing)");
        }
        if e3b {
            eprintln!(
                "cascade: SourceReader: experiment CASCADE_TIER2_E3B=1 active (MF GPU decode + own D3D11 video processor)"
            );
        }

        // ── D3D11 device + context (the swapchain owner) ──
        //    Created before the reader so the MF device-manager hook-up
        // has it early; behaviour is unchanged without. adds
        //    D3D11_CREATE_DEVICE_VIDEO_SUPPORT — the documented flag for
        //    video processing features on the device (MF's XVP / our
        //    D3D11 video processor).
        let (device, context) = match create_d3d11(e3_requested) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("cascade: SourceReader error: D3D11: {e}");
                let _ = unsafe { MFShutdown() };
                unsafe {
                    windows::Win32::System::Com::CoUninitialize();
                }
                return;
            }
        };

        let e3_manager = if e3_requested {
            match create_dxgi_device_manager(&device) {
                Ok(manager) => {
                    if e3b {
                        eprintln!(
                            "cascade: SourceReader: experiment CASCADE_TIER2_E3B: D3D manager attached (no XVP; own video processor)"
                        );
                    } else {
                        eprintln!(
                            "cascade: SourceReader: experiment CASCADE_TIER2_E3=1 active (D3D manager + advanced video processing)"
                        );
                    }
                    Some(manager)
                }
                Err(reason) => {
                    eprintln!("cascade: SourceReader warning: E3 disabled: {reason}");
                    None
                }
            }
        } else {
            None
        };

        // ── Create the SourceReader (hardware decode via D3D manager) ──
        // experiment: when
        // requested, MF gets our D3D device so decode + YUV→RGB + resize
        // run on the GPU. Any setup failure falls back to the NV12 CPU path.
        let e3_plan = e3_manager.as_ref().map(|manager| E3Request {
            manager,
            out_size: (config.width, config.height),
            native: e3_native,
            // never enables advanced video processing (that is 's
            // XVP, measured crashing) and negotiates NV12 instead of RGB32.
            advanced: !e3_no_adv && !e3b,
            nv12_gpu: e3b,
        });
        let mut e3_active = false;
        let mut reader = match open_reader(&config.source, e3_plan) {
            Ok(r) => {
                e3_active = e3_plan.is_some();
                r
            }
            Err(reason) => {
                let fatal = |reason: String| {
                    eprintln!("cascade: SourceReader error: {reason}");
                    let _ = unsafe { MFShutdown() };
                    unsafe {
                        windows::Win32::System::Com::CoUninitialize();
                    }
                };
                if e3_plan.is_some() {
                    eprintln!(
                        "cascade: SourceReader warning: E3 setup failed ({reason}); falling back to NV12 CPU path"
                    );
                    match open_reader(&config.source, None) {
                        Ok(r) => r,
                        Err(reason) => {
                            fatal(reason);
                            return;
                        }
                    }
                } else {
                    fatal(reason);
                    return;
                }
            }
        };

        // ── Native video dimensions (surface size usually differs) ──
        let (video_w, video_h) = match query_frame_size(&reader) {
            Ok(size) => size,
            Err(err) => {
                eprintln!(
                    "cascade: SourceReader warning: MF_MT_FRAME_SIZE unavailable ({}); assuming surface size",
                    err.message()
                );
                (config.width, config.height)
            }
        };

        // ── Colour tags from the bitstream (colour fix) ──
        // MF ignores H.264 VUI colour flags and the
        // canonical gate asset has no `colr` box, so the SPS VUI is
        // the only trustworthy matrix source.
        // Computed once per load: the sequence header belongs to the file,
        // so the reader reopens below (fallback) do not change it.
        let colour = sequence_header_tags(&reader);
        eprintln!(
            "cascade: SourceReader: colour tags: matrix={} range={} (VUI matrix_coefficients={})",
            colour.matrix.as_str(),
            colour.range.as_str(),
            colour
                .matrix_coefficients
                .map(|c| c.to_string())
                .unwrap_or_else(|| "<absent>".to_string())
        );

        // ── Negotiate a DECODED output ──
        // Without an explicit output type the reader hands out the native
        // compressed H.264 access units (~140 KB per sample), not decoded
        // frames; the buffer-size check below would then reject every sample.
        if e3_active && !e3b {
            // open_reader already negotiated RGB32 at the surface size.
            eprintln!(
                "cascade: SourceReader output subtype (E3): {}",
                output_subtype(&reader)
            );
        } else {
            // NV12 (plain, fallback, or: GPU-backed with the manager).
            eprintln!(
                "cascade: SourceReader output subtype before: {}",
                output_subtype(&reader)
            );
            if let Err(reason) = negotiate_nv12(&reader) {
                eprintln!("cascade: SourceReader error: {reason}");
                let _ = unsafe { MFShutdown() };
                unsafe {
                    windows::Win32::System::Com::CoUninitialize();
                }
                return;
            }
            eprintln!(
                "cascade: SourceReader output subtype after: {}",
                output_subtype(&reader)
            );
        }

        // ── Swapchain + staging texture ──
        // Always BGRA8: flip-model swap chains only accept R16G16B16A16_FLOAT,
        // B8G8R8A8_UNORM, R8G8B8A8_UNORM and R10G10B10A2_UNORM (X8 was
        // rejected with DXGI_ERROR_INVALID_CALL, measured). 's X8 texture
        // is therefore copied across (identical 4-byte layout; the swapchain
        // ignores alpha via DXGI_ALPHA_MODE_IGNORE).
        let surface_format = DXGI_FORMAT_B8G8R8A8_UNORM;
        let swapchain = match create_swapchain(
            &device,
            config.hwnd,
            config.width,
            config.height,
            surface_format,
        ) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("cascade: SourceReader error: swapchain: {e}");
                let _ = unsafe { MFShutdown() };
                unsafe {
                    windows::Win32::System::Com::CoUninitialize();
                }
                return;
            }
        };
        // renders straight into the back buffer via VideoProcessorBlt;
        // the staging texture is only needed by the / CPU convert path.
        // Skip it in pure mode to save ~8-15 MB of private working set.
        let staging: Option<ID3D11Texture2D> = if e3b && e3_active {
            None
        } else {
            match create_staging(&device, config.width, config.height, surface_format) {
                Ok(t) => Some(t),
                Err(e) => {
                    eprintln!("cascade: SourceReader error: staging: {e}");
                    let _ = unsafe { MFShutdown() };
                    unsafe {
                        windows::Win32::System::Com::CoUninitialize();
                    }
                    return;
                }
            }
        };
        // SAFETY: the swap chain owns its buffer; it is released by Drop.
        let back_buffer: ID3D11Texture2D = match unsafe { swapchain.GetBuffer(0) } {
            Ok(b) => b,
            Err(err) => {
                eprintln!("cascade: SourceReader error: GetBuffer: {}", err.message());
                let _ = unsafe { MFShutdown() };
                unsafe {
                    windows::Win32::System::Com::CoUninitialize();
                }
                return;
            }
        };

        // ── Own D3D11 video processor (NV12 → BGRA + scale on GPU) ──
        // Created after the back buffer (it renders into it). The exact
        // input size is learned from the first frame's texture, so no video
        // dimension assumptions are made here. On any setup failure the
        // reader is reopened WITHOUT the D3D manager so samples become
        // CPU-readable again and the NV12 CPU path takes over.
        let mut e3b_pipe = if e3b && e3_active {
            let frame_rate = query_frame_rate(&reader).unwrap_or((60, 1));
            match E3bPipeline::create(
                &device,
                &context,
                &back_buffer,
                frame_rate,
                (config.width, config.height),
                colour,
            ) {
                Ok(pipe) => {
                    eprintln!(
                        "cascade: SourceReader E3b: video processor ready (frame rate {}/{})",
                        frame_rate.0, frame_rate.1
                    );
                    Some(pipe)
                }
                Err(reason) => {
                    eprintln!(
                        "cascade: SourceReader warning: E3b setup failed ({reason}); falling back to NV12 CPU path"
                    );
                    e3_active = false;
                    let fatal = |reason: String| {
                        eprintln!("cascade: SourceReader error: {reason}");
                        let _ = unsafe { MFShutdown() };
                        unsafe {
                            windows::Win32::System::Com::CoUninitialize();
                        }
                    };
                    match open_reader(&config.source, None) {
                        Ok(fresh) => {
                            reader = fresh;
                            if let Err(err) = negotiate_nv12(&reader) {
                                fatal(err);
                                return;
                            }
                            None
                        }
                        Err(err) => {
                            fatal(err);
                            return;
                        }
                    }
                }
            }
        } else {
            None
        };

        // Log video FPS for diagnostics. Query here (works for all paths,
        // not just ) so the startup line always shows the source frame rate.
        let video_fps = query_frame_rate(&reader).unwrap_or((0, 1));
        let video_fps_num = if video_fps.1 > 0 {
            video_fps.0 as f32 / video_fps.1 as f32
        } else {
            0.0
        };

        eprintln!(
            "cascade: SourceReader playback started: video {video_w}x{video_h}@{video_fps_num:.1} fps -> surface {}x{}",
            config.width, config.height
        );

        let mut frame_count: u64 = 0;
        let mut last_timestamp: i64 = -1;
        let mut warned_read = false;
        let mut warned_flags = false;
        let mut warned_convert = false;
        let mut warned_lock = false;
        let mut warned_map = false;
        let mut warned_present = false;
        let mut warned_alloc = false;
        let mut warned_staging = false;
        // one-shot log of the consume path and of each distinct error.
        let mut last_e3_path: Option<E3Path> = None;
        let mut last_e3_err: Option<String> = None;
        let mut eos_total: u64 = 0;
        let mut nosample_streak: u64 = 0;
        let mut same_ts_streak: u64 = 0;
        let mut log_paused = false;
        let mut frames_at_last_eos: u64 = 0;
        let mut stalled_eos: u32 = 0;
        // RND experiment toggle (off in normal operation).
        let skip_present = std::env::var_os("CASCADE_TIER2_SKIP_PRESENT").is_some();
        if skip_present {
            eprintln!("cascade: SourceReader: experiment CASCADE_TIER2_SKIP_PRESENT=1 active");
        }
        // RND diagnosis toggle: top 16 staging rows = solid red.
        let debug_top = std::env::var_os("CASCADE_TIER2_DEBUG_TOP").is_some();
        if debug_top {
            eprintln!("cascade: SourceReader: diagnosis CASCADE_TIER2_DEBUG_TOP=1 active");
        }

        // Frame pacing: max_fps cap (if set) vsync is the default pacing.
        let max_fps = config.max_fps;
        let mut last_present = Instant::now();
        // FPS measurement: count frames over 1-second windows for stats().
        let mut frames_this_second: u64 = 0;
        let mut fps_last_update = Instant::now();

        loop {
            // Check for commands (non-blocking).
            if let Ok(cmd) = cmd_rx.try_recv() {
                match cmd {
                    PlaybackCmd::Play => config.want_playing.store(true, Ordering::Release),
                    PlaybackCmd::Pause => config.want_playing.store(false, Ordering::Release),
                    PlaybackCmd::SetLooping(on) => config.looping.store(on, Ordering::Release),
                    PlaybackCmd::Stop => break,
                }
            }

            if !config.want_playing.load(Ordering::Acquire) {
                if !log_paused {
                    log_paused = true;
                    eprintln!(
                        "cascade: SourceReader: paused (want_playing=false) at frame {frame_count}"
                    );
                }
                thread::sleep(Duration::from_millis(5));
                continue;
            }
            if log_paused {
                log_paused = false;
                eprintln!("cascade: SourceReader: resumed at frame {frame_count}");
            }

            // ── Read one decoded sample ──
            let mut actual_stream: u32 = 0;
            let mut stream_flags: u32 = 0;
            let mut timestamp: i64 = 0;
            let mut sample: Option<IMFSample> = None;

            // SAFETY: out-params are initialised; `sample` starts as None.
            // Only the video stream is selected, so read it directly:
            // MF_SOURCE_READER_ALL_STREAMS with a deselected set returns
            // MF_E_INVALIDSTREAMNUMBER.
            let hr = unsafe {
                reader.ReadSample(
                    MF_SOURCE_READER_FIRST_VIDEO_STREAM,
                    0,
                    Some(&mut actual_stream as *mut u32),
                    Some(&mut stream_flags as *mut u32),
                    Some(&mut timestamp as *mut i64),
                    Some(&mut sample as *mut Option<IMFSample>),
                )
            };

            if let Err(err) = hr {
                if !warned_read {
                    warned_read = true;
                    eprintln!("cascade: SourceReader error: ReadSample: {}", err.message());
                    // A device-lost error names its real cause (removed /
                    // hung / suspended + driver reason) instead of the
                    // generic ReadSample message.
                    // SAFETY: device is live on this thread.
                    let removed = unsafe { device.GetDeviceRemovedReason() };
                    eprintln!("cascade: SourceReader: GetDeviceRemovedReason -> {removed:?}");
                }
                thread::sleep(Duration::from_millis(5));
                continue;
            }

            if stream_flags & MF_SOURCE_READER_FLAG_ERROR != 0 && !warned_flags {
                warned_flags = true;
                eprintln!(
                    "cascade: SourceReader error: stream flagged error (flags={stream_flags:#x})"
                );
            }

            // End of stream: rewind (looping) or stop. EOS may arrive with a
            // final sample or on its own call, so this runs before the
            // sample handling below.
            if stream_flags & MF_SOURCE_READER_FLAG_END_OF_STREAM != 0 {
                eos_total += 1;
                if !config.looping.load(Ordering::Acquire) {
                    eprintln!("cascade: SourceReader: EOS #{eos_total}, looping off -> pause");
                    config.want_playing.store(false, Ordering::Release);
                } else {
                    // An EOS that produces no frame since the previous one
                    // means the rewind did not actually move the position.
                    if frame_count == frames_at_last_eos {
                        stalled_eos += 1;
                    } else {
                        frames_at_last_eos = frame_count;
                        stalled_eos = 0;
                    }

                    if stalled_eos >= 3 {
                        // Reopen the source: a fresh reader always starts at
                        // position 0, so looping survives a no-op seek.
                        let reopen_e3 = if e3_active { e3_plan } else { None };
                        match open_reader(&config.source, reopen_e3) {
                            Ok(fresh) => {
                                eprintln!(
                                    "cascade: SourceReader: EOS stalled {stalled_eos}x -> reader reopened (frame {frame_count})"
                                );
                                reader = fresh;
                                stalled_eos = 0;
                                // already negotiated RGB32 inside open_reader.
                                if !e3_active {
                                    if let Err(reason) = negotiate_nv12(&reader) {
                                        eprintln!(
                                            "cascade: SourceReader error: renegotiate: {reason}"
                                        );
                                    }
                                }
                            }
                            Err(reason) => {
                                eprintln!("cascade: SourceReader error: reopen failed: {reason}");
                                config.want_playing.store(false, Ordering::Release);
                            }
                        }
                    } else if let Err(msg) = seek_start(&reader) {
                        eprintln!(
                            "cascade: SourceReader error: EOS #{eos_total} seek failed: {msg}"
                        );
                    }

                    last_timestamp = -1;
                    if eos_total <= 3 || eos_total.is_multiple_of(500) {
                        eprintln!(
                            "cascade: SourceReader: EOS #{eos_total} rewound after {frame_count} frames"
                        );
                    }
                }
            }

            let Some(sample) = sample else {
                // No sample and no forward progress: count it so a stall is
                // visible in the log instead of silent.
                nosample_streak += 1;
                if nosample_streak <= 3 || nosample_streak.is_multiple_of(300) {
                    eprintln!(
                        "cascade: SourceReader: no sample (streak={nosample_streak}, flags={stream_flags:#x}, ts={timestamp})"
                    );
                }
                thread::sleep(Duration::from_millis(5));
                continue;
            };
            nosample_streak = 0;

            // Same timestamp = the source has not advanced (content fps below
            // the loop rate): the previous frame is still on screen, skip the
            // conversion entirely.
            if timestamp == last_timestamp {
                same_ts_streak += 1;
                if same_ts_streak <= 3 || same_ts_streak.is_multiple_of(600) {
                    eprintln!(
                        "cascade: SourceReader: timestamp stuck at {timestamp} (streak={same_ts_streak})"
                    );
                }
                thread::sleep(Duration::from_millis(5));
                continue;
            }
            same_ts_streak = 0;
            last_timestamp = timestamp;

            // RND experiment only:
            // CASCADE_TIER2_SKIP_PRESENT=1 measures decode-only CPU — the
            // convert/upload/present path is skipped, pacing via sleep(16).
            if skip_present {
                frame_count = frame_count.wrapping_add(1);
                config.frames_read.fetch_add(1, Ordering::Release);
                thread::sleep(Duration::from_millis(16));
                continue;
            }

            // ── Consume the decoded frame ──
            // `cpu_frame` runs the NV12 CPU converter whenever the frame is
            // not handled on the GPU: normal Tier 2, fallback, after
            // a failed setup, or an sample that turned out not to be
            // GPU-backed (software decode fallback).
            let mut cpu_frame = !e3_active;
            if e3b && e3_active {
                // MF hands us an NV12 DXGI texture; our video
                // processor converts + scales it straight into the back
                // buffer (never through the CPU). Each distinct error is
                // logged once.
                match e3b_pipe.as_mut() {
                    Some(pipe) => {
                        match pipe.consume(&sample) {
                            Ok(()) => {
                                if last_e3_path != Some(E3B_PATH) {
                                    eprintln!("cascade: SourceReader E3b: consuming frames via {E3B_PATH}");
                                    last_e3_path = Some(E3B_PATH);
                                }
                                last_e3_err = None;
                            }
                            Err(E3bFrame::NotGpu) => {
                                if last_e3_path != Some("cpu-fallback") {
                                    eprintln!(
                                    "cascade: SourceReader E3b: sample not GPU-backed; using the NV12 CPU path"
                                );
                                    last_e3_path = Some("cpu-fallback");
                                }
                                cpu_frame = true;
                            }
                            Err(E3bFrame::Hard(msg)) => {
                                if last_e3_err.as_deref() != Some(msg.as_str()) {
                                    eprintln!("cascade: SourceReader E3b: {msg}");
                                    last_e3_err = Some(msg);
                                }
                                thread::sleep(Duration::from_millis(5));
                                continue;
                            }
                        }
                    }
                    // Defensive: e3b && e3_active implies the pipeline was
                    // built; if not, the CPU path keeps the frames flowing.
                    None => cpu_frame = true,
                }
            } else if e3_active {
                // RGB32 arrives either as a DXGI texture (GPU path:
                // CopyResource straight to the back buffer) or as a CPU
                // buffer (row copy into staging). Each distinct error is
                // logged once instead of every frame.
                let out_w = config.width as usize;
                let out_h = config.height as usize;
                // SAFETY: sample and D3D objects are live on this thread.
                match unsafe {
                    consume_e3_sample(
                        &sample,
                        &context,
                        staging.as_ref().unwrap(), // path: staging is always Some
                        &back_buffer,
                        out_w,
                        out_h,
                        debug_top,
                    )
                } {
                    Ok(path) => {
                        if last_e3_path != Some(path) {
                            eprintln!("cascade: SourceReader E3: consuming frames via {path}");
                            last_e3_path = Some(path);
                        }
                        last_e3_err = None;
                    }
                    Err(msg) => {
                        if last_e3_err.as_deref() != Some(msg.as_str()) {
                            eprintln!("cascade: SourceReader E3: {msg}");
                            last_e3_err = Some(msg);
                        }
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                }
            }

            if cpu_frame {
                // ── NV12 → BGR8 → copy → present ──
                // GPU path doesn't use staging; only the CPU fallback
                // path needs it. If staging is None (pure mode), skip.
                let Some(staging) = &staging else {
                    if !warned_staging {
                        warned_staging = true;
                        eprintln!("cascade: SourceReader E3b: CPU fallback without staging texture; skipping frame");
                    }
                    thread::sleep(Duration::from_millis(5));
                    continue;
                };

                // SAFETY: sample is live.
                let Ok(buffer) = (unsafe { sample.ConvertToContiguousBuffer() }) else {
                    if !warned_convert {
                        warned_convert = true;
                        eprintln!("cascade: SourceReader error: ConvertToContiguousBuffer failed (DXVA surface without CPU copy?)");
                    }
                    thread::sleep(Duration::from_millis(5));
                    continue;
                };

                let mut buf_ptr: *mut u8 = std::ptr::null_mut();
                let mut buf_len: u32 = 0;
                // SAFETY: `buf_ptr`/`buf_len` are initialised; Unlock is paired.
                if unsafe { buffer.Lock(&mut buf_ptr, None, Some(&mut buf_len)) }.is_err() {
                    if !warned_lock {
                        warned_lock = true;
                        eprintln!("cascade: SourceReader error: buffer Lock failed");
                    }
                    thread::sleep(Duration::from_millis(5));
                    continue;
                }

                let needed = video_w as usize * video_h as usize * 3 / 2;
                if (buf_len as usize) < needed {
                    unsafe {
                        let _ = buffer.Unlock();
                    }
                    if !warned_convert {
                        warned_convert = true;
                        eprintln!(
                            "cascade: SourceReader error: NV12 buffer too small: {buf_len} < {needed} (video {video_w}x{video_h})"
                        );
                    }
                    thread::sleep(Duration::from_millis(5));
                    continue;
                }

                let mut mapped: D3D11_MAPPED_SUBRESOURCE = unsafe { std::mem::zeroed() };
                // SAFETY: staging texture has CPU write access.
                if unsafe { context.Map(staging, 0, D3D11_MAP_WRITE, 0, Some(&mut mapped)) }
                    .is_err()
                {
                    unsafe {
                        let _ = buffer.Unlock();
                    }
                    if !warned_map {
                        warned_map = true;
                        eprintln!("cascade: SourceReader error: Map(staging) failed");
                    }
                    thread::sleep(Duration::from_millis(5));
                    continue;
                }

                let dst: *mut u8 = mapped.pData as *mut u8;
                let row_pitch = mapped.RowPitch as usize;
                let src = buf_ptr as *const u8;

                // The decoder allocates the luma plane at macroblock-padded
                // height (1080 content -> 1088 rows), so the chroma plane starts
                // after the ALLOCATED height, not the display height. Reading it
                // from video_h shifted UV by 8 luma rows: the first ~11 output
                // rows got chroma from luma padding bytes and rendered as a
                // solid green band (measured 0,167..229,0 at y=2). Derive the
                // allocated height from the real buffer size; fall back to the
                // display height when the size does not divide evenly.
                let uv_offset = {
                    let alloc_h =
                        nv12_alloc_height(buf_len as usize, video_w as usize, video_h as usize);
                    if alloc_h != video_h as usize && !warned_alloc {
                        warned_alloc = true;
                        eprintln!(
                            "cascade: SourceReader: NV12 allocated height {alloc_h} != display {} (uv offset fix)",
                            video_h
                        );
                    }
                    video_w as usize * alloc_h
                };

                // SAFETY: `buf_len >= needed`, so both planes are in bounds;
                // `dst` covers `width x height` at `row_pitch`.
                unsafe {
                    convert_nv12_to_bgr8(
                        src,
                        dst,
                        row_pitch,
                        uv_offset,
                        video_w as usize,
                        video_h as usize,
                        config.width as usize,
                        config.height as usize,
                        colour,
                    );
                }

                // RND diagnosis: paint the
                // first 16 staging rows solid red. If the on-screen band turns
                // red, the band comes from our converted content; if it stays
                // green, it is added downstream of the staging texture.
                if debug_top {
                    // SAFETY: rows 0..16 x width are inside the mapped texture.
                    unsafe {
                        for oy in 0..16.min(config.height as usize) {
                            let row = dst.add(oy * row_pitch);
                            for ox in 0..config.width as usize {
                                let off = ox * 4;
                                *row.add(off) = 0;
                                *row.add(off + 1) = 0;
                                *row.add(off + 2) = 255;
                                *row.add(off + 3) = 255;
                            }
                        }
                    }
                }

                unsafe {
                    context.Unmap(staging, 0);
                    let _ = buffer.Unlock();
                }

                // staging → back buffer (the DEFAULT back buffer cannot be mapped).
                unsafe {
                    context.CopyResource(&back_buffer, staging);
                }
            }

            // Present with vsync: this is the frame pacing.
            // SAFETY: swap chain is alive for the whole loop.
            let hr = unsafe { swapchain.Present(1, DXGI_PRESENT(0)) };
            if hr.is_err() && !warned_present {
                warned_present = true;
                eprintln!("cascade: SourceReader error: Present {hr:?}");
            }

            // FPS measurement for stats().
            frames_this_second += 1;
            if fps_last_update.elapsed() >= Duration::from_secs(1) {
                let elapsed = fps_last_update.elapsed().as_secs_f32();
                let fps = (frames_this_second as f32 / elapsed).round() as u32;
                config.measured_fps.store(fps, Ordering::Release);
                frames_this_second = 0;
                fps_last_update = Instant::now();
            }

            // max_fps cap: if the caller asked for a frame-rate limit,
            // sleep after presenting so we don't run uncapped at display
            // refresh. vsync already caps at the display rate; this is for
            // battery saving or heavy videos that can't keep up.
            if let Some(fps) = max_fps {
                let target_interval = Duration::from_secs_f32(1.0 / fps as f32);
                let elapsed = last_present.elapsed();
                if elapsed < target_interval {
                    thread::sleep(target_interval - elapsed);
                }
            }
            last_present = Instant::now();

            frame_count = frame_count.wrapping_add(1);
            config.frames_read.fetch_add(1, Ordering::Release);
            config.frames_presented.fetch_add(1, Ordering::Release);

            if frame_count.is_multiple_of(600) {
                eprintln!("cascade: SourceReader frames={frame_count}");
            }
        }

        // Cleanup on this thread only. The video processor and its render
        // target view reference the back buffer, so they are released first.
        drop(e3b_pipe);
        drop(back_buffer);
        drop(staging);
        drop(swapchain);
        drop(context);
        drop(device);
        let _ = unsafe { MFShutdown() };
        unsafe {
            windows::Win32::System::Com::CoUninitialize();
        }
        eprintln!("cascade: SourceReader playback stopped after {frame_count} frames");
    }

    fn start_playback_thread(&mut self, hwnd: isize, width: u32, height: u32) -> BackendResult<()> {
        let source = self
            .source
            .clone()
            .ok_or_else(|| BackendError::Mf("no source loaded".into()))?;

        let (cmd_tx, cmd_rx) = mpsc::channel();

        let config = PlaybackConfig {
            source,
            hwnd,
            width,
            height,
            want_playing: Arc::clone(&self.want_playing),
            looping: Arc::clone(&self.looping),
            frames_read: Arc::clone(&self.frames_read),
            frames_presented: Arc::clone(&self.frames_presented),
            measured_fps: Arc::clone(&self.measured_fps),
            max_fps: self.opts.max_fps,
        };

        let thread = thread::spawn(move || {
            Self::playback_thread(config, cmd_rx);
        });

        self.cmd_tx = Some(cmd_tx);
        self.thread = Some(thread);
        Ok(())
    }

    fn stop_playback_thread(&mut self) {
        if let Some(tx) = self.cmd_tx.take() {
            let _ = tx.send(PlaybackCmd::Stop);
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for SourceReaderBackend {
    fn drop(&mut self) {
        self.stop_playback_thread();
        // SAFETY: balances the MFStartup in `new`. MF shuts down before COM
        // so it can still use COM objects while tearing down.
        let _ = unsafe { MFShutdown() };
        if self.com_ours {
            // SAFETY: balances the CoInitializeEx `new` performed on the
            // constructing thread; this backend is owned and dropped by the
            // same thread that constructed it (the daemon's main thread).
            unsafe { windows::Win32::System::Com::CoUninitialize() };
        }
    }
}

impl MediaBackend for SourceReaderBackend {
    fn load(&mut self, source: &Path, opts: &PlaybackOptions) -> BackendResult<()> {
        // 5 +: prove a hardware decoder exists (unfiltered, the
        // load gate), then record whether H.264 registration was actually
        // observed. The two answers must not be conflated.
        let evidence = probe_hw_decoder()?;
        std::fs::metadata(source).map_err(|err| BackendError::Open {
            path: source.display().to_string(),
            reason: err.to_string(),
        })?;
        self.source = Some(source.to_path_buf());
        self.opts = opts.clone();
        self.h264_evidence = Some(evidence);
        Ok(())
    }

    fn play(&mut self) -> BackendResult<()> {
        self.want_playing.store(true, Ordering::Release);
        if let Some(tx) = &self.cmd_tx {
            let _ = tx.send(PlaybackCmd::Play);
        }
        Ok(())
    }

    fn pause(&mut self) -> BackendResult<()> {
        self.want_playing.store(false, Ordering::Release);
        if let Some(tx) = &self.cmd_tx {
            let _ = tx.send(PlaybackCmd::Pause);
        }
        Ok(())
    }

    fn set_looping(&mut self, on: bool) -> BackendResult<()> {
        self.looping.store(on, Ordering::Release);
        if let Some(tx) = &self.cmd_tx {
            let _ = tx.send(PlaybackCmd::SetLooping(on));
        }
        Ok(())
    }

    fn set_volume(&mut self, _level: u8) -> BackendResult<()> {
        // No audio stream exists on this route — every stream but the video
        // is deselected at load (the `--aid=no` equivalent), so there is no
        // session to adjust.
        Ok(())
    }

    fn stats(&self) -> BackendStats {
        BackendStats {
            // Only a confirmed H.264 MFT registration may answer `true`.
            // `Unobservable` (and no load at all) stays `None` — claiming
            // `Some(false)` would be a guess, which forbids.
            hardware_decode: self.h264_evidence.and_then(H264Evidence::as_backend_report),
            decoded_frames: self.frames_read.load(Ordering::Acquire),
            dropped_frames: 0,
            video_size: (
                self.width.load(Ordering::Acquire),
                self.height.load(Ordering::Acquire),
            ),
            fps: self.measured_fps.load(Ordering::Acquire) as f32,
            // Not an MF media engine: the `engine` status field is the
            // engine backend's alone.
            engine_mode: None,
        }
    }

    fn attach_surface(&mut self, surface: HostSurface) -> BackendResult<()> {
        self.width.store(surface.rect.w as u32, Ordering::Release);
        self.height.store(surface.rect.h as u32, Ordering::Release);
        self.start_playback_thread(surface.hwnd, surface.rect.w as u32, surface.rect.h as u32)
    }

    fn detach_surface(&mut self) -> BackendResult<()> {
        self.stop_playback_thread();
        self.width.store(0, Ordering::Release);
        self.height.store(0, Ordering::Release);
        Ok(())
    }

    fn shutdown(self: Box<Self>) {
        drop(self);
    }
}

/// setup handed to `open_reader` (experiments, `CASCADE_TIER2_E3*`).
#[derive(Clone, Copy)]
struct E3Request<'a> {
    manager: &'a IMFDXGIDeviceManager,
    /// Surface size to negotiate (ignored when `native` is set).
    out_size: (u32, u32),
    /// Diagnostic knob (`CASCADE_TIER2_E3_NATIVE=1`): negotiate the native
    /// video size so XVP converts WITHOUT scaling — isolates whether the
    /// Intel driver crash comes from the resize step.
    native: bool,
    /// Diagnostic knob (`CASCADE_TIER2_E3_NO_ADV=1`): do NOT set
    /// MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING — the classic
    /// converter MFT path instead of the XVP (which crashes the Intel
    /// driver on this machine, measured 4/4).
    advanced: bool,
    /// (`CASCADE_TIER2_E3B=1`): negotiate NV12 instead of RGB32 —
    /// MF only decodes on the GPU; conversion/scaling are ours
    /// (`ID3D11VideoProcessor`), so no XVP is ever created.
    nv12_gpu: bool,
}

/// Open the file and return a reader with only the video stream selected.
///
/// `e3` (experiment): attaches MF's DXGI device manager (hardware
/// decode + GPU processing) and negotiates RGB32 output, so MF does
/// YUV→RGB (and optionally the resize) on the GPU. Any failure is an
/// error so the caller can reopen WITHOUT the D3D manager — a reader that
/// keeps the manager while we consume NV12 on the CPU produces GPU-backed
/// samples whose `Lock` fails (measured).
///
/// Used both at startup and as the loop-rewind fallback when a seek is a
/// silent no-op (a fresh reader always starts at position 0).
fn open_reader(source: &Path, e3: Option<E3Request<'_>>) -> Result<IMFSourceReader, String> {
    let url = file_url(source);
    eprintln!("cascade: SourceReader opening {url}");

    let mut attrs: Option<IMFAttributes> = None;
    // SAFETY: plain COM creation, out-param initialised to None.
    unsafe { MFCreateAttributes(&mut attrs, 0) }
        .map_err(|err| format!("MFCreateAttributes: {}", err.message()))?;
    let attrs = attrs.ok_or_else(|| "MFCreateAttributes returned no object".to_string())?;

    // Ask MF to use the hardware H.264 decoder (DXVA). Without this the
    // SourceReader silently falls back to the software decoder — measured on
    // this machine: GPU videodecode counter stayed at 0% and CPU was ~17%.
    // SAFETY: `attrs` is a live IMFAttributes; a failed set is a warning only.
    if let Err(err) = unsafe { attrs.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1) } {
        eprintln!(
            "cascade: SourceReader warning: enable hardware transforms: {}",
            err.message()
        );
    }

    // hand MF our D3D device. Per the MF_SOURCE_READER_D3D_MANAGER
    // docs this — not MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS — is the
    // switch that makes the source reader decode on the GPU.
    if let Some(req) = e3 {
        unsafe { attrs.SetUnknown(&MF_SOURCE_READER_D3D_MANAGER, req.manager) }
            .map_err(|err| format!("SetUnknown(D3D_MANAGER): {}", err.message()))?;
        // Enables the video processor (XVP): YUV→RGB + resize on the GPU.
        // Skipped by the CASCADE_TIER2_E3_NO_ADV diagnostic knob.
        if req.advanced {
            unsafe { attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1) }
                .map_err(|err| {
                    format!("SetUINT32(ADVANCED_VIDEO_PROCESSING): {}", err.message())
                })?;
        }
    }

    let wide: Vec<u16> = url.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: `wide` is nul-terminated and lives through the call.
    let reader = unsafe {
        MFCreateSourceReaderFromURL(windows::core::PCWSTR::from_raw(wide.as_ptr()), Some(&attrs))
    }
    .map_err(|err| format!("MFCreateSourceReaderFromURL: {}", err.message()))?;

    // Order matters: deselect everything FIRST, then select the video
    // stream — the reverse order deselects the video again and ReadSample
    // fails with MF_E_INVALIDSTREAMNUMBER.
    // SAFETY: live reader; failures are logged as warnings.
    let _ = unsafe { reader.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS, false) };
    if let Err(err) =
        unsafe { reader.SetStreamSelection(MF_SOURCE_READER_FIRST_VIDEO_STREAM, true) }
    {
        eprintln!(
            "cascade: SourceReader warning: select video stream: {}",
            err.message()
        );
    }

    // output: RGB32 — MF scales on the GPU (or converts only, when the
    // native-size diagnostic knob is set). (nv12_gpu) keeps NV12: the
    // caller negotiates it below and our own video processor does convert +
    // scale, so RGB32 (and with it the crashing XVP) is never requested.
    if let Some(req) = e3 {
        if !req.nv12_gpu {
            let (out_w, out_h) = if req.native {
                query_frame_size(&reader)
                    .map_err(|err| format!("query_frame_size: {}", err.message()))?
            } else {
                req.out_size
            };
            eprintln!(
                "cascade: SourceReader E3: negotiating RGB32 {out_w}x{out_h}{}",
                if req.native {
                    " (native: convert only, no scaling)"
                } else {
                    ""
                }
            );
            negotiate_rgb32(&reader, out_w, out_h)?;
        } else {
            eprintln!(
                "cascade: SourceReader E3b: manager attached, NV12 output (GPU decode; convert+scale are ours)"
            );
        }
    }
    Ok(reader)
}

/// Open the file for a CPU-side one-frame consumer (the thumbnail
/// reader): video stream selected, no D3D manager,
/// no output negotiation — the caller negotiates its own output type.
/// `pub(crate)` wrapper so `E3Request` stays private.
pub(crate) fn open_reader_plain(source: &Path) -> Result<IMFSourceReader, String> {
    open_reader(source, None)
}

/// Seek to the start of the stream (media time 0, units of 100 ns).
///
/// The `PROPVARIANT` must be `VT_I8`: a `VT_EMPTY` value returns S_OK but
/// does not move the position, which silently turned every loop rewind into
/// a no-op (measured: EOS fired ~160x/s with no frames produced).
fn seek_start(reader: &IMFSourceReader) -> Result<(), String> {
    let mut var = PROPVARIANT::default();
    // SAFETY: `var` is zeroed, so every byte is valid; we then set the type
    // and the matching union member for VT_I8.
    unsafe {
        (*var.Anonymous.Anonymous).vt = VT_I8;
        (*var.Anonymous.Anonymous).Anonymous.hVal = 0;
    }
    // SAFETY: reader is live; GUID_NULL selects its default time format.
    unsafe { reader.SetCurrentPosition(&windows::core::GUID::zeroed(), &var) }
        .map_err(|err| err.message())
}

/// Read `MF_MT_FRAME_SIZE` (UINT64: width in the high dword) from the
/// native media type of the first video stream.
/// `pub(crate)`: the thumbnail reader negotiates RGB32 at native size.
pub(crate) fn query_frame_size(reader: &IMFSourceReader) -> windows::core::Result<(u32, u32)> {
    // SAFETY: stream/type indices are the documented defaults for a file
    // with a single video stream; MF owns the returned media type.
    let mt = unsafe { reader.GetNativeMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM, 0) }?;
    // SAFETY: `mt` is a live IMFAttributes; the key exists on video types.
    let packed = unsafe { mt.GetUINT64(&MF_MT_FRAME_SIZE) }?;
    Ok(((packed >> 32) as u32, packed as u32))
}

/// Split an `MF_MT_FRAME_RATE` UINT64 (numerator in the high dword) into
/// its two dwords. Zero parts are preserved — the caller substitutes a
/// default instead of dividing by zero.
fn frame_rate_from_u64(packed: u64) -> (u32, u32) {
    ((packed >> 32) as u32, packed as u32)
}

/// Read `MF_MT_FRAME_RATE` from the native video type for the video
/// processor content description. `None` when the file does not report one.
pub(crate) fn query_frame_rate(reader: &IMFSourceReader) -> Option<(u32, u32)> {
    // SAFETY: stream/type indices are the documented video-stream defaults.
    let mt = unsafe { reader.GetNativeMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM, 0) }.ok()?;
    // SAFETY: `mt` is a live media type; a missing key surfaces as an error.
    let packed = unsafe { mt.GetUINT64(&MF_MT_FRAME_RATE) }.ok()?;
    let (num, den) = frame_rate_from_u64(packed);
    if num == 0 || den == 0 {
        return None;
    }
    Some((num, den))
}

/// Format the current output subtype of the video stream for diagnostics.
fn output_subtype(reader: &IMFSourceReader) -> String {
    // SAFETY: reader is live; indices are the video stream default.
    match unsafe { reader.GetCurrentMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM) } {
        Ok(mt) => subtype_name(&mt),
        Err(err) => format!("<GetCurrentMediaType failed: {}>", err.message()),
    }
}

/// `MF_MT_SUBTYPE` of a media type as a readable GUID.
fn subtype_name(mt: &IMFMediaType) -> String {
    // SAFETY: `mt` is live; missing keys surface as an error string.
    match unsafe { mt.GetGUID(&MF_MT_SUBTYPE) } {
        Ok(guid) => format!("{guid:?}"),
        Err(err) => format!("<GetGUID failed: {}>", err.message()),
    }
}

/// Ask the reader for decoded NV12 frames on the video stream.
///
/// Only NV12 is supported by the CPU converter; anything else is a hard
/// error so the daemon log shows exactly what happened.
/// `pub(crate)`: the thumbnail reader (thumbnail.rs) uses the same
/// proven output type (amendment — RGB32 is a
/// measured dead end on this machine, D.3).
pub(crate) fn negotiate_nv12(reader: &IMFSourceReader) -> Result<(), String> {
    // SAFETY: plain COM object creation.
    let mt = unsafe { MFCreateMediaType() }
        .map_err(|err| format!("MFCreateMediaType: {}", err.message()))?;
    // SAFETY: `mt` is a fresh media type; keys are the documented ones.
    unsafe { mt.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video) }
        .map_err(|err| format!("SetGUID(MAJOR_TYPE): {}", err.message()))?;
    unsafe { mt.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12) }
        .map_err(|err| format!("SetGUID(SUBTYPE): {}", err.message()))?;
    // SAFETY: reader and media type are live and belong to this thread.
    unsafe { reader.SetCurrentMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM, None, &mt) }
        .map_err(|err| format!("SetCurrentMediaType(NV12): {}", err.message()))
}

/// Which path an frame took: `"cpu-buffer"` or `"gpu-texture"`.
type E3Path = &'static str;

/// create an MF DXGI device manager bound to `device`.
///
/// The manager is attached to the SourceReader as
/// `MF_SOURCE_READER_D3D_MANAGER` — per the Microsoft docs that is the
/// switch which makes the source reader decode on the GPU (not
/// `MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS`, which is a no-op for DXVA).
pub(crate) fn create_dxgi_device_manager(
    device: &ID3D11Device,
) -> Result<IMFDXGIDeviceManager, String> {
    let mut reset_token: u32 = 0;
    let mut manager: Option<IMFDXGIDeviceManager> = None;
    // SAFETY: out-params are initialised; MF owns the returned manager.
    unsafe { MFCreateDXGIDeviceManager(&mut reset_token, &mut manager) }
        .map_err(|err| format!("MFCreateDXGIDeviceManager: {}", err.message()))?;
    let manager =
        manager.ok_or_else(|| "MFCreateDXGIDeviceManager returned no object".to_string())?;
    // SAFETY: `device` lives on this thread together with the manager, and
    // `&ID3D11Device` converts to the `IUnknown` COM parameter.
    unsafe { manager.ResetDevice(device, reset_token) }
        .map_err(|err| format!("ResetDevice: {}", err.message()))?;
    Ok(manager)
}

/// ask the reader for RGB32 frames at `out_w x out_h`.
///
/// With advanced video processing enabled the reader inserts MF's video
/// processor (XVP): YUV→RGB conversion and the resize both run on the GPU,
/// so the CPU only ever copies or forwards the finished frame.
/// The video processor needs a frame rate: copy it from the native type.
/// A missing native rate is tolerated — the reader may still accept the
/// type without it.
/// `pub(crate)`: the thumbnail reader (thumbnail.rs) reuses it at native
/// size.
pub(crate) fn negotiate_rgb32(
    reader: &IMFSourceReader,
    out_w: u32,
    out_h: u32,
) -> Result<(), String> {
    // SAFETY: plain COM object creation.
    let mt = unsafe { MFCreateMediaType() }
        .map_err(|err| format!("MFCreateMediaType: {}", err.message()))?;
    // SAFETY: `mt` is a fresh media type; keys are the documented ones.
    unsafe { mt.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video) }
        .map_err(|err| format!("SetGUID(MAJOR_TYPE): {}", err.message()))?;
    unsafe { mt.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32) }
        .map_err(|err| format!("SetGUID(SUBTYPE): {}", err.message()))?;
    // MF_MT_FRAME_SIZE is a UINT64: width in the high dword, height in the
    // low dword (same packing as query_frame_size reads it).
    unsafe { mt.SetUINT64(&MF_MT_FRAME_SIZE, ((out_w as u64) << 32) | u64::from(out_h)) }
        .map_err(|err| format!("SetUINT64(FRAME_SIZE): {}", err.message()))?;
    // The video processor needs a frame rate: copy it from the native type.
    // A missing native rate is tolerated — the reader may still accept the
    // type without it.
    // SAFETY: stream index 0 is the video stream of this file.
    if let Ok(native) = unsafe { reader.GetNativeMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM, 0) }
    {
        // SAFETY: `native` is a live video media type.
        if let Ok(rate) = unsafe { native.GetUINT64(&MF_MT_FRAME_RATE) } {
            let _ = unsafe { mt.SetUINT64(&MF_MT_FRAME_RATE, rate) };
        }
    }
    // SAFETY: reader and media type are live and belong to this thread.
    unsafe { reader.SetCurrentMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM, None, &mt) }.map_err(
        |err| {
            format!(
                "SetCurrentMediaType(RGB32 {out_w}x{out_h}): {}",
                err.message()
            )
        },
    )
}

/// Infer the row stride of a locked RGB32 buffer from its total size.
///
/// A tight buffer holds exactly `out_w * 4 * out_h` bytes; padded buffers
/// are a whole number of `out_h` rows of at least `out_w * 4` bytes.
/// Returns `None` when the size cannot cover the surface.
/// `pub(crate)`: the thumbnail reader validates its locked buffer the same
/// way.
pub(crate) fn rgb32_stride(buf_len: usize, out_w: usize, out_h: usize) -> Option<usize> {
    if out_w == 0 || out_h == 0 {
        return None;
    }
    let row = out_w.checked_mul(4)?;
    let total = row.checked_mul(out_h)?;
    if buf_len == total {
        return Some(row);
    }
    if buf_len.is_multiple_of(out_h) {
        let stride = buf_len / out_h;
        if stride >= row {
            return Some(stride);
        }
    }
    None
}

/// Consume one sample and put the finished frame on screen.
///
/// Two layouts are possible and the reader decides which:
/// CPU buffer: RGB32 rows are copied into the staging texture.
/// DXGI texture (GPU): `CopyResource` straight into the back buffer —
///   the pixel data never leaves the GPU (the mpv-style zero-copy path).
///
/// Returns the path taken, or a human-readable reason to skip the frame;
/// the caller logs each distinct reason once.
///
/// # Safety
/// `sample`, `context`, `staging` and `back_buffer` must be live objects
/// owned by this playback thread.
unsafe fn consume_e3_sample(
    sample: &IMFSample,
    context: &ID3D11DeviceContext,
    staging: &ID3D11Texture2D,
    back_buffer: &ID3D11Texture2D,
    out_w: usize,
    out_h: usize,
    debug_top: bool,
) -> Result<E3Path, String> {
    // The sample carries one buffer: DXGI-backed when MF processed on the
    // GPU, system memory otherwise. GetBufferByIndex is the fallback for
    // readers where ConvertToContiguousBuffer refuses DXGI buffers.
    let buffer = sample
        .ConvertToContiguousBuffer()
        .or_else(|_| sample.GetBufferByIndex(0))
        .map_err(|err| format!("no sample buffer: {}", err.message()))?;

    // ── CPU path: the buffer is lockable ──
    // IMF2DBuffer reports the real pitch and covers bottom-up layouts.
    if let Ok(b2d) = buffer.cast::<IMF2DBuffer>() {
        let mut scanline: *mut u8 = std::ptr::null_mut();
        let mut pitch: i32 = 0;
        // SAFETY: live buffer; out-params initialised.
        if unsafe { b2d.Lock2D(&mut scanline, &mut pitch) }.is_ok() {
            // SAFETY: objects are live on this thread; source bounds follow
            // the IMF2DBuffer contract for the locked region.
            let result = unsafe {
                copy_rgb32_rows_to_staging(
                    context,
                    staging,
                    back_buffer,
                    scanline,
                    pitch as isize,
                    out_w,
                    out_h,
                    debug_top,
                )
            };
            // SAFETY: Lock2D succeeded, so the unlock is paired.
            unsafe {
                let _ = b2d.Unlock2D();
            }
            result?;
            return Ok("cpu-buffer");
        }
    }

    let mut buf_ptr: *mut u8 = std::ptr::null_mut();
    let mut buf_len: u32 = 0;
    // SAFETY: `buf_ptr`/`buf_len` are initialised; Unlock is paired.
    if unsafe { buffer.Lock(&mut buf_ptr, None, Some(&mut buf_len)) }.is_ok() {
        let result = match rgb32_stride(buf_len as usize, out_w, out_h) {
            // SAFETY: the stride covers out_w*4 bytes per row for out_h
            // rows, which is exactly what rgb32_stride verified.
            Some(stride) => unsafe {
                copy_rgb32_rows_to_staging(
                    context,
                    staging,
                    back_buffer,
                    buf_ptr,
                    stride as isize,
                    out_w,
                    out_h,
                    debug_top,
                )
            },
            None => Err(format!(
                "RGB32 buffer too small: {buf_len} < {} (surface {out_w}x{out_h})",
                out_w * out_h * 4
            )),
        };
        // SAFETY: Lock succeeded, so the unlock is paired.
        unsafe {
            let _ = buffer.Unlock();
        }
        result?;
        return Ok("cpu-buffer");
    }

    // ── GPU path: not CPU-readable → DXGI texture ──
    e3_consume_gpu(&buffer, context, back_buffer, out_w, out_h)
}

/// Copy one RGB32 frame into the staging texture and onto the back buffer.
///
/// `src_row0` points at the top row of the image; `src_pitch` is the byte
/// distance to the next row and may be negative (bottom-up layout, per the
/// IMF2DBuffer convention where `scanline0` is the first image row).
///
/// # Safety
/// `src_row0` must reference `out_h` rows of `out_w * 4` bytes at
/// `src_pitch`; the D3D objects must be live on this thread.
#[allow(clippy::too_many_arguments)]
unsafe fn copy_rgb32_rows_to_staging(
    context: &ID3D11DeviceContext,
    staging: &ID3D11Texture2D,
    back_buffer: &ID3D11Texture2D,
    src_row0: *const u8,
    src_pitch: isize,
    out_w: usize,
    out_h: usize,
    debug_top: bool,
) -> Result<(), String> {
    let mut mapped: D3D11_MAPPED_SUBRESOURCE = unsafe { std::mem::zeroed() };
    // SAFETY: staging texture has CPU write access.
    if unsafe { context.Map(staging, 0, D3D11_MAP_WRITE, 0, Some(&mut mapped)) }.is_err() {
        return Err("Map(staging) failed".to_string());
    }
    let dst = mapped.pData as *mut u8;
    let row_pitch = mapped.RowPitch as usize;
    // SAFETY: rows stay inside the mapped staging texture; the source size
    // was verified by the caller (rgb32_stride / IMF2DBuffer contract).
    unsafe {
        for oy in 0..out_h {
            let src_row = src_row0.offset(oy as isize * src_pitch);
            std::ptr::copy_nonoverlapping(src_row, dst.add(oy * row_pitch), out_w * 4);
        }
        if debug_top {
            for oy in 0..16.min(out_h) {
                let row = dst.add(oy * row_pitch);
                for ox in 0..out_w {
                    let off = ox * 4;
                    *row.add(off) = 0;
                    *row.add(off + 1) = 0;
                    *row.add(off + 2) = 255;
                    *row.add(off + 3) = 255;
                }
            }
        }
        context.Unmap(staging, 0);
    }
    // staging → back buffer (the DEFAULT back buffer cannot be mapped).
    // SAFETY: both textures are alive and have the same size/format.
    unsafe {
        context.CopyResource(back_buffer, staging);
    }
    Ok(())
}

/// GPU path: forward the sample's DXGI texture straight to the back
/// buffer. The pixels never leave the GPU (mpv-style zero-copy present).
///
/// # Safety
/// All objects must be live on this playback thread.
unsafe fn e3_consume_gpu(
    buffer: &IMFMediaBuffer,
    context: &ID3D11DeviceContext,
    back_buffer: &ID3D11Texture2D,
    out_w: usize,
    out_h: usize,
) -> Result<E3Path, String> {
    let dxgi_buf = buffer
        .cast::<IMFDXGIBuffer>()
        .map_err(|_| "sample buffer is neither lockable nor a DXGI buffer".to_string())?;
    let mut raw: *mut core::ffi::c_void = std::ptr::null_mut();
    // SAFETY: `dxgi_buf` is live; the IID is the documented one; the out
    // param starts null and the call AddRefs the returned interface.
    unsafe { dxgi_buf.GetResource(&<ID3D11Texture2D as Interface>::IID, &mut raw) }
        .map_err(|err| format!("GetResource(ID3D11Texture2D): {}", err.message()))?;
    // SAFETY: `raw` was AddRef'd by GetResource and is ours to release.
    let texture = unsafe { ID3D11Texture2D::from_raw(raw) };

    let mut desc: D3D11_TEXTURE2D_DESC = unsafe { std::mem::zeroed() };
    // SAFETY: live texture; out-param initialised.
    unsafe { texture.GetDesc(&mut desc) };
    // One-shot evidence line for the RND doc (which format MF really
    // hands us on this driver).
    if !E3_GPU_DESC_LOGGED.swap(true, Ordering::Relaxed) {
        eprintln!(
            "cascade: SourceReader E3: GPU texture {}x{} {:?}",
            desc.Width, desc.Height, desc.Format
        );
    }
    if desc.Width as usize != out_w || desc.Height as usize != out_h {
        return Err(format!(
            "E3 texture is {}x{}, surface is {out_w}x{out_h}",
            desc.Width, desc.Height
        ));
    }
    // MF tags its RGB32 output as B8G8R8X8_UNORM (measured: DXGI_FORMAT(88);
    // the X byte carries no alpha). Accept both BGRA8 families — same
    // 4-byte B,G,R,(A|X) layout — and the swapchain matches the family at
    // creation time so the copy below is identical-format.
    let bgra8 = matches!(
        desc.Format,
        DXGI_FORMAT_B8G8R8A8_TYPELESS
            | DXGI_FORMAT_B8G8R8A8_UNORM
            | DXGI_FORMAT_B8G8R8A8_UNORM_SRGB
            | DXGI_FORMAT_B8G8R8X8_TYPELESS
            | DXGI_FORMAT_B8G8R8X8_UNORM
            | DXGI_FORMAT_B8G8R8X8_UNORM_SRGB
    );
    if !bgra8 {
        return Err(format!(
            "E3 texture format is {desc:?}, need the B8G8R8A8/B8G8R8X8 type groups"
        ));
    }
    // SAFETY: same size and format (checked above); both are alive.
    unsafe {
        context.CopyResource(back_buffer, &texture);
    }
    Ok("gpu-texture")
}

/// Path label of the consume mode (E3Path value, one-shot logged).
const E3B_PATH: E3Path = "e3b-video-processor";

/// Why one frame could not be consumed on the GPU.
pub(crate) enum E3bFrame {
    /// The sample buffer is not a DXGI texture (software decode fallback):
    /// the caller should use the NV12 CPU path for this frame instead.
    NotGpu,
    /// Any other failure — skip the frame; the caller logs it once.
    Hard(String),
}

impl E3bFrame {
    /// One-line reason for the log/fallback counter (engine path).
    pub(crate) fn why(&self) -> String {
        match self {
            E3bFrame::NotGpu => "sample buffer is not a DXGI texture".to_string(),
            E3bFrame::Hard(msg) => msg.clone(),
        }
    }
}

/// Lazily-built video processor for one input size.
struct E3bCore {
    in_w: u32,
    in_h: u32,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    /// Video-processor output view of the back buffer (VideoProcessorBlt
    /// target; an ordinary RTV is not accepted by the API).
    out_view: ID3D11VideoProcessorOutputView,
}

/// our own D3D11 video processor — NV12 → BGRA8 conversion and the
/// resize, both on the GPU, straight into the swapchain back buffer.
///
/// This is the mpv-style architecture built from plain Microsoft APIs: MF
/// only decodes (DXVA via the DXGI device manager); MF's own video
/// processor (XVP) is never created — it crashes the Intel driver on this
/// machine (measured 4/4, Appendix D).
///
/// The enumerator + processor are built lazily from the FIRST frame's real
/// texture description, so a macroblock-padded or otherwise unexpected
/// decoder surface size cannot desynchronize the pipeline.
///
/// All methods run on the playback thread; no COM interface here crosses
/// a thread boundary.
pub(crate) struct E3bPipeline {
    /// The video-device view of the D3D11 device: `CreateVideoProcessor*`
    /// lives here, not on `ID3D11Device`.
    video_device: ID3D11VideoDevice,
    video_ctx: ID3D11VideoContext,
    /// Cloned so the output view can be (re)built per input size.
    back_buffer: ID3D11Texture2D,
    frame_rate: (u32, u32),
    out_size: (u32, u32),
    /// Colour tags parsed from the SPS/VUI (`sequence_header_tags`): the
    /// matrix + range the video processor must be told
    ///
    colour: ColourTags,
    /// How the video is fitted into `out_size` (
    /// Defaults to the full-rect behaviour Tier 2 has always had
    /// (frozen); the engine backend calls [`E3bPipeline::set_fit`].
    fit: Fit,
    core: Option<E3bCore>,
    /// Input views cached per texture pointer (MF pools a handful).
    views: Vec<(usize, ID3D11VideoProcessorInputView)>,
}

impl E3bPipeline {
    /// Create the fixed part: video context + video device. A failure here
    /// fails the whole attempt (the caller reopens the reader without
    /// the D3D manager).
    pub(crate) fn create(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        back_buffer: &ID3D11Texture2D,
        frame_rate: (u32, u32),
        out_size: (u32, u32),
        colour: ColourTags,
    ) -> Result<Self, String> {
        let video_ctx = context
            .cast::<ID3D11VideoContext>()
            .map_err(|err| format!("ID3D11VideoContext: {}", err.message()))?;
        let video_device = device
            .cast::<ID3D11VideoDevice>()
            .map_err(|err| format!("ID3D11VideoDevice: {}", err.message()))?;
        Ok(Self {
            video_device,
            video_ctx,
            back_buffer: back_buffer.clone(),
            frame_rate,
            out_size,
            colour,
            // Full-rect src→dst: exactly what Tier 2 has always done (the
            // engine backend overrides this via `set_fit` before blitting).
            fit: Fit::Stretch,
            core: None,
            views: Vec::new(),
        })
    }

    /// Choose the fit mode. Takes effect at
    /// the next (re)build of the processor core; dropping the core here
    /// makes that unconditional even if a frame was already blitted.
    pub(crate) fn set_fit(&mut self, fit: Fit) {
        self.fit = fit;
        self.core = None;
        self.views.clear();
    }

    /// (Re)build enumerator + processor for an input texture size.
    /// Fails the frame (and, during the first consume, logs once) — the
    /// CPU fallback keeps the video alive either way.
    fn build_core(&mut self, in_w: u32, in_h: u32) -> Result<(), String> {
        let rate = DXGI_RATIONAL {
            Numerator: self.frame_rate.0,
            Denominator: self.frame_rate.1.max(1),
        };
        let (out_w, out_h) = self.out_size;
        let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate,
            InputWidth: in_w,
            InputHeight: in_h,
            OutputFrameRate: rate,
            OutputWidth: out_w,
            OutputHeight: out_h,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };
        // SAFETY: live video device; the descriptor is fully initialised.
        let enumerator = unsafe { self.video_device.CreateVideoProcessorEnumerator(&content) }
            .map_err(|err| format!("CreateVideoProcessorEnumerator: {}", err.message()))?;
        // NV12 must be an accepted INPUT format for this content combo.
        // SAFETY: live enumerator; DXGI_FORMAT_NV12 is a plain enum value.
        let input_caps = unsafe { enumerator.CheckVideoProcessorFormat(DXGI_FORMAT_NV12) }
            .map_err(|err| format!("CheckVideoProcessorFormat(NV12): {}", err.message()))?;
        if input_caps == 0 {
            return Err("NV12 is not accepted as video processor input".to_string());
        }
        let mut caps: D3D11_VIDEO_PROCESSOR_CAPS = unsafe { std::mem::zeroed() };
        // SAFETY: live enumerator; the out-param is initialised.
        unsafe { enumerator.GetVideoProcessorCaps(&mut caps) }
            .map_err(|err| format!("GetVideoProcessorCaps: {}", err.message()))?;
        if caps.MaxInputStreams < 1 {
            return Err(format!(
                "video processor supports {} input streams",
                caps.MaxInputStreams
            ));
        }
        if caps.RateConversionCapsCount < 1 {
            return Err("video processor has no rate conversion mode".to_string());
        }
        // SAFETY: index 0 exists (checked above); live video device.
        let processor = unsafe { self.video_device.CreateVideoProcessor(&enumerator, 0) }
            .map_err(|err| format!("CreateVideoProcessor: {}", err.message()))?;
        // Output view of the back buffer: the VideoProcessorBlt target
        // (the API rejects an ordinary render target view).
        let out_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
            },
        };
        let mut out_view: Option<ID3D11VideoProcessorOutputView> = None;
        // SAFETY: live video device, back buffer and enumerator; the
        // descriptor matches the back buffer (single 2D texture, mip 0).
        unsafe {
            self.video_device.CreateVideoProcessorOutputView(
                &self.back_buffer,
                &enumerator,
                &out_desc,
                Some(&mut out_view),
            )
        }
        .map_err(|err| format!("CreateVideoProcessorOutputView: {}", err.message()))?;
        let out_view = out_view
            .ok_or_else(|| "CreateVideoProcessorOutputView returned no object".to_string())?;
        // Colour space: matrix + range from the SPS/VUI parser for THIS
        // stream — never assumed BT.709 + limited.
        // The BGRA back buffer takes full-range RGB in every case.
        let color = D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
            _bitfield: color_space_bits(self.colour.matrix, self.colour.range),
        };
        // SAFETY: stream 0 belongs to this processor; the call is void.
        unsafe {
            self.video_ctx
                .VideoProcessorSetStreamColorSpace(&processor, 0, &color)
        };
        // Explicit source/destination rects (that is the scaling), from the
        // pure fit math (`fit.rs`): full frame by default, letterboxed or
        // cropped per the requested mode : never distort).
        let (src, dst) = fit_rects(self.fit, (in_w, in_h), (out_w, out_h));
        // SAFETY: live processor; rects fit in i32 (video/surface sizes).
        unsafe {
            self.video_ctx
                .VideoProcessorSetStreamSourceRect(&processor, 0, true, Some(&src));
            self.video_ctx
                .VideoProcessorSetStreamDestRect(&processor, 0, true, Some(&dst));
        }
        // Input views were enumerator-bound: a new size drops them.
        self.views.clear();
        self.core = Some(E3bCore {
            in_w,
            in_h,
            enumerator,
            processor,
            out_view,
        });
        eprintln!("cascade: SourceReader E3b: video processor {in_w}x{in_h} -> {out_w}x{out_h}");
        Ok(())
    }

    /// Consume one sample: get its NV12 DXGI texture, convert + scale it
    /// into the back buffer via our video processor. The pixel data never
    /// touches the CPU (mpv-style zero-copy present).
    fn consume(&mut self, sample: &IMFSample) -> Result<(), E3bFrame> {
        // SAFETY: `sample` is live on this thread; both calls return an
        // AddRef'd buffer (or an error) that we release on drop.
        let buffer = unsafe { sample.ConvertToContiguousBuffer() }
            .or_else(|_| unsafe { sample.GetBufferByIndex(0) })
            .map_err(|err| E3bFrame::Hard(format!("no sample buffer: {}", err.message())))?;
        let dxgi_buf = buffer
            .cast::<IMFDXGIBuffer>()
            .map_err(|_| E3bFrame::NotGpu)?;
        let mut raw: *mut core::ffi::c_void = std::ptr::null_mut();
        // SAFETY: live buffer; the documented IID; the out-param starts
        // null and the call AddRefs the returned interface.
        unsafe { dxgi_buf.GetResource(&<ID3D11Texture2D as Interface>::IID, &mut raw) }.map_err(
            |err| E3bFrame::Hard(format!("GetResource(ID3D11Texture2D): {}", err.message())),
        )?;
        // SAFETY: `raw` was AddRef'd by GetResource and is ours to release.
        let texture = unsafe { ID3D11Texture2D::from_raw(raw) };
        let mut desc: D3D11_TEXTURE2D_DESC = unsafe { std::mem::zeroed() };
        // SAFETY: live texture; the out-param is initialised.
        unsafe { texture.GetDesc(&mut desc) };
        // One-shot evidence for the RND doc: what the decoder really hands us.
        if !E3_GPU_DESC_LOGGED.swap(true, Ordering::Relaxed) {
            eprintln!(
                "cascade: SourceReader E3b: decoded GPU texture {}x{} {:?}",
                desc.Width, desc.Height, desc.Format
            );
        }
        self.blit_texture(&texture)
    }

    /// Blit one NV12 texture into the back buffer: (re)build the video
    /// processor for the texture's size, then `VideoProcessorBlt` with the
    /// explicit colour space and the fit rects. Shared by the Tier 2 sample
    /// path and the engine's `TransferVideoFrame` destination
    /// both callers hand over a live NV12
    /// texture on this thread.
    pub(crate) fn blit_texture(&mut self, texture: &ID3D11Texture2D) -> Result<(), E3bFrame> {
        let mut desc: D3D11_TEXTURE2D_DESC = unsafe { std::mem::zeroed() };
        // SAFETY: live texture; the out-param is initialised.
        unsafe { texture.GetDesc(&mut desc) };
        if desc.Format != DXGI_FORMAT_NV12 {
            return Err(E3bFrame::Hard(format!(
                "input texture format is {:?}, need DXGI_FORMAT_NV12",
                desc.Format
            )));
        }

        // (Re)build the processor when the decoder surface size is new.
        let needs_core = match &self.core {
            Some(core) => core.in_w != desc.Width || core.in_h != desc.Height,
            None => true,
        };
        if needs_core {
            self.build_core(desc.Width, desc.Height)
                .map_err(E3bFrame::Hard)?;
        }

        // Input view, cached per texture pointer (MF pools a few).
        let key = texture.as_raw() as usize;
        let cached = self
            .views
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v.clone());
        let view = match cached {
            Some(view) => view,
            None => {
                let view_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                    // 0 = "use the DXGI format the resource was created
                    // with" (MS docs for this struct); NV12 was verified
                    // above. A DXGI_FORMAT value here is NOT a FOURCC.
                    FourCC: 0,
                    ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                    Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                        Texture2D: D3D11_TEX2D_VPIV {
                            MipSlice: 0,
                            ArraySlice: 0,
                        },
                    },
                };
                let enumerator = self
                    .core
                    .as_ref()
                    .ok_or_else(|| E3bFrame::Hard("video processor missing".to_string()))?
                    .enumerator
                    .clone();
                let mut view: Option<ID3D11VideoProcessorInputView> = None;
                // SAFETY: live video device/texture/enumerator; the desc
                // matches the texture (NV12, checked above).
                unsafe {
                    self.video_device.CreateVideoProcessorInputView(
                        texture,
                        &enumerator,
                        &view_desc,
                        Some(&mut view),
                    )
                }
                .map_err(|err| {
                    E3bFrame::Hard(format!(
                        "CreateVideoProcessorInputView: {} ({:#010X})",
                        err.message(),
                        err.code().0 as u32
                    ))
                })?;
                let view =
                    view.ok_or_else(|| E3bFrame::Hard("input view object missing".to_string()))?;
                if self.views.len() >= 16 {
                    // Defensive bound: the decoder pool is far smaller;
                    // drop stale entries instead of growing without limit.
                    self.views.clear();
                }
                self.views.push((key, view.clone()));
                view
            }
        };

        let core = self
            .core
            .as_ref()
            .ok_or_else(|| E3bFrame::Hard("video processor missing".to_string()))?;
        let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: BOOL::from(true),
            OutputIndex: 0,
            InputFrameOrField: 0,
            PastFrames: 0,
            FutureFrames: 0,
            ppPastSurfaces: std::ptr::null_mut(),
            pInputSurface: ManuallyDrop::new(Some(view)),
            ppFutureSurfaces: std::ptr::null_mut(),
            ppPastSurfacesRight: std::ptr::null_mut(),
            pInputSurfaceRight: ManuallyDrop::new(None),
            ppFutureSurfacesRight: std::ptr::null_mut(),
        };
        // SAFETY: all objects live on this thread; one enabled stream with
        // exactly one input surface (constructed above).
        let blt = unsafe {
            self.video_ctx.VideoProcessorBlt(
                &core.processor,
                &core.out_view,
                0,
                std::slice::from_ref(&stream),
            )
        };
        // SAFETY: the reference moved into the stream struct is ours to
        // release after the (synchronous) Blt call.
        let input_ref = unsafe { ManuallyDrop::take(&mut stream.pInputSurface) };
        drop(input_ref);
        blt.map_err(|err| E3bFrame::Hard(format!("VideoProcessorBlt: {}", err.message())))?;
        Ok(())
    }
}

/// Colour tags for the loaded stream: parse `MF_MT_MPEG_SEQUENCE_HEADER`
/// (Annex-B / avcC SPS+PPS) with our own SPS/VUI parser
/// Media Foundation ignores H.264 VUI colour
/// flags on build 26200.9711 and the canonical
/// gate asset carries no `colr` box, so the bitstream is the only
/// trustworthy matrix source. Any failure — attribute absent, blob
/// unreadable, malformed SPS — yields [`ColourTags::default`] (BT.709 +
/// limited range), identical to the pre-fix hardcode, so a missing blob can
/// never regress below today's behaviour.
pub(crate) fn sequence_header_tags(reader: &IMFSourceReader) -> ColourTags {
    use windows::Win32::Media::MediaFoundation::MF_MT_MPEG_SEQUENCE_HEADER;
    // SAFETY: documented stream/type indices for a single-video-stream
    // file; `mt` is a live media type owned by this call's return value.
    let mt = match unsafe { reader.GetNativeMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM, 0) } {
        Ok(mt) => mt,
        Err(_) => return ColourTags::default(),
    };
    // SAFETY: out-param filled on success; the attribute key is documented.
    let total = match unsafe { mt.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER) } {
        Ok(total) => total as usize,
        Err(_) => return ColourTags::default(),
    };
    // Sequence headers are a few dozen bytes; anything outside a sane
    // window means the attribute is not what we think it is.
    if total == 0 || total > 64 * 1024 {
        return ColourTags::default();
    }
    let mut buf = vec![0u8; total];
    // SAFETY: buffer length == the size MF just reported.
    match unsafe { mt.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut buf, None) } {
        Ok(()) => colour_tags(&buf),
        Err(_) => ColourTags::default(),
    }
}

/// Bitfield for `D3D11_VIDEO_PROCESSOR_COLOR_SPACE` (LSB-first, d3d11.h;
/// MS docs verified 2026-11-04):
/// bit0 Usage=0 (playback), bit1 RGB_Range=0 (full-range RGB output),
/// bit2 YCbCr_Matrix (0 = BT.601, 1 = BT.709), bit3 YCbCr_xvYCC=0
/// (conventional), bits4-5 Nominal_Range (0 = full range, 1 = 16-235
/// limited). Matrix and nominal range come from the SPS/VUI parser
/// instead of the old hardcoded
/// "HD H.264 = BT.709 + limited" assumption.
fn color_space_bits(matrix: VideoMatrix, range: VideoRange) -> u32 {
    let mut bits = 0u32; // Usage = playback, RGB_Range = full, xvYCC off
    if matrix == VideoMatrix::Bt709 {
        bits |= 0b0100; // YCbCr_Matrix = BT.709
    }
    if range == VideoRange::Limited {
        bits |= 0b1_0000; // Nominal_Range = 16-235
    }
    bits
}

/// Create D3D11 device + context.
///
/// `video_support` adds `D3D11_CREATE_DEVICE_VIDEO_SUPPORT`: MF's
/// video processor runs on this device and the flag is the documented
/// switch for video-processing features.
pub(crate) fn create_d3d11(
    video_support: bool,
) -> BackendResult<(ID3D11Device, ID3D11DeviceContext)> {
    let mut device: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;
    let levels = [D3D_FEATURE_LEVEL_11_0];
    let mut flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT;
    if video_support {
        flags |= D3D11_CREATE_DEVICE_VIDEO_SUPPORT;
    }
    // SAFETY: standard device creation with initialised out-params.
    unsafe {
        D3D11CreateDevice(
            None::<&IDXGIAdapter>,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            flags,
            Some(&levels),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
        .map_err(|err| BackendError::Mf(format!("D3D11CreateDevice: {}", err.message())))?;
    }
    let device = device.ok_or_else(|| BackendError::Mf("no D3D11 device".into()))?;
    let context = context.ok_or_else(|| BackendError::Mf("no D3D11 context".into()))?;
    log_adapter(&device);
    Ok((device, context))
}

/// Create a DXGI flip swapchain on `hwnd`.
///
/// `format` is `B8G8R8A8_UNORM` normally and `B8G8R8X8_UNORM` under:
/// MF tags its RGB32 output as X8 (measured `DXGI_FORMAT(88)`), so matching
/// it makes the consume `CopyResource` an identical-format copy.
pub(crate) fn create_swapchain(
    device: &ID3D11Device,
    hwnd: isize,
    width: u32,
    height: u32,
    format: DXGI_FORMAT,
) -> BackendResult<IDXGISwapChain1> {
    // SAFETY: plain casts along the DXGI object chain.
    let dxgi_device: IDXGIDevice = device
        .cast()
        .map_err(|err| BackendError::Mf(format!("ID3D11Device→IDXGIDevice: {}", err.message())))?;
    let adapter = unsafe {
        dxgi_device
            .GetAdapter()
            .map_err(|err| BackendError::Mf(format!("GetAdapter: {}", err.message())))?
    };
    let factory: IDXGIFactory2 = unsafe {
        adapter
            .GetParent()
            .map_err(|err| BackendError::Mf(format!("GetParent factory: {}", err.message())))?
    };

    let desc = DXGI_SWAP_CHAIN_DESC1 {
        Width: width,
        Height: height,
        Format: format,
        Stereo: false.into(),
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
        BufferCount: 2,
        Scaling: DXGI_SCALING_STRETCH,
        SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
        AlphaMode: DXGI_ALPHA_MODE_IGNORE,
        Flags: 0,
    };
    // SAFETY: the host window lives as long as this thread (detach joins
    // before the host is destroyed).
    let swapchain: IDXGISwapChain1 = unsafe {
        factory
            .CreateSwapChainForHwnd(
                device,
                HWND(hwnd as *mut _),
                &desc,
                None,
                None::<&IDXGIOutput>,
            )
            .map_err(|err| BackendError::Mf(format!("CreateSwapChainForHwnd: {}", err.message())))?
    };
    Ok(swapchain)
}

/// CPU-writable staging texture, same size/format as the swap chain buffer.
fn create_staging(
    device: &ID3D11Device,
    width: u32,
    height: u32,
    format: DXGI_FORMAT,
) -> BackendResult<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
        MiscFlags: 0,
    };
    let mut texture: Option<ID3D11Texture2D> = None;
    // SAFETY: `desc` is valid; out-param starts as None.
    unsafe {
        device
            .CreateTexture2D(&desc, None, Some(&mut texture))
            .map_err(|err| {
                BackendError::Mf(format!("CreateTexture2D(staging): {}", err.message()))
            })?;
    }
    texture.ok_or_else(|| BackendError::Mf("no staging texture".into()))
}

/// Allocated luma height of a locked NV12 buffer, in rows.
///
/// Decoders pad the luma plane to a multiple of 16 (macroblock) rows: 1080
/// content is allocated as 1088 rows and the chroma plane starts after row
/// 1088. Assuming the display height shifted the chroma plane by 8 rows and
/// rendered the first ~11 output rows from luma padding (solid green band,
/// measured 2026-11-04).
///
/// Returns `video_h` when the buffer size does not divide into whole NV12
/// frames at `video_w` (unknown layout -> assume tight packing).
/// `pub(crate)`: the thumbnail reader derives its UV offset the same way.
pub(crate) fn nv12_alloc_height(buf_len: usize, video_w: usize, video_h: usize) -> usize {
    let half_row = video_w * 3 / 2;
    let alloc_h = if half_row > 0 && buf_len.is_multiple_of(half_row) {
        buf_len / half_row
    } else {
        video_h
    };
    alloc_h.max(video_h)
}

/// Convert NV12 (Y + interleaved UV) to BGR8 with nearest-neighbour scaling
/// from `video_w x video_h` into `out_w x out_h`.
///
/// `dst` is a mapped staging texture whose `row_pitch` may exceed the stride.
/// `uv_plane_offset` is where the chroma plane starts in `src`: the decoder
/// allocates the luma plane at macroblock-padded height (1080 -> 1088), so
/// it must be computed from the real buffer size, not `video_w * video_h`.
///
/// # Safety
/// `src` must hold at least `uv_plane_offset + video_w * video_h / 2`
/// readable bytes and `dst` must hold `out_h` rows of `row_pitch` bytes.
/// `pub(crate)`: the thumbnail reader (thumbnail.rs) reuses this
/// unit-tested converter.
#[allow(clippy::too_many_arguments)]
pub(crate) unsafe fn convert_nv12_to_bgr8(
    src: *const u8,
    dst: *mut u8,
    row_pitch: usize,
    uv_plane_offset: usize,
    video_w: usize,
    video_h: usize,
    out_w: usize,
    out_h: usize,
    colour: ColourTags,
) {
    if video_w == 0 || video_h == 0 || out_w == 0 || out_h == 0 {
        return;
    }
    let uv_plane = src.add(uv_plane_offset);
    // Matrix + range come from the SPS/VUI parser;
    // this converter used to hardcode BT.601 limited while the GPU path
    // hardcoded BT.709 limited — both now read the same ColourTags.
    let (y_offset, luma_k, kr, kg_r, kg_b, kb) = yuv_to_rgb_coeffs(colour);

    for oy in 0..out_h {
        let sy = oy * video_h / out_h;
        let y_row = sy * video_w;
        let uv_row = (sy / 2) * (video_w / 2);
        let dst_row = oy * row_pitch;

        for ox in 0..out_w {
            let sx = ox * video_w / out_w;

            // SAFETY: sx < video_w, sy < video_h and the caller checked
            // the buffer size, so both planes stay in bounds.
            let c = *src.add(y_row + sx) as i32 - y_offset;
            let uv_offset = (uv_row + sx / 2) * 2;
            let d = *uv_plane.add(uv_offset) as i32 - 128;
            let e = *uv_plane.add(uv_offset + 1) as i32 - 128;

            let r = ((luma_k * c + kr * e + 128) >> 8).clamp(0, 255);
            let g = ((luma_k * c - kg_r * d - kg_b * e + 128) >> 8).clamp(0, 255);
            let b = ((luma_k * c + kb * d + 128) >> 8).clamp(0, 255);

            let off = dst_row + ox * 4;
            *dst.add(off) = b as u8;
            *dst.add(off + 1) = g as u8;
            *dst.add(off + 2) = r as u8;
            *dst.add(off + 3) = 0xFF;
        }
    }
}

/// Integer YUV→RGB coefficients for the stream's colour tags, as
/// `(y_offset, luma_k, kr, kg_r, kg_b, kb)` with the output computed as
/// `(luma_k * (Y - y_offset) [+/-] w * (Cb/Cr - 128) + 128) >> 8`.
/// Standard 8-bit forms (ITU-R BT.601 / BT.709):
///
/// | matrix | range | y_offset | luma_k | kr | kg_r | kg_b | kb |
/// |--------|---------|----------|--------|-----|------|------|-----|
/// | 601 | limited | 16 | 298 | 409 | 100 | 208 | 516 |
/// | 601 | full | 0 | 256 | 359 | 88 | 183 | 454 |
/// | 709 | limited | 16 | 298 | 459 | 55 | 136 | 541 |
/// | 709 | full | 0 | 256 | 403 | 48 | 120 | 475 |
///
/// The full-range chroma weights are the limited-range ones scaled by
/// 224/255 (the limited→full chroma amplitude ratio), rounded to nearest —
/// e.g. 409 · 224/255 = 359.4 → 359.
fn yuv_to_rgb_coeffs(colour: ColourTags) -> (i32, i32, i32, i32, i32, i32) {
    let (y_offset, luma_k) = match colour.range {
        VideoRange::Limited => (16, 298),
        VideoRange::Full => (0, 256),
    };
    let (kr, kg_r, kg_b, kb) = match (colour.matrix, colour.range) {
        (VideoMatrix::Bt601, VideoRange::Limited) => (409, 100, 208, 516),
        (VideoMatrix::Bt601, VideoRange::Full) => (359, 88, 183, 454),
        (VideoMatrix::Bt709, VideoRange::Limited) => (459, 55, 136, 541),
        (VideoMatrix::Bt709, VideoRange::Full) => (403, 48, 120, 475),
    };
    (y_offset, luma_k, kr, kg_r, kg_b, kb)
}

/// `Path` → `file:///` URL for `MFCreateSourceReaderFromURL` (and
/// `IMFMediaEngine::SetSource`).
///
/// ASCII specials are percent-encoded; **non-ASCII travels raw**. Measured
/// on the target machine (Windows 11 build 26200, 2026-11-10): MF does
/// NOT decode percent-encoded UTF-8 as UTF-8 — a URL with `%%92%BC…`
/// fails with `0x80070002` ("cannot find the file") while the same URL
/// with the raw characters (`蒼のワルツ…`, a bare path) opens fine
/// (`mf-url-variants.ps1`, vs ). The URL string reaches MF as
/// UTF-16, so the raw characters survive intact.
pub(crate) fn file_url(path: &Path) -> String {
    let absolute = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let mut raw = absolute.to_string_lossy().replace('\\', "/");
    if let Some(rest) = raw.strip_prefix("//?/UNC/") {
        raw = format!("//{rest}");
    } else if let Some(rest) = raw.strip_prefix("//?/") {
        raw = rest.to_string();
    }
    let mut url = if raw.starts_with("//") {
        String::from("file:")
    } else {
        String::from("file:///")
    };
    for c in raw.chars() {
        match c {
            'A'..='Z'
            | 'a'..='z'
            | '0'..='9'
            | '-'
            | '_'
            | '.'
            | '~'
            | '/'
            | ':'
            | '!'
            | '$'
            | '&'
            | '\''
            | '('
            | ')'
            | '*'
            | '+'
            | ','
            | ';'
            | '=' => url.push(c),
            // ASCII outside the safe set (space, `%`, `#`, `?`, …) keeps
            // being percent-encoded — that part MF does decode correctly.
            c if c.is_ascii() => url.push_str(&format!("%{:02X}", c as u32)),
            // Everything else (CJK, accents, …) goes through raw.
            c => url.push(c),
        }
    }
    url
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_url_percent_encodes_spaces() {
        // canonicalize fails for a path that does not exist; the fallback
        // must still produce a valid URL.
        assert_eq!(
            file_url(Path::new(r"C:\videos\my clip.mp4")),
            "file:///C:/videos/my%20clip.mp4"
        );
    }

    #[test]
    fn file_url_keeps_plain_paths() {
        assert_eq!(
            file_url(Path::new(r"C:\videos\clip.mp4")),
            "file:///C:/videos/clip.mp4"
        );
    }

    #[test]
    fn file_url_keeps_non_ascii_raw() {
        // MF does NOT decode percent-encoded UTF-8 (measured 2026-11-10,
        // Windows 11 build 26200: `%%92%BC…` -> 0x80070002, raw `蒼…` ->
        // S_OK — see the `file_url` doc comment). The GUI will hand
        // us arbitrary Unicode paths, so non-ASCII must travel raw while
        // ASCII specials stay percent-encoded.
        assert_eq!(
            file_url(Path::new(r"C:\videos\あ.mp4")),
            "file:///C:/videos/あ.mp4"
        );
        // Mixed: ASCII space encoded, CJK characters raw.
        assert_eq!(
            file_url(Path::new(r"C:\videos\蒼のワルツ - Eve MV.mp4")),
            "file:///C:/videos/蒼のワルツ%20-%20Eve%20MV.mp4"
        );
    }

    #[test]
    fn nv12_alloc_height_detects_macroblock_padding() {
        // 1080 content allocated as 1088 luma rows (16-row padding).
        assert_eq!(nv12_alloc_height(1920 * 1088 * 3 / 2, 1920, 1080), 1088);
        // Tightly packed buffer keeps the display height.
        assert_eq!(nv12_alloc_height(1920 * 1080 * 3 / 2, 1920, 1080), 1080);
        // 720 content allocated as 728 rows.
        assert_eq!(nv12_alloc_height(1280 * 728 * 3 / 2, 1280, 720), 728);
    }

    #[test]
    fn nv12_alloc_height_falls_back_on_unknown_layout() {
        // Size does not divide into whole frames -> assume tight packing.
        assert_eq!(nv12_alloc_height(1920 * 1080 * 3 / 2 + 1, 1920, 1080), 1080);
        // Never smaller than the display height.
        assert_eq!(nv12_alloc_height(0, 1920, 1080), 1080);
    }

    #[test]
    fn rgb32_stride_tight_buffer() {
        // Exact fit: stride is the row size.
        assert_eq!(rgb32_stride(1366 * 768 * 4, 1366, 768), Some(1366 * 4));
    }

    #[test]
    fn rgb32_stride_padded_rows() {
        // Rows padded to 8-byte alignment: 1366*4 = 5464 -> 5472.
        assert_eq!(rgb32_stride(5472 * 768, 1366, 768), Some(5472));
    }

    #[test]
    fn rgb32_stride_rejects_short_or_degenerate_buffers() {
        // Too small for one row per output row.
        assert_eq!(rgb32_stride(1366 * 4 * 768 - 1, 1366, 768), None);
        assert_eq!(rgb32_stride(100, 1366, 768), None);
        // Degenerate surfaces never produce a stride.
        assert_eq!(rgb32_stride(0, 0, 768), None);
        assert_eq!(rgb32_stride(0, 1366, 0), None);
    }

    #[test]
    fn frame_rate_packs_high_dword_first() {
        // MF_MT_FRAME_RATE packs numerator in the high dword.
        assert_eq!(frame_rate_from_u64((60 << 32) | 1), (60, 1));
        assert_eq!(frame_rate_from_u64((24_000 << 32) | 1_001), (24_000, 1_001));
        // Degenerate values pass through; the caller substitutes a default.
        assert_eq!(frame_rate_from_u64(0), (0, 0));
    }

    #[test]
    fn color_space_bits_follow_colour_tags() {
        // The pre-fix hardcode was 0x14: (BT.709, limited) must keep it —
        // VUI-less content behaves exactly as before (no regression).
        assert_eq!(
            color_space_bits(VideoMatrix::Bt709, VideoRange::Limited),
            0x14
        );
        // The other three combinations, from the SPS/VUI parser.
        assert_eq!(
            color_space_bits(VideoMatrix::Bt601, VideoRange::Limited),
            0x10
        );
        assert_eq!(color_space_bits(VideoMatrix::Bt709, VideoRange::Full), 0x04);
        assert_eq!(color_space_bits(VideoMatrix::Bt601, VideoRange::Full), 0x00);
        // Field placement (d3d11.h, MS docs verified 2026-11-04).
        let bits = color_space_bits(VideoMatrix::Bt709, VideoRange::Limited);
        assert_eq!(bits & 0b1, 0); // Usage = playback
        assert_eq!(bits & 0b10, 0); // RGB_Range = full
        assert_eq!(bits >> 2 & 0b1, 1); // YCbCr_Matrix = BT.709
        assert_eq!(bits >> 3 & 0b1, 0); // YCbCr_xvYCC = conventional
        assert_eq!(bits >> 4 & 0b11, 0b01); // Nominal_Range = 16-235
        assert_eq!(
            color_space_bits(VideoMatrix::Bt601, VideoRange::Full) >> 4 & 0b11,
            0b00
        );
        assert_eq!(
            color_space_bits(VideoMatrix::Bt601, VideoRange::Full) >> 2 & 0b1,
            0
        );
    }

    /// Run the CPU converter over one 2×2 NV12 frame filled with a single
    /// YCbCr value and return the first output pixel as BGRA.
    fn convert_pixel(y: u8, u: u8, v: u8, colour: ColourTags) -> [u8; 4] {
        // 2×2 NV12: 4 luma bytes + one interleaved UV pair (chroma is
        // 2×2 subsampled → a single 2-byte row).
        let nv12 = [y, y, y, y, u, v];
        let mut out = [0u8; 2 * 2 * 4];
        // SAFETY: 6-byte input (4 Y + 2 UV) and 16-byte output; `row_pitch`
        // 8 = 2 pixels × 4 bytes; both planes in bounds for a 2×2 frame.
        unsafe {
            convert_nv12_to_bgr8(nv12.as_ptr(), out.as_mut_ptr(), 8, 4, 2, 2, 2, 2, colour);
        }
        [out[0], out[1], out[2], out[3]]
    }

    fn tags(matrix: VideoMatrix, range: VideoRange) -> ColourTags {
        ColourTags {
            matrix,
            range,
            matrix_coefficients: match matrix {
                VideoMatrix::Bt709 => Some(1),
                VideoMatrix::Bt601 => Some(5),
            },
        }
    }

    #[test]
    fn cpu_converter_applies_stream_matrix_to_yellow_bar() {
        // Measured SMPTE yellow from `bars.mp4` (notes NV12 row:
        // Y=162 Cb=44 Cr=142, limited range) — the exact signature.
        // BT.601 → (192,191,1); BT.709 → (195,181,0). The 601 result must
        // land within tolerance of the ffmpeg auto (VUI=601) reference
        // (192,190,0), the 709 result near the forced-709 reference.
        let bgra601 = convert_pixel(162, 44, 142, tags(VideoMatrix::Bt601, VideoRange::Limited));
        assert_eq!(bgra601, [1, 191, 192, 255]);
        let bgra709 = convert_pixel(162, 44, 142, tags(VideoMatrix::Bt709, VideoRange::Limited));
        assert_eq!(bgra709, [0, 181, 195, 255]);
        // White is matrix-independent (no chroma): both must agree.
        let white601 = convert_pixel(180, 128, 128, tags(VideoMatrix::Bt601, VideoRange::Limited));
        let white709 = convert_pixel(180, 128, 128, tags(VideoMatrix::Bt709, VideoRange::Limited));
        assert_eq!(white601, [191, 191, 191, 255]);
        assert_eq!(white709, [191, 191, 191, 255]);
    }

    #[test]
    fn cpu_converter_full_range_never_touches_the_limited_offset() {
        // Full-range white (Y=255): luma_k = 256, y_offset = 0 → exactly
        // 255 on every channel, in both matrices (limited-range math on a
        // full-range value would clip at 240-ish instead).
        assert_eq!(
            convert_pixel(255, 128, 128, tags(VideoMatrix::Bt601, VideoRange::Full)),
            [255, 255, 255, 255]
        );
        assert_eq!(
            convert_pixel(255, 128, 128, tags(VideoMatrix::Bt709, VideoRange::Full)),
            [255, 255, 255, 255]
        );
        // Full-range yellow (Y=226, Cb=0, Cr=149 — the full-range encoding
        // of RGB 255,255,0): must reconstruct (255,255,0) with the
        // full-range chroma weights (359/88/183/454), not the limited set.
        assert_eq!(
            convert_pixel(226, 0, 149, tags(VideoMatrix::Bt601, VideoRange::Full)),
            [0, 255, 255, 255]
        );
    }

    #[test]
    fn h264_evidence_maps_to_honest_backend_report() {
        // Pure mapping: confirmed registration is the only path to `true`.
        assert_eq!(H264Evidence::MftRegistered.as_backend_report(), Some(true));
        assert_eq!(H264Evidence::Unobservable.as_backend_report(), None);
        // Before `load()` no probe has run, so `stats()` must omit `hwdec`
        // rather than assert a `false` it has not observed.
        let backend = SourceReaderBackend::new().expect("backend must construct");
        assert_eq!(backend.h264_evidence, None);
        assert_eq!(backend.stats().hardware_decode, None);
    }

    #[test]
    fn probe_hw_decoder_returns_result() {
        // The probe needs MF + COM started on the calling thread — the
        // production order is `new()` then `load()`. Called bare on a virgin
        // thread `MFTEnumEx` blocks indefinitely (observed 2026-11-07).
        // With the precondition in place the target machine (Intel UHD,
        // build 26200.9711) returns Ok and a machine without a hardware
        // decoder returns Err — either is valid; the test ensures the probe
        // neither panics nor hangs.
        let _backend = SourceReaderBackend::new().expect("backend must construct");
        let _ = probe_hw_decoder();
    }

    #[test]
    fn stats_reports_zero_fps_before_frames() {
        // measured_fps starts at 0; stats() must not return NaN or garbage
        // before any frame has been presented.
        let backend = SourceReaderBackend::new().expect("backend must construct");
        let stats = backend.stats();
        assert_eq!(stats.fps, 0.0);
        assert_eq!(stats.hardware_decode, None);
    }
}
