//! A character on the pen's opcode: this host's own message, which an
//! established host never sees (docs/01-protocol.md 11.1).
//!
//! **The stream byte says it is a character**: [`MARK`], its low four bits
//! clear. An established host drops a pen message whose stream byte is 3 or
//! more before reading anything else of it.
//!
//! ```text
//! a0  the code point                     a character's: not 0, no surrogate's
//! a1  0
//! a2  0x90 << 24
//! ```
//!
//! **One message per character**, past U+FFFF too: a sequence is its code
//! points as consecutive messages, in order. A character is typed as it
//! arrives and nothing of it is held, so a release has nothing to find.

use crate::control::{Control, op};

/// The stream byte for a character.
pub const MARK: u8 = 0x90;

/// The message's three arguments for one character.
#[must_use]
pub fn arguments(character: char) -> (u32, u32, u32) {
    (u32::from(character), 0, u32::from(MARK) << 24)
}

/// Read a character, or `None` for a message that is not one: another opcode
/// or stream byte, anything but zero where the message carries nothing, or a
/// code point that is no character to type -- 0, a surrogate's, or past
/// U+10FFFF.
#[must_use]
pub fn parse(message: &Control<'_>) -> Option<char> {
    if message.opcode != op::PEN_TOUCH || message.a1 != 0 || message.a2 != u32::from(MARK) << 24 {
        return None;
    }
    char::from_u32(message.a0).filter(|character| *character != '\0')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{CONTROL_HEADER_LEN, encode_header};

    fn message(a0: u32, a1: u32, a2: u32) -> Control<'static> {
        Control {
            a0,
            a1,
            a2,
            opcode: op::PEN_TOUCH,
            body: &[],
        }
    }

    /// **The code point is the first argument, whole**, one past U+FFFF as
    /// well, and the mark alone is the third argument's high byte.
    #[test]
    fn the_code_point_sits_where_the_wire_puts_it() {
        let (a0, a1, a2) = arguments('\u{1F600}');
        let mut buf = [0u8; CONTROL_HEADER_LEN];
        encode_header(&mut buf, &message(a0, a1, a2)).unwrap();
        assert_eq!(
            buf,
            [
                0x00,
                0x01,
                0xF6,
                0x00,
                0,
                0,
                0,
                0,
                0x90,
                0,
                0,
                0,
                op::PEN_TOUCH
            ]
        );
        let read = crate::control::parse(&buf).unwrap();
        assert_eq!(parse(&read), Some('\u{1F600}'));
    }

    /// **Every character reads back as it was written**, U+0001 to U+10FFFF.
    #[test]
    fn every_character_reads_back() {
        for character in (1..=0x10_FFFF).filter_map(char::from_u32) {
            let (a0, a1, a2) = arguments(character);
            assert_eq!(parse(&message(a0, a1, a2)), Some(character));
        }
    }

    /// **Only a character is read**: not 0, a surrogate's code point or one
    /// past the last; not with anything where the message carries nothing;
    /// not on the mark's neighbours, a contact's or a pen's stream byte; not
    /// on another opcode.
    #[test]
    fn only_a_character_is_read() {
        let stream = u32::from(MARK) << 24;
        for code in [0, 0xD800, 0xDBFF, 0xDC00, 0xDFFF, 0x11_0000, u32::MAX] {
            assert_eq!(parse(&message(code, 0, stream)), None, "{code:#x}");
        }
        assert_eq!(
            parse(&message(0x41, 1, stream)),
            None,
            "the second argument"
        );
        for low in [1, 0x100, 0x1_0000] {
            assert_eq!(parse(&message(0x41, 0, stream | low)), None, "{low:#x}");
        }
        for byte in [0x00u32, 0x02, 0x80, 0x8F, 0x91, 0x9F, 0xA0, 0xFF] {
            assert_eq!(
                parse(&message(0x41, 0, byte << 24)),
                None,
                "stream byte {byte:#x}"
            );
        }
        let mut other = message(0x41, 0, stream);
        other.opcode = op::MOUSE_MOTION;
        assert_eq!(parse(&other), None);
    }
}
