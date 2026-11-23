//! Metadata probe for the library.
//!
//! Reads stream attributes from the container only — **no frame is
//! decoded**, nothing is written, and the user's file is opened read-only
//! One call answers `library.add`'s "does this file exist
//! and what is in it?".

use std::path::Path;

use cascade_core::library::VideoMetadata;
use windows::core::PCWSTR;
use windows::Win32::Media::MediaFoundation::{
    MFCreateSourceReaderFromURL, MFShutdown, MFStartup, MFVideoFormat_AV1, MFVideoFormat_H264,
    MFVideoFormat_HEVC, MFVideoFormat_MPEG2, MFVideoTransFunc_2084, MFVideoTransFunc_HLG,
    MFSTARTUP_FULL, MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_MPEG2_LEVEL,
    MF_MT_MPEG2_PROFILE, MF_MT_SUBTYPE, MF_MT_TRANSFER_FUNCTION, MF_PD_DURATION, MF_VERSION,
};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
use windows::Win32::System::Variant::{VT_I8, VT_UI8};

use crate::source_reader::{
    file_url, MF_SOURCE_READER_ALL_STREAMS, MF_SOURCE_READER_FIRST_VIDEO_STREAM,
};

/// Probe one file: existence is checked by the caller (it wants its own
/// error text), everything else comes from the container's video type.
///
/// Self-contained by design: COM and MF are started on the calling
/// thread and shut down again before returning, so any caller (daemon
/// main loop, CLI, future GUI) can probe without prior setup — and no MF
/// or COM state is left behind by the call.
pub fn probe(path: &Path) -> Result<VideoMetadata, String> {
    // Same order as `cascade_win::device::decode`: MFT/source-reader MF
    // calls block or fail without COM + MF started on this thread
    // (observed 2026-11-07 in the cascade-media probe).
    let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    // RPC_E_CHANGED_MODE: the thread already has another apartment. Then
    // this call did not initialize anything, so it must not be uninitialized.
    let com_owned = com.is_ok();
    // SAFETY: exact `MF_VERSION`, balancing the MFShutdown below.
    let mf = unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) };
    if mf.is_err() {
        if com_owned {
            // SAFETY: balances the successful CoInitializeEx above.
            unsafe { CoUninitialize() };
        }
        return Err("media foundation is not available".to_string());
    }
    let result = probe_container(path);
    // Shutdown order: MF first, then COM.
    let _ = unsafe { MFShutdown() };
    if com_owned {
        // SAFETY: balances the successful CoInitializeEx above.
        unsafe { CoUninitialize() };
    }
    result
}

/// The container read itself. Every MF object created here (the source
/// reader, its media types) dies inside this call — plain data is what
/// returns (leak rules: never a retained MF object).
fn probe_container(path: &Path) -> Result<VideoMetadata, String> {
    let url = file_url(path);
    let wide: Vec<u16> = url.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: `wide` is nul-terminated and outlives the call; MF does not
    // keep the pointer.
    let reader = unsafe { MFCreateSourceReaderFromURL(PCWSTR::from_raw(wide.as_ptr()), None) }
        .map_err(|err| format!("cannot open: {}", err.message()))?;

    // The native video type carries the container-level attributes.
    // SAFETY: documented stream/type indices; a file without a video
    // stream fails here instead of returning an audio type.
    let mt = unsafe { reader.GetNativeMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM, 0) }
        .map_err(|_| "no video stream in this file".to_string())?;

    // SAFETY: `mt` is a live IMFMediaType; a missing key is an error.
    let packed = unsafe { mt.GetUINT64(&MF_MT_FRAME_SIZE) }
        .map_err(|_| "video stream reports no frame size".to_string())?;
    let width = (packed >> 32) as u32;
    let height = packed as u32;

    // SAFETY: as above; the key is optional (absent → None).
    let fps = unsafe { mt.GetUINT64(&MF_MT_FRAME_RATE) }
        .ok()
        .and_then(fps_from_packed);

    // SAFETY: the reader owns the returned PROPVARIANT; it holds a scalar
    // (VT_UI8/VT_I8), which needs no PropVariantClear.
    let duration_ms =
        unsafe { reader.GetPresentationAttribute(MF_SOURCE_READER_ALL_STREAMS, &MF_PD_DURATION) }
            .ok()
            .and_then(duration_ms_from);

    // SAFETY: optional key on the video type.
    let bitrate_bps = unsafe { mt.GetUINT32(&MF_MT_AVG_BITRATE) }
        .ok()
        .map(u64::from);

    let codec = unsafe { mt.GetGUID(&MF_MT_SUBTYPE) }
        .map(|guid| codec_name(&guid))
        .unwrap_or_else(|_| "unknown".to_string());

    // MPEG-2/H.264/HEVC profile and level are UINT32s when present; the
    // assessment engine interprets the numbers, the library stores
    // them verbatim.
    let profile = unsafe { mt.GetUINT32(&MF_MT_MPEG2_PROFILE) }
        .ok()
        .map(|value| value.to_string());
    let level = unsafe { mt.GetUINT32(&MF_MT_MPEG2_LEVEL) }
        .ok()
        .map(|value| value.to_string());

    // HDR detection: PQ (2084) and HLG are the HDR transfer
    // functions; a present-but-other value is a declared SDR transfer;
    // an absent attribute stays None — unknown, never guessed.
    let hdr = unsafe { mt.GetUINT32(&MF_MT_TRANSFER_FUNCTION) }
        .ok()
        .map(hdr_from_transfer);

    Ok(VideoMetadata {
        width,
        height,
        fps,
        duration_ms,
        bitrate_bps,
        codec,
        profile,
        level,
        hdr,
    })
}

/// Is this `MF_MT_TRANSFER_FUNCTION` value an HDR transfer (PQ/HLG)?
fn hdr_from_transfer(value: u32) -> bool {
    value == MFVideoTransFunc_2084.0 as u32 || value == MFVideoTransFunc_HLG.0 as u32
}

/// `MF_MT_FRAME_RATE` packs numerator (high dword) over denominator.
fn fps_from_packed(packed: u64) -> Option<f64> {
    let num = (packed >> 32) as u32;
    let den = packed as u32;
    if num == 0 || den == 0 {
        return None;
    }
    Some(f64::from(num) / f64::from(den))
}

/// Duration arrives in 100-ns ticks (`MF_PD_DURATION`); convert to ms.
fn duration_ms_from(var: PROPVARIANT) -> Option<u64> {
    // SAFETY: the variant is live and zero-initialised by MF; we read the
    // type tag and the matching union member only.
    let vt = unsafe { (*var.Anonymous.Anonymous).vt };
    let ticks = match vt {
        VT_UI8 => unsafe { (*var.Anonymous.Anonymous).Anonymous.uhVal },
        VT_I8 => (unsafe { (*var.Anonymous.Anonymous).Anonymous.hVal }) as u64,
        _ => return None,
    };
    Some(ticks / 10_000)
}

/// A readable codec name for the library row (items).
fn codec_name(guid: &windows::core::GUID) -> String {
    if *guid == MFVideoFormat_H264 {
        "H264".to_string()
    } else if *guid == MFVideoFormat_HEVC {
        "HEVC".to_string()
    } else if *guid == MFVideoFormat_AV1 {
        "AV1".to_string()
    } else if *guid == MFVideoFormat_MPEG2 {
        "MPEG2".to_string()
    } else {
        // Fourcc-style GUIDs (VP90, WMV3, …) read well as their ASCII
        // bytes; anything else falls back to the GUID's first dword.
        let bytes = guid.data1.to_le_bytes();
        let text: String = bytes
            .iter()
            .map(|&b| {
                if b.is_ascii_graphic() {
                    char::from(b)
                } else {
                    '.'
                }
            })
            .collect();
        if text.contains('.') {
            format!("{:08X}", guid.data1)
        } else {
            text
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::core::implement;
    use windows::Win32::Media::MediaFoundation::{IMFMediaEngineNotify, IMFMediaEngineNotify_Impl};

    #[test]
    fn fps_from_packed_splits_numerator_and_denominator() {
        // 30000/1001 ≈ 29.97 (NTSC), packed numerator-first.
        let packed = (30_000u64 << 32) | 1_001;
        let fps = fps_from_packed(packed).expect("rate");
        assert!((fps - 29.97).abs() < 0.01, "{fps}");
        assert_eq!(fps_from_packed(24 << 32 | 1), Some(24.0));
        // A zero denominator would divide by zero — report None instead.
        assert_eq!(fps_from_packed(30 << 32), None);
        assert_eq!(fps_from_packed(0), None);
    }

    #[test]
    fn duration_is_100ns_ticks_converted_to_ms() {
        // Build a VT_UI8 variant holding 5 000 000 000 ticks = 500 s.
        let mut var = PROPVARIANT::default();
        // SAFETY: `var` is zeroed; set the tag and the matching member.
        unsafe {
            (*var.Anonymous.Anonymous).vt = VT_UI8;
            (*var.Anonymous.Anonymous).Anonymous.uhVal = 5_000_000_000;
        }
        assert_eq!(duration_ms_from(var), Some(500_000));
    }

    #[test]
    fn codec_names_are_readable_and_stable() {
        // These strings appear in `library list` output and the GUI grid.
        assert_eq!(codec_name(&MFVideoFormat_H264), "H264");
        assert_eq!(codec_name(&MFVideoFormat_HEVC), "HEVC");
        assert_eq!(codec_name(&MFVideoFormat_AV1), "AV1");
        assert_eq!(codec_name(&MFVideoFormat_MPEG2), "MPEG2");
        // VP90 as fourcc bytes; a GUID with no printable bytes degrades
        // to its hex dword instead of mojibake.
        let vp9 = windows::core::GUID::from_values(0x30395056, 0, 0, [0; 8]);
        assert_eq!(codec_name(&vp9), "VP90");
        let odd = windows::core::GUID::from_values(0x0000_0001, 0, 0, [0; 8]);
        assert_eq!(codec_name(&odd), "00000001");
    }

    #[test]
    fn hdr_transfer_values_map_to_sdr_or_hdr() {
        // PQ (2084) and HLG are HDR; every declared SDR transfer is not;
        // the mapping decides 's severity on the wire (core assess).
        assert!(hdr_from_transfer(MFVideoTransFunc_2084.0 as u32));
        assert!(hdr_from_transfer(MFVideoTransFunc_HLG.0 as u32));
        assert!(!hdr_from_transfer(5)); // BT.709
        assert!(!hdr_from_transfer(0)); // Unknown declared as unknown
    }

    /// Manual probe of a real file — run by hand, never in CI (no media
    /// files are committed). See verification.
    #[test]
    #[ignore]
    fn probe_reads_a_real_file() {
        let path =
            std::path::Path::new(r"C:\Users\Zahraan Dzakii\Videos\Furina Midnight Maedow.mp4");
        let meta = probe(path).expect("probe");
        assert!(meta.width > 0 && meta.height > 0, "{meta:?}");
        assert!(meta.duration_ms.unwrap_or(0) > 0, "{meta:?}");
        assert!(!meta.codec.is_empty(), "{meta:?}");
        println!("{meta:?}");
    }

    // ── R&D: colour-matrix localisation (quality / ) ────────────────
    //
    // The /AB1 bars failure (42.9% on 2026-11-09, s7-pixel log) matches
    // "content encoded with BT.601, decoded as BT.709" within 1-2 units on
    // three independent primaries, and bars.mp4 declares bt470bg in its H.264
    // VUI (verified on the raw stream) but has NO mp4 colr box. This probe
    // reads the colour attributes Media Foundation reports at two points:
    //
    // native = GetNativeMediaType: what the SOURCE made of the file
    //             (colr box / VUI as the MF demuxer sees it)
    // nv12 = GetCurrentMediaType after negotiating NV12: what the
    //             DECODER hands to the presenter (EVR in the wallpaper path)
    //
    // Interpretation:
    // native absent/709 + nv12 709 -> source drops the VUI tag
    // native 601 + nv12 709 -> decoder drops it
    // native 601 + nv12 601 -> source+decoder correct; the loss is
    //                                    in the presenter (EVR/MediaEngine)
    // That decides where a fix may live : read
    // MF_MT_YUV_MATRIX, set it explicitly).
    //
    // Run by hand, never in CI (no media files are committed). Override the
    // target with CASCADE_PROBE_FILE=<path>.
    #[test]
    #[ignore]
    fn probe_color_attributes_at_source_and_decoder() {
        let path =
            std::path::PathBuf::from(std::env::var("CASCADE_PROBE_FILE").unwrap_or_else(|_| {
                r"C:\Users\Zahraan Dzakii\AppData\Local\Temp\s7\bars.mp4".to_string()
            }));
        // Same COM/MF startup order as `probe()` (observed 2026-11-07:
        // MF calls block without COM + MF on the calling thread).
        let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        let com_owned = com.is_ok();
        // SAFETY: exact `MF_VERSION`, balancing the MFShutdown below.
        let mf = unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) };
        assert!(mf.is_ok(), "MFStartup failed");
        let result = color_probe_body(&path);
        // Shutdown order: MF first, then COM.
        let _ = unsafe { MFShutdown() };
        if com_owned {
            // SAFETY: balances the successful CoInitializeEx above.
            unsafe { CoUninitialize() };
        }
        result.expect("color probe");
    }

    /// The probe itself. Opens the file read-only through the source
    /// reader and prints colour attributes at both stages;
    /// creates no sample, decodes no frame.
    fn color_probe_body(path: &std::path::Path) -> Result<(), String> {
        use windows::Win32::Media::MediaFoundation::{
            MFCreateMediaType, MFMediaType_Video, MFVideoFormat_NV12, MF_MT_MAJOR_TYPE,
            MF_MT_MPEG_SEQUENCE_HEADER,
        };

        let url = file_url(path);
        let wide: Vec<u16> = url.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: `wide` is nul-terminated and outlives the call; MF does
        // not keep the pointer.
        let reader = unsafe { MFCreateSourceReaderFromURL(PCWSTR::from_raw(wide.as_ptr()), None) }
            .map_err(|err| format!("cannot open: {}", err.message()))?;

        // SAFETY: documented stream/type indices for a single-video-stream
        // file; a file without video fails here.
        let native = unsafe { reader.GetNativeMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM, 0) }
            .map_err(|_| "no video stream in this file".to_string())?;
        println!("native: {}", color_attrs(&native));

        // Does the reader expose the avcC sequence header (SPS/PPS)? If
        // yes, a pure SPS/VUI parser can derive the matrix for files whose
        // only colour signal is the bitstream VUI:
        // no container box walk needed.
        // SAFETY: `native` is a live media type; the size is an out-param.
        match unsafe { native.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER) } {
            Ok(total) => {
                let mut buf = vec![0u8; total as usize];
                // SAFETY: the buffer matches the size MF just reported.
                match unsafe { native.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut buf, None) } {
                    Ok(()) => {
                        let head: Vec<String> =
                            buf.iter().take(48).map(|b| format!("{b:02X}")).collect();
                        println!("avcC: {total} bytes: {}", head.join(" "));
                    }
                    Err(err) => println!("avcC: {total} bytes, read failed: {}", err.message()),
                }
            }
            Err(_) => println!("avcC: <absent>"),
        }

        // Negotiate NV12 output — the same call the Tier 2 reader uses
        // (`source_reader::negotiate_nv12`), which forces the decoder to
        // pick an output media type.
        // SAFETY: plain COM object creation.
        let mt = unsafe { MFCreateMediaType() }
            .map_err(|err| format!("MFCreateMediaType: {}", err.message()))?;
        // SAFETY: `mt` is a fresh media type; the keys are documented.
        unsafe { mt.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video) }
            .map_err(|err| format!("SetGUID(MAJOR_TYPE): {}", err.message()))?;
        unsafe { mt.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12) }
            .map_err(|err| format!("SetGUID(SUBTYPE): {}", err.message()))?;
        // SAFETY: reader and media type are live and belong to this thread.
        unsafe { reader.SetCurrentMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM, None, &mt) }
            .map_err(|err| format!("SetCurrentMediaType(NV12): {}", err.message()))?;
        // SAFETY: the stream now has a negotiated output type.
        let out = unsafe { reader.GetCurrentMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM) }
            .map_err(|err| format!("GetCurrentMediaType: {}", err.message()))?;
        println!("nv12:  {}", color_attrs(&out));
        Ok(())
    }

    /// Printable colour attributes of one media type. A missing key prints
    /// as `<absent>` — an absent `MF_MT_YUV_MATRIX` is exactly what this
    /// probe looks for, so absence must never look like a value.
    fn color_attrs(mt: &windows::Win32::Media::MediaFoundation::IMFMediaType) -> String {
        use windows::core::GUID;
        use windows::Win32::Media::MediaFoundation::{
            MF_MT_TRANSFER_FUNCTION, MF_MT_VIDEO_NOMINAL_RANGE, MF_MT_VIDEO_PRIMARIES,
            MF_MT_YUV_MATRIX,
        };

        fn u32_or(mt: &windows::Win32::Media::MediaFoundation::IMFMediaType, key: &GUID) -> String {
            // SAFETY: `mt` is a live IMFMediaType; a missing key surfaces
            // as an error, which becomes `<absent>`.
            unsafe { mt.GetUINT32(key) }
                .map(|value| value.to_string())
                .unwrap_or_else(|_| "<absent>".to_string())
        }
        fn matrix_name(value: u32) -> &'static str {
            match value {
                0 => "Unknown",
                1 => "BT.709",
                2 => "BT.601",
                3 => "SMPTE240M",
                4 => "BT.2020-10",
                5 => "BT.2020-12",
                _ => "?",
            }
        }

        let subtype = match unsafe { mt.GetGUID(&MF_MT_SUBTYPE) } {
            Ok(guid) => codec_name(&guid),
            Err(_) => "<no subtype>".to_string(),
        };
        let matrix = match unsafe { mt.GetUINT32(&MF_MT_YUV_MATRIX) } {
            Ok(value) => format!("{value} ({})", matrix_name(value)),
            Err(_) => "<absent>".to_string(),
        };
        format!(
            "subtype={subtype} matrix={matrix} range={} transfer={} primaries={}",
            u32_or(mt, &MF_MT_VIDEO_NOMINAL_RANGE),
            u32_or(mt, &MF_MT_TRANSFER_FUNCTION),
            u32_or(mt, &MF_MT_VIDEO_PRIMARIES),
        )
    }

    // ──: frame-server mode ─────────────────

    /// Required event sink for the frame-server probe: the callback
    /// attribute must exist in every MediaEngine mode (MSDN
    /// `MF_MEDIA_ENGINE_CALLBACK`), readiness is polled instead.
    #[implement(IMFMediaEngineNotify)]
    struct FrameServerNotify;

    impl IMFMediaEngineNotify_Impl for FrameServerNotify_Impl {
        fn EventNotify(
            &self,
            _event: u32,
            _param1: usize,
            _param2: u32,
        ) -> windows::core::Result<()> {
            Ok(())
        }
    }

    /// Frame-server feasibility probe.
    ///
    /// Creates a MediaEngine **without** `MF_MEDIA_ENGINE_PLAYBACK_HWND`
    /// (frame-server is the documented default mode), attaches our D3D11
    /// device via `MF_MEDIA_ENGINE_DXGI_MANAGER`, then follows the Microsoft
    /// contract: `OnVideoStreamTick` -> `S_OK` -> `TransferVideoFrame` into
    /// our own texture. Reads back pixels at the SMPTE bar centres to
    /// which chroma matrix the engine's own YUV->RGB conversion uses
    /// (Experiment B proved the EVR *rendering* path decodes `bars.mp4` as
    /// BT.709; this asks whether the frame-server blit path differs), tries
    /// an NV12 destination (raw frames for a converter we control), and
    /// queries `IMFMediaEngineEx::GetStreamAttribute(MF_MT_YUV_MATRIX)`.
    ///
    /// Run by hand, never in CI. Override the file with `CASCADE_PROBE_FILE`.
    #[test]
    #[ignore]
    fn probe_frame_server_transfer() {
        let path =
            std::path::PathBuf::from(std::env::var("CASCADE_PROBE_FILE").unwrap_or_else(|_| {
                r"C:\Users\Zahraan Dzakii\AppData\Local\Temp\s7\bars.mp4".to_string()
            }));
        // STA matches the daemon's engine thread (engine.rs `ComInit`,
        // same COM/MF startup order as the colour probe.
        use windows::Win32::System::Com::COINIT_APARTMENTTHREADED;
        let com = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        let com_owned = com.is_ok();
        // SAFETY: exact `MF_VERSION`, balancing the MFShutdown below.
        let mf = unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) };
        assert!(mf.is_ok(), "MFStartup failed");
        let result = frame_server_body(&path);
        // Shutdown order: MF first, then COM.
        let _ = unsafe { MFShutdown() };
        if com_owned {
            // SAFETY: balances the successful CoInitializeEx above.
            unsafe { CoUninitialize() };
        }
        result.expect("frame-server probe");
    }

    /// The probe body. The engine is released (never `Shutdown()`) when
    /// this fn returns — the same release path `engine.rs` uses when its
    /// playback window is gone; the caller then runs MFShutdown.
    fn frame_server_body(path: &std::path::Path) -> Result<(), String> {
        use std::time::{Duration, Instant};
        use windows::core::{IUnknown, Interface, BSTR};
        use windows::Win32::Foundation::{HMODULE, RECT};
        use windows::Win32::Graphics::Direct3D::{
            D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0,
        };
        use windows::Win32::Graphics::Direct3D11::{
            D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
            D3D11_BIND_RENDER_TARGET, D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ,
            D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE, D3D11_USAGE_DEFAULT,
            D3D11_USAGE_STAGING,
        };
        use windows::Win32::Graphics::Dxgi::Common::{
            DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC,
        };
        use windows::Win32::Graphics::Dxgi::IDXGIAdapter;
        use windows::Win32::Media::MediaFoundation::{
            CLSID_MFMediaEngineClassFactory, IMFAttributes, IMFDXGIDeviceManager, IMFMediaEngine,
            IMFMediaEngineClassFactory, IMFMediaEngineEx, MFCreateAttributes,
            MFCreateDXGIDeviceManager, MF_MEDIA_ENGINE_CALLBACK, MF_MEDIA_ENGINE_DXGI_MANAGER,
            MF_MEDIA_ENGINE_READY_HAVE_CURRENT_DATA, MF_MT_YUV_MATRIX,
        };
        use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
        use windows::Win32::UI::WindowsAndMessaging::{
            DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE,
        };

        /// Drain the STA queue while waiting (engine.rs `pump_messages`).
        fn pump() {
            let mut msg = MSG::default();
            // SAFETY: PeekMessageW fills `msg`; dispatch like engine.rs.
            while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
                let _ = unsafe { TranslateMessage(&msg) };
                // SAFETY: `msg` was filled by PeekMessageW.
                unsafe { DispatchMessageW(&msg) };
            }
        }

        /// Printable result of a `Result<()>` COM call.
        fn hr_string(res: &Result<(), windows::core::Error>) -> String {
            match res {
                Ok(()) => "S_OK".to_string(),
                Err(err) => format!("0x{:08X} {}", err.code().0, err.message()),
            }
        }

        /// One texture with explicit usage/bind/cpu-access flags.
        fn tex(
            device: &ID3D11Device,
            w: u32,
            h: u32,
            format: DXGI_FORMAT,
            usage: D3D11_USAGE,
            bind: u32,
            access: u32,
        ) -> Result<ID3D11Texture2D, String> {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: w,
                Height: h,
                MipLevels: 1,
                ArraySize: 1,
                Format: format,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: usage,
                BindFlags: bind,
                CPUAccessFlags: access,
                MiscFlags: 0,
            };
            let mut texture: Option<ID3D11Texture2D> = None;
            // SAFETY: `desc` is valid; out-param starts as None.
            unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }
                .map_err(|err| format!("CreateTexture2D: {}", err.message()))?;
            texture.ok_or_else(|| "no texture".to_string())
        }

        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        let levels = [D3D_FEATURE_LEVEL_11_0];
        // SAFETY: standard device creation; out-params start as None.
        // VIDEO_SUPPORT because the engine's blit may use its video
        // processor on this device; BGRA is required for the BGRA target.
        unsafe {
            D3D11CreateDevice(
                None::<&IDXGIAdapter>,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                Some(&levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
        }
        .map_err(|err| format!("D3D11CreateDevice: {}", err.message()))?;
        let device = device.ok_or_else(|| "no D3D11 device".to_string())?;
        let context = context.ok_or_else(|| "no D3D11 context".to_string())?;

        let mut reset_token: u32 = 0;
        let mut manager: Option<IMFDXGIDeviceManager> = None;
        // SAFETY: out-params initialised; MF owns the returned manager.
        unsafe { MFCreateDXGIDeviceManager(&mut reset_token, &mut manager) }
            .map_err(|err| format!("MFCreateDXGIDeviceManager: {}", err.message()))?;
        let manager = manager.ok_or_else(|| "no DXGI manager".to_string())?;
        // SAFETY: device and manager are live on this thread.
        unsafe { manager.ResetDevice(&device, reset_token) }
            .map_err(|err| format!("ResetDevice: {}", err.message()))?;

        let mut attrs: Option<IMFAttributes> = None;
        // SAFETY: valid out-pointer; size 0 = "no initial attributes".
        unsafe { MFCreateAttributes(&mut attrs, 0) }
            .map_err(|err| format!("MFCreateAttributes: {}", err.message()))?;
        let attrs = attrs.ok_or_else(|| "no attributes".to_string())?;
        // Frame-server mode: PLAYBACK_HWND is deliberately NOT set (MSDN
        // `CreateInstance`: rendering mode requires PLAYBACK_HWND or
        // PLAYBACK_VISUAL; without either the engine is in its default
        // frame-server mode).
        // SAFETY: documented attribute; SetUnknown AddRefs the manager.
        unsafe { attrs.SetUnknown(&MF_MEDIA_ENGINE_DXGI_MANAGER, &manager) }
            .map_err(|err| format!("SetUnknown DXGI_MANAGER: {}", err.message()))?;
        let notify: IMFMediaEngineNotify = FrameServerNotify.into();
        // SAFETY: the sink is mandatory in every mode (MSDN CALLBACK).
        unsafe { attrs.SetUnknown(&MF_MEDIA_ENGINE_CALLBACK, &notify) }
            .map_err(|err| format!("SetUnknown CALLBACK: {}", err.message()))?;
        // SAFETY: documented in-proc class factory; null outer interface.
        let factory: IMFMediaEngineClassFactory = unsafe {
            CoCreateInstance(&CLSID_MFMediaEngineClassFactory, None, CLSCTX_INPROC_SERVER)
        }
        .map_err(|err| format!("class factory: {}", err.message()))?;
        // SAFETY: dwflags 0 is the only documented value for CreateInstance.
        let engine: IMFMediaEngine = unsafe { factory.CreateInstance(0, &attrs) }
            .map_err(|err| format!("CreateInstance: {}", err.message()))?;
        // SAFETY: live engine.
        unsafe { engine.SetMuted(true) }.map_err(|err| format!("SetMuted: {}", err.message()))?;

        let url = file_url(path);
        // SAFETY: engine is live; the BSTR outlives the call.
        unsafe { engine.SetSource(&BSTR::from(url.as_str())) }
            .map_err(|err| format!("SetSource: {}", err.message()))?;
        // SAFETY: live engine.
        unsafe { engine.Load() }.map_err(|err| format!("Load: {}", err.message()))?;
        // Bounded readiness wait (engine.rs `wait_ready` shape).
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            // SAFETY: live engine; no out-parameters.
            let ready = unsafe { engine.GetReadyState() };
            if ready as i32 >= MF_MEDIA_ENGINE_READY_HAVE_CURRENT_DATA.0 {
                break;
            }
            if Instant::now() >= deadline {
                return Err(format!("not ready after 5 s (state {ready})"));
            }
            pump();
            std::thread::sleep(Duration::from_millis(10));
        }
        // SAFETY: live engine.
        unsafe { engine.SetLoop(true) }.map_err(|err| format!("SetLoop: {}", err.message()))?;
        // SAFETY: live engine.
        unsafe { engine.Play() }.map_err(|err| format!("Play: {}", err.message()))?;

        let (mut width, mut height) = (0u32, 0u32);
        // SAFETY: live engine; both out-params are valid.
        unsafe { engine.GetNativeVideoSize(Some(&mut width), Some(&mut height)) }
            .map_err(|err| format!("GetNativeVideoSize: {}", err.message()))?;
        if width == 0 || height == 0 {
            return Err("zero video size".into());
        }
        println!("frame-server: playing {width}x{height}");

        // Destination 1: BGRA — the engine converts YUV->RGB itself, so the
        // bar pixels answer the matrix question directly. Bind
        // RENDER_TARGET for the engine's blit; fall back to no bind if the
        // driver refuses that combination.
        let bgra = match tex(
            &device,
            width,
            height,
            DXGI_FORMAT_B8G8R8A8_UNORM,
            D3D11_USAGE_DEFAULT,
            D3D11_BIND_RENDER_TARGET.0 as u32,
            0,
        ) {
            Ok(texture) => texture,
            Err(first) => {
                println!("bgra bind=RT failed ({first}); retrying bind=0");
                tex(
                    &device,
                    width,
                    height,
                    DXGI_FORMAT_B8G8R8A8_UNORM,
                    D3D11_USAGE_DEFAULT,
                    0,
                    0,
                )?
            }
        };
        // Destination 2: NV12 — raw frames a converter we control could
        // consume; whether the engine accepts a non-RGB destination is the
        // measurement.
        let nv12 = tex(
            &device,
            width,
            height,
            DXGI_FORMAT_NV12,
            D3D11_USAGE_DEFAULT,
            D3D11_BIND_RENDER_TARGET.0 as u32,
            0,
        )
        .or_else(|err| {
            println!("nv12 bind=RT failed ({err}); retrying bind=0");
            tex(
                &device,
                width,
                height,
                DXGI_FORMAT_NV12,
                D3D11_USAGE_DEFAULT,
                0,
                0,
            )
        })?;
        let bgra_dst: IUnknown = bgra
            .cast()
            .map_err(|err| format!("BGRA->IUnknown: {}", err.message()))?;
        let nv12_dst: IUnknown = nv12
            .cast()
            .map_err(|err| format!("NV12->IUnknown: {}", err.message()))?;

        let rect = RECT {
            left: 0,
            top: 0,
            right: width as i32,
            bottom: height as i32,
        };
        // Microsoft docs (`OnVideoStreamTick`): call once per vertical
        // blank; S_OK = new frame ready -> `TransferVideoFrame`. The safe
        // wrapper cannot tell S_OK from S_FALSE (both are success
        // HRESULTs and `HRESULT::ok` accepts both), so the raw HRESULT is
        // read from the vtable.
        let deadline = Instant::now() + Duration::from_secs(4);
        let mut new_frames: u32 = 0;
        let mut bgra_result = "not attempted".to_string();
        let mut nv12_result = "not attempted".to_string();
        while Instant::now() < deadline && new_frames < 3 {
            let mut pts: i64 = 0;
            // SAFETY: live engine; the vtable call fills `pts` per API.
            let hr = unsafe {
                let vtable = Interface::vtable(&engine);
                (vtable.OnVideoStreamTick)(Interface::as_raw(&engine), &mut pts)
            };
            if hr.0 < 0 {
                return Err(format!("OnVideoStreamTick: 0x{:08X}", hr.0));
            }
            if hr.0 == 0 {
                new_frames += 1;
                if new_frames == 1 {
                    println!("first new frame, pts={pts}");
                }
                if bgra_result == "not attempted" {
                    // SAFETY: engine and destination are live; the full
                    // video rect is the destination (the engine scales and
                    // letterboxes itself, MSDN TransferVideoFrame).
                    let res = unsafe { engine.TransferVideoFrame(&bgra_dst, None, &rect, None) };
                    bgra_result = hr_string(&res);
                }
                if nv12_result == "not attempted" {
                    // SAFETY: as above, second destination.
                    let res = unsafe { engine.TransferVideoFrame(&nv12_dst, None, &rect, None) };
                    nv12_result = hr_string(&res);
                }
            } else {
                // S_FALSE: no new frame — wait for the next tick, keeping
                // the STA queue drained.
                pump();
                std::thread::sleep(Duration::from_millis(2));
            }
        }
        println!(
            "OnVideoStreamTick: new_frames={new_frames} bgra={bgra_result} nv12={nv12_result}"
        );

        // Does the engine surface the matrix as a read-only stream
        // attribute? : no setter exists; this is the query channel.)
        let ex = engine
            .cast::<IMFMediaEngineEx>()
            .map_err(|err| format!("cast IMFMediaEngineEx: {}", err.message()))?;
        // SAFETY: the call fills the PROPVARIANT; union members are read
        // only for the scalar tags the match accepts (engine.rs
        // `read_statistics` pattern). Read-only query either way.
        let matrix_attr = unsafe {
            match ex.GetStreamAttribute(0, &MF_MT_YUV_MATRIX) {
                Ok(pv) => {
                    let vt = pv.Anonymous.Anonymous.vt.0;
                    match vt {
                        19 => pv.Anonymous.Anonymous.Anonymous.ulVal.to_string(),
                        3 => pv.Anonymous.Anonymous.Anonymous.lVal.to_string(),
                        other => format!("vt={other}"),
                    }
                }
                Err(err) => format!("0x{:08X} {}", err.code().0, err.message()),
            }
        };
        println!("GetStreamAttribute(MF_MT_YUV_MATRIX): {matrix_attr}");

        // Pixel readback: copy the BGRA destination into a CPU-readable
        // staging texture and sample the centre of each of the seven SMPTE
        // top-row bars (ffmpeg smptebars order: white, yellow, cyan, green,
        // magenta, red, blue left to right).
        let staging = tex(
            &device,
            width,
            height,
            DXGI_FORMAT_B8G8R8A8_UNORM,
            D3D11_USAGE_STAGING,
            0,
            D3D11_CPU_ACCESS_READ.0 as u32,
        )?;
        // SAFETY: live context and textures.
        unsafe { context.CopyResource(&staging, &bgra) };
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: staging has CPU read access; the out-param is filled on
        // success.
        unsafe { context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)) }
            .map_err(|err| format!("Map: {}", err.message()))?;
        let pitch = mapped.RowPitch as usize;
        // `pData` is a plain struct field; the pointer stays valid until
        // the Unmap below.
        let base = mapped.pData.cast::<u8>();
        if base.is_null() {
            // SAFETY: balanced against the Map above.
            unsafe { context.Unmap(&staging, 0) };
            return Err("mapped pData is null".into());
        }
        let labels = ["white", "yellow", "cyan", "green", "magenta", "red", "blue"];
        for (i, label) in labels.iter().enumerate() {
            // Centre of bar i: the seven bars split the width evenly, so
            // bar i spans [i*W/7, (i+1)*W/7) and its centre is (2i+1)*W/14.
            let x = (((i as u32 * 2 + 1) * width) / 14) as usize;
            // One eighth down the height: inside the top band of the frame.
            let y = (height / 8) as usize;
            // SAFETY: x,y are inside the surface; the row covers `x`.
            let px = unsafe { base.add(y * pitch + x * 4) };
            // SAFETY: four bytes of one BGRA texel.
            let (b, g, r) = unsafe { (*px, *px.add(1), *px.add(2)) };
            println!("bar {label:>6}: measured RGB = ({r},{g},{b})");
        }
        // SAFETY: balanced against the Map above.
        unsafe { context.Unmap(&staging, 0) };

        // NV12 readback: is the transferred NV12 the *raw decoded* frame
        // (matrix-independent Y'CbCr a converter we control could consume),
        // or has the engine already converted/scaled it? Compare Y and
        // Cb/Cr at the same bar centres against ffmpeg's raw yuv420p
        // decode (ground truth produced by mk-rowrefs.ps1).
        let nv12_staging = tex(
            &device,
            width,
            height,
            DXGI_FORMAT_NV12,
            D3D11_USAGE_STAGING,
            0,
            D3D11_CPU_ACCESS_READ.0 as u32,
        )?;
        // SAFETY: live context and textures.
        unsafe { context.CopyResource(&nv12_staging, &nv12) };
        let mut mapped_nv = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: staging has CPU read access; out-param filled on success.
        unsafe { context.Map(&nv12_staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped_nv)) }
            .map_err(|err| format!("Map NV12: {}", err.message()))?;
        let nv_pitch = mapped_nv.RowPitch as usize;
        // `pData` is a plain struct field; valid until Unmap below.
        let nv_base = mapped_nv.pData.cast::<u8>();
        if nv_base.is_null() {
            // SAFETY: balanced against the Map above.
            unsafe { context.Unmap(&nv12_staging, 0) };
            return Err("mapped NV12 pData is null".into());
        }
        for (i, label) in labels.iter().enumerate() {
            let x = (((i as u32 * 2 + 1) * width) / 14) as usize;
            let y = (height / 8) as usize;
            // Chroma plane is 2x subsampled, hence (x/2, y/2).
            let (cx, cy) = (x / 2, y / 2);
            // SAFETY: all coordinates are inside the mapped surface; the
            // NV12 UV plane starts `RowPitch * Height` bytes in.
            let (yy, cb, cr) = unsafe {
                let yv = *nv_base.add(y * nv_pitch + x);
                let uv = nv_base.add(height as usize * nv_pitch + cy * nv_pitch + cx * 2);
                (yv, *uv, *uv.add(1))
            };
            println!("bar {label:>6}: NV12 = Y={yy} Cb={cb} Cr={cr}");
        }
        // SAFETY: balanced against the Map above.
        unsafe { context.Unmap(&nv12_staging, 0) };
        Ok(())
    }
}
