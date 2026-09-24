//! The HEVC decoded picture buffer: picture order, the reference picture
//! set that decides what stays, the lists a slice decodes against, and the
//! order pictures leave.

use crate::ParseError;
use crate::h264::slice::List;
use crate::hevc::slice::{MAX_REFS, SliceHeader};
use crate::hevc::sps::Sps;

type Result<T> = core::result::Result<T, ParseError>;

/// Pictures the buffer can hold: the standard's sixteen, the one being
/// decoded, and one spare for a picture read back after its place was
/// taken.
pub const MAX_PICTURES: usize = 18;

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
pub struct Entry {
    pub slot: Slot,
    pub poc: i32,
    pub reference: Reference,
    pub output_needed: bool,
    /// Pictures decoded since this one, while it waited.
    pub latency: u32,
}

/// Which of the current picture's sets a reference belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Set {
    StCurrBefore,
    StCurrAfter,
    LtCurr,
    /// Kept for later pictures, not used by this one.
    Foll,
}

/// A reference the device is told about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefPic {
    pub slot: Slot,
    pub poc: i32,
    pub long_term: bool,
    pub set: Set,
}

/// A slice's list: indices into the picture's references.
pub type RefList = List<Option<u8>, MAX_REFS>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Output {
    pub slot: Slot,
    pub poc: i32,
}

/// What the buffer knows about the picture being decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Current {
    pub slot: Slot,
    pub poc: i32,
    pub output: bool,
    pub irap: bool,
    pub no_rasl_output: bool,
}

#[derive(Debug)]
pub struct Dpb {
    entries: [Option<Entry>; MAX_PICTURES],
    max_dec_pic_buffering: usize,
    reorder: u32,
    max_latency_pictures: Option<u32>,
    /// The previous picture with temporal id zero that counts for order:
    /// its order count's halves.
    prev_tid0_poc: i32,
    /// The first picture of the stream, or the first after an end of
    /// sequence, starts a random-access period whose leading pictures are
    /// dropped.
    first_picture: bool,
    /// Whether the last random-access picture started such a period.
    no_rasl_output: bool,
    slot_in_dpb: [bool; MAX_PICTURES],
    slot_pending_output: [bool; MAX_PICTURES],
    output: List<Option<Output>, MAX_PICTURES>,
    /// The five sets for the current picture, as references.
    references: List<Option<RefPic>, MAX_PICTURES>,
}

impl Default for Dpb {
    fn default() -> Self {
        Self::new()
    }
}

impl Dpb {
    pub fn new() -> Self {
        Self {
            entries: [None; MAX_PICTURES],
            max_dec_pic_buffering: 1,
            reorder: 0,
            max_latency_pictures: None,
            prev_tid0_poc: 0,
            first_picture: true,
            no_rasl_output: true,
            slot_in_dpb: [false; MAX_PICTURES],
            slot_pending_output: [false; MAX_PICTURES],
            output: List::default(),
            references: List::default(),
        }
    }

    pub fn configure(&mut self, sps: &Sps) {
        self.max_dec_pic_buffering = usize::from(sps.max_dec_pic_buffering_minus1) + 1;
        self.reorder = u32::from(sps.max_num_reorder_pics);
        self.max_latency_pictures = if sps.max_latency_increase_plus1 != 0 {
            Some(
                u32::from(sps.max_num_reorder_pics)
                    .saturating_add(sps.max_latency_increase_plus1)
                    .saturating_sub(1),
            )
        } else {
            None
        };
    }

    pub fn reorder(&self) -> u32 {
        self.reorder
    }

    pub fn slots(&self) -> usize {
        MAX_PICTURES
    }

    /// The references of the current picture, once [`Dpb::begin`] has
    /// derived its sets.
    pub fn references(&self) -> &List<Option<RefPic>, MAX_PICTURES> {
        &self.references
    }

    pub fn clear(&mut self) {
        for index in 0..MAX_PICTURES {
            if let Some(e) = self.entries.get_mut(index).and_then(|e| e.take()) {
                self.release_slot(e.slot);
            }
        }
        self.first_picture = true;
        self.prev_tid0_poc = 0;
        self.output = List::default();
        self.slot_pending_output = [false; MAX_PICTURES];
        self.references = List::default();
    }

    /// An end of sequence: the next picture starts afresh.
    pub fn end_of_sequence(&mut self) {
        self.first_picture = true;
    }

    fn release_slot(&mut self, slot: Slot) {
        if let Some(held) = self.slot_in_dpb.get_mut(slot) {
            *held = false;
        }
    }

    pub fn taken(&mut self, slot: Slot) {
        if let Some(pending) = self.slot_pending_output.get_mut(slot) {
            *pending = false;
        }
    }

    pub fn next_output(&mut self) -> Option<Output> {
        if self.output.len == 0 {
            return None;
        }
        let first = self.output.items.first().copied().flatten();
        for i in 1..self.output.len {
            let next = self.output.items.get(i).copied().flatten();
            if let Some(slot) = self.output.items.get_mut(i - 1) {
                *slot = next;
            }
        }
        self.output.len -= 1;
        first
    }

    pub fn has_output(&self) -> bool {
        self.output.len > 0
    }

    fn free_slot(&self) -> Option<Slot> {
        (0..MAX_PICTURES).find(|&s| {
            !self.slot_in_dpb.get(s).copied().unwrap_or(true)
                && !self.slot_pending_output.get(s).copied().unwrap_or(true)
        })
    }

    fn fullness(&self) -> usize {
        self.entries.iter().filter(|e| e.is_some()).count()
    }

    fn waiting_output(&self) -> u32 {
        let count = self
            .entries
            .iter()
            .flatten()
            .filter(|e| e.output_needed)
            .count();
        u32::try_from(count).unwrap_or(u32::MAX)
    }

    fn latency_exceeded(&self) -> bool {
        match self.max_latency_pictures {
            Some(max) => self
                .entries
                .iter()
                .flatten()
                .any(|e| e.output_needed && e.latency >= max),
            None => false,
        }
    }

    fn compact(&mut self) {
        for i in 0..MAX_PICTURES {
            let done = match self.entries.get(i).copied().flatten() {
                Some(e) => e.reference == Reference::Unused && !e.output_needed,
                None => false,
            };
            if done {
                if let Some(e) = self.entries.get_mut(i).and_then(|e| e.take()) {
                    self.release_slot(e.slot);
                }
            }
        }
    }

    fn bump_one(&mut self) -> bool {
        let mut best: Option<(usize, i32)> = None;
        for (i, e) in self.entries.iter().enumerate() {
            if let Some(e) = e {
                if e.output_needed && best.is_none_or(|(_, poc)| e.poc < poc) {
                    best = Some((i, e.poc));
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
            if let Some(o) = self.output.items.get_mut(self.output.len) {
                *o = Some(Output { slot, poc });
            }
            self.output.len = (self.output.len + 1).min(MAX_PICTURES);
        }
        self.compact();
        true
    }

    fn flush(&mut self) {
        while self.bump_one() {}
    }

    fn discard_waiting(&mut self) {
        for e in self.entries.iter_mut().flatten() {
            e.output_needed = false;
        }
        self.compact();
    }

    /// Begin the current picture from its first slice: derive its order
    /// count and its reference sets, mark the buffer, let pictures leave
    /// that must, and claim a slot. `None` when the picture is a leading
    /// picture that cannot be decoded and is dropped.
    pub fn begin(&mut self, sps: &Sps, header: &SliceHeader) -> Result<Option<Current>> {
        let irap = header.is_irap();
        if irap {
            self.no_rasl_output = header.is_idr() || header.is_bla() || self.first_picture;
        }
        if header.is_rasl() && self.no_rasl_output {
            // A leading picture of a random-access period this stream
            // joined at: its references were never decoded.
            return Ok(None);
        }

        // 8.3.1: the order count.
        let max_lsb = i64::from(sps.max_pic_order_cnt_lsb());
        let poc = if irap && self.no_rasl_output {
            i32::try_from(header.pic_order_cnt_lsb).unwrap_or(0)
        } else {
            let prev_lsb = i64::from(self.prev_tid0_poc).rem_euclid(max_lsb);
            let prev_msb = i64::from(self.prev_tid0_poc) - prev_lsb;
            let lsb = i64::from(header.pic_order_cnt_lsb);
            let msb = if lsb < prev_lsb && prev_lsb - lsb >= max_lsb / 2 {
                prev_msb + max_lsb
            } else if lsb > prev_lsb && lsb - prev_lsb > max_lsb / 2 {
                prev_msb - max_lsb
            } else {
                prev_msb
            };
            i32::try_from(msb + lsb).unwrap_or(0)
        };

        // 8.3.2: the reference picture set. An IDR carries none.
        self.references = List::default();
        if header.is_idr() {
            for e in self.entries.iter_mut().flatten() {
                e.reference = Reference::Unused;
            }
        } else {
            self.derive_rps(sps, header, poc)?;
        }

        // C.5.2.2: what leaves before this picture is decoded.
        if irap && self.no_rasl_output && !self.first_picture {
            let no_output = header.no_output_of_prior_pics || header.is_cra();
            if no_output {
                self.discard_waiting();
            } else {
                self.flush();
            }
            // Every picture is out of the buffer now: none is referenced.
            for e in self.entries.iter_mut().flatten() {
                e.reference = Reference::Unused;
                e.output_needed = false;
            }
            self.compact();
        } else {
            self.compact();
            while self.waiting_output() > self.reorder
                || self.latency_exceeded()
                || self.fullness() >= self.max_dec_pic_buffering.max(1)
            {
                if !self.bump_one() {
                    break;
                }
            }
        }

        while self.free_slot().is_none() {
            if !self.bump_one() {
                break;
            }
        }
        let slot = self.free_slot().ok_or(ParseError::TooMany)?;
        if let Some(held) = self.slot_in_dpb.get_mut(slot) {
            *held = true;
        }
        let output = header.pic_output && !(header.is_rasl() && self.no_rasl_output);
        Ok(Some(Current {
            slot,
            poc,
            output,
            irap,
            no_rasl_output: self.no_rasl_output,
        }))
    }

    fn derive_rps(&mut self, sps: &Sps, header: &SliceHeader, poc: i32) -> Result<()> {
        let max_lsb = sps.max_pic_order_cnt_lsb();
        let rps = &header.st_rps;
        // What the current picture's sets name, and whether each is used
        // by it.
        let mut wanted: List<Option<(i32, bool, Set)>, 48> = List::default();
        for i in 0..usize::from(rps.num_negative) {
            let d = rps.delta_s0.get(i).copied().unwrap_or(0);
            let used = rps.used_s0.get(i).copied().unwrap_or(false);
            let set = if used { Set::StCurrBefore } else { Set::Foll };
            wanted.push(Some((poc.wrapping_add(d), false, set)))?;
        }
        for i in 0..usize::from(rps.num_positive) {
            let d = rps.delta_s1.get(i).copied().unwrap_or(0);
            let used = rps.used_s1.get(i).copied().unwrap_or(false);
            let set = if used { Set::StCurrAfter } else { Set::Foll };
            wanted.push(Some((poc.wrapping_add(d), false, set)))?;
        }
        // Long-term entries match on the full count when the cycle is
        // present and on the low bits otherwise; the flag says which.
        let mut long_wanted: List<Option<(i32, bool, Set)>, 32> = List::default();
        for lt in header.long_terms.as_slice() {
            let set = if lt.used_by_curr_pic {
                Set::LtCurr
            } else {
                Set::Foll
            };
            let value = if lt.delta_poc_msb_present {
                let msb = poc - poc.rem_euclid(i32::try_from(max_lsb).unwrap_or(1));
                let cycle = i32::try_from(lt.delta_poc_msb_cycle).unwrap_or(0);
                let target = i32::try_from(lt.poc_lsb).unwrap_or(0) + msb
                    - cycle.wrapping_mul(i32::try_from(max_lsb).unwrap_or(1));
                (target, true, set)
            } else {
                (i32::try_from(lt.poc_lsb).unwrap_or(0), false, set)
            };
            long_wanted.push(Some(value))?;
        }

        // Mark: every entry named stays with its kind; the rest are freed.
        let mut keep = [Reference::Unused; MAX_PICTURES];
        let mut found_short = [false; 48];
        let mut found_long = [false; 32];
        for (i, e) in self.entries.iter().enumerate() {
            let Some(e) = e else {
                continue;
            };
            if e.reference == Reference::Unused {
                continue;
            }
            // Long-term matches first: a picture already long-term stays so
            // when named; a short-term picture named as long-term becomes it.
            let mut kind = Reference::Unused;
            for (n, w) in long_wanted.as_slice().iter().enumerate() {
                let Some((target, full, _)) = w else {
                    continue;
                };
                let matches = if *full {
                    e.poc == *target
                } else {
                    e.poc.rem_euclid(i32::try_from(max_lsb).unwrap_or(1)) == *target
                };
                if matches && !found_long.get(n).copied().unwrap_or(true) {
                    kind = Reference::Long;
                    if let Some(f) = found_long.get_mut(n) {
                        *f = true;
                    }
                    break;
                }
            }
            if kind == Reference::Unused && e.reference == Reference::Short {
                for (n, w) in wanted.as_slice().iter().enumerate() {
                    let Some((target, _, _)) = w else {
                        continue;
                    };
                    if e.poc == *target && !found_short.get(n).copied().unwrap_or(true) {
                        kind = Reference::Short;
                        if let Some(f) = found_short.get_mut(n) {
                            *f = true;
                        }
                        break;
                    }
                }
            }
            if let Some(k) = keep.get_mut(i) {
                *k = kind;
            }
        }
        for (e, k) in self.entries.iter_mut().zip(keep.iter()) {
            if let Some(e) = e {
                e.reference = *k;
            }
        }

        // A picture the current one uses and the buffer does not hold is a
        // broken chain the stream will not repair.
        for (n, w) in wanted.as_slice().iter().enumerate() {
            if let Some((_, _, set)) = w {
                if *set != Set::Foll && !found_short.get(n).copied().unwrap_or(false) {
                    return Err(ParseError::OutOfRange);
                }
            }
        }
        for (n, w) in long_wanted.as_slice().iter().enumerate() {
            if let Some((_, _, set)) = w {
                if *set != Set::Foll && !found_long.get(n).copied().unwrap_or(false) {
                    return Err(ParseError::OutOfRange);
                }
            }
        }

        // The references, in the order the sets name them: before, after,
        // long-term, then the rest.
        let push = |dpb: &mut Self, target: i32, full: bool, long: bool, set: Set| {
            for e in dpb.entries.iter().flatten() {
                let matches = if long {
                    e.reference == Reference::Long
                        && if full {
                            e.poc == target
                        } else {
                            e.poc.rem_euclid(i32::try_from(max_lsb).unwrap_or(1)) == target
                        }
                } else {
                    e.reference == Reference::Short && e.poc == target
                };
                if matches
                    && !dpb
                        .references
                        .as_slice()
                        .iter()
                        .flatten()
                        .any(|r| r.slot == e.slot)
                {
                    let _ = dpb.references.push(Some(RefPic {
                        slot: e.slot,
                        poc: e.poc,
                        long_term: long,
                        set,
                    }));
                    return;
                }
            }
        };
        for w in wanted.as_slice().iter().flatten() {
            if w.2 == Set::StCurrBefore {
                push(self, w.0, true, false, Set::StCurrBefore);
            }
        }
        for w in wanted.as_slice().iter().flatten() {
            if w.2 == Set::StCurrAfter {
                push(self, w.0, true, false, Set::StCurrAfter);
            }
        }
        for w in long_wanted.as_slice().iter().flatten() {
            if w.2 == Set::LtCurr {
                push(self, w.0, w.1, true, Set::LtCurr);
            }
        }
        for w in wanted.as_slice().iter().flatten() {
            if w.2 == Set::Foll {
                push(self, w.0, true, false, Set::Foll);
            }
        }
        for w in long_wanted.as_slice().iter().flatten() {
            if w.2 == Set::Foll {
                push(self, w.0, w.1, true, Set::Foll);
            }
        }
        Ok(())
    }

    /// 8.3.4: the lists for one slice, as indices into the references.
    pub fn reference_lists(&self, header: &SliceHeader) -> Result<(RefList, RefList)> {
        let mut l0 = RefList::default();
        let mut l1 = RefList::default();
        if header.slice_type.is_intra() {
            return Ok((l0, l1));
        }
        let refs = self.references.as_slice();
        let index_of = |set: Set| -> List<u8, MAX_PICTURES> {
            let mut out = List::default();
            for (i, r) in refs.iter().enumerate() {
                if r.is_some_and(|r| r.set == set) {
                    let _ = out.push(u8::try_from(i).unwrap_or(0));
                }
            }
            out
        };
        let before = index_of(Set::StCurrBefore);
        let after = index_of(Set::StCurrAfter);
        let long = index_of(Set::LtCurr);
        let total = before.len + after.len + long.len;
        if total == 0 {
            return Err(ParseError::OutOfRange);
        }
        let build = |first: &List<u8, MAX_PICTURES>,
                     second: &List<u8, MAX_PICTURES>,
                     active: usize,
                     modification: Option<[u8; MAX_REFS]>|
         -> Result<RefList> {
            let count = active.max(total);
            let mut temp: List<u8, 48> = List::default();
            while temp.len < count {
                for i in first
                    .as_slice()
                    .iter()
                    .chain(second.as_slice())
                    .chain(long.as_slice())
                {
                    if temp.len >= count {
                        break;
                    }
                    temp.push(*i)?;
                }
            }
            let mut out = RefList::default();
            for i in 0..active.min(MAX_REFS) {
                let index = match modification {
                    Some(entries) => usize::from(entries.get(i).copied().unwrap_or(0)),
                    None => i,
                };
                let value = temp
                    .as_slice()
                    .get(index)
                    .copied()
                    .ok_or(ParseError::OutOfRange)?;
                out.push(Some(value))?;
            }
            Ok(out)
        };
        l0 = build(
            &before,
            &after,
            usize::from(header.num_ref_idx_l0_active_minus1) + 1,
            header.list_modification_l0,
        )?;
        if header.slice_type.is_b() {
            l1 = build(
                &after,
                &before,
                usize::from(header.num_ref_idx_l1_active_minus1) + 1,
                header.list_modification_l1,
            )?;
        }
        Ok((l0, l1))
    }

    /// The current picture has been decoded: store it and decide what
    /// leaves (C.5.2.3).
    pub fn finish(&mut self, current: &Current, header: &SliceHeader) -> Result<()> {
        for e in self.entries.iter_mut().flatten() {
            if e.output_needed {
                e.latency = e.latency.saturating_add(1);
            }
        }
        let entry = Entry {
            slot: current.slot,
            poc: current.poc,
            reference: Reference::Short,
            output_needed: current.output,
            latency: 0,
        };
        let free = self
            .entries
            .iter_mut()
            .find(|e| e.is_none())
            .ok_or(ParseError::TooMany)?;
        *free = Some(entry);

        if header.temporal_id == 0
            && !header.is_rasl()
            && !header.is_radl()
            && !header.is_sub_layer_non_reference()
        {
            self.prev_tid0_poc = current.poc;
        }
        self.first_picture = false;

        while self.waiting_output() > self.reorder || self.latency_exceeded() {
            if !self.bump_one() {
                break;
            }
        }
        Ok(())
    }

    pub fn drain(&mut self) {
        self.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hevc::slice::{LongTerm, SliceType, unit};
    use crate::hevc::sps::{MAX_LT_SPS, MAX_ST_RPS, ScalingLists, Sps, StRps};

    fn sps(reorder: u8) -> Sps {
        Sps {
            id: 0,
            vps_id: 0,
            max_sub_layers_minus1: 0,
            profile_idc: 1,
            profile_compatibility: 0,
            chroma_format_idc: 1,
            separate_colour_plane: false,
            width: 128,
            height: 128,
            conformance_window: None,
            bit_depth_luma_minus8: 0,
            bit_depth_chroma_minus8: 0,
            log2_max_pic_order_cnt_lsb_minus4: 0,
            max_dec_pic_buffering_minus1: 4,
            max_num_reorder_pics: reorder,
            max_latency_increase_plus1: 0,
            log2_min_luma_coding_block_size_minus3: 0,
            log2_diff_max_min_luma_coding_block_size: 3,
            log2_min_luma_transform_block_size_minus2: 0,
            log2_diff_max_min_luma_transform_block_size: 3,
            max_transform_hierarchy_depth_inter: 0,
            max_transform_hierarchy_depth_intra: 0,
            scaling_list_enabled: false,
            scaling: ScalingLists::DEFAULT,
            amp_enabled: false,
            sample_adaptive_offset_enabled: false,
            pcm_enabled: false,
            pcm_sample_bit_depth_luma_minus1: 0,
            pcm_sample_bit_depth_chroma_minus1: 0,
            log2_min_pcm_luma_coding_block_size_minus3: 0,
            log2_diff_max_min_pcm_luma_coding_block_size: 0,
            pcm_loop_filter_disabled: false,
            num_short_term_ref_pic_sets: 0,
            st_rps: [StRps::default(); MAX_ST_RPS],
            long_term_ref_pics_present: true,
            num_long_term_ref_pics_sps: 0,
            lt_ref_pic_poc_lsb_sps: [0; MAX_LT_SPS],
            used_by_curr_pic_lt_sps: [false; MAX_LT_SPS],
            temporal_mvp_enabled: false,
            strong_intra_smoothing_enabled: false,
            video_full_range: false,
            range: Default::default(),
        }
    }

    /// A picture with the negative deltas given, all used, and the
    /// positive ones kept for later pictures.
    fn header_with(
        nal_unit_type: u8,
        poc_lsb: u32,
        negatives: &[i32],
        positives: &[i32],
    ) -> SliceHeader {
        let mut rps = StRps::default();
        for (i, d) in negatives.iter().enumerate() {
            rps.delta_s0[i] = *d;
            rps.used_s0[i] = true;
        }
        rps.num_negative = negatives.len() as u8;
        for (i, d) in positives.iter().enumerate() {
            rps.delta_s1[i] = *d;
            rps.used_s1[i] = false;
        }
        rps.num_positive = positives.len() as u8;
        SliceHeader {
            nal_unit_type,
            temporal_id: 0,
            first_slice_segment_in_pic: true,
            no_output_of_prior_pics: false,
            pps_id: 0,
            dependent_slice_segment: false,
            slice_segment_address: 0,
            slice_type: if negatives.is_empty() {
                SliceType::I
            } else {
                SliceType::P
            },
            pic_output: true,
            colour_plane_id: 0,
            pic_order_cnt_lsb: poc_lsb,
            st_rps: rps,
            st_rps_from_sps: false,
            st_rps_bits: 0,
            long_terms: List::default(),
            temporal_mvp_enabled: false,
            sao_luma: false,
            sao_chroma: false,
            num_ref_idx_l0_active_minus1: negatives.len().saturating_sub(1) as u8,
            num_ref_idx_l1_active_minus1: 0,
            list_modification_l0: None,
            list_modification_l1: None,
            mvd_l1_zero: false,
            cabac_init: false,
            collocated_from_l0: true,
            collocated_ref_idx: 0,
            weights: None,
            five_minus_max_num_merge_cand: 0,
            slice_qp_delta: 0,
            slice_cb_qp_offset: 0,
            slice_cr_qp_offset: 0,
            cu_chroma_qp_offset_enabled: false,
            deblocking_filter_disabled: false,
            beta_offset_div2: 0,
            tc_offset_div2: 0,
            loop_filter_across_slices_enabled: false,
            num_entry_point_offsets: 0,
            header_bytes: 0,
            header_escapes: 0,
        }
    }

    fn header(nal_unit_type: u8, poc_lsb: u32, negatives: &[i32]) -> SliceHeader {
        header_with(nal_unit_type, poc_lsb, negatives, &[])
    }

    fn decode(dpb: &mut Dpb, sps: &Sps, h: &SliceHeader) -> Option<Current> {
        let current = dpb.begin(sps, h).unwrap()?;
        dpb.finish(&current, h).unwrap();
        Some(current)
    }

    fn drain_outputs(dpb: &mut Dpb) -> Vec<i32> {
        let mut out = Vec::new();
        while let Some(o) = dpb.next_output() {
            out.push(o.poc);
            dpb.taken(o.slot);
        }
        out
    }

    #[test]
    fn a_reference_the_buffer_does_not_hold_is_refused() {
        let sps = sps(0);
        let mut dpb = Dpb::new();
        dpb.configure(&sps);
        decode(&mut dpb, &sps, &header(unit::IDR_N_LP, 0, &[]));
        drain_outputs(&mut dpb);
        // Picture 1 names picture -1, which was never decoded.
        assert!(dpb.begin(&sps, &header(unit::TRAIL_R, 1, &[-2])).is_err());
    }

    #[test]
    fn the_set_keeps_what_it_names_and_drops_the_rest() {
        let sps = sps(0);
        let mut dpb = Dpb::new();
        dpb.configure(&sps);
        decode(&mut dpb, &sps, &header(unit::IDR_N_LP, 0, &[]));
        decode(&mut dpb, &sps, &header(unit::TRAIL_R, 1, &[-1]));
        decode(&mut dpb, &sps, &header(unit::TRAIL_R, 2, &[-1, -2]));
        drain_outputs(&mut dpb);
        // Picture 3 keeps only 2: 0 and 1 leave the buffer.
        let current = dpb
            .begin(&sps, &header(unit::TRAIL_R, 3, &[-1]))
            .unwrap()
            .unwrap();
        let refs: Vec<_> = dpb
            .references()
            .as_slice()
            .iter()
            .flatten()
            .map(|r| (r.poc, r.set))
            .collect();
        assert_eq!(refs, vec![(2, Set::StCurrBefore)]);
        let (l0, _) = dpb
            .reference_lists(&header(unit::TRAIL_R, 3, &[-1]))
            .unwrap();
        assert_eq!(l0.as_slice(), &[Some(0)]);
        // Only the reference remains; the current picture is stored when
        // it has been decoded.
        assert_eq!(dpb.fullness(), 1);
        let _ = current;
    }

    #[test]
    fn a_long_term_reference_matches_on_the_low_bits_and_lists_last() {
        let sps = sps(0);
        let mut dpb = Dpb::new();
        dpb.configure(&sps);
        decode(&mut dpb, &sps, &header(unit::IDR_N_LP, 0, &[]));
        decode(&mut dpb, &sps, &header(unit::TRAIL_R, 1, &[-1]));
        drain_outputs(&mut dpb);
        // Picture 2 keeps 1 as short-term and 0 as long-term by its low bits.
        let mut h = header(unit::TRAIL_R, 2, &[-1]);
        h.long_terms
            .push(LongTerm {
                poc_lsb: 0,
                used_by_curr_pic: true,
                delta_poc_msb_present: false,
                delta_poc_msb_cycle: 0,
            })
            .unwrap();
        h.num_ref_idx_l0_active_minus1 = 1;
        dpb.begin(&sps, &h).unwrap().unwrap();
        let refs: Vec<_> = dpb
            .references()
            .as_slice()
            .iter()
            .flatten()
            .map(|r| (r.poc, r.long_term, r.set))
            .collect();
        assert_eq!(
            refs,
            vec![(1, false, Set::StCurrBefore), (0, true, Set::LtCurr)]
        );
        let (l0, _) = dpb.reference_lists(&h).unwrap();
        assert_eq!(l0.as_slice(), &[Some(0), Some(1)]);
    }

    #[test]
    fn leading_pictures_of_a_random_access_point_that_starts_the_stream_are_dropped() {
        let sps = sps(0);
        let mut dpb = Dpb::new();
        dpb.configure(&sps);
        // The stream is joined at a clean random access picture.
        assert!(decode(&mut dpb, &sps, &header(unit::CRA, 8, &[])).is_some());
        // Its leading pictures reference what came before: dropped.
        assert!(
            dpb.begin(&sps, &header(unit::RASL_N, 6, &[-2]))
                .unwrap()
                .is_none()
        );
        // What follows decodes.
        assert!(decode(&mut dpb, &sps, &header(unit::TRAIL_R, 9, &[-1])).is_some());
        assert_eq!(drain_outputs(&mut dpb), vec![8, 9]);
    }

    #[test]
    fn pictures_are_held_to_the_declared_reorder_depth() {
        let sps = sps(2);
        let mut dpb = Dpb::new();
        dpb.configure(&sps);
        decode(&mut dpb, &sps, &header(unit::IDR_N_LP, 0, &[]));
        assert!(drain_outputs(&mut dpb).is_empty());
        decode(&mut dpb, &sps, &header(unit::TRAIL_R, 4, &[-4]));
        assert!(drain_outputs(&mut dpb).is_empty());
        // The third picture sits between the two in order and keeps the
        // later one for the pictures after it; the first now leaves.
        decode(&mut dpb, &sps, &header_with(unit::TRAIL_N, 2, &[-2], &[2]));
        assert_eq!(drain_outputs(&mut dpb), vec![0]);
        decode(&mut dpb, &sps, &header(unit::TRAIL_R, 8, &[-4]));
        assert_eq!(drain_outputs(&mut dpb), vec![2]);
        dpb.drain();
        assert_eq!(drain_outputs(&mut dpb), vec![4, 8]);
    }

    #[test]
    fn a_refresh_lets_the_waiting_pictures_out_first() {
        let sps = sps(2);
        let mut dpb = Dpb::new();
        dpb.configure(&sps);
        decode(&mut dpb, &sps, &header(unit::IDR_N_LP, 0, &[]));
        decode(&mut dpb, &sps, &header(unit::TRAIL_R, 1, &[-1]));
        assert!(drain_outputs(&mut dpb).is_empty());
        decode(&mut dpb, &sps, &header(unit::IDR_W_RADL, 0, &[]));
        assert_eq!(drain_outputs(&mut dpb), vec![0, 1]);
    }
}
