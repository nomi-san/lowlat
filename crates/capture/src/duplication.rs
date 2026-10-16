//! Desktop duplication: one output's composed desktop, handed over by the
//! system as a texture on the capture's device each time it changes, and the
//! pointer the system draws over it, reported beside it (docs/05-host.md
//! sections 2 and 8).
//!
//! **The calling thread's state is part of the interface.** The duplication
//! call refuses a thread that is not per-monitor aware, and every coordinate
//! the system hands back is scaled on one that is not; and the duplication
//! and the pointer both read the desktop the thread is attached to, which the
//! lock screen and an elevation prompt replace with another. So [`Aware`],
//! [`Awake`] and [`InputDesktop`] belong to the thread that made them, and
//! everything here runs on that thread.

use core::marker::PhantomData;

use lowlat_drivers::d3d11::{self as dx, Com, Device};
use lowlat_drivers::ffi::d3d11::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_MODE_ROTATION, DXGI_OUTDUPL_DESC,
    DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTDUPL_POINTER_SHAPE_INFO,
    DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR, DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR,
    DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME, HRESULT, ID3D11Texture2D, IDXGIOutput,
    IDXGIOutput5, IDXGIOutputDuplication, IDXGIResource, IUnknown,
};
use lowlat_drivers::ffi::d3d11_guids::{IID_ID3D11Texture2D, IID_IDXGIOutput5};
use lowlat_drivers::vcall;
use windows_sys::Win32::Devices::Display::{
    DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
    DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_SOURCE_DEVICE_NAME,
    DISPLAYCONFIG_TARGET_DEVICE_NAME, DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes,
    QDC_ONLY_ACTIVE_PATHS, QueryDisplayConfig,
};
use windows_sys::Win32::Foundation::GENERIC_ALL;
use windows_sys::Win32::Graphics::Dwm::{DWM_TIMING_INFO, DwmGetCompositionTimingInfo};
use windows_sys::Win32::Graphics::Gdi::{DISPLAY_DEVICEW, EnumDisplayDevicesW};
use windows_sys::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows_sys::Win32::System::Power::{
    ES_CONTINUOUS, ES_DISPLAY_REQUIRED, SetThreadExecutionState,
};
use windows_sys::Win32::System::RemoteDesktop::{
    ProcessIdToSessionId, WTSGetActiveConsoleSessionId,
};
use windows_sys::Win32::System::StationsAndDesktops::{
    CloseDesktop, GetUserObjectInformationW, HDESK, OpenInputDesktop, SetThreadDesktop, UOI_NAME,
};
use windows_sys::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetThreadDpiAwarenessContext,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_MOVE, MOUSEINPUT, SendInput,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CURSOR_SHOWING, CURSOR_SUPPRESSED, CURSORINFO, GetCursorInfo,
};

/// The largest pointer the wire carries a picture of, a side.
pub const POINTER_LIMIT: u32 = 256;

/// The duplication answered nothing within the time.
const DXGI_ERROR_WAIT_TIMEOUT: HRESULT = 0x887A_0027_u32 as HRESULT;
/// The desktop the duplication was made on is gone: switched, or its mode
/// changed.
const DXGI_ERROR_ACCESS_LOST: HRESULT = 0x887A_0026_u32 as HRESULT;
const DXGI_ERROR_INVALID_CALL: HRESULT = 0x887A_0001_u32 as HRESULT;
const DXGI_ERROR_UNSUPPORTED: HRESULT = 0x887A_0004_u32 as HRESULT;
const DXGI_ERROR_NOT_CURRENTLY_AVAILABLE: HRESULT = 0x887A_0022_u32 as HRESULT;
const DXGI_ERROR_SESSION_DISCONNECTED: HRESULT = 0x887A_0028_u32 as HRESULT;
const DXGI_ERROR_DEVICE_REMOVED: HRESULT = 0x887A_0005_u32 as HRESULT;
const DXGI_ERROR_DEVICE_HUNG: HRESULT = 0x887A_0006_u32 as HRESULT;
const DXGI_ERROR_DEVICE_RESET: HRESULT = 0x887A_0007_u32 as HRESULT;
const E_ACCESSDENIED: HRESULT = 0x8007_0005_u32 as HRESULT;

/// Why the duplication could not be had or read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// The desktop it was made on is gone, to a switch of desktop or of
    /// mode: made again, on the input desktop, it answers.
    Lost,
    /// Refused for now: the desktop is out of this process's reach while it
    /// is switching or locked, or the session is not the console's.
    Busy(i32),
    /// Refused outright, which a thread that is not per-monitor aware is,
    /// and an output the device cannot duplicate.
    Unsupported,
    /// The device is gone, and nothing made on it answers again.
    DeviceLost(i32),
    /// Any other failure, with its result.
    Status(i32),
    /// The device's own call failed.
    Device(dx::Error),
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Lost => f.write_str("the duplicated desktop is gone"),
            Self::Busy(hr) => write!(f, "the desktop is out of reach for now, 0x{hr:08x}"),
            Self::Unsupported => f.write_str("the output cannot be duplicated from this thread"),
            Self::DeviceLost(hr) => write!(f, "the device is gone, 0x{hr:08x}"),
            Self::Status(hr) => write!(f, "duplication call returned 0x{hr:08x}"),
            Self::Device(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for Error {}

/// A result as this module reads it.
fn classify(hr: HRESULT) -> Result<(), Error> {
    match hr {
        _ if hr >= 0 => Ok(()),
        DXGI_ERROR_ACCESS_LOST | DXGI_ERROR_INVALID_CALL => Err(Error::Lost),
        E_ACCESSDENIED | DXGI_ERROR_NOT_CURRENTLY_AVAILABLE | DXGI_ERROR_SESSION_DISCONNECTED => {
            Err(Error::Busy(hr))
        }
        DXGI_ERROR_UNSUPPORTED => Err(Error::Unsupported),
        DXGI_ERROR_DEVICE_REMOVED | DXGI_ERROR_DEVICE_HUNG | DXGI_ERROR_DEVICE_RESET => {
            Err(Error::DeviceLost(hr))
        }
        _ => Err(Error::Status(hr)),
    }
}

/// The calling thread made per-monitor aware until the guard drops, which
/// puts back what it was.
///
/// **The thread's, not the process's**, so a library sets it on its own
/// threads and on a call it serves without moving its application's.
#[derive(Debug)]
pub struct Aware {
    previous: DPI_AWARENESS_CONTEXT,
    /// Put back on the thread that set it.
    _thread: PhantomData<*const ()>,
}

impl Aware {
    pub fn per_monitor() -> Self {
        // SAFETY: a context the system defines; the call changes only this
        // thread's.
        let previous =
            unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        Self {
            previous,
            _thread: PhantomData,
        }
    }
}

impl Drop for Aware {
    fn drop(&mut self) {
        // A null context means the call above refused, and there is nothing
        // to put back.
        if !self.previous.is_null() {
            // SAFETY: the context this thread had, set back on it.
            unsafe { SetThreadDpiAwarenessContext(self.previous) };
        }
    }
}

/// The display kept on while the guard lives: without it a monitor nobody
/// at the desk touches goes to sleep under a guest, and the duplication
/// delivers nothing.
#[derive(Debug)]
pub struct Awake {
    _thread: PhantomData<*const ()>,
}

impl Awake {
    pub fn hold() -> Self {
        // SAFETY: flags the call documents; the state is this thread's.
        unsafe { SetThreadExecutionState(ES_CONTINUOUS | ES_DISPLAY_REQUIRED) };
        Self {
            _thread: PhantomData,
        }
    }
}

impl Drop for Awake {
    fn drop(&mut self) {
        // SAFETY: as above; continuous alone clears what the hold set.
        unsafe { SetThreadExecutionState(ES_CONTINUOUS) };
    }
}

/// Wake the display with one pointer move of no distance. **Holding it on
/// ([`Awake`]) does not wake one that slept**, which gives no pictures until
/// input wakes it: measured, held asleep for eight seconds, woken by this in
/// 170 ms. Answers whether the system took the move, which it does from the
/// input desktop alone.
pub fn wake() -> bool {
    let input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: 0,
                dwFlags: MOUSEEVENTF_MOVE,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    // SAFETY: one live input of the size stated.
    let inserted = unsafe {
        SendInput(
            1,
            &raw const input,
            i32::try_from(size_of::<INPUT>()).unwrap_or(0),
        )
    };
    inserted == 1
}

/// The desktop that receives input, which this thread is attached to.
///
/// **Kept rather than closed.** A desktop a thread is attached to cannot be
/// closed, so the handle lives until the thread moves to another, and the
/// one it replaces is closed then.
#[derive(Debug)]
pub struct InputDesktop {
    handle: HDESK,
    _thread: PhantomData<*const ()>,
}

impl InputDesktop {
    /// Attach this thread to the input desktop as it is now: the user's,
    /// the lock screen's or an elevation prompt's. Refused on a thread that
    /// owns a window or a hook, and where this process may not open it.
    pub fn attach() -> Result<Self, Error> {
        // SAFETY: no flags, not inherited, every right; the handle is checked.
        let handle = unsafe { OpenInputDesktop(0, 0, GENERIC_ALL) };
        if handle.is_null() {
            return Err(Error::Busy(last_error()));
        }
        // SAFETY: a desktop just opened.
        if unsafe { SetThreadDesktop(handle) } == 0 {
            let error = last_error();
            // SAFETY: the handle opened above, attached to nothing.
            unsafe { CloseDesktop(handle) };
            return Err(Error::Busy(error));
        }
        Ok(Self {
            handle,
            _thread: PhantomData,
        })
    }

    /// The desktop's own name -- `Default` for the user's, `Winlogon` for
    /// the secure one -- or empty where the system will not say.
    pub fn name(&self) -> String {
        let mut name = [0u16; 64];
        let mut needed = 0u32;
        // SAFETY: a live desktop handle; the buffer and its length in bytes
        // are the local's, and the length out is a live local.
        let read = unsafe {
            GetUserObjectInformationW(
                self.handle,
                UOI_NAME,
                name.as_mut_ptr().cast(),
                u32::try_from(size_of_val(&name)).unwrap_or(0),
                &raw mut needed,
            )
        };
        if read == 0 {
            return String::new();
        }
        let end = name.iter().position(|&c| c == 0).unwrap_or(name.len());
        String::from_utf16_lossy(name.get(..end).unwrap_or_default())
    }
}

impl Drop for InputDesktop {
    fn drop(&mut self) {
        // **Fails while this thread is still attached to it**, which is the
        // ordinary case at a teardown; the handle then goes with the thread.
        // SAFETY: the handle `attach` opened, closed once.
        unsafe { CloseDesktop(self.handle) };
    }
}

/// The calling thread's last error as a result.
fn last_error() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// What the duplication says its output is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mode {
    /// The image's size, which is the panel's: on a turned output the
    /// desktop's sides are these the other way round.
    pub width: u32,
    pub height: u32,
    /// Presents a second, as a ratio.
    pub refresh: (u32, u32),
    pub rotation: DXGI_MODE_ROTATION,
}

/// One frame the duplication handed over.
#[derive(Debug)]
pub struct Frame {
    /// The image, when the frame carries a new one: the duplication's own
    /// texture, read until the frame is released and never after.
    pub image: Option<Com<ID3D11Texture2D>>,
    /// When the system presented that image, by the performance counter.
    pub presented: i64,
    /// What the frame says of the pointer, when it moved or changed.
    pub pointer: Option<Seen>,
}

/// What one frame says of the pointer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Seen {
    /// The system draws it over the image, rather than into the image or not
    /// at all.
    pub visible: bool,
    /// The bytes of a new shape the frame carries; zero for none.
    pub shape_bytes: u32,
}

/// A pointer's shape as the system describes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shape {
    pub kind: Kind,
    /// The picture's size, a monochrome one's height the real one.
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub hot_x: i32,
    pub hot_y: i32,
}

/// The three kinds of shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Two one-bit masks, AND then XOR, the picture's height twice over.
    Monochrome,
    /// Straight BGRA.
    Colour,
    /// BGRA whose alpha says, for each pixel, opaque or inverted.
    MaskedColour,
}

/// The duplication of one output on one device.
pub struct Duplication {
    duplication: Com<IDXGIOutputDuplication>,
    /// A frame is acquired and not yet released.
    held: bool,
    mode: Mode,
}

impl core::fmt::Debug for Duplication {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Duplication")
            .field("held", &self.held)
            .field("mode", &self.mode)
            .finish()
    }
}

impl Duplication {
    /// Duplicate `output` on `device`, asked for eight-bit BGRA: on a desktop
    /// in a wider format the system converts.
    ///
    /// **The calling thread must be per-monitor aware** ([`Aware`]), or the
    /// call is refused as unsupported.
    pub fn new(device: &Device, output: &Com<IDXGIOutput>) -> Result<Self, Error> {
        let five = dx::query::<IDXGIOutput5>(output.as_ptr().cast(), &IID_IDXGIOutput5)
            .map_err(|_| Error::Unsupported)?;
        let formats: [DXGI_FORMAT; 1] = [DXGI_FORMAT_B8G8R8A8_UNORM];
        let mut raw: *mut IDXGIOutputDuplication = core::ptr::null_mut();
        // SAFETY: a live output and device; the format list and the output
        // are live locals.
        let hr = unsafe {
            vcall!(
                five.as_ptr(),
                DuplicateOutput1,
                device.device().cast::<IUnknown>(),
                0,
                1,
                formats.as_ptr(),
                &raw mut raw
            )
        }
        .ok_or(Error::Device(dx::Error::MissingSymbol))?;
        classify(hr)?;
        // SAFETY: a duplication whose reference the call handed over.
        let duplication = unsafe { Com::from_raw(raw) }.ok_or(Error::Unsupported)?;
        // SAFETY: plain data the call fills whole.
        let mut desc: DXGI_OUTDUPL_DESC = unsafe { core::mem::zeroed() };
        // SAFETY: a live duplication; the output is a live local.
        unsafe { vcall!(duplication.as_ptr(), GetDesc, &raw mut desc) };
        let mode = Mode {
            width: desc.ModeDesc.Width,
            height: desc.ModeDesc.Height,
            refresh: (
                desc.ModeDesc.RefreshRate.Numerator,
                desc.ModeDesc.RefreshRate.Denominator,
            ),
            rotation: desc.Rotation,
        };
        Ok(Self {
            duplication,
            held: false,
            mode,
        })
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// The next frame, waiting up to `timeout_ms` for one; nothing when the
    /// desktop showed nothing new in the time. The frame before is released
    /// first, as the duplication requires.
    pub fn next(&mut self, timeout_ms: u32) -> Result<Option<Frame>, Error> {
        self.release();
        // SAFETY: plain data the call fills.
        let mut info: DXGI_OUTDUPL_FRAME_INFO = unsafe { core::mem::zeroed() };
        let mut resource: *mut IDXGIResource = core::ptr::null_mut();
        // SAFETY: a live duplication; the outputs are live locals.
        let hr = unsafe {
            vcall!(
                self.duplication.as_ptr(),
                AcquireNextFrame,
                timeout_ms,
                &raw mut info,
                &raw mut resource
            )
        }
        .ok_or(Error::Device(dx::Error::MissingSymbol))?;
        if hr == DXGI_ERROR_WAIT_TIMEOUT {
            return Ok(None);
        }
        classify(hr)?;
        self.held = true;
        // SAFETY: a resource whose reference the call handed over.
        let resource = unsafe { Com::from_raw(resource) };
        // SAFETY: the union is the one integer.
        let presented = unsafe { info.LastPresentTime.QuadPart };
        // SAFETY: as above.
        let moved = unsafe { info.LastMouseUpdateTime.QuadPart } != 0;
        let image = match resource {
            Some(resource) if presented != 0 => Some(
                dx::query::<ID3D11Texture2D>(resource.as_ptr().cast(), &IID_ID3D11Texture2D)
                    .map_err(Error::Device)?,
            ),
            _ => None,
        };
        Ok(Some(Frame {
            image,
            presented,
            pointer: moved.then_some(Seen {
                visible: info.PointerPosition.Visible != 0,
                shape_bytes: info.PointerShapeBufferSize,
            }),
        }))
    }

    /// The new shape the frame held now carries, read into `into`; only
    /// while the frame is held.
    pub fn shape(&self, into: &mut Vec<u8>, bytes: u32) -> Result<Shape, Error> {
        let wanted = usize::try_from(bytes).unwrap_or(usize::MAX);
        if into.len() < wanted {
            into.resize(wanted, 0);
        }
        let mut needed = 0u32;
        // SAFETY: plain data the call fills.
        let mut info: DXGI_OUTDUPL_POINTER_SHAPE_INFO = unsafe { core::mem::zeroed() };
        // SAFETY: a live duplication with a frame held; the buffer is live
        // and at least the length passed.
        let hr = unsafe {
            vcall!(
                self.duplication.as_ptr(),
                GetFramePointerShape,
                u32::try_from(into.len()).unwrap_or(u32::MAX),
                into.as_mut_ptr().cast(),
                &raw mut needed,
                &raw mut info
            )
        }
        .ok_or(Error::Device(dx::Error::MissingSymbol))?;
        classify(hr)?;
        let kind = match i32::try_from(info.Type) {
            Ok(DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME) => Kind::Monochrome,
            Ok(DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR) => Kind::Colour,
            Ok(DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR) => Kind::MaskedColour,
            _ => return Err(Error::Status(DXGI_ERROR_UNSUPPORTED)),
        };
        into.truncate(
            usize::try_from(needed)
                .unwrap_or(into.len())
                .min(into.len()),
        );
        Ok(Shape {
            kind,
            width: info.Width,
            height: if kind == Kind::Monochrome {
                info.Height / 2
            } else {
                info.Height
            },
            pitch: info.Pitch,
            hot_x: info.HotSpot.x,
            hot_y: info.HotSpot.y,
        })
    }

    /// Release the frame held, if one is: the system may then write the next
    /// image into the texture it lent. Ordered on the device behind whatever
    /// was queued reading it, since the release is the texture's own lock.
    pub fn release(&mut self) {
        if self.held {
            self.held = false;
            // SAFETY: a live duplication with a frame held.
            unsafe { vcall!(self.duplication.as_ptr(), ReleaseFrame) };
        }
    }
}

impl Drop for Duplication {
    fn drop(&mut self) {
        self.release();
    }
}

/// Where the pointer points and how the system shows it, read on the
/// calling thread's desktop and in its coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    pub x: i32,
    pub y: i32,
    /// Shown: an application has not hidden it, and a mouse is there.
    pub showing: bool,
    /// Held back by the system while a touch or a pen drives.
    pub suppressed: bool,
}

/// The pointer as the system shows it, or nothing where this thread cannot
/// read it, which is a desktop it is not attached to.
pub fn cursor() -> Option<Cursor> {
    let mut info = CURSORINFO {
        cbSize: u32::try_from(size_of::<CURSORINFO>()).unwrap_or(0),
        ..CURSORINFO::default()
    };
    // SAFETY: a live structure whose size is stamped.
    if unsafe { GetCursorInfo(&raw mut info) } == 0 {
        return None;
    }
    Some(Cursor {
        x: info.ptScreenPos.x,
        y: info.ptScreenPos.y,
        showing: info.flags & CURSOR_SHOWING != 0,
        suppressed: info.flags & CURSOR_SUPPRESSED != 0,
    })
}

/// One active output as the system's display configuration describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shown {
    /// The output's name, `DISPLAY1`.
    pub name: String,
    /// How many times a second it presents, as a ratio; zero below the line
    /// where the system will not say.
    pub refresh: (u32, u32),
    /// The monitor's own device path: the same monitor on the same connector
    /// whatever number the output carries, which a driver restart renews.
    /// Empty where the system will not say.
    pub monitor: String,
}

/// How many times a second the output named `name` presents, as a ratio,
/// from the system's own description of its active displays; nothing for a
/// name it does not drive.
pub fn refresh_of(name: &str) -> Option<(u32, u32)> {
    shown()
        .into_iter()
        .find(|output| output.name == name)
        .and_then(|output| (output.refresh.1 != 0).then_some(output.refresh))
}

/// How many times a second the desktop is composed, as a ratio: the
/// compositor's one clock, which every output's pictures are made on.
///
/// **An output's mode can overstate it.** With another display the primary,
/// an output is composed at the primary's rate whatever its own mode says --
/// a virtual display at 144 or 240 was handed 120 pictures a second under a
/// 120 Hz primary, measured -- so the rate an output presents at is the lower
/// of this and its mode. Nothing where the compositor will not say.
pub fn composed_rate() -> Option<(u32, u32)> {
    let mut info = DWM_TIMING_INFO {
        cbSize: u32::try_from(size_of::<DWM_TIMING_INFO>()).unwrap_or(0),
        ..Default::default()
    };
    // SAFETY: a live structure with its size stamped; no window asks for the
    // desktop's own timing, the one form the call takes.
    if unsafe { DwmGetCompositionTimingInfo(core::ptr::null_mut(), &raw mut info) } < 0 {
        return None;
    }
    // Copied out whole: the structure is packed, so its fields are read by
    // value and never borrowed.
    let rate = info.rateRefresh;
    let (numerator, denominator) = (rate.uiNumerator, rate.uiDenominator);
    (numerator != 0 && denominator != 0).then_some((numerator, denominator))
}

/// Whether the system lists a monitor behind the output named `name`.
///
/// **An output can stay on the desktop with nothing behind it.** The last
/// monitor's cable pulled leaves its output active and part of the desktop,
/// composed for nobody, and nothing marks the change; the monitor leaves only
/// the list of monitors under the output, which is what this reads. A virtual
/// display lists its monitor the way a panel does.
pub fn monitored(name: &str) -> bool {
    let source: Vec<u16> = format!(r"\\.\{name}")
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let mut monitor = DISPLAY_DEVICEW {
        cb: u32::try_from(size_of::<DISPLAY_DEVICEW>()).unwrap_or(0),
        ..Default::default()
    };
    // SAFETY: a terminated name and a live structure with its size stamped.
    unsafe { EnumDisplayDevicesW(source.as_ptr(), 0, &raw mut monitor, 0) != 0 }
}

/// A name the system wrote, up to its terminator.
fn wide(text: &[u16]) -> String {
    let end = text.iter().position(|&c| c == 0).unwrap_or(text.len());
    String::from_utf16_lossy(text.get(..end).unwrap_or_default())
}

/// Every active output as the system's display configuration has it: its
/// name, its rate and the monitor it shows.
pub fn shown() -> Vec<Shown> {
    let (mut paths, mut modes) = (0u32, 0u32);
    // SAFETY: the outputs are live locals.
    if unsafe { GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &raw mut paths, &raw mut modes) }
        != 0
    {
        return Vec::new();
    }
    let mut path_list = vec![DISPLAYCONFIG_PATH_INFO::default(); paths as usize];
    let mut mode_list = vec![DISPLAYCONFIG_MODE_INFO::default(); modes as usize];
    // SAFETY: each list is as long as the count passed with it.
    let status = unsafe {
        QueryDisplayConfig(
            QDC_ONLY_ACTIVE_PATHS,
            &raw mut paths,
            path_list.as_mut_ptr(),
            &raw mut modes,
            mode_list.as_mut_ptr(),
            core::ptr::null_mut(),
        )
    };
    if status != 0 {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(paths as usize);
    for path in path_list.iter().take(paths as usize) {
        let mut source = DISPLAYCONFIG_SOURCE_DEVICE_NAME::default();
        source.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME;
        source.header.size =
            u32::try_from(size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>()).unwrap_or(0);
        source.header.adapterId = path.sourceInfo.adapterId;
        source.header.id = path.sourceInfo.id;
        // SAFETY: a live request whose size and kind are stamped.
        if unsafe { DisplayConfigGetDeviceInfo(&raw mut source.header) } != 0 {
            continue;
        }
        let mut target = DISPLAYCONFIG_TARGET_DEVICE_NAME::default();
        target.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME;
        target.header.size =
            u32::try_from(size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>()).unwrap_or(0);
        target.header.adapterId = path.targetInfo.adapterId;
        target.header.id = path.targetInfo.id;
        // SAFETY: as above.
        let monitor = if unsafe { DisplayConfigGetDeviceInfo(&raw mut target.header) } == 0 {
            wide(&target.monitorDevicePath)
        } else {
            String::new()
        };
        let named = wide(&source.viewGdiDeviceName);
        let rate = path.targetInfo.refreshRate;
        out.push(Shown {
            name: named.strip_prefix(r"\\.\").unwrap_or(&named).to_string(),
            refresh: (rate.Numerator, rate.Denominator),
            monitor,
        });
    }
    out
}

/// Whether this process runs in the console's session, which is the one
/// whose desktop a display shows.
pub fn in_console_session() -> bool {
    let mut session = 0u32;
    // SAFETY: this process's own identifier and a live output.
    let known = unsafe { ProcessIdToSessionId(std::process::id(), &raw mut session) } != 0;
    // SAFETY: a query with no arguments.
    known && session == unsafe { WTSGetActiveConsoleSessionId() }
}

/// How long ago, in milliseconds, the performance counter read `then`.
pub fn age_ms(then: i64) -> f64 {
    let (mut now, mut frequency) = (0i64, 0i64);
    // SAFETY: live outputs; the calls cannot fail on a supported system.
    unsafe {
        QueryPerformanceCounter(&raw mut now);
        QueryPerformanceFrequency(&raw mut frequency);
    }
    if frequency <= 0 {
        return 0.0;
    }
    #[allow(
        clippy::cast_precision_loss,
        reason = "a difference of counter ticks, milliseconds apart"
    )]
    let ms = now.saturating_sub(then) as f64 * 1000.0 / frequency as f64;
    ms
}

/// A pointer's shape as straight RGBA, into `into`, and its size: colour as
/// it is; a masked colour's inverting pixels dark grey, nearly opaque, and
/// its transparent ones clear; a monochrome one's black and white as they
/// are and its inverting pixels black inside a light halo. **A picture
/// cannot invert the screen beneath it**, which is what those pixels do, so
/// each is drawn as something that reads on light and dark alike. **A
/// masked colour made only of black, white, clear and inverting pixels is
/// a monochrome shape in another form** -- the system hands a display driver
/// every monochrome pointer so -- and is drawn as one. Nothing for a shape
/// larger than [`POINTER_LIMIT`] or a buffer shorter than it says.
pub fn pointer_rgba(shape: &Shape, raw: &[u8], into: &mut Vec<u8>) -> Option<(u32, u32)> {
    let (width, height) = (shape.width, shape.height);
    if width == 0 || height == 0 || width > POINTER_LIMIT || height > POINTER_LIMIT {
        return None;
    }
    let (w, h) = (width as usize, height as usize);
    into.clear();
    into.resize(w * h * 4, 0);
    if shape.kind == Kind::MaskedColour
        && let Some(masks) = as_monochrome(shape, raw)
    {
        let masked = Shape {
            kind: Kind::Monochrome,
            pitch: u32::try_from(w.div_ceil(8)).ok()?,
            ..*shape
        };
        monochrome(&masked, &masks, into)?;
        return Some((width, height));
    }
    match shape.kind {
        Kind::Colour | Kind::MaskedColour => {
            let pitch = (shape.pitch as usize).max(w * 4);
            for y in 0..h {
                let row = raw.get(y * pitch..y * pitch + w * 4)?;
                for (x, bgra) in row.chunks_exact(4).enumerate() {
                    let (b, g, r, a) = (bgra[0], bgra[1], bgra[2], bgra[3]);
                    let rgba = match shape.kind {
                        Kind::MaskedColour if a == 0xFF && (b | g | r) != 0 => {
                            [0x60, 0x60, 0x60, 0xE0]
                        }
                        Kind::MaskedColour if a == 0xFF => [r, g, b, 0],
                        Kind::MaskedColour => [r, g, b, 0xFF],
                        _ => [r, g, b, a],
                    };
                    let at = (y * w + x) * 4;
                    into.get_mut(at..at + 4)?.copy_from_slice(&rgba);
                }
            }
        }
        Kind::Monochrome => monochrome(shape, raw, into)?,
    }
    Some((width, height))
}

/// The two masks of the monochrome shape a masked colour one is, when it is
/// one: the AND mask's rows, then the XOR mask's, one bit a pixel from the
/// high bit down. Each pixel is black (drawn, black), white (drawn, white),
/// clear (inverting by black, which changes nothing) or inverting (by
/// white); nothing for a shape with any other pixel.
fn as_monochrome(shape: &Shape, raw: &[u8]) -> Option<Vec<u8>> {
    let (w, h) = (shape.width as usize, shape.height as usize);
    let pitch = (shape.pitch as usize).max(w * 4);
    let row_bytes = w.div_ceil(8);
    let mut masks = vec![0u8; row_bytes * h * 2];
    for y in 0..h {
        let row = raw.get(y * pitch..y * pitch + w * 4)?;
        for (x, bgra) in row.chunks_exact(4).enumerate() {
            let colour = (bgra[0], bgra[1], bgra[2]);
            let (and, xor) = match (colour, bgra[3]) {
                ((0, 0, 0), 0x00) => (false, false),
                ((0xFF, 0xFF, 0xFF), 0x00) => (false, true),
                ((0, 0, 0), 0xFF) => (true, false),
                ((0xFF, 0xFF, 0xFF), 0xFF) => (true, true),
                _ => return None,
            };
            let bit = 0x80u8 >> (x % 8);
            if and {
                *masks.get_mut(y * row_bytes + x / 8)? |= bit;
            }
            if xor {
                *masks.get_mut((h + y) * row_bytes + x / 8)? |= bit;
            }
        }
    }
    Some(masks)
}

/// Light around an inverting pixel: strong within one pixel, faint within
/// two, laid only on what is clear or faint.
const HALO_NEAR: [u8; 4] = [0xFF, 0xFF, 0xFF, 0x60];
const HALO_FAR: [u8; 4] = [0xFF, 0xFF, 0xFF, 0x20];

/// The two masks of a monochrome shape into RGBA: the AND mask's rows first,
/// then the XOR mask's, one bit a pixel from the high bit down.
fn monochrome(shape: &Shape, raw: &[u8], into: &mut [u8]) -> Option<()> {
    let (w, h) = (shape.width as usize, shape.height as usize);
    let pitch = (shape.pitch as usize).max(w.div_ceil(8));
    let xor_at = pitch * h;
    let bit = |base: usize, x: usize, y: usize| -> Option<bool> {
        let byte = raw.get(base + y * pitch + x / 8)?;
        Some(byte & (0x80 >> (x % 8)) != 0)
    };
    for y in 0..h {
        for x in 0..w {
            let (and, xor) = (bit(0, x, y)?, bit(xor_at, x, y)?);
            let at = (y * w + x) * 4;
            match (and, xor) {
                (false, false) => into.get_mut(at..at + 4)?.copy_from_slice(&[0, 0, 0, 0xFF]),
                (false, true) => into.get_mut(at..at + 4)?.copy_from_slice(&[0xFF; 4]),
                // Clear, and whatever halo already fell here stays.
                (true, false) => {}
                (true, true) => {
                    into.get_mut(at..at + 4)?.copy_from_slice(&[0, 0, 0, 0xFF]);
                    for dy in -2isize..=2 {
                        for dx in -2isize..=2 {
                            let (Some(nx), Some(ny)) =
                                (x.checked_add_signed(dx), y.checked_add_signed(dy))
                            else {
                                continue;
                            };
                            if nx >= w || ny >= h {
                                continue;
                            }
                            let at = (ny * w + nx) * 4;
                            let pixel = into.get_mut(at..at + 4)?;
                            if pixel == [0; 4] || pixel == HALO_FAR {
                                let halo = if dx.abs() <= 1 && dy.abs() <= 1 {
                                    HALO_NEAR
                                } else {
                                    HALO_FAR
                                };
                                pixel.copy_from_slice(&halo);
                            }
                        }
                    }
                }
            }
        }
    }
    Some(())
}

/// Whether every pixel of a picture is the first one: a pointer drawn as one
/// colour, or not drawn at all, which reads as no pointer.
pub fn one_value(rgba: &[u8]) -> bool {
    let mut pixels = rgba.chunks_exact(4);
    let Some(first) = pixels.next() else {
        return true;
    };
    pixels.all(|pixel| pixel == first)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape(kind: Kind, width: u32, height: u32, pitch: u32) -> Shape {
        Shape {
            kind,
            width,
            height,
            pitch,
            hot_x: 0,
            hot_y: 0,
        }
    }

    fn pixel(rgba: &[u8], width: u32, x: u32, y: u32) -> [u8; 4] {
        let at = ((y * width + x) * 4) as usize;
        rgba[at..at + 4].try_into().expect("four bytes")
    }

    /// **A colour shape is straight BGRA**, so what travels is the same
    /// pixels with red and blue in a picture's order, alpha untouched, and
    /// the rows taken at the shape's own pitch.
    #[test]
    fn a_colour_shape_travels_as_it_is() {
        // Two pixels a row, rows eight bytes apart plus four of padding.
        let raw = [
            1, 2, 3, 4, 5, 6, 7, 8, 0xEE, 0xEE, 0xEE, 0xEE, 9, 10, 11, 12, 13, 14, 15, 16, 0xEE,
            0xEE, 0xEE, 0xEE,
        ];
        let mut out = Vec::new();
        let size = pointer_rgba(&shape(Kind::Colour, 2, 2, 12), &raw, &mut out);
        assert_eq!(size, Some((2, 2)));
        assert_eq!(pixel(&out, 2, 0, 0), [3, 2, 1, 4]);
        assert_eq!(pixel(&out, 2, 1, 0), [7, 6, 5, 8]);
        assert_eq!(pixel(&out, 2, 0, 1), [11, 10, 9, 12]);
        assert_eq!(pixel(&out, 2, 1, 1), [15, 14, 13, 16]);
    }

    /// **A masked colour's alpha is a mask, not a coverage**: clear draws the
    /// colour opaque, set inverts the screen, which no picture can, so it is
    /// grey and nearly opaque, or clear where it would invert by nothing.
    #[test]
    fn a_masked_colour_inverts_as_grey_and_draws_the_rest_opaque() {
        let raw = [
            10, 20, 30, 0x00, // drawn: opaque
            10, 20, 30, 0xFF, // inverting: grey
            0, 0, 0, 0xFF, // inverting by nothing: clear
            0, 0, 0, 0x00, // drawn black: opaque black
        ];
        let mut out = Vec::new();
        pointer_rgba(&shape(Kind::MaskedColour, 4, 1, 16), &raw, &mut out).expect("a picture");
        assert_eq!(pixel(&out, 4, 0, 0), [30, 20, 10, 0xFF]);
        assert_eq!(pixel(&out, 4, 1, 0), [0x60, 0x60, 0x60, 0xE0]);
        assert_eq!(pixel(&out, 4, 2, 0)[3], 0);
        assert_eq!(pixel(&out, 4, 3, 0), [0, 0, 0, 0xFF]);
    }

    /// **A monochrome shape handed over as masked colour is drawn as the
    /// monochrome one it is**: the system converts every monochrome pointer
    /// so for a display driver -- AND 0 to opaque, XOR 0 black and XOR 1
    /// white; AND 1 to inverting, by black (clear) or by white -- and the
    /// text cursor must read the same from both sources, its inverting
    /// pixels black in a light halo rather than grey.
    #[test]
    fn a_monochrome_shape_as_masked_colour_draws_as_monochrome() {
        // The monochrome test's shape below: eight wide, five tall.
        let and = [0b0011_1111u8, 0xFF, 0b1111_1111, 0xFF, 0xFF];
        let xor = [0b0100_0000u8, 0x00, 0b0000_1000, 0x00, 0x00];
        let mut masks = Vec::new();
        masks.extend_from_slice(&and);
        masks.extend_from_slice(&xor);
        let mut monochrome = Vec::new();
        pointer_rgba(&shape(Kind::Monochrome, 8, 5, 1), &masks, &mut monochrome)
            .expect("a picture");
        // The same shape as the system converts it, BGRA a pixel.
        let mut raw = Vec::new();
        for y in 0..5 {
            for x in 0..8 {
                let bit = |mask: &[u8; 5]| mask[y] & (0x80 >> x) != 0;
                raw.extend_from_slice(match (bit(&and), bit(&xor)) {
                    (false, false) => &[0, 0, 0, 0x00],
                    (false, true) => &[0xFF, 0xFF, 0xFF, 0x00],
                    (true, false) => &[0, 0, 0, 0xFF],
                    (true, true) => &[0xFF, 0xFF, 0xFF, 0xFF],
                });
            }
        }
        let mut masked = Vec::new();
        pointer_rgba(&shape(Kind::MaskedColour, 8, 5, 32), &raw, &mut masked).expect("a picture");
        assert_eq!(masked, monochrome);
        assert_eq!(pixel(&masked, 8, 4, 2), [0, 0, 0, 0xFF], "inverting: black");
        assert_eq!(pixel(&masked, 8, 3, 2), HALO_NEAR, "in a light halo");
        // One coloured inverting pixel and it is a masked colour shape again:
        // that pixel grey, and no halo anywhere.
        raw[(2 * 8 + 4) * 4..(2 * 8 + 4) * 4 + 4].copy_from_slice(&[0x40, 0x80, 0xC0, 0xFF]);
        pointer_rgba(&shape(Kind::MaskedColour, 8, 5, 32), &raw, &mut masked).expect("a picture");
        assert_eq!(pixel(&masked, 8, 4, 2), [0x60, 0x60, 0x60, 0xE0]);
        assert_eq!(pixel(&masked, 8, 3, 2)[3], 0, "clear, no halo");
    }

    /// **The two masks of a monochrome shape**: AND clear is drawn, black or
    /// white by XOR; AND set is clear, or inverting where XOR is set too --
    /// drawn black inside a light halo, strong next to it and faint one
    /// further out, never over a pixel that is drawn. The height the system
    /// gives covers both masks.
    #[test]
    fn a_monochrome_shape_draws_its_inverting_pixels_black_in_a_halo() {
        // Eight wide, five tall: one byte a row per mask. Row 0 black then
        // white then clear; row 2 one inverting pixel at x = 4.
        let and = [0b0011_1111, 0xFF, 0b1111_1111, 0xFF, 0xFF];
        let xor = [0b0100_0000, 0x00, 0b0000_1000, 0x00, 0x00];
        let mut raw = Vec::new();
        raw.extend_from_slice(&and);
        raw.extend_from_slice(&xor);
        let mut out = Vec::new();
        let size = pointer_rgba(&shape(Kind::Monochrome, 8, 5, 1), &raw, &mut out);
        assert_eq!(size, Some((8, 5)));
        assert_eq!(
            pixel(&out, 8, 0, 0),
            [0, 0, 0, 0xFF],
            "AND 0 XOR 0 is black"
        );
        assert_eq!(pixel(&out, 8, 1, 0), [0xFF; 4], "AND 0 XOR 1 is white");
        assert_eq!(pixel(&out, 8, 2, 0), HALO_FAR, "clear, two rows above");
        assert_eq!(pixel(&out, 8, 4, 2), [0, 0, 0, 0xFF], "the inverting pixel");
        for (x, y) in [
            (3, 1),
            (4, 1),
            (5, 1),
            (3, 2),
            (5, 2),
            (3, 3),
            (4, 3),
            (5, 3),
        ] {
            assert_eq!(pixel(&out, 8, x, y), HALO_NEAR, "({x},{y}) next to it");
        }
        for (x, y) in [(2, 2), (6, 2), (4, 4), (2, 4), (6, 0)] {
            assert_eq!(pixel(&out, 8, x, y), HALO_FAR, "({x},{y}) two out");
        }
        assert_eq!(pixel(&out, 8, 7, 2)[3], 0, "three out is clear");
        assert_eq!(
            pixel(&out, 8, 1, 0),
            [0xFF; 4],
            "a drawn pixel keeps its colour"
        );
    }

    /// **A blank or one-colour pointer reads as none**, since nothing tells it
    /// from the screen; one differing pixel is a pointer.
    #[test]
    fn a_picture_of_one_value_is_no_pointer() {
        assert!(one_value(&[0; 64]));
        assert!(one_value(&[9, 9, 9, 9, 9, 9, 9, 9]));
        assert!(!one_value(&[0, 0, 0, 0, 0, 0, 0, 1]));
    }

    /// **Every active output names the monitor it shows**, by a path that is
    /// the monitor's and the connector's rather than the output's number --
    /// what follows a display through a renumbering -- and two outputs never
    /// share one. Needs a display, so off by default.
    #[test]
    #[ignore = "requires a display"]
    fn every_output_names_its_monitor() {
        let shown = shown();
        assert!(!shown.is_empty(), "no active output");
        for output in &shown {
            println!("{} {:?} {}", output.name, output.refresh, output.monitor);
            assert!(
                !output.monitor.is_empty(),
                "{} names no monitor",
                output.name
            );
            assert_eq!(
                shown.iter().filter(|o| o.monitor == output.monitor).count(),
                1,
                "{} shares its monitor's path",
                output.name
            );
        }
    }

    /// **The input desktop attaches and says its name**: on a desktop a
    /// person is using, the user's own, `Default`. The lock screen's is
    /// `Winlogon`, which only the system account reaches. Needs an
    /// interactive desktop, so off by default.
    #[test]
    #[ignore = "requires an interactive desktop"]
    fn the_input_desktop_attaches_and_says_its_name() {
        let desktop = InputDesktop::attach().expect("the input desktop");
        assert_eq!(desktop.name(), "Default");
    }

    /// A shape larger than the wire carries, or shorter than it says, is
    /// refused rather than read past.
    #[test]
    fn a_shape_too_large_or_too_short_is_refused() {
        let mut out = Vec::new();
        let big = shape(Kind::Colour, POINTER_LIMIT + 1, 1, (POINTER_LIMIT + 1) * 4);
        assert_eq!(pointer_rgba(&big, &[0; 4096], &mut out), None);
        let short = shape(Kind::Colour, 4, 4, 16);
        assert_eq!(pointer_rgba(&short, &[0; 32], &mut out), None);
        let mono = shape(Kind::Monochrome, 8, 4, 1);
        assert_eq!(pointer_rgba(&mono, &[0; 5], &mut out), None);
    }
}
