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
//! 8  1  reserved, 0x01
//! 9  1  flags
//! ```
//!
//! The endianness flips relative to the rest of the protocol, where sequences
//! and lengths are big endian. Getting it backwards yields a plausible frame
//! with absurd dimensions.

use crate::error::{Error, Result};

/// Bytes of header ahead of the bitstream.
pub const VIDEO_HEADER_LEN: usize = 10;

const ROTATION_MASK: u8 = 0x07;
const FLAG_KEYFRAME: u8 = 0x08;
const FLAG_FULLSCREEN: u8 = 0x10;
const RESERVED_BYTE: u8 = 0x01;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoHeader {
    /// Encoder **generation** counter, not a frame counter. It stays constant
    /// across a session's frames and increments only on reconfiguration, so
    /// anything ordering or deduplicating frames by it is broken.
    pub frame_id: u32,
    pub width: u16,
    pub height: u16,
    pub rotation: Rotation,
    /// As received this is unreliable: peers leave it clear on every frame,
    /// keyframes included. Use [`is_keyframe`] instead. We do set it when
    /// sending.
    pub keyframe: bool,
    pub fullscreen: bool,
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
    let &flags = content.get(9).ok_or(Error::ShortPacket)?;
    Ok(VideoHeader {
        frame_id,
        width,
        height,
        rotation: Rotation::from_bits(flags),
        keyframe: flags & FLAG_KEYFRAME != 0,
        fullscreen: flags & FLAG_FULLSCREEN != 0,
    })
}

/// Write the header into the start of a video message's content.
pub fn encode(out: &mut [u8], header: &VideoHeader) -> Result<usize> {
    let out = out
        .get_mut(..VIDEO_HEADER_LEN)
        .ok_or(Error::BufferTooSmall)?;
    let mut flags = header.rotation as u8;
    if header.keyframe {
        flags |= FLAG_KEYFRAME;
    }
    if header.fullscreen {
        flags |= FLAG_FULLSCREEN;
    }
    let [f0, f1, f2, f3] = header.frame_id.to_le_bytes();
    let [w0, w1] = header.width.to_le_bytes();
    let [h0, h1] = header.height.to_le_bytes();
    let bytes = [f0, f1, f2, f3, w0, w1, h0, h1, RESERVED_BYTE, flags];
    out.copy_from_slice(&bytes);
    Ok(VIDEO_HEADER_LEN)
}

/// Classify a video message as a keyframe.
///
/// Checks the header bit first, then falls back to the bitstream. **The
/// fallback is the path that actually fires**: peers leave the bit clear on
/// every frame, so a receiver trusting it alone sees zero keyframes forever.
pub fn is_keyframe(content: &[u8], codec: Codec) -> bool {
    let Some(&flags) = content.get(9) else {
        return false;
    };
    if flags & FLAG_KEYFRAME != 0 {
        return true;
    }
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
            rotation: Rotation::from_bits(flags),
            keyframe: flags & FLAG_KEYFRAME != 0,
            fullscreen: flags & FLAG_FULLSCREEN != 0,
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
            rotation: Rotation::None,
            keyframe: true,
            fullscreen: false,
        };
        let mut buf = [0u8; 16];
        assert_eq!(encode(&mut buf, &header).unwrap(), VIDEO_HEADER_LEN);
        assert_eq!(&buf[0..4], &[0x01, 0x02, 0x03, 0x04]);
        assert_eq!(&buf[4..6], &1920u16.to_le_bytes());
        assert_eq!(&buf[6..8], &1080u16.to_le_bytes());
        assert_eq!(buf[8], RESERVED_BYTE);
        assert_eq!(buf[9], Rotation::None as u8 | FLAG_KEYFRAME);
        assert_eq!(parse(&buf).unwrap(), header);
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
            rotation: Rotation::None,
            keyframe: false,
            fullscreen: false,
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

    #[test]
    fn honours_the_header_bit_when_a_peer_does_set_it() {
        let flags = Rotation::None as u8 | FLAG_KEYFRAME;
        assert!(is_keyframe(
            &content(flags, &[0, 0, 0, 1, 0x41]),
            Codec::H264
        ));
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
}
