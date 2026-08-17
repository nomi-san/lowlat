//! Control and input messages (docs/01-protocol.md 11).
//!
//! Thirteen bytes, three big-endian arguments then an opcode, optionally
//! followed by a body. Offsets are relative to the message **content**: what
//! follows the four-byte length prefix.
//!
//! ```text
//! 0  4  argument 0, big endian
//! 4  4  argument 1, big endian
//! 8  4  argument 2, big endian
//! 12 1  opcode
//! ```

use crate::error::{Error, Result};

/// Bytes of header ahead of any body.
pub const CONTROL_HEADER_LEN: usize = 13;

/// Channel carrying control and input.
pub const CONTROL_CHANNEL: u8 = 0;

/// Opcodes, as raw values.
///
/// Kept as constants rather than an enum so an unrecognised opcode passes
/// through the parser untouched. The protocol is additive, and a peer sending
/// something newer than us must not be a parse failure.
pub mod op {
    // Received by a host.
    pub const KEYBOARD: u8 = 0;
    pub const MOUSE_BUTTON: u8 = 1;
    pub const MOUSE_WHEEL: u8 = 2;
    pub const MOUSE_MOTION: u8 = 3;
    pub const GAMEPAD_BUTTON: u8 = 4;
    pub const GAMEPAD_AXIS: u8 = 5;
    pub const GAMEPAD_UNPLUG: u8 = 6;
    pub const INIT: u8 = 11;
    pub const ENCODER_CONFIG: u8 = 13;
    pub const USER_DATA: u8 = 17;
    pub const GAMEPAD_STATE: u8 = 23;
    pub const RELEASE: u8 = 24;
    pub const MOUSE_MOTION_STREAM: u8 = 26;
    pub const PEN_TOUCH: u8 = 30;

    // Sent by a host.
    pub const CURSOR: u8 = 9;
    pub const DISCONNECT: u8 = 10;
    pub const BLOCKED: u8 = 16;
    pub const RUMBLE: u8 = 20;
    pub const ENCODE_LATENCY: u8 = 21;
    pub const GUEST_LIST: u8 = 25;
    pub const HOST_MODE: u8 = 28;
    /// Announces the generation the video header's frame identifier will carry,
    /// on the frame after an encoder initialization.
    pub const ENCODER_GENERATION: u8 = 29;

    /// Short name for logs. Never allocates; unknown opcodes render as
    /// `"unknown"` and the numeric value should be logged alongside.
    pub const fn name(opcode: u8) -> &'static str {
        match opcode {
            KEYBOARD => "keyboard",
            MOUSE_BUTTON => "mouse-button",
            MOUSE_WHEEL => "mouse-wheel",
            MOUSE_MOTION => "mouse-motion",
            GAMEPAD_BUTTON => "gamepad-button",
            GAMEPAD_AXIS => "gamepad-axis",
            GAMEPAD_UNPLUG => "gamepad-unplug",
            CURSOR => "cursor",
            DISCONNECT => "disconnect",
            INIT => "init",
            ENCODER_CONFIG => "encoder-config",
            BLOCKED => "blocked",
            USER_DATA => "user-data",
            RUMBLE => "rumble",
            ENCODE_LATENCY => "encode-latency",
            GAMEPAD_STATE => "gamepad-state",
            RELEASE => "release",
            GUEST_LIST => "guest-list",
            MOUSE_MOTION_STREAM => "mouse-motion-stream",
            HOST_MODE => "host-mode",
            ENCODER_GENERATION => "encoder-generation",
            PEN_TOUCH => "pen-touch",
            _ => "unknown",
        }
    }
}

/// A control message: header plus whatever body followed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Control<'a> {
    pub a0: u32,
    pub a1: u32,
    pub a2: u32,
    pub opcode: u8,
    pub body: &'a [u8],
}

fn be32(src: &[u8], offset: usize) -> Result<u32> {
    src.get(offset..offset + 4)
        .and_then(|s| <[u8; 4]>::try_from(s).ok())
        .map(u32::from_be_bytes)
        .ok_or(Error::ShortPacket)
}

/// Parse a control message from a message's content.
pub fn parse(content: &[u8]) -> Result<Control<'_>> {
    let a0 = be32(content, 0)?;
    let a1 = be32(content, 4)?;
    let a2 = be32(content, 8)?;
    let &opcode = content.get(12).ok_or(Error::ShortPacket)?;
    let body = content
        .get(CONTROL_HEADER_LEN..)
        .ok_or(Error::ShortPacket)?;
    Ok(Control {
        a0,
        a1,
        a2,
        opcode,
        body,
    })
}

/// Write a control header. The caller appends any body itself.
pub fn encode_header(out: &mut [u8], control: &Control<'_>) -> Result<usize> {
    let out = out
        .get_mut(..CONTROL_HEADER_LEN)
        .ok_or(Error::BufferTooSmall)?;
    let [a0, a1, a2, a3] = control.a0.to_be_bytes();
    let [b0, b1, b2, b3] = control.a1.to_be_bytes();
    let [c0, c1, c2, c3] = control.a2.to_be_bytes();
    let bytes = [
        a0,
        a1,
        a2,
        a3,
        b0,
        b1,
        b2,
        b3,
        c0,
        c1,
        c2,
        c3,
        control.opcode,
    ];
    out.copy_from_slice(&bytes);
    Ok(CONTROL_HEADER_LEN)
}

/// Length a string body must declare.
///
/// **Includes the terminator.** A body sized `strlen` alone parses as valid
/// and then fails silently on the peer, which is a slow bug to find.
pub const fn string_body_len(text_len: usize) -> usize {
    text_len + 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_big_endian_layout() {
        let control = Control {
            a0: 0x0102_0304,
            a1: 0x0506_0708,
            a2: 0x090A_0B0C,
            opcode: op::MOUSE_MOTION,
            body: &[],
        };
        let mut buf = [0u8; 32];
        assert_eq!(
            encode_header(&mut buf, &control).unwrap(),
            CONTROL_HEADER_LEN
        );
        assert_eq!(&buf[0..4], &[1, 2, 3, 4]);
        assert_eq!(&buf[4..8], &[5, 6, 7, 8]);
        assert_eq!(&buf[8..12], &[9, 10, 11, 12]);
        assert_eq!(buf[12], op::MOUSE_MOTION);
        assert_eq!(parse(&buf[..CONTROL_HEADER_LEN]).unwrap(), control);
    }

    #[test]
    fn a_body_follows_the_header() {
        let mut buf = [0u8; 32];
        let control = Control {
            a0: 5,
            a1: 0,
            a2: 0,
            opcode: op::USER_DATA,
            body: &[],
        };
        encode_header(&mut buf, &control).unwrap();
        buf[CONTROL_HEADER_LEN..CONTROL_HEADER_LEN + 5].copy_from_slice(b"hello");
        let parsed = parse(&buf[..CONTROL_HEADER_LEN + 5]).unwrap();
        assert_eq!(parsed.body, b"hello");
        assert_eq!(parsed.opcode, op::USER_DATA);
    }

    /// An opcode we do not know must parse, not fail. The protocol is additive.
    #[test]
    fn an_unknown_opcode_parses() {
        let mut buf = [0u8; 16];
        let control = Control {
            a0: 0,
            a1: 0,
            a2: 0,
            opcode: 200,
            body: &[],
        };
        encode_header(&mut buf, &control).unwrap();
        assert_eq!(parse(&buf).unwrap().opcode, 200);
        assert_eq!(op::name(200), "unknown");
    }

    #[test]
    fn rejects_a_short_header() {
        for len in 0..CONTROL_HEADER_LEN {
            assert!(
                parse(&[0u8; CONTROL_HEADER_LEN][..len]).is_err(),
                "len {len}"
            );
        }
    }

    #[test]
    fn string_bodies_include_the_terminator() {
        assert_eq!(string_body_len(5), 6);
        assert_eq!(string_body_len(0), 1);
    }
}
