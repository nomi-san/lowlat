//! The sound server's client interface, loaded at runtime.
//!
//! Loaded rather than linked, for the reason every runtime here is: a machine
//! without it has **no audio rather than a service that will not start**. This
//! file is the whole surface and the only place in the crate that contains
//! `unsafe`.
//!
//! **Two libraries.** Everything used here is in the client library except the
//! call that turns an error code into a string, which lives beside it.

use core::ffi::{c_char, c_int, c_void};

use lowlat_common::dynlib::Library;

use crate::Error;

/// Opaque handles. Held and handed back, never dereferenced here.
pub(crate) type MainLoop = c_void;
pub(crate) type MainLoopApi = c_void;
pub(crate) type Context = c_void;
pub(crate) type Stream = c_void;

/// Sixteen-bit little endian, which is what an uncompressed packet carries and
/// what the encoder reads.
pub(crate) const SAMPLE_S16LE: c_int = 3;

pub(crate) const CONTEXT_READY: c_int = 4;
pub(crate) const CONTEXT_FAILED: c_int = 5;
pub(crate) const CONTEXT_TERMINATED: c_int = 6;

pub(crate) const STREAM_READY: c_int = 2;
pub(crate) const STREAM_FAILED: c_int = 3;
pub(crate) const STREAM_TERMINATED: c_int = 4;

/// **Never start a sound server of our own.** A service running outside the
/// session connects to the one that is there or reports that there is none;
/// spawning one would produce a server with nothing playing into it and a host
/// streaming silence it could not explain.
pub(crate) const CONTEXT_NOAUTOSPAWN: c_int = 1;

/// Take the fragment size as a latency request rather than as a plain buffer
/// size.
pub(crate) const STREAM_ADJUST_LATENCY: c_int = 0x2000;

/// The server's own state, which is where the default output lives.
pub(crate) const SUBSCRIPTION_MASK_SERVER: u32 = 0x0080;

/// What is asked for and what a read returns.
#[repr(C)]
#[derive(Debug)]
pub(crate) struct SampleSpec {
    pub format: c_int,
    pub rate: u32,
    pub channels: u8,
}

/// **Only `fragsize` matters to a reader.** The rest are asked for as the
/// server's own defaults, which is what `u32::MAX` means in each of them.
#[repr(C)]
#[derive(Debug)]
pub(crate) struct BufferAttr {
    pub maxlength: u32,
    pub tlength: u32,
    pub prebuf: u32,
    pub minreq: u32,
    pub fragsize: u32,
}

impl BufferAttr {
    /// Defaults everywhere except the fragment, which is the frame this host
    /// sends.
    pub(crate) const fn fragments_of(bytes: u32) -> Self {
        Self {
            maxlength: u32::MAX,
            tlength: u32::MAX,
            prebuf: u32::MAX,
            minreq: u32::MAX,
            fragsize: bytes,
        }
    }
}

/// The prefix of a device's description that this crate reads.
///
/// **Transcribed from the interface's own headers and checked against them**,
/// not guessed: the assertions below are the compiler repeating what `offsetof`
/// says. A source and a sink describe themselves with different structures
/// whose first ten fields are laid out identically, which is why one definition
/// serves both -- and why the last two fields are named for what each kind uses
/// them for rather than for one of them.
///
/// Only the prefix is defined. Nothing here allocates one; they arrive by
/// pointer from the interface, so a definition that stops early is a definition
/// that reads less.
#[repr(C)]
#[derive(Debug)]
pub(crate) struct DeviceInfo {
    /// The device's own name, which is **also how a reader checks that this
    /// layout is right**: it was asked for by name, so a mismatch means the
    /// bytes are being read at the wrong offsets.
    pub name: *const c_char,
    pub index: u32,
    pub description: *const c_char,
    pub sample_spec: SampleSpec,
    /// Opaque here: 132 bytes at four-byte alignment.
    pub channel_map: [u32; 33],
    pub owner_module: u32,
    /// Opaque here: 132 bytes at four-byte alignment.
    pub volume: [u32; 33],
    pub mute: c_int,
    /// For a source, the sink it monitors. For a sink, its monitor source.
    pub paired: u32,
    /// The name of that pair, or null.
    pub paired_name: *const c_char,
}

const _: () = assert!(core::mem::offset_of!(DeviceInfo, mute) == 304);
const _: () = assert!(core::mem::offset_of!(DeviceInfo, paired) == 308);
const _: () = assert!(core::mem::offset_of!(DeviceInfo, paired_name) == 312);

pub(crate) type NotifyStream = unsafe extern "C" fn(*mut Stream, *mut c_void);
pub(crate) type RequestStream = unsafe extern "C" fn(*mut Stream, usize, *mut c_void);
pub(crate) type SubscribeContext = unsafe extern "C" fn(*mut Context, u32, u32, *mut c_void);

type MainLoopNew = unsafe extern "C" fn() -> *mut MainLoop;
type MainLoopFree = unsafe extern "C" fn(*mut MainLoop);
type MainLoopGetApi = unsafe extern "C" fn(*mut MainLoop) -> *mut MainLoopApi;
type MainLoopPrepare = unsafe extern "C" fn(*mut MainLoop, c_int) -> c_int;
type MainLoopPoll = unsafe extern "C" fn(*mut MainLoop) -> c_int;
type MainLoopDispatch = unsafe extern "C" fn(*mut MainLoop) -> c_int;

type ContextNew = unsafe extern "C" fn(*mut MainLoopApi, *const c_char) -> *mut Context;
type ContextConnect =
    unsafe extern "C" fn(*mut Context, *const c_char, c_int, *const c_void) -> c_int;
type ContextDisconnect = unsafe extern "C" fn(*mut Context);
type ContextUnref = unsafe extern "C" fn(*mut Context);
type ContextGetState = unsafe extern "C" fn(*const Context) -> c_int;
type ContextErrno = unsafe extern "C" fn(*const Context) -> c_int;
type ContextSubscribe =
    unsafe extern "C" fn(*mut Context, u32, *const c_void, *mut c_void) -> *mut c_void;
type ContextSetSubscribeCallback =
    unsafe extern "C" fn(*mut Context, Option<SubscribeContext>, *mut c_void);
type OperationUnref = unsafe extern "C" fn(*mut c_void);

pub(crate) type DeviceInfoCb =
    unsafe extern "C" fn(*mut Context, *const DeviceInfo, c_int, *mut c_void);
pub(crate) type SuccessCb = unsafe extern "C" fn(*mut Context, c_int, *mut c_void);

type ContextGetInfoByName = unsafe extern "C" fn(
    *mut Context,
    *const c_char,
    Option<DeviceInfoCb>,
    *mut c_void,
) -> *mut c_void;
type ContextGetInfoList =
    unsafe extern "C" fn(*mut Context, Option<DeviceInfoCb>, *mut c_void) -> *mut c_void;
type ContextSetSinkMute = unsafe extern "C" fn(
    *mut Context,
    *const c_char,
    c_int,
    Option<SuccessCb>,
    *mut c_void,
) -> *mut c_void;

type StreamNew = unsafe extern "C" fn(
    *mut Context,
    *const c_char,
    *const SampleSpec,
    *const c_void,
) -> *mut Stream;
type StreamConnectRecord =
    unsafe extern "C" fn(*mut Stream, *const c_char, *const BufferAttr, c_int) -> c_int;
type StreamDisconnect = unsafe extern "C" fn(*mut Stream) -> c_int;
type StreamUnref = unsafe extern "C" fn(*mut Stream);
type StreamGetState = unsafe extern "C" fn(*const Stream) -> c_int;
type StreamSetReadCallback = unsafe extern "C" fn(*mut Stream, Option<RequestStream>, *mut c_void);
type StreamSetMovedCallback = unsafe extern "C" fn(*mut Stream, Option<NotifyStream>, *mut c_void);
type StreamPeek = unsafe extern "C" fn(*mut Stream, *mut *const c_void, *mut usize) -> c_int;
type StreamDrop = unsafe extern "C" fn(*mut Stream) -> c_int;
type StreamGetDeviceName = unsafe extern "C" fn(*const Stream) -> *const c_char;

/// Every call this crate makes, resolved once.
pub(crate) struct Pulse {
    pub mainloop_new: MainLoopNew,
    pub mainloop_free: MainLoopFree,
    pub mainloop_get_api: MainLoopGetApi,
    pub mainloop_prepare: MainLoopPrepare,
    pub mainloop_poll: MainLoopPoll,
    pub mainloop_dispatch: MainLoopDispatch,
    pub context_new: ContextNew,
    pub context_connect: ContextConnect,
    pub context_disconnect: ContextDisconnect,
    pub context_unref: ContextUnref,
    pub context_get_state: ContextGetState,
    pub context_errno: ContextErrno,
    pub context_subscribe: ContextSubscribe,
    pub context_set_subscribe_callback: ContextSetSubscribeCallback,
    pub operation_unref: OperationUnref,
    pub source_info_by_name: ContextGetInfoByName,
    pub sink_info_by_name: ContextGetInfoByName,
    pub set_sink_mute_by_name: ContextSetSinkMute,
    pub sink_info_list: ContextGetInfoList,
    pub stream_new: StreamNew,
    pub stream_connect_record: StreamConnectRecord,
    pub stream_disconnect: StreamDisconnect,
    pub stream_unref: StreamUnref,
    pub stream_get_state: StreamGetState,
    pub stream_set_read_callback: StreamSetReadCallback,
    pub stream_set_moved_callback: StreamSetMovedCallback,
    pub stream_peek: StreamPeek,
    pub stream_drop: StreamDrop,
    pub stream_get_device_name: StreamGetDeviceName,
    /// Last, so it outlives the addresses taken from it.
    _library: Library,
}

impl core::fmt::Debug for Pulse {
    /// A table of addresses says nothing a log can use.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("Pulse(loaded)")
    }
}

// SAFETY: the handles inside are addresses in this process, not thread-affine,
// and every one is read-only after load. The objects they operate on are what
// carry a thread rule, and those never leave the capture thread.
unsafe impl Send for Pulse {}
unsafe impl Sync for Pulse {}

impl Pulse {
    /// Resolve the client interface, or say which half is missing.
    pub(crate) fn load() -> Result<Self, Error> {
        let library = Library::open(c"libpulse.so.0").ok_or(Error::Unavailable)?;
        // SAFETY: every signature is transcribed from the interface's own
        // headers. The pointers borrow `library`, which is moved into the
        // returned value and dropped after them.
        unsafe {
            Ok(Self {
                mainloop_new: library
                    .symbol(c"pa_mainloop_new")
                    .ok_or(Error::Incomplete)?,
                mainloop_free: library
                    .symbol(c"pa_mainloop_free")
                    .ok_or(Error::Incomplete)?,
                mainloop_get_api: library
                    .symbol(c"pa_mainloop_get_api")
                    .ok_or(Error::Incomplete)?,
                mainloop_prepare: library
                    .symbol(c"pa_mainloop_prepare")
                    .ok_or(Error::Incomplete)?,
                mainloop_poll: library
                    .symbol(c"pa_mainloop_poll")
                    .ok_or(Error::Incomplete)?,
                mainloop_dispatch: library
                    .symbol(c"pa_mainloop_dispatch")
                    .ok_or(Error::Incomplete)?,
                context_new: library.symbol(c"pa_context_new").ok_or(Error::Incomplete)?,
                context_connect: library
                    .symbol(c"pa_context_connect")
                    .ok_or(Error::Incomplete)?,
                context_disconnect: library
                    .symbol(c"pa_context_disconnect")
                    .ok_or(Error::Incomplete)?,
                context_unref: library
                    .symbol(c"pa_context_unref")
                    .ok_or(Error::Incomplete)?,
                context_get_state: library
                    .symbol(c"pa_context_get_state")
                    .ok_or(Error::Incomplete)?,
                context_errno: library
                    .symbol(c"pa_context_errno")
                    .ok_or(Error::Incomplete)?,
                context_subscribe: library
                    .symbol(c"pa_context_subscribe")
                    .ok_or(Error::Incomplete)?,
                context_set_subscribe_callback: library
                    .symbol(c"pa_context_set_subscribe_callback")
                    .ok_or(Error::Incomplete)?,
                operation_unref: library
                    .symbol(c"pa_operation_unref")
                    .ok_or(Error::Incomplete)?,
                source_info_by_name: library
                    .symbol(c"pa_context_get_source_info_by_name")
                    .ok_or(Error::Incomplete)?,
                sink_info_by_name: library
                    .symbol(c"pa_context_get_sink_info_by_name")
                    .ok_or(Error::Incomplete)?,
                set_sink_mute_by_name: library
                    .symbol(c"pa_context_set_sink_mute_by_name")
                    .ok_or(Error::Incomplete)?,
                sink_info_list: library
                    .symbol(c"pa_context_get_sink_info_list")
                    .ok_or(Error::Incomplete)?,
                stream_new: library.symbol(c"pa_stream_new").ok_or(Error::Incomplete)?,
                stream_connect_record: library
                    .symbol(c"pa_stream_connect_record")
                    .ok_or(Error::Incomplete)?,
                stream_disconnect: library
                    .symbol(c"pa_stream_disconnect")
                    .ok_or(Error::Incomplete)?,
                stream_unref: library
                    .symbol(c"pa_stream_unref")
                    .ok_or(Error::Incomplete)?,
                stream_get_state: library
                    .symbol(c"pa_stream_get_state")
                    .ok_or(Error::Incomplete)?,
                stream_set_read_callback: library
                    .symbol(c"pa_stream_set_read_callback")
                    .ok_or(Error::Incomplete)?,
                stream_set_moved_callback: library
                    .symbol(c"pa_stream_set_moved_callback")
                    .ok_or(Error::Incomplete)?,
                stream_peek: library.symbol(c"pa_stream_peek").ok_or(Error::Incomplete)?,
                stream_drop: library.symbol(c"pa_stream_drop").ok_or(Error::Incomplete)?,
                stream_get_device_name: library
                    .symbol(c"pa_stream_get_device_name")
                    .ok_or(Error::Incomplete)?,
                _library: library,
            })
        }
    }
}
