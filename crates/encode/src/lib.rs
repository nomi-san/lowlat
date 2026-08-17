//! Video encoders. Submit and poll are separate operations so the trait
//! cannot express a serialized capture-to-encode loop.
//!
//! See docs/05-host.md section 4.

// Phase 5 lands the hardware backend; Phase 11 the software one.

pub mod bitstream;
pub mod cuda;
mod ffi;
pub mod h264;
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
    ) -> (Vec<u8>, usize) {
        let mut source = lowlat_capture::synthetic::Synthetic::new(width, height);
        let mut stream = Vec::new();
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
                    stream.extend_from_slice(bitstream);
                    if keyframe {
                        keyframes += 1;
                    }
                    collected += 1;
                }
                Poll::Pending => std::hint::spin_loop(),
            }
        }
        (stream, keyframes)
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
        let (vendor_stream, vendor_keyframes) =
            encode_run(&mut vendor, WIDTH, HEIGHT, FRAMES, DEPTH);

        let va = vaapi::Vaapi::load().expect("display runtime");
        let display = va.open(c"/dev/dri/renderD128").expect("render node");
        let caps = display.caps(vaapi::Codec::H264).expect("caps");
        let context = display
            .create_context(caps, WIDTH, HEIGHT, DEPTH)
            .expect("context");
        let mut open = context.encoder(params, 20_000_000).expect("encoder");
        let (open_stream, open_keyframes) = encode_run(&mut open, WIDTH, HEIGHT, FRAMES, DEPTH);

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
