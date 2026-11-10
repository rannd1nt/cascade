//! Pure fit math: where a video frame lands inside the host surface
//! (`design-p1-color.md` §4.5.1, `quality.md` §4.7 — never distort).
//!
//! One function, no Win32 state: the D3D11 video processor consumes the
//! returned rects as stream source/destination rectangles. `Contain` is the
//! only mode the daemon requests today (`PlaybackOptions::default()`); the
//! other two are implemented and unit-tested for completeness (design §4.3).

use cascade_core::Fit;
use windows::Win32::Foundation::RECT;

/// Source and destination rects for one (video, surface) pair.
///
/// * `Contain`: full source → letterboxed destination (black bars are the
///   back-buffer clear outside this rect).
/// * `Cover`: cropped source → full destination.
/// * `Stretch`: full source → full destination.
///
/// Rounding rule: the limiting axis is computed with integer math and the
/// free axis rounds **up**, clamped to the surface — so a sub-pixel aspect
/// remainder can never leave a 1-px sliver of undefined back buffer at an
/// edge (the classic contain artefact), while real letterbox bars stay.
pub(crate) fn fit_rects(fit: Fit, video: (u32, u32), surface: (u32, u32)) -> (RECT, RECT) {
    let (vw, vh) = sane(video);
    let (sw, sh) = sane(surface);
    match fit {
        Fit::Stretch => (full(vw, vh), full(sw, sh)),
        Fit::Contain => {
            // Integer cross-multiplication avoids float rounding drift:
            // vw/vh <= sw/sh  <=>  vw*sh <= vh*sw  → height-limited.
            let (dw, dh) = if i64::from(vw) * i64::from(sh) <= i64::from(vh) * i64::from(sw) {
                // Height-limited: full height, width = ceil(vw*sh/vh).
                (ceil_div(vw, sh, vh).min(sw).max(1), sh)
            } else {
                // Width-limited: full width, height = ceil(vh*sw/vw).
                (sw, ceil_div(vh, sw, vw).min(sh).max(1))
            };
            let left = (sw - dw) / 2;
            let top = (sh - dh) / 2;
            (
                full(vw, vh),
                RECT {
                    left: left as i32,
                    top: top as i32,
                    right: (left + dw) as i32,
                    bottom: (top + dh) as i32,
                },
            )
        }
        Fit::Cover => {
            // Crop the long axis: if the video is relatively wider
            // (vw*sh >= vh*sw), height fills and the width crops; else the
            // reverse. Integer maths, rounded to nearest, clamped inside
            // the frame so the src rect is always a valid sub-rectangle.
            let (sx, sy, cw, ch) = if i64::from(vw) * i64::from(sh) >= i64::from(vh) * i64::from(sw)
            {
                // Width = ceil(sw*vh/sh), centred; height stays full.
                let cw = ceil_div(sw, vh, sh).min(vw).max(1);
                ((vw - cw) / 2, 0, cw, vh)
            } else {
                let ch = ceil_div(sh, vw, sw).min(vh).max(1);
                (0, (vh - ch) / 2, vw, ch)
            };
            (
                RECT {
                    left: sx as i32,
                    top: sy as i32,
                    right: (sx + cw) as i32,
                    bottom: (sy + ch) as i32,
                },
                full(sw, sh),
            )
        }
    }
}

/// Degenerate inputs (0 dimensions) behave as a full-frame stretch instead
/// of producing empty rects the video processor would reject.
fn sane(v: (u32, u32)) -> (u32, u32) {
    if v.0 == 0 || v.1 == 0 {
        (1, 1)
    } else {
        v
    }
}

/// `ceil(a * b / c)` for non-zero `c` (callers pass positive values).
fn ceil_div(a: u32, b: u32, c: u32) -> u32 {
    let (a, b, c) = (u64::from(a), u64::from(b), u64::from(c));
    (a * b).div_ceil(c) as u32
}

fn full(w: u32, h: u32) -> RECT {
    RECT {
        left: 0,
        top: 0,
        right: w as i32,
        bottom: h as i32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Aspect of a rect in per-mille-of-ratio (1778 ≈ 16:9): tests only
    /// compare these ratios against each other, never against floats.
    fn aspect_permille(r: &RECT) -> i64 {
        let w = i64::from(r.right - r.left);
        let h = i64::from(r.bottom - r.top);
        if h == 0 {
            return i64::MAX;
        }
        w * 1000 / h
    }

    #[test]
    fn contain_landscape_video_in_wider_surface_letterboxes_left_right() {
        // 4:3 1440x1080 on a 1366x768 surface: height-limited.
        let (src, dst) = fit_rects(Fit::Contain, (1440, 1080), (1366, 768));
        assert_eq!(
            (src.left, src.top, src.right, src.bottom),
            (0, 0, 1440, 1080)
        );
        // dw = ceil(1440*768/1080) = 1024, centred.
        assert_eq!(
            (dst.left, dst.top, dst.right, dst.bottom),
            (171, 0, 1195, 768)
        );
    }

    #[test]
    fn contain_16x9_in_16x9_never_leaves_a_sliver() {
        // The exact test-machine shape: 1920x1080 onto 1366x768. Exact
        // width is 1365.33 — floor would leave a 1-px undefined column.
        let (src, dst) = fit_rects(Fit::Contain, (1920, 1080), (1366, 768));
        assert_eq!((src.right, src.bottom), (1920, 1080));
        assert_eq!(dst.left, 0);
        assert_eq!(dst.right, 1366, "the free axis must round up to the edge");
        assert_eq!(dst.bottom, 768);
    }

    #[test]
    fn contain_covers_the_surface_with_no_negative_rects() {
        let cases = [
            ((1920u32, 1080u32), (1366u32, 768u32)),
            ((1080, 1920), (1366, 768)),
            ((1440, 1080), (1366, 768)),
            ((1366, 768), (1366, 768)),
            ((640, 480), (1366, 768)),
            ((3840, 2160), (1366, 768)),
        ];
        for (video, surface) in cases {
            let (src, dst) = fit_rects(Fit::Contain, video, surface);
            assert_eq!(src.left, 0, "{video:?}");
            assert_eq!(src.top, 0, "{video:?}");
            assert_eq!((src.right, src.bottom), (video.0 as i32, video.1 as i32));
            assert!(dst.left >= 0 && dst.top >= 0, "{video:?}");
            assert!(dst.right <= surface.0 as i32 && dst.bottom <= surface.1 as i32);
            assert!(dst.right > dst.left && dst.bottom > dst.top, "{video:?}");
            // Centred: the slack splits into two halves differing by ≤ 1 px.
            let slack_x = (surface.0 as i32) - (dst.right - dst.left);
            assert!(
                slack_x - 2 * dst.left <= 1 && 2 * dst.left - slack_x <= 1,
                "{video:?}"
            );
            let slack_y = (surface.1 as i32) - (dst.bottom - dst.top);
            assert!(
                slack_y - 2 * dst.top <= 1 && 2 * dst.top - slack_y <= 1,
                "{video:?}"
            );
        }
    }

    #[test]
    fn contain_preserves_aspect_within_two_pixels() {
        // 21:9 2560x1080 on 1366x768: clearly width-limited → full width,
        // height = ceil(1080*1366/2560) = 577 (exact 576.3), centred.
        let (src, dst) = fit_rects(Fit::Contain, (2560, 1080), (1366, 768));
        let exact_h = i64::from(1080) * 1366 / i64::from(2560); // 576, floored
        let dst_h = i64::from(dst.bottom - dst.top);
        assert!(dst_h - exact_h <= 2 && exact_h - dst_h <= 2, "dst={dst:?}");
        assert_eq!(src.right - src.left, 2560);
        // dst is centred vertically.
        assert_eq!(i64::from(dst.top), (768 - dst_h) / 2);
        assert_eq!(dst.right - dst.left, 1366);
    }

    #[test]
    fn cover_fills_the_surface_and_crops_the_long_axis() {
        // 4:3 video is narrower than the surface → crop height (full width).
        let (src, dst) = fit_rects(Fit::Cover, (1440, 1080), (1366, 768));
        assert_eq!(
            (dst.left, dst.top, dst.right, dst.bottom),
            (0, 0, 1366, 768)
        );
        assert_eq!((src.left, src.right), (0, 1440));
        // ch = ceil(768*1440/1366) = 810, centred: top = (1080-810)/2 = 135.
        assert_eq!((src.top, src.bottom), (135, 945));
    }

    #[test]
    fn cover_wide_video_crops_the_width() {
        // 21:9 2560x1080 on 1366x768: relatively wide → crop width.
        let (src, dst) = fit_rects(Fit::Cover, (2560, 1080), (1366, 768));
        assert_eq!(
            (dst.left, dst.top, dst.right, dst.bottom),
            (0, 0, 1366, 768)
        );
        assert_eq!((src.top, src.bottom), (0, 1080));
        // cw = ceil(1366*1080/768) = ceil(1920.94) = 1921, centred:
        // left = (2560-1921)/2 = 319, right = 319+1921 = 2240.
        assert_eq!((src.left, src.right), (319, 2240));
    }

    #[test]
    fn stretch_is_exactly_full_rect() {
        let (src, dst) = fit_rects(Fit::Stretch, (1920, 1080), (1366, 768));
        assert_eq!((src.right, src.bottom), (1920, 1080));
        assert_eq!((dst.right, dst.bottom), (1366, 768));
        assert_eq!((src.left, src.top, dst.left, dst.top), (0, 0, 0, 0));
    }

    #[test]
    fn degenerate_inputs_never_produce_empty_rects() {
        for fit in [Fit::Contain, Fit::Cover, Fit::Stretch] {
            let (src, dst) = fit_rects(fit, (0, 0), (1366, 768));
            assert!(src.right > src.left && src.bottom > src.top, "{fit:?}");
            assert!(dst.right > dst.left && dst.bottom > dst.top, "{fit:?}");
            let (src, dst) = fit_rects(fit, (1920, 1080), (0, 0));
            assert!(src.right > src.left && src.bottom > src.top, "{fit:?}");
            assert!(dst.right > dst.left && dst.bottom > dst.top, "{fit:?}");
        }
    }

    #[test]
    fn cover_src_is_always_inside_the_frame() {
        let cases = [
            ((1920u32, 1080u32), (1366u32, 768u32)),
            ((1080, 1920), (1366, 768)),
            ((1440, 1080), (1366, 768)),
            ((640, 480), (1366, 768)),
        ];
        for (video, surface) in cases {
            let (src, dst) = fit_rects(Fit::Cover, video, surface);
            assert_eq!((dst.left, dst.top), (0, 0), "{video:?}");
            assert_eq!(
                (dst.right, dst.bottom),
                (surface.0 as i32, surface.1 as i32)
            );
            assert!(src.left >= 0 && src.top >= 0, "{video:?} {src:?}");
            assert!(
                src.right <= video.0 as i32 && src.bottom <= video.1 as i32,
                "{video:?}"
            );
            assert!(src.right > src.left && src.bottom > src.top, "{video:?}");
            // Cropped src aspect ≈ surface aspect: within 0.5 % (integer
            // rect rounding swings small rects by up to ~6 per-mille).
            let sa = aspect_permille(&src);
            let da = aspect_permille(&dst);
            assert!(
                (sa - da).abs() <= da / 200,
                "{video:?} src={src:?} dst={dst:?}"
            );
        }
    }
}
