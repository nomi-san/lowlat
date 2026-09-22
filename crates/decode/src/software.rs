//! The software backend: the machine's own libavcodec decodes, and the
//! pictures leave in the same four formats the hardware backends hand out.
//!
//! The library reads the bitstream itself here, so none of this crate's
//! readers run; what this backend owns is the unit's hand-over, the picture's
//! hand-out, and the conversion between them. The decoder produces three
//! planes with ten-bit samples in the low bits; the copy that hands a picture
//! out interleaves the chroma for NV12 and P010 and shifts ten-bit samples to
//! the high bits, so an application sees one format set whatever decodes,
//! and no flag exists to get wrong.
//!
//! **Nothing here allocates per unit on this side.** The packet the unit is
//! copied into is sized by the library, which pads it as its decoders
//! require and keeps the buffer's ownership; what the library allocates
//! within itself is its own.

use core::ffi::{CStr, c_int};

use lowlat_core::video::{Codec, VideoHeader};
use lowlat_drivers::lavc::{AVCodecContext, AVDictionary, EAGAIN, Frame, Lavc, Packet};
pub use lowlat_drivers::lavc::{Origin, Refusal};

use crate::{Caps, Decoder, Fault, Fed, Format, Picture, Planes};

/// `AVERROR_EOF`: the tag `EOF ` as a negative word, what a drained decoder
/// answers once it has nothing left.
const EOF: c_int = -0x2046_4F45;

/// The most worker threads asked of the decoder. Slice threading only: the
/// parallelism is within a picture, so no picture waits on a later one.
const MAX_THREADS: usize = 4;
const THREADS: [&CStr; MAX_THREADS] = [c"1", c"2", c"3", c"4"];

/// The worker threads asked of the decoder on a machine with `parallelism`
/// hardware threads: as many as the machine has, and never more than
/// [`MAX_THREADS`]. **Never more than the machine has**: workers past the
/// cores are not parallelism but contention, and on a two-core machine four
/// of them starve the application's own threads -- the window's message
/// pump among them -- which reads as the application hanging. The library
/// raises no thread's priority for the same reason: it runs inside the
/// application's process, and a worker above the application's threads
/// inverts the order the application chose.
fn slice_threads(parallelism: usize) -> usize {
    parallelism.clamp(1, MAX_THREADS)
}

/// What a loaded pair decodes. The depth and chroma rows are the second
/// codec's decoder's, which takes every profile the readers admit; each is
/// proved by the fixtures rather than asserted.
pub fn caps(lavc: &Lavc) -> Caps {
    Caps {
        h264: lavc.decodes_h264,
        hevc: lavc.decodes_hevc,
        hevc_10: lavc.decodes_hevc,
        hevc_444: lavc.decodes_hevc,
        hevc_444_10: lavc.decodes_hevc,
    }
}

/// The decoder over one loaded pair.
pub struct Backend<'a> {
    lavc: &'a Lavc,
    codec: Codec,
    context: *mut AVCodecContext,
    packet: *mut Packet,
    /// The picture last received and not yet taken, when `held`.
    frame: *mut Frame,
    held: bool,
    /// Whether the decoder has been told the stream ended: a test's need,
    /// since a live stream never ends this way. Nothing is fed after it.
    drained: bool,
    /// Pictures handed out since the build: the order this backend reports,
    /// since the library hands them out in output order.
    order: i32,
    /// The last unit's decode and the last picture's hand-out, in
    /// microseconds, for the log.
    pub decode_us: u32,
    pub readback_us: u32,
}

impl core::fmt::Debug for Backend<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Backend")
            .field("codec", &self.codec)
            .field("built", &!self.context.is_null())
            .field("held", &self.held)
            .finish()
    }
}

impl<'a> Backend<'a> {
    pub fn new(lavc: &'a Lavc) -> Self {
        Self {
            lavc,
            codec: Codec::H264,
            context: core::ptr::null_mut(),
            packet: core::ptr::null_mut(),
            frame: core::ptr::null_mut(),
            held: false,
            drained: false,
            order: 0,
            decode_us: 0,
            readback_us: 0,
        }
    }

    /// The layout a frame's format is handed out as; none for a format this
    /// backend does not convert.
    fn format_of(&self, format: c_int) -> Option<Format> {
        let f = &self.lavc.formats;
        if format == f.yuv420p || format == f.yuvj420p {
            Some(Format::Nv12)
        } else if format == f.yuv420p10le {
            Some(Format::P010)
        } else if format == f.yuv444p || format == f.yuvj444p {
            Some(Format::Yuv444)
        } else if format == f.yuv444p10le {
            Some(Format::Yuv444_16)
        } else {
            None
        }
    }

    /// The held picture's size and layout.
    fn held_output(&self) -> Option<(u32, u32, Format)> {
        if !self.held || self.frame.is_null() {
            return None;
        }
        // SAFETY: the frame is the library's, live until freed in
        // `destroy`, and `held` says the library filled it.
        let (width, height, format) = unsafe {
            (
                core::ptr::addr_of!((*self.frame).width).read(),
                core::ptr::addr_of!((*self.frame).height).read(),
                core::ptr::addr_of!((*self.frame).format).read(),
            )
        };
        let format = self.format_of(format)?;
        let width = u32::try_from(width).ok()?;
        let height = u32::try_from(height).ok()?;
        (width > 0 && height > 0).then_some((width, height, format))
    }

    /// The size and layout the pictures [`Decoder::take`] hands out have,
    /// once one is held: the library says nothing about a stream before it
    /// has decoded a picture of it.
    pub fn output(&self) -> Option<(u32, u32, Format)> {
        self.held_output()
    }

    /// Tell the decoder the stream ended, so what it still holds comes out
    /// through [`Decoder::take`]; a test's need, since a live stream never
    /// ends this way. Nothing is fed after it.
    pub fn drain(&mut self) {
        if self.context.is_null() || self.drained {
            return;
        }
        self.drained = true;
        // SAFETY: a null packet is the library's own end-of-stream signal.
        unsafe { (self.lavc.send_packet)(self.context, core::ptr::null()) };
        if !self.held {
            // A fault here surfaces on the take that finds nothing.
            let _ = self.receive();
        }
    }

    /// Receive the next picture into the frame, if one is ready. Only ever
    /// called with none held.
    fn receive(&mut self) -> Result<(), Fault> {
        // SAFETY: the context and the frame are live; the frame holds no
        // picture, so the library may fill it.
        let rc = unsafe { (self.lavc.receive_frame)(self.context, self.frame) };
        if rc == 0 {
            self.held = true;
            Ok(())
        } else if rc == EAGAIN || rc == EOF {
            Ok(())
        } else {
            lowlat_common::log_warn!("decode: software receive failed, rc={rc}");
            Err(Fault::Unrecoverable)
        }
    }

    /// Let go of the held picture.
    fn release(&mut self) {
        if self.held {
            // SAFETY: the frame is live and holds a picture.
            unsafe { (self.lavc.frame_unref)(self.frame) };
            self.held = false;
        }
    }
}

impl Decoder for Backend<'_> {
    fn build(&mut self, header: &VideoHeader) -> Result<(), Fault> {
        self.destroy();
        self.codec = header.codec;
        let name = match header.codec {
            Codec::H264 => c"h264",
            Codec::H265 => c"hevc",
        };
        // SAFETY: a name lookup on the library's registry; the context is
        // allocated for that codec and freed in `destroy`.
        let codec = unsafe { (self.lavc.find_decoder_by_name)(name.as_ptr()) };
        if codec.is_null() {
            // The pair lacks this codec: the capability said so and nothing
            // a keyframe brings changes it.
            return Err(Fault::Fatal);
        }
        // SAFETY: as above.
        let context = unsafe { (self.lavc.alloc_context)(codec) };
        if context.is_null() {
            return Err(Fault::Fatal);
        }
        self.context = context;

        let threads = slice_threads(std::thread::available_parallelism().map_or(1, |n| n.get()));
        let mut options: *mut AVDictionary = core::ptr::null_mut();
        // Slice threading and nothing else. The low-delay flag was measured
        // on every committed clip and changed nothing: a stream that declares
        // its reordering is held to it and one that declares none is put
        // out at once with or without it.
        // SAFETY: the dictionary is the library's; the option names and
        // values are static strings, and the dictionary is freed after the
        // open whatever the open answered.
        let rc = unsafe {
            (self.lavc.dict_set)(
                &raw mut options,
                c"threads".as_ptr(),
                THREADS.get(threads - 1).unwrap_or(&c"1").as_ptr(),
                0,
            );
            (self.lavc.dict_set)(
                &raw mut options,
                c"thread_type".as_ptr(),
                c"slice".as_ptr(),
                0,
            );
            let rc = (self.lavc.open)(context, codec, &raw mut options);
            (self.lavc.dict_free)(&raw mut options);
            rc
        };
        if rc < 0 {
            lowlat_common::log_warn!("decode: software open failed, rc={rc}");
            self.destroy();
            return Err(Fault::Fatal);
        }
        // SAFETY: allocated through the library, freed in `destroy`.
        unsafe {
            self.frame = (self.lavc.frame_alloc)();
            self.packet = (self.lavc.packet_alloc)();
        }
        if self.frame.is_null() || self.packet.is_null() {
            self.destroy();
            return Err(Fault::Fatal);
        }
        self.held = false;
        self.drained = false;
        self.order = 0;
        Ok(())
    }

    fn feed(&mut self, unit: &[u8]) -> Result<Fed, Fault> {
        if self.context.is_null() || self.drained {
            return Err(Fault::Fatal);
        }
        let Ok(len) = c_int::try_from(unit.len()) else {
            return Err(Fault::Unrecoverable);
        };
        let started = lowlat_common::clock::Time::now();
        // SAFETY: the packet is live; the library sizes and pads its buffer
        // and the copy stays within `len` bytes of it.
        let rc = unsafe {
            (self.lavc.packet_unref)(self.packet);
            let rc = (self.lavc.new_packet)(self.packet, len);
            if rc < 0 {
                return Err(Fault::Unrecoverable);
            }
            let data = core::ptr::addr_of!((*self.packet).data).read();
            if data.is_null() {
                return Err(Fault::Unrecoverable);
            }
            core::ptr::copy_nonoverlapping(unit.as_ptr(), data, unit.len());
            (self.lavc.send_packet)(self.context, self.packet)
        };
        let rc = if rc == EAGAIN {
            // The decoder wants its picture taken before it takes another
            // unit: take it into the held frame and offer the unit once
            // more. With one picture already held there is nowhere to put
            // it, which is a decoder that has stopped following the rules.
            if self.held {
                return Err(Fault::Unrecoverable);
            }
            self.receive()?;
            // SAFETY: the packet is still the one filled above.
            unsafe { (self.lavc.send_packet)(self.context, self.packet) }
        } else {
            rc
        };
        if rc < 0 {
            lowlat_common::log_warn!("decode: software send failed, rc={rc}");
            return Err(Fault::Unrecoverable);
        }
        if !self.held {
            self.receive()?;
        }
        self.decode_us = micros(lowlat_common::clock::elapsed_ms(started));
        Ok(if self.held {
            Fed::Picture
        } else {
            Fed::NeedMoreData
        })
    }

    fn take(&mut self, out: &mut Planes<'_>) -> Result<Option<Picture>, Fault> {
        let Some((width, height, format)) = self.held_output() else {
            if self.held {
                // A picture in a layout this backend does not convert: the
                // stream is one no keyframe will change.
                // SAFETY: the frame is live and holds a picture.
                let (name, width, height) = unsafe {
                    (
                        self.lavc
                            .format_name(core::ptr::addr_of!((*self.frame).format).read()),
                        core::ptr::addr_of!((*self.frame).width).read(),
                        core::ptr::addr_of!((*self.frame).height).read(),
                    )
                };
                lowlat_common::log_warn!(
                    "decode: software picture unusable, format={name} width={width} height={height}"
                );
                self.release();
                return Err(Fault::Fatal);
            }
            return Ok(None);
        };
        let started = lowlat_common::clock::Time::now();
        // SAFETY: the frame is live and holds a picture of `width` x
        // `height`, whose planes the library owns until the release below.
        let converted = unsafe { convert(&*self.frame, format, width, height, out) };
        self.release();
        converted?;
        // The next picture ready, so a flush leaves none late.
        self.receive()?;
        self.readback_us = micros(lowlat_common::clock::elapsed_ms(started));
        let order = self.order;
        self.order = self.order.wrapping_add(1);
        Ok(Some(Picture {
            format,
            width,
            height,
            order,
        }))
    }

    fn destroy(&mut self) {
        self.release();
        // SAFETY: each was allocated through the library and is freed once;
        // the context's free closes it. Freed in the reverse of their
        // making, the context last.
        unsafe {
            if !self.packet.is_null() {
                (self.lavc.packet_free)(&raw mut self.packet);
            }
            if !self.frame.is_null() {
                (self.lavc.frame_free)(&raw mut self.frame);
            }
            if !self.context.is_null() {
                (self.lavc.free_context)(&raw mut self.context);
            }
        }
        self.packet = core::ptr::null_mut();
        self.frame = core::ptr::null_mut();
        self.context = core::ptr::null_mut();
        self.held = false;
        self.drained = false;
    }
}

impl Drop for Backend<'_> {
    fn drop(&mut self) {
        self.destroy();
    }
}

/// Whole microseconds from a millisecond figure, saturated.
fn micros(ms: f64) -> u32 {
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a non-negative duration in whole microseconds, saturated"
    )]
    let us = (ms * 1000.0).max(0.0).min(f64::from(u32::MAX)) as u32;
    us
}

/// A plane of the library's frame as rows: the pointer, its pitch, and the
/// bytes a row carries.
struct Source {
    data: *const u8,
    pitch: usize,
}

impl Source {
    /// Row `row`, `bytes` long.
    ///
    /// # Safety
    ///
    /// The row is within the plane the library allocated.
    unsafe fn row(&self, row: usize, bytes: usize) -> &[u8] {
        // SAFETY: the caller's contract.
        unsafe { core::slice::from_raw_parts(self.data.add(row * self.pitch), bytes) }
    }
}

/// The frame's planes into the caller's, converted to `format`.
///
/// # Safety
///
/// `frame` holds a decoded picture of `width` x `height` in the layout
/// `format` was derived from.
unsafe fn convert(
    frame: &Frame,
    format: Format,
    width: u32,
    height: u32,
    out: &mut Planes<'_>,
) -> Result<(), Fault> {
    let width = usize::try_from(width).map_err(|_| Fault::Unrecoverable)?;
    let height = usize::try_from(height).map_err(|_| Fault::Unrecoverable)?;
    let plane = |index: usize| -> Result<Source, Fault> {
        let data = frame.data.get(index).copied().ok_or(Fault::Unrecoverable)?;
        let pitch = frame
            .linesize
            .get(index)
            .copied()
            .ok_or(Fault::Unrecoverable)?;
        if data.is_null() || pitch <= 0 {
            return Err(Fault::Unrecoverable);
        }
        Ok(Source {
            data: data.cast_const(),
            pitch: usize::try_from(pitch).map_err(|_| Fault::Unrecoverable)?,
        })
    };
    let (y, u, v) = (plane(0)?, plane(1)?, plane(2)?);
    let ten_bit = format.sample() == 2;
    let row_bytes = width * format.sample();

    // Luma: a copy, or a shift of every sample to the high bits.
    let rows_y = height.min(out.y.len() / out.y_pitch.max(1));
    for row in 0..rows_y {
        // SAFETY: within the luma plane of a `height`-row picture.
        let from = unsafe { y.row(row, row_bytes) };
        let to = out
            .y
            .get_mut(row * out.y_pitch..row * out.y_pitch + row_bytes)
            .ok_or(Fault::Unrecoverable)?;
        if ten_bit {
            shift_row(from, to);
        } else {
            to.copy_from_slice(from);
        }
    }

    let chroma_rows = format.chroma_rows(height);
    if format.full_chroma() {
        // Three planes: each chroma plane is the picture's size.
        let rows_u = chroma_rows.min(out.uv.len() / out.uv_pitch.max(1));
        for row in 0..rows_u {
            // SAFETY: within the first chroma plane.
            let from = unsafe { u.row(row, row_bytes) };
            let to = out
                .uv
                .get_mut(row * out.uv_pitch..row * out.uv_pitch + row_bytes)
                .ok_or(Fault::Unrecoverable)?;
            if ten_bit {
                shift_row(from, to);
            } else {
                to.copy_from_slice(from);
            }
        }
        let rows_v = chroma_rows.min(out.v.len() / out.v_pitch.max(1));
        for row in 0..rows_v {
            // SAFETY: within the second chroma plane.
            let from = unsafe { v.row(row, row_bytes) };
            let to = out
                .v
                .get_mut(row * out.v_pitch..row * out.v_pitch + row_bytes)
                .ok_or(Fault::Unrecoverable)?;
            if ten_bit {
                shift_row(from, to);
            } else {
                to.copy_from_slice(from);
            }
        }
    } else {
        // Two planes out of three: the chroma planes interleaved, each half
        // the width, into one row of the picture's width in bytes.
        let half = width.div_ceil(2) * format.sample();
        let rows_uv = chroma_rows.min(out.uv.len() / out.uv_pitch.max(1));
        for row in 0..rows_uv {
            // SAFETY: within the chroma planes, each `half` bytes a row.
            let (from_u, from_v) = unsafe { (u.row(row, half), v.row(row, half)) };
            let to = out
                .uv
                .get_mut(row * out.uv_pitch..row * out.uv_pitch + 2 * half)
                .ok_or(Fault::Unrecoverable)?;
            if ten_bit {
                interleave_shift_row(from_u, from_v, to);
            } else {
                interleave_row(from_u, from_v, to);
            }
        }
    }
    Ok(())
}

/// A row of bytes as the sixteen-bit samples it holds, when it starts on a
/// sample boundary; a row that does not takes the byte-wise path.
fn samples(row: &[u8]) -> Option<&[u16]> {
    // SAFETY: every bit pattern is a valid `u16`; the middle part is the
    // aligned one by construction, and it is the whole row when the head
    // is empty and the row is an even number of bytes.
    let (head, middle, tail) = unsafe { row.align_to::<u16>() };
    (head.is_empty() && tail.is_empty()).then_some(middle)
}

fn samples_mut(row: &mut [u8]) -> Option<&mut [u16]> {
    // SAFETY: as [`samples`].
    let (head, middle, tail) = unsafe { row.align_to_mut::<u16>() };
    (head.is_empty() && tail.is_empty()).then_some(middle)
}

/// Sixteen-bit samples with the value in the low ten bits, to the high ten
/// of sixteen: what P010 and the sixteen-bit planar layouts hold. Bytes in
/// and out, native order, as the library lays its planes out on this
/// platform.
fn shift_row(from: &[u8], to: &mut [u8]) {
    if let (Some(from), Some(to)) = (samples(from), samples_mut(to)) {
        for (s, d) in from.iter().zip(to.iter_mut()) {
            *d = *s << 6;
        }
        return;
    }
    for (src, dst) in from.chunks_exact(2).zip(to.chunks_exact_mut(2)) {
        if let ([a, b], [c, d]) = (src, dst) {
            let [lo, hi] = (u16::from_ne_bytes([*a, *b]) << 6).to_ne_bytes();
            *c = lo;
            *d = hi;
        }
    }
}

/// Two eight-bit chroma rows into one interleaved row: U, V, U, V.
fn interleave_row(u: &[u8], v: &[u8], to: &mut [u8]) {
    for ((cu, cv), dst) in u.iter().zip(v.iter()).zip(to.chunks_exact_mut(2)) {
        if let [a, b] = dst {
            *a = *cu;
            *b = *cv;
        }
    }
}

/// Two ten-bit chroma rows into one interleaved row, shifted to the high
/// bits as [`shift_row`] shifts.
fn interleave_shift_row(u: &[u8], v: &[u8], to: &mut [u8]) {
    if let (Some(u), Some(v), Some(to)) = (samples(u), samples(v), samples_mut(to)) {
        for ((cu, cv), dst) in u.iter().zip(v.iter()).zip(to.chunks_exact_mut(2)) {
            if let [a, b] = dst {
                *a = *cu << 6;
                *b = *cv << 6;
            }
        }
        return;
    }
    for ((cu, cv), dst) in u
        .chunks_exact(2)
        .zip(v.chunks_exact(2))
        .zip(to.chunks_exact_mut(4))
    {
        if let ([u0, u1], [v0, v1], [a, b, c, d]) = (cu, cv, dst) {
            let [ul, uh] = (u16::from_ne_bytes([*u0, *u1]) << 6).to_ne_bytes();
            let [vl, vh] = (u16::from_ne_bytes([*v0, *v1]) << 6).to_ne_bytes();
            *a = ul;
            *b = uh;
            *c = vl;
            *d = vh;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The conversions against the same thing written plainly, on rows that
    /// are not a multiple of anything convenient.
    #[test]
    fn the_conversions_match_a_plain_reference() {
        let width = 13usize;
        let u: Vec<u8> = (0..width).map(|i| (i * 7 + 1) as u8).collect();
        let v: Vec<u8> = (0..width).map(|i| (i * 11 + 3) as u8).collect();
        let mut out = vec![0u8; 2 * width];
        interleave_row(&u, &v, &mut out);
        for i in 0..width {
            assert_eq!(out[2 * i], u[i]);
            assert_eq!(out[2 * i + 1], v[i]);
        }

        let samples: Vec<u16> = (0..width).map(|i| (i * 79 % 1024) as u16).collect();
        let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_ne_bytes()).collect();
        let mut shifted = vec![0u8; bytes.len()];
        shift_row(&bytes, &mut shifted);
        for (i, s) in samples.iter().enumerate() {
            let got = u16::from_ne_bytes([shifted[2 * i], shifted[2 * i + 1]]);
            assert_eq!(got, s << 6, "sample {i}");
            assert_eq!(got & 0x3f, 0, "the low bits are clear");
        }

        let u16s: Vec<u16> = (0..width).map(|i| (i * 37 % 1024) as u16).collect();
        let v16s: Vec<u16> = (0..width).map(|i| (i * 53 % 1024) as u16).collect();
        let ub: Vec<u8> = u16s.iter().flat_map(|s| s.to_ne_bytes()).collect();
        let vb: Vec<u8> = v16s.iter().flat_map(|s| s.to_ne_bytes()).collect();
        let mut inter = vec![0u8; 4 * width];
        interleave_shift_row(&ub, &vb, &mut inter);
        for i in 0..width {
            let cu = u16::from_ne_bytes([inter[4 * i], inter[4 * i + 1]]);
            let cv = u16::from_ne_bytes([inter[4 * i + 2], inter[4 * i + 3]]);
            assert_eq!(cu, u16s[i] << 6);
            assert_eq!(cv, v16s[i] << 6);
        }
    }

    /// The sixteen-bit conversions take the byte-wise path when a row does
    /// not start on a sample boundary, and both paths agree.
    #[test]
    fn an_unaligned_row_converts_the_same() {
        let width = 64usize;
        let mut storage = vec![0u8; 2 * width + 8];
        let mut out_a = vec![0u8; 2 * width + 8];
        let mut out_b = vec![0u8; 2 * width + 8];
        for (i, b) in storage.iter_mut().enumerate() {
            *b = (i * 13 % 251) as u8;
        }
        // Aligned source and destination.
        shift_row(&storage[..2 * width], &mut out_a[..2 * width]);
        // The same bytes read one byte in, written one byte in.
        let shifted: Vec<u8> = storage[1..2 * width + 1].to_vec();
        shift_row(&shifted, &mut out_b[1..2 * width + 1]);
        let a: Vec<u16> = out_a[..2 * width]
            .chunks_exact(2)
            .map(|c| u16::from_ne_bytes([c[0], c[1]]))
            .collect();
        let b: Vec<u16> = out_b[1..2 * width + 1]
            .chunks_exact(2)
            .map(|c| u16::from_ne_bytes([c[0], c[1]]))
            .collect();
        let expect: Vec<u16> = storage[..2 * width]
            .chunks_exact(2)
            .map(|c| u16::from_ne_bytes([c[0], c[1]]) << 6)
            .collect();
        assert_eq!(a, expect);
        let expect_b: Vec<u16> = shifted
            .chunks_exact(2)
            .map(|c| u16::from_ne_bytes([c[0], c[1]]) << 6)
            .collect();
        assert_eq!(b, expect_b);
    }

    /// **The workers never outnumber the machine's threads**, and never
    /// exceed four: one on one, two on two, four on sixteen.
    #[test]
    fn the_workers_follow_the_machine() {
        assert_eq!(slice_threads(1), 1);
        assert_eq!(slice_threads(2), 2);
        assert_eq!(slice_threads(3), 3);
        assert_eq!(slice_threads(4), 4);
        assert_eq!(slice_threads(8), 4);
        assert_eq!(slice_threads(16), 4);
        assert_eq!(slice_threads(0), 1);
    }

    /// The end-of-stream code is the four tag bytes, negated, as the library
    /// defines it: `EOF ` read as a little-endian word.
    #[test]
    fn the_end_of_stream_code_is_the_tag() {
        let tag = i32::from_le_bytes(*b"EOF ");
        assert_eq!(EOF, -tag);
    }

    /// The `p` percentile, `p` in hundredths.
    fn percentile(samples: &mut [u128], p: usize) -> u128 {
        samples.sort_unstable();
        let at = (samples.len().saturating_sub(1) * p).div_ceil(100);
        samples[at.min(samples.len().saturating_sub(1))]
    }

    /// **The four conversions timed at 2560x1440**, the copy from a frame's
    /// three planes into the caller's slots, on this machine: the cost the
    /// software backend adds to a picture, to set beside the decode time.
    /// `cargo test --release -p lowlat-decode --lib software -- --ignored
    /// --nocapture`.
    #[test]
    #[ignore = "a timing probe"]
    fn the_conversions_at_2560x1440() {
        let (width, height) = (2560usize, 1440usize);
        for (format, ten_bit, full) in [
            (Format::Nv12, false, false),
            (Format::P010, true, false),
            (Format::Yuv444, false, true),
            (Format::Yuv444_16, true, true),
        ] {
            let sample = if ten_bit { 2 } else { 1 };
            let chroma_w = if full { width } else { width.div_ceil(2) };
            let chroma_h = if full { height } else { height.div_ceil(2) };
            // The library's planes: a pitch of its own, 64-byte aligned.
            let pitch = (width * sample).div_ceil(64) * 64;
            let chroma_pitch = (chroma_w * sample).div_ceil(64) * 64;
            let mut y = vec![0u8; pitch * height + 64];
            let mut u = vec![0u8; chroma_pitch * chroma_h + 64];
            let mut v = vec![0u8; chroma_pitch * chroma_h + 64];
            for (i, b) in y.iter_mut().enumerate() {
                *b = (i % 251) as u8;
            }
            for (i, b) in u.iter_mut().enumerate() {
                *b = (i % 241) as u8;
            }
            for (i, b) in v.iter_mut().enumerate() {
                *b = (i % 239) as u8;
            }
            let mut planes = [core::ptr::null_mut::<u8>(); 8];
            let mut linesize = [0 as c_int; 8];
            planes[0] = y.as_mut_ptr();
            planes[1] = u.as_mut_ptr();
            planes[2] = v.as_mut_ptr();
            linesize[0] = pitch as c_int;
            linesize[1] = chroma_pitch as c_int;
            linesize[2] = chroma_pitch as c_int;
            let frame = Frame {
                data: planes,
                linesize,
                extended_data: core::ptr::null_mut(),
                width: width as c_int,
                height: height as c_int,
                nb_samples: 0,
                format: 0,
            };
            // The slots, at the picture's own pitch as the queue lays them.
            let out_pitch = width * sample;
            let mut oy = vec![0u8; out_pitch * height];
            let mut ouv = vec![0u8; out_pitch * chroma_h];
            let mut ov = vec![0u8; if full { out_pitch * chroma_h } else { 0 }];
            let mut times = Vec::new();
            for _ in 0..200 {
                let mut out = Planes {
                    y: &mut oy,
                    y_pitch: out_pitch,
                    uv: &mut ouv,
                    uv_pitch: out_pitch,
                    v: &mut ov,
                    v_pitch: out_pitch,
                };
                let started = std::time::Instant::now();
                // SAFETY: the frame's planes are the vectors above, sized
                // for the picture.
                unsafe { convert(&frame, format, width as u32, height as u32, &mut out) }
                    .expect("convert");
                times.push(started.elapsed().as_micros());
            }
            println!(
                "{format:?} at {width}x{height}: p50 {} us p95 {} us max {} us",
                percentile(&mut times, 50),
                percentile(&mut times, 95),
                times.iter().max().copied().unwrap_or(0)
            );
        }
    }
}
