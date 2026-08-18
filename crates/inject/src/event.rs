//! Expansion from one control message to kernel input events.
//!
//! **Nothing here touches a device.** A message goes in, events come out
//! through a sink, and the guest's held state moves. That is what makes the
//! two things most worth proving -- that nothing stays held after a guest
//! vanishes, and that revoking a permission releases what it was holding --
//! testable with no hardware and no display stack (docs/05-host.md section 7).

use crate::gamepad::{self, MAX_PADS};
use crate::usage;
use lowlat_core::control::{Control, op};

/// Event types, as the kernel numbers them.
const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;
const EV_ABS: u16 = 0x03;
const EV_MSC: u16 = 0x04;

const SYN_REPORT: u16 = 0;
const MSC_SCAN: u16 = 4;

const REL_X: u16 = 0x00;
const REL_Y: u16 = 0x01;
const REL_HWHEEL: u16 = 0x06;
const REL_WHEEL: u16 = 0x08;
const REL_WHEEL_HI_RES: u16 = 0x0b;
const REL_HWHEEL_HI_RES: u16 = 0x0c;

const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;

const BTN_LEFT: u16 = 0x110;
const BTN_RIGHT: u16 = 0x111;
const BTN_MIDDLE: u16 = 0x112;
const BTN_SIDE: u16 = 0x113;
const BTN_EXTRA: u16 = 0x114;

const KEY_CAPSLOCK: u16 = 58;
const KEY_NUMLOCK: u16 = 69;

/// The peer's codes for the two lock keys, needed because a tap this host
/// decides on still announces a scanned code the way a real one would.
const USAGE_CAPS_LOCK: u16 = 57;
const USAGE_NUM_LOCK: u16 = 83;

/// The modifier bits that report a lock rather than a held key.
const MOD_NUM: u32 = 0x1000;
const MOD_CAPS: u32 = 0x2000;

/// The wheel units one detent is worth.
///
/// A peer counts wheel movement in these rather than in detents, so the
/// stepped axis is this divided out and the fine axis is the raw figure.
const WHEEL_DETENT: i32 = 120;

/// Both absolute axes span this, whatever the output's shape.
///
/// The two axes are mapped to the output independently, so a square range on
/// an oblong output still reaches every corner, and 65535 is finer than any
/// output this will drive.
pub const ABS_RANGE: i32 = 65535;

/// Kernel key codes this can emit all fall below this.
///
/// Sized from the table rather than from the kernel's maximum, which is 767:
/// a per-guest array is paid for by every guest, and a test holds the bound
/// honest.
const KEY_SLOTS: usize = 256;

/// Peer button numbers run one through five.
const BUTTONS: usize = 5;

/// Events written in one call, before the sink is handed them.
///
/// A mass release is chunked into as many of these as it needs, and each
/// chunk ends at a report boundary, so a consumer never sees half a report.
const SCRATCH: usize = 64;

/// One kernel input event, without the timestamp the kernel fills in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Event {
    pub kind: u16,
    pub code: u16,
    pub value: i32,
}

/// Which of a guest's devices a batch belongs to.
///
/// **The two pointers are separate devices** and cannot be merged: an
/// absolute pointer declares axes and a property that a relative one must not
/// have (docs/07-platforms.md section 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Device {
    Keyboard,
    Pointer,
    PointerAbsolute,
    /// **Addressed by the peer's own pad identifier**, not by a slot. It is a
    /// value the peer chose and the sink keeps one device per distinct one it
    /// is given, up to a cap the expander enforces.
    Gamepad(u32),
}

/// Where expanded events go.
///
/// A sink rather than a returned slice because one message can produce events
/// for more than one device, and a release produces more than fits in any
/// buffer worth carrying per guest.
pub trait Sink {
    fn emit(&mut self, device: Device, events: &[Event]);

    /// Take a pad away.
    ///
    /// **Not an event.** Destroying the device is how a pad reports that it
    /// was unplugged, and it is also what releases everything the pad held, so
    /// there is nothing to emit first.
    fn unplug(&mut self, _pad: u32) {}
}

/// What a guest is allowed to drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Permissions {
    pub keyboard: bool,
    pub pointer: bool,
    pub gamepad: bool,
}

impl Default for Permissions {
    fn default() -> Self {
        Self {
            keyboard: true,
            pointer: true,
            gamepad: true,
        }
    }
}

/// The extents absolute coordinates are expressed in.
///
/// **This is the desktop's shape, not the encoded frame's.** A peer viewing a
/// rotated output already sends coordinates in the orientation the desktop is
/// in, so a rotated stream swaps what is passed here and rotates nothing at
/// injection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Extents {
    pub width: u32,
    pub height: u32,
}

/// One guest's input state and the expansion that reads it.
#[derive(Debug)]
pub struct Injector {
    extents: Extents,
    permissions: Permissions,
    /// Held keys, by kernel code rather than by the peer's code.
    ///
    /// **Two of the peer's codes can name one kernel key.** Tracking the
    /// peer's code instead would let a guest press one name and release the
    /// other, and leave the key held with nothing tracking it.
    keys: [bool; KEY_SLOTS],
    buttons: [bool; BUTTONS],
    /// Which pointer moved last, so a click lands on the device the position
    /// came from.
    absolute: bool,
    /// The last absolute position, kept only for the hidden-pointer case.
    last_abs: Option<(i32, i32)>,
    /// Whether the local pointer is hidden, which is a peer-independent
    /// statement that motion is being used to aim rather than to point.
    hidden: bool,
    /// What the peer last said its locks were. `None` until it says anything:
    /// the first message sets the baseline and taps nothing, because a host
    /// below the display server cannot read the state it would be correcting
    /// towards and a guess is wrong half the time.
    num_lock: Option<bool>,
    caps_lock: Option<bool>,
    /// Whether this guest currently has the pointer.
    ///
    /// **Only the pointer**, and only when the host is arbitrating it. Every
    /// guest types at once and every guest's pads are its own devices; the
    /// pointer is the one thing they genuinely share, because the display
    /// stack merges every pointer device on a seat into one cursor.
    floor: bool,
    /// One entry per pad this guest holds, in the order they first arrived.
    pads: [Option<Pad>; MAX_PADS],
    /// What has arrived, for the line a live run is read from.
    tally: Tally,
    scratch: [Event; SCRATCH],
    used: usize,
}

impl Injector {
    #[must_use]
    pub fn new(extents: Extents) -> Self {
        Self {
            extents,
            permissions: Permissions::default(),
            keys: [false; KEY_SLOTS],
            buttons: [false; BUTTONS],
            absolute: false,
            last_abs: None,
            hidden: false,
            num_lock: None,
            caps_lock: None,
            floor: true,
            pads: [None; MAX_PADS],
            tally: Tally::default(),
            scratch: [Event {
                kind: 0,
                code: 0,
                value: 0,
            }; SCRATCH],
            used: 0,
        }
    }

    /// Change what this guest may drive, releasing anything a withdrawn
    /// permission was holding.
    ///
    /// **The release happens here rather than in the message path.** A
    /// permission can be withdrawn from a guest that is sending nothing, and
    /// waiting for its next message to notice would leave a key held for as
    /// long as it stays quiet.
    pub fn set_permissions(&mut self, permissions: Permissions, out: &mut impl Sink) {
        let was = self.permissions;
        self.permissions = permissions;
        if was.keyboard && !permissions.keyboard {
            self.release_keys(out);
        }
        if was.pointer && !permissions.pointer {
            self.release_buttons(out);
        }
    }

    /// Say whether this guest currently has the pointer.
    ///
    /// **Losing it releases the buttons it was holding, here and now.** The
    /// alternative is to wait for the guest's own release to arrive and drop
    /// it for want of the pointer, which leaves a button down on a machine
    /// nobody is driving. A guest that simply stopped moving never sends
    /// another pointer message at all, so waiting is waiting forever.
    pub fn set_floor(&mut self, held: bool, out: &mut impl Sink) {
        if self.floor && !held {
            self.release_buttons(out);
        }
        self.floor = held;
    }

    /// Whether this guest is holding any pointer button.
    ///
    /// **A gesture that is still going, stated as plainly as it can be.** A
    /// guest mid-drag sends nothing at all while it pauses, so elapsed silence
    /// cannot tell a finished gesture from a paused one; a button that is
    /// still down can.
    #[must_use]
    pub fn holds_pointer_button(&self) -> bool {
        self.buttons.iter().any(|held| *held)
    }

    /// The first pad holding every button in `mask`, if any.
    ///
    /// A read of state this already keeps. It exists for the live-run probe
    /// that stands in for a local application raising an effect
    /// ([`crate::uinput::Devices::rumble`] is the real source).
    #[must_use]
    pub fn pad_holding(&self, mask: u16) -> Option<u32> {
        self.pads
            .iter()
            .flatten()
            .find(|pad| pad.buttons & mask == mask)
            .map(|pad| pad.id)
    }

    /// Report whether the local pointer is hidden.
    ///
    /// Nothing calls this yet; the signal is a property of the captured
    /// session and arrives with capture (docs/05-host.md section 8.2). The
    /// conversion it drives is here because it owns the last-position state,
    /// which every motion path has to maintain either way.
    pub fn set_cursor_hidden(&mut self, hidden: bool) {
        if !hidden {
            self.last_abs = None;
        }
        self.hidden = hidden;
    }

    /// What this guest has sent, by kind.
    #[must_use]
    pub fn tally(&self) -> Tally {
        self.tally
    }

    /// Expand one control message.
    pub fn on_control(&mut self, message: &Control<'_>, out: &mut impl Sink) {
        self.tally.count(message.opcode);
        match message.opcode {
            op::KEYBOARD if self.permissions.keyboard => {
                self.keyboard(message.a0, message.a1, message.a2 != 0, out);
            }
            op::MOUSE_BUTTON if self.pointer_allowed() => {
                self.button(message.a0, message.a1 != 0, out);
            }
            op::MOUSE_WHEEL if self.pointer_allowed() => {
                self.wheel(as_i32(message.a0), as_i32(message.a1), out);
            }
            op::MOUSE_MOTION if self.pointer_allowed() => {
                self.motion(message.a0 != 0, as_i32(message.a1), as_i32(message.a2), out);
            }
            op::MOUSE_MOTION_STREAM if self.pointer_allowed() => {
                // **The two coordinates are packed differently.** The
                // horizontal one is unsigned and the vertical one is signed,
                // which is not symmetry anybody would invent and is not
                // guessable from the horizontal half.
                let relative = message.a0 & 1 != 0;
                let x = i32::from(low16(message.a1));
                let y = i32::from(high16_signed(message.a1));
                self.motion(relative, x, y, out);
            }
            op::GAMEPAD_BUTTON if self.permissions.gamepad => {
                // Arguments are the button, whether it is down, and the pad.
                let Some(bit) = gamepad::bit_for_index(message.a0) else {
                    return;
                };
                let Some(index) = self.pad(message.a2, out) else {
                    return;
                };
                let Some(pad) = self.pads.get_mut(index).and_then(Option::as_mut) else {
                    return;
                };
                let before_state = pad.buttons;
                if message.a1 == 0 {
                    pad.buttons &= !bit;
                } else {
                    pad.buttons |= bit;
                }
                let after = pad.buttons;
                let id = pad.id;
                self.buttons_changed(id, before_state, after, out);
            }
            op::GAMEPAD_AXIS if self.permissions.gamepad => {
                // Arguments are the axis, its value, and the pad.
                let Some(index) = self.pad(message.a2, out) else {
                    return;
                };
                let Some(id) = self.pads.get(index).and_then(|p| p.as_ref()).map(|p| p.id) else {
                    return;
                };
                let value = high16_signed(message.a1 << 16);
                self.axis(id, message.a0, value, out);
            }
            op::GAMEPAD_UNPLUG if self.permissions.gamepad => {
                self.unplug(message.a2, out);
            }
            op::GAMEPAD_STATE if self.permissions.gamepad => {
                self.pad_state(message.a0, message.body, out);
            }
            // **A peer that lost focus says so**, and everything it holds has
            // to come up whether or not it is allowed to press anything now.
            op::RELEASE => self.release_all(out),
            _ => {}
        }
    }

    /// Release everything this guest holds.
    ///
    /// **Pads are centred, not unplugged.** A peer that lost focus still has
    /// its controller plugged in, and taking the device away would reach an
    /// application as the controller being pulled out of the machine.
    pub fn release_all(&mut self, out: &mut impl Sink) {
        self.release_keys(out);
        self.release_buttons(out);
        self.centre_pads(out);
    }

    /// The slot a peer's pad identifier occupies, taking one if it is new.
    ///
    /// **Bounded.** The identifier is the peer's and nothing constrains it, so
    /// a peer that varies the field would otherwise get a device per value.
    fn pad(&mut self, id: u32, out: &mut impl Sink) -> Option<usize> {
        if let Some(index) = self.pads.iter().position(|p| p.is_some_and(|p| p.id == id)) {
            return Some(index);
        }
        let index = self.pads.iter().position(Option::is_none)?;
        *self.pads.get_mut(index)? = Some(Pad { id, buttons: 0 });
        // **The device is created by the first event sent to it.** Nothing is
        // sent here, so a pad that is announced and never used costs a slot
        // and no device.
        let _ = out;
        Some(index)
    }

    fn unplug(&mut self, id: u32, out: &mut impl Sink) {
        let Some(index) = self.pads.iter().position(|p| p.is_some_and(|p| p.id == id)) else {
            return;
        };
        if let Some(slot) = self.pads.get_mut(index) {
            *slot = None;
        }
        out.unplug(id);
    }

    /// Centre every pad without taking any away.
    fn centre_pads(&mut self, out: &mut impl Sink) {
        for index in 0..MAX_PADS {
            let Some(pad) = self.pads.get(index).copied().flatten() else {
                continue;
            };
            if let Some(slot) = self.pads.get_mut(index).and_then(Option::as_mut) {
                slot.buttons = 0;
            }
            self.buttons_changed(pad.id, pad.buttons, 0, out);
            self.used = 0;
            for axis in [
                gamepad::axis::X,
                gamepad::axis::Y,
                gamepad::axis::RX,
                gamepad::axis::RY,
                gamepad::axis::Z,
                gamepad::axis::RZ,
            ] {
                self.push(EV_ABS, axis, 0);
            }
            self.report();
            self.flush(Device::Gamepad(pad.id), out);
        }
    }

    /// Emit whatever changed between two button states.
    fn buttons_changed(&mut self, id: u32, before: u16, after: u16, out: &mut impl Sink) {
        let changed = before ^ after;
        if changed == 0 {
            return;
        }
        self.used = 0;
        for bit in gamepad::KEYED_BITS {
            if changed & bit == 0 {
                continue;
            }
            if let Some(code) = gamepad::key_for_bit(bit) {
                self.push(EV_KEY, code, i32::from(after & bit != 0));
            }
        }
        // The direction pad moves as axes, and only when one of its own bits
        // moved: a face button must not restate it.
        const HAT: u16 = gamepad::bit::DPAD_UP
            | gamepad::bit::DPAD_DOWN
            | gamepad::bit::DPAD_LEFT
            | gamepad::bit::DPAD_RIGHT;
        if changed & HAT != 0 {
            self.push(EV_ABS, gamepad::axis::HAT0X, gamepad::hat_x(after));
            self.push(EV_ABS, gamepad::axis::HAT0Y, gamepad::hat_y(after));
        }
        if self.used == 0 {
            return;
        }
        self.report();
        self.flush(Device::Gamepad(id), out);
    }

    /// One axis of one pad.
    fn axis(&mut self, id: u32, which: u32, value: i16, out: &mut impl Sink) {
        let (code, scaled) = match which {
            0 => (gamepad::axis::X, gamepad::stick(value, false)),
            1 => (gamepad::axis::Y, gamepad::stick(value, true)),
            2 => (gamepad::axis::RX, gamepad::stick(value, false)),
            3 => (gamepad::axis::RY, gamepad::stick(value, true)),
            4 => (gamepad::axis::Z, gamepad::trigger_from_axis(value)),
            5 => (gamepad::axis::RZ, gamepad::trigger_from_axis(value)),
            _ => return,
        };
        self.used = 0;
        self.push(EV_ABS, code, scaled);
        self.report();
        self.flush(Device::Gamepad(id), out);
    }

    /// A whole pad in one message.
    ///
    /// **The first three bytes of the body are the peer's uninitialised
    /// stack** (docs/01-protocol.md 11.1). They are skipped and never read.
    fn pad_state(&mut self, id: u32, body: &[u8], out: &mut impl Sink) {
        const PAD: usize = 3;
        let Some(fields) = body.get(PAD..PAD + 12) else {
            return;
        };
        let pair = |offset: usize| -> [u8; 2] {
            let bytes = fields.get(offset..offset + 2).unwrap_or(&[0, 0]);
            [
                bytes.first().copied().unwrap_or(0),
                bytes.get(1).copied().unwrap_or(0),
            ]
        };
        let at = |offset: usize| i16::from_be_bytes(pair(offset));
        let buttons = u16::from_be_bytes(pair(0));
        let Some(index) = self.pad(id, out) else {
            return;
        };
        let before = self
            .pads
            .get(index)
            .and_then(|p| p.as_ref())
            .map_or(0, |p| p.buttons);
        if let Some(pad) = self.pads.get_mut(index).and_then(Option::as_mut) {
            pad.buttons = buttons;
        }
        self.buttons_changed(id, before, buttons, out);

        self.used = 0;
        self.push(EV_ABS, gamepad::axis::X, gamepad::stick(at(2), false));
        self.push(EV_ABS, gamepad::axis::Y, gamepad::stick(at(4), true));
        self.push(EV_ABS, gamepad::axis::RX, gamepad::stick(at(6), false));
        self.push(EV_ABS, gamepad::axis::RY, gamepad::stick(at(8), true));
        self.push(
            EV_ABS,
            gamepad::axis::Z,
            i32::from(fields.get(10).copied().unwrap_or(0)),
        );
        self.push(
            EV_ABS,
            gamepad::axis::RZ,
            i32::from(fields.get(11).copied().unwrap_or(0)),
        );
        self.report();
        self.flush(Device::Gamepad(id), out);
    }

    /// Whether pointer input from this guest reaches a device.
    ///
    /// Two independent gates and both must hold: what the guest is entitled
    /// to, which changes rarely, and whether it has the pointer right now,
    /// which changes as guests take turns.
    fn pointer_allowed(&self) -> bool {
        self.permissions.pointer && self.floor
    }

    fn keyboard(&mut self, code: u32, modifiers: u32, pressed: bool, out: &mut impl Sink) {
        let Ok(peer_code) = u16::try_from(code) else {
            return;
        };
        let Some(key) = usage::key_code(peer_code) else {
            return;
        };
        let Some(held) = self.keys.get_mut(usize::from(key)) else {
            return;
        };
        // **A release for a key that was never pressed is dropped.** A peer
        // that reconnects mid-keystroke, or one whose permission was revoked
        // and restored, sends releases for keys this host never saw down, and
        // passing those on interrupts whatever a local user is holding.
        if !pressed && !*held {
            return;
        }
        *held = pressed;

        self.used = 0;
        self.lock_sync(modifiers, key);
        self.key_events(peer_code, key, pressed);
        self.flush(Device::Keyboard, out);
    }

    /// Bring the local locks into step with what the peer reports.
    ///
    /// **A peer reports its lock state on every keystroke, and that is the
    /// only way this host learns of a change it did not see the key for.** A
    /// guest that toggles a lock while looking at something else comes back
    /// with a state this machine never applied, and from then on every letter
    /// it types is the wrong case.
    ///
    /// Only a change is acted on, and never on the message carrying the lock
    /// key itself: that key is being injected anyway and would toggle twice.
    ///
    /// **The initial state cannot be repaired here.** Correcting towards the
    /// local state means reading it, and there is nothing below the display
    /// server that reports it for the seat. The nearest thing is the light on
    /// a physical keyboard, which is exposed by the kernel and which the
    /// display server does drive, but it presumes a physical keyboard exists
    /// and is one keyboard's answer to a question about the seat. It is not
    /// used, and the drift it would fix is one-time and self-correcting the
    /// first time anybody presses the key.
    fn lock_sync(&mut self, modifiers: u32, injected: u16) {
        let num = modifiers & MOD_NUM != 0;
        if self.num_lock.replace(num) == Some(!num) && injected != KEY_NUMLOCK {
            self.tap(USAGE_NUM_LOCK, KEY_NUMLOCK);
        }
        let caps = modifiers & MOD_CAPS != 0;
        if self.caps_lock.replace(caps) == Some(!caps) && injected != KEY_CAPSLOCK {
            self.tap(USAGE_CAPS_LOCK, KEY_CAPSLOCK);
        }
    }

    fn tap(&mut self, usage: u16, key: u16) {
        self.key_events(usage, key, true);
        self.key_events(usage, key, false);
    }

    fn key_events(&mut self, usage: u16, key: u16, pressed: bool) {
        // Hardware announces the code it scanned before the key it decided
        // on, and a consumer reading the raw code from a real keyboard gets
        // the same thing from this one.
        if let Some(scan) = usage::scan_code(usage) {
            self.push(EV_MSC, MSC_SCAN, scan);
        }
        self.push(EV_KEY, key, i32::from(pressed));
        self.report();
    }

    fn button(&mut self, button: u32, pressed: bool, out: &mut impl Sink) {
        let Some(index) = (button as usize).checked_sub(1) else {
            return;
        };
        let Some(held) = self.buttons.get_mut(index) else {
            return;
        };
        let Some(code) = button_code(button) else {
            return;
        };
        if !pressed && !*held {
            return;
        }
        *held = pressed;

        self.used = 0;
        self.push(EV_KEY, code, i32::from(pressed));
        self.report();
        let device = self.pointer();
        self.flush(device, out);
    }

    fn wheel(&mut self, x: i32, y: i32, out: &mut impl Sink) {
        if x == 0 && y == 0 {
            return;
        }
        self.used = 0;
        // **Both the stepped and the fine axis go out.** A peer counting in
        // detent units divides to nothing when it sends less than one, and a
        // consumer that reads only the stepped axis sees no scrolling at all
        // from a peer whose platform counts in whole detents.
        if y != 0 {
            self.push(EV_REL, REL_WHEEL, detents(y));
            self.push(EV_REL, REL_WHEEL_HI_RES, y);
        }
        if x != 0 {
            self.push(EV_REL, REL_HWHEEL, detents(x));
            self.push(EV_REL, REL_HWHEEL_HI_RES, x);
        }
        self.report();
        let device = self.pointer();
        self.flush(device, out);
    }

    fn motion(&mut self, relative: bool, x: i32, y: i32, out: &mut impl Sink) {
        if relative {
            self.last_abs = None;
            self.absolute = false;
            self.used = 0;
            self.push(EV_REL, REL_X, x);
            self.push(EV_REL, REL_Y, y);
            self.report();
            self.flush(Device::Pointer, out);
            return;
        }

        // **A hidden pointer means the position is being used to aim.** The
        // peer keeps sending absolute coordinates because it does not know
        // that, so the difference between them is the motion that was meant.
        // The first sample after the pointer hides establishes where it is
        // and moves nothing; treating it as a delta from the origin throws
        // the aim across the screen.
        if self.hidden {
            let previous = self.last_abs.replace((x, y));
            let Some((px, py)) = previous else {
                return;
            };
            self.absolute = false;
            self.used = 0;
            self.push(EV_REL, REL_X, x - px);
            self.push(EV_REL, REL_Y, y - py);
            self.report();
            self.flush(Device::Pointer, out);
            return;
        }

        self.last_abs = None;
        self.absolute = true;
        self.used = 0;
        self.push(EV_ABS, ABS_X, scale(x, self.extents.width));
        self.push(EV_ABS, ABS_Y, scale(y, self.extents.height));
        self.report();
        self.flush(Device::PointerAbsolute, out);
    }

    fn release_keys(&mut self, out: &mut impl Sink) {
        self.used = 0;
        for key in 0..KEY_SLOTS {
            if self.keys.get(key) != Some(&true) {
                continue;
            }
            if let Some(held) = self.keys.get_mut(key) {
                *held = false;
            }
            // A release is not a keystroke, so the scanned code is left off:
            // nothing scanned anything, and a consumer correlating the two
            // would see a code with no cause.
            #[allow(
                clippy::cast_possible_truncation,
                reason = "the loop bound is the array length, which is 256"
            )]
            self.push(EV_KEY, key as u16, 0);
            if self.used + 1 >= SCRATCH {
                self.report();
                self.flush(Device::Keyboard, out);
                self.used = 0;
            }
        }
        if self.used > 0 {
            self.report();
            self.flush(Device::Keyboard, out);
        }
        // The peer's locks are no longer known, rather than known to be off.
        // Its next message sets the baseline again without toggling anything.
        self.num_lock = None;
        self.caps_lock = None;
    }

    fn release_buttons(&mut self, out: &mut impl Sink) {
        self.used = 0;
        for index in 0..BUTTONS {
            if self.buttons.get(index) != Some(&true) {
                continue;
            }
            if let Some(held) = self.buttons.get_mut(index) {
                *held = false;
            }
            #[allow(clippy::cast_possible_truncation, reason = "the loop bound is five")]
            if let Some(code) = button_code(index as u32 + 1) {
                self.push(EV_KEY, code, 0);
            }
        }
        if self.used > 0 {
            self.report();
            let device = self.pointer();
            self.flush(device, out);
        }
    }

    /// The device a click or a wheel belongs to.
    ///
    /// Whichever pointer moved last, because that is the one the visible
    /// position came from.
    fn pointer(&self) -> Device {
        if self.absolute {
            Device::PointerAbsolute
        } else {
            Device::Pointer
        }
    }

    fn push(&mut self, kind: u16, code: u16, value: i32) {
        if let Some(slot) = self.scratch.get_mut(self.used) {
            *slot = Event { kind, code, value };
            self.used += 1;
        }
    }

    fn report(&mut self) {
        self.push(EV_SYN, SYN_REPORT, 0);
    }

    fn flush(&mut self, device: Device, out: &mut impl Sink) {
        if let Some(events) = self.scratch.get(..self.used) {
            out.emit(device, events);
        }
        self.used = 0;
    }
}

/// A count of what a guest has sent, by kind.
///
/// **Counted where it arrives, before any gate.** A live run's first question
/// is whether the input reached the host at all, and a tally taken after the
/// permission and pointer gates cannot tell "nothing arrived" from "everything
/// was dropped".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tally {
    pub keys: u32,
    pub buttons: u32,
    pub wheels: u32,
    pub motions: u32,
    pub pads: u32,
}

impl Tally {
    fn count(&mut self, opcode: u8) {
        let slot = match opcode {
            op::KEYBOARD => &mut self.keys,
            op::MOUSE_BUTTON => &mut self.buttons,
            op::MOUSE_WHEEL => &mut self.wheels,
            op::MOUSE_MOTION | op::MOUSE_MOTION_STREAM => &mut self.motions,
            op::GAMEPAD_BUTTON | op::GAMEPAD_AXIS | op::GAMEPAD_STATE | op::GAMEPAD_UNPLUG => {
                &mut self.pads
            }
            _ => return,
        };
        *slot = slot.saturating_add(1);
    }
}

/// One pad a guest holds.
#[derive(Debug, Clone, Copy)]
struct Pad {
    /// The peer's identifier for it, which is also how the device is
    /// addressed.
    id: u32,
    /// What it is holding, in the whole-pad message's bits whichever message
    /// set them.
    buttons: u16,
}

fn button_code(button: u32) -> Option<u16> {
    match button {
        1 => Some(BTN_LEFT),
        2 => Some(BTN_MIDDLE),
        3 => Some(BTN_RIGHT),
        4 => Some(BTN_SIDE),
        5 => Some(BTN_EXTRA),
        _ => None,
    }
}

/// Whole detents, never rounding a real movement away to nothing.
fn detents(units: i32) -> i32 {
    let whole = units / WHEEL_DETENT;
    if whole != 0 {
        whole
    } else if units > 0 {
        1
    } else {
        -1
    }
}

/// A coordinate in the output's extent, onto the absolute axis.
///
/// **The extent is inclusive.** A peer reports the far edge as the width
/// itself rather than one less, so dividing by the width is what puts that
/// edge at the end of the axis.
fn scale(value: i32, extent: u32) -> i32 {
    let extent = i64::from(extent);
    if extent <= 0 {
        return 0;
    }
    let clamped = i64::from(value).clamp(0, extent);
    #[allow(
        clippy::cast_possible_truncation,
        reason = "the quotient cannot exceed the axis range, which is 65535"
    )]
    {
        (clamped * i64::from(ABS_RANGE) / extent) as i32
    }
}

#[allow(
    clippy::cast_possible_wrap,
    reason = "a signed wire field is carried in an unsigned argument"
)]
const fn as_i32(value: u32) -> i32 {
    value as i32
}

#[allow(
    clippy::cast_possible_truncation,
    reason = "the low half is taken deliberately"
)]
const fn low16(value: u32) -> u16 {
    value as u16
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    reason = "the high half is taken deliberately and is signed on the wire"
)]
const fn high16_signed(value: u32) -> i16 {
    (value >> 16) as i16
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records what a device layer would have written.
    #[derive(Debug, Default)]
    struct Recorder {
        batches: Vec<(Device, Vec<Event>)>,
        unplugged: Vec<u32>,
    }

    impl Sink for Recorder {
        fn emit(&mut self, device: Device, events: &[Event]) {
            self.batches.push((device, events.to_vec()));
        }

        fn unplug(&mut self, pad: u32) {
            self.unplugged.push(pad);
        }
    }

    impl Recorder {
        fn all(&self) -> Vec<Event> {
            self.batches.iter().flat_map(|(_, e)| e.clone()).collect()
        }

        fn keys_at(&self, value: i32) -> Vec<u16> {
            self.all()
                .into_iter()
                .filter(|e| e.kind == EV_KEY && e.value == value)
                .map(|e| e.code)
                .collect()
        }

        fn devices(&self) -> Vec<Device> {
            self.batches.iter().map(|(d, _)| *d).collect()
        }

        /// How many distinct devices were addressed, which is not the number
        /// of batches: one pad message produces a button batch and an axis
        /// batch.
        fn distinct_devices(&self) -> usize {
            let mut seen: Vec<Device> = Vec::new();
            for (device, _) in &self.batches {
                if !seen.contains(device) {
                    seen.push(*device);
                }
            }
            seen.len()
        }
    }

    fn injector() -> Injector {
        Injector::new(Extents {
            width: 1920,
            height: 1080,
        })
    }

    fn control(opcode: u8, a0: u32, a1: u32, a2: u32) -> Control<'static> {
        Control {
            a0,
            a1,
            a2,
            opcode,
            body: &[],
        }
    }

    fn key(a0: u32, pressed: bool) -> Control<'static> {
        control(op::KEYBOARD, a0, 0, u32::from(pressed))
    }

    #[test]
    fn a_keystroke_announces_its_scanned_code_then_the_key() {
        let (mut inject, mut out) = (injector(), Recorder::default());
        inject.on_control(&key(4, true), &mut out);
        assert_eq!(
            out.batches,
            vec![(
                Device::Keyboard,
                vec![
                    Event {
                        kind: EV_MSC,
                        code: MSC_SCAN,
                        value: 0x0007_0004
                    },
                    Event {
                        kind: EV_KEY,
                        code: 30,
                        value: 1
                    },
                    Event {
                        kind: EV_SYN,
                        code: SYN_REPORT,
                        value: 0
                    },
                ]
            )]
        );
    }

    /// Gate item 3. **The one that must never regress**: a guest that vanishes
    /// mid-keystroke leaves keys down on a machine nobody is sitting at.
    #[test]
    fn nothing_stays_held_when_a_guest_vanishes() {
        let (mut inject, mut out) = (injector(), Recorder::default());
        for usage in [224, 4, 26] {
            inject.on_control(&key(usage, true), &mut out);
        }
        inject.on_control(&control(op::MOUSE_BUTTON, 1, 1, 0), &mut out);
        out.batches.clear();

        inject.release_all(&mut out);

        let mut released = out.keys_at(0);
        released.sort_unstable();
        assert_eq!(released, vec![17, 29, 30, BTN_LEFT]);
        assert!(out.keys_at(1).is_empty(), "a release pressed something");

        // And a second release finds nothing left to do.
        out.batches.clear();
        inject.release_all(&mut out);
        assert!(out.batches.is_empty());
    }

    /// Gate item 5. Withdrawing a permission is the same guard as a
    /// disconnect, and it must not disturb what the other permissions hold.
    #[test]
    fn revoking_a_permission_releases_only_what_it_held() {
        let (mut inject, mut out) = (injector(), Recorder::default());
        inject.on_control(&key(4, true), &mut out);
        inject.on_control(&key(26, true), &mut out);
        inject.on_control(&control(op::MOUSE_BUTTON, 3, 1, 0), &mut out);
        out.batches.clear();

        inject.set_permissions(
            Permissions {
                keyboard: false,
                ..Permissions::default()
            },
            &mut out,
        );

        let mut released = out.keys_at(0);
        released.sort_unstable();
        assert_eq!(released, vec![17, 30], "the pointer button was disturbed");

        // The button is still held, and still comes up on a disconnect.
        out.batches.clear();
        inject.release_all(&mut out);
        assert_eq!(out.keys_at(0), vec![BTN_RIGHT]);
    }

    #[test]
    fn a_withdrawn_permission_drops_what_arrives_under_it() {
        let (mut inject, mut out) = (injector(), Recorder::default());
        inject.set_permissions(
            Permissions {
                keyboard: false,
                ..Permissions::default()
            },
            &mut out,
        );
        inject.on_control(&key(4, true), &mut out);
        assert!(out.batches.is_empty());
        // The pointer still works.
        inject.on_control(&control(op::MOUSE_BUTTON, 1, 1, 0), &mut out);
        assert_eq!(out.keys_at(1), vec![BTN_LEFT]);
    }

    /// **A release nobody pressed is not passed on.** It would interrupt a key
    /// a local user is holding.
    #[test]
    fn a_release_for_an_untracked_key_is_dropped() {
        let (mut inject, mut out) = (injector(), Recorder::default());
        inject.on_control(&key(4, false), &mut out);
        assert!(out.batches.is_empty());
    }

    /// Two of the peer's codes name one kernel key, so pressing under one name
    /// and releasing under the other must still let the key up.
    #[test]
    fn a_key_pressed_under_one_name_releases_under_the_other() {
        let (mut inject, mut out) = (injector(), Recorder::default());
        inject.on_control(&key(101, true), &mut out);
        out.batches.clear();
        inject.on_control(&key(118, false), &mut out);
        assert_eq!(out.keys_at(0), vec![127]);

        out.batches.clear();
        inject.release_all(&mut out);
        assert!(out.batches.is_empty(), "the key was still tracked as held");
    }

    /// The peer reports its locks on every keystroke, and a change it made
    /// while looking elsewhere is the only way this host hears of it.
    #[test]
    fn a_lock_the_peer_toggled_unseen_is_applied_before_the_next_key() {
        let (mut inject, mut out) = (injector(), Recorder::default());
        // First message sets the baseline and toggles nothing.
        inject.on_control(&control(op::KEYBOARD, 4, MOD_CAPS, 1), &mut out);
        assert_eq!(out.keys_at(1), vec![30]);

        // Caps went off somewhere this host never saw.
        out.batches.clear();
        inject.on_control(&control(op::KEYBOARD, 5, 0, 1), &mut out);
        assert_eq!(
            out.keys_at(1),
            vec![KEY_CAPSLOCK, 48],
            "the lock was not brought into step ahead of the key"
        );
        assert_eq!(out.keys_at(0), vec![KEY_CAPSLOCK]);
    }

    /// The message carrying the lock key itself already toggles it. Syncing
    /// as well would toggle twice and land back where it started.
    #[test]
    fn the_lock_key_itself_is_not_also_synced() {
        let (mut inject, mut out) = (injector(), Recorder::default());
        inject.on_control(&control(op::KEYBOARD, 4, 0, 1), &mut out);
        out.batches.clear();
        inject.on_control(&control(op::KEYBOARD, 57, MOD_CAPS, 1), &mut out);
        assert_eq!(out.keys_at(1), vec![KEY_CAPSLOCK]);
    }

    /// An unchanged lock state on every keystroke must not toggle anything.
    #[test]
    fn an_unchanged_lock_is_left_alone() {
        let (mut inject, mut out) = (injector(), Recorder::default());
        for usage in [4, 5, 6] {
            inject.on_control(&control(op::KEYBOARD, usage, MOD_NUM, 1), &mut out);
        }
        assert_eq!(out.keys_at(1), vec![30, 48, 46]);
    }

    #[test]
    fn absolute_motion_reaches_both_ends_of_the_axis() {
        let (mut inject, mut out) = (injector(), Recorder::default());
        inject.on_control(&control(op::MOUSE_MOTION, 0, 0, 0), &mut out);
        inject.on_control(&control(op::MOUSE_MOTION, 0, 1920, 1080), &mut out);
        // Past the edge, which a peer does send.
        inject.on_control(&control(op::MOUSE_MOTION, 0, 4000, 4000), &mut out);

        let values: Vec<i32> = out
            .all()
            .into_iter()
            .filter(|e| e.kind == EV_ABS)
            .map(|e| e.value)
            .collect();
        assert_eq!(
            values,
            vec![0, 0, ABS_RANGE, ABS_RANGE, ABS_RANGE, ABS_RANGE]
        );
        assert!(out.devices().iter().all(|d| *d == Device::PointerAbsolute));
    }

    /// A rotated output is a swap of the extents, not a rotation of the
    /// coordinate: the peer already sends them the way the desktop is.
    #[test]
    fn a_rotated_output_is_a_swap_of_the_extents() {
        let mut inject = Injector::new(Extents {
            width: 1080,
            height: 1920,
        });
        let mut out = Recorder::default();
        inject.on_control(&control(op::MOUSE_MOTION, 0, 1080, 1920), &mut out);
        let values: Vec<i32> = out
            .all()
            .into_iter()
            .filter(|e| e.kind == EV_ABS)
            .map(|e| e.value)
            .collect();
        assert_eq!(values, vec![ABS_RANGE, ABS_RANGE]);
    }

    #[test]
    fn relative_motion_goes_to_the_other_device() {
        let (mut inject, mut out) = (injector(), Recorder::default());
        inject.on_control(
            &control(op::MOUSE_MOTION, 1, as_u32(-5), as_u32(7)),
            &mut out,
        );
        assert_eq!(out.devices(), vec![Device::Pointer]);
        assert_eq!(
            out.all()
                .into_iter()
                .filter(|e| e.kind == EV_REL)
                .map(|e| (e.code, e.value))
                .collect::<Vec<_>>(),
            vec![(REL_X, -5), (REL_Y, 7)]
        );
    }

    /// A click belongs to whichever pointer produced the position it is at.
    #[test]
    fn a_click_follows_the_pointer_that_moved_last() {
        let (mut inject, mut out) = (injector(), Recorder::default());
        inject.on_control(&control(op::MOUSE_MOTION, 0, 100, 100), &mut out);
        inject.on_control(&control(op::MOUSE_BUTTON, 1, 1, 0), &mut out);
        inject.on_control(&control(op::MOUSE_MOTION, 1, 1, 1), &mut out);
        inject.on_control(&control(op::MOUSE_BUTTON, 1, 0, 0), &mut out);
        assert_eq!(
            out.devices(),
            vec![
                Device::PointerAbsolute,
                Device::PointerAbsolute,
                Device::Pointer,
                Device::Pointer,
            ]
        );
    }

    /// The second stream's motion packs one coordinate signed and the other
    /// not, which is the half nobody guesses.
    #[test]
    fn the_second_streams_motion_unpacks_a_signed_vertical() {
        let (mut inject, mut out) = (injector(), Recorder::default());
        inject.set_cursor_hidden(true);
        // Prime, then move up and to the right.
        #[allow(
            clippy::cast_sign_loss,
            reason = "the vertical half is signed on the wire and packed as bits"
        )]
        let packed = |x: u16, y: i16| u32::from(x) | (u32::from(y as u16) << 16);
        inject.on_control(
            &control(op::MOUSE_MOTION_STREAM, 0, packed(100, 100), 0),
            &mut out,
        );
        inject.on_control(
            &control(op::MOUSE_MOTION_STREAM, 0, packed(140, -60_i16), 0),
            &mut out,
        );
        assert_eq!(
            out.all()
                .into_iter()
                .filter(|e| e.kind == EV_REL)
                .map(|e| (e.code, e.value))
                .collect::<Vec<_>>(),
            vec![(REL_X, 40), (REL_Y, -160)]
        );
    }

    /// A hidden pointer is aiming, so the first sample only says where it is.
    #[test]
    fn a_hidden_pointer_primes_before_it_moves() {
        let (mut inject, mut out) = (injector(), Recorder::default());
        inject.set_cursor_hidden(true);
        inject.on_control(&control(op::MOUSE_MOTION, 0, 500, 500), &mut out);
        assert!(
            out.batches.is_empty(),
            "the priming sample moved the pointer"
        );

        inject.on_control(&control(op::MOUSE_MOTION, 0, 510, 495), &mut out);
        assert_eq!(out.devices(), vec![Device::Pointer]);
        assert_eq!(
            out.all()
                .into_iter()
                .filter(|e| e.kind == EV_REL)
                .map(|e| (e.code, e.value))
                .collect::<Vec<_>>(),
            vec![(REL_X, 10), (REL_Y, -5)]
        );
    }

    #[test]
    fn a_pointer_that_reappears_goes_back_to_absolute() {
        let (mut inject, mut out) = (injector(), Recorder::default());
        inject.set_cursor_hidden(true);
        inject.on_control(&control(op::MOUSE_MOTION, 0, 500, 500), &mut out);
        inject.set_cursor_hidden(false);
        inject.on_control(&control(op::MOUSE_MOTION, 0, 510, 495), &mut out);
        assert_eq!(out.devices(), vec![Device::PointerAbsolute]);
    }

    /// A peer counting in whole detents sends less than one unit's worth, and
    /// a bare division rounds it away.
    #[test]
    fn a_scroll_smaller_than_one_detent_still_scrolls() {
        let (mut inject, mut out) = (injector(), Recorder::default());
        inject.on_control(&control(op::MOUSE_WHEEL, 0, as_u32(1), 0), &mut out);
        inject.on_control(&control(op::MOUSE_WHEEL, 0, as_u32(-1), 0), &mut out);
        inject.on_control(&control(op::MOUSE_WHEEL, 0, as_u32(240), 0), &mut out);
        assert_eq!(
            out.all()
                .into_iter()
                .filter(|e| e.kind == EV_REL)
                .map(|e| (e.code, e.value))
                .collect::<Vec<_>>(),
            vec![
                (REL_WHEEL, 1),
                (REL_WHEEL_HI_RES, 1),
                (REL_WHEEL, -1),
                (REL_WHEEL_HI_RES, -1),
                (REL_WHEEL, 2),
                (REL_WHEEL_HI_RES, 240),
            ]
        );
    }

    #[test]
    fn a_peer_losing_focus_releases_everything() {
        let (mut inject, mut out) = (injector(), Recorder::default());
        inject.on_control(&key(4, true), &mut out);
        inject.on_control(&control(op::MOUSE_BUTTON, 2, 1, 0), &mut out);
        out.batches.clear();
        inject.on_control(&control(op::RELEASE, 0, 0, 0), &mut out);
        let mut released = out.keys_at(0);
        released.sort_unstable();
        assert_eq!(released, vec![30, BTN_MIDDLE]);
    }

    /// A mass release is chunked, and every chunk still ends at a report.
    #[test]
    fn a_release_of_everything_holdable_ends_every_chunk_at_a_report() {
        let (mut inject, mut out) = (injector(), Recorder::default());
        let mut held = 0;
        for usage in 0..=u16::MAX {
            if usage::key_code(usage).is_some() {
                inject.on_control(&key(u32::from(usage), true), &mut out);
                held += 1;
            }
        }
        assert_eq!(held, 176);
        out.batches.clear();

        inject.release_all(&mut out);
        for (_, events) in &out.batches {
            assert!(events.len() <= SCRATCH);
            let last = events.last().copied();
            assert_eq!(
                last.map(|e| (e.kind, e.code)),
                Some((EV_SYN, SYN_REPORT)),
                "a chunk did not end at a report"
            );
        }
        // **Twelve kernel keys have more than one name.** Holding all 176 of
        // the peer's codes holds 162 keys, and a tracker keyed on the peer's
        // code instead of the kernel's would report 176 and release some of
        // them twice.
        assert_eq!(out.keys_at(0).len(), 162);
    }

    fn pad_state_body(buttons: u16, lx: i16, ly: i16, rx: i16, ry: i16, lt: u8, rt: u8) -> Vec<u8> {
        // Three bytes of the peer's stack, then the fields. The padding is
        // deliberately not zero here: a reader that validates it would pass
        // against a zeroed fixture and fail against a real peer.
        let mut body = vec![0xAB, 0xCD, 0xEF];
        body.extend_from_slice(&buttons.to_be_bytes());
        for value in [lx, ly, rx, ry] {
            body.extend_from_slice(&value.to_be_bytes());
        }
        body.push(lt);
        body.push(rt);
        body
    }

    fn state(id: u32, body: &[u8]) -> Control<'_> {
        Control {
            a0: id,
            a1: 0,
            a2: 0,
            opcode: op::GAMEPAD_STATE,
            body,
        }
    }

    fn abs(out: &Recorder) -> Vec<(u16, i32)> {
        out.all()
            .into_iter()
            .filter(|e| e.kind == EV_ABS)
            .map(|e| (e.code, e.value))
            .collect()
    }

    /// **A whole pad in one message**, including the three bytes of the peer's
    /// stack in front of it.
    #[test]
    fn a_whole_pad_arrives_in_one_message() {
        let (mut inject, mut out) = (injector(), Recorder::default());
        let body = pad_state_body(
            gamepad::bit::A | gamepad::bit::DPAD_LEFT,
            100,
            200,
            -300,
            -400,
            7,
            9,
        );
        inject.on_control(&state(3, &body), &mut out);

        assert!(out.devices().iter().all(|d| *d == Device::Gamepad(3)));
        assert_eq!(out.keys_at(1), vec![gamepad::key::SOUTH]);
        assert_eq!(
            abs(&out),
            vec![
                // The direction pad moved, so its axes go out with the button.
                (gamepad::axis::HAT0X, -1),
                (gamepad::axis::HAT0Y, 0),
                // Vertical inverts, horizontal does not.
                (gamepad::axis::X, 100),
                (gamepad::axis::Y, -200),
                (gamepad::axis::RX, -300),
                (gamepad::axis::RY, 400),
                (gamepad::axis::Z, 7),
                (gamepad::axis::RZ, 9),
            ]
        );
    }

    /// Only what changed goes out, so a pad reporting sixty times a second
    /// does not restate every button it is holding.
    #[test]
    fn a_repeated_state_reports_no_button_twice() {
        let (mut inject, mut out) = (injector(), Recorder::default());
        let body = pad_state_body(gamepad::bit::A, 0, 0, 0, 0, 0, 0);
        inject.on_control(&state(1, &body), &mut out);
        assert_eq!(out.keys_at(1), vec![gamepad::key::SOUTH]);

        out.batches.clear();
        inject.on_control(&state(1, &body), &mut out);
        assert!(out.keys_at(1).is_empty(), "a held button was restated");
        assert!(out.keys_at(0).is_empty());

        // Letting go does go out.
        out.batches.clear();
        let released = pad_state_body(0, 0, 0, 0, 0, 0, 0);
        inject.on_control(&state(1, &released), &mut out);
        assert_eq!(out.keys_at(0), vec![gamepad::key::SOUTH]);
    }

    /// **Both message forms drive one held state.** A guest that presses with
    /// the per-button message and lets go with the whole-pad message must not
    /// leave the button down.
    #[test]
    fn the_two_message_forms_share_one_held_state() {
        let (mut inject, mut out) = (injector(), Recorder::default());
        // Index 1 is B, which is bit 0x2000 and nothing like index 1.
        inject.on_control(&control(op::GAMEPAD_BUTTON, 1, 1, 5), &mut out);
        assert_eq!(out.keys_at(1), vec![gamepad::key::EAST]);

        out.batches.clear();
        inject.on_control(&state(5, &pad_state_body(0, 0, 0, 0, 0, 0, 0)), &mut out);
        assert_eq!(out.keys_at(0), vec![gamepad::key::EAST]);
    }

    /// The per-axis message carries a trigger as a signed sixteen-bit pull and
    /// the whole-pad message carries the same trigger as a byte. Truncating
    /// instead of scaling never reaches the end of the axis.
    #[test]
    fn a_trigger_axis_is_scaled_rather_than_truncated() {
        let (mut inject, mut out) = (injector(), Recorder::default());
        inject.on_control(&control(op::GAMEPAD_AXIS, 4, i16::MAX as u32, 1), &mut out);
        assert_eq!(abs(&out), vec![(gamepad::axis::Z, 255)]);
    }

    #[test]
    fn a_pad_is_taken_away_by_its_own_message() {
        let (mut inject, mut out) = (injector(), Recorder::default());
        inject.on_control(
            &state(2, &pad_state_body(gamepad::bit::A, 0, 0, 0, 0, 0, 0)),
            &mut out,
        );
        inject.on_control(&control(op::GAMEPAD_UNPLUG, 0, 0, 2), &mut out);
        assert_eq!(out.unplugged, vec![2]);

        // And the slot comes back, so a peer can replug forever.
        out.batches.clear();
        for id in 100..104 {
            inject.on_control(
                &state(id, &pad_state_body(gamepad::bit::A, 0, 0, 0, 0, 0, 0)),
                &mut out,
            );
        }
        assert_eq!(out.distinct_devices(), 4);
    }

    /// **The identifier is the peer's and nothing bounds it.** A fifth pad is
    /// refused rather than given a device.
    #[test]
    fn a_guest_holds_no_more_pads_than_there_are_slots() {
        let (mut inject, mut out) = (injector(), Recorder::default());
        for id in 0..8 {
            inject.on_control(
                &state(id, &pad_state_body(gamepad::bit::A, 0, 0, 0, 0, 0, 0)),
                &mut out,
            );
        }
        assert_eq!(out.distinct_devices(), MAX_PADS);
    }

    /// **Losing the pointer lets go of the buttons it was holding.** The
    /// guest that lost it has stopped moving by definition, so it will send no
    /// release of its own; waiting for one leaves a button down on a machine
    /// somebody else is now driving.
    #[test]
    fn losing_the_pointer_releases_the_buttons_it_was_holding() {
        let (mut inject, mut out) = (injector(), Recorder::default());
        inject.on_control(&control(op::MOUSE_BUTTON, 1, 1, 0), &mut out);
        inject.on_control(&key(4, true), &mut out);
        out.batches.clear();

        inject.set_floor(false, &mut out);
        assert_eq!(out.keys_at(0), vec![BTN_LEFT]);

        // The keyboard is untouched: every guest types at once.
        out.batches.clear();
        inject.on_control(&key(5, true), &mut out);
        assert_eq!(out.keys_at(1), vec![48]);
    }

    #[test]
    fn nothing_from_the_pointer_reaches_a_device_without_it() {
        let (mut inject, mut out) = (injector(), Recorder::default());
        inject.set_floor(false, &mut out);
        for message in [
            control(op::MOUSE_BUTTON, 1, 1, 0),
            control(op::MOUSE_WHEEL, 0, 120, 0),
            control(op::MOUSE_MOTION, 0, 100, 100),
            control(op::MOUSE_MOTION_STREAM, 0, 5, 0),
        ] {
            inject.on_control(&message, &mut out);
        }
        assert!(out.batches.is_empty());

        // And it all works again when the pointer comes back.
        inject.set_floor(true, &mut out);
        inject.on_control(&control(op::MOUSE_MOTION, 0, 100, 100), &mut out);
        assert_eq!(out.devices(), vec![Device::PointerAbsolute]);
    }

    /// Gamepads are each their own device, so two guests never contend for
    /// one and losing the pointer must not disturb a pad.
    #[test]
    fn a_pad_is_untouched_by_the_pointer_changing_hands() {
        let (mut inject, mut out) = (injector(), Recorder::default());
        inject.on_control(
            &state(1, &pad_state_body(gamepad::bit::A, 0, 0, 0, 0, 0, 0)),
            &mut out,
        );
        out.batches.clear();

        inject.set_floor(false, &mut out);
        assert!(out.batches.is_empty(), "the pad was disturbed");

        inject.on_control(
            &state(1, &pad_state_body(gamepad::bit::B, 0, 0, 0, 0, 0, 0)),
            &mut out,
        );
        assert_eq!(out.keys_at(1), vec![gamepad::key::EAST]);
    }

    /// Losing it twice releases once: the second is not a new loss.
    #[test]
    fn losing_the_pointer_again_releases_nothing_more() {
        let (mut inject, mut out) = (injector(), Recorder::default());
        inject.on_control(&control(op::MOUSE_BUTTON, 1, 1, 0), &mut out);
        inject.set_floor(false, &mut out);
        out.batches.clear();
        inject.set_floor(false, &mut out);
        assert!(out.batches.is_empty());
    }

    /// **A peer losing focus centres its pads and does not unplug them.**
    /// Taking the device away reaches an application as the controller being
    /// pulled out of the machine.
    #[test]
    fn losing_focus_centres_a_pad_without_unplugging_it() {
        let (mut inject, mut out) = (injector(), Recorder::default());
        inject.on_control(
            &state(
                1,
                &pad_state_body(
                    gamepad::bit::A | gamepad::bit::DPAD_UP,
                    900,
                    900,
                    0,
                    0,
                    40,
                    0,
                ),
            ),
            &mut out,
        );
        out.batches.clear();

        inject.on_control(&control(op::RELEASE, 0, 0, 0), &mut out);
        assert!(out.unplugged.is_empty(), "the pad was unplugged");
        assert_eq!(out.keys_at(0), vec![gamepad::key::SOUTH]);
        let centred = abs(&out);
        assert!(centred.contains(&(gamepad::axis::HAT0Y, 0)));
        for axis in [
            gamepad::axis::X,
            gamepad::axis::Y,
            gamepad::axis::RX,
            gamepad::axis::RY,
            gamepad::axis::Z,
            gamepad::axis::RZ,
        ] {
            assert!(
                centred.contains(&(axis, 0)),
                "{axis:#x} was left where it was"
            );
        }
    }

    #[test]
    fn a_pad_message_is_dropped_when_the_permission_is_off() {
        let (mut inject, mut out) = (injector(), Recorder::default());
        inject.set_permissions(
            Permissions {
                gamepad: false,
                ..Permissions::default()
            },
            &mut out,
        );
        inject.on_control(
            &state(1, &pad_state_body(gamepad::bit::A, 0, 0, 0, 0, 0, 0)),
            &mut out,
        );
        assert!(out.batches.is_empty());
    }

    /// A body too short to hold a pad must not read past it.
    #[test]
    fn a_short_pad_body_is_dropped() {
        let (mut inject, mut out) = (injector(), Recorder::default());
        for len in 0..15 {
            let body = vec![0u8; len];
            inject.on_control(&state(1, &body), &mut out);
        }
        assert!(out.batches.is_empty());
    }

    /// An opcode this does not handle must not produce events, and must not
    /// be a parse failure either.
    #[test]
    fn an_unhandled_opcode_produces_nothing() {
        let (mut inject, mut out) = (injector(), Recorder::default());
        inject.on_control(&control(op::INIT, 1, 2, 3), &mut out);
        inject.on_control(&control(200, 1, 2, 3), &mut out);
        assert!(out.batches.is_empty());
    }

    #[test]
    fn a_button_outside_the_range_is_dropped() {
        let (mut inject, mut out) = (injector(), Recorder::default());
        inject.on_control(&control(op::MOUSE_BUTTON, 0, 1, 0), &mut out);
        inject.on_control(&control(op::MOUSE_BUTTON, 6, 1, 0), &mut out);
        assert!(out.batches.is_empty());
    }

    /// Every code the table can produce fits the per-guest held array.
    #[test]
    fn every_key_the_table_produces_fits_the_tracker() {
        for usage in 0..=u16::MAX {
            if let Some(code) = usage::key_code(usage) {
                assert!(usize::from(code) < KEY_SLOTS, "usage {usage} -> {code}");
            }
        }
    }

    #[allow(
        clippy::cast_sign_loss,
        reason = "a signed wire field is carried in an unsigned argument"
    )]
    const fn as_u32(value: i32) -> u32 {
        value as u32
    }
}
