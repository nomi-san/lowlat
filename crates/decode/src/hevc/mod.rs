//! HEVC: reading an access unit into what the device decodes it from.
//!
//! The shape of the H.264 reader: parameter sets kept, slice headers read,
//! the picture placed in the buffer, the reference lists derived, a [`Job`]
//! staged for the backend and the buffer settled once it has decoded.

pub mod dpb;
pub mod pps;
pub mod slice;
pub mod sps;

use crate::ParseError;
use crate::h264::slice::List;
use crate::nal::Units;
use dpb::{Current, Dpb, Output, RefList, RefPic};
use pps::Pps;
use slice::{SliceHeader, unit};
use sps::Sps;

type Result<T> = core::result::Result<T, ParseError>;

/// Slice segments a picture may carry.
pub const MAX_SLICES: usize = 32;

#[derive(Debug, Clone, Copy)]
pub struct Slice {
    pub header: SliceHeader,
    pub offset: usize,
    pub len: usize,
    pub l0: RefList,
    pub l1: RefList,
}

#[derive(Debug, Clone)]
pub struct Job {
    pub sps: Sps,
    pub pps: Pps,
    pub current: Current,
    pub references: List<Option<RefPic>, { dpb::MAX_PICTURES }>,
    pub slices: List<Option<Slice>, MAX_SLICES>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Read {
    Nothing,
    Picture,
    FormatChanged,
}

#[derive(Debug)]
pub struct Stream {
    sps: Box<[Option<Sps>; 16]>,
    pps: Box<[Option<Pps>; 64]>,
    active_sps: Option<Sps>,
    pub dpb: Dpb,
    job: Box<Job>,
    staged: bool,
}

impl Default for Stream {
    fn default() -> Self {
        Self::new()
    }
}

impl Stream {
    pub fn new() -> Self {
        let job = Box::new(Job {
            sps: placeholder_sps(),
            pps: placeholder_pps(),
            current: Current {
                slot: 0,
                poc: 0,
                output: false,
                irap: false,
                no_rasl_output: false,
            },
            references: List::default(),
            slices: List::default(),
        });
        Self {
            sps: Box::new([None; 16]),
            pps: Box::new([None; 64]),
            active_sps: None,
            dpb: Dpb::new(),
            job,
            staged: false,
        }
    }

    pub fn active_sps(&self) -> Option<&Sps> {
        self.active_sps.as_ref()
    }

    pub fn job(&self) -> Option<&Job> {
        self.staged.then_some(&*self.job)
    }

    pub fn next_output(&mut self) -> Option<Output> {
        self.dpb.next_output()
    }

    fn sps_of(&self, id: u8) -> Option<Sps> {
        self.sps.get(usize::from(id)).copied().flatten()
    }

    fn pps_of(&self, id: u8) -> Option<Pps> {
        self.pps.get(usize::from(id)).copied().flatten()
    }

    /// Read one access unit.
    pub fn read(&mut self, unit: &[u8]) -> Result<Read> {
        self.staged = false;
        self.job.slices.len = 0;
        let mut first: Option<SliceHeader> = None;
        let mut previous: Option<SliceHeader> = None;

        for nal in Units::new(unit) {
            let Some(&head) = nal.bytes.first() else {
                continue;
            };
            let unit_type = (head >> 1) & 0x3F;
            let payload = nal.bytes.get(2..).unwrap_or(&[]);
            match unit_type {
                unit::VPS => {}
                unit::SPS => {
                    let sps = sps::parse(payload)?;
                    let main = sps.profile_idc == 1 || sps.profile_compatibility & (1 << 30) != 0;
                    let main10 = sps.profile_idc == 2 || sps.profile_compatibility & (1 << 29) != 0;
                    let rext = sps.profile_idc == 4 || sps.profile_compatibility & (1 << 27) != 0;
                    // Main, Main 10 and the range extensions at 4:2:0 or
                    // 4:4:4, eight or ten bits, the two depths equal: what a
                    // device here decodes. Separate colour planes are three
                    // monochrome pictures, which no device takes as one.
                    if !(main || main10 || rext)
                        || !(sps.chroma_format_idc == 1 || sps.chroma_format_idc == 3)
                        || sps.separate_colour_plane
                        || sps.bit_depth_luma_minus8 > 2
                        || sps.bit_depth_luma_minus8 != sps.bit_depth_chroma_minus8
                    {
                        return Err(ParseError::Unsupported);
                    }
                    if let Some(slot) = self.sps.get_mut(usize::from(sps.id)) {
                        *slot = Some(sps);
                    }
                }
                unit::PPS => {
                    let pps = pps::parse(payload, |id| self.sps_of(id))?;
                    if let Some(slot) = self.pps.get_mut(usize::from(pps.id)) {
                        *slot = Some(pps);
                    }
                }
                unit::EOS | unit::EOB => self.dpb.end_of_sequence(),
                0..=9 | 16..=21 => {
                    let header = slice::parse(
                        nal.bytes,
                        |id| self.pps_of(id),
                        |id| self.sps_of(id),
                        previous.as_ref(),
                    )?;
                    if header.first_slice_segment_in_pic && first.is_some() {
                        // A second picture in one unit.
                        return Err(ParseError::Unsupported);
                    }
                    if first.is_none() {
                        if !header.first_slice_segment_in_pic {
                            // A picture that does not start at its start
                            // lost its first slice.
                            return Err(ParseError::OutOfRange);
                        }
                        first = Some(header);
                    }
                    previous = Some(header);
                    let slot = self
                        .job
                        .slices
                        .items
                        .get_mut(self.job.slices.len)
                        .ok_or(ParseError::TooMany)?;
                    *slot = Some(Slice {
                        header,
                        offset: nal.offset,
                        len: nal.bytes.len(),
                        l0: RefList::default(),
                        l1: RefList::default(),
                    });
                    self.job.slices.len += 1;
                }
                // Reserved picture types, delimiters, supplemental
                // information, filler: nothing to decode.
                _ => {}
            }
        }

        let Some(first) = first else {
            return Ok(Read::Nothing);
        };
        let pps = self
            .pps_of(first.pps_id)
            .ok_or(ParseError::NoParameterSet)?;
        let sps = self.sps_of(pps.sps_id).ok_or(ParseError::NoParameterSet)?;

        match self.active_sps {
            Some(active) if !same_format(&active, &sps) => {
                self.dpb.drain();
                self.active_sps = None;
                return Ok(Read::FormatChanged);
            }
            Some(active) if active != sps => {
                self.dpb.configure(&sps);
                self.active_sps = Some(sps);
            }
            Some(_) => {}
            None => {
                self.dpb.configure(&sps);
                self.active_sps = Some(sps);
            }
        }

        let Some(current) = self.dpb.begin(&sps, &first)? else {
            // A leading picture dropped at the start of a period.
            return Ok(Read::Nothing);
        };
        self.job.sps = sps;
        self.job.pps = pps;
        self.job.current = current;
        self.job.references = *self.dpb.references();
        for slice in self
            .job
            .slices
            .items
            .iter_mut()
            .take(self.job.slices.len)
            .flatten()
        {
            let (l0, l1) = self.dpb.reference_lists(&slice.header)?;
            slice.l0 = l0;
            slice.l1 = l1;
        }
        self.staged = true;
        Ok(Read::Picture)
    }

    pub fn finish(&mut self) -> Result<()> {
        if !self.staged {
            return Ok(());
        }
        self.staged = false;
        let first = self
            .job
            .slices
            .items
            .first()
            .copied()
            .flatten()
            .ok_or(ParseError::OutOfRange)?;
        self.dpb.finish(&self.job.current, &first.header)
    }

    pub fn abandon(&mut self) {
        self.staged = false;
    }

    /// Forget everything, without a fresh allocation.
    pub fn reset(&mut self) {
        for slot in self.sps.iter_mut() {
            *slot = None;
        }
        for slot in self.pps.iter_mut() {
            *slot = None;
        }
        self.active_sps = None;
        self.staged = false;
        self.job.slices.len = 0;
        self.dpb.clear();
    }

    pub fn drain(&mut self) {
        self.dpb.drain();
    }
}

fn same_format(a: &Sps, b: &Sps) -> bool {
    a.width == b.width
        && a.height == b.height
        && a.bit_depth_luma_minus8 == b.bit_depth_luma_minus8
        && a.chroma_format_idc == b.chroma_format_idc
}

fn placeholder_sps() -> Sps {
    Sps {
        id: 0,
        vps_id: 0,
        max_sub_layers_minus1: 0,
        profile_idc: 0,
        profile_compatibility: 0,
        chroma_format_idc: 1,
        separate_colour_plane: false,
        width: 0,
        height: 0,
        conformance_window: None,
        bit_depth_luma_minus8: 0,
        bit_depth_chroma_minus8: 0,
        log2_max_pic_order_cnt_lsb_minus4: 0,
        max_dec_pic_buffering_minus1: 0,
        max_num_reorder_pics: 0,
        max_latency_increase_plus1: 0,
        log2_min_luma_coding_block_size_minus3: 0,
        log2_diff_max_min_luma_coding_block_size: 0,
        log2_min_luma_transform_block_size_minus2: 0,
        log2_diff_max_min_luma_transform_block_size: 0,
        max_transform_hierarchy_depth_inter: 0,
        max_transform_hierarchy_depth_intra: 0,
        scaling_list_enabled: false,
        scaling: sps::ScalingLists::DEFAULT,
        amp_enabled: false,
        sample_adaptive_offset_enabled: false,
        pcm_enabled: false,
        pcm_sample_bit_depth_luma_minus1: 0,
        pcm_sample_bit_depth_chroma_minus1: 0,
        log2_min_pcm_luma_coding_block_size_minus3: 0,
        log2_diff_max_min_pcm_luma_coding_block_size: 0,
        pcm_loop_filter_disabled: false,
        num_short_term_ref_pic_sets: 0,
        st_rps: [sps::StRps::default(); sps::MAX_ST_RPS],
        long_term_ref_pics_present: false,
        num_long_term_ref_pics_sps: 0,
        lt_ref_pic_poc_lsb_sps: [0; sps::MAX_LT_SPS],
        used_by_curr_pic_lt_sps: [false; sps::MAX_LT_SPS],
        temporal_mvp_enabled: false,
        strong_intra_smoothing_enabled: false,
        range: sps::RangeExtension::default(),
    }
}

fn placeholder_pps() -> Pps {
    Pps {
        id: 0,
        sps_id: 0,
        dependent_slice_segments_enabled: false,
        output_flag_present: false,
        num_extra_slice_header_bits: 0,
        sign_data_hiding_enabled: false,
        cabac_init_present: false,
        num_ref_idx_l0_default_active_minus1: 0,
        num_ref_idx_l1_default_active_minus1: 0,
        init_qp_minus26: 0,
        constrained_intra_pred: false,
        transform_skip_enabled: false,
        cu_qp_delta_enabled: false,
        diff_cu_qp_delta_depth: 0,
        cb_qp_offset: 0,
        cr_qp_offset: 0,
        slice_chroma_qp_offsets_present: false,
        weighted_pred: false,
        weighted_bipred: false,
        transquant_bypass_enabled: false,
        tiles_enabled: false,
        entropy_coding_sync_enabled: false,
        num_tile_columns_minus1: 0,
        num_tile_rows_minus1: 0,
        uniform_spacing: true,
        column_width_minus1: [0; pps::MAX_TILE_COLUMNS],
        row_height_minus1: [0; pps::MAX_TILE_ROWS],
        loop_filter_across_tiles_enabled: true,
        loop_filter_across_slices_enabled: false,
        deblocking_filter_override_enabled: false,
        disable_deblocking_filter: false,
        beta_offset_div2: 0,
        tc_offset_div2: 0,
        scaling_list_data_present: false,
        scaling: sps::ScalingLists::DEFAULT,
        lists_modification_present: false,
        log2_parallel_merge_level_minus2: 0,
        slice_segment_header_extension_present: false,
        range: pps::RangeExtension::default(),
    }
}
