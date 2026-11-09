//! Metadata probe for the library (design-m4 §4, §5.1).
//!
//! Reads stream attributes from the container only — **no frame is
//! decoded**, nothing is written, and the user's file is opened read-only
//! (AGENTS §3.3). One call answers `library.add`'s "does this file exist
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
/// Self-contained by design (D13): COM and MF are started on the calling
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
    // Shutdown order: MF first, then COM (design-m2 §3.3).
    let _ = unsafe { MFShutdown() };
    if com_owned {
        // SAFETY: balances the successful CoInitializeEx above.
        unsafe { CoUninitialize() };
    }
    result
}

/// The container read itself. Every MF object created here (the source
/// reader, its media types) dies inside this call — plain data is what
/// returns (D13 leak rules: never a retained MF object).
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
    // assessment engine (S5) interprets the numbers, the library stores
    // them verbatim.
    let profile = unsafe { mt.GetUINT32(&MF_MT_MPEG2_PROFILE) }
        .ok()
        .map(|value| value.to_string());
    let level = unsafe { mt.GetUINT32(&MF_MT_MPEG2_LEVEL) }
        .ok()
        .map(|value| value.to_string());

    // HDR detection (M7): PQ (2084) and HLG are the HDR transfer
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

/// A readable codec name for the library row (`ipc.md` §3 items).
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
        // the mapping decides M7's severity on the wire (core assess).
        assert!(hdr_from_transfer(MFVideoTransFunc_2084.0 as u32));
        assert!(hdr_from_transfer(MFVideoTransFunc_HLG.0 as u32));
        assert!(!hdr_from_transfer(5)); // BT.709
        assert!(!hdr_from_transfer(0)); // Unknown declared as unknown
    }

    /// Manual probe of a real file — run by hand, never in CI (no media
    /// files are committed, AGENTS §11). See design-m4 S3 verification.
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
}
