//! AMD's own video runtime, opened at run time: its decoder, on a device the
//! caller made.
//!
//! **The runtime is the display driver's**, in the system directory, and it
//! answers whatever version it is initialised with, so nothing here is decided
//! by a version number: what a caller needs of a decoder it asks the decoder,
//! by the property's name. The factory the runtime hands out lives as long as
//! the process and is never released.
//!
//! **Its interfaces are C++ classes**, called through their tables. Only the
//! methods used are declared, each at the slot the runtime's headers give it,
//! which a test asserts; the rest of each table is padding. The one pair of
//! overloads the compiler lays out the other way round from the headers' C
//! transcription is in the padding.
//!
//! **Everything is on the caller's device**: a context is initialised on it --
//! which turns the device's multithread protection on, since the runtime's own
//! threads use its immediate context too -- and a decoder's surfaces are that
//! device's textures.
//!
//! **A decoder hands a picture out as soon as its unit is submitted**, the
//! decode still running: work queued on the device after the hand-out is
//! ordered behind the decode by the device, and a fence signalled then passes
//! once the picture is decoded. It takes some thirty units before it pushes
//! back, and then it spins in the submit, so a caller keeps its own count of
//! what is unfinished.

use core::ffi::{CStr, c_void};
use core::fmt;
use core::marker::PhantomData;
use core::ptr::NonNull;

use lowlat_common::dynlib::Library;

use crate::d3d11::Device;

/// A call's result, as the runtime numbers it.
pub type Status = i32;

const OK: Status = 0;
const EOF: Status = 23;
const REPEAT: Status = 24;
const INPUT_FULL: Status = 25;
const RESOLUTION_CHANGED: Status = 26;
const NEED_MORE_INPUT: Status = 44;

/// The runtime's library, installed with the display driver.
const LIBRARY: &CStr = c"amfrt64.dll";
/// The headers' version, 1.5.0: the most this side asks of a runtime.
const HEADER_VERSION: u64 = 1 << 48 | 5 << 32;
/// Host memory, the kind of buffer a unit is handed over in.
const MEMORY_HOST: i32 = 1;
/// The device interface a context asks its device for: the first, which
/// every device made here has.
const DX11_0: i32 = 110;
const VARIANT_BOOL: i32 = 1;
const VARIANT_INT64: i32 = 2;
/// Room for the longest name passed, with its terminator.
const NAME_CAPACITY: usize = 64;

/// How many reference pictures a decoder holds back before it hands one out,
/// an `i64`: [`REORDER_LOW_LATENCY`] or the default, which holds as many as
/// the stream references plus one.
pub const REORDER_MODE: &str = "ReorderMode";
/// Every picture handed out as soon as its unit is decoded, in decode order:
/// right for a stream that does not reorder, and for a caller that puts the
/// order back itself.
pub const REORDER_LOW_LATENCY: i64 = 2;
/// The decoder's low-latency mode, a `bool`: the engine kept at a clock that
/// decodes a picture paced in real time as fast as one back to back, where
/// the default lets the clock fall between pictures.
pub const LOW_LATENCY_DECODE: &str = "LowLatencyDecode";

type QueryVersion = unsafe extern "C" fn(*mut u64) -> Status;
type Init = unsafe extern "C" fn(u64, *mut *mut c_void) -> Status;

/// Why the runtime or a call on it failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// No runtime, which is the ordinary case without AMD's driver.
    Unavailable,
    /// Loaded, but missing an entry point it must export.
    MissingSymbol,
    /// A call failed, with its status.
    Status(Status),
    /// More than a buffer or a name holds.
    TooLarge,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable => f.write_str("AMD video runtime not available"),
            Self::MissingSymbol => f.write_str("AMD video runtime is missing an entry point"),
            Self::Status(s) => write!(f, "AMD video runtime returned status {s}"),
            Self::TooLarge => f.write_str("more than an AMD video buffer holds"),
        }
    }
}

impl std::error::Error for Error {}

type Result<T> = core::result::Result<T, Error>;

fn check(status: Status) -> Result<()> {
    if status == OK {
        Ok(())
    } else {
        Err(Error::Status(status))
    }
}

/// The decoders built, by codec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    H264,
    /// Both depths: a ten-bit stream is told apart by the layout the
    /// decoder is initialised with.
    Hevc,
}

impl Codec {
    const fn id(self) -> &'static str {
        match self {
            Self::H264 => "AMFVideoDecoderUVD_H264_AVC",
            Self::Hevc => "AMFVideoDecoderHW_H265_HEVC",
        }
    }
}

/// The layouts a decoder hands pictures out in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// Eight bits, a luma plane and an interleaved chroma plane.
    Nv12,
    /// Ten bits in the high bits of sixteen, the same two planes. **A
    /// ten-bit stream must be decoded into this**: into the other it
    /// decodes every picture wrongly, without an error.
    P010,
}

impl Layout {
    const fn code(self) -> i32 {
        match self {
            Self::Nv12 => 1,
            Self::P010 => 10,
        }
    }

    const fn of(code: i32) -> Option<Self> {
        match code {
            1 => Some(Self::Nv12),
            10 => Some(Self::P010),
            _ => None,
        }
    }
}

/// What a submitted unit came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Submitted {
    /// Taken: a picture it completes is handed out by the next query.
    Taken,
    /// Taken, and it completes no picture on its own.
    MoreInput,
    /// Taken, and it holds more than one picture: submitted again with no
    /// unit to go on.
    Repeat,
    /// Not taken: the decoder's queue is full.
    Full,
    /// Not taken: the stream changed size under the decoder.
    ResolutionChanged,
}

/// A name as the runtime takes one: UTF-16, terminated. Every name passed
/// here is ASCII.
struct Name([u16; NAME_CAPACITY]);

impl Name {
    fn new(text: &str) -> Result<Self> {
        let mut out = [0u16; NAME_CAPACITY];
        // The last place stays the terminator.
        let room = out.get_mut(..NAME_CAPACITY - 1).ok_or(Error::TooLarge)?;
        if text.len() > room.len() || !text.is_ascii() {
            return Err(Error::TooLarge);
        }
        for (to, from) in room.iter_mut().zip(text.bytes()) {
            *to = u16::from(from);
        }
        Ok(Self(out))
    }

    fn as_ptr(&self) -> *const u16 {
        self.0.as_ptr()
    }
}

/// An interface's identity.
#[repr(C)]
struct Guid {
    data1: u32,
    data2: u16,
    data3: u16,
    data4: [u8; 8],
}

const IID_SURFACE: Guid = Guid {
    data1: 0x3075_dbe3,
    data2: 0x8718,
    data3: 0x4cfa,
    data4: [0x86, 0xfb, 0x21, 0x14, 0xc0, 0xa5, 0xa4, 0x51],
};

/// A property's value: its kind, then a value as wide as the widest the
/// runtime has (a rectangle). Passed by value.
#[repr(C)]
#[derive(Clone, Copy)]
struct Variant {
    kind: i32,
    value: Value,
}

#[repr(C)]
#[derive(Clone, Copy)]
union Value {
    boolean: u8,
    int64: i64,
    wide: [u64; 2],
}

impl Variant {
    fn boolean(value: bool) -> Self {
        let mut v = Value { wide: [0; 2] };
        v.boolean = u8::from(value);
        Self {
            kind: VARIANT_BOOL,
            value: v,
        }
    }

    fn int64(value: i64) -> Self {
        let mut v = Value { wide: [0; 2] };
        v.int64 = value;
        Self {
            kind: VARIANT_INT64,
            value: v,
        }
    }
}

// The tables. Every method takes the object as its first argument; a slot
// not used is a `usize` of padding, so each used one sits at its place.

/// The base interface's three, which begin every table but the factory's.
#[repr(C)]
struct BaseTable {
    acquire: unsafe extern "system" fn(*mut c_void) -> i32,
    release: unsafe extern "system" fn(*mut c_void) -> i32,
    query_interface:
        unsafe extern "system" fn(*mut c_void, *const Guid, *mut *mut c_void) -> Status,
}

#[repr(C)]
struct FactoryTable {
    create_context: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> Status,
    create_component:
        unsafe extern "system" fn(*mut c_void, *mut c_void, *const u16, *mut *mut c_void) -> Status,
    rest: [usize; 5],
}

#[repr(C)]
struct ContextTable {
    base: BaseTable,
    storage: [usize; 10],
    terminate: unsafe extern "system" fn(*mut c_void) -> Status,
    dx9: [usize; 4],
    init_dx11: unsafe extern "system" fn(*mut c_void, *mut c_void, i32) -> Status,
    devices: [usize; 24],
    alloc_buffer: unsafe extern "system" fn(*mut c_void, i32, usize, *mut *mut c_void) -> Status,
    rest: [usize; 11],
}

#[repr(C)]
struct ComponentTable {
    base: BaseTable,
    set_property: unsafe extern "system" fn(*mut c_void, *const u16, Variant) -> Status,
    get_property: usize,
    has_property: unsafe extern "system" fn(*mut c_void, *const u16) -> u8,
    storage: [usize; 11],
    init: unsafe extern "system" fn(*mut c_void, i32, i32, i32) -> Status,
    reinit: usize,
    terminate: unsafe extern "system" fn(*mut c_void) -> Status,
    drain_flush: [usize; 2],
    submit_input: unsafe extern "system" fn(*mut c_void, *mut c_void) -> Status,
    query_output: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> Status,
    rest: [usize; 4],
}

/// A buffer's: the data interface's, then its own.
#[repr(C)]
struct BufferTable {
    base: BaseTable,
    storage: [usize; 10],
    data: [usize; 6],
    set_pts: unsafe extern "system" fn(*mut c_void, i64),
    get_pts: usize,
    duration: [usize; 2],
    set_size: unsafe extern "system" fn(*mut c_void, usize) -> Status,
    get_size: usize,
    get_native: unsafe extern "system" fn(*mut c_void) -> *mut c_void,
    observers: [usize; 2],
}

/// A surface's: the data interface's, then its own.
#[repr(C)]
struct SurfaceTable {
    base: BaseTable,
    storage: [usize; 10],
    data: [usize; 6],
    set_pts: usize,
    get_pts: unsafe extern "system" fn(*mut c_void) -> i64,
    duration: [usize; 2],
    get_format: unsafe extern "system" fn(*mut c_void) -> i32,
    planes_count: usize,
    get_plane_at: unsafe extern "system" fn(*mut c_void, usize) -> *mut c_void,
    rest: [usize; 7],
}

#[repr(C)]
struct PlaneTable {
    base: BaseTable,
    kind: usize,
    get_native: unsafe extern "system" fn(*mut c_void) -> *mut c_void,
    rest: [usize; 8],
}

/// The table of the object at `this`.
///
/// # Safety
///
/// `this` is a live object whose table is laid out as `T`, and the table
/// outlives the borrow, which the runtime's tables, being static, do.
unsafe fn table<'t, T>(this: NonNull<c_void>) -> &'t T {
    // SAFETY: the caller's contract; an object begins with its table's
    // address.
    unsafe { &**this.as_ptr().cast::<*const T>() }
}

/// Give up one reference to `this`.
///
/// # Safety
///
/// `this` is a live object this side holds a reference on, given up once.
unsafe fn release(this: NonNull<c_void>) {
    // SAFETY: the caller's contract; every table but the factory's begins
    // with the base interface's.
    let base = unsafe { table::<BaseTable>(this) };
    // SAFETY: as above.
    unsafe { (base.release)(this.as_ptr()) };
}

/// The loaded runtime and the factory it handed out.
pub struct Amf {
    factory: NonNull<c_void>,
    version: u64,
    /// Last, so it outlives the addresses taken from it.
    _library: Library,
}

impl fmt::Debug for Amf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Amf")
            .field("version", &format_args!("{:#x}", self.version))
            .finish()
    }
}

impl Amf {
    /// Open the runtime and initialise it at the lesser of its version and
    /// the headers'.
    pub fn load() -> Result<Self> {
        let library = Library::open(LIBRARY).ok_or(Error::Unavailable)?;
        // SAFETY: both signatures are the runtime header's.
        let query: QueryVersion =
            unsafe { library.symbol(c"AMFQueryVersion") }.ok_or(Error::MissingSymbol)?;
        // SAFETY: as above.
        let init: Init = unsafe { library.symbol(c"AMFInit") }.ok_or(Error::MissingSymbol)?;
        let mut version = 0u64;
        // SAFETY: the output is a live local.
        check(unsafe { query(&raw mut version) })?;
        let mut factory: *mut c_void = core::ptr::null_mut();
        // SAFETY: as above.
        check(unsafe { init(version.min(HEADER_VERSION), &raw mut factory) })?;
        let factory = NonNull::new(factory).ok_or(Error::Unavailable)?;
        Ok(Self {
            factory,
            version,
            _library: library,
        })
    }

    /// The runtime's own version, as it reports it: major, minor, release
    /// and build, sixteen bits each from the top.
    pub fn version(&self) -> u64 {
        self.version
    }

    fn factory(&self) -> &FactoryTable {
        // SAFETY: the factory the runtime handed out, whose table is the
        // factory's and which lives as long as the process.
        unsafe { table(self.factory) }
    }

    /// A context on `device`, which the context must not outlive.
    pub fn context<'d>(&self, device: &'d Device) -> Result<Context<'d>> {
        let mut raw: *mut c_void = core::ptr::null_mut();
        // SAFETY: a live factory; the output is a live local.
        check(unsafe { (self.factory().create_context)(self.factory.as_ptr(), &raw mut raw) })?;
        let context = Context {
            raw: NonNull::new(raw).ok_or(Error::Unavailable)?,
            _device: PhantomData,
        };
        // SAFETY: a live context and a live device, which outlives it by the
        // lifetime above.
        check(unsafe {
            (context.table().init_dx11)(context.raw.as_ptr(), device.device().cast(), DX11_0)
        })?;
        Ok(context)
    }

    /// A decoder for `codec` in `context`, not yet initialised.
    pub fn decoder(&self, context: &Context<'_>, codec: Codec) -> Result<Component> {
        let id = Name::new(codec.id())?;
        let mut raw: *mut c_void = core::ptr::null_mut();
        // SAFETY: a live factory and context; the name and the output are
        // live for the call.
        check(unsafe {
            (self.factory().create_component)(
                self.factory.as_ptr(),
                context.raw.as_ptr(),
                id.as_ptr(),
                &raw mut raw,
            )
        })?;
        Ok(Component {
            raw: NonNull::new(raw).ok_or(Error::Unavailable)?,
        })
    }
}

/// A context on a device of the caller's: terminated and released on drop,
/// after everything made in it.
pub struct Context<'d> {
    raw: NonNull<c_void>,
    _device: PhantomData<&'d Device>,
}

impl fmt::Debug for Context<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Context")
    }
}

impl Context<'_> {
    fn table(&self) -> &ContextTable {
        // SAFETY: a live context, whose table is the context's.
        unsafe { table(self.raw) }
    }

    /// A buffer of host memory, `capacity` bytes, a unit at a time handed
    /// over in it.
    pub fn buffer(&self, capacity: usize) -> Result<Buffer> {
        let mut raw: *mut c_void = core::ptr::null_mut();
        // SAFETY: a live context; the output is a live local.
        check(unsafe {
            (self.table().alloc_buffer)(self.raw.as_ptr(), MEMORY_HOST, capacity, &raw mut raw)
        })?;
        Ok(Buffer {
            raw: NonNull::new(raw).ok_or(Error::Unavailable)?,
            capacity,
        })
    }
}

impl Drop for Context<'_> {
    fn drop(&mut self) {
        // SAFETY: a live context, terminated once and then released once.
        unsafe {
            (self.table().terminate)(self.raw.as_ptr());
            release(self.raw);
        }
    }
}

/// A decoder: terminated and released on drop, which comes before its
/// context's.
pub struct Component {
    raw: NonNull<c_void>,
}

impl fmt::Debug for Component {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Component")
    }
}

impl Component {
    fn table(&self) -> &ComponentTable {
        // SAFETY: a live component, whose table is the component's.
        unsafe { table(self.raw) }
    }

    /// Whether the decoder has the property `name`: how a runtime is asked
    /// what it can do.
    pub fn has(&self, name: &str) -> bool {
        let Ok(name) = Name::new(name) else {
            return false;
        };
        // SAFETY: a live component and a terminated name.
        unsafe { (self.table().has_property)(self.raw.as_ptr(), name.as_ptr()) != 0 }
    }

    pub fn set_bool(&self, name: &str, value: bool) -> Result<()> {
        self.set(name, Variant::boolean(value))
    }

    pub fn set_int(&self, name: &str, value: i64) -> Result<()> {
        self.set(name, Variant::int64(value))
    }

    fn set(&self, name: &str, value: Variant) -> Result<()> {
        let name = Name::new(name)?;
        // SAFETY: a live component and a terminated name; the value is
        // plain data passed by value.
        check(unsafe { (self.table().set_property)(self.raw.as_ptr(), name.as_ptr(), value) })
    }

    /// Initialise the decoder for pictures of `width` x `height`, handed out
    /// in `layout`.
    pub fn init(&self, layout: Layout, width: u32, height: u32) -> Result<()> {
        let width = i32::try_from(width).map_err(|_| Error::TooLarge)?;
        let height = i32::try_from(height).map_err(|_| Error::TooLarge)?;
        // SAFETY: a live component.
        check(unsafe { (self.table().init)(self.raw.as_ptr(), layout.code(), width, height) })
    }

    /// Hand the decoder the unit in `buffer`. The buffer is the caller's
    /// again once this returns: the decoder keeps no reference to it.
    pub fn submit(&self, buffer: &Buffer) -> Result<Submitted> {
        // SAFETY: a live component and a live buffer.
        Self::submitted(unsafe {
            (self.table().submit_input)(self.raw.as_ptr(), buffer.raw.as_ptr())
        })
    }

    /// Go on with the unit last submitted, which held more than one
    /// picture.
    pub fn resubmit(&self) -> Result<Submitted> {
        // SAFETY: a live component; no unit is the call's own way of saying
        // "the rest of the last one".
        Self::submitted(unsafe {
            (self.table().submit_input)(self.raw.as_ptr(), core::ptr::null_mut())
        })
    }

    fn submitted(status: Status) -> Result<Submitted> {
        match status {
            OK => Ok(Submitted::Taken),
            NEED_MORE_INPUT => Ok(Submitted::MoreInput),
            REPEAT => Ok(Submitted::Repeat),
            INPUT_FULL => Ok(Submitted::Full),
            RESOLUTION_CHANGED => Ok(Submitted::ResolutionChanged),
            other => Err(Error::Status(other)),
        }
    }

    /// The next picture the decoder hands out, if one is ready: at once for
    /// a unit submitted, its decode still running on the device.
    pub fn query(&self) -> Result<Option<Surface>> {
        let mut data: *mut c_void = core::ptr::null_mut();
        // SAFETY: a live component; the output is a live local.
        let status = unsafe { (self.table().query_output)(self.raw.as_ptr(), &raw mut data) };
        match status {
            OK => {}
            REPEAT | EOF => return Ok(None),
            other => return Err(Error::Status(other)),
        }
        let Some(data) = NonNull::new(data) else {
            return Ok(None);
        };
        // The data handed out is a surface; its surface interface is asked
        // for, and the data's own reference given up.
        let mut surface: *mut c_void = core::ptr::null_mut();
        // SAFETY: a live object the call handed a reference on; the identity
        // and the output are live.
        let status = unsafe {
            let base = table::<BaseTable>(data);
            let status = (base.query_interface)(data.as_ptr(), &IID_SURFACE, &raw mut surface);
            release(data);
            status
        };
        check(status)?;
        Ok(Some(Surface {
            raw: NonNull::new(surface).ok_or(Error::Status(status))?,
        }))
    }
}

impl Drop for Component {
    fn drop(&mut self) {
        // SAFETY: a live component, terminated once and then released once.
        unsafe {
            (self.table().terminate)(self.raw.as_ptr());
            release(self.raw);
        }
    }
}

/// A buffer of host memory a unit is handed over in: made once, filled for
/// each unit, released on drop, which comes before its context's.
pub struct Buffer {
    raw: NonNull<c_void>,
    capacity: usize,
}

impl fmt::Debug for Buffer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Buffer")
            .field("capacity", &self.capacity)
            .finish()
    }
}

impl Buffer {
    fn table(&self) -> &BufferTable {
        // SAFETY: a live buffer, whose table is the buffer's.
        unsafe { table(self.raw) }
    }

    /// Put `bytes` in the buffer, as its whole content.
    pub fn fill(&mut self, bytes: &[u8]) -> Result<()> {
        if bytes.len() > self.capacity {
            return Err(Error::TooLarge);
        }
        // SAFETY: a live buffer, sized within the capacity it was made with.
        check(unsafe { (self.table().set_size)(self.raw.as_ptr(), bytes.len()) })?;
        // SAFETY: as above.
        let native = unsafe { (self.table().get_native)(self.raw.as_ptr()) };
        if native.is_null() {
            return Err(Error::Unavailable);
        }
        // SAFETY: the buffer's host memory, at least `bytes.len()` bytes as
        // just sized, which nothing else writes; the two do not overlap.
        unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), native.cast::<u8>(), bytes.len()) };
        Ok(())
    }

    /// Mark the unit with `pts`, which the surface it completes carries out.
    pub fn set_pts(&self, pts: i64) {
        // SAFETY: a live buffer.
        unsafe { (self.table().set_pts)(self.raw.as_ptr(), pts) };
    }
}

impl Drop for Buffer {
    fn drop(&mut self) {
        // SAFETY: a live buffer this holds one reference on.
        unsafe { release(self.raw) };
    }
}

/// A decoded picture, handed out while its decode may still be running on
/// the device: released on drop, which gives it back to the decoder.
pub struct Surface {
    raw: NonNull<c_void>,
}

impl fmt::Debug for Surface {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Surface")
    }
}

impl Surface {
    fn table(&self) -> &SurfaceTable {
        // SAFETY: a live surface, whose table is the surface's.
        unsafe { table(self.raw) }
    }

    /// The mark of the unit that completed it.
    pub fn pts(&self) -> i64 {
        // SAFETY: a live surface.
        unsafe { (self.table().get_pts)(self.raw.as_ptr()) }
    }

    /// The layout the picture is in; `None` for one this side does not know.
    pub fn layout(&self) -> Option<Layout> {
        // SAFETY: a live surface.
        Layout::of(unsafe { (self.table().get_format)(self.raw.as_ptr()) })
    }

    /// The texture the picture is in, an `ID3D11Texture2D` of the context's
    /// device, both planes in the one texture: borrowed from the surface, no
    /// reference of its own, and valid while the surface is held. Null where
    /// the surface has none.
    pub fn texture(&self) -> *mut c_void {
        // SAFETY: a live surface; its first plane is borrowed for the call.
        let plane = unsafe { (self.table().get_plane_at)(self.raw.as_ptr(), 0) };
        let Some(plane) = NonNull::new(plane) else {
            return core::ptr::null_mut();
        };
        // SAFETY: a live plane of a live surface, whose table is the plane's.
        unsafe { (table::<PlaneTable>(plane).get_native)(plane.as_ptr()) }
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        // SAFETY: a live surface this holds one reference on.
        unsafe { release(self.raw) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::d3d11::D3d11;
    use core::mem::{align_of, offset_of, size_of};

    /// Where slot `n` of a table begins.
    fn slot(n: usize) -> usize {
        n * size_of::<usize>()
    }

    /// **Every method used sits at the slot the runtime's headers give it**,
    /// and every table is as long as the headers' -- counted there by hand,
    /// method by method, since a slot off by one calls another method with
    /// these arguments.
    #[test]
    fn every_method_sits_at_its_slot() {
        assert_eq!(offset_of!(BaseTable, release), slot(1));
        assert_eq!(offset_of!(BaseTable, query_interface), slot(2));

        assert_eq!(offset_of!(FactoryTable, create_context), slot(0));
        assert_eq!(offset_of!(FactoryTable, create_component), slot(1));
        assert_eq!(size_of::<FactoryTable>(), slot(7));

        assert_eq!(offset_of!(ContextTable, terminate), slot(13));
        assert_eq!(offset_of!(ContextTable, init_dx11), slot(18));
        assert_eq!(offset_of!(ContextTable, alloc_buffer), slot(43));
        assert_eq!(size_of::<ContextTable>(), slot(55));

        assert_eq!(offset_of!(ComponentTable, set_property), slot(3));
        assert_eq!(offset_of!(ComponentTable, has_property), slot(5));
        assert_eq!(offset_of!(ComponentTable, init), slot(17));
        assert_eq!(offset_of!(ComponentTable, terminate), slot(19));
        assert_eq!(offset_of!(ComponentTable, submit_input), slot(22));
        assert_eq!(offset_of!(ComponentTable, query_output), slot(23));
        assert_eq!(size_of::<ComponentTable>(), slot(28));

        assert_eq!(offset_of!(BufferTable, set_pts), slot(19));
        assert_eq!(offset_of!(BufferTable, set_size), slot(23));
        assert_eq!(offset_of!(BufferTable, get_native), slot(25));
        assert_eq!(size_of::<BufferTable>(), slot(28));

        assert_eq!(offset_of!(SurfaceTable, get_pts), slot(20));
        assert_eq!(offset_of!(SurfaceTable, get_format), slot(23));
        assert_eq!(offset_of!(SurfaceTable, get_plane_at), slot(25));
        assert_eq!(size_of::<SurfaceTable>(), slot(33));

        assert_eq!(offset_of!(PlaneTable, get_native), slot(4));
        assert_eq!(size_of::<PlaneTable>(), slot(13));
    }

    /// A property's value is its kind and then sixteen bytes, eight-aligned:
    /// twenty-four in all, passed by value.
    #[test]
    fn a_variant_is_a_kind_and_sixteen_bytes() {
        assert_eq!(size_of::<Variant>(), 24);
        assert_eq!(align_of::<Variant>(), 8);
        assert_eq!(offset_of!(Variant, value), 8);
        assert_eq!(size_of::<Guid>(), 16);
        // SAFETY: reading back the member just written.
        assert_eq!(unsafe { Variant::int64(2).value.int64 }, 2);
        // SAFETY: as above.
        assert_eq!(unsafe { Variant::boolean(true).value.boolean }, 1);
    }

    /// A name goes over as terminated UTF-16; one too long for the room, or
    /// not ASCII, is refused rather than cut short.
    #[test]
    fn a_name_is_terminated_and_never_cut_short() {
        let name = Name::new(LOW_LATENCY_DECODE).unwrap();
        let text: Vec<u16> = LOW_LATENCY_DECODE.bytes().map(u16::from).collect();
        assert_eq!(&name.0[..text.len()], text.as_slice());
        assert_eq!(name.0[text.len()], 0);
        assert!(Name::new(&"x".repeat(NAME_CAPACITY - 1)).is_ok());
        assert_eq!(
            Name::new(&"x".repeat(NAME_CAPACITY)).err(),
            Some(Error::TooLarge)
        );
        assert_eq!(Name::new("Reorder\u{e9}").err(), Some(Error::TooLarge));
    }

    /// The layouts go over as the runtime numbers them.
    #[test]
    fn a_layout_is_numbered_as_the_runtime_numbers_it() {
        for layout in [Layout::Nv12, Layout::P010] {
            assert_eq!(Layout::of(layout.code()), Some(layout));
        }
        assert_eq!(Layout::Nv12.code(), 1);
        assert_eq!(Layout::P010.code(), 10);
        assert_eq!(Layout::of(15), None);
    }

    /// On a machine with AMD's runtime and GPU: the runtime loads, a
    /// context is made on a device of ours, and both decoders build and
    /// take the two low-latency properties.
    #[test]
    #[ignore = "requires AMD's runtime and GPU"]
    fn the_runtime_builds_both_decoders_on_a_device_of_ours() {
        let d3d11 = D3d11::load().unwrap();
        let adapter = d3d11
            .adapters()
            .unwrap()
            .into_iter()
            .find(|a| a.decodes_here() && a.maker() == Some("AMD"))
            .expect("an AMD GPU");
        let device = d3d11.open(adapter.luid).unwrap();
        let amf = Amf::load().unwrap();
        eprintln!("runtime version {:#x}", amf.version());
        let context = amf.context(&device).unwrap();
        for (codec, layout) in [
            (Codec::H264, Layout::Nv12),
            (Codec::Hevc, Layout::Nv12),
            (Codec::Hevc, Layout::P010),
        ] {
            let decoder = amf.decoder(&context, codec).unwrap();
            assert!(decoder.has(LOW_LATENCY_DECODE), "{codec:?}");
            decoder.set_int(REORDER_MODE, REORDER_LOW_LATENCY).unwrap();
            decoder.set_bool(LOW_LATENCY_DECODE, true).unwrap();
            decoder.init(layout, 1920, 1080).unwrap();
        }
        let mut buffer = context.buffer(4096).unwrap();
        buffer.fill(&[0, 0, 0, 1]).unwrap();
        assert_eq!(buffer.fill(&[0; 4097]).err(), Some(Error::TooLarge));
    }
}
