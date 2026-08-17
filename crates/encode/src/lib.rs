//! Video encoders. Submit and poll are separate operations so the trait
//! cannot express a serialized capture-to-encode loop.
//!
//! See docs/05-host.md section 4.

// Phase 5 lands the hardware backend; Phase 11 the software one.

pub mod bitstream;
pub mod cuda;
mod ffi;
pub mod h264;
pub mod h265;
pub mod nvenc;
pub mod vaapi;

/// What a collect found.
///
/// Shared by every backend, because the caller downstream of an encoder is the
/// packetiser and it has no business knowing which one produced the bytes.
#[derive(Debug)]
pub enum Poll<'a> {
    /// A finished access unit.
    ///
    /// **Valid only until the next call on this encoder.** The buffer belongs
    /// to the driver and is handed back on the next submit or collect, so a
    /// consumer that keeps the slice rather than copying out of it reads a
    /// picture that has since been overwritten.
    Ready {
        bitstream: &'a [u8],
        /// Whether this access unit can be decoded without any before it.
        /// The delivery gate needs it to release a guest that is skipping,
        /// and no backend reports it the same way, so each one answers here.
        keyframe: bool,
    },
    /// Nothing finished yet. **Not an error and not a wait**: the caller goes
    /// round its loop and asks again.
    Pending,
}

/// One video encoder.
///
/// **Submit and poll are separate, and that is the whole point.** A blocking
/// `encode(frame) -> bitstream` cannot overlap the next frame's preparation,
/// which caps a pipeline far below the rate the hardware can hold. The trait
/// is shaped so the serialized form cannot be written through it.
///
/// The error type stays each backend's own rather than being flattened into
/// one. A caller instantiates against a concrete backend and gets that
/// backend's diagnosis; collapsing them here would trade a status a driver
/// can be asked about for a variant that says only which vendor failed.
pub trait Encoder {
    type Error: core::error::Error;

    /// Hand a frame to the hardware. **Returns as soon as it is queued**, not
    /// when it is encoded.
    ///
    /// Refuses with the backend's queue-full error when as many pictures are
    /// in flight as it holds, which is back pressure rather than a fault: the
    /// caller collects one and tries again.
    fn submit(
        &mut self,
        frame: &lowlat_capture::Frame<'_>,
        force_keyframe: bool,
    ) -> Result<(), Self::Error>;

    /// Collect a finished picture, or report that none is ready.
    ///
    /// **Never blocks waiting for a picture that has not finished.** A
    /// not-ready answer costs a driver round trip; it must not cost a frame.
    fn poll(&mut self) -> Result<Poll<'_>, Self::Error>;

    /// Change the bitrate for subsequent pictures.
    ///
    /// **Never reinitialises the encoder and never forces a keyframe.** The
    /// congestion controller moves this many times a minute, and either of
    /// those would put a visible stutter in the stream every time the network
    /// hiccuped.
    fn reconfigure(&mut self, bitrate_bps: u32) -> Result<(), Self::Error>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive any encoder from the synthetic source, keeping `depth` pictures
    /// in flight.
    ///
    /// **This function is the trait's reason to exist.** It is written once
    /// and compiled against both backends, so a difference either one still
    /// carries in its shape is a compile error here rather than an interface
    /// that describes only whichever was written first.
    fn encode_run<E: Encoder>(
        encoder: &mut E,
        width: u32,
        height: u32,
        frames: usize,
        depth: usize,
        // Called once per collected unit, after the borrow the collect
        // handed out has ended, so it can ask the encoder what else that
        // collect reported, or change its rate. A backend with nothing to add
        // passes a no-op.
        mut observe: impl FnMut(&mut E, usize, &[u8]),
    ) -> (Vec<u8>, usize) {
        let mut source = lowlat_capture::synthetic::Synthetic::new(width, height);
        let mut stream = Vec::new();
        let mut unit = Vec::new();
        let (mut submitted, mut collected, mut keyframes) = (0usize, 0usize, 0usize);

        while collected < frames {
            if submitted < frames && submitted - collected < depth {
                // The first picture is forced; the rest are left to the
                // encoder, which is what makes the count below meaningful.
                encoder
                    .submit(&source.acquire(), submitted == 0)
                    .expect("submit");
                submitted += 1;
            }
            match encoder.poll().expect("poll") {
                Poll::Ready {
                    bitstream,
                    keyframe,
                } => {
                    assert!(!bitstream.is_empty(), "an empty access unit");
                    unit.clear();
                    unit.extend_from_slice(bitstream);
                    if keyframe {
                        keyframes += 1;
                    }
                }
                Poll::Pending => {
                    std::hint::spin_loop();
                    continue;
                }
            }
            observe(encoder, collected, &unit);
            stream.extend_from_slice(&unit);
            collected += 1;
        }
        (stream, keyframes)
    }

    /// Check the vendor backend's collect block against the bytes it
    /// describes.
    ///
    /// **The collect used to race the encoder, and this is what stops it
    /// coming back.** A refresh picture reported megabytes it had not coded
    /// and the slice count came back as noise, both because the block was
    /// read before the driver had finished writing it. From the bytes alone
    /// that is invisible: a fresh output buffer is already zero, so a length
    /// past the picture looks exactly like a picture with zeros after it.
    ///
    /// Every assertion below is a way for that race to show itself. The
    /// picture kind and the quantiser say the block is being read where the
    /// driver writes; the slice count and the trailing span say it is being
    /// read after the driver is done.
    fn audit_collect(index: usize, unit: &[u8], report: nvenc::LockReport) {
        // A well formed unit cannot end in a zero byte: the trailing bits put
        // a stop bit in the last one, and escaping forbids three zeros inside
        // a payload. So anything after this offset was appended.
        let last_set = unit.iter().rposition(|&byte| byte != 0);
        let trailing = last_set.map_or(unit.len(), |at| unit.len() - at - 1);

        println!(
            "  picture {index}: {} bytes, {} coded, {} trailing, {:?} qp {}",
            unit.len(),
            last_set.map_or(0, |at| at + 1),
            trailing,
            report.picture,
            report.frame_avg_qp
        );

        // The forced refresh is the first picture and the encoder is left to
        // choose the rest. A block read at the wrong offsets does not land on
        // the kind that was asked for.
        if index == 0 {
            assert_eq!(
                report.picture,
                nvenc::Picture::Idr,
                "the forced refresh did not come back as one, so the collect \
                 block is not being read where the driver wrote it"
            );
        } else {
            assert!(
                matches!(report.picture, nvenc::Picture::P | nvenc::Picture::NonRefP),
                "picture {index} came back as {:?}, which is neither predicted \
                 kind and so is not a picture kind at all",
                report.picture
            );
        }
        assert_eq!(report.picture_struct, 1, "a picture that is not one frame");
        // **What ties this block to the picture it is supposed to describe.**
        // Nothing sets it on submit, so the driver counting it in collect
        // order is the one field that says the two are the same picture. A
        // pool slot filled from a block belonging to a different frame is not
        // something anything downstream could detect.
        assert_eq!(
            u32::try_from(index).unwrap_or(u32::MAX),
            report.frame_idx,
            "the block describes a different picture from the one collected"
        );
        assert!(
            (1..=51).contains(&report.frame_avg_qp),
            "quantiser {} is outside the range the codec has, so the field is \
             not the quantiser",
            report.frame_avg_qp
        );
        // **This is the sensitive one, so it is asserted everywhere.** The
        // slice count was noise on the final collect of every run until the
        // collect stopped reading a block the driver had not finished
        // writing, and it was noise for the same reason the refresh picture
        // claimed megabytes it had not coded. Both went away together.
        //
        // So a count that is not one now means the collect is racing the
        // encoder again, and this catches it one picture after it starts
        // rather than in a packetiser splitting a unit on a count out of
        // nowhere.
        //
        // The interface only promises the field when slice offsets are asked
        // for, which we do not do. If this ever fails on a different driver,
        // rule that out before suspecting the collect.
        assert_eq!(
            report.num_slices, 1,
            "picture {index} reports {} slices, where every configured picture \
             is one",
            report.num_slices
        );

        // The length has to be the coded length, with nothing after it. Not a
        // style check: the buffers are reused, so a length past the picture
        // hands the caller whatever the buffer held last, and the caller
        // cannot tell.
        assert_eq!(
            trailing, 0,
            "picture {index} reports {} bytes past its last coded byte, which \
             is the buffer's previous contents",
            trailing
        );
    }

    /// Count access units of one type, by start code.
    ///
    /// Escaping makes a three-byte start code impossible inside a payload, so
    /// this is exact rather than approximate.
    fn units_of(stream: &[u8], kind: u8) -> usize {
        stream
            .windows(4)
            .filter(|window| window[..3] == [0, 0, 1] && window[3] & 0x1F == kind)
            .count()
    }

    /// **Gate A item 3.** A bitrate change reconfigures the encoder live: no
    /// keyframe crosses it, and nothing is reinitialised.
    ///
    /// The congestion controller moves the rate many times a minute. A
    /// backend that answered a rate change with a refresh would put a
    /// bitrate spike and a visible stutter in the stream every time the
    /// network hiccuped, which is the opposite of what the change is for.
    ///
    /// **Counted at the encoder, not inferred.** Both the flag each backend
    /// reports and the start codes in the bitstream are counted, because a
    /// backend could report the flag correctly and still code a refresh.
    ///
    /// Needs both drivers, so it is off by default. Run with
    /// `cargo test -p lowlat-encode -- --ignored no_keyframe`.
    #[test]
    #[ignore = "requires both vendors' drivers"]
    fn no_keyframe_crosses_a_bitrate_change() {
        const WIDTH: u32 = 1920;
        const HEIGHT: u32 = 1080;
        const FRAMES: usize = 120;
        const DEPTH: usize = 4;

        // A rate that moves on every picture, across the whole range the
        // controller can reach. A single change could be answered correctly by
        // accident; a hundred cannot.
        let rate_for = |index: usize| -> u32 {
            let step = u32::try_from(index % 20).unwrap_or(0);
            2_000_000 + step * 2_000_000
        };

        let params = h264::Params {
            width: WIDTH,
            height: HEIGHT,
            fps: 60,
            level_idc: 42,
            log2_max_frame_num_minus4: 4,
            log2_max_poc_lsb_minus4: 4,
            max_num_ref_frames: 1,
        };

        let cuda = cuda::Cuda::load().expect("compute runtime");
        let device = cuda.any_device().expect("a device");
        let compute = cuda.retain_primary(&device).expect("context");
        let api = nvenc::Api::load().expect("encoder runtime");
        let session = api.open_session(compute).expect("session");
        let mut vendor = session
            .initialize(
                &cuda,
                nvenc::Config {
                    codec: nvenc::Codec::H264,
                    width: WIDTH,
                    height: HEIGHT,
                    fps: 60,
                    bitrate_bps: 20_000_000,
                    min_qp: nvenc::DEFAULT_MIN_QP,
                },
            )
            .expect("initialize");
        let (vendor_stream, vendor_keyframes) = encode_run(
            &mut vendor,
            WIDTH,
            HEIGHT,
            FRAMES,
            DEPTH,
            |encoder, index, _| {
                // Through the trait explicitly: a backend may carry an
                // inherent method of the same name, and the one under test is
                // the one the loop calls.
                Encoder::reconfigure(encoder, rate_for(index)).expect("reconfigure");
            },
        );

        let va = vaapi::Vaapi::load().expect("display runtime");
        let display = va.open(c"/dev/dri/renderD128").expect("render node");
        let caps = display.caps(vaapi::Codec::H264).expect("caps");
        let context = display
            .create_context(caps, WIDTH, HEIGHT, DEPTH)
            .expect("context");
        let mut open = context.encoder(params, 20_000_000).expect("encoder");
        let (open_stream, open_keyframes) = encode_run(
            &mut open,
            WIDTH,
            HEIGHT,
            FRAMES,
            DEPTH,
            |encoder, index, _| {
                // Through the trait explicitly: a backend may carry an
                // inherent method of the same name, and the one under test is
                // the one the loop calls.
                Encoder::reconfigure(encoder, rate_for(index)).expect("reconfigure");
            },
        );

        for (name, stream, reported) in [
            ("vendor", &vendor_stream, vendor_keyframes),
            ("open", &open_stream, open_keyframes),
        ] {
            let coded = units_of(stream, 5);
            let pictures = coded + units_of(stream, 1);
            println!(
                "{name}: {} bytes, {pictures} pictures, {coded} coded refreshes, {reported} reported",
                stream.len()
            );
            assert_eq!(pictures, FRAMES, "{name} produced the wrong picture count");
            // **One, and it is the forced first picture.** Any refresh after
            // that one crossed a rate change.
            assert_eq!(
                coded,
                1,
                "{name} coded {coded} refreshes across {} rate changes",
                FRAMES - 1
            );
            assert_eq!(
                reported,
                1,
                "{name} reported {reported} keyframes across {} rate changes",
                FRAMES - 1
            );
        }
    }

    /// What the refresh actually costs, at each quantiser floor.
    ///
    /// **This is the standing measurement behind a burst that was reported and
    /// never existed.** The refresh was believed to cost megabytes, which at
    /// sixty frames a second would be a burst of roughly two thousand packets
    /// in one frame interval and would exceed the delivery gate's own ceiling
    /// at ordinary rates. It was a length reported by a collect that raced the
    /// driver: the picture was six hundred and fifty-one bytes all along.
    /// Measured here so the claim is a number rather than a memory.
    ///
    /// Needs the vendor driver. Run with
    /// `cargo test -p lowlat-encode --release -- --ignored the_refresh`.
    #[test]
    #[ignore = "requires the vendor driver"]
    fn the_refresh_cost_under_each_bound() {
        const WIDTH: u32 = 1920;
        const HEIGHT: u32 = 1080;
        const FRAMES: usize = 12;
        // Uncompressed, for scale: a picture that approaches this is not being
        // compressed in any useful sense.
        let raw = WIDTH as usize * HEIGHT as usize * 3 / 2;

        // **A ceiling and an initial quantiser were swept here too and moved
        // nothing**: the refresh stayed 650 bytes at quantiser 8 whatever
        // either was set to. They are not configured and this no longer sweeps
        // them; the floor is the only one of the three that changes anything.
        let cases: [(&str, u32); 3] = [("floor 5", 5), ("floor 10", 10), ("floor 22", 22)];

        println!("raw 4:2:0 frame is {raw} bytes");
        for (name, min_qp) in cases {
            let cuda = cuda::Cuda::load().expect("compute runtime");
            let device = cuda.any_device().expect("a device");
            let compute = cuda.retain_primary(&device).expect("context");
            let api = nvenc::Api::load().expect("encoder runtime");
            let session = api.open_session(compute).expect("session");
            let mut encoder = session
                .initialize(
                    &cuda,
                    nvenc::Config {
                        codec: nvenc::Codec::H264,
                        width: WIDTH,
                        height: HEIGHT,
                        fps: 60,
                        bitrate_bps: 20_000_000,
                        min_qp,
                    },
                )
                .expect("initialize");

            let mut first = 0usize;
            let mut rest = 0usize;
            let mut qp_first = 0u32;
            let (_stream, _keyframes) = encode_run(
                &mut encoder,
                WIDTH,
                HEIGHT,
                FRAMES,
                4,
                |encoder, index, unit| {
                    if index == 0 {
                        first = unit.len();
                        qp_first = encoder.last_lock().map_or(0, |r| r.frame_avg_qp);
                    } else {
                        rest += unit.len();
                    }
                },
            );
            println!(
                "  {name:<24} refresh {first:>8} bytes ({:>5.1}% of raw, qp {qp_first:>2}), \
                 predicted mean {:>6} bytes",
                100.0 * first as f64 / raw as f64,
                rest / (FRAMES - 1)
            );
        }
    }

    /// Both hardware backends, one generic loop, one source.
    ///
    /// Needs both drivers, so it is off by default. Run with
    /// `cargo test -p lowlat-encode -- --ignored both_backends`.
    #[test]
    #[ignore = "requires both vendors' drivers"]
    fn both_backends_encode_the_same_source_through_one_loop() {
        const WIDTH: u32 = 1920;
        const HEIGHT: u32 = 1080;
        const FRAMES: usize = 8;
        const DEPTH: usize = 4;

        let params = h264::Params {
            width: WIDTH,
            height: HEIGHT,
            fps: 60,
            level_idc: 42,
            log2_max_frame_num_minus4: 4,
            log2_max_poc_lsb_minus4: 4,
            max_num_ref_frames: 1,
        };

        let cuda = cuda::Cuda::load().expect("compute runtime");
        let device = cuda.any_device().expect("a device");
        let compute = cuda.retain_primary(&device).expect("context");
        let api = nvenc::Api::load().expect("encoder runtime");
        let session = api.open_session(compute).expect("session");
        let mut vendor = session
            .initialize(
                &cuda,
                nvenc::Config {
                    codec: nvenc::Codec::H264,
                    width: WIDTH,
                    height: HEIGHT,
                    fps: 60,
                    bitrate_bps: 20_000_000,
                    min_qp: nvenc::DEFAULT_MIN_QP,
                },
            )
            .expect("initialize");
        println!("vendor collect blocks:");
        let (vendor_stream, vendor_keyframes) = encode_run(
            &mut vendor,
            WIDTH,
            HEIGHT,
            FRAMES,
            DEPTH,
            |encoder, index, unit| {
                let report = encoder.last_lock().expect("a collect reported nothing");
                audit_collect(index, unit, report);
            },
        );

        let va = vaapi::Vaapi::load().expect("display runtime");
        let display = va.open(c"/dev/dri/renderD128").expect("render node");
        let caps = display.caps(vaapi::Codec::H264).expect("caps");
        let context = display
            .create_context(caps, WIDTH, HEIGHT, DEPTH)
            .expect("context");
        let mut open = context.encoder(params, 20_000_000).expect("encoder");
        let (open_stream, open_keyframes) =
            encode_run(&mut open, WIDTH, HEIGHT, FRAMES, DEPTH, |_, _, _| {});

        for (name, stream, keyframes) in [
            ("vendor", &vendor_stream, vendor_keyframes),
            ("open", &open_stream, open_keyframes),
        ] {
            println!(
                "{name}: {} bytes, {} slices, {} keyframes reported",
                stream.len(),
                units_of(stream, 5) + units_of(stream, 1),
                keyframes
            );
            assert_eq!(
                units_of(stream, 5) + units_of(stream, 1),
                FRAMES,
                "{name} produced the wrong number of pictures"
            );
            assert_eq!(
                stream[4] & 0x1F,
                7,
                "{name} does not open with a sequence set"
            );
            assert!(keyframes >= 1, "{name} reported no keyframe at all");

            // Written where the frame checker can be pointed at it, which is
            // what turns "it encoded" into "it encoded what it was given".
            let path = format!("/tmp/lowlat-{name}.h264");
            std::fs::write(&path, stream).expect("write");
            println!("  wrote {path}");
        }

        // The forced picture is the first one either way. Beyond that the two
        // differ honestly: one still refreshes every picture and says so.
        // **Both honour the absence of a request now**, which is what makes
        // the reconfigure gate statable at all: a backend refreshing every
        // picture would satisfy any keyframe assertion trivially.
        assert_eq!(
            (vendor_keyframes, open_keyframes),
            (1, 1),
            "a backend that honours the absence of a request emitted extras"
        );
    }
}
