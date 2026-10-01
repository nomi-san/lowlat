//! The system's own decoder, in software: the media framework's H.264
//! decoder, and its HEVC extension where that is installed and licensed,
//! eight-bit 4:2:0 and, for HEVC, ten-bit 4:2:0, handed out as planes.
//!
//! **Every sequence parameter set is read here before the decoder sees its
//! unit**, and a stream the decoder would mishandle is refused: H.264 of any
//! chroma or depth but eight-bit 4:2:0 -- on which the decoder hangs inside
//! its output call -- or cropped from the left or the top, which it writes to
//! the wrong place; HEVC of any chroma but 4:2:0 or depth but eight and ten
//! bits. The depth picks the output's layout, so a ten-bit stream never meets
//! an eight-bit output, which the decoder fills without an error. The
//! parameter set also gives the size the decoder is made for and the visible
//! picture and range handed out; nothing else is read: the decoder orders its
//! own pictures, and a host never reorders.
//!
//! **One picture out of each unit's own call**: the decoder is asked to hand
//! a picture out as soon as it can be decoded, and every unit ends with an
//! access unit delimiter, which tells the HEVC decoder's parser the unit is
//! whole -- without it every picture comes out a unit late, and a picture of
//! several slices not at all. **One input and one output buffer, made once**:
//! the input written only once the decoder has said it needs more, since it
//! holds the last unit until then; the output's length cleared before each
//! call, without which a buffer used again is refused, and made again only
//! when the stream's size changes -- a buffer made per picture costs its page
//! faults, half a millisecond at 1440p. The decoder's rows are copied out by
//! its own stride, the chroma after its coded height: a picture whose width
//! is not a multiple of sixteen is stored wider, and one 1080 rows tall
//! 1088.

use core::fmt;

use lowlat_core::video::{Codec, VideoHeader};
use lowlat_drivers::mf::{self as framework, Coding, Layout, Mf, Out, Sample, Transform};

use crate::split::{copy_rows, micros};
use crate::{Caps, Decoder, Fault, Fed, Format, Picture, Planes, h264, hevc, nal};

/// The delimiter ending every unit, each codec's: an access unit
/// delimiter of any picture type.
const DELIMITER_H264: [u8; 6] = [0, 0, 0, 1, 0x09, 0xf0];
const DELIMITER_HEVC: [u8; 7] = [0, 0, 0, 1, 0x46, 0x01, 0x50];
/// The input buffer's granule: grown to the next one past a unit that does
/// not fit, which only a keyframe larger than any before does.
const INPUT_GRANULE: usize = 1 << 20;
/// Stream changes taken in one call before the decoder is given up: one is
/// every change, two a change met while another is renegotiated.
const MOST_CHANGES: u32 = 4;
/// Pictures let go before a unit is written, at most: those a stream's
/// reorder depth holds, which a host's stream has none of.
const MOST_DRAINED: u32 = 32;
/// The size a decoder is made at to ask whether it is made at all.
const PROBE_SIZE: (u32, u32) = (1280, 720);

/// Why a decoder could not be made or a unit decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// The framework refused a call while decoding.
    Framework(framework::Error),
    /// The decoder could not be made or configured for the stream.
    Build(framework::Error),
    /// A stream this decoder must not be given.
    NoProfile,
    /// A unit that could not be read, or a decoder that answered out of
    /// turn.
    Stream,
    /// A picture larger than the caller's planes, or a unit larger than the
    /// largest that comes.
    TooLarge,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Framework(e) | Self::Build(e) => write!(f, "{e}"),
            Self::NoProfile => f.write_str("system decoder takes no stream of this shape"),
            Self::Stream => f.write_str("system decoder lost the stream"),
            Self::TooLarge => f.write_str("picture or unit larger than the decoder holds"),
        }
    }
}

impl std::error::Error for Error {}

impl From<framework::Error> for Error {
    fn from(e: framework::Error) -> Self {
        Self::Framework(e)
    }
}

type Result<T> = core::result::Result<T, Error>;

/// What the stream's last sequence parameter set says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stream {
    ten_bit: bool,
    coded: (u32, u32),
    visible: (u32, u32),
    full_range: bool,
}

impl Stream {
    const fn format(self) -> Format {
        Format::of(self.ten_bit, false)
    }
}

/// A decoder made for a stream and what it holds.
struct Built {
    transform: Transform,
    output: Sample,
    layout: Layout,
    stream: Stream,
    /// A picture in the output buffer, not yet taken.
    held: bool,
    /// The decoder has said it needs more input since the last unit.
    drained: bool,
}

/// The system's decoder: made at the first parameter set, for the codec the
/// stream was built for.
pub struct Backend {
    mf: &'static Mf,
    codec: Codec,
    built: Option<Built>,
    input: Option<Sample>,
    /// The largest unit that comes.
    unit_bytes: usize,
    order: i32,
    /// The last unit's decode, from its input to its picture out, and its
    /// copy out, in microseconds, for the log.
    pub decode_us: u32,
    pub readback_us: u32,
}

impl fmt::Debug for Backend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Backend")
            .field("codec", &self.codec)
            .field("stream", &self.built.as_ref().map(|b| b.stream))
            .finish()
    }
}

/// What the system's decoders decode here, each made and dropped to ask:
/// H.264 eight-bit 4:2:0, and HEVC at eight and ten bits where its
/// extension is made, licence and all; with each one's module version or why
/// it was not made.
pub fn caps(mf: &'static Mf) -> (Caps, Made, Made) {
    let make = |coding| -> Made {
        let decoder = mf.decoder(coding)?;
        decoder.set_input(PROBE_SIZE.0, PROBE_SIZE.1)?;
        Ok(decoder.version())
    };
    let (h264, hevc) = (make(Coding::H264), make(Coding::Hevc));
    let caps = Caps {
        h264: h264.is_ok(),
        hevc: hevc.is_ok(),
        hevc_10: hevc.is_ok(),
        hevc_444: false,
        hevc_444_10: false,
    };
    (caps, h264, hevc)
}

/// A decoder made, with its module's version where it says, or why not.
pub type Made = core::result::Result<Option<[u16; 4]>, framework::Error>;

/// The worker threads a decoder is given: two for HEVC, which decodes faster
/// on two than on one a processor; half the processors for H.264, which
/// keeps the default's speed at less of the processor's time.
fn workers(coding: Coding) -> u32 {
    let threads = std::thread::available_parallelism().map_or(1, usize::from);
    let count = match coding {
        Coding::Hevc => threads.min(2),
        Coding::H264 => (threads / 2).max(1),
    };
    u32::try_from(count).unwrap_or(1)
}

impl Backend {
    /// The decoder over `mf`; `unit_bytes` the largest unit that comes.
    pub fn new(mf: &'static Mf, unit_bytes: usize) -> Self {
        Self {
            mf,
            codec: Codec::H264,
            built: None,
            input: None,
            unit_bytes,
            order: 0,
            decode_us: 0,
            readback_us: 0,
        }
    }

    /// The size and layout the pictures [`Decoder::take`] hands out have,
    /// once a parameter set has said.
    pub fn output(&self) -> Option<(u32, u32, Format)> {
        let stream = self.built.as_ref()?.stream;
        Some((stream.visible.0, stream.visible.1, stream.format()))
    }

    /// Let every picture held for the stream's order out, as at the end of
    /// a stream; a test's need, since a live stream never ends this way.
    pub fn drain(&mut self) {
        if let Some(built) = &mut self.built
            && built.transform.drain().is_ok()
        {
            built.drained = false;
        }
    }

    fn coding(&self) -> Coding {
        match self.codec {
            Codec::H264 => Coding::H264,
            Codec::H265 => Coding::Hevc,
        }
    }

    /// The stream the unit's last sequence parameter set says, if it holds
    /// one; one this decoder must not be given is refused.
    fn read_sets(&self, unit: &[u8]) -> Result<Option<Stream>> {
        let mut found = None;
        for nal in nal::Units::new(unit) {
            let Some(&head) = nal.bytes.first() else {
                continue;
            };
            found = match self.codec {
                Codec::H264 if head & 0x1f == 7 => {
                    let payload = nal.bytes.get(1..).unwrap_or(&[]);
                    let sps = h264::sps::parse(payload).map_err(|_| Error::Stream)?;
                    let cropped_ahead = sps
                        .crop
                        .is_some_and(|[left, _, top, _]| left != 0 || top != 0);
                    if sps.chroma_format_idc != 1
                        || sps.bit_depth_luma_minus8 != 0
                        || sps.bit_depth_chroma_minus8 != 0
                        || cropped_ahead
                    {
                        return Err(Error::NoProfile);
                    }
                    Some(Stream {
                        ten_bit: false,
                        coded: (sps.coded_width(), sps.coded_height()),
                        visible: sps.visible(),
                        full_range: sps.vui.video_full_range,
                    })
                }
                Codec::H265 if (head >> 1) & 0x3f == 33 => {
                    let payload = nal.bytes.get(2..).unwrap_or(&[]);
                    let sps = hevc::sps::parse(payload).map_err(|_| Error::Stream)?;
                    let depth = sps.bit_depth_luma_minus8;
                    if sps.chroma_format_idc != 1
                        || !matches!(depth, 0 | 2)
                        || sps.bit_depth_chroma_minus8 != depth
                    {
                        return Err(Error::NoProfile);
                    }
                    Some(Stream {
                        ten_bit: depth == 2,
                        coded: (sps.width, sps.height),
                        visible: sps.visible(),
                        full_range: sps.video_full_range,
                    })
                }
                _ => found,
            };
        }
        Ok(found)
    }

    /// The decoder for `stream`: made, set to hand each picture out at
    /// once on its share of the processor, its input the stream's codec and
    /// size, its output the stream's depth -- or, for a ten-bit stream the
    /// decoder has not seen yet, eight bits until it has -- and streaming.
    fn make(&self, stream: Stream) -> core::result::Result<Built, framework::Error> {
        let coding = self.coding();
        let transform = self.mf.decoder(coding)?;
        transform.low_latency()?;
        transform.workers(workers(coding))?;
        transform.set_input(stream.coded.0, stream.coded.1)?;
        let layout = transform
            .set_output(stream.ten_bit)
            .or_else(|_| transform.set_output(false))?;
        let output = self.mf.sample(layout.size)?;
        // Only once an output is set: one decoder faults on it before.
        transform.begin()?;
        Ok(Built {
            transform,
            output,
            layout,
            stream,
            held: false,
            drained: true,
        })
    }

    fn decode(&mut self, unit: &[u8]) -> Result<Fed> {
        if unit.len() > self.unit_bytes {
            return Err(Error::TooLarge);
        }
        if let Some(stream) = self.read_sets(unit)? {
            match &mut self.built {
                // A change of depth is a decoder of another layout; a change
                // of size the decoder follows itself.
                Some(built) if built.stream.ten_bit != stream.ten_bit => {
                    return Ok(Fed::FormatChanged);
                }
                Some(built) => built.stream = stream,
                None => self.built = Some(self.make(stream).map_err(Error::Build)?),
            }
        }
        let delimiter: &[u8] = match self.codec {
            Codec::H264 => &DELIMITER_H264,
            Codec::H265 => &DELIMITER_HEVC,
        };
        let needed = unit.len() + delimiter.len();
        if self
            .input
            .as_ref()
            .is_none_or(|i| (i.capacity() as usize) < needed)
        {
            let capacity = u32::try_from(needed.next_multiple_of(INPUT_GRANULE))
                .map_err(|_| Error::TooLarge)?;
            self.input = Some(self.mf.sample(capacity)?);
        }
        let (Some(built), Some(input)) = (&mut self.built, &self.input) else {
            // No parameter set yet: nothing to decode with.
            return Ok(Fed::NeedMoreData);
        };
        // The decoder holds the last unit until it has said it needs more:
        // whatever it still has is let go first, never written over.
        let mut let_go = 0;
        while !built.drained {
            built.held = false;
            pull(built, self.mf)?;
            let_go += 1;
            if let_go > MOST_DRAINED {
                return Err(Error::Stream);
            }
        }
        input.write(&[unit, delimiter])?;
        let started = lowlat_common::clock::Time::now();
        if !built.transform.input(input)? {
            return Err(Error::Stream);
        }
        built.drained = false;
        pull(built, self.mf)?;
        self.decode_us = micros(lowlat_common::clock::elapsed_ms(started));
        Ok(if built.held {
            Fed::Picture
        } else {
            Fed::NeedMoreData
        })
    }

    fn copy(&mut self, out: &mut Planes<'_>) -> Result<Picture> {
        let built = self.built.as_mut().ok_or(Error::Stream)?;
        let (layout, stream) = (built.layout, built.stream);
        let format = stream.format();
        let (x, y, width, height) = layout.visible;
        // The decoder's own picture is the parameter set's, at its depth, or
        // the copy would walk a buffer laid out for another -- or hand out
        // eight bits of a ten-bit stream, which the decoder writes without a
        // word.
        if (width, height) != stream.visible || layout.ten_bit != stream.ten_bit {
            return Err(Error::NoProfile);
        }
        let started = lowlat_common::clock::Time::now();
        let sample = format.sample();
        let (stride, x, y) = (layout.stride as usize, x as usize, y as usize);
        let (width, height) = (width as usize, height as usize);
        let luma = y * stride + x * sample;
        let chroma = stride * layout.height as usize + y / 2 * stride + x * sample;
        built
            .output
            .read(|bytes| {
                copy_rows(
                    bytes,
                    luma,
                    stride,
                    out.y,
                    out.y_pitch,
                    width * sample,
                    height,
                )?;
                copy_rows(
                    bytes,
                    chroma,
                    stride,
                    out.uv,
                    out.uv_pitch,
                    format.chroma_row_bytes(width),
                    format.chroma_rows(height),
                )
            })?
            .map_err(|_| Error::TooLarge)?;
        built.held = false;
        self.readback_us = micros(lowlat_common::clock::elapsed_ms(started));
        self.order = self.order.wrapping_add(1);
        Ok(Picture {
            format,
            width: stream.visible.0,
            height: stream.visible.1,
            order: self.order,
            full_range: stream.full_range,
        })
    }

    fn teardown(&mut self) {
        self.built = None;
    }
}

/// One output call, the stream's changes taken: a picture held, or the
/// decoder drained.
fn pull(built: &mut Built, mf: &Mf) -> Result<()> {
    for _ in 0..MOST_CHANGES {
        match built.transform.output(&built.output)? {
            Out::Picture => {
                built.held = true;
                return Ok(());
            }
            Out::NeedInput => {
                built.drained = true;
                return Ok(());
            }
            // The stream's size, or a depth the decoder had not seen at its
            // making: its output set again, at the depth the parameter set
            // says, and a buffer that fits.
            Out::Changed => {
                built.layout =
                    built
                        .transform
                        .set_output(built.stream.ten_bit)
                        .map_err(|e| match e {
                            framework::Error::NoLayout => Error::NoProfile,
                            e => Error::Framework(e),
                        })?;
                if built.output.capacity() < built.layout.size {
                    built.output = mf.sample(built.layout.size)?;
                }
            }
        }
    }
    Err(Error::Stream)
}

impl Decoder for Backend {
    fn build(&mut self, header: &VideoHeader) -> core::result::Result<(), Fault> {
        self.teardown();
        self.codec = header.codec;
        // The decoder itself waits for the first parameter set, which says
        // the size and the depth.
        Ok(())
    }

    fn feed(&mut self, unit: &[u8]) -> core::result::Result<Fed, Fault> {
        match self.decode(unit) {
            Ok(fed) => Ok(fed),
            // A stream the decoder must not be given, or one it will not be
            // made for, would be refused again on every keyframe asked for;
            // nothing to ask.
            Err(Error::NoProfile | Error::TooLarge | Error::Build(_)) => Err(Fault::Fatal),
            Err(_) => Err(Fault::Unrecoverable),
        }
    }

    fn take(&mut self, out: &mut Planes<'_>) -> core::result::Result<Option<Picture>, Fault> {
        let Some(built) = &mut self.built else {
            return Ok(None);
        };
        if !built.held {
            if built.drained {
                return Ok(None);
            }
            pull(built, self.mf).map_err(|e| match e {
                Error::NoProfile => Fault::Fatal,
                _ => Fault::Unrecoverable,
            })?;
            if !built.held {
                return Ok(None);
            }
        }
        self.copy(out).map(Some).map_err(|e| match e {
            Error::NoProfile => Fault::Fatal,
            _ => Fault::Unrecoverable,
        })
    }

    fn destroy(&mut self) {
        self.teardown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two workers for HEVC at most, half the processors for H.264, never
    /// none.
    #[test]
    fn the_workers_are_a_share_of_the_processors() {
        let threads = std::thread::available_parallelism().map_or(1, usize::from);
        assert_eq!(workers(Coding::Hevc) as usize, threads.min(2));
        assert_eq!(workers(Coding::H264) as usize, (threads / 2).max(1));
    }
}
