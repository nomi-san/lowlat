//! Input: what the application reports, turned into the wire's vocabulary
//! (docs/10-client.md section 8).
//!
//! The application says where it drew the picture and what happened in its
//! window; the mapping here turns window positions into the picture's own
//! pixels, guards what every client guards, and produces the control
//! message. It runs on the session thread, which is where the picture's size
//! is known and the control channel is written.

use lowlat_core::control::{Control, op};
use lowlat_core::video::Rotation;

/// Where the application drew the picture, in the units its positions use.
///
/// A zero rectangle means there is no picture area, and absolute positions
/// are not sent until one is set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Viewport {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// A whole pad at one moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PadState {
    pub buttons: u16,
    pub lx: i16,
    pub ly: i16,
    pub rx: i16,
    pub ry: i16,
    pub lt: u8,
    pub rt: u8,
}

/// What the application reports. Positions are in the window's units.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Input {
    /// A key by its usage code, with the modifier mask in the wire's bits.
    Key {
        code: u32,
        mods: u32,
        pressed: bool,
    },
    /// A mouse button, with where the pointer was.
    Button {
        button: u32,
        pressed: bool,
        x: i32,
        y: i32,
    },
    Wheel {
        x: i32,
        y: i32,
    },
    /// A position, or a delta when relative.
    Motion {
        x: i32,
        y: i32,
        relative: bool,
    },
    PadButton {
        pad: u32,
        button: u32,
        pressed: bool,
    },
    PadAxis {
        pad: u32,
        axis: u32,
        value: i16,
    },
    PadState {
        pad: u32,
        state: PadState,
    },
    PadUnplug {
        pad: u32,
    },
    /// Everything held comes up; the application reports it on losing focus.
    ReleaseAll,
}

/// What travels from the application's thread to the session's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Request {
    Input(Input),
    Viewport(Viewport),
    /// A new declaration: the flags, already masked by capability.
    Video(u32),
}

/// Entries the ring between the two threads holds.
pub(crate) const RING_DEPTH: usize = 1024;

/// The pad states remembered for deduplication, by identifier.
const PADS: usize = 4;

/// One message, ready for the header writer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Wire {
    pub a0: u32,
    pub a1: u32,
    pub a2: u32,
    pub opcode: u8,
    body: [u8; 15],
    body_len: usize,
}

impl Wire {
    const fn bare(opcode: u8, a0: u32, a1: u32, a2: u32) -> Self {
        Self {
            a0,
            a1,
            a2,
            opcode,
            body: [0; 15],
            body_len: 0,
        }
    }

    pub(crate) fn control(&self) -> Control<'_> {
        Control {
            a0: self.a0,
            a1: self.a1,
            a2: self.a2,
            opcode: self.opcode,
            body: self.body.get(..self.body_len).unwrap_or(&[]),
        }
    }
}

/// The mapping from the window to the picture, and the guards around it.
#[derive(Debug, Default)]
pub(crate) struct Mapper {
    viewport: Viewport,
    /// The picture as shown: the stream's size with width and height swapped
    /// for a quarter turn, because a host expects coordinates in the
    /// orientation its output is in. Zero until a picture has arrived.
    picture: (i64, i64),
    pads: [Option<(u32, PadState)>; PADS],
}

/// `value * num / den`, rounded to nearest, for non-negative operands.
const fn scale(value: i64, num: i64, den: i64) -> i64 {
    (value * num * 2 + den) / (den * 2)
}

/// A signed value in an unsigned argument: the wire's convention for deltas,
/// wheel movement and axis values, read back with a narrowing cast.
const fn signed(value: i32) -> u32 {
    u32::from_ne_bytes(value.to_ne_bytes())
}

impl Mapper {
    pub(crate) fn set_viewport(&mut self, viewport: Viewport) {
        self.viewport = viewport;
    }

    pub(crate) fn set_picture(&mut self, width: u16, height: u16, rotation: Rotation) {
        let (w, h) = (i64::from(width), i64::from(height));
        self.picture = match rotation {
            Rotation::Deg90 | Rotation::Deg270 => (h, w),
            _ => (w, h),
        };
    }

    fn mapped(&self) -> bool {
        self.viewport.w > 0 && self.viewport.h > 0 && self.picture.0 > 0 && self.picture.1 > 0
    }

    fn in_picture(&self, x: i32, y: i32) -> bool {
        let v = self.viewport;
        self.mapped() && x >= v.x && x < v.x + v.w && y >= v.y && y < v.y + v.h
    }

    /// One axis into the picture: scaled, clamped, and **one short of the far
    /// edge bumped onto it**, so the edge is reachable from a window whose
    /// last pixel maps just inside it.
    fn axis(window: i32, origin: i32, extent: i32, picture: i64) -> u32 {
        let offset = i64::from(window) - i64::from(origin);
        let mapped = if offset <= 0 {
            0
        } else {
            scale(offset, picture, i64::from(extent))
        };
        let clamped = if mapped >= picture - 1 {
            picture
        } else {
            mapped
        };
        u32::try_from(clamped).unwrap_or(0)
    }

    /// A window position into the picture's pixels.
    fn to_picture(&self, x: i32, y: i32) -> (u32, u32) {
        let v = self.viewport;
        (
            Self::axis(x, v.x, v.w, self.picture.0),
            Self::axis(y, v.y, v.h, self.picture.1),
        )
    }

    /// A delta scaled by the ratio of the picture to the rectangle, so a
    /// picture drawn at half size still turns the host's pointer by the
    /// distance the hand moved.
    fn delta(&self, dx: i32, dy: i32) -> (i32, i32) {
        if !self.mapped() {
            return (dx, dy);
        }
        let v = self.viewport;
        let one = |d: i32, picture: i64, extent: i32| -> i32 {
            let magnitude = scale(i64::from(d).abs(), picture, i64::from(extent));
            let signed = if d < 0 { -magnitude } else { magnitude };
            i32::try_from(signed).unwrap_or(d)
        };
        (one(dx, self.picture.0, v.w), one(dy, self.picture.1, v.h))
    }

    /// A picture position back into the window, for the warp that leaves
    /// relative mode. The inverse of [`Self::to_picture`] without the bump.
    pub(crate) fn to_window(&self, x: u16, y: u16) -> (i32, i32) {
        if !self.mapped() {
            return (i32::from(x), i32::from(y));
        }
        let v = self.viewport;
        let one = |p: u16, picture: i64, origin: i32, extent: i32| -> i32 {
            let offset = scale(i64::from(p), i64::from(extent), picture);
            i32::try_from(i64::from(origin) + offset).unwrap_or(origin)
        };
        (
            one(x, self.picture.0, v.x, v.w),
            one(y, self.picture.1, v.y, v.h),
        )
    }

    /// What one report puts on the wire, or nothing when a rule drops it.
    pub(crate) fn encode(&mut self, input: &Input) -> Option<Wire> {
        match *input {
            // A code of zero names no key.
            Input::Key { code: 0, .. } => None,
            Input::Key {
                code,
                mods,
                pressed,
            } => Some(Wire::bare(op::KEYBOARD, code, mods, u32::from(pressed))),
            // A press outside the picture is not sent; a release always is,
            // so a drag that leaves the window ends cleanly on the host.
            Input::Button {
                pressed: true,
                x,
                y,
                ..
            } if !self.in_picture(x, y) => None,
            Input::Button {
                button, pressed, ..
            } => Some(Wire::bare(op::MOUSE_BUTTON, button, u32::from(pressed), 0)),
            Input::Wheel { x, y } => Some(Wire::bare(op::MOUSE_WHEEL, signed(x), signed(y), 0)),
            Input::Motion {
                x,
                y,
                relative: true,
            } => {
                let (dx, dy) = self.delta(x, y);
                Some(Wire::bare(op::MOUSE_MOTION, 1, signed(dx), signed(dy)))
            }
            Input::Motion { .. } if !self.mapped() => None,
            Input::Motion { x, y, .. } => {
                let (px, py) = self.to_picture(x, y);
                Some(Wire::bare(op::MOUSE_MOTION, 0, px, py))
            }
            Input::PadButton {
                pad,
                button,
                pressed,
            } => Some(Wire::bare(
                op::GAMEPAD_BUTTON,
                button,
                u32::from(pressed),
                pad,
            )),
            Input::PadAxis { pad, axis, value } => Some(Wire::bare(
                op::GAMEPAD_AXIS,
                axis,
                signed(i32::from(value)),
                pad,
            )),
            Input::PadState { pad, state } => {
                if !self.pad_changed(pad, state) {
                    return None;
                }
                let mut wire = Wire::bare(op::GAMEPAD_STATE, pad, 0, 0);
                // Three bytes of padding a host skips, then the fields.
                let [b0, b1] = state.buttons.to_be_bytes();
                let [lx0, lx1] = state.lx.to_be_bytes();
                let [ly0, ly1] = state.ly.to_be_bytes();
                let [rx0, rx1] = state.rx.to_be_bytes();
                let [ry0, ry1] = state.ry.to_be_bytes();
                wire.body = [
                    0, 0, 0, b0, b1, lx0, lx1, ly0, ly1, rx0, rx1, ry0, ry1, state.lt, state.rt,
                ];
                wire.body_len = 15;
                Some(wire)
            }
            Input::PadUnplug { pad } => {
                if let Some(slot) = self
                    .pads
                    .iter_mut()
                    .find(|s| s.is_some_and(|(id, _)| id == pad))
                {
                    *slot = None;
                }
                Some(Wire::bare(op::GAMEPAD_UNPLUG, 0, 0, pad))
            }
            Input::ReleaseAll => Some(Wire::bare(op::RELEASE, 0, 0, 0)),
        }
    }

    /// Whether a pad's state differs from the last one sent for it, recording
    /// it. A pad with no slot left is sent every time.
    fn pad_changed(&mut self, pad: u32, state: PadState) -> bool {
        if let Some((_, last)) = self.pads.iter_mut().flatten().find(|(id, _)| *id == pad) {
            let changed = *last != state;
            *last = state;
            return changed;
        }
        if let Some(slot) = self.pads.iter_mut().find(|s| s.is_none()) {
            *slot = Some((pad, state));
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mapper(viewport: Viewport, width: u16, height: u16, rotation: Rotation) -> Mapper {
        let mut m = Mapper::default();
        m.set_viewport(viewport);
        m.set_picture(width, height, rotation);
        m
    }

    fn motion(m: &mut Mapper, x: i32, y: i32) -> Option<(u32, u32, u32)> {
        m.encode(&Input::Motion {
            x,
            y,
            relative: false,
        })
        .map(|w| (w.a0, w.a1, w.a2))
    }

    /// The picture is 1920x1080 drawn at half size at (10, 20): window
    /// pixels map through the rectangle, the last window pixel reaches the
    /// far edge, and positions past the rectangle clamp.
    #[test]
    fn a_window_position_lands_in_the_picture() {
        let mut m = mapper(
            Viewport {
                x: 10,
                y: 20,
                w: 960,
                h: 540,
            },
            1920,
            1080,
            Rotation::None,
        );
        assert_eq!(motion(&mut m, 10, 20), Some((0, 0, 0)));
        assert_eq!(motion(&mut m, 490, 290), Some((0, 960, 540)));
        // The last pixel of a half-size rectangle is two short by the ratio,
        // and the bump is one pixel only: the host clamps the rest.
        assert_eq!(motion(&mut m, 969, 559), Some((0, 1918, 1078)));
        assert_eq!(motion(&mut m, 5000, -7), Some((0, 1920, 0)));
    }

    /// A stream shown a quarter turn has its extents swapped: coordinates
    /// are in the orientation the output is in.
    #[test]
    fn a_rotated_picture_swaps_its_extents() {
        let mut m = mapper(
            Viewport {
                x: 0,
                y: 0,
                w: 1080,
                h: 1920,
            },
            1920,
            1080,
            Rotation::Deg90,
        );
        assert_eq!(motion(&mut m, 1079, 1919), Some((0, 1080, 1920)));
    }

    /// The bump is only the one pixel short of the edge, not a general
    /// rounding up.
    #[test]
    fn the_bump_is_one_pixel() {
        let mut m = mapper(
            Viewport {
                x: 0,
                y: 0,
                w: 100,
                h: 100,
            },
            100,
            100,
            Rotation::None,
        );
        assert_eq!(motion(&mut m, 98, 98), Some((0, 98, 98)));
        assert_eq!(motion(&mut m, 99, 99), Some((0, 100, 100)));
    }

    /// Without a rectangle, or before a picture, absolute motion is not
    /// sent; deltas and keys still are.
    #[test]
    fn nothing_absolute_goes_out_before_the_rectangle() {
        let mut m = Mapper::default();
        m.set_picture(1920, 1080, Rotation::None);
        assert!(motion(&mut m, 5, 5).is_none());
        assert!(
            m.encode(&Input::Motion {
                x: 3,
                y: -4,
                relative: true
            })
            .is_some()
        );
        assert!(
            m.encode(&Input::Key {
                code: 4,
                mods: 0,
                pressed: true
            })
            .is_some()
        );
        m.set_viewport(Viewport {
            x: 0,
            y: 0,
            w: 10,
            h: 10,
        });
        assert!(motion(&mut m, 5, 5).is_some());
        let mut unshown = Mapper::default();
        unshown.set_viewport(Viewport {
            x: 0,
            y: 0,
            w: 10,
            h: 10,
        });
        assert!(motion(&mut unshown, 5, 5).is_none());
    }

    /// A press outside the rectangle is dropped and its release is sent.
    #[test]
    fn a_press_outside_is_dropped_and_its_release_is_not() {
        let mut m = mapper(
            Viewport {
                x: 100,
                y: 100,
                w: 100,
                h: 100,
            },
            200,
            200,
            Rotation::None,
        );
        let press = |x, y| Input::Button {
            button: 1,
            pressed: true,
            x,
            y,
        };
        assert!(m.encode(&press(50, 150)).is_none());
        assert!(m.encode(&press(200, 150)).is_none());
        assert!(m.encode(&press(199, 199)).is_some());
        let release = Input::Button {
            button: 1,
            pressed: false,
            x: 5000,
            y: 5000,
        };
        assert_eq!(
            m.encode(&release).map(|w| (w.opcode, w.a0, w.a1)),
            Some((1, 1, 0))
        );
    }

    /// Relative deltas are scaled by the picture-to-rectangle ratio, sign
    /// kept, and pass through unscaled before the mapping exists.
    #[test]
    fn deltas_scale_with_the_picture() {
        let mut m = mapper(
            Viewport {
                x: 0,
                y: 0,
                w: 960,
                h: 540,
            },
            1920,
            1080,
            Rotation::None,
        );
        let w = m
            .encode(&Input::Motion {
                x: -3,
                y: 7,
                relative: true,
            })
            .unwrap();
        assert_eq!((w.a0, w.a1 as i32, w.a2 as i32), (1, -6, 14));
    }

    /// A key of code zero is nothing; the mask and the press travel as given.
    #[test]
    fn a_zero_key_code_is_dropped() {
        let mut m = Mapper::default();
        assert!(
            m.encode(&Input::Key {
                code: 0,
                mods: 0x2000,
                pressed: true
            })
            .is_none()
        );
        let w = m
            .encode(&Input::Key {
                code: 4,
                mods: 0x1041,
                pressed: false,
            })
            .unwrap();
        assert_eq!((w.opcode, w.a0, w.a1, w.a2), (0, 4, 0x1041, 0));
    }

    /// The pad state's body: three bytes of padding, then the fields big
    /// endian; an unchanged state for the same pad is not repeated, a
    /// changed one is, and an unplug forgets it.
    #[test]
    fn a_pad_state_is_laid_out_and_deduplicated() {
        let mut m = Mapper::default();
        let state = PadState {
            buttons: 0x1234,
            lx: -2,
            ly: 3,
            rx: -4,
            ry: 5,
            lt: 6,
            rt: 7,
        };
        let w = m.encode(&Input::PadState { pad: 9, state }).unwrap();
        assert_eq!((w.opcode, w.a0, w.a1, w.a2), (23, 9, 0, 0));
        let c = w.control();
        assert_eq!(c.body.len(), 15);
        assert_eq!(
            &c.body[3..],
            &[0x12, 0x34, 0xFF, 0xFE, 0, 3, 0xFF, 0xFC, 0, 5, 6, 7]
        );
        assert!(m.encode(&Input::PadState { pad: 9, state }).is_none());
        assert!(m.encode(&Input::PadState { pad: 10, state }).is_some());
        let moved = PadState { lx: 0, ..state };
        assert!(
            m.encode(&Input::PadState {
                pad: 9,
                state: moved
            })
            .is_some()
        );
        assert!(m.encode(&Input::PadUnplug { pad: 9 }).is_some());
        assert!(
            m.encode(&Input::PadState {
                pad: 9,
                state: moved
            })
            .is_some()
        );
    }

    /// The warp position is the picture position put back through the
    /// rectangle.
    #[test]
    fn the_warp_position_inverts_the_mapping() {
        let m = mapper(
            Viewport {
                x: 10,
                y: 20,
                w: 960,
                h: 540,
            },
            1920,
            1080,
            Rotation::None,
        );
        assert_eq!(m.to_window(0, 0), (10, 20));
        assert_eq!(m.to_window(1920, 1080), (970, 560));
        assert_eq!(m.to_window(960, 540), (490, 290));
    }
}
