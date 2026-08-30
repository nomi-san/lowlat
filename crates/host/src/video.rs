//! Turning an encoded access unit into a message on the video channel.
//!
//! Video is not a control message. It rides the ordinary message framing on
//! its own channel with a ten-byte header ahead of the bitstream; see
//! docs/01-protocol.md section 11.3.
//!
//! **Almost everything here is per session rather than per frame.** The
//! dimensions, the rotation and the generation counter are fixed for a stream
//! and only the keyframe flag moves, which is why this is a value that
//! outlives a frame rather than a function that takes six arguments.

use lowlat_core::message::Message;
pub use lowlat_core::video::Rotation;
use lowlat_core::video::{VIDEO_HEADER_LEN, VideoHeader, encode};

/// One stream's fixed facts, and the header buffer they are written into.
#[derive(Debug)]
pub struct Packetiser {
    header: VideoHeader,
    /// Written once per frame and lent to the message, so the header costs no
    /// allocation and no per-frame arithmetic beyond the flags.
    bytes: [u8; VIDEO_HEADER_LEN],
}

impl Packetiser {
    /// Say what depth the pictures now are.
    ///
    /// **Called when the encoder is rebuilt, which is the only time it can
    /// change.** The header is written once here and lent to every frame, so
    /// a stale value would describe every picture until the next rebuild.
    pub fn set_ten_bit(&mut self, ten_bit: bool) {
        // The bytes are rewritten from the header on the next frame, so
        // recording it here is the whole of the change.
        self.header.ten_bit = ten_bit;
    }

    /// Begin a stream.
    ///
    /// **The rotation is one-based**, so an unrotated display is
    /// [`Rotation::None`] and never [`Rotation::Unknown`]. Emitting zero says
    /// the orientation is unknown, which is a different claim.
    pub fn new(width: u16, height: u16, rotation: Rotation, ten_bit: bool) -> Self {
        Self {
            header: VideoHeader {
                // **A generation, not a frame counter.** It stays constant for
                // the life of a configuration, so a peer that ordered or
                // deduplicated frames by it would see one value forever.
                frame_id: 1,
                width,
                height,
                rotation,
                // **What the pictures really are, and it must be exactly
                // that.** The bit next to the rotation names ten-bit colour
                // and a receiver builds its decoder for the depth it names
                // before any bitstream is parsed, so a bit disagreeing with
                // the stream fails every picture whichever way it disagrees:
                // clear over ten-bit pictures and set over eight-bit ones are
                // the same fault. It is told rather than decided here because
                // the encoder settled the depth.
                ten_bit,
                // Not set: this stream is a desktop, not a fullscreen capture
                // of one application, and the flag is the peer's cue to change
                // how it presents.
                fullscreen: false,
            },
            bytes: [0; VIDEO_HEADER_LEN],
        }
    }

    /// The generation a peer is currently seeing.
    pub fn generation(&self) -> u32 {
        self.header.frame_id
    }

    /// Note that the encoder was reconfigured.
    ///
    /// **Only a reconfiguration moves this**, which is what makes it mean
    /// anything to a peer. A bitrate change is not one: it neither reinitialises
    /// the encoder nor changes what a decoder must do, so it leaves the
    /// generation alone.
    pub fn reconfigured(&mut self) {
        self.header.frame_id = self.header.frame_id.wrapping_add(1);
    }

    /// The header for one coded access unit, ready to precede it on the wire.
    ///
    /// **Nothing in the header says which pictures are keyframes, and nothing
    /// should.** The bit that was read as a keyframe marker names the colour
    /// depth, so setting it on a keyframe tells a receiver the stream is ten
    /// bit; one decoder family then builds itself for ten bit and fails every
    /// picture, and hardware that cannot decode ten bit at all fails at the
    /// first submission. A receiver classifies keyframes from the bitstream,
    /// which is what ours does and what every recorded host requires.
    ///
    /// The argument is kept so callers read as they did; it names the picture
    /// for the caller's sake and changes nothing in the bytes.
    pub fn header(&mut self, _keyframe: bool) -> Option<&[u8]> {
        encode(&mut self.bytes, &self.header).ok()?;
        Some(&self.bytes)
    }

    /// Frame a coded access unit for sending.
    ///
    /// The returned message borrows both the header written here and the
    /// caller's bitstream, so it must be enqueued before the next frame is
    /// framed.
    pub fn frame<'a>(&'a mut self, bitstream: &'a [u8], keyframe: bool) -> Option<Message<'a>> {
        let header = self.header(keyframe)?;
        Message::new(header, bitstream).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lowlat_core::message::{LENGTH_PREFIX_LEN, parse_length_prefix};
    use lowlat_core::video::{VideoHeader, parse};

    /// The body capacity of a default datagram, which is what the corpus was
    /// recorded at.
    const BODY: usize = 1193;

    fn coded(len: usize) -> Vec<u8> {
        let mut unit = vec![0u8; len];
        // A parameter set, so the classifier has something real to find. Only
        // where it fits: the sizing cases below go down to a single byte.
        if let Some(head) = unit.get_mut(..5) {
            head.copy_from_slice(&[0, 0, 0, 1, 0x67]);
        }
        unit
    }

    /// Read the header back the way a peer does: out of the first fragment's
    /// body, past the length prefix.
    ///
    /// **Not out of the value that wrote it.** Checking a header against the
    /// structure it came from proves the two agree and nothing else; this goes
    /// through the framing, which is where docs/01-protocol.md section 11.3
    /// fixes the absolute offsets.
    fn header_as_a_peer_sees_it(message: &Message<'_>) -> VideoHeader {
        let mut body = [0u8; BODY];
        let first = message
            .fragment(0, BODY, &mut body)
            .expect("a first fragment")
            .expect("written");
        let declared = parse_length_prefix(&body[..first.len]).expect("prefix");
        assert_eq!(declared, message.total_len(), "the prefix disagrees");
        parse(&body[LENGTH_PREFIX_LEN..first.len]).expect("header")
    }

    #[test]
    fn the_header_round_trips_through_the_parser_a_peer_would_use() {
        let mut packetiser = Packetiser::new(1920, 1080, Rotation::None, false);
        let unit = coded(64);
        let message = packetiser.frame(&unit, true).expect("framed");

        let header = header_as_a_peer_sees_it(&message);
        assert_eq!(header.width, 1920);
        assert_eq!(header.height, 1080);
        assert_eq!(header.rotation, Rotation::None);
        // It names the colour depth, and this stream is eight-bit.
        assert!(
            !header.ten_bit,
            "an eight-bit stream claimed ten-bit colour"
        );
        assert!(!header.fullscreen);
        assert_eq!(header.frame_id, 1);
    }

    /// **The depth bit describes the pictures, in both directions.**
    ///
    /// A receiver builds its decoder from this before it parses any bitstream,
    /// so the two ways of getting it wrong cost the same thing: clear over
    /// ten-bit pictures and set over eight-bit ones both produce a decoder
    /// built for the wrong depth, which fails every picture and reports it as
    /// a decode error rather than as a mismatch. An earlier version of this
    /// host set the bit on keyframes, believing it meant something else, and
    /// one decoder family failed every frame for it.
    #[test]
    fn the_depth_bit_says_what_the_stream_is() {
        for ten_bit in [false, true] {
            let mut packetiser = Packetiser::new(1920, 1080, Rotation::None, ten_bit);
            let unit = coded(32);
            let message = packetiser.frame(&unit, false).expect("framed");
            let header = header_as_a_peer_sees_it(&message);
            assert_eq!(
                header.ten_bit, ten_bit,
                "a ten_bit={ten_bit} stream described itself as {}",
                header.ten_bit
            );
        }
    }

    /// **A rebuilt encoder changes the depth of every frame after it.**
    ///
    /// The header is written from one struct and lent to each frame, so a
    /// depth recorded once and never updated would describe every picture
    /// until the next rebuild -- and describe them wrongly, in the one field a
    /// receiver reads before it parses any bitstream.
    #[test]
    fn a_rebuild_changes_the_depth_of_later_frames() {
        let mut packetiser = Packetiser::new(1920, 1080, Rotation::None, false);
        let unit = coded(32);

        let first = packetiser.frame(&unit, false).expect("framed");
        assert!(
            !header_as_a_peer_sees_it(&first).ten_bit,
            "an eight-bit stream described itself as ten"
        );

        packetiser.set_ten_bit(true);
        packetiser.reconfigured();
        let second = packetiser.frame(&unit, false).expect("framed");
        assert!(
            header_as_a_peer_sees_it(&second).ten_bit,
            "the frames after a rebuild still describe the old depth"
        );
    }

    /// **Upright is one, not zero.** A stream that emitted zero would be
    /// telling every peer its orientation is unknown.
    #[test]
    fn an_unrotated_stream_says_upright_rather_than_unknown() {
        let mut packetiser = Packetiser::new(1920, 1080, Rotation::None, false);
        let unit = coded(32);
        let message = packetiser.frame(&unit, false).expect("framed");
        let header = header_as_a_peer_sees_it(&message);
        assert_eq!(header.rotation, Rotation::None);
        assert_ne!(
            header.rotation,
            Rotation::Unknown,
            "an upright display was reported as unknown"
        );
    }

    /// The generation is not a frame counter, and the strongest way to say so
    /// is that framing many frames does not move it.
    #[test]
    fn the_generation_holds_across_frames_and_moves_only_on_reconfiguration() {
        let mut packetiser = Packetiser::new(1280, 720, Rotation::None, false);
        let unit = coded(48);
        for _ in 0..50 {
            let message = packetiser.frame(&unit, false).expect("framed");
            assert_eq!(header_as_a_peer_sees_it(&message).frame_id, 1);
        }
        packetiser.reconfigured();
        let message = packetiser.frame(&unit, false).expect("framed");
        assert_eq!(header_as_a_peer_sees_it(&message).frame_id, 2);
    }

    /// A quarter turn swaps what a peer maps pointer coordinates against,
    /// while the coded buffer stays landscape.
    #[test]
    fn a_quarter_turn_swaps_the_display_dimensions_and_not_the_coded_ones() {
        let mut packetiser = Packetiser::new(1920, 1080, Rotation::Deg90, false);
        let unit = coded(32);
        let message = packetiser.frame(&unit, false).expect("framed");
        let header = header_as_a_peer_sees_it(&message);
        assert_eq!((header.width, header.height), (1920, 1080));
        assert_eq!(header.display_dimensions(), (1080, 1920));
    }

    /// The framing arithmetic a peer's reassembler depends on: the length
    /// prefix counts the header and the bitstream, and the fragment count
    /// follows from it.
    #[test]
    fn the_length_prefix_covers_the_header_and_the_bitstream() {
        let mut packetiser = Packetiser::new(1920, 1080, Rotation::None, false);
        for len in [1usize, 100, 1179, 1180, 1181, 5000, 60000] {
            let unit = coded(len);
            let message = packetiser.frame(&unit, false).expect("framed");
            assert_eq!(
                message.total_len() as usize,
                VIDEO_HEADER_LEN + len,
                "the prefix does not cover both parts"
            );
            // The first fragment also carries the four-byte prefix, so the
            // boundary is where that pushes the payload past a body.
            let expected = (VIDEO_HEADER_LEN + len + LENGTH_PREFIX_LEN).div_ceil(BODY);
            assert_eq!(message.fragment_count(BODY), expected, "at {len} bytes");
        }
    }
}
