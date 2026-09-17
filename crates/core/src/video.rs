//! Video message framing (docs/01-protocol.md 11.3).
//!
//! Ten bytes ahead of the bitstream, inside the ordinary message framing, so
//! every offset here is relative to the message **content**: what follows the
//! four-byte length prefix.
//!
//! ```text
//! 0  4  frame identifier, little endian
//! 4  2  width, little endian
//! 6  2  height, little endian
//! 8  1  codec, 1 for H.264 and 2 for HEVC
//! 9  1  flags
//! ```
//!
//! The endianness flips relative to the rest of the protocol, where sequences
//! and lengths are big endian. Getting it backwards yields a plausible frame
//! with absurd dimensions.
//!
//! A message with bit 6 of the flags set is keyframe metadata rather than a
//! picture: eleven more bytes after the header, and the keyframe it describes
//! follows as the next message. A peer that declared the video protocol in
//! its initialization is sent one before every keyframe, and the keyframe
//! itself carries bit 5; a peer that did not is sent neither.

use crate::error::{Error, Result};

/// Bytes of header ahead of the bitstream.
pub const VIDEO_HEADER_LEN: usize = 10;
/// The whole of a keyframe-metadata message: the header and its body.
pub const METADATA_LEN: usize = 21;

const ROTATION_MASK: u8 = 0x07;
/// **Ten-bit colour, and it is not a keyframe marker.**
///
/// A receiver builds its decoder for the depth this bit names, before any
/// bitstream is parsed. Setting it on an eight-bit stream makes one decoder
/// family initialise for ten-bit and fail every picture, which is reported as
/// a decode error rather than as a mismatch, and hardware that cannot decode
/// ten-bit at all fails at the first submission.
///
/// It was read as a keyframe flag here until a peer was found refusing our
/// stream over it. The recording said so all along: a host leaves it clear on
/// every message including its own keyframes, which is inexplicable for a
/// keyframe marker and exactly right for a depth that stream does not use.
const FLAG_TEN_BIT: u8 = 0x08;
/// The host's session is locked, or is not the one at its console. A receiver
/// hands it to its application; nothing else turns on it. Never set by us.
const FLAG_LOCKED: u8 = 0x10;
/// This picture was announced by a metadata message. A receiver with a
/// decoder feeds the picture to it whatever it is led by, and does not apply
/// its generation rule; a receiver with none builds one from it.
const FLAG_ANNOUNCED: u8 = 0x20;
/// This message is keyframe metadata, not a picture.
const FLAG_METADATA: u8 = 0x40;
/// The word that opens the metadata body. Not read by any receiver.
const METADATA_LEAD: u32 = 1;
/// Bits of the metadata word.
const META_REBUILT: u32 = 0x01;
const META_KEYFRAME: u32 = 0x02;
/// The chroma byte of the metadata body says 4:2:0 as 2 and 4:4:4 as 0.
const CHROMA_420: u8 = 2;
const CHROMA_444: u8 = 0;

/// Display orientation.
///
/// **One-based.** Zero means unspecified, not upright. A host emitting `0` for
/// an unrotated display is saying "unknown", and a reader treating `1` as
/// rotated is off by one, because a quarter turn is `2`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Rotation {
    Unknown = 0,
    None = 1,
    Deg90 = 2,
    Deg180 = 3,
    Deg270 = 4,
}

impl Rotation {
    pub const fn from_bits(bits: u8) -> Self {
        match bits & ROTATION_MASK {
            1 => Rotation::None,
            2 => Rotation::Deg90,
            3 => Rotation::Deg180,
            4 => Rotation::Deg270,
            _ => Rotation::Unknown,
        }
    }

    /// True for a quarter turn, where displayed width and height swap.
    pub const fn quarter_turn(self) -> bool {
        matches!(self, Rotation::Deg90 | Rotation::Deg270)
    }
}

/// Which bitstream the payload carries. Needed to classify a keyframe, since
/// the unit header differs between them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    H264,
    H265,
}

impl Codec {
    const fn wire(self) -> u8 {
        match self {
            Codec::H264 => 1,
            Codec::H265 => 2,
        }
    }

    /// Older hosts write `1` whatever they code, so anything but `2` is the
    /// first codec, and a receiver classifies from the bitstream regardless.
    const fn from_wire(byte: u8) -> Self {
        if byte == 2 { Codec::H265 } else { Codec::H264 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoHeader {
    /// Encoder **generation** counter, not a frame counter. It stays constant
    /// across a session's frames and increments only on reconfiguration, so
    /// anything ordering or deduplicating frames by it is broken.
    pub frame_id: u32,
    pub width: u16,
    pub height: u16,
    pub codec: Codec,
    pub rotation: Rotation,
    /// **Ten-bit colour.** A receiver builds its decoder for this depth before
    /// parsing any bitstream, so it must describe the stream and nothing else.
    pub ten_bit: bool,
    /// The host's session is locked.
    pub locked: bool,
    /// Announced by a metadata message; see [`FLAG_ANNOUNCED`].
    pub announced: bool,
    /// Keyframe metadata rather than a picture; see [`parse_metadata`].
    pub metadata: bool,
}

/// What a keyframe-metadata message says about the keyframe after it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyframeMetadata {
    /// The encoder was rebuilt: new parameter sets and a new reference chain.
    /// A receiver tears its decoder down on this, before the keyframe.
    pub rebuilt: bool,
    /// A keyframe follows. What a receiver behind on the channel looks ahead
    /// for.
    pub keyframe: bool,
    pub ten_bit: bool,
    pub chroma_444: bool,
    pub rotation: Rotation,
}

impl VideoHeader {
    /// Display dimensions, accounting for rotation.
    ///
    /// The encoded buffer is always in the encoder's landscape size; a quarter
    /// turn means the desktop is portrait and the dimensions a peer maps
    /// pointer coordinates against are the swapped pair.
    pub const fn display_dimensions(&self) -> (u16, u16) {
        if self.rotation.quarter_turn() {
            (self.height, self.width)
        } else {
            (self.width, self.height)
        }
    }
}

fn le16(src: &[u8], offset: usize) -> Result<u16> {
    src.get(offset..offset + 2)
        .and_then(|s| <[u8; 2]>::try_from(s).ok())
        .map(u16::from_le_bytes)
        .ok_or(Error::ShortPacket)
}

/// Parse the header from a video message's content.
pub fn parse(content: &[u8]) -> Result<VideoHeader> {
    let frame_id = content
        .get(0..4)
        .and_then(|s| <[u8; 4]>::try_from(s).ok())
        .map(u32::from_le_bytes)
        .ok_or(Error::ShortPacket)?;
    let width = le16(content, 4)?;
    let height = le16(content, 6)?;
    let &codec = content.get(8).ok_or(Error::ShortPacket)?;
    let &flags = content.get(9).ok_or(Error::ShortPacket)?;
    Ok(VideoHeader {
        frame_id,
        width,
        height,
        codec: Codec::from_wire(codec),
        rotation: Rotation::from_bits(flags),
        ten_bit: flags & FLAG_TEN_BIT != 0,
        locked: flags & FLAG_LOCKED != 0,
        announced: flags & FLAG_ANNOUNCED != 0,
        metadata: flags & FLAG_METADATA != 0,
    })
}

/// Write the header into the start of a video message's content.
pub fn encode(out: &mut [u8], header: &VideoHeader) -> Result<usize> {
    let out = out
        .get_mut(..VIDEO_HEADER_LEN)
        .ok_or(Error::BufferTooSmall)?;
    let mut flags = header.rotation as u8;
    if header.ten_bit {
        flags |= FLAG_TEN_BIT;
    }
    if header.locked {
        flags |= FLAG_LOCKED;
    }
    if header.announced {
        flags |= FLAG_ANNOUNCED;
    }
    if header.metadata {
        flags |= FLAG_METADATA;
    }
    let [f0, f1, f2, f3] = header.frame_id.to_le_bytes();
    let [w0, w1] = header.width.to_le_bytes();
    let [h0, h1] = header.height.to_le_bytes();
    let bytes = [f0, f1, f2, f3, w0, w1, h0, h1, header.codec.wire(), flags];
    out.copy_from_slice(&bytes);
    Ok(VIDEO_HEADER_LEN)
}

/// Write a whole keyframe-metadata message: `header` with both protocol bits
/// set, then the eleven bytes that describe the keyframe after it.
pub fn encode_metadata(
    out: &mut [u8],
    header: &VideoHeader,
    metadata: &KeyframeMetadata,
) -> Result<usize> {
    let announced = VideoHeader {
        announced: true,
        metadata: true,
        ..*header
    };
    encode(out, &announced)?;
    let body = out
        .get_mut(VIDEO_HEADER_LEN..METADATA_LEN)
        .ok_or(Error::BufferTooSmall)?;
    let mut word = 0u32;
    if metadata.rebuilt {
        word |= META_REBUILT;
    }
    if metadata.keyframe {
        word |= META_KEYFRAME;
    }
    let [l0, l1, l2, l3] = METADATA_LEAD.to_le_bytes();
    let [m0, m1, m2, m3] = word.to_le_bytes();
    let depth = if metadata.ten_bit { 2 } else { 1 };
    let chroma = if metadata.chroma_444 {
        CHROMA_444
    } else {
        CHROMA_420
    };
    body.copy_from_slice(&[
        l0,
        l1,
        l2,
        l3,
        m0,
        m1,
        m2,
        m3,
        depth,
        chroma,
        metadata.rotation as u8,
    ]);
    Ok(METADATA_LEN)
}

/// Read the body of a keyframe-metadata message, one whose header parsed
/// with [`VideoHeader::metadata`] set.
pub fn parse_metadata(content: &[u8]) -> Result<KeyframeMetadata> {
    let word = content
        .get(14..18)
        .and_then(|s| <[u8; 4]>::try_from(s).ok())
        .map(u32::from_le_bytes)
        .ok_or(Error::ShortPacket)?;
    let &depth = content.get(18).ok_or(Error::ShortPacket)?;
    let &chroma = content.get(19).ok_or(Error::ShortPacket)?;
    let &rotation = content.get(20).ok_or(Error::ShortPacket)?;
    Ok(KeyframeMetadata {
        rebuilt: word & META_REBUILT != 0,
        keyframe: word & META_KEYFRAME != 0,
        ten_bit: depth == 2,
        chroma_444: chroma == CHROMA_444,
        rotation: Rotation::from_bits(rotation),
    })
}

/// Classify a video message as a keyframe.
///
/// **From the bitstream, and only from the bitstream.** No header bit carries
/// this: the one that was read as a keyframe marker names the colour depth
/// instead, so a receiver consulting it would call every ten-bit frame a
/// keyframe and every eight-bit keyframe a predicted frame.
pub fn is_keyframe(content: &[u8], codec: Codec) -> bool {
    let Some(bitstream) = content.get(VIDEO_HEADER_LEN..) else {
        return false;
    };
    let Some(unit) = first_unit_byte(bitstream) else {
        return false;
    };
    match codec {
        // Parameter set or instantaneous refresh.
        Codec::H264 => matches!(unit & 0x1F, 7 | 5),
        // Parameter sets 32 to 34, and the random-access picture range 16 to 21.
        Codec::H265 => {
            let unit_type = (unit >> 1) & 0x3F;
            matches!(unit_type, 32..=34 | 16..=21)
        }
    }
}

/// Whether a video message's bitstream is led by a parameter set: the unit a
/// receiver builds a decoder from, and the one that rebuilds an existing
/// decoder when the header does not say otherwise.
///
/// Narrower than [`is_keyframe`]: an instantaneous refresh without parameter
/// sets ahead of it is a keyframe, but nothing can be built from it.
pub fn leads_with_parameter_set(content: &[u8], codec: Codec) -> bool {
    let Some(unit) = content.get(VIDEO_HEADER_LEN..).and_then(first_unit_byte) else {
        return false;
    };
    match codec {
        Codec::H264 => unit & 0x1F == 7,
        Codec::H265 => (unit >> 1) & 0x3F == 32,
    }
}

/// The byte following the first start code, if the bitstream begins with one.
fn first_unit_byte(bitstream: &[u8]) -> Option<u8> {
    match bitstream.get(..4) {
        Some([0, 0, 0, 1]) => bitstream.get(4).copied(),
        _ => match bitstream.get(..3) {
            Some([0, 0, 1]) => bitstream.get(3).copied(),
            _ => None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn content(flags: u8, bitstream: &[u8]) -> [u8; 64] {
        let mut buf = [0u8; 64];
        let header = VideoHeader {
            frame_id: 1,
            width: 1920,
            height: 1080,
            codec: Codec::H264,
            rotation: Rotation::from_bits(flags),
            ten_bit: flags & FLAG_TEN_BIT != 0,
            locked: flags & FLAG_LOCKED != 0,
            announced: flags & FLAG_ANNOUNCED != 0,
            metadata: flags & FLAG_METADATA != 0,
        };
        encode(&mut buf, &header).unwrap();
        buf[VIDEO_HEADER_LEN..VIDEO_HEADER_LEN + bitstream.len()].copy_from_slice(bitstream);
        buf
    }

    #[test]
    fn round_trip_and_little_endian_layout() {
        let header = VideoHeader {
            frame_id: 0x0403_0201,
            width: 1920,
            height: 1080,
            codec: Codec::H265,
            rotation: Rotation::None,
            ten_bit: true,
            locked: false,
            announced: false,
            metadata: false,
        };
        let mut buf = [0u8; 16];
        assert_eq!(encode(&mut buf, &header).unwrap(), VIDEO_HEADER_LEN);
        assert_eq!(&buf[0..4], &[0x01, 0x02, 0x03, 0x04]);
        assert_eq!(&buf[4..6], &1920u16.to_le_bytes());
        assert_eq!(&buf[6..8], &1080u16.to_le_bytes());
        assert_eq!(buf[8], 2);
        assert_eq!(buf[9], Rotation::None as u8 | FLAG_TEN_BIT);
        assert_eq!(parse(&buf).unwrap(), header);
    }

    /// The codec byte was a constant `1` on every recorded host, so it read as
    /// reserved; a receiver must not refuse the values it never saw.
    #[test]
    fn the_codec_byte_reads_two_as_hevc_and_everything_else_as_h264() {
        let mut buf = content(Rotation::None as u8, &[0, 0, 0, 1, 0x65]);
        assert_eq!(parse(&buf).unwrap().codec, Codec::H264);
        buf[8] = 2;
        assert_eq!(parse(&buf).unwrap().codec, Codec::H265);
        buf[8] = 0;
        assert_eq!(parse(&buf).unwrap().codec, Codec::H264);
    }

    /// **The metadata message is 21 bytes and it is not a picture.** Both
    /// protocol bits are set on it, the keyframe bit and the rebuilt bit sit in
    /// the word at 14, and the classifier never mistakes its body for a start
    /// code.
    #[test]
    fn keyframe_metadata_round_trips_and_never_classifies_as_a_keyframe() {
        let header = VideoHeader {
            frame_id: 7,
            width: 2560,
            height: 1440,
            codec: Codec::H264,
            rotation: Rotation::Deg90,
            ten_bit: true,
            locked: false,
            announced: false,
            metadata: false,
        };
        let metadata = KeyframeMetadata {
            rebuilt: true,
            keyframe: true,
            ten_bit: true,
            chroma_444: false,
            rotation: Rotation::Deg90,
        };
        let mut buf = [0u8; 32];
        assert_eq!(
            encode_metadata(&mut buf, &header, &metadata).unwrap(),
            METADATA_LEN
        );
        let parsed = parse(&buf).unwrap();
        assert!(parsed.metadata && parsed.announced);
        assert_eq!(
            (parsed.frame_id, parsed.width, parsed.height),
            (7, 2560, 1440)
        );
        assert!(parsed.ten_bit);
        assert_eq!(buf[9], Rotation::Deg90 as u8 | FLAG_TEN_BIT | 0x60);
        assert_eq!(&buf[10..14], &[1, 0, 0, 0]);
        assert_eq!(&buf[14..18], &[3, 0, 0, 0]);
        assert_eq!(&buf[18..21], &[2, CHROMA_420, Rotation::Deg90 as u8]);
        assert_eq!(parse_metadata(&buf[..METADATA_LEN]).unwrap(), metadata);
        assert!(!is_keyframe(&buf[..METADATA_LEN], Codec::H264));
        assert!(!is_keyframe(&buf[..METADATA_LEN], Codec::H265));

        // A keyframe that was not a rebuild, on a full-chroma eight-bit stream.
        let plain = KeyframeMetadata {
            rebuilt: false,
            keyframe: true,
            ten_bit: false,
            chroma_444: true,
            rotation: Rotation::None,
        };
        encode_metadata(&mut buf, &header, &plain).unwrap();
        assert_eq!(&buf[14..18], &[2, 0, 0, 0]);
        assert_eq!(&buf[18..21], &[1, CHROMA_444, Rotation::None as u8]);
        assert_eq!(parse_metadata(&buf[..METADATA_LEN]).unwrap(), plain);

        // Short of the body, it is refused rather than read past.
        assert_eq!(parse_metadata(&buf[..20]), Err(Error::ShortPacket));
        assert_eq!(
            encode_metadata(&mut buf[..20], &header, &plain),
            Err(Error::BufferTooSmall)
        );
    }

    /// Upright is 1, not 0. Emitting 0 says "unspecified".
    #[test]
    fn rotation_is_one_based() {
        assert_eq!(Rotation::from_bits(0), Rotation::Unknown);
        assert_eq!(Rotation::from_bits(1), Rotation::None);
        assert_eq!(Rotation::from_bits(2), Rotation::Deg90);
        assert_eq!(Rotation::None as u8, 1);
        assert!(!Rotation::None.quarter_turn());
        assert!(Rotation::Deg90.quarter_turn());
        assert!(Rotation::Deg270.quarter_turn());
        assert!(!Rotation::Deg180.quarter_turn());
    }

    #[test]
    fn quarter_turn_swaps_display_dimensions() {
        let mut header = VideoHeader {
            frame_id: 0,
            width: 1920,
            height: 1080,
            codec: Codec::H264,
            rotation: Rotation::None,
            ten_bit: false,
            locked: false,
            announced: false,
            metadata: false,
        };
        assert_eq!(header.display_dimensions(), (1920, 1080));
        header.rotation = Rotation::Deg90;
        assert_eq!(header.display_dimensions(), (1080, 1920));
    }

    /// The case that matters: the flag is clear, as it always is in practice.
    #[test]
    fn classifies_from_the_bitstream_when_the_flag_is_clear() {
        let upright = Rotation::None as u8;
        // H.264 parameter set and instantaneous refresh.
        assert!(is_keyframe(
            &content(upright, &[0, 0, 0, 1, 0x67]),
            Codec::H264
        ));
        assert!(is_keyframe(
            &content(upright, &[0, 0, 0, 1, 0x65]),
            Codec::H264
        ));
        // A non-refresh slice is not a keyframe.
        assert!(!is_keyframe(
            &content(upright, &[0, 0, 0, 1, 0x41]),
            Codec::H264
        ));
    }

    #[test]
    fn classifies_h265_parameter_sets_and_refresh_pictures() {
        let upright = Rotation::None as u8;
        // Type 32 (VPS) and 33 (SPS) sit in bits 1 to 6.
        assert!(is_keyframe(
            &content(upright, &[0, 0, 0, 1, 32 << 1]),
            Codec::H265
        ));
        assert!(is_keyframe(
            &content(upright, &[0, 0, 0, 1, 33 << 1]),
            Codec::H265
        ));
        // Type 19, an instantaneous refresh.
        assert!(is_keyframe(
            &content(upright, &[0, 0, 0, 1, 19 << 1]),
            Codec::H265
        ));
        // Type 1, an ordinary picture.
        assert!(!is_keyframe(
            &content(upright, &[0, 0, 0, 1, 1 << 1]),
            Codec::H265
        ));
    }

    /// **The bit beside the rotation is the colour depth, not a keyframe.**
    /// A receiver that reads it as a keyframe calls every ten-bit predicted
    /// frame a keyframe, and a sender that writes it on its keyframes tells
    /// every receiver its eight-bit stream is ten-bit. One decoder family acts
    /// on that before parsing a single byte of bitstream, builds itself for a
    /// depth the stream does not carry, and fails every picture; hardware
    /// without ten-bit support fails at the first submission.
    /// *Named regression test.*
    #[test]
    fn the_ten_bit_flag_is_not_a_keyframe_marker() {
        let flags = Rotation::None as u8 | FLAG_TEN_BIT;
        // A predicted slice, with the depth bit set. It is still predicted.
        assert!(!is_keyframe(
            &content(flags, &[0, 0, 0, 1, 0x41]),
            Codec::H264
        ));
        // And a refresh is still a refresh with the bit clear, which is how
        // every recorded host sends one.
        let upright = Rotation::None as u8;
        assert!(is_keyframe(
            &content(upright, &[0, 0, 0, 1, 0x65]),
            Codec::H264
        ));
    }

    /// Nothing we emit sets it, because this stream is eight bit.
    #[test]
    fn a_sent_header_never_claims_ten_bit() {
        let mut buf = [0u8; 16];
        let header = VideoHeader {
            frame_id: 1,
            width: 1920,
            height: 1080,
            codec: Codec::H264,
            rotation: Rotation::None,
            ten_bit: false,
            locked: false,
            announced: false,
            metadata: false,
        };
        encode(&mut buf, &header).expect("encode");
        assert_eq!(
            buf[9] & FLAG_TEN_BIT,
            0,
            "an eight-bit stream claimed ten-bit colour"
        );
    }

    #[test]
    fn accepts_a_three_byte_start_code() {
        let upright = Rotation::None as u8;
        assert!(is_keyframe(
            &content(upright, &[0, 0, 1, 0x67]),
            Codec::H264
        ));
    }

    #[test]
    fn rejects_truncated_content() {
        assert!(!is_keyframe(&[], Codec::H264));
        assert!(!is_keyframe(&[0u8; 5], Codec::H264));
        // Header present, no bitstream.
        assert!(!is_keyframe(&[0u8; VIDEO_HEADER_LEN], Codec::H264));
        assert!(parse(&[0u8; 9]).is_err());
    }

    /// A refresh picture is a keyframe and builds nothing; a parameter set is
    /// both.
    #[test]
    fn a_parameter_set_leads_and_a_refresh_alone_does_not() {
        let mut sps = [0u8; VIDEO_HEADER_LEN + 5];
        sps[VIDEO_HEADER_LEN..].copy_from_slice(&[0, 0, 0, 1, 0x67]);
        assert!(leads_with_parameter_set(&sps, Codec::H264));
        assert!(is_keyframe(&sps, Codec::H264));

        let mut idr = sps;
        idr[VIDEO_HEADER_LEN + 4] = 0x65;
        assert!(!leads_with_parameter_set(&idr, Codec::H264));
        assert!(is_keyframe(&idr, Codec::H264));

        let mut vps = sps;
        vps[VIDEO_HEADER_LEN + 4] = 32 << 1;
        assert!(leads_with_parameter_set(&vps, Codec::H265));
        let mut cra = sps;
        cra[VIDEO_HEADER_LEN + 4] = 21 << 1;
        assert!(!leads_with_parameter_set(&cra, Codec::H265));
        assert!(is_keyframe(&cra, Codec::H265));
        assert!(!leads_with_parameter_set(&[0u8; 4], Codec::H264));
    }
}
