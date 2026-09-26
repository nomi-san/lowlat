//! The mark on the established path, through the system's QoS service.
//!
//! **A per-socket traffic class is accepted and ignored on this platform**, so
//! the mark is asked per destination instead: an audio-video flow, which the
//! system carries as class selector 5 on the wire and, on a wireless link, in
//! the video access category. Measured on a wireless client beside a
//! 250 Mbit/s upload from the same machine: marked datagrams crossed the air
//! in 1.0 ms at the median and 20 ms at the 99th percentile, against 25 and 181
//! unmarked; on an idle link the two were the same. Voice, the other class an
//! unprivileged process may ask for, was a little better at the tail and
//! stamps the network-control class on the wire, which is not ours to claim.
//!
//! **The flow is asked for with the socket connected.** The service marks an
//! unconnected socket only when it is bound to a specific address, and the
//! media socket is bound to the wildcard so that it hears every address this
//! host holds. Connected to the destination, it is accepted with no
//! destination named, and the flow keeps marking every send there after the
//! socket is disconnected again, whatever source a send claims and whichever
//! interface it leaves by -- measured, as is the wildcard coming back with the
//! disconnect. For the moment between the two calls the system discards what
//! arrives from anywhere else, which the protocol recovers from as it does
//! any loss; once per path, never per datagram.
//!
//! **Loaded at run time, never linked.** A system without the service's
//! library sends unmarked, says so once per session, and loses nothing else.

use core::net::SocketAddr;
use std::io;
use std::sync::OnceLock;

use lowlat_common::dynlib::Library;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Networking::WinSock::{SOCKADDR, SOCKET};

use super::socket::Socket;

/// The interface version the calls below are written against.
#[repr(C)]
struct Version {
    major: u16,
    minor: u16,
}

type Create = unsafe extern "system" fn(*const Version, *mut HANDLE) -> i32;
type Add = unsafe extern "system" fn(HANDLE, SOCKET, *const SOCKADDR, i32, u32, *mut u32) -> i32;
type Remove = unsafe extern "system" fn(HANDLE, SOCKET, u32, u32) -> i32;
type Close = unsafe extern "system" fn(HANDLE) -> i32;

/// The audio-video traffic type.
const AUDIO_VIDEO: i32 = 3;
/// Mark only: no probing of the path and no shaping of the rate.
const NON_ADAPTIVE: u32 = 0x2;

/// The service's four calls, and the library they came from, kept together.
struct Api {
    create: Create,
    add: Add,
    remove: Remove,
    close: Close,
    _library: Library,
}

/// The calls, resolved once per process, or `None` where the library or any
/// of them is missing.
fn api() -> Option<&'static Api> {
    static API: OnceLock<Option<Api>> = OnceLock::new();
    API.get_or_init(|| {
        let library = Library::open(c"qwave.dll")?;
        // SAFETY: each type is the call's signature and calling convention
        // as the service defines it, and each pointer is kept beside the
        // library it came from.
        unsafe {
            Some(Api {
                create: library.symbol(c"QOSCreateHandle")?,
                add: library.symbol(c"QOSAddSocketToFlow")?,
                remove: library.symbol(c"QOSRemoveSocketFromFlow")?,
                close: library.symbol(c"QOSCloseHandle")?,
                _library: library,
            })
        }
    })
    .as_ref()
}

/// One socket's mark: the service handle, and the flow on the destination
/// marked last.
pub(super) struct Marking {
    handle: HANDLE,
    flow: u32,
    to: Option<SocketAddr>,
}

impl core::fmt::Debug for Marking {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Marking")
            .field("flow", &self.flow)
            .field("to", &self.to)
            .finish()
    }
}

// SAFETY: the service handle is a process-wide token, not thread-affine; it
// is used only by the thread that owns the socket it marks.
unsafe impl Send for Marking {}

impl Marking {
    pub(super) fn new() -> Self {
        Self {
            handle: core::ptr::null_mut(),
            flow: 0,
            to: None,
        }
    }

    /// Mark `to`, replacing the flow on whatever was marked before.
    ///
    /// **Never fails the session.** A destination the service will not mark
    /// is sent to unmarked, and said once; it is not asked again until the
    /// path moves.
    pub(super) fn mark(&mut self, socket: &Socket, to: SocketAddr) {
        if self.to == Some(to) {
            return;
        }
        self.to = Some(to);
        let Some(api) = api() else {
            lowlat_common::log_warn!("net: path left unmarked, qos=absent to={}", to);
            return;
        };
        if self.handle.is_null() {
            let version = Version { major: 1, minor: 0 };
            let mut handle: HANDLE = core::ptr::null_mut();
            // SAFETY: a version and a writable handle, which is what the call
            // takes.
            if unsafe { (api.create)(&raw const version, &raw mut handle) } == 0 {
                lowlat_common::log_warn!(
                    "net: path left unmarked, qos=refused to={} err={}",
                    to,
                    io::Error::last_os_error()
                );
                return;
            }
            self.handle = handle;
        }
        if self.flow != 0 {
            // SAFETY: a flow this handle added on this socket, removed once.
            unsafe { (api.remove)(self.handle, socket.raw(), self.flow, 0) };
            self.flow = 0;
        }
        // Connected for the one call; see the module note.
        if let Err(error) = socket.connect(to) {
            lowlat_common::log_warn!("net: path left unmarked, to={} err={}", to, error);
            return;
        }
        let mut flow: u32 = 0;
        // SAFETY: an open handle and our own socket, connected, so no
        // destination is named; a flow id of zero asks for a new flow.
        let added = unsafe {
            (api.add)(
                self.handle,
                socket.raw(),
                core::ptr::null(),
                AUDIO_VIDEO,
                NON_ADAPTIVE,
                &raw mut flow,
            )
        };
        // Read before the disconnect's own call can overwrite it.
        let refused = io::Error::last_os_error();
        if let Err(error) = socket.disconnect() {
            // Still connected, the socket hears this destination and nothing
            // else; nothing here can do better than say so.
            lowlat_common::log_error!(
                "net: socket left connected by the mark, to={} err={}",
                to,
                error
            );
        }
        if added == 0 {
            lowlat_common::log_warn!("net: path left unmarked, to={} err={}", to, refused);
            return;
        }
        self.flow = flow;
        lowlat_common::log_info!("net: path marked, to={} flow={}", to, flow);
    }

    /// The flow on the destination marked last, zero for none.
    #[cfg(test)]
    pub(super) fn flow(&self) -> u32 {
        self.flow
    }
}

impl Drop for Marking {
    fn drop(&mut self) {
        if self.handle.is_null() {
            return;
        }
        if let Some(api) = api() {
            // SAFETY: the handle came from the service and is closed once;
            // closing it removes every flow it added.
            unsafe { (api.close)(self.handle) };
        }
    }
}

/// Whether the service is there to ask, for the tests that need it.
#[cfg(test)]
pub(super) fn available() -> bool {
    api().is_some()
}
