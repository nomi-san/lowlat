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
//!
//! **A peer that declared the video protocol is sent a keyframe-metadata
//! message before every keyframe, and the keyframe carries the announced
//! bit.** The message says whether the encoder was rebuilt, which is what
//! such a peer tears its decoder down on; a peer that did not declare it
//! rebuilds on the parameter sets instead, so it is sent neither.

use lowlat_core::message::Message;
pub use lowlat_core::video::{Codec, Rotation};
use lowlat_core::video::{
    KeyframeMetadata, METADATA_LEN, VIDEO_HEADER_LEN, VideoHeader, encode, encode_metadata,
};

/// One stream's fixed facts, and the header buffer they are written into.
#[derive(Debug)]
pub struct Packetiser {
    header: VideoHeader,
    /// Written once per frame and lent to the message, so the header costs no
    /// allocation and no per-frame arithmetic beyond the flags.
    bytes: [u8; VIDEO_HEADER_LEN],
    /// The metadata message, written once per announced keyframe.
    announcement: [u8; METADATA_LEN],
    /// What the metadata says beyond the header.
    chroma_444: bool,
    /// Whether this guest declared the video protocol. Nothing is announced
    /// and the announced bit is never set until it has.
    announces: bool,
    /// True from construction and from every reconfiguration until a keyframe
    /// has gone out: that keyframe's parameter sets are new to the peer, and
    /// its announcement says so.
    rebuilt: bool,
}

impl Packetiser {
    /// Say what the pictures now are.
    ///
    /// **Called when the encoder is rebuilt, and when a guest takes its seat
    /// on a stream that already runs.** The header is written once here and
    /// lent to every frame, so a stale value would describe every picture
    /// until the next rebuild -- and the depth is the one field a receiver
    /// acts on before parsing any bitstream.
    pub fn set_colour(&mut self, codec: Codec, ten_bit: bool, chroma_444: bool) {
        // The bytes are rewritten from the header on the next frame, so
        // recording it here is the whole of the change.
        self.header.codec = codec;
        self.header.ten_bit = ten_bit;
        self.chroma_444 = chroma_444;
    }

    /// Whether this guest reads the video protocol, from its initialization.
    pub fn set_announces(&mut self, announces: bool) {
        self.announces = announces;
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
                // Until the stream says otherwise, in the same call that
                // settles the depth.
                codec: Codec::H264,
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
                // Not set: this host has no lock state to report, and clear
                // reads as unlocked.
                locked: false,
                // Per frame, below.
                announced: false,
                metadata: false,
            },
            bytes: [0; VIDEO_HEADER_LEN],
            announcement: [0; METADATA_LEN],
            chroma_444: false,
            announces: false,
            rebuilt: true,
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
        self.rebuilt = true;
    }

    /// The metadata message that precedes a keyframe, for a guest that reads
    /// the protocol; `None` for any other picture or any other guest.
    ///
    /// **Sent whole and first.** It says whether the encoder was rebuilt since
    /// the peer's last keyframe, which is what the peer tears its decoder down
    /// on, so the picture must not go out ahead of it.
    pub fn announcement(&mut self, keyframe: bool) -> Option<&[u8]> {
        if !(self.announces && keyframe) {
            return None;
        }
        let metadata = KeyframeMetadata {
            rebuilt: self.rebuilt,
            keyframe: true,
            ten_bit: self.header.ten_bit,
            chroma_444: self.chroma_444,
            rotation: self.header.rotation,
        };
        encode_metadata(&mut self.announcement, &self.header, &metadata).ok()?;
        Some(&self.announcement)
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
    /// What a keyframe does carry, for a guest that reads the protocol, is
    /// the announced bit: this picture had a metadata message ahead of it.
    pub fn header(&mut self, keyframe: bool) -> Option<&[u8]> {
        self.header.announced = self.announces && keyframe;
        encode(&mut self.bytes, &self.header).ok()?;
        Some(&self.bytes)
    }

    /// Note that a framed picture went out.
    ///
    /// **After the send, not before it.** A keyframe that was refused never
    /// reached the peer, so the next one still has to say the encoder was
    /// rebuilt; clearing the latch on framing alone would say it once to
    /// nobody.
    pub fn sent(&mut self, keyframe: bool) {
        if keyframe {
            self.rebuilt = false;
        }
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
        assert!(!header.locked && !header.announced && !header.metadata);
        assert_eq!(header.codec, Codec::H264);
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

        packetiser.set_colour(Codec::H264, true, false);
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

    /// **A guest that did not declare the protocol is sent the older framing
    /// exactly**: no metadata message, and no announced bit on anything.
    /// *Named regression test.*
    #[test]
    fn a_guest_that_did_not_declare_the_protocol_gets_no_announcement_and_no_bit() {
        let mut packetiser = Packetiser::new(1920, 1080, Rotation::None, false);
        let unit = coded(64);
        for keyframe in [true, false, true] {
            assert!(packetiser.announcement(keyframe).is_none());
            let message = packetiser.frame(&unit, keyframe).expect("framed");
            let header = header_as_a_peer_sees_it(&message);
            assert!(!header.announced && !header.metadata);
            packetiser.sent(keyframe);
        }
    }

    /// **A guest that declared it is sent the pair, and only on keyframes.**
    /// The message goes ahead of the keyframe with the rebuilt bit set until a
    /// keyframe has gone out, and the keyframe carries the announced bit; a
    /// predicted picture carries neither.
    #[test]
    fn a_declared_guest_is_announced_every_keyframe_and_told_of_a_rebuild_once() {
        let mut packetiser = Packetiser::new(1920, 1080, Rotation::Deg90, false);
        packetiser.set_announces(true);
        packetiser.set_colour(Codec::H265, true, false);
        let unit = coded(64);

        // A predicted picture: nothing.
        assert!(packetiser.announcement(false).is_none());
        let predicted = packetiser.frame(&unit, false).expect("framed");
        assert!(!header_as_a_peer_sees_it(&predicted).announced);
        packetiser.sent(false);

        // The first keyframe says the encoder is new to this peer.
        let first = packetiser.announcement(true).expect("announced").to_vec();
        assert_eq!(first.len(), METADATA_LEN);
        let opened = parse(&first).expect("header");
        assert!(opened.metadata && opened.announced);
        assert_eq!(opened.codec, Codec::H265);
        assert!(opened.ten_bit);
        assert_eq!(opened.frame_id, 1);
        let told = lowlat_core::video::parse_metadata(&first).expect("metadata");
        assert!(told.rebuilt && told.keyframe && told.ten_bit && !told.chroma_444);
        assert_eq!(told.rotation, Rotation::Deg90);
        let keyframe = packetiser.frame(&unit, true).expect("framed");
        let header = header_as_a_peer_sees_it(&keyframe);
        assert!(header.announced && !header.metadata);
        assert_eq!(header.codec, Codec::H265);
        packetiser.sent(true);

        // The next keyframe is announced again, with nothing rebuilt.
        let again = packetiser.announcement(true).expect("announced").to_vec();
        assert!(
            !lowlat_core::video::parse_metadata(&again)
                .expect("metadata")
                .rebuilt
        );
        packetiser.sent(true);

        // A reconfiguration arms it again, and a refused keyframe does not
        // spend it: only a keyframe that went out does.
        packetiser.reconfigured();
        let rebuilt = packetiser.announcement(true).expect("announced").to_vec();
        assert!(
            lowlat_core::video::parse_metadata(&rebuilt)
                .expect("metadata")
                .rebuilt
        );
        assert_eq!(parse(&rebuilt).expect("header").frame_id, 2);
        // Not sent: the transport refused the picture.
        let still = packetiser.announcement(true).expect("announced").to_vec();
        assert!(
            lowlat_core::video::parse_metadata(&still)
                .expect("metadata")
                .rebuilt
        );
        packetiser.sent(true);
        let spent = packetiser.announcement(true).expect("announced").to_vec();
        assert!(
            !lowlat_core::video::parse_metadata(&spent)
                .expect("metadata")
                .rebuilt
        );
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
