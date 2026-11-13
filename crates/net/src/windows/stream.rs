//! The relay's stream through Winsock: a TCP socket on the loop's own
//! completion port, under its own key (docs/02-io-shell.md 6).
//!
//! **The port is the loop's only wait**, so everything the stream needs to be
//! told arrives through it: the connect; a zero-length receive that completes
//! when bytes have arrived -- the bytes themselves are read with an ordinary
//! receive into the stream's buffer, so nothing the system holds points into
//! storage that moves; and a send from the stream's staging, posted when the
//! socket could not take it at once. The pass's deadline arrives through it
//! as well, as the datagram loop's does (`deadline.rs`).
//!
//! **Teardown takes every operation back** before what it was posted with
//! goes, as the datagram receives' teardown does.

use core::net::SocketAddr;
use std::io;
use std::mem;
use std::os::windows::io::{AsRawSocket, FromRawSocket, OwnedSocket};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{HANDLE, WAIT_TIMEOUT};
use windows_sys::Win32::Networking::WinSock as ws;
use windows_sys::Win32::Storage::FileSystem::SetFileCompletionNotificationModes;
use windows_sys::Win32::System::IO::{
    CancelIoEx, CreateIoCompletionPort, GetQueuedCompletionStatusEx, OVERLAPPED, OVERLAPPED_ENTRY,
};
use windows_sys::Win32::System::WindowsProgramming::FILE_SKIP_COMPLETION_PORT_ON_SUCCESS;

use super::deadline::Deadline;
use super::qos::Marking;
use super::socket::{last_error, len_of, start, to_storage};
use super::wake::Wake;
use super::{KEY_DEADLINE, KEY_STREAM, KEY_WAKE};
use crate::shell::Ready;
use crate::stream::Written;

/// Completions taken in one call: the stream's three operations, the wake
/// and the deadline.
const ENTRIES: usize = 8;

/// How long teardown waits for its cancelled operations to come back.
const RETURN_WAIT: Duration = Duration::from_secs(1);

/// The connect's extension, which the socket provider hands out rather than
/// the library exporting it.
type ConnectEx = unsafe extern "system" fn(
    ws::SOCKET,
    *const ws::SOCKADDR,
    i32,
    *const core::ffi::c_void,
    u32,
    *mut u32,
    *mut OVERLAPPED,
) -> i32;

/// What the system writes into while an operation is posted: boxed once and
/// never moved, so the addresses handed over stay good.
#[repr(C)]
struct Ops {
    connect: OVERLAPPED,
    readable: OVERLAPPED,
    send: OVERLAPPED,
    /// The zero-length receive's buffer, which holds nothing.
    zero: ws::WSABUF,
    /// A posted send's buffer, pointing into the stream's staging.
    sending: ws::WSABUF,
}

/// Which operation a completion belongs to.
#[derive(Clone, Copy)]
enum Op {
    Connect,
    Readable,
    Send,
}

/// The stream's socket, the operations posted on it, and the wake whose port
/// they complete on.
///
/// Fields drop in order after `drop` has taken every operation back: the mark
/// before the socket it names, and the socket and the deadline's timer before
/// the port they are joined and tied to.
pub(crate) struct StreamIo {
    marking: Marking,
    socket: OwnedSocket,
    ops: Box<Ops>,
    deadline: Deadline,
    wake: Wake,
    connecting: bool,
    /// A zero-length receive is posted.
    reading: bool,
    /// Bytes may be waiting: the zero-length receive completed.
    readable: bool,
    /// A send is posted.
    sending: bool,
    /// A posted send came back.
    finished: Option<io::Result<usize>>,
    /// The connect failed, for the next wait to say.
    failed: Option<io::Error>,
    /// A wake taken off the port, for the next wait to report.
    woken: bool,
    /// Operations the system holds.
    posted: usize,
    closing: bool,
    entries: [OVERLAPPED_ENTRY; ENTRIES],
}

impl core::fmt::Debug for StreamIo {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("StreamIo")
            .field("connecting", &self.connecting)
            .field("readable", &self.readable)
            .field("sending", &self.sending)
            .field("posted", &self.posted)
            .field("deadline", &self.deadline)
            .field("marking", &self.marking)
            .finish_non_exhaustive()
    }
}

// SAFETY: the raw pointers inside the boxed operations point at that box or at
// the owning stream's staging, which outlives this (it drops after it); the
// system uses them only while an operation is posted, and teardown takes every
// one back. Nothing here is shared between threads.
unsafe impl Send for StreamIo {}

impl StreamIo {
    /// A dual-stack TCP socket on the wake's port, its connect to `server`
    /// begun.
    pub(crate) fn connect(server: SocketAddr, wake: Wake) -> io::Result<Self> {
        start()?;
        // SAFETY: socket creation with constant arguments. Overlapped, so its
        // operations complete on the port; not inherited by a child process.
        let raw = unsafe {
            ws::WSASocketW(
                i32::from(ws::AF_INET6),
                ws::SOCK_STREAM,
                ws::IPPROTO_TCP,
                core::ptr::null(),
                0,
                ws::WSA_FLAG_OVERLAPPED | ws::WSA_FLAG_NO_HANDLE_INHERIT,
            )
        };
        if raw == ws::INVALID_SOCKET {
            return Err(last_error());
        }
        let mut io = Self {
            marking: Marking::new(),
            // SAFETY: a fresh socket we own and have registered nowhere else;
            // the owned handle closes it on drop.
            socket: unsafe { OwnedSocket::from_raw_socket(raw as u64) },
            // SAFETY: plain system structures, for which zero is valid.
            ops: Box::new(unsafe { mem::zeroed::<Ops>() }),
            deadline: Deadline::new(),
            wake,
            connecting: false,
            reading: false,
            readable: false,
            sending: false,
            finished: None,
            failed: None,
            woken: false,
            posted: 0,
            closing: false,
            entries: [OVERLAPPED_ENTRY::default(); ENTRIES],
        };
        io.set_int(ws::IPPROTO_IPV6, ws::IPV6_V6ONLY, 0)?;
        // A pass's messages are written together already; nothing waits to
        // gather more. No buffer sizes: the stack tunes a stream's own.
        io.set_int(ws::IPPROTO_TCP, ws::TCP_NODELAY, 1)?;
        io.bind_any()?;
        io.set_nonblocking()?;
        io.join_port()?;
        let connect = connect_ex_of(raw)?;
        let (addr, len) = to_storage(server);
        let mut sent: u32 = 0;
        // SAFETY: a bound overlapped socket, a sockaddr_in6 and its length, no
        // data, and the connect's own overlapped structure, idle and pinned in
        // the boxed operations.
        let ok = unsafe {
            connect(
                raw,
                (&raw const addr).cast(),
                len,
                core::ptr::null(),
                0,
                &raw mut sent,
                &raw mut io.ops.connect,
            )
        };
        if ok != 0 {
            // Completed at once; with completion on success skipped, no entry
            // follows.
            io.connected_now()?;
        } else {
            let error = last_error();
            if error.raw_os_error() != Some(ws::WSA_IO_PENDING) {
                return Err(error);
            }
            io.connecting = true;
            io.posted += 1;
        }
        Ok(io)
    }

    pub(crate) fn wake(&self) -> &Wake {
        &self.wake
    }

    pub(crate) fn take_wake(&mut self) -> io::Result<bool> {
        self.wake.take()
    }

    pub(crate) fn connected(&self) -> bool {
        !self.connecting
    }

    /// A send is posted: its buffer must not change until it is finished.
    pub(crate) fn sending(&self) -> bool {
        self.sending
    }

    /// A posted send that came back, with the bytes it carried.
    pub(crate) fn finished(&mut self) -> Option<io::Result<usize>> {
        self.finished.take()
    }

    /// Mark the stream's flow to `to`, once it is connected.
    pub(crate) fn mark(&mut self, to: SocketAddr) {
        if self.connected() {
            self.marking.mark_connected(self.raw(), to);
        }
    }

    /// Wait for the stream -- the connect, bytes, a send's return -- or the
    /// wake, or the deadline, armed as a timer on the port; at once when
    /// `now`, or while something is already in hand. The send's return
    /// arrives on the port whether or not anything is `writing`.
    pub(crate) fn wait(&mut self, timeout_ms: f64, _writing: bool, now: bool) -> io::Result<Ready> {
        if let Some(error) = self.failed.take() {
            return Err(error);
        }
        let in_hand = now || self.readable || self.woken || self.finished.is_some();
        let timeout = if in_hand {
            0
        } else {
            self.deadline.arm(self.wake.port(), timeout_ms)
        };
        self.collect(timeout)?;
        if let Some(error) = self.failed.take() {
            return Err(error);
        }
        Ok(Ready {
            socket: self.readable || self.finished.is_some() || now,
            wake: mem::take(&mut self.woken),
        })
    }

    /// Read what has arrived into `into`: zero is the stream's end, and
    /// `WouldBlock` that nothing is there, with the zero-length receive posted
    /// again to say when something is.
    pub(crate) fn read(&mut self, into: &mut [u8]) -> io::Result<usize> {
        if self.connecting {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        let len = i32::try_from(into.len()).unwrap_or(i32::MAX);
        // SAFETY: a writable buffer of at least `len` bytes; the socket is
        // non-blocking, so the call returns at once.
        let rc = unsafe { ws::recv(self.raw(), into.as_mut_ptr(), len, 0) };
        if let Ok(read) = usize::try_from(rc) {
            if read > 0 && read < into.len() {
                // Short of the room, so nothing more was there: the zero-length
                // receive goes out now, rather than after one more read to be
                // told so, and completes at once if more has come since.
                self.readable = false;
                self.post_read();
            }
            return Ok(read);
        }
        let error = last_error();
        if error.raw_os_error() == Some(ws::WSAEWOULDBLOCK) {
            self.readable = false;
            self.post_read();
            return Err(io::ErrorKind::WouldBlock.into());
        }
        Err(error)
    }

    /// Write `from`: taken at once, or posted and held until it comes back on
    /// the port, its buffer left alone until then.
    pub(crate) fn write(&mut self, from: &[u8]) -> io::Result<Written> {
        if self.sending {
            return Ok(Written::Pending);
        }
        let len = u32::try_from(from.len()).map_err(|_| io::Error::other("a send past u32"))?;
        self.ops.sending = ws::WSABUF {
            len,
            buf: from.as_ptr().cast_mut(),
        };
        self.ops.send = OVERLAPPED::default();
        let mut sent: u32 = 0;
        // SAFETY: the buffer describes the stream's staging, which the stream
        // leaves unchanged while a send is posted and drops after this; the
        // send's own overlapped structure, idle and pinned in the boxed
        // operations.
        let rc = unsafe {
            ws::WSASend(
                self.raw(),
                &raw mut self.ops.sending,
                1,
                &raw mut sent,
                0,
                &raw mut self.ops.send,
                None,
            )
        };
        if rc == 0 {
            // Taken at once; no entry follows.
            return Ok(Written::Now(usize::try_from(sent).unwrap_or(0)));
        }
        let error = last_error();
        if error.raw_os_error() == Some(ws::WSA_IO_PENDING) {
            self.sending = true;
            self.posted += 1;
            return Ok(Written::Pending);
        }
        Err(error)
    }

    /// The socket knows itself connected once told; then the zero-length
    /// receive goes out.
    fn connected_now(&mut self) -> io::Result<()> {
        // SAFETY: an option with no value, on our own socket.
        let rc = unsafe {
            ws::setsockopt(
                self.raw(),
                ws::SOL_SOCKET,
                ws::SO_UPDATE_CONNECT_CONTEXT,
                core::ptr::null(),
                0,
            )
        };
        if rc != 0 {
            return Err(last_error());
        }
        self.connecting = false;
        self.post_read();
        Ok(())
    }

    /// Post the zero-length receive, unless one is posted. One that completes
    /// at once, or fails at once, leaves the bytes or the error for the read.
    fn post_read(&mut self) {
        if self.reading || self.closing {
            return;
        }
        self.ops.readable = OVERLAPPED::default();
        let mut bytes: u32 = 0;
        let mut flags: u32 = 0;
        // SAFETY: a zero-length buffer and the receive's own overlapped
        // structure, idle and pinned in the boxed operations.
        let rc = unsafe {
            ws::WSARecv(
                self.raw(),
                &raw mut self.ops.zero,
                1,
                &raw mut bytes,
                &raw mut flags,
                &raw mut self.ops.readable,
                None,
            )
        };
        if rc == 0 {
            self.readable = true;
            return;
        }
        if last_error().raw_os_error() == Some(ws::WSA_IO_PENDING) {
            self.reading = true;
            self.posted += 1;
        } else {
            self.readable = true;
        }
    }

    /// Take what the port holds, waiting up to `timeout` milliseconds.
    fn collect(&mut self, timeout: u32) -> io::Result<()> {
        let mut removed: u32 = 0;
        // SAFETY: the entry array is ENTRIES long and writable, and the port
        // is open for as long as the wake is.
        let ok = unsafe {
            GetQueuedCompletionStatusEx(
                self.wake.port(),
                self.entries.as_mut_ptr(),
                u32::try_from(ENTRIES).unwrap_or(0),
                &raw mut removed,
                timeout,
                0,
            )
        };
        if ok == 0 {
            let error = io::Error::last_os_error();
            return match error.raw_os_error() {
                Some(code) if u32::try_from(code) == Ok(WAIT_TIMEOUT) => Ok(()),
                _ => Err(error),
            };
        }
        let count = usize::try_from(removed).unwrap_or(0).min(ENTRIES);
        for at in 0..count {
            let Some(entry) = self.entries.get(at).copied() else {
                break;
            };
            match entry.lpCompletionKey {
                KEY_WAKE => self.woken = true,
                KEY_STREAM => self.completed(entry.lpOverlapped),
                KEY_DEADLINE => self.deadline.fired(),
                _ => {}
            }
        }
        Ok(())
    }

    /// One operation came back.
    fn completed(&mut self, overlapped: *mut OVERLAPPED) {
        let op = if overlapped == &raw mut self.ops.connect {
            Op::Connect
        } else if overlapped == &raw mut self.ops.readable {
            Op::Readable
        } else if overlapped == &raw mut self.ops.send {
            Op::Send
        } else {
            return;
        };
        self.posted = self.posted.saturating_sub(1);
        let result = self.result_of(overlapped);
        if self.closing {
            return;
        }
        match op {
            // A failed connect never connected: it stays connecting, for good.
            Op::Connect => {
                if let Err(error) = result.and_then(|_| self.connected_now()) {
                    self.failed = Some(error);
                }
            }
            // Bytes, or an error the read will find.
            Op::Readable => {
                self.reading = false;
                self.readable = true;
            }
            // The bytes the completion counts, not the bytes asked for.
            Op::Send => {
                self.sending = false;
                self.finished = Some(result.map(|sent| usize::try_from(sent).unwrap_or(0)));
            }
        }
    }

    /// How a completed operation went.
    fn result_of(&self, overlapped: *mut OVERLAPPED) -> io::Result<u32> {
        let mut bytes: u32 = 0;
        let mut flags: u32 = 0;
        // SAFETY: a completed operation's own overlapped structure, on our
        // socket; nothing waits.
        let ok = unsafe {
            ws::WSAGetOverlappedResult(self.raw(), overlapped, &raw mut bytes, 0, &raw mut flags)
        };
        if ok == 0 {
            Err(last_error())
        } else {
            Ok(bytes)
        }
    }

    fn raw(&self) -> ws::SOCKET {
        // A socket handle is pointer sized on every Windows target, so the
        // conversion from the standard library's 64-bit form is exact.
        usize::try_from(self.socket.as_raw_socket()).unwrap_or(ws::INVALID_SOCKET)
    }

    fn handle(&self) -> HANDLE {
        core::ptr::without_provenance_mut(self.raw())
    }

    /// Bound to the wildcard, which the connect's extension requires.
    fn bind_any(&self) -> io::Result<()> {
        let addr = ws::SOCKADDR_IN6 {
            sin6_family: ws::AF_INET6,
            ..Default::default()
        };
        // SAFETY: a sockaddr_in6 and its exact length.
        let rc = unsafe {
            ws::bind(
                self.raw(),
                (&raw const addr).cast(),
                len_of::<ws::SOCKADDR_IN6>(),
            )
        };
        if rc != 0 {
            return Err(last_error());
        }
        Ok(())
    }

    fn set_nonblocking(&self) -> io::Result<()> {
        let mut on: u32 = 1;
        // SAFETY: the command takes a pointer to one u32, which is passed.
        if unsafe { ws::ioctlsocket(self.raw(), ws::FIONBIO, &raw mut on) } != 0 {
            return Err(last_error());
        }
        Ok(())
    }

    /// Join the wake's port under the stream's key, completion on success
    /// skipped.
    fn join_port(&self) -> io::Result<()> {
        let handle = self.handle();
        // SAFETY: joins our own socket to the loop's port. The port outlives
        // the socket: the socket drops first.
        let port = unsafe { CreateIoCompletionPort(handle, self.wake.port(), KEY_STREAM, 0) };
        if port.is_null() {
            return Err(io::Error::last_os_error());
        }
        let skip = u8::try_from(FILE_SKIP_COMPLETION_PORT_ON_SUCCESS)
            .map_err(|_| io::Error::other("completion mode outside its field"))?;
        // SAFETY: a mode change on our own socket.
        if unsafe { SetFileCompletionNotificationModes(handle, skip) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn set_int(&self, level: i32, name: i32, value: i32) -> io::Result<()> {
        // SAFETY: an int option and its exact size.
        let rc = unsafe {
            ws::setsockopt(
                self.raw(),
                level,
                name,
                (&raw const value).cast(),
                len_of::<i32>(),
            )
        };
        if rc != 0 {
            return Err(last_error());
        }
        Ok(())
    }
}

impl Drop for StreamIo {
    fn drop(&mut self) {
        self.closing = true;
        if self.posted == 0 {
            return;
        }
        // SAFETY: cancels every operation posted on our own socket; each still
        // comes back through the port, cancelled, and is counted off below.
        unsafe { CancelIoEx(self.handle(), core::ptr::null()) };
        let started = Instant::now();
        while self.posted > 0 && started.elapsed() < RETURN_WAIT {
            if self.collect(10).is_err() {
                break;
            }
        }
        if self.posted > 0 {
            // The system still holds operations into this storage; freeing it
            // would let a late completion write into freed memory.
            lowlat_common::log_error!(
                "net: stream operations not returned at close, posted={}",
                self.posted
            );
            // SAFETY: plain system structures, for which zero is valid.
            let replacement = Box::new(unsafe { mem::zeroed::<Ops>() });
            let _kept: &'static mut Ops = Box::leak(mem::replace(&mut self.ops, replacement));
        }
    }
}

/// The connect's extension, from the socket's provider.
fn connect_ex_of(socket: ws::SOCKET) -> io::Result<ConnectEx> {
    let id = ws::WSAID_CONNECTEX;
    let mut function: Option<ConnectEx> = None;
    let mut returned: u32 = 0;
    // SAFETY: the extension lookup reads one identifier and writes one
    // function pointer; both buffers and their lengths are exact, and the call
    // is synchronous.
    let rc = unsafe {
        ws::WSAIoctl(
            socket,
            ws::SIO_GET_EXTENSION_FUNCTION_POINTER,
            (&raw const id).cast(),
            u32::try_from(mem::size_of_val(&id)).unwrap_or(0),
            (&raw mut function).cast(),
            u32::try_from(mem::size_of_val(&function)).unwrap_or(0),
            &raw mut returned,
            core::ptr::null_mut(),
            None,
        )
    };
    if rc != 0 {
        return Err(last_error());
    }
    function.ok_or_else(|| io::Error::other("the provider has no connect extension"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::net::Ipv6Addr;

    /// **The stream's deadline is the timer's too**: a quiet wait on a
    /// connected stream ends on it, not before it and not on the system's
    /// tick after it, as the datagram loop's does (`io.rs`). The middle of
    /// nine waits, for the reason given there.
    #[test]
    fn a_quiet_wait_ends_on_its_deadline() {
        let listener = std::net::TcpListener::bind((Ipv6Addr::LOCALHOST, 0)).expect("listener");
        let server = listener.local_addr().expect("addr");
        let mut io = StreamIo::connect(server, Wake::new().expect("wake")).expect("connect");
        let (_peer, _) = listener.accept().expect("accept");
        let started = Instant::now();
        while !io.connected() && started.elapsed() < Duration::from_secs(2) {
            io.wait(50.0, false, false).expect("wait");
        }
        assert!(io.connected(), "the connect never came back");
        assert!(io.deadline.timed(), "the loop's timer was not made");

        let mut waits: Vec<f64> = (0..9)
            .map(|_| {
                let started = Instant::now();
                let ready = io.wait(5.0, false, false).expect("wait");
                assert!(!ready.socket && !ready.wake, "the wait was not quiet");
                started.elapsed().as_secs_f64() * 1000.0
            })
            .collect();
        waits.sort_by(f64::total_cmp);
        assert!(
            waits[4] >= 5.0,
            "the waits ended before their deadline: {waits:?}"
        );
        assert!(
            waits[4] < 12.0,
            "the waits ran on to the system's tick, in ms: {waits:?}"
        );
    }
}
