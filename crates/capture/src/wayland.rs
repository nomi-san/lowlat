//! The session protocol's framing, as much of it as two clients need.
//!
//! Requests are a header of the object and a packed size and opcode, then
//! arguments of four bytes each; a string is its length including a
//! terminator, then the bytes, padded out to four. Every reply is the same
//! shape, which is what lets an event nothing here understands be stepped
//! over by its size rather than having to be described.

/// The connection itself, which is object one and never allocated.
pub(crate) const DISPLAY: u32 = 1;

pub(crate) fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_ne_bytes());
}

/// A string is its length including a terminator, then the bytes, padded to
/// the four-byte alignment every argument sits on.
pub(crate) fn put_str(out: &mut Vec<u8>, value: &str) {
    let bytes = value.as_bytes();
    put_u32(out, u32::try_from(bytes.len() + 1).unwrap_or(1));
    out.extend_from_slice(bytes);
    out.push(0);
    while out.len() % 4 != 0 {
        out.push(0);
    }
}

pub(crate) fn read_u32(bytes: &[u8]) -> Option<u32> {
    Some(u32::from_ne_bytes(bytes.get(..4)?.try_into().ok()?))
}

pub(crate) fn read_i32(bytes: &[u8]) -> Option<i32> {
    Some(i32::from_ne_bytes(bytes.get(..4)?.try_into().ok()?))
}

/// The string at an offset, without its terminator.
pub(crate) fn read_str(body: &[u8], at: usize) -> Option<String> {
    let length = read_u32(body.get(at..)?)? as usize;
    let bytes = body.get(at + 4..at + 4 + length.checked_sub(1)?)?;
    String::from_utf8(bytes.to_vec()).ok()
}

/// The last argument of a message whose string length is not known in advance.
///
/// A padded string is followed by whatever comes after it, and stepping over
/// one to reach a fixed final argument costs the same arithmetic twice; the
/// final four bytes are the argument either way.
pub(crate) fn trailing_u32(body: &[u8]) -> Option<u32> {
    read_u32(body.get(body.len().checked_sub(4)?..)?)
}
