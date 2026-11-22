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
use lowlat_core::pad::{self, Inbound};

/// Event types, as the kernel numbers them.
pub(crate) const EV_SYN: u16 = 0x00;
pub(crate) const EV_KEY: u16 = 0x01;
pub(crate) const EV_REL: u16 = 0x02;
pub(crate) const EV_ABS: u16 = 0x03;
const EV_MSC: u16 = 0x04;

const SYN_REPORT: u16 = 0;
const MSC_SCAN: u16 = 4;

pub(crate) const REL_X: u16 = 0x00;
pub(crate) const REL_Y: u16 = 0x01;
const REL_HWHEEL: u16 = 0x06;
const REL_WHEEL: u16 = 0x08;
pub(crate) const REL_WHEEL_HI_RES: u16 = 0x0b;
pub(crate) const REL_HWHEEL_HI_RES: u16 = 0x0c;

pub(crate) const ABS_X: u16 = 0x00;
pub(crate) const ABS_Y: u16 = 0x01;

pub(crate) const BTN_LEFT: u16 = 0x110;
pub(crate) const BTN_RIGHT: u16 = 0x111;
pub(crate) const BTN_MIDDLE: u16 = 0x112;
pub(crate) const BTN_SIDE: u16 = 0x113;
pub(crate) const BTN_EXTRA: u16 = 0x114;

const KEY_CAPSLOCK: u16 = 58;
const KEY_NUMLOCK: u16 = 69;

/// The peer's codes for the two lock keys, needed because a tap this host
/// decides on still announces a scanned code the way a real one would.
const USAGE_CAPS_LOCK: u16 = 57;
const USAGE_NUM_LOCK: u16 = 83;

/// The modifier bits that report a lock rather than a held key.
const MOD_NUM: u32 = 0x1000;
const MOD_CAPS: u32 = 0x2000;

/// The modifier bits for left and right Command, Windows or Super.
const MOD_GUI: u32 = 0x0400 | 0x0800;

/// The modifier bits for left and right Alt, for left and right Control, and
/// for left and right Shift.
const MOD_ALT: u32 = 0x0100 | 0x0200;
const MOD_CTRL: u32 = 0x0040 | 0x0080;
const MOD_SHIFT: u32 = 0x0001 | 0x0002;

/// The peer's codes a guest held to one application never presses
/// (docs/05-host.md section 7.4): the Windows keys, Print Screen -- which
/// raises the system's own screen capture -- and the keys that act on the
/// host's sound and media player rather than on an application: mute and the
/// volume on the keyboard page, and the peer's media keys past its end.
const SCOPE_REFUSED: [u16; 6] = [227, 231, 70, 127, 128, 129];
const SCOPE_REFUSED_MEDIA: core::ops::RangeInclusive<u16> = 258..=263;

/// The peer's codes for Tab and Escape, which the task switcher's, the start
/// menu's and the task manager's chords end in, and for the two Alts and the
/// two Controls they are held under.
const USAGE_TAB: u16 = 43;
const USAGE_ESCAPE: u16 = 41;
const USAGE_ALTS: [u16; 2] = [226, 230];
const USAGE_CTRLS: [u16; 2] = [224, 228];
const USAGE_SHIFTS: [u16; 2] = [225, 229];

/// The peer's codes for the eight modifiers, Control to right Command.
const MODIFIERS: core::ops::RangeInclusive<u16> = 224..=231;

/// The peer's codes for the attention chord, in the order it is pressed.
///
/// **The peer's codes rather than the kernel's**, so the chord travels the
/// same mapping every other key does and there is one table to be wrong.
const ATTENTION: [u16; 3] = [224, 226, 76];

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
pub(crate) const SCRATCH: usize = 64;

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

    /// A pad's own report, for a pad sent as its reports (docs/05-host.md
    /// section 7.2): a feature report to keep for the device's creation, or
    /// an input report to write into it -- the first of which creates it.
    /// Nothing is expanded here; the report is the device's own.
    fn report(&mut self, _pad: u32, _inbound: &Inbound<'_>) {}

    /// Whether a pad's own report has anywhere to go: a device presented as
    /// that product, or an application holding one.
    ///
    /// **A sink that says no never sees a report**, and the pad is made from
    /// the whole states a peer sends beside its reports instead. Taken, the
    /// first report would fix the pad as one that drops those states, and the
    /// peer's controller would do nothing at all.
    fn takes_reports(&self) -> bool {
        true
    }

    /// Whether this guest may press Guide, for a sink that plays a pad from
    /// its own reports as the sixteen-button pad rather than from the events
    /// made here (docs/05-host.md section 7.2). What is made here leaves
    /// Guide out already where it may not.
    fn guide(&mut self, _allowed: bool) {}

    /// The host's own lock state, where the platform can read it.
    ///
    /// **Nothing below a display server can**, which is the default; where it
    /// is read, a lock is corrected towards the peer's from the first message.
    fn host_locks(&self) -> Option<Locks> {
        None
    }

    /// Whether the host holds its guests to one application
    /// (docs/05-host.md section 7.4): a sink that can judge one says so, and
    /// [`Sink::gate`] answers for it. None by default.
    fn scoped(&self) -> bool {
        false
    }

    /// Whether the application the host is held to is where input of this
    /// kind lands now. **Asked as the input arrives**, since the owner can
    /// bring anything else to the front at any moment; open where nothing is
    /// held.
    fn gate(&mut self, _at: Gate) -> bool {
        true
    }

    /// Put a pad sent as its own reports at rest -- no button down, the
    /// sticks centred -- for as long as nothing else is written to it: the
    /// pads whose guest is held away from the application meanwhile.
    fn neutral(&mut self, _pad: u32) {}

    /// Type one keystroke of no key at all, where a platform takes Alt let go
    /// alone as a window's menu key: a held guest's Tab or Escape under Alt was
    /// refused, and the Alt that comes up next must not open the menu of the
    /// application in front.
    fn mask_alt(&mut self) {}

    /// Where a held guest's pointer may be (docs/05-host.md section 7.4): the
    /// application's window the picture shows, on its display. Asked as an
    /// absolute position arrives, of a sink that holds its guests
    /// ([`Sink::scoped`]); **nothing where none can be said, and the position
    /// then goes nowhere.**
    fn bounds(&mut self) -> Option<Bounds> {
        None
    }

    /// Bring the application's next window to the front for a held guest's
    /// Tab under Alt, as the system's own switcher would in its place:
    /// `backwards` with Shift down as well, `again` for a Tab after another
    /// under the same Alt, which steps on through the order the first took.
    /// With none of the application's windows in front, its topmost comes
    /// forward.
    fn next_window(&mut self, _backwards: bool, _again: bool) {}
}

/// What a guest held to one application needs of it before its input goes in
/// (docs/05-host.md section 7.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    /// The application in front: keys, pointer motion, pads.
    Front,
    /// The application's window under the pointer as well: buttons and the
    /// wheel, which act where the pointer is rather than where the keys go --
    /// the system's own pointer, while it moves by deltas.
    Pointer,
    /// The application's window at this point of the desktop as well, in its
    /// own pixels measured from its corner, as [`Place`] is: buttons and the
    /// wheel where a position put the pointer. **Not the system's pointer**,
    /// which follows a position a fraction of a millisecond late on some ways
    /// in, so a click judged there lands where the window was not asked about.
    At { x: i32, y: i32 },
}

/// A rectangle of the desktop in its own pixels, measured from its corner as
/// [`Place`] is: its left and top edges in, its right and bottom out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bounds {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

/// Which locks are on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Locks {
    pub num: bool,
    pub caps: bool,
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

/// The extents absolute coordinates are expressed in, and where they land.
///
/// **This is the captured output's shape, not the encoded frame's.** A peer
/// viewing a rotated output already sends coordinates in the orientation the
/// output is in, so a rotated stream swaps what is passed here and rotates
/// nothing at injection.
///
/// **The place is separate from the shape, and with one output they agree.**
/// An absolute device is spread by the layer above over the whole desktop, so
/// a coordinate normalised against the captured output alone lands
/// proportionally short of where it belongs on any desktop bigger than that
/// output, and the far part of the screen cannot be reached at all. Carrying
/// the output's rectangle and the desktop it sits in makes that one
/// multiplication instead of a special case.
///
/// **The rectangle is in the desktop's own units and the shape is in the
/// picture's**, which are the same units only while nothing is scaled. Both
/// ends of the mapping have to be in one space, so the picture's coordinate is
/// converted into the desktop's rather than assumed to already be there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Extents {
    /// The space a peer's coordinates arrive in: the picture's own pixels.
    pub width: u32,
    pub height: u32,
    /// The captured output's rectangle, measured from the desktop's own
    /// corner, and the desktop it sits in.
    pub place: Place,
}

/// Where the captured output sits in the desktop around it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Place {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    pub desktop_width: u32,
    pub desktop_height: u32,
}

impl Extents {
    /// The picture is the whole desktop: one output, or a layout nothing could
    /// be learned about, which behave identically and correctly.
    #[must_use]
    pub const fn alone(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            place: Place {
                x: 0,
                y: 0,
                width,
                height,
                desktop_width: width,
                desktop_height: height,
            },
        }
    }

    /// The picture is a rectangle within a larger desktop.
    #[must_use]
    pub const fn placed(width: u32, height: u32, place: Place) -> Self {
        Self {
            width,
            height,
            place,
        }
    }
}

/// Where a held guest's absolute position put the pointer, in the desktop's
/// pixels measured from its corner, and whether the guest's own position lay
/// past the application's window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct At {
    x: i32,
    y: i32,
    outside: bool,
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
    /// Which device each held button went down on.
    ///
    /// **A release has to reach the device that took the press.** Which
    /// pointer device an event goes to follows whichever kind of motion
    /// arrived last, and a peer switches between them mid-gesture: a drag that
    /// begins with the pointer placed and continues with deltas, or a client
    /// flipping to relative mode while a button is down. Routing the release
    /// by what is current then sends it to a device that never saw the press,
    /// and leaves the button held on a machine nobody is driving.
    pressed_on: [Device; BUTTONS],
    /// Which pointer moved last, so a click lands on the device the position
    /// came from.
    absolute: bool,
    /// The last absolute position, kept only while the pointer is held.
    last_abs: Option<(i32, i32)>,
    /// The last absolute position injected, in the picture's own pixels.
    ///
    /// **What the pointer was told to be**, which is the only thing on this
    /// host that knows where the pointer was *asked* to go rather than where
    /// it ended up. Nothing reports a pointer's hotspot, and the difference
    /// between this and where the display then drew the pointer is exactly it.
    commanded: Option<(i32, i32)>,
    /// Where the last absolute position put the pointer of a guest held to
    /// one application, and whether the guest's own position lay past the
    /// application's window: what its next click is judged by.
    at: Option<At>,
    /// Whether an application on the host holds the pointer, which is a
    /// peer-independent statement that motion is being used to aim rather
    /// than to point (docs/05-host.md section 7).
    relative: bool,
    /// What the peer last said its locks were. `None` until it says anything:
    /// below a display server the first message sets the baseline and taps
    /// nothing, because the state it would be correcting towards cannot be
    /// read and a guess is wrong half the time.
    num_lock: Option<bool>,
    caps_lock: Option<bool>,
    /// Whether this guest currently has the pointer.
    ///
    /// **Only the pointer**, and only when the host is arbitrating it. Every
    /// guest types at once and every guest's pads are its own devices; the
    /// pointer is the one thing they genuinely share, because the display
    /// stack merges every pointer device on a seat into one cursor.
    floor: bool,
    /// Whether this guest owns the host, which alone lets it press Guide.
    owner: bool,
    /// Whether the application the host is held to was in front when last
    /// asked: its going is when what the guest holds comes up.
    front: bool,
    /// Whether a held guest's Tab under the Alt it holds now has already
    /// brought another of the application's windows forward.
    switching: bool,
    /// One entry per pad this guest holds, in the order they first arrived.
    pads: [Option<Pad>; MAX_PADS],
    /// What has arrived, for the line a live run is read from.
    tally: Tally,
    scratch: [Event; SCRATCH],
    used: usize,
}

impl Injector {
    /// The last absolute position injected, in the picture's own pixels.
    pub fn commanded(&self) -> Option<(i32, i32)> {
        self.commanded
    }

    /// The extents absolute coordinates arrive in.
    ///
    /// **Settled by the stream, not by configuration.** A display decides its
    /// own size and the peer is told what the picture really is, so the space
    /// its coordinates are expressed in is that size and not the one a host
    /// was started with. The two differing scales every absolute position by
    /// the ratio between them, which reads as a pointer that reaches the edge
    /// of the screen three quarters of the way across.
    pub fn set_extents(&mut self, extents: Extents) {
        self.extents = extents;
    }

    #[must_use]
    pub fn new(extents: Extents) -> Self {
        Self {
            extents,
            permissions: Permissions::default(),
            keys: [false; KEY_SLOTS],
            buttons: [false; BUTTONS],
            pressed_on: [Device::PointerAbsolute; BUTTONS],
            absolute: false,
            last_abs: None,
            commanded: None,
            at: None,
            relative: false,
            num_lock: None,
            caps_lock: None,
            floor: true,
            owner: false,
            front: true,
            switching: false,
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
        if was.gamepad && !permissions.gamepad {
            self.release_pads(out);
        }
    }

    /// Say whether this guest owns the host.
    ///
    /// **Guide is the owner's alone.** On the Xbox pad it raises the host's
    /// own overlay -- the system's game bar, a launcher's -- which reaches
    /// past whatever a guest was given to play, so for a guest that does not
    /// own the host a press of Guide alone is dropped and a whole state is
    /// taken without it -- **and for the owner while the host holds its
    /// guests to one application** (docs/05-host.md section 7.4), which holds
    /// the owner as well. A pad presented as its product keeps its own
    /// button, which is the product's.
    pub fn set_owner(&mut self, owner: bool, out: &mut impl Sink) {
        self.owner = owner;
        out.guide(owner);
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

    /// Ask whether the application this guest is held to is still in front,
    /// and let go of what the guest holds if it is not.
    ///
    /// **On a timer as well as as input arrives**, for the reason the pointer
    /// is: the desk brings another program to the front while the guest
    /// holds a key and sends nothing, and the key is typed into that program
    /// by the system's own repeat until the guest's release comes.
    pub fn follow_gate(&mut self, out: &mut impl Sink) {
        if !self.held(out) {
            self.front = true;
            return;
        }
        let front = out.gate(Gate::Front);
        self.follow(front, out);
    }

    /// Whether this guest is held to one application: a host that holds its
    /// guests to one. **The owner as well** (docs/05-host.md section 7.4):
    /// every seat is shown the application alone, and an owner left free
    /// behind that picture steers the desktop unseen.
    fn held(&self, out: &impl Sink) -> bool {
        out.scoped()
    }

    /// Whether this guest may press Guide: the owner, while nothing holds it.
    fn presses_guide(&self, out: &impl Sink) -> bool {
        self.owner && !self.held(out)
    }

    /// Whether input that needs `at` goes in, letting go of what the guest
    /// holds where the application has left the front since it was last
    /// asked.
    fn admits(&mut self, at: Gate, out: &mut impl Sink) -> bool {
        if !self.held(out) {
            return true;
        }
        let front = out.gate(Gate::Front);
        self.follow(front, out);
        front && (at == Gate::Front || self.under(out))
    }

    /// Whether a click or a scroll lands on the application: **where the
    /// guest put the pointer** while it points -- nowhere past the window it
    /// is held to, rather than on that window's edge -- and where the system's
    /// pointer is while it moves by deltas.
    fn under(&self, out: &mut impl Sink) -> bool {
        match self.at {
            Some(at) if self.absolute => !at.outside && out.gate(Gate::At { x: at.x, y: at.y }),
            _ => out.gate(Gate::Pointer),
        }
    }

    /// The application in front or not, as just asked: its going releases
    /// every key and button and puts every pad at rest, as a revoked
    /// permission does (docs/05-host.md section 7.4).
    fn follow(&mut self, front: bool, out: &mut impl Sink) {
        if self.front && !front {
            self.release_keys(out);
            self.release_buttons(out);
            self.centre_pads(out);
            for pad in self.pads.iter().flatten() {
                if pad.raw {
                    out.neutral(pad.id);
                }
            }
        }
        self.front = front;
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

    /// Report whether an application on the host holds the pointer.
    ///
    /// **The intent, not the overlay** (docs/05-host.md section 8.1): the
    /// pointer hidden by the system's show state, or a picture of one value,
    /// read as motion arrives. A pointer that merely left the captured output
    /// is not held, and converting its motion would lose it. The display
    /// that has a source for the signal publishes it; the conversion is here
    /// because it owns the last-position state, which every motion path has
    /// to maintain either way.
    pub fn set_relative(&mut self, relative: bool) {
        if !relative {
            self.last_abs = None;
        }
        self.relative = relative;
    }

    /// What this guest has sent, by kind.
    #[must_use]
    pub fn tally(&self) -> Tally {
        self.tally
    }

    /// Expand one control message.
    pub fn on_control(&mut self, message: &Control<'_>, out: &mut impl Sink) {
        self.tally.count(message.opcode);
        // **A release is never held back** by the application a guest is held
        // to: what went down while it was in front comes up wherever the
        // focus is now, and what did not is dropped as never held.
        match message.opcode {
            op::KEYBOARD if self.permissions.keyboard => {
                let pressed = message.a2 != 0;
                if !pressed || self.admits(Gate::Front, out) {
                    self.keyboard(message.a0, message.a1, pressed, out);
                } else {
                    self.behind(message.a0, message.a1, out);
                }
            }
            op::MOUSE_BUTTON if self.pointer_allowed() => {
                let pressed = message.a1 != 0;
                if !pressed || self.admits(Gate::Pointer, out) {
                    self.button(message.a0, pressed, out);
                }
            }
            op::MOUSE_WHEEL if self.pointer_allowed() && self.admits(Gate::Pointer, out) => {
                self.wheel(as_i32(message.a0), as_i32(message.a1), out);
            }
            op::MOUSE_MOTION if self.pointer_allowed() && self.admits(Gate::Front, out) => {
                self.motion(message.a0 != 0, as_i32(message.a1), as_i32(message.a2), out);
            }
            op::MOUSE_MOTION_STREAM if self.pointer_allowed() && self.admits(Gate::Front, out) => {
                // **The two coordinates are packed differently.** The
                // horizontal one is unsigned and the vertical one is signed,
                // which is not symmetry anybody would invent and is not
                // guessable from the horizontal half.
                let relative = message.a0 & 1 != 0;
                let x = i32::from(low16(message.a1));
                let y = i32::from(high16_signed(message.a1));
                self.motion(relative, x, y, out);
            }
            // **A pad sent as its own report is that report** (docs/05-host.md
            // section 7.2): the first message that can create a pad fixes
            // its family, a report pad takes only reports, and a
            // sixteen-button pad ignores them.
            op::PAD_REPORT if self.permissions.gamepad && self.admits(Gate::Front, out) => {
                self.pad_report(message, out);
            }
            op::GAMEPAD_BUTTON if self.permissions.gamepad && self.admits(Gate::Front, out) => {
                // Arguments are the button, whether it is down, and the pad.
                let Some(bit) = gamepad::bit_for_index(message.a0) else {
                    return;
                };
                if bit == gamepad::bit::GUIDE && !self.presses_guide(out) {
                    return;
                }
                let Some(index) = self.standard_pad(message.a2, out) else {
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
            op::GAMEPAD_AXIS if self.permissions.gamepad && self.admits(Gate::Front, out) => {
                // Arguments are the axis, its value, and the pad.
                let Some(index) = self.standard_pad(message.a2, out) else {
                    return;
                };
                let Some(id) = self.pads.get(index).and_then(|p| p.as_ref()).map(|p| p.id) else {
                    return;
                };
                let value = high16_signed(message.a1 << 16);
                self.axis(id, message.a0, value, out);
            }
            // **An unplug lets go and presses nothing**, so it needs no
            // permission: a guest whose pads were taken from it may still take
            // them away.
            op::GAMEPAD_UNPLUG => {
                self.unplug(message.a2, out);
            }
            op::GAMEPAD_STATE if self.permissions.gamepad && self.admits(Gate::Front, out) => {
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

    /// Type the attention chord, and leave nothing of it held.
    ///
    /// **A chord this host decides on, not a key a guest pressed**, so it is
    /// injected the way a person would type it -- both modifiers down, the
    /// third key down, then all three up in reverse -- and the guest's own
    /// held state is brought into line with the result rather than consulted
    /// first. A guest holding one of the three when it asks gets it released,
    /// which is what pressing the chord on a real keyboard would leave behind
    /// too.
    ///
    /// **Refused without the keyboard**, because the chord is keystrokes and a
    /// guest that may not type may not type these either.
    ///
    /// Answers whether it was typed.
    pub fn secure_attention(&mut self, out: &mut impl Sink) -> bool {
        if !self.permissions.keyboard {
            return false;
        }
        self.used = 0;
        for usage in ATTENTION {
            let Some(key) = usage::key_code(usage) else {
                continue;
            };
            self.key_events(usage, key, true);
        }
        for usage in ATTENTION.iter().rev() {
            let Some(key) = usage::key_code(*usage) else {
                continue;
            };
            self.key_events(*usage, key, false);
            if let Some(held) = self.keys.get_mut(usize::from(key)) {
                *held = false;
            }
        }
        self.flush(Device::Keyboard, out);
        true
    }

    /// The slot a peer's pad identifier occupies, taking one if it is new.
    ///
    /// **Bounded.** The identifier is the peer's and nothing constrains it, so
    /// a peer that varies the field would otherwise get a device per value.
    fn pad(&mut self, id: u32, raw: bool, out: &mut impl Sink) -> Option<usize> {
        if let Some(index) = self.pads.iter().position(|p| p.is_some_and(|p| p.id == id)) {
            return Some(index);
        }
        let index = self.pads.iter().position(Option::is_none)?;
        *self.pads.get_mut(index)? = Some(Pad {
            id,
            buttons: 0,
            raw,
        });
        // **The device is created by the first event sent to it.** Nothing is
        // sent here, so a pad that is announced and never used costs a slot
        // and no device.
        let _ = out;
        Some(index)
    }

    /// The slot of a sixteen-button pad, taking one if the identifier is
    /// new; `None` for a pad sent as its own reports, whose report is the
    /// superset of what the message would say.
    fn standard_pad(&mut self, id: u32, out: &mut impl Sink) -> Option<usize> {
        let index = self.pad(id, false, out)?;
        let raw = self
            .pads
            .get(index)
            .and_then(|p| p.as_ref())
            .is_some_and(|p| p.raw);
        (!raw).then_some(index)
    }

    /// A pad's own report: to the sink for a pad that is, or becomes, a
    /// report pad. An established peer's ten-byte block names no product and
    /// creates nothing; a report for a sixteen-button pad is dropped, and so
    /// is a new pad's report the sink has nowhere to put.
    ///
    /// **The sink's answer fixes a new pad's family alone**: a pad already
    /// sent as its reports keeps them whatever the sink answers now, since it
    /// drops its states and would do nothing at all.
    fn pad_report(&mut self, message: &Control<'_>, out: &mut impl Sink) {
        let Some(inbound) = pad::parse_report(message) else {
            return;
        };
        if matches!(inbound, Inbound::TouchBlock) {
            return;
        }
        let known = self
            .pads
            .iter()
            .flatten()
            .find(|p| p.id == message.a1)
            .map(|p| p.raw);
        let raw = match known {
            Some(raw) => raw,
            None => out.takes_reports() && self.pad(message.a1, true, out).is_some(),
        };
        if raw {
            out.report(message.a1, &inbound);
        }
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

    /// Let go of every pad, as a withdrawn pad permission does
    /// (docs/05-host.md section 7.2): a sixteen-button pad is centred and
    /// stays, and a report pad is unplugged, since its reports are refused
    /// from now on and its device has no state but the last of them.
    fn release_pads(&mut self, out: &mut impl Sink) {
        self.centre_pads(out);
        for index in 0..MAX_PADS {
            if let Some(pad) = self.pads.get(index).copied().flatten()
                && pad.raw
            {
                self.unplug(pad.id, out);
            }
        }
    }

    /// Centre every pad without taking any away. A report pad is what its
    /// last report says, and its reports keep coming whether or not the peer
    /// has focus, so it is left alone.
    fn centre_pads(&mut self, out: &mut impl Sink) {
        for index in 0..MAX_PADS {
            let Some(pad) = self.pads.get(index).copied().flatten() else {
                continue;
            };
            if pad.raw {
                continue;
            }
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
    ///
    /// **The vertical axes point down here**, unlike the whole state's
    /// (docs/01-protocol.md section 11.1), which is the axis's own direction,
    /// so nothing is inverted.
    fn axis(&mut self, id: u32, which: u32, value: i16, out: &mut impl Sink) {
        let (code, scaled) = match which {
            0 => (gamepad::axis::X, gamepad::stick(value, false)),
            1 => (gamepad::axis::Y, gamepad::stick(value, false)),
            2 => (gamepad::axis::RX, gamepad::stick(value, false)),
            3 => (gamepad::axis::RY, gamepad::stick(value, false)),
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
        let mut buttons = u16::from_be_bytes(pair(0));
        if !self.presses_guide(out) {
            buttons &= !gamepad::bit::GUIDE;
        }
        let Some(index) = self.standard_pad(id, out) else {
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

    /// Whether a key a guest held to one application presses is one it may
    /// not: a key of [`SCOPE_REFUSED`] or past the peer's keyboard page, a key
    /// under the Windows keys, Tab under Alt, Escape under Alt or Control.
    ///
    /// **A modifier is down where either side says so**: the peer's mask, or
    /// a key this guest holds here.
    fn refused_in_scope(&self, usage: u16, modifiers: u32) -> bool {
        let alt = modifiers & MOD_ALT != 0 || self.holds_any(&USAGE_ALTS);
        let ctrl = modifiers & MOD_CTRL != 0 || self.holds_any(&USAGE_CTRLS);
        SCOPE_REFUSED.contains(&usage)
            || SCOPE_REFUSED_MEDIA.contains(&usage)
            || modifiers & MOD_GUI != 0
            || (usage == USAGE_TAB && alt)
            || (usage == USAGE_ESCAPE && (alt || ctrl))
    }

    /// A held guest's key pressed while none of the application's windows is
    /// in front, which goes nowhere -- **but Tab under Alt brings the
    /// application's topmost window forward** (docs/05-host.md section 7.4):
    /// a launcher's main window left unfocused as its splash went, or one
    /// shown behind another program, has no other way back for a guest held
    /// to it. A Tab after it under the same Alt steps on. Alt is the peer's
    /// mask alone here, its press having gone nowhere either.
    fn behind(&mut self, code: u32, modifiers: u32, out: &mut impl Sink) {
        if code != u32::from(USAGE_TAB) || modifiers & MOD_ALT == 0 {
            return;
        }
        let again = core::mem::replace(&mut self.switching, true);
        out.next_window(modifiers & MOD_SHIFT != 0, again);
    }

    /// Whether this guest holds any of the keys the peer names by these codes.
    fn holds_any(&self, usages: &[u16]) -> bool {
        usages.iter().any(|&usage| {
            usage::key_code(usage)
                .and_then(|key| self.keys.get(usize::from(key)))
                .copied()
                .unwrap_or(false)
        })
    }

    fn keyboard(&mut self, code: u32, modifiers: u32, pressed: bool, out: &mut impl Sink) {
        let Ok(peer_code) = u16::try_from(code) else {
            traced(code, modifiers, pressed, 0, "no-key");
            return;
        };
        let Some(key) = usage::key_code(peer_code) else {
            traced(code, modifiers, pressed, 0, "no-key");
            return;
        };
        // An Alt let go ends a run of Tabs under it, as a key with no Alt down
        // anywhere does: the next Tab under an Alt starts the order afresh.
        let alt = modifiers & MOD_ALT != 0 || self.holds_any(&USAGE_ALTS);
        if !alt || (!pressed && USAGE_ALTS.contains(&peer_code)) {
            self.switching = false;
        }
        // **A guest held to one application presses nothing that reaches past
        // it** (docs/05-host.md section 7.4): the keys that open the shell or
        // switch away from the application, and those that act on the host's
        // own sound. Alt or Control going down under a held Tab or Escape lets
        // the held key go first, or the system's own repeat would make the
        // chord from it.
        if pressed && self.held(out) {
            if self.refused_in_scope(peer_code, modifiers) {
                // **A key refused under Alt leaves the Alt alone**, and an Alt
                // let go alone is the window's menu key.
                if alt {
                    out.mask_alt();
                }
                // **Tab under Alt brings the application's next window to the
                // front** in place of the system's switcher, which the picture
                // would not show: Shift steps backwards, and a Tab after
                // another under the same Alt steps on. A press reaches here
                // only while the application is in front.
                if alt && peer_code == USAGE_TAB {
                    let backwards = modifiers & MOD_SHIFT != 0 || self.holds_any(&USAGE_SHIFTS);
                    let again = core::mem::replace(&mut self.switching, true);
                    out.next_window(backwards, again);
                }
                traced(code, modifiers, pressed, key, "scope");
                return;
            }
            if USAGE_ALTS.contains(&peer_code) || USAGE_CTRLS.contains(&peer_code) {
                let ends = [USAGE_TAB, USAGE_ESCAPE].map(usage::key_code);
                self.release_keys_but(out, |held| !ends.contains(&Some(held)));
            }
        }
        let host = out.host_locks();
        // **Where the host's locks can be read, a lock key is the peer's lock
        // state, never a keystroke.** A peer whose platform reports a lock as
        // a modifier sends the key down as the lock goes on and up as it goes
        // off, so a key passed through toggles the host once every two
        // presses; every peer's message carries the state, and the host is
        // brought to it.
        if host.is_some() && (key == KEY_CAPSLOCK || key == KEY_NUMLOCK) {
            self.used = 0;
            self.lock_sync(modifiers, key, host);
            self.flush(Device::Keyboard, out);
            traced(code, modifiers, pressed, key, "lock");
            return;
        }
        // **While the peer's mask holds Command, Windows or Super, none of its
        // other keys stays held here.** A macOS peer's window never hears a key
        // let go while Command is down, so it never sends the release, and a
        // key left held is typed again by this machine's own repeat until the
        // next press of it. The mask says Command whatever the codes say -- a
        // peer that swaps Command for Control swaps the codes alone -- so a key
        // down as Command arrives comes up then, and one pressed under it goes
        // down and up at once: a keystroke per press or repeat the peer sends,
        // which is all a shortcut is.
        let commanding = modifiers & MOD_GUI != 0;
        if commanding {
            self.release_keys_but(out, is_modifier);
        }
        let tapped = commanding && pressed && !is_modifier(key);
        let Some(held) = self.keys.get_mut(usize::from(key)) else {
            traced(code, modifiers, pressed, key, "no-slot");
            return;
        };
        // **A release for a key that was never pressed is dropped.** A peer
        // that reconnects mid-keystroke, or one whose permission was revoked
        // and restored, sends releases for keys this host never saw down, and
        // passing those on interrupts whatever a local user is holding.
        if !pressed && !*held {
            traced(code, modifiers, pressed, key, "not-held");
            return;
        }
        *held = pressed && !tapped;

        self.used = 0;
        self.lock_sync(modifiers, key, host);
        self.key_events(peer_code, key, pressed);
        if tapped {
            self.key_events(peer_code, key, false);
        }
        self.flush(Device::Keyboard, out);
        traced(
            code,
            modifiers,
            pressed,
            key,
            if tapped { "tapped" } else { "sent" },
        );
    }

    /// Bring the local locks into step with what the peer reports.
    ///
    /// **A peer reports its lock state on every keystroke, and that is the
    /// only way this host learns of a change it did not see the key for.** A
    /// guest that toggles a lock while looking at something else comes back
    /// with a state this machine never applied, and from then on every letter
    /// it types is the wrong case.
    ///
    /// Only a change is acted on, and below a display server never on the
    /// message carrying the lock key itself: that key is being injected
    /// anyway and would toggle twice.
    ///
    /// **Where the host's own state can be read, a change is corrected
    /// towards it**: a lock is tapped only where the host's differs, so a lock
    /// already where the peer has it is left alone. The peer's first message
    /// counts for a lock it has on and not for one it has off, since a peer
    /// with no such key reports it off and would turn the host's off. The
    /// lock key's own message is the peer's state outright.
    ///
    /// **Below a display server the initial state cannot be repaired.**
    /// Correcting towards the local state means reading it, and there is
    /// nothing there that reports it for the seat. The nearest thing is the
    /// light on a physical keyboard, which is exposed by the kernel and which
    /// the display server does drive, but it presumes a physical keyboard
    /// exists and is one keyboard's answer to a question about the seat. It is
    /// not used, and the drift it would fix is one-time and self-correcting
    /// the first time anybody presses the key.
    fn lock_sync(&mut self, modifiers: u32, injected: u16, host: Option<Locks>) {
        let num = modifiers & MOD_NUM != 0;
        let was = self.num_lock.replace(num);
        if lock_owed(was, num, host.map(|h| h.num), injected == KEY_NUMLOCK) {
            self.tap(USAGE_NUM_LOCK, KEY_NUMLOCK);
        }
        let caps = modifiers & MOD_CAPS != 0;
        let was = self.caps_lock.replace(caps);
        if lock_owed(was, caps, host.map(|h| h.caps), injected == KEY_CAPSLOCK) {
            self.tap(USAGE_CAPS_LOCK, KEY_CAPSLOCK);
        }
    }

    fn tap(&mut self, usage: u16, key: u16) {
        lowlat_common::log_debug!("inject: lock tapped, key={key}");
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

        // Chosen once, on the way down, and remembered for the way up. The
        // side buttons are the relative pointer's whichever pointer moved
        // last: the absolute one declares the three primary buttons and no
        // more, which is what keeps it a pointer to the joystick handler.
        let device = if pressed {
            let device = if code == BTN_SIDE || code == BTN_EXTRA {
                Device::Pointer
            } else {
                self.pointer()
            };
            if let Some(slot) = self.pressed_on.get_mut(index) {
                *slot = device;
            }
            device
        } else {
            self.pressed_on
                .get(index)
                .copied()
                .unwrap_or(Device::Pointer)
        };

        self.used = 0;
        self.push(EV_KEY, code, i32::from(pressed));
        self.report();
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

        // **A held pointer means the position is being used to aim.** The
        // peer keeps sending absolute coordinates because it does not know
        // that, so the difference between them is the motion that was meant.
        // The first sample after the pointer is taken establishes where it is
        // and moves nothing; treating it as a delta from the origin throws
        // the aim across the screen.
        if self.relative {
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
        let place = self.extents.place;
        let axes = if self.held(out) {
            self.held_axes(x, y, out)
        } else {
            // **Recorded where it is actually injected**, past the permission
            // gate and past the arbiter, because it is read as evidence of
            // where the pointer was put. A position that was refused was not
            // commanded.
            self.commanded = Some((x, y));
            Some((
                scale(
                    x,
                    self.extents.width,
                    place.x,
                    place.width,
                    place.desktop_width,
                ),
                scale(
                    y,
                    self.extents.height,
                    place.y,
                    place.height,
                    place.desktop_height,
                ),
            ))
        };
        let Some((across, down)) = axes else {
            return;
        };
        self.used = 0;
        self.push(EV_ABS, ABS_X, across);
        self.push(EV_ABS, ABS_Y, down);
        self.report();
        self.flush(Device::PointerAbsolute, out);
    }

    /// The axes a held guest's position goes in at (docs/05-host.md section
    /// 7.4): **on the application's window the picture shows**, a position
    /// past it kept at its edge and remembered as past it, so that a click
    /// asked there goes nowhere rather than onto that edge -- a window's
    /// border resizes it, its caption's buttons close it. Nothing where none of the
    /// window is on the captured output, and a click after it goes nowhere
    /// either.
    fn held_axes(&mut self, x: i32, y: i32, out: &mut impl Sink) -> Option<(i32, i32)> {
        let place = self.extents.place;
        let (width, height) = (self.extents.width, self.extents.height);
        let spans = out
            .bounds()
            .and_then(|bounds| on_output(bounds, place))
            .filter(|_| width > 0 && height > 0);
        let Some((across, down)) = spans else {
            self.at = Some(At {
                x: 0,
                y: 0,
                outside: true,
            });
            return None;
        };
        let (px, py) = (pixel(x, width, place.width), pixel(y, height, place.height));
        let outside = !across.contains(&px) || !down.contains(&py);
        let px = px.clamp(*across.start(), *across.end());
        let py = py.clamp(*down.start(), *down.end());
        let (dx, dy) = (i64::from(place.x) + px, i64::from(place.y) + py);
        self.at = Some(At {
            x: i32::try_from(dx).unwrap_or(i32::MAX),
            y: i32::try_from(dy).unwrap_or(i32::MAX),
            outside,
        });
        self.commanded = Some(if outside {
            (
                picture(px, width, place.width),
                picture(py, height, place.height),
            )
        } else {
            (x, y)
        });
        Some((
            axis(dx, place.desktop_width),
            axis(dy, place.desktop_height),
        ))
    }

    /// Release every key this guest holds, as a withdrawn keyboard permission
    /// does: for keys about to go another way, so none is left down on the way
    /// they came (docs/05-host.md section 7).
    pub fn release_keys(&mut self, out: &mut impl Sink) {
        self.release_keys_but(out, |_| false);
        self.switching = false;
        // The peer's locks are no longer known, rather than known to be off.
        // Its next message sets the baseline again without toggling anything.
        self.num_lock = None;
        self.caps_lock = None;
    }

    /// Release every key this guest holds that `kept` does not keep.
    fn release_keys_but(&mut self, out: &mut impl Sink, kept: impl Fn(u16) -> bool) {
        self.used = 0;
        for key in 0..KEY_SLOTS {
            if self.keys.get(key) != Some(&true) {
                continue;
            }
            #[allow(
                clippy::cast_possible_truncation,
                reason = "the loop bound is the array length, which is 256"
            )]
            let code = key as u16;
            if kept(code) {
                continue;
            }
            if let Some(held) = self.keys.get_mut(key) {
                *held = false;
            }
            // A release is not a keystroke, so the scanned code is left off:
            // nothing scanned anything, and a consumer correlating the two
            // would see a code with no cause.
            self.push(EV_KEY, code, 0);
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
    }

    /// Release every pointer button this guest holds, as a withdrawn pointer
    /// permission does: for buttons about to go another way, so none is left
    /// down on the way they came (docs/05-host.md section 7).
    pub fn release_buttons(&mut self, out: &mut impl Sink) {
        // **Grouped by the device each went down on**, because a release sent
        // anywhere else leaves the button held where it really is. Two passes
        // over five buttons, on a path that runs when a guest leaves.
        for device in [Device::PointerAbsolute, Device::Pointer] {
            self.used = 0;
            for index in 0..BUTTONS {
                if self.buttons.get(index) != Some(&true) {
                    continue;
                }
                if self.pressed_on.get(index).copied() != Some(device) {
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
                self.flush(device, out);
            }
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
    /// Sent as its own reports rather than as the sixteen-button messages,
    /// fixed by the first message that created it.
    raw: bool,
}

/// One line per keyboard message, at the debug level.
///
/// **The decision, not just what arrived.** A key that will not release is
/// either a release the peer never sent or one this dropped for want of a
/// matching press, and those have different fixes and look identical from
/// outside: `took` names which happened. A line taken at the wire could not
/// tell them apart.
///
/// Not on the trace level, because that one is compiled out of the build a
/// live run uses. It costs a comparison while the level is below it, and keys
/// arrive at the rate a person types.
fn traced(usage: u32, modifiers: u32, pressed: bool, key: u16, took: &str) {
    lowlat_common::log_debug!(
        "inject: key seen, usage={usage} mod={modifiers:#06x} pressed={} key={key} took={took}",
        u8::from(pressed)
    );
}

/// Whether a lock is tapped: `was` and `now` what the peer said of it at its
/// last message and at this one, `host` the host's own state where that can
/// be read, and `named` whether this message is the lock's own key.
fn lock_owed(was: Option<bool>, now: bool, host: Option<bool>, named: bool) -> bool {
    match host {
        Some(host) if named => host != now,
        Some(host) => was.map_or(now, |was| was != now) && host != now,
        None => !named && was == Some(!now),
    }
}

/// Whether a kernel key code is a modifier's, by the table's own translation
/// of the peer's codes for them: one table to be wrong.
fn is_modifier(key: u16) -> bool {
    MODIFIERS
        .into_iter()
        .any(|usage| usage::key_code(usage) == Some(key))
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

/// A coordinate in the picture, onto the absolute axis the desktop is spread
/// over.
///
/// Three steps, and the middle one is the whole of what a second display
/// costs: clamp into the picture, convert into the captured output's own
/// rectangle, then place that rectangle within the desktop. With one output
/// the rectangle is the desktop and the two conversions cancel.
///
/// **The extent is inclusive.** A peer reports the far edge as the width
/// itself rather than one less, so that edge is the output's last pixel.
///
/// **Clamping is what keeps a guest on the captured display.** A coordinate
/// past the picture would otherwise place the pointer on the next output,
/// where nothing here can see it: the pointer plane on this output empties,
/// which is indistinguishable from an application hiding the pointer, and the
/// peer is told to switch to relative motion.
///
/// **Aimed at the pixel's centre** (*2026-10-15*). The input stack takes the
/// axis back to a pixel as `value * desktop / 65536`, truncated, on both
/// platforms, so a value at a pixel's near edge lands on the one before it:
/// measured, every position a pixel up and to the left, and an output's first
/// column on the neighbouring output's last.
fn scale(value: i32, extent: u32, origin: u32, within: u32, desktop: u32) -> i32 {
    if extent == 0 || desktop == 0 {
        return 0;
    }
    axis(i64::from(origin) + pixel(value, extent, within), desktop)
}

/// The captured output's pixel a coordinate in the picture names, clamped
/// into the picture first: [`scale`]'s first two steps.
fn pixel(value: i32, extent: u32, within: u32) -> i64 {
    let extent = i64::from(extent.max(1));
    let clamped = i64::from(value).clamp(0, extent);
    (clamped * i64::from(within) / extent).clamp(0, (i64::from(within) - 1).max(0))
}

/// The coordinate in the picture an output's pixel is at: [`pixel`] the
/// other way.
fn picture(pixel: i64, extent: u32, within: u32) -> i32 {
    i32::try_from(pixel * i64::from(extent) / i64::from(within.max(1))).unwrap_or(i32::MAX)
}

/// The absolute axis value aimed at a desktop pixel's centre: [`scale`]'s
/// last step.
fn axis(placed: i64, desktop: u32) -> i32 {
    if desktop == 0 {
        return 0;
    }
    let desktop = i64::from(desktop);
    #[allow(
        clippy::cast_possible_truncation,
        reason = "the quotient is clamped to the axis range, which is 65535"
    )]
    {
        ((2 * placed + 1) * (i64::from(ABS_RANGE) + 1) / (2 * desktop))
            .clamp(0, i64::from(ABS_RANGE)) as i32
    }
}

/// The captured output's pixels a rectangle of the desktop covers, on each
/// axis: nothing where none of it is on the output.
fn on_output(
    bounds: Bounds,
    place: Place,
) -> Option<(
    core::ops::RangeInclusive<i64>,
    core::ops::RangeInclusive<i64>,
)> {
    let span = |from: i32, to: i32, origin: u32, within: u32| {
        let first = (i64::from(from) - i64::from(origin)).max(0);
        let last = (i64::from(to) - 1 - i64::from(origin)).min(i64::from(within) - 1);
        (first <= last).then_some(first..=last)
    };
    Some((
        span(bounds.left, bounds.right, place.x, place.width)?,
        span(bounds.top, bounds.bottom, place.y, place.height)?,
    ))
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
        /// The pad reports handed on: the pad, and whether an input report
        /// (true) or a feature report.
        reports: Vec<(u32, bool)>,
        /// The host's locks, for a sink that can read them.
        locks: Option<Locks>,
        /// A sink with nowhere to put a pad's own report.
        refuses_reports: bool,
        /// What the sink was last told of Guide.
        guide: Option<bool>,
        /// The application the host holds its guests to, where it holds them:
        /// whether it is in front, and whether it is under the pointer.
        scope: Option<(bool, bool)>,
        /// The pads sent as their reports that were put at rest.
        rested: Vec<u32>,
        /// How many keystrokes of no key were typed for an Alt.
        masks: u32,
        /// Where a held guest's pointer may be, in the desktop's pixels.
        bounds: Option<Bounds>,
        /// The points of the desktop a click or a scroll was judged at.
        judged: Vec<(i32, i32)>,
        /// The application's next window asked for: backwards, and again.
        switched: Vec<(bool, bool)>,
    }

    impl Sink for Recorder {
        fn emit(&mut self, device: Device, events: &[Event]) {
            self.batches.push((device, events.to_vec()));
        }

        fn unplug(&mut self, pad: u32) {
            self.unplugged.push(pad);
        }

        fn report(&mut self, pad: u32, inbound: &Inbound<'_>) {
            self.reports
                .push((pad, matches!(inbound, Inbound::Input { .. })));
        }

        fn takes_reports(&self) -> bool {
            !self.refuses_reports
        }

        fn guide(&mut self, allowed: bool) {
            self.guide = Some(allowed);
        }

        fn host_locks(&self) -> Option<Locks> {
            self.locks
        }

        fn scoped(&self) -> bool {
            self.scope.is_some()
        }

        fn gate(&mut self, at: Gate) -> bool {
            if let Gate::At { x, y } = at {
                self.judged.push((x, y));
            }
            self.scope.is_none_or(|(front, under)| match at {
                Gate::Front => front,
                Gate::Pointer | Gate::At { .. } => under,
            })
        }

        fn neutral(&mut self, pad: u32) {
            self.rested.push(pad);
        }

        fn mask_alt(&mut self) {
            self.masks += 1;
        }

        fn bounds(&mut self) -> Option<Bounds> {
            self.bounds
        }

        fn next_window(&mut self, backwards: bool, again: bool) {
            self.switched.push((backwards, again));
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
        Injector::new(Extents::alone(1920, 1080))
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
        let menu = usage::key_code(101).unwrap();
        assert_eq!(usage::key_code(118), Some(menu));
        inject.on_control(&key(101, true), &mut out);
        out.batches.clear();
        inject.on_control(&key(118, false), &mut out);
        assert_eq!(out.keys_at(0), vec![menu]);

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

    /// The tap is built from these, so they must be the table's own codes
    /// for the two lock keys on whichever platform the table is.
    #[test]
    fn the_lock_keys_are_the_tables_own() {
        assert_eq!(usage::key_code(USAGE_CAPS_LOCK), Some(KEY_CAPSLOCK));
        assert_eq!(usage::key_code(USAGE_NUM_LOCK), Some(KEY_NUMLOCK));
    }

    /// **Where the host's locks can be read, a lock the peer has on at its
    /// first message is applied** before the key; one already there is left
    /// alone.
    #[test]
    fn a_lock_the_peer_has_on_at_first_is_applied() {
        let mut out = Recorder {
            locks: Some(Locks {
                num: true,
                caps: false,
            }),
            ..Recorder::default()
        };
        let mut inject = injector();
        inject.on_control(&control(op::KEYBOARD, 4, MOD_CAPS | MOD_NUM, 1), &mut out);
        assert_eq!(out.keys_at(1), vec![KEY_CAPSLOCK, 30]);
        assert_eq!(out.keys_at(0), vec![KEY_CAPSLOCK]);
    }

    /// **A lock the peer reports off at its first message is left alone**: a
    /// peer with no such key reports it off, and taking that at its word would
    /// turn the host's lock off for every guest that has none.
    #[test]
    fn a_lock_the_peer_reports_off_at_first_is_left_alone() {
        let mut out = Recorder {
            locks: Some(Locks {
                num: true,
                caps: true,
            }),
            ..Recorder::default()
        };
        let mut inject = injector();
        inject.on_control(&control(op::KEYBOARD, 4, 0, 1), &mut out);
        assert_eq!(out.keys_at(1), vec![30]);
    }

    /// **Where the host's locks can be read, a lock key is the peer's lock
    /// state, never a keystroke.** A peer that reports the lock as a modifier
    /// sends the key down as the lock goes on and up as it goes off; passed
    /// through, the host toggled once every two presses. Each message brings
    /// the host to the state it reports, whichever way the peer sends it.
    #[test]
    fn a_lock_key_is_the_peers_state_where_the_host_reads_its_own() {
        let locks = |caps| Some(Locks { num: false, caps });
        let mut out = Recorder {
            locks: locks(false),
            ..Recorder::default()
        };
        let mut inject = injector();
        // Down as the lock goes on, up as it goes off.
        inject.on_control(&control(op::KEYBOARD, 57, MOD_CAPS, 1), &mut out);
        assert_eq!(out.keys_at(1), vec![KEY_CAPSLOCK]);
        assert_eq!(out.keys_at(0), vec![KEY_CAPSLOCK], "not a tap");
        out.locks = locks(true);
        out.batches.clear();
        inject.on_control(&control(op::KEYBOARD, 57, 0, 0), &mut out);
        assert_eq!(out.keys_at(1), vec![KEY_CAPSLOCK], "the lock going off");
        assert_eq!(out.keys_at(0), vec![KEY_CAPSLOCK]);

        // Pressed and released, the mask the new state: the press brings the
        // host to it and the release finds it there.
        out.locks = locks(false);
        out.batches.clear();
        inject.on_control(&control(op::KEYBOARD, 57, MOD_CAPS, 1), &mut out);
        out.locks = locks(true);
        inject.on_control(&control(op::KEYBOARD, 57, MOD_CAPS, 0), &mut out);
        assert_eq!(out.keys_at(1), vec![KEY_CAPSLOCK], "toggled twice");

        // Nothing of the key is held, so a guest leaving releases nothing.
        out.batches.clear();
        inject.release_all(&mut out);
        assert!(out.keys_at(0).is_empty());
    }

    /// **A change the host already made is not made again.** The peer's
    /// state changed since its last message, and the host's is already
    /// there: toggling would undo it.
    #[test]
    fn a_change_the_host_already_has_is_not_tapped() {
        let mut out = Recorder {
            locks: Some(Locks {
                num: false,
                caps: false,
            }),
            ..Recorder::default()
        };
        let mut inject = injector();
        inject.on_control(&control(op::KEYBOARD, 4, 0, 1), &mut out);
        out.locks = Some(Locks {
            num: false,
            caps: true,
        });
        out.batches.clear();
        inject.on_control(&control(op::KEYBOARD, 5, MOD_CAPS, 1), &mut out);
        assert_eq!(out.keys_at(1), vec![48], "the lock was toggled again");

        // And an unchanged peer state never taps, whatever the host says.
        out.locks = Some(Locks {
            num: true,
            caps: false,
        });
        out.batches.clear();
        inject.on_control(&control(op::KEYBOARD, 6, MOD_CAPS, 1), &mut out);
        assert_eq!(out.keys_at(1), vec![46]);
    }

    /// **A modified keystroke leaves nothing behind**, which is the shape a
    /// key that would not release was reported in: a modifier held, a letter
    /// tapped under it, then both let go. Nothing about a held Control reaches
    /// the expansion -- the mask is read for the locks and for Command alone --
    /// so the four messages are four independent keys, and this says so.
    #[test]
    fn a_letter_typed_under_a_modifier_leaves_nothing_held() {
        let (mut inject, mut out) = (injector(), Recorder::default());
        // Left control down, t down, t up, left control up, with the mask
        // reporting control the way a peer does while it is held.
        const CTRL: u32 = 0x40;
        inject.on_control(&control(op::KEYBOARD, 224, CTRL, 1), &mut out);
        inject.on_control(&control(op::KEYBOARD, 23, CTRL, 1), &mut out);
        inject.on_control(&control(op::KEYBOARD, 23, CTRL, 0), &mut out);
        inject.on_control(&control(op::KEYBOARD, 224, 0, 0), &mut out);
        assert_eq!(out.keys_at(1), vec![29, 20]);
        assert_eq!(out.keys_at(0), vec![20, 29]);

        out.batches.clear();
        inject.release_all(&mut out);
        assert!(
            out.batches.is_empty(),
            "something was still tracked as held"
        );
    }

    /// **A mask that reports the modifier on some messages and not others
    /// releases no modifier.** A peer's mask is its platform reporting itself
    /// at that keystroke, and one sighting of a left control press carrying
    /// only the meta bit is on the record -- a macOS peer's Command, its code
    /// swapped for Control -- so a modifier missing from the mask must not
    /// release anything.
    #[test]
    fn a_modifier_absent_from_the_mask_is_not_released() {
        let (mut inject, mut out) = (injector(), Recorder::default());
        inject.on_control(&control(op::KEYBOARD, 224, 0x400, 1), &mut out);
        out.batches.clear();
        inject.on_control(&control(op::KEYBOARD, 23, 0, 1), &mut out);
        assert_eq!(out.keys_at(0), Vec::<u16>::new());
        assert_eq!(out.keys_at(1), vec![20]);
    }

    /// **A key pressed under Command goes down and up at once**, its release
    /// never awaited: a macOS peer sends none for a key let go while Command
    /// is held, and a key left held is typed again by the host's own repeat.
    /// Each press or repeat is one keystroke; the release that never came is
    /// not missed, and one that does come finds nothing held. Command is the
    /// mask's to say, its code here swapped for Control as such a peer sends it.
    #[test]
    fn a_key_pressed_under_command_is_a_keystroke_and_nothing_stays_held() {
        let (mut inject, mut out) = (injector(), Recorder::default());
        const LGUI: u32 = 0x400;
        inject.on_control(&control(op::KEYBOARD, 224, LGUI, 1), &mut out);
        out.batches.clear();
        // e, then e again as the peer's repeat; no release of it.
        inject.on_control(&control(op::KEYBOARD, 8, LGUI, 1), &mut out);
        assert_eq!(out.batches.len(), 1, "a keystroke split across batches");
        assert_eq!(out.keys_at(1), vec![18]);
        assert_eq!(out.keys_at(0), vec![18]);
        inject.on_control(&control(op::KEYBOARD, 8, LGUI, 1), &mut out);
        assert_eq!(out.keys_at(1), vec![18, 18]);
        assert_eq!(out.keys_at(0), vec![18, 18]);
        // Command up, and a release of e that did arrive.
        inject.on_control(&control(op::KEYBOARD, 224, 0, 0), &mut out);
        inject.on_control(&control(op::KEYBOARD, 8, 0, 0), &mut out);
        assert_eq!(out.keys_at(0), vec![18, 18, 29]);

        out.batches.clear();
        inject.release_all(&mut out);
        assert!(out.batches.is_empty(), "something was still held");

        // The right Command says the same.
        inject.on_control(&control(op::KEYBOARD, 25, 0x800, 1), &mut out);
        assert_eq!(out.keys_at(0), vec![47], "held under the right Command");
    }

    /// **A key down as Command arrives comes up then**, before Command
    /// itself: its release, made while Command is held, would never come. The
    /// modifiers stay held, Shift pressed under Command among them, and the
    /// letter under both is a keystroke.
    #[test]
    fn a_key_held_as_command_arrives_comes_up_and_the_modifiers_stay() {
        let (mut inject, mut out) = (injector(), Recorder::default());
        const LGUI: u32 = 0x400;
        let command = usage::key_code(227).unwrap();
        inject.on_control(&control(op::KEYBOARD, 8, 0, 1), &mut out);
        out.batches.clear();
        inject.on_control(&control(op::KEYBOARD, 227, LGUI, 1), &mut out);
        let order: Vec<(u16, i32)> = out
            .all()
            .into_iter()
            .filter(|e| e.kind == EV_KEY)
            .map(|e| (e.code, e.value))
            .collect();
        assert_eq!(order, vec![(18, 0), (command, 1)], "e not let go first");

        out.batches.clear();
        inject.on_control(&control(op::KEYBOARD, 225, LGUI | 0x1, 1), &mut out);
        inject.on_control(&control(op::KEYBOARD, 29, LGUI | 0x1, 1), &mut out);
        assert_eq!(out.keys_at(1), vec![42, 44]);
        assert_eq!(out.keys_at(0), vec![44], "a modifier under Command let go");

        out.batches.clear();
        inject.release_all(&mut out);
        let mut released = out.keys_at(0);
        released.sort_unstable();
        let mut held = vec![42, command];
        held.sort_unstable();
        assert_eq!(released, held);
    }

    /// The chord is typed as a person types it and leaves nothing of itself
    /// held, including the parts a guest was already holding.
    #[test]
    fn the_attention_chord_is_typed_and_fully_released() {
        let (mut inject, mut out) = (injector(), Recorder::default());
        // The guest is already holding one of the three when it asks.
        inject.on_control(&key(224, true), &mut out);
        out.batches.clear();

        assert!(inject.secure_attention(&mut out));
        let chord: Vec<u16> = ATTENTION
            .iter()
            .map(|&u| usage::key_code(u).unwrap())
            .collect();
        assert_eq!(out.keys_at(1), chord);
        assert_eq!(
            out.keys_at(0),
            chord.iter().rev().copied().collect::<Vec<_>>()
        );
        assert_eq!(out.devices(), vec![Device::Keyboard]);

        out.batches.clear();
        inject.release_all(&mut out);
        assert!(
            out.batches.is_empty(),
            "the chord left one of its own keys tracked as held"
        );
    }

    /// The chord is keystrokes, so a guest that may not type may not ask for
    /// it either.
    #[test]
    fn the_attention_chord_needs_the_keyboard() {
        let (mut inject, mut out) = (injector(), Recorder::default());
        inject.set_permissions(
            Permissions {
                keyboard: false,
                ..Permissions::default()
            },
            &mut out,
        );
        out.batches.clear();
        assert!(!inject.secure_attention(&mut out));
        assert!(out.batches.is_empty());
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
    fn absolute_motion_reaches_both_ends_of_the_picture() {
        let (mut inject, mut out) = (injector(), Recorder::default());
        inject.on_control(&control(op::MOUSE_MOTION, 0, 0, 0), &mut out);
        inject.on_control(&control(op::MOUSE_MOTION, 0, 1920, 1080), &mut out);
        // Past the edge, which a peer does send.
        inject.on_control(&control(op::MOUSE_MOTION, 0, 4000, 4000), &mut out);

        assert_eq!(
            landed(&out, 1920, 1080),
            vec![(0, 0), (1919, 1079), (1919, 1079)]
        );
        assert!(axes(&out).iter().all(|v| (0..=ABS_RANGE).contains(v)));
        assert!(out.devices().iter().all(|d| *d == Device::PointerAbsolute));
    }

    /// Where the input stack puts each absolute position emitted: it takes an
    /// axis value back to a pixel as `value * desktop / 65536`, truncated
    /// (measured on Windows; the Linux input library computes the same).
    fn landed(out: &Recorder, desktop_width: u32, desktop_height: u32) -> Vec<(u32, u32)> {
        let pixel = |value: i32, desktop: u32| {
            u32::try_from(i64::from(value) * i64::from(desktop) / 65536).unwrap()
        };
        axes(out)
            .chunks(2)
            .map(|pair| {
                (
                    pixel(pair[0], desktop_width),
                    pixel(pair[1], desktop_height),
                )
            })
            .collect()
    }

    /// The picture placed in a desktop wider than it is.
    ///
    /// A second output to the right, the captured one at the corner: exactly
    /// the arrangement the failure was found in.
    fn beside_another() -> Injector {
        Injector::new(Extents::placed(
            2560,
            1440,
            Place {
                x: 0,
                y: 0,
                width: 2560,
                height: 1440,
                desktop_width: 4480,
                desktop_height: 1440,
            },
        ))
    }

    /// Every absolute value the injector emitted, in order.
    fn axes(out: &Recorder) -> Vec<i32> {
        out.all()
            .into_iter()
            .filter(|e| e.kind == EV_ABS)
            .map(|e| e.value)
            .collect()
    }

    /// **The axis is spread over the whole desktop, not over the picture.** A
    /// coordinate normalised against the picture alone puts the picture's far
    /// edge at the desktop's far edge, so a guest reaches its own display's
    /// edge partway across and the rest of the picture is unreachable. Here
    /// that is 2560 of 4480, and the edge would otherwise arrive at 57 percent.
    ///
    /// **The failure is per axis**, which this checks in one run: both outputs
    /// are the same height, so the vertical scale is one and the vertical is
    /// already right. A fix applied to the pair rather than to each axis moves
    /// the vertical too.
    #[test]
    fn a_picture_beside_another_output_reaches_its_own_edge_and_no_further() {
        let (mut inject, mut out) = (beside_another(), Recorder::default());
        inject.on_control(&control(op::MOUSE_MOTION, 0, 2560, 1440), &mut out);

        assert_eq!(landed(&out, 4480, 1440), vec![(2559, 1439)]);
    }

    /// **A coordinate past the picture stays on the captured output.** Without
    /// the clamp it lands on the next one, where the pointer plane this host
    /// reads goes empty -- which is the same thing an application hiding the
    /// pointer produces, so the peer is told to switch to relative motion and
    /// the pointer has to be walked back by hand.
    #[test]
    fn a_coordinate_past_the_picture_stops_at_the_captured_output() {
        let (mut inject, mut out) = (beside_another(), Recorder::default());
        inject.on_control(&control(op::MOUSE_MOTION, 0, 4000, 4000), &mut out);

        assert_eq!(landed(&out, 4480, 1440), vec![(2559, 1439)]);
    }

    /// **An output that is not at the desktop's corner starts partway along
    /// the axis.** Zero on the wire is the picture's own corner, never the
    /// desktop's, and treating the two as the same puts every position a whole
    /// output to one side.
    #[test]
    fn an_output_away_from_the_corner_is_offset_along_the_axis() {
        let mut inject = Injector::new(Extents::placed(
            2560,
            1440,
            Place {
                x: 1920,
                y: 0,
                width: 2560,
                height: 1440,
                desktop_width: 4480,
                desktop_height: 1440,
            },
        ));
        let mut out = Recorder::default();
        inject.on_control(&control(op::MOUSE_MOTION, 0, 0, 0), &mut out);
        inject.on_control(&control(op::MOUSE_MOTION, 0, 2560, 0), &mut out);

        // The output's own first column, never the neighbour's last.
        assert_eq!(landed(&out, 4480, 1440), vec![(1920, 0), (4479, 0)]);
    }

    /// **A scaled output's rectangle is smaller than its picture**, and the
    /// two are in different units. Converting the picture's coordinate into
    /// the rectangle's units is what keeps both ends of the ratio in one
    /// space; skipping it walks the pointer off the output by the scale
    /// factor.
    #[test]
    fn a_scaled_output_maps_through_its_own_rectangle() {
        let mut inject = Injector::new(Extents::placed(
            2560,
            1440,
            Place {
                x: 0,
                y: 0,
                width: 1280,
                height: 720,
                desktop_width: 2560,
                desktop_height: 720,
            },
        ));
        let mut out = Recorder::default();
        inject.on_control(&control(op::MOUSE_MOTION, 0, 2560, 1440), &mut out);

        assert_eq!(landed(&out, 2560, 720), vec![(1279, 719)]);
    }

    /// **Every pixel of one output lands on itself**: the rectangle is the
    /// desktop and the two conversions cancel, and the axis is aimed at each
    /// pixel's centre, so the input stack's truncation lands on it rather
    /// than on the one before, which is where every position went until it
    /// was measured.
    #[test]
    fn every_pixel_of_one_output_lands_on_itself() {
        let (mut inject, mut out) = (injector(), Recorder::default());
        for x in 0..1920 {
            inject.on_control(&control(op::MOUSE_MOTION, 0, x, x % 1080), &mut out);
        }
        let expected: Vec<(u32, u32)> = (0..1920).map(|x| (x, x % 1080)).collect();
        assert_eq!(landed(&out, 1920, 1080), expected);
    }

    /// **The side buttons are the relative pointer's, whichever pointer
    /// moved last.** The absolute pointer declares the three primary buttons
    /// and no more, so a side button pressed after an absolute move goes to
    /// the relative device, its release follows it there, and a primary
    /// button beside it still lands on the absolute one.
    #[test]
    fn a_side_button_lands_on_the_relative_pointer() {
        let mut inject = Injector::new(Extents::alone(1920, 1080));
        let mut out = Recorder::default();
        inject.on_control(&control(op::MOUSE_MOTION, 0, 100, 100), &mut out);
        let mut down = Recorder::default();
        inject.on_control(&control(op::MOUSE_BUTTON, 4, 1, 0), &mut down);
        inject.on_control(&control(op::MOUSE_BUTTON, 5, 1, 0), &mut down);
        inject.on_control(&control(op::MOUSE_BUTTON, 1, 1, 0), &mut down);
        assert_eq!(
            down.devices(),
            vec![Device::Pointer, Device::Pointer, Device::PointerAbsolute]
        );
        assert_eq!(down.keys_at(1), vec![BTN_SIDE, BTN_EXTRA, BTN_LEFT]);
        let mut up = Recorder::default();
        inject.on_control(&control(op::MOUSE_BUTTON, 4, 0, 0), &mut up);
        assert_eq!(up.devices(), vec![Device::Pointer]);
        assert_eq!(up.keys_at(0), vec![BTN_SIDE]);
        // The bulk release too: the side button still held goes to the
        // relative device, the primary one to the absolute.
        let mut gone = Recorder::default();
        inject.release_all(&mut gone);
        assert_eq!(
            gone.devices(),
            vec![Device::PointerAbsolute, Device::Pointer]
        );
        assert_eq!(gone.keys_at(0), vec![BTN_LEFT, BTN_EXTRA]);
    }

    /// **A release has to reach the device that took the press.** Which
    /// pointer device an event goes to follows whichever kind of motion
    /// arrived last, and a peer changes kind mid-gesture: a drag that begins
    /// with the pointer placed and continues with deltas, or a client that
    /// flips to relative mode with a button down. Routed by what is current,
    /// the release goes to a device that never saw the press and the button
    /// stays held on a machine nobody is driving.
    #[test]
    fn a_button_is_released_on_the_device_that_took_it() {
        let mut inject = Injector::new(Extents::alone(1920, 1080));
        let mut out = Recorder::default();

        // Placed, then pressed: both land on the absolute device. Then the
        // peer switches to deltas without letting go, which moves every later
        // event to the relative one.
        inject.on_control(&control(op::MOUSE_MOTION, 0, 100, 100), &mut out);
        inject.on_control(&control(op::MOUSE_BUTTON, 1, 1, 0), &mut out);
        assert_eq!(out.devices(), vec![Device::PointerAbsolute; 2]);
        inject.on_control(&control(op::MOUSE_MOTION, 1, 5, 5), &mut out);

        // The release must still go where the press did.
        let mut up = Recorder::default();
        inject.on_control(&control(op::MOUSE_BUTTON, 1, 0, 0), &mut up);
        assert_eq!(up.keys_at(0), vec![BTN_LEFT], "nothing was released");
        assert_eq!(
            up.devices(),
            vec![Device::PointerAbsolute],
            "the release went to a device that never saw the press"
        );

        // And the bulk release, which runs when a guest leaves, does the same.
        // **Pressed on the relative device here**, deliberately: a release
        // that simply went to whichever device the sweep visited first would
        // be right by accident if the press had been on the absolute one.
        let mut inject = Injector::new(Extents::alone(1920, 1080));
        let mut out = Recorder::default();
        inject.on_control(&control(op::MOUSE_MOTION, 1, 5, 5), &mut out);
        inject.on_control(&control(op::MOUSE_BUTTON, 1, 1, 0), &mut out);
        inject.on_control(&control(op::MOUSE_MOTION, 0, 100, 100), &mut out);
        let mut gone = Recorder::default();
        inject.release_all(&mut gone);
        assert_eq!(gone.keys_at(0), vec![BTN_LEFT], "nothing was released");
        assert_eq!(
            gone.devices(),
            vec![Device::Pointer],
            "the sweep released it on a device that never held it"
        );
    }

    /// A rotated output is a swap of the extents, not a rotation of the
    /// coordinate: the peer already sends them the way the desktop is.
    #[test]
    fn a_rotated_output_is_a_swap_of_the_extents() {
        let mut inject = Injector::new(Extents::alone(1080, 1920));
        let mut out = Recorder::default();
        inject.on_control(&control(op::MOUSE_MOTION, 0, 1080, 1920), &mut out);
        assert_eq!(landed(&out, 1080, 1920), vec![(1079, 1919)]);
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

    /// A press belongs to whichever pointer produced the position it is at.
    ///
    /// **The release does not, and this test used to say it did.** A button
    /// goes down on one device and the kernel holds it there; a release sent
    /// to another leaves it down on the first, on a machine nobody is driving.
    /// See `a_button_is_released_on_the_device_that_took_it`.
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
                Device::PointerAbsolute,
            ]
        );
    }

    /// The second stream's motion packs one coordinate signed and the other
    /// not, which is the half nobody guesses.
    #[test]
    fn the_second_streams_motion_unpacks_a_signed_vertical() {
        let (mut inject, mut out) = (injector(), Recorder::default());
        inject.set_relative(true);
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

    /// A held pointer is aiming, so the first sample only says where it is.
    #[test]
    fn a_held_pointer_primes_before_it_moves() {
        let (mut inject, mut out) = (injector(), Recorder::default());
        inject.set_relative(true);
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
    fn a_pointer_let_go_goes_back_to_absolute() {
        let (mut inject, mut out) = (injector(), Recorder::default());
        inject.set_relative(true);
        inject.on_control(&control(op::MOUSE_MOTION, 0, 500, 500), &mut out);
        inject.set_relative(false);
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
        let mut keys: Vec<u16> = Vec::new();
        for usage in 0..=u16::MAX {
            if let Some(code) = usage::key_code(usage) {
                inject.on_control(&key(u32::from(usage), true), &mut out);
                held += 1;
                if !keys.contains(&code) {
                    keys.push(code);
                }
            }
        }
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
        // **Some keys have more than one name.** Holding every one of the
        // peer's codes holds fewer keys than codes -- on Linux 162 for 176 --
        // and a tracker keyed on the peer's code instead of the table's would
        // report every code and release some keys twice.
        assert!(keys.len() < held, "no key has two names any more");
        assert_eq!(out.keys_at(0).len(), keys.len());
        #[cfg(not(windows))]
        assert_eq!((held, keys.len()), (176, 162));
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

    /// **The per-axis message's vertical axes point down**, as the axis
    /// itself does, so they pass through where the whole state's are
    /// inverted: a stick pushed away from the player is negative in one and
    /// positive in the other, and reaches the axis negative from both.
    #[test]
    fn the_per_axis_vertical_points_down_and_passes_through() {
        let (mut inject, mut out) = (injector(), Recorder::default());
        inject.on_control(&control(op::GAMEPAD_AXIS, 1, as_u32(-20_000), 1), &mut out);
        inject.on_control(&control(op::GAMEPAD_AXIS, 3, 20_000, 1), &mut out);
        inject.on_control(&control(op::GAMEPAD_AXIS, 1, i16::MIN as u32, 1), &mut out);
        assert_eq!(
            abs(&out),
            vec![
                (gamepad::axis::Y, -20_000),
                (gamepad::axis::RY, 20_000),
                (gamepad::axis::Y, -32_768),
            ]
        );

        let mut whole = Recorder::default();
        inject.on_control(
            &state(1, &pad_state_body(0, 0, 20_000, 0, 0, 0, 0)),
            &mut whole,
        );
        assert!(abs(&whole).contains(&(gamepad::axis::Y, -20_000)));
    }

    /// **A whole state folds back into the report it came from**, through the
    /// events the expander made of it: a device handed whole reports is
    /// handed what the peer sent, with away from the player still positive,
    /// and the per-axis message lands the same way up.
    #[test]
    fn a_whole_state_folds_back_into_the_report_it_came_from() {
        let (mut inject, mut out) = (injector(), Recorder::default());
        // Guide is among the buttons, and the owner's alone.
        inject.set_owner(true, &mut out);
        let sent = gamepad::Report {
            buttons: gamepad::bit::A
                | gamepad::bit::Y
                | gamepad::bit::LEFT_SHOULDER
                | gamepad::bit::START
                | gamepad::bit::GUIDE
                | gamepad::bit::DPAD_UP
                | gamepad::bit::DPAD_RIGHT,
            lx: -12_345,
            ly: 23_456,
            rx: 32_767,
            ry: -32_767,
            left_trigger: 7,
            right_trigger: 255,
        };
        let body = pad_state_body(
            sent.buttons,
            sent.lx,
            sent.ly,
            sent.rx,
            sent.ry,
            sent.left_trigger,
            sent.right_trigger,
        );
        inject.on_control(&state(2, &body), &mut out);
        let mut folded = gamepad::Report::default();
        for (device, events) in &out.batches {
            assert_eq!(*device, Device::Gamepad(2));
            folded.apply(events);
        }
        assert_eq!(folded, sent);

        out.batches.clear();
        inject.on_control(&state(2, &pad_state_body(0, 0, 0, 0, 0, 0, 0)), &mut out);
        for (_, events) in &out.batches {
            folded.apply(events);
        }
        assert_eq!(folded, gamepad::Report::default());

        // Pushed away from the player, which this message carries negative.
        out.batches.clear();
        inject.on_control(&control(op::GAMEPAD_AXIS, 1, as_u32(-20_000), 2), &mut out);
        for (_, events) in &out.batches {
            folded.apply(events);
        }
        assert_eq!(folded.ly, 20_000);

        // The one value that does not come back: the far end, turned over.
        out.batches.clear();
        inject.on_control(
            &state(2, &pad_state_body(0, 0, i16::MIN, 0, 0, 0, 0)),
            &mut out,
        );
        for (_, events) in &out.batches {
            folded.apply(events);
        }
        assert_eq!(folded.ly, -32_767);
    }

    /// **A sink with nowhere to put a pad's own report never sees one**, and
    /// the pad is made from the whole states a peer sends beside its reports.
    /// Taken, the report would fix the pad as one that drops those states.
    #[test]
    fn a_report_a_sink_cannot_take_leaves_the_pad_to_its_states() {
        let mut out = Recorder {
            refuses_reports: true,
            ..Recorder::default()
        };
        let mut inject = injector();
        let ds5: &[u8; 64] = include_bytes!("../../core/tests/data/pad/ds5/input-idle.bin");
        inject.on_control(
            &Control {
                a0: 64,
                a1: 3,
                a2: 0x0CE6,
                opcode: op::PAD_REPORT,
                body: ds5,
            },
            &mut out,
        );
        assert!(out.reports.is_empty());
        assert!(inject.pads.iter().all(Option::is_none), "a pad was taken");

        let body = pad_state_body(gamepad::bit::A, 0, 0, 0, 0, 0, 0);
        inject.on_control(&state(3, &body), &mut out);
        assert_eq!(out.keys_at(1), vec![gamepad::key::SOUTH]);
        assert!(out.devices().iter().all(|d| *d == Device::Gamepad(3)));
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

    /// **Withdrawing the pad permission lets go of every pad**, as withdrawing
    /// the keyboard does of its keys: a sixteen-button pad is centred and
    /// stays plugged in, and a pad sent as its reports is unplugged, since its
    /// reports are refused from then on and nothing else brings it to rest.
    #[test]
    fn withdrawing_the_pad_permission_lets_go_of_every_pad() {
        let (mut inject, mut out) = (injector(), Recorder::default());
        let ds5: &[u8; 64] = include_bytes!("../../core/tests/data/pad/ds5/input-idle.bin");
        inject.on_control(
            &state(1, &pad_state_body(gamepad::bit::A, 900, 0, 0, 0, 40, 0)),
            &mut out,
        );
        inject.on_control(
            &Control {
                a0: 64,
                a1: 2,
                a2: 0x0CE6,
                opcode: op::PAD_REPORT,
                body: ds5,
            },
            &mut out,
        );
        assert_eq!(out.reports, vec![(2, true)]);
        out.batches.clear();

        inject.set_permissions(
            Permissions {
                gamepad: false,
                ..Permissions::default()
            },
            &mut out,
        );
        assert_eq!(out.keys_at(0), vec![gamepad::key::SOUTH]);
        let centred = abs(&out);
        for axis in [gamepad::axis::X, gamepad::axis::Z] {
            assert!(
                centred.contains(&(axis, 0)),
                "{axis:#x} was left where it was"
            );
        }
        assert!(
            out.devices().iter().all(|d| *d == Device::Gamepad(1)),
            "a pad other than the sixteen-button one was written"
        );
        assert_eq!(
            out.unplugged,
            vec![2],
            "the report pad was left at its last report"
        );
    }

    /// **An unplug lets go and presses nothing**, so it is taken without the
    /// pad permission: a guest whose pads were taken from it can still take
    /// them away.
    #[test]
    fn an_unplug_needs_no_pad_permission() {
        let (mut inject, mut out) = (injector(), Recorder::default());
        inject.on_control(&state(1, &pad_state_body(0, 0, 0, 0, 0, 0, 0)), &mut out);
        inject.set_permissions(
            Permissions {
                gamepad: false,
                ..Permissions::default()
            },
            &mut out,
        );
        inject.on_control(&control(op::GAMEPAD_UNPLUG, 0, 0, 1), &mut out);
        assert_eq!(out.unplugged, vec![1]);
    }

    /// **Guide is the owner's alone**: on the Xbox pad it raises the host's
    /// own overlay. A guest is not the owner until it is said to be; its press
    /// of Guide alone is dropped and its whole states are taken without it,
    /// the rest of them intact. The owner's reaches the pad both ways, and the
    /// sink is told.
    #[test]
    fn guide_is_pressed_for_the_owner_alone() {
        let (mut guest, mut out) = (injector(), Recorder::default());
        guest.on_control(&control(op::GAMEPAD_BUTTON, 5, 1, 1), &mut out);
        assert!(out.batches.is_empty(), "a guest's Guide reached a pad");
        let both = gamepad::bit::GUIDE | gamepad::bit::A;
        guest.on_control(
            &state(2, &pad_state_body(both, 900, 0, 0, 0, 0, 0)),
            &mut out,
        );
        assert_eq!(
            out.keys_at(1),
            vec![gamepad::key::SOUTH],
            "the state kept Guide, or lost A"
        );
        assert!(
            abs(&out)
                .iter()
                .any(|&(code, value)| code == gamepad::axis::X && value != 0),
            "the state's stick went with Guide"
        );

        let (mut owner, mut out) = (injector(), Recorder::default());
        owner.set_owner(true, &mut out);
        assert_eq!(out.guide, Some(true), "the sink was not told");
        owner.on_control(&control(op::GAMEPAD_BUTTON, 5, 1, 1), &mut out);
        owner.on_control(
            &state(2, &pad_state_body(gamepad::bit::GUIDE, 0, 0, 0, 0, 0, 0)),
            &mut out,
        );
        assert_eq!(out.keys_at(1), vec![gamepad::key::MODE, gamepad::key::MODE]);
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

    /// **A pad sent as its reports keeps them when the sink stops taking new
    /// ones**: the sink's answer fixes a new pad's family alone, so a report
    /// pad plugged before goes on, and one plugged after is made from its
    /// states.
    #[test]
    fn a_report_pad_keeps_its_reports_when_the_sink_stops_taking_new_ones() {
        let (mut inject, mut out) = (injector(), Recorder::default());
        let ds5: &[u8; 64] = include_bytes!("../../core/tests/data/pad/ds5/input-idle.bin");
        let report = |pad: u32| Control {
            a0: 64,
            a1: pad,
            a2: 0x0CE6,
            opcode: op::PAD_REPORT,
            body: ds5,
        };
        inject.on_control(&report(3), &mut out);
        assert_eq!(out.reports, vec![(3, true)]);

        out.refuses_reports = true;
        inject.on_control(&report(3), &mut out);
        assert_eq!(
            out.reports,
            vec![(3, true), (3, true)],
            "the pad already sent as its reports went quiet"
        );
        inject.on_control(&report(4), &mut out);
        assert_eq!(out.reports.len(), 2, "a new pad's report was taken");
        let state = pad_state_body(gamepad::bit::A, 0, 0, 0, 0, 0, 0);
        inject.on_control(
            &Control {
                a0: 4,
                a1: 0,
                a2: 0,
                opcode: op::GAMEPAD_STATE,
                body: &state,
            },
            &mut out,
        );
        assert_eq!(
            out.distinct_devices(),
            1,
            "the new pad not made from its states"
        );
    }

    /// **The first message that can create a pad fixes its family.** A report
    /// makes a report pad, whose states and buttons are then dropped; a state
    /// makes a sixteen-button pad, whose reports are then dropped; the
    /// established peer's ten-byte block creates nothing; an unplug frees
    /// the identifier for either family. A release-all leaves a report pad
    /// alone.
    #[test]
    fn a_pads_family_is_fixed_by_the_message_that_created_it() {
        let (mut inject, mut out) = (injector(), Recorder::default());
        let ds5: &[u8; 64] = include_bytes!("../../core/tests/data/pad/ds5/input-idle.bin");
        let calibration = include_bytes!("../../core/tests/data/pad/ds5/feature-calibration.bin");
        let feature = Control {
            a0: 41,
            a1: 3,
            a2: 0x0CE6 | lowlat_core::pad::FEATURE_BIT,
            opcode: op::PAD_REPORT,
            body: calibration,
        };
        let report = Control {
            a0: 64,
            a1: 3,
            a2: 0x0CE6,
            opcode: op::PAD_REPORT,
            body: ds5,
        };
        // The block alone: nothing.
        let block = Control {
            a0: 10,
            a1: 9,
            a2: 0,
            opcode: op::PAD_REPORT,
            body: &ds5[33..43],
        };
        inject.on_control(&block, &mut out);
        assert!(out.reports.is_empty());
        assert!(inject.pads.iter().all(Option::is_none));

        // A report pad: the feature then the input reach the sink, the
        // state beside them does not make a device.
        inject.on_control(&feature, &mut out);
        inject.on_control(&report, &mut out);
        let state = pad_state_body(gamepad::bit::A, 0, 0, 0, 0, 0, 0);
        inject.on_control(
            &Control {
                a0: 3,
                a1: 0,
                a2: 0,
                opcode: op::GAMEPAD_STATE,
                body: &state,
            },
            &mut out,
        );
        inject.on_control(&control(op::GAMEPAD_BUTTON, 0, 1, 3), &mut out);
        inject.on_control(&control(op::GAMEPAD_AXIS, 0, 1000, 3), &mut out);
        assert_eq!(out.reports, vec![(3, false), (3, true)]);
        assert!(
            out.batches.is_empty(),
            "a report pad produced device events"
        );
        inject.release_all(&mut out);
        assert!(out.batches.is_empty());

        // A sixteen-button pad: its report is dropped.
        inject.on_control(
            &Control {
                a0: 4,
                a1: 0,
                a2: 0,
                opcode: op::GAMEPAD_STATE,
                body: &state,
            },
            &mut out,
        );
        assert_eq!(out.distinct_devices(), 1);
        inject.on_control(&Control { a1: 4, ..report }, &mut out);
        assert_eq!(out.reports.len(), 2);

        // Unplugged, the identifier may be the other family.
        inject.on_control(&control(op::GAMEPAD_UNPLUG, 0, 0, 3), &mut out);
        assert_eq!(out.unplugged, vec![3]);
        inject.on_control(
            &Control {
                a0: 3,
                a1: 0,
                a2: 0,
                opcode: op::GAMEPAD_STATE,
                body: &state,
            },
            &mut out,
        );
        assert_eq!(out.distinct_devices(), 2);
        inject.on_control(&report, &mut out);
        assert_eq!(out.reports.len(), 2);

        // Without the pad permission nothing is handed on.
        let mut denied = injector();
        denied.set_permissions(
            Permissions {
                gamepad: false,
                ..Permissions::default()
            },
            &mut out,
        );
        denied.on_control(&Control { a1: 5, ..report }, &mut out);
        assert_eq!(out.reports.len(), 2);
    }

    /// A guest held to one application, the application in front and under
    /// the pointer, its window the whole output.
    fn held_guest() -> (Injector, Recorder) {
        let out = Recorder {
            scope: Some((true, true)),
            bounds: Some(Bounds {
                left: 0,
                top: 0,
                right: 1920,
                bottom: 1080,
            }),
            ..Recorder::default()
        };
        (injector(), out)
    }

    fn code(usage: u16) -> u16 {
        usage::key_code(usage).expect("the table names the key")
    }

    /// **A guest held to one application types into it alone**
    /// (docs/05-host.md section 7.4): its keys go in while the application is
    /// in front and are dropped while anything else is, and a key it held as
    /// the application went comes up then.
    #[test]
    fn a_held_guest_types_only_while_the_application_is_in_front() {
        let (mut inject, mut out) = held_guest();
        inject.on_control(&key(4, true), &mut out);
        assert_eq!(out.keys_at(1), vec![code(4)]);

        out.scope = Some((false, false));
        out.batches.clear();
        inject.on_control(&key(5, true), &mut out);
        assert!(
            out.keys_at(1).is_empty(),
            "a key went in behind the application"
        );
        assert_eq!(out.keys_at(0), vec![code(4)], "the held key was left down");

        // Its release finds nothing held, and goes nowhere.
        out.batches.clear();
        inject.on_control(&key(4, false), &mut out);
        assert!(out.batches.is_empty());

        out.scope = Some((true, true));
        inject.on_control(&key(5, true), &mut out);
        assert_eq!(out.keys_at(1), vec![code(5)]);
    }

    /// **The application leaving the front lets go of what the guest holds**
    /// with nothing arriving from the guest, as a revoked permission does:
    /// keys and buttons up, a sixteen-button pad centred, a pad sent as its
    /// reports put at rest by the sink and left plugged in.
    #[test]
    fn the_application_leaving_the_front_releases_what_a_held_guest_holds() {
        let (mut inject, mut out) = held_guest();
        let ds5: &[u8; 64] = include_bytes!("../../core/tests/data/pad/ds5/input-idle.bin");
        inject.on_control(&key(26, true), &mut out);
        inject.on_control(&control(op::MOUSE_BUTTON, 1, 1, 0), &mut out);
        inject.on_control(
            &state(1, &pad_state_body(gamepad::bit::A, 900, 0, 0, 0, 40, 0)),
            &mut out,
        );
        inject.on_control(
            &Control {
                a0: 64,
                a1: 2,
                a2: 0x0CE6,
                opcode: op::PAD_REPORT,
                body: ds5,
            },
            &mut out,
        );
        out.batches.clear();
        inject.follow_gate(&mut out);
        assert!(
            out.batches.is_empty(),
            "something came up with the application in front"
        );

        out.scope = Some((false, true));
        inject.follow_gate(&mut out);
        let mut up = out.keys_at(0);
        up.sort_unstable();
        let mut expected = vec![code(26), BTN_LEFT, gamepad::key::SOUTH];
        expected.sort_unstable();
        assert_eq!(up, expected);
        assert!(
            abs(&out).contains(&(gamepad::axis::X, 0)),
            "the stick was left where it was"
        );
        assert_eq!(
            out.rested,
            vec![2],
            "the report pad was left at its last report"
        );
        assert!(out.unplugged.is_empty(), "a pad was taken away");

        // Asked again, there is nothing more to let go of.
        out.batches.clear();
        inject.follow_gate(&mut out);
        assert!(out.batches.is_empty());
        assert_eq!(out.rested, vec![2]);
    }

    /// **A click and a scroll need the application under the pointer too**:
    /// the pointer moves while the application is in front, a press lands only
    /// on its windows, and a button dragged off them still comes up.
    #[test]
    fn a_held_guest_clicks_and_scrolls_only_over_the_application() {
        let (mut inject, mut out) = held_guest();
        out.scope = Some((true, false));
        inject.on_control(&control(op::MOUSE_BUTTON, 1, 1, 0), &mut out);
        inject.on_control(&control(op::MOUSE_WHEEL, 0, 120, 0), &mut out);
        assert!(
            out.batches.is_empty(),
            "a click or a scroll went in past the application"
        );
        inject.on_control(&control(op::MOUSE_MOTION, 1, 5, 5), &mut out);
        assert!(!out.batches.is_empty(), "the pointer did not move");

        out.batches.clear();
        out.scope = Some((true, true));
        inject.on_control(&control(op::MOUSE_BUTTON, 1, 1, 0), &mut out);
        assert_eq!(out.keys_at(1), vec![BTN_LEFT]);
        out.scope = Some((true, false));
        inject.on_control(&control(op::MOUSE_BUTTON, 1, 0, 0), &mut out);
        assert_eq!(
            out.keys_at(0),
            vec![BTN_LEFT],
            "the dragged button stayed down"
        );
    }

    /// **The owner is held as every guest is while a scope is set**: every
    /// seat is shown the application alone, so behind another program its
    /// keys and clicks go nowhere, the Windows key never goes in, and Guide
    /// neither as a button nor in a whole state. With no scope set, all of
    /// them do.
    #[test]
    fn the_owner_is_held_while_a_scope_is_set() {
        let (mut inject, mut out) = held_guest();
        inject.set_owner(true, &mut out);
        out.scope = Some((false, false));
        inject.on_control(&key(4, true), &mut out);
        inject.on_control(&control(op::MOUSE_BUTTON, 1, 1, 0), &mut out);
        assert!(
            out.keys_at(1).is_empty(),
            "the owner's input went in behind the application"
        );

        out.scope = Some((true, true));
        inject.on_control(&key(227, true), &mut out);
        inject.on_control(&control(op::GAMEPAD_BUTTON, 5, 1, 1), &mut out);
        inject.on_control(
            &state(1, &pad_state_body(gamepad::bit::GUIDE, 0, 0, 0, 0, 0, 0)),
            &mut out,
        );
        assert!(
            out.keys_at(1).is_empty(),
            "a held owner pressed {:?}",
            out.keys_at(1)
        );

        out.scope = None;
        out.batches.clear();
        inject.on_control(&key(227, true), &mut out);
        inject.on_control(&control(op::GAMEPAD_BUTTON, 5, 1, 1), &mut out);
        assert_eq!(out.keys_at(1), vec![code(227), gamepad::key::MODE]);
    }

    /// **A held guest's pointer stays on the application's window**: a
    /// position on it goes in as asked, one past it at the window's edge and
    /// recorded as put there, on each axis alone.
    #[test]
    fn a_held_guests_pointer_stays_on_the_applications_window() {
        let (mut inject, mut out) = held_guest();
        out.bounds = Some(Bounds {
            left: 100,
            top: 200,
            right: 500,
            bottom: 600,
        });
        let at = |x: i32, y: i32| {
            vec![
                (ABS_X, scale(x, 1920, 0, 1920, 1920)),
                (ABS_Y, scale(y, 1080, 0, 1080, 1080)),
            ]
        };
        for (asked, put) in [
            ((300, 400), (300, 400)),
            ((50, 900), (100, 599)),
            ((1900, 10), (499, 200)),
            ((300, 1080), (300, 599)),
        ] {
            out.batches.clear();
            inject.on_control(
                &control(op::MOUSE_MOTION, 0, asked.0 as u32, asked.1 as u32),
                &mut out,
            );
            assert_eq!(abs(&out), at(put.0, put.1), "asked {asked:?}");
            assert_eq!(inject.commanded(), Some(put), "asked {asked:?}");
        }

        // With nothing held, the same position goes where it was asked.
        let (mut free, mut out) = (injector(), Recorder::default());
        free.on_control(&control(op::MOUSE_MOTION, 0, 50, 900), &mut out);
        assert_eq!(abs(&out), at(50, 900));
    }

    /// **A click asked past the window goes nowhere** rather than onto its
    /// edge, and the wheel with it; on the window, a click is judged where the
    /// guest put the pointer -- not where the system's pointer is -- and goes
    /// nowhere where that point is not the application's. A button pressed on
    /// the window still comes up past it.
    #[test]
    fn a_held_guests_click_past_the_window_goes_nowhere() {
        let (mut inject, mut out) = held_guest();
        out.bounds = Some(Bounds {
            left: 100,
            top: 200,
            right: 500,
            bottom: 600,
        });
        let motion = |x: u32, y: u32| control(op::MOUSE_MOTION, 0, x, y);
        inject.on_control(&motion(50, 900), &mut out);
        out.batches.clear();
        inject.on_control(&control(op::MOUSE_BUTTON, 1, 1, 0), &mut out);
        inject.on_control(&control(op::MOUSE_WHEEL, 0, 120, 0), &mut out);
        assert!(
            out.batches.is_empty(),
            "a click or a scroll went in past the window"
        );
        assert!(
            out.judged.is_empty(),
            "a point past the window was asked about"
        );

        inject.on_control(&motion(300, 400), &mut out);
        out.batches.clear();
        inject.on_control(&control(op::MOUSE_BUTTON, 1, 1, 0), &mut out);
        assert_eq!(out.keys_at(1), vec![BTN_LEFT]);
        assert_eq!(out.judged, vec![(300, 400)]);
        inject.on_control(&motion(50, 900), &mut out);
        inject.on_control(&control(op::MOUSE_BUTTON, 1, 0, 0), &mut out);
        assert_eq!(
            out.keys_at(0),
            vec![BTN_LEFT],
            "the dragged button stayed down"
        );

        out.scope = Some((true, false));
        inject.on_control(&motion(300, 400), &mut out);
        out.batches.clear();
        inject.on_control(&control(op::MOUSE_BUTTON, 1, 1, 0), &mut out);
        assert!(
            out.batches.is_empty(),
            "a click went in where the point is not the application's"
        );

        // Moved by deltas, a click is judged at the system's pointer.
        out.scope = Some((true, true));
        out.judged.clear();
        inject.on_control(&control(op::MOUSE_MOTION, 1, 5, 5), &mut out);
        inject.on_control(&control(op::MOUSE_BUTTON, 1, 1, 0), &mut out);
        assert!(
            out.judged.is_empty(),
            "a click after deltas was judged at a position"
        );
        assert_eq!(out.keys_at(1), vec![BTN_LEFT]);
    }

    /// **No window of the application on the captured output, and the
    /// pointer goes nowhere**, nor a click after it: nothing said where it may
    /// be, or a window wholly past the output.
    #[test]
    fn with_no_window_on_the_output_a_held_guests_pointer_goes_nowhere() {
        for bounds in [
            None,
            Some(Bounds {
                left: 1920,
                top: 0,
                right: 2500,
                bottom: 500,
            }),
        ] {
            let (mut inject, mut out) = held_guest();
            out.bounds = bounds;
            inject.on_control(&control(op::MOUSE_MOTION, 0, 300, 400), &mut out);
            inject.on_control(&control(op::MOUSE_BUTTON, 1, 1, 0), &mut out);
            assert!(out.batches.is_empty(), "{bounds:?}: something went in");
        }
    }

    /// **Tab under Alt brings the application's next window forward**: still
    /// never typed, its Alt masked, each Tab under one Alt stepping on, Shift
    /// stepping backwards, and an Alt let go starting the order afresh -- by
    /// a held Alt or the peer's mask alone. Escape under Alt switches nothing,
    /// and with nothing held Alt+Tab is the system's own.
    #[test]
    fn alt_tab_brings_the_applications_next_window_forward() {
        let (mut inject, mut out) = held_guest();
        inject.on_control(&key(226, true), &mut out);
        inject.on_control(&key(43, true), &mut out);
        inject.on_control(&key(43, false), &mut out);
        inject.on_control(&key(43, true), &mut out);
        inject.on_control(&key(225, true), &mut out);
        inject.on_control(&key(43, true), &mut out);
        inject.on_control(&key(225, false), &mut out);
        inject.on_control(&key(41, true), &mut out);
        // Alt let go and pressed again, the peer's mask saying Alt throughout.
        inject.on_control(&control(op::KEYBOARD, 226, 0x0100, 0), &mut out);
        inject.on_control(&control(op::KEYBOARD, 226, 0x0100, 1), &mut out);
        inject.on_control(&control(op::KEYBOARD, 43, 0x0100, 1), &mut out);
        assert_eq!(
            out.switched,
            vec![(false, false), (false, true), (true, true), (false, false)]
        );
        assert!(
            !out.keys_at(1).contains(&code(43)),
            "a Tab went in under Alt"
        );
        assert_eq!(out.masks, 5);

        // The mask alone, no Alt held here: a fresh order each time it lets
        // go between.
        inject.on_control(&key(226, false), &mut out);
        out.switched.clear();
        inject.on_control(&control(op::KEYBOARD, 43, 0x0200, 1), &mut out);
        inject.on_control(&control(op::KEYBOARD, 43, 0x0200, 0), &mut out);
        inject.on_control(&control(op::KEYBOARD, 43, 0x0201, 1), &mut out);
        inject.on_control(&key(4, true), &mut out);
        inject.on_control(&control(op::KEYBOARD, 43, 0x0100, 1), &mut out);
        assert_eq!(
            out.switched,
            vec![(false, false), (true, true), (false, false)]
        );

        let (mut free, mut out) = (injector(), Recorder::default());
        free.on_control(&key(226, true), &mut out);
        free.on_control(&key(43, true), &mut out);
        assert!(out.switched.is_empty());
        assert_eq!(out.keys_at(1), vec![code(226), code(43)]);
    }

    /// **Alt+Tab brings the application forward from behind**: with none of
    /// its windows in front, a held guest's Alt goes nowhere and its Tab under
    /// it -- by the peer's mask -- asks for the application's window, no Alt
    /// masked since none went down; a Tab after it under the same Alt, the
    /// application now in front, steps on. A Tab with no Alt in the mask, and
    /// Escape under Alt, ask for nothing.
    #[test]
    fn alt_tab_brings_the_application_forward_from_behind() {
        let (mut inject, mut out) = held_guest();
        out.scope = Some((false, false));
        inject.on_control(&control(op::KEYBOARD, 226, 0x0100, 1), &mut out);
        inject.on_control(&control(op::KEYBOARD, 43, 0, 1), &mut out);
        inject.on_control(&control(op::KEYBOARD, 41, 0x0100, 1), &mut out);
        assert!(out.switched.is_empty(), "{:?}", out.switched);
        inject.on_control(&control(op::KEYBOARD, 43, 0x0100, 1), &mut out);
        assert_eq!(out.switched, vec![(false, false)]);
        assert!(
            out.batches.is_empty(),
            "a key went in behind the application"
        );
        assert_eq!(out.masks, 0, "an Alt was masked that never went down");

        out.scope = Some((true, true));
        inject.on_control(&control(op::KEYBOARD, 43, 0x0100, 0), &mut out);
        inject.on_control(&control(op::KEYBOARD, 43, 0x0101, 1), &mut out);
        assert_eq!(out.switched, vec![(false, false), (true, true)]);

        // Alt let go, the application behind again: afresh.
        inject.on_control(&control(op::KEYBOARD, 226, 0, 0), &mut out);
        out.scope = Some((false, false));
        inject.on_control(&control(op::KEYBOARD, 43, 0x0100, 1), &mut out);
        assert_eq!(out.switched.last(), Some(&(false, false)));
    }

    /// **The keys that reach past the application never go in from a held
    /// guest**: the Windows keys and a key under them by the peer's mask,
    /// Print Screen, the host's sound and media keys, Tab under Alt and
    /// Escape under Alt or Control -- by a key held here or by the mask alone.
    /// Tab, Escape and Alt with any other key still go in.
    #[test]
    fn a_held_guest_never_presses_the_shells_keys_or_the_switching_chords() {
        let (mut inject, mut out) = held_guest();
        for usage in [227, 231, 70, 127, 128, 129, 258, 261] {
            inject.on_control(&key(usage, true), &mut out);
        }
        inject.on_control(&control(op::KEYBOARD, 21, 0x0400, 1), &mut out);
        assert!(out.keys_at(1).is_empty(), "{:?} went in", out.keys_at(1));

        inject.on_control(&key(226, true), &mut out);
        inject.on_control(&key(43, true), &mut out);
        inject.on_control(&key(41, true), &mut out);
        inject.on_control(&key(226, false), &mut out);
        inject.on_control(&key(224, true), &mut out);
        inject.on_control(&key(41, true), &mut out);
        inject.on_control(&key(224, false), &mut out);
        inject.on_control(&control(op::KEYBOARD, 43, 0x0100, 1), &mut out);
        inject.on_control(&control(op::KEYBOARD, 41, 0x0080, 1), &mut out);
        assert_eq!(out.keys_at(1), vec![code(226), code(224)]);

        out.batches.clear();
        for usage in [43, 41] {
            inject.on_control(&key(usage, true), &mut out);
            inject.on_control(&key(usage, false), &mut out);
        }
        inject.on_control(&key(226, true), &mut out);
        inject.on_control(&key(61, true), &mut out);
        assert_eq!(
            out.keys_at(1),
            vec![code(43), code(41), code(226), code(61)]
        );
    }

    /// **A key refused under Alt is followed by a keystroke of no key**, so
    /// the Alt let go after it is not the window's menu key: under a held Alt
    /// or the mask's. A refusal with no Alt down types none, and neither does
    /// a key under Alt that goes in.
    #[test]
    fn a_key_refused_under_alt_masks_the_alt() {
        let (mut inject, mut out) = held_guest();
        inject.on_control(&key(226, true), &mut out);
        inject.on_control(&key(43, true), &mut out);
        assert_eq!(out.masks, 1);
        inject.on_control(&key(61, true), &mut out);
        inject.on_control(&key(226, false), &mut out);
        inject.on_control(&key(227, true), &mut out);
        assert_eq!(out.masks, 1, "a refusal with no Alt down typed one");
        inject.on_control(&control(op::KEYBOARD, 41, 0x0200, 1), &mut out);
        assert_eq!(out.masks, 2);
    }

    /// **Alt or Control going down under a held Tab or Escape lets it go
    /// first**, or the system's own repeat of the held key would make the
    /// chord; where nothing holds the guest, nothing changes.
    #[test]
    fn alt_or_control_under_a_held_tab_or_escape_lets_it_go_first() {
        let (mut inject, mut out) = held_guest();
        inject.on_control(&key(43, true), &mut out);
        out.batches.clear();
        inject.on_control(&key(226, true), &mut out);
        assert_eq!(
            out.keys_at(0),
            vec![code(43)],
            "the Tab was left held under Alt"
        );
        assert_eq!(out.keys_at(1), vec![code(226)]);

        inject.on_control(&key(226, false), &mut out);
        inject.on_control(&key(41, true), &mut out);
        out.batches.clear();
        inject.on_control(&key(228, true), &mut out);
        assert_eq!(
            out.keys_at(0),
            vec![code(41)],
            "the Escape was left held under Control"
        );

        let (mut free, mut out) = (injector(), Recorder::default());
        free.on_control(&key(43, true), &mut out);
        free.on_control(&key(226, true), &mut out);
        assert!(out.keys_at(0).is_empty());
    }

    /// **A held guest's pads go in only while the application is in front**,
    /// wherever the pointer is.
    #[test]
    fn a_held_guests_pads_go_in_only_while_the_application_is_in_front() {
        let (mut inject, mut out) = held_guest();
        out.scope = Some((false, true));
        let a = pad_state_body(gamepad::bit::A, 0, 0, 0, 0, 0, 0);
        inject.on_control(&state(1, &a), &mut out);
        inject.on_control(&control(op::GAMEPAD_BUTTON, 0, 1, 1), &mut out);
        assert!(
            out.batches.is_empty(),
            "a pad went in behind the application"
        );

        out.scope = Some((true, false));
        inject.on_control(&state(1, &a), &mut out);
        assert_eq!(out.keys_at(1), vec![gamepad::key::SOUTH]);
    }
}
