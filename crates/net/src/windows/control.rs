//! Control-message arithmetic: the system headers' macros, written out.
//!
//! A control message is a header -- a length the width of a pointer, then a
//! level and a type -- followed by its data at the natural alignment, and the
//! next message starts at the header's alignment past the length the first
//! one declares. The headers define all of this as macros, which no
//! declaration crate can carry, so it is here once, for the receive side and
//! the send side both, and checked against the sizes the headers imply.

use core::mem;

use windows_sys::Win32::Networking::WinSock::CMSGHDR;

/// The header: a pointer-width length, then two ints.
const HEADER: usize = mem::size_of::<CMSGHDR>();

/// The alignment of a header and of the data after it: the natural maximum,
/// which is the header's own on every target this builds for.
const ALIGN: usize = mem::align_of::<CMSGHDR>();

/// Round up to the alignment.
const fn align(len: usize) -> usize {
    (len + ALIGN - 1) & !(ALIGN - 1)
}

/// Where a message's data starts, past its header.
const DATA: usize = align(HEADER);

/// Room one message carrying `data` bytes takes, padding included.
pub(super) const fn space(data: usize) -> usize {
    align(HEADER + align(data))
}

/// The length a message carrying `data` bytes declares in its header.
const fn declared(data: usize) -> usize {
    DATA + data
}

/// Write one message carrying `value` at `at`, returning where the next one
/// goes, or `None` when it does not fit.
pub(super) fn put<T: Copy>(
    buf: &mut [u8],
    at: usize,
    level: i32,
    kind: i32,
    value: T,
) -> Option<usize> {
    let end = at.checked_add(space(mem::size_of::<T>()))?;
    let room = buf.get_mut(at..end)?;
    let header = CMSGHDR {
        cmsg_len: declared(mem::size_of::<T>()),
        cmsg_level: level,
        cmsg_type: kind,
    };
    // SAFETY: `room` spans a header and the aligned value, checked above, and
    // both writes tolerate any alignment.
    unsafe {
        core::ptr::write_unaligned(room.as_mut_ptr().cast::<CMSGHDR>(), header);
        core::ptr::write_unaligned(room.as_mut_ptr().add(DATA).cast::<T>(), value);
    }
    Some(end)
}

/// The messages in `buf`, which is exactly as long as the system said it
/// wrote, as their level, type and data. Stops at the first header that does
/// not fit or declares less than a header, rather than looping on it.
pub(super) fn messages(buf: &[u8]) -> impl Iterator<Item = (i32, i32, &[u8])> {
    let mut at = 0usize;
    core::iter::from_fn(move || {
        let header = buf.get(at..at.checked_add(HEADER)?)?;
        // SAFETY: a whole header's bytes are present; the read tolerates any
        // alignment.
        let header = unsafe { core::ptr::read_unaligned(header.as_ptr().cast::<CMSGHDR>()) };
        if header.cmsg_len < DATA {
            return None;
        }
        let data = buf.get(at.checked_add(DATA)?..at.checked_add(header.cmsg_len)?)?;
        at = at.checked_add(align(header.cmsg_len))?;
        Some((header.cmsg_level, header.cmsg_type, data))
    })
}

/// The value a message carries, if it carries at least one.
///
/// `T` must be plain data for which any bit pattern is a value, which the
/// packet-information structures are.
pub(super) fn read<T: Copy>(data: &[u8]) -> Option<T> {
    let bytes = data.get(..mem::size_of::<T>())?;
    // SAFETY: `bytes` spans a whole `T`, the read tolerates any alignment, and
    // the caller's `T` is plain data.
    Some(unsafe { core::ptr::read_unaligned(bytes.as_ptr().cast::<T>()) })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The headers' figures on a 64-bit target, written out rather than
    /// derived, so a wrong alignment fails here instead of agreeing with
    /// itself: a sixteen-byte header, data straight after it, and each
    /// message padded to eight.
    #[cfg(target_pointer_width = "64")]
    #[test]
    fn the_arithmetic_matches_the_headers() {
        assert_eq!(HEADER, 16);
        assert_eq!(DATA, 16);
        assert_eq!(declared(4), 20);
        assert_eq!(space(4), 24, "a segment size");
        assert_eq!(space(8), 24, "a v4 packet information");
        assert_eq!(space(20), 40, "a v6 packet information");
    }

    /// Written messages read back as written, and the walk steps over each
    /// one's padding to the next.
    #[test]
    fn messages_round_trip() {
        let mut buf = [0u8; 64];
        let next = put(&mut buf, 0, 17, 2, 1200u32).expect("room");
        let end = put(&mut buf, next, 41, 19, [7u8; 20]).expect("room");
        assert_eq!(end, 64);

        let found: Vec<_> = messages(&buf[..end]).collect();
        assert_eq!(found.len(), 2);
        assert_eq!((found[0].0, found[0].1), (17, 2));
        assert_eq!(read::<u32>(found[0].2), Some(1200));
        assert_eq!((found[1].0, found[1].1), (41, 19));
        assert_eq!(read::<[u8; 20]>(found[1].2), Some([7u8; 20]));
    }

    /// A header declaring less than a header is the end, not a message of
    /// negative length and not a step of zero that loops forever.
    #[test]
    fn a_short_declared_length_ends_the_walk() {
        let mut buf = [0u8; 32];
        put(&mut buf, 0, 0, 19, 0u64).expect("room");
        buf[0] = 0;
        assert_eq!(messages(&buf).count(), 0);
    }

    /// A message that does not fit is refused rather than written past the
    /// end.
    #[test]
    fn a_message_that_does_not_fit_is_refused() {
        let mut buf = [0u8; 30];
        assert_eq!(put(&mut buf, 8, 0, 19, 0u64), None);
    }
}
