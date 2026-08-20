//! The pointer message, opcode 9.
//!
//! Thirteen bytes of header then twenty-one of body, and optionally an image
//! after that. Two forms share the layout: one carries the picture, the other
//! names a picture the far side already has.
//!
//! **Three states travel here and none of them implies another**
//! (docs/05-host.md section 8.1). Hidden is what an application asked for and
//! is what relative mode is derived from; suppressed is the pointer withheld
//! because somebody is using touch, and is not relative; whether a pointer is
//! being drawn at all is a third thing again.

use crate::control::{self, Control, op};
use crate::error::{Error, Result};

/// Body length, after the thirteen-byte header.
pub const BODY_LEN: usize = 21;

/// The bits one update carries.
///
/// **A set, not an enumeration.** Several are set at once and the far side
/// reads each independently, so the type models a set of bits rather than a
/// choice between them. Naming them here is what keeps the values in one
/// place: anything that spells `0x0002` for itself agrees with the encoder
/// only until one of them changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Flags(u16);

impl Flags {
    /// Nothing set.
    pub const NONE: Self = Self(0);
    /// An image travels with this message.
    pub const IMAGE: Self = Self(0x0002);
    /// No image: the far side already holds one, named by its checksum.
    pub const CACHED: Self = Self(0x0010);
    /// The far side must forget every image it holds before reading this one.
    pub const FORGET: Self = Self(0x0020);
    /// Motion should be sent as deltas rather than positions.
    pub const RELATIVE: Self = Self(0x0100);
    /// The pointer was hidden by an application. **Relative mode is derived
    /// from this and from nothing else**, but the two are separate bits and
    /// the far side reads them separately.
    pub const HIDDEN: Self = Self(0x0200);
    /// The pointer is withheld because input is arriving by touch. **Not
    /// relative**, and folding it into relative traps a pointer that was never
    /// taken over.
    pub const SUPPRESSED: Self = Self(0x0800);

    /// The value as it goes on the wire.
    pub const fn bits(self) -> u16 {
        self.0
    }

    /// Read a set off the wire.
    pub const fn from_bits(bits: u16) -> Self {
        Self(bits)
    }

    /// True when every bit of `other` is set here.
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

impl core::ops::BitOr for Flags {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl core::ops::BitOrAssign for Flags {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

/// What to say about the pointer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Update {
    /// Which stream this pointer belongs to. A host with one stream sends
    /// zero; the field exists because a pointer belongs to the picture it is
    /// drawn on, not to the session.
    pub stream: u8,
    /// **Relative to the stream's own corner, not the desktop's.** A pointer
    /// outside the stream is not this stream's to report at all.
    pub x: u16,
    pub y: u16,
    /// Where inside the image the pointer actually points.
    pub hot_x: u16,
    pub hot_y: u16,
    pub hidden: bool,
    pub relative: bool,
    pub suppressed: bool,
}

/// What the far side should do about the picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Image<'a> {
    /// Nothing about the picture changed; this is a position or a mode.
    Unchanged,
    /// Here it is, with its size and the checksum a later message names it by.
    ///
    /// The dimensions travel with the picture rather than with the update,
    /// because a message that disagreed with its own image would be read as
    /// the image.
    Fresh {
        png: &'a [u8],
        width: u16,
        height: u16,
        checksum: u32,
    },
    /// The far side already has it. Nothing but the checksum travels.
    Cached { checksum: u32 },
}

/// How large a buffer this message needs.
pub const fn encoded_len(image: usize) -> usize {
    control::CONTROL_HEADER_LEN + BODY_LEN + image
}

/// Write the message, returning how much of `out` was used.
///
/// **Whether the far side may be told to reuse a picture is the caller's to
/// decide**, and it is a per-peer question: a peer that never said it could
/// cache must be sent the picture every time, and telling it otherwise leaves
/// it drawing whatever it last had.
pub fn encode(out: &mut [u8], update: &Update, image: Image<'_>, forget: bool) -> Result<usize> {
    let mut flags = Flags::NONE;
    if update.hidden {
        flags |= Flags::HIDDEN;
    }
    if update.relative {
        flags |= Flags::RELATIVE;
    }
    if update.suppressed {
        flags |= Flags::SUPPRESSED;
    }
    if forget {
        flags |= Flags::FORGET;
    }

    // **The two forms differ in more than a flag.** A fresh picture puts the
    // hotspot in the body and the picture after it; a cached one puts the
    // hotspot in the header's arguments and the checksum where the hotspot
    // would have been. Writing one form's fields into the other's slots
    // produces a message that parses and points somewhere else.
    let (size, header_a0, header_a1, at15, at17) = match image {
        Image::Unchanged => (0u32, 0, 0, update.hot_x, update.hot_y),
        Image::Fresh { png, .. } => {
            flags |= Flags::IMAGE;
            let size = u32::try_from(png.len()).map_err(|_| Error::BufferTooSmall)?;
            (size, 0, 0, update.hot_x, update.hot_y)
        }
        Image::Cached { checksum } => {
            // **The image bit is not set here, and that is not an oversight.**
            // The far side reads the two independently: it takes an image off
            // the wire when the image bit is set, and looks one up when the
            // cached bit is. Setting both makes it read the body that follows
            // as a picture, and there is no picture.
            flags |= Flags::CACHED;
            (
                0,
                u32::from(update.hot_x),
                u32::from(update.hot_y),
                (checksum & 0xFFFF) as u16,
                (checksum >> 16) as u16,
            )
        }
    };

    let header = Control {
        a0: header_a0,
        a1: header_a1,
        a2: u32::from(update.stream),
        opcode: op::CURSOR,
        body: &[],
    };
    let mut at = control::encode_header(out, &header)?;

    // Three bytes nobody reads, and they are written rather than left as
    // whatever the buffer held.
    put(out, &mut at, &[0, 0, 0])?;
    put(out, &mut at, &size.to_be_bytes())?;
    let (width, height) = match image {
        Image::Fresh { width, height, .. } => (width, height),
        _ => (0, 0),
    };
    put(out, &mut at, &width.to_be_bytes())?;
    put(out, &mut at, &height.to_be_bytes())?;
    put(out, &mut at, &update.x.to_be_bytes())?;
    put(out, &mut at, &update.y.to_be_bytes())?;
    put(out, &mut at, &at15.to_be_bytes())?;
    put(out, &mut at, &at17.to_be_bytes())?;
    put(out, &mut at, &flags.bits().to_be_bytes())?;

    if let Image::Fresh { png, .. } = image {
        put(out, &mut at, png)?;
    }
    Ok(at)
}

fn put(out: &mut [u8], at: &mut usize, bytes: &[u8]) -> Result<()> {
    out.get_mut(*at..*at + bytes.len())
        .ok_or(Error::BufferTooSmall)?
        .copy_from_slice(bytes);
    *at += bytes.len();
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::vec;

    use super::*;

    fn flags_of(message: &[u8]) -> Flags {
        Flags::from_bits(u16::from_be_bytes([message[13 + 19], message[13 + 20]]))
    }

    /// **The bit values, written down.** Relative is 0x0100 and hidden is
    /// 0x0200, which reads backwards from the order the two are usually
    /// discussed in and is the way round the far side has always read them.
    /// Exchanging them puts a peer into relative mode whenever the pointer is
    /// merely hidden, and leaves it in absolute mode during mouselook.
    #[test]
    fn the_bits_are_the_values_the_far_side_reads() {
        assert_eq!(Flags::IMAGE.bits(), 0x0002);
        assert_eq!(Flags::CACHED.bits(), 0x0010);
        assert_eq!(Flags::FORGET.bits(), 0x0020);
        assert_eq!(Flags::RELATIVE.bits(), 0x0100);
        assert_eq!(Flags::HIDDEN.bits(), 0x0200);
        assert_eq!(Flags::SUPPRESSED.bits(), 0x0800);
    }

    /// A named picture carries no picture bit, because the far side reads the
    /// two independently and would take the next bytes for an image.
    #[test]
    fn a_named_picture_does_not_claim_to_carry_one() {
        let mut out = vec![0u8; encoded_len(0)];
        let used = encode(
            &mut out,
            &Update::default(),
            Image::Cached { checksum: 7 },
            false,
        )
        .expect("encode");
        let flags = flags_of(&out[..used]);
        assert!(flags.contains(Flags::CACHED));
        assert!(
            !flags.contains(Flags::IMAGE),
            "a named picture claimed to carry one"
        );
        assert_eq!(used, encoded_len(0), "something followed the body");
    }

    /// **Suppressed is its own bit and is not relative.** Folding the two
    /// together traps the pointer of anyone who touched a screen.
    #[test]
    fn the_three_states_are_three_bits() {
        let mut out = vec![0u8; encoded_len(0)];
        for (update, wanted) in [
            (
                Update {
                    hidden: true,
                    ..Update::default()
                },
                Flags::HIDDEN,
            ),
            (
                Update {
                    relative: true,
                    ..Update::default()
                },
                Flags::RELATIVE,
            ),
            (
                Update {
                    suppressed: true,
                    ..Update::default()
                },
                Flags::SUPPRESSED,
            ),
        ] {
            let used = encode(&mut out, &update, Image::Unchanged, false).expect("encode");
            assert_eq!(flags_of(&out[..used]), wanted, "for {update:?}");
        }
    }

    /// The cached form moves the hotspot into the header and puts the checksum
    /// where the hotspot was. Reading one form with the other's rules points
    /// the pointer somewhere else entirely.
    #[test]
    fn the_cached_form_moves_the_hotspot_into_the_header() {
        let update = Update {
            hot_x: 3,
            hot_y: 9,
            ..Update::default()
        };
        let mut out = vec![0u8; encoded_len(0)];

        let used = encode(&mut out, &update, Image::Unchanged, false).expect("encode");
        let fresh = &out[..used];
        assert_eq!(u32::from_be_bytes(fresh[0..4].try_into().unwrap()), 0);
        assert_eq!(u16::from_be_bytes([fresh[13 + 15], fresh[13 + 16]]), 3);

        let used = encode(
            &mut out,
            &update,
            Image::Cached {
                checksum: 0xDEAD_BEEF,
            },
            false,
        )
        .expect("encode");
        let cached = &out[..used];
        assert_eq!(u32::from_be_bytes(cached[0..4].try_into().unwrap()), 3);
        assert_eq!(u32::from_be_bytes(cached[4..8].try_into().unwrap()), 9);
        // Low half first, then the high half.
        assert_eq!(
            u16::from_be_bytes([cached[13 + 15], cached[13 + 16]]),
            0xBEEF
        );
        assert_eq!(
            u16::from_be_bytes([cached[13 + 17], cached[13 + 18]]),
            0xDEAD
        );
        assert!(flags_of(cached).contains(Flags::CACHED));
    }

    /// The picture follows the body and the declared size is its length.
    #[test]
    fn a_fresh_picture_follows_the_body() {
        let png = [1u8, 2, 3, 4, 5];
        let mut out = vec![0u8; encoded_len(png.len())];
        let used = encode(
            &mut out,
            &Update::default(),
            Image::Fresh {
                png: &png,
                width: 7,
                height: 21,
                checksum: 0,
            },
            false,
        )
        .expect("encode");
        assert_eq!(used, encoded_len(png.len()));
        assert_eq!(
            u32::from_be_bytes(out[13 + 3..13 + 7].try_into().unwrap()),
            png.len() as u32
        );
        assert_eq!(&out[13 + BODY_LEN..used], &png);
        assert!(flags_of(&out[..used]).contains(Flags::IMAGE));
        // The picture's own size, not the update's.
        assert_eq!(u16::from_be_bytes([out[13 + 7], out[13 + 8]]), 7);
        assert_eq!(u16::from_be_bytes([out[13 + 9], out[13 + 10]]), 21);
    }

    /// A short buffer is refused rather than half-written.
    #[test]
    fn a_short_buffer_is_refused() {
        let mut out = [0u8; 20];
        assert!(encode(&mut out, &Update::default(), Image::Unchanged, false).is_err());
    }
}
