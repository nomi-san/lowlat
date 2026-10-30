//! Input: what the application reports, turned into the wire's vocabulary
//! (docs/10-client.md section 8).
//!
//! The application says where it drew the picture and what happened in its
//! window; the mapping here turns window positions into the picture's own
//! pixels, guards what every client guards, and produces the control
//! message. It runs on the session thread, which is where the picture's size
//! is known and the control channel is written.

use lowlat_core::control::{Control, op};
use lowlat_core::microphone;
use lowlat_core::pad::{self, Product};
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

/// A whole pad at one moment, as the whole-pad message carries it. The
/// core's, which also reads a controller's own report into it.
pub type PadState = pad::State;

/// Which of a pad's reports the application handed over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportKind {
    /// An input report: what the pad is doing.
    Input,
    /// A feature report the pad answered: calibration or firmware, sent
    /// ahead of the first input report.
    Feature,
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
    /// A DualShock 4's or a DualSense's own report, already in the USB form
    /// (docs/01-protocol.md 11.1): an input report of [`pad::INPUT_LEN`]
    /// bytes, or a feature report of `len` bytes, and how the pad is
    /// attached, which decides the framing of what comes back.
    PadReport {
        pad: u32,
        product: Product,
        kind: ReportKind,
        transport: pad::Transport,
        len: u8,
        report: [u8; pad::INPUT_LEN],
    },
    /// Everything held comes up; the application reports it on losing focus.
    ReleaseAll,
    /// The application's own message: a header and no body, sent exactly as
    /// given, its arguments big endian as every header's are. Nothing here
    /// reads it, so an opcode the library also speaks changes nothing the
    /// library keeps -- one on the unplug's opcode forgets no pad.
    Control {
        opcode: u8,
        a0: u32,
        a1: u32,
        a2: u32,
    },
}

/// Whether the application may send `opcode` as its own message. Not one the
/// library sends for the session itself -- its departure, the initialization,
/// the declaration and the keyframe request, its decode-time report,
/// diagnostics -- which sent behind its back leaves the host and the library
/// disagreeing about the session; and not one whose message has a body, which
/// a header alone misframes.
pub(crate) const fn sendable(opcode: u8) -> bool {
    !matches!(
        opcode,
        op::DISCONNECT
            | op::INIT
            | op::ENCODER_CONFIG
            | op::ENCODE_LATENCY
            | op::DIAGNOSTICS
            | op::USER_DATA
            | op::GAMEPAD_STATE
            | op::PAD_REPORT
            | op::VIRTUAL_DEVICE
    )
}

/// What travels from the application's thread to the session's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Request {
    Input(Input),
    Viewport(Viewport),
    /// A new declaration: the flags, already masked by capability.
    Video(u32),
    /// Another decoder, chosen by the application and already probed: the
    /// declaration as the new decoder masks it. The choice itself waits for
    /// the decode thread where the seam left it.
    Decoder(u32),
}

/// Entries the ring between the two threads holds.
pub(crate) const RING_DEPTH: usize = 1024;

/// Ten milliseconds of the application's microphone, mono: what crosses to the
/// session thread.
pub(crate) type MicFrame = [i16; microphone::FRAME];

/// Frames the microphone's own ring holds: two hundred milliseconds. Beside
/// the input's rather than in it, whose entries are a few dozen bytes.
pub(crate) const MICROPHONE_DEPTH: usize = 20;

/// Cuts what the application hands over into whole frames, the remainder
/// carried to the next call: **a receiver drops a tail that is not a whole
/// frame** (docs/01-protocol.md 11.4b), so the lengths a capture device happens
/// to deliver are never passed on.
#[derive(Debug, Clone)]
pub(crate) struct Framer {
    frame: MicFrame,
    held: usize,
}

impl Default for Framer {
    fn default() -> Self {
        Self {
            frame: [0; microphone::FRAME],
            held: 0,
        }
    }
}

impl Framer {
    /// Take samples, handing every frame they complete to `whole`, in order.
    pub(crate) fn feed(&mut self, mut samples: &[i16], mut whole: impl FnMut(&MicFrame)) {
        while !samples.is_empty() {
            let take = (microphone::FRAME - self.held).min(samples.len());
            if let (Some(dst), Some(src)) = (
                self.frame.get_mut(self.held..self.held + take),
                samples.get(..take),
            ) {
                dst.copy_from_slice(src);
            }
            self.held += take;
            samples = samples.get(take..).unwrap_or(&[]);
            if self.held == microphone::FRAME {
                whole(&self.frame);
                self.held = 0;
            }
        }
    }

    /// Forget a partial frame: the host stopped taking the microphone.
    pub(crate) fn clear(&mut self) {
        self.held = 0;
    }
}

/// The pad states remembered for deduplication, by identifier.
const PADS: usize = 4;

/// The largest body a message here carries: a controller's report.
const BODY_MAX: usize = pad::INPUT_LEN;

/// One message, ready for the header writer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Wire {
    pub a0: u32,
    pub a1: u32,
    pub a2: u32,
    pub opcode: u8,
    body: [u8; BODY_MAX],
    body_len: usize,
}

impl Wire {
    const fn bare(opcode: u8, a0: u32, a1: u32, a2: u32) -> Self {
        Self {
            a0,
            a1,
            a2,
            opcode,
            body: [0; BODY_MAX],
            body_len: 0,
        }
    }

    /// A message whose first argument is its body's length.
    fn with_body(opcode: u8, a1: u32, a2: u32, body: &[u8]) -> Self {
        let mut wire = Self::bare(opcode, u32::try_from(body.len()).unwrap_or(0), a1, a2);
        let len = body.len().min(BODY_MAX);
        if let (Some(dst), Some(src)) = (wire.body.get_mut(..len), body.get(..len)) {
            dst.copy_from_slice(src);
        }
        wire.body_len = len;
        wire
    }

    /// The whole-pad message names its pad in the first argument, not the
    /// length.
    const fn named(mut self, pad: u32) -> Self {
        self.a0 = pad;
        self
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

/// What is remembered of a pad sent as its own report: the product, how it
/// is attached, and the sequence a wireless DualSense's output reports
/// carry. And for a wireless DualSense, whether its lights have been reset
/// since it connected, and the host's last light write before they were.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Raw {
    product: Product,
    transport: pad::Transport,
    seq: u8,
    settled: bool,
    lights: Option<Lights>,
}

/// A light write a wireless DualSense ignored, kept to be written again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Lights {
    report: [u8; pad::DS5_OUTPUT_LEN],
    len: usize,
}

/// One pad the application has named: the last state sent for it, and what
/// its reports said about it when it is sent as reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Slot {
    id: u32,
    /// The last state sent, once one has been.
    last: Option<PadState>,
    raw: Option<Raw>,
}

/// The mapping from the window to the picture, and the guards around it.
#[derive(Debug, Default)]
pub(crate) struct Mapper {
    viewport: Viewport,
    /// The picture as shown: the stream's size with width and height swapped
    /// for a quarter turn, because a host expects coordinates in the
    /// orientation its output is in. Zero until a picture has arrived.
    picture: (i64, i64),
    pads: [Option<Slot>; PADS],
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

/// One axis's value as the per-axis message carries it.
///
/// **The message's vertical axes point down** (docs/01-protocol.md section
/// 11.1), opposite to the whole state's, which is the direction the call
/// takes: away from the player is positive to an application either way, and
/// is turned over here, the far end of the range held inside it.
fn wire_axis(axis: u32, value: i16) -> i16 {
    match axis {
        // The left and right sticks' vertical axes.
        1 | 3 => value.saturating_neg(),
        _ => value,
    }
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
            // **A delta goes as the device reported it**, whatever size the
            // picture is drawn at: it is the hand's motion in the mouse's own
            // counts, not a distance in the window, and the host moves its
            // pointer by it as by a mouse of its own. Scaled by the drawn
            // ratio, a picture stretched to a larger window dragged and
            // aimed slower than the hand.
            Input::Motion {
                x,
                y,
                relative: true,
            } => Some(Wire::bare(op::MOUSE_MOTION, 1, signed(x), signed(y))),
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
                signed(i32::from(wire_axis(axis, value))),
                pad,
            )),
            Input::PadState { pad, state } => self.pad_state(pad, state),
            Input::PadUnplug { pad } => {
                if let Some(slot) = self
                    .pads
                    .iter_mut()
                    .find(|s| s.is_some_and(|slot| slot.id == pad))
                {
                    *slot = None;
                }
                Some(Wire::bare(op::GAMEPAD_UNPLUG, 0, 0, pad))
            }
            // A report is several messages; the driver asks for them apart.
            Input::PadReport { .. } => None,
            Input::ReleaseAll => Some(Wire::bare(op::RELEASE, 0, 0, 0)),
            Input::Control { opcode, a0, a1, a2 } => Some(Wire::bare(opcode, a0, a1, a2)),
        }
    }

    /// The whole-pad state message for one pad, or `None` when nothing
    /// changed since the last one sent for it.
    fn pad_state(&mut self, pad: u32, state: PadState) -> Option<Wire> {
        if !self.pad_changed(pad, state) {
            return None;
        }
        let [b0, b1] = state.buttons.to_be_bytes();
        let [lx0, lx1] = state.lx.to_be_bytes();
        let [ly0, ly1] = state.ly.to_be_bytes();
        let [rx0, rx1] = state.rx.to_be_bytes();
        let [ry0, ry1] = state.ry.to_be_bytes();
        // Three bytes of padding a host skips, then the fields.
        Some(
            Wire::with_body(
                op::GAMEPAD_STATE,
                0,
                0,
                &[
                    0, 0, 0, b0, b1, lx0, lx1, ly0, ly1, rx0, rx1, ry0, ry1, state.lt, state.rt,
                ],
            )
            .named(pad),
        )
    }

    /// What one of a pad's own reports puts on the wire, in order
    /// (docs/10-client.md section 8): the report itself; for a DualShock 4
    /// the touch block an established host's DualShock mode reads; and,
    /// for an input report, the standard state it implies when that changed
    /// -- so a host that reads no reports still has a pad, and one that
    /// does has its slot. A feature report is one message. The product and
    /// the transport are remembered for what comes back.
    pub(crate) fn encode_report(
        &mut self,
        pad: u32,
        product: Product,
        kind: ReportKind,
        transport: pad::Transport,
        report: &[u8],
    ) -> [Option<Wire>; 3] {
        let feature = kind == ReportKind::Feature;
        // A feature report says nothing about the transport; the input
        // reports do, and a late feature report must not unsay it.
        self.remember_raw(pad, product, (!feature).then_some(transport));
        let a2 = u32::from(product.product_id()) | if feature { pad::FEATURE_BIT } else { 0 };
        if feature {
            return [
                Some(Wire::with_body(op::PAD_REPORT, pad, a2, report)),
                None,
                None,
            ];
        }
        let Ok(usb) = <&[u8; pad::INPUT_LEN]>::try_from(report) else {
            return [None, None, None];
        };
        let (body, block): (&[u8], Option<Wire>) = match product {
            Product::DualShock4 => (
                pad::ds4_body(usb),
                Some(Wire::with_body(
                    op::PAD_REPORT,
                    pad,
                    0,
                    &pad::ds4_touch_block(usb),
                )),
            ),
            Product::DualSense => (usb, None),
            _ => (usb, None),
        };
        [
            Some(Wire::with_body(op::PAD_REPORT, pad, a2, body)),
            block,
            self.pad_state(pad, pad::state(product, usb)),
        ]
    }

    /// Frame what the host's device was written for the pad it names: an
    /// output report in the pad's own framing, a feature write as it is.
    /// `None` for a pad this mapper never sent as reports.
    pub(crate) fn frame_output(
        &mut self,
        pad: u32,
        kind: pad::OutputKind,
        report: &[u8],
        out: &mut [u8; pad::REPORT_MAX],
    ) -> Option<usize> {
        let slot = self.pads.iter_mut().flatten().find(|s| s.id == pad)?;
        let raw = slot.raw.as_mut()?;
        match kind {
            pad::OutputKind::Output => {
                let len =
                    pad::frame_output(raw.product, raw.transport, raw.seq, report, out).ok()?;
                raw.seq = raw.seq.wrapping_add(1) & 0x0F;
                if raw.product == Product::DualSense
                    && raw.transport == pad::Transport::Bluetooth
                    && !raw.settled
                    && pad::ds5_sets_lights(report)
                {
                    let mut kept = [0u8; pad::DS5_OUTPUT_LEN];
                    if let Some(dst) = kept.get_mut(..report.len()) {
                        dst.copy_from_slice(report);
                        raw.lights = Some(Lights {
                            report: kept,
                            len: report.len(),
                        });
                    }
                }
                Some(len)
            }
            pad::OutputKind::Feature => {
                let dst = out.get_mut(..report.len())?;
                dst.copy_from_slice(report);
                Some(report.len())
            }
        }
    }

    /// A wireless DualSense takes no light write after it connects until a
    /// report resets its lights, which a host's writes never do. So once a
    /// connection, at the first input report with the pad's clock past its
    /// connection's light animation, the pad is handed a report resetting
    /// the lights, and behind it the host's last light write from before,
    /// which the pad ignored; what the host writes after goes as it is. In
    /// order, the reports to hand out, each framed in `out`.
    pub(crate) fn settle_lights(
        &mut self,
        pad: u32,
        report: &[u8; pad::INPUT_LEN],
        out: &mut [[u8; pad::REPORT_MAX]; 2],
    ) -> [Option<usize>; 2] {
        let Some(raw) = self
            .pads
            .iter_mut()
            .flatten()
            .find(|s| s.id == pad)
            .and_then(|slot| slot.raw.as_mut())
        else {
            return [None, None];
        };
        if raw.product != Product::DualSense
            || raw.transport != pad::Transport::Bluetooth
            || raw.settled
            || pad::ds5_clock(report) < pad::DS5_LIGHTS_SETTLED
        {
            return [None, None];
        }
        raw.settled = true;
        let lights = raw.lights.take();
        let [reset_out, lights_out] = out;
        let mut frame = |report: &[u8], out: &mut [u8; pad::REPORT_MAX]| {
            let len = pad::frame_output(raw.product, raw.transport, raw.seq, report, out).ok()?;
            raw.seq = raw.seq.wrapping_add(1) & 0x0F;
            Some(len)
        };
        let reset = frame(&pad::ds5_reset_lights(), reset_out);
        let written = lights
            .and_then(|lights| frame(lights.report.get(..lights.len)?, lights_out))
            .filter(|_| reset.is_some());
        [reset, written]
    }

    /// The slot for a pad, made if there is room.
    fn slot(&mut self, pad: u32) -> Option<&mut Slot> {
        if let Some(index) = self
            .pads
            .iter()
            .position(|s| s.is_some_and(|s| s.id == pad))
        {
            return self.pads.get_mut(index).and_then(Option::as_mut);
        }
        let free = self.pads.iter().position(Option::is_none)?;
        let slot = self.pads.get_mut(free)?;
        *slot = Some(Slot {
            id: pad,
            last: None,
            raw: None,
        });
        slot.as_mut()
    }

    fn remember_raw(&mut self, pad: u32, product: Product, transport: Option<pad::Transport>) {
        if let Some(slot) = self.slot(pad) {
            let (seq, known) = slot
                .raw
                .map_or((0, pad::Transport::Usb), |raw| (raw.seq, raw.transport));
            let transport = transport.unwrap_or(known);
            // A pad that came back on another transport is a new connection.
            let (settled, lights) = slot
                .raw
                .filter(|raw| raw.transport == transport && raw.product == product)
                .map_or((false, None), |raw| (raw.settled, raw.lights));
            slot.raw = Some(Raw {
                product,
                transport,
                seq,
                settled,
                lights,
            });
        }
    }

    /// Whether a pad's state differs from the last one sent for it, recording
    /// it. A pad with no slot left is sent every time.
    fn pad_changed(&mut self, pad: u32, state: PadState) -> bool {
        let Some(slot) = self.slot(pad) else {
            return true;
        };
        let changed = slot.last != Some(state);
        slot.last = Some(state);
        changed
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

    /// **A vertical axis goes on the wire pointing down**, the call taking the
    /// whole state's direction: away from the player is positive to the
    /// application and negative on the wire; the horizontal axes and the
    /// triggers pass as they are, and the far end stays inside the range.
    #[test]
    fn a_vertical_pad_axis_points_down_on_the_wire() {
        let mut m = Mapper::default();
        let mut axis = |axis, value| {
            m.encode(&Input::PadAxis {
                pad: 2,
                axis,
                value,
            })
            .map(|w| (w.opcode, w.a0, w.a1, w.a2))
        };
        let on_wire = |axis, value: i32| Some((op::GAMEPAD_AXIS, axis, signed(value), 2));
        assert_eq!(axis(1, 20_000), on_wire(1, -20_000));
        assert_eq!(axis(3, -20_000), on_wire(3, 20_000));
        assert_eq!(axis(0, 20_000), on_wire(0, 20_000));
        assert_eq!(axis(2, -5), on_wire(2, -5));
        assert_eq!(axis(4, 20_000), on_wire(4, 20_000));
        assert_eq!(axis(1, i16::MIN), on_wire(1, i32::from(i16::MAX)));
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

    /// Relative deltas go as the device reported them, sign kept, with the
    /// picture drawn at half its size, at twice it, and before any mapping
    /// exists.
    #[test]
    fn deltas_pass_through_whatever_the_picture_is_drawn_at() {
        let at = |w: i32, h: i32| mapper(Viewport { x: 0, y: 0, w, h }, 1920, 1080, Rotation::None);
        for mut m in [at(960, 540), at(3840, 2160), Mapper::default()] {
            for (x, y) in [(-3, 7), (1, -1), (-120, 45)] {
                let w = m
                    .encode(&Input::Motion {
                        x,
                        y,
                        relative: true,
                    })
                    .unwrap();
                assert_eq!((w.a0, w.a1 as i32, w.a2 as i32), (1, x, y));
            }
        }
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

    /// The application's own message goes out exactly as given, its arguments
    /// big endian as every header's are: a caller whose peer reads them the
    /// other way round swaps them itself, and these bytes are its own. It
    /// needs neither a rectangle nor a picture.
    #[test]
    fn an_application_message_goes_out_as_given() {
        let mut m = Mapper::default();
        let w = m
            .encode(&Input::Control {
                opcode: 6,
                a0: 0x10FF_FFFF,
                a1: 0xA11E_0000,
                a2: 0x0100_0000,
            })
            .unwrap();
        let mut header = [0u8; lowlat_core::control::CONTROL_HEADER_LEN];
        lowlat_core::control::encode_header(&mut header, &w.control()).unwrap();
        assert_eq!(
            header,
            [
                0x10, 0xFF, 0xFF, 0xFF, 0xA1, 0x1E, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x06
            ]
        );
        assert!(w.control().body.is_empty());
    }

    /// One on the unplug's opcode forgets no pad: the same state is still not
    /// repeated after it, where after a real unplug it is.
    #[test]
    fn an_application_message_forgets_no_pad() {
        let mut m = Mapper::default();
        let state = PadState {
            buttons: 1,
            ..PadState::default()
        };
        assert!(m.encode(&Input::PadState { pad: 0, state }).is_some());
        assert!(
            m.encode(&Input::Control {
                opcode: op::GAMEPAD_UNPLUG,
                a0: 0,
                a1: 0,
                a2: 0,
            })
            .is_some()
        );
        assert!(
            m.encode(&Input::PadState { pad: 0, state }).is_none(),
            "the application's message forgot the pad"
        );
        assert!(m.encode(&Input::PadUnplug { pad: 0 }).is_some());
        assert!(m.encode(&Input::PadState { pad: 0, state }).is_some());
    }

    /// Whatever lengths the application hands over, frames go on whole and in
    /// order, the remainder carried to the next call; a cleared remainder is
    /// not sent with the next frame.
    #[test]
    fn the_microphone_is_cut_into_whole_frames() {
        let samples: Vec<i16> = (0..1500).map(|i| i16::try_from(i).unwrap()).collect();
        let mut framer = Framer::default();
        let mut frames: Vec<MicFrame> = Vec::new();
        let mut at = 0;
        for len in [300, 300, 360, 540] {
            framer.feed(&samples[at..at + len], |frame| frames.push(*frame));
            at += len;
        }
        assert_eq!(frames.len(), 3, "1500 samples are three frames and 60 held");
        assert_eq!((frames[0][0], frames[0][479]), (0, 479));
        assert_eq!((frames[1][0], frames[1][479]), (480, 959));
        assert_eq!((frames[2][0], frames[2][479]), (960, 1439));

        framer.clear();
        frames.clear();
        framer.feed(&[7; 480], |frame| frames.push(*frame));
        assert_eq!(frames.len(), 1);
        assert!(
            frames[0].iter().all(|&s| s == 7),
            "the cleared remainder came back"
        );
    }

    const DS5_IDLE: &[u8; pad::INPUT_LEN] =
        include_bytes!("../../core/tests/data/pad/ds5/input-idle.bin");
    const DS5_HELD: &[u8; pad::INPUT_LEN] =
        include_bytes!("../../core/tests/data/pad/ds5/input-held.bin");
    const DS4_IDLE: &[u8; pad::INPUT_LEN] =
        include_bytes!("../../core/tests/data/pad/ds4/input-idle.bin");
    const DS5_CALIBRATION: &[u8] =
        include_bytes!("../../core/tests/data/pad/ds5/feature-calibration.bin");

    fn wires(reports: [Option<Wire>; 3]) -> Vec<(u8, u32, u32, u32, usize)> {
        reports
            .iter()
            .flatten()
            .map(|w| (w.opcode, w.a0, w.a1, w.a2, w.body_len))
            .collect()
    }

    /// A DualSense's report goes out as itself and as the state it implies,
    /// in that order; the same state again goes out as the report alone; a
    /// changed one brings the state back.
    #[test]
    fn a_dualsense_report_is_the_report_then_the_state_it_implies() {
        let mut m = Mapper::default();
        let first = m.encode_report(
            3,
            Product::DualSense,
            ReportKind::Input,
            pad::Transport::Usb,
            DS5_IDLE,
        );
        assert_eq!(
            wires(first),
            vec![
                (op::PAD_REPORT, 64, 3, 0x0CE6, 64),
                (op::GAMEPAD_STATE, 3, 0, 0, 15)
            ]
        );
        assert_eq!(first[0].unwrap().control().body, DS5_IDLE);
        let again = m.encode_report(
            3,
            Product::DualSense,
            ReportKind::Input,
            pad::Transport::Usb,
            DS5_IDLE,
        );
        assert_eq!(wires(again), vec![(op::PAD_REPORT, 64, 3, 0x0CE6, 64)]);
        let held = m.encode_report(
            3,
            Product::DualSense,
            ReportKind::Input,
            pad::Transport::Usb,
            DS5_HELD,
        );
        assert_eq!(wires(held).len(), 2);
        let state = held[2].unwrap();
        assert_eq!(&state.control().body[3..5], &pad::bit::A.to_be_bytes());
    }

    /// A DualShock 4's report goes out without its identifier byte, then as
    /// the ten-byte block an established host reads under no product, then
    /// as the state.
    #[test]
    fn a_dualshock_report_travels_as_body_block_and_state() {
        let mut m = Mapper::default();
        let out = m.encode_report(
            5,
            Product::DualShock4,
            ReportKind::Input,
            pad::Transport::Usb,
            DS4_IDLE,
        );
        assert_eq!(
            wires(out),
            vec![
                (op::PAD_REPORT, 63, 5, 0x09CC, 63),
                (op::PAD_REPORT, 10, 5, 0, 10),
                (op::GAMEPAD_STATE, 5, 0, 0, 15)
            ]
        );
        assert_eq!(out[0].unwrap().control().body, &DS4_IDLE[1..]);
        assert_eq!(out[1].unwrap().control().body, &DS4_IDLE[33..43]);
    }

    /// A feature report is one message with the feature bit, and it says
    /// nothing about the transport: the one the input reports gave stays.
    #[test]
    fn a_feature_report_is_one_message_and_keeps_the_transport() {
        let mut m = Mapper::default();
        let out = m.encode_report(
            3,
            Product::DualSense,
            ReportKind::Feature,
            pad::Transport::Usb,
            DS5_CALIBRATION,
        );
        assert_eq!(
            wires(out),
            vec![(op::PAD_REPORT, 41, 3, 0x0CE6 | pad::FEATURE_BIT, 41)]
        );
        m.encode_report(
            3,
            Product::DualSense,
            ReportKind::Input,
            pad::Transport::Bluetooth,
            DS5_IDLE,
        );
        m.encode_report(
            3,
            Product::DualSense,
            ReportKind::Feature,
            pad::Transport::Usb,
            DS5_CALIBRATION,
        );
        let mut framed = [0u8; pad::REPORT_MAX];
        let mut report = [0u8; pad::DS5_OUTPUT_MIN_LEN];
        report[0] = pad::DS5_OUTPUT_ID;
        assert_eq!(
            m.frame_output(3, pad::OutputKind::Output, &report, &mut framed),
            Some(pad::BT_OUTPUT_LEN)
        );
    }

    /// What comes back is framed for the pad's transport, a wireless
    /// DualSense's sequence advancing per report; a feature write passes as
    /// it is; a pad never sent as reports has no framing.
    #[test]
    fn output_is_framed_for_the_pad_it_names() {
        let mut m = Mapper::default();
        let mut framed = [0u8; pad::REPORT_MAX];
        let mut report = [0u8; pad::DS5_OUTPUT_MIN_LEN];
        report[0] = pad::DS5_OUTPUT_ID;
        report[3] = 200;
        assert_eq!(
            m.frame_output(3, pad::OutputKind::Output, &report, &mut framed),
            None
        );
        m.encode_report(
            3,
            Product::DualSense,
            ReportKind::Input,
            pad::Transport::Usb,
            DS5_IDLE,
        );
        assert_eq!(
            m.frame_output(3, pad::OutputKind::Output, &report, &mut framed),
            Some(pad::DS5_OUTPUT_MIN_LEN)
        );
        assert_eq!(&framed[..pad::DS5_OUTPUT_MIN_LEN], &report);
        assert_eq!(
            m.frame_output(3, pad::OutputKind::Feature, &report[..5], &mut framed),
            Some(5)
        );
        assert_eq!(&framed[..5], &report[..5]);

        m.encode_report(
            4,
            Product::DualSense,
            ReportKind::Input,
            pad::Transport::Bluetooth,
            DS5_IDLE,
        );
        let mut seqs = Vec::new();
        for _ in 0..17 {
            assert_eq!(
                m.frame_output(4, pad::OutputKind::Output, &report, &mut framed),
                Some(pad::BT_OUTPUT_LEN)
            );
            seqs.push(framed[1] >> 4);
        }
        assert_eq!(seqs[..3], [0, 1, 2]);
        assert_eq!(seqs[15..], [15, 0]);
        assert_eq!(framed[0], 0x31);
        assert_eq!(&framed[3..50], &report[1..]);

        // The wrong product's report for the pad is refused.
        let mut wrong = [0u8; pad::DS4_OUTPUT_LEN];
        wrong[0] = pad::DS4_OUTPUT_ID;
        assert_eq!(
            m.frame_output(4, pad::OutputKind::Output, &wrong, &mut framed),
            None
        );
    }

    /// A wireless DualSense takes no light write after it connects until a
    /// report of its own resets its lights: at the first input report with the
    /// pad's clock past its connection's light animation it is handed that
    /// reset, once a connection, and behind it the host's last light write
    /// from before; later writes go as they are. A pad on USB never is, and
    /// one back on another transport is a new connection.
    #[test]
    fn a_wireless_dualsense_has_its_lights_reset_once_a_connection() {
        let at = |clock: u32| {
            let mut input = *DS5_IDLE;
            input[28..32].copy_from_slice(&clock.to_le_bytes());
            input
        };
        let early = at(pad::DS5_LIGHTS_SETTLED - 3_000_000);
        let late = at(pad::DS5_LIGHTS_SETTLED);
        let mut lights = [0u8; pad::DS5_OUTPUT_MIN_LEN];
        lights[0] = pad::DS5_OUTPUT_ID;
        lights[2] = 0x14;
        lights[44..48].copy_from_slice(&[0x04, 0, 64, 255]);
        let mut motors = [0u8; pad::DS5_OUTPUT_MIN_LEN];
        motors[0] = pad::DS5_OUTPUT_ID;
        motors[1] = 0x03;
        motors[3] = 200;
        let mut framed = [0u8; pad::REPORT_MAX];
        let mut again = [[0u8; pad::REPORT_MAX]; 2];
        let wireless = pad::Transport::Bluetooth;
        let output = pad::OutputKind::Output;

        let nothing = [None, None];
        let reset = [Some(pad::BT_OUTPUT_LEN), None];
        let both = [Some(pad::BT_OUTPUT_LEN); 2];

        // A light write while the animation plays is kept, and handed out
        // again behind the reset, in sequence after the two before.
        let mut m = Mapper::default();
        m.encode_report(5, Product::DualSense, ReportKind::Input, wireless, &early);
        assert_eq!(m.settle_lights(5, &early, &mut again), nothing);
        assert!(m.frame_output(5, output, &lights, &mut framed).is_some());
        assert!(m.frame_output(5, output, &motors, &mut framed).is_some());
        assert_eq!(m.settle_lights(5, &early, &mut again), nothing);
        assert_eq!(m.settle_lights(5, &late, &mut again), both);
        assert_eq!(&again[0][..5], &[0x31, 0x20, 0x10, 0, 0x08]);
        assert_eq!(again[1][1] >> 4, 3);
        assert_eq!(&again[1][3..50], &lights[1..]);
        assert_eq!(m.settle_lights(5, &late, &mut again), nothing);
        // A write after it goes as it is and is not kept.
        assert!(m.frame_output(5, output, &lights, &mut framed).is_some());
        assert_eq!(&framed[3..50], &lights[1..]);
        assert_eq!(m.settle_lights(5, &late, &mut again), nothing);

        // Past the animation at its first report: the reset alone, at once.
        m.encode_report(6, Product::DualSense, ReportKind::Input, wireless, &late);
        assert_eq!(m.settle_lights(6, &late, &mut again), reset);
        assert_eq!(&again[0][..5], &[0x31, 0x00, 0x10, 0, 0x08]);
        assert!(m.frame_output(6, output, &lights, &mut framed).is_some());
        assert_eq!(framed[1] >> 4, 1);
        assert_eq!(m.settle_lights(6, &late, &mut again), nothing);

        // On USB, never.
        let usb = pad::Transport::Usb;
        m.encode_report(7, Product::DualSense, ReportKind::Input, usb, &early);
        assert!(m.frame_output(7, output, &lights, &mut framed).is_some());
        assert_eq!(m.settle_lights(7, &late, &mut again), nothing);

        // Back on another transport is a new connection: reset again, and the
        // light write kept before is gone.
        let mut m = Mapper::default();
        m.encode_report(8, Product::DualSense, ReportKind::Input, wireless, &early);
        assert!(m.frame_output(8, output, &lights, &mut framed).is_some());
        m.encode_report(8, Product::DualSense, ReportKind::Input, usb, &early);
        m.encode_report(8, Product::DualSense, ReportKind::Input, wireless, &late);
        assert_eq!(m.settle_lights(8, &late, &mut again), reset);
        m.encode_report(9, Product::DualSense, ReportKind::Input, wireless, &late);
        assert_eq!(m.settle_lights(9, &late, &mut again), reset);
        m.encode_report(9, Product::DualSense, ReportKind::Input, usb, &late);
        m.encode_report(9, Product::DualSense, ReportKind::Input, wireless, &late);
        assert_eq!(m.settle_lights(9, &late, &mut again), reset);
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
