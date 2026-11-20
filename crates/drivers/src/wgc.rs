//! The system's capture of a window: the runtime's entry points it is reached
//! through, resolved by name, and the device it captures on.
//!
//! **A service of the signed-in user's.** The system serves the capture per
//! signed-in user, and refuses it to a process of the system account however
//! its thread acts, so only a process running as the user reaches it
//! (docs/impl-plan-windows.md W5).
//!
//! The objects are held by [`Com`] and called through their tables with
//! [`vcall!`](crate::vcall). Nothing here captures; the capture is the
//! backend's, which takes the runtime's objects from here.

use core::ffi::{CStr, c_void};
use core::fmt;
use core::marker::PhantomData;

use lowlat_common::dynlib::Library;

use crate::d3d11::{Com, Device};
use crate::ffi::d3d11::IUnknown;
use crate::ffi::d3d11_guids::IID_IDXGIDevice;
use crate::ffi::wgc::{
    __x_ABI_CWindows_CGraphics_CDirectX_CDirect3D11_CIDirect3DDevice as Direct3DDevice, GUID,
    HRESULT, HSTRING, IInspectable,
};
use crate::ffi::wgc_guids::IID___x_ABI_CWindows_CGraphics_CDirectX_CDirect3D11_CIDirect3DDevice;

type RoInitialize = unsafe extern "system" fn(i32) -> HRESULT;
type RoUninitialize = unsafe extern "system" fn();
type RoGetActivationFactory =
    unsafe extern "system" fn(HSTRING, *const GUID, *mut *mut c_void) -> HRESULT;
type WindowsCreateString = unsafe extern "system" fn(*const u16, u32, *mut HSTRING) -> HRESULT;
type WindowsDeleteString = unsafe extern "system" fn(HSTRING) -> HRESULT;
type CreateDirect3D11DeviceFromDxgiDevice =
    unsafe extern "system" fn(*mut c_void, *mut *mut IInspectable) -> HRESULT;

const RO_INIT_MULTITHREADED: i32 = 1;
/// The thread is in an apartment of another kind already, which serves.
const RPC_E_CHANGED_MODE: HRESULT = 0x8001_0106_u32 as HRESULT;

/// Why the runtime or one of its objects could not be had.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// A system library did not load.
    Unavailable,
    /// Loaded, but missing an entry point or a table entry it must have.
    MissingSymbol,
    /// A call failed, with its result.
    Status(i32),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable => f.write_str("window capture libraries not available"),
            Self::MissingSymbol => f.write_str("window capture library missing an entry point"),
            Self::Status(s) => write!(f, "window capture call returned 0x{s:08x}"),
        }
    }
}

impl std::error::Error for Error {}

type Result<T> = core::result::Result<T, Error>;

/// A failed result as an error.
pub fn check(hr: HRESULT) -> Result<()> {
    if hr < 0 {
        Err(Error::Status(hr))
    } else {
        Ok(())
    }
}

/// Ask `object` for another of its interfaces.
///
/// # Safety
///
/// `object` is a live interface of any kind: every table begins with the
/// base interface's three entries, which is all this calls.
pub unsafe fn query<T>(object: *mut c_void, iid: &GUID) -> Result<Com<T>> {
    let base = object.cast::<IInspectable>();
    let mut out: *mut c_void = core::ptr::null_mut();
    // SAFETY: the caller's live object, reached through the base entries
    // every table starts with; the identifier and the output are live.
    let hr = unsafe { crate::vcall!(base, QueryInterface, iid, &raw mut out) }
        .ok_or(Error::MissingSymbol)?;
    check(hr)?;
    // SAFETY: an interface of the asked-for type, whose reference is ours.
    unsafe { Com::from_raw(out.cast::<T>()) }.ok_or(Error::MissingSymbol)
}

/// The table of the interface a frame's surface hands its texture over by,
/// which the platform declares for C++ alone; its identifier is generated
/// with the rest (`IID_IDirect3DDxgiInterfaceAccess`).
#[repr(C)]
#[derive(Debug)]
// The generated tables' names, which `vcall!` reaches the table by.
#[allow(non_snake_case)]
pub struct IDirect3DDxgiInterfaceAccess {
    pub lpVtbl: *const IDirect3DDxgiInterfaceAccessVtbl,
}

#[repr(C)]
#[derive(Debug)]
#[allow(non_snake_case)]
pub struct IDirect3DDxgiInterfaceAccessVtbl {
    pub QueryInterface: Option<
        unsafe extern "system" fn(
            *mut IDirect3DDxgiInterfaceAccess,
            *const GUID,
            *mut *mut c_void,
        ) -> HRESULT,
    >,
    pub AddRef: Option<unsafe extern "system" fn(*mut IDirect3DDxgiInterfaceAccess) -> u32>,
    pub Release: Option<unsafe extern "system" fn(*mut IDirect3DDxgiInterfaceAccess) -> u32>,
    /// The Direct3D interface behind the surface: its texture, asked for by
    /// the texture's identifier.
    pub GetInterface: Option<
        unsafe extern "system" fn(
            *mut IDirect3DDxgiInterfaceAccess,
            *const crate::ffi::d3d11::GUID,
            *mut *mut c_void,
        ) -> HRESULT,
    >,
}

/// The table of the interface a window is handed to, to be made a capture
/// item: **declared without its object argument** in the platform's header,
/// which writes it for C++ alone, so written here; its identifier is
/// generated with the rest (`IID_IGraphicsCaptureItemInterop`).
#[repr(C)]
#[derive(Debug)]
// As above.
#[allow(non_snake_case)]
pub struct IGraphicsCaptureItemInterop {
    pub lpVtbl: *const IGraphicsCaptureItemInteropVtbl,
}

#[repr(C)]
#[derive(Debug)]
#[allow(non_snake_case)]
pub struct IGraphicsCaptureItemInteropVtbl {
    pub QueryInterface: Option<
        unsafe extern "system" fn(
            *mut IGraphicsCaptureItemInterop,
            *const GUID,
            *mut *mut c_void,
        ) -> HRESULT,
    >,
    pub AddRef: Option<unsafe extern "system" fn(*mut IGraphicsCaptureItemInterop) -> u32>,
    pub Release: Option<unsafe extern "system" fn(*mut IGraphicsCaptureItemInterop) -> u32>,
    /// The item of a window, as the interface the identifier names.
    pub CreateForWindow: Option<
        unsafe extern "system" fn(
            *mut IGraphicsCaptureItemInterop,
            *mut c_void,
            *const GUID,
            *mut *mut c_void,
        ) -> HRESULT,
    >,
    /// The item of a monitor, likewise.
    pub CreateForMonitor: Option<
        unsafe extern "system" fn(
            *mut IGraphicsCaptureItemInterop,
            *mut c_void,
            *const GUID,
            *mut *mut c_void,
        ) -> HRESULT,
    >,
}

/// The capture's runtime: its activation and its strings, and the device
/// wrapper the frames are made on.
pub struct Wgc {
    _combase: Library,
    _d3d11: Library,
    ro_initialize: RoInitialize,
    ro_uninitialize: RoUninitialize,
    activation_factory: RoGetActivationFactory,
    create_string: WindowsCreateString,
    delete_string: WindowsDeleteString,
    create_device: CreateDirect3D11DeviceFromDxgiDevice,
}

impl fmt::Debug for Wgc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Wgc")
    }
}

/// A resolved entry point, or the error for a library missing it.
///
/// # Safety
///
/// As [`Library::symbol`]: `T` describes the symbol as the library defines
/// it.
unsafe fn symbol<T: Copy>(library: &Library, name: &CStr) -> Result<T> {
    // SAFETY: the caller's contract.
    unsafe { library.symbol(name) }.ok_or(Error::MissingSymbol)
}

impl Wgc {
    /// Open the system's libraries. They ship with every system that has the
    /// capture; one without it lacks the entry points.
    pub fn load() -> Result<Self> {
        let combase = Library::open(c"combase.dll").ok_or(Error::Unavailable)?;
        let d3d11 = Library::open(c"d3d11.dll").ok_or(Error::Unavailable)?;
        // SAFETY: each type is the entry point's declaration in the
        // platform's headers, up to the calling convention, which on this
        // platform is one.
        unsafe {
            Ok(Self {
                ro_initialize: symbol(&combase, c"RoInitialize")?,
                ro_uninitialize: symbol(&combase, c"RoUninitialize")?,
                activation_factory: symbol(&combase, c"RoGetActivationFactory")?,
                create_string: symbol(&combase, c"WindowsCreateString")?,
                delete_string: symbol(&combase, c"WindowsDeleteString")?,
                create_device: symbol(&d3d11, c"CreateDirect3D11DeviceFromDXGIDevice")?,
                _combase: combase,
                _d3d11: d3d11,
            })
        }
    }

    /// Put the calling thread in the runtime's multithreaded apartment until
    /// the guard drops. **A thread in an apartment of another kind already
    /// is left in it**: the capture is made from either, and leaving it is
    /// its owner's.
    pub fn apartment(&self) -> Result<Apartment> {
        // SAFETY: a flag the call documents.
        let hr = unsafe { (self.ro_initialize)(RO_INIT_MULTITHREADED) };
        if hr == RPC_E_CHANGED_MODE {
            return Ok(Apartment {
                leave: None,
                _thread: PhantomData,
            });
        }
        check(hr)?;
        Ok(Apartment {
            leave: Some(self.ro_uninitialize),
            _thread: PhantomData,
        })
    }

    /// The activation factory of the runtime class named `class`, as the
    /// interface `iid` names, which must be `T`'s.
    pub fn factory<T>(&self, class: &str, iid: &GUID) -> Result<Com<T>> {
        let wide: Vec<u16> = class.encode_utf16().collect();
        let len = u32::try_from(wide.len()).map_err(|_| Error::Status(-1))?;
        let mut name: HSTRING = core::ptr::null_mut();
        // SAFETY: the units and their count are live for the call, which
        // copies them; the output is a live local.
        check(unsafe { (self.create_string)(wide.as_ptr(), len, &raw mut name) })?;
        let mut factory: *mut c_void = core::ptr::null_mut();
        // SAFETY: a string made just above; the identifier and the output
        // are live.
        let hr = unsafe { (self.activation_factory)(name, iid, &raw mut factory) };
        // SAFETY: the string made above, deleted once.
        unsafe { (self.delete_string)(name) };
        check(hr)?;
        // SAFETY: a factory of the asked-for interface, whose reference is
        // ours.
        unsafe { Com::from_raw(factory.cast::<T>()) }.ok_or(Error::MissingSymbol)
    }

    /// The runtime's device over `device`, which the frame pool makes its
    /// frames on: the frames are then textures of `device`'s own.
    pub fn device(&self, device: &Device) -> Result<Com<Direct3DDevice>> {
        let dxgi =
            crate::d3d11::query::<c_void>(device.device().cast::<IUnknown>(), &IID_IDXGIDevice)
                .map_err(|_| Error::MissingSymbol)?;
        let mut inspectable: *mut IInspectable = core::ptr::null_mut();
        // SAFETY: a live device's interface; the output is a live local.
        check(unsafe { (self.create_device)(dxgi.as_ptr(), &raw mut inspectable) })?;
        // SAFETY: an object whose reference the call handed over.
        let inspectable = unsafe { Com::from_raw(inspectable) }.ok_or(Error::MissingSymbol)?;
        // SAFETY: a live object.
        unsafe {
            query::<Direct3DDevice>(
                inspectable.as_ptr().cast(),
                &IID___x_ABI_CWindows_CGraphics_CDirectX_CDirect3D11_CIDirect3DDevice,
            )
        }
    }
}

/// The calling thread in the runtime's multithreaded apartment, which it
/// leaves on drop if it joined it here. The thread's: never sent.
#[derive(Debug)]
pub struct Apartment {
    leave: Option<RoUninitialize>,
    _thread: PhantomData<*const ()>,
}

impl Drop for Apartment {
    fn drop(&mut self) {
        if let Some(leave) = self.leave {
            // SAFETY: the thread that joined, leaving once; the library
            // stays mapped for the process's life.
            unsafe { leave() };
        }
    }
}
