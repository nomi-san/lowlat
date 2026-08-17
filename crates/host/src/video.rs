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
use lowlat_core::video::{Rotation, VIDEO_HEADER_LEN, VideoHeader, encode};

/// One stream's fixed facts, and the header buffer they are written into.
#[derive(Debug)]
pub struct Packetiser {
    header: VideoHeader,
    /// Written once per frame and lent to the message, so the header costs no
    /// allocation and no per-frame arithmetic beyond the flags.
    bytes: [u8; VIDEO_HEADER_LEN],
}

impl Packetiser {
    /// Begin a stream.
    ///
    /// **The rotation is one-based**, so an unrotated display is
    /// [`Rotation::None`] and never [`Rotation::Unknown`]. Emitting zero says
    /// the orientation is unknown, which is a different claim.
    pub fn new(width: u16, height: u16, rotation: Rotation) -> Self {
        Self {
            header: VideoHeader {
                // **A generation, not a frame counter.** It stays constant for
                // the life of a configuration, so a peer that ordered or
                // deduplicated frames by it would see one value forever.
                frame_id: 1,
                width,
                height,
                rotation,
                keyframe: false,
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

    /// Frame a coded access unit for sending.
    ///
    /// The returned message borrows both the header written here and the
    /// caller's bitstream, so it must be enqueued before the next frame is
    /// framed.
    ///
    /// **The keyframe flag is set when it is true.** Peers leave it clear on
    /// every frame including keyframes, so a receiver cannot rely on it and
    /// ours sniffs the bitstream instead. Setting it is still right: it is
    /// strictly more informative and costs nothing, and a peer that does read
    /// it is better served.
    pub fn frame<'a>(&'a mut self, bitstream: &'a [u8], keyframe: bool) -> Option<Message<'a>> {
        self.header.keyframe = keyframe;
        encode(&mut self.bytes, &self.header).ok()?;
        Message::new(&self.bytes, bitstream).ok()
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
        let mut packetiser = Packetiser::new(1920, 1080, Rotation::None);
        let unit = coded(64);
        let message = packetiser.frame(&unit, true).expect("framed");

        let header = header_as_a_peer_sees_it(&message);
        assert_eq!(header.width, 1920);
        assert_eq!(header.height, 1080);
        assert_eq!(header.rotation, Rotation::None);
        assert!(header.keyframe, "the flag we do set was not set");
        assert!(!header.fullscreen);
        assert_eq!(header.frame_id, 1);
    }

    /// **Upright is one, not zero.** A stream that emitted zero would be
    /// telling every peer its orientation is unknown.
    #[test]
    fn an_unrotated_stream_says_upright_rather_than_unknown() {
        let mut packetiser = Packetiser::new(1920, 1080, Rotation::None);
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
        let mut packetiser = Packetiser::new(1280, 720, Rotation::None);
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
        let mut packetiser = Packetiser::new(1920, 1080, Rotation::Deg90);
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
        let mut packetiser = Packetiser::new(1920, 1080, Rotation::None);
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
