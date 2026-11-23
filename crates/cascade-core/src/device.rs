//! `DeviceProfile`: the machine facts `assess()` consumes.
//!
//! Pure data and pure helpers only — no Win32, no I/O (crate doc rule).
//! Collection lives in `cascade-win/src/device.rs`; `cascade doctor` prints
//! the profile; the `assess()` reads it.

use std::collections::{BTreeMap, BTreeSet};

use crate::Rect;

/// Everything `doctor` prints and `assess` consumes about this machine.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DeviceProfile {
    pub monitors: Vec<MonitorProfile>,
    pub gpus: Vec<GpuAdapter>,
    pub cpu: Option<CpuInfo>,
    pub ram: Option<RamInfo>,
    pub decode: Option<DecodeCapability>,
}

/// Per-monitor facts: identity/geometry from `monitor::enumerate` plus the
/// refresh/DPI/HDR extras collected for. Every extra is `Option` — a mode
/// the OS does not report stays `None` instead of a guessed zero.
#[derive(Clone, Debug, PartialEq)]
pub struct MonitorProfile {
    pub id: String,
    pub rect: Rect,
    pub work: Rect,
    pub primary: bool,
    pub refresh_hz: Option<u32>,
    pub dpi: Option<u32>,
    pub hdr: Option<bool>,
    pub bits_per_color: Option<u32>,
    /// DXGI color space label, e.g. `SDR (BT.709)` — `None` when the output
    /// has no `IDXGIOutput6` desc (driver too old for HDR metadata).
    pub color_space: Option<String>,
}

/// One DXGI adapter. `dedicated_mb` is 0 on iGPUs that expose no dedicated
/// aperture — printed honestly, not padded with the shared number.
#[derive(Clone, Debug, PartialEq)]
pub struct GpuAdapter {
    pub name: String,
    pub vendor_id: u32,
    pub dedicated_mb: u64,
    pub shared_mb: u64,
    /// Display-driver version from the class key (read-only), matched by
    /// `DriverDesc == name`; `None` when absent.
    pub driver_version: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CpuInfo {
    /// `ProcessorNameString`; `None` when the registry key is unreadable.
    pub name: Option<String>,
    /// Physical cores (counted as `RelationProcessorCore` entries).
    pub cores: u32,
    /// Logical processors (hardware threads).
    pub threads: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RamInfo {
    pub total_mb: u64,
    pub available_mb: u64,
}

/// Video-decode capability of this machine, from two independent sources:
///
/// * `decoders` — hardware video-decoder MFTs (`MFTEnumEx` with
///   `MFT_ENUM_FLAG_HARDWARE`), with their advertised input types.
/// * `gpu_codecs` — codec families the GPU reports via
///   `ID3D11VideoDevice::GetVideoDecoderProfile` (vendor-generic: Intel,
///   NVIDIA, AMD alike).
///
/// The union is necessary because the two sources are not redundant: on the
/// target machine (Intel UHD, driver 30.0.101.1960, measured 2026-11-09) the
/// hardware-MFT list contains only M-JPEG decoders — no H.264/HEVC MFT is
/// registered at all — yet the GPU's VideoDecode engine is busy 12–14% while
/// the daemon plays H.264 1080p60 (hardware decode demonstrably active). The
/// MFT `HARDWARE` flag alone would report "no H.264 decode" on such machines.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DecodeCapability {
    pub decoders: Vec<HardwareDecoder>,
    /// Sorted, deduplicated fourcc labels (`h264`, `hevc`, `vp90`, `av01`, …)
    /// the D3D11 video device reports. Empty when the driver exposes no
    /// decoder profiles (query failed or genuinely none).
    pub gpu_codecs: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct HardwareDecoder {
    /// `MFT_FRIENDLY_NAME_Attribute` when the MFT reports one.
    pub name: String,
    /// One entry per advertised input type (subtype/profile/level triple).
    pub inputs: Vec<DecodeInput>,
}

/// One advertised decoder input type. `profile`/`level` are the raw
/// `MF_MT_MPEG2_PROFILE` / `MF_MT_MPEG2_LEVEL` numbers (`None` when the
/// attribute is absent — common on single-profile hardware decoders).
#[derive(Clone, Debug, PartialEq)]
pub struct DecodeInput {
    /// Lower-cased fourcc label, e.g. `h264`, `hevc` (see [`subtype_label`]).
    pub subtype: String,
    pub profile: Option<u32>,
    pub level: Option<u32>,
}

/// Profiles and levels aggregated over every decoder for one subtype.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CodecSupport {
    pub profiles: BTreeSet<u32>,
    pub levels: BTreeSet<u32>,
}

/// Aggregate both sources into `subtype -> (profiles, levels)`.
///
/// MFT input types contribute their profile/level detail; a GPU codec with no
/// MFT detail joins the map with *empty* sets — "decodes this codec, detail
/// unknown" — never a guessed profile or level. Duplicates collapse (sets),
/// so a machine with two H.264 MFTs reports one `h264` row — what `assess`
/// needs to answer "does this device decode profile P at level L?".
pub fn codec_support(capability: &DecodeCapability) -> BTreeMap<String, CodecSupport> {
    let mut out: BTreeMap<String, CodecSupport> = BTreeMap::new();
    for decoder in &capability.decoders {
        for input in &decoder.inputs {
            let entry = out.entry(input.subtype.clone()).or_default();
            if let Some(profile) = input.profile {
                entry.profiles.insert(profile);
            }
            if let Some(level) = input.level {
                entry.levels.insert(level);
            }
        }
    }
    for codec in &capability.gpu_codecs {
        out.entry(codec.clone()).or_default();
    }
    out
}

/// Subtype label from an `MF_MT_SUBTYPE` GUID's fourcc (`Data1`), when the
/// GUID has the MF video-format shape. `H264` → `h264`, `HEVC` → `hevc`;
/// anything outside the shape falls back to a stable hex label so `doctor`
/// never invents a codec name.
pub fn subtype_label(data1: u32, data2: u16, data3: u16, data4: [u8; 8]) -> String {
    const MF_SUFFIX: (u16, u16, [u8; 8]) = (
        0x0000,
        0x0010,
        [0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71],
    );
    if (data2, data3, data4) != MF_SUFFIX {
        return format!("guid:{data1:08x}{data2:04x}{data3:04x}");
    }
    let bytes = data1.to_le_bytes();
    if bytes.iter().all(|b| b.is_ascii_graphic()) {
        String::from_utf8_lossy(&bytes).to_ascii_lowercase()
    } else {
        format!("guid:{data1:08x}")
    }
}

/// Human name for an `MF_MT_MPEG2_PROFILE` value on `subtype`, falling back
/// to the raw number (never a wrong name — the tables are the known sets
/// from ITU-T H.264 Annex A and ITU-T H.265).
pub fn profile_name(subtype: &str, profile: u32) -> String {
    let name = match (subtype, profile) {
        ("h264", 66) => Some("Baseline"),
        ("h264", 77) => Some("Main"),
        ("h264", 88) => Some("Extended"),
        ("h264", 100) => Some("High"),
        ("h264", 110) => Some("High10"),
        ("h264", 122) => Some("High422"),
        ("h264", 244) => Some("High444"),
        ("hevc", 1) => Some("Main"),
        ("hevc", 2) => Some("Main10"),
        ("hevc", 3) => Some("Main Still"),
        _ => None,
    };
    match name {
        Some(name) => name.to_string(),
        None => profile.to_string(),
    }
}

/// Human name for an `MF_MT_MPEG2_LEVEL` value on `subtype`.
///
/// H.264 levels encode as `major * 10 + minor` (`31` → `3.1`); HEVC uses
/// `major * 30` with an offset sublevel (`63` → `2.1`, `200` → `6.2`).
/// Values outside the standard tables print raw.
pub fn level_name(subtype: &str, level: u32) -> String {
    if subtype == "h264" {
        const KNOWN: [u32; 19] = [
            10, 11, 12, 13, 20, 21, 22, 30, 31, 32, 40, 41, 42, 50, 51, 52, 60, 61, 62,
        ];
        if KNOWN.contains(&level) {
            return format!("{}.{}", level / 10, level % 10);
        }
    }
    if subtype == "hevc" {
        const TABLE: [(u32, &str); 13] = [
            (30, "1"),
            (60, "2"),
            (63, "2.1"),
            (90, "3"),
            (93, "3.1"),
            (120, "4"),
            (123, "4.1"),
            (150, "5"),
            (153, "5.1"),
            (156, "5.2"),
            (180, "6"),
            (186, "6.1"),
            (200, "6.2"),
        ];
        if let Some((_, name)) = TABLE.iter().find(|(value, _)| *value == level) {
            return (*name).to_string();
        }
    }
    level.to_string()
}

/// Vendor name for a PCI vendor id from `DXGI_ADAPTER_DESC1::VendorId`.
/// Unlisted vendors print the raw id — a short honest label beats a guess.
pub fn vendor_label(vendor_id: u32) -> String {
    match vendor_id {
        0x8086 => "Intel".to_string(),
        0x10DE => "NVIDIA".to_string(),
        0x1002 | 0x1022 => "AMD".to_string(),
        0x1414 => "Microsoft".to_string(),
        other => format!("vendor 0x{other:04x}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// MF video-format GUID suffix: `{0000-0010-8000-00AA00389B71}`.
    const SUFFIX: (u16, u16, [u8; 8]) = (
        0x0000,
        0x0010,
        [0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71],
    );

    #[test]
    fn subtype_label_decodes_mf_fourcc() {
        // 'H264' and 'HEVC' stored little-endian in Data1.
        assert_eq!(
            subtype_label(0x3436_3248, SUFFIX.0, SUFFIX.1, SUFFIX.2),
            "h264"
        );
        assert_eq!(
            subtype_label(0x4356_4548, SUFFIX.0, SUFFIX.1, SUFFIX.2),
            "hevc"
        );
        // 'VP90' → vp90; no special-casing per codec.
        assert_eq!(
            subtype_label(0x3039_5056, SUFFIX.0, SUFFIX.1, SUFFIX.2),
            "vp90"
        );
    }

    #[test]
    fn subtype_label_rejects_non_video_guids() {
        // Wrong suffix never becomes a codec name.
        let label = subtype_label(0x3436_3248, 0x0001, 0x0010, [0u8; 8]);
        assert!(label.starts_with("guid:"), "got {label}");
        // Non-printable Data1 also falls back instead of inventing text.
        let label = subtype_label(0x0000_0001, SUFFIX.0, SUFFIX.1, SUFFIX.2);
        assert!(label.starts_with("guid:"), "got {label}");
    }

    #[test]
    fn profile_name_covers_known_sets_and_falls_back() {
        assert_eq!(profile_name("h264", 66), "Baseline");
        assert_eq!(profile_name("h264", 100), "High");
        assert_eq!(profile_name("h264", 244), "High444");
        assert_eq!(profile_name("hevc", 2), "Main10");
        // Unknown numbers print raw rather than a wrong name.
        assert_eq!(profile_name("h264", 999), "999");
        assert_eq!(profile_name("av01", 1), "1");
    }

    #[test]
    fn level_name_handles_both_encodings() {
        // H.264: major*10 + minor.
        assert_eq!(level_name("h264", 31), "3.1");
        assert_eq!(level_name("h264", 52), "5.2");
        assert_eq!(level_name("h264", 40), "4.0");
        assert_eq!(level_name("h264", 99), "99"); // out of table, raw
                                                  // HEVC: major*30 + offset sublevel.
        assert_eq!(level_name("hevc", 30), "1");
        assert_eq!(level_name("hevc", 63), "2.1");
        assert_eq!(level_name("hevc", 200), "6.2");
        assert_eq!(level_name("hevc", 999), "999"); // out of table, raw
    }

    #[test]
    fn codec_support_dedupes_across_decoders_and_keeps_unknowable_none() {
        let capability = DecodeCapability {
            decoders: vec![
                HardwareDecoder {
                    name: "a".into(),
                    inputs: vec![
                        DecodeInput {
                            subtype: "h264".into(),
                            profile: Some(100),
                            level: Some(51),
                        },
                        DecodeInput {
                            subtype: "h264".into(),
                            profile: Some(100), // duplicate collapses
                            level: None,        // attribute absent: not a level
                        },
                    ],
                },
                HardwareDecoder {
                    name: "b".into(),
                    inputs: vec![DecodeInput {
                        subtype: "hevc".into(),
                        profile: Some(2),
                        level: Some(123),
                    }],
                },
            ],
            gpu_codecs: vec!["h264".into(), "hevc".into()],
        };
        let support = codec_support(&capability);
        assert_eq!(support.len(), 2);
        let h264 = &support["h264"];
        assert_eq!(h264.profiles, BTreeSet::from([100]));
        assert_eq!(h264.levels, BTreeSet::from([51]));
        assert_eq!(support["hevc"].profiles, BTreeSet::from([2]));
        assert_eq!(support["hevc"].levels, BTreeSet::from([123]));
    }

    #[test]
    fn codec_support_of_an_empty_capability_is_empty() {
        assert!(codec_support(&DecodeCapability::default()).is_empty());
    }

    /// The measured target-machine shape: hardware MFTs advertise only
    /// M-JPEG, while the D3D11 query reports h264/hevc. The GPU codecs must
    /// appear in the map with empty (unknown) profile/level sets — this is
    /// what keeps `assess()` from a false "no hardware decode" warning.
    #[test]
    fn codec_support_unions_in_gpu_only_codecs_with_unknown_detail() {
        let capability = DecodeCapability {
            decoders: vec![HardwareDecoder {
                name: "mjpeg".into(),
                inputs: vec![DecodeInput {
                    subtype: "mjpg".into(),
                    profile: None,
                    level: None,
                }],
            }],
            gpu_codecs: vec!["av01".into(), "h264".into(), "hevc".into()],
        };
        let support = codec_support(&capability);
        assert_eq!(
            support.keys().collect::<Vec<_>>(),
            ["av01", "h264", "hevc", "mjpg"]
        );
        // MFT-derived detail survives, GPU-only codecs stay honestly empty.
        assert_eq!(support["h264"].profiles, BTreeSet::new());
        assert_eq!(support["h264"].levels, BTreeSet::new());
        assert_eq!(support["mjpg"].profiles, BTreeSet::new());
    }

    #[test]
    fn vendor_label_matches_the_common_pcie_ids() {
        assert_eq!(vendor_label(0x8086), "Intel");
        assert_eq!(vendor_label(0x10DE), "NVIDIA");
        assert_eq!(vendor_label(0x1002), "AMD");
        assert_eq!(vendor_label(0x1234), "vendor 0x1234");
    }
}
