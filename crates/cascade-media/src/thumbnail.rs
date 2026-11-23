//! First-frame thumbnails.
//!
//! One video file in, one fitted RGBA frame out — plus JPEG encode and
//! decode for the disk cache. Called **only by `cascade-ui`**: the daemon
//! never decodes a thumbnail (isolation).
//!
//! Each call is self-contained: COM + Media Foundation start on the
//! calling thread and shut down again before returning (the
//! `probe::probe` pattern), so the dedicated thumb thread needs no setup
//! of its own.
//!
//! amendment (measured, D.3/D.5): MF
//! refuses RGB32 output without XVP and XVP crashes this machine's Intel
//! driver 4/4 — so the decoder output is NV12 (the daemon's proven
//! default path, no video processor involved) and YUV→RGB is our own
//! converter, honouring the stream's SPS/VUI colour tags.

use std::path::Path;

use image::imageops::FilterType;
use windows::Win32::Media::MediaFoundation::{
    IMFSample, IMFSourceReader, MFShutdown, MFStartup, MFSTARTUP_FULL, MF_VERSION,
};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};

use crate::colour::ColourTags;
use crate::source_reader::{
    convert_nv12_to_bgr8, negotiate_nv12, nv12_alloc_height, open_reader_plain, query_frame_size,
    sequence_header_tags, MF_SOURCE_READER_FIRST_VIDEO_STREAM, MF_SOURCE_READER_FLAG_END_OF_STREAM,
    MF_SOURCE_READER_FLAG_ERROR,
};

/// Fitted thumbnail size: 2× the 160×90 pt tile, so
/// one texture stays sharp at 100–200 % DPI.
pub const THUMB_WIDTH: u32 = 320;
pub const THUMB_HEIGHT: u32 = 180;
/// JPEG quality for the disk cache.
pub const JPEG_QUALITY: u8 = 80;

/// One frame: top-down RGBA8, tightly packed (4 bytes per pixel).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl Frame {
    /// Build a frame after checking the buffer covers `width × height`.
    fn new(width: u32, height: u32, rgba: Vec<u8>) -> Result<Self, String> {
        let expect = width as usize * height as usize * 4;
        if rgba.len() != expect {
            return Err(format!(
                "frame buffer {} bytes != {expect} for {width}x{height}",
                rgba.len()
            ));
        }
        Ok(Self {
            width,
            height,
            rgba,
        })
    }
}

/// Decode the first video frame of `path` at native size, converted to
/// RGBA with the stream's own colour tags.
///
/// Self-contained per call: COM + MF start here and shut down before
/// returning, exactly like `probe::probe`.
pub fn decode_first_frame(path: &Path) -> Result<Frame, String> {
    // Same order as `cascade-media::probe`: MF calls block or fail
    // without COM + MF started on this thread.
    let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    // RPC_E_CHANGED_MODE: another apartment already owns this thread —
    // then this call initialised nothing and must not be uninitialised.
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
    let result = first_frame(path);
    // Shutdown order: MF first, then COM.
    let _ = unsafe { MFShutdown() };
    if com_owned {
        // SAFETY: balances the successful CoInitializeEx above.
        unsafe { CoUninitialize() };
    }
    result
}

/// The decode itself: open → colour tags → NV12 → first sample → RGBA.
fn first_frame(path: &Path) -> Result<Frame, String> {
    let reader = open_reader_plain(path)?;
    // SPS/VUI tags first (same order as the engine): the sequence
    // header belongs to the file, unaffected by output negotiation.
    let colour = sequence_header_tags(&reader);
    let (width, height) =
        query_frame_size(&reader).map_err(|err| format!("frame size: {}", err.message()))?;
    negotiate_nv12(&reader)?;
    let sample = read_first_sample(&reader)?;
    nv12_frame_to_rgba(&sample, width as usize, height as usize, colour)
}

/// Read samples until the first video frame arrives. The video stream is
/// the only one selected, so every sample here is a decoded frame; the
/// bound only tolerates a couple of empty reads before giving up.
fn read_first_sample(reader: &IMFSourceReader) -> Result<IMFSample, String> {
    for _ in 0..8 {
        let mut actual_stream: u32 = 0;
        let mut stream_flags: u32 = 0;
        let mut timestamp: i64 = 0;
        let mut sample: Option<IMFSample> = None;
        // SAFETY: out-params are initialised; only the video stream is
        // selected (open_reader_plain deselects everything else first).
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
            return Err(format!("ReadSample: {}", err.message()));
        }
        if stream_flags & MF_SOURCE_READER_FLAG_ERROR != 0 {
            return Err("decoder reported an error on the first read".to_string());
        }
        if let Some(sample) = sample {
            return Ok(sample);
        }
        if stream_flags & MF_SOURCE_READER_FLAG_END_OF_STREAM != 0 {
            return Err("the file produced no frames".to_string());
        }
    }
    Err("no frame after 8 reads".to_string())
}

/// CPU path for one decoded sample: lock its NV12 buffer and convert it
/// to an RGBA frame at native size (own converter + the stream's tags).
fn nv12_frame_to_rgba(
    sample: &IMFSample,
    video_w: usize,
    video_h: usize,
    colour: ColourTags,
) -> Result<Frame, String> {
    // SAFETY: sample is live on this thread.
    let buffer = unsafe { sample.ConvertToContiguousBuffer() }
        .map_err(|err| format!("ConvertToContiguousBuffer: {}", err.message()))?;
    let mut buf_ptr: *mut u8 = std::ptr::null_mut();
    let mut buf_len: u32 = 0;
    // SAFETY: `buf_ptr`/`buf_len` are initialised; Unlock is paired.
    if unsafe { buffer.Lock(&mut buf_ptr, None, Some(&mut buf_len)) }.is_err() {
        return Err("frame buffer lock failed".to_string());
    }
    let result = convert_locked_nv12(buf_ptr, buf_len as usize, video_w, video_h, colour);
    // SAFETY: Lock succeeded, so the unlock is paired.
    unsafe {
        let _ = buffer.Unlock();
    }
    result
}

/// Convert a locked NV12 buffer to an RGBA `Frame`.
///
/// Mirrors the engine's CPU consume path: the decoder pads the luma
/// plane to whole macroblocks (`nv12_alloc_height`), the converter walks
/// rows in display order (top-down, RnD D.4), and the colour matrix/
/// range come from the SPS/VUI parser — never a guess.
fn convert_locked_nv12(
    src: *const u8,
    buf_len: usize,
    video_w: usize,
    video_h: usize,
    colour: ColourTags,
) -> Result<Frame, String> {
    let needed = video_w * video_h * 3 / 2;
    if buf_len < needed {
        return Err(format!(
            "NV12 buffer too small: {buf_len} < {needed} (video {video_w}x{video_h})"
        ));
    }
    let uv_offset = video_w * nv12_alloc_height(buf_len, video_w, video_h);
    let mut bgra = vec![0u8; video_w * video_h * 4];
    // SAFETY: `buf_len >= needed` covers both planes (checked above);
    // `bgra` holds `video_h` rows of `video_w * 4` bytes, which is
    // exactly what the converter writes at `row_pitch = video_w * 4`.
    unsafe {
        convert_nv12_to_bgr8(
            src,
            bgra.as_mut_ptr(),
            video_w * 4,
            uv_offset,
            video_w,
            video_h,
            video_w,
            video_h,
            colour,
        );
    }
    bgra_to_rgba(&mut bgra);
    Frame::new(video_w as u32, video_h as u32, bgra)
}

/// MF's converter writes B,G,R,A bytes; egui and the `image` crate want
/// R,G,B,A. In-place channel swap, pure — unit-tested.
fn bgra_to_rgba(pixels: &mut [u8]) {
    // Index walk, not `chunks_exact(4)`: clippy suggests `as_chunks`
    // (Rust 1.88) but the workspace MSRV is 1.80 (Cargo.toml).
    let full = pixels.len() / 4 * 4;
    for i in (0..full).step_by(4) {
        pixels.swap(i, i + 2);
    }
}

/// Contain-fit into `out_w × out_h`: Triangle downscale, then pad
/// the letterbox with `pad` (RGBA) so the result is always exactly the
/// cache size. A frame that already matches passes through untouched.
pub fn fit_contain(frame: &Frame, out_w: u32, out_h: u32, pad: [u8; 4]) -> Result<Frame, String> {
    if frame.width == 0 || frame.height == 0 {
        return Err("cannot fit an empty frame".to_string());
    }
    if frame.width == out_w && frame.height == out_h {
        return Ok(frame.clone());
    }
    let src = image::RgbaImage::from_raw(frame.width, frame.height, frame.rgba.clone())
        .ok_or_else(|| "frame buffer does not match its size".to_string())?;
    let scale = (out_w as f32 / frame.width as f32).min(out_h as f32 / frame.height as f32);
    let scaled_w = ((frame.width as f32 * scale).round() as u32).clamp(1, out_w);
    let scaled_h = ((frame.height as f32 * scale).round() as u32).clamp(1, out_h);
    let scaled = if (scaled_w, scaled_h) == (frame.width, frame.height) {
        src
    } else {
        image::imageops::resize(&src, scaled_w, scaled_h, FilterType::Triangle)
    };
    let mut out = image::RgbaImage::from_pixel(out_w, out_h, image::Rgba(pad));
    let offset_x = (out_w - scaled_w) / 2;
    let offset_y = (out_h - scaled_h) / 2;
    image::imageops::overlay(&mut out, &scaled, i64::from(offset_x), i64::from(offset_y));
    Frame::new(out_w, out_h, out.into_raw())
}

/// Encode a frame as JPEG at `quality` for the disk cache.
///
/// JPEG has no alpha channel; every pixel this module produces is
/// opaque, so the A byte is dropped, not composited.
pub fn encode_jpeg(frame: &Frame, quality: u8) -> Result<Vec<u8>, String> {
    let mut rgb = Vec::with_capacity(frame.rgba.len() / 4 * 3);
    // Index walk, not `chunks_exact(4)` (workspace MSRV 1.80,
    // `bgra_to_rgba`).
    let full = frame.rgba.len() / 4 * 4;
    for i in (0..full).step_by(4) {
        rgb.extend_from_slice(&frame.rgba[i..i + 3]);
    }
    let img = image::RgbImage::from_raw(frame.width, frame.height, rgb)
        .ok_or_else(|| "frame buffer does not match its size".to_string())?;
    let mut out = Vec::new();
    let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality);
    encoder
        .encode_image(&img)
        .map_err(|err| format!("jpeg encode: {err}"))?;
    Ok(out)
}

/// Decode a cached JPEG back into a frame (cache-hit path of ).
pub fn decode_jpeg(bytes: &[u8]) -> Result<Frame, String> {
    let img = image::load_from_memory_with_format(bytes, image::ImageFormat::Jpeg)
        .map_err(|err| format!("jpeg decode: {err}"))?
        .to_rgba8();
    Ok(Frame {
        width: img.width(),
        height: img.height(),
        rgba: img.into_raw(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Solid-colour synthetic frame (no file system, no MF — pure).
    fn solid(width: u32, height: u32, rgba: [u8; 4]) -> Frame {
        Frame {
            width,
            height,
            rgba: rgba.repeat((width * height) as usize),
        }
    }

    #[test]
    fn bgra_swap_exchanges_red_and_blue_only() {
        let mut pixels = [1u8, 2, 3, 4, 10, 20, 30, 40];
        bgra_to_rgba(&mut pixels);
        assert_eq!(pixels, [3, 2, 1, 4, 30, 20, 10, 40]);
    }

    #[test]
    fn bgra_swap_is_a_no_op_on_empty_input() {
        let mut empty: Vec<u8> = Vec::new();
        bgra_to_rgba(&mut empty);
        assert!(empty.is_empty());
    }

    #[test]
    fn fit_passes_through_an_exact_frame() {
        let frame = solid(THUMB_WIDTH, THUMB_HEIGHT, [9, 9, 9, 255]);
        let fitted = fit_contain(&frame, THUMB_WIDTH, THUMB_HEIGHT, [0, 0, 0, 255]).unwrap();
        assert_eq!(fitted, frame);
    }

    #[test]
    fn fit_letterboxes_a_wide_frame_with_vertical_padding() {
        // 400x100 (4:1) into 320x180: scale = min(0.8, 1.8) = 0.8 ->
        // 320x80, padded 50 rows top and bottom with `pad`.
        let frame = solid(400, 100, [255, 0, 0, 255]);
        let fitted = fit_contain(&frame, 320, 180, [1, 2, 3, 255]).unwrap();
        assert_eq!((fitted.width, fitted.height), (320, 180));
        let at = |x: usize, y: usize| {
            let off = (y * 320 + x) * 4;
            &fitted.rgba[off..off + 4]
        };
        assert_eq!(at(0, 0), &[1, 2, 3, 255], "top pad row");
        assert_eq!(at(319, 179), &[1, 2, 3, 255], "bottom pad row");
        // Inside the scaled content the source colour survives (a solid
        // red frame scales to solid red; Triangle cannot invent hues).
        assert_eq!(at(160, 90), &[255, 0, 0, 255], "content centre");
    }

    #[test]
    fn fit_pillarboxes_a_tall_frame_with_horizontal_padding() {
        // 100x400 (1:4) into 320x180: scale = min(3.2, 0.45) = 0.45 ->
        // 45x180, padded left/right.
        let frame = solid(100, 400, [0, 255, 0, 255]);
        let fitted = fit_contain(&frame, 320, 180, [7, 7, 7, 255]).unwrap();
        assert_eq!((fitted.width, fitted.height), (320, 180));
        let at = |x: usize, y: usize| {
            let off = (y * 320 + x) * 4;
            &fitted.rgba[off..off + 4]
        };
        assert_eq!(at(0, 90), &[7, 7, 7, 255], "left pad column");
        assert_eq!(at(319, 90), &[7, 7, 7, 255], "right pad column");
        assert_eq!(at(160, 90), &[0, 255, 0, 255], "content centre");
    }

    #[test]
    fn fit_rejects_an_empty_frame() {
        let frame = Frame {
            width: 0,
            height: 0,
            rgba: Vec::new(),
        };
        assert!(fit_contain(&frame, 320, 180, [0, 0, 0, 255]).is_err());
    }

    #[test]
    fn jpeg_round_trip_keeps_size_and_hues() {
        let frame = solid(THUMB_WIDTH, THUMB_HEIGHT, [200, 40, 40, 255]);
        let jpeg = encode_jpeg(&frame, JPEG_QUALITY).unwrap();
        assert!(!jpeg.is_empty(), "encoder produced no bytes");
        assert_eq!(&jpeg[..2], &[0xFF, 0xD8], "SOI marker");
        let back = decode_jpeg(&jpeg).unwrap();
        assert_eq!((back.width, back.height), (THUMB_WIDTH, THUMB_HEIGHT));
        // Lossy codec: allow a small per-channel drift around the source.
        // Index walk, not `chunks_exact(4)` (workspace MSRV 1.80).
        for index in 0..back.rgba.len() / 4 {
            let got = &back.rgba[index * 4..index * 4 + 4];
            let want = &frame.rgba[index * 4..index * 4 + 4];
            for channel in 0..3 {
                let delta = i16::from(got[channel]) - i16::from(want[channel]);
                assert!(delta.abs() <= 10, "channel drift {delta} > 10");
            }
            assert_eq!(got[3], 255, "alpha stays opaque");
        }
    }

    #[test]
    fn decode_jpeg_rejects_garbage() {
        assert!(decode_jpeg(b"not a jpeg at all").is_err());
    }

    #[test]
    fn frame_new_rejects_a_short_buffer() {
        assert!(Frame::new(2, 2, vec![0u8; 15]).is_err());
        assert!(Frame::new(2, 2, vec![0u8; 16]).is_ok());
    }
}
