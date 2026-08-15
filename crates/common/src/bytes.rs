//! Bounds-checked fixed-width reads and writes for wire fields.
//!
//! Wire fields are fixed width with explicit endianness. Nothing here
//! transmutes a struct onto a byte range, and every accessor returns an option
//! rather than panicking, because the input is hostile by definition.

macro_rules! reader {
    ($name:ident, $ty:ty, $conv:ident, $width:expr) => {
        #[doc = concat!("Read a `", stringify!($ty), "` at `offset`, or `None` if out of range.")]
        pub fn $name(src: &[u8], offset: usize) -> Option<$ty> {
            let end = offset.checked_add($width)?;
            let slice = src.get(offset..end)?;
            let array: [u8; $width] = slice.try_into().ok()?;
            Some(<$ty>::$conv(array))
        }
    };
}

macro_rules! writer {
    ($name:ident, $ty:ty, $conv:ident, $width:expr) => {
        #[doc = concat!("Write a `", stringify!($ty), "` at `offset`, or `None` if out of range.")]
        pub fn $name(dst: &mut [u8], offset: usize, value: $ty) -> Option<()> {
            let end = offset.checked_add($width)?;
            let slice = dst.get_mut(offset..end)?;
            slice.copy_from_slice(&value.$conv());
            Some(())
        }
    };
}

reader!(read_u16_be, u16, from_be_bytes, 2);
reader!(read_u32_be, u32, from_be_bytes, 4);
reader!(read_u64_be, u64, from_be_bytes, 8);
reader!(read_u16_le, u16, from_le_bytes, 2);
reader!(read_u32_le, u32, from_le_bytes, 4);
reader!(read_u64_le, u64, from_le_bytes, 8);

writer!(write_u16_be, u16, to_be_bytes, 2);
writer!(write_u32_be, u32, to_be_bytes, 4);
writer!(write_u64_be, u64, to_be_bytes, 8);
writer!(write_u16_le, u16, to_le_bytes, 2);
writer!(write_u32_le, u32, to_le_bytes, 4);
writer!(write_u64_le, u64, to_le_bytes, 8);

/// Read a single byte at `offset`.
pub fn read_u8(src: &[u8], offset: usize) -> Option<u8> {
    src.get(offset).copied()
}

/// Write a single byte at `offset`.
pub fn write_u8(dst: &mut [u8], offset: usize, value: u8) -> Option<()> {
    *dst.get_mut(offset)? = value;
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_big_endian() {
        let mut buf = [0u8; 16];
        write_u32_be(&mut buf, 4, 0xDEAD_BEEF).unwrap();
        assert_eq!(read_u32_be(&buf, 4), Some(0xDEAD_BEEF));
        assert_eq!(&buf[4..8], &[0xDE, 0xAD, 0xBE, 0xEF]);
    }

    #[test]
    fn round_trips_little_endian() {
        let mut buf = [0u8; 16];
        write_u16_le(&mut buf, 0, 0x1234).unwrap();
        assert_eq!(read_u16_le(&buf, 0), Some(0x1234));
        assert_eq!(&buf[0..2], &[0x34, 0x12]);
    }

    #[test]
    fn refuses_reads_past_the_end() {
        let buf = [0u8; 4];
        assert_eq!(read_u32_be(&buf, 1), None);
        assert_eq!(read_u64_be(&buf, 0), None);
        assert_eq!(read_u8(&buf, 4), None);
    }

    #[test]
    fn refuses_writes_past_the_end() {
        let mut buf = [0u8; 4];
        assert_eq!(write_u32_be(&mut buf, 1, 0), None);
        assert_eq!(write_u8(&mut buf, 4, 0), None);
        assert_eq!(buf, [0u8; 4], "a refused write must not modify anything");
    }

    #[test]
    fn offset_overflow_does_not_wrap() {
        let buf = [0u8; 8];
        assert_eq!(read_u32_be(&buf, usize::MAX), None);
    }
}
