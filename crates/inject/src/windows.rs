//! The devices a guest's input lands on, on Windows (docs/05-host.md section
//! 7): the system's batch input call for the keyboard and the pointer, by
//! scancode, and an Xbox 360 pad on the ViGEm bus for each of the guest's
//! pads, spoken to over the bus's own requests.
//!
//! **The calling thread's state is part of the interface.** The input call
//! reaches only the desktop that receives input, which the lock screen and an
//! elevation prompt replace with another, and refuses a thread on any other;
//! so the guest's thread is attached to the input desktop as its devices are
//! made, and again whenever the call refuses, and the refused input goes
//! again at once. The thread is made per-monitor aware as well, so a position
//! is spread over the desktop in the physical pixels the captured output is
//! measured in. Both belong to the guest's thread, which makes and drops its
//! devices, and are put back as they go.
//!
//! **The two pointers are one mouse here**: the call has one, which takes
//! deltas and positions alike.
//!
//! **Where the application switches it on** (docs/06-api.md section 3), the
//! guest's keys and pointer go to a keyboard and a mouse of the host's own
//! input driver instead (docs/11-drivers.md section 2), which the system takes
//! as devices': made as the switch goes on, each one's input held until the
//! system lists it, and the batch input call again wherever the driver gives
//! none or its device goes. **The keyboard never types the attention chord**
//! (docs/05-host.md section 7.3): a Delete going down while Ctrl and Alt are
//! held goes through the batch input call, which raises no secure screen. **A
//! position off the primary display goes through the batch input call** too:
//! the mouse's absolute collection spans the primary alone.
//!
//! **Where the owner sets `virtual_pads`**, a DualShock 4 or a DualSense a
//! guest sends as its own reports is presented as that product on the same
//! driver, and what a program writes to it goes back to the guest; a pad whose
//! device never comes, or goes, plays on the bus from its own reports.

use lowlat_core::pad::{self, Feature, Inbound, Product};
use lowlat_hid_proto::control::{self, Features, WrittenKind};
use lowlat_hid_proto::{Kind, keyboard, mouse};
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    CM_GET_DEVICE_INTERFACE_LIST_PRESENT, CM_Get_Device_Interface_List_SizeW,
    CM_Get_Device_Interface_ListW, CR_SUCCESS,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_BUSY, ERROR_INVALID_PARAMETER, ERROR_IO_INCOMPLETE,
    ERROR_IO_PENDING, GENERIC_ALL, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
    POINT, RECT,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::{
    CancelIoEx, DeviceIoControl, GetOverlappedResult, OVERLAPPED,
};
use windows_sys::Win32::System::StationsAndDesktops::{
    CloseDesktop, HDESK, OpenInputDesktop, SetThreadDesktop,
};
use windows_sys::Win32::System::Threading::CreateEventW;
use windows_sys::Win32::UI::Controls::{
    HSYNTHETICPOINTERDEVICE, POINTER_FEEDBACK_DEFAULT, POINTER_FEEDBACK_MODE, POINTER_TYPE_INFO,
    POINTER_TYPE_INFO_0,
};
use windows_sys::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetThreadDpiAwarenessContext,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, GetKeyState, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSEEVENTF_ABSOLUTE,
    MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN,
    MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
    MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT,
    SendInput, VK_CAPITAL, VK_CONTROL, VK_MENU, VK_NUMLOCK, VK_PAUSE,
};
use windows_sys::Win32::UI::Input::Pointer::{
    POINTER_FLAG_DOWN, POINTER_FLAG_INCONTACT, POINTER_FLAG_INRANGE, POINTER_FLAG_UP,
    POINTER_FLAG_UPDATE, POINTER_FLAGS, POINTER_INFO, POINTER_PEN_INFO, POINTER_TOUCH_INFO,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, PEN_FLAG_BARREL, PEN_FLAG_ERASER, PEN_FLAG_INVERTED, PEN_MASK_PRESSURE,
    PEN_MASK_ROTATION, PEN_MASK_TILT_X, PEN_MASK_TILT_Y, POINTER_INPUT_TYPE, PT_PEN, PT_TOUCH,
    SM_CXSCREEN, SM_CXVIRTUALSCREEN, SM_CYSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
    SM_YVIRTUALSCREEN, TOUCH_FLAG_NONE, TOUCH_MASK_CONTACTAREA, TOUCH_MASK_NONE,
    TOUCH_MASK_ORIENTATION, TOUCH_MASK_PRESSURE, XBUTTON1, XBUTTON2,
};

use crate::answers;
use crate::event::{self, Device, Event, Locks, SCRATCH, Sink, TOUCH_SLOTS};
use crate::gamepad::{self, MAX_PADS};
use crate::handback::{Forward, Forwarding, PadWritten, Rumble, WRITTEN_MAX, Written};
use crate::hid;
use crate::scancode::{MAKE_ONLY, PAUSE, SET1, SYSRQ};
use crate::usage::{self, HidKey};
use crate::vigem;

/// Why a guest's devices could not be created: never, here. The input call
/// needs nothing made for it, and the pad bus is opened by the first pad.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {}

impl core::fmt::Display for Error {
    fn fmt(&self, _f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match *self {}
    }
}

impl std::error::Error for Error {}

/// The identity a pad on the bus claims: the bus's own default, the wired
/// Xbox 360 pad every consumer of a pad recognises.
const PAD_VENDOR: u16 = 0x045e;
const PAD_PRODUCT: u16 = 0x028e;

/// The bus numbers its pads; a number another program holds is refused, so
/// the next is asked for. This many are tried before a pad is given up.
const SERIALS: u32 = 64;

/// The modifiers' own codes, from the table.
const LCTRL: u8 = SET1[224];
const RCTRL: u8 = SET1[228];
const LALT: u8 = SET1[226];
const RALT: u8 = SET1[230];

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    reason = "one input is a few dozen bytes"
)]
const INPUT_SIZE: i32 = size_of::<INPUT>() as i32;

/// How long the input driver's keyboard may take to deliver before the keys
/// held for it go through the batch input call instead. The system lists a
/// device within milliseconds, and within half a second when it comes back on
/// a name just let go; a slot's first setup is the longest wait.
const DELIVERS_WITHIN_MS: f64 = 5_000.0;

/// Events held for one of the input driver's devices while it cannot deliver
/// yet: a person types a few keys in the wait, or moves the pointer a few
/// dozen times at three events a move, and a slot costs eight bytes.
const HELD: usize = 256;

/// A request body's size, as the bus is told it.
#[allow(
    clippy::cast_possible_truncation,
    reason = "a request body is a few dozen bytes"
)]
const fn size<T>() -> u32 {
    size_of::<T>() as u32
}

/// One guest's devices.
///
/// **Fields drop in the order written**: the pads first, whose requests to
/// the bus are cancelled and waited for while it is open, then the bus,
/// whose closing unplugs anything left; the thread's own state last.
pub struct Devices {
    pads: [Option<Pad>; MAX_PADS],
    bus: Option<Bus>,
    /// Pads are off for this guest: there is no bus, or it refused one.
    /// Said once.
    no_pads: bool,
    /// Set once, so a guest holding more pads than there are slots is said
    /// rather than silently short of one.
    refused: bool,
    forward: Option<Forwarding>,
    keyboard: Keyboard,
    /// The application's switch: the keys go to the input driver's keyboard
    /// while it is on.
    virtual_input: bool,
    /// That keyboard, while the switch is on and the driver gave one.
    hid: Option<HidKeyboard>,
    /// What the keyboard was sent while that one could not deliver yet.
    held: Held,
    /// The driver's mouse, as its keyboard is.
    mouse: Option<HidMouse>,
    /// What the pointer was sent while that one could not deliver yet.
    mouse_held: Held,
    /// The owner's setting: a pad sent as its reports is presented as its
    /// product on the input driver while it is on.
    virtual_pads: bool,
    /// The driver gave a pad none, so the pads plugged after it are played
    /// from their whole states, until the setting is asked again.
    no_hid_pads: bool,
    /// The pads sent as their reports, on the input driver or, where it gave
    /// none, on the bus from their reports.
    hid_pads: [Option<HidPad>; MAX_PADS],
    /// The middle of every such pad's address: this guest's.
    middle: [u8; 3],
    /// Whether this guest may press Guide, as the expander says: a pad played
    /// on the bus from its own reports leaves it out where not.
    guide: bool,
    /// The input call is refusing, so the next refusal is not said again.
    refusing: bool,
    /// The guest's pen, on a synthetic pen of the system's own.
    pen: Pen,
    /// The guest's touch, on a synthetic touch surface of the system's own.
    touch: Touches,
    desktop: Option<Desktop>,
    /// The application the owner holds the host's guests to, judged here as
    /// input arrives, where one is set.
    scope: Option<crate::scope::Judge>,
    /// The system's clip of the pointer this guest set, keeping its motion by
    /// deltas on the application's window.
    clip: crate::scope::Clip,
    /// The application's windows this guest's Tabs under one Alt step through.
    switch: crate::scope::Switch,
    _aware: Aware,
}

impl core::fmt::Debug for Devices {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Devices")
            .field("pads", &self.pads.iter().flatten().count())
            .field("bus", &self.bus.is_some())
            .field("forward", &self.forward.is_some())
            .field("virtual_input", &self.virtual_input)
            .field("hid", &self.hid)
            .field("mouse", &self.mouse)
            .field("virtual_pads", &self.virtual_pads)
            .field("hid_pads", &self.hid_pads.iter().flatten().count())
            .field("pen", &self.pen.device.is_some())
            .field("touch", &self.touch.device.is_some())
            .finish_non_exhaustive()
    }
}

impl Devices {
    /// Make a guest's devices, on the guest's own thread: the thread made
    /// per-monitor aware and attached to the input desktop as it is now.
    pub fn create(guest: &str) -> Result<Self, Error> {
        let aware = Aware::per_monitor();
        let desktop = Desktop::attach();
        if desktop.is_none() {
            lowlat_common::log_warn!(
                "inject: the input desktop could not be attached, error={}",
                last_error()
            );
        }
        Ok(Self {
            pads: [const { None }; MAX_PADS],
            bus: None,
            no_pads: false,
            refused: false,
            forward: None,
            keyboard: Keyboard::default(),
            virtual_input: false,
            hid: None,
            held: Held::default(),
            mouse: None,
            mouse_held: Held::default(),
            virtual_pads: false,
            no_hid_pads: false,
            hid_pads: [const { None }; MAX_PADS],
            middle: answers::middle(guest),
            guide: false,
            refusing: false,
            pen: Pen::default(),
            touch: Touches::default(),
            desktop,
            scope: None,
            clip: crate::scope::Clip::default(),
            switch: crate::scope::Switch::default(),
            _aware: aware,
        })
    }

    /// Hold this guest to one application, or to none (docs/05-host.md
    /// section 7.4) -- the owner as well. What the application had been asked
    /// here is forgotten with it, and the clip this guest set let go of.
    pub fn set_scope(&mut self, scope: Option<std::sync::Arc<crate::Scope>>) {
        self.clip.release();
        self.switch = crate::scope::Switch::default();
        self.scope = scope.map(crate::scope::Judge::new);
    }

    /// The pointer kept on the application's window the picture shows, before
    /// motion by deltas goes in; let go of where no window of it is in front.
    fn confine(&mut self) {
        let window = self
            .scope
            .as_mut()
            .and_then(crate::scope::Judge::shown)
            .and_then(crate::scope::on_its_display);
        match window {
            Some(window) => self.clip.hold(window),
            None => self.clip.release(),
        }
    }

    /// Move the guest's keys and pointer to a keyboard and a mouse of the
    /// host's own input driver, or back to the batch input call
    /// (docs/06-api.md section 3). **What the old way holds is the caller's to
    /// release first**: the expander knows what is held, and releases it the
    /// way it went down.
    ///
    /// On, both are made now and each one's input held until the system lists
    /// it; with no driver, or none admitting this process, the batch input call
    /// stays, said once. Off, both are let go of off this thread: a device's
    /// end waits while its neutral reports arrive, which the guest's loop is
    /// not held up by. Asking for what is already so changes nothing.
    pub fn set_virtual_input(&mut self, on: bool) {
        if on == self.virtual_input {
            return;
        }
        self.virtual_input = on;
        if on {
            self.hid = HidKeyboard::make();
            self.mouse = HidMouse::make();
        } else {
            // What waited for those devices never reached anything, and
            // nothing of it is held.
            self.held = Held::default();
            self.mouse_held = Held::default();
            if let Some(hid) = self.hid.take() {
                hid.close_aside();
            }
            if let Some(mouse) = self.mouse.take() {
                mouse.close_aside();
            }
        }
    }

    /// The keys on the input driver's keyboard: each key's report written as
    /// its report ends, a Delete going down under Ctrl and Alt through the
    /// batch input call instead. A keyboard that refuses a report is taken to
    /// be gone -- its end releases what it held -- and that report and the rest
    /// go through the batch input call.
    fn keys_to_hid(&mut self, events: &[Event]) {
        let Some(mut hid) = self.hid.take() else {
            self.keys_to_batch(events);
            return;
        };
        let mut lost = None;
        // Where the report being built began: what it holds reached nothing
        // until its end is written.
        let mut report = 0;
        for (at, event) in events.iter().enumerate() {
            let written = match event.kind {
                event::EV_KEY => {
                    let down = event.value != 0;
                    match usage::hid_key(event.code) {
                        Some(HidKey::Key(key)) => {
                            match hid.keys.key(key, down, &mut || self.system_ctrl_alt()) {
                                Step::Hid | Step::Nothing => Ok(()),
                                Step::Batch => {
                                    let code = u8::try_from(event.code).unwrap_or(0);
                                    self.send(&[key_input(Stroke::Scan(code), down)]);
                                    Ok(())
                                }
                                Step::ReleasedFirst => hid.write_keys().map(|()| {
                                    hid.keys.set(key, down);
                                }),
                            }
                        }
                        Some(HidKey::Consumer(key)) => {
                            hid.keys.consumer(key, down);
                            Ok(())
                        }
                        None => Ok(()),
                    }
                }
                event::EV_SYN => hid.flush().map(|()| report = at + 1),
                _ => Ok(()),
            };
            if let Err(error) = written {
                lost = Some(error);
                break;
            }
        }
        match lost {
            None => self.hid = Some(hid),
            Some(error) => {
                lowlat_common::log_warn!(
                    "inject: the input driver's keyboard is gone, keys go through the batch input call, slot={} error={error}",
                    hid.slot
                );
                hid.close_aside();
                self.keys_to_batch(events.get(report..).unwrap_or_default());
            }
        }
    }

    /// The keys through the batch input call, by scancode.
    fn keys_to_batch(&mut self, events: &[Event]) {
        let mut inputs = Inputs::new();
        for event in events.iter().filter(|e| e.kind == event::EV_KEY) {
            let down = event.value != 0;
            if let Some(stroke) = self.keyboard.stroke(event.code, down) {
                inputs.push(key_input(stroke, down));
            }
        }
        self.send(inputs.as_slice());
    }

    /// The pointer on the input driver's mouse: every button, wheel and motion
    /// on its relative collection, a position on the primary display on its
    /// absolute one, and a position on any other display through the batch
    /// input call, the absolute collection spanning the primary alone. A mouse
    /// that refuses a report is taken to be gone -- its end releases what it
    /// held -- and that report and the rest go through the batch input call.
    fn pointer_to_hid(&mut self, events: &[Event]) {
        let Some(mut mouse) = self.mouse.take() else {
            self.pointer_to_batch(events);
            return;
        };
        let mut lost = None;
        // Where the report being read began: what it holds reached nothing
        // until its end is written.
        let mut report = 0;
        for (at, event) in events.iter().enumerate() {
            mouse.pointer.take(event);
            if event.kind != event::EV_SYN {
                continue;
            }
            let device = &mut mouse.device;
            let written = mouse.pointer.ended(primary_place, |pointed| match pointed {
                Pointed::Relative(report) => device.write(&report),
                Pointed::Absolute(report) => device.write(&report),
                Pointed::Elsewhere(x, y) => {
                    self.send(&[mouse_input(x, y, 0, ABSOLUTE)]);
                    Ok(())
                }
            });
            if let Err(error) = written {
                lost = Some(error);
                break;
            }
            report = at + 1;
        }
        match lost {
            None => self.mouse = Some(mouse),
            Some(error) => {
                lowlat_common::log_warn!(
                    "inject: the input driver's mouse is gone, the pointer goes through the batch input call, slot={} error={error}",
                    mouse.slot
                );
                mouse.close_aside();
                self.pointer_to_batch(events.get(report..).unwrap_or_default());
            }
        }
    }

    /// The pointer through the batch input call.
    fn pointer_to_batch(&mut self, events: &[Event]) {
        let mut inputs = Inputs::new();
        pointer_inputs(events, &mut inputs);
        self.send(inputs.as_slice());
    }

    /// Whether Ctrl and Alt are held anywhere on the machine, as the system's
    /// own key state says -- another guest's keyboard, the desk's, this one's.
    ///
    /// **Read on the input desktop**, which alone the state answers for, so
    /// the thread is attached to it first: on the driver's path nothing else
    /// moves it there. Where it cannot be, both count as held, so a Delete goes
    /// the way that raises nothing.
    fn system_ctrl_alt(&mut self) -> (bool, bool) {
        let Some(desktop) = Desktop::attach() else {
            return (true, true);
        };
        self.desktop = Some(desktop);
        // SAFETY: two reads of a key's state, with keys the system defines;
        // the high bit is the key down.
        let held = |key: u16| unsafe { GetAsyncKeyState(i32::from(key)) } < 0;
        (held(VK_CONTROL), held(VK_MENU))
    }

    /// The input driver's keyboard, each pass: the keys held for it typed once
    /// the system lists it, or through the batch input call once it is past
    /// its time; and a Delete held on it let go of as Ctrl and Alt arrive on
    /// another keyboard.
    fn tick_keyboard(&mut self) {
        let Some(mut hid) = self.hid.take() else {
            return;
        };
        if !hid.ready {
            let after_ms = lowlat_common::clock::elapsed_ms(hid.made);
            if hid.device.listed() {
                hid.ready = true;
                lowlat_common::log_info!(
                    "inject: the input driver's keyboard delivers, slot={} after_ms={after_ms:.0}",
                    hid.slot
                );
                self.hid = Some(hid);
                let held = core::mem::take(&mut self.held);
                self.keys_to_hid(held.events());
            } else if after_ms >= DELIVERS_WITHIN_MS {
                lowlat_common::log_warn!(
                    "inject: the input driver's keyboard never delivered, keys go through the batch input call, slot={} after_ms={after_ms:.0}",
                    hid.slot
                );
                hid.close_aside();
                let held = core::mem::take(&mut self.held);
                self.keys_to_batch(held.events());
            } else {
                self.hid = Some(hid);
            }
            return;
        }
        // **The other order, from another keyboard**: nothing tells this one
        // when Ctrl and Alt arrive there, so it asks while a Delete is held,
        // before the keyboard's own repeat types it again under them.
        if hid.keys.holds_delete() {
            let system = self.system_ctrl_alt();
            if hid.keys.let_go_over(system)
                && let Err(error) = hid.write_keys()
            {
                lowlat_common::log_warn!(
                    "inject: the input driver's keyboard is gone, keys go through the batch input call, slot={} error={error}",
                    hid.slot
                );
                hid.close_aside();
                return;
            }
        }
        self.hid = Some(hid);
    }

    /// The input driver's mouse, each pass: what was held for it sent once the
    /// system lists it, or through the batch input call once it is past its
    /// time.
    fn tick_mouse(&mut self) {
        let Some((slot, made, listed)) = self
            .mouse
            .as_ref()
            .filter(|mouse| !mouse.ready)
            .map(|mouse| (mouse.slot, mouse.made, mouse.device.listed()))
        else {
            return;
        };
        let after_ms = lowlat_common::clock::elapsed_ms(made);
        if listed {
            if let Some(mouse) = self.mouse.as_mut() {
                mouse.ready = true;
            }
            lowlat_common::log_info!(
                "inject: the input driver's mouse delivers, slot={slot} after_ms={after_ms:.0}"
            );
            let held = core::mem::take(&mut self.mouse_held);
            self.pointer_to_hid(held.events());
        } else if after_ms >= DELIVERS_WITHIN_MS {
            lowlat_common::log_warn!(
                "inject: the input driver's mouse never delivered, the pointer goes through the batch input call, slot={slot} after_ms={after_ms:.0}"
            );
            if let Some(mouse) = self.mouse.take() {
                mouse.close_aside();
            }
            let held = core::mem::take(&mut self.mouse_held);
            self.pointer_to_batch(held.events());
        }
    }

    /// The owner's setting for the pads sent as their reports (docs/06-api.md
    /// section 3): presented as their products on the input driver while it
    /// is on. **A change reaches the pads plugged after it**: one already
    /// plugged keeps the family its first message fixed.
    pub fn set_virtual_pads(&mut self, on: bool) {
        self.virtual_pads = on;
        if on {
            // The driver may give the next pad one where it gave the last none.
            self.no_hid_pads = false;
        }
    }

    /// The report pad for a peer's identifier, a slot taken if it is new;
    /// none past the cap, or for a product other than the one its first
    /// message fixed.
    fn hid_pad(&mut self, id: u32, product: Product) -> Option<usize> {
        if let Some(index) = self
            .hid_pads
            .iter()
            .position(|p| p.as_ref().is_some_and(|p| p.id == id))
        {
            let same = self
                .hid_pads
                .get(index)
                .and_then(Option::as_ref)
                .is_some_and(|p| p.product == product);
            return same.then_some(index);
        }
        let index = self.hid_pads.iter().position(Option::is_none)?;
        *self.hid_pads.get_mut(index)? = Some(HidPad::new(id, product));
        Some(index)
    }

    /// A report pad's own report: a feature report kept for its device's
    /// answers; an input report into its device -- made at the first, the
    /// report held until the system lists it -- or onto the bus where it has
    /// none.
    fn pad_report(&mut self, id: u32, inbound: &Inbound<'_>) {
        let (product, report) = match inbound {
            Inbound::Feature {
                product,
                feature,
                report,
            } => {
                if let Some(index) = self.hid_pad(id, *product)
                    && let Some(pad) = self.hid_pads.get_mut(index).and_then(Option::as_mut)
                {
                    pad.features.set(*feature, report);
                }
                return;
            }
            Inbound::Input { product, report } => (*product, *report),
            Inbound::TouchBlock => return,
        };
        let Some(index) = self.hid_pad(id, product) else {
            return;
        };
        let address = answers::address(self.middle, index);
        let Some(pad) = self.hid_pads.get_mut(index).and_then(Option::as_mut) else {
            return;
        };
        if !pad.on_bus && pad.device.is_none() && !pad.make(address) {
            // **The pads plugged after it are made from their states**, the
            // driver having given this one none; this one plays on the bus
            // from its reports.
            pad.on_bus = true;
            self.no_hid_pads = true;
        }
        let on_bus = if pad.on_bus {
            true
        } else if pad.ready {
            !pad.put(&report)
        } else {
            pad.held = Some(report);
            false
        };
        if on_bus {
            self.play_on_bus(id, bus_report(product, &report, self.presses_guide()));
        }
    }

    /// Whether a pad played on the bus from its reports keeps Guide: as the
    /// expander says, and never while the host holds its guests to one
    /// application, which holds the owner too (docs/05-host.md section 7.4).
    fn presses_guide(&self) -> bool {
        self.guide && self.scope.is_none()
    }

    /// A report pad's state onto the bus, as the sixteen-button pad its own
    /// reports carry.
    fn play_on_bus(&mut self, id: u32, report: gamepad::Report) {
        let Some(index) = self.pad(id) else {
            return;
        };
        if let (Some(slot), Some(bus)) = (self.pads.get_mut(index), self.bus.as_ref())
            && let Some(pad) = slot.as_mut()
        {
            pad.report = report;
            if pad.settle(bus) {
                pad.submit(bus);
            } else {
                *slot = None;
            }
        }
    }

    /// The input driver's pads, each pass: the report held for a pad written
    /// once the system lists its device, or the pad onto the bus once it is
    /// past its time.
    fn tick_pads(&mut self) {
        for index in 0..self.hid_pads.len() {
            let Some(pad) = self.hid_pads.get_mut(index).and_then(Option::as_mut) else {
                continue;
            };
            if pad.ready || pad.on_bus {
                continue;
            }
            let Some(listed) = pad.device.as_ref().map(hid::Device::listed) else {
                continue;
            };
            let after_ms = lowlat_common::clock::elapsed_ms(pad.made);
            let held = if listed {
                pad.ready = true;
                lowlat_common::log_info!(
                    "inject: the input driver's pad delivers, pad={} slot={} after_ms={after_ms:.0}",
                    pad.id,
                    pad.slot
                );
                pad.held
                    .take()
                    .filter(|report| !pad.put(report))
                    .map(|report| (pad.id, pad.product, report))
            } else if after_ms >= DELIVERS_WITHIN_MS {
                lowlat_common::log_warn!(
                    "inject: the input driver's pad never delivered, it plays on the bus from its reports, pad={} slot={} after_ms={after_ms:.0}",
                    pad.id,
                    pad.slot
                );
                pad.leave_for_the_bus();
                pad.held.take().map(|report| (pad.id, pad.product, report))
            } else {
                None
            };
            if let Some((id, product, report)) = held {
                self.play_on_bus(id, bus_report(product, &report, self.presses_guide()));
            }
        }
    }

    /// Hand every report pad's reports to `to` instead of playing the pad
    /// from its whole states, and tell it when the pad is gone: the
    /// application's device stands in (docs/05-host.md section 7.2). Set
    /// before the first report.
    pub fn forward_reports(&mut self, to: Forward) {
        self.forward = Some(Forwarding::new(to));
    }

    /// What a program wrote to one of this guest's pads on the input driver,
    /// since the last ask; polled as [`Self::rumble`] is, and for the same
    /// reason. A pad whose device answers no more plays on the bus from its
    /// next report.
    pub fn written(&mut self) -> Option<PadWritten> {
        for pad in self.hid_pads.iter_mut().flatten() {
            let Some(device) = pad.device.as_mut() else {
                continue;
            };
            match device.written() {
                Ok(Some(written)) => {
                    return Some(PadWritten {
                        pad: pad.id,
                        written: handed_back(&written),
                    });
                }
                Ok(None) => {}
                Err(error) => {
                    lowlat_common::log_warn!(
                        "inject: the input driver's pad is gone, it plays on the bus from its reports, pad={} error={error}",
                        pad.id
                    );
                    pad.leave_for_the_bus();
                }
            }
        }
        None
    }

    /// Put each pad the bus has made ready to use, and its held report in.
    ///
    /// **Called from the loop that already runs**, so a pad that arrived with
    /// one message and nothing after it is still put in once it can be. The
    /// input driver's keyboard, mouse and pads are followed here too.
    pub fn tick(&mut self) {
        self.refresh_pen();
        self.refresh_touch();
        self.tick_keyboard();
        self.tick_mouse();
        self.tick_pads();
        let Some(bus) = self.bus.as_ref() else {
            return;
        };
        for slot in &mut self.pads {
            let Some(pad) = slot.as_mut() else {
                continue;
            };
            let was_ready = pad.ready;
            if !pad.settle(bus) {
                *slot = None;
            } else if !was_ready && pad.ready {
                pad.submit(bus);
            }
        }
    }

    /// What any of this guest's pads has been asked to vibrate at, since the
    /// last ask.
    ///
    /// **Polled rather than pushed.** The bus answers a request this thread
    /// keeps outstanding for each pad, and the guest's loop is already
    /// turning; a thread per pad would be a thread for a message that arrives
    /// when somebody drives over rubble.
    pub fn rumble(&mut self) -> Option<Rumble> {
        let bus = self.bus.as_ref()?;
        for pad in self.pads.iter_mut().flatten() {
            if let Some(rumble) = pad.rumbled(bus) {
                return Some(rumble);
            }
        }
        None
    }

    /// The slot of the pad for a peer's identifier, plugged in if this is its
    /// first event.
    fn pad(&mut self, id: u32) -> Option<usize> {
        if let Some(index) = self
            .pads
            .iter()
            .position(|p| p.as_ref().is_some_and(|p| p.id == id))
        {
            return Some(index);
        }
        if self.no_pads {
            return None;
        }
        if self.bus.is_none() {
            match Bus::open() {
                Ok(bus) => self.bus = Some(bus),
                Err(error) => {
                    self.no_pads = true;
                    lowlat_common::log_warn!("inject: no pad bus, pads are off, error={error}");
                    return None;
                }
            }
        }
        let bus = self.bus.as_ref()?;
        let Some(index) = self.pads.iter().position(Option::is_none) else {
            if !self.refused {
                self.refused = true;
                lowlat_common::log_warn!("inject: no room for another pad, pads={MAX_PADS}");
            }
            return None;
        };
        match Pad::plug(bus, id) {
            Ok(pad) => {
                lowlat_common::log_info!("inject: pad created, pad={id} serial={}", pad.serial);
                *self.pads.get_mut(index)? = Some(pad);
                Some(index)
            }
            Err(error) => {
                self.no_pads = true;
                lowlat_common::log_warn!(
                    "inject: pad not created, pads are off, pad={id} error={error}"
                );
                None
            }
        }
    }

    /// Put input in through the system's call, attaching this thread to the
    /// input desktop when the call refuses a thread that is not on it.
    fn send(&mut self, inputs: &[INPUT]) {
        let mut rest = inputs;
        let mut attached = false;
        while !rest.is_empty() {
            let count = u32::try_from(rest.len()).unwrap_or(0);
            // SAFETY: `count` inputs from a live slice, each of the size
            // passed; the call copies them before it returns.
            let sent = unsafe { SendInput(count, rest.as_ptr(), INPUT_SIZE) };
            rest = rest.get(sent as usize..).unwrap_or_default();
            if rest.is_empty() {
                break;
            }
            let error = last_error();
            // **Refused off the input desktop**, which the lock screen and
            // an elevation prompt replace with another: attached to the
            // desktop that has it now, the rest goes again.
            if error == ERROR_ACCESS_DENIED
                && !attached
                && let Some(desktop) = Desktop::attach()
            {
                attached = true;
                self.desktop = Some(desktop);
                continue;
            }
            if !self.refusing {
                self.refusing = true;
                lowlat_common::log_warn!(
                    "inject: input refused, error={error} lost={}",
                    rest.len()
                );
            }
            return;
        }
        if attached {
            lowlat_common::log_info!("inject: input moved to the input desktop as it is now");
        }
        self.refusing = false;
    }

    /// A pen's events: its state kept, and a frame at each report's end.
    fn pen_events(&mut self, events: &[Event]) {
        for event in events {
            if event.kind == event::EV_SYN {
                self.pen_frame();
            } else {
                self.pen.now.take(event);
            }
        }
    }

    /// One report of the guest's pen, as a frame of its synthetic pen -- made
    /// at the first -- or as the pointer where the system has none or refuses
    /// one.
    fn pen_frame(&mut self) {
        let Some(flags) = pen_flags(&self.pen.sent, &self.pen.now) else {
            return;
        };
        if !self.pen.as_pointer && self.pen.device.is_none() {
            self.pen.device = Synthetic::make(PT_PEN, 1);
            if self.pen.device.is_none() {
                self.pen.as_pointer = true;
                lowlat_common::log_warn!(
                    "inject: no synthetic pen, the pen drives the pointer, error={}",
                    last_error()
                );
            }
        }
        if self.pen.as_pointer {
            self.pen_as_pointer();
        } else {
            let now = self.pen.now;
            let frame = pen_info(flags, &now, desktop_pixel(now.x, now.y, desktop_now()));
            put(
                self.pen.device.as_ref(),
                &[frame],
                &mut self.pen.refusing,
                &mut self.desktop,
            );
        }
        self.pen.sent = self.pen.now;
        self.pen.at = Some(lowlat_common::clock::Time::now());
    }

    /// The guest's pen in range, told again where nothing new has been said
    /// for [`PEN_REFRESH_MS`]: **the system lifts a pen it hears nothing
    /// from**, half a second after its last frame. Asked on every pass of the
    /// guest's loop, which passes at least every 50 ms.
    fn refresh_pen(&mut self) {
        let sent = self.pen.sent;
        if self.pen.as_pointer || self.pen.device.is_none() || !sent.in_range() {
            return;
        }
        if self
            .pen
            .at
            .is_some_and(|at| lowlat_common::clock::elapsed_ms(at) < PEN_REFRESH_MS)
        {
            return;
        }
        let mut flags = POINTER_FLAG_INRANGE | POINTER_FLAG_UPDATE;
        if sent.down() {
            flags |= POINTER_FLAG_INCONTACT;
        }
        let frame = pen_info(flags, &sent, desktop_pixel(sent.x, sent.y, desktop_now()));
        put(
            self.pen.device.as_ref(),
            &[frame],
            &mut self.pen.refusing,
            &mut self.desktop,
        );
        self.pen.at = Some(lowlat_common::clock::Time::now());
    }

    /// Cancel what the guest's pen holds: **the device's end is what a window
    /// is told is a cancel** -- a frame saying so arrives as a lift -- and the
    /// next frame makes another. Where the pen drives the pointer, its button
    /// comes up.
    fn cancel_pen(&mut self) {
        if self.pen.as_pointer && self.pen.sent.down() {
            self.send(&[mouse_input(0, 0, 0, MOUSEEVENTF_LEFTUP)]);
        }
        self.pen.device = None;
        self.pen.now = PenState::default();
        self.pen.sent = PenState::default();
        self.pen.at = None;
    }

    /// The pen as the pointer, where the system has no synthetic pen: a
    /// position over the whole desktop while in range, and the left button
    /// for its touch.
    fn pen_as_pointer(&mut self) {
        let (sent, now) = (self.pen.sent, self.pen.now);
        let mut inputs = Inputs::new();
        if now.in_range() {
            inputs.push(mouse_input(now.x, now.y, 0, ABSOLUTE));
        }
        if now.down() != sent.down() {
            let flags = if now.down() {
                MOUSEEVENTF_LEFTDOWN
            } else {
                MOUSEEVENTF_LEFTUP
            };
            inputs.push(mouse_input(0, 0, 0, flags));
        }
        self.send(inputs.as_slice());
    }

    /// A touch surface's events: each slot kept, and a frame at each report's
    /// end.
    fn touch_events(&mut self, events: &[Event]) {
        for event in events {
            if event.kind == event::EV_SYN {
                self.touch_frame();
            } else {
                self.touch.take(event);
            }
        }
    }

    /// One report of the guest's touch, as a frame of its synthetic touch
    /// surface, made at the first: [`touch_frame`]'s contacts, every one the
    /// surface holds. Where the system has none, or refuses one, touch goes
    /// nowhere, said once.
    fn touch_frame(&mut self) {
        let (frame, count) = touch_frame(&self.touch.sent, &self.touch.now, desktop_now());
        self.touch.sent = self.touch.now;
        if count == 0 {
            return;
        }
        if !self.touch.unavailable && self.touch.device.is_none() {
            self.touch.device = Synthetic::make(PT_TOUCH, u32::try_from(TOUCH_SLOTS).unwrap_or(1));
            if self.touch.device.is_none() {
                self.touch.unavailable = true;
                lowlat_common::log_warn!(
                    "inject: no synthetic touch surface, touch goes nowhere, error={}",
                    last_error()
                );
            }
        }
        put(
            self.touch.device.as_ref(),
            frame.get(..count).unwrap_or_default(),
            &mut self.touch.refusing,
            &mut self.desktop,
        );
        self.touch.at = Some(lowlat_common::clock::Time::now());
    }

    /// The guest's contacts, told again where nothing new has been said for
    /// [`PEN_REFRESH_MS`], as the pen is: the system lifts a contact it hears
    /// nothing from.
    fn refresh_touch(&mut self) {
        if self.touch.device.is_none() || !self.touch.sent.iter().any(TouchSlot::live) {
            return;
        }
        if self
            .touch
            .at
            .is_some_and(|at| lowlat_common::clock::elapsed_ms(at) < PEN_REFRESH_MS)
        {
            return;
        }
        let (frame, count) = touch_frame(&self.touch.sent, &self.touch.sent, desktop_now());
        put(
            self.touch.device.as_ref(),
            frame.get(..count).unwrap_or_default(),
            &mut self.touch.refusing,
            &mut self.desktop,
        );
        self.touch.at = Some(lowlat_common::clock::Time::now());
    }

    /// Cancel every contact the guest's touch holds: the device's end, as the
    /// pen's is, which a window is told is a cancel of each.
    fn cancel_touches(&mut self) {
        self.touch.device = None;
        self.touch.now = [TouchSlot::default(); TOUCH_SLOTS];
        self.touch.sent = [TouchSlot::default(); TOUCH_SLOTS];
        self.touch.at = None;
    }
}

/// How often a pen in range or a contact is told again with nothing new to
/// say. **The system lifts a held pen or contact 521 to 524 ms after its last
/// frame** (measured 2026-11-26, Windows 11 24H2): a fifth of that, so a loop
/// running late by 400 ms is still in time, at 70 us a frame.
const PEN_REFRESH_MS: f64 = 100.0;

/// The system's synthetic pointer, which came with Windows 10 1809: **found
/// at run time**, so a host on an older system still loads; its guests' pens
/// drive the pointer and their touch goes nowhere.
#[derive(Clone, Copy)]
struct PointerCalls {
    create: unsafe extern "system" fn(
        POINTER_INPUT_TYPE,
        u32,
        POINTER_FEEDBACK_MODE,
    ) -> HSYNTHETICPOINTERDEVICE,
    inject:
        unsafe extern "system" fn(HSYNTHETICPOINTERDEVICE, *const POINTER_TYPE_INFO, u32) -> i32,
    destroy: unsafe extern "system" fn(HSYNTHETICPOINTERDEVICE),
}

/// The calls, found once for the process.
fn pointer_calls() -> Option<PointerCalls> {
    static CALLS: std::sync::OnceLock<Option<(lowlat_common::dynlib::Library, PointerCalls)>> =
        std::sync::OnceLock::new();
    CALLS
        .get_or_init(|| {
            let user32 = lowlat_common::dynlib::Library::open_system(c"user32.dll")?;
            // SAFETY: each type is the export's own signature as the system
            // declares it, and the library stays mapped with the calls.
            let calls = unsafe {
                PointerCalls {
                    create: user32.symbol(c"CreateSyntheticPointerDevice")?,
                    inject: user32.symbol(c"InjectSyntheticPointerInput")?,
                    destroy: user32.symbol(c"DestroySyntheticPointerDevice")?,
                }
            };
            Some((user32, calls))
        })
        .as_ref()
        .map(|(_, calls)| *calls)
}

/// A synthetic pointer of the system's own -- a pen, or a touch surface of
/// several contacts -- its feedback the system's own, destroyed with this.
struct Synthetic {
    handle: HSYNTHETICPOINTERDEVICE,
    calls: PointerCalls,
}

impl Synthetic {
    /// One device of `kind`, for up to `contacts` at once; none where the
    /// system has no synthetic pointer or refuses one.
    fn make(kind: POINTER_INPUT_TYPE, contacts: u32) -> Option<Self> {
        let calls = pointer_calls()?;
        // SAFETY: an export of the system's, with its own types.
        let handle = unsafe { (calls.create)(kind, contacts, POINTER_FEEDBACK_DEFAULT) };
        (!handle.is_null()).then_some(Self { handle, calls })
    }

    fn inject(&self, frames: &[POINTER_TYPE_INFO]) -> bool {
        let Ok(count) = u32::try_from(frames.len()) else {
            return false;
        };
        // SAFETY: `count` frames from a live slice, into the device this made.
        unsafe { (self.calls.inject)(self.handle, frames.as_ptr(), count) != 0 }
    }
}

impl Drop for Synthetic {
    fn drop(&mut self) {
        // SAFETY: the device this made, destroyed once.
        unsafe { (self.calls.destroy)(self.handle) };
    }
}

/// Frames into a synthetic device, **attaching this thread to the input
/// desktop when the system refuses them**, as the batch input call is; a
/// refusal that stays is said once.
fn put(
    device: Option<&Synthetic>,
    frames: &[POINTER_TYPE_INFO],
    refusing: &mut bool,
    desktop: &mut Option<Desktop>,
) {
    let Some(device) = device else {
        return;
    };
    if frames.is_empty() || device.inject(frames) {
        *refusing = false;
        return;
    }
    let error = last_error();
    if let Some(attached) = Desktop::attach() {
        *desktop = Some(attached);
        if device.inject(frames) {
            *refusing = false;
            return;
        }
    }
    if !core::mem::replace(refusing, true) {
        lowlat_common::log_warn!(
            "inject: a pointer frame refused, error={error} contacts={}",
            frames.len()
        );
    }
}

/// A guest's pen: its device, made at the pen's first frame on the guest's
/// thread, and what it was last told.
#[derive(Default)]
struct Pen {
    device: Option<Synthetic>,
    /// What the expander's events say, to the last of them.
    now: PenState,
    /// What the last frame told the system.
    sent: PenState,
    /// When that frame went in.
    at: Option<lowlat_common::clock::Time>,
    /// The system has no synthetic pen for this guest: the pen drives the
    /// pointer.
    as_pointer: bool,
    /// A frame was refused, so the next refusal is not said again.
    refusing: bool,
}

/// A pen at one moment, as its events say: where it is on the absolute
/// axis, how it is held, which of its parts are down.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct PenState {
    x: i32,
    y: i32,
    pressure: i32,
    rotation: i32,
    tilt_x: i32,
    tilt_y: i32,
    pen: bool,
    rubber: bool,
    touching: bool,
    barrel: bool,
}

impl PenState {
    fn in_range(&self) -> bool {
        self.pen || self.rubber
    }

    /// Touching, which a pen out of range is not.
    fn down(&self) -> bool {
        self.touching && self.in_range()
    }

    fn take(&mut self, event: &Event) {
        let on = event.value != 0;
        match (event.kind, event.code) {
            (event::EV_ABS, event::ABS_X) => self.x = event.value,
            (event::EV_ABS, event::ABS_Y) => self.y = event.value,
            (event::EV_ABS, event::ABS_PRESSURE) => self.pressure = event.value,
            (event::EV_ABS, event::ABS_Z) => self.rotation = event.value,
            (event::EV_ABS, event::ABS_TILT_X) => self.tilt_x = event.value,
            (event::EV_ABS, event::ABS_TILT_Y) => self.tilt_y = event.value,
            (event::EV_KEY, event::BTN_TOOL_PEN) => self.pen = on,
            (event::EV_KEY, event::BTN_TOOL_RUBBER) => self.rubber = on,
            (event::EV_KEY, event::BTN_TOUCH) => self.touching = on,
            (event::EV_KEY, event::BTN_STYLUS) => self.barrel = on,
            _ => {}
        }
    }
}

/// A guest's touch surface: its device, made at the first contact on the
/// guest's thread, and each slot as the events say and as the last frame
/// told the system.
#[derive(Default)]
struct Touches {
    device: Option<Synthetic>,
    now: [TouchSlot; TOUCH_SLOTS],
    sent: [TouchSlot; TOUCH_SLOTS],
    /// The slot the events name.
    slot: usize,
    /// When the last frame went in.
    at: Option<lowlat_common::clock::Time>,
    /// The system has no synthetic touch surface for this guest: touch goes
    /// nowhere.
    unavailable: bool,
    /// A frame was refused, so the next refusal is not said again.
    refusing: bool,
}

impl Touches {
    fn take(&mut self, event: &Event) {
        if (event.kind, event.code) == (event::EV_ABS, event::ABS_MT_SLOT) {
            self.slot = usize::try_from(event.value).unwrap_or(usize::MAX);
            return;
        }
        let Some(slot) = self.now.get_mut(self.slot) else {
            return;
        };
        match (event.kind, event.code) {
            (event::EV_ABS, event::ABS_MT_TRACKING_ID) => slot.tracking = event.value,
            (event::EV_ABS, event::ABS_MT_POSITION_X) => slot.x = event.value,
            (event::EV_ABS, event::ABS_MT_POSITION_Y) => slot.y = event.value,
            (event::EV_ABS, event::ABS_MT_PRESSURE) => slot.pressure = event.value,
            (event::EV_ABS, event::ABS_MT_TOUCH_MAJOR) => slot.major = event.value,
            (event::EV_ABS, event::ABS_MT_TOUCH_MINOR) => slot.minor = event.value,
            (event::EV_ABS, event::ABS_MT_ORIENTATION) => slot.orientation = event.value,
            (event::EV_ABS, event::ABS_MT_DISTANCE) => slot.hovering = event.value > 0,
            _ => {}
        }
    }
}

/// A touch contact in its slot, as its events say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TouchSlot {
    /// Its tracking number, or -1 where the slot holds no contact.
    tracking: i32,
    x: i32,
    y: i32,
    pressure: i32,
    /// Its size across and down, in the desktop's pixels; 0 not reported.
    major: i32,
    minor: i32,
    /// Degrees, or -1 not reported.
    orientation: i32,
    hovering: bool,
}

impl Default for TouchSlot {
    fn default() -> Self {
        Self {
            tracking: -1,
            x: 0,
            y: 0,
            pressure: 0,
            major: 0,
            minor: 0,
            orientation: -1,
            hovering: false,
        }
    }
}

impl TouchSlot {
    fn live(&self) -> bool {
        self.tracking >= 0
    }

    fn down(&self) -> bool {
        self.live() && !self.hovering
    }
}

/// The pointer flags a frame says of a pointer, from what the last frame told
/// the system -- in range, and touching -- and what is so now; nothing for a
/// pointer out of range that stays so.
fn transition(was: (bool, bool), now: (bool, bool)) -> Option<POINTER_FLAGS> {
    let (was_in, was_down) = (was.0, was.0 && was.1);
    let (now_in, now_down) = (now.0, now.0 && now.1);
    let contact = POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT;
    Some(match (was_down, now_in, now_down) {
        (true, _, true) => contact | POINTER_FLAG_UPDATE,
        (false, _, true) => contact | POINTER_FLAG_DOWN,
        (true, true, false) => POINTER_FLAG_INRANGE | POINTER_FLAG_UP,
        (true, false, false) => POINTER_FLAG_UP,
        (false, true, false) => POINTER_FLAG_INRANGE | POINTER_FLAG_UPDATE,
        (false, false, false) if was_in => POINTER_FLAG_UPDATE,
        (false, false, false) => return None,
    })
}

/// The pointer flags a pen's frame says: [`transition`] of its range and its
/// touch.
fn pen_flags(sent: &PenState, now: &PenState) -> Option<POINTER_FLAGS> {
    transition((sent.in_range(), sent.down()), (now.in_range(), now.down()))
}

/// A frame of a guest's touch surface, from what the last frame told the
/// system and what each slot holds now: **every contact the surface holds**
/// -- the system takes one missing from a frame as lifted -- and each one
/// ending, at its last place, with the lift; the frames and how many there are.
fn touch_frame(
    sent: &[TouchSlot; TOUCH_SLOTS],
    now: &[TouchSlot; TOUCH_SLOTS],
    desktop: (i32, i32, i32, i32),
) -> ([POINTER_TYPE_INFO; TOUCH_SLOTS], usize) {
    let blank = touch_info(0, 0, &TouchSlot::default(), desktop);
    let mut frame = [blank; TOUCH_SLOTS];
    let mut count = 0;
    for (index, (sent, now)) in sent.iter().zip(now).enumerate() {
        let Some(flags) = transition((sent.live(), sent.down()), (now.live(), now.down())) else {
            continue;
        };
        let slot = if now.live() { now } else { sent };
        if let Some(entry) = frame.get_mut(count) {
            *entry = touch_info(index, flags, slot, desktop);
            count += 1;
        }
    }
    (frame, count)
}

/// One pointer's part of a frame: which, what changed, and where.
fn pointer_info(
    kind: POINTER_INPUT_TYPE,
    id: u32,
    flags: POINTER_FLAGS,
    pixel: (i32, i32),
) -> POINTER_INFO {
    let origin = POINT { x: 0, y: 0 };
    POINTER_INFO {
        pointerType: kind,
        pointerId: id,
        frameId: 0,
        pointerFlags: flags,
        sourceDevice: core::ptr::null_mut(),
        hwndTarget: core::ptr::null_mut(),
        ptPixelLocation: POINT {
            x: pixel.0,
            y: pixel.1,
        },
        ptHimetricLocation: origin,
        ptPixelLocationRaw: origin,
        ptHimetricLocationRaw: origin,
        dwTime: 0,
        historyCount: 0,
        InputData: 0,
        dwKeyStates: 0,
        PerformanceCount: 0,
        ButtonChangeType: 0,
    }
}

/// A pen's frame at a desktop pixel. **Pressure only while touching**; the
/// turn and both tilts always; the eraser end in range is the pen turned
/// over, and touching, its eraser.
fn pen_info(flags: POINTER_FLAGS, now: &PenState, pixel: (i32, i32)) -> POINTER_TYPE_INFO {
    use lowlat_core::pen::{PRESSURE_MAX, ROTATION_SPAN, TILT_MAX};
    let mut held = 0;
    if now.barrel {
        held |= PEN_FLAG_BARREL;
    }
    if now.rubber {
        held |= PEN_FLAG_INVERTED;
        if now.down() {
            held |= PEN_FLAG_ERASER;
        }
    }
    let mut mask = PEN_MASK_ROTATION | PEN_MASK_TILT_X | PEN_MASK_TILT_Y;
    let pressure = if now.down() {
        mask |= PEN_MASK_PRESSURE;
        now.pressure.clamp(0, i32::from(PRESSURE_MAX))
    } else {
        0
    };
    let tilt = |value: i32| value.clamp(-i32::from(TILT_MAX), i32::from(TILT_MAX));
    POINTER_TYPE_INFO {
        r#type: PT_PEN,
        Anonymous: POINTER_TYPE_INFO_0 {
            penInfo: POINTER_PEN_INFO {
                pointerInfo: pointer_info(PT_PEN, 0, flags, pixel),
                penFlags: held,
                penMask: mask,
                pressure: pressure.unsigned_abs(),
                rotation: now
                    .rotation
                    .clamp(0, i32::from(ROTATION_SPAN) - 1)
                    .unsigned_abs(),
                tiltX: tilt(now.tilt_x),
                tiltY: tilt(now.tilt_y),
            },
        },
    }
}

/// A contact's part of a touch frame, its number the slot's. **Pressure and
/// the contact's area only while touching**, the area centred on the contact
/// where both of its sides were reported; the orientation where it was.
fn touch_info(
    index: usize,
    flags: POINTER_FLAGS,
    slot: &TouchSlot,
    desktop: (i32, i32, i32, i32),
) -> POINTER_TYPE_INFO {
    use lowlat_core::pen::{PRESSURE_MAX, ROTATION_SPAN};
    let (x, y) = desktop_pixel(slot.x, slot.y, desktop);
    let nothing = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    let mut mask = TOUCH_MASK_NONE;
    let (mut area, mut pressure) = (nothing, 0);
    if flags & POINTER_FLAG_INCONTACT != 0 {
        mask |= TOUCH_MASK_PRESSURE;
        pressure = slot.pressure.clamp(0, i32::from(PRESSURE_MAX));
        if slot.major > 0 && slot.minor > 0 {
            mask |= TOUCH_MASK_CONTACTAREA;
            let (left, top) = (x - slot.major / 2, y - slot.minor / 2);
            area = RECT {
                left,
                top,
                right: left + slot.major,
                bottom: top + slot.minor,
            };
        }
    }
    let orientation = if slot.orientation >= 0 {
        mask |= TOUCH_MASK_ORIENTATION;
        slot.orientation.min(i32::from(ROTATION_SPAN) - 1)
    } else {
        0
    };
    POINTER_TYPE_INFO {
        r#type: PT_TOUCH,
        Anonymous: POINTER_TYPE_INFO_0 {
            touchInfo: POINTER_TOUCH_INFO {
                pointerInfo: pointer_info(
                    PT_TOUCH,
                    u32::try_from(index).unwrap_or(0),
                    flags,
                    (x, y),
                ),
                touchFlags: TOUCH_FLAG_NONE,
                touchMask: mask,
                rcContact: area,
                rcContactRaw: nothing,
                orientation: orientation.unsigned_abs(),
                pressure: pressure.unsigned_abs(),
            },
        },
    }
}

/// The desktop pixel an absolute axis value names, **as the system takes one
/// back**: `value x width / 65536` from the desktop's corner, truncated, which
/// the expander aims at each pixel's centre.
fn desktop_pixel(x: i32, y: i32, desktop: (i32, i32, i32, i32)) -> (i32, i32) {
    let (left, top, width, height) = desktop;
    let pixel = |value: i32, origin: i32, extent: i32| {
        i32::try_from(i64::from(origin) + i64::from(value) * i64::from(extent) / 65536)
            .unwrap_or(origin)
    };
    (pixel(x, left, width), pixel(y, top, height))
}

/// The desktop as the system lays it out now, in the physical pixels of a
/// per-monitor aware thread: its corner and its size.
fn desktop_now() -> (i32, i32, i32, i32) {
    // SAFETY: reads of the system's metrics.
    let metric = |index| unsafe { GetSystemMetrics(index) };
    (
        metric(SM_XVIRTUALSCREEN),
        metric(SM_YVIRTUALSCREEN),
        metric(SM_CXVIRTUALSCREEN),
        metric(SM_CYVIRTUALSCREEN),
    )
}

impl Sink for Devices {
    fn emit(&mut self, device: Device, events: &[Event]) {
        // **A held guest's motion by deltas stays on the application's
        // window** (docs/05-host.md section 7.4): the system's clip of the
        // pointer, set before it goes in, holds whichever way it goes in. A
        // position is kept on the window by the expander instead, and lets the
        // clip go: the desk's own mouse is held by it too.
        if self.scope.is_some() {
            match device {
                Device::Pointer if moves_by_deltas(events) => self.confine(),
                Device::PointerAbsolute | Device::Pen | Device::Touch => self.clip.release(),
                _ => {}
            }
        }
        match device {
            Device::Keyboard => match self.hid.as_ref().map(|hid| hid.ready) {
                Some(true) => self.keys_to_hid(events),
                Some(false) => self.held.push(events),
                None => self.keys_to_batch(events),
            },
            Device::Pointer | Device::PointerAbsolute => {
                match self.mouse.as_ref().map(|mouse| mouse.ready) {
                    Some(true) => self.pointer_to_hid(events),
                    Some(false) => self.mouse_held.push(events),
                    None => self.pointer_to_batch(events),
                }
            }
            Device::Gamepad(id) => {
                let Some(index) = self.pad(id) else {
                    return;
                };
                if let (Some(slot), Some(bus)) = (self.pads.get_mut(index), self.bus.as_ref())
                    && let Some(pad) = slot.as_mut()
                {
                    pad.report.apply(events);
                    if pad.settle(bus) {
                        pad.submit(bus);
                    } else {
                        *slot = None;
                    }
                }
            }
            Device::Pen => self.pen_events(events),
            Device::Touch => self.touch_events(events),
        }
    }

    fn cancel(&mut self, device: Device) {
        match device {
            Device::Pen => self.cancel_pen(),
            Device::Touch => self.cancel_touches(),
            _ => {}
        }
    }

    fn unplug(&mut self, pad: u32) {
        if let Some(slot) = self
            .pads
            .iter_mut()
            .find(|slot| slot.as_ref().is_some_and(|p| p.id == pad))
            && let Some(taken) = slot.take()
        {
            if let Some(bus) = self.bus.as_ref() {
                taken.unplug(bus);
            }
            lowlat_common::log_info!("inject: pad unplugged, pad={pad}");
        }
        // **Let go of off this thread**: a device's end waits while its
        // neutral report arrives, which the guest's loop is not held up by.
        if let Some(slot) = self
            .hid_pads
            .iter_mut()
            .find(|slot| slot.as_ref().is_some_and(|p| p.id == pad))
            && let Some(taken) = slot.take()
        {
            if let Some(device) = taken.device {
                close_aside(device);
            }
            lowlat_common::log_info!("inject: pad unplugged, pad={pad}");
        }
        if self.forward.as_mut().is_some_and(|f| f.unplug(pad)) {
            lowlat_common::log_info!("inject: pad unplugged, pad={pad}");
        }
    }

    fn report(&mut self, pad: u32, inbound: &Inbound<'_>) {
        if let Some(forward) = self.forward.as_mut() {
            forward.report(pad, inbound);
            return;
        }
        self.pad_report(pad, inbound);
    }

    /// **To an application holding the device, or to the input driver where
    /// the owner has `virtual_pads` on** and the driver has not given a pad
    /// none: otherwise a new pad's own report is not taken, and the pad is
    /// played from its whole states. A pad already sent as its reports keeps
    /// them whatever this says.
    fn takes_reports(&self) -> bool {
        self.forward.is_some() || (self.virtual_pads && !self.no_hid_pads)
    }

    fn guide(&mut self, allowed: bool) {
        self.guide = allowed;
    }

    fn scoped(&self) -> bool {
        self.scope.is_some()
    }

    fn gate(&mut self, at: event::Gate) -> bool {
        let Some(judge) = self.scope.as_mut() else {
            return true;
        };
        let open = judge.gate(at, &mut self.desktop);
        // **The application gone from the front lets the clip go**: the
        // system keeps it for the window it was set for, and the desk's own
        // mouse with it.
        if !open && at == event::Gate::Front {
            self.clip.release();
        }
        open
    }

    fn bounds(&mut self) -> Option<event::Bounds> {
        let window = self.scope.as_mut()?.shown()?;
        crate::scope::on_its_display(window).map(crate::scope::on_desktop)
    }

    fn next_window(&mut self, backwards: bool, again: bool) {
        if let Some(judge) = self.scope.as_mut() {
            self.switch.step(judge, backwards, again);
        }
    }

    /// **The system's unassigned virtual key**, down and up through the batch
    /// input call whichever way the guest's keys go: a window takes an Alt let
    /// go after another key as part of a chord, not as its menu key.
    fn mask_alt(&mut self) {
        const UNASSIGNED: u16 = 0xE8;
        self.send(&[
            key_input(Stroke::Virtual(UNASSIGNED), true),
            key_input(Stroke::Virtual(UNASSIGNED), false),
        ]);
    }

    /// **The product's own report at rest**, as a real pad sends it, the way
    /// any of its reports goes: into its device, onto the bus where it has
    /// none, or to the application holding the devices.
    fn neutral(&mut self, id: u32) {
        let product = match self.forward.as_ref() {
            Some(forward) => forward.product(id),
            None => self
                .hid_pads
                .iter()
                .flatten()
                .find(|known| known.id == id)
                .map(|known| known.product),
        };
        let Some(product) = product else {
            return;
        };
        let Some(report) = kind_of(product)
            .and_then(|kind| kind.neutral().first().copied())
            .and_then(|rest| <[u8; pad::INPUT_LEN]>::try_from(rest).ok())
        else {
            return;
        };
        self.report(id, &Inbound::Input { product, report });
    }

    fn host_locks(&self) -> Option<Locks> {
        // The low bit is the toggle, read on this thread; it follows a lock
        // changed anywhere on the desk, a thread with no window included.
        // SAFETY: two reads of a key's state, with keys the system defines.
        let on = |key: u16| unsafe { GetKeyState(i32::from(key)) } & 1 != 0;
        Some(Locks {
            num: on(VK_NUMLOCK),
            caps: on(VK_CAPITAL),
        })
    }
}

/// Whether a text console would turn the attention chord into a restart.
/// Windows has no such console.
#[must_use]
pub fn console_takes_the_chord() -> bool {
    false
}

/// One key as the input call takes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stroke {
    /// A Set-1 code, the E0 prefix in bit 7.
    Scan(u8),
    /// A code sent as the byte it is, for a key that sends a make code and
    /// no break: bit 7 is the code's own, not the prefix.
    Make(u8),
    /// A virtual key, for the one key no scancode carries.
    Virtual(u16),
}

/// The guest's keyboard as the input call needs it: which of Ctrl and Alt
/// it holds, and how each of the two keys that change form went down.
///
/// **Ctrl and Alt are this keyboard's, not the machine's**, as the system's
/// own keyboard driver keeps them for each keyboard: Pause is Break under
/// Ctrl and PrintScreen is SysRq under Alt, and a key held on another
/// keyboard changes neither.
#[derive(Debug, Default)]
struct Keyboard {
    /// Left and right, a bit each.
    ctrl: u8,
    alt: u8,
    /// The stroke Pause and PrintScreen went down as, which is the stroke
    /// they repeat and come up as: a key that comes up as another stroke
    /// leaves the first held.
    pause: Option<Stroke>,
    print: Option<Stroke>,
}

impl Keyboard {
    /// The stroke one key event goes out as, or `None` for one that sends
    /// nothing.
    fn stroke(&mut self, code: u16, down: bool) -> Option<Stroke> {
        let code = u8::try_from(code).ok()?;
        let side = |held: &mut u8, bit: u8| {
            if down {
                *held |= bit;
            } else {
                *held &= !bit;
            }
        };
        match code {
            LCTRL => side(&mut self.ctrl, 1),
            RCTRL => side(&mut self.ctrl, 2),
            LALT => side(&mut self.alt, 1),
            RALT => side(&mut self.alt, 2),
            _ => {}
        }
        if code == PAUSE {
            let ctrl = self.ctrl != 0;
            return held_as(&mut self.pause, down, || {
                if ctrl {
                    Stroke::Scan(PAUSE)
                } else {
                    Stroke::Virtual(VK_PAUSE)
                }
            });
        }
        if code == SYSRQ.0 {
            let alt = self.alt != 0;
            return held_as(&mut self.print, down, || {
                Stroke::Scan(if alt { SYSRQ.1 } else { SYSRQ.0 })
            });
        }
        if MAKE_ONLY.contains(&code) {
            return down.then_some(Stroke::Make(code));
        }
        Some(Stroke::Scan(code))
    }
}

/// The stroke a key that changes form goes out as: chosen as it goes down
/// and kept while it is held, so its repeats and its release are the same
/// stroke whatever happened to the modifiers meanwhile.
fn held_as(
    slot: &mut Option<Stroke>,
    down: bool,
    choose: impl FnOnce() -> Stroke,
) -> Option<Stroke> {
    if down {
        Some(*slot.get_or_insert_with(choose))
    } else {
        slot.take()
    }
}

/// The keyboard page's two Deletes, the main key's and the keypad's, either of
/// which under Ctrl and Alt is the attention chord.
const DELETES: [u8; 2] = [0x4c, 0x63];
/// The modifiers' bits in the keys report: left and right Ctrl, left and
/// right Alt.
const CTRL_BITS: u8 = 0b0001_0001;
const ALT_BITS: u8 = 0b0100_0100;
/// The first modifier's usage; the others follow it bit by bit.
const FIRST_MODIFIER: u8 = 0xe0;
/// Consumer usages held at once: the media and volume keys there are.
const CONSUMER_HELD: usize = 8;

/// Which way a Delete went down, and comes up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Way {
    Hid,
    Batch,
}

/// What a key asks of the two paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    /// The HID keyboard's keys changed, or did not: its report goes as the
    /// report ends.
    Hid,
    /// A Delete goes this way through the batch input call.
    Batch,
    /// A Delete's release whose key was let go of already.
    Nothing,
    /// A Delete held on the HID keyboard was let go of there as Ctrl and Alt
    /// came together over it: the keys report goes now, without the key that
    /// completed them, and then that key is set.
    ReleasedFirst,
}

/// Whether Ctrl and Alt are both held: each on this keyboard, its `own`
/// modifiers, or anywhere on the machine as the `system` says.
fn chord(own: u8, system: (bool, bool)) -> bool {
    (own & CTRL_BITS != 0 || system.0) && (own & ALT_BITS != 0 || system.1)
}

/// The guest's keys as the input driver's keyboard carries them, apart from
/// the device: what each key changes, and which reports then go.
///
/// **The attention chord's rule is here** (docs/05-host.md section 7.3): a
/// keyboard's Ctrl, Alt and Delete raise the secure screen, which the batch
/// input call's never do, so a Delete going down while Ctrl and Alt are held --
/// on this keyboard or anywhere -- goes through the batch input call and comes
/// up the way it went down; and a Delete held here when Ctrl and Alt come
/// together over it is let go of here first, before the keyboard's own repeat
/// can type it under them.
#[derive(Debug, Default)]
struct HidKeys {
    keys: keyboard::Keys,
    /// The consumer usages held, the newest last; the report carries the
    /// newest.
    consumer: [u16; CONSUMER_HELD],
    consumer_held: usize,
    /// What was last written of each report, so an unchanged one is not
    /// written again.
    keys_written: keyboard::Keys,
    consumer_written: u16,
    /// The way each Delete went down while it is held.
    deletes: [Option<Way>; 2],
}

impl HidKeys {
    fn modifiers(&self) -> u8 {
        self.keys.report().get(1).copied().unwrap_or(0)
    }

    /// One key of the keyboard page, `system` asked for the machine's Ctrl and
    /// Alt only where a Delete's way depends on them.
    fn key(&mut self, usage: u8, down: bool, system: &mut impl FnMut() -> (bool, bool)) -> Step {
        if let Some(index) = DELETES.iter().position(|&delete| delete == usage) {
            return self.delete(index, usage, down, system);
        }
        let bit = usage
            .checked_sub(FIRST_MODIFIER)
            .and_then(|shift| 1u8.checked_shl(u32::from(shift)))
            .filter(|bit| bit & (CTRL_BITS | ALT_BITS) != 0)
            .unwrap_or(0);
        if down
            && bit != 0
            && self.deletes.contains(&Some(Way::Hid))
            && chord(self.modifiers() | bit, system())
        {
            self.let_go_of_deletes();
            return Step::ReleasedFirst;
        }
        self.keys.set(usage, down);
        Step::Hid
    }

    fn delete(
        &mut self,
        index: usize,
        usage: u8,
        down: bool,
        system: &mut impl FnMut() -> (bool, bool),
    ) -> Step {
        let modifiers = self.modifiers();
        let Some(slot) = self.deletes.get_mut(index) else {
            return Step::Nothing;
        };
        if !down {
            return match slot.take() {
                Some(Way::Hid) => {
                    self.keys.set(usage, false);
                    Step::Hid
                }
                Some(Way::Batch) => Step::Batch,
                None => Step::Nothing,
            };
        }
        // A repeat goes the way the key went down.
        let way = *slot.get_or_insert_with(|| {
            if chord(modifiers, system()) {
                Way::Batch
            } else {
                Way::Hid
            }
        });
        match way {
            Way::Hid => {
                self.keys.set(usage, true);
                Step::Hid
            }
            Way::Batch => Step::Batch,
        }
    }

    /// A key set as it stands, after a release ahead of it went.
    fn set(&mut self, usage: u8, down: bool) {
        self.keys.set(usage, down);
    }

    fn holds_delete(&self) -> bool {
        self.deletes.contains(&Some(Way::Hid))
    }

    fn let_go_of_deletes(&mut self) {
        for (slot, usage) in self.deletes.iter_mut().zip(DELETES) {
            if *slot == Some(Way::Hid) {
                *slot = None;
                self.keys.set(usage, false);
            }
        }
    }

    /// Let go of a Delete held here while Ctrl and Alt are held, by this
    /// keyboard or as the `system` says: answers whether one was.
    fn let_go_over(&mut self, system: (bool, bool)) -> bool {
        if !self.holds_delete() || !chord(self.modifiers(), system) {
            return false;
        }
        self.let_go_of_deletes();
        true
    }

    /// A consumer usage down or up.
    fn consumer(&mut self, usage: u16, down: bool) {
        let held = self.consumer.get(..self.consumer_held).unwrap_or_default();
        if let Some(at) = held.iter().position(|&key| key == usage) {
            self.consumer.copy_within(at + 1..self.consumer_held, at);
            self.consumer_held -= 1;
        }
        if down && let Some(slot) = self.consumer.get_mut(self.consumer_held) {
            *slot = usage;
            self.consumer_held += 1;
        }
    }

    fn consumer_now(&self) -> u16 {
        self.consumer_held
            .checked_sub(1)
            .and_then(|last| self.consumer.get(last).copied())
            .unwrap_or(0)
    }

    /// The keys report as it stands, taken as written.
    fn keys_now(&mut self) -> [u8; keyboard::KEYS_REPORT_LEN] {
        self.keys_written = self.keys;
        *self.keys.report()
    }

    /// The reports changed since last written, taken as written: the keys,
    /// then the consumer usage.
    fn changed(
        &mut self,
    ) -> (
        Option<[u8; keyboard::KEYS_REPORT_LEN]>,
        Option<[u8; keyboard::CONSUMER_REPORT_LEN]>,
    ) {
        let keys = (self.keys != self.keys_written).then(|| self.keys_now());
        let now = self.consumer_now();
        let consumer = if now == self.consumer_written {
            None
        } else {
            self.consumer_written = now;
            keyboard::consumer_report(now)
        };
        (keys, consumer)
    }
}

/// The guest's keyboard on the host's own input driver.
#[derive(Debug)]
struct HidKeyboard {
    device: hid::Device,
    slot: u8,
    keys: HidKeys,
    /// Whether the system lists it, from when what is written reaches the
    /// system.
    ready: bool,
    made: lowlat_common::clock::Time,
}

impl HidKeyboard {
    /// A keyboard in the first slot the driver gives, or the reason it gives
    /// none, said.
    fn make() -> Option<Self> {
        let (device, slot) = made(
            Kind::Keyboard,
            Features::new(),
            "keyboard",
            "keys go through the batch input call",
        )?;
        Some(Self {
            device,
            slot,
            keys: HidKeys::default(),
            ready: false,
            made: lowlat_common::clock::Time::now(),
        })
    }

    /// The reports that changed, in order.
    fn flush(&mut self) -> Result<(), hid::Error> {
        let (keys, consumer) = self.keys.changed();
        if let Some(report) = keys {
            self.device.write(&report)?;
        }
        if let Some(report) = consumer {
            self.device.write(&report)?;
        }
        Ok(())
    }

    /// The keys report as it stands, now.
    fn write_keys(&mut self) -> Result<(), hid::Error> {
        let report = self.keys.keys_now();
        self.device.write(&report)
    }

    fn close_aside(self) {
        close_aside(self.device);
    }
}

/// A device of `kind` answering `features` in the first slot the driver gives
/// that kind, with that slot, or the reason it gives none said: the device
/// `named`, and what happens `instead`.
fn made(kind: Kind, features: Features, named: &str, instead: &str) -> Option<(hid::Device, u8)> {
    let mut why = hid::Error::Absent;
    for slot in 0..u8::try_from(kind.slots()).unwrap_or(0) {
        match hid::Device::create(kind, slot, features) {
            Ok(device) => {
                lowlat_common::log_info!("inject: the input driver's {named} made, slot={slot}");
                return Some((device, slot));
            }
            // **The driver keeps a kind's slots across every handle**: a slot
            // another guest's device holds is refused as busy, and the next is
            // asked for.
            Err(error @ hid::Error::Refused(ERROR_BUSY)) => why = error,
            Err(error) => {
                why = error;
                break;
            }
        }
    }
    lowlat_common::log_warn!("inject: no {named} from the input driver, {instead}, error={why}");
    None
}

/// Let a device go off this thread: its end waits while its neutral reports
/// arrive, which the guest's loop is not held up by. A thread that cannot be
/// started lets it go here instead.
fn close_aside(device: hid::Device) {
    let _ = std::thread::Builder::new()
        .name("lowlat-hid-close".to_owned())
        .spawn(move || drop(device));
}

/// What a pointer report goes out as on the input driver's mouse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pointed {
    /// A report of the relative collection: the buttons, motion, the wheels.
    Relative([u8; mouse::RELATIVE_REPORT_LEN]),
    /// A report of the absolute collection: a position on the primary display.
    Absolute([u8; mouse::ABSOLUTE_REPORT_LEN]),
    /// A position on another display, over the whole desktop, as the batch
    /// input call takes it.
    Elsewhere(i32, i32),
}

/// The guest's pointer as the input driver's mouse carries it, apart from the
/// device: the buttons it holds, and what the report being read says.
///
/// **Every button and wheel rides the relative collection**, whichever of the
/// expander's two pointers it was sent to, and the absolute one carries
/// positions alone (docs/11-drivers.md section 2.3): each collection is a
/// pointer of its own to the system, with its own buttons, so one collection
/// holding them all is what lets a release reach the press it ends.
#[derive(Debug, Default)]
struct HidPointer {
    /// The relative collection's buttons, as held.
    buttons: u8,
    /// A button went down or up in the report being read.
    changed: bool,
    dx: i32,
    dy: i32,
    wheel: i32,
    pan: i32,
    position: Option<(i32, i32)>,
}

impl HidPointer {
    /// One event of the report being read. The wheels are taken in the units
    /// the peer counted, the mouse's multipliers making a count a hundred and
    /// twentieth of a notch; the stepped axes are the same turns again.
    fn take(&mut self, event: &Event) {
        let add = |to: &mut i32| *to = to.saturating_add(event.value);
        match (event.kind, event.code) {
            (event::EV_REL, event::REL_X) => add(&mut self.dx),
            (event::EV_REL, event::REL_Y) => add(&mut self.dy),
            (event::EV_REL, event::REL_WHEEL_HI_RES) => add(&mut self.wheel),
            (event::EV_REL, event::REL_HWHEEL_HI_RES) => add(&mut self.pan),
            (event::EV_ABS, event::ABS_X) => self.position.get_or_insert((0, 0)).0 = event.value,
            (event::EV_ABS, event::ABS_Y) => self.position.get_or_insert((0, 0)).1 = event.value,
            (event::EV_KEY, code) => {
                if let Some(bit) = button_bit(code) {
                    if event.value == 0 {
                        self.buttons &= !bit;
                    } else {
                        self.buttons |= bit;
                    }
                    self.changed = true;
                }
            }
            _ => {}
        }
    }

    /// The report read so far, as it goes out: a position first, on the
    /// absolute collection where `primary` places it there and elsewhere
    /// through the batch input call, then the relative collection's reports
    /// -- as many as the motion's range needs, one where only a button changed.
    fn ended<E>(
        &mut self,
        primary: impl Fn(i32, i32) -> Option<(u16, u16)>,
        mut out: impl FnMut(Pointed) -> Result<(), E>,
    ) -> Result<(), E> {
        if let Some((x, y)) = self.position.take() {
            out(match primary(x, y) {
                Some((x, y)) => Pointed::Absolute(mouse::absolute(x, y)),
                None => Pointed::Elsewhere(x, y),
            })?;
        }
        let moved = |p: &Self| (p.dx, p.dy, p.wheel, p.pan) != (0, 0, 0, 0);
        if !core::mem::take(&mut self.changed) && !moved(self) {
            return Ok(());
        }
        loop {
            let report = mouse::relative(
                self.buttons,
                step(&mut self.dx),
                step(&mut self.dy),
                step(&mut self.wheel),
                step(&mut self.pan),
            );
            out(Pointed::Relative(report))?;
            if !moved(self) {
                return Ok(());
            }
        }
    }
}

/// As much of `left` as one relative field carries, taken off it.
fn step(left: &mut i32) -> i16 {
    let max = i32::from(mouse::RELATIVE_MAX);
    let taken = (*left).clamp(-max, max);
    *left -= taken;
    i16::try_from(taken).unwrap_or(0)
}

/// The relative collection's bit for a button.
fn button_bit(code: u16) -> Option<u8> {
    Some(match code {
        event::BTN_LEFT => mouse::LEFT,
        event::BTN_RIGHT => mouse::RIGHT,
        event::BTN_MIDDLE => mouse::MIDDLE,
        event::BTN_SIDE => mouse::BACK,
        event::BTN_EXTRA => mouse::FORWARD,
        _ => return None,
    })
}

/// A position the expander spread over the whole desktop, `x` and `y` in the
/// batch input call's normalised range, as a value of the mouse's absolute
/// collection, which spans the primary display alone: `None` off it. The
/// desktop is `(left, top, width, height)` on the system's axes, the primary at
/// their origin `primary`'s sides.
///
/// **The pixel the batch input call would put the pointer on, then that
/// pixel's centre on the collection**: the system takes a value back to a
/// pixel as `v x W / 65536` over the desktop and as `v x W / 32768` on the
/// collection, truncated both, so a position lands where it would have either
/// way.
fn on_primary(
    x: i32,
    y: i32,
    desktop: (i32, i32, i32, i32),
    primary: (i32, i32),
) -> Option<(u16, u16)> {
    let (left, top, width, height) = desktop;
    let pixel = |value: i32, origin: i32, extent: i32| {
        i64::from(origin) + i64::from(value) * i64::from(extent) / 65536
    };
    let (px, py) = (pixel(x, left, width), pixel(y, top, height));
    let (pw, ph) = (i64::from(primary.0), i64::from(primary.1));
    if !(0..pw).contains(&px) || !(0..ph).contains(&py) {
        return None;
    }
    let value = |pixel: i64, extent: i64| {
        u16::try_from(((2 * pixel + 1) * 32768 / (2 * extent)).min(i64::from(mouse::ABSOLUTE_MAX)))
            .ok()
    };
    Some((value(px, pw)?, value(py, ph)?))
}

/// [`on_primary`] for the desktop as the system lays it out now: the metrics
/// of a per-monitor aware thread, in the physical pixels the batch input call
/// spreads a position over.
fn primary_place(x: i32, y: i32) -> Option<(u16, u16)> {
    // SAFETY: reads of the system's metrics.
    let metric = |index| unsafe { GetSystemMetrics(index) };
    on_primary(
        x,
        y,
        (
            metric(SM_XVIRTUALSCREEN),
            metric(SM_YVIRTUALSCREEN),
            metric(SM_CXVIRTUALSCREEN),
            metric(SM_CYVIRTUALSCREEN),
        ),
        (metric(SM_CXSCREEN), metric(SM_CYSCREEN)),
    )
}

/// The guest's mouse on the host's own input driver.
#[derive(Debug)]
struct HidMouse {
    device: hid::Device,
    slot: u8,
    pointer: HidPointer,
    /// Whether the system lists it, from when what is written reaches the
    /// system.
    ready: bool,
    made: lowlat_common::clock::Time,
}

impl HidMouse {
    /// A mouse in the first slot the driver gives, or the reason it gives none,
    /// said.
    fn make() -> Option<Self> {
        let (device, slot) = made(
            Kind::Mouse,
            Features::new(),
            "mouse",
            "the pointer goes through the batch input call",
        )?;
        Some(Self {
            device,
            slot,
            pointer: HidPointer::default(),
            ready: false,
            made: lowlat_common::clock::Time::now(),
        })
    }

    fn close_aside(self) {
        close_aside(self.device);
    }
}

/// A pad sent as its own reports, presented as its product on the host's own
/// input driver (docs/11-drivers.md section 2.3).
///
/// **Played on the bus from its own reports where its device never comes, or
/// goes**: its first report fixed it as a pad that drops the sixteen-button
/// states sent beside the reports, so otherwise it would do nothing at all.
#[derive(Debug)]
struct HidPad {
    /// The peer's identifier for it.
    id: u32,
    product: Product,
    /// What the peer sent ahead of its first input report, for its device's
    /// answers.
    features: answers::Features,
    /// From the first input report until it goes.
    device: Option<hid::Device>,
    /// The driver's slot for it.
    slot: u8,
    /// Whether the system lists it, from when what is written reaches the
    /// system.
    ready: bool,
    made: lowlat_common::clock::Time,
    /// The latest report while the device cannot deliver yet: each report is
    /// the whole pad, so the last is all that is owed.
    held: Option<[u8; pad::INPUT_LEN]>,
    /// Played on the bus from its reports.
    on_bus: bool,
}

impl HidPad {
    fn new(id: u32, product: Product) -> Self {
        Self {
            id,
            product,
            features: answers::Features::new(),
            device: None,
            slot: 0,
            ready: false,
            made: lowlat_common::clock::Time::now(),
            held: None,
            on_bus: false,
        }
    }

    /// The device, in the first slot the driver gives its kind, answering the
    /// table its peer's answers and its address make; false where the driver
    /// gives none, said.
    fn make(&mut self, address: [u8; 6]) -> bool {
        let Some(kind) = kind_of(self.product) else {
            return false;
        };
        let table = table(self.product, &self.features, &address);
        let Some((device, slot)) = made(kind, table, "pad", "it plays on the bus") else {
            return false;
        };
        self.device = Some(device);
        self.slot = slot;
        self.made = lowlat_common::clock::Time::now();
        true
    }

    /// One report into the device; false where it refuses, the device then
    /// let go and the pad on the bus.
    fn put(&mut self, report: &[u8; pad::INPUT_LEN]) -> bool {
        let Some(device) = self.device.as_mut() else {
            return false;
        };
        match device.write(report) {
            Ok(()) => true,
            Err(error) => {
                lowlat_common::log_warn!(
                    "inject: the input driver's pad is gone, it plays on the bus from its reports, pad={} error={error}",
                    self.id
                );
                self.leave_for_the_bus();
                false
            }
        }
    }

    /// Let the device go, off this thread, and play the pad on the bus.
    fn leave_for_the_bus(&mut self) {
        if let Some(device) = self.device.take() {
            close_aside(device);
        }
        self.on_bus = true;
    }
}

/// The input driver's kind for a product.
fn kind_of(product: Product) -> Option<Kind> {
    match product {
        Product::DualShock4 => Some(Kind::DualShock4),
        Product::DualSense => Some(Kind::DualSense),
        _ => None,
    }
}

/// What a pad's device answers the system's questions with: its calibration
/// and firmware -- the peer's, else a real pad's -- and its pairing around
/// this host's address for it, each identifier byte first, as the Linux host
/// answers each question as it is asked.
fn table(product: Product, features: &answers::Features, address: &[u8; 6]) -> Features {
    let mut table = Features::new();
    let mut out = [0u8; pad::FEATURE_MAX];
    for id in [
        Feature::Calibration.id(product),
        Feature::Firmware.id(product),
        answers::pairing_id(product),
    ] {
        if let Some(len) = features.answer(product, id, address, &mut out)
            && let Some(report) = out.get(..len)
        {
            // Three reports, fewer than a table holds and each within its
            // room, which the tests check.
            let _ = table.set(report);
        }
    }
    table
}

/// The bus pad's report for what a pad's own report says: the sixteen-button
/// state it carries, but for the touchpad's click, which the bus pad has no
/// button for, and Guide where the guest may not press it.
fn bus_report(product: Product, report: &[u8; pad::INPUT_LEN], guide: bool) -> gamepad::Report {
    let state = pad::state(product, report);
    let mut left_out = gamepad::bit::TOUCHPAD;
    if !guide {
        left_out |= gamepad::bit::GUIDE;
    }
    gamepad::Report {
        buttons: state.buttons & !left_out,
        lx: state.lx,
        ly: state.ly,
        rx: state.rx,
        ry: state.ry,
        left_trigger: state.lt,
        right_trigger: state.rt,
    }
}

/// What the driver says a program wrote, as the guest's loop hands it back.
fn handed_back(written: &control::Written) -> Written {
    let bytes = written.report();
    let mut report = [0u8; WRITTEN_MAX];
    let len = bytes.len().min(report.len());
    if let (Some(dst), Some(src)) = (report.get_mut(..len), bytes.get(..len)) {
        dst.copy_from_slice(src);
    }
    Written {
        kind: match written.kind {
            WrittenKind::Output => pad::OutputKind::Output,
            WrittenKind::Feature => pad::OutputKind::Feature,
        },
        len,
        report,
    }
}

/// A device's events while it cannot deliver yet.
///
/// **Held rather than dropped**: what falls in the wait is the first thing
/// somebody did. Bounded, and at the bound the oldest whole report goes,
/// never part of one -- half a report is a state change nobody asked for, and
/// losing a press whose release survives is the harmless direction.
#[derive(Debug)]
struct Held {
    events: [Event; HELD],
    used: usize,
    /// Set once, so an overflow is said rather than repeated.
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

    fn discard_oldest_report(&mut self) {
        let end = self
            .events
            .get(..self.used)
            .and_then(|held| held.iter().position(|e| e.kind == event::EV_SYN))
            .map_or(self.used, |at| at + 1);
        self.events.copy_within(end..self.used, 0);
        self.used -= end;
        if !self.reported {
            self.reported = true;
            lowlat_common::log_warn!(
                "inject: input held for an input driver's device overflowed, held={HELD}"
            );
        }
    }

    fn events(&self) -> &[Event] {
        self.events.get(..self.used).unwrap_or_default()
    }
}

fn key_input(stroke: Stroke, down: bool) -> INPUT {
    let up = if down { 0 } else { KEYEVENTF_KEYUP };
    let (virtual_key, scan, flags) = match stroke {
        Stroke::Scan(code) => {
            let prefix = if code & 0x80 == 0 {
                0
            } else {
                KEYEVENTF_EXTENDEDKEY
            };
            (0, u16::from(code & 0x7f), KEYEVENTF_SCANCODE | prefix)
        }
        Stroke::Make(code) => (0, u16::from(code), KEYEVENTF_SCANCODE),
        Stroke::Virtual(key) => (key, 0, 0),
    };
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: virtual_key,
                wScan: scan,
                dwFlags: flags | up,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// Whether a pointer's events move it by deltas, as an application holding
/// the pointer is moved.
fn moves_by_deltas(events: &[Event]) -> bool {
    events
        .iter()
        .any(|e| e.kind == event::EV_REL && (e.code == event::REL_X || e.code == event::REL_Y))
}

fn mouse_input(dx: i32, dy: i32, data: u32, flags: u32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// A pointer batch as the input call takes it: one move a report, a wheel a
/// turned axis, a button a press or a release.
///
/// **The fine wheel axes, not the stepped ones.** The stepped axis is the
/// fine one divided into detents, for the kernel's consumers; the call takes
/// the units the peer counted, and taking both would scroll twice.
fn pointer_inputs(events: &[Event], inputs: &mut Inputs) {
    let mut moved: Option<(i32, i32, u32)> = None;
    for e in events {
        match (e.kind, e.code) {
            (event::EV_REL, event::REL_X) => {
                moved.get_or_insert((0, 0, MOUSEEVENTF_MOVE)).0 = e.value
            }
            (event::EV_REL, event::REL_Y) => {
                moved.get_or_insert((0, 0, MOUSEEVENTF_MOVE)).1 = e.value
            }
            (event::EV_ABS, event::ABS_X) => moved.get_or_insert((0, 0, ABSOLUTE)).0 = e.value,
            (event::EV_ABS, event::ABS_Y) => moved.get_or_insert((0, 0, ABSOLUTE)).1 = e.value,
            (event::EV_REL, event::REL_WHEEL_HI_RES) => {
                inputs.push(mouse_input(0, 0, bits(e.value), MOUSEEVENTF_WHEEL));
            }
            (event::EV_REL, event::REL_HWHEEL_HI_RES) => {
                inputs.push(mouse_input(0, 0, bits(e.value), MOUSEEVENTF_HWHEEL));
            }
            (event::EV_KEY, code) => {
                if let Some((flags, data)) = button(code, e.value != 0) {
                    inputs.push(mouse_input(0, 0, data, flags));
                }
            }
            (event::EV_SYN, _) => {
                if let Some((dx, dy, flags)) = moved.take() {
                    inputs.push(mouse_input(dx, dy, 0, flags));
                }
            }
            _ => {}
        }
    }
}

/// A position over the whole desktop, which the expander has already placed
/// the captured output's picture in.
const ABSOLUTE: u32 = MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK;

/// The flags and data a button's press or release goes out as.
fn button(code: u16, down: bool) -> Option<(u32, u32)> {
    let pick = |pressed: u32, released: u32| if down { pressed } else { released };
    let x = pick(MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP);
    Some(match code {
        event::BTN_LEFT => (pick(MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP), 0),
        event::BTN_RIGHT => (pick(MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP), 0),
        event::BTN_MIDDLE => (pick(MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP), 0),
        event::BTN_SIDE => (x, u32::from(XBUTTON1)),
        event::BTN_EXTRA => (x, u32::from(XBUTTON2)),
        _ => return None,
    })
}

/// A signed wheel count in the call's unsigned field, bit for bit.
#[allow(
    clippy::cast_sign_loss,
    reason = "the field is signed to the system and unsigned in its declaration"
)]
const fn bits(value: i32) -> u32 {
    value as u32
}

/// One call's worth of input: the expander hands over no more events a
/// batch than this, and no event becomes more than one input.
struct Inputs {
    list: [INPUT; SCRATCH],
    used: usize,
}

impl Inputs {
    fn new() -> Self {
        Self {
            list: [mouse_input(0, 0, 0, 0); SCRATCH],
            used: 0,
        }
    }

    fn push(&mut self, input: INPUT) {
        if let Some(slot) = self.list.get_mut(self.used) {
            *slot = input;
            self.used += 1;
        }
    }

    fn as_slice(&self) -> &[INPUT] {
        self.list.get(..self.used).unwrap_or_default()
    }
}

/// This thread on the desktop that receives input, as it is now.
///
/// **Kept rather than closed**: a desktop a thread is attached to cannot be
/// closed, so the handle lives until the thread moves to another, and the one
/// it replaces is closed then.
#[derive(Debug)]
pub(crate) struct Desktop(HDESK);

impl Desktop {
    /// Refused on a thread that owns a window or a hook, and where this
    /// process may not open the desktop.
    pub(crate) fn attach() -> Option<Self> {
        // SAFETY: no flags, not inherited, every right; the handle is checked.
        let handle = unsafe { OpenInputDesktop(0, 0, GENERIC_ALL) };
        if handle.is_null() {
            return None;
        }
        // SAFETY: a desktop just opened.
        if unsafe { SetThreadDesktop(handle) } == 0 {
            // SAFETY: the handle opened above, attached to nothing.
            unsafe { CloseDesktop(handle) };
            return None;
        }
        Some(Self(handle))
    }
}

impl Drop for Desktop {
    fn drop(&mut self) {
        // **Fails while this thread is still attached to it**, which is the
        // ordinary case as a guest ends; the handle then goes with the thread.
        // SAFETY: the handle `attach` opened, closed once.
        unsafe { CloseDesktop(self.0) };
    }
}

/// This thread made per-monitor aware until the guard drops, which puts
/// back what it was.
#[derive(Debug)]
struct Aware(DPI_AWARENESS_CONTEXT);

impl Aware {
    fn per_monitor() -> Self {
        // SAFETY: a context the system defines; the call changes only this
        // thread's.
        Self(unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) })
    }
}

impl Drop for Aware {
    fn drop(&mut self) {
        // A null context means the call above refused, and there is nothing
        // to put back.
        if !self.0.is_null() {
            // SAFETY: the context this thread had, set back on it.
            unsafe { SetThreadDpiAwarenessContext(self.0) };
        }
    }
}

/// The calling thread's last error.
fn last_error() -> u32 {
    std::io::Error::last_os_error()
        .raw_os_error()
        .and_then(|code| u32::try_from(code).ok())
        .unwrap_or(0)
}

/// Why the pad bus could not be had.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BusError {
    /// No bus is installed, or none is running.
    Absent,
    /// A bus is there and would not be opened.
    Refused(u32),
    /// A bus is there and speaks another version of its requests.
    Version(u32),
}

impl core::fmt::Display for BusError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Absent => f.write_str("no bus device"),
            Self::Refused(code) => write!(f, "the bus would not open, error={code}"),
            Self::Version(code) => write!(f, "the bus refused this version, error={code}"),
        }
    }
}

/// The pad bus, opened for one guest.
///
/// **Closing it unplugs every pad it plugged in**, which is how a guest that
/// leaves takes its pads with it whatever path it leaves by.
#[derive(Debug)]
struct Bus {
    handle: HANDLE,
    /// For the requests this thread waits on.
    event: HANDLE,
}

impl Bus {
    fn open() -> Result<Self, BusError> {
        let mut last = BusError::Absent;
        for path in bus_paths() {
            // SAFETY: a NUL-terminated path; no security attributes and no
            // template, as the call allows.
            let handle = unsafe {
                CreateFileW(
                    path.as_ptr(),
                    GENERIC_READ | GENERIC_WRITE,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    core::ptr::null(),
                    OPEN_EXISTING,
                    FILE_FLAG_OVERLAPPED,
                    core::ptr::null_mut(),
                )
            };
            if handle == INVALID_HANDLE_VALUE {
                last = BusError::Refused(last_error());
                continue;
            }
            let Some(event) = event() else {
                last = BusError::Refused(last_error());
                // SAFETY: the handle opened above, closed once.
                unsafe { CloseHandle(handle) };
                continue;
            };
            let bus = Self { handle, event };
            let version = vigem::VIGEM_CHECK_VERSION {
                Size: size::<vigem::VIGEM_CHECK_VERSION>(),
                Version: vigem::VIGEM_COMMON_VERSION,
            };
            match bus.call(vigem::IOCTL_VIGEM_CHECK_VERSION, &version) {
                Ok(()) => return Ok(bus),
                Err(code) => last = BusError::Version(code),
            }
        }
        Err(last)
    }

    /// One request this thread waits for, with nothing to read back.
    fn call<T>(&self, code: u32, body: &T) -> Result<(), u32> {
        let mut overlapped = OVERLAPPED {
            hEvent: self.event,
            ..OVERLAPPED::default()
        };
        let mut moved = 0u32;
        // SAFETY: the body and the overlapped outlive the request, which is
        // waited for below before either goes; the handle is this bus's.
        let done = unsafe {
            DeviceIoControl(
                self.handle,
                code,
                core::ptr::from_ref(body).cast(),
                size::<T>(),
                core::ptr::null_mut(),
                0,
                &raw mut moved,
                &raw mut overlapped,
            )
        };
        if done != 0 {
            return Ok(());
        }
        let error = last_error();
        if error != ERROR_IO_PENDING {
            return Err(error);
        }
        // SAFETY: the request above, on this handle, with its own event.
        if unsafe { GetOverlappedResult(self.handle, &raw const overlapped, &raw mut moved, 1) }
            == 0
        {
            return Err(last_error());
        }
        Ok(())
    }
}

impl Drop for Bus {
    fn drop(&mut self) {
        // SAFETY: the two handles `open` made, closed once each.
        unsafe {
            CloseHandle(self.handle);
            CloseHandle(self.event);
        }
    }
}

/// A manual-reset event, for a request to be waited on.
fn event() -> Option<HANDLE> {
    // SAFETY: no security attributes and no name, manual reset, not set.
    let event = unsafe { CreateEventW(core::ptr::null(), 1, 0, core::ptr::null()) };
    (!event.is_null()).then_some(event)
}

/// The device paths of every pad bus present, each NUL-terminated.
fn bus_paths() -> Vec<Vec<u16>> {
    let mut length = 0u32;
    // SAFETY: the length is a live local; the identifier is a constant.
    let asked = unsafe {
        CM_Get_Device_Interface_List_SizeW(
            &raw mut length,
            &vigem::GUID_DEVINTERFACE_BUSENUM_VIGEM,
            core::ptr::null(),
            CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
        )
    };
    if asked != CR_SUCCESS || length <= 1 {
        return Vec::new();
    }
    let mut list = vec![0u16; length as usize];
    // SAFETY: the buffer is `length` characters, as the call is told.
    let listed = unsafe {
        CM_Get_Device_Interface_ListW(
            &vigem::GUID_DEVINTERFACE_BUSENUM_VIGEM,
            core::ptr::null(),
            list.as_mut_ptr(),
            length,
            CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
        )
    };
    if listed != CR_SUCCESS {
        return Vec::new();
    }
    list.split(|&c| c == 0)
        .filter(|path| !path.is_empty())
        .map(|path| path.iter().copied().chain([0]).collect())
        .collect()
}

/// A request the bus keeps until it has something to say: that a pad is
/// ready, or what it was asked to vibrate at.
///
/// **On the heap, and never moved or freed while the bus holds it**: the bus
/// writes its answer into the body and the overlapped whenever it comes.
/// Dropped while the bus holds it, it is cancelled and waited for first.
struct Pending<T> {
    overlapped: OVERLAPPED,
    body: T,
    handle: HANDLE,
    state: State,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Idle,
    Held,
    Answered(Result<(), u32>),
}

impl<T> Pending<T> {
    fn new(body: T) -> Option<Box<Self>> {
        Some(Box::new(Self {
            overlapped: OVERLAPPED {
                hEvent: event()?,
                ..OVERLAPPED::default()
            },
            body,
            handle: core::ptr::null_mut(),
            state: State::Idle,
        }))
    }

    /// Ask, the body going in and, where `back`, the answer coming out in it.
    /// Each ask starts from a cleared overlapped, as the system asks of one
    /// handed to it again; its event is kept.
    fn ask(&mut self, bus: &Bus, code: u32, back: bool) {
        self.handle = bus.handle;
        self.overlapped = OVERLAPPED {
            hEvent: self.overlapped.hEvent,
            ..OVERLAPPED::default()
        };
        let size = size::<T>();
        let body = core::ptr::from_mut(&mut self.body).cast::<core::ffi::c_void>();
        let mut moved = 0u32;
        // SAFETY: the body and the overlapped are this boxed request's and
        // stay put until it is answered or cancelled and waited for.
        let done = unsafe {
            DeviceIoControl(
                bus.handle,
                code,
                body,
                size,
                if back { body } else { core::ptr::null_mut() },
                if back { size } else { 0 },
                &raw mut moved,
                &raw mut self.overlapped,
            )
        };
        self.state = if done != 0 {
            State::Answered(Ok(()))
        } else {
            match last_error() {
                ERROR_IO_PENDING => State::Held,
                error => State::Answered(Err(error)),
            }
        };
    }

    /// The answer, once there is one, taken.
    fn answer(&mut self) -> Option<Result<(), u32>> {
        match self.state {
            State::Idle => None,
            State::Answered(result) => {
                self.state = State::Idle;
                Some(result)
            }
            State::Held => {
                let mut moved = 0u32;
                // SAFETY: the request is this one's, on the handle it went to.
                let done = unsafe {
                    GetOverlappedResult(self.handle, &raw const self.overlapped, &raw mut moved, 0)
                };
                let result = if done != 0 {
                    Ok(())
                } else {
                    match last_error() {
                        ERROR_IO_INCOMPLETE => return None,
                        error => Err(error),
                    }
                };
                self.state = State::Idle;
                Some(result)
            }
        }
    }
}

impl<T> Drop for Pending<T> {
    fn drop(&mut self) {
        if self.state == State::Held {
            let mut moved = 0u32;
            // SAFETY: the request is this one's; it is waited for until the
            // bus lets it go, after which nothing writes to it.
            unsafe {
                CancelIoEx(self.handle, &raw const self.overlapped);
                GetOverlappedResult(self.handle, &raw const self.overlapped, &raw mut moved, 1);
            }
        }
        // SAFETY: the event `new` made, closed once.
        unsafe { CloseHandle(self.overlapped.hEvent) };
    }
}

/// One pad on the bus, addressed by the identifier its peer gave it.
struct Pad {
    id: u32,
    /// The bus's number for it.
    serial: u32,
    /// The whole pad as it stands, folded from its events; **held until the
    /// bus says the pad is ready**, since each report is the whole pad and
    /// the last is all that is owed.
    report: gamepad::Report,
    ready: bool,
    waiting: Box<Pending<vigem::VIGEM_WAIT_DEVICE_READY>>,
    asked: Box<Pending<vigem::XUSB_REQUEST_NOTIFICATION>>,
    /// What was last reported, so an effect replayed at the same strength
    /// does not put another message on the wire.
    last_rumble: (u8, u8),
    /// Set once, so a report the bus refuses is said rather than repeated.
    refused: bool,
    plugged: lowlat_common::clock::Time,
}

impl Pad {
    /// Plug a pad in, at the first number the bus will give it, and ask to
    /// be told when it is ready.
    fn plug(bus: &Bus, id: u32) -> Result<Self, u32> {
        let mut last = 0;
        for serial in 1..=SERIALS {
            let target = vigem::VIGEM_PLUGIN_TARGET {
                Size: size::<vigem::VIGEM_PLUGIN_TARGET>(),
                SerialNo: serial,
                TargetType: vigem::Xbox360Wired,
                VendorId: PAD_VENDOR,
                ProductId: PAD_PRODUCT,
            };
            match bus.call(vigem::IOCTL_VIGEM_PLUGIN_TARGET, &target) {
                Ok(()) => {
                    let (Some(mut waiting), Some(asked)) = (
                        Pending::new(vigem::VIGEM_WAIT_DEVICE_READY {
                            Size: size::<vigem::VIGEM_WAIT_DEVICE_READY>(),
                            SerialNo: serial,
                        }),
                        Pending::new(notification(serial)),
                    ) else {
                        let error = last_error();
                        unplug(bus, serial);
                        return Err(error);
                    };
                    waiting.ask(bus, vigem::IOCTL_VIGEM_WAIT_DEVICE_READY, false);
                    return Ok(Self {
                        id,
                        serial,
                        report: gamepad::Report::default(),
                        ready: false,
                        waiting,
                        asked,
                        last_rumble: (0, 0),
                        refused: false,
                        plugged: lowlat_common::clock::Time::now(),
                    });
                }
                Err(error) => last = error,
            }
        }
        Err(last)
    }

    /// Whether the pad still stands: once the bus says it is ready, reports
    /// go in from the next one and the first ask for its rumble goes out. A
    /// bus too old to say counts as ready, as its pads are once plugged in.
    fn settle(&mut self, bus: &Bus) -> bool {
        if self.ready {
            return true;
        }
        match self.waiting.answer() {
            None => true,
            Some(Ok(()) | Err(ERROR_INVALID_PARAMETER)) => {
                self.ready = true;
                lowlat_common::log_info!(
                    "inject: pad ready, pad={} after_ms={:.0}",
                    self.id,
                    lowlat_common::clock::elapsed_ms(self.plugged)
                );
                self.asked
                    .ask(bus, vigem::IOCTL_XUSB_REQUEST_NOTIFICATION, true);
                true
            }
            Some(Err(error)) => {
                lowlat_common::log_warn!(
                    "inject: pad never became ready, pad={} error={error}",
                    self.id
                );
                unplug(bus, self.serial);
                false
            }
        }
    }

    /// Put the pad's report in, once it is ready.
    fn submit(&mut self, bus: &Bus) {
        if !self.ready {
            return;
        }
        let report = vigem::XUSB_SUBMIT_REPORT {
            Size: size::<vigem::XUSB_SUBMIT_REPORT>(),
            SerialNo: self.serial,
            Report: vigem::XUSB_REPORT {
                wButtons: self.report.buttons,
                bLeftTrigger: self.report.left_trigger,
                bRightTrigger: self.report.right_trigger,
                sThumbLX: self.report.lx,
                sThumbLY: self.report.ly,
                sThumbRX: self.report.rx,
                sThumbRY: self.report.ry,
            },
        };
        if let Err(error) = bus.call(vigem::IOCTL_XUSB_SUBMIT_REPORT, &report)
            && !self.refused
        {
            self.refused = true;
            lowlat_common::log_warn!("inject: pad report refused, pad={} error={error}", self.id);
        }
    }

    /// What the pad was asked to vibrate at, when that changed, and the next
    /// ask sent at once so no change falls between two.
    fn rumbled(&mut self, bus: &Bus) -> Option<Rumble> {
        match self.asked.answer()? {
            Ok(()) => {
                let heard = (self.asked.body.LargeMotor, self.asked.body.SmallMotor);
                self.asked.body = notification(self.serial);
                self.asked
                    .ask(bus, vigem::IOCTL_XUSB_REQUEST_NOTIFICATION, true);
                if heard == self.last_rumble {
                    return None;
                }
                self.last_rumble = heard;
                Some(Rumble {
                    pad: self.id,
                    large: heard.0,
                    small: heard.1,
                })
            }
            Err(error) => {
                lowlat_common::log_warn!(
                    "inject: pad rumble no longer heard, pad={} error={error}",
                    self.id
                );
                None
            }
        }
    }

    /// Take the pad out of the bus, its requests let go first.
    fn unplug(self, bus: &Bus) {
        let serial = self.serial;
        drop(self);
        unplug(bus, serial);
    }
}

fn notification(serial: u32) -> vigem::XUSB_REQUEST_NOTIFICATION {
    vigem::XUSB_REQUEST_NOTIFICATION {
        Size: size::<vigem::XUSB_REQUEST_NOTIFICATION>(),
        SerialNo: serial,
        LargeMotor: 0,
        SmallMotor: 0,
        LedNumber: 0,
    }
}

fn unplug(bus: &Bus, serial: u32) {
    let target = vigem::VIGEM_UNPLUG_TARGET {
        Size: size::<vigem::VIGEM_UNPLUG_TARGET>(),
        SerialNo: serial,
    };
    if let Err(error) = bus.call(vigem::IOCTL_VIGEM_UNPLUG_TARGET, &target) {
        lowlat_common::log_warn!("inject: pad not unplugged, serial={serial} error={error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pen_at(pen: bool, rubber: bool, touching: bool) -> PenState {
        PenState {
            pen,
            rubber,
            touching,
            ..PenState::default()
        }
    }

    /// **Each change of a pen is the frame the system's pen takes**: in range,
    /// down, moving, lifted to hovering, lifted out of range, out of range; and
    /// nothing for a pen that was out of range and stays so.
    #[test]
    fn a_pens_change_is_its_frames_flags() {
        let out = PenState::default();
        let hover = pen_at(true, false, false);
        let down = pen_at(true, false, true);
        let eraser = pen_at(false, true, true);
        let contact = POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT;
        for (sent, now, flags) in [
            (out, hover, Some(POINTER_FLAG_INRANGE | POINTER_FLAG_UPDATE)),
            (hover, down, Some(contact | POINTER_FLAG_DOWN)),
            (out, down, Some(contact | POINTER_FLAG_DOWN)),
            (down, down, Some(contact | POINTER_FLAG_UPDATE)),
            (down, eraser, Some(contact | POINTER_FLAG_UPDATE)),
            (down, hover, Some(POINTER_FLAG_INRANGE | POINTER_FLAG_UP)),
            (down, out, Some(POINTER_FLAG_UP)),
            (hover, out, Some(POINTER_FLAG_UPDATE)),
            (out, out, None),
            // Touching with neither end in range is not touching.
            (out, pen_at(false, false, true), None),
        ] {
            assert_eq!(pen_flags(&sent, &now), flags, "{sent:?} -> {now:?}");
        }
    }

    /// **The pixel is the one the expander aimed at**: an axis value aimed at
    /// a pixel's centre comes back as that pixel, on a desktop whose corner is
    /// left of the primary display.
    #[test]
    fn an_axis_value_comes_back_as_its_pixel() {
        let desktop = (-1920, -200, 4480, 1640);
        let aimed = |pixel: i32, extent: i32| (2 * pixel + 1) * 65536 / (2 * extent);
        for (px, py) in [(0, 0), (1, 1), (2239, 819), (4479, 1639)] {
            assert_eq!(
                desktop_pixel(aimed(px, 4480), aimed(py, 1640), desktop),
                (-1920 + px, -200 + py)
            );
        }
    }

    /// **A frame says what the pen is doing and no more**: pressure only
    /// while touching, held to its range; the turn and both tilts always,
    /// held to theirs; the barrel button; the eraser end in range turned over
    /// and, touching, the eraser.
    #[test]
    fn a_pens_frame_holds_its_parts_to_their_ranges() {
        let frame = |now: PenState| {
            let info = pen_info(POINTER_FLAG_INRANGE, &now, (7, 8));
            // SAFETY: the frame is a pen's, as this test made it.
            let pen = unsafe { info.Anonymous.penInfo };
            (
                info.r#type,
                pen.pointerInfo.ptPixelLocation.x,
                pen.pointerInfo.ptPixelLocation.y,
                pen.penFlags,
                pen.penMask,
                pen.pressure,
                pen.rotation,
                pen.tiltX,
                pen.tiltY,
            )
        };
        let tilted = PenState {
            pressure: 5000,
            rotation: 400,
            tilt_x: -120,
            tilt_y: 120,
            barrel: true,
            ..pen_at(true, false, false)
        };
        let all_but_pressure = PEN_MASK_ROTATION | PEN_MASK_TILT_X | PEN_MASK_TILT_Y;
        assert_eq!(
            frame(tilted),
            (
                PT_PEN,
                7,
                8,
                PEN_FLAG_BARREL,
                all_but_pressure,
                0,
                359,
                -90,
                90
            )
        );
        let touching = PenState {
            touching: true,
            ..tilted
        };
        assert_eq!(frame(touching).4, all_but_pressure | PEN_MASK_PRESSURE);
        assert_eq!(frame(touching).5, 1024);
        assert_eq!(frame(pen_at(false, true, false)).3, PEN_FLAG_INVERTED);
        assert_eq!(
            frame(pen_at(false, true, true)).3,
            PEN_FLAG_INVERTED | PEN_FLAG_ERASER
        );
    }

    /// **The expander's reports make the frames a stroke needs**, the double
    /// click as an up and a down: what a pen's messages put on this sink's
    /// own state, frame by frame.
    #[test]
    fn a_pens_reports_make_a_strokes_frames() {
        use lowlat_core::control::{Control, op};
        use lowlat_core::pen::{Pen, flag};

        #[derive(Default)]
        struct Frames {
            sent: PenState,
            now: PenState,
            flags: Vec<Option<POINTER_FLAGS>>,
        }
        impl Sink for Frames {
            fn emit(&mut self, device: Device, events: &[Event]) {
                assert_eq!(device, Device::Pen);
                for event in events {
                    if event.kind == event::EV_SYN {
                        self.flags.push(pen_flags(&self.sent, &self.now));
                        self.sent = self.now;
                    } else {
                        self.now.take(event);
                    }
                }
            }
        }

        let mut inject = event::Injector::new(event::Extents::alone(1920, 1080));
        let mut out = Frames::default();
        for flags in [
            0,
            flag::TOUCHING,
            flag::TOUCHING,
            flag::TOUCHING | flag::DOUBLE_CLICK,
            0,
            flag::LEAVE,
        ] {
            let (a0, a1, a2) = Pen {
                x: 100,
                y: 100,
                flags,
                ..Pen::default()
            }
            .arguments();
            let message = Control {
                a0,
                a1,
                a2,
                opcode: op::PEN_TOUCH,
                body: &[],
            };
            inject.on_control(&message, &mut out);
        }
        let contact = POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT;
        assert_eq!(
            out.flags,
            vec![
                Some(POINTER_FLAG_INRANGE | POINTER_FLAG_UPDATE),
                Some(contact | POINTER_FLAG_DOWN),
                Some(contact | POINTER_FLAG_UPDATE),
                Some(contact | POINTER_FLAG_UPDATE),
                Some(POINTER_FLAG_INRANGE | POINTER_FLAG_UP),
                Some(contact | POINTER_FLAG_DOWN),
                Some(POINTER_FLAG_INRANGE | POINTER_FLAG_UP),
                Some(POINTER_FLAG_UPDATE),
            ]
        );
    }

    /// **A stroke reaches a window as a pen's**: down on the pixel asked,
    /// held a second and a half on the refresh alone -- the system lifts a pen
    /// it hears nothing from after half of one -- then lifted; and a touch
    /// cancelled as the device goes reaches the window as cancelled, where a
    /// lift would not have. The window is the test's own, over the primary
    /// display's centre, and never takes the focus.
    #[test]
    #[ignore = "puts a pen on the desk, over a window of the test's own"]
    fn a_pens_stroke_reaches_a_window() {
        use std::sync::Mutex;
        use std::time::{Duration, Instant};
        use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
        use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows_sys::Win32::UI::Input::Pointer::{GetPointerInfo, POINTER_FLAG_CANCELED};
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, MSG, PM_REMOVE,
            PeekMessageW, RegisterClassExW, TranslateMessage, UnregisterClassW, WM_POINTERACTIVATE,
            WM_POINTERDOWN, WM_POINTERUP, WM_POINTERUPDATE, WNDCLASSEXW, WS_EX_NOACTIVATE,
            WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP, WS_VISIBLE, WindowFromPoint,
        };

        /// One pointer message the window was sent, and when.
        #[derive(Debug, Clone, Copy)]
        struct Seen {
            at: Instant,
            msg: u32,
            flags: u32,
            x: i32,
            y: i32,
        }
        static SEEN: Mutex<Vec<Seen>> = Mutex::new(Vec::new());

        unsafe extern "system" fn window(
            hwnd: HWND,
            msg: u32,
            wparam: WPARAM,
            lparam: LPARAM,
        ) -> LRESULT {
            const PA_NOACTIVATE: LRESULT = 3;
            if msg == WM_POINTERACTIVATE {
                return PA_NOACTIVATE;
            }
            if msg == WM_POINTERUPDATE {
                return 0;
            }
            if msg == WM_POINTERDOWN || msg == WM_POINTERUP {
                // SAFETY: a structure the call fills, for the pointer the
                // message names.
                let mut info: POINTER_INFO = unsafe { core::mem::zeroed() };
                let id = u32::try_from(wparam & 0xFFFF).unwrap_or(0);
                if unsafe { GetPointerInfo(id, &mut info) } != 0 {
                    SEEN.lock().unwrap().push(Seen {
                        at: Instant::now(),
                        msg,
                        flags: info.pointerFlags,
                        x: info.ptPixelLocation.x,
                        y: info.ptPixelLocation.y,
                    });
                }
                return 0;
            }
            // SAFETY: the system's own handling of everything else.
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }

        let mut devices = Devices::create("pen test").unwrap();
        let class: Vec<u16> = "lowlat-pen-test\0".encode_utf16().collect();
        // SAFETY: this module's own instance, a class of the test's own, and
        // a window of it that the test destroys.
        let (instance, hwnd) = unsafe {
            let instance = GetModuleHandleW(core::ptr::null());
            let class_info = WNDCLASSEXW {
                cbSize: size::<WNDCLASSEXW>(),
                style: 0,
                lpfnWndProc: Some(window),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: instance,
                hIcon: core::ptr::null_mut(),
                hCursor: core::ptr::null_mut(),
                hbrBackground: core::ptr::null_mut(),
                lpszMenuName: core::ptr::null(),
                lpszClassName: class.as_ptr(),
                hIconSm: core::ptr::null_mut(),
            };
            assert_ne!(RegisterClassExW(&class_info), 0);
            let (cx, cy) = (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN));
            let hwnd = CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                class.as_ptr(),
                class.as_ptr(),
                WS_POPUP | WS_VISIBLE,
                cx / 2 - 160,
                cy / 2 - 160,
                320,
                320,
                core::ptr::null_mut(),
                core::ptr::null_mut(),
                instance,
                core::ptr::null(),
            );
            assert!(!hwnd.is_null());
            (instance, hwnd)
        };
        let pump = |ms: u64| {
            let until = Instant::now() + Duration::from_millis(ms);
            // SAFETY: a message structure the call fills, dispatched on the
            // thread that owns the window.
            let mut msg: MSG = unsafe { core::mem::zeroed() };
            while Instant::now() < until {
                while unsafe { PeekMessageW(&mut msg, core::ptr::null_mut(), 0, 0, PM_REMOVE) } != 0
                {
                    unsafe {
                        TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        };
        pump(300);

        // SAFETY: reads of the system's metrics.
        let centre = unsafe {
            (
                GetSystemMetrics(SM_CXSCREEN) / 2,
                GetSystemMetrics(SM_CYSCREEN) / 2,
            )
        };
        // SAFETY: which window is at a point.
        let under = unsafe {
            WindowFromPoint(POINT {
                x: centre.0,
                y: centre.1,
            })
        };
        assert_eq!(under, hwnd, "the test's window is not at the centre");
        let (left, top, width, height) = desktop_now();
        let aimed = |pixel: i32, origin: i32, extent: i32| {
            (2 * (pixel - origin) + 1) * 65536 / (2 * extent)
        };
        let report = |in_range: bool, touching: bool| {
            let event = |kind, code, value| Event { kind, code, value };
            [
                event(event::EV_ABS, event::ABS_X, aimed(centre.0, left, width)),
                event(event::EV_ABS, event::ABS_Y, aimed(centre.1, top, height)),
                event(event::EV_ABS, event::ABS_PRESSURE, 512),
                event(event::EV_KEY, event::BTN_TOOL_PEN, i32::from(in_range)),
                event(event::EV_KEY, event::BTN_TOUCH, i32::from(touching)),
                event(event::EV_SYN, 0, 0),
            ]
        };

        devices.emit(Device::Pen, &report(true, false));
        pump(30);
        devices.emit(Device::Pen, &report(true, true));
        let held = Instant::now();
        while held.elapsed() < Duration::from_millis(1500) {
            devices.tick();
            pump(20);
        }
        devices.emit(Device::Pen, &report(true, false));
        pump(100);
        devices.emit(Device::Pen, &report(false, false));
        pump(100);
        devices.emit(Device::Pen, &report(true, true));
        pump(100);
        devices.cancel(Device::Pen);
        pump(300);

        // SAFETY: the test's own window and class, as made above.
        unsafe {
            DestroyWindow(hwnd);
            UnregisterClassW(class.as_ptr(), instance);
        }
        let seen = SEEN.lock().unwrap().clone();
        let kinds: Vec<(u32, bool)> = seen
            .iter()
            .map(|seen| (seen.msg, seen.flags & POINTER_FLAG_CANCELED != 0))
            .collect();
        assert_eq!(
            kinds,
            vec![
                (WM_POINTERDOWN, false),
                (WM_POINTERUP, false),
                (WM_POINTERDOWN, false),
                (WM_POINTERUP, true),
            ],
            "{seen:?}"
        );
        assert_eq!(
            (seen[0].x, seen[0].y),
            centre,
            "the pen went down elsewhere"
        );
        let lifted = seen[1].at.duration_since(seen[0].at);
        assert!(
            lifted >= Duration::from_millis(1400),
            "the pen was lifted {lifted:?} after it went down"
        );
    }

    /// **Every frame carries every contact the surface holds**: the expander's
    /// reports for two contacts, through this sink's own slots -- the first
    /// down, the second down beside it, the first lifted, the second moved --
    /// as the contacts and flags of each frame, by slot.
    #[test]
    fn every_touch_frame_carries_every_contact() {
        use lowlat_core::control::{Control, op};
        use lowlat_core::touch::{Touch, flag};

        #[derive(Default)]
        struct Frames {
            touches: Touches,
            frames: Vec<Vec<(u32, POINTER_FLAGS)>>,
        }
        impl Sink for Frames {
            fn emit(&mut self, device: Device, events: &[Event]) {
                assert_eq!(device, Device::Touch);
                for event in events {
                    if event.kind != event::EV_SYN {
                        self.touches.take(event);
                        continue;
                    }
                    let (frame, count) =
                        touch_frame(&self.touches.sent, &self.touches.now, (0, 0, 1920, 1080));
                    self.touches.sent = self.touches.now;
                    self.frames.push(
                        frame[..count]
                            .iter()
                            .map(|info| {
                                // SAFETY: a touch frame, as `touch_frame` made it.
                                let pointer = unsafe { info.Anonymous.touchInfo.pointerInfo };
                                (pointer.pointerId, pointer.pointerFlags)
                            })
                            .collect(),
                    );
                }
            }
        }

        let mut inject = event::Injector::new(event::Extents::alone(1920, 1080));
        let mut out = Frames::default();
        for (id, flags) in [
            (3, flag::TOUCHING),
            (7, flag::TOUCHING),
            (3, flag::LEAVE),
            (7, flag::TOUCHING),
        ] {
            let (a0, a1, a2) = Touch {
                id,
                x: 100,
                y: 100,
                flags,
                ..Touch::default()
            }
            .arguments();
            let message = Control {
                a0,
                a1,
                a2,
                opcode: op::PEN_TOUCH,
                body: &[],
            };
            inject.on_control(&message, &mut out);
        }
        let contact = POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT;
        assert_eq!(
            out.frames,
            vec![
                vec![(0, contact | POINTER_FLAG_DOWN)],
                vec![
                    (0, contact | POINTER_FLAG_UPDATE),
                    (1, contact | POINTER_FLAG_DOWN)
                ],
                vec![(0, POINTER_FLAG_UP), (1, contact | POINTER_FLAG_UPDATE)],
                vec![(1, contact | POINTER_FLAG_UPDATE)],
            ]
        );
    }

    /// **A contact's frame says what it is doing and no more**: its number is
    /// its slot's; pressure and the area only while touching, the area centred
    /// on it where both sides were reported; the orientation where it was.
    #[test]
    fn a_contacts_frame_holds_what_it_reported() {
        let frame = |flags, slot: TouchSlot| {
            let info = touch_info(4, flags, &slot, (0, 0, 65536, 65536));
            // SAFETY: a touch frame, as `touch_info` made it.
            let touch = unsafe { info.Anonymous.touchInfo };
            (
                touch.pointerInfo.pointerId,
                touch.touchMask,
                (
                    touch.rcContact.left,
                    touch.rcContact.top,
                    touch.rcContact.right,
                    touch.rcContact.bottom,
                ),
                touch.pressure,
                touch.orientation,
            )
        };
        let touching = TouchSlot {
            tracking: 1,
            x: 100,
            y: 200,
            pressure: 5000,
            major: 10,
            minor: 6,
            orientation: 400,
            hovering: false,
        };
        let contact = POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT | POINTER_FLAG_UPDATE;
        assert_eq!(
            frame(contact, touching),
            (
                4,
                TOUCH_MASK_PRESSURE | TOUCH_MASK_CONTACTAREA | TOUCH_MASK_ORIENTATION,
                (95, 197, 105, 203),
                1024,
                359
            )
        );
        let unreported = TouchSlot {
            major: 0,
            orientation: -1,
            ..touching
        };
        assert_eq!(frame(contact, unreported).1, TOUCH_MASK_PRESSURE);
        let hovering = POINTER_FLAG_INRANGE | POINTER_FLAG_UPDATE;
        assert_eq!(
            frame(hovering, unreported),
            (4, TOUCH_MASK_NONE, (0, 0, 0, 0), 0, 0)
        );
    }

    /// **Two fingers reach a window as touch**: both down on the pixels asked
    /// and dragged a second and a half, moved only every 600 ms -- longer than
    /// the system keeps a contact it hears nothing from, so the refresh alone
    /// holds them between -- neither lifted until it is lifted, the first with
    /// the second still down; and two more cancelled together as the device
    /// goes, each seen as cancelled. **They move because the system holds a
    /// still contact's messages back until it moves or lifts**, and one still
    /// for most of a second is its own press and hold, never the window's. The
    /// window is the test's own, over the primary display's centre, and never
    /// takes the focus.
    #[test]
    #[ignore = "puts two fingers on the desk, over a window of the test's own"]
    fn two_fingers_reach_a_window() {
        use std::sync::Mutex;
        use std::time::{Duration, Instant};
        use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
        use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows_sys::Win32::UI::Input::Pointer::{GetPointerInfo, POINTER_FLAG_CANCELED};
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, MSG, PM_REMOVE,
            PeekMessageW, RegisterClassExW, TranslateMessage, UnregisterClassW, WM_POINTERACTIVATE,
            WM_POINTERDOWN, WM_POINTERUP, WM_POINTERUPDATE, WNDCLASSEXW, WS_EX_NOACTIVATE,
            WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP, WS_VISIBLE, WindowFromPoint,
        };

        /// One pointer message the window was sent, and when.
        #[derive(Debug, Clone, Copy)]
        struct Seen {
            at: Instant,
            msg: u32,
            flags: u32,
            x: i32,
            y: i32,
        }
        static SEEN: Mutex<Vec<Seen>> = Mutex::new(Vec::new());

        unsafe extern "system" fn window(
            hwnd: HWND,
            msg: u32,
            wparam: WPARAM,
            lparam: LPARAM,
        ) -> LRESULT {
            const PA_NOACTIVATE: LRESULT = 3;
            if msg == WM_POINTERACTIVATE {
                return PA_NOACTIVATE;
            }
            if msg == WM_POINTERUPDATE {
                return 0;
            }
            if msg == WM_POINTERDOWN || msg == WM_POINTERUP {
                // SAFETY: a structure the call fills, for the pointer the
                // message names.
                let mut info: POINTER_INFO = unsafe { core::mem::zeroed() };
                let id = u32::try_from(wparam & 0xFFFF).unwrap_or(0);
                if unsafe { GetPointerInfo(id, &mut info) } != 0 {
                    SEEN.lock().unwrap().push(Seen {
                        at: Instant::now(),
                        msg,
                        flags: info.pointerFlags,
                        x: info.ptPixelLocation.x,
                        y: info.ptPixelLocation.y,
                    });
                }
                return 0;
            }
            // SAFETY: the system's own handling of everything else.
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }

        let mut devices = Devices::create("touch test").unwrap();
        let class: Vec<u16> = "lowlat-touch-test\0".encode_utf16().collect();
        // SAFETY: this module's own instance, a class of the test's own, and
        // a window of it that the test destroys.
        let (instance, hwnd) = unsafe {
            let instance = GetModuleHandleW(core::ptr::null());
            let class_info = WNDCLASSEXW {
                cbSize: size::<WNDCLASSEXW>(),
                style: 0,
                lpfnWndProc: Some(window),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: instance,
                hIcon: core::ptr::null_mut(),
                hCursor: core::ptr::null_mut(),
                hbrBackground: core::ptr::null_mut(),
                lpszMenuName: core::ptr::null(),
                lpszClassName: class.as_ptr(),
                hIconSm: core::ptr::null_mut(),
            };
            assert_ne!(RegisterClassExW(&class_info), 0);
            let (cx, cy) = (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN));
            let hwnd = CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                class.as_ptr(),
                class.as_ptr(),
                WS_POPUP | WS_VISIBLE,
                cx / 2 - 160,
                cy / 2 - 160,
                320,
                320,
                core::ptr::null_mut(),
                core::ptr::null_mut(),
                instance,
                core::ptr::null(),
            );
            assert!(!hwnd.is_null());
            (instance, hwnd)
        };
        let pump = |ms: u64| {
            let until = Instant::now() + Duration::from_millis(ms);
            // SAFETY: a message structure the call fills, dispatched on the
            // thread that owns the window.
            let mut msg: MSG = unsafe { core::mem::zeroed() };
            while Instant::now() < until {
                while unsafe { PeekMessageW(&mut msg, core::ptr::null_mut(), 0, 0, PM_REMOVE) } != 0
                {
                    unsafe {
                        TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        };
        pump(300);

        // SAFETY: reads of the system's metrics.
        let centre = unsafe {
            (
                GetSystemMetrics(SM_CXSCREEN) / 2,
                GetSystemMetrics(SM_CYSCREEN) / 2,
            )
        };
        let fingers = [(centre.0 - 60, centre.1), (centre.0 + 60, centre.1)];
        for (x, y) in fingers {
            // SAFETY: which window is at a point.
            let under = unsafe { WindowFromPoint(POINT { x, y }) };
            assert_eq!(under, hwnd, "the test's window is not at ({x}, {y})");
        }
        let (left, top, width, height) = desktop_now();
        let aimed = |pixel: i32, origin: i32, extent: i32| {
            (2 * (pixel - origin) + 1) * 65536 / (2 * extent)
        };
        let report = |slot: i32, tracking: i32, (x, y): (i32, i32)| {
            let event = |kind, code, value| Event { kind, code, value };
            [
                event(event::EV_ABS, event::ABS_MT_SLOT, slot),
                event(event::EV_ABS, event::ABS_MT_TRACKING_ID, tracking),
                event(
                    event::EV_ABS,
                    event::ABS_MT_POSITION_X,
                    aimed(x, left, width),
                ),
                event(
                    event::EV_ABS,
                    event::ABS_MT_POSITION_Y,
                    aimed(y, top, height),
                ),
                event(event::EV_ABS, event::ABS_MT_PRESSURE, 512),
                event(event::EV_ABS, event::ABS_MT_DISTANCE, 0),
                event(event::EV_SYN, 0, 0),
            ]
        };

        let moved = |(x, y): (i32, i32), by: i32| (x, y + by);
        devices.emit(Device::Touch, &report(0, 1, fingers[0]));
        devices.emit(Device::Touch, &report(1, 2, fingers[1]));
        let held = Instant::now();
        let mut by = 0;
        while held.elapsed() < Duration::from_millis(1500) {
            if held.elapsed()
                >= Duration::from_millis(600) * u32::try_from(by / 20 + 1).unwrap_or(1)
            {
                by += 20;
                devices.emit(Device::Touch, &report(0, 1, moved(fingers[0], by)));
                devices.emit(Device::Touch, &report(1, 2, moved(fingers[1], by)));
            }
            devices.tick();
            pump(20);
        }
        let lifted = Instant::now();
        devices.emit(Device::Touch, &report(0, -1, moved(fingers[0], by)));
        pump(100);
        devices.emit(Device::Touch, &report(1, -1, moved(fingers[1], by)));
        pump(100);
        devices.emit(Device::Touch, &report(0, 3, fingers[0]));
        devices.emit(Device::Touch, &report(1, 4, fingers[1]));
        devices.emit(Device::Touch, &report(0, 3, moved(fingers[0], 20)));
        devices.emit(Device::Touch, &report(1, 4, moved(fingers[1], 20)));
        pump(100);
        devices.cancel(Device::Touch);
        pump(300);

        // SAFETY: the test's own window and class, as made above.
        unsafe {
            DestroyWindow(hwnd);
            UnregisterClassW(class.as_ptr(), instance);
        }
        let seen = SEEN.lock().unwrap().clone();
        let kinds: Vec<(u32, bool, (i32, i32))> = seen
            .iter()
            .map(|seen| {
                (
                    seen.msg,
                    seen.flags & POINTER_FLAG_CANCELED != 0,
                    (seen.x, seen.y),
                )
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                (WM_POINTERDOWN, false, fingers[0]),
                (WM_POINTERDOWN, false, fingers[1]),
                (WM_POINTERUP, false, moved(fingers[0], by)),
                (WM_POINTERUP, false, moved(fingers[1], by)),
                (WM_POINTERDOWN, false, fingers[0]),
                (WM_POINTERDOWN, false, fingers[1]),
                (WM_POINTERUP, true, moved(fingers[0], 20)),
                (WM_POINTERUP, true, moved(fingers[1], 20)),
            ],
            "{seen:?}"
        );
        assert!(
            seen[2].at >= lifted,
            "the first finger came up before it was lifted: {seen:?}"
        );
    }

    fn keys(keyboard: &mut Keyboard, codes: &[(u8, bool)]) -> Vec<Option<Stroke>> {
        codes
            .iter()
            .map(|&(code, down)| keyboard.stroke(u16::from(code), down))
            .collect()
    }

    /// **Motion by deltas is what the clip holds**: either axis moved by a
    /// delta, and neither a wheel's turn nor a position.
    #[test]
    fn deltas_are_the_motion_the_clip_holds() {
        let ev = |kind, code, value| Event { kind, code, value };
        assert!(moves_by_deltas(&[ev(event::EV_REL, event::REL_Y, 3)]));
        assert!(moves_by_deltas(&[
            ev(event::EV_KEY, 0x110, 1),
            ev(event::EV_REL, event::REL_X, -2),
        ]));
        assert!(!moves_by_deltas(&[ev(event::EV_REL, 0x08, 1)]));
        assert!(!moves_by_deltas(&[ev(event::EV_ABS, 0x00, 500)]));
    }

    /// **A pad played on the bus from its reports loses Guide while a scope
    /// holds the guests**, the owner among them, and has it again once the
    /// scope is cleared.
    #[test]
    fn a_held_owners_bus_pad_has_no_guide() {
        let mut sink = Devices::create("scope-guide").expect("a guest's devices");
        sink.guide(true);
        assert!(sink.presses_guide());
        let me = std::env::current_exe().expect("this test's own path");
        sink.set_scope(Some(std::sync::Arc::new(crate::Scope::new([
            me.as_os_str()
        ]))));
        assert!(!sink.presses_guide(), "a held owner's pad kept Guide");
        sink.set_scope(None);
        assert!(sink.presses_guide());
    }

    /// An ordinary key is its code, the E0 half carried as the prefix flag.
    #[test]
    fn a_key_goes_out_as_its_scancode() {
        let mut keyboard = Keyboard::default();
        let delete = crate::usage::key_code(76).unwrap();
        assert_eq!(
            keyboard.stroke(delete, true),
            Some(Stroke::Scan(0xd3)),
            "delete"
        );
        let input = key_input(Stroke::Scan(0xd3), false);
        // SAFETY: a keyboard input, read as one.
        let ki = unsafe { input.Anonymous.ki };
        assert_eq!(ki.wScan, 0x53);
        assert_eq!(
            ki.dwFlags,
            KEYEVENTF_SCANCODE | KEYEVENTF_EXTENDEDKEY | KEYEVENTF_KEYUP
        );
        let input = key_input(Stroke::Scan(0x1e), true);
        // SAFETY: as above.
        let ki = unsafe { input.Anonymous.ki };
        assert_eq!((ki.wScan, ki.dwFlags), (0x1e, KEYEVENTF_SCANCODE));
    }

    /// **Pause is its virtual key, and Break under Ctrl**, as the system's
    /// own translation has it; it comes up as the stroke it went down as,
    /// whatever Ctrl did meanwhile.
    #[test]
    fn pause_is_its_virtual_key_and_break_under_ctrl() {
        let mut keyboard = Keyboard::default();
        assert_eq!(
            keys(&mut keyboard, &[(PAUSE, true), (PAUSE, false)]),
            [Some(Stroke::Virtual(VK_PAUSE)); 2]
        );
        assert_eq!(
            keys(
                &mut keyboard,
                &[(LCTRL, true), (PAUSE, true), (LCTRL, false), (PAUSE, false)]
            ),
            [
                Some(Stroke::Scan(LCTRL)),
                Some(Stroke::Scan(PAUSE)),
                Some(Stroke::Scan(LCTRL)),
                Some(Stroke::Scan(PAUSE)),
            ]
        );
        // Held through a Ctrl press, it repeats as it went down.
        assert_eq!(
            keys(
                &mut keyboard,
                &[(PAUSE, true), (RCTRL, true), (PAUSE, true), (PAUSE, false)]
            ),
            [
                Some(Stroke::Virtual(VK_PAUSE)),
                Some(Stroke::Scan(RCTRL)),
                Some(Stroke::Virtual(VK_PAUSE)),
                Some(Stroke::Virtual(VK_PAUSE)),
            ]
        );
    }

    /// **PrintScreen is its own code, and SysRq under either Alt.**
    #[test]
    fn print_screen_is_sysrq_under_alt() {
        let mut keyboard = Keyboard::default();
        assert_eq!(
            keyboard.stroke(u16::from(SYSRQ.0), true),
            Some(Stroke::Scan(0xb7))
        );
        keyboard.stroke(u16::from(SYSRQ.0), false);
        assert_eq!(
            keys(
                &mut keyboard,
                &[
                    (RALT, true),
                    (SYSRQ.0, true),
                    (RALT, false),
                    (SYSRQ.0, false)
                ]
            ),
            [
                Some(Stroke::Scan(RALT)),
                Some(Stroke::Scan(0x54)),
                Some(Stroke::Scan(RALT)),
                Some(Stroke::Scan(0x54)),
            ]
        );
    }

    /// A key that sends no break sends nothing as it comes up, and its code
    /// is never read as the prefix flag.
    #[test]
    fn a_make_only_key_is_sent_down_and_never_up() {
        let mut keyboard = Keyboard::default();
        for code in MAKE_ONLY {
            assert_eq!(
                keyboard.stroke(u16::from(code), true),
                Some(Stroke::Make(code))
            );
            assert_eq!(keyboard.stroke(u16::from(code), false), None);
        }
        let input = key_input(Stroke::Make(0xf2), true);
        // SAFETY: a keyboard input, read as one.
        let ki = unsafe { input.Anonymous.ki };
        assert_eq!((ki.wScan, ki.dwFlags), (0xf2, KEYEVENTF_SCANCODE));
    }

    const CTRL_KEY: u8 = 0xe0;
    const ALT_KEY: u8 = 0xe2;
    const RIGHT_ALT_KEY: u8 = 0xe6;
    const DELETE: u8 = 0x4c;
    const KEYPAD_DELETE: u8 = 0x63;
    const A_KEY: u8 = 0x04;

    fn key(keys: &mut HidKeys, usage: u8, down: bool, system: (bool, bool)) -> Step {
        keys.key(usage, down, &mut || system)
    }

    const NONE: (bool, bool) = (false, false);

    /// **A Delete going down under this keyboard's Ctrl and Alt goes through
    /// the batch input call, never onto the HID keyboard**, and repeats and
    /// comes up that way whatever the modifiers do meanwhile; the keypad's
    /// Delete the same, under either Alt.
    #[test]
    fn a_delete_under_this_keyboards_ctrl_and_alt_goes_through_the_batch_call() {
        let mut keys = HidKeys::default();
        assert_eq!(key(&mut keys, CTRL_KEY, true, NONE), Step::Hid);
        assert_eq!(key(&mut keys, ALT_KEY, true, NONE), Step::Hid);
        assert_eq!(key(&mut keys, DELETE, true, NONE), Step::Batch);
        assert!(
            !keys.keys.is_down(DELETE),
            "the Delete reached the HID keyboard"
        );
        assert_eq!(key(&mut keys, CTRL_KEY, false, NONE), Step::Hid);
        assert_eq!(key(&mut keys, DELETE, true, NONE), Step::Batch, "a repeat");
        assert_eq!(key(&mut keys, DELETE, false, NONE), Step::Batch);
        assert_eq!(key(&mut keys, ALT_KEY, false, NONE), Step::Hid);

        let mut keys = HidKeys::default();
        key(&mut keys, CTRL_KEY, true, NONE);
        key(&mut keys, RIGHT_ALT_KEY, true, NONE);
        assert_eq!(key(&mut keys, KEYPAD_DELETE, true, NONE), Step::Batch);
        assert!(!keys.keys.is_down(KEYPAD_DELETE));
        assert_eq!(key(&mut keys, KEYPAD_DELETE, false, NONE), Step::Batch);
    }

    /// **Ctrl and Alt held anywhere count**, each on this keyboard or as the
    /// system's own key state says -- another guest's, the desk's.
    #[test]
    fn a_delete_under_the_systems_ctrl_and_alt_goes_through_the_batch_call() {
        let mut keys = HidKeys::default();
        assert_eq!(key(&mut keys, DELETE, true, (true, true)), Step::Batch);
        assert_eq!(key(&mut keys, DELETE, false, NONE), Step::Batch);

        let mut keys = HidKeys::default();
        key(&mut keys, ALT_KEY, true, NONE);
        assert_eq!(
            key(&mut keys, DELETE, true, (true, false)),
            Step::Batch,
            "this keyboard's Alt, the system's Ctrl"
        );

        let mut keys = HidKeys::default();
        assert_eq!(
            key(&mut keys, DELETE, true, (true, false)),
            Step::Hid,
            "Ctrl alone"
        );
        assert!(keys.keys.is_down(DELETE));
    }

    /// **Without both, a Delete is the HID keyboard's** like any key, down,
    /// repeated and up.
    #[test]
    fn a_delete_without_ctrl_and_alt_is_the_hid_keyboards() {
        let mut keys = HidKeys::default();
        key(&mut keys, CTRL_KEY, true, NONE);
        assert_eq!(key(&mut keys, DELETE, true, NONE), Step::Hid);
        assert!(keys.keys.is_down(DELETE));
        assert_eq!(key(&mut keys, DELETE, true, NONE), Step::Hid);
        assert_eq!(key(&mut keys, DELETE, false, NONE), Step::Hid);
        assert!(!keys.keys.is_down(DELETE));
    }

    /// **The other order**: a Delete held on the HID keyboard when Ctrl and
    /// Alt come together over it is let go of there first -- the report that
    /// goes next holds neither the Delete nor the key that completed the
    /// chord -- and its repeats then go through the batch input call; and
    /// Ctrl and Alt arriving on another keyboard let it go as well.
    #[test]
    fn ctrl_and_alt_coming_together_over_a_held_delete_let_it_go_first() {
        let mut keys = HidKeys::default();
        assert_eq!(key(&mut keys, DELETE, true, NONE), Step::Hid);
        assert_eq!(
            key(&mut keys, CTRL_KEY, true, NONE),
            Step::Hid,
            "Ctrl alone"
        );
        assert!(keys.keys.is_down(DELETE));
        assert_eq!(key(&mut keys, ALT_KEY, true, NONE), Step::ReleasedFirst);
        let report = keys.keys_now();
        assert_eq!(
            report[1], 0b0000_0001,
            "Ctrl alone in the report that goes first"
        );
        assert!(!keys.keys.is_down(DELETE), "the Delete stayed held");
        keys.set(ALT_KEY, true);
        assert!(keys.keys.is_down(ALT_KEY));
        assert_eq!(
            key(&mut keys, DELETE, true, NONE),
            Step::Batch,
            "its repeat"
        );
        assert_eq!(key(&mut keys, DELETE, false, NONE), Step::Batch);

        let mut keys = HidKeys::default();
        key(&mut keys, DELETE, true, NONE);
        assert!(!keys.let_go_over((true, false)), "Ctrl alone elsewhere");
        assert!(keys.keys.is_down(DELETE));
        assert!(
            keys.let_go_over((true, true)),
            "Ctrl and Alt on another keyboard"
        );
        assert!(!keys.keys.is_down(DELETE));
        assert_eq!(key(&mut keys, DELETE, false, NONE), Step::Nothing);
        assert!(!keys.let_go_over((true, true)), "nothing held to let go of");

        let mut keys = HidKeys::default();
        key(&mut keys, A_KEY, true, NONE);
        key(&mut keys, CTRL_KEY, true, NONE);
        assert_eq!(
            key(&mut keys, ALT_KEY, true, NONE),
            Step::Hid,
            "no Delete held, so nothing goes first"
        );
    }

    /// **A report goes only when it changed**, the keys and the consumer usage
    /// each, the consumer carrying the newest usage still held.
    #[test]
    fn a_report_goes_only_when_it_changed() {
        let mut keys = HidKeys::default();
        assert_eq!(keys.changed(), (None, None), "nothing yet");
        key(&mut keys, A_KEY, true, NONE);
        let (sent, consumer) = keys.changed();
        assert_eq!(consumer, None);
        let sent = sent.expect("the key's report");
        assert_eq!(sent[0], keyboard::KEYS_REPORT_ID);
        assert_eq!(sent[2], 1 << 4, "usage 0x04 is the bitmap's fifth bit");
        assert_eq!(keys.changed(), (None, None), "unchanged");

        keys.consumer(0xe9, true);
        assert_eq!(keys.changed(), (None, Some([2, 0xe9, 0x00])));
        keys.consumer(0xe2, true);
        assert_eq!(keys.changed(), (None, Some([2, 0xe2, 0x00])), "the newest");
        keys.consumer(0xe2, false);
        assert_eq!(
            keys.changed(),
            (None, Some([2, 0xe9, 0x00])),
            "the one still held"
        );
        keys.consumer(0xe9, false);
        assert_eq!(keys.changed(), (None, Some(keyboard::NEUTRAL_CONSUMER)));
        assert_eq!(keys.changed(), (None, None));
    }

    /// **The hold drops the oldest whole report at its bound**, never part of
    /// one, and keeps the newest.
    #[test]
    fn the_hold_drops_the_oldest_whole_report_at_its_bound() {
        let report = |code: u16| {
            [
                Event {
                    kind: event::EV_KEY,
                    code,
                    value: 1,
                },
                Event {
                    kind: event::EV_KEY,
                    code,
                    value: 0,
                },
                Event {
                    kind: event::EV_SYN,
                    code: 0,
                    value: 0,
                },
            ]
        };
        let mut held = Held::default();
        let mut code = 1;
        while held.used + 3 <= HELD {
            held.push(&report(code));
            code += 1;
        }
        assert!(!held.reported);
        held.push(&report(999));
        assert!(held.reported, "an overflow went unsaid");
        let events = held.events();
        assert_eq!(events.len() % 3, 0, "a report was cut");
        assert!(events.chunks(3).all(|r| r[2].kind == event::EV_SYN));
        assert_eq!(events[events.len() - 3].code, 999, "the newest was lost");
        assert_ne!(events[0].code, 1, "the oldest survived");
    }

    fn flags(inputs: &Inputs) -> Vec<(i32, i32, u32, u32)> {
        inputs
            .as_slice()
            .iter()
            .map(|input| {
                // SAFETY: every input the pointer path makes is a mouse one.
                let mi = unsafe { input.Anonymous.mi };
                (mi.dx, mi.dy, mi.mouseData, mi.dwFlags)
            })
            .collect()
    }

    /// One move a report, absolute over the virtual desktop or relative; the
    /// wheels' fine axes in the units the peer counted and the stepped ones
    /// dropped; the side buttons as the extra buttons.
    #[test]
    fn a_pointer_batch_becomes_the_calls_mouse_inputs() {
        use crate::event::{Extents, Injector};
        use lowlat_core::control::{Control, op};
        #[derive(Default)]
        struct Last(Vec<Event>);
        impl Sink for Last {
            fn emit(&mut self, _device: Device, events: &[Event]) {
                self.0.extend_from_slice(events);
            }
        }
        let control = |opcode, a0, a1, a2| Control {
            a0,
            a1,
            a2,
            opcode,
            body: &[],
        };
        let mut inject = Injector::new(Extents::alone(1920, 1080));
        let mut last = Last::default();
        inject.on_control(&control(op::MOUSE_MOTION, 0, 1920, 1080), &mut last);
        inject.on_control(&control(op::MOUSE_MOTION, 1, 5, (-7i32) as u32), &mut last);
        inject.on_control(
            &control(op::MOUSE_WHEEL, 30, (-120i32) as u32, 0),
            &mut last,
        );
        inject.on_control(&control(op::MOUSE_BUTTON, 4, 1, 0), &mut last);
        inject.on_control(&control(op::MOUSE_BUTTON, 1, 1, 0), &mut last);
        let mut inputs = Inputs::new();
        pointer_inputs(&last.0, &mut inputs);
        assert_eq!(
            flags(&inputs),
            vec![
                // The centre of the last pixel each way, for an input stack
                // that truncates value * extent / 65536.
                (65518, 65505, 0, ABSOLUTE),
                (5, -7, 0, MOUSEEVENTF_MOVE),
                (0, 0, bits(-120), MOUSEEVENTF_WHEEL),
                (0, 0, 30, MOUSEEVENTF_HWHEEL),
                (0, 0, u32::from(XBUTTON1), MOUSEEVENTF_XDOWN),
                (0, 0, 0, MOUSEEVENTF_LEFTDOWN),
            ]
        );
    }

    /// The expander's batches as the input driver's mouse takes them, the
    /// desktop one 1920x1080 primary unless `primary` says otherwise.
    fn pointed(
        controls: &[(u8, u32, u32, u32)],
        primary: impl Fn(i32, i32) -> Option<(u16, u16)>,
    ) -> Vec<Pointed> {
        use crate::event::{Extents, Injector};
        use lowlat_core::control::Control;
        #[derive(Default)]
        struct Batches(Vec<Vec<Event>>);
        impl Sink for Batches {
            fn emit(&mut self, _device: Device, events: &[Event]) {
                self.0.push(events.to_vec());
            }
        }
        let mut inject = Injector::new(Extents::alone(1920, 1080));
        let mut batches = Batches::default();
        for &(opcode, a0, a1, a2) in controls {
            let control = Control {
                a0,
                a1,
                a2,
                opcode,
                body: &[],
            };
            inject.on_control(&control, &mut batches);
        }
        let mut pointer = HidPointer::default();
        let mut out = Vec::new();
        for event in batches.0.iter().flatten() {
            pointer.take(event);
            if event.kind == event::EV_SYN {
                pointer
                    .ended(&primary, |p| {
                        out.push(p);
                        Ok::<(), ()>(())
                    })
                    .unwrap();
            }
        }
        out
    }

    fn alone(x: i32, y: i32) -> Option<(u16, u16)> {
        on_primary(x, y, (0, 0, 1920, 1080), (1920, 1080))
    }

    /// **Every button and wheel rides the relative collection, whichever of
    /// the expander's pointers it went to**: a click after a position, which
    /// the expander sends to the absolute pointer and releases there, is the
    /// relative collection's, carried through the motion beside it; the wheels
    /// in the units the peer counted; a position on the absolute collection at
    /// its pixel's centre.
    #[test]
    fn every_button_and_wheel_rides_the_relative_collection() {
        use lowlat_core::control::op;
        use mouse::{BACK, LEFT, absolute, relative};
        let out = pointed(
            &[
                (op::MOUSE_MOTION, 0, 1920, 1080),
                (op::MOUSE_BUTTON, 1, 1, 0),
                (op::MOUSE_MOTION, 1, 5, (-7i32) as u32),
                (op::MOUSE_WHEEL, 30, (-120i32) as u32, 0),
                (op::MOUSE_BUTTON, 1, 0, 0),
                (op::MOUSE_BUTTON, 4, 1, 0),
                (op::MOUSE_BUTTON, 4, 0, 0),
            ],
            alone,
        );
        assert_eq!(
            out,
            vec![
                // The last pixel's centre each way, for a system taking the
                // collection's value back as v * extent / 32768.
                Pointed::Absolute(absolute(32759, 32752)),
                Pointed::Relative(relative(LEFT, 0, 0, 0, 0)),
                Pointed::Relative(relative(LEFT, 5, -7, 0, 0)),
                Pointed::Relative(relative(LEFT, 0, 0, -120, 30)),
                Pointed::Relative(relative(0, 0, 0, 0, 0)),
                Pointed::Relative(relative(BACK, 0, 0, 0, 0)),
                Pointed::Relative(relative(0, 0, 0, 0, 0)),
            ]
        );
    }

    /// **Motion past a report's range is split, none of it lost**, the held
    /// buttons in every part; a position off the primary display goes to the
    /// batch input call as the expander spread it.
    #[test]
    fn a_long_move_is_split_and_a_position_elsewhere_goes_through_the_call() {
        use lowlat_core::control::op;
        let out = pointed(
            &[
                (op::MOUSE_BUTTON, 3, 1, 0),
                (op::MOUSE_MOTION, 1, 40_000, (-70_000i32) as u32),
                (op::MOUSE_MOTION, 0, 960, 540),
            ],
            |_, _| None,
        );
        let mut moved = (0, 0);
        let mut parts = 0;
        for p in &out {
            if let Pointed::Relative(report) = p {
                assert!(mouse::relative_allowed(report), "{report:?}");
                if report[1] == mouse::RIGHT {
                    let word =
                        |at: usize| i32::from(i16::from_le_bytes([report[at], report[at + 1]]));
                    moved = (moved.0 + word(2), moved.1 + word(4));
                    parts += 1;
                }
            }
        }
        assert_eq!(moved, (40_000, -70_000), "motion lost in the split");
        assert_eq!(parts, 1 + 3, "the press, then the move in three parts");
        assert_eq!(
            out.last(),
            Some(&Pointed::Elsewhere(32785, 32798)),
            "a position elsewhere not as the expander spread it"
        );
    }

    /// **A position lands on the pixel the batch input call would have put it
    /// on**, every pixel of the primary across, and nowhere off it: a display
    /// beside it, one left of it -- the desktop's corner negative -- and the
    /// rows a taller neighbour adds below its bottom edge.
    #[test]
    fn a_position_on_the_primary_lands_on_its_pixel_and_one_off_it_is_refused() {
        // The expander's value for a pixel of the desktop, its centre.
        let spread = |pixel: i32, origin: i32, extent: i32| {
            i32::try_from((2 * i64::from(pixel - origin) + 1) * 65536 / (2 * i64::from(extent)))
                .unwrap()
        };
        for desktop in [(0, 0, 4480, 1440), (-1920, 0, 4480, 2148)] {
            let (left, top, width, height) = desktop;
            for px in 0..2560 {
                let x = spread(px, left, width);
                let (hx, hy) = on_primary(x, spread(700, top, height), desktop, (2560, 1440))
                    .expect("on the primary");
                assert_eq!(
                    i64::from(hx) * 2560 / 32768,
                    i64::from(px),
                    "{desktop:?} x {px}"
                );
                assert_eq!(i64::from(hy) * 1440 / 32768, 700, "{desktop:?} y");
            }
            for (px, py) in [(-1, 700), (2560, 700), (0, 1440), (2559, -1)] {
                if !(left..left + width).contains(&px) || !(top..top + height).contains(&py) {
                    continue;
                }
                assert_eq!(
                    on_primary(
                        spread(px, left, width),
                        spread(py, top, height),
                        desktop,
                        (2560, 1440)
                    ),
                    None,
                    "{desktop:?} {px},{py} placed on the primary"
                );
            }
        }
        assert_eq!(
            on_primary(spread(2559, 0, 4480), 0, (0, 0, 4480, 1440), (2560, 1440)),
            Some((32761, 11)),
            "the primary's last column"
        );
    }
}

/// Tests that plug a pad into the running system's bus.
///
/// **Excluded from the default suite.** A pad appears to every program on the
/// desk while one runs. Run them deliberately:
///
/// ```text
/// cargo test -p lowlat-inject -- --ignored --test-threads 1
/// ```
#[cfg(test)]
mod pad_tests {
    use super::*;
    use lowlat_hid_proto::control::Create;

    const DS4_HELD: &[u8; pad::INPUT_LEN] =
        include_bytes!("../../core/tests/data/pad/ds4/input-held.bin");
    const DS5_HELD: &[u8; pad::INPUT_LEN] =
        include_bytes!("../../core/tests/data/pad/ds5/input-held.bin");

    /// **A report pad played on the bus is the state its report carries**:
    /// the cross its report holds is the bus pad's A, the sticks and triggers
    /// where the report has them, and the touchpad's click, which the bus pad
    /// has no button for, left out.
    #[test]
    fn a_report_pad_on_the_bus_plays_its_reports_state() {
        for (product, held) in [
            (Product::DualShock4, DS4_HELD),
            (Product::DualSense, DS5_HELD),
        ] {
            let state = pad::state(product, held);
            let bus = bus_report(product, held, true);
            assert_ne!(
                bus.buttons & gamepad::bit::A,
                0,
                "{product:?}: the cross held"
            );
            assert_eq!(bus.buttons, state.buttons, "{product:?}");
            assert_eq!(
                (bus.lx, bus.ly, bus.rx, bus.ry),
                (state.lx, state.ly, state.rx, state.ry),
                "{product:?}"
            );
            assert_eq!(
                (bus.left_trigger, bus.right_trigger),
                (state.lt, state.rt),
                "{product:?}"
            );
        }
        // The DualSense's third button byte, the touchpad's click its second
        // bit.
        let mut clicked = *DS5_HELD;
        clicked[10] |= 0x02;
        assert_ne!(
            pad::state(Product::DualSense, &clicked).buttons & gamepad::bit::TOUCHPAD,
            0
        );
        let bus = bus_report(Product::DualSense, &clicked, true);
        assert_eq!(
            bus.buttons & gamepad::bit::TOUCHPAD,
            0,
            "the touchpad's click on the bus"
        );
        assert_ne!(bus.buttons & gamepad::bit::A, 0);
    }

    /// **Guide is the owner's alone on the bus too**: a report pad played
    /// there has its PS button as Guide, which only an owner's report keeps;
    /// a guest's loses it and nothing else.
    #[test]
    fn a_report_pad_on_the_bus_presses_guide_for_the_owner_alone() {
        // The third button byte's first bit, wherever each product has it.
        for (product, held, at) in [
            (Product::DualShock4, DS4_HELD, 7),
            (Product::DualSense, DS5_HELD, 10),
        ] {
            let mut ps = *held;
            ps[at] |= 0x01;
            assert_ne!(
                pad::state(product, &ps).buttons & gamepad::bit::GUIDE,
                0,
                "{product:?}"
            );
            let owner = bus_report(product, &ps, true);
            let guest = bus_report(product, &ps, false);
            assert_ne!(
                owner.buttons & gamepad::bit::GUIDE,
                0,
                "{product:?}: the owner's"
            );
            assert_eq!(
                guest.buttons & gamepad::bit::GUIDE,
                0,
                "{product:?}: a guest's"
            );
            assert_eq!(
                guest.buttons | gamepad::bit::GUIDE,
                owner.buttons,
                "{product:?}"
            );
        }
    }

    /// **A report pad's table is the answers the Linux host gives**: its
    /// calibration and firmware -- the peer's where it sent them, else a
    /// real pad's -- and its pairing around this host's address, three
    /// reports the create carries whole.
    #[test]
    fn a_pads_table_is_the_answers_the_linux_host_gives() {
        let address = answers::address(answers::middle("7"), 2);
        for product in [Product::DualShock4, Product::DualSense] {
            let mut sent = answers::Features::new();
            let mut calibration = answers::canned(product, Feature::Calibration).to_vec();
            calibration[1] ^= 0xFF;
            sent.set(Feature::Calibration, &calibration);
            for features in [answers::Features::new(), sent] {
                let table = table(product, &features, &address);
                assert_eq!(table.len(), 3, "{product:?}");
                for id in [
                    Feature::Calibration.id(product),
                    Feature::Firmware.id(product),
                    answers::pairing_id(product),
                ] {
                    let mut out = [0u8; pad::FEATURE_MAX];
                    let len = features.answer(product, id, &address, &mut out).unwrap();
                    assert_eq!(table.get(id), Some(&out[..len]), "{product:?} {id:#x}");
                }
                let kind = kind_of(product).unwrap();
                let mut request = [0u8; Create::LEN];
                Create {
                    kind,
                    slot: 9,
                    features: table,
                }
                .encode(&mut request)
                .unwrap();
                assert_eq!(Create::decode(&request).unwrap().features, table);
            }
            assert_eq!(
                table(product, &sent, &address).get(Feature::Calibration.id(product)),
                Some(&calibration[..]),
                "{product:?}: the peer's own calibration"
            );
        }
    }

    /// **What the driver says a program wrote goes to the loop as it came**:
    /// its kind, its length and its bytes.
    #[test]
    fn a_write_is_handed_back_as_it_came() {
        let report = [0x02, 0x03, 0x14, 0xFF];
        let output = handed_back(&control::Written::new(WrittenKind::Output, &report).unwrap());
        assert_eq!(output.kind, pad::OutputKind::Output);
        assert_eq!(&output.report[..output.len], &report);
        let feature = handed_back(&control::Written::new(WrittenKind::Feature, &report).unwrap());
        assert_eq!(feature.kind, pad::OutputKind::Feature);
    }
}

#[cfg(test)]
mod bus_tests {
    use super::*;
    use crate::event::{Extents, Injector};
    use lowlat_core::control::{Control, op};
    use std::time::{Duration, Instant};
    use windows_sys::Win32::UI::Input::XboxController::{
        XINPUT_STATE, XINPUT_VIBRATION, XInputGetState, XInputSetState, XUSER_MAX_COUNT,
    };

    /// The pad slots a game sees connected now.
    fn connected() -> Vec<u32> {
        (0..XUSER_MAX_COUNT)
            .filter(|&index| {
                let mut state = XINPUT_STATE::default();
                // SAFETY: a live state for the call to fill.
                unsafe { XInputGetState(index, &raw mut state) == 0 }
            })
            .collect()
    }

    fn state(index: u32) -> Option<XINPUT_STATE> {
        let mut state = XINPUT_STATE::default();
        // SAFETY: as above.
        (unsafe { XInputGetState(index, &raw mut state) } == 0).then_some(state)
    }

    /// Run the loop's own calls until `done` says so, or give up.
    fn until(devices: &mut Devices, within: Duration, mut done: impl FnMut(&mut Devices) -> bool) {
        let started = Instant::now();
        while !done(devices) {
            assert!(started.elapsed() < within, "gave up after {within:?}");
            devices.tick();
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// **A whole state reaches a game as the pad it says**, from the bus's
    /// ready on, with up still up; **what a game asks it to vibrate at comes
    /// back** for the peer; and the pad's own message takes it away again.
    #[test]
    #[ignore = "plugs a pad into the system's bus"]
    fn a_pad_reaches_a_game_and_its_rumble_comes_back() {
        let before = connected();
        let mut devices = Devices::create("bus-test").expect("devices");
        let mut inject = Injector::new(Extents::alone(1920, 1080));
        let mut body = vec![0xAB, 0xCD, 0xEF];
        body.extend_from_slice(&(gamepad::bit::A | gamepad::bit::DPAD_LEFT).to_be_bytes());
        for value in [1_000i16, 20_000, -300, -32_767] {
            body.extend_from_slice(&value.to_be_bytes());
        }
        body.extend_from_slice(&[7, 250]);
        let message = Control {
            a0: 7,
            a1: 0,
            a2: 0,
            opcode: op::GAMEPAD_STATE,
            body: &body,
        };
        let plugged = Instant::now();
        inject.on_control(&message, &mut devices);
        until(&mut devices, Duration::from_secs(5), |d| {
            d.pads.iter().flatten().any(|pad| pad.ready)
        });
        let ready_ms = plugged.elapsed().as_secs_f64() * 1000.0;

        let mut index = None;
        until(&mut devices, Duration::from_secs(5), |_| {
            index = connected().into_iter().find(|i| !before.contains(i));
            index.is_some()
        });
        let index = index.unwrap();
        let seen_ms = plugged.elapsed().as_secs_f64() * 1000.0;
        let pad = state(index).unwrap().Gamepad;
        assert_eq!(
            pad.wButtons,
            gamepad::bit::A | gamepad::bit::DPAD_LEFT,
            "buttons"
        );
        assert_eq!(
            (pad.sThumbLX, pad.sThumbLY, pad.sThumbRX, pad.sThumbRY),
            (1_000, 20_000, -300, -32_767)
        );
        assert_eq!((pad.bLeftTrigger, pad.bRightTrigger), (7, 250));

        let vibration = XINPUT_VIBRATION {
            wLeftMotorSpeed: 0x8080,
            wRightMotorSpeed: 0x4040,
        };
        // SAFETY: a live vibration for the call to read.
        assert_eq!(unsafe { XInputSetState(index, &raw const vibration) }, 0);
        let mut heard = None;
        until(&mut devices, Duration::from_secs(2), |d| {
            heard = d.rumble();
            heard.is_some()
        });
        assert_eq!(
            heard,
            Some(Rumble {
                pad: 7,
                large: 0x80,
                small: 0x40
            })
        );
        let stop = XINPUT_VIBRATION::default();
        // SAFETY: as above.
        unsafe { XInputSetState(index, &raw const stop) };

        inject.on_control(
            &Control {
                a0: 0,
                a1: 0,
                a2: 7,
                opcode: op::GAMEPAD_UNPLUG,
                body: &[],
            },
            &mut devices,
        );
        until(&mut devices, Duration::from_secs(5), |_| {
            !connected().contains(&index)
        });
        println!("ready after {ready_ms:.0} ms, seen by a game after {seen_ms:.0} ms");
    }

    /// **A guest that leaves takes its pads with it**, whatever path it
    /// leaves by: its devices go, and the bus closing unplugs what is left.
    #[test]
    #[ignore = "plugs a pad into the system's bus"]
    fn a_guests_pads_go_with_its_devices() {
        let before = connected();
        let mut devices = Devices::create("bus-test").expect("devices");
        let mut inject = Injector::new(Extents::alone(1920, 1080));
        let body = [0u8; 15];
        inject.on_control(
            &Control {
                a0: 3,
                a1: 0,
                a2: 0,
                opcode: op::GAMEPAD_STATE,
                body: &body,
            },
            &mut devices,
        );
        let mut index = None;
        until(&mut devices, Duration::from_secs(5), |_| {
            index = connected().into_iter().find(|i| !before.contains(i));
            index.is_some()
        });
        let index = index.unwrap();
        drop(devices);
        let started = Instant::now();
        while connected().contains(&index) {
            assert!(started.elapsed() < Duration::from_secs(5), "the pad stayed");
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

/// Tests that type through the host's own input driver.
///
/// **Excluded from the default suite**, and run from an elevated shell on the
/// console with the input driver installed (scripts/hid-install.ps1): a key
/// goes down on the desk while one runs -- F24, which nothing here answers.
///
/// ```text
/// cargo test -p lowlat-inject --lib windows::hid_tests -- --ignored --test-threads 1
/// ```
#[cfg(test)]
mod hid_tests {
    use super::*;
    use crate::event::{Extents, Injector};
    use lowlat_core::control::{Control, op};
    use std::time::{Duration, Instant};

    /// F24: the peer's code, its usage, the system's key.
    const F24: u32 = 115;
    const F24_USAGE: u8 = 0x73;
    const VK_F24: i32 = 0x87;

    fn down() -> bool {
        // SAFETY: a read of a key's state.
        unsafe { GetAsyncKeyState(VK_F24) < 0 }
    }

    fn f24(pressed: bool) -> Control<'static> {
        Control {
            a0: F24,
            a1: 0,
            a2: u32::from(pressed),
            opcode: op::KEYBOARD,
            body: &[],
        }
    }

    /// Run the loop's own pass until `done` says so, or give up.
    fn until(
        devices: &mut Devices,
        within: Duration,
        what: &str,
        mut done: impl FnMut(&mut Devices) -> bool,
    ) -> Duration {
        let started = Instant::now();
        while !done(devices) {
            assert!(
                started.elapsed() < within,
                "{what}: gave up after {within:?}"
            );
            devices.tick();
            std::thread::sleep(Duration::from_millis(1));
        }
        started.elapsed()
    }

    /// **A guest's keys go to the input driver's keyboard while the switch is
    /// on, and a key held across the switch comes up**: a key typed as the
    /// switch goes on is held until the system lists the keyboard, then goes
    /// down on it; switched off with it held, it comes up -- released the way
    /// it went down -- and the keyboard goes; switched on again, a keyboard is
    /// made again and a key goes down and up on it.
    #[test]
    #[ignore = "types on the desk through the input driver"]
    fn a_guests_keys_go_to_the_drivers_keyboard_and_come_up_at_the_switch() {
        assert!(!down(), "F24 down before the test");
        let mut devices = Devices::create("hid-test").unwrap();
        let mut inject = Injector::new(Extents::alone(1920, 1080));
        devices.set_virtual_input(true);
        let slot = devices
            .hid
            .as_ref()
            .expect("a keyboard from the driver")
            .slot;
        inject.on_control(&f24(true), &mut devices);
        assert!(
            !down(),
            "the key went down before its keyboard could deliver it"
        );
        let ready = until(&mut devices, Duration::from_secs(10), "listed", |d| {
            d.hid.as_ref().is_some_and(|hid| hid.ready)
        });
        let typed = until(&mut devices, Duration::from_secs(2), "down", |_| down());
        assert!(
            devices
                .hid
                .as_ref()
                .is_some_and(|hid| hid.keys.keys.is_down(F24_USAGE)),
            "the key went down another way"
        );

        inject.release_keys(&mut devices);
        devices.set_virtual_input(false);
        let up = until(&mut devices, Duration::from_secs(2), "up", |_| !down());
        let gone = until(&mut devices, Duration::from_secs(5), "unlisted", |_| {
            !hid::listed(Kind::Keyboard, slot)
        });

        devices.set_virtual_input(true);
        inject.on_control(&f24(true), &mut devices);
        let again = until(&mut devices, Duration::from_secs(10), "listed again", |d| {
            d.hid.as_ref().is_some_and(|hid| hid.ready)
        });
        until(&mut devices, Duration::from_secs(2), "down again", |_| {
            down()
        });
        inject.on_control(&f24(false), &mut devices);
        until(&mut devices, Duration::from_secs(2), "up again", |_| {
            !down()
        });
        drop(devices);
        println!(
            "listed after {ready:?}, down {typed:?} later; up {up:?} after the switch, \
             unlisted {gone:?} later; listed again after {again:?}"
        );
    }

    /// Where the system has the pointer, in this thread's physical pixels.
    fn cursor() -> (i32, i32) {
        let mut at = windows_sys::Win32::Foundation::POINT { x: 0, y: 0 };
        // SAFETY: a live local the call fills.
        unsafe { windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos(&raw mut at) };
        (at.x, at.y)
    }

    /// **A guest's pointer goes to the input driver's mouse while the switch is
    /// on**: held until the system lists the mouse, a position on the primary
    /// display then lands on its pixel through the absolute collection, and a
    /// move goes through the relative one, the mouse kept throughout; switched
    /// off, the mouse goes. No button is pressed: a click would land on the
    /// desk.
    ///
    /// Run after the keyboard check, as a run orders them, its mouse is made
    /// while raw input may still list that check's in the same slot: held
    /// until the system lists it under a handle of its own.
    #[test]
    #[ignore = "moves the desk's pointer through the input driver"]
    fn a_guests_pointer_goes_to_the_drivers_mouse() {
        use crate::event::Place;
        let mut devices = Devices::create("hid-mouse-test").unwrap();
        // SAFETY: reads of the system's metrics.
        let metric = |index| unsafe { GetSystemMetrics(index) };
        let (width, height) = (metric(SM_CXSCREEN), metric(SM_CYSCREEN));
        let (left, top) = (metric(SM_XVIRTUALSCREEN), metric(SM_YVIRTUALSCREEN));
        let side = |value: i32| u32::try_from(value).unwrap();
        let mut inject = Injector::new(Extents::placed(
            side(width),
            side(height),
            Place {
                x: side(-left),
                y: side(-top),
                width: side(width),
                height: side(height),
                desktop_width: side(metric(SM_CXVIRTUALSCREEN)),
                desktop_height: side(metric(SM_CYVIRTUALSCREEN)),
            },
        ));
        let motion = |relative: bool, x: i32, y: i32| Control {
            a0: u32::from(relative),
            a1: side(x),
            a2: side(y),
            opcode: op::MOUSE_MOTION,
            body: &[],
        };
        devices.set_virtual_input(true);
        let slot = devices
            .mouse
            .as_ref()
            .expect("a mouse from the driver")
            .slot;
        let stale = hid::listed(Kind::Mouse, slot);
        let ready = until(&mut devices, Duration::from_secs(10), "listed", |d| {
            d.mouse.as_ref().is_some_and(|mouse| mouse.ready)
        });
        let mut landed = Vec::new();
        for (x, y) in [(0, 0), (width - 1, height - 1), (width / 2, height / 3)] {
            inject.on_control(&motion(false, x, y), &mut devices);
            landed.push(until(
                &mut devices,
                Duration::from_secs(2),
                "landed",
                |_| cursor() == (x, y),
            ));
        }
        let before = cursor();
        inject.on_control(&motion(true, 0, 40), &mut devices);
        until(&mut devices, Duration::from_secs(2), "moved", |_| {
            cursor().1 > before.1
        });
        assert!(
            devices.mouse.as_ref().is_some_and(|mouse| mouse.ready),
            "the mouse refused a report and went"
        );
        devices.set_virtual_input(false);
        let gone = until(&mut devices, Duration::from_secs(5), "unlisted", |_| {
            !hid::listed(Kind::Mouse, slot)
        });
        drop(devices);
        println!(
            "primary {width}x{height}: the slot listed as the mouse was made: {stale}; \
             listed after {ready:?}, positions landed after {landed:?}, moved from {before:?}; \
             unlisted {gone:?} after the switch"
        );
    }

    /// A game's handle on a pad, closed however the check ends: one left open
    /// holds the removed pad's node until the process goes.
    struct Game(HANDLE);

    impl Drop for Game {
        fn drop(&mut self) {
            // SAFETY: the handle this owns, closed once.
            unsafe { CloseHandle(self.0) };
        }
    }

    /// The input driver's pad of `product`, as a game finds it: the HID
    /// interface of Sony's identity whose path names the framework, opened
    /// for reading and writing beside every other user.
    fn drivers_pad(product: u16) -> Option<Game> {
        use windows_sys::Win32::Devices::HumanInterfaceDevice::{
            GUID_DEVINTERFACE_HID, HIDD_ATTRIBUTES, HidD_GetAttributes,
        };
        let class = GUID_DEVINTERFACE_HID;
        let mut len = 0u32;
        // SAFETY: live outputs; no device filter.
        unsafe {
            CM_Get_Device_Interface_List_SizeW(
                &raw mut len,
                &raw const class,
                core::ptr::null(),
                CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
            )
        };
        let mut list = vec![0u16; usize::try_from(len).ok()? + 1];
        // SAFETY: a buffer of the length given.
        let cr = unsafe {
            CM_Get_Device_Interface_ListW(
                &raw const class,
                core::ptr::null(),
                list.as_mut_ptr(),
                u32::try_from(list.len()).ok()?,
                CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
            )
        };
        assert_eq!(cr, CR_SUCCESS);
        for path in list.split(|unit| *unit == 0).filter(|p| !p.is_empty()) {
            let name = String::from_utf16_lossy(path).to_uppercase();
            if !name.contains("HID_DEVICE_SYSTEM_VHF") {
                continue;
            }
            let path: Vec<u16> = path.iter().copied().chain(Some(0)).collect();
            // SAFETY: a terminated path.
            let handle = unsafe {
                CreateFileW(
                    path.as_ptr(),
                    GENERIC_READ | GENERIC_WRITE,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    core::ptr::null(),
                    OPEN_EXISTING,
                    0,
                    core::ptr::null_mut(),
                )
            };
            if handle == INVALID_HANDLE_VALUE {
                continue;
            }
            let mut attributes = HIDD_ATTRIBUTES {
                Size: u32::try_from(size_of::<HIDD_ATTRIBUTES>()).unwrap(),
                VendorID: 0,
                ProductID: 0,
                VersionNumber: 0,
            };
            // SAFETY: the handle just opened and a sized structure.
            let ours = unsafe { HidD_GetAttributes(handle, &raw mut attributes) }
                && attributes.VendorID == pad::Product::VENDOR_ID
                && attributes.ProductID == product;
            if ours {
                return Some(Game(handle));
            }
            // SAFETY: the handle just opened, closed once.
            unsafe { CloseHandle(handle) };
        }
        None
    }

    /// **A guest's DualSense goes to the input driver's pad while the owner
    /// has the setting on, and what a game writes comes back**: made at its
    /// first report, held until the system lists it, then the report a game
    /// reads; an output report a game writes -- to the interrupt pipe, as games
    /// write one -- handed back for the guest; the pad gone at its unplug.
    #[test]
    #[ignore = "makes a DualSense on the input driver"]
    fn a_guests_dualsense_goes_to_the_drivers_pad_and_its_writes_come_back() {
        use windows_sys::Win32::Devices::HumanInterfaceDevice::HidD_GetInputReport;
        use windows_sys::Win32::Storage::FileSystem::WriteFile;
        const HELD: &[u8; pad::INPUT_LEN] =
            include_bytes!("../../core/tests/data/pad/ds5/input-held.bin");
        let mut devices = Devices::create("hid-pad-test").unwrap();
        let mut inject = Injector::new(Extents::alone(1920, 1080));
        devices.set_virtual_pads(true);
        assert!(devices.takes_reports(), "the setting on, the reports taken");
        let report = Control {
            a0: 64,
            a1: 7,
            a2: u32::from(pad::Product::DualSense.product_id()),
            opcode: op::PAD_REPORT,
            body: HELD,
        };
        inject.on_control(&report, &mut devices);
        let slot = devices
            .hid_pads
            .iter()
            .flatten()
            .next()
            .expect("a pad from the driver")
            .slot;
        let ready = until(&mut devices, Duration::from_secs(10), "listed", |d| {
            d.hid_pads.iter().flatten().any(|pad| pad.ready)
        });

        let game =
            drivers_pad(pad::Product::DualSense.product_id()).expect("the pad as a game finds it");
        let game_handle = game.0;
        let mut read = [0u8; pad::INPUT_LEN];
        read[0] = pad::USB_INPUT_ID;
        // SAFETY: the game's handle and a buffer of its length.
        let got = unsafe {
            HidD_GetInputReport(
                game_handle,
                read.as_mut_ptr().cast(),
                u32::try_from(read.len()).unwrap(),
            )
        };
        assert!(got, "the pad's input report not read");
        assert_eq!(&read, HELD, "the report a game reads is not the guest's");

        let mut lights = [0u8; pad::DS5_OUTPUT_MIN_LEN];
        lights[0] = pad::DS5_OUTPUT_ID;
        lights[2] = 0x04;
        lights[45..48].copy_from_slice(&[0x00, 0xFF, 0x40]);
        let mut moved = 0u32;
        // SAFETY: the game's handle, opened for synchronous requests, and a
        // buffer of its length.
        let wrote = unsafe {
            WriteFile(
                game_handle,
                lights.as_ptr(),
                u32::try_from(lights.len()).unwrap(),
                &raw mut moved,
                core::ptr::null_mut(),
            )
        };
        assert!(
            wrote != 0,
            "the game's output report refused, error={}",
            last_error()
        );
        // Another program on the machine may write to a pad as it appears --
        // its own lightbar -- so the game's report is looked for among them.
        let mut back = None;
        let came = until(&mut devices, Duration::from_secs(2), "handed back", |d| {
            while back.is_none()
                && let Some(written) = d.written()
            {
                if written.written.report.get(..written.written.len) == Some(&lights[..]) {
                    back = Some(written);
                }
            }
            back.is_some()
        });
        let back = back.unwrap();
        assert_eq!(back.pad, 7);
        assert_eq!(back.written.kind, pad::OutputKind::Output);
        drop(game);

        inject.on_control(
            &Control {
                a0: 0,
                a1: 0,
                a2: 7,
                opcode: op::GAMEPAD_UNPLUG,
                body: &[],
            },
            &mut devices,
        );
        let gone = until(&mut devices, Duration::from_secs(5), "unlisted", |_| {
            !hid::listed(Kind::DualSense, slot)
        });
        drop(devices);
        println!(
            "listed after {ready:?}; a game's output report handed back {came:?} after; \
             unlisted {gone:?} after the unplug"
        );
    }
}
