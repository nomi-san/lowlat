//! The capture of one window as the system composes it (docs/05-host.md
//! section 2): each picture handed over as a texture on the capture's own
//! device as it arrives, the window's popups and menus in it, the pointer
//! never.
//!
//! **Made by a process of the signed-in user's.** The system serves the
//! capture per signed-in user and refuses it to the system account
//! ([`lowlat_drivers::wgc`]).
//!
//! **A picture is told by an event.** The system calls this side's handler on
//! a thread of its own as each picture arrives; the handler notes when, and
//! sets the event the stream's thread waits on. Pictures are taken on that
//! thread, the newest of those waiting, the older ones given back at once.
//!
//! **A picture's frame is held until the next take**, so the buffer it lies
//! in is not written again while the conversion queued from it may still be
//! reading it.

use core::ffi::c_void;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicI64, AtomicU32, AtomicU64, Ordering, fence};

use lowlat_drivers::d3d11::{Com, Device};
use lowlat_drivers::ffi::d3d11::ID3D11Texture2D;
use lowlat_drivers::ffi::d3d11_guids::IID_ID3D11Texture2D;
use lowlat_drivers::ffi::wgc::{
    __x_ABI_CWindows_CFoundation_CIClosable as Closable,
    __x_ABI_CWindows_CFoundation_CTimeSpan as TimeSpan,
    __x_ABI_CWindows_CGraphics_CCapture_CIDirect3D11CaptureFrame as Frame,
    __x_ABI_CWindows_CGraphics_CCapture_CIDirect3D11CaptureFramePool as FramePool,
    __x_ABI_CWindows_CGraphics_CCapture_CIDirect3D11CaptureFramePoolStatics2 as FramePoolStatics,
    __x_ABI_CWindows_CGraphics_CCapture_CIGraphicsCaptureItem as Item,
    __x_ABI_CWindows_CGraphics_CCapture_CIGraphicsCaptureSession as Session,
    __x_ABI_CWindows_CGraphics_CCapture_CIGraphicsCaptureSession2 as Session2,
    __x_ABI_CWindows_CGraphics_CCapture_CIGraphicsCaptureSession3 as Session3,
    __x_ABI_CWindows_CGraphics_CCapture_CIGraphicsCaptureSession5 as Session5,
    __x_ABI_CWindows_CGraphics_CCapture_CIGraphicsCaptureSession6 as Session6,
    __x_ABI_CWindows_CGraphics_CCapture_CIGraphicsCaptureSessionStatics as SessionStatics,
    __x_ABI_CWindows_CGraphics_CDirectX_CDirect3D11_CIDirect3DDevice as Direct3DDevice,
    __x_ABI_CWindows_CGraphics_CDirectX_CDirect3D11_CIDirect3DSurface as Surface,
    __x_ABI_CWindows_CGraphics_CSizeInt32 as SizeInt32, DirectXPixelFormat_B8G8R8A8UIntNormalized,
    EventRegistrationToken, GUID, HRESULT,
};
use lowlat_drivers::ffi::wgc_guids::{
    IID___FITypedEventHandler_2_Windows__CGraphics__CCapture__CDirect3D11CaptureFramePool_IInspectable as IID_ARRIVED,
    IID___FITypedEventHandler_2_Windows__CGraphics__CCapture__CGraphicsCaptureItem_IInspectable as IID_CLOSED,
    IID___x_ABI_CWindows_CFoundation_CIClosable as IID_CLOSABLE,
    IID___x_ABI_CWindows_CGraphics_CCapture_CIDirect3D11CaptureFramePoolStatics2 as IID_POOL_STATICS,
    IID___x_ABI_CWindows_CGraphics_CCapture_CIGraphicsCaptureItem as IID_ITEM,
    IID___x_ABI_CWindows_CGraphics_CCapture_CIGraphicsCaptureSession2 as IID_SESSION2,
    IID___x_ABI_CWindows_CGraphics_CCapture_CIGraphicsCaptureSession3 as IID_SESSION3,
    IID___x_ABI_CWindows_CGraphics_CCapture_CIGraphicsCaptureSession5 as IID_SESSION5,
    IID___x_ABI_CWindows_CGraphics_CCapture_CIGraphicsCaptureSession6 as IID_SESSION6,
    IID___x_ABI_CWindows_CGraphics_CCapture_CIGraphicsCaptureSessionStatics as IID_SESSION_STATICS,
    IID_IAgileObject, IID_IDirect3DDxgiInterfaceAccess, IID_IGraphicsCaptureItemInterop,
    IID_IUnknown, RuntimeClass_Windows_Graphics_Capture_Direct3D11CaptureFramePool,
    RuntimeClass_Windows_Graphics_Capture_GraphicsCaptureItem,
    RuntimeClass_Windows_Graphics_Capture_GraphicsCaptureSession,
};
use lowlat_drivers::vcall;
use lowlat_drivers::wgc::{
    Apartment, Error, IDirect3DDxgiInterfaceAccess, IGraphicsCaptureItemInterop, Wgc, check, query,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, DUPLICATE_SAME_ACCESS, DuplicateHandle, HANDLE, HWND, LPARAM, RECT, WAIT_OBJECT_0,
};
use windows_sys::Win32::Graphics::Dwm::{
    DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute,
};
use windows_sys::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFOEXW, MonitorFromWindow,
};
use windows_sys::Win32::System::Performance::QueryPerformanceCounter;
use windows_sys::Win32::System::Threading::{
    CreateEventW, GetCurrentProcess, SetEvent, WaitForSingleObject,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GA_ROOTOWNER, GW_OWNER, GWL_EXSTYLE, GetAncestor, GetForegroundWindow, GetWindow,
    GetWindowLongPtrW, IsIconic, IsWindow, IsWindowVisible, WS_EX_TOOLWINDOW,
};

/// The frame pool's buffers: the one a picture is held in until the next
/// take, and one for the system to write the next into.
const BUFFERS: i32 = 2;

/// The shortest interval between pictures the session is asked for, in the
/// runtime's 100 ns units: a millisecond. **Its default, 16 ms, halves a
/// 120 Hz source** (measured 2026-11-19).
const INTERVAL: i64 = 10_000;

const S_OK: HRESULT = 0;
const E_NOINTERFACE: HRESULT = 0x8000_4002_u32 as HRESULT;
const E_POINTER: HRESULT = 0x8000_4003_u32 as HRESULT;

type Result<T> = core::result::Result<T, Error>;

/// Whether the system has the capture at all: a process of the system
/// account is refused it, as an older system lacks it.
pub fn supported(wgc: &Wgc) -> Result<bool> {
    let _apartment = wgc.apartment()?;
    let statics: Com<SessionStatics> = wgc.factory(
        RuntimeClass_Windows_Graphics_Capture_GraphicsCaptureSession,
        &IID_SESSION_STATICS,
    )?;
    let mut answer = 0u8;
    // SAFETY: a live factory; the output is a live local.
    check(
        unsafe { vcall!(statics.as_ptr(), IsSupported, &raw mut answer) }
            .ok_or(Error::MissingSymbol)?,
    )?;
    Ok(answer != 0)
}

/// The window in front, as the window it belongs to: **its root owner**, so
/// a dialog of an application's brings its main window, which a capture
/// takes the dialog into as one of its own. Nothing while nothing is in
/// front, as off the desktop that takes input.
pub fn foreground() -> Option<HWND> {
    // SAFETY: no arguments; a window or null.
    let window = unsafe { GetForegroundWindow() };
    if window.is_null() {
        return None;
    }
    // SAFETY: a window handle; its root owner, or null for a window gone.
    let owner = unsafe { GetAncestor(window, GA_ROOTOWNER) };
    Some(if owner.is_null() { window } else { owner })
}

/// Whether a window has a picture to take now: it is there, shown, not
/// minimised, and not cloaked -- on another virtual desktop, or kept off the
/// screen by its program.
#[allow(
    clippy::not_unsafe_ptr_arg_deref,
    reason = "a window handle is a value the system checks at each call, never memory read here"
)]
pub fn on_screen(window: HWND) -> bool {
    // SAFETY: a window handle, which the calls check.
    let shown =
        unsafe { IsWindow(window) != 0 && IsWindowVisible(window) != 0 && IsIconic(window) == 0 };
    let mut cloaked = 0u32;
    // SAFETY: a window handle and an output of the attribute's size.
    let read = unsafe {
        DwmGetWindowAttribute(window, DWMWA_CLOAKED as u32, (&raw mut cloaked).cast(), 4)
    };
    shown && (read < 0 || cloaked == 0)
}

/// Where a window's picture lies on the desktop, in its pixels: **its
/// frame's bounds**, the part a capture of it holds -- a window's rectangle
/// takes in the invisible border the system resizes it by. The calling
/// thread's awareness sets the units: per-monitor aware, the pixels are the
/// displays' own.
#[allow(
    clippy::not_unsafe_ptr_arg_deref,
    reason = "a window handle is a value the system checks at each call, never memory read here"
)]
pub fn frame(window: HWND) -> Option<(i32, i32, u32, u32)> {
    let mut bounds = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    // SAFETY: a window handle and an output of the attribute's size.
    let read = unsafe {
        DwmGetWindowAttribute(
            window,
            DWMWA_EXTENDED_FRAME_BOUNDS as u32,
            (&raw mut bounds).cast(),
            u32::try_from(core::mem::size_of::<RECT>()).ok()?,
        )
    };
    let width = u32::try_from(bounds.right.checked_sub(bounds.left)?).ok()?;
    let height = u32::try_from(bounds.bottom.checked_sub(bounds.top)?).ok()?;
    (read >= 0).then_some((bounds.left, bounds.top, width, height))
}

/// The output a window is on, by the name the output goes by without its
/// `\\.\`: the monitor it overlaps the most, the nearest when it overlaps
/// none.
#[allow(
    clippy::not_unsafe_ptr_arg_deref,
    reason = "a window handle is a value the system checks at each call, never memory read here"
)]
pub fn output_of(window: HWND) -> Option<String> {
    // SAFETY: a window handle; a monitor, the nearest whatever it is.
    let monitor = unsafe { MonitorFromWindow(window, MONITOR_DEFAULTTONEAREST) };
    // SAFETY: plain data the call fills, its size set first.
    let mut info: MONITORINFOEXW = unsafe { core::mem::zeroed() };
    info.monitorInfo.cbSize = u32::try_from(core::mem::size_of::<MONITORINFOEXW>()).ok()?;
    // SAFETY: a monitor handle and an output of the size it says.
    if unsafe { GetMonitorInfoW(monitor, (&raw mut info).cast()) } == 0 {
        return None;
    }
    let end = info
        .szDevice
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(info.szDevice.len());
    let name = String::from_utf16_lossy(info.szDevice.get(..end)?);
    Some(name.strip_prefix(r"\\.\").unwrap_or(&name).to_string())
}

/// The topmost window on the screen that `holds` takes, in the order the
/// system stacks them, of those a picture can start on: **windows of their
/// own** -- no owner and not a tool window -- since a popup, a tooltip or a
/// dialog comes in its owner's capture.
pub fn topmost(holds: &mut dyn FnMut(HWND) -> bool) -> Option<HWND> {
    struct Walk<'a> {
        holds: &'a mut dyn FnMut(HWND) -> bool,
        found: HWND,
    }
    unsafe extern "system" fn each(window: HWND, data: LPARAM) -> i32 {
        // SAFETY: the walk below, alive for the enumeration that calls this.
        let walk = unsafe { &mut *(data as *mut Walk<'_>) };
        // SAFETY: a window handle the enumeration gives.
        let own = unsafe {
            GetWindow(window, GW_OWNER).is_null()
                && GetWindowLongPtrW(window, GWL_EXSTYLE) & WS_EX_TOOLWINDOW as isize == 0
        };
        if own && on_screen(window) && (walk.holds)(window) {
            walk.found = window;
            return 0;
        }
        1
    }
    let mut walk = Walk {
        holds,
        found: core::ptr::null_mut(),
    };
    // SAFETY: a callback that reads the walk only while the enumeration
    // runs; its answer is the stop.
    unsafe { EnumWindows(Some(each), (&raw mut walk) as LPARAM) };
    (!walk.found.is_null()).then_some(walk.found)
}

/// What a session was asked to do that the system it runs on has no way to:
/// each is a later interface of the session's, absent before its build.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Lacking {
    /// The pointer stays out of the picture: absent, it is drawn in.
    pub cursor: bool,
    /// No border drawn on the screen around the window.
    pub border: bool,
    /// The millisecond interval: absent, pictures come at most every 16 ms.
    pub interval: bool,
    /// The window's popups and menus in the picture.
    pub secondary: bool,
}

/// One of the two handlers this side gives the system -- a picture arrived,
/// or the window's capture closed -- as an object of the system's interface
/// kind: a table, a count, and what its calls leave.
#[repr(C)]
struct Handler {
    table: *const HandlerTable,
    refs: AtomicU32,
    /// The handler interface it is, which it answers to.
    iid: GUID,
    /// The handler's own handle to the event its calls set.
    event: HANDLE,
    calls: AtomicU64,
    /// When the last call came, on the performance counter.
    last: AtomicI64,
}

/// Both handler interfaces' table: the base three, then the call, whose two
/// arguments -- the sender and nothing -- go unread.
#[repr(C)]
#[allow(non_snake_case)]
struct HandlerTable {
    QueryInterface:
        unsafe extern "system" fn(*mut Handler, *const GUID, *mut *mut c_void) -> HRESULT,
    AddRef: unsafe extern "system" fn(*mut Handler) -> u32,
    Release: unsafe extern "system" fn(*mut Handler) -> u32,
    Invoke: unsafe extern "system" fn(*mut Handler, *mut c_void, *mut c_void) -> HRESULT,
}

static HANDLER_TABLE: HandlerTable = HandlerTable {
    QueryInterface: handler_query,
    AddRef: handler_add_ref,
    Release: handler_release,
    Invoke: handler_invoke,
};

fn same(a: &GUID, b: &GUID) -> bool {
    a.Data1 == b.Data1 && a.Data2 == b.Data2 && a.Data3 == b.Data3 && a.Data4 == b.Data4
}

/// **Agile**: the system calls a free-threaded pool's handler from a thread
/// of its own, and asks first whether it may.
unsafe extern "system" fn handler_query(
    this: *mut Handler,
    iid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    if out.is_null() || iid.is_null() {
        return E_POINTER;
    }
    // SAFETY: the system's live object and identifier, as the interface
    // contract has them.
    let (me, asked) = unsafe { (&*this, &*iid) };
    if same(asked, &IID_IUnknown) || same(asked, &IID_IAgileObject) || same(asked, &me.iid) {
        me.refs.fetch_add(1, Ordering::Relaxed);
        // SAFETY: the caller's output.
        unsafe { *out = this.cast() };
        S_OK
    } else {
        // SAFETY: the caller's output.
        unsafe { *out = core::ptr::null_mut() };
        E_NOINTERFACE
    }
}

unsafe extern "system" fn handler_add_ref(this: *mut Handler) -> u32 {
    // SAFETY: a live object, as the interface contract has it. A count is
    // taken only from a held reference, so Relaxed.
    unsafe { &*this }.refs.fetch_add(1, Ordering::Relaxed) + 1
}

unsafe extern "system" fn handler_release(this: *mut Handler) -> u32 {
    // SAFETY: a live object, as the interface contract has it.
    let left = unsafe { &*this }.refs.fetch_sub(1, Ordering::Release) - 1;
    if left == 0 {
        // Every release before the last happens before the object goes.
        fence(Ordering::Acquire);
        // SAFETY: the last reference: made by `Box::into_raw` and reached by
        // nothing else now.
        let me = unsafe { Box::from_raw(this) };
        // SAFETY: the handler's own handle, closed once.
        unsafe { CloseHandle(me.event) };
    }
    left
}

unsafe extern "system" fn handler_invoke(
    this: *mut Handler,
    _sender: *mut c_void,
    _args: *mut c_void,
) -> HRESULT {
    // SAFETY: a live object, as the interface contract has it.
    let me = unsafe { &*this };
    let mut now = 0i64;
    // SAFETY: a live output.
    unsafe { QueryPerformanceCounter(&raw mut now) };
    me.last.store(now, Ordering::Relaxed);
    // Release: a reader that sees the count sees the time stored before it.
    me.calls.fetch_add(1, Ordering::Release);
    // SAFETY: the handler's own handle.
    unsafe { SetEvent(me.event) };
    S_OK
}

/// This side's reference to a handler, given back on drop.
struct HandlerRef(NonNull<Handler>);

impl HandlerRef {
    /// A handler answering to `iid`, setting `event` -- a handle of its own
    /// to it is taken -- at each call.
    fn new(iid: GUID, event: HANDLE) -> Result<Self> {
        let mut own: HANDLE = core::ptr::null_mut();
        // SAFETY: this process's handle, duplicated within it; the output is
        // a live local.
        let duplicated = unsafe {
            DuplicateHandle(
                GetCurrentProcess(),
                event,
                GetCurrentProcess(),
                &raw mut own,
                0,
                0,
                DUPLICATE_SAME_ACCESS,
            )
        };
        if duplicated == 0 {
            return Err(Error::Status(last_error()));
        }
        let handler = Box::new(Handler {
            table: &raw const HANDLER_TABLE,
            refs: AtomicU32::new(1),
            iid,
            event: own,
            calls: AtomicU64::new(0),
            last: AtomicI64::new(0),
        });
        // SAFETY: `Box::into_raw` never gives null.
        Ok(Self(unsafe {
            NonNull::new_unchecked(Box::into_raw(handler))
        }))
    }

    fn as_ptr(&self) -> *mut Handler {
        self.0.as_ptr()
    }

    fn get(&self) -> &Handler {
        // SAFETY: a live object this reference keeps.
        unsafe { self.0.as_ref() }
    }
}

impl Drop for HandlerRef {
    fn drop(&mut self) {
        // SAFETY: this side's own reference, given back once.
        unsafe { handler_release(self.0.as_ptr()) };
    }
}

fn last_error() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// Close a runtime object that can be: what the system holds for it goes at
/// once rather than at the last reference.
fn close(object: *mut c_void) {
    // SAFETY: the caller's live object.
    if let Ok(closable) = unsafe { query::<Closable>(object, &IID_CLOSABLE) } {
        // SAFETY: a live object.
        unsafe { vcall!(closable.as_ptr(), Close) };
    }
}

/// The picture a take found: the texture of the pool's buffer it lies in, on
/// the capture's device, and its size there -- the window's own, at the
/// buffer's top left; the buffer can be larger.
#[derive(Debug)]
pub struct Picture {
    pub texture: Com<ID3D11Texture2D>,
    pub width: u32,
    pub height: u32,
    /// When the system said it arrived, on the performance counter.
    pub arrived: i64,
}

/// The capture of one window, made and read on one thread.
pub struct Window {
    session: Com<Session>,
    pool: Com<FramePool>,
    item: Com<Item>,
    device: Com<Direct3DDevice>,
    arrived: HandlerRef,
    arrived_token: EventRegistrationToken,
    closed: HandlerRef,
    closed_token: EventRegistrationToken,
    /// The event the arrival handler sets, this side's handle to it.
    event: HANDLE,
    /// The pool's buffers' size.
    size: SizeInt32,
    /// The frame whose picture the last take handed out.
    held: Option<Com<Frame>>,
    lacking: Lacking,
    // Last, so the apartment is left after everything made in it has gone.
    _apartment: Apartment,
}

impl core::fmt::Debug for Window {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Window")
            .field("size", &(self.size.Width, self.size.Height))
            .field("held", &self.held.is_some())
            .field("lacking", &self.lacking)
            .finish_non_exhaustive()
    }
}

impl Window {
    /// Capture `window` on `device`: its pictures B8G8R8A8 textures of
    /// `device`'s own, the pointer left out of them, no border drawn around
    /// it on the screen, a picture a millisecond at most, its popups and
    /// menus in; what the system cannot do of these is in
    /// [`Window::lacking`].
    pub fn new(wgc: &Wgc, device: &Device, window: *mut c_void) -> Result<Self> {
        let apartment = wgc.apartment()?;
        let interop: Com<IGraphicsCaptureItemInterop> = wgc.factory(
            RuntimeClass_Windows_Graphics_Capture_GraphicsCaptureItem,
            &IID_IGraphicsCaptureItemInterop,
        )?;
        let mut raw: *mut c_void = core::ptr::null_mut();
        // SAFETY: a live factory; the window is the caller's and the system
        // checks it; the identifier and the output are live.
        check(
            unsafe {
                vcall!(
                    interop.as_ptr(),
                    CreateForWindow,
                    window.cast(),
                    &IID_ITEM,
                    &raw mut raw
                )
            }
            .ok_or(Error::MissingSymbol)?,
        )?;
        // SAFETY: an item whose reference the call handed over.
        let item: Com<Item> = unsafe { Com::from_raw(raw.cast()) }.ok_or(Error::MissingSymbol)?;
        let mut size = SizeInt32 {
            Width: 0,
            Height: 0,
        };
        // SAFETY: a live item; the output is a live local.
        check(
            unsafe { vcall!(item.as_ptr(), get_Size, &raw mut size) }
                .ok_or(Error::MissingSymbol)?,
        )?;
        let runtime_device = wgc.device(device)?;
        let statics: Com<FramePoolStatics> = wgc.factory(
            RuntimeClass_Windows_Graphics_Capture_Direct3D11CaptureFramePool,
            &IID_POOL_STATICS,
        )?;
        let mut raw: *mut FramePool = core::ptr::null_mut();
        // SAFETY: a live factory and device; the output is a live local.
        check(
            unsafe {
                vcall!(
                    statics.as_ptr(),
                    CreateFreeThreaded,
                    runtime_device.as_ptr(),
                    DirectXPixelFormat_B8G8R8A8UIntNormalized,
                    BUFFERS,
                    size,
                    &raw mut raw
                )
            }
            .ok_or(Error::MissingSymbol)?,
        )?;
        // SAFETY: a pool whose reference the call handed over.
        let pool = unsafe { Com::from_raw(raw) }.ok_or(Error::MissingSymbol)?;
        let mut raw: *mut Session = core::ptr::null_mut();
        // SAFETY: a live pool and item; the output is a live local.
        check(
            unsafe {
                vcall!(
                    pool.as_ptr(),
                    CreateCaptureSession,
                    item.as_ptr(),
                    &raw mut raw
                )
            }
            .ok_or(Error::MissingSymbol)?,
        )?;
        // SAFETY: a session whose reference the call handed over.
        let session = unsafe { Com::from_raw(raw) }.ok_or(Error::MissingSymbol)?;
        let lacking = configure(&session);

        // SAFETY: an unnamed auto-reset event, unsignalled; checked.
        let event = unsafe { CreateEventW(core::ptr::null(), 0, 0, core::ptr::null()) };
        if event.is_null() {
            return Err(Error::Status(last_error()));
        }
        Self::started(
            item,
            pool,
            session,
            runtime_device,
            event,
            size,
            lacking,
            apartment,
        )
    }

    /// The handlers given to the system and the capture started. **`event`
    /// is the window's once the window exists**, closed by its drop from
    /// there, failure or not; before it, here.
    #[allow(
        clippy::too_many_arguments,
        reason = "the parts `new` made, put together once"
    )]
    fn started(
        item: Com<Item>,
        pool: Com<FramePool>,
        session: Com<Session>,
        device: Com<Direct3DDevice>,
        event: HANDLE,
        size: SizeInt32,
        lacking: Lacking,
        apartment: Apartment,
    ) -> Result<Self> {
        let handlers = HandlerRef::new(IID_ARRIVED, event)
            .and_then(|arrived| HandlerRef::new(IID_CLOSED, event).map(|closed| (arrived, closed)));
        let (arrived, closed) = match handlers {
            Ok(both) => both,
            Err(error) => {
                // SAFETY: the caller's event, which nothing else keeps yet.
                unsafe { CloseHandle(event) };
                return Err(error);
            }
        };
        let mut window = Self {
            session,
            pool,
            item,
            device,
            arrived,
            arrived_token: EventRegistrationToken { value: 0 },
            closed,
            closed_token: EventRegistrationToken { value: 0 },
            event,
            size,
            held: None,
            lacking,
            _apartment: apartment,
        };
        // SAFETY: a live pool and handler of the pool's handler interface;
        // the output is the window's own.
        check(
            unsafe {
                vcall!(
                    window.pool.as_ptr(),
                    add_FrameArrived,
                    window.arrived.as_ptr().cast(),
                    &raw mut window.arrived_token
                )
            }
            .ok_or(Error::MissingSymbol)?,
        )?;
        // SAFETY: as above, for the item's.
        check(
            unsafe {
                vcall!(
                    window.item.as_ptr(),
                    add_Closed,
                    window.closed.as_ptr().cast(),
                    &raw mut window.closed_token
                )
            }
            .ok_or(Error::MissingSymbol)?,
        )?;
        // SAFETY: a live session, fully set.
        check(
            unsafe { vcall!(window.session.as_ptr(), StartCapture) }.ok_or(Error::MissingSymbol)?,
        )?;
        Ok(window)
    }

    /// What the session was asked for and the system has no way to do.
    pub fn lacking(&self) -> Lacking {
        self.lacking
    }

    /// Wait up to `timeout_ms` for a picture to arrive, or the window's
    /// capture to close: true if either did.
    pub fn wait(&self, timeout_ms: u32) -> bool {
        // SAFETY: this window's handle to the event.
        unsafe { WaitForSingleObject(self.event, timeout_ms) == WAIT_OBJECT_0 }
    }

    /// The system's handle the window's pictures are told by, for a caller
    /// waiting on it beside others. Set once for every picture or more.
    pub fn event(&self) -> HANDLE {
        self.event
    }

    /// Whether the window's capture has closed -- the window gone -- after
    /// which no picture comes.
    pub fn closed(&self) -> bool {
        self.closed.get().calls.load(Ordering::Acquire) != 0
    }

    /// How many pictures the system has said arrived.
    pub fn arrivals(&self) -> u64 {
        self.arrived.get().calls.load(Ordering::Acquire)
    }

    /// The newest picture waiting, every older one given back, and the frame
    /// of the one taken before given back too; nothing when none waits.
    pub fn take(&mut self) -> Result<Option<Picture>> {
        let mut newest: Option<Com<Frame>> = None;
        loop {
            let mut raw: *mut Frame = core::ptr::null_mut();
            // SAFETY: a live pool; the output is a live local.
            check(
                unsafe { vcall!(self.pool.as_ptr(), TryGetNextFrame, &raw mut raw) }
                    .ok_or(Error::MissingSymbol)?,
            )?;
            // SAFETY: a frame whose reference the call handed over, or none.
            let Some(frame) = (unsafe { Com::from_raw(raw) }) else {
                break;
            };
            if let Some(older) = newest.replace(frame) {
                close(older.as_ptr().cast());
            }
        }
        let Some(frame) = newest else {
            return Ok(None);
        };
        let arrived = self.arrived.get().last.load(Ordering::Relaxed);
        let mut content = SizeInt32 {
            Width: 0,
            Height: 0,
        };
        // SAFETY: a live frame; the output is a live local.
        check(
            unsafe { vcall!(frame.as_ptr(), get_ContentSize, &raw mut content) }
                .ok_or(Error::MissingSymbol)?,
        )?;
        let texture = Self::texture_of(&frame)?;
        if let Some(before) = self.held.replace(frame) {
            close(before.as_ptr().cast());
        }
        // **The pool follows the window's size**, made again at the new one
        // for the pictures after this; this one lies in a buffer of the old.
        if (content.Width, content.Height) != (self.size.Width, self.size.Height)
            && content.Width > 0
            && content.Height > 0
        {
            // SAFETY: a live pool and device.
            check(
                unsafe {
                    vcall!(
                        self.pool.as_ptr(),
                        Recreate,
                        self.device.as_ptr(),
                        DirectXPixelFormat_B8G8R8A8UIntNormalized,
                        BUFFERS,
                        content
                    )
                }
                .ok_or(Error::MissingSymbol)?,
            )?;
            self.size = content;
        }
        let (Ok(width), Ok(height)) = (u32::try_from(content.Width), u32::try_from(content.Height))
        else {
            return Err(Error::Status(-1));
        };
        Ok(Some(Picture {
            texture,
            width,
            height,
            arrived,
        }))
    }

    /// The texture a frame's surface lies in.
    fn texture_of(frame: &Com<Frame>) -> Result<Com<ID3D11Texture2D>> {
        let mut raw: *mut Surface = core::ptr::null_mut();
        // SAFETY: a live frame; the output is a live local.
        check(
            unsafe { vcall!(frame.as_ptr(), get_Surface, &raw mut raw) }
                .ok_or(Error::MissingSymbol)?,
        )?;
        // SAFETY: a surface whose reference the call handed over.
        let surface = unsafe { Com::from_raw(raw) }.ok_or(Error::MissingSymbol)?;
        // SAFETY: a live surface.
        let access: Com<IDirect3DDxgiInterfaceAccess> =
            unsafe { query(surface.as_ptr().cast(), &IID_IDirect3DDxgiInterfaceAccess) }?;
        let mut texture: *mut c_void = core::ptr::null_mut();
        // SAFETY: a live surface's access; the identifier and the output are
        // live.
        check(
            unsafe {
                vcall!(
                    access.as_ptr(),
                    GetInterface,
                    &IID_ID3D11Texture2D,
                    &raw mut texture
                )
            }
            .ok_or(Error::MissingSymbol)?,
        )?;
        // SAFETY: a texture whose reference the call handed over.
        unsafe { Com::from_raw(texture.cast()) }.ok_or(Error::MissingSymbol)
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        // SAFETY: the pool and item the handlers were given to, with the
        // tokens they gave back; a token still zero was never given.
        unsafe {
            if self.arrived_token.value != 0 {
                vcall!(self.pool.as_ptr(), remove_FrameArrived, self.arrived_token);
            }
            if self.closed_token.value != 0 {
                vcall!(self.item.as_ptr(), remove_Closed, self.closed_token);
            }
        }
        if let Some(held) = self.held.take() {
            close(held.as_ptr().cast());
        }
        close(self.session.as_ptr().cast());
        close(self.pool.as_ptr().cast());
        // SAFETY: this window's handle, closed once; each handler keeps its
        // own until its last reference goes.
        unsafe { CloseHandle(self.event) };
    }
}

/// Ask the session for what it is set to do; what its system has no
/// interface for is returned.
fn configure(session: &Com<Session>) -> Lacking {
    let base = session.as_ptr().cast::<c_void>();
    let mut lacking = Lacking::default();
    // SAFETY: a live session, asked for its later interfaces; each call is
    // that interface's own setter.
    unsafe {
        lacking.cursor = query::<Session2>(base, &IID_SESSION2)
            .ok()
            .and_then(|it| vcall!(it.as_ptr(), put_IsCursorCaptureEnabled, 0))
            .is_none_or(|hr| hr < 0);
        lacking.border = query::<Session3>(base, &IID_SESSION3)
            .ok()
            .and_then(|it| vcall!(it.as_ptr(), put_IsBorderRequired, 0))
            .is_none_or(|hr| hr < 0);
        lacking.interval = query::<Session5>(base, &IID_SESSION5)
            .ok()
            .and_then(|it| {
                vcall!(
                    it.as_ptr(),
                    put_MinUpdateInterval,
                    TimeSpan { Duration: INTERVAL }
                )
            })
            .is_none_or(|hr| hr < 0);
        lacking.secondary = query::<Session6>(base, &IID_SESSION6)
            .ok()
            .and_then(|it| vcall!(it.as_ptr(), put_IncludeSecondaryWindows, 1))
            .is_none_or(|hr| hr < 0);
    }
    lacking
}

#[cfg(test)]
mod tests {
    use super::*;
    use lowlat_drivers::d3d11::D3d11;
    use lowlat_drivers::ffi::d3d11::{
        D3D11_BOX, D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE,
        D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC,
        ID3D11Resource,
    };
    use windows_sys::Win32::Graphics::Gdi::CreateSolidBrush;
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, MSG, PM_REMOVE,
        PeekMessageW, RegisterClassW, TranslateMessage, WNDCLASSW, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
    };

    fn query_handler(handler: &HandlerRef, iid: &GUID) -> (HRESULT, *mut c_void) {
        let mut out: *mut c_void = core::ptr::dangling_mut();
        // SAFETY: a live handler; the identifier and the output are live.
        let hr = unsafe { handler_query(handler.as_ptr(), iid, &raw mut out) };
        (hr, out)
    }

    /// **A handler answers to the base interface, to being agile and to its
    /// own interface, and to nothing else**, counts its references, and each
    /// call counts, notes its time and sets its event.
    #[test]
    fn a_handler_answers_its_interfaces_and_tells_each_call() {
        // SAFETY: an unnamed auto-reset event; checked.
        let event = unsafe { CreateEventW(core::ptr::null(), 0, 0, core::ptr::null()) };
        assert!(!event.is_null(), "an event");
        let handler = HandlerRef::new(IID_ARRIVED, event).expect("a handler");
        for iid in [IID_IUnknown, IID_IAgileObject, IID_ARRIVED] {
            let (hr, out) = query_handler(&handler, &iid);
            assert_eq!(hr, S_OK, "an interface it is was refused");
            assert_eq!(out, handler.as_ptr().cast(), "another object answered");
        }
        assert_eq!(handler.get().refs.load(Ordering::Relaxed), 4);
        let (hr, out) = query_handler(&handler, &IID_CLOSED);
        assert_eq!(hr, E_NOINTERFACE, "the other handler's interface was taken");
        assert!(out.is_null(), "a refusal left an object");
        for _ in 0..3 {
            // SAFETY: a reference the queries above took.
            unsafe { handler_release(handler.as_ptr()) };
        }
        assert_eq!(handler.get().refs.load(Ordering::Relaxed), 1);

        // SAFETY: the event, not yet set.
        assert_ne!(unsafe { WaitForSingleObject(event, 0) }, WAIT_OBJECT_0);
        // SAFETY: a live handler; the arguments go unread.
        let hr = unsafe {
            handler_invoke(
                handler.as_ptr(),
                core::ptr::null_mut(),
                core::ptr::null_mut(),
            )
        };
        assert_eq!(hr, S_OK);
        assert_eq!(handler.get().calls.load(Ordering::Acquire), 1);
        assert!(
            handler.get().last.load(Ordering::Relaxed) > 0,
            "no time noted"
        );
        // SAFETY: the event, set by the call through the handler's own
        // handle to it.
        assert_eq!(unsafe { WaitForSingleObject(event, 0) }, WAIT_OBJECT_0);
        drop(handler);
        // SAFETY: this test's own handle, closed once.
        unsafe { CloseHandle(event) };
    }

    unsafe extern "system" fn plain(
        window: *mut c_void,
        message: u32,
        w: usize,
        l: isize,
    ) -> isize {
        // SAFETY: the system's own handling of the system's message.
        unsafe { DefWindowProcW(window, message, w, l) }
    }

    fn pump() {
        // SAFETY: plain data the call fills.
        let mut message: MSG = unsafe { core::mem::zeroed() };
        // SAFETY: this thread's queue; the message is a live local.
        while unsafe { PeekMessageW(&raw mut message, core::ptr::null_mut(), 0, 0, PM_REMOVE) } != 0
        {
            // SAFETY: a message just taken off this thread's queue.
            unsafe {
                TranslateMessage(&raw const message);
                DispatchMessageW(&raw const message);
            }
        }
    }

    /// The pixel at (x, y) of `texture`, BGRA.
    fn pixel(device: &Device, texture: &Com<ID3D11Texture2D>, x: u32, y: u32) -> [u8; 4] {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: 1,
            Height: 1,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ as u32,
            MiscFlags: 0,
        };
        let staging = device.texture(&desc).expect("a staging texture");
        let region = D3D11_BOX {
            left: x,
            top: y,
            front: 0,
            right: x + 1,
            bottom: y + 1,
            back: 1,
        };
        let context = device.context();
        // SAFETY: plain data the call fills.
        let mut mapped: D3D11_MAPPED_SUBRESOURCE = unsafe { core::mem::zeroed() };
        // SAFETY: the device's context on this thread; both textures live and
        // of one format; the mapping is read and unmapped before the staging
        // texture goes.
        unsafe {
            vcall!(
                context,
                CopySubresourceRegion,
                staging.as_ptr().cast::<ID3D11Resource>(),
                0,
                0,
                0,
                0,
                texture.as_ptr().cast::<ID3D11Resource>(),
                0,
                &raw const region
            );
            let hr = vcall!(
                context,
                Map,
                staging.as_ptr().cast::<ID3D11Resource>(),
                0,
                D3D11_MAP_READ,
                0,
                &raw mut mapped
            )
            .expect("the entry");
            assert!(hr >= 0, "map: 0x{hr:08x}");
            let bytes = *mapped.pData.cast::<[u8; 4]>();
            vcall!(context, Unmap, staging.as_ptr().cast::<ID3D11Resource>(), 0);
            bytes
        }
    }

    /// **A window of this test's own is captured as it is drawn**: its
    /// pictures arrive, come at its size and carry its colour, and the
    /// capture closes as the window goes. Needs a desktop and a GPU, and a
    /// process of the signed-in user's, so off by default: `cargo test -p
    /// lowlat-capture window -- --ignored --nocapture`.
    #[test]
    #[ignore = "requires a desktop and a GPU"]
    fn a_window_is_captured_as_it_is_drawn() {
        let d3d11 = D3d11::load().expect("the system's libraries");
        let adapter = d3d11
            .adapters()
            .expect("the walk")
            .into_iter()
            .find(lowlat_drivers::d3d11::Adapter::decodes_here)
            .expect("an adapter that renders");
        let device = d3d11.open(adapter.luid).expect("a device");
        let wgc = Wgc::load().expect("the capture's libraries");
        assert!(supported(&wgc).expect("the question"), "no capture here");

        let name: Vec<u16> = "lowlat-capture-test\0".encode_utf16().collect();
        // SAFETY: this process's module, and a class of its own with the
        // system's handling and a solid brush; checked below.
        let window = unsafe {
            let module = GetModuleHandleW(core::ptr::null());
            let class = WNDCLASSW {
                lpfnWndProc: Some(plain),
                hInstance: module,
                // Red 30, green 160, blue 90.
                hbrBackground: CreateSolidBrush(0x005A_A01E),
                lpszClassName: name.as_ptr(),
                // SAFETY: the rest of the class is plain data, zero by default.
                ..core::mem::zeroed()
            };
            RegisterClassW(&raw const class);
            CreateWindowExW(
                0,
                name.as_ptr(),
                name.as_ptr(),
                WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                100,
                100,
                400,
                300,
                core::ptr::null_mut(),
                core::ptr::null_mut(),
                module,
                core::ptr::null(),
            )
        };
        assert!(!window.is_null(), "the test's window");
        pump();

        let mut capture = Window::new(&wgc, &device, window).expect("a capture of the window");
        assert_eq!(capture.lacking(), Lacking::default(), "a setting refused");
        // **Taken until the window's colour shows**: the first pictures can
        // come before the new window has painted.
        let began = lowlat_common::clock::Time::now();
        let (mut pictures, mut seen) = (0u32, (0, 0, 0));
        loop {
            pump();
            if capture.wait(10)
                && let Some(picture) = capture.take().expect("a take")
            {
                pictures += 1;
                assert!(picture.arrived > 0, "no arrival time");
                // The frame bounds: a little narrower than the window's
                // 400 x 300.
                assert!(
                    (300..=400).contains(&picture.width) && (200..=300).contains(&picture.height),
                    "the picture is {}x{}",
                    picture.width,
                    picture.height
                );
                let [b, g, r, _] = pixel(
                    &device,
                    &picture.texture,
                    picture.width / 2,
                    picture.height / 2,
                );
                seen = (r, g, b);
                if seen == (30, 160, 90) {
                    break;
                }
            }
            assert!(
                lowlat_common::clock::elapsed_ms(began) < 3000.0,
                "the window's colour not seen in 3 s: {pictures} picture(s), the last {seen:?}"
            );
        }
        assert!(capture.arrivals() >= u64::from(pictures));
        eprintln!("window capture: the colour in picture {pictures}");

        // SAFETY: the test's own window.
        unsafe { DestroyWindow(window) };
        let gone = lowlat_common::clock::Time::now();
        while !capture.closed() {
            pump();
            capture.wait(10);
            assert!(
                lowlat_common::clock::elapsed_ms(gone) < 3000.0,
                "the capture did not close with its window"
            );
        }
    }
}
