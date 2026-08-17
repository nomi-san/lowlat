//! Parameter sets, written out.
//!
//! One backend cannot state the colour description any other way: its sequence
//! parameters carry an aspect-ratio flag and nothing else, so the description
//! required by [05 §3.1](../../../docs/05-host.md) has to be written here and
//! handed over as a packed header.
//!
//! The layout is the coding standard's, not a vendor's, so everything here is
//! checkable without hardware.

use crate::bitstream::{BitWriter, escape};

/// Four-byte start code. The three-byte form is legal and this is what a
/// parameter set is conventionally prefixed with.
const START_CODE: [u8; 4] = [0, 0, 0, 1];

/// High profile: 8-bit 4:2:0 with the transform and prediction tools a modern
/// decoder expects. Baseline would cost quality for compatibility nothing
/// still needs.
const PROFILE_HIGH: u32 = 100;

/// Unspecified, which is what a frame that never touched an analogue format is.
const VIDEO_FORMAT_UNSPECIFIED: u32 = 5;
/// BT.709, for primaries, transfer and matrix alike.
const BT709: u32 = 1;

/// Everything both parameter sets need.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Params {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// Encoded as ten times the level number, so 4.2 is 42.
    pub level_idc: u32,
    /// Bits of frame number before it wraps, less four.
    pub log2_max_frame_num_minus4: u32,
    /// Bits of picture order count before it wraps, less four.
    pub log2_max_poc_lsb_minus4: u32,
    /// One is enough without B-frames: each picture references the last.
    pub max_num_ref_frames: u32,
}

impl Params {
    /// Macroblocks across, rounding up.
    fn width_in_mbs(&self) -> u32 {
        self.width.div_ceil(16)
    }

    /// Macroblock rows, rounding up.
    fn height_in_mbs(&self) -> u32 {
        self.height.div_ceil(16)
    }

    /// Cropping needed because the coded size is a whole number of
    /// macroblocks and the real size may not be.
    ///
    /// **The units are chroma samples, not pixels**, which for 4:2:0 means
    /// each unit is two rows or two columns. Writing pixels here produces a
    /// picture cropped to half of what was intended, on every decoder, and it
    /// looks like a capture bug.
    fn crop(&self) -> (u32, u32) {
        let right = (self.width_in_mbs() * 16 - self.width) / 2;
        let bottom = (self.height_in_mbs() * 16 - self.height) / 2;
        (right, bottom)
    }
}

/// Write the sequence parameter set, start code and escaping included.
///
/// Returns the bytes written into `out`, or `None` if it does not fit.
pub fn sequence_parameter_set(params: &Params, out: &mut [u8]) -> Option<usize> {
    let mut raw = [0u8; 128];
    let mut w = BitWriter::new(&mut raw);

    w.bits(PROFILE_HIGH, 8);
    // The six constraint flags and the two reserved bits, all clear: we assert
    // no profile restriction beyond the one the profile itself implies.
    w.bits(0, 8);
    w.bits(params.level_idc, 8);
    w.ue(0); // this sequence parameter set's identifier

    // Present only for the high profiles, which is why the profile choice
    // above changes the shape of what follows.
    w.ue(1); // 4:2:0
    w.ue(0); // luma depth, less eight
    w.ue(0); // chroma depth, less eight
    w.bit(false); // no lossless transform bypass
    w.bit(false); // no scaling matrices

    w.ue(params.log2_max_frame_num_minus4);
    w.ue(0); // picture order count type zero: counts sent explicitly
    w.ue(params.log2_max_poc_lsb_minus4);
    w.ue(params.max_num_ref_frames);
    w.bit(false); // frame numbers never skip

    w.ue(params.width_in_mbs() - 1);
    w.ue(params.height_in_mbs() - 1);
    w.bit(true); // frames only, never fields
    w.bit(true); // direct 8x8 inference, required when frames only

    let (right, bottom) = params.crop();
    let cropping = right > 0 || bottom > 0;
    w.bit(cropping);
    if cropping {
        w.ue(0);
        w.ue(right);
        w.ue(0);
        w.ue(bottom);
    }

    // The whole reason this function exists.
    w.bit(true); // usability information follows
    w.bit(false); // no aspect ratio
    w.bit(false); // no overscan information
    w.bit(true); // video signal type present
    w.bits(VIDEO_FORMAT_UNSPECIFIED, 3);
    w.bit(false); // limited range
    w.bit(true); // colour description present
    w.bits(BT709, 8); // primaries
    w.bits(BT709, 8); // transfer
    w.bits(BT709, 8); // matrix
    w.bit(false); // no chroma sample location

    w.bit(true); // timing information present
    // Two ticks per frame is the convention when frames are not fields, so
    // the time scale is twice the frame rate.
    w.bits(1, 32);
    w.bits(params.fps.saturating_mul(2), 32);
    w.bit(true); // frame rate is fixed

    w.bit(false); // no hypothetical reference decoder, either kind
    w.bit(false);
    w.bit(false); // no picture structure signalling
    w.bit(false); // no bitstream restrictions

    if !w.trailing_bits() {
        return None;
    }
    let payload = w.finish();

    // Reference-marked, type seven.
    emit(0x67, payload, out)
}

/// Write the picture parameter set, start code and escaping included.
pub fn picture_parameter_set(out: &mut [u8]) -> Option<usize> {
    let mut raw = [0u8; 64];
    let mut w = BitWriter::new(&mut raw);

    w.ue(0); // this picture parameter set's identifier
    w.ue(0); // the sequence parameter set it belongs to
    w.bit(true); // arithmetic coding, which costs nothing and codes better
    w.bit(false); // picture order count not sent per field
    w.ue(0); // one slice group
    w.ue(0); // one reference in the first list
    w.ue(0); // and one in the second, unused without B-frames
    w.bit(false); // no weighted prediction
    w.bits(0, 2); // nor bi-directional weighting
    w.se(0); // initial quantiser offset from twenty-six
    w.se(0); // and the same for the second quantiser
    w.se(0); // no chroma quantiser offset
    w.bit(true); // slices may carry deblocking control
    w.bit(false); // prediction may cross slice boundaries
    w.bit(false); // no redundant pictures

    if !w.trailing_bits() {
        return None;
    }
    let payload = w.finish();

    // Reference-marked, type eight.
    emit(0x68, payload, out)
}

/// Start code, unit header, then the escaped payload.
fn emit(header: u8, payload: &[u8], out: &mut [u8]) -> Option<usize> {
    let mut written = 0usize;
    for byte in START_CODE {
        *out.get_mut(written)? = byte;
        written += 1;
    }
    *out.get_mut(written)? = header;
    written += 1;
    // The unit header counts toward the zero run, but it is never zero for
    // either set, so escaping may start fresh from the payload.
    written += escape(payload, out.get_mut(written..)?)?;
    Some(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> Params {
        Params {
            width: 1920,
            height: 1080,
            fps: 60,
            level_idc: 42,
            log2_max_frame_num_minus4: 0,
            log2_max_poc_lsb_minus4: 0,
            max_num_ref_frames: 1,
        }
    }

    #[test]
    fn a_sequence_set_starts_the_way_a_decoder_looks_for_one() {
        let mut out = [0u8; 256];
        let n = sequence_parameter_set(&params(), &mut out).expect("room");
        assert_eq!(&out[..4], &START_CODE);
        assert_eq!(out[4], 0x67, "not a reference-marked sequence set");
        assert_eq!(out[5], 100, "not the high profile");
        assert_eq!(out[7], 42, "level not carried through");
        assert!(n > 10);
    }

    #[test]
    fn a_picture_set_starts_the_way_a_decoder_looks_for_one() {
        let mut out = [0u8; 128];
        let n = picture_parameter_set(&mut out).expect("room");
        assert_eq!(&out[..4], &START_CODE);
        assert_eq!(out[4], 0x68, "not a reference-marked picture set");
        assert!(n > 5);
    }

    /// 1080 is not a whole number of macroblocks: 68 rows cover 1088, so eight
    /// rows are cropped. **In chroma samples that is four, not eight**, and
    /// writing pixels here crops twice as much as intended.
    #[test]
    fn cropping_is_in_chroma_samples_not_pixels() {
        let p = params();
        assert_eq!(p.height_in_mbs(), 68);
        assert_eq!(p.crop(), (0, 4), "cropping was written in pixels");

        // A size that is already whole needs no cropping at all.
        let square = Params {
            height: 1088,
            ..params()
        };
        assert_eq!(square.crop(), (0, 0));
    }

    /// Write the pair somewhere an independent parser can read them.
    ///
    /// **The tests above check structure, not meaning.** They agree with the
    /// writer because both came from the same reading of the standard, so
    /// nothing here yet proves a decoder reads the colour description the way
    /// it is intended. A parameter set alone is not enough for that: a decoder
    /// reports nothing about a stream with no picture in it.
    ///
    /// The check arrives with the first frame this backend encodes, when the
    /// sets travel with a picture and an external decoder can be asked what it
    /// sees. This dumper is the tool for that, kept rather than deleted so the
    /// step is set up rather than remembered.
    #[test]
    #[ignore = "writes a file for an external parser"]
    fn dump_for_an_external_parser() {
        let mut out = [0u8; 256];
        let n = sequence_parameter_set(&params(), &mut out).expect("room");
        let mut pps = [0u8; 128];
        let m = picture_parameter_set(&mut pps).expect("room");

        let path = std::env::var("LOWLAT_DUMP").unwrap_or_else(|_| "/tmp/params.h264".into());
        let mut bytes = out[..n].to_vec();
        bytes.extend_from_slice(&pps[..m]);
        std::fs::write(&path, &bytes).expect("write");
        println!("wrote {path}, {} bytes", bytes.len());
    }

    #[test]
    fn a_short_buffer_refuses_rather_than_truncating() {
        let mut out = [0u8; 4];
        assert!(sequence_parameter_set(&params(), &mut out).is_none());
        assert!(picture_parameter_set(&mut out).is_none());
    }
}

/// A slice header, and the exact number of bits it occupies.
///
/// **Not byte aligned, and that is the point.** Slice data follows the header
/// immediately in the same unit, so there are no trailing bits and the length
/// has to be carried in bits rather than bytes. A caller that rounds up and
/// reports bytes tells the driver to start the slice data on the wrong
/// boundary.
#[derive(Debug, Clone, Copy)]
pub struct SliceHeader {
    pub bytes_written: usize,
    pub bit_length: usize,
}

/// Write the slice header for an instantaneous refresh.
///
/// The only slice type produced so far, because predicted slices need the
/// reference bookkeeping that is not written yet.
pub fn idr_slice_header(params: &Params, out: &mut [u8]) -> Option<SliceHeader> {
    let mut raw = [0u8; 64];
    let mut w = BitWriter::new(&mut raw);

    w.ue(0); // first macroblock in this slice
    // Seven rather than two: the values above four assert that **every** slice
    // in the picture has this type, which is what lets a decoder know the
    // picture is entirely intra without reading the rest.
    w.ue(7);
    w.ue(0); // the picture parameter set to use

    // Fixed width, and the width is what the sequence parameters declared. Get
    // this wrong and every field after it is read at the wrong offset.
    w.bits(0, params.log2_max_frame_num_minus4 + 4);
    w.ue(0); // this refresh's identifier
    w.bits(0, params.log2_max_poc_lsb_minus4 + 4);

    // Reference picture marking, present because this slice is a reference.
    w.bit(false); // earlier pictures may still be output
    w.bit(false); // not a long-term reference

    w.se(0); // no quantiser offset from the picture's initial value

    // Deblocking control, present because the picture parameter set says so.
    w.ue(0); // filter on, and it may cross slice boundaries
    w.se(0); // no alpha offset
    w.se(0); // no beta offset

    // **No trailing bits.** The slice data continues in this same unit, so
    // padding here would insert bits into the middle of it.
    let bit_length = w.bit_len();
    let payload = w.finish();

    // Reference-marked, type five: a slice of an instantaneous refresh.
    let bytes_written = emit(0x65, payload, out)?;
    Some(SliceHeader {
        bytes_written,
        // The start code and unit header precede the payload and count toward
        // what the driver is told.
        bit_length: bit_length + (START_CODE.len() + 1) * 8,
    })
}
