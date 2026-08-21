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
    /// Turns per-frame timing on. A peer sends it with every bit clear in an
    /// ordinary session, which is a request to send nothing extra.
    pub const DIAGNOSTICS: u8 = 35;

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
    /// Per-frame timing, behind the flag [`DIAGNOSTICS`] carries.
    pub const FRAME_TIMING: u8 = 34;

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
            // **Named without a direction.** It travels both ways carrying
            // different things -- a host's encode time out, a peer's decode
            // time in -- so naming it for one direction mislabels the other
            // in the log, which is exactly where the two get confused.
            ENCODE_LATENCY => "latency",
            GAMEPAD_STATE => "gamepad-state",
            RELEASE => "release",
            GUEST_LIST => "guest-list",
            MOUSE_MOTION_STREAM => "mouse-motion-stream",
            HOST_MODE => "host-mode",
            ENCODER_GENERATION => "encoder-generation",
            PEN_TOUCH => "pen-touch",
            DIAGNOSTICS => "diagnostics",
            FRAME_TIMING => "frame-timing",
            _ => "unknown",
        }
    }
}

/// What the disconnect opcode's first argument carries.
///
/// **A peer renders these; they are not ours to invent.** Each one names a
/// reason a session ended in terms the peer already has a string for, so a
/// value outside the set shows as a blank reason rather than as an error.
///
/// **Never zero.** A peer stores the status and stops on a non-zero one, so a
/// disconnect carrying zero fires its callback and leaves the session running,
/// which reads as a host that said goodbye and kept streaming.
pub mod status {
    /// Every seat is taken.
    pub const NO_ROOM: i32 = 11;
    /// No encoder could be built for what was asked of it.
    ///
    /// This is the one a peer gets when it asks for a configuration the device
    /// will not encode, because the failure surfaces as an encoder that will
    /// not initialize rather than as a capability answered in advance.
    pub const ENCODER_UNAVAILABLE: i32 = -15000;
    /// A picture failed to encode.
    pub const ENCODE_FAILED: i32 = -15002;
    /// The device would not say what it can encode.
    pub const ENCODER_CAPABILITIES: i32 = -15110;
    /// There is nothing to capture.
    ///
    /// **Distinct from an encoder that would not start, and the difference is
    /// not pedantic.** A display that has powered down leaves the encoder
    /// perfectly able and nothing to feed it; reporting that as an encoder
    /// failure sends whoever reads it to the wrong half of the machine.
    pub const CAPTURE_UNAVAILABLE: i32 = -14003;
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

/// The most a body may carry, its terminator counted.
///
/// **A peer's own ceiling, not ours.** One over it is refused at the far end
/// without being sent, so a sender that does not check simply loses the
/// message with nothing to say why.
pub const USER_DATA_MAX: usize = 0x10_0000;

/// The sub-identifier and text an application message carries.
///
/// **The SDK never looks inside the text.** The sub-identifier and the body
/// are an application's own protocol; what belongs here is only the framing
/// they arrive in (docs/01-protocol.md 11.1).
///
/// **A trailing terminator is stripped and never required.** Every sender
/// known to us counts one, and an application reading the text as a C string
/// needs it, so it is always written on the way out
/// ([`string_body_len`]). On the way in it is dropped rather than insisted on:
/// this is a pass-through, and refusing a message because a peer framed its
/// own payload differently loses something the SDK was never entitled to
/// judge.
///
/// **The declared length bounds the body and cannot extend it.** A peer that
/// says more than it sent is taken at what it sent.
#[must_use]
pub fn user_data<'a>(message: &Control<'a>) -> Option<(u32, &'a [u8])> {
    if message.opcode != op::USER_DATA {
        return None;
    }
    let declared = usize::try_from(message.a0).unwrap_or(usize::MAX);
    let body = message.body.get(..declared).unwrap_or(message.body);
    Some((message.a1, body.strip_suffix(&[0]).unwrap_or(body)))
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

    /// **Every opcode either side sends has a name.** A live run reads the
    /// vocabulary a peer speaks off this table, and one that renders as
    /// "unknown" sends the reader to the protocol document to look up a number
    /// that was known all along. Found by a live run doing exactly that.
    #[test]
    fn every_opcode_this_protocol_defines_has_a_name() {
        use op::{
            BLOCKED, CURSOR, DIAGNOSTICS, DISCONNECT, ENCODE_LATENCY, ENCODER_CONFIG,
            ENCODER_GENERATION, FRAME_TIMING, GAMEPAD_AXIS, GAMEPAD_BUTTON, GAMEPAD_STATE,
            GAMEPAD_UNPLUG, GUEST_LIST, HOST_MODE, INIT, KEYBOARD, MOUSE_BUTTON, MOUSE_MOTION,
            MOUSE_MOTION_STREAM, MOUSE_WHEEL, PEN_TOUCH, RELEASE, RUMBLE, USER_DATA, name,
        };
        for opcode in [
            KEYBOARD,
            MOUSE_BUTTON,
            MOUSE_WHEEL,
            MOUSE_MOTION,
            GAMEPAD_BUTTON,
            GAMEPAD_AXIS,
            GAMEPAD_UNPLUG,
            CURSOR,
            DISCONNECT,
            INIT,
            ENCODER_CONFIG,
            BLOCKED,
            USER_DATA,
            RUMBLE,
            ENCODE_LATENCY,
            GAMEPAD_STATE,
            RELEASE,
            GUEST_LIST,
            MOUSE_MOTION_STREAM,
            HOST_MODE,
            ENCODER_GENERATION,
            PEN_TOUCH,
            FRAME_TIMING,
            DIAGNOSTICS,
        ] {
            assert_ne!(name(opcode), "unknown", "opcode {opcode} has no name");
        }
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

    /// **The terminator is counted and then stripped**, so an application is
    /// handed the text it was sent and not the byte that ends it.
    #[test]
    fn application_message_text_loses_the_terminator_that_framed_it() {
        let message = Control {
            a0: 6,
            a1: 11,
            a2: 0,
            opcode: op::USER_DATA,
            body: b"hello\0",
        };
        assert_eq!(user_data(&message), Some((11, &b"hello"[..])));
    }

    /// **A body framed without one is still delivered.** This is a pass
    /// through, and refusing a message because a peer counted its own payload
    /// differently loses something the SDK was never entitled to judge.
    #[test]
    fn application_message_without_a_terminator_still_arrives() {
        let message = Control {
            a0: 5,
            a1: 12,
            a2: 0,
            opcode: op::USER_DATA,
            body: b"hello",
        };
        assert_eq!(user_data(&message), Some((12, &b"hello"[..])));
    }

    /// **A declared length bounds the body and never extends it.** A peer that
    /// claims more than it sent is taken at what it sent, rather than the
    /// parser reading past what arrived.
    #[test]
    fn a_declared_length_cannot_reach_past_what_arrived() {
        let over = Control {
            a0: 4096,
            a1: 1,
            a2: 0,
            opcode: op::USER_DATA,
            body: b"hi\0",
        };
        assert_eq!(user_data(&over), Some((1, &b"hi"[..])));

        // And one that claims less is taken at its word, which is what lets a
        // sender put its own padding after the text.
        let under = Control {
            a0: 3,
            a1: 1,
            a2: 0,
            opcode: op::USER_DATA,
            body: b"hi\0junk",
        };
        assert_eq!(user_data(&under), Some((1, &b"hi"[..])));
    }

    /// Nothing else is an application message, whatever its body looks like.
    #[test]
    fn only_the_application_opcode_carries_application_text() {
        let message = Control {
            a0: 6,
            a1: 11,
            a2: 0,
            opcode: op::KEYBOARD,
            body: b"hello\0",
        };
        assert_eq!(user_data(&message), None);
    }
}
