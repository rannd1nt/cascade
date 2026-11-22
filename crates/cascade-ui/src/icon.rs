//! GUI window icon (design-m5-p4 §13, I1).
//!
//! The executable resource and the tray already use `img/cascade.ico`
//! (resource id 1), but winit 0.30 registers its window class with a null
//! icon, so the library window and its taskbar button need an icon of their
//! own. egui takes unmultiplied RGBA pixels.
//!
//! The source of truth is `img/cascade.png`: its pixels are identical to
//! the `cascade.ico` frame that Explorer and the taskbar show (measured on
//! the target: MAE 0 after a 256x256 resize). `img/cascade-256.png` is a
//! different, older art and must not be used here.
//!
//! `cascade.png` is 234x241, so it is stretched to the 256x256 square egui
//! wants — exactly what `cascade.ico` does — keeping the window icon and
//! the taskbar icon identical.
//!
//! Decoded only here, in the UI process. The daemon never links this code.

use std::sync::Arc;

use eframe::egui::IconData;
use image::imageops::FilterType;

/// Embedded at compile time so the release stays a single executable
/// (AGENTS §3.10) and cannot drift from the shipped binary.
const PNG: &[u8] = include_bytes!("../../../img/cascade.png");

/// egui wants a square whose edges are multiples of 4; 256 is also the
/// largest frame `cascade.ico` ships, so both icons share one size.
const SIZE: u32 = 256;

/// Decode the embedded PNG and stretch it to the square RGBA image egui
/// expects. Stretching (not padding) mirrors `cascade.ico`'s own 256x256
/// frame, so the window icon matches the taskbar at the same size.
pub fn window_icon() -> Result<Arc<IconData>, String> {
    let decoded = image::load_from_memory_with_format(PNG, image::ImageFormat::Png)
        .map_err(|err| format!("decode window icon: {err}"))?
        .to_rgba8();
    let (source_w, source_h) = decoded.dimensions();
    if source_w == 0 || source_h == 0 {
        return Err(format!("empty window icon {source_w}x{source_h}"));
    }
    let square = image::imageops::resize(&decoded, SIZE, SIZE, FilterType::Triangle);
    let (width, height) = square.dimensions();
    // Guard the output we hand winit: a malformed icon must fail here
    // rather than render a broken window.
    if width != SIZE || height != SIZE || !SIZE.is_multiple_of(4) {
        return Err(format!("invalid window icon size {width}x{height}"));
    }
    Ok(Arc::new(IconData {
        rgba: square.into_raw(),
        width,
        height,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The source asset's exact pixel size, pinned so the window icon can
    /// never silently regress to the wrong art (`cascade-256.png`).
    const SOURCE_DIMENSIONS: (u32, u32) = (234, 241);

    #[test]
    fn embedded_source_is_cascade_png_not_the_old_256_art() {
        // Pins the asset: cascade.png (234x241) is the art shared with
        // cascade.ico. cascade-256.png is 256x256 and is a different logo.
        let decoded = image::load_from_memory_with_format(PNG, image::ImageFormat::Png)
            .expect("embedded icon decodes")
            .to_rgba8();
        assert_eq!(
            decoded.dimensions(),
            SOURCE_DIMENSIONS,
            "window icon must come from img/cascade.png (234x241)"
        );
    }

    #[test]
    fn window_icon_is_square_rgba_with_visible_coloured_pixels() {
        let icon = window_icon().expect("cascade.png is embedded and valid");
        assert_eq!(icon.width, SIZE);
        assert_eq!(icon.height, SIZE);
        assert_eq!(icon.rgba.len(), (SIZE * SIZE * 4) as usize);
        assert!(
            icon.rgba.chunks(4).any(|px| px[3] != 0),
            "the icon must not be fully transparent"
        );
        // The Cascade mark is coloured; a blank or grey placeholder would
        // mean the wrong source got embedded.
        let coloured = icon
            .rgba
            .chunks(4)
            .filter(|px| {
                let max = px[0].max(px[1]).max(px[2]);
                let min = px[0].min(px[1]).min(px[2]);
                max.saturating_sub(min) > 30
            })
            .count();
        assert!(
            coloured > 100,
            "expected the coloured Cascade mark, found {coloured} coloured pixels"
        );
    }
}
