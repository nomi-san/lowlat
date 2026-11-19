//! A monitor's EDID: one block per slot (docs/11-drivers.md section 1.3).
//!
//! **Nothing in it changes but the serial, which carries the slot.** The system
//! knows a monitor by its EDID, so one that changed across a replug -- a new
//! preferred timing for a new mode, say -- would be a new monitor and lose its
//! arrangement; the modes travel in the lists the driver answers with instead.
//! The serial is also how the driver finds a monitor's slot when the system
//! asks for the modes of a description and names no monitor.
//!
//! The block is the driver's own -- its brand's, dumped or generated
//! (section 1.10) -- or a real monitor's a person dumped and named in the file
//! (section 1.4); either way its serial is patched per slot. A different block
//! is a different monitor to the system, with nothing of the old one's
//! arrangement.

use lowlat_brand::edid as edid_block;

use crate::{MAX_MONITORS, brand};

/// One block.
pub const LEN: usize = edid_block::LEN;
/// The most an EDID here carries: the base block and one extension.
pub const MAX_LEN: usize = edid_block::MAX_LEN;

/// The serial's upper half, the brand's; the slot is the lower.
const SERIAL_BASE: u32 = brand::SERIAL_BASE;
/// Where the serial sits, and how far a description must match the block
/// before it: the header, the manufacturer and the product.
const SERIAL: core::ops::Range<usize> = 12..16;

/// An EDID of one block or two.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Edid {
    bytes: [u8; MAX_LEN],
    len: usize,
}

impl Default for Edid {
    fn default() -> Self {
        Self::own()
    }
}

impl Edid {
    /// The driver's own: the block its brand dumped, or the one generated for
    /// it, as the build checked it.
    pub fn own() -> Self {
        let mut bytes = [0u8; MAX_LEN];
        let len = brand::EDID.len().min(MAX_LEN);
        if let (Some(to), Some(from)) = (bytes.get_mut(..len), brand::EDID.get(..len)) {
            to.copy_from_slice(from);
        }
        Self { bytes, len }
    }

    /// A dumped EDID: one block or two, the header first, each block's
    /// checksum right, and the extension count saying how many follow.
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        if !edid_block::valid(bytes) {
            return None;
        }
        let mut edid = Self {
            bytes: [0u8; MAX_LEN],
            len: bytes.len(),
        };
        edid.bytes.get_mut(..bytes.len())?.copy_from_slice(bytes);
        Some(edid)
    }

    pub fn as_slice(&self) -> &[u8] {
        self.bytes.get(..self.len).unwrap_or(&[])
    }

    /// This EDID for a slot: its serial carries the slot, and the base block's
    /// checksum follows. A slot past the last is clamped to it rather than
    /// refused, since the driver never asks for one.
    pub fn for_slot(&self, slot: u8) -> Self {
        let slot = slot.min(u8::try_from(MAX_MONITORS - 1).unwrap_or(u8::MAX));
        let mut edid = *self;
        let serial = SERIAL_BASE | u32::from(slot);
        if let Some(field) = edid.bytes.get_mut(SERIAL) {
            field.copy_from_slice(&serial.to_le_bytes());
        }
        let total = edid_block::sum(edid.bytes.get(..LEN - 1).unwrap_or(&[]));
        if let Some(checksum) = edid.bytes.get_mut(LEN - 1) {
            *checksum = 0u8.wrapping_sub(total);
        }
        edid
    }

    /// The slot a description of this EDID's names, or `None` for any other.
    pub fn slot_of(&self, description: &[u8]) -> Option<u8> {
        if description.len() < LEN {
            return None;
        }
        if description.get(..SERIAL.start)? != self.bytes.get(..SERIAL.start)? {
            return None;
        }
        let serial = u32::from_le_bytes(description.get(SERIAL)?.try_into().ok()?);
        if serial & 0xffff_0000 != SERIAL_BASE {
            return None;
        }
        let slot = usize::try_from(serial & 0xffff).ok()?;
        if slot >= MAX_MONITORS {
            return None;
        }
        u8::try_from(slot).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lowlat_brand::edid::sum;

    const HEADER: [u8; 8] = [0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00];
    const EXTENSIONS: usize = 126;

    fn valid(edid: &Edid) -> bool {
        edid.as_slice().chunks(LEN).all(|block| sum(block) == 0)
    }

    #[test]
    fn every_slots_block_is_valid_and_names_its_slot() {
        let own = Edid::own();
        for slot in 0..u8::try_from(MAX_MONITORS).expect("fits") {
            let block = own.for_slot(slot);
            assert_eq!(&block.as_slice()[..8], &HEADER);
            assert!(valid(&block), "slot {slot}: the checksum");
            assert_eq!(own.slot_of(block.as_slice()), Some(slot));
        }
        assert_eq!(
            own.slot_of(own.for_slot(200).as_slice()),
            Some(15),
            "past the last, clamped"
        );
    }

    #[test]
    fn slots_differ_in_the_serial_and_checksum_alone() {
        let a = Edid::own().for_slot(0);
        let b = Edid::own().for_slot(9);
        for (i, (x, y)) in a.as_slice().iter().zip(b.as_slice()).enumerate() {
            if !SERIAL.contains(&i) && i != LEN - 1 {
                assert_eq!(x, y, "byte {i} differs between slots");
            }
        }
        assert_ne!(a.as_slice()[SERIAL], b.as_slice()[SERIAL]);
    }

    /// **The serial's upper half is the maker's first two letters**, which
    /// for lowlat's own is the `LW` its monitors have always carried.
    #[test]
    fn the_serial_carries_the_makers_first_two_letters_and_the_slot() {
        let block = Edid::own().for_slot(7);
        let maker = lowlat_brand::edid::maker(block.as_slice()).expect("three letters");
        assert_eq!(
            &block.as_slice()[SERIAL],
            &[7, 0, maker[1], maker[0]],
            "low byte first"
        );
    }

    #[test]
    fn a_foreign_or_short_description_names_no_slot() {
        let own = Edid::own();
        let mut block = *own
            .for_slot(3)
            .as_slice()
            .first_chunk::<LEN>()
            .expect("a block");
        assert_eq!(own.slot_of(&block[..LEN - 1]), None);
        block[9] ^= 1;
        assert_eq!(own.slot_of(&block), None);
        let mut far = *own
            .for_slot(3)
            .as_slice()
            .first_chunk::<LEN>()
            .expect("a block");
        far[12] = 0x40;
        assert_eq!(own.slot_of(&far), None, "a slot past the last");
        let mut other = *own
            .for_slot(3)
            .as_slice()
            .first_chunk::<LEN>()
            .expect("a block");
        other[15] ^= 0x80;
        assert_eq!(own.slot_of(&other), None, "another serial range");
    }

    /// A real monitor's two blocks, as a person would dump them: another
    /// maker, one extension, both checksums right.
    fn dumped() -> [u8; MAX_LEN] {
        let mut bytes = [0u8; MAX_LEN];
        bytes[..LEN].copy_from_slice(
            &lowlat_brand::edid::generate(b"DEL", 0x1234, b"Dell").expect("a block"),
        );
        bytes[8..12].copy_from_slice(&[0x10, 0xac, 0x34, 0x12]);
        bytes[12..16].copy_from_slice(&[0xaa, 0xbb, 0xcc, 0xdd]);
        bytes[EXTENSIONS] = 1;
        bytes[LEN - 1] = 0u8.wrapping_sub(sum(&bytes[..LEN - 1]));
        bytes[LEN] = 0x02;
        bytes[LEN + 1] = 0x03;
        bytes[MAX_LEN - 1] = 0u8.wrapping_sub(sum(&bytes[LEN..MAX_LEN - 1]));
        bytes
    }

    #[test]
    fn a_dumped_edid_is_taken_whole_and_patched_per_slot() {
        let bytes = dumped();
        let edid = Edid::parse(&bytes).expect("valid");
        assert_eq!(edid.as_slice(), &bytes[..]);
        let five = edid.for_slot(5);
        assert!(valid(&five), "both checksums after the patch");
        assert_eq!(edid.slot_of(five.as_slice()), Some(5));
        assert_eq!(
            &five.as_slice()[8..12],
            &[0x10, 0xac, 0x34, 0x12],
            "its maker kept"
        );
        assert_eq!(
            &five.as_slice()[LEN..],
            &bytes[LEN..],
            "the extension untouched"
        );
        assert_eq!(
            Edid::own().slot_of(five.as_slice()),
            None,
            "not the driver's own block's"
        );
    }

    #[test]
    fn a_damaged_dump_is_refused() {
        let good = dumped();
        assert!(Edid::parse(&good).is_some());
        // One good block and a zero byte: every checksum holds, the length
        // alone is wrong.
        let mut long = [0u8; LEN + 1];
        long[..LEN].copy_from_slice(
            Edid::own()
                .for_slot(0)
                .as_slice()
                .get(..LEN)
                .expect("a block"),
        );
        assert!(Edid::parse(&long).is_none(), "a length of no block count");
        // A header broken with its checksum kept right.
        let mut header = good;
        header[1] = 0;
        header[LEN - 1] = 0u8.wrapping_sub(sum(&header[..LEN - 1]));
        assert!(Edid::parse(&header).is_none(), "the header");
        let mut base = good;
        base[20] ^= 1;
        assert!(Edid::parse(&base).is_none(), "the base block's checksum");
        let mut extension = good;
        extension[LEN + 5] ^= 1;
        assert!(
            Edid::parse(&extension).is_none(),
            "the extension's checksum"
        );
        let mut count = *good.first_chunk::<LEN>().expect("a block");
        count[EXTENSIONS] = 1;
        count[LEN - 1] = 0u8.wrapping_sub(sum(&count[..LEN - 1]));
        assert!(
            Edid::parse(&count).is_none(),
            "one block saying another follows"
        );
        let own = Edid::own().for_slot(0);
        assert!(Edid::parse(own.as_slice()).is_some(), "the driver's own");
    }
}
