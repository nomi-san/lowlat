//! A touch contact on the pen's opcode: this host's own message, which an
//! established host never sees (docs/01-protocol.md 11.1).
//!
//! **The stream byte says it is a contact**: [`MARK`] with the contact's number
//! in its low four bits. An established host drops a pen message whose stream
//! byte is 3 or more before reading anything else of it.
//!
//! ```text
//! a0  x << 16 | y                                        the picture's pixels
//! a1  pressure << 16 | orientation                       0 not reported; degrees
//! a2  (0x80 | id) << 24 | width << 16 | height << 8 | flags
//! ```
//!
//! The size is the contact's across and down, in units of two of the picture's
//! pixels, 0 where not reported; the orientation is [`ORIENTATION_UNKNOWN`]
//! where not reported. **The flags are the contact's state, not its changes**:
//! touching, gone -- lifted, or cancelled -- or neither, hovering.

use crate::control::{Control, op};

/// The stream byte's mark for a contact, its number below.
pub const MARK: u8 = 0x80;

/// The contact numbers the stream byte can carry.
pub const IDS: u8 = 16;

/// The flags byte.
pub mod flag {
    /// The contact is gone: lifted, or with [`CANCELED`] cancelled.
    pub const LEAVE: u8 = 0x01;
    /// The contact is touching the surface.
    pub const TOUCHING: u8 = 0x02;
    /// With [`LEAVE`]: the client cancelled the contact rather than lifting
    /// it, and nothing it did is to count.
    pub const CANCELED: u8 = 0x04;
}

/// An orientation nobody reported.
pub const ORIENTATION_UNKNOWN: u16 = 0xFFFF;

/// One contact at one moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Touch {
    /// Which contact, below [`IDS`].
    pub id: u8,
    /// Where it is, in the picture's own pixels.
    pub x: u16,
    pub y: u16,
    /// How hard it presses, to 1024; 0 not reported.
    pub pressure: u16,
    /// Its turn in degrees below 360; [`ORIENTATION_UNKNOWN`] not reported.
    pub orientation: u16,
    /// Its size across and down, in units of two of the picture's pixels;
    /// 0 not reported.
    pub width: u8,
    pub height: u8,
    /// [`flag`] bits.
    pub flags: u8,
}

impl Touch {
    /// The message's three arguments.
    #[must_use]
    pub fn arguments(&self) -> (u32, u32, u32) {
        (
            (u32::from(self.x) << 16) | u32::from(self.y),
            (u32::from(self.pressure) << 16) | u32::from(self.orientation),
            u32::from_be_bytes([MARK | (self.id & 0x0F), self.width, self.height, self.flags]),
        )
    }
}

/// Read a contact, or `None` for a message that is not one: another opcode,
/// or a stream byte without the mark.
#[must_use]
pub fn parse(message: &Control<'_>) -> Option<Touch> {
    if message.opcode != op::PEN_TOUCH {
        return None;
    }
    let [mark, width, height, flags] = message.a2.to_be_bytes();
    if mark & 0xF0 != MARK {
        return None;
    }
    let [x0, x1, y0, y1] = message.a0.to_be_bytes();
    let [p0, p1, o0, o1] = message.a1.to_be_bytes();
    Some(Touch {
        id: mark & 0x0F,
        x: u16::from_be_bytes([x0, x1]),
        y: u16::from_be_bytes([y0, y1]),
        pressure: u16::from_be_bytes([p0, p1]),
        orientation: u16::from_be_bytes([o0, o1]),
        width,
        height,
        flags,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{CONTROL_HEADER_LEN, encode_header};

    fn message(touch: &Touch) -> Control<'static> {
        let (a0, a1, a2) = touch.arguments();
        Control {
            a0,
            a1,
            a2,
            opcode: op::PEN_TOUCH,
            body: &[],
        }
    }

    /// **Each field sits where the wire puts it**: the horizontal position
    /// high, the mark and the contact's number in the third argument's high
    /// byte, the size and the flags below it.
    #[test]
    fn each_field_sits_where_the_wire_puts_it() {
        let touch = Touch {
            id: 9,
            x: 0x0102,
            y: 0x0304,
            pressure: 0x0405,
            orientation: 0x0106,
            width: 7,
            height: 8,
            flags: flag::TOUCHING,
        };
        let mut buf = [0u8; CONTROL_HEADER_LEN];
        encode_header(&mut buf, &message(&touch)).unwrap();
        assert_eq!(
            buf,
            [
                0x01,
                0x02,
                0x03,
                0x04,
                0x04,
                0x05,
                0x01,
                0x06,
                0x89,
                7,
                8,
                flag::TOUCHING,
                op::PEN_TOUCH
            ]
        );
        let read = crate::control::parse(&buf).unwrap();
        assert_eq!(parse(&read), Some(touch));
    }

    /// **A contact reads back as it was written**, every number and every
    /// flags byte, and the ends of each field.
    #[test]
    fn a_contact_reads_back_as_it_was_written() {
        for id in 0..IDS {
            for flags in 0..=u8::MAX {
                for (x, y, size) in [(0, 0, 0), (u16::MAX, u16::MAX, u8::MAX), (1920, 1079, 20)] {
                    let touch = Touch {
                        id,
                        x,
                        y,
                        pressure: 1024,
                        orientation: ORIENTATION_UNKNOWN,
                        width: size,
                        height: size,
                        flags,
                    };
                    assert_eq!(parse(&message(&touch)), Some(touch));
                }
            }
        }
    }

    /// **Only the mark is a contact**: a pen's stream, the bytes an
    /// established host drops, and the mark's neighbours are not one, nor is
    /// another opcode with a contact's arguments.
    #[test]
    fn only_the_mark_is_a_contact() {
        for byte in [0x00u8, 0x02, 0x03, 0x7F, 0x90, 0x9F, 0xFF] {
            let control = Control {
                a0: 0,
                a1: 0,
                a2: u32::from(byte) << 24,
                opcode: op::PEN_TOUCH,
                body: &[],
            };
            assert_eq!(parse(&control), None, "stream byte {byte:#x}");
        }
        let mut other = message(&Touch::default());
        other.opcode = op::MOUSE_MOTION;
        assert_eq!(parse(&other), None);
    }
}
