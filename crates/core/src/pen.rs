//! A pen on the control channel (docs/01-protocol.md 11.1).
//!
//! One message carries the pen's whole state in the header's three
//! arguments, and no body:
//!
//! ```text
//! a0  x << 16 | y                                       the picture's pixels
//! a1  pressure << 16 | rotation                         to 1024; degrees
//! a2  stream << 24 | tilt x << 16 | tilt y << 8 | flags
//! ```
//!
//! **The horizontal position is the high half**, the other way round from the
//! motion message for a second stream, so neither unpacks the other.
//!
//! **The stream byte also says whether the message is a pen.** A pen names the
//! stream it is over, below [`STREAMS`]; an established host drops a message
//! whose byte is that or more before reading anything else of it, so the
//! byte's higher values carry what this host reads besides a pen.

use crate::control::{Control, op};

/// The flags byte, as the wire sets it.
pub mod flag {
    /// The pen has left the range the surface senses it at.
    pub const LEAVE: u8 = 0x01;
    /// The pen is touching the surface.
    pub const TOUCHING: u8 = 0x02;
    /// The pen is turned over, its eraser towards the surface.
    pub const INVERTED: u8 = 0x04;
    /// The eraser is touching the surface.
    pub const ERASER: u8 = 0x08;
    /// The first barrel button is held.
    pub const BARREL: u8 = 0x10;
    /// The second barrel button is held.
    pub const BARREL_2: u8 = 0x20;
    /// One of the pen's tips is touching the surface.
    pub const TIP: u8 = 0x40;
    /// A barrel button was double clicked.
    pub const DOUBLE_CLICK: u8 = 0x80;
}

/// The streams a pen names. A stream byte at or past this is not a pen.
pub const STREAMS: u8 = 3;

/// The most pressure says.
pub const PRESSURE_MAX: u16 = 1024;

/// Rotation runs in degrees below this.
pub const ROTATION_SPAN: u16 = 360;

/// The most a tilt leans either way, in degrees.
pub const TILT_MAX: i8 = 90;

/// A pen at one moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Pen {
    /// Where it is, in the picture's own pixels.
    pub x: u16,
    pub y: u16,
    /// How hard it presses, up to [`PRESSURE_MAX`].
    pub pressure: u16,
    /// Its turn about its own length, in degrees below [`ROTATION_SPAN`].
    pub rotation: u16,
    /// The stream it is over, below [`STREAMS`].
    pub stream: u8,
    /// How far it leans to the right and towards the user, in degrees
    /// within [`TILT_MAX`].
    pub tilt_x: i8,
    pub tilt_y: i8,
    /// [`flag`] bits.
    pub flags: u8,
}

impl Pen {
    /// The message's three arguments.
    #[must_use]
    pub fn arguments(&self) -> (u32, u32, u32) {
        let [tilt_x] = self.tilt_x.to_ne_bytes();
        let [tilt_y] = self.tilt_y.to_ne_bytes();
        (
            (u32::from(self.x) << 16) | u32::from(self.y),
            (u32::from(self.pressure) << 16) | u32::from(self.rotation),
            u32::from_be_bytes([self.stream, tilt_x, tilt_y, self.flags]),
        )
    }
}

/// Read a pen, or `None` for a message that is not one: another opcode, or a
/// stream byte at or past [`STREAMS`].
#[must_use]
pub fn parse(message: &Control<'_>) -> Option<Pen> {
    if message.opcode != op::PEN_TOUCH {
        return None;
    }
    let [stream, tilt_x, tilt_y, flags] = message.a2.to_be_bytes();
    if stream >= STREAMS {
        return None;
    }
    let [x0, x1, y0, y1] = message.a0.to_be_bytes();
    let [p0, p1, r0, r1] = message.a1.to_be_bytes();
    Some(Pen {
        x: u16::from_be_bytes([x0, x1]),
        y: u16::from_be_bytes([y0, y1]),
        pressure: u16::from_be_bytes([p0, p1]),
        rotation: u16::from_be_bytes([r0, r1]),
        stream,
        tilt_x: i8::from_ne_bytes([tilt_x]),
        tilt_y: i8::from_ne_bytes([tilt_y]),
        flags,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{CONTROL_HEADER_LEN, encode_header};

    fn message(pen: &Pen) -> Control<'static> {
        let (a0, a1, a2) = pen.arguments();
        Control {
            a0,
            a1,
            a2,
            opcode: op::PEN_TOUCH,
            body: &[],
        }
    }

    /// **Each field sits where the wire puts it**: the horizontal position in
    /// the first argument's high half, the stream in the third's high byte,
    /// a negative tilt as its two's complement byte.
    #[test]
    fn each_field_sits_where_the_wire_puts_it() {
        let pen = Pen {
            x: 0x0102,
            y: 0x0304,
            pressure: 0x0405,
            rotation: 0x0107,
            stream: 2,
            tilt_x: -3,
            tilt_y: 4,
            flags: flag::TOUCHING | flag::BARREL,
        };
        let mut buf = [0u8; CONTROL_HEADER_LEN];
        encode_header(&mut buf, &message(&pen)).unwrap();
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
                0x07,
                2,
                0xFD,
                4,
                0x12,
                op::PEN_TOUCH
            ]
        );
        let read = crate::control::parse(&buf).unwrap();
        assert_eq!(parse(&read), Some(pen));
    }

    /// **A pen reads back as it was written**, every flags byte, every
    /// stream, and the ends of each field.
    #[test]
    fn a_pen_reads_back_as_it_was_written() {
        for flags in 0..=u8::MAX {
            for (x, y) in [(0, 0), (u16::MAX, u16::MAX), (1920, 1079)] {
                for tilt in [-TILT_MAX, -1, 0, 1, TILT_MAX, i8::MIN, i8::MAX] {
                    for stream in 0..STREAMS {
                        let pen = Pen {
                            x,
                            y,
                            pressure: PRESSURE_MAX,
                            rotation: ROTATION_SPAN - 1,
                            stream,
                            tilt_x: tilt,
                            tilt_y: tilt.saturating_neg(),
                            flags,
                        };
                        assert_eq!(parse(&message(&pen)), Some(pen));
                    }
                }
            }
        }
    }

    /// **A stream byte at or past the pen's streams is not a pen**, nor is
    /// another opcode with a pen's arguments.
    #[test]
    fn a_stream_byte_past_the_pens_streams_is_not_a_pen() {
        for stream in [STREAMS, 0x7F, 0x80, 0x8F, 0x90, u8::MAX] {
            let control = Control {
                a0: 0,
                a1: 0,
                a2: u32::from(stream) << 24,
                opcode: op::PEN_TOUCH,
                body: &[],
            };
            assert_eq!(parse(&control), None, "stream byte {stream:#x}");
        }
        let mut other = message(&Pen::default());
        other.opcode = op::MOUSE_MOTION;
        assert_eq!(parse(&other), None);
    }
}
