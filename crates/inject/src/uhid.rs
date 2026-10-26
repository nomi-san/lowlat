//! A controller presented on the kernel's HID layer.
//!
//! **The second device layer, and the controller decides which** (docs/07-
//! platforms.md section 4.2): a DualShock 4 or a DualSense is worth presenting
//! only as itself, with its own descriptor and identity, so that the kernel's
//! own driver claims it and produces what it produces for the real thing --
//! the touchpad and the motion sensors as devices of their own, the lights,
//! and a raw node every consumer that recognises the model opens. The input
//! reports the peer sends are written into the device as they are
//! (docs/05-host.md section 7.2); what an application writes back comes out
//! here for the peer.
//!
//! **The driver asks questions when the device appears**, and will not attach
//! to one that does not answer: the pairing report, the firmware report and
//! the calibration report, by their identifiers. The answers are what the
//! peer sent ahead of its first input report, with a default for what it did
//! not send, and the address is this host's own ([`Features`]). The questions
//! arrive on the same descriptor the device was created on and have to be
//! answered within the kernel's patience, which is why the descriptor is
//! polled from the loop that already turns rather than by a thread per pad.

use core::fmt::Write as _;
use std::os::fd::{AsRawFd as _, OwnedFd};
use std::os::unix::fs::OpenOptionsExt as _;

use lowlat_core::pad::{self, Product};

/// The size of one event on the node, which is what a read returns and
/// what a create must cover: the type word and the largest request.
const EVENT_LEN: usize = 4 + 128 + 64 + 64 + 2 + 2 + 4 + 4 + 4 + 4 + 4096;

/// The event types, as the kernel numbers them.
mod event {
    pub(super) const DESTROY: u32 = 1;
    pub(super) const START: u32 = 2;
    pub(super) const STOP: u32 = 3;
    pub(super) const OPEN: u32 = 4;
    pub(super) const CLOSE: u32 = 5;
    pub(super) const OUTPUT: u32 = 6;
    pub(super) const GET_REPORT: u32 = 9;
    pub(super) const GET_REPORT_REPLY: u32 = 10;
    pub(super) const CREATE2: u32 = 11;
    pub(super) const INPUT2: u32 = 12;
    pub(super) const SET_REPORT: u32 = 13;
    pub(super) const SET_REPORT_REPLY: u32 = 14;
}

/// Where each request's fields sit after the type word.
mod at {
    pub(super) const NAME: usize = 4;
    pub(super) const PHYS: usize = 132;
    pub(super) const UNIQ: usize = 196;
    pub(super) const RD_SIZE: usize = 260;
    pub(super) const BUS: usize = 262;
    pub(super) const VENDOR: usize = 264;
    pub(super) const PRODUCT: usize = 268;
    pub(super) const VERSION: usize = 272;
    pub(super) const RD_DATA: usize = 280;
    pub(super) const INPUT2_SIZE: usize = 4;
    pub(super) const INPUT2_DATA: usize = 6;
    pub(super) const OUTPUT_DATA: usize = 4;
    pub(super) const OUTPUT_SIZE: usize = 4100;
    pub(super) const OUTPUT_RTYPE: usize = 4102;
    pub(super) const REQUEST_ID: usize = 4;
    pub(super) const REQUEST_RNUM: usize = 8;
    pub(super) const REQUEST_RTYPE: usize = 9;
    pub(super) const SET_SIZE: usize = 10;
    pub(super) const SET_DATA: usize = 12;
    pub(super) const REPLY_ERR: usize = 8;
    pub(super) const REPLY_SIZE: usize = 10;
    pub(super) const REPLY_DATA: usize = 12;
}

/// Report types in a request.
const FEATURE_REPORT: u8 = 0;
const OUTPUT_REPORT: u8 = 1;

/// The device claims the bus the real one is on, so the driver reads the USB
/// forms of every report and checks no checksum.
const BUS_USB: u16 = 0x03;
/// What the real pads report as their version; the driver reads nothing
/// from it.
const VERSION: u32 = 0x8111;

pub use crate::handback::{WRITTEN_MAX, Written};

/// Why a device could not be created; the same three tellings-apart the
/// input layer makes, because each has a different fix.
pub use crate::uinput::Error;

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// What the device answers the driver's questions with, the same answers
/// whichever platform presents the pad.
pub use crate::answers::Features;

/// The real product's report descriptor, as the kernel exposed it.
fn descriptor(product: Product) -> &'static [u8] {
    match product {
        Product::DualShock4 => include_bytes!("../../core/tests/data/pad/ds4/descriptor.bin"),
        _ => include_bytes!("../../core/tests/data/pad/ds5/descriptor.bin"),
    }
}

/// One controller on the HID layer.
///
/// **No `Drop`, deliberately.** Closing the descriptor destroys the device,
/// as it does on the input layer.
#[derive(Debug)]
pub struct HidPad {
    fd: OwnedFd,
    product: Product,
    address: [u8; 6],
    features: Features,
    /// Whether the kernel has the device running (see [`Self::started`]).
    started: bool,
}

/// A fixed-size text field, NUL padded.
fn put_text(out: &mut [u8], text: &str) {
    for (dst, src) in out
        .iter_mut()
        .zip(text.bytes().chain(core::iter::repeat(0)))
    {
        *dst = src;
    }
    if let Some(last) = out.last_mut() {
        *last = 0;
    }
}

impl HidPad {
    /// Create the device. The driver's questions follow on the descriptor
    /// and are answered by [`Self::poll`].
    ///
    /// `guest` and `pad` name the device's physical location, as the input
    /// layer's devices are named; `address` is the identity the driver keys
    /// on and must be unique on this host.
    pub fn create(
        guest: &str,
        pad: u32,
        product: Product,
        address: [u8; 6],
        features: Features,
    ) -> Result<Self, Error> {
        let fd = match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open("/dev/uhid")
        {
            Ok(file) => OwnedFd::from(file),
            Err(error) => {
                return Err(match error.kind() {
                    std::io::ErrorKind::NotFound => Error::NoModule,
                    std::io::ErrorKind::PermissionDenied => Error::NotPermitted,
                    _ => Error::Failed(error.raw_os_error().unwrap_or(0)),
                });
            }
        };
        let rd = descriptor(product);
        let mut request = [0u8; EVENT_LEN];
        put_u32(&mut request, 0, event::CREATE2);
        if let Some(name) = request.get_mut(at::NAME..at::PHYS) {
            put_text(name, product.name());
        }
        if let Some(phys) = request.get_mut(at::PHYS..at::UNIQ) {
            let mut text = Text::default();
            let _ = write!(text, "lowlat/{guest}/pad{pad}");
            put_text(phys, text.as_str());
        }
        if let Some(uniq) = request.get_mut(at::UNIQ..at::RD_SIZE) {
            let mut text = Text::default();
            let _ = write!(
                text,
                "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                address.first().copied().unwrap_or(0),
                address.get(1).copied().unwrap_or(0),
                address.get(2).copied().unwrap_or(0),
                address.get(3).copied().unwrap_or(0),
                address.get(4).copied().unwrap_or(0),
                address.get(5).copied().unwrap_or(0)
            );
            put_text(uniq, text.as_str());
        }
        put_u16(
            &mut request,
            at::RD_SIZE,
            u16::try_from(rd.len()).unwrap_or(0),
        );
        put_u16(&mut request, at::BUS, BUS_USB);
        put_u32(&mut request, at::VENDOR, u32::from(Product::VENDOR_ID));
        put_u32(&mut request, at::PRODUCT, u32::from(product.product_id()));
        put_u32(&mut request, at::VERSION, VERSION);
        if let Some(data) = request.get_mut(at::RD_DATA..at::RD_DATA + rd.len()) {
            data.copy_from_slice(rd);
        }
        // The whole request, descriptor included: the kernel reads the
        // descriptor's length from the request and the rest from the write.
        let len = at::RD_DATA + rd.len();
        if !write_all(&fd, request.get(..len).unwrap_or(&[])) {
            return Err(match errno() {
                libc::EPERM | libc::EACCES => Error::NotPermitted,
                other => Error::Confined(other),
            });
        }
        Ok(Self {
            fd,
            product,
            address,
            features,
            started: false,
        })
    }

    #[must_use]
    pub const fn product(&self) -> Product {
        self.product
    }

    /// Whether the kernel has the device running: started for a driver's
    /// probe, and stopped again if the probe failed.
    #[must_use]
    pub const fn started(&self) -> bool {
        self.started
    }

    /// One input report in the USB form, into the device.
    pub fn input(&self, report: &[u8; pad::INPUT_LEN]) -> Result<(), Error> {
        let mut request = [0u8; at::INPUT2_DATA + pad::INPUT_LEN];
        put_u32(&mut request, 0, event::INPUT2);
        put_u16(
            &mut request,
            at::INPUT2_SIZE,
            u16::try_from(pad::INPUT_LEN).unwrap_or(0),
        );
        if let Some(data) = request.get_mut(at::INPUT2_DATA..) {
            data.copy_from_slice(report);
        }
        if write_all(&self.fd, &request) {
            Ok(())
        } else {
            Err(Error::Failed(errno()))
        }
    }

    /// Service what the kernel put on the descriptor: the driver's questions
    /// are answered here, and a report an application wrote to the device
    /// is returned, one per call. `None` when there is nothing more.
    pub fn poll(&mut self) -> Option<Written> {
        let mut event = [0u8; EVENT_LEN];
        loop {
            // SAFETY: the buffer is EVENT_LEN bytes and the descriptor is
            // open for the call's duration.
            let n =
                unsafe { libc::read(self.fd.as_raw_fd(), event.as_mut_ptr().cast(), event.len()) };
            if n < 4 {
                return None;
            }
            match get_u32(&event, 0) {
                event::START => self.started = true,
                event::STOP => self.started = false,
                event::GET_REPORT => self.answer(&event),
                event::SET_REPORT => {
                    let id = get_u32(&event, at::REQUEST_ID);
                    // A set-report carries its kind too: a writer without the
                    // output path reaches this with an output report.
                    let rtype = event
                        .get(at::REQUEST_RTYPE)
                        .copied()
                        .unwrap_or(FEATURE_REPORT);
                    let written = self.written(&event, at::SET_DATA, at::SET_SIZE, rtype);
                    self.reply(event::SET_REPORT_REPLY, id, 0, &[]);
                    if let Some(written) = written {
                        return Some(written);
                    }
                }
                event::OUTPUT => {
                    let rtype = event
                        .get(at::OUTPUT_RTYPE)
                        .copied()
                        .unwrap_or(OUTPUT_REPORT);
                    if let Some(written) =
                        self.written(&event, at::OUTPUT_DATA, at::OUTPUT_SIZE, rtype)
                    {
                        return Some(written);
                    }
                }
                event::OPEN | event::CLOSE | event::DESTROY => {}
                _ => {}
            }
        }
    }

    /// A report the kernel handed down, as [`Written`].
    fn written(&self, event: &[u8], data_at: usize, size_at: usize, rtype: u8) -> Option<Written> {
        let size = usize::from(get_u16(event, size_at));
        let data = event.get(data_at..data_at + size)?;
        // A report written without its identifier byte carries the device's
        // only one; the driver and the raw node both write it with the byte,
        // so an empty or oversize write is dropped rather than guessed at --
        // and said, because a writer framing for the other transport looks
        // like silence otherwise.
        if data.is_empty() || data.len() > WRITTEN_MAX {
            lowlat_common::log_warn!(
                "inject: pad write dropped, len={} id={:#04x} type={rtype}",
                data.len(),
                data.first().copied().unwrap_or(0)
            );
            return None;
        }
        let mut report = [0u8; WRITTEN_MAX];
        report.get_mut(..data.len())?.copy_from_slice(data);
        Some(Written {
            kind: if rtype == FEATURE_REPORT {
                pad::OutputKind::Feature
            } else {
                pad::OutputKind::Output
            },
            len: data.len(),
            report,
        })
    }

    /// Answer a question with the feature report it names, or with an error
    /// for one nobody defined.
    fn answer(&self, event: &[u8]) {
        let id = get_u32(event, at::REQUEST_ID);
        let rnum = event.get(at::REQUEST_RNUM).copied().unwrap_or(0);
        let rtype = event
            .get(at::REQUEST_RTYPE)
            .copied()
            .unwrap_or(FEATURE_REPORT);
        let mut out = [0u8; pad::FEATURE_MAX];
        let answer = if rtype == FEATURE_REPORT {
            self.features
                .answer(self.product, rnum, &self.address, &mut out)
        } else {
            None
        };
        match answer {
            Some(len) => self.reply(
                event::GET_REPORT_REPLY,
                id,
                0,
                out.get(..len).unwrap_or(&[]),
            ),
            None => {
                lowlat_common::log_info!(
                    "inject: pad asked for a report nobody defined, id={rnum:#x} type={rtype}"
                );
                self.reply(
                    event::GET_REPORT_REPLY,
                    id,
                    u16::try_from(libc::EIO).unwrap_or(5),
                    &[],
                );
            }
        }
    }

    fn reply(&self, kind: u32, id: u32, err: u16, data: &[u8]) {
        let mut request = [0u8; at::REPLY_DATA + pad::FEATURE_MAX];
        put_u32(&mut request, 0, kind);
        put_u32(&mut request, at::REQUEST_ID, id);
        put_u16(&mut request, at::REPLY_ERR, err);
        let len = if kind == event::GET_REPORT_REPLY {
            put_u16(
                &mut request,
                at::REPLY_SIZE,
                u16::try_from(data.len()).unwrap_or(0),
            );
            if let Some(dst) = request.get_mut(at::REPLY_DATA..at::REPLY_DATA + data.len()) {
                dst.copy_from_slice(data);
            }
            at::REPLY_DATA + data.len()
        } else {
            at::REPLY_SIZE
        };
        if !write_all(&self.fd, request.get(..len).unwrap_or(&[])) {
            lowlat_common::log_warn!("inject: pad reply refused, errno={}", errno());
        }
    }
}

fn write_all(fd: &OwnedFd, bytes: &[u8]) -> bool {
    // SAFETY: the pointer and length describe `bytes`, and the descriptor is
    // open for the call's duration.
    let n = unsafe { libc::write(fd.as_raw_fd(), bytes.as_ptr().cast(), bytes.len()) };
    usize::try_from(n).is_ok_and(|n| n == bytes.len())
}

fn put_u16(out: &mut [u8], at: usize, value: u16) {
    if let Some(dst) = out.get_mut(at..at + 2) {
        dst.copy_from_slice(&value.to_ne_bytes());
    }
}

fn put_u32(out: &mut [u8], at: usize, value: u32) {
    if let Some(dst) = out.get_mut(at..at + 4) {
        dst.copy_from_slice(&value.to_ne_bytes());
    }
}

fn get_u16(src: &[u8], at: usize) -> u16 {
    src.get(at..at + 2)
        .and_then(|s| <[u8; 2]>::try_from(s).ok())
        .map_or(0, u16::from_ne_bytes)
}

fn get_u32(src: &[u8], at: usize) -> u32 {
    src.get(at..at + 4)
        .and_then(|s| <[u8; 4]>::try_from(s).ok())
        .map_or(0, u32::from_ne_bytes)
}

/// A small fixed text buffer for the device's strings.
#[derive(Debug)]
struct Text {
    bytes: [u8; 64],
    len: usize,
}

impl Default for Text {
    fn default() -> Self {
        Self {
            bytes: [0; 64],
            len: 0,
        }
    }
}

impl Text {
    fn as_str(&self) -> &str {
        core::str::from_utf8(self.bytes.get(..self.len).unwrap_or(&[])).unwrap_or("")
    }
}

impl core::fmt::Write for Text {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let take = s.len().min(self.bytes.len() - self.len);
        if let (Some(dst), Some(src)) = (
            self.bytes.get_mut(self.len..self.len + take),
            s.as_bytes().get(..take),
        ) {
            dst.copy_from_slice(src);
            self.len += take;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The descriptors are the real products' whole.
    #[test]
    fn the_descriptors_are_the_real_products() {
        assert_eq!(descriptor(Product::DualShock4).len(), 507);
        assert_eq!(descriptor(Product::DualSense).len(), 289);
    }

    /// Text fields are NUL padded and never overrun.
    #[test]
    fn text_fields_are_padded_and_bounded() {
        let mut field = [0xFFu8; 8];
        put_text(&mut field, "abc");
        assert_eq!(&field, b"abc\0\0\0\0\0");
        put_text(&mut field, "a longer name than fits");
        assert_eq!(&field[..7], b"a longe");
        assert_eq!(field[7], 0);
    }

    /// **The device, on the real kernel**: created, claimed by the kernel's
    /// own driver after its three questions, its raw node and evdev nodes
    /// present with the identity given, an input report accepted, the
    /// lightbar reset the driver writes at probe coming back as an output
    /// report, and a second device under the same address refused. Needs
    /// `/dev/uhid`, so off by default.
    #[test]
    #[ignore = "needs /dev/uhid and the playstation driver"]
    fn a_dualsense_is_claimed_by_the_kernels_own_driver() {
        let ds5_idle: &[u8; pad::INPUT_LEN] =
            include_bytes!("../../core/tests/data/pad/ds5/input-idle.bin");
        let ds4_idle: &[u8; pad::INPUT_LEN] =
            include_bytes!("../../core/tests/data/pad/ds4/input-idle.bin");
        for product in [Product::DualSense, Product::DualShock4] {
            let idle = match product {
                Product::DualShock4 => ds4_idle,
                _ => ds5_idle,
            };
            let address = [0x02, 0x4c, 0x4c, 0x00, 0x01, 0x07];
            let mut pad =
                HidPad::create("test", 7, product, address, Features::new()).expect("the device");
            let mut outputs = Vec::new();
            let began = std::time::Instant::now();
            while began.elapsed() < std::time::Duration::from_secs(3) {
                while let Some(written) = pad.poll() {
                    outputs.push(written);
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            assert!(
                pad.started(),
                "{product:?}: the driver never started the device"
            );
            // The driver's probe ends with an output report: the lights.
            assert!(
                outputs.iter().any(
                    |w| w.kind == pad::OutputKind::Output && w.report[0] == product.output_id()
                ),
                "{product:?}: no output report from the probe, got {outputs:?}"
            );
            // The nodes the driver makes, under our identity.
            let uniq = "02:4c:4c:00:01:07";
            let mut nodes = 0;
            for entry in std::fs::read_dir("/sys/class/input").expect("sysfs") {
                let path = entry.expect("entry").path();
                // The event nodes alone: the joystick and mouse handlers of
                // the same devices sit beside them.
                if !path
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with("event"))
                {
                    continue;
                }
                let Ok(found) = std::fs::read_to_string(path.join("device/uniq")) else {
                    continue;
                };
                if found.trim() == uniq {
                    nodes += 1;
                    // The location is the HID device's, the parent of the
                    // input device; the driver gives the input nodes none.
                    let uevent = std::fs::read_to_string(path.join("device/device/uevent"))
                        .unwrap_or_default();
                    assert!(
                        uevent.contains("HID_PHYS=lowlat/test/pad7\n"),
                        "{product:?}: the location is missing from {uevent}"
                    );
                }
            }
            assert_eq!(
                nodes, 3,
                "{product:?}: the pad, the motion sensors and the touchpad"
            );
            let mut report = *idle;
            // Cross held, in each product's own button byte.
            let buttons_at = match product {
                Product::DualShock4 => 5,
                _ => 8,
            };
            report[buttons_at] |= 0x20;
            pad.input(&report).expect("an input report");
            // The same address again is refused by the driver, which is why
            // the address must be unique per pad: the kernel starts the
            // device for the probe and stops it when the probe fails, so
            // after its questions the twin is not running.
            let mut twin = HidPad::create("test", 8, product, address, Features::new())
                .expect("the node opens");
            let began = std::time::Instant::now();
            let mut ran = false;
            while began.elapsed() < std::time::Duration::from_secs(2) {
                let _ = twin.poll();
                ran |= twin.started();
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            assert!(ran, "{product:?}: the twin was never probed");
            assert!(
                !twin.started(),
                "{product:?}: a second device under the same address was accepted"
            );
            drop(twin);
            drop(pad);
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    }

    /// **A window onto what an application writes to the virtual pads.**
    /// Both products are presented for five minutes and every write that
    /// reaches the descriptor is printed with its time, kind, length and
    /// head, as is any write dropped for its size. Not a check: run it with
    /// `--nocapture` while a game launcher or a page drives the pads.
    #[test]
    #[ignore = "a window, not a check; needs /dev/uhid"]
    fn whatever_is_written_to_the_virtual_pads_is_printed() {
        lowlat_common::log::set_sink(|level, message| eprintln!("[{level:?}] {message}"));
        let ds4 = HidPad::create(
            "window",
            1,
            Product::DualShock4,
            [0x02, 0x4c, 0x4c, 0x00, 0x02, 0x01],
            Features::new(),
        )
        .expect("the DualShock 4");
        let ds5 = HidPad::create(
            "window",
            2,
            Product::DualSense,
            [0x02, 0x4c, 0x4c, 0x00, 0x02, 0x02],
            Features::new(),
        )
        .expect("the DualSense");
        let ds4_idle: &[u8; pad::INPUT_LEN] =
            include_bytes!("../../core/tests/data/pad/ds4/input-idle.bin");
        let ds5_idle: &[u8; pad::INPUT_LEN] =
            include_bytes!("../../core/tests/data/pad/ds5/input-idle.bin");
        let mut pads = [(ds4, "DualShock4", ds4_idle), (ds5, "DualSense", ds5_idle)];
        let began = std::time::Instant::now();
        let mut fed = 0u64;
        while began.elapsed() < std::time::Duration::from_secs(300) {
            // Alive: an idle report every four milliseconds, a wired pad's
            // rate, so a consumer that waits for input sees some.
            if began.elapsed().as_millis() as u64 / 4 > fed {
                fed += 1;
                for (pad, _, idle) in &mut pads {
                    if pad.started() {
                        let _ = pad.input(idle);
                    }
                }
            }
            for (pad, name, _) in &mut pads {
                while let Some(written) = pad.poll() {
                    eprintln!(
                        "{:8.3}s {name} {:?} len={} {:02x?}",
                        began.elapsed().as_secs_f64(),
                        written.kind,
                        written.len,
                        &written.report[..written.len.min(12)]
                    );
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    /// **A rumble raised on the virtual pad comes back as its output report
    /// with the motors set**, the driver's own force-feedback path: an effect
    /// uploaded and played on the pad's event node reaches the descriptor as
    /// the report the peer's real pad is to be written. Needs `/dev/uhid` and
    /// the seat's access to the pad's event node, so off by default.
    #[test]
    #[ignore = "needs /dev/uhid and the playstation driver"]
    fn a_rumble_on_the_virtual_pad_comes_back_as_its_output_report() {
        #[repr(C)]
        struct FfEffect {
            kind: u16,
            id: i16,
            direction: u16,
            trigger: [u16; 2],
            replay: [u16; 2],
            _pad: [u8; 2],
            // The union, of which the rumble effect is the first four bytes:
            // strong then weak, each a u16.
            u: [u64; 4],
        }
        const EVIOCSFF: libc::c_ulong = 0x4030_4580;
        const FF_RUMBLE: u16 = 0x50;
        const EV_FF: u16 = 0x15;

        for (product, motors_at) in [(Product::DualSense, 3), (Product::DualShock4, 4)] {
            let address = [0x02, 0x4c, 0x4c, 0x00, 0x01, 0x09];
            let mut pad =
                HidPad::create("test", 9, product, address, Features::new()).expect("the device");
            let began = std::time::Instant::now();
            while began.elapsed() < std::time::Duration::from_secs(3) {
                let _ = pad.poll();
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            assert!(pad.started(), "{product:?}: never started");

            // The gamepad node: ours by address, and not the sensors or the
            // touchpad beside it.
            let uniq = "02:4c:4c:00:01:09";
            let mut node = None;
            for entry in std::fs::read_dir("/sys/class/input").expect("sysfs") {
                let path = entry.expect("entry").path();
                let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
                let Some(name) = name.filter(|n| n.starts_with("event")) else {
                    continue;
                };
                let found = std::fs::read_to_string(path.join("device/uniq")).unwrap_or_default();
                let device = std::fs::read_to_string(path.join("device/name")).unwrap_or_default();
                if found.trim() == uniq
                    && !device.contains("Motion")
                    && !device.contains("Touchpad")
                {
                    node = Some(format!("/dev/input/{name}"));
                }
            }
            let node = node.expect("the pad's event node");
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&node)
                .unwrap_or_else(|e| panic!("{node}: {e}"));
            let mut effect = FfEffect {
                kind: FF_RUMBLE,
                id: -1,
                direction: 0,
                trigger: [0; 2],
                replay: [1000, 0],
                _pad: [0; 2],
                u: [0; 4],
            };
            effect.u[0] = u64::from(0xC000u16) | (u64::from(0x8000u16) << 16);
            // SAFETY: the structure is the kernel's own layout and the
            // descriptor is open.
            let rc = unsafe { libc::ioctl(file.as_raw_fd(), EVIOCSFF, &raw mut effect) };
            assert_eq!(rc, 0, "{product:?}: uploading the effect failed");
            let play = |value: i32| {
                use std::io::Write as _;
                let mut event = [0u8; 24];
                event[16..18].copy_from_slice(&EV_FF.to_ne_bytes());
                event[18..20].copy_from_slice(&effect.id.to_ne_bytes());
                event[20..24].copy_from_slice(&value.to_ne_bytes());
                (&file)
                    .write_all(&event)
                    .unwrap_or_else(|e| panic!("{product:?}: playing failed: {e}"));
            };
            play(1);
            let mut outputs = Vec::new();
            let began = std::time::Instant::now();
            while began.elapsed() < std::time::Duration::from_millis(300) {
                while let Some(written) = pad.poll() {
                    outputs.push(written);
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            play(0);
            let began = std::time::Instant::now();
            while began.elapsed() < std::time::Duration::from_millis(300) {
                while let Some(written) = pad.poll() {
                    outputs.push(written);
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            let heads: Vec<Vec<u8>> = outputs.iter().map(|w| w.report[..8].to_vec()).collect();
            eprintln!("{product:?}: outputs {heads:02x?}");
            assert!(
                outputs.iter().any(|w| w.kind == pad::OutputKind::Output
                    && w.report[0] == product.output_id()
                    && w.report[motors_at] == 0x80
                    && w.report[motors_at + 1] == 0xC0),
                "{product:?}: no output report carried the motors, got {heads:02x?}"
            );
            assert!(
                outputs.iter().any(|w| w.kind == pad::OutputKind::Output
                    && w.report[motors_at] == 0
                    && w.report[motors_at + 1] == 0
                    && w.report[1] != 0),
                "{product:?}: the stop never came, got {heads:02x?}"
            );
            drop(pad);
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    }
}
