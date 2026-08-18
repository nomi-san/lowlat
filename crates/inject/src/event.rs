//! Expansion from one control message to kernel input events.
//!
//! **Nothing here touches a device.** A message goes in, events come out
//! through a sink, and the guest's held state moves. That is what makes the
//! two things most worth proving -- that nothing stays held after a guest
//! vanishes, and that revoking a permission releases what it was holding --
//! testable with no hardware and no display stack (docs/05-host.md section 7).

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
}

/// Where expanded events go.
///
/// A sink rather than a returned slice because one message can produce events
/// for more than one device, and a release produces more than fits in any
/// buffer worth carrying per guest.
pub trait Sink {
    fn emit(&mut self, device: Device, events: &[Event]);
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

    /// Expand one control message.
    pub fn on_control(&mut self, message: &Control<'_>, out: &mut impl Sink) {
        match message.opcode {
            op::KEYBOARD if self.permissions.keyboard => {
                self.keyboard(message.a0, message.a1, message.a2 != 0, out);
            }
            op::MOUSE_BUTTON if self.permissions.pointer => {
                self.button(message.a0, message.a1 != 0, out);
            }
            op::MOUSE_WHEEL if self.permissions.pointer => {
                self.wheel(as_i32(message.a0), as_i32(message.a1), out);
            }
            op::MOUSE_MOTION if self.permissions.pointer => {
                self.motion(message.a0 != 0, as_i32(message.a1), as_i32(message.a2), out);
            }
            op::MOUSE_MOTION_STREAM if self.permissions.pointer => {
                // **The two coordinates are packed differently.** The
                // horizontal one is unsigned and the vertical one is signed,
                // which is not symmetry anybody would invent and is not
                // guessable from the horizontal half.
                let relative = message.a0 & 1 != 0;
                let x = i32::from(low16(message.a1));
                let y = i32::from(high16_signed(message.a1));
                self.motion(relative, x, y, out);
            }
            // **A peer that lost focus says so**, and everything it holds has
            // to come up whether or not it is allowed to press anything now.
            op::RELEASE => self.release_all(out),
            _ => {}
        }
    }

    /// Release everything this guest holds.
    pub fn release_all(&mut self, out: &mut impl Sink) {
        self.release_keys(out);
        self.release_buttons(out);
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
    }

    impl Sink for Recorder {
        fn emit(&mut self, device: Device, events: &[Event]) {
            self.batches.push((device, events.to_vec()));
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
        let packed = |x: u16, y: i16| u32::from(x) | (u32::from(y.cast_unsigned()) << 16);
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
