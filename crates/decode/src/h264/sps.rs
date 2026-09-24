//! The H.264 sequence parameter set.

use crate::ParseError;
use crate::bits::BitReader;

type Result<T> = core::result::Result<T, ParseError>;

/// The lists the standard names as the default when none is coded (Tables
/// 7-3 and 7-4), in raster order: the standard lists them in the coded
/// (zig-zag) order, and the device takes raster.
pub const DEFAULT_4X4_INTRA: [u8; 16] = [
    6, 13, 20, 28, 13, 20, 28, 32, 20, 28, 32, 37, 28, 32, 37, 42,
];
pub const DEFAULT_4X4_INTER: [u8; 16] = [
    10, 14, 20, 24, 14, 20, 24, 27, 20, 24, 27, 30, 24, 27, 30, 34,
];
pub const DEFAULT_8X8_INTRA: [u8; 64] = [
    6, 10, 13, 16, 18, 23, 25, 27, 10, 11, 16, 18, 23, 25, 27, 29, 13, 16, 18, 23, 25, 27, 29, 31,
    16, 18, 23, 25, 27, 29, 31, 33, 18, 23, 25, 27, 29, 31, 33, 36, 23, 25, 27, 29, 31, 33, 36, 38,
    25, 27, 29, 31, 33, 36, 38, 40, 27, 29, 31, 33, 36, 38, 40, 42,
];
pub const DEFAULT_8X8_INTER: [u8; 64] = [
    9, 13, 15, 17, 19, 21, 22, 24, 13, 13, 17, 19, 21, 22, 24, 25, 15, 17, 19, 21, 22, 24, 25, 27,
    17, 19, 21, 22, 24, 25, 27, 28, 19, 21, 22, 24, 25, 27, 28, 30, 21, 22, 24, 25, 27, 28, 30, 32,
    22, 24, 25, 27, 28, 30, 32, 33, 24, 25, 27, 28, 30, 32, 33, 35,
];

/// The zig-zag order a coded 4x4 list arrives in, to raster.
const ZIGZAG_4X4: [usize; 16] = [0, 1, 4, 8, 5, 2, 3, 6, 9, 12, 13, 10, 7, 11, 14, 15];
const ZIGZAG_8X8: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

/// Scaling lists in raster order, as the device wants them: six 4x4 and two
/// 8x8 (luma intra and inter; the 4:4:4 chroma 8x8 lists are refused with
/// the format).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScalingLists {
    pub list_4x4: [[u8; 16]; 6],
    pub list_8x8: [[u8; 64]; 2],
}

impl ScalingLists {
    /// All flat: what a stream with no lists means.
    pub const FLAT: Self = Self {
        list_4x4: [[16; 16]; 6],
        list_8x8: [[16; 64]; 2],
    };

    /// The fall-back rule for a 4x4 list that is not coded: the default for
    /// the first of each kind, the list before it otherwise.
    fn fallback_4x4(&mut self, i: usize) {
        let source = match i {
            0 => DEFAULT_4X4_INTRA,
            3 => DEFAULT_4X4_INTER,
            _ => self.list_4x4.get(i - 1).copied().unwrap_or([16; 16]),
        };
        if let Some(list) = self.list_4x4.get_mut(i) {
            *list = source;
        }
    }

    fn fallback_8x8(&mut self, i: usize) {
        if let Some(list) = self.list_8x8.get_mut(i) {
            *list = if i == 0 {
                DEFAULT_8X8_INTRA
            } else {
                DEFAULT_8X8_INTER
            };
        }
    }
}

/// One coded list: returns whether the default should be used instead.
fn scaling_list(r: &mut BitReader<'_>, out: &mut [u8], zigzag: &[usize]) -> Result<bool> {
    let mut last: i32 = 8;
    let mut next: i32 = 8;
    let mut use_default = false;
    for (j, &at) in zigzag.iter().enumerate() {
        if next != 0 {
            let delta = r.se()?;
            if !(-128..=127).contains(&delta) {
                return Err(ParseError::OutOfRange);
            }
            next = (last + delta + 256).rem_euclid(256);
            use_default = j == 0 && next == 0;
        }
        let value = if next == 0 { last } else { next };
        *out.get_mut(at).ok_or(ParseError::OutOfRange)? =
            u8::try_from(value).map_err(|_| ParseError::OutOfRange)?;
        last = value;
    }
    Ok(use_default)
}

/// Parse the scaling-list syntax shared by the SPS and the PPS into `lists`,
/// which the caller has pre-filled with what an uncoded list falls back to
/// under rule B. `count` is 8 for 4:2:0.
///
/// The bit-exact fall-back: a list whose flag is clear falls back to the
/// list before it (or the default for the first of each kind) under rule A
/// at the SPS, and to the SPS's list under rule B at the PPS; a coded list
/// whose first delta lands on zero uses the default.
pub fn scaling_lists(
    r: &mut BitReader<'_>,
    lists: &mut ScalingLists,
    count: usize,
    rule_a: bool,
) -> Result<()> {
    for i in 0..count.min(8) {
        let present = r.flag()?;
        if i < 6 {
            if present {
                let mut coded = [0u8; 16];
                let list = if scaling_list(r, &mut coded, &ZIGZAG_4X4)? {
                    if i < 3 {
                        DEFAULT_4X4_INTRA
                    } else {
                        DEFAULT_4X4_INTER
                    }
                } else {
                    coded
                };
                if let Some(slot) = lists.list_4x4.get_mut(i) {
                    *slot = list;
                }
            } else if rule_a || i % 3 != 0 {
                // Rule A, and rule B's "not the first of its kind" case,
                // both take the previous list; rule B's first of a kind
                // keeps what the caller filled in from the sequence set.
                lists.fallback_4x4(i);
            }
        } else {
            let k = i - 6;
            if present {
                let mut coded = [0u8; 64];
                if scaling_list(r, &mut coded, &ZIGZAG_8X8)? {
                    lists.fallback_8x8(k);
                } else if let Some(slot) = lists.list_8x8.get_mut(k) {
                    *slot = coded;
                }
            } else if rule_a {
                lists.fallback_8x8(k);
            }
        }
    }
    Ok(())
}

/// What the VUI carries that a decoder or a renderer acts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Vui {
    /// From `bitstream_restriction`, if it was coded.
    pub max_num_reorder_frames: Option<u32>,
    pub max_dec_frame_buffering: Option<u32>,
    /// The samples span their depth's whole range rather than the video
    /// range. Clear when the stream says nothing, which is what the
    /// standard infers.
    pub video_full_range: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sps {
    pub profile_idc: u8,
    pub constraint_flags: u8,
    pub level_idc: u8,
    pub id: u8,
    pub chroma_format_idc: u8,
    pub separate_colour_plane: bool,
    pub bit_depth_luma_minus8: u8,
    pub bit_depth_chroma_minus8: u8,
    pub qpprime_y_zero_transform_bypass: bool,
    pub scaling_matrix_present: bool,
    pub scaling: ScalingLists,
    pub log2_max_frame_num_minus4: u8,
    pub pic_order_cnt_type: u8,
    pub log2_max_pic_order_cnt_lsb_minus4: u8,
    pub delta_pic_order_always_zero: bool,
    pub offset_for_non_ref_pic: i32,
    pub offset_for_top_to_bottom_field: i32,
    pub num_ref_frames_in_pic_order_cnt_cycle: u8,
    pub offset_for_ref_frame: [i32; 255],
    pub max_num_ref_frames: u8,
    pub gaps_in_frame_num_allowed: bool,
    pub pic_width_in_mbs_minus1: u16,
    pub pic_height_in_map_units_minus1: u16,
    pub frame_mbs_only: bool,
    pub mb_adaptive_frame_field: bool,
    pub direct_8x8_inference: bool,
    pub crop: Option<[u32; 4]>,
    pub vui: Vui,
}

impl Sps {
    /// Coded width in luma samples.
    pub fn coded_width(&self) -> u32 {
        (u32::from(self.pic_width_in_mbs_minus1) + 1) * 16
    }

    /// Coded height in luma samples (frame height, whatever the coding).
    pub fn coded_height(&self) -> u32 {
        (u32::from(self.pic_height_in_map_units_minus1) + 1)
            * 16
            * if self.frame_mbs_only { 1 } else { 2 }
    }

    /// Height in map units of a frame.
    pub fn frame_height_in_mbs(&self) -> u32 {
        (u32::from(self.pic_height_in_map_units_minus1) + 1)
            * if self.frame_mbs_only { 1 } else { 2 }
    }

    /// The visible picture after cropping.
    pub fn visible(&self) -> (u32, u32) {
        let (w, h) = (self.coded_width(), self.coded_height());
        let Some([left, right, top, bottom]) = self.crop else {
            return (w, h);
        };
        // Crop units for 4:2:0: 2 horizontally, 2 x (2 - frame_mbs_only)
        // vertically. Other formats are refused before this is asked.
        let unit_x = if self.chroma_format_idc == 0 || self.chroma_format_idc == 3 {
            1
        } else {
            2
        };
        let unit_y = (if self.chroma_format_idc == 1 { 2 } else { 1 })
            * if self.frame_mbs_only { 1 } else { 2 };
        let cw = w.saturating_sub((left + right).saturating_mul(unit_x));
        let ch = h.saturating_sub((top + bottom).saturating_mul(unit_y));
        (cw.max(1), ch.max(1))
    }

    pub fn max_frame_num(&self) -> u32 {
        1 << (u32::from(self.log2_max_frame_num_minus4) + 4)
    }

    pub fn max_pic_order_cnt_lsb(&self) -> u32 {
        1 << (u32::from(self.log2_max_pic_order_cnt_lsb_minus4) + 4)
    }

    /// The DPB size in frames the level allows at this picture size
    /// (Table A-1), capped at sixteen.
    pub fn max_dpb_frames(&self) -> u32 {
        let max_dpb_mbs: u32 = match self.level_idc {
            9 => 396,
            10 => 396,
            11 => {
                if self.constraint_flags & 0x10 != 0 && self.profile_idc != 100 {
                    396
                } else {
                    900
                }
            }
            12 | 13 | 20 => 2376,
            21 => 4752,
            22 | 30 => 8100,
            31 => 18000,
            32 => 20480,
            40 | 41 => 32768,
            42 => 34816,
            50 => 110400,
            51 | 52 => 184320,
            _ => 696320,
        };
        let frame_mbs = (u32::from(self.pic_width_in_mbs_minus1) + 1) * self.frame_height_in_mbs();
        (max_dpb_mbs / frame_mbs.max(1)).clamp(1, 16)
    }

    /// How many frames the decoder holds back before output: what the
    /// stream said, or nothing when it said nothing (the adaptive rule in
    /// the picture buffer covers a stream that reorders without saying so).
    pub fn reorder_frames(&self) -> Option<u32> {
        self.vui.max_num_reorder_frames
    }

    /// How many frames the picture buffer must hold.
    pub fn dpb_frames(&self) -> u32 {
        let declared = self
            .vui
            .max_dec_frame_buffering
            .unwrap_or_else(|| self.max_dpb_frames());
        declared
            .max(u32::from(self.max_num_ref_frames))
            .clamp(1, 16)
    }
}

fn hrd_parameters(r: &mut BitReader<'_>) -> Result<()> {
    let cpb_cnt_minus1 = r.ue_max(31)?;
    r.skip(4)?; // bit_rate_scale
    r.skip(4)?; // cpb_size_scale
    for _ in 0..=cpb_cnt_minus1 {
        r.ue()?; // bit_rate_value_minus1
        r.ue()?; // cpb_size_value_minus1
        r.flag()?; // cbr_flag
    }
    r.skip(5)?; // initial_cpb_removal_delay_length_minus1
    r.skip(5)?; // cpb_removal_delay_length_minus1
    r.skip(5)?; // dpb_output_delay_length_minus1
    r.skip(5)?; // time_offset_length
    Ok(())
}

fn vui_parameters(r: &mut BitReader<'_>) -> Result<Vui> {
    let mut vui = Vui::default();
    if r.flag()? {
        // aspect_ratio_info_present
        let idc = r.u8(8)?;
        if idc == 255 {
            r.skip(16)?;
            r.skip(16)?;
        }
    }
    if r.flag()? {
        // overscan_info_present
        r.flag()?;
    }
    if r.flag()? {
        // video_signal_type_present
        r.skip(3)?; // video_format
        vui.video_full_range = r.flag()?;
        if r.flag()? {
            // colour_description_present
            r.skip(8)?;
            r.skip(8)?;
            r.skip(8)?;
        }
    }
    if r.flag()? {
        // chroma_loc_info_present
        r.ue()?;
        r.ue()?;
    }
    if r.flag()? {
        // timing_info_present
        r.skip(32)?;
        r.skip(32)?;
        r.flag()?;
    }
    let nal_hrd = r.flag()?;
    if nal_hrd {
        hrd_parameters(r)?;
    }
    let vcl_hrd = r.flag()?;
    if vcl_hrd {
        hrd_parameters(r)?;
    }
    if nal_hrd || vcl_hrd {
        r.flag()?; // low_delay_hrd
    }
    r.flag()?; // pic_struct_present
    if r.flag()? {
        // bitstream_restriction
        r.flag()?; // motion_vectors_over_pic_boundaries
        r.ue()?; // max_bytes_per_pic_denom
        r.ue()?; // max_bits_per_mb_denom
        r.ue()?; // log2_max_mv_length_horizontal
        r.ue()?; // log2_max_mv_length_vertical
        vui.max_num_reorder_frames = Some(r.ue_max(16)?);
        vui.max_dec_frame_buffering = Some(r.ue_max(16)?);
    }
    Ok(vui)
}

/// Parse a sequence parameter set from its payload (after the header byte).
pub fn parse(payload: &[u8]) -> Result<Sps> {
    let mut r = BitReader::new(payload);
    let profile_idc = r.u8(8)?;
    let constraint_flags = r.u8(8)?;
    let level_idc = r.u8(8)?;
    let id = u8::try_from(r.ue_max(31)?).map_err(|_| ParseError::OutOfRange)?;

    let mut chroma_format_idc = 1u8;
    let mut separate_colour_plane = false;
    let mut bit_depth_luma_minus8 = 0u8;
    let mut bit_depth_chroma_minus8 = 0u8;
    let mut qpprime_y_zero_transform_bypass = false;
    let mut scaling_matrix_present = false;
    let mut scaling = ScalingLists::FLAT;
    if matches!(
        profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        chroma_format_idc = u8::try_from(r.ue_max(3)?).map_err(|_| ParseError::OutOfRange)?;
        if chroma_format_idc == 3 {
            separate_colour_plane = r.flag()?;
        }
        bit_depth_luma_minus8 = u8::try_from(r.ue_max(6)?).map_err(|_| ParseError::OutOfRange)?;
        bit_depth_chroma_minus8 = u8::try_from(r.ue_max(6)?).map_err(|_| ParseError::OutOfRange)?;
        qpprime_y_zero_transform_bypass = r.flag()?;
        scaling_matrix_present = r.flag()?;
        if scaling_matrix_present {
            let count = if chroma_format_idc == 3 { 12 } else { 8 };
            scaling_lists(&mut r, &mut scaling, count, true)?;
        }
    }

    let log2_max_frame_num_minus4 =
        u8::try_from(r.ue_max(12)?).map_err(|_| ParseError::OutOfRange)?;
    let pic_order_cnt_type = u8::try_from(r.ue_max(2)?).map_err(|_| ParseError::OutOfRange)?;
    let mut log2_max_pic_order_cnt_lsb_minus4 = 0u8;
    let mut delta_pic_order_always_zero = false;
    let mut offset_for_non_ref_pic = 0i32;
    let mut offset_for_top_to_bottom_field = 0i32;
    let mut num_ref_frames_in_pic_order_cnt_cycle = 0u8;
    let mut offset_for_ref_frame = [0i32; 255];
    match pic_order_cnt_type {
        0 => {
            log2_max_pic_order_cnt_lsb_minus4 =
                u8::try_from(r.ue_max(12)?).map_err(|_| ParseError::OutOfRange)?;
        }
        1 => {
            delta_pic_order_always_zero = r.flag()?;
            offset_for_non_ref_pic = r.se()?;
            offset_for_top_to_bottom_field = r.se()?;
            num_ref_frames_in_pic_order_cnt_cycle =
                u8::try_from(r.ue_max(255)?).map_err(|_| ParseError::OutOfRange)?;
            for i in 0..usize::from(num_ref_frames_in_pic_order_cnt_cycle) {
                *offset_for_ref_frame
                    .get_mut(i)
                    .ok_or(ParseError::OutOfRange)? = r.se()?;
            }
        }
        _ => {}
    }
    let max_num_ref_frames = u8::try_from(r.ue_max(16)?).map_err(|_| ParseError::OutOfRange)?;
    let gaps_in_frame_num_allowed = r.flag()?;
    let pic_width_in_mbs_minus1 =
        u16::try_from(r.ue_max(1023)?).map_err(|_| ParseError::OutOfRange)?;
    let pic_height_in_map_units_minus1 =
        u16::try_from(r.ue_max(1023)?).map_err(|_| ParseError::OutOfRange)?;
    let frame_mbs_only = r.flag()?;
    let mut mb_adaptive_frame_field = false;
    if !frame_mbs_only {
        mb_adaptive_frame_field = r.flag()?;
    }
    let direct_8x8_inference = r.flag()?;
    let crop = if r.flag()? {
        Some([r.ue()?, r.ue()?, r.ue()?, r.ue()?])
    } else {
        None
    };
    let vui = if r.flag()? {
        vui_parameters(&mut r)?
    } else {
        Vui::default()
    };

    Ok(Sps {
        profile_idc,
        constraint_flags,
        level_idc,
        id,
        chroma_format_idc,
        separate_colour_plane,
        bit_depth_luma_minus8,
        bit_depth_chroma_minus8,
        qpprime_y_zero_transform_bypass,
        scaling_matrix_present,
        scaling,
        log2_max_frame_num_minus4,
        pic_order_cnt_type,
        log2_max_pic_order_cnt_lsb_minus4,
        delta_pic_order_always_zero,
        offset_for_non_ref_pic,
        offset_for_top_to_bottom_field,
        num_ref_frames_in_pic_order_cnt_cycle,
        offset_for_ref_frame,
        max_num_ref_frames,
        gaps_in_frame_num_allowed,
        pic_width_in_mbs_minus1,
        pic_height_in_map_units_minus1,
        frame_mbs_only,
        mb_adaptive_frame_field,
        direct_8x8_inference,
        crop,
        vui,
    })
}
