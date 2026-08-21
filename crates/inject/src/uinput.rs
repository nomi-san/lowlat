//! The devices themselves.
//!
//! Three per guest: a keyboard, a relative pointer, and an absolute pointer.
//! **The two pointers cannot be one device.** An absolute pointer declares
//! axes and a property a relative one must not have, and a single device
//! carrying both is read differently by different consumers
//! (docs/07-platforms.md section 4).
//!
//! Nothing here decides anything. It opens the node, declares what the device
//! can do, and writes batches the expander already built.

use crate::event::{ABS_RANGE, Device, Event, Sink};
use crate::gamepad::{self, MAX_PADS};
use crate::usage;
use core::fmt::Write as _;
use std::os::fd::{AsRawFd as _, OwnedFd};

/// Request numbers, as the kernel's own headers compute them.
mod ioctl {
    pub(super) const DEV_CREATE: libc::c_ulong = 0x0000_5501;
    pub(super) const DEV_SETUP: libc::c_ulong = 0x405c_5503;
    pub(super) const ABS_SETUP: libc::c_ulong = 0x401c_5504;
    pub(super) const SET_EVBIT: libc::c_ulong = 0x4004_5564;
    pub(super) const SET_KEYBIT: libc::c_ulong = 0x4004_5565;
    pub(super) const SET_RELBIT: libc::c_ulong = 0x4004_5566;
    pub(super) const SET_ABSBIT: libc::c_ulong = 0x4004_5567;
    pub(super) const SET_MSCBIT: libc::c_ulong = 0x4004_5568;
    pub(super) const SET_PROPBIT: libc::c_ulong = 0x4004_556e;
    pub(super) const SET_FFBIT: libc::c_ulong = 0x4004_556b;
    pub(super) const SET_PHYS: libc::c_ulong = 0x4008_556c;
    pub(super) const BEGIN_FF_UPLOAD: libc::c_ulong = 0xc068_55c8;
    pub(super) const END_FF_UPLOAD: libc::c_ulong = 0x4068_55c9;
    pub(super) const BEGIN_FF_ERASE: libc::c_ulong = 0xc00c_55ca;
    pub(super) const END_FF_ERASE: libc::c_ulong = 0x400c_55cb;
}

const EV_SYN: libc::c_int = 0x00;
/// The report marker, as the event stream carries it.
const SYN: u16 = 0x00;
const EV_KEY: libc::c_int = 0x01;
const EV_REL: libc::c_int = 0x02;
const EV_ABS: libc::c_int = 0x03;
const EV_MSC: libc::c_int = 0x04;
const EV_FF: u16 = 0x15;
/// The kernel asks the device's creator a question through this.
const EV_UINPUT: u16 = 0x0101;
const UI_FF_UPLOAD: u16 = 1;
const UI_FF_ERASE: u16 = 2;
/// The only effect offered. See [`crate::gamepad`] for why.
const FF_RUMBLE: libc::c_int = 0x50;
/// Not an effect: a scale applied to all of them.
const FF_GAIN: u16 = 0x60;

/// Effects one pad may have uploaded at once.
///
/// Applications upload a handful and reuse them. The bound exists so a pad
/// costs a fixed amount, not because anything approaches it.
const EFFECTS: u32 = 16;

const MSC_SCAN: libc::c_int = 4;
const REL_X: libc::c_int = 0x00;
const REL_Y: libc::c_int = 0x01;
const REL_HWHEEL: libc::c_int = 0x06;
const REL_WHEEL: libc::c_int = 0x08;
const REL_WHEEL_HI_RES: libc::c_int = 0x0b;
const REL_HWHEEL_HI_RES: libc::c_int = 0x0c;
const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
const BTN_FIRST: libc::c_int = 0x110;
const BTN_LAST: libc::c_int = 0x114;

/// Says the device points at a position rather than reporting a surface
/// somebody is touching. Without it an absolute device with buttons is read
/// as a touch surface by parts of the input stack.
const INPUT_PROP_POINTER: libc::c_int = 0x00;

/// The device is not on any physical bus and does not claim to be.
const BUS_VIRTUAL: u16 = 0x06;
/// A pad claims this instead. See [`PAD`].
const BUS_USB: u16 = 0x03;

/// Not a real vendor's number. Distinctive so these devices are greppable in
/// a device listing, and on a bus where nothing can collide with it.
const VENDOR: u16 = 0x6c6c;
const VERSION: u16 = 1;

/// What a device tells the world it is.
#[derive(Debug, Clone, Copy)]
struct Identity {
    bus: u16,
    vendor: u16,
    product: u16,
    version: u16,
    /// Effects it claims room for. **Non-zero exactly when force feedback is
    /// declared**, or the device is refused outright.
    effects: u32,
}

impl Identity {
    /// A device that is ours and says so.
    const fn ours(product: u16) -> Self {
        Self {
            bus: BUS_VIRTUAL,
            vendor: VENDOR,
            product,
            version: VERSION,
            effects: 0,
        }
    }
}

/// **A pad borrows a real controller's identity, and it has to.**
///
/// Everything that reads a gamepad decides what its buttons mean by looking
/// the identity up in a table: the browser gamepad interface, the common
/// controller libraries, and every per-title mapping people share. A device
/// with an identity nobody has heard of is delivered as a bag of numbered
/// buttons and axes, and no amount of correct input makes it usable.
///
/// **The mapping this selects is the one we actually emit.** The Linux entry
/// for this identity is buttons zero to ten in kernel-code order and axes zero
/// to five, with the direction pad on the first hat -- which is exactly the
/// device [`pad`] builds, checked entry by entry rather than assumed. Claiming
/// an identity whose mapping did not match would be worse than claiming none.
///
/// The honest half is not lost: [`UI_SET_PHYS`] carries which guest and which
/// pad, which is what a physical-location string is for.
const PAD: Identity = Identity {
    bus: BUS_USB,
    vendor: 0x045e,
    product: 0x028e,
    version: 0x0114,
    effects: EFFECTS,
};

/// What a pad calls itself, which is what a real one on this platform calls
/// itself.
const PAD_NAME: &str = "Microsoft X-Box 360 pad";

const NAME_LEN: usize = 80;

/// Events converted in one call to the kernel.
///
/// **A report ends at its own marker, not at a write boundary**, so a batch
/// larger than this is split across calls without splitting a report.
const CHUNK: usize = 64;

/// How long after creation a device is assumed to be delivering.
///
/// **A created device is not a usable one.** The kernel publishes it in about
/// a quarter of a millisecond and the display stack does not start delivering
/// from it for a fifth of a second or more; events written into that gap are
/// accepted and go nowhere at all. Measured here with the same three devices a
/// guest really gets: first delivery at 200 to 260 ms across six runs.
///
/// **There is nothing to wait on instead**, and this was tested rather than
/// assumed. The one observable milestone in the gap is the device manager
/// granting the group on the event node, which happens at 50 to 82 ms and does
/// not predict delivery: the same 51 ms grant preceded delivery at 200 ms in
/// one run and 230 ms in another. It is a precondition, not a signal.
///
/// So the figure is a clock, and it is set above the worst case rather than at
/// it, because the two errors are not symmetric: a first keystroke arriving
/// late is invisible, and one that vanishes reads as a network fault. It is
/// also unmeasured under a compositor, where the devices arrive by a different
/// route and there is no reason to expect it faster.
const USABLE_AFTER_MS: f64 = 400.0;

/// Events held per device while it becomes usable.
///
/// Generous on purpose: a person types perhaps three keys in the window, and
/// the cost of a slot is eight bytes.
const HELD: usize = 256;

/// Why a device could not be created.
///
/// **The three that are not a bug in this program are told apart**, because
/// each has a different fix and one message covering all of them turns every
/// deployment into a conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The device node does not exist. The kernel module is not loaded.
    NoModule,
    /// The node exists and this process may not write to it. The group
    /// membership or the rule granting it is missing.
    NotPermitted,
    /// The node opened and the kernel refused to create the device. Something
    /// is confining this process beyond file permissions.
    Confined(i32),
    /// Anything else, carrying what the kernel said.
    Failed(i32),
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NoModule => f.write_str("input device node absent, kernel module not loaded"),
            Self::NotPermitted => {
                f.write_str("input device node not writable, group or rule missing")
            }
            Self::Confined(errno) => write!(f, "device creation refused, errno={errno}"),
            Self::Failed(errno) => write!(f, "device setup failed, errno={errno}"),
        }
    }
}

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// One virtual device.
///
/// **No `Drop`, deliberately.** Closing the descriptor is what destroys the
/// device, and the kernel releases every key it was holding as it goes. An
/// explicit destroy before the close does the same thing twice, and a
/// hand-written `Drop` here would read as though it were load bearing.
#[derive(Debug)]
struct Node {
    fd: OwnedFd,
    /// Events produced before the device could deliver them.
    held: Held,
    /// Effects an application has uploaded to this device.
    ///
    /// **Held here rather than answered and forgotten**, because playing one
    /// names it by identifier and nothing else says how hard to vibrate.
    effects: [Effect; EFFECTS as usize],
    /// A scale over every effect, which an application may turn down.
    gain: u16,
    /// **Each device has its own deadline**, because they are not created
    /// together: a guest's keyboard and pointers arrive when it is admitted
    /// and a pad arrives whenever it first sends one, which may be minutes
    /// later.
    created: lowlat_common::clock::Time,
    usable: bool,
}

/// Events waiting for a device to become usable.
///
/// **Its own type, with no descriptor**, so the bound and what happens at the
/// bound are testable without publishing a device to the running session.
#[derive(Debug)]
struct Held {
    events: [Event; HELD],
    used: usize,
    /// Set once, so an overflow is reported rather than repeated.
    reported: bool,
}

impl Default for Held {
    fn default() -> Self {
        Self {
            events: [Event {
                kind: 0,
                code: 0,
                value: 0,
            }; HELD],
            used: 0,
            reported: false,
        }
    }
}

impl Held {
    /// **Held rather than dropped.** The window is a fifth of a second and
    /// what falls in it is the first thing somebody typed.
    fn push(&mut self, events: &[Event]) {
        for event in events {
            if self.used >= HELD {
                self.discard_oldest_report();
            }
            if let Some(slot) = self.events.get_mut(self.used) {
                *slot = *event;
                self.used += 1;
            }
        }
    }

    /// Make room by dropping the oldest whole report.
    ///
    /// **A whole report, never part of one.** Half a report reaches a consumer
    /// as a state change nobody asked for. Losing a press whose release
    /// survives is the harmless direction: the kernel discards a release for a
    /// key that is not down.
    fn discard_oldest_report(&mut self) {
        let end = self
            .events
            .get(..self.used)
            .and_then(|held| held.iter().position(|e| e.kind == SYN))
            .map_or(self.used, |at| at + 1);
        self.events.copy_within(end..self.used, 0);
        self.used -= end;
        if !self.reported {
            self.reported = true;
            lowlat_common::log_warn!(
                "inject: held events overflowed before the device was usable, held={HELD}"
            );
        }
    }

    fn take(&mut self) -> usize {
        core::mem::replace(&mut self.used, 0)
    }
}

impl Node {
    /// Open the node and start describing a device on it.
    ///
    /// **Read as well as written.** Only a pad ever has anything to read, but
    /// the kernel asks its questions on the same descriptor the device was
    /// created on, so a write-only one could never be answered. Non-blocking
    /// throughout: nothing here may park a guest's thread.
    fn open() -> Result<Self, Error> {
        // SAFETY: a constant NUL-terminated path and flags the call defines.
        let raw = unsafe {
            libc::open(
                c"/dev/uinput".as_ptr(),
                libc::O_RDWR | libc::O_CLOEXEC | libc::O_NONBLOCK,
            )
        };
        if raw < 0 {
            return Err(match errno() {
                libc::ENOENT | libc::ENODEV => Error::NoModule,
                libc::EACCES | libc::EPERM => Error::NotPermitted,
                other => Error::Failed(other),
            });
        }
        // SAFETY: the descriptor is fresh from open and is not owned elsewhere.
        let fd = unsafe { <OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(raw) };
        Ok(Self {
            fd,
            held: Held::default(),
            effects: [Effect::default(); EFFECTS as usize],
            gain: u16::MAX,
            created: lowlat_common::clock::Time::now(),
            usable: false,
        })
    }

    fn set(&self, request: libc::c_ulong, bit: libc::c_int) -> Result<(), Error> {
        // SAFETY: the request takes an int by value and the descriptor is ours.
        let rc = unsafe { libc::ioctl(self.fd.as_raw_fd(), request, bit) };
        if rc < 0 {
            Err(Error::Failed(errno()))
        } else {
            Ok(())
        }
    }

    /// Say where the device is, which is how one of ours is told from another
    /// when they all claim the same identity.
    fn set_phys(&self, phys: &str) -> Result<(), Error> {
        let mut bytes = [0u8; NAME_LEN];
        for (slot, byte) in bytes.iter_mut().take(NAME_LEN - 1).zip(phys.as_bytes()) {
            *slot = *byte;
        }
        // SAFETY: the request reads a NUL-terminated string, and the buffer
        // outlives the call with its last byte left zero.
        let rc = unsafe {
            libc::ioctl(
                self.fd.as_raw_fd(),
                ioctl::SET_PHYS,
                bytes.as_ptr().cast::<libc::c_char>(),
            )
        };
        if rc < 0 {
            Err(Error::Failed(errno()))
        } else {
            Ok(())
        }
    }

    fn create(&self, name: &str, identity: Identity) -> Result<(), Error> {
        let mut setup = UinputSetup {
            bustype: identity.bus,
            vendor: identity.vendor,
            product: identity.product,
            version: identity.version,
            name: [0; NAME_LEN],
            ff_effects_max: identity.effects,
        };
        // Truncated rather than refused: the name is a label in a device
        // listing and nothing reads it back.
        for (slot, byte) in setup
            .name
            .iter_mut()
            .take(NAME_LEN - 1)
            .zip(name.as_bytes())
        {
            *slot = *byte;
        }
        // SAFETY: the request reads a uinput_setup, which is what is passed,
        // and the layout is fixed by the kernel's own header.
        let rc = unsafe {
            libc::ioctl(
                self.fd.as_raw_fd(),
                ioctl::DEV_SETUP,
                core::ptr::from_ref(&setup),
            )
        };
        if rc < 0 {
            return Err(Error::Failed(errno()));
        }
        // SAFETY: the request takes no argument.
        let rc = unsafe { libc::ioctl(self.fd.as_raw_fd(), ioctl::DEV_CREATE) };
        if rc < 0 {
            // **This is the interesting failure.** The node opened, so the
            // module is there and the permissions are right, and the kernel
            // still said no.
            return Err(Error::Confined(errno()));
        }
        Ok(())
    }

    fn absolute_axis(&self, code: u16, maximum: i32) -> Result<(), Error> {
        self.absolute_axis_signed(code, 0, maximum)
    }

    fn absolute_axis_signed(&self, code: u16, minimum: i32, maximum: i32) -> Result<(), Error> {
        let setup = UinputAbsSetup {
            code,
            filler: 0,
            value: 0,
            minimum,
            maximum,
            fuzz: 0,
            flat: 0,
            resolution: 0,
        };
        // SAFETY: the request reads a uinput_abs_setup, which is what is
        // passed, with the layout the kernel's header fixes.
        let rc = unsafe {
            libc::ioctl(
                self.fd.as_raw_fd(),
                ioctl::ABS_SETUP,
                core::ptr::from_ref(&setup),
            )
        };
        if rc < 0 {
            Err(Error::Failed(errno()))
        } else {
            Ok(())
        }
    }

    /// Send a batch, holding it if the device cannot deliver yet.
    fn send(&mut self, events: &[Event]) -> Result<(), Error> {
        if !self.usable {
            if lowlat_common::clock::elapsed_ms(self.created) < USABLE_AFTER_MS {
                self.held.push(events);
                return Ok(());
            }
            self.usable = true;
            self.flush()?;
        }
        write_to(&self.fd, events)
    }

    /// Release anything held once the device can deliver it.
    fn tick(&mut self) -> Result<(), Error> {
        if self.usable || lowlat_common::clock::elapsed_ms(self.created) < USABLE_AFTER_MS {
            return Ok(());
        }
        self.usable = true;
        self.flush()
    }

    /// Answer whatever the kernel has asked, and report what to vibrate at.
    ///
    /// **Answering is not optional.** An upload the creator never completes
    /// leaves the application's own call blocked until it times out, and it
    /// then reports a controller that cannot rumble.
    fn poll_rumble(&mut self) -> Option<(u16, u16)> {
        let mut latest = None;
        let mut frame = [0u8; size_of::<InputEvent>()];
        loop {
            // SAFETY: the buffer is one event long and lives across the call.
            let read = unsafe {
                libc::read(
                    self.fd.as_raw_fd(),
                    frame.as_mut_ptr().cast::<libc::c_void>(),
                    frame.len(),
                )
            };
            // A whole event or nothing: a short read is a closed or drained
            // descriptor, and a negative one is "nothing waiting".
            if !usize::try_from(read).is_ok_and(|got| got == frame.len()) {
                return latest;
            }
            let at = |offset: usize| -> u16 {
                u16::from_ne_bytes([
                    frame.get(offset).copied().unwrap_or(0),
                    frame.get(offset + 1).copied().unwrap_or(0),
                ])
            };
            let value = i32::from_ne_bytes([
                frame.get(20).copied().unwrap_or(0),
                frame.get(21).copied().unwrap_or(0),
                frame.get(22).copied().unwrap_or(0),
                frame.get(23).copied().unwrap_or(0),
            ]);
            let (kind, code) = (at(16), at(18));
            match (kind, code) {
                (EV_UINPUT, UI_FF_UPLOAD) => self.take_upload(value),
                (EV_UINPUT, UI_FF_ERASE) => self.take_erase(value),
                // **The gain is a scale, not an effect**, and an application
                // that turns it down expects that to be honoured rather than
                // treated as an unknown effect identifier.
                (EV_FF, FF_GAIN) => self.gain = u16::try_from(value).unwrap_or(u16::MAX),
                (EV_FF, _) => {
                    latest = Some(self.play(code, value != 0));
                }
                _ => {}
            }
        }
    }

    /// Take an uploaded effect and tell the kernel it was accepted.
    fn take_upload(&mut self, request: i32) {
        let mut upload = UinputFfUpload {
            request_id: u32::try_from(request).unwrap_or(0),
            retval: 0,
            effect: FfEffect::default(),
            old: FfEffect::default(),
        };
        // SAFETY: the request reads and writes a uinput_ff_upload, which is
        // what is passed, with the layout the kernel's header fixes.
        if unsafe {
            libc::ioctl(
                self.fd.as_raw_fd(),
                ioctl::BEGIN_FF_UPLOAD,
                core::ptr::from_mut(&mut upload),
            )
        } < 0
        {
            return;
        }
        let (id, strong, weak) = (upload.effect.id, upload.effect.strong, upload.effect.weak);
        // **Accepted even when there is no room for it.** Refusing an upload
        // is reported to the application as a broken device; forgetting one
        // costs a single effect nobody is playing.
        upload.retval = 0;
        // SAFETY: as above, and the struct is unchanged apart from the result.
        unsafe {
            libc::ioctl(
                self.fd.as_raw_fd(),
                ioctl::END_FF_UPLOAD,
                core::ptr::from_ref(&upload),
            );
        }
        let slot = self
            .effects
            .iter()
            .position(|e| e.used && e.id == id)
            .or_else(|| self.effects.iter().position(|e| !e.used));
        if let Some(slot) = slot.and_then(|at| self.effects.get_mut(at)) {
            *slot = Effect {
                id,
                used: true,
                strong,
                weak,
            };
        }
    }

    fn take_erase(&mut self, request: i32) {
        let mut erase = UinputFfErase {
            request_id: u32::try_from(request).unwrap_or(0),
            retval: 0,
            effect_id: 0,
        };
        // SAFETY: the request reads and writes a uinput_ff_erase.
        if unsafe {
            libc::ioctl(
                self.fd.as_raw_fd(),
                ioctl::BEGIN_FF_ERASE,
                core::ptr::from_mut(&mut erase),
            )
        } < 0
        {
            return;
        }
        let id = i16::try_from(erase.effect_id).unwrap_or(-1);
        erase.retval = 0;
        // SAFETY: as above.
        unsafe {
            libc::ioctl(
                self.fd.as_raw_fd(),
                ioctl::END_FF_ERASE,
                core::ptr::from_ref(&erase),
            );
        }
        if let Some(slot) = self.effects.iter_mut().find(|e| e.used && e.id == id) {
            *slot = Effect::default();
        }
    }

    /// What starting or stopping an effect comes to, at the current gain.
    fn play(&mut self, id: u16, started: bool) -> (u16, u16) {
        if !started {
            return (0, 0);
        }
        let Some(effect) = self
            .effects
            .iter()
            .find(|e| e.used && e.id == i16::try_from(id).unwrap_or(-1))
        else {
            return (0, 0);
        };
        let scale = |value: u16| -> u16 {
            let scaled = u32::from(value) * u32::from(self.gain) / u32::from(u16::MAX);
            u16::try_from(scaled).unwrap_or(u16::MAX)
        };
        (scale(effect.strong), scale(effect.weak))
    }

    /// Write everything held, if there is anything.
    fn flush(&mut self) -> Result<(), Error> {
        let used = self.held.take();
        match self.held.events.get(..used) {
            Some(events) if used > 0 => write_to(&self.fd, events),
            _ => Ok(()),
        }
    }
}

fn write_to(fd: &OwnedFd, events: &[Event]) -> Result<(), Error> {
    {
        for chunk in events.chunks(CHUNK) {
            let mut out = [InputEvent::EMPTY; CHUNK];
            let mut used = 0;
            for (slot, event) in out.iter_mut().zip(chunk) {
                // The kernel stamps the time. Sending our own would date the
                // event from when it was expanded rather than from when it
                // was delivered, which is the figure anything downstream is
                // actually measuring.
                *slot = InputEvent {
                    seconds: 0,
                    microseconds: 0,
                    kind: event.kind,
                    code: event.code,
                    value: event.value,
                };
                used += 1;
            }
            let Some(filled) = out.get(..used) else {
                continue;
            };
            let bytes = size_of::<InputEvent>() * used;
            // SAFETY: the pointer and length describe `filled`, which lives
            // until the call returns, and the descriptor is ours.
            let written = unsafe {
                libc::write(
                    fd.as_raw_fd(),
                    filled.as_ptr().cast::<libc::c_void>(),
                    bytes,
                )
            };
            if written < 0 {
                return Err(Error::Failed(errno()));
            }
        }
        Ok(())
    }
}

#[repr(C)]
#[derive(Debug)]
struct UinputSetup {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
    name: [u8; NAME_LEN],
    ff_effects_max: u32,
}

#[repr(C)]
#[derive(Debug)]
struct UinputAbsSetup {
    code: u16,
    filler: u16,
    value: i32,
    minimum: i32,
    maximum: i32,
    fuzz: i32,
    flat: i32,
    resolution: i32,
}

/// What a pad is asked to vibrate at, on the way back to its peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rumble {
    /// The peer's own identifier for the pad, so it needs no mapping back.
    pub pad: u32,
    /// **Eight bits each, as the peer carries them.** The kernel counts a
    /// motor in sixteen and the peer widens what it is sent by repeating the
    /// byte, so taking the high byte is exactly the inverse and a value
    /// survives the round trip unchanged.
    pub large: u8,
    pub small: u8,
}

/// One effect a pad is holding, reduced to what a peer can express.
#[derive(Debug, Clone, Copy, Default)]
struct Effect {
    id: i16,
    used: bool,
    strong: u16,
    weak: u16,
}

#[repr(C)]
#[derive(Debug)]
struct UinputFfUpload {
    request_id: u32,
    retval: i32,
    effect: FfEffect,
    old: FfEffect,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct FfEffect {
    kind: u16,
    id: i16,
    direction: u16,
    trigger_button: u16,
    trigger_interval: u16,
    replay_length: u16,
    replay_delay: u16,
    /// The union, of which only the plain magnitude arm is read: two
    /// sixteen-bit motors at its front, then padding to the union's size.
    strong: u16,
    weak: u16,
    rest: [u8; 28],
}

#[repr(C)]
#[derive(Debug, Default)]
struct UinputFfErase {
    request_id: u32,
    retval: i32,
    effect_id: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct InputEvent {
    seconds: i64,
    microseconds: i64,
    kind: u16,
    code: u16,
    value: i32,
}

impl InputEvent {
    const EMPTY: Self = Self {
        seconds: 0,
        microseconds: 0,
        kind: 0,
        code: 0,
        value: 0,
    };
}

/// One guest's three devices.
#[derive(Debug)]
pub struct Devices {
    keyboard: Node,
    pointer: Node,
    pointer_absolute: Node,
    /// **Created on first use, not at admission.** A guest's keyboard and
    /// pointers exist because it connected; a pad exists because it sent one,
    /// and how many it has is not knowable in advance.
    pads: [Option<PadNode>; MAX_PADS],
    /// Set once, so a guest holding more pads than there are slots is
    /// reported rather than silently short of one.
    refused: bool,
    label: NameBuf,
}

/// One pad's device, addressed by the identifier its peer gave it.
#[derive(Debug)]
struct PadNode {
    id: u32,
    node: Node,
    /// What was last reported, so an effect replayed at the same strength
    /// does not put another message on the wire.
    last_rumble: (u8, u8),
}

impl Devices {
    /// Create all three, or none.
    ///
    /// **Named with the guest's own label** so a device listing says which
    /// session a device belongs to, which is the first question asked of one
    /// behaving oddly. The label is whatever the caller correlates its logs
    /// on; a long one is truncated rather than refused.
    pub fn create(guest: &str) -> Result<Self, Error> {
        Ok(Self {
            keyboard: keyboard(guest)?,
            pointer: pointer(guest)?,
            pointer_absolute: pointer_absolute(guest)?,
            pads: [const { None }; MAX_PADS],
            refused: false,
            label: {
                let mut label = NameBuf::default();
                let _ = label.write_str(guest);
                label
            },
        })
    }

    /// Release anything held once the devices can deliver it.
    ///
    /// **Called from the loop that already runs, not only on the next event.**
    /// A guest that types one key and waits would otherwise have it sit in the
    /// queue until it typed a second.
    pub fn tick(&mut self) {
        let pads = self
            .pads
            .iter_mut()
            .filter_map(Option::as_mut)
            .map(|p| &mut p.node);
        for node in [
            &mut self.keyboard,
            &mut self.pointer,
            &mut self.pointer_absolute,
        ]
        .into_iter()
        .chain(pads)
        {
            if let Err(error) = node.tick() {
                lowlat_common::log_warn!("inject: held events lost, error={error}");
            }
        }
    }

    /// What any of this guest's pads has been asked to vibrate at, since the
    /// last ask.
    ///
    /// **Polled rather than pushed.** The kernel raises these on the same
    /// descriptor the device was created on, and the guest's loop is already
    /// turning; a thread per pad would be a thread per pad for a message that
    /// arrives when somebody drives over rubble.
    pub fn rumble(&mut self) -> Option<Rumble> {
        for pad in self.pads.iter_mut().filter_map(Option::as_mut) {
            if let Some((strong, weak)) = pad.node.poll_rumble() {
                let (large, small) = (high_byte(strong), high_byte(weak));
                if (large, small) == pad.last_rumble {
                    continue;
                }
                pad.last_rumble = (large, small);
                return Some(Rumble {
                    pad: pad.id,
                    large,
                    small,
                });
            }
        }
        None
    }

    /// The device for a pad, creating it if this is the first event for it.
    fn pad(&mut self, id: u32) -> Option<&mut Node> {
        if let Some(index) = self
            .pads
            .iter()
            .position(|p| p.as_ref().is_some_and(|p| p.id == id))
        {
            return self
                .pads
                .get_mut(index)
                .and_then(Option::as_mut)
                .map(|p| &mut p.node);
        }
        let Some(index) = self.pads.iter().position(Option::is_none) else {
            if !self.refused {
                self.refused = true;
                lowlat_common::log_warn!("inject: no room for another pad, pads={MAX_PADS}");
            }
            return None;
        };
        let node = match pad(self.label.as_str(), id) {
            Ok(node) => node,
            Err(error) => {
                lowlat_common::log_warn!("inject: pad not created, pad={id} error={error}");
                return None;
            }
        };
        lowlat_common::log_info!("inject: pad created, pad={id}");
        let slot = self.pads.get_mut(index)?;
        *slot = Some(PadNode {
            id,
            node,
            last_rumble: (0, 0),
        });
        slot.as_mut().map(|p| &mut p.node)
    }
}

impl Sink for Devices {
    fn emit(&mut self, device: Device, events: &[Event]) {
        let node = match device {
            Device::Keyboard => &mut self.keyboard,
            Device::Pointer => &mut self.pointer,
            Device::PointerAbsolute => &mut self.pointer_absolute,
            Device::Gamepad(id) => {
                let Some(node) = self.pad(id) else { return };
                node
            }
        };
        if let Err(error) = node.send(events) {
            // A write that fails is not recoverable here and must not stop the
            // session: the guest is still connected and still being sent
            // video. It is reported and the events are lost.
            lowlat_common::log_warn!("inject: write failed, device={device:?} error={error}");
        }
    }

    fn unplug(&mut self, pad: u32) {
        let Some(index) = self
            .pads
            .iter()
            .position(|p| p.as_ref().is_some_and(|p| p.id == pad))
        else {
            return;
        };
        // Dropping the device is the unplug, and it is also what releases
        // everything the pad was holding.
        if let Some(slot) = self.pads.get_mut(index) {
            *slot = None;
        }
        lowlat_common::log_info!("inject: pad unplugged, pad={pad}");
    }
}

fn name(kind: &str, guest: &str) -> NameBuf {
    let mut buf = NameBuf::default();
    // The write cannot fail: the buffer discards past its capacity.
    let _ = write!(buf, "lowlat {kind} (guest {guest})");
    buf
}

fn keyboard(guest: &str) -> Result<Node, Error> {
    let node = Node::open()?;
    node.set(ioctl::SET_EVBIT, EV_SYN)?;
    node.set(ioctl::SET_EVBIT, EV_KEY)?;
    node.set(ioctl::SET_EVBIT, EV_MSC)?;
    node.set(ioctl::SET_MSCBIT, MSC_SCAN)?;
    // **No indicator lights.** A device claiming them takes about twice as
    // long to become usable, because the display server does keyboard-state
    // setup for it, and nothing ever reads an injected keyboard's lights.
    //
    // **No auto-repeat either.** Repeat comes from the display server while a
    // key is held, and the kernel discards a second press for a key already
    // down, so a peer's own repeats cost nothing and a second source would
    // only double them.
    let mut declared = [false; 0x300];
    for peer_code in 0..=u16::MAX {
        let Some(code) = usage::key_code(peer_code) else {
            continue;
        };
        let Some(seen) = declared.get_mut(usize::from(code)) else {
            continue;
        };
        if core::mem::replace(seen, true) {
            continue;
        }
        node.set(ioctl::SET_KEYBIT, libc::c_int::from(code))?;
    }
    node.create(name("keyboard", guest).as_str(), Identity::ours(1))?;
    Ok(node)
}

fn pointer(guest: &str) -> Result<Node, Error> {
    let node = Node::open()?;
    node.set(ioctl::SET_EVBIT, EV_SYN)?;
    node.set(ioctl::SET_EVBIT, EV_KEY)?;
    node.set(ioctl::SET_EVBIT, EV_REL)?;
    for button in BTN_FIRST..=BTN_LAST {
        node.set(ioctl::SET_KEYBIT, button)?;
    }
    for axis in [
        REL_X,
        REL_Y,
        REL_WHEEL,
        REL_WHEEL_HI_RES,
        REL_HWHEEL,
        REL_HWHEEL_HI_RES,
    ] {
        node.set(ioctl::SET_RELBIT, axis)?;
    }
    node.create(name("pointer", guest).as_str(), Identity::ours(2))?;
    Ok(node)
}

fn pointer_absolute(guest: &str) -> Result<Node, Error> {
    let node = Node::open()?;
    node.set(ioctl::SET_PROPBIT, INPUT_PROP_POINTER)?;
    node.set(ioctl::SET_EVBIT, EV_SYN)?;
    node.set(ioctl::SET_EVBIT, EV_KEY)?;
    node.set(ioctl::SET_EVBIT, EV_REL)?;
    node.set(ioctl::SET_EVBIT, EV_ABS)?;
    for button in BTN_FIRST..=BTN_LAST {
        node.set(ioctl::SET_KEYBIT, button)?;
    }
    // **The wheels are on both pointers and the motion axes are not.** A
    // wheel event belongs to whichever pointer the position came from, and a
    // device carrying both kinds of motion is the merge this design avoids.
    for axis in [REL_WHEEL, REL_WHEEL_HI_RES, REL_HWHEEL, REL_HWHEEL_HI_RES] {
        node.set(ioctl::SET_RELBIT, axis)?;
    }
    for axis in [ABS_X, ABS_Y] {
        node.set(ioctl::SET_ABSBIT, libc::c_int::from(axis))?;
        node.absolute_axis(axis, ABS_RANGE)?;
    }
    node.create(name("pointer absolute", guest).as_str(), Identity::ours(3))?;
    Ok(node)
}

/// The peer's half of a sixteen-bit motor.
///
/// It widens what it is sent by repeating the byte, so this is the exact
/// inverse and a value survives the round trip.
const fn high_byte(value: u16) -> u8 {
    (value >> 8) as u8
}

fn pad(guest: &str, id: u32) -> Result<Node, Error> {
    let node = Node::open()?;
    node.set(ioctl::SET_EVBIT, EV_SYN)?;
    node.set(ioctl::SET_EVBIT, EV_KEY)?;
    node.set(ioctl::SET_EVBIT, EV_ABS)?;
    for code in [
        gamepad::key::SOUTH,
        gamepad::key::EAST,
        gamepad::key::NORTH,
        gamepad::key::WEST,
        gamepad::key::TL,
        gamepad::key::TR,
        gamepad::key::SELECT,
        gamepad::key::START,
        gamepad::key::MODE,
        gamepad::key::THUMBL,
        gamepad::key::THUMBR,
    ] {
        node.set(ioctl::SET_KEYBIT, libc::c_int::from(code))?;
    }
    // **The ranges are the pad's, not ours to choose.** An application reads
    // them and scales its own deadzones by them, so a stick declared over a
    // different span reaches the same code as a different stick.
    for axis in [
        gamepad::axis::X,
        gamepad::axis::Y,
        gamepad::axis::RX,
        gamepad::axis::RY,
    ] {
        node.set(ioctl::SET_ABSBIT, libc::c_int::from(axis))?;
        node.absolute_axis_signed(axis, -gamepad::STICK_RANGE - 1, gamepad::STICK_RANGE)?;
    }
    for axis in [gamepad::axis::Z, gamepad::axis::RZ] {
        node.set(ioctl::SET_ABSBIT, libc::c_int::from(axis))?;
        node.absolute_axis(axis, gamepad::TRIGGER_RANGE)?;
    }
    // The direction pad is two axes that take exactly three values each.
    for axis in [gamepad::axis::HAT0X, gamepad::axis::HAT0Y] {
        node.set(ioctl::SET_ABSBIT, libc::c_int::from(axis))?;
        node.absolute_axis_signed(axis, -1, 1)?;
    }
    // **Declared, or an application sees a pad that cannot rumble.** Only the
    // plain magnitude effect: it is what the common controller libraries
    // raise, and the shaped ones would mean carrying an envelope simulation to
    // produce two numbers a peer can express.
    node.set(ioctl::SET_EVBIT, EV_FF.into())?;
    node.set(ioctl::SET_FFBIT, FF_RUMBLE)?;
    // Which guest and which pad, since the name is a borrowed one and every
    // pad on this machine shares it.
    let mut phys = NameBuf::default();
    let _ = write!(phys, "lowlat/guest{guest}/pad{id}");
    node.set_phys(phys.as_str())?;
    node.create(PAD_NAME, PAD)?;
    Ok(node)
}

/// A device name built without allocating, discarding anything past the
/// kernel's limit.
#[derive(Debug)]
struct NameBuf {
    bytes: [u8; NAME_LEN],
    used: usize,
}

impl Default for NameBuf {
    fn default() -> Self {
        Self {
            bytes: [0; NAME_LEN],
            used: 0,
        }
    }
}

impl NameBuf {
    fn as_str(&self) -> &str {
        self.bytes
            .get(..self.used)
            .and_then(|b| core::str::from_utf8(b).ok())
            .unwrap_or("lowlat")
    }
}

impl core::fmt::Write for NameBuf {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        for byte in text.as_bytes() {
            let Some(slot) = self.bytes.get_mut(self.used) else {
                return Ok(());
            };
            *slot = *byte;
            self.used += 1;
        }
        Ok(())
    }
}

#[cfg(test)]
mod held_tests {
    use super::*;

    fn report(code: u16) -> [Event; 2] {
        [
            Event {
                kind: 1,
                code,
                value: 1,
            },
            Event {
                kind: SYN,
                code: 0,
                value: 0,
            },
        ]
    }

    #[test]
    fn everything_pushed_is_kept_while_there_is_room() {
        let mut held = Held::default();
        for code in 0..8 {
            held.push(&report(code));
        }
        assert_eq!(held.take(), 16);
        assert!(!held.reported);
    }

    /// **The oldest whole report goes, never part of one.** Half a report
    /// reaches a consumer as a state change nobody asked for.
    ///
    /// Reports of three events, deliberately: the queue does not divide by
    /// three, so dropping one event at a time leaves the front of the queue
    /// mid-report instead of cancelling out the way an even size would.
    #[test]
    fn the_bound_discards_whole_reports_from_the_front() {
        let mut held = Held::default();
        let triple = |code: u16| {
            [
                Event {
                    kind: 1,
                    code,
                    value: 1,
                },
                Event {
                    kind: 1,
                    code,
                    value: 0,
                },
                Event {
                    kind: SYN,
                    code: 0,
                    value: 0,
                },
            ]
        };
        // Codes start at one: the report marker carries a code of zero, so a
        // key numbered zero cannot be told from a marker.
        let mut code = 1;
        while held.used + 3 <= HELD {
            held.push(&triple(code));
            code += 1;
        }
        let before = held.used;
        held.push(&triple(9999));
        assert!(held.used <= HELD);
        assert!(held.reported, "an overflow went unreported");

        // Every report in the queue is whole: two keys then the marker, from
        // the very first event to the last.
        let used = held.used;
        let mut keys = 0;
        for event in &held.events[..used] {
            if event.kind == SYN {
                assert_eq!(keys, 2, "a report was cut short at the front");
                keys = 0;
            } else {
                keys += 1;
            }
        }
        assert_eq!(keys, 0, "the queue ends mid-report");
        assert!(held.events[..used].iter().any(|e| e.code == 9999));
        assert!(
            !held.events[..used]
                .iter()
                .any(|e| e.kind == 1 && e.code == 1),
            "the oldest report survived"
        );
        assert!(before >= HELD - 2);
    }

    /// A report longer than the whole queue must not spin or corrupt the
    /// count. It cannot happen from the expander, which is why it is checked.
    #[test]
    fn a_report_longer_than_the_queue_does_not_wedge_it() {
        let mut held = Held::default();
        let long: Vec<Event> = (0..HELD + 10)
            .map(|i| Event {
                kind: 1,
                code: u16::try_from(i % 1000).unwrap(),
                value: 1,
            })
            .collect();
        held.push(&long);
        assert!(held.used <= HELD);
        assert!(held.reported);
    }

    /// **The peer widens a motor by repeating the byte**, so the high byte is
    /// the exact inverse and a value survives the round trip. Taking the low
    /// byte instead is silently wrong at every strength but the ones where
    /// the two halves happen to match.
    #[test]
    fn a_motor_survives_the_round_trip_to_the_peer_and_back() {
        for byte in 0..=u8::MAX {
            let widened = u16::from(byte) | (u16::from(byte) << 8);
            assert_eq!(high_byte(widened), byte);
        }
        assert_eq!(high_byte(0xC000), 0xC0);
        assert_eq!(high_byte(u16::MAX), u8::MAX);
        assert_eq!(high_byte(0), 0);
    }

    #[test]
    fn taking_empties_it() {
        let mut held = Held::default();
        held.push(&report(1));
        assert_eq!(held.take(), 2);
        assert_eq!(held.take(), 0);
    }
}

/// Tests that create real devices.
///
/// **Excluded from the default suite.** They publish devices to the running
/// session and inject into it, so a developer running the suite must not have
/// their keyboard and pointer joined by a second set. Run them deliberately:
///
/// ```text
/// sg input -c "cargo test -p lowlat-inject -- --ignored --test-threads 1"
/// ```
///
/// The group is needed because membership added to a user does not reach a
/// shell that was already running.
#[cfg(test)]
mod device_tests {
    use super::*;
    use crate::event::{Extents, Injector, Permissions};
    use crate::gamepad;
    use lowlat_core::control::{Control, op};
    use std::io::Read as _;

    /// Reads back what the kernel actually delivered, from the event node the
    /// device published.
    ///
    /// **This is the independent half of the check.** Asserting that a write
    /// returned success proves the descriptor accepted bytes and nothing
    /// else; the events are only real once they come back out of the input
    /// layer.
    struct Reader {
        file: std::fs::File,
    }

    impl Reader {
        /// **Polls rather than sleeping a fixed time.** Two delays sit
        /// between creating a device and reading from it, and only the first
        /// is short: the node is published in about 1.4 ms and is owned by
        /// root alone until the device manager grants the group, which takes
        /// 80 to 120 ms here. A fixed sleep either wastes that or races it.
        fn open(name: &str) -> Option<Self> {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            loop {
                if let Some(file) = Self::try_open(name) {
                    use std::os::fd::AsRawFd as _;
                    // SAFETY: a drain with nothing left must return rather
                    // than park the test forever.
                    unsafe {
                        let flags = libc::fcntl(file.as_raw_fd(), libc::F_GETFL);
                        libc::fcntl(file.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK);
                    }
                    return Some(Self { file });
                }
                if std::time::Instant::now() >= deadline {
                    return None;
                }
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        }

        /// Whether the named device still has a node at all.
        fn gone(name: &str) -> bool {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while std::time::Instant::now() < deadline {
                if Self::node(name).is_none() {
                    return true;
                }
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            false
        }

        /// **Pads are found by where they are, not what they are called.**
        /// Every pad claims one borrowed name, so the name identifies the
        /// model and the location identifies the device.
        fn phys(phys: &str) -> Option<String> {
            Self::by(|path| {
                std::fs::read_to_string(path.join("device/phys"))
                    .is_ok_and(|found| found.trim() == phys)
            })
        }

        fn open_phys(location: &str) -> Option<Self> {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            loop {
                if let Some(leaf) = Self::phys(location)
                    && let Ok(file) = std::fs::File::open(format!("/dev/input/{leaf}"))
                {
                    use std::os::fd::AsRawFd as _;
                    // SAFETY: a drain with nothing left must return rather
                    // than park the test forever.
                    unsafe {
                        let flags = libc::fcntl(file.as_raw_fd(), libc::F_GETFL);
                        libc::fcntl(file.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK);
                    }
                    return Some(Self { file });
                }
                if std::time::Instant::now() >= deadline {
                    return None;
                }
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        }

        fn gone_phys(location: &str) -> bool {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while std::time::Instant::now() < deadline {
                if Self::phys(location).is_none() {
                    return true;
                }
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            false
        }

        fn node(name: &str) -> Option<String> {
            Self::by(|path| {
                std::fs::read_to_string(path.join("device/name"))
                    .is_ok_and(|found| found.trim() == name)
            })
        }

        fn by(matches: impl Fn(&std::path::Path) -> bool) -> Option<String> {
            for entry in std::fs::read_dir("/sys/class/input").ok()?.flatten() {
                let path = entry.path();
                let Some(leaf) = path.file_name().and_then(|l| l.to_str()).map(str::to_owned)
                else {
                    continue;
                };
                if !leaf.starts_with("event") {
                    continue;
                }
                if matches(&path) {
                    return Some(leaf);
                }
            }
            None
        }

        fn try_open(name: &str) -> Option<std::fs::File> {
            let leaf = Self::node(name)?;
            std::fs::File::open(format!("/dev/input/{leaf}")).ok()
        }

        /// Everything readable right now, as (type, code, value).
        fn drain(&mut self) -> Vec<(u16, u16, i32)> {
            let mut out = Vec::new();
            let mut bytes = [0u8; size_of::<InputEvent>() * 64];
            while let Ok(read) = self.file.read(&mut bytes) {
                if read == 0 {
                    break;
                }
                for frame in bytes[..read].chunks_exact(size_of::<InputEvent>()) {
                    let at = |o: usize| u16::from_ne_bytes([frame[o], frame[o + 1]]);
                    let value = i32::from_ne_bytes([frame[20], frame[21], frame[22], frame[23]]);
                    out.push((at(16), at(18), value));
                }
            }
            out
        }

        /// Everything readable within a short window, so a test does not race
        /// the kernel's own delivery.
        fn settled(&mut self) -> Vec<(u16, u16, i32)> {
            std::thread::sleep(std::time::Duration::from_millis(30));
            self.drain()
        }

        fn keys(&mut self) -> Vec<(u16, i32)> {
            self.settled()
                .into_iter()
                .filter(|(kind, _, _)| *kind == 0x01)
                .map(|(_, code, value)| (code, value))
                .collect()
        }
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

    /// Wait out the readiness deadline and release what was held.
    ///
    /// **The tests go through the real path rather than around it.** A device
    /// that has not reached its deadline holds everything, and a test that
    /// skipped that would be testing a configuration nothing ships.
    fn ready(devices: &mut Devices) {
        std::thread::sleep(std::time::Duration::from_millis(
            USABLE_AFTER_MS as u64 + 20,
        ));
        devices.tick();
    }

    fn injector() -> Injector {
        Injector::new(Extents::alone(1920, 1080))
    }

    #[test]
    #[ignore = "creates real input devices"]
    fn three_devices_are_created_and_named_per_guest() {
        let devices = Devices::create("7").expect("create");
        for kind in ["keyboard", "pointer", "pointer absolute"] {
            let wanted = format!("lowlat {kind} (guest 7)");
            assert!(Reader::open(&wanted).is_some(), "no node named {wanted}");
        }
        drop(devices);
        assert!(
            Reader::gone("lowlat keyboard (guest 7)"),
            "the device outlived the handle"
        );
    }

    /// **A key written into the device comes back out of the input layer**,
    /// scanned code first, exactly as one from a real keyboard does.
    #[test]
    #[ignore = "creates real input devices"]
    fn a_keystroke_reaches_the_input_layer() {
        let mut devices = Devices::create("8").expect("create");
        ready(&mut devices);
        let mut reader = Reader::open("lowlat keyboard (guest 8)").expect("node");
        reader.drain();

        // F13, which nothing on a desktop binds, so a delivered one is
        // harmless as well as unambiguous.
        let mut inject = injector();
        inject.on_control(&control(op::KEYBOARD, 104, 0, 1), &mut devices);
        inject.on_control(&control(op::KEYBOARD, 104, 0, 0), &mut devices);

        assert_eq!(
            reader.settled(),
            vec![
                (0x04, 4, 0x0007_0068),
                (0x01, 183, 1),
                (0x00, 0, 0),
                (0x04, 4, 0x0007_0068),
                (0x01, 183, 0),
                (0x00, 0, 0),
            ]
        );
    }

    /// Gate item 3, against the kernel rather than against the expander: the
    /// releases a vanishing guest produces really do arrive.
    #[test]
    #[ignore = "creates real input devices"]
    fn a_vanishing_guest_releases_into_the_input_layer() {
        let mut devices = Devices::create("9").expect("create");
        ready(&mut devices);
        let mut reader = Reader::open("lowlat keyboard (guest 9)").expect("node");

        let mut inject = injector();
        for usage in [104, 105, 106] {
            inject.on_control(&control(op::KEYBOARD, usage, 0, 1), &mut devices);
        }
        reader.settled();

        inject.release_all(&mut devices);
        let mut released: Vec<u16> = reader
            .keys()
            .into_iter()
            .filter(|(_, value)| *value == 0)
            .map(|(code, _)| code)
            .collect();
        released.sort_unstable();
        assert_eq!(released, vec![183, 184, 185]);
    }

    /// **The published axis range is what consumers scale by**, and nothing
    /// in the event stream reveals it: the kernel passes a value straight
    /// through whatever range the device declared, so a device set up with
    /// half the range still reports the same numbers and every consumer puts
    /// the pointer at twice the position.
    #[test]
    #[ignore = "creates real input devices"]
    fn the_absolute_axes_are_published_at_the_range_they_are_written_in() {
        let devices = Devices::create("12").expect("create");
        let reader = Reader::open("lowlat pointer absolute (guest 12)").expect("node");
        for (axis, request) in [(ABS_X, 0x8018_4540u64), (ABS_Y, 0x8018_4541u64)] {
            let mut info = UinputAbsSetup {
                code: axis,
                filler: 0,
                value: 0,
                minimum: 0,
                maximum: 0,
                fuzz: 0,
                flat: 0,
                resolution: 0,
            };
            use std::os::fd::AsRawFd as _;
            // SAFETY: the request writes an input_absinfo, and the pointer is
            // offset to the absinfo half of the setup struct, which has that
            // layout.
            let rc = unsafe {
                libc::ioctl(
                    reader.file.as_raw_fd(),
                    request,
                    core::ptr::from_mut(&mut info.value),
                )
            };
            assert_eq!(rc, 0, "axis {axis} could not be read back");
            assert_eq!((info.minimum, info.maximum), (0, ABS_RANGE), "axis {axis}");
        }
        drop(devices);
    }

    /// The absolute pointer reaches the far corner, which is the check that
    /// the scaling and the inclusive extent agree.
    #[test]
    #[ignore = "creates real input devices"]
    fn absolute_motion_reaches_the_far_corner() {
        let mut devices = Devices::create("10").expect("create");
        ready(&mut devices);
        let mut reader = Reader::open("lowlat pointer absolute (guest 10)").expect("node");
        reader.drain();

        let mut inject = injector();
        inject.on_control(&control(op::MOUSE_MOTION, 0, 1920, 1080), &mut devices);

        let absolute: Vec<(u16, i32)> = reader
            .settled()
            .into_iter()
            .filter(|(kind, _, _)| *kind == 0x03)
            .map(|(_, code, value)| (code, value))
            .collect();
        assert_eq!(absolute, vec![(ABS_X, ABS_RANGE), (ABS_Y, ABS_RANGE)]);
    }

    /// **Events produced before the device can deliver are held, not lost.**
    /// This is the whole reason the deadline exists: written into the gap they
    /// are accepted by the kernel and go nowhere at all.
    #[test]
    #[ignore = "creates real input devices"]
    fn events_written_before_the_device_is_usable_are_held_and_then_arrive() {
        let mut devices = Devices::create("13").expect("create");
        let mut reader = Reader::open("lowlat keyboard (guest 13)").expect("node");
        reader.drain();

        let mut inject = injector();
        inject.on_control(&control(op::KEYBOARD, 104, 0, 1), &mut devices);
        inject.on_control(&control(op::KEYBOARD, 104, 0, 0), &mut devices);
        assert!(
            reader.settled().is_empty(),
            "an event reached the device before its deadline"
        );

        ready(&mut devices);
        assert_eq!(reader.keys(), vec![(183, 1), (183, 0)]);
    }

    /// **Does destroying the device release what it holds?** The whole
    /// teardown path turns on this. If the kernel does it, a guest thread can
    /// let the handle drop; if not, every exit path owes an explicit release
    /// and the one that is forgotten is the one that strands a key.
    ///
    /// **Read from a thread that is already draining**, which is what a real
    /// consumer is. A reader that waits and looks afterwards sees nothing at
    /// all: once the device is gone the kernel answers a read with "no such
    /// device" before it looks at what is still buffered, so the releases are
    /// real but unreachable to anything that was not already listening. That
    /// is a property of the test, not of the teardown, and getting it wrong
    /// the first time made a working kernel look broken.
    #[test]
    #[ignore = "creates real input devices"]
    fn destroying_a_device_releases_what_it_holds() {
        let mut devices = Devices::create("14").expect("create");
        ready(&mut devices);
        let leaf = Reader::node("lowlat keyboard (guest 14)").expect("node");

        let mut listening = std::fs::File::open(format!("/dev/input/{leaf}")).expect("open");
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let collector = std::sync::Arc::clone(&seen);
        let reader = std::thread::spawn(move || {
            let mut bytes = [0u8; size_of::<InputEvent>() * 64];
            while let Ok(read) = listening.read(&mut bytes) {
                if read == 0 {
                    break;
                }
                let mut got = collector.lock().unwrap();
                for frame in bytes[..read].chunks_exact(size_of::<InputEvent>()) {
                    let kind = u16::from_ne_bytes([frame[16], frame[17]]);
                    let code = u16::from_ne_bytes([frame[18], frame[19]]);
                    let value = i32::from_ne_bytes([frame[20], frame[21], frame[22], frame[23]]);
                    got.push((kind, code, value));
                }
            }
        });

        let mut inject = injector();
        for usage in [104, 105] {
            inject.on_control(&control(op::KEYBOARD, usage, 0, 1), &mut devices);
        }
        std::thread::sleep(std::time::Duration::from_millis(60));

        // No release_all: the handle simply goes away, as it does when a
        // guest thread returns.
        drop(devices);
        reader.join().expect("reader");

        let got = seen.lock().unwrap();
        let pressed: Vec<u16> = got
            .iter()
            .filter(|(kind, _, value)| *kind == 1 && *value == 1)
            .map(|(_, code, _)| *code)
            .collect();
        let mut released: Vec<u16> = got
            .iter()
            .filter(|(kind, _, value)| *kind == 1 && *value == 0)
            .map(|(_, code, _)| *code)
            .collect();
        released.sort_unstable();
        assert_eq!(pressed, vec![183, 184]);
        assert_eq!(
            released,
            vec![183, 184],
            "the kernel did not release what the device held"
        );
    }

    /// **A pad appears when the guest first sends one, and reports.** Read
    /// back through the event node, so what is asserted is what the input
    /// layer delivered rather than what the write returned.
    #[test]
    #[ignore = "creates real input devices"]
    fn a_pad_appears_on_first_use_and_reports() {
        let mut devices = Devices::create("15").expect("create");
        assert!(
            Reader::phys("lowlat/guest15/pad9").is_none(),
            "a pad existed before the guest sent one"
        );

        let mut inject = injector();
        let mut body = vec![0xAB, 0xCD, 0xEF];
        body.extend_from_slice(&gamepad::bit::A.to_be_bytes());
        for value in [1000i16, 2000, -3000, -4000] {
            body.extend_from_slice(&value.to_be_bytes());
        }
        body.push(200);
        body.push(50);
        inject.on_control(
            &Control {
                a0: 9,
                a1: 0,
                a2: 0,
                opcode: op::GAMEPAD_STATE,
                body: &body,
            },
            &mut devices,
        );

        let mut reader = Reader::open_phys("lowlat/guest15/pad9").expect("the pad has no node");
        // The first report was held while the device became usable, so it
        // arrives when the deadline passes rather than now.
        std::thread::sleep(std::time::Duration::from_millis(
            USABLE_AFTER_MS as u64 + 20,
        ));
        devices.tick();

        let seen = reader.settled();
        let keys: Vec<(u16, i32)> = seen
            .iter()
            .filter(|(kind, _, _)| *kind == 0x01)
            .map(|(_, code, value)| (*code, *value))
            .collect();
        let axes: Vec<(u16, i32)> = seen
            .iter()
            .filter(|(kind, _, _)| *kind == 0x03)
            .map(|(_, code, value)| (*code, *value))
            .collect();
        assert_eq!(keys, vec![(gamepad::key::SOUTH, 1)]);
        assert_eq!(
            axes,
            vec![
                (gamepad::axis::X, 1000),
                (gamepad::axis::Y, -2000),
                (gamepad::axis::RX, -3000),
                (gamepad::axis::RY, 4000),
                (gamepad::axis::Z, 200),
                (gamepad::axis::RZ, 50),
            ]
        );

        // And it goes away when the guest says so.
        inject.on_control(
            &Control {
                a0: 0,
                a1: 0,
                a2: 9,
                opcode: op::GAMEPAD_UNPLUG,
                body: &[],
            },
            &mut devices,
        );
        assert!(Reader::gone_phys("lowlat/guest15/pad9"), "the pad stayed");
    }

    /// **The identity is what every consumer maps the buttons by**, so it is
    /// read back from the device rather than assumed from the constant that
    /// was written. A pad whose identity did not take is delivered as numbered
    /// buttons and is unusable however correct the input is.
    #[test]
    #[ignore = "creates real input devices"]
    fn a_pad_presents_the_identity_its_mapping_is_keyed_on() {
        let mut devices = Devices::create("18").expect("create");
        let mut inject = injector();
        inject.on_control(
            &Control {
                a0: 4,
                a1: 0,
                a2: 2,
                opcode: op::GAMEPAD_AXIS,
                body: &[],
            },
            &mut devices,
        );
        let reader = Reader::open_phys("lowlat/guest18/pad2").expect("the pad has no node");

        let mut id = [0u16; 4];
        use std::os::fd::AsRawFd as _;
        // EVIOCGID, which is _IOR('E', 0x02, struct input_id).
        // SAFETY: the request writes four u16, which is what is passed.
        let rc = unsafe {
            libc::ioctl(
                reader.file.as_raw_fd(),
                0x8008_4502u64,
                id.as_mut_ptr().cast::<libc::c_void>(),
            )
        };
        assert_eq!(rc, 0, "the identity could not be read back");
        // **Written out rather than compared against the constant**, because
        // a comparison against the constant scales with it and would accept
        // any identity at all. All four numbers are part of the key a mapping
        // is looked up by, the version included: change it and the lookup
        // finds a different entry or none.
        assert_eq!(
            id,
            [0x0003, 0x045e, 0x028e, 0x0114],
            "the pad does not present the identity its mapping is keyed on"
        );
        assert_eq!(id, [PAD.bus, PAD.vendor, PAD.product, PAD.version]);

        // And it is still tellable from another guest's, which is the half the
        // borrowed name gives up.
        let leaf = Reader::phys("lowlat/guest18/pad2").expect("node");
        let name =
            std::fs::read_to_string(format!("/sys/class/input/{leaf}/device/name")).expect("name");
        assert_eq!(name.trim(), PAD_NAME);
    }

    /// **The axis ranges are what an application scales its deadzones by**,
    /// and nothing in the event stream reveals them.
    #[test]
    #[ignore = "creates real input devices"]
    fn a_pads_axes_are_published_at_the_ranges_they_are_written_in() {
        let mut devices = Devices::create("16").expect("create");
        let mut inject = injector();
        inject.on_control(
            &Control {
                a0: 4,
                a1: 0,
                a2: 1,
                opcode: op::GAMEPAD_AXIS,
                body: &[],
            },
            &mut devices,
        );
        let reader = Reader::open_phys("lowlat/guest16/pad1").expect("the pad has no node");

        let wanted = [
            (gamepad::axis::X, -32768, 32767),
            (gamepad::axis::Y, -32768, 32767),
            (gamepad::axis::Z, 0, 255),
            (gamepad::axis::RZ, 0, 255),
            (gamepad::axis::HAT0X, -1, 1),
            (gamepad::axis::HAT0Y, -1, 1),
        ];
        for (axis, minimum, maximum) in wanted {
            let mut info = UinputAbsSetup {
                code: axis,
                filler: 0,
                value: 0,
                minimum: 0,
                maximum: 0,
                fuzz: 0,
                flat: 0,
                resolution: 0,
            };
            use std::os::fd::AsRawFd as _;
            // SAFETY: the request writes an input_absinfo, and the pointer is
            // offset to the absinfo half of the setup struct.
            let rc = unsafe {
                libc::ioctl(
                    reader.file.as_raw_fd(),
                    0x8018_4540u64 + u64::from(axis),
                    core::ptr::from_mut(&mut info.value),
                )
            };
            assert_eq!(rc, 0, "axis {axis:#x} could not be read back");
            assert_eq!(
                (info.minimum, info.maximum),
                (minimum, maximum),
                "{axis:#x}"
            );
        }
    }

    /// **An application's rumble reaches the peer.** Driven the way a real one
    /// does it: upload an effect to the event node with `EVIOCSFF`, then play
    /// it by writing an `EV_FF` event at the identifier the kernel assigned.
    /// Nothing here simulates the kernel; it is the kernel doing the asking.
    ///
    /// **The application has to be on its own thread**, and that is not a
    /// tidiness point. The upload blocks inside the kernel until the device's
    /// creator answers it, so a test that uploads and then looks for the
    /// answer deadlocks against itself -- measured at thirty seconds, the
    /// kernel's own patience. In a real session the guest's loop is turning
    /// while some other process uploads, which is what this reproduces.
    #[test]
    #[ignore = "creates real input devices"]
    fn an_applications_rumble_reaches_the_peer() {
        use std::io::Write as _;
        use std::os::fd::AsRawFd as _;

        let mut devices = Devices::create("17").expect("create");
        let mut inject = injector();
        // Any pad event creates the device.
        inject.on_control(
            &Control {
                a0: 0,
                a1: 0,
                a2: 1,
                opcode: op::GAMEPAD_AXIS,
                body: &[],
            },
            &mut devices,
        );
        let leaf = Reader::phys("lowlat/guest17/pad1").expect("the pad has no node");
        // The node is root-owned until the device manager grants the group.
        let _wait = Reader::open_phys("lowlat/guest17/pad1").expect("node");

        let application = std::thread::spawn(move || {
            let mut node = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(format!("/dev/input/{leaf}"))
                .expect("open for writing");
            let mut effect = FfEffect {
                kind: 0x50,
                id: -1, // let the kernel assign one
                strong: 0xC000,
                weak: 0x4000,
                ..FfEffect::default()
            };
            // EVIOCSFF, which is _IOW('E', 0x80, struct ff_effect).
            const EVIOCSFF: libc::c_ulong = 0x4030_4580;
            // SAFETY: the request reads a ff_effect, which is what is passed.
            let rc = unsafe {
                libc::ioctl(node.as_raw_fd(), EVIOCSFF, core::ptr::from_mut(&mut effect))
            };
            assert_eq!(rc, 0, "the kernel refused the effect");

            let mut play = |value: i32| {
                let mut frame = [0u8; size_of::<InputEvent>()];
                frame[16..18].copy_from_slice(&0x15u16.to_ne_bytes());
                #[allow(clippy::cast_sign_loss)]
                frame[18..20].copy_from_slice(&(effect.id as u16).to_ne_bytes());
                frame[20..24].copy_from_slice(&value.to_ne_bytes());
                node.write_all(&frame).expect("play");
            };
            play(1);
            std::thread::sleep(std::time::Duration::from_millis(200));
            play(0);
            std::thread::sleep(std::time::Duration::from_millis(200));
        });

        let mut reported = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            while let Some(rumble) = devices.rumble() {
                reported.push(rumble);
            }
            if application.is_finished() && reported.len() >= 2 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        application.join().expect("application");
        while let Some(rumble) = devices.rumble() {
            reported.push(rumble);
        }

        assert_eq!(
            reported,
            vec![
                Rumble {
                    pad: 1,
                    large: 0xC0,
                    small: 0x40
                },
                Rumble {
                    pad: 1,
                    large: 0,
                    small: 0
                },
            ],
            "the effect did not come back as the peer would receive it"
        );
    }

    /// Gate item 5 against the kernel: revoking releases what it held.
    #[test]
    #[ignore = "creates real input devices"]
    fn revoking_a_permission_releases_into_the_input_layer() {
        let mut devices = Devices::create("11").expect("create");
        ready(&mut devices);
        let mut reader = Reader::open("lowlat keyboard (guest 11)").expect("node");

        let mut inject = injector();
        inject.on_control(&control(op::KEYBOARD, 104, 0, 1), &mut devices);
        reader.settled();

        inject.set_permissions(
            Permissions {
                keyboard: false,
                ..Permissions::default()
            },
            &mut devices,
        );
        assert_eq!(reader.keys(), vec![(183, 0)]);
    }
}
