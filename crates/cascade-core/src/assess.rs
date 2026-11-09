//! The assessment engine (design-m4 §5.2): pure data in, structured
//! warnings out — one unit test per metric row M1–M9.
//!
//! D13 leak rules apply to every caller: this module allocates plain data
//! only. It never creates an engine, swapchain, D3D device, or Media
//! Foundation object, and it never runs the load pipeline — the caller
//! probes the file first (container attributes, SourceReader dropped in
//! that call) and hands the numbers here. Warnings are derived at read
//! time from live inputs, never persisted, so a GPU or monitor change
//! cannot stale them.
//!
//! Severities: `info` and `warn` never block anything (they are printed
//! and the user's request applies — design-m4 §5.3); `error` is the only
//! severity that blocks `wallpaper.set`, and only the codec policy
//! (`architecture.md` §6) and HDR produce it.

use serde::{Deserialize, Serialize};

use crate::device::{codec_support, level_name, profile_name, DeviceProfile, MonitorProfile};
use crate::library::VideoMetadata;
use crate::Fit;

// --- Policy thresholds (constants in one place, design-m4 §5.2) -------------
// These are policy, not measurements: tuned only with measured evidence
// (AGENTS §8). Every one is exercised by a unit test below.

/// M3: below this fraction of the display on both axes the upscale is
/// visible (soft image).
const MIN_UPSCALE_RATIO: f64 = 0.70;
/// M3: above this fraction of the display on both axes the extra pixels
/// are decoded but never shown.
const MAX_WASTE_RATIO: f64 = 1.50;
/// M4: aspect-ratio mismatch tolerance (1 %).
const AR_TOLERANCE: f64 = 0.01;
/// M5: at or below this frame rate on a faster display, motion judders.
const MAX_JUDDER_FPS: f64 = 24.0;
/// M8: warn below this much free VRAM after surface allocations.
const VRAM_HEADROOM_WARN_MB: u64 = 256;
/// M8: adapters at or below this dedicated size are aperture iGPUs (the
/// measured target reports 128 MB dedicated) — VRAM headroom on such a
/// machine is system RAM, which M9 already covers. Skipping avoids a
/// permanent false positive (amended design-m4 §5.2, S5).
const VRAM_APERTURE_MAX_MB: u64 = 256;
/// M9: total RAM floors.
const RAM_MIN_MB: u64 = 4096;
const RAM_4K_MIN_MB: u64 = 8192;
/// 4K video detection for M9.
const UHD_WIDTH: u32 = 3840;
const UHD_HEIGHT: u32 = 2160;
/// M8 surface estimate: double-buffered RGBA8 swapchain per monitor…
const SWAPCHAIN_BUFFERS: u64 = 2;
/// …and H.264's maximum reference-frame set for the decode surfaces (NV12,
/// macroblock-aligned). An estimate by design — labelled as such on the
/// wire, never presented as a measurement.
const DECODE_SURFACES: u64 = 16;
/// M6: bits-per-pixel-per-frame budget keyed by codec class.
const BPP_H264: f64 = 0.10;
const BPP_MODERN_HIGH_EFFICIENCY: f64 = 0.06; // HEVC, VP9
const BPP_AV1: f64 = 0.05;
const BPP_LEGACY: f64 = 0.20; // MPEG-2, WMV9
const BPP_DEFAULT: f64 = 0.10;

/// Which metric produced a warning (`ipc.md` §9; stable wire strings —
/// the M5 GUI badge keys off them).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Metric {
    Codec,
    Level,
    Resolution,
    Aspect,
    // "fps", not serde's automatic "frame_rate": the wire string predates
    // this module (status/monitor rows) and the M5 badge keys off it.
    #[serde(rename = "fps")]
    FrameRate,
    Bitrate,
    Hdr,
    Vram,
    Ram,
}

impl Metric {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Codec => "codec",
            Self::Level => "level",
            Self::Resolution => "resolution",
            Self::Aspect => "aspect",
            Self::FrameRate => "fps",
            Self::Bitrate => "bitrate",
            Self::Hdr => "hdr",
            Self::Vram => "vram",
            Self::Ram => "ram",
        }
    }
}

/// How much a warning matters. Only [`Severity::Error`] blocks a set
/// (design-m4 §5.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Info,
    Warn,
    Error,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }
}

/// One structured warning (design-m4 §5.2): the record the CLI prints and
/// the M5 GUI `(i)` badge renders. `detail` carries the measured values;
/// `consequence` and `fix` are plain English, derived from them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Warning {
    pub metric: Metric,
    pub severity: Severity,
    pub detail: String,
    pub consequence: String,
    pub fix: String,
}

/// The "format video so it is not cropped" answer (design-m4 §5.2):
/// `width` × `height` is the exact display resolution (never asks the
/// user to change their screen), `note` names the standard 16:9 ladder
/// alternative when this display's AR matches one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recommendation {
    pub width: u32,
    pub height: u32,
    pub note: String,
}

/// Everything `assess()` answers for one video on one monitor.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Assessment {
    pub warnings: Vec<Warning>,
    pub recommendation: Recommendation,
}

impl Assessment {
    /// Any blocking warning (design-m4 §5.3 step 2)? The first error is
    /// the rejection message `wallpaper.set` sends.
    pub fn first_error(&self) -> Option<&Warning> {
        self.warnings
            .iter()
            .find(|warning| warning.severity == Severity::Error)
    }
}

/// Normalise a probe codec label to the device-side fourcc key so the two
/// sides of D12 compare equal (`H264` → `h264`, `AV1` → `av01`, …).
/// Unknown labels lowercase verbatim — never renamed to something we
/// did not measure.
pub fn codec_key(raw: &str) -> String {
    let lowered = raw.trim().to_ascii_lowercase();
    match lowered.as_str() {
        "av1" => "av01".to_string(),
        "wmv3" | "wmv2" | "wmv1" => "wmv9".to_string(),
        "h265" => "hevc".to_string(),
        "avc" | "avc1" | "avc3" => "h264".to_string(),
        other => other.to_string(),
    }
}

/// Is this codec inside the v1 supported set (`architecture.md` §6)?
/// H.264 8-bit 4:2:0 only — everything else is an `error` severity and the
/// single class of warning that blocks `wallpaper.set`.
pub fn policy_supported(codec_key: &str) -> bool {
    codec_key == "h264"
}

/// H.264 profiles outside the 8-bit 4:2:0 baseline (ITU-T H.264 Annex A:
/// High10/High422/High444).
fn is_high_bit_depth_profile(profile: u32) -> bool {
    matches!(profile, 110 | 122 | 244)
}

/// Evaluate every metric row for one video on one target monitor.
///
/// `device` is the live [`DeviceProfile`] (collected fresh per event —
/// D13: warnings are derived at read time), `monitor` is the assigned
/// output, `fit` is the playback fit the engine would use.
pub fn assess(
    video: &VideoMetadata,
    device: &DeviceProfile,
    monitor: &MonitorProfile,
    fit: Fit,
) -> Assessment {
    let codec = codec_key(&video.codec);
    let support = device.decode.as_ref().map(codec_support);
    let mut warnings = Vec::new();

    m1_codec(video, &codec, support.as_ref(), &mut warnings);
    m2_level(video, &codec, support.as_ref(), &mut warnings);
    m3_resolution(video, monitor, &mut warnings);
    m4_aspect(video, monitor, fit, &mut warnings);
    m5_frame_rate(video, monitor, &mut warnings);
    m6_bitrate(video, &codec, &mut warnings);
    m7_hdr(video, monitor, &mut warnings);
    m8_vram(device, video, &mut warnings);
    m9_ram(device, video, &mut warnings);

    Assessment {
        warnings,
        recommendation: recommend(monitor),
    }
}

/// M1: codec in the device decode set (D12: D3D11 GPU profiles ∪ hardware
/// MFT input types) — `warn` when it is not ("plays, expect high CPU");
/// the explicit policy rejects are `error`.
fn m1_codec(
    video: &VideoMetadata,
    codec: &str,
    support: Option<&std::collections::BTreeMap<String, crate::device::CodecSupport>>,
    warnings: &mut Vec<Warning>,
) {
    let raw = video.codec.as_str();
    if !policy_supported(codec) {
        warnings.push(Warning {
            metric: Metric::Codec,
            severity: Severity::Error,
            detail: format!("codec policy: {raw} ({codec}) is outside the v1 supported set"),
            consequence: "Cascade v1 will not play this file — there is no software fallback \
                          (architecture.md §6 codec policy)"
                .to_string(),
            fix: "re-encode to H.264 (8-bit 4:2:0) to play it here".to_string(),
        });
        return;
    }
    if let Some(profile) = video
        .profile
        .as_deref()
        .and_then(|value| value.parse::<u32>().ok())
    {
        if is_high_bit_depth_profile(profile) {
            warnings.push(Warning {
                metric: Metric::Codec,
                severity: Severity::Error,
                detail: format!(
                    "codec policy: H.264 {} profile ({profile}) is not 8-bit 4:2:0",
                    profile_name(codec, profile)
                ),
                consequence: "the v1 pipeline decodes NV12 (8-bit 4:2:0) only; higher bit depth \
                              and chroma subsampling are not decoded"
                    .to_string(),
                fix: "re-encode at High profile, 8-bit 4:2:0".to_string(),
            });
            return;
        }
    }
    let Some(support) = support else {
        // Device decode capability was not collected — no measurement, no
        // claim (never report an absence we did not observe).
        return;
    };
    let Some(entry) = support.get(codec) else {
        warnings.push(Warning {
            metric: Metric::Codec,
            severity: Severity::Warn,
            detail: format!(
                "no hardware {raw} decoder reported (D3D11 GPU profiles union hardware MFTs)"
            ),
            consequence: "video plays in software — expect high CPU use, heat and battery drain"
                .to_string(),
            fix: "it still plays; update the GPU driver if the machine should decode it \
                  (see `cascade doctor`)"
                .to_string(),
        });
        return;
    };
    // GPU-only codecs carry empty sets: "supported, detail unknown" (D12).
    // Only a machine that advertises detail gets the profile comparison.
    if entry.profiles.is_empty() {
        return;
    }
    let file_profile = video
        .profile
        .as_deref()
        .and_then(|value| value.parse::<u32>().ok());
    if let Some(profile) = file_profile {
        if !entry.profiles.contains(&profile) {
            let names: Vec<String> = entry
                .profiles
                .iter()
                .map(|known| profile_name(codec, *known))
                .collect();
            warnings.push(Warning {
                metric: Metric::Codec,
                severity: Severity::Warn,
                detail: format!(
                    "H.264 {} profile is not in the device decode set ({})",
                    profile_name(codec, profile),
                    names.join(", ")
                ),
                consequence: "video plays, but expect high CPU use and battery drain".to_string(),
                fix: format!("re-encode at {} profile", names.join(" or ")),
            });
        }
    }
}

/// M2: file level vs the device's advertised maximum level (H.264/HEVC
/// level numbers are monotonic in both encodings, so a numeric max works).
fn m2_level(
    video: &VideoMetadata,
    codec: &str,
    support: Option<&std::collections::BTreeMap<String, crate::device::CodecSupport>>,
    warnings: &mut Vec<Warning>,
) {
    if !matches!(codec, "h264" | "hevc") {
        return;
    }
    let Some(file_level) = video.level.as_deref().and_then(|v| v.parse::<u32>().ok()) else {
        return;
    };
    let Some(max_device) = support
        .and_then(|map| map.get(codec))
        .map(|entry| entry.levels.clone())
        .filter(|levels| !levels.is_empty())
        .and_then(|levels| levels.into_iter().max())
    else {
        // No advertised level detail (GPU-only codec, D12): unknown, skip.
        return;
    };
    if file_level > max_device {
        warnings.push(Warning {
            metric: Metric::Level,
            severity: Severity::Warn,
            detail: format!(
                "file level {} exceeds the device maximum {}",
                level_name(codec, file_level),
                level_name(codec, max_device)
            ),
            consequence: "the hardware decoder may refuse this stream and playback falls back \
                          to software"
                .to_string(),
            fix: format!(
                "re-encode at level {} or below",
                level_name(codec, max_device)
            ),
        });
    }
}

/// M3: resolution vs display pixels — too small on both axes (visible
/// upscale) or too large on both axes (decode cost for pixels never seen).
fn m3_resolution(video: &VideoMetadata, monitor: &MonitorProfile, warnings: &mut Vec<Warning>) {
    let (mw, mh) = (f64::from(monitor.rect.w), f64::from(monitor.rect.h));
    let (vw, vh) = (f64::from(video.width), f64::from(video.height));
    if mw <= 0.0 || mh <= 0.0 || vw <= 0.0 || vh <= 0.0 {
        return;
    }
    let (rw, rh) = (vw / mw, vh / mh);
    if rw < MIN_UPSCALE_RATIO && rh < MIN_UPSCALE_RATIO {
        warnings.push(Warning {
            metric: Metric::Resolution,
            severity: Severity::Warn,
            detail: format!(
                "{}x{} covers {:.0}% x {:.0}% of the {}x{} display",
                video.width,
                video.height,
                rw * 100.0,
                rh * 100.0,
                monitor.rect.w,
                monitor.rect.h
            ),
            consequence: "the image is upscaled to fill the screen — soft edges and visible \
                          blur"
                .to_string(),
            fix: "encode at the display resolution or 1920x1080 (see the recommendation below)"
                .to_string(),
        });
    } else if rw > MAX_WASTE_RATIO && rh > MAX_WASTE_RATIO {
        warnings.push(Warning {
            metric: Metric::Resolution,
            severity: Severity::Warn,
            detail: format!(
                "{}x{} is {:.2}x the {}x{} display's pixel count per axis",
                video.width, video.height, rw, monitor.rect.w, monitor.rect.h
            ),
            consequence: "the GPU decodes pixels the display never shows — extra decode cost \
                          and VRAM for nothing"
                .to_string(),
            fix: "re-encode at the display resolution (see the recommendation below)".to_string(),
        });
    }
}

/// M4: aspect ratio vs the display, phrased for the actual `Fit` — bars
/// with `Contain`, cropped area with `Cover`, nothing with `Stretch`.
fn m4_aspect(
    video: &VideoMetadata,
    monitor: &MonitorProfile,
    fit: Fit,
    warnings: &mut Vec<Warning>,
) {
    if fit == Fit::Stretch || monitor.rect.h <= 0 || video.height == 0 {
        return;
    }
    let video_ar = f64::from(video.width) / f64::from(video.height);
    let display_ar = f64::from(monitor.rect.w) / f64::from(monitor.rect.h);
    if display_ar <= 0.0 {
        return;
    }
    let delta = (video_ar - display_ar).abs() / display_ar;
    if delta <= AR_TOLERANCE {
        return;
    }
    let video_is_wider = video_ar > display_ar;
    // Symmetric fractions: with Contain, bars cover 1 - display_ar/video_ar
    // (or its reciprocal) of the long axis; with Cover, the same fraction
    // of the video is cropped away.
    let fraction = if video_is_wider {
        1.0 - display_ar / video_ar
    } else {
        1.0 - video_ar / display_ar
    };
    let (consequence, fix) = match fit {
        Fit::Contain => {
            let where_ = if video_is_wider {
                "width (left and right)"
            } else {
                "height (top and bottom)"
            };
            (
                format!(
                    "black bars take {:.0}% of the display {} — the video does not fill the screen",
                    fraction * 100.0,
                    where_
                ),
                format!(
                    "encode at {}x{} (the display's aspect ratio) to avoid bars",
                    monitor.rect.w, monitor.rect.h
                ),
            )
        }
        Fit::Cover => {
            let where_ = if video_is_wider {
                "sides"
            } else {
                "top and bottom"
            };
            (
                format!(
                    "the crop takes {:.0}% of the video's {} — part of the picture never shows",
                    fraction * 100.0,
                    where_
                ),
                format!(
                    "encode at {}x{} (the display's aspect ratio) to avoid the crop",
                    monitor.rect.w, monitor.rect.h
                ),
            )
        }
        Fit::Stretch => (String::new(), String::new()), // unreachable, see guard
    };
    warnings.push(Warning {
        metric: Metric::Aspect,
        severity: Severity::Warn,
        detail: format!(
            "video aspect ratio {:.4} vs display {:.4} ({:.2}% off)",
            video_ar,
            display_ar,
            delta * 100.0
        ),
        consequence,
        fix,
    });
}

/// M5: frame rate vs display refresh — faster than the display wastes
/// decode (`info`), slow content on a fast display judders (`warn`).
fn m5_frame_rate(video: &VideoMetadata, monitor: &MonitorProfile, warnings: &mut Vec<Warning>) {
    let (Some(fps), Some(hz)) = (video.fps, monitor.refresh_hz) else {
        return;
    };
    if fps <= 0.0 || hz == 0 {
        return;
    }
    let hz_f = f64::from(hz);
    if fps > hz_f {
        warnings.push(Warning {
            metric: Metric::FrameRate,
            severity: Severity::Info,
            detail: format!(
                "{fps:.0} fps content on a {hz} Hz display (extra {:.0} fps)",
                fps - hz_f
            ),
            consequence: "frames above the display's refresh rate are decoded and discarded — \
                          wasted decode time"
                .to_string(),
            fix: format!("encode at {hz} fps, or cap playback with a max-fps setting"),
        });
    } else if fps <= MAX_JUDDER_FPS && hz_f > MAX_JUDDER_FPS {
        warnings.push(Warning {
            metric: Metric::FrameRate,
            severity: Severity::Warn,
            detail: format!("{fps:.0} fps content on a {hz} Hz display"),
            consequence: format!(
                "{hz}/{fps:.0} is not a whole multiple — motion stutters (judder)"
            ),
            fix: "re-encode at 25, 30 or 60 fps, or pick a display mode that divides the \
                  content's frame rate"
                .to_string(),
        });
    }
}

/// M6: bitrate vs the decode budget for (resolution × frame rate × codec
/// class). Budgets are policy constants — measured evidence can move them
/// (AGENTS §8), a single loud file cannot.
fn m6_bitrate(video: &VideoMetadata, codec: &str, warnings: &mut Vec<Warning>) {
    let (Some(bitrate), Some(fps)) = (video.bitrate_bps, video.fps) else {
        return;
    };
    if video.width == 0 || video.height == 0 || fps <= 0.0 {
        return;
    }
    let budget = f64::from(video.width) * f64::from(video.height) * fps * bpp_budget(codec);
    if (bitrate as f64) <= budget {
        return;
    }
    warnings.push(Warning {
        metric: Metric::Bitrate,
        severity: Severity::Warn,
        detail: format!(
            "{} kbps vs {:.0} kbps budget for {}x{}@{:.0} ({:.2} bits/pixel for {codec})",
            bitrate / 1000,
            budget / 1000.0,
            video.width,
            video.height,
            fps,
            bpp_budget(codec)
        ),
        consequence: "above the decode budget for this size and frame rate — expect decoder \
                      strain or dropped frames on weaker hardware"
            .to_string(),
        fix: format!("re-encode at {:.0} kbps or below", budget / 1000.0),
    });
}

fn bpp_budget(codec: &str) -> f64 {
    match codec {
        "h264" => BPP_H264,
        "hevc" | "vp90" => BPP_MODERN_HIGH_EFFICIENCY,
        "av01" => BPP_AV1,
        "mpeg2" | "wmv9" => BPP_LEGACY,
        _ => BPP_DEFAULT,
    }
}

/// M7: HDR — HDR content is a v1 policy `error` (no tone mapping,
/// `architecture.md` §6); SDR content on an HDR display is a `warn`.
fn m7_hdr(video: &VideoMetadata, monitor: &MonitorProfile, warnings: &mut Vec<Warning>) {
    if video.hdr == Some(true) {
        warnings.push(Warning {
            metric: Metric::Hdr,
            severity: Severity::Error,
            detail: "codec policy: HDR transfer function (PQ/HLG) in the file".to_string(),
            consequence: "Cascade v1 has no tone mapping — HDR content renders washed out or \
                          clipped (architecture.md §6)"
                .to_string(),
            fix: "re-encode in SDR (BT.709) to play it here".to_string(),
        });
        return;
    }
    if monitor.hdr == Some(true) {
        warnings.push(Warning {
            metric: Metric::Hdr,
            severity: Severity::Warn,
            detail: "display is in HDR mode; this video is SDR (or its transfer function is \
                     not declared)"
                .to_string(),
            consequence: "Windows tone-maps the wallpaper on an HDR display — brightness and \
                          colors may look off"
                .to_string(),
            fix: "turn HDR off for this display, or accept Windows' tone mapping".to_string(),
        });
    }
}

/// M8: VRAM headroom after the surfaces this session needs (estimate —
/// swapchains for every monitor plus the decode surfaces). Skipped on
/// aperture iGPUs (dedicated ≤ 256 MB, the measured target): their "VRAM"
/// is system RAM and M9 already reports memory pressure there.
fn m8_vram(device: &DeviceProfile, video: &VideoMetadata, warnings: &mut Vec<Warning>) {
    let dedicated = device
        .gpus
        .iter()
        .map(|gpu| gpu.dedicated_mb)
        .max()
        .unwrap_or(0);
    if dedicated == 0 || dedicated <= VRAM_APERTURE_MAX_MB {
        return;
    }
    let surfaces = estimated_surface_mb(device, video);
    if dedicated.saturating_sub(surfaces) >= VRAM_HEADROOM_WARN_MB {
        return;
    }
    warnings.push(Warning {
        metric: Metric::Vram,
        severity: Severity::Warn,
        detail: format!(
            "{} MB dedicated VRAM, est. {} MB of surfaces ({} monitors + decode), {} MB headroom \
             left",
            dedicated,
            surfaces,
            device.monitors.len(),
            dedicated.saturating_sub(surfaces)
        ),
        consequence: "below 256 MB headroom — texture allocations may fail and the driver may \
                      evict, causing flicker or a device reset"
            .to_string(),
        fix: "play a lower-resolution video or reduce the number of active monitors".to_string(),
    });
}

/// The M8 surface estimate (labelled "est." everywhere it is shown).
fn estimated_surface_mb(device: &DeviceProfile, video: &VideoMetadata) -> u64 {
    let mut bytes: u64 = 0;
    for monitor in &device.monitors {
        let area = monitor.rect.w.max(0) as u64 * monitor.rect.h.max(0) as u64;
        bytes += SWAPCHAIN_BUFFERS * area * 4; // RGBA8 back+front buffer
    }
    // Macroblock-aligned NV12 decode surfaces (16x16 luma blocks).
    let dw = u64::from(video.width).div_ceil(16) * 16;
    let dh = u64::from(video.height).div_ceil(16) * 16;
    bytes += DECODE_SURFACES * dw * dh * 3 / 2;
    bytes.div_ceil(1024 * 1024)
}

/// M9: system RAM headroom — total below 4 GB warns; below 8 GB with 4K
/// content warns (decode buffers plus a low-RAM machine).
fn m9_ram(device: &DeviceProfile, video: &VideoMetadata, warnings: &mut Vec<Warning>) {
    let Some(ram) = device.ram.as_ref() else {
        return;
    };
    let is_4k = video.width >= UHD_WIDTH || video.height >= UHD_HEIGHT;
    if ram.total_mb < RAM_MIN_MB {
        warnings.push(Warning {
            metric: Metric::Ram,
            severity: Severity::Warn,
            detail: format!("{} MB total RAM", ram.total_mb),
            consequence: "the engine plus the desktop needs headroom — a low-RAM machine will \
                          page and stutter"
                .to_string(),
            fix: "no fix on Cascade's side; a lighter video helps".to_string(),
        });
    } else if ram.total_mb < RAM_4K_MIN_MB && is_4k {
        warnings.push(Warning {
            metric: Metric::Ram,
            severity: Severity::Warn,
            detail: format!(
                "{} MB total RAM with {}x{} content",
                ram.total_mb, video.width, video.height
            ),
            consequence: "4K decode buffers on a low-RAM machine — memory pressure while \
                          other apps run"
                .to_string(),
            fix: "re-encode at 1920x1080 (see the recommendation below)".to_string(),
        });
    }
}

/// The "format video so it is not cropped" answer for one display
/// (design-m4 §5.2): the exact display resolution preserves the display's
/// AR perfectly and never asks the user to change their screen; the note
/// names the smallest standard 16:9 ladder size that still covers the
/// display, with its AR delta, or says honestly that no ladder matches.
pub fn recommend(monitor: &MonitorProfile) -> Recommendation {
    let width = monitor.rect.w.max(0) as u32;
    let height = monitor.rect.h.max(0) as u32;
    if width == 0 || height == 0 {
        return Recommendation {
            width,
            height,
            note: "display reports no usable resolution".to_string(),
        };
    }
    let display_ar = f64::from(width) / f64::from(height);
    const LADDER: [(u32, u32); 4] = [(1280, 720), (1920, 1080), (2560, 1440), (3840, 2160)];
    // Closest ladder AR for the "no match" wording.
    let closest_delta = LADDER
        .iter()
        .map(|(w, h)| {
            let ar = f64::from(*w) / f64::from(*h);
            (ar - display_ar).abs() / display_ar
        })
        .fold(f64::INFINITY, f64::min);
    // Smallest ladder size that covers the display on both axes; when the
    // display is bigger than 4K, the largest ladder (scaled down).
    let covering = LADDER
        .iter()
        .find(|(w, h)| *w >= width && *h >= height)
        .or_else(|| LADDER.last());
    let note = match covering {
        Some(&(lw, lh)) => {
            let ladder_ar = f64::from(lw) / f64::from(lh);
            let delta = (ladder_ar - display_ar).abs() / display_ar;
            if delta > AR_TOLERANCE {
                format!(
                    "no standard 16:9 encode matches this display (closest delta \
                     {:.2}%); encode at display resolution",
                    delta * 100.0
                )
            } else if lw == width && lh == height {
                format!("{lw}x{lh} is a standard encode size and fits this display exactly")
            } else {
                format!(
                    "or {lw}x{lh} (AR {ladder_ar:.4}, delta {:.2}%) encoded and scaled to fit",
                    delta * 100.0
                )
            }
        }
        None => format!(
            "no standard 16:9 encode matches this display (closest delta {:.2}%); encode at \
             display resolution",
            closest_delta * 100.0
        ),
    };
    Recommendation {
        width,
        height,
        note,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::{DecodeCapability, GpuAdapter, RamInfo};
    use crate::Rect;

    /// The measured baseline: H.264 1080p60 on the target's 1366x768@60Hz
    /// SDR display with a device that reports h264 decode — must produce
    /// zero warnings, or one of the thresholds is wrong for this machine.
    fn baseline_video() -> VideoMetadata {
        VideoMetadata {
            width: 1920,
            height: 1080,
            fps: Some(60.0),
            duration_ms: Some(60_000),
            bitrate_bps: Some(8_000_000),
            codec: "H264".to_string(),
            profile: Some("100".to_string()),
            level: Some("41".to_string()),
            hdr: Some(false),
        }
    }

    fn baseline_monitor() -> MonitorProfile {
        MonitorProfile {
            id: r"\\.\DISPLAY1".to_string(),
            rect: Rect::new(0, 0, 1366, 768),
            work: Rect::new(0, 0, 1366, 728),
            primary: true,
            refresh_hz: Some(60),
            dpi: Some(96),
            hdr: Some(false),
            bits_per_color: Some(8),
            color_space: Some("SDR (BT.709)".to_string()),
        }
    }

    fn baseline_device() -> DeviceProfile {
        DeviceProfile {
            monitors: vec![baseline_monitor()],
            gpus: vec![GpuAdapter {
                name: "Test GPU".to_string(),
                vendor_id: 0x8086,
                dedicated_mb: 128, // aperture iGPU: M8 skips (S5 amendment)
                shared_mb: 8060,
                driver_version: Some("30.0.101.1960".to_string()),
            }],
            cpu: None,
            ram: Some(RamInfo {
                total_mb: 16122,
                available_mb: 9000,
            }),
            decode: Some(DecodeCapability {
                decoders: Vec::new(),
                gpu_codecs: vec!["av01".to_string(), "h264".to_string(), "hevc".to_string()],
            }),
        }
    }

    fn warnings_for(
        video: &VideoMetadata,
        device: &DeviceProfile,
        monitor: &MonitorProfile,
    ) -> Vec<Warning> {
        assess(video, device, monitor, Fit::Contain).warnings
    }

    /// Design-m4 §5.2 contract: the measured baseline is clean.
    #[test]
    fn baseline_produces_no_warnings() {
        let warnings = warnings_for(&baseline_video(), &baseline_device(), &baseline_monitor());
        assert!(warnings.is_empty(), "{warnings:#?}");
    }

    /// Metric ids are wire strings the M5 badge keys off — assert serde
    /// and `as_str` agree so the two can never drift.
    #[test]
    fn metric_and_severity_wire_strings_are_stable() {
        for (metric, expected) in [
            (Metric::Codec, "codec"),
            (Metric::Level, "level"),
            (Metric::Resolution, "resolution"),
            (Metric::Aspect, "aspect"),
            (Metric::FrameRate, "fps"),
            (Metric::Bitrate, "bitrate"),
            (Metric::Hdr, "hdr"),
            (Metric::Vram, "vram"),
            (Metric::Ram, "ram"),
        ] {
            assert_eq!(metric.as_str(), expected);
            let json = serde_json::to_string(&metric).expect("json");
            assert_eq!(json, format!("\"{expected}\""));
        }
        assert_eq!(Severity::Info.as_str(), "info");
        assert_eq!(Severity::Warn.as_str(), "warn");
        assert_eq!(Severity::Error.as_str(), "error");
        assert_eq!(
            serde_json::to_string(&Severity::Error).expect("json"),
            "\"error\""
        );
    }

    /// Probe labels are uppercase (`H264`) while D12 device labels are
    /// lowercase fourcc (`h264`); aliases line the two vocabularies up.
    #[test]
    fn codec_key_normalises_probe_labels_to_device_labels() {
        assert_eq!(codec_key("H264"), "h264");
        assert_eq!(codec_key("HEVC"), "hevc");
        assert_eq!(codec_key("AV1"), "av01");
        assert_eq!(codec_key("WMV3"), "wmv9");
        assert_eq!(codec_key("h265"), "hevc");
        assert_eq!(codec_key("avc1"), "h264");
        assert_eq!(codec_key("VP90"), "vp90");
        assert_eq!(codec_key("  Mpeg2 "), "mpeg2");
        // Unknown codecs lowercase verbatim — never renamed to a guess.
        assert_eq!(codec_key("THEODORA"), "theodora");
        assert!(policy_supported(&codec_key("H264")));
        assert!(!policy_supported(&codec_key("HEVC")));
    }

    /// M1: policy-reject codec → `error`, the only blocking class.
    #[test]
    fn m1_policy_rejected_codec_is_an_error() {
        let mut video = baseline_video();
        video.codec = "HEVC".to_string();
        video.bitrate_bps = Some(1_000_000); // under the M6 budget for HEVC
        let warnings = warnings_for(&video, &baseline_device(), &baseline_monitor());
        assert_eq!(warnings.len(), 1, "{warnings:#?}");
        assert_eq!(warnings[0].metric, Metric::Codec);
        assert_eq!(warnings[0].severity, Severity::Error);
        assert!(assess(
            &video,
            &baseline_device(),
            &baseline_monitor(),
            Fit::Contain
        )
        .first_error()
        .is_some());
    }

    /// M1: a high-bit-depth H.264 profile is a policy error too
    /// (architecture §6: 8-bit 4:2:0 only).
    #[test]
    fn m1_high_bit_depth_profile_is_an_error() {
        let mut video = baseline_video();
        video.profile = Some("110".to_string()); // High10
        let warnings = warnings_for(&video, &baseline_device(), &baseline_monitor());
        assert_eq!(warnings.len(), 1, "{warnings:#?}");
        assert_eq!(warnings[0].severity, Severity::Error);
        assert!(warnings[0].detail.contains("High10"), "{warnings:#?}");
    }

    /// M1: codec absent from the D12 device set → `warn` ("plays, expect
    /// high CPU"), never an error.
    #[test]
    fn m1_missing_hardware_decode_warns_but_does_not_block() {
        let mut device = baseline_device();
        device.decode = Some(DecodeCapability {
            decoders: Vec::new(),
            gpu_codecs: vec!["av01".to_string()], // no h264 at all
        });
        let warnings = warnings_for(&baseline_video(), &device, &baseline_monitor());
        assert_eq!(warnings.len(), 1, "{warnings:#?}");
        assert_eq!(warnings[0].metric, Metric::Codec);
        assert_eq!(warnings[0].severity, Severity::Warn);
    }

    /// M1 (D12): a GPU-only codec advertises no profile detail — that is
    /// "supported, detail unknown", never a false "no hardware decode".
    #[test]
    fn m1_gpu_only_codec_with_unknown_detail_does_not_warn() {
        let device = baseline_device(); // gpu-only h264, empty profile sets
        let warnings = warnings_for(&baseline_video(), &device, &baseline_monitor());
        assert!(warnings.is_empty(), "{warnings:#?}");
    }

    /// M1: when the device *does* advertise profile detail and the file
    /// sits outside it → `warn`.
    #[test]
    fn m1_profile_outside_the_device_set_warns() {
        let mut device = baseline_device();
        device.decode = Some(DecodeCapability {
            decoders: vec![crate::device::HardwareDecoder {
                name: "test decoder".to_string(),
                inputs: vec![crate::device::DecodeInput {
                    subtype: "h264".to_string(),
                    profile: Some(100),
                    level: Some(51),
                }],
            }],
            gpu_codecs: vec!["h264".to_string()],
        });
        let mut video = baseline_video();
        video.profile = Some("66".to_string()); // Baseline, not advertised
        let warnings = warnings_for(&video, &device, &baseline_monitor());
        assert_eq!(warnings.len(), 1, "{warnings:#?}");
        assert_eq!(warnings[0].metric, Metric::Codec);
        assert_eq!(warnings[0].severity, Severity::Warn);
        assert!(warnings[0].detail.contains("Baseline"), "{warnings:#?}");
    }

    /// M2: file level above the device's advertised maximum → `warn`.
    #[test]
    fn m2_level_above_the_device_maximum_warns() {
        let mut device = baseline_device();
        device.decode = Some(DecodeCapability {
            decoders: vec![crate::device::HardwareDecoder {
                name: "test decoder".to_string(),
                inputs: vec![crate::device::DecodeInput {
                    subtype: "h264".to_string(),
                    profile: Some(100),
                    level: Some(40), // device tops out at 4.0
                }],
            }],
            gpu_codecs: vec!["h264".to_string()],
        });
        let mut video = baseline_video();
        video.level = Some("51".to_string()); // file is 5.1
        let warnings = warnings_for(&video, &device, &baseline_monitor());
        assert_eq!(warnings.len(), 1, "{warnings:#?}");
        assert_eq!(warnings[0].metric, Metric::Level);
        assert_eq!(warnings[0].severity, Severity::Warn);
        assert!(warnings[0].detail.contains("5.1"), "{warnings:#?}");
        assert!(warnings[0].detail.contains("4.0"), "{warnings:#?}");
        // Same level, no warning.
        video.level = Some("40".to_string());
        assert!(warnings_for(&video, &device, &baseline_monitor()).is_empty());
    }

    /// M3: both axes well below 70 % → upscale warning; both above 150 %
    /// → waste warning; a single off axis is M4's problem, not M3's.
    #[test]
    fn m3_resolution_warns_both_ways_but_never_on_one_axis_alone() {
        let device = baseline_device();
        let monitor = baseline_monitor();
        // 640x360 on 1366x768: 47% x 47% → soft upscale. Bitrate set under
        // the M6 budget for this size so the row stays isolated.
        let mut small = baseline_video();
        small.width = 640;
        small.height = 360;
        small.bitrate_bps = Some(1_000_000);
        let warnings = warnings_for(&small, &device, &monitor);
        assert_eq!(warnings.len(), 1, "{warnings:#?}");
        assert_eq!(warnings[0].metric, Metric::Resolution);
        assert!(warnings[0].detail.contains("47%"), "{warnings:#?}");
        // 3840x2160 on 1366x768: 2.81x per axis → decoded, never seen.
        let mut huge = baseline_video();
        huge.width = 3840;
        huge.height = 2160;
        let warnings = warnings_for(&huge, &device, &monitor);
        assert_eq!(warnings.len(), 1, "{warnings:#?}");
        assert_eq!(warnings[0].metric, Metric::Resolution);
        // Narrow-but-short (one axis each side): no M3 claim.
        let mut odd = baseline_video();
        odd.width = 1366;
        odd.height = 500;
        assert!(warnings_for(&odd, &device, &monitor)
            .iter()
            .all(|warning| warning.metric != Metric::Resolution));
    }

    /// M4: AR mismatch > 1 % names the actual cost of the actual `Fit`.
    #[test]
    fn m4_aspect_names_bars_for_contain_and_crop_for_cover() {
        let device = baseline_device();
        let monitor = baseline_monitor();
        let mut video = baseline_video();
        video.width = 1440;
        video.height = 1080; // 4:3 on 16:9 — 25% off
        let contain = warnings_for(&video, &device, &monitor);
        assert_eq!(contain.len(), 1, "{contain:#?}");
        assert_eq!(contain[0].metric, Metric::Aspect);
        assert!(contain[0].consequence.contains("bars"), "{contain:#?}");
        assert!(contain[0].consequence.contains("height"), "{contain:#?}");
        let cover = assess(&video, &device, &monitor, Fit::Cover).warnings;
        assert_eq!(cover.len(), 1, "{cover:#?}");
        assert!(cover[0].consequence.contains("crop"), "{cover:#?}");
        // Stretch shows everything distorted — no bars, no crop claim.
        assert!(assess(&video, &device, &monitor, Fit::Stretch)
            .warnings
            .iter()
            .all(|warning| warning.metric != Metric::Aspect));
        // The recommendation keeps the display's exact AR.
        let rec = recommend(&monitor);
        assert_eq!((rec.width, rec.height), (1366, 768));
    }

    /// M4 edge: a video wider than the display flips bars to the sides
    /// with Contain and to the sides' crop with Cover.
    #[test]
    fn m4_aspect_flips_the_axis_with_the_shape_of_the_mismatch() {
        let device = baseline_device();
        let monitor = baseline_monitor();
        let mut video = baseline_video();
        video.width = 2400;
        video.height = 1080; // 2.22 vs 1.78 — wider than the display
        let contain = warnings_for(&video, &device, &monitor);
        assert_eq!(contain.len(), 1, "{contain:#?}");
        assert!(contain[0].consequence.contains("width"), "{contain:#?}");
        let cover = assess(&video, &device, &monitor, Fit::Cover).warnings;
        assert!(cover[0].consequence.contains("sides"), "{cover:#?}");
    }

    /// M5: above the display's Hz → `info` (wasted decode); ≤ 24 fps on a
    /// 60 Hz display → `warn` (judder); in-between rates stay silent.
    #[test]
    fn m5_frame_rate_info_above_hz_and_warn_at_judder_rates() {
        let device = baseline_device();
        let monitor = baseline_monitor();
        let mut fast = baseline_video();
        fast.fps = Some(120.0);
        let warnings = warnings_for(&fast, &device, &monitor);
        assert_eq!(warnings.len(), 1, "{warnings:#?}");
        assert_eq!(warnings[0].metric, Metric::FrameRate);
        assert_eq!(warnings[0].severity, Severity::Info);
        let mut slow = baseline_video();
        slow.fps = Some(24.0);
        slow.bitrate_bps = Some(1_000_000); // under the M6 budget at 24 fps
        let warnings = warnings_for(&slow, &device, &monitor);
        assert_eq!(warnings.len(), 1, "{warnings:#?}");
        assert_eq!(warnings[0].severity, Severity::Warn);
        assert!(warnings[0].consequence.contains("judder"), "{warnings:#?}");
        // 25–60 fps on 60 Hz: no claim either way (bitrate kept under the
        // M6 budget at every frame rate so this row stays isolated).
        for fps in [25.0, 30.0, 60.0] {
            let mut video = baseline_video();
            video.fps = Some(fps);
            video.bitrate_bps = Some(1_000_000);
            assert!(warnings_for(&video, &device, &monitor).is_empty(), "{fps}");
        }
    }

    /// M6: above the bits-per-pixel budget for (size × fps × codec class)
    /// → `warn`; at or below it stays silent. Budget for the baseline is
    /// 1920·1080·60·0.10 = 12 441 600 bps.
    #[test]
    fn m6_bitrate_warns_only_above_the_budget() {
        let device = baseline_device();
        let monitor = baseline_monitor();
        let mut video = baseline_video();
        video.bitrate_bps = Some(12_441_600); // exactly at budget
        assert!(warnings_for(&video, &device, &monitor).is_empty());
        video.bitrate_bps = Some(20_000_000); // over
        let warnings = warnings_for(&video, &device, &monitor);
        assert_eq!(warnings.len(), 1, "{warnings:#?}");
        assert_eq!(warnings[0].metric, Metric::Bitrate);
        assert!(warnings[0].detail.contains("12442 kbps"), "{warnings:#?}");
        // Unknown bitrate or fps → no budget to compare against.
        video.bitrate_bps = None;
        assert!(warnings_for(&video, &device, &monitor).is_empty());
    }

    /// M7: HDR content is the policy `error`; SDR content on an HDR
    /// display is a `warn`; SDR on SDR is silent.
    #[test]
    fn m7_hdr_policy_error_and_sdr_on_hdr_warn() {
        let device = baseline_device();
        let monitor = baseline_monitor();
        let mut hdr = baseline_video();
        hdr.hdr = Some(true);
        let warnings = warnings_for(&hdr, &device, &monitor);
        assert_eq!(warnings.len(), 1, "{warnings:#?}");
        assert_eq!(warnings[0].metric, Metric::Hdr);
        assert_eq!(warnings[0].severity, Severity::Error);
        let mut hdr_monitor = baseline_monitor();
        hdr_monitor.hdr = Some(true);
        let warnings = warnings_for(&baseline_video(), &device, &hdr_monitor);
        assert_eq!(warnings.len(), 1, "{warnings:#?}");
        assert_eq!(warnings[0].metric, Metric::Hdr);
        assert_eq!(warnings[0].severity, Severity::Warn);
        // HDR content on an HDR display is still the policy error (v1 has
        // no tone mapping at all).
        let warnings = warnings_for(&hdr, &device, &hdr_monitor);
        assert_eq!(warnings.len(), 1, "{warnings:#?}");
        assert_eq!(warnings[0].severity, Severity::Error);
    }

    /// M8: aperture iGPUs (dedicated ≤ 256 MB — the measured target has
    /// 128 MB) skip the metric entirely; a real dGPU under 256 MB
    /// headroom after the surface estimate warns.
    #[test]
    fn m8_vram_skips_aperture_igpus_and_warns_a_tight_dedicated_gpu() {
        // Aperture iGPU: no M3-style false positive, no warning ever.
        let warnings = warnings_for(&baseline_video(), &baseline_device(), &baseline_monitor());
        assert!(warnings
            .iter()
            .all(|warning| warning.metric != Metric::Vram));
        // 4 GB dGPU: 4096 - (swapchains + 1080p decode surfaces) ≈ 4 GB
        // headroom → fine.
        let mut comfortable = baseline_device();
        comfortable.gpus[0].dedicated_mb = 4096;
        let warnings = warnings_for(&baseline_video(), &comfortable, &baseline_monitor());
        assert!(
            warnings
                .iter()
                .all(|warning| warning.metric != Metric::Vram),
            "{warnings:#?}"
        );
        // 384 MB dedicated: est. surfaces = 1 monitor swapchain (8 MB) +
        // 4K decode (190 MB) = 198 MB → 186 MB headroom < 256 → warn.
        // (4K on this display also trips M3 — filter to this row.)
        let mut tight = baseline_device();
        tight.gpus[0].dedicated_mb = 384;
        let mut video = baseline_video();
        video.width = 3840;
        video.height = 2160;
        video.bitrate_bps = Some(1_000_000); // under the M6 budget at 4K60
        let vram: Vec<Warning> = warnings_for(&video, &tight, &baseline_monitor())
            .into_iter()
            .filter(|warning| warning.metric == Metric::Vram)
            .collect();
        assert_eq!(vram.len(), 1, "{vram:#?}");
        assert_eq!(vram[0].severity, Severity::Warn);
        assert!(vram[0].detail.contains("headroom"), "{vram:#?}");
    }

    /// M9: total RAM below 4 GB warns; below 8 GB with 4K content warns;
    /// the measured 16 GB machine is silent.
    #[test]
    fn m9_ram_warns_on_low_total_and_on_4k_with_8gb() {
        let monitor = baseline_monitor();
        let mut low = baseline_device();
        low.ram = Some(RamInfo {
            total_mb: 3072,
            available_mb: 1024,
        });
        let warnings = warnings_for(&baseline_video(), &low, &monitor);
        assert_eq!(warnings.len(), 1, "{warnings:#?}");
        assert_eq!(warnings[0].metric, Metric::Ram);
        let mut medium = baseline_device();
        medium.ram = Some(RamInfo {
            total_mb: 6144,
            available_mb: 3000,
        });
        // 4K on this display also trips M3 (waste) — filter to this row.
        let mut video = baseline_video();
        video.width = 3840;
        video.height = 2160;
        video.bitrate_bps = Some(1_000_000); // under the M6 budget at 4K60
        let ram: Vec<Warning> = warnings_for(&video, &medium, &monitor)
            .into_iter()
            .filter(|warning| warning.metric == Metric::Ram)
            .collect();
        assert_eq!(ram.len(), 1, "{ram:#?}");
        // 8 GB + 1080p: silent.
        medium.ram = Some(RamInfo {
            total_mb: 8192,
            available_mb: 4000,
        });
        let warnings = warnings_for(&baseline_video(), &medium, &monitor);
        assert!(warnings.is_empty(), "{warnings:#?}");
    }

    /// The recommendation contract: exact display size, ladder alternative
    /// only when the display's AR matches 16:9 within 1 %.
    #[test]
    fn recommend_keeps_the_display_ar_and_names_the_ladder_alternative() {
        let monitor = baseline_monitor(); // 1366x768, AR 1.7786
        let rec = recommend(&monitor);
        assert_eq!((rec.width, rec.height), (1366, 768));
        assert!(rec.note.contains("1920x1080"), "{}", rec.note);
        assert!(rec.note.contains("delta"), "{}", rec.note);
        // An ultrawide display gets an honest "no ladder matches".
        let mut ultrawide = baseline_monitor();
        ultrawide.rect = Rect::new(0, 0, 3440, 1440);
        let rec = recommend(&ultrawide);
        assert_eq!((rec.width, rec.height), (3440, 1440));
        assert!(rec.note.contains("no standard"), "{}", rec.note);
        // A display that *is* a ladder size says so without redundancy.
        let mut full_hd = baseline_monitor();
        full_hd.rect = Rect::new(0, 0, 1920, 1080);
        let rec = recommend(&full_hd);
        assert!(
            rec.note.contains("fits this display exactly"),
            "{}",
            rec.note
        );
        // Degenerate geometry never divides by zero.
        let mut broken = baseline_monitor();
        broken.rect = Rect::new(0, 0, 0, 0);
        assert_eq!(recommend(&broken).width, 0);
    }

    /// The wire shape of an assessment (ipc.md §9): snake_case fields, the
    /// stable metric/severity strings, and a recommendation alongside.
    #[test]
    fn assessment_serialises_to_the_documented_shape() {
        let assessment = assess(
            &baseline_video(),
            &baseline_device(),
            &baseline_monitor(),
            Fit::Contain,
        );
        let json = serde_json::to_string(&assessment).expect("json");
        assert!(json.contains("\"warnings\""), "{json}");
        assert!(json.contains("\"recommendation\""), "{json}");
        let with_warning = {
            let mut video = baseline_video();
            video.codec = "HEVC".to_string();
            assess(
                &video,
                &baseline_device(),
                &baseline_monitor(),
                Fit::Contain,
            )
        };
        let json = serde_json::to_string(&with_warning).expect("json");
        for key in [
            "\"metric\":\"codec\"",
            "\"severity\":\"error\"",
            "\"detail\"",
            "\"consequence\"",
            "\"fix\"",
            "\"width\"",
            "\"height\"",
            "\"note\"",
        ] {
            assert!(json.contains(key), "missing {key} in {json}");
        }
        // Round-trips — the daemon serialises, the CLI decodes.
        let back: Assessment = serde_json::from_str(&json).expect("decode");
        assert_eq!(back, with_warning);
    }
}
