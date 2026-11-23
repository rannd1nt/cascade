//! H.264 SPS/VUI colour tags parsed from the bitstream (colour fix,
//!
//!
//! Media Foundation ignores H.264 VUI colour flags on build 26200.9711
//! and the canonical gate asset carries no
//! `colr` box at all — so the only trustworthy source for the
//! YCbCr matrix is the SPS itself. The sequence header arrives as
//! `MF_MT_MPEG_SEQUENCE_HEADER` (Annex-B for MP4, measured 42 bytes for
//! `bars.mp4`), which may also be an avcC record; both are accepted.
//!
//! This module is pure bit-twiddling: no Win32, no MF, unit-testable per
//! Any parse failure yields the documented defaults (BT.709 +
//! limited range — the behaviour every path had before this fix, so a
//! malformed stream can never regress worse than the old hardcode).

/// YCbCr → RGB matrix to use for conversion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoMatrix {
    /// VUI `matrix_coefficients` 1 (also the default for absent/unknown).
    Bt709,
    /// VUI `matrix_coefficients` 5 (BT.470BG) or 6 (SMPTE 170M) — both are
    /// the BT.601 coefficient set for conversion purposes.
    Bt601,
}

/// Nominal range of the decoded Y'CbCr samples.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoRange {
    /// 16–235 luma (VUI `video_full_range_flag` = 0, also the default).
    Limited,
    /// 0–255 luma (VUI `video_full_range_flag` = 1).
    Full,
}

impl VideoMatrix {
    /// Stable lowercase spelling for log lines (`bt.709` / `bt.601`).
    pub fn as_str(self) -> &'static str {
        match self {
            VideoMatrix::Bt709 => "bt.709",
            VideoMatrix::Bt601 => "bt.601",
        }
    }
}

impl VideoRange {
    /// Stable lowercase spelling for log lines (`limited` / `full`).
    pub fn as_str(self) -> &'static str {
        match self {
            VideoRange::Limited => "limited",
            VideoRange::Full => "full",
        }
    }
}

/// Colour tags for one stream: what to tell the converter, and what the
/// bitstream actually said (for logging and honest diagnostics).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColourTags {
    pub matrix: VideoMatrix,
    pub range: VideoRange,
    /// Raw VUI `matrix_coefficients` when it was signalled; `None` when the
    /// VUI carries no colour description (the default was applied).
    pub matrix_coefficients: Option<u8>,
}

impl Default for ColourTags {
    /// Absent VUI colour description: BT.709 + limited range, matching the
    /// pre-fix hardcode (`source_reader.rs` `color_space_bits` = `0x14`) so
    /// VUI-less content behaves exactly as before.
    fn default() -> Self {
        Self {
            matrix: VideoMatrix::Bt709,
            range: VideoRange::Limited,
            matrix_coefficients: None,
        }
    }
}

impl ColourTags {
    /// Map VUI `matrix_coefficients` + `video_full_range_flag` to tags.
    /// Unsupported coefficient values fall back to the default (709) but
    /// keep the raw value visible.
    fn from_vui(matrix_coefficients: Option<u8>, full_range: bool) -> Self {
        let matrix = match matrix_coefficients {
            Some(1) => VideoMatrix::Bt709,
            Some(5) | Some(6) => VideoMatrix::Bt601,
            _ => VideoMatrix::Bt709,
        };
        Self {
            matrix,
            range: if full_range {
                VideoRange::Full
            } else {
                VideoRange::Limited
            },
            matrix_coefficients,
        }
    }
}

/// Derive colour tags from an `MF_MT_MPEG_SEQUENCE_HEADER` blob
/// (Annex-B SPS+PPS or an avcC record). Never fails: any malformed or
/// missing structure yields [`ColourTags::default`].
pub fn colour_tags(sequence_header: &[u8]) -> ColourTags {
    find_sps(sequence_header)
        .map(to_rbsp)
        .and_then(|rbsp| parse_sps(&rbsp).ok())
        .unwrap_or_default()
}

/// Extract the first SPS NAL payload (NAL header byte included) from an
/// Annex-B blob or an avcC record.
fn find_sps(blob: &[u8]) -> Option<&[u8]> {
    if blob.starts_with(&[0, 0, 1]) || blob.starts_with(&[0, 0, 0, 1]) {
        return find_sps_annexb(blob);
    }
    if blob.first() == Some(&1) {
        return find_sps_avcc(blob);
    }
    // Neither recognised layout: no start code, not a record.
    None
}

/// Walk Annex-B start codes until a NAL whose type is 7 (SPS).
fn find_sps_annexb(blob: &[u8]) -> Option<&[u8]> {
    let mut i = 0usize;
    while i + 3 <= blob.len() {
        // 4-byte start code first (00 00 00 01), then 3-byte (00 00 01).
        let nal_start = if blob[i..].starts_with(&[0, 0, 0, 1]) {
            i + 4
        } else if blob[i..].starts_with(&[0, 0, 1]) {
            i + 3
        } else {
            i += 1;
            continue;
        };
        if nal_start >= blob.len() {
            return None;
        }
        // NAL payload ends at the next start code, or at the end of the
        // blob when this is the last NAL (a 3-byte start code cannot start
        // in the final two bytes, so "not found" means "runs to the end").
        let mut end = nal_start;
        let mut found_next = false;
        while end + 3 <= blob.len() {
            if blob[end..].starts_with(&[0, 0, 0, 1]) || blob[end..].starts_with(&[0, 0, 1]) {
                found_next = true;
                break;
            }
            end += 1;
        }
        if !found_next {
            end = blob.len();
        }
        if blob[nal_start] & 0x1F == 7 {
            return Some(&blob[nal_start..end]);
        }
        i = nal_start;
    }
    None
}

/// Walk an avcC record to its first SPS (`numSPS` in the lower 5 bits of
/// byte 5, then length-prefixed NALs).
fn find_sps_avcc(blob: &[u8]) -> Option<&[u8]> {
    if blob.len() < 7 {
        return None;
    }
    let num_sps = (blob[5] & 0x1F) as usize;
    let mut off = 6usize;
    for _ in 0..num_sps {
        if off + 2 > blob.len() {
            return None;
        }
        let len = u16::from_be_bytes([blob[off], blob[off + 1]]) as usize;
        off += 2;
        if off + len > blob.len() || len == 0 {
            return None;
        }
        let nal = &blob[off..off + len];
        if nal[0] & 0x1F == 7 {
            return Some(nal);
        }
        off += len;
    }
    None
}

/// Remove H.264 emulation-prevention bytes (`00 00 03 xx` → `00 00 xx`).
fn to_rbsp(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0usize;
    for &byte in nal {
        if zeros >= 2 && byte == 0x03 {
            zeros = 0;
            continue;
        }
        out.push(byte);
        zeros = if byte == 0 { zeros + 1 } else { 0 };
    }
    out
}

/// MSB-first bit reader with the exp-Golomb helpers SPS parsing needs.
/// Every read is bounds-checked; overflow returns `Err`.
struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    /// `u(n)`, n ≤ 32.
    fn u(&mut self, n: u32) -> Result<u32, ()> {
        if n == 0 {
            return Ok(0);
        }
        let mut value = 0u32;
        for _ in 0..n {
            let byte = *self.data.get(self.pos >> 3).ok_or(())?;
            let bit = (byte >> (7 - (self.pos & 7))) & 1;
            value = (value << 1) | u32::from(bit);
            self.pos += 1;
        }
        Ok(value)
    }

    /// `ue(v)` unsigned exp-Golomb.
    fn ue(&mut self) -> Result<u32, ()> {
        let mut leading = 0u32;
        while self.u(1)? == 0 {
            leading += 1;
            if leading > 31 {
                return Err(());
            }
        }
        let suffix = if leading == 0 { 0 } else { self.u(leading)? };
        Ok((1 << leading) - 1 + suffix)
    }

    /// `se(v)` signed exp-Golomb.
    fn se(&mut self) -> Result<i32, ()> {
        let code = self.ue()?;
        if code == 0 {
            return Ok(0);
        }
        let magnitude = (code >> 1) as i32;
        if code & 1 == 1 {
            Ok(magnitude + 1)
        } else {
            Ok(-magnitude)
        }
    }

    /// Skip one scaling list (`size` entries of the nextScale/lastScale
    /// recurrence, H.264 7.3.2.1.1).
    fn skip_scaling_list(&mut self, size: usize) -> Result<(), ()> {
        let mut last_scale = 8i32;
        let mut next_scale = 8i32;
        for j in 0..size {
            if next_scale != 0 {
                let delta = self.se()?;
                next_scale = (last_scale + delta + 256) % 256;
            }
            let use_default = j == 0 && next_scale == 0;
            if !use_default {
                last_scale = if next_scale == 0 {
                    last_scale
                } else {
                    next_scale
                };
            }
        }
        Ok(())
    }
}

/// Profiles that carry the extended High-profile fields (H.264 Table 7-1).
const HIGH_PROFILES: [u8; 11] = [100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139];

/// Parse one RBSP SPS up to (and including) the VUI colour description.
/// Stopping early — VUI absent, `video_signal_type_present_flag` = 0 —
/// yields `ColourTags::default()` for that level of signalling.
fn parse_sps(rbsp: &[u8]) -> Result<ColourTags, ()> {
    let mut bits = Bits::new(rbsp);
    bits.u(8)?; // NAL header byte (find_sps already checked nal_unit_type == 7)
    let profile = bits.u(8)?;
    let _constraints = bits.u(8)?;
    let _level = bits.u(8)?;
    let _sps_id = bits.ue()?;
    if HIGH_PROFILES.contains(&(profile as u8)) {
        let chroma_format_idc = bits.ue()?;
        if chroma_format_idc == 3 {
            bits.u(1)?; // separate_colour_plane_flag
        }
        bits.ue()?; // bit_depth_luma_minus8
        bits.ue()?; // bit_depth_chroma_minus8
        bits.u(1)?; // qpprime_y_zero_transform_bypass_flag
        if bits.u(1)? == 1 {
            // seq_scaling_matrix_present_flag
            let lists = if chroma_format_idc != 3 { 8 } else { 12 };
            for i in 0..lists {
                if bits.u(1)? == 1 {
                    bits.skip_scaling_list(if i < 6 { 16 } else { 64 })?;
                }
            }
        }
    }
    bits.ue()?; // log2_max_frame_num_minus4
    let pic_order_cnt_type = bits.ue()?;
    if pic_order_cnt_type == 0 {
        bits.ue()?; // log2_max_pic_order_cnt_lsb_minus4
    } else if pic_order_cnt_type == 1 {
        bits.u(1)?; // delta_pic_order_always_zero_flag
        bits.se()?; // offset_for_non_ref_pic
        bits.se()?; // offset_for_top_to_bottom_field
        let cycle = bits.ue()?;
        if cycle > 255 {
            return Err(()); // garbage, not a real stream
        }
        for _ in 0..cycle {
            bits.se()?;
        }
    }
    bits.ue()?; // max_num_ref_frames
    bits.u(1)?; // gaps_in_frame_num_value_allowed_flag
    bits.ue()?; // pic_width_in_mbs_minus1
    bits.ue()?; // pic_height_in_map_units_minus1
    if bits.u(1)? == 0 {
        // frame_mbs_only_flag = 0
        bits.u(1)?; // mb_adaptive_frame_field_flag
    }
    bits.u(1)?; // direct_8x8_inference_flag
    if bits.u(1)? == 1 {
        // frame_cropping_flag
        for _ in 0..4 {
            bits.ue()?;
        }
    }
    if bits.u(1)? == 0 {
        // vui_parameters_present_flag = 0
        return Ok(ColourTags::default());
    }
    parse_vui(&mut bits)
}

/// Parse the VUI up to the colour description (H.264 E.1.1). Everything
/// after `matrix_coefficients` is irrelevant to conversion and skipped.
fn parse_vui(bits: &mut Bits) -> Result<ColourTags, ()> {
    if bits.u(1)? == 1 {
        // aspect_ratio_info_present_flag
        let aspect_ratio_idc = bits.u(8)?;
        if aspect_ratio_idc == 0xFF {
            bits.u(16)?; // sar_width
            bits.u(16)?; // sar_height
        }
    }
    if bits.u(1)? == 1 {
        bits.u(1)?; // overscan_appropriate_flag
    }
    if bits.u(1)? == 0 {
        // video_signal_type_present_flag = 0: no range, no colour desc.
        return Ok(ColourTags::default());
    }
    bits.u(3)?; // video_format
    let full_range = bits.u(1)? == 1; // video_full_range_flag
    let matrix_coefficients = if bits.u(1)? == 1 {
        // colour_description_present_flag
        bits.u(8)?; // colour_primaries
        bits.u(8)?; // transfer_characteristics
        Some(bits.u(8)? as u8) // matrix_coefficients
    } else {
        None
    };
    Ok(ColourTags::from_vui(matrix_coefficients, full_range))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Temp\s7\bars.mp4` sequence header, exactly as
    /// `MF_MT_MPEG_SEQUENCE_HEADER` reports it (probe 2026-11-10, notes ):
    /// Annex-B SPS + PPS, 42 bytes. ffprobe: `color_space=bt470bg`
    /// (matrix_coefficients 5), `color_range=tv`.
    const BARS_SEQUENCE_HEADER: &[u8] = &[
        0x00, 0x00, 0x01, 0x67, 0x64, 0x00, 0x2A, 0xAC, 0xD9, 0x40, 0x78, 0x02, 0x27, 0xE5, 0xC0,
        0x5A, 0x81, 0x01, 0x02, 0xA0, 0x00, 0x00, 0x03, 0x00, 0x20, 0x00, 0x00, 0x0F, 0x01, 0xE3,
        0x06, 0x32, 0xC0, 0x00, 0x00, 0x01, 0x68, 0xEB, 0xE3, 0xCB, 0x22, 0xC0,
    ];

    /// `Furina Midnight Maedow.mp4` sequence header (same probe, 40 bytes).
    /// ffprobe: `color_space=color_transfer=color_primaries=bt709`,
    /// `color_range=tv`.
    const FURINA_SEQUENCE_HEADER: &[u8] = &[
        0x00, 0x00, 0x01, 0x67, 0x64, 0x00, 0x2A, 0xAC, 0xD1, 0x00, 0x78, 0x02, 0x27, 0xE5, 0xC0,
        0x5A, 0x80, 0x80, 0x80, 0xA0, 0x00, 0x00, 0x03, 0x00, 0x20, 0x00, 0x00, 0x0F, 0x01, 0xE3,
        0x06, 0x22, 0x40, 0x00, 0x00, 0x01, 0x68, 0xEB, 0x8F, 0x2C,
    ];

    #[test]
    fn bars_gate_asset_is_bt601_limited() {
        // The whole fix in one assertion: the gate source must parse as
        // BT.601, or the desktop decodes it as 709 and fails again.
        assert_eq!(
            colour_tags(BARS_SEQUENCE_HEADER),
            ColourTags {
                matrix: VideoMatrix::Bt601,
                range: VideoRange::Limited,
                matrix_coefficients: Some(5),
            }
        );
    }

    #[test]
    fn furina_wallpaper_is_bt709_limited() {
        assert_eq!(
            colour_tags(FURINA_SEQUENCE_HEADER),
            ColourTags {
                matrix: VideoMatrix::Bt709,
                range: VideoRange::Limited,
                matrix_coefficients: Some(1),
            }
        );
    }

    #[test]
    fn avcc_record_yields_the_same_tags_as_annexb() {
        // Same SPS wrapped in an avcC record: version/profile/level bytes,
        // 0xFF lengthSize, one SPS with a 2-byte big-endian length, PPS.
        let sps = &BARS_SEQUENCE_HEADER[3..33]; // NAL header 0x67 + 29 SPS bytes
        let mut avcc = vec![0x01, 0x64, 0x00, 0x2A, 0xFF, 0xE1];
        avcc.extend_from_slice(&(sps.len() as u16).to_be_bytes());
        avcc.extend_from_slice(sps);
        avcc.extend_from_slice(&[0x01, 0x00, 0x04, 0x68, 0xEB, 0xE3, 0xCB]);
        assert_eq!(colour_tags(&avcc), colour_tags(BARS_SEQUENCE_HEADER));
    }

    #[test]
    fn absent_vui_keeps_the_legacy_defaults() {
        // Hand-built baseline SPS ending at vui_parameters_present_flag = 0:
        // profile 66, level 30, 16×16, frame-mbs-only. Defaults must equal
        // the pre-fix hardcode (BT.709 + limited) — no regression path.
        let mut bits: Vec<bool> = Vec::new();
        let push_u = |bits: &mut Vec<bool>, value: u32, n: u32| {
            for i in (0..n).rev() {
                bits.push((value >> i) & 1 == 1);
            }
        };
        let push_ue = |bits: &mut Vec<bool>, value: u32| {
            let code = value + 1;
            let width = 32 - code.leading_zeros();
            for _ in 0..width - 1 {
                bits.push(false);
            }
            push_u(bits, code, width);
        };
        push_u(&mut bits, 66, 8); // profile_idc Baseline
        push_u(&mut bits, 0, 8); // constraint flags
        push_u(&mut bits, 30, 8); // level_idc
        push_ue(&mut bits, 0); // seq_parameter_set_id
        push_ue(&mut bits, 0); // log2_max_frame_num_minus4
        push_ue(&mut bits, 0); // pic_order_cnt_type
        push_ue(&mut bits, 0); // log2_max_pic_order_cnt_lsb_minus4
        push_ue(&mut bits, 1); // max_num_ref_frames
        bits.push(false); // gaps_in_frame_num_value_allowed_flag
        push_ue(&mut bits, 0); // pic_width_in_mbs_minus1 (16 px)
        push_ue(&mut bits, 0); // pic_height_in_map_units_minus1
        bits.push(true); // frame_mbs_only_flag
        bits.push(true); // direct_8x8_inference_flag
        bits.push(false); // frame_cropping_flag
        bits.push(false); // vui_parameters_present_flag
        bits.push(true); // rbsp_stop_one_bit
        let mut rbsp = Vec::with_capacity(bits.len() / 8 + 1);
        for chunk in bits.chunks(8) {
            let mut byte = 0u8;
            for (i, bit) in chunk.iter().enumerate() {
                if *bit {
                    byte |= 1 << (7 - i);
                }
            }
            rbsp.push(byte);
        }
        // Wrap as Annex-B: start code + NAL header 0x67 (SPS), as
        // `MF_MT_MPEG_SEQUENCE_HEADER` delivers it.
        let mut annexb = vec![0x00, 0x00, 0x01, 0x67];
        annexb.extend_from_slice(&rbsp);
        assert_eq!(colour_tags(&annexb), ColourTags::default());
    }

    #[test]
    fn video_signal_without_colour_description_reports_range_only() {
        // Same skeleton as above but with a VUI: aspect absent, overscan
        // absent, video_signal present, video_format = 5, FULL range,
        // colour_description absent → range Full, matrix falls back to the
        // default, raw coefficients stay unsignalled (None).
        let mut bits: Vec<bool> = Vec::new();
        let push_u = |bits: &mut Vec<bool>, value: u32, n: u32| {
            for i in (0..n).rev() {
                bits.push((value >> i) & 1 == 1);
            }
        };
        let push_ue = |bits: &mut Vec<bool>, value: u32| {
            let code = value + 1;
            let width = 32 - code.leading_zeros();
            for _ in 0..width - 1 {
                bits.push(false);
            }
            push_u(bits, code, width);
        };
        push_u(&mut bits, 66, 8);
        push_u(&mut bits, 0, 8);
        push_u(&mut bits, 30, 8);
        push_ue(&mut bits, 0);
        push_ue(&mut bits, 0);
        push_ue(&mut bits, 0);
        push_ue(&mut bits, 0);
        push_ue(&mut bits, 1);
        bits.push(false);
        push_ue(&mut bits, 0);
        push_ue(&mut bits, 0);
        bits.push(true); // frame_mbs_only_flag
        bits.push(true); // direct_8x8_inference_flag
        bits.push(false); // frame_cropping_flag
        bits.push(true); // vui_parameters_present_flag
        bits.push(false); // aspect_ratio_info_present_flag
        bits.push(false); // overscan_info_present_flag
        bits.push(true); // video_signal_type_present_flag
        push_u(&mut bits, 5, 3); // video_format
        bits.push(true); // video_full_range_flag
        bits.push(false); // colour_description_present_flag
        bits.push(true); // rbsp_stop_one_bit
        let mut rbsp = Vec::with_capacity(bits.len() / 8 + 1);
        for chunk in bits.chunks(8) {
            let mut byte = 0u8;
            for (i, bit) in chunk.iter().enumerate() {
                if *bit {
                    byte |= 1 << (7 - i);
                }
            }
            rbsp.push(byte);
        }
        let mut annexb = vec![0x00, 0x00, 0x01, 0x67];
        annexb.extend_from_slice(&rbsp);
        assert_eq!(
            colour_tags(&annexb),
            ColourTags {
                matrix: VideoMatrix::Bt709,
                range: VideoRange::Full,
                matrix_coefficients: None,
            }
        );
    }

    #[test]
    fn malformed_input_never_panics_and_defaults() {
        // Truncated real SPS (reads past the end mid-parse).
        assert_eq!(
            colour_tags(&BARS_SEQUENCE_HEADER[..10]),
            ColourTags::default()
        );
        // Not Annex-B, not avcC.
        assert_eq!(colour_tags(&[0x01, 0x02, 0x03]), ColourTags::default());
        // Empty.
        assert_eq!(colour_tags(&[]), ColourTags::default());
        // SPS-shaped garbage: plausible start code, nonsense payload.
        let garbage = [0x00, 0x00, 0x01, 0x67, 0xFF, 0xFF, 0xFF, 0xFF];
        assert_eq!(colour_tags(&garbage), ColourTags::default());
    }

    #[test]
    fn unsupported_matrix_coefficients_keep_raw_value_and_default_matrix() {
        // matrix_coefficients = 7 (FCC): not in our mapping, so the default
        // matrix applies but the raw signal stays visible for logging.
        let tags = ColourTags::from_vui(Some(7), false);
        assert_eq!(tags.matrix, VideoMatrix::Bt709);
        assert_eq!(tags.matrix_coefficients, Some(7));
        // Unspecified (0) behaves the same.
        assert_eq!(
            ColourTags::from_vui(Some(0), false).matrix,
            VideoMatrix::Bt709
        );
        // BT.601 family maps both documented values.
        assert_eq!(
            ColourTags::from_vui(Some(5), false).matrix,
            VideoMatrix::Bt601
        );
        assert_eq!(
            ColourTags::from_vui(Some(6), true).matrix,
            VideoMatrix::Bt601
        );
        assert_eq!(ColourTags::from_vui(Some(6), true).range, VideoRange::Full);
    }
}
