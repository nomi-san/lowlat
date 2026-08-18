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
}

const EV_SYN: libc::c_int = 0x00;
/// The report marker, as the event stream carries it.
const SYN: u16 = 0x00;
const EV_KEY: libc::c_int = 0x01;
const EV_REL: libc::c_int = 0x02;
const EV_ABS: libc::c_int = 0x03;
const EV_MSC: libc::c_int = 0x04;

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

/// Not a real vendor's number. Distinctive so these devices are greppable in
/// a device listing, and on a bus where nothing can collide with it.
const VENDOR: u16 = 0x6c6c;
const VERSION: u16 = 1;

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
    fn open() -> Result<Self, Error> {
        // SAFETY: a constant NUL-terminated path and flags the call defines.
        let raw = unsafe {
            libc::open(
                c"/dev/uinput".as_ptr(),
                libc::O_WRONLY | libc::O_CLOEXEC | libc::O_NONBLOCK,
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

    fn create(&self, name: &str, product: u16) -> Result<(), Error> {
        let mut setup = UinputSetup {
            bustype: BUS_VIRTUAL,
            vendor: VENDOR,
            product,
            version: VERSION,
            name: [0; NAME_LEN],
            ff_effects_max: 0,
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
        let setup = UinputAbsSetup {
            code,
            filler: 0,
            value: 0,
            minimum: 0,
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

    /// Write everything held, if there is anything.
    fn flush(&mut self) -> Result<(), Error> {
        let used = self.held.take();
        match self.held.events.get(..used) {
            Some(events) if used > 0 => write_to(&self.fd, events),
            _ => Ok(()),
        }
    }

    fn write(&self, events: &[Event]) -> Result<(), Error> {
        write_to(&self.fd, events)
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
    created: lowlat_common::clock::Time,
    usable: bool,
}

impl Devices {
    /// Create all three, or none.
    ///
    /// **Named per guest** so a device listing says which session a device
    /// belongs to, which is the first question asked of one that is behaving
    /// oddly.
    pub fn create(guest: u32) -> Result<Self, Error> {
        Ok(Self {
            keyboard: keyboard(guest)?,
            pointer: pointer(guest)?,
            pointer_absolute: pointer_absolute(guest)?,
            created: lowlat_common::clock::Time::now(),
            usable: false,
        })
    }

    /// Release anything held once the devices can deliver it.
    ///
    /// **Called from the loop that already runs, not only on the next event.**
    /// A guest that types one key and waits would otherwise have it sit in the
    /// queue until it typed a second.
    pub fn tick(&mut self) {
        if self.usable {
            return;
        }
        if lowlat_common::clock::elapsed_ms(self.created) < USABLE_AFTER_MS {
            return;
        }
        self.usable = true;
        for node in [
            &mut self.keyboard,
            &mut self.pointer,
            &mut self.pointer_absolute,
        ] {
            if let Err(error) = node.flush() {
                lowlat_common::log_warn!("inject: held events lost, error={error}");
            }
        }
    }
}

impl Sink for Devices {
    fn emit(&mut self, device: Device, events: &[Event]) {
        self.tick();
        let usable = self.usable;
        let node = match device {
            Device::Keyboard => &mut self.keyboard,
            Device::Pointer => &mut self.pointer,
            Device::PointerAbsolute => &mut self.pointer_absolute,
        };
        if !usable {
            node.held.push(events);
            return;
        }
        if let Err(error) = node.write(events) {
            // A write that fails is not recoverable here and must not stop the
            // session: the guest is still connected and still being sent
            // video. It is reported and the events are lost.
            lowlat_common::log_warn!("inject: write failed, device={device:?} error={error}");
        }
    }
}

fn name(kind: &str, guest: u32) -> NameBuf {
    let mut buf = NameBuf::default();
    // The write cannot fail: the buffer discards past its capacity.
    let _ = write!(buf, "lowlat {kind} (guest {guest})");
    buf
}

fn keyboard(guest: u32) -> Result<Node, Error> {
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
    node.create(name("keyboard", guest).as_str(), 1)?;
    Ok(node)
}

fn pointer(guest: u32) -> Result<Node, Error> {
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
    node.create(name("pointer", guest).as_str(), 2)?;
    Ok(node)
}

fn pointer_absolute(guest: u32) -> Result<Node, Error> {
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
    node.create(name("pointer absolute", guest).as_str(), 3)?;
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

        fn node(name: &str) -> Option<String> {
            for entry in std::fs::read_dir("/sys/class/input").ok()?.flatten() {
                let path = entry.path();
                let Some(leaf) = path.file_name().and_then(|l| l.to_str()).map(str::to_owned)
                else {
                    continue;
                };
                if !leaf.starts_with("event") {
                    continue;
                }
                let Ok(found) = std::fs::read_to_string(path.join("device/name")) else {
                    continue;
                };
                if found.trim() == name {
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
        Injector::new(Extents {
            width: 1920,
            height: 1080,
        })
    }

    #[test]
    #[ignore = "creates real input devices"]
    fn three_devices_are_created_and_named_per_guest() {
        let devices = Devices::create(7).expect("create");
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
        let mut devices = Devices::create(8).expect("create");
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
        let mut devices = Devices::create(9).expect("create");
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
        let devices = Devices::create(12).expect("create");
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
        let mut devices = Devices::create(10).expect("create");
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
        let mut devices = Devices::create(13).expect("create");
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

    /// Gate item 5 against the kernel: revoking releases what it held.
    #[test]
    #[ignore = "creates real input devices"]
    fn revoking_a_permission_releases_into_the_input_layer() {
        let mut devices = Devices::create(11).expect("create");
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
