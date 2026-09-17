//! H.264: reading an access unit into what the device decodes it from.
//!
//! The [`Stream`] holds the parameter sets a stream has carried and its
//! picture buffer. Fed an access unit, it reads every slice's header,
//! places the picture in the buffer, derives the reference lists each slice
//! decodes against, and stages a [`Job`] in the standard's own terms for
//! the backend; once the backend has decoded it, [`Stream::finish`] marks
//! the buffer and decides what leaves it.

pub mod dpb;
pub mod pps;
pub mod slice;
pub mod sps;

use crate::ParseError;
use crate::nal::Units;
use dpb::{Current, Dpb, Output, RefPic};
use pps::Pps;
use slice::{List, MAX_REFS, SliceHeader};
use sps::Sps;

type Result<T> = core::result::Result<T, ParseError>;

/// Slices a picture may carry. A picture split finer than this is refused
/// rather than decoded in part.
pub const MAX_SLICES: usize = 32;

/// One slice of the current picture, ready for the device.
#[derive(Debug, Clone, Copy)]
pub struct Slice {
    pub header: SliceHeader,
    /// Where the unit's bytes lie in the access unit, header byte included.
    pub offset: usize,
    pub len: usize,
    pub l0: List<Option<RefPic>, MAX_REFS>,
    pub l1: List<Option<RefPic>, MAX_REFS>,
}

/// A picture to decode. Allocated once with the stream and filled in place.
#[derive(Debug, Clone)]
pub struct Job {
    pub sps: Sps,
    pub pps: Pps,
    pub current: Current,
    pub references: List<Option<RefPic>, 16>,
    pub slices: List<Option<Slice>, MAX_SLICES>,
}

/// What a fed access unit amounted to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Read {
    /// No picture: parameter sets or nothing decodable.
    Nothing,
    /// A picture is staged in [`Stream::job`].
    Picture,
    /// The sequence changed under the decoder: size or depth.
    FormatChanged,
}

/// The stream's state.
#[derive(Debug)]
pub struct Stream {
    sps: Box<[Option<Sps>; 32]>,
    pps: Box<[Option<Pps>; 256]>,
    /// The sequence in force: the one the active picture set names.
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
                structure: dpb::Structure::Frame,
                frame_num: 0,
                top_poc: 0,
                bottom_poc: 0,
                idr: false,
                reference: false,
                mmco5: false,
                pair_of: None,
            },
            references: List::default(),
            slices: List::default(),
        });
        Self {
            sps: Box::new([None; 32]),
            pps: Box::new([None; 256]),
            active_sps: None,
            dpb: Dpb::new(),
            job,
            staged: false,
        }
    }

    /// The sequence in force, once a picture has named one.
    pub fn active_sps(&self) -> Option<&Sps> {
        self.active_sps.as_ref()
    }

    /// The staged picture, if a read staged one.
    pub fn job(&self) -> Option<&Job> {
        self.staged.then_some(&*self.job)
    }

    /// The next picture leaving the buffer.
    pub fn next_output(&mut self) -> Option<Output> {
        self.dpb.next_output()
    }

    fn sps_of(&self, id: u8) -> Option<Sps> {
        self.sps.get(usize::from(id)).copied().flatten()
    }

    fn pps_of(&self, id: u8) -> Option<Pps> {
        self.pps.get(usize::from(id)).copied().flatten()
    }

    /// Read one access unit. A staged picture from an earlier read that was
    /// never finished is dropped.
    pub fn read(&mut self, unit: &[u8]) -> Result<Read> {
        self.staged = false;
        self.job.slices.len = 0;
        let mut first: Option<SliceHeader> = None;

        for nal in Units::new(unit) {
            let Some(&head) = nal.bytes.first() else {
                continue;
            };
            let unit_type = head & 0x1F;
            let payload = nal.bytes.get(1..).unwrap_or(&[]);
            match unit_type {
                7 => {
                    let sps = sps::parse(payload)?;
                    if sps.chroma_format_idc != 1
                        || sps.bit_depth_luma_minus8 != 0
                        || sps.bit_depth_chroma_minus8 != 0
                    {
                        // Only 4:2:0 at eight bits is decodable here.
                        return Err(ParseError::Unsupported);
                    }
                    if let Some(slot) = self.sps.get_mut(usize::from(sps.id)) {
                        *slot = Some(sps);
                    }
                }
                8 => {
                    let pps = pps::parse(payload, |id| self.sps_of(id))?;
                    if let Some(slot) = self.pps.get_mut(usize::from(pps.id)) {
                        *slot = Some(pps);
                    }
                }
                1 | 5 => {
                    let header =
                        slice::parse(nal.bytes, |id| self.pps_of(id), |id| self.sps_of(id))?;
                    if header.redundant_pic_cnt != 0 {
                        // Redundant pictures are for concealment; the
                        // primary is what is decoded.
                        continue;
                    }
                    match &first {
                        Some(first) if new_picture(first, &header, self) => {
                            // A second picture in one unit: fields of a pair
                            // framed together, or a stream this does not
                            // handle. Refused whole rather than decoded in
                            // part.
                            return Err(ParseError::Unsupported);
                        }
                        Some(_) => {}
                        None => first = Some(header),
                    }
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
                        l0: List::default(),
                        l1: List::default(),
                    });
                    self.job.slices.len += 1;
                }
                // Delimiters, supplemental information, end of sequence,
                // filler, extensions: nothing to decode.
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
                // A new sequence with another format: the caller rebuilds.
                // The buffer's pictures leave in order first.
                self.dpb.drain();
                self.active_sps = None;
                return Ok(Read::FormatChanged);
            }
            Some(active) if active != sps => {
                // Same format, other parameters: adopt them.
                self.dpb.configure(&sps);
                self.active_sps = Some(sps);
            }
            Some(_) => {}
            None => {
                self.dpb.configure(&sps);
                self.active_sps = Some(sps);
            }
        }

        let current = self.dpb.begin(&sps, &first)?;
        self.job.sps = sps;
        self.job.pps = pps;
        self.job.current = current;
        self.job.references = self.dpb.reference_frames(&current);
        for slice in self
            .job
            .slices
            .items
            .iter_mut()
            .take(self.job.slices.len)
            .flatten()
        {
            let (l0, l1) = self.dpb.reference_lists(&sps, &current, &slice.header)?;
            slice.l0 = l0;
            slice.l1 = l1;
        }
        self.staged = true;
        Ok(Read::Picture)
    }

    /// The staged picture has been decoded.
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
        self.dpb
            .finish(&self.job.sps, &self.job.current, &first.header)
    }

    /// The staged picture could not be decoded: the buffer forgets it.
    pub fn abandon(&mut self) {
        self.staged = false;
    }

    /// Forget everything: the parameter sets, the buffer, the staged
    /// picture. What a fresh decoder starts from, without a fresh
    /// allocation.
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

    /// Everything waiting leaves, in order.
    pub fn drain(&mut self) {
        self.dpb.drain();
    }
}

/// A sequence set with nothing in it, for the job before a stream has
/// carried one.
fn placeholder_sps() -> Sps {
    Sps {
        profile_idc: 0,
        constraint_flags: 0,
        level_idc: 0,
        id: 0,
        chroma_format_idc: 1,
        separate_colour_plane: false,
        bit_depth_luma_minus8: 0,
        bit_depth_chroma_minus8: 0,
        qpprime_y_zero_transform_bypass: false,
        scaling_matrix_present: false,
        scaling: sps::ScalingLists::FLAT,
        log2_max_frame_num_minus4: 0,
        pic_order_cnt_type: 0,
        log2_max_pic_order_cnt_lsb_minus4: 0,
        delta_pic_order_always_zero: false,
        offset_for_non_ref_pic: 0,
        offset_for_top_to_bottom_field: 0,
        num_ref_frames_in_pic_order_cnt_cycle: 0,
        offset_for_ref_frame: [0; 255],
        max_num_ref_frames: 0,
        gaps_in_frame_num_allowed: false,
        pic_width_in_mbs_minus1: 0,
        pic_height_in_map_units_minus1: 0,
        frame_mbs_only: true,
        mb_adaptive_frame_field: false,
        direct_8x8_inference: false,
        crop: None,
        vui: sps::Vui::default(),
    }
}

fn placeholder_pps() -> Pps {
    Pps {
        id: 0,
        sps_id: 0,
        entropy_coding_mode: false,
        bottom_field_pic_order_in_frame_present: false,
        num_slice_groups_minus1: 0,
        num_ref_idx_l0_default_active_minus1: 0,
        num_ref_idx_l1_default_active_minus1: 0,
        weighted_pred: false,
        weighted_bipred_idc: 0,
        pic_init_qp_minus26: 0,
        pic_init_qs_minus26: 0,
        chroma_qp_index_offset: 0,
        deblocking_filter_control_present: false,
        constrained_intra_pred: false,
        redundant_pic_cnt_present: false,
        transform_8x8_mode: false,
        scaling_matrix_present: false,
        scaling: sps::ScalingLists::FLAT,
        second_chroma_qp_index_offset: 0,
    }
}

/// The two sequences would need different surfaces.
fn same_format(a: &Sps, b: &Sps) -> bool {
    a.coded_width() == b.coded_width()
        && a.coded_height() == b.coded_height()
        && a.bit_depth_luma_minus8 == b.bit_depth_luma_minus8
        && a.chroma_format_idc == b.chroma_format_idc
}

/// 7.4.1.2.4: whether `next` begins a new primary coded picture after
/// `first`.
fn new_picture(first: &SliceHeader, next: &SliceHeader, stream: &Stream) -> bool {
    if first.frame_num != next.frame_num
        || first.pps_id != next.pps_id
        || first.field_pic != next.field_pic
        || (first.field_pic && first.bottom_field != next.bottom_field)
        || (first.nal_ref_idc == 0) != (next.nal_ref_idc == 0)
        || first.is_idr() != next.is_idr()
        || (first.is_idr() && first.idr_pic_id != next.idr_pic_id)
    {
        return true;
    }
    let poc_type = stream
        .pps_of(first.pps_id)
        .and_then(|p| stream.sps_of(p.sps_id))
        .map_or(0, |s| s.pic_order_cnt_type);
    match poc_type {
        0 => {
            first.pic_order_cnt_lsb != next.pic_order_cnt_lsb
                || first.delta_pic_order_cnt_bottom != next.delta_pic_order_cnt_bottom
        }
        1 => first.delta_pic_order_cnt != next.delta_pic_order_cnt,
        _ => false,
    }
}
