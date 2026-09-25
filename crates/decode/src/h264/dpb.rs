//! The H.264 decoded picture buffer: picture order, reference marking, the
//! reference lists a slice decodes against, and the order pictures leave.
//!
//! Frames are the unit of storage and a field is half of one: a
//! complementary field pair shares an entry and a surface, each field with
//! its own order count and its own marking, exactly as the standard has it.
//! Every list here is a fixed array; the standard bounds them all.

use crate::ParseError;
use crate::h264::slice::{List, MAX_REFS, Marking, Mmco, Modification, SliceHeader, SliceType};
use crate::h264::sps::Sps;

type Result<T> = core::result::Result<T, ParseError>;

/// Frames the buffer can hold: the standard's sixteen, the picture being
/// decoded, and one spare for a picture read back after its place was taken.
pub const MAX_FRAMES: usize = 18;

/// Where a picture's samples live, as the backend numbers its surfaces.
pub type Slot = usize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Reference {
    #[default]
    Unused,
    Short,
    Long,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Parity {
    Top,
    Bottom,
}

impl Parity {
    fn index(self) -> usize {
        match self {
            Self::Top => 0,
            Self::Bottom => 1,
        }
    }

    fn opposite(self) -> Self {
        match self {
            Self::Top => Self::Bottom,
            Self::Bottom => Self::Top,
        }
    }
}

/// How the current picture is coded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Structure {
    Frame,
    Field(Parity),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Field {
    pub poc: i32,
    pub reference: Reference,
    /// Whether this field has been decoded (a frame decodes both).
    pub present: bool,
}

/// One frame in the buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    pub slot: Slot,
    pub frame_num: u32,
    /// Set for every entry before a picture's lists are built.
    pub frame_num_wrap: i32,
    pub long_term_frame_idx: u32,
    /// Top, then bottom.
    pub fields: [Field; 2],
    /// Inserted for a gap in frame numbers: never output.
    pub non_existing: bool,
    pub output_needed: bool,
}

impl Entry {
    fn is_reference(&self) -> bool {
        self.fields.iter().any(|f| f.reference != Reference::Unused)
    }

    fn has_short(&self) -> bool {
        self.fields.iter().any(|f| f.reference == Reference::Short)
    }

    fn has_long(&self) -> bool {
        self.fields.iter().any(|f| f.reference == Reference::Long)
    }

    /// Both fields present and marked alike: what a frame reference is.
    fn frame_reference(&self, kind: Reference) -> bool {
        self.fields.iter().all(|f| f.present && f.reference == kind)
    }

    /// The order count of the frame, over the fields present.
    pub fn poc(&self) -> i32 {
        match (self.fields[0].present, self.fields[1].present) {
            (true, true) => self.fields[0].poc.min(self.fields[1].poc),
            (true, false) => self.fields[0].poc,
            (false, true) => self.fields[1].poc,
            (false, false) => i32::MAX,
        }
    }

    /// The order count over the fields marked with `kind`, for the field
    /// list orderings.
    fn poc_marked(&self, kind: Reference) -> i32 {
        let marked: [Option<i32>; 2] = [
            (self.fields[0].present && self.fields[0].reference == kind)
                .then_some(self.fields[0].poc),
            (self.fields[1].present && self.fields[1].reference == kind)
                .then_some(self.fields[1].poc),
        ];
        match marked {
            [Some(a), Some(b)] => a.min(b),
            [Some(a), None] | [None, Some(a)] => a,
            [None, None] => i32::MAX,
        }
    }

    fn field(&self, parity: Parity) -> &Field {
        // Two fields, two parities; the index is always in range.
        self.fields.get(parity.index()).unwrap_or(&self.fields[0])
    }

    fn field_mut(&mut self, parity: Parity) -> &mut Field {
        let index = parity.index();
        if index == 0 {
            &mut self.fields[0]
        } else {
            &mut self.fields[1]
        }
    }
}

/// A reference in a slice's list, as the device is told.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefPic {
    pub slot: Slot,
    pub top_poc: i32,
    pub bottom_poc: i32,
    /// The frame number for a short-term reference, the long-term index
    /// for a long-term one.
    pub frame_idx: u32,
    pub long_term: bool,
    /// The field referenced, or the whole frame.
    pub parity: Option<Parity>,
}

/// A slice's reference list.
pub type RefList = List<Option<RefPic>, MAX_REFS>;

/// A picture ready to leave the buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Output {
    pub slot: Slot,
    pub poc: i32,
}

/// What a picture is, once its first slice has been read: the values the
/// buffer needs to place it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Current {
    pub slot: Slot,
    pub structure: Structure,
    pub frame_num: u32,
    pub top_poc: i32,
    pub bottom_poc: i32,
    pub idr: bool,
    pub reference: bool,
    pub mmco5: bool,
    /// The entry index of the first field when this is the second of a pair.
    pub pair_of: Option<usize>,
}

/// The previous picture's values the order count derivation carries.
#[derive(Debug, Clone, Copy, Default)]
struct Previous {
    poc_msb: i32,
    poc_lsb: u32,
    frame_num: u32,
    frame_num_offset: u32,
    ref_frame_num: u32,
    /// The last picture carried a reset, and how its counts ended.
    mmco5: bool,
    mmco5_bottom: bool,
    top_poc_after_mmco5: i32,
}

#[derive(Debug)]
pub struct Dpb {
    entries: [Option<Entry>; MAX_FRAMES],
    /// Frames the stream may hold in the buffer (references plus pictures
    /// waiting to leave), from the sequence set.
    capacity: usize,
    max_num_ref_frames: u32,
    max_long_term_frame_idx: Option<u32>,
    /// Pictures held back before output. From the stream when it said;
    /// otherwise zero, raised when a picture proves it reorders.
    reorder: u32,
    reorder_declared: bool,
    last_output_poc: Option<i32>,
    previous_decoded_poc: Option<i32>,
    previous: Previous,
    /// Slots the backend numbers: held by an entry, held for read-back, or
    /// free.
    slot_in_dpb: [bool; MAX_FRAMES],
    slot_pending_output: [bool; MAX_FRAMES],
    output: List<Option<Output>, MAX_FRAMES>,
}

impl Default for Dpb {
    fn default() -> Self {
        Self::new()
    }
}

impl Dpb {
    pub fn new() -> Self {
        Self {
            entries: [None; MAX_FRAMES],
            capacity: 1,
            max_num_ref_frames: 1,
            max_long_term_frame_idx: None,
            reorder: 0,
            reorder_declared: false,
            last_output_poc: None,
            previous_decoded_poc: None,
            previous: Previous::default(),
            slot_in_dpb: [false; MAX_FRAMES],
            slot_pending_output: [false; MAX_FRAMES],
            output: List::default(),
        }
    }

    /// Size the buffer for a sequence. Called at the sequence set that
    /// builds the decoder; a change of size is a rebuild.
    pub fn configure(&mut self, sps: &Sps) {
        self.capacity = usize::try_from(sps.dpb_frames()).unwrap_or(16).min(16);
        self.max_num_ref_frames = u32::from(sps.max_num_ref_frames).max(1);
        if let Some(reorder) = sps.reorder_frames() {
            self.reorder = reorder;
            self.reorder_declared = true;
        }
    }

    /// Slots the backend must provide.
    pub fn slots(&self) -> usize {
        MAX_FRAMES
    }

    /// The reorder depth in force.
    pub fn reorder(&self) -> u32 {
        self.reorder
    }

    /// Remove everything, as a fresh sequence does.
    pub fn clear(&mut self) {
        for index in 0..MAX_FRAMES {
            if let Some(e) = self.entries.get_mut(index).and_then(|e| e.take()) {
                self.release_slot(e.slot);
            }
        }
        self.max_long_term_frame_idx = None;
        self.last_output_poc = None;
        self.previous_decoded_poc = None;
        self.previous = Previous::default();
        self.reorder = 0;
        self.reorder_declared = false;
        self.output = List::default();
        self.slot_pending_output = [false; MAX_FRAMES];
    }

    fn release_slot(&mut self, slot: Slot) {
        if let Some(held) = self.slot_in_dpb.get_mut(slot) {
            *held = false;
        }
    }

    /// The backend has read this picture back; its slot may be reused once
    /// the buffer no longer references it.
    pub fn taken(&mut self, slot: Slot) {
        if let Some(pending) = self.slot_pending_output.get_mut(slot) {
            *pending = false;
        }
    }

    /// The next picture to leave, in output order.
    pub fn next_output(&mut self) -> Option<Output> {
        if self.output.len == 0 {
            return None;
        }
        let first = self.output.items.first().copied().flatten();
        // Shift down: at most eighteen entries, once per output.
        for i in 1..self.output.len {
            let next = self.output.items.get(i).copied().flatten();
            if let Some(slot) = self.output.items.get_mut(i - 1) {
                *slot = next;
            }
        }
        self.output.len -= 1;
        first
    }

    /// Whether a picture is waiting to be taken.
    pub fn has_output(&self) -> bool {
        self.output.len > 0
    }

    fn free_slot(&self) -> Option<Slot> {
        (0..MAX_FRAMES).find(|&s| {
            !self.slot_in_dpb.get(s).copied().unwrap_or(true)
                && !self.slot_pending_output.get(s).copied().unwrap_or(true)
        })
    }

    fn entry_count(&self) -> usize {
        self.entries.iter().filter(|e| e.is_some()).count()
    }

    fn short_term_count(&self) -> u32 {
        let count = self
            .entries
            .iter()
            .flatten()
            .filter(|e| e.has_short())
            .count();
        u32::try_from(count).unwrap_or(u32::MAX)
    }

    fn long_term_count(&self) -> u32 {
        let count = self
            .entries
            .iter()
            .flatten()
            .filter(|e| e.has_long())
            .count();
        u32::try_from(count).unwrap_or(u32::MAX)
    }

    /// Drop entries that are neither referenced nor waiting for output.
    fn compact(&mut self) {
        for i in 0..MAX_FRAMES {
            let done = match self.entries.get(i).copied().flatten() {
                Some(e) => !e.is_reference() && !e.output_needed,
                None => false,
            };
            if done {
                if let Some(e) = self.entries.get_mut(i).and_then(|e| e.take()) {
                    self.release_slot(e.slot);
                }
            }
        }
    }

    /// Emit the waiting picture with the smallest order count.
    fn bump_one(&mut self) -> bool {
        let mut best: Option<(usize, i32)> = None;
        for (i, e) in self.entries.iter().enumerate() {
            if let Some(e) = e {
                if e.output_needed && !e.non_existing && best.is_none_or(|(_, poc)| e.poc() < poc) {
                    best = Some((i, e.poc()));
                }
            }
        }
        let Some((index, poc)) = best else {
            return false;
        };
        if let Some(Some(e)) = self.entries.get_mut(index) {
            e.output_needed = false;
            let slot = e.slot;
            if let Some(p) = self.slot_pending_output.get_mut(slot) {
                *p = true;
            }
            let _ = self
                .output
                .items
                .get_mut(self.output.len)
                .map(|o| *o = Some(Output { slot, poc }));
            self.output.len = (self.output.len + 1).min(MAX_FRAMES);
            self.last_output_poc = Some(poc);
        }
        self.compact();
        true
    }

    fn waiting_output(&self) -> u32 {
        let count = self
            .entries
            .iter()
            .flatten()
            .filter(|e| e.output_needed && !e.non_existing)
            .count();
        u32::try_from(count).unwrap_or(u32::MAX)
    }

    /// Output everything waiting, in order.
    fn flush(&mut self) {
        while self.bump_one() {}
    }

    /// Discard everything waiting without output.
    fn discard_waiting(&mut self) {
        for e in self.entries.iter_mut().flatten() {
            e.output_needed = false;
        }
        self.compact();
    }

    // ----- the order count (8.2.1) -----

    /// Derive the current picture's order counts and frame-number offsets
    /// from its first slice.
    pub fn order_counts(&self, sps: &Sps, header: &SliceHeader) -> (i32, i32) {
        let structure = if header.field_pic {
            Structure::Field(if header.bottom_field {
                Parity::Bottom
            } else {
                Parity::Top
            })
        } else {
            Structure::Frame
        };
        let idr = header.is_idr();
        match sps.pic_order_cnt_type {
            0 => {
                let (prev_msb, prev_lsb) = if idr {
                    (0, 0)
                } else if self.previous.mmco5 {
                    if self.previous.mmco5_bottom {
                        (0, 0)
                    } else {
                        (
                            0,
                            u32::try_from(self.previous.top_poc_after_mmco5).unwrap_or(0),
                        )
                    }
                } else {
                    (self.previous.poc_msb, self.previous.poc_lsb)
                };
                let max_lsb = i64::from(sps.max_pic_order_cnt_lsb());
                let lsb = i64::from(header.pic_order_cnt_lsb);
                let prev_lsb = i64::from(prev_lsb);
                let prev_msb = i64::from(prev_msb);
                let msb = if lsb < prev_lsb && (prev_lsb - lsb) >= max_lsb / 2 {
                    prev_msb + max_lsb
                } else if lsb > prev_lsb && (lsb - prev_lsb) > max_lsb / 2 {
                    prev_msb - max_lsb
                } else {
                    prev_msb
                };
                let top = i32::try_from(msb + lsb).unwrap_or(0);
                match structure {
                    Structure::Frame => (top, top.wrapping_add(header.delta_pic_order_cnt_bottom)),
                    Structure::Field(Parity::Top) => (top, top),
                    Structure::Field(Parity::Bottom) => (top, top),
                }
            }
            1 => {
                let offset = self.frame_num_offset(sps, header);
                let cycle = u32::from(sps.num_ref_frames_in_pic_order_cnt_cycle);
                let mut abs_frame_num = if cycle != 0 {
                    offset.wrapping_add(header.frame_num)
                } else {
                    0
                };
                if header.nal_ref_idc == 0 && abs_frame_num > 0 {
                    abs_frame_num -= 1;
                }
                let mut expected: i64 = 0;
                if abs_frame_num > 0 && cycle != 0 {
                    let cycle_count = (abs_frame_num - 1) / cycle;
                    let in_cycle = (abs_frame_num - 1) % cycle;
                    let per_cycle: i64 = sps
                        .offset_for_ref_frame
                        .iter()
                        .take(cycle as usize)
                        .map(|&o| i64::from(o))
                        .sum();
                    expected = i64::from(cycle_count) * per_cycle;
                    for o in sps.offset_for_ref_frame.iter().take(in_cycle as usize + 1) {
                        expected += i64::from(*o);
                    }
                }
                if header.nal_ref_idc == 0 {
                    expected += i64::from(sps.offset_for_non_ref_pic);
                }
                let expected = i32::try_from(expected).unwrap_or(0);
                match structure {
                    Structure::Frame => {
                        let top = expected.wrapping_add(header.delta_pic_order_cnt[0]);
                        let bottom = top
                            .wrapping_add(sps.offset_for_top_to_bottom_field)
                            .wrapping_add(header.delta_pic_order_cnt[1]);
                        (top, bottom)
                    }
                    Structure::Field(Parity::Top) => {
                        let top = expected.wrapping_add(header.delta_pic_order_cnt[0]);
                        (top, top)
                    }
                    Structure::Field(Parity::Bottom) => {
                        let bottom = expected
                            .wrapping_add(sps.offset_for_top_to_bottom_field)
                            .wrapping_add(header.delta_pic_order_cnt[0]);
                        (bottom, bottom)
                    }
                }
            }
            _ => {
                let offset = self.frame_num_offset(sps, header);
                let temp = if idr {
                    0
                } else {
                    let n = i64::from(offset.wrapping_add(header.frame_num));
                    if header.nal_ref_idc == 0 {
                        2 * n - 1
                    } else {
                        2 * n
                    }
                };
                let temp = i32::try_from(temp).unwrap_or(0);
                (temp, temp)
            }
        }
    }

    /// The frame-number offset the type 1 and 2 derivations carry forward.
    fn frame_num_offset(&self, sps: &Sps, header: &SliceHeader) -> u32 {
        if header.is_idr() {
            return 0;
        }
        let prev = if self.previous.mmco5 {
            0
        } else {
            self.previous.frame_num_offset
        };
        if self.previous.frame_num > header.frame_num {
            prev.wrapping_add(sps.max_frame_num())
        } else {
            prev
        }
    }

    // ----- placing the current picture -----

    /// Whether the picture is the second field of the pair whose first field
    /// is the last entry decoded: same frame number, opposite parity, the
    /// first field alone and not yet paired.
    fn pair_of(&self, header: &SliceHeader, parity: Parity) -> Option<usize> {
        if header.is_idr() {
            return None;
        }
        let mut latest: Option<(usize, Entry)> = None;
        for (i, e) in self.entries.iter().enumerate() {
            if let Some(e) = e {
                if !e.non_existing
                    && e.frame_num == header.frame_num
                    && e.field(parity.opposite()).present
                    && !e.field(parity).present
                {
                    latest = Some((i, *e));
                }
            }
        }
        // A first field that is a reference can only pair with a reference,
        // and one that is not with a non-reference.
        latest
            .filter(|(_, e)| {
                e.field(parity.opposite()).reference != Reference::Unused || !header.is_reference()
            })
            .map(|(i, _)| i)
    }

    /// Begin the current picture: detect a gap in frame numbers and fill it,
    /// find the pair for a second field, or claim a fresh slot. Returns what
    /// the buffer knows about the picture.
    pub fn begin(&mut self, sps: &Sps, header: &SliceHeader) -> Result<Current> {
        let structure = if header.field_pic {
            Structure::Field(if header.bottom_field {
                Parity::Bottom
            } else {
                Parity::Top
            })
        } else {
            Structure::Frame
        };
        let idr = header.is_idr();
        let mmco5 = header.has_mmco5();

        if idr {
            // Prior pictures leave first, or are dropped if the stream says.
            let drop = matches!(
                header.marking,
                Marking::Idr {
                    no_output_of_prior_pics: true,
                    ..
                }
            );
            for e in self.entries.iter_mut().flatten() {
                for f in e.fields.iter_mut() {
                    f.reference = Reference::Unused;
                }
            }
            if drop {
                self.discard_waiting();
            } else {
                self.flush();
            }
            self.compact();
            self.max_long_term_frame_idx = None;
            self.previous.ref_frame_num = 0;
            self.last_output_poc = None;
            self.previous_decoded_poc = None;
        }

        // A second field joins its first.
        let pair_of = match structure {
            Structure::Field(parity) => self.pair_of(header, parity),
            Structure::Frame => None,
        };

        if pair_of.is_none() && !idr {
            self.fill_gap(sps, header)?;
        }

        let (top_poc, bottom_poc) = self.order_counts(sps, header);

        let slot = match pair_of {
            Some(index) => self
                .entries
                .get(index)
                .copied()
                .flatten()
                .map(|e| e.slot)
                .ok_or(ParseError::OutOfRange)?,
            None => {
                // Room for the picture: pictures waiting to leave go first.
                while self.free_slot().is_none() || self.entry_count() > self.capacity.max(1) {
                    if !self.bump_one() {
                        break;
                    }
                }
                let slot = self.free_slot().ok_or(ParseError::TooMany)?;
                if let Some(held) = self.slot_in_dpb.get_mut(slot) {
                    *held = true;
                }
                slot
            }
        };

        Ok(Current {
            slot,
            structure,
            frame_num: header.frame_num,
            top_poc,
            bottom_poc,
            idr,
            reference: header.is_reference(),
            mmco5,
            pair_of,
        })
    }

    /// Insert the frames a gap in frame numbers implies (8.2.5.2).
    fn fill_gap(&mut self, sps: &Sps, header: &SliceHeader) -> Result<()> {
        let max = sps.max_frame_num();
        let prev = self.previous.ref_frame_num;
        if header.frame_num == prev || header.frame_num == (prev + 1) % max {
            return Ok(());
        }
        if !sps.gaps_in_frame_num_allowed {
            // A missing reference: what the stream will not repair.
            return Err(ParseError::OutOfRange);
        }
        let mut unused = (prev + 1) % max;
        let mut inserted = 0u32;
        while unused != header.frame_num {
            if inserted > 16 * 2 {
                return Err(ParseError::TooMany);
            }
            self.sliding_window();
            while self.free_slot().is_none() || self.entry_count() > self.capacity.max(1) {
                if !self.bump_one() {
                    break;
                }
            }
            let slot = self.free_slot().ok_or(ParseError::TooMany)?;
            if let Some(held) = self.slot_in_dpb.get_mut(slot) {
                *held = true;
            }
            let entry = Entry {
                slot,
                frame_num: unused,
                frame_num_wrap: 0,
                long_term_frame_idx: 0,
                fields: [
                    Field {
                        poc: 0,
                        reference: Reference::Short,
                        present: true,
                    },
                    Field {
                        poc: 0,
                        reference: Reference::Short,
                        present: true,
                    },
                ],
                non_existing: true,
                output_needed: false,
            };
            self.insert(entry)?;
            self.previous.ref_frame_num = unused;
            self.previous.frame_num = unused;
            unused = (unused + 1) % max;
            inserted += 1;
        }
        Ok(())
    }

    fn insert(&mut self, entry: Entry) -> Result<()> {
        let free = self
            .entries
            .iter_mut()
            .find(|e| e.is_none())
            .ok_or(ParseError::TooMany)?;
        *free = Some(entry);
        Ok(())
    }

    // ----- picture numbers and reference lists (8.2.4) -----

    /// Assign `FrameNumWrap` to every short-term entry for the current
    /// picture.
    fn number_frames(&mut self, sps: &Sps, current: &Current) {
        let max = i64::from(sps.max_frame_num());
        let now = i64::from(current.frame_num);
        for e in self.entries.iter_mut().flatten() {
            let n = i64::from(e.frame_num);
            let wrap = if n > now { n - max } else { n };
            e.frame_num_wrap = i32::try_from(wrap).unwrap_or(0);
        }
    }

    /// `PicNum` of a reference field or frame for the current picture.
    fn pic_num(&self, e: &Entry, parity: Option<Parity>, current: &Current) -> i32 {
        match (current.structure, parity) {
            (Structure::Frame, _) => e.frame_num_wrap,
            (Structure::Field(cur), Some(p)) => {
                if p == cur {
                    2 * e.frame_num_wrap + 1
                } else {
                    2 * e.frame_num_wrap
                }
            }
            (Structure::Field(_), None) => e.frame_num_wrap,
        }
    }

    fn long_term_pic_num(&self, e: &Entry, parity: Option<Parity>, current: &Current) -> u32 {
        match (current.structure, parity) {
            (Structure::Frame, _) => e.long_term_frame_idx,
            (Structure::Field(cur), Some(p)) => {
                if p == cur {
                    2 * e.long_term_frame_idx + 1
                } else {
                    2 * e.long_term_frame_idx
                }
            }
            (Structure::Field(_), None) => e.long_term_frame_idx,
        }
    }

    fn ref_pic(&self, e: &Entry, parity: Option<Parity>) -> RefPic {
        let long_term = match parity {
            Some(p) => e.field(p).reference == Reference::Long,
            None => e.frame_reference(Reference::Long),
        };
        RefPic {
            slot: e.slot,
            top_poc: e.fields[0].poc,
            bottom_poc: e.fields[1].poc,
            frame_idx: if long_term {
                e.long_term_frame_idx
            } else {
                e.frame_num
            },
            long_term,
            parity,
        }
    }

    /// The reference frames the device is told about for the current
    /// picture: every entry with any field marked.
    pub fn reference_frames(&self, current: &Current) -> List<Option<RefPic>, 16> {
        let mut out = List::default();
        for e in self.entries.iter().flatten() {
            if !e.is_reference() {
                continue;
            }
            // The first field of the current pair is a reference the device
            // needs named, with the current surface.
            let _ = current;
            if out.len < 16 {
                let _ = out.push(Some(self.ref_pic(e, None)));
            }
        }
        out
    }

    /// Build the initial and modified reference lists for one slice
    /// (8.2.4.2 and 8.2.4.3).
    pub fn reference_lists(
        &mut self,
        sps: &Sps,
        current: &Current,
        header: &SliceHeader,
    ) -> Result<(RefList, RefList)> {
        self.number_frames(sps, current);
        let mut l0 = List::default();
        let mut l1 = List::default();
        if header.slice_type.is_intra() {
            return Ok((l0, l1));
        }
        match current.structure {
            Structure::Frame => {
                if header.slice_type.is_b() {
                    self.init_b_frame(current, &mut l0, &mut l1);
                } else {
                    self.init_p_frame(current, &mut l0);
                }
            }
            Structure::Field(parity) => {
                if header.slice_type.is_b() {
                    self.init_b_field(current, parity, &mut l0, &mut l1);
                } else {
                    self.init_p_field(current, parity, &mut l0);
                }
            }
        }
        let active0 = usize::from(header.num_ref_idx_l0_active_minus1) + 1;
        let active1 = usize::from(header.num_ref_idx_l1_active_minus1) + 1;
        // Longer than active is truncated; shorter is filled by repeating,
        // which no conformant stream needs but a device may index.
        self.modify(sps, current, &header.modifications_l0, active0, &mut l0)?;
        if header.slice_type.is_b() {
            self.modify(sps, current, &header.modifications_l1, active1, &mut l1)?;
        }
        Ok((l0, l1))
    }

    fn sorted_short_frames(&self, current: &Current) -> List<Option<Entry>, MAX_FRAMES> {
        // Short-term frame references (both fields short-term) by PicNum
        // descending.
        let mut list: List<Option<Entry>, MAX_FRAMES> = List::default();
        for e in self.entries.iter().flatten() {
            if e.frame_reference(Reference::Short) {
                let _ = list.push(Some(*e));
            }
        }
        let n = list.len;
        let items = &mut list.items;
        // Insertion sort over at most eighteen entries.
        for i in 1..n {
            let mut j = i;
            while j > 0 {
                let (a, b) = (
                    items.get(j - 1).copied().flatten(),
                    items.get(j).copied().flatten(),
                );
                let swap = match (a, b) {
                    (Some(a), Some(b)) => {
                        self.pic_num(&a, None, current) < self.pic_num(&b, None, current)
                    }
                    _ => false,
                };
                if !swap {
                    break;
                }
                items.swap(j - 1, j);
                j -= 1;
            }
        }
        list
    }

    fn sorted_long_frames(&self) -> List<Option<Entry>, MAX_FRAMES> {
        let mut list: List<Option<Entry>, MAX_FRAMES> = List::default();
        for e in self.entries.iter().flatten() {
            if e.frame_reference(Reference::Long) {
                let _ = list.push(Some(*e));
            }
        }
        sort_by_key(&mut list, |e| i64::from(e.long_term_frame_idx));
        list
    }

    fn init_p_frame(&self, current: &Current, l0: &mut List<Option<RefPic>, MAX_REFS>) {
        for e in self
            .sorted_short_frames(current)
            .as_slice()
            .iter()
            .flatten()
        {
            let _ = l0.push(Some(self.ref_pic(e, None)));
        }
        for e in self.sorted_long_frames().as_slice().iter().flatten() {
            let _ = l0.push(Some(self.ref_pic(e, None)));
        }
    }

    fn init_b_frame(
        &self,
        current: &Current,
        l0: &mut List<Option<RefPic>, MAX_REFS>,
        l1: &mut List<Option<RefPic>, MAX_REFS>,
    ) {
        let poc = current.top_poc.min(current.bottom_poc);
        let mut short: List<Option<Entry>, MAX_FRAMES> = List::default();
        for e in self.entries.iter().flatten() {
            if e.frame_reference(Reference::Short) {
                let _ = short.push(Some(*e));
            }
        }
        // L0: below the current by POC descending, then above ascending.
        let mut below: List<Option<Entry>, MAX_FRAMES> = List::default();
        let mut above: List<Option<Entry>, MAX_FRAMES> = List::default();
        for e in short.as_slice().iter().flatten() {
            if e.poc() < poc {
                let _ = below.push(Some(*e));
            } else {
                let _ = above.push(Some(*e));
            }
        }
        sort_by_key(&mut below, |e| -i64::from(e.poc()));
        sort_by_key(&mut above, |e| i64::from(e.poc()));
        let long = self.sorted_long_frames();
        for e in below.as_slice().iter().flatten() {
            let _ = l0.push(Some(self.ref_pic(e, None)));
        }
        for e in above.as_slice().iter().flatten() {
            let _ = l0.push(Some(self.ref_pic(e, None)));
        }
        for e in long.as_slice().iter().flatten() {
            let _ = l0.push(Some(self.ref_pic(e, None)));
        }
        for e in above.as_slice().iter().flatten() {
            let _ = l1.push(Some(self.ref_pic(e, None)));
        }
        for e in below.as_slice().iter().flatten() {
            let _ = l1.push(Some(self.ref_pic(e, None)));
        }
        for e in long.as_slice().iter().flatten() {
            let _ = l1.push(Some(self.ref_pic(e, None)));
        }
        swap_if_equal(l0, l1);
    }

    /// The frames with a field marked `kind`, for the field-list orderings.
    fn field_frames(&self, kind: Reference) -> List<Option<Entry>, MAX_FRAMES> {
        let mut list: List<Option<Entry>, MAX_FRAMES> = List::default();
        for e in self.entries.iter().flatten() {
            if e.fields.iter().any(|f| f.present && f.reference == kind) {
                let _ = list.push(Some(*e));
            }
        }
        list
    }

    /// 8.2.4.2.5: fields from a frame list, alternating parity from the
    /// current field's.
    fn alternate(
        &self,
        frames: &List<Option<Entry>, MAX_FRAMES>,
        parity: Parity,
        kind: Reference,
        out: &mut List<Option<RefPic>, MAX_REFS>,
    ) {
        let mut next = [0usize; 2]; // per parity, the next frame index to try
        let mut want = parity;
        loop {
            let mut found = None;
            let start = next.get(want.index()).copied().unwrap_or(0);
            for i in start..frames.len {
                if let Some(e) = frames.items.get(i).copied().flatten() {
                    let f = e.field(want);
                    if f.present && f.reference == kind {
                        found = Some((i, e));
                        break;
                    }
                }
            }
            match found {
                Some((i, e)) => {
                    if let Some(n) = next.get_mut(want.index()) {
                        *n = i + 1;
                    }
                    if out.push(Some(self.ref_pic(&e, Some(want)))).is_err() {
                        return;
                    }
                    want = want.opposite();
                }
                None => {
                    // This parity is exhausted: the rest of the other parity
                    // follows in order, or nothing is left.
                    let other = want.opposite();
                    let start = next.get(other.index()).copied().unwrap_or(0);
                    for i in start..frames.len {
                        if let Some(e) = frames.items.get(i).copied().flatten() {
                            let f = e.field(other);
                            if f.present
                                && f.reference == kind
                                && out.push(Some(self.ref_pic(&e, Some(other)))).is_err()
                            {
                                return;
                            }
                        }
                    }
                    return;
                }
            }
        }
    }

    fn init_p_field(
        &self,
        current: &Current,
        parity: Parity,
        l0: &mut List<Option<RefPic>, MAX_REFS>,
    ) {
        let mut short = self.field_frames(Reference::Short);
        // By FrameNumWrap descending; the first field of the current pair
        // has the current frame number and sorts first.
        let _ = current;
        sort_by_key(&mut short, |e| -i64::from(e.frame_num_wrap));
        self.alternate(&short, parity, Reference::Short, l0);
        let mut long = self.field_frames(Reference::Long);
        sort_by_key(&mut long, |e| i64::from(e.long_term_frame_idx));
        self.alternate(&long, parity, Reference::Long, l0);
    }

    fn init_b_field(
        &self,
        current: &Current,
        parity: Parity,
        l0: &mut List<Option<RefPic>, MAX_REFS>,
        l1: &mut List<Option<RefPic>, MAX_REFS>,
    ) {
        let poc = match parity {
            Parity::Top => current.top_poc,
            Parity::Bottom => current.bottom_poc,
        };
        let short = self.field_frames(Reference::Short);
        let mut below: List<Option<Entry>, MAX_FRAMES> = List::default();
        let mut above: List<Option<Entry>, MAX_FRAMES> = List::default();
        for e in short.as_slice().iter().flatten() {
            if e.poc_marked(Reference::Short) <= poc {
                let _ = below.push(Some(*e));
            } else {
                let _ = above.push(Some(*e));
            }
        }
        sort_by_key(&mut below, |e| -i64::from(e.poc_marked(Reference::Short)));
        sort_by_key(&mut above, |e| i64::from(e.poc_marked(Reference::Short)));
        let mut frames0: List<Option<Entry>, MAX_FRAMES> = List::default();
        let mut frames1: List<Option<Entry>, MAX_FRAMES> = List::default();
        for e in below.as_slice().iter().flatten() {
            let _ = frames0.push(Some(*e));
        }
        for e in above.as_slice().iter().flatten() {
            let _ = frames0.push(Some(*e));
        }
        for e in above.as_slice().iter().flatten() {
            let _ = frames1.push(Some(*e));
        }
        for e in below.as_slice().iter().flatten() {
            let _ = frames1.push(Some(*e));
        }
        let mut long = self.field_frames(Reference::Long);
        sort_by_key(&mut long, |e| i64::from(e.long_term_frame_idx));
        self.alternate(&frames0, parity, Reference::Short, l0);
        self.alternate(&long, parity, Reference::Long, l0);
        self.alternate(&frames1, parity, Reference::Short, l1);
        self.alternate(&long, parity, Reference::Long, l1);
        swap_if_equal(l0, l1);
    }

    /// 8.2.4.3: apply a slice's modifications and cut the list to its
    /// active length.
    fn modify(
        &self,
        sps: &Sps,
        current: &Current,
        ops: &List<Option<Modification>, { MAX_REFS + 1 }>,
        active: usize,
        list: &mut List<Option<RefPic>, MAX_REFS>,
    ) -> Result<()> {
        let (max_pic_num, curr_pic_num) = match current.structure {
            Structure::Frame => (i64::from(sps.max_frame_num()), i64::from(current.frame_num)),
            Structure::Field(_) => (
                2 * i64::from(sps.max_frame_num()),
                2 * i64::from(current.frame_num) + 1,
            ),
        };
        // The list is treated as `active` long throughout; entries beyond
        // what was found are absent.
        let active = active.min(MAX_REFS);
        let mut pred = curr_pic_num;
        for (index, op) in ops.as_slice().iter().flatten().enumerate() {
            if index >= active {
                return Err(ParseError::TooMany);
            }
            let found = match *op {
                Modification::Subtract(diff) | Modification::Add(diff) => {
                    let abs = i64::from(diff) + 1;
                    let mut no_wrap = if matches!(op, Modification::Subtract(_)) {
                        pred - abs
                    } else {
                        pred + abs
                    };
                    if no_wrap < 0 {
                        no_wrap += max_pic_num;
                    } else if no_wrap >= max_pic_num {
                        no_wrap -= max_pic_num;
                    }
                    pred = no_wrap;
                    let pic_num = if no_wrap > curr_pic_num {
                        no_wrap - max_pic_num
                    } else {
                        no_wrap
                    };
                    self.find_short(current, pic_num)
                }
                Modification::LongTerm(n) => self.find_long(current, n),
            };
            let found = found.ok_or(ParseError::OutOfRange)?;
            // 8.2.4.3.1: the entries from `index` move up one, the picture
            // goes in at `index`, and its later occurrence is dropped. The
            // working list is one longer than the active count.
            let mut work: [Option<RefPic>; MAX_REFS + 1] = [None; MAX_REFS + 1];
            for (w, item) in work.iter_mut().zip(list.items.iter()) {
                *w = *item;
            }
            let mut c = active;
            while c > index {
                let below = work.get(c - 1).copied().flatten();
                if let Some(slot) = work.get_mut(c) {
                    *slot = below;
                }
                c -= 1;
            }
            if let Some(slot) = work.get_mut(index) {
                *slot = Some(found);
            }
            let mut n = index + 1;
            for c in (index + 1)..=active {
                let item = work.get(c).copied().flatten();
                if let Some(item) = item {
                    if !same_ref(&item, &found) {
                        if let Some(slot) = work.get_mut(n) {
                            *slot = Some(item);
                        }
                        n += 1;
                    }
                }
            }
            for slot in work.iter_mut().skip(n) {
                *slot = None;
            }
            for (item, w) in list.items.iter_mut().zip(work.iter()) {
                *item = *w;
            }
            list.len = n.min(MAX_REFS).max(index + 1);
        }
        // Cut to the active length; a short list stays short.
        if list.len > active {
            for slot in list.items.iter_mut().skip(active) {
                *slot = None;
            }
            list.len = active;
        }
        Ok(())
    }

    fn find_short(&self, current: &Current, pic_num: i64) -> Option<RefPic> {
        for e in self.entries.iter().flatten() {
            match current.structure {
                Structure::Frame => {
                    if e.frame_reference(Reference::Short)
                        && i64::from(self.pic_num(e, None, current)) == pic_num
                    {
                        return Some(self.ref_pic(e, None));
                    }
                }
                Structure::Field(_) => {
                    for p in [Parity::Top, Parity::Bottom] {
                        let f = e.field(p);
                        if f.present
                            && f.reference == Reference::Short
                            && i64::from(self.pic_num(e, Some(p), current)) == pic_num
                        {
                            return Some(self.ref_pic(e, Some(p)));
                        }
                    }
                }
            }
        }
        None
    }

    fn find_long(&self, current: &Current, long_term_pic_num: u32) -> Option<RefPic> {
        for e in self.entries.iter().flatten() {
            match current.structure {
                Structure::Frame => {
                    if e.frame_reference(Reference::Long)
                        && self.long_term_pic_num(e, None, current) == long_term_pic_num
                    {
                        return Some(self.ref_pic(e, None));
                    }
                }
                Structure::Field(_) => {
                    for p in [Parity::Top, Parity::Bottom] {
                        let f = e.field(p);
                        if f.present
                            && f.reference == Reference::Long
                            && self.long_term_pic_num(e, Some(p), current) == long_term_pic_num
                        {
                            return Some(self.ref_pic(e, Some(p)));
                        }
                    }
                }
            }
        }
        None
    }

    // ----- marking after decoding (8.2.5) -----

    /// Sliding window over the short-term references (8.2.5.3): room for
    /// the current picture, made before it is stored.
    fn sliding_window(&mut self) {
        while self.short_term_count() + self.long_term_count() >= self.max_num_ref_frames.max(1)
            && self.evict_oldest_short()
        {}
    }

    /// Unmark the short-term frame with the smallest `FrameNumWrap`.
    /// Returns whether there was one.
    fn evict_oldest_short(&mut self) -> bool {
        let mut oldest: Option<(usize, i32)> = None;
        for (i, e) in self.entries.iter().enumerate() {
            if let Some(e) = e {
                if e.has_short() && oldest.is_none_or(|(_, w)| e.frame_num_wrap < w) {
                    oldest = Some((i, e.frame_num_wrap));
                }
            }
        }
        let Some((index, _)) = oldest else {
            return false;
        };
        if let Some(Some(e)) = self.entries.get_mut(index) {
            for f in e.fields.iter_mut() {
                if f.reference == Reference::Short {
                    f.reference = Reference::Unused;
                }
            }
        }
        self.compact();
        true
    }

    fn unmark_long_term_idx(&mut self, idx: u32, except_slot: Option<Slot>) {
        for e in self.entries.iter_mut().flatten() {
            if e.has_long() && e.long_term_frame_idx == idx && Some(e.slot) != except_slot {
                for f in e.fields.iter_mut() {
                    if f.reference == Reference::Long {
                        f.reference = Reference::Unused;
                    }
                }
            }
        }
    }

    /// The current picture has been decoded: mark it and the buffer per its
    /// slice header, store it, and decide what leaves.
    pub fn finish(&mut self, sps: &Sps, current: &Current, header: &SliceHeader) -> Result<()> {
        // Number the frames against the current picture for the operations
        // that name picture numbers.
        self.number_frames(sps, current);
        let mut top_poc = current.top_poc;
        let mut bottom_poc = current.bottom_poc;
        let mut long_term = false;
        let mut long_term_frame_idx = 0u32;

        if current.reference {
            match header.marking {
                Marking::Idr {
                    long_term: idr_long,
                    ..
                } => {
                    self.max_long_term_frame_idx = if idr_long { Some(0) } else { None };
                    long_term = idr_long;
                    long_term_frame_idx = 0;
                }
                Marking::Adaptive(ops) => {
                    for op in ops.as_slice().iter().flatten() {
                        self.apply_mmco(
                            sps,
                            current,
                            *op,
                            &mut long_term,
                            &mut long_term_frame_idx,
                        )?;
                    }
                }
                Marking::Sliding | Marking::None => {
                    // The second field of a reference pair takes its first
                    // field's marking; a first field or a frame slides.
                    let second_of_reference_pair = current.pair_of.is_some_and(|index| {
                        self.entries
                            .get(index)
                            .copied()
                            .flatten()
                            .is_some_and(|e| e.has_short())
                    });
                    if !second_of_reference_pair {
                        self.sliding_window();
                    }
                }
            }
        }

        if current.mmco5 {
            // The counts restart from this picture.
            let temp = match current.structure {
                Structure::Frame => top_poc.min(bottom_poc),
                Structure::Field(Parity::Top) => top_poc,
                Structure::Field(Parity::Bottom) => bottom_poc,
            };
            top_poc = top_poc.wrapping_sub(temp);
            bottom_poc = bottom_poc.wrapping_sub(temp);
        }

        let reference = if !current.reference {
            Reference::Unused
        } else if long_term {
            Reference::Long
        } else {
            Reference::Short
        };
        let frame_num = if current.mmco5 { 0 } else { current.frame_num };

        match current.pair_of {
            Some(index) => {
                let entry = self
                    .entries
                    .get_mut(index)
                    .and_then(|e| e.as_mut())
                    .ok_or(ParseError::OutOfRange)?;
                if let Structure::Field(parity) = current.structure {
                    let poc = match parity {
                        Parity::Top => top_poc,
                        Parity::Bottom => bottom_poc,
                    };
                    let field = entry.field_mut(parity);
                    field.present = true;
                    field.poc = poc;
                    field.reference = reference;
                    if long_term {
                        entry.long_term_frame_idx = long_term_frame_idx;
                    }
                }
            }
            None => {
                let (top, bottom) = match current.structure {
                    Structure::Frame => (true, true),
                    Structure::Field(Parity::Top) => (true, false),
                    Structure::Field(Parity::Bottom) => (false, true),
                };
                let entry = Entry {
                    slot: current.slot,
                    frame_num,
                    frame_num_wrap: i32::try_from(frame_num).unwrap_or(0),
                    long_term_frame_idx,
                    fields: [
                        Field {
                            poc: top_poc,
                            reference: if top { reference } else { Reference::Unused },
                            present: top,
                        },
                        Field {
                            poc: bottom_poc,
                            reference: if bottom { reference } else { Reference::Unused },
                            present: bottom,
                        },
                    ],
                    non_existing: false,
                    output_needed: true,
                };
                self.insert(entry)?;
            }
        }

        // Too many references is a stream fault the standard forbids; slide
        // rather than fail, which is what every decoder here does.
        while self.short_term_count() + self.long_term_count() > self.max_num_ref_frames.max(1)
            && self.evict_oldest_short()
        {}

        // The values the next picture's order count derivation needs.
        if current.reference {
            self.previous.ref_frame_num = frame_num;
            self.previous.poc_msb = 0;
            self.previous.poc_lsb = 0;
            if sps.pic_order_cnt_type == 0 {
                let msb = i64::from(top_poc) - i64::from(header.pic_order_cnt_lsb);
                let (msb, lsb) = if current.mmco5 {
                    (0, 0)
                } else {
                    (i32::try_from(msb).unwrap_or(0), header.pic_order_cnt_lsb)
                };
                // A bottom field's msb derives from its own count.
                if let Structure::Field(Parity::Bottom) = current.structure {
                    let msb = i64::from(bottom_poc) - i64::from(header.pic_order_cnt_lsb);
                    self.previous.poc_msb = if current.mmco5 {
                        0
                    } else {
                        i32::try_from(msb).unwrap_or(0)
                    };
                } else {
                    self.previous.poc_msb = msb;
                }
                self.previous.poc_lsb = lsb;
            }
        }
        // The offset is derived against the previous picture's frame number,
        // so it is taken before that number is replaced; replaced first, a
        // wrap never advances it and every later count restarts low.
        self.previous.frame_num_offset = if current.mmco5 {
            0
        } else {
            self.frame_num_offset(sps, header)
        };
        self.previous.frame_num = frame_num;
        self.previous.mmco5 = current.mmco5;
        self.previous.mmco5_bottom = matches!(current.structure, Structure::Field(Parity::Bottom));
        self.previous.top_poc_after_mmco5 = top_poc;

        // Output: a reset flushes what came before it; otherwise pictures
        // leave when more than the reorder depth wait, and a picture that
        // proves the stream reorders deeper than it said raises the depth.
        if current.mmco5 {
            // Everything before the reset leaves first, in its own order.
            let keep = self.entries.iter().position(|e| {
                e.is_some_and(|e| e.slot == current.slot && current.pair_of.is_none())
            });
            let mut current_waiting = false;
            if let Some(index) = keep {
                if let Some(Some(e)) = self.entries.get_mut(index) {
                    current_waiting = e.output_needed;
                    e.output_needed = false;
                }
            }
            self.flush();
            if let Some(index) = keep {
                if let Some(Some(e)) = self.entries.get_mut(index) {
                    e.output_needed = current_waiting;
                }
            }
            self.last_output_poc = None;
        }
        self.bump_as_needed(current, header);
        Ok(())
    }

    fn bump_as_needed(&mut self, current: &Current, header: &SliceHeader) {
        // A stream that said nothing about reordering is held back only as
        // far as it proves it needs: a bi-predicted picture or a jump in the
        // order count means pictures are coming that belong before what was
        // just decoded, and a picture that arrives below the last output
        // raises the depth once more and leaves now rather than never.
        if !self.reorder_declared && !current.idr {
            let poc = current.top_poc.min(current.bottom_poc);
            if header.slice_type.is_b()
                || self
                    .previous_decoded_poc
                    .is_some_and(|prev| i64::from(poc) - i64::from(prev) > 2)
            {
                self.reorder = self.reorder.max(1);
            }
            if self.last_output_poc.is_some_and(|last| poc < last) {
                self.reorder = (self.reorder + 1).min(16);
            }
        }
        self.previous_decoded_poc = Some(current.top_poc.min(current.bottom_poc));
        while self.waiting_output() > self.reorder {
            if !self.bump_one() {
                break;
            }
        }
        // Waiting pictures beyond what the buffer holds leave regardless.
        while self.entry_count() > self.capacity.max(1) {
            if !self.bump_one() {
                break;
            }
        }
    }

    fn apply_mmco(
        &mut self,
        sps: &Sps,
        current: &Current,
        op: Mmco,
        long_term: &mut bool,
        long_term_frame_idx: &mut u32,
    ) -> Result<()> {
        let (max_pic_num, curr_pic_num) = match current.structure {
            Structure::Frame => (i64::from(sps.max_frame_num()), i64::from(current.frame_num)),
            Structure::Field(_) => (
                2 * i64::from(sps.max_frame_num()),
                2 * i64::from(current.frame_num) + 1,
            ),
        };
        let _ = max_pic_num;
        match op {
            Mmco::UnmarkShort(diff) => {
                let pic_num = curr_pic_num - (i64::from(diff) + 1);
                self.unmark_short_by_pic_num(current, pic_num);
            }
            Mmco::UnmarkLong(n) => {
                self.unmark_long_by_pic_num(current, n);
            }
            Mmco::ToLong(diff, idx) => {
                let pic_num = curr_pic_num - (i64::from(diff) + 1);
                // The index leaves any other frame that held it, except the
                // frame the named field belongs to.
                let target = self.find_short(current, pic_num).map(|r| r.slot);
                self.unmark_long_term_idx(idx, target);
                for e in self.entries.iter_mut().flatten() {
                    if Some(e.slot) != target {
                        continue;
                    }
                    match current.structure {
                        Structure::Frame => {
                            for f in e.fields.iter_mut() {
                                if f.reference == Reference::Short {
                                    f.reference = Reference::Long;
                                }
                            }
                        }
                        Structure::Field(_) => {
                            for p in [Parity::Top, Parity::Bottom] {
                                let wrap = e.frame_num_wrap;
                                let num = if Structure::Field(p) == current.structure {
                                    2 * i64::from(wrap) + 1
                                } else {
                                    2 * i64::from(wrap)
                                };
                                let f = e.field_mut(p);
                                if f.present && f.reference == Reference::Short && num == pic_num {
                                    f.reference = Reference::Long;
                                }
                            }
                        }
                    }
                    e.long_term_frame_idx = idx;
                }
            }
            Mmco::MaxLongTerm(plus1) => {
                self.max_long_term_frame_idx = plus1.checked_sub(1);
                let max = self.max_long_term_frame_idx;
                for e in self.entries.iter_mut().flatten() {
                    if e.has_long() && max.is_none_or(|m| e.long_term_frame_idx > m) {
                        for f in e.fields.iter_mut() {
                            if f.reference == Reference::Long {
                                f.reference = Reference::Unused;
                            }
                        }
                    }
                }
            }
            Mmco::UnmarkAll => {
                for e in self.entries.iter_mut().flatten() {
                    for f in e.fields.iter_mut() {
                        f.reference = Reference::Unused;
                    }
                }
                self.max_long_term_frame_idx = None;
            }
            Mmco::CurrentLong(idx) => {
                // The first field of the current pair keeps the index.
                let own = current
                    .pair_of
                    .and_then(|i| self.entries.get(i).copied().flatten())
                    .map(|e| e.slot);
                self.unmark_long_term_idx(idx, own);
                *long_term = true;
                *long_term_frame_idx = idx;
            }
        }
        self.compact();
        Ok(())
    }

    fn unmark_short_by_pic_num(&mut self, current: &Current, pic_num: i64) {
        for e in self.entries.iter_mut().flatten() {
            match current.structure {
                Structure::Frame => {
                    if e.frame_reference(Reference::Short) && i64::from(e.frame_num_wrap) == pic_num
                    {
                        for f in e.fields.iter_mut() {
                            f.reference = Reference::Unused;
                        }
                    }
                }
                Structure::Field(cur) => {
                    for p in [Parity::Top, Parity::Bottom] {
                        let num = if p == cur {
                            2 * i64::from(e.frame_num_wrap) + 1
                        } else {
                            2 * i64::from(e.frame_num_wrap)
                        };
                        let f = e.field_mut(p);
                        if f.present && f.reference == Reference::Short && num == pic_num {
                            f.reference = Reference::Unused;
                        }
                    }
                }
            }
        }
    }

    fn unmark_long_by_pic_num(&mut self, current: &Current, long_term_pic_num: u32) {
        for e in self.entries.iter_mut().flatten() {
            match current.structure {
                Structure::Frame => {
                    if e.frame_reference(Reference::Long)
                        && e.long_term_frame_idx == long_term_pic_num
                    {
                        for f in e.fields.iter_mut() {
                            f.reference = Reference::Unused;
                        }
                    }
                }
                Structure::Field(cur) => {
                    for p in [Parity::Top, Parity::Bottom] {
                        let num = if p == cur {
                            2 * e.long_term_frame_idx + 1
                        } else {
                            2 * e.long_term_frame_idx
                        };
                        let f = e.field_mut(p);
                        if f.present && f.reference == Reference::Long && num == long_term_pic_num {
                            f.reference = Reference::Unused;
                        }
                    }
                }
            }
        }
    }

    /// Flush at the end of the stream or a rebuild: every waiting picture
    /// leaves in order.
    pub fn drain(&mut self) {
        self.flush();
    }

    /// Whether `slot` belongs to the first field of the current pair, for
    /// a device that needs the current surface named among the references.
    pub fn entry_slot(&self, index: usize) -> Option<Slot> {
        self.entries.get(index).copied().flatten().map(|e| e.slot)
    }
}

/// Two list entries name the same picture.
fn same_ref(a: &RefPic, b: &RefPic) -> bool {
    a.slot == b.slot && a.parity == b.parity && a.long_term == b.long_term
}

/// 8.2.4.2.3 and .4: a second list identical to the first with more than
/// one entry swaps its first two.
fn swap_if_equal(l0: &List<Option<RefPic>, MAX_REFS>, l1: &mut List<Option<RefPic>, MAX_REFS>) {
    if l1.len > 1 && l1.len == l0.len {
        let same = l0
            .as_slice()
            .iter()
            .zip(l1.as_slice().iter())
            .all(|(a, b)| match (a, b) {
                (Some(a), Some(b)) => same_ref(a, b),
                _ => false,
            });
        if same {
            l1.items.swap(0, 1);
        }
    }
}

/// A stable insertion sort by key over a fixed list.
fn sort_by_key<const N: usize>(list: &mut List<Option<Entry>, N>, key: impl Fn(&Entry) -> i64) {
    let n = list.len.min(N);
    for i in 1..n {
        let mut j = i;
        while j > 0 {
            let swap = match (
                list.items.get(j - 1).copied().flatten(),
                list.items.get(j).copied().flatten(),
            ) {
                (Some(a), Some(b)) => key(&a) > key(&b),
                _ => false,
            };
            if !swap {
                break;
            }
            list.items.swap(j - 1, j);
            j -= 1;
        }
    }
}

impl SliceType {
    /// Whether a list of this length is what the slice type calls for.
    pub fn uses_l1(self) -> bool {
        self.is_b()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::h264::slice::{List, Marking, Mmco, SliceType};
    use crate::h264::sps::{ScalingLists, Sps, Vui};

    fn sps(max_num_ref_frames: u8, gaps: bool, frame_mbs_only: bool) -> Sps {
        Sps {
            profile_idc: 100,
            constraint_flags: 0,
            level_idc: 42,
            id: 0,
            chroma_format_idc: 1,
            separate_colour_plane: false,
            bit_depth_luma_minus8: 0,
            bit_depth_chroma_minus8: 0,
            qpprime_y_zero_transform_bypass: false,
            scaling_matrix_present: false,
            scaling: ScalingLists::FLAT,
            log2_max_frame_num_minus4: 0,
            pic_order_cnt_type: 0,
            log2_max_pic_order_cnt_lsb_minus4: 2,
            delta_pic_order_always_zero: false,
            offset_for_non_ref_pic: 0,
            offset_for_top_to_bottom_field: 0,
            num_ref_frames_in_pic_order_cnt_cycle: 0,
            offset_for_ref_frame: [0; 255],
            max_num_ref_frames,
            gaps_in_frame_num_allowed: gaps,
            pic_width_in_mbs_minus1: 7,
            pic_height_in_map_units_minus1: 7,
            frame_mbs_only,
            mb_adaptive_frame_field: false,
            direct_8x8_inference: true,
            crop: None,
            vui: Vui {
                max_num_reorder_frames: Some(0),
                max_dec_frame_buffering: Some(4),
                video_full_range: false,
            },
        }
    }

    fn header(frame_num: u32, poc_lsb: u32, idr: bool, marking: Marking) -> SliceHeader {
        SliceHeader {
            nal_unit_type: if idr { 5 } else { 1 },
            nal_ref_idc: 1,
            first_mb_in_slice: 0,
            slice_type: if idr { SliceType::I } else { SliceType::P },
            pps_id: 0,
            colour_plane_id: 0,
            frame_num,
            field_pic: false,
            bottom_field: false,
            idr_pic_id: 0,
            pic_order_cnt_lsb: poc_lsb,
            delta_pic_order_cnt_bottom: 0,
            delta_pic_order_cnt: [0; 2],
            redundant_pic_cnt: 0,
            direct_spatial_mv_pred: false,
            num_ref_idx_l0_active_minus1: 3,
            num_ref_idx_l1_active_minus1: 0,
            modifications_l0: List::default(),
            modifications_l1: List::default(),
            weights: None,
            marking,
            cabac_init_idc: 0,
            slice_qp_delta: 0,
            sp_for_switch: false,
            slice_qs_delta: 0,
            disable_deblocking_filter_idc: 0,
            slice_alpha_c0_offset_div2: 0,
            slice_beta_offset_div2: 0,
            header_bits: 0,
        }
    }

    fn idr() -> SliceHeader {
        header(
            0,
            0,
            true,
            Marking::Idr {
                no_output_of_prior_pics: false,
                long_term: false,
            },
        )
    }

    fn p(frame_num: u32) -> SliceHeader {
        header(frame_num, frame_num * 2, false, Marking::Sliding)
    }

    fn adaptive(frame_num: u32, ops: &[Mmco]) -> SliceHeader {
        let mut list = List::default();
        for op in ops {
            list.push(Some(*op)).unwrap();
        }
        header(frame_num, frame_num * 2, false, Marking::Adaptive(list))
    }

    /// Decode one picture end to end, taking what leaves.
    fn decode(dpb: &mut Dpb, sps: &Sps, h: &SliceHeader) -> Current {
        let current = dpb.begin(sps, h).unwrap();
        dpb.finish(sps, &current, h).unwrap();
        while let Some(o) = dpb.next_output() {
            dpb.taken(o.slot);
        }
        current
    }

    /// The frame numbers in list 0 for a picture, in order.
    fn list0(dpb: &mut Dpb, sps: &Sps, h: &SliceHeader) -> Vec<(u32, bool)> {
        let current = dpb.begin(sps, h).unwrap();
        let (l0, _) = dpb.reference_lists(sps, &current, h).unwrap();
        dpb.finish(sps, &current, h).unwrap();
        while let Some(o) = dpb.next_output() {
            dpb.taken(o.slot);
        }
        l0.as_slice()
            .iter()
            .flatten()
            .map(|r| (r.frame_idx, r.long_term))
            .collect()
    }

    #[test]
    fn the_sliding_window_evicts_the_oldest_frame() {
        let sps = sps(2, false, true);
        let mut dpb = Dpb::new();
        dpb.configure(&sps);
        decode(&mut dpb, &sps, &idr());
        decode(&mut dpb, &sps, &p(1));
        decode(&mut dpb, &sps, &p(2));
        // Two references: the newest first, and the refresh is gone.
        assert_eq!(list0(&mut dpb, &sps, &p(3)), vec![(2, false), (1, false)]);
    }

    #[test]
    fn memory_management_one_unmarks_by_picture_number() {
        let sps = sps(3, false, true);
        let mut dpb = Dpb::new();
        dpb.configure(&sps);
        decode(&mut dpb, &sps, &idr());
        decode(&mut dpb, &sps, &p(1));
        decode(&mut dpb, &sps, &p(2));
        // Picture number 3 - (1 + 1) = 1 leaves; 0 and 2 stay.
        decode(&mut dpb, &sps, &adaptive(3, &[Mmco::UnmarkShort(1)]));
        assert_eq!(
            list0(&mut dpb, &sps, &p(4)),
            vec![(3, false), (2, false), (0, false)]
        );
    }

    #[test]
    fn memory_management_three_makes_a_long_term_reference_listed_last() {
        let sps = sps(3, false, true);
        let mut dpb = Dpb::new();
        dpb.configure(&sps);
        decode(&mut dpb, &sps, &idr());
        decode(&mut dpb, &sps, &p(1));
        decode(&mut dpb, &sps, &p(2));
        // Picture number 3 - 1 = 2 becomes long-term index 0.
        decode(&mut dpb, &sps, &adaptive(3, &[Mmco::ToLong(0, 0)]));
        assert_eq!(
            list0(&mut dpb, &sps, &p(4)),
            vec![(3, false), (1, false), (0, true)]
        );
        // Unmarking the long-term picture by its number removes it.
        decode(&mut dpb, &sps, &adaptive(5, &[Mmco::UnmarkLong(0)]));
        assert_eq!(
            list0(&mut dpb, &sps, &p(6)),
            vec![(5, false), (4, false), (3, false)]
        );
    }

    #[test]
    fn memory_management_five_empties_the_buffer_and_restarts_the_count() {
        let sps = sps(3, false, true);
        let mut dpb = Dpb::new();
        dpb.configure(&sps);
        decode(&mut dpb, &sps, &idr());
        decode(&mut dpb, &sps, &p(1));
        let reset = decode(&mut dpb, &sps, &adaptive(2, &[Mmco::UnmarkAll]));
        assert!(reset.mmco5);
        // Only the reset picture remains, numbered zero, so the next frame
        // number is one and no gap is seen.
        assert_eq!(list0(&mut dpb, &sps, &p(1)), vec![(0, false)]);
    }

    #[test]
    fn a_gap_in_frame_numbers_is_filled_when_allowed_and_refused_when_not() {
        let sps = sps(4, true, true);
        let mut dpb = Dpb::new();
        dpb.configure(&sps);
        decode(&mut dpb, &sps, &idr());
        decode(&mut dpb, &sps, &p(1));
        // 2 and 3 are missing: filled by frames that are never output and
        // listed like any other reference.
        assert_eq!(
            list0(&mut dpb, &sps, &p(4)),
            vec![(3, false), (2, false), (1, false), (0, false)]
        );
        let strict = self::sps(4, false, true);
        let mut dpb = Dpb::new();
        dpb.configure(&strict);
        decode(&mut dpb, &strict, &idr());
        assert!(dpb.begin(&strict, &p(3)).is_err());
    }

    #[test]
    fn a_field_pair_shares_a_slot_and_a_field_list_alternates_parity() {
        let sps = sps(2, false, false);
        let mut dpb = Dpb::new();
        dpb.configure(&sps);
        let mut top = idr();
        top.field_pic = true;
        let first = dpb.begin(&sps, &top).unwrap();
        dpb.finish(&sps, &first, &top).unwrap();
        // The second field is not itself a refresh.
        let mut bottom = header(0, 0, false, Marking::Sliding);
        bottom.slice_type = SliceType::I;
        bottom.field_pic = true;
        bottom.bottom_field = true;
        let second = dpb.begin(&sps, &bottom).unwrap();
        assert_eq!(
            second.slot, first.slot,
            "the second field took its own slot"
        );
        assert!(second.pair_of.is_some());
        dpb.finish(&sps, &second, &bottom).unwrap();
        while let Some(o) = dpb.next_output() {
            dpb.taken(o.slot);
        }
        // A predicted top field lists the top field first, then the bottom.
        let mut next = p(1);
        next.field_pic = true;
        next.num_ref_idx_l0_active_minus1 = 1;
        let current = dpb.begin(&sps, &next).unwrap();
        let (l0, _) = dpb.reference_lists(&sps, &current, &next).unwrap();
        let parities: Vec<_> = l0.as_slice().iter().flatten().map(|r| r.parity).collect();
        assert_eq!(parities, vec![Some(Parity::Top), Some(Parity::Bottom)]);
        assert!(l0.as_slice().iter().flatten().all(|r| r.slot == first.slot));
    }
}
