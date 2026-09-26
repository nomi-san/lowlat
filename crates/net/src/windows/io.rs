//! The completion port: the wait, the receive, and the storage the system
//! writes into, owned together for a session.
//!
//! **The wait is the receive.** Receives are posted into slots ahead of time
//! and the port hands back the ones that completed, a whole pool's worth at a
//! time. A single outstanding receive and a readiness poll loses a keyframe
//! burst outright, so the pool is not an optimisation.
//!
//! **A slot belongs to the system while its receive is posted.** The pool is
//! allocated once and never moves. A slot goes back to the system when the
//! batch it was handed out in is done with, which the next mutable call on
//! this object proves, since the batch is borrowed from it; and teardown
//! cancels every posted receive and waits for each to come back before the
//! storage may go.
//!
//! **A receive that completes at once completes here, not on the port.** The
//! socket skips the port on synchronous success, so a burst already queued
//! when a slot is posted costs no round trip through the wait: the slot joins
//! the datagrams in hand, and a wait does not block while any are.
//!
//! **The port carries the wake as well**, so an entry taken outside a wait --
//! a drain collecting what completed since -- can be the wake. It is kept and
//! reported by the next wait, which then does not block; dropping it would
//! leave the work it announced to sit out the timeout.

use core::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::io;
use std::mem;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{HANDLE, WAIT_TIMEOUT};
use windows_sys::Win32::Networking::WinSock as ws;
use windows_sys::Win32::Storage::FileSystem::SetFileCompletionNotificationModes;
use windows_sys::Win32::System::IO::{
    CancelIoEx, CreateIoCompletionPort, GetQueuedCompletionStatusEx, OVERLAPPED, OVERLAPPED_ENTRY,
};
use windows_sys::Win32::System::WindowsProgramming::FILE_SKIP_COMPLETION_PORT_ON_SUCCESS;

use super::qos::Marking;
use super::socket::{Socket, from_storage, last_error, len_of};
use super::wake::Wake;
use super::{KEY_RECV, KEY_WAKE, control};
use crate::shell::{MIN_WAIT_MS, Ready};
use crate::socket::RECV_SLOT;

/// Receives kept posted, and the most completions taken in one call.
const SLOTS: usize = 256;

/// Room for one datagram's control messages: one packet information, well
/// under this in either family.
const CONTROL_LEN: usize = 64;

/// How long teardown waits for its cancelled receives to come back.
const RETURN_WAIT: Duration = Duration::from_secs(1);

/// Control message storage, aligned as the system requires.
#[repr(align(8))]
#[derive(Clone, Copy)]
struct Control([u8; CONTROL_LEN]);

/// The message receive, which the socket provider hands out rather than the
/// library exporting it.
type Receive = unsafe extern "system" fn(
    ws::SOCKET,
    *mut ws::WSAMSG,
    *mut u32,
    *mut OVERLAPPED,
    ws::LPWSAOVERLAPPED_COMPLETION_ROUTINE,
) -> i32;

/// One posted receive and everything the system writes for it.
///
/// The message points into the slot itself, so a slot must not move once its
/// pointers are set: the pool is boxed once and never resized.
#[repr(C)]
struct Slot {
    /// First, so the structure a completion names is the slot's own address.
    overlapped: OVERLAPPED,
    msg: ws::WSAMSG,
    buffer: ws::WSABUF,
    name: ws::SOCKADDR_STORAGE,
    control: Control,
    bytes: [u8; RECV_SLOT],
    /// Bytes received, once completed.
    len: usize,
}

/// The socket, the wake and the receive storage, owned together for a
/// session.
///
/// Fields drop in order after `drop` has taken every receive back: the mark
/// before the socket it names, and the socket before the port it is joined to.
pub(crate) struct Io {
    marking: Marking,
    socket: Socket,
    wake: Wake,
    receive: Option<Receive>,
    slots: Box<[Slot]>,
    /// Slots whose receive completed, in the order they did, not yet handed
    /// out.
    done: Vec<u16>,
    /// The slots the last drain handed out, posted again at the next call.
    batch: Vec<u16>,
    entries: Box<[OVERLAPPED_ENTRY]>,
    /// Receives the system holds.
    posted: usize,
    /// A wake taken off the port outside a wait, for the next wait to report.
    woken: bool,
    /// Why the receive could not be set up at construction, reported by every
    /// call after it rather than by a constructor that cannot fail.
    broken: Option<i32>,
    closing: bool,
}

impl core::fmt::Debug for Io {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Io")
            .field("socket", &self.socket)
            .field("posted", &self.posted)
            .field("in_hand", &self.done.len())
            .field("batch", &self.batch.len())
            .field("marking", &self.marking)
            .finish()
    }
}

// SAFETY: the raw pointers inside the slots point only into this object's own
// boxed pool, which moves with it and is never aliased elsewhere; the system
// writes through them only while a receive is posted, and that ends before the
// pool is freed. Nothing here is shared between threads without the usual
// borrowing rules.
unsafe impl Send for Io {}

impl Io {
    /// Allocate the receive storage and post every slot. Once per session,
    /// never on a data path.
    ///
    /// A failure to set the receive up is kept and returned by the first call
    /// that needs it, so the signature is the same on every platform.
    pub(crate) fn new(socket: Socket, wake: Wake) -> Self {
        let mut io = Self {
            marking: Marking::new(),
            socket,
            wake,
            receive: None,
            slots: pool(),
            done: Vec::with_capacity(SLOTS),
            batch: Vec::with_capacity(SLOTS),
            entries: vec![OVERLAPPED_ENTRY::default(); SLOTS].into_boxed_slice(),
            posted: 0,
            woken: false,
            broken: None,
            closing: false,
        };
        if let Err(error) = io.arm() {
            lowlat_common::log_error!("net: receive not set up, err={}", error);
            io.broken = Some(error.raw_os_error().unwrap_or(0));
        }
        io
    }

    pub(crate) fn socket(&self) -> &Socket {
        &self.socket
    }

    pub(crate) fn wake(&self) -> &Wake {
        &self.wake
    }

    /// Join the loop's port, skip it on synchronous success, and post the pool.
    fn arm(&mut self) -> io::Result<()> {
        self.receive = Some(receive_of(&self.socket)?);
        let handle = self.handle();
        // SAFETY: joins our own socket to the loop's port under the receive
        // key. The port outlives the socket: the socket drops first, and the
        // port is shared with every producer's end of the wake.
        let port = unsafe { CreateIoCompletionPort(handle, self.wake.port(), KEY_RECV, 0) };
        if port.is_null() {
            return Err(io::Error::last_os_error());
        }
        let skip = u8::try_from(FILE_SKIP_COMPLETION_PORT_ON_SUCCESS)
            .map_err(|_| io::Error::other("completion mode outside its field"))?;
        // SAFETY: a mode change on our own socket.
        if unsafe { SetFileCompletionNotificationModes(handle, skip) } == 0 {
            return Err(io::Error::last_os_error());
        }
        for index in 0..SLOTS {
            self.post(index)?;
        }
        Ok(())
    }

    /// The socket as the handle the port and the cancel take.
    fn handle(&self) -> HANDLE {
        core::ptr::without_provenance_mut(self.socket.raw())
    }

    /// Wait for a datagram or the wake, or the deadline.
    ///
    /// Reports which of the two the port spoke about, so the pass can leave
    /// the quiet one alone. Does not block while a datagram or a wake is
    /// already in hand.
    pub(crate) fn wait(&mut self, timeout_ms: f64) -> io::Result<Ready> {
        self.check()?;
        self.recycle()?;
        let timeout = if self.done.is_empty() && !self.woken {
            // Rounded up, never truncated: a fractional wait rounded down
            // wakes just before the armed deadline, and the pass then pays a
            // second wake to act on it.
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "clamped to the wait bounds by the caller"
            )]
            let ms = timeout_ms.max(MIN_WAIT_MS).ceil() as u32;
            ms
        } else {
            0
        };
        self.collect(timeout)?;
        Ok(Ready {
            socket: !self.done.is_empty(),
            wake: mem::take(&mut self.woken),
        })
    }

    /// Consume the pending wake, if any. See [`Wake::take`] for why this
    /// comes before the application is pulled.
    pub(crate) fn take_wake(&mut self) -> io::Result<bool> {
        self.wake.take()
    }

    /// Hand out every datagram in hand as one batch.
    ///
    /// Returns how many. Zero means nothing has arrived since; the caller
    /// stops when it sees a batch short of the pool, because a whole pool
    /// means more may be queued behind it.
    pub(crate) fn drain(&mut self) -> io::Result<usize> {
        self.check()?;
        self.recycle()?;
        if self.done.len() < SLOTS {
            self.collect(0)?;
        }
        mem::swap(&mut self.batch, &mut self.done);
        Ok(self.batch.len())
    }

    /// True when the last drain handed out the whole pool, so more may be
    /// queued.
    pub(crate) fn saturated(&self) -> bool {
        self.batch.len() == SLOTS
    }

    /// The datagrams from the last drain: the address each came from, the
    /// local address it arrived at, and the bytes.
    ///
    /// A v4-mapped source is reported as IPv4, structurally. The local
    /// address is `None` when the system attached no packet information,
    /// which a caller treats as "could not say" rather than an error.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (SocketAddr, Option<IpAddr>, &[u8])> {
        self.batch.iter().filter_map(move |&index| {
            let slot = self.slots.get(usize::from(index))?;
            let bytes = slot.bytes.get(..slot.len)?;
            let from = from_storage(&slot.name)?;
            Some((from, local_of(slot), bytes))
        })
    }

    /// Mark the established path, so what the socket sends there is carried
    /// in the platform's traffic class. See [`Marking::mark`].
    pub(crate) fn mark(&mut self, to: SocketAddr) {
        self.marking.mark(&self.socket, to);
    }

    fn check(&self) -> io::Result<()> {
        match self.broken {
            Some(code) => Err(io::Error::from_raw_os_error(code)),
            None => Ok(()),
        }
    }

    /// Post again the slots the last drain handed out. The caller is done
    /// with them: this runs only from a call that borrows the object mutably.
    fn recycle(&mut self) -> io::Result<()> {
        let mut batch = mem::take(&mut self.batch);
        let result = batch
            .iter()
            .try_for_each(|&index| self.post(usize::from(index)));
        // Emptied and handed back, so its storage is reused rather than
        // grown.
        batch.clear();
        self.batch = batch;
        result
    }

    /// Hand a slot to the system for the next datagram.
    ///
    /// A receive that completes at once is in hand when this returns. One the
    /// system fails at once -- a datagram too large for the slot, a report of
    /// an unreachable peer -- consumed nothing worth keeping, and the slot is
    /// posted again.
    fn post(&mut self, index: usize) -> io::Result<()> {
        let Some(receive) = self.receive else {
            return Ok(());
        };
        let socket = self.socket.raw();
        loop {
            let Some(slot) = self.slots.get_mut(index) else {
                return Ok(());
            };
            // The address and control lengths are in and out: the system
            // overwrites each with what it wrote, so a slot posted again
            // without resetting them truncates both.
            slot.overlapped = OVERLAPPED::default();
            slot.msg.namelen = len_of::<ws::SOCKADDR_STORAGE>();
            slot.msg.Control.len = u32::try_from(CONTROL_LEN).unwrap_or(0);
            slot.msg.dwFlags = 0;
            slot.len = 0;
            let mut bytes: u32 = 0;
            // SAFETY: the message describes storage inside the slot, which is
            // pinned for the socket's life -- the pool never moves, and
            // teardown takes every posted receive back before freeing it --
            // and the overlapped structure is the slot's own and idle, since
            // this slot is not posted.
            let rc = unsafe {
                receive(
                    socket,
                    &raw mut slot.msg,
                    &raw mut bytes,
                    &raw mut slot.overlapped,
                    None,
                )
            };
            if rc == 0 {
                slot.len = usize::try_from(bytes).unwrap_or(0);
                self.done.push(u16::try_from(index).unwrap_or(u16::MAX));
                return Ok(());
            }
            let error = last_error();
            match error.raw_os_error() {
                Some(ws::WSA_IO_PENDING) => {
                    self.posted += 1;
                    return Ok(());
                }
                Some(ws::WSAEMSGSIZE | ws::WSAECONNRESET | ws::WSAENETRESET) => {}
                _ => return Err(error),
            }
        }
    }

    /// Take what the port holds, waiting up to `timeout` milliseconds for the
    /// first entry.
    fn collect(&mut self, timeout: u32) -> io::Result<()> {
        let mut removed: u32 = 0;
        // SAFETY: the entry array is SLOTS long and writable, and the port is
        // open for as long as the wake is.
        let ok = unsafe {
            GetQueuedCompletionStatusEx(
                self.wake.port(),
                self.entries.as_mut_ptr(),
                u32::try_from(SLOTS).unwrap_or(0),
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
        let count = usize::try_from(removed).unwrap_or(0).min(SLOTS);
        for at in 0..count {
            let Some(entry) = self.entries.get(at).copied() else {
                break;
            };
            match entry.lpCompletionKey {
                KEY_WAKE => self.woken = true,
                KEY_RECV => self.completed(&entry)?,
                _ => {}
            }
        }
        Ok(())
    }

    /// One receive came back.
    fn completed(&mut self, entry: &OVERLAPPED_ENTRY) -> io::Result<()> {
        let Some(index) = self.index_of(entry.lpOverlapped) else {
            return Ok(());
        };
        self.posted = self.posted.saturating_sub(1);
        // The status the receive finished with. Anything but success -- a
        // datagram cut to the slot, an unreachable report, a cancel at
        // teardown -- carries nothing to hand out.
        if entry.Internal == 0 {
            if let Some(slot) = self.slots.get_mut(index) {
                slot.len = usize::try_from(entry.dwNumberOfBytesTransferred).unwrap_or(0);
            }
            self.done.push(u16::try_from(index).unwrap_or(u16::MAX));
            return Ok(());
        }
        if self.closing {
            return Ok(());
        }
        self.post(index)
    }

    /// Which slot an overlapped structure belongs to, or `None` for one that is
    /// not ours.
    fn index_of(&self, overlapped: *mut OVERLAPPED) -> Option<usize> {
        let at = overlapped.addr().checked_sub(self.slots.as_ptr().addr())?;
        let size = mem::size_of::<Slot>();
        let index = at / size;
        (at % size == 0 && index < self.slots.len()).then_some(index)
    }
}

impl Drop for Io {
    fn drop(&mut self) {
        self.closing = true;
        if self.posted == 0 {
            return;
        }
        // SAFETY: cancels every receive posted on our own socket; each still
        // comes back through the port, cancelled, and is counted off below.
        unsafe { CancelIoEx(self.handle(), core::ptr::null()) };
        let started = Instant::now();
        while self.posted > 0 && started.elapsed() < RETURN_WAIT {
            if self.collect(10).is_err() {
                break;
            }
        }
        if self.posted > 0 {
            // The system still holds receives into this storage. Freeing it
            // would let a late completion write into freed memory, so it is
            // kept for the life of the process instead.
            lowlat_common::log_error!(
                "net: receives not returned at close, posted={}",
                self.posted
            );
            let _kept: &'static mut [Slot] = Box::leak(mem::take(&mut self.slots));
        }
    }
}

/// The pool, with every slot's message pointing into the slot itself.
fn pool() -> Box<[Slot]> {
    let mut slots: Box<[Slot]> = (0..SLOTS)
        .map(|_| {
            // SAFETY: every field is plain data -- system structures, bytes
            // and a length -- for which zero is a valid value.
            unsafe { mem::zeroed::<Slot>() }
        })
        .collect();
    // The pointers are set once the pool is in its final place, since they
    // point into it.
    for slot in slots.iter_mut() {
        slot.buffer = ws::WSABUF {
            len: u32::try_from(RECV_SLOT).unwrap_or(0),
            buf: slot.bytes.as_mut_ptr(),
        };
        slot.msg = ws::WSAMSG {
            name: (&raw mut slot.name).cast(),
            namelen: len_of::<ws::SOCKADDR_STORAGE>(),
            lpBuffers: &raw mut slot.buffer,
            dwBufferCount: 1,
            Control: ws::WSABUF {
                len: u32::try_from(CONTROL_LEN).unwrap_or(0),
                buf: slot.control.0.as_mut_ptr(),
            },
            dwFlags: 0,
        };
    }
    slots
}

/// The message receive, from the socket's provider.
fn receive_of(socket: &Socket) -> io::Result<Receive> {
    let id = ws::WSAID_WSARECVMSG;
    let mut function: ws::LPFN_WSARECVMSG = None;
    let mut returned: u32 = 0;
    // SAFETY: the extension lookup reads one identifier and writes one
    // function pointer; both buffers and their lengths are exact, and the
    // call is synchronous.
    let rc = unsafe {
        ws::WSAIoctl(
            socket.raw(),
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
    function.ok_or_else(|| io::Error::other("the provider has no message receive"))
}

/// The local address a datagram arrived at, from its control messages.
///
/// The v4 answer arrives at the v4 level and the v6 answer at the v6 level,
/// each as the arrival address; a v4 arrival reported v4-mapped is a v4
/// address.
fn local_of(slot: &Slot) -> Option<IpAddr> {
    let len = usize::try_from(slot.msg.Control.len).ok()?.min(CONTROL_LEN);
    let control = slot.control.0.get(..len)?;
    for (level, kind, data) in control::messages(control) {
        if level == ws::IPPROTO_IP && kind == ws::IP_PKTINFO {
            let info: ws::IN_PKTINFO = control::read(data)?;
            // SAFETY: every view of the address union is plain bytes; the
            // word is in network order, so its bytes are the octets.
            let word = unsafe { info.ipi_addr.S_un.S_addr };
            return Some(IpAddr::V4(Ipv4Addr::from(word.to_ne_bytes())));
        }
        if level == ws::IPPROTO_IPV6 && kind == ws::IPV6_PKTINFO {
            let info: ws::IN6_PKTINFO = control::read(data)?;
            // SAFETY: as above, for the v6 union.
            let ip = Ipv6Addr::from(unsafe { info.ipi6_addr.u.Byte });
            return Some(match ip.to_ipv4_mapped() {
                Some(v4) => IpAddr::V4(v4),
                None => IpAddr::V6(ip),
            });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::net::Ipv6Addr;

    fn receiver() -> (Io, SocketAddr) {
        let socket = Socket::open(0).expect("open receiver");
        let mut to = socket.local_addr().expect("addr");
        to.set_ip(IpAddr::V6(Ipv6Addr::LOCALHOST));
        (Io::new(socket, Wake::new().expect("wake")), to)
    }

    /// **A wake a drain takes off the port is reported by the next wait**,
    /// which does not block. The port carries the wake beside the receives,
    /// so a drain collecting what completed since the wait can be the one to
    /// take it; forgotten there, the work it announced sits out the timeout.
    #[test]
    fn a_wake_taken_by_a_drain_is_not_lost() {
        let (mut io, _) = receiver();
        let producer = io.wake().handle().expect("handle");

        producer.notify().expect("notify");
        assert_eq!(io.drain().expect("drain"), 0, "nothing was sent");

        let started = Instant::now();
        let ready = io.wait(1000.0).expect("wait");
        assert!(ready.wake, "the wake the drain took was not reported");
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "the wait blocked on a wake already in hand: {:?}",
            started.elapsed()
        );
        assert!(io.take_wake().expect("take"), "the wake was not armed");
    }

    /// A burst larger than the pool arrives whole: what the posted receives
    /// cannot hold waits in the socket's buffer and completes as each slot is
    /// posted again.
    #[test]
    fn a_burst_larger_than_the_pool_arrives_whole() {
        let sender = Socket::open(0).expect("open sender");
        let (mut io, to) = receiver();

        let burst = SLOTS * 3;
        for index in 0..burst {
            let payload = (index as u32).to_le_bytes();
            sender.send_to(&payload, to).expect("send");
        }

        let mut seen = Vec::new();
        let started = Instant::now();
        while seen.len() < burst && started.elapsed() < Duration::from_secs(5) {
            if !io.wait(100.0).expect("wait").socket {
                continue;
            }
            loop {
                let got = io.drain().expect("drain");
                for (_, _, bytes) in io.iter() {
                    seen.push(u32::from_le_bytes(bytes.try_into().expect("four bytes")));
                }
                if got == 0 || !io.saturated() {
                    break;
                }
            }
        }
        assert_eq!(seen.len(), burst, "the burst did not arrive whole");
        assert!(
            seen.windows(2).all(|pair| pair[0] < pair[1]),
            "datagrams arrived out of order"
        );
    }

    /// Teardown with every receive posted returns promptly: each cancelled
    /// receive comes back through the port and is counted off.
    #[test]
    fn teardown_takes_every_receive_back() {
        let (io, _) = receiver();
        assert_eq!(io.posted, SLOTS, "the pool was not posted");
        let started = Instant::now();
        drop(io);
        assert!(
            started.elapsed() < RETURN_WAIT,
            "teardown waited out its bound: {:?}",
            started.elapsed()
        );
    }

    /// Marking the path never fails the session; where the service is there
    /// it gives the destination a flow; and the socket still hears every
    /// address afterwards, with the pool's receives posted throughout -- the
    /// mark connects it for a moment, and a disconnect that did not put the
    /// wildcard back would leave it hearing the marked destination alone.
    #[test]
    fn the_path_is_marked_and_the_socket_still_hears_everything() {
        let sender = Socket::open(0).expect("sender");
        let (mut io, local) = receiver();
        let to = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)), 9);
        io.mark(to);
        if super::super::qos::available() {
            assert_ne!(io.marking.flow(), 0, "the service gave the path no flow");
        }
        // Marking the same destination again asks nothing new.
        let flow = io.marking.flow();
        io.mark(to);
        assert_eq!(io.marking.flow(), flow);

        for destination in [
            local,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), local.port()),
        ] {
            sender.send_to(b"after", destination).expect("send");
            assert!(
                io.wait(1000.0).expect("wait").socket,
                "nothing arrived at {destination} after the mark"
            );
            assert_eq!(io.drain().expect("drain"), 1);
        }
    }
}
