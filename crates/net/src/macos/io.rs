//! The wait and batched receive, over one socket and one wake on the wake's
//! kernel queue.
//!
//! **One call per burst, never one per datagram.** A single outstanding
//! receive plus a poll loses a keyframe burst outright. On one platform that
//! was the difference between zero and complete delivery of a burst on
//! loopback, so the batch is not an optimisation.
//!
//! **The batched receive is a call the system exports and declares nowhere
//! public**, found by name once, with its message header declared here. Where
//! it is not found, the same storage takes one datagram a call, up to a batch
//! a drain. Either way each datagram's control messages come with it, and its
//! arrival address is among them.
//!
//! Storage is allocated once and reused forever. The system writes straight
//! into the slots, so a received datagram costs no copy on our side and no
//! allocation on any path after construction.

use core::ffi::{c_int, c_uint, c_void};
use core::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::io;
use std::mem;
use std::sync::OnceLock;

use super::queue;
use super::socket::{Socket, from_storage, socklen};
use super::wake::Wake;
use crate::shell::Ready;
use crate::socket::{RECV_BATCH, RECV_SLOT};

/// The batched receive's message header: the system's own message header
/// with the datagram's length after it.
#[repr(C)]
struct MessageX {
    name: *mut c_void,
    name_len: libc::socklen_t,
    iov: *mut libc::iovec,
    iov_len: c_int,
    control: *mut c_void,
    control_len: libc::socklen_t,
    flags: c_int,
    /// Out: the datagram's length.
    data_len: usize,
}

// The system's layout on both architectures, measured.
const _: () = assert!(mem::size_of::<MessageX>() == 56);

type ReceiveX = unsafe extern "C" fn(c_int, *mut MessageX, c_uint, c_int) -> isize;

/// How a drain takes datagrams from the system.
#[derive(Clone, Copy)]
enum Receive {
    /// A batch a call.
    Batched(ReceiveX),
    /// A datagram a call, where the batched call is not found.
    Single,
}

impl Receive {
    /// The batched call where the system has it. Looked up once a process.
    fn found() -> Self {
        static FOUND: OnceLock<Option<ReceiveX>> = OnceLock::new();
        let found = FOUND.get_or_init(|| {
            // SAFETY: a lookup in the process's own namespace by a valid name.
            let symbol = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c"recvmsg_x".as_ptr()) };
            if symbol.is_null() {
                lowlat_common::log_info!("net: batched receive not found, a datagram a call");
                return None;
            }
            // SAFETY: the signature is the system's own for this call.
            Some(unsafe { mem::transmute::<*mut c_void, ReceiveX>(symbol) })
        });
        match *found {
            Some(call) => Self::Batched(call),
            None => Self::Single,
        }
    }
}

/// The socket, the wake and the receive storage, owned together for a
/// session.
#[derive(Debug)]
pub(crate) struct Io {
    socket: Socket,
    wake: Wake,
    batch: Batch,
    /// Why the socket could not join the queue at construction, reported by
    /// every call after it rather than by a constructor that cannot fail.
    broken: Option<i32>,
}

impl Io {
    /// Allocate the receive storage and join the socket to the wake's queue.
    /// Once per session, never on a data path.
    ///
    /// A failure to join is kept and returned by the first call that needs
    /// it, so the signature is the same on every platform.
    pub(crate) fn new(socket: Socket, wake: Wake) -> Self {
        Self::with(socket, wake, Receive::found())
    }

    /// As [`Io::new`], with the receive named rather than found.
    fn with(socket: Socket, wake: Wake, receive: Receive) -> Self {
        let ident = usize::try_from(socket.raw()).unwrap_or(usize::MAX);
        let read = queue::event(ident, libc::EVFILT_READ, libc::EV_ADD, 0, 0);
        let broken = match queue::apply(wake.queue(), &[read]) {
            Ok(()) => None,
            Err(error) => {
                lowlat_common::log_error!("net: receive not set up, err={}", error);
                Some(error.raw_os_error().unwrap_or(libc::EINVAL))
            }
        };
        Self {
            socket,
            wake,
            batch: Batch::new(receive),
            broken,
        }
    }

    fn check(&self) -> io::Result<()> {
        match self.broken {
            Some(code) => Err(io::Error::from_raw_os_error(code)),
            None => Ok(()),
        }
    }

    pub(crate) fn socket(&self) -> &Socket {
        &self.socket
    }

    pub(crate) fn wake(&self) -> &Wake {
        &self.wake
    }

    /// Wait for the socket or the wake, or the deadline, which the wait arms
    /// as a critical timer in the same call (`queue.rs`).
    ///
    /// Reports which of the two the queue spoke about rather than a bare
    /// "something happened", so the pass can leave the quiet one alone.
    pub(crate) fn wait(&mut self, timeout_ms: f64) -> io::Result<Ready> {
        self.check()?;
        let mut events = [queue::NONE; queue::EVENTS];
        let got = queue::wait(
            self.wake.queue(),
            &[queue::deadline(timeout_ms)],
            &mut events,
            false,
        )?;
        let mut ready = Ready::default();
        for event in events.get(..got).unwrap_or_default() {
            // The structure is packed: the field is read by value.
            let filter = event.filter;
            if filter == libc::EVFILT_READ {
                ready.socket = true;
            } else if filter == libc::EVFILT_USER {
                ready.wake = true;
            }
        }
        Ok(ready)
    }

    /// Consume the pending wake, if any. See [`Wake::take`] for why this
    /// comes before the application is pulled.
    pub(crate) fn take_wake(&mut self) -> io::Result<bool> {
        self.wake.take()
    }

    /// Pull whatever the system has queued, up to the batch size.
    ///
    /// Returns how many datagrams arrived. Zero means the queue is drained;
    /// the caller stops when it sees a short batch, because a full one means
    /// there may be more behind it.
    pub(crate) fn drain(&mut self) -> io::Result<usize> {
        self.check()?;
        self.batch.drain(&self.socket)
    }

    /// True when the last drain filled every slot, so more may be queued.
    pub(crate) fn saturated(&self) -> bool {
        self.batch.filled == RECV_BATCH
    }

    /// Mark the established path. Nothing to do here: a v6 send is marked by
    /// the socket's class, a v4 one by a control message of its own.
    pub(crate) fn mark(&mut self, _to: SocketAddr) {}

    /// The datagrams from the last drain: the address each came from, the
    /// local address it arrived at, and the bytes.
    ///
    /// A v4-mapped source is reported as IPv4, structurally. The local
    /// address is `None` when the system attached no packet information,
    /// which a caller treats as "could not say" rather than an error.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (SocketAddr, Option<IpAddr>, &[u8])> {
        self.batch.iter()
    }
}

/// Room for one datagram's control messages: one packet-information
/// structure, well under this either way.
const CONTROL_LEN: usize = 64;

/// Control message storage, aligned as the system requires.
#[repr(align(8))]
#[derive(Clone, Copy)]
struct Control([u8; CONTROL_LEN]);

/// Reusable receive storage: slots, addresses, and the headers pointing at
/// them.
///
/// The headers hold raw pointers into the boxed slot, address and control
/// arrays. Boxing is what makes that sound: moving a `Batch` moves the boxes,
/// not the heap allocations they point at, so the pointers stay valid for the
/// life of the object.
struct Batch {
    slots: Box<[[u8; RECV_SLOT]]>,
    names: Box<[libc::sockaddr_storage]>,
    /// Never read through this handle, and load bearing anyway: every header
    /// holds a raw pointer into it, so the field exists to keep the
    /// allocation alive. Removing it because nothing reads it would leave the
    /// system writing through dangling pointers.
    #[allow(dead_code, reason = "kept alive for the pointers in msgs")]
    iovs: Box<[libc::iovec]>,
    /// As `iovs`: the headers point into it, and the system writes each
    /// datagram's packet information there.
    #[allow(dead_code, reason = "kept alive for the pointers in msgs")]
    controls: Box<[Control]>,
    msgs: Box<[MessageX]>,
    receive: Receive,
    filled: usize,
}

impl core::fmt::Debug for Batch {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Batch")
            .field("capacity", &self.slots.len())
            .field("filled", &self.filled)
            .field("batched", &matches!(self.receive, Receive::Batched(_)))
            .finish()
    }
}

impl Batch {
    fn new(receive: Receive) -> Self {
        let mut slots = vec![[0u8; RECV_SLOT]; RECV_BATCH].into_boxed_slice();
        // SAFETY: sockaddr_storage and the header structures are plain data
        // with no invalid bit patterns, so an all-zero value is valid.
        let mut names =
            vec![unsafe { mem::zeroed::<libc::sockaddr_storage>() }; RECV_BATCH].into_boxed_slice();
        let mut iovs = vec![unsafe { mem::zeroed::<libc::iovec>() }; RECV_BATCH].into_boxed_slice();
        let mut controls = vec![Control([0u8; CONTROL_LEN]); RECV_BATCH].into_boxed_slice();
        let mut msgs = (0..RECV_BATCH)
            .map(|_| MessageX {
                name: core::ptr::null_mut(),
                name_len: 0,
                iov: core::ptr::null_mut(),
                iov_len: 1,
                control: core::ptr::null_mut(),
                control_len: 0,
                flags: 0,
                data_len: 0,
            })
            .collect::<Vec<_>>()
            .into_boxed_slice();

        let parts = slots
            .iter_mut()
            .zip(names.iter_mut())
            .zip(iovs.iter_mut())
            .zip(controls.iter_mut())
            .zip(msgs.iter_mut());
        for ((((slot, name), iov), control), msg) in parts {
            iov.iov_base = slot.as_mut_ptr().cast();
            iov.iov_len = RECV_SLOT;
            msg.iov = iov;
            msg.name = core::ptr::from_mut(name).cast();
            msg.control = control.0.as_mut_ptr().cast();
        }

        Self {
            slots,
            names,
            iovs,
            controls,
            msgs,
            receive,
            filled: 0,
        }
    }

    fn drain(&mut self, socket: &Socket) -> io::Result<usize> {
        // The address and control lengths are in and out: the system
        // overwrites each with what it actually wrote, so a reused header that
        // is not reset presents the previous datagram's lengths on the next
        // call and truncates both. Reset every slot, every pass.
        let name_len = socklen(mem::size_of::<libc::sockaddr_storage>());
        let control_len = socklen(CONTROL_LEN);
        for msg in self.msgs.iter_mut() {
            msg.name_len = name_len;
            msg.control_len = control_len;
            msg.flags = 0;
            msg.data_len = 0;
        }
        self.filled = 0;

        self.filled = match self.receive {
            Receive::Batched(call) => {
                // SAFETY: `msgs` is a contiguous array of exactly RECV_BATCH
                // initialised headers, each pointing at storage this object
                // owns and keeps alive. MSG_DONTWAIT keeps the call from
                // blocking, so a caller that has already waited never parks
                // here.
                let got = unsafe {
                    call(
                        socket.raw(),
                        self.msgs.as_mut_ptr(),
                        c_uint::try_from(RECV_BATCH).unwrap_or(1),
                        libc::MSG_DONTWAIT,
                    )
                };
                if got < 0 {
                    let error = io::Error::last_os_error();
                    return match error.kind() {
                        // Drained, or interrupted before anything arrived.
                        // Neither is a failure; both mean "nothing more this
                        // pass".
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted => Ok(0),
                        _ => Err(error),
                    };
                }
                usize::try_from(got).unwrap_or(0)
            }
            Receive::Single => self.drain_single(socket)?,
        };
        Ok(self.filled)
    }

    /// One datagram a call into the same storage, until nothing is queued or
    /// the batch is full.
    fn drain_single(&mut self, socket: &Socket) -> io::Result<usize> {
        let mut count = 0;
        for msg in self.msgs.iter_mut() {
            // The system's own header over the same storage.
            // SAFETY: msghdr is plain data; zeroing it is the documented way
            // to start.
            let mut plain: libc::msghdr = unsafe { mem::zeroed() };
            plain.msg_name = msg.name;
            plain.msg_namelen = msg.name_len;
            plain.msg_iov = msg.iov;
            plain.msg_iovlen = msg.iov_len;
            plain.msg_control = msg.control;
            plain.msg_controllen = msg.control_len;
            // SAFETY: every pointer names storage this batch owns, with the
            // lengths stated; MSG_DONTWAIT keeps the call from blocking.
            let got = unsafe { libc::recvmsg(socket.raw(), &raw mut plain, libc::MSG_DONTWAIT) };
            if got < 0 {
                let error = io::Error::last_os_error();
                return match error.kind() {
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted => Ok(count),
                    // What arrived before the failure is handed out; the next
                    // drain meets the failure again if it stays.
                    _ if count > 0 => Ok(count),
                    _ => Err(error),
                };
            }
            msg.name_len = plain.msg_namelen;
            msg.control_len = plain.msg_controllen;
            msg.flags = plain.msg_flags;
            msg.data_len = usize::try_from(got).unwrap_or(0);
            count += 1;
        }
        Ok(count)
    }

    fn iter(&self) -> impl Iterator<Item = (SocketAddr, Option<IpAddr>, &[u8])> {
        (0..self.filled).filter_map(move |index| {
            let msg = self.msgs.get(index)?;
            let bytes = self.slots.get(index)?.get(..msg.data_len)?;
            let from = from_storage(self.names.get(index)?)?;
            Some((from, local_of(msg), bytes))
        })
    }
}

/// The local address a datagram arrived at, from its control messages.
///
/// Both families arrive as `(IPPROTO_IPV6, IPV6_PKTINFO)` on the dual-stack
/// socket, a v4 arrival v4-mapped: the v4-level option that would report it
/// in its own form is refused on this socket.
fn local_of(msg: &MessageX) -> Option<IpAddr> {
    // The system's macros walk its own header; this one names the same
    // control storage.
    // SAFETY: msghdr is plain data; zeroing it is the documented way to start.
    let mut plain: libc::msghdr = unsafe { mem::zeroed() };
    plain.msg_control = msg.control;
    plain.msg_controllen = msg.control_len;
    // SAFETY: the control pointer and length name storage this batch owns,
    // sized by the system to what it actually wrote; the macros only compute
    // offsets inside that region, and the reads are unaligned-tolerant.
    unsafe {
        let mut cmsg = libc::CMSG_FIRSTHDR(&raw const plain);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::IPPROTO_IPV6 && (*cmsg).cmsg_type == libc::IPV6_PKTINFO {
                let info =
                    core::ptr::read_unaligned(libc::CMSG_DATA(cmsg).cast::<libc::in6_pktinfo>());
                let ip = Ipv6Addr::from(info.ipi6_addr.s6_addr);
                // Structural, as everywhere: a v4 arrival reported v4-mapped
                // is a v4 address.
                return Some(match ip.to_ipv4_mapped() {
                    Some(v4) => IpAddr::V4(v4),
                    None => IpAddr::V6(ip),
                });
            }
            cmsg = libc::CMSG_NXTHDR(&raw const plain, cmsg);
        }
        None
    }
}

// SAFETY: the raw pointers inside the headers point only into this object's
// own boxed allocations, which move with it and are never aliased elsewhere,
// and the call they would name is a plain function. Nothing here is shared
// between threads without the usual borrowing rules, so a Batch is as
// sendable as the bytes it holds.
unsafe impl Send for Batch {}

#[cfg(test)]
mod tests {
    use super::*;
    use core::net::Ipv4Addr;

    /// **Each receive takes a burst whole, with each datagram's source and
    /// arrival address**: the batched one, which must be found here, and
    /// the one a call, driven by name where the system would have the other.
    #[test]
    fn each_receive_takes_a_burst_with_its_addresses() {
        let found = Receive::found();
        assert!(
            matches!(found, Receive::Batched(_)),
            "the batched receive was not found"
        );
        for receive in [found, Receive::Single] {
            let sender = Socket::open(0).expect("sender");
            let from_port = sender.local_addr().expect("addr").port();
            let socket = Socket::open(0).expect("receiver");
            let port = socket.local_addr().expect("addr").port();
            let mut io = Io::with(socket, Wake::new().expect("wake"), receive);

            let v4 = IpAddr::V4(Ipv4Addr::LOCALHOST);
            let v6 = IpAddr::V6(Ipv6Addr::LOCALHOST);
            for (index, to) in [v4, v6, v4].into_iter().enumerate() {
                sender
                    .send_to(&[index as u8; 300], SocketAddr::new(to, port))
                    .expect("send");
            }
            // The loopback delivers after the send has returned: the burst is
            // counted queued before the one call that must take it whole.
            let began = std::time::Instant::now();
            while queued(io.socket()) < 3 && began.elapsed().as_secs() < 1 {
                std::thread::yield_now();
            }

            assert!(io.wait(1000.0).expect("wait").socket, "nothing arrived");
            assert_eq!(
                io.drain().expect("drain"),
                3,
                "the burst did not arrive whole"
            );
            let seen: Vec<_> = io
                .iter()
                .map(|(from, local, bytes)| {
                    (
                        from.ip(),
                        from.port(),
                        local,
                        bytes.first().copied(),
                        bytes.len(),
                    )
                })
                .collect();
            assert_eq!(
                seen,
                std::vec![
                    (v4, from_port, Some(v4), Some(0), 300),
                    (v6, from_port, Some(v6), Some(1), 300),
                    (v4, from_port, Some(v4), Some(2), 300),
                ],
                "a datagram, its source or its arrival address went astray"
            );
            assert_eq!(io.drain().expect("drain"), 0, "a drained socket gave more");
        }
    }

    /// Datagrams queued on `socket`, as the system counts them.
    fn queued(socket: &Socket) -> usize {
        // The system's option for the count: its header declares it, the
        // bindings do not.
        const SO_NUMRCVPKT: c_int = 0x1112;
        let mut count: c_int = 0;
        let mut len = socklen(mem::size_of::<c_int>());
        // SAFETY: an int option buffer and its length, both writable.
        let rc = unsafe {
            libc::getsockopt(
                socket.raw(),
                libc::SOL_SOCKET,
                SO_NUMRCVPKT,
                (&raw mut count).cast(),
                &raw mut len,
            )
        };
        assert_eq!(rc, 0, "the queued count cannot be read");
        usize::try_from(count).unwrap_or(0)
    }

    /// p50, p95 and p99 of `samples`, sorted in place.
    fn quantiles(samples: &mut [f64]) -> (f64, f64, f64) {
        samples.sort_by(f64::total_cmp);
        let at = |p: f64| samples[((samples.len() - 1) as f64 * p).round() as usize];
        (at(0.50), at(0.95), at(0.99))
    }

    /// **A measurement, not a gate**: how late the loop's wait lands on its
    /// deadline with nothing arriving, twenty 5 ms waits, on an idle Mac; the
    /// queue's own timeout in its place lands an eighth to a quarter late.
    /// Run with `cargo test -p lowlat-net --lib wait_lateness -- --ignored
    /// --nocapture`.
    #[test]
    #[ignore = "a measurement, run by hand on an idle Mac"]
    fn wait_lateness() {
        let socket = Socket::open(0).expect("socket");
        let mut io = Io::new(socket, Wake::new().expect("wake"));
        let mut late_us: Vec<f64> = (0..20)
            .map(|_| {
                let begin = std::time::Instant::now();
                let ready = io.wait(5.0).expect("wait");
                assert!(!ready.socket && !ready.wake, "the wait was not quiet");
                begin.elapsed().as_secs_f64().mul_add(1e6, -5000.0)
            })
            .collect();
        let (p50, p95, p99) = quantiles(&mut late_us);
        println!("wait 5 ms: late by p50 {p50:.1} p95 {p95:.1} p99 {p99:.1} us");
    }

    /// **A measurement, not a gate**: the queued-burst drain of the portable
    /// `receive_cost`, through each receive -- the batched one and the one a
    /// call -- so the two are timed on the same burst. Run in release with
    /// `cargo test -p lowlat-net --release --lib receive_each_way -- --ignored
    /// --nocapture`.
    #[test]
    #[ignore = "a measurement, run by hand in release"]
    fn receive_each_way() {
        const BURST: usize = 2550;
        const SIZE: usize = 1200;
        const ROUNDS: usize = 50;
        for receive in [Receive::found(), Receive::Single] {
            let sender = Socket::open(0).expect("sender");
            let socket = Socket::open(0).expect("receiver");
            let to = SocketAddr::new(
                IpAddr::V6(Ipv6Addr::LOCALHOST),
                socket.local_addr().expect("addr").port(),
            );
            let mut io = Io::with(socket, Wake::new().expect("wake"), receive);
            let payload = [0x5Au8; SIZE];
            let mut per_burst = Vec::with_capacity(ROUNDS);
            for _ in 0..ROUNDS {
                for _ in 0..BURST {
                    while let Err(error) = sender.send_to(&payload, to) {
                        assert_eq!(error.kind(), io::ErrorKind::WouldBlock, "send: {error}");
                        std::thread::yield_now();
                    }
                }
                std::thread::sleep(core::time::Duration::from_millis(20));
                let started = std::time::Instant::now();
                let mut got = 0;
                while got < BURST && io.wait(100.0).expect("wait").socket {
                    loop {
                        let drained = io.drain().expect("drain");
                        got += io
                            .iter()
                            .filter(|(_, _, bytes)| bytes.len() == SIZE)
                            .count();
                        if drained == 0 || !io.saturated() {
                            break;
                        }
                    }
                }
                per_burst.push(started.elapsed().as_secs_f64() * 1e3);
                assert_eq!(got, BURST, "the burst did not arrive whole");
            }
            let (p50, p95, p99) = quantiles(&mut per_burst);
            let name = match receive {
                Receive::Batched(_) => "batched",
                Receive::Single => "one a call",
            };
            println!(
                "receive {name}: {BURST} x {SIZE} B queued: p50 {p50:.3} p95 {p95:.3} p99 \
                 {p99:.3} ms a burst, p50 {:.0} ns a datagram",
                p50 * 1e6 / BURST as f64
            );
        }
    }

    /// **A measurement, not a gate**: from a producer's notify to the loop's
    /// wait returning with the wake, a thousand times, the loop parked in the
    /// queue each time. Run in release with `cargo test -p lowlat-net
    /// --release --lib wake_latency -- --ignored --nocapture`.
    #[test]
    #[ignore = "a measurement, run by hand in release"]
    fn wake_latency() {
        const WAKES: usize = 1000;
        let socket = Socket::open(0).expect("socket");
        let mut io = Io::new(socket, Wake::new().expect("wake"));
        let handle = io.wake().handle().expect("handle");
        let (sent, notified) = std::sync::mpsc::channel::<std::time::Instant>();
        let producer = std::thread::spawn(move || {
            for _ in 0..WAKES {
                std::thread::sleep(core::time::Duration::from_millis(2));
                let at = std::time::Instant::now();
                handle.notify().expect("notify");
                sent.send(at).expect("send");
            }
        });
        let mut latency_us = Vec::with_capacity(WAKES);
        while latency_us.len() < WAKES {
            if io.wait(50.0).expect("wait").wake {
                let back = std::time::Instant::now();
                assert!(
                    io.take_wake().expect("take"),
                    "a wake reported and not armed"
                );
                let at = notified.recv().expect("the notify's time");
                latency_us.push(back.duration_since(at).as_secs_f64() * 1e6);
            }
        }
        producer.join().expect("producer");
        let (p50, p95, p99) = quantiles(&mut latency_us);
        println!("wake: notify to wait return p50 {p50:.1} p95 {p95:.1} p99 {p99:.1} us");
    }

    /// **A measurement, not a gate, and the receiving half of one**: bursts
    /// from another machine over the network, the receive thread spending
    /// `LOWLAT_BURST_WORK_NS` on each datagram as a loop that decrypts and
    /// delivers does, through each receive in turn (`LOWLAT_BURST_SECONDS`
    /// each). The sender, on the other machine, sends numbered bursts of
    /// 2550 x 1200 B to `LOWLAT_BURST_PORT` (default 47500); each datagram
    /// carries its burst and its index, little-endian, in its first eight
    /// bytes. Run with `cargo test -p lowlat-net --release --lib lan_burst --
    /// --ignored --nocapture`.
    #[test]
    #[ignore = "a measurement, needs a sender on another machine"]
    fn lan_burst() {
        let number = |name: &str, default: u64| {
            std::env::var(name)
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(default)
        };
        let port = u16::try_from(number("LOWLAT_BURST_PORT", 47500)).expect("a port");
        let work = core::time::Duration::from_nanos(number("LOWLAT_BURST_WORK_NS", 2000));
        let seconds = number("LOWLAT_BURST_SECONDS", 20);
        for receive in [Receive::found(), Receive::Single] {
            let socket = Socket::open(port).expect("socket");
            assert_eq!(
                socket.local_addr().expect("addr").port(),
                port,
                "the port walked"
            );
            println!(
                "lan_burst: granted receive buffer {} B",
                socket.granted_recv_buffer()
            );
            let mut io = Io::with(socket, Wake::new().expect("wake"), receive);
            let mut bursts: std::collections::BTreeMap<u32, (u32, f64, f64)> =
                std::collections::BTreeMap::new();
            let began = std::time::Instant::now();
            let mut busy = core::time::Duration::ZERO;
            while began.elapsed().as_secs() < seconds {
                if !io.wait(50.0).expect("wait").socket {
                    continue;
                }
                let pass = std::time::Instant::now();
                loop {
                    let drained = io.drain().expect("drain");
                    let now_ms = began.elapsed().as_secs_f64() * 1e3;
                    for (_, _, bytes) in io.iter() {
                        let Some(head) = bytes.get(..8) else { continue };
                        let burst = u32::from_le_bytes(head[..4].try_into().expect("four"));
                        let entry = bursts.entry(burst).or_insert((0, now_ms, now_ms));
                        entry.0 += 1;
                        entry.2 = now_ms;
                        let spin = std::time::Instant::now();
                        while spin.elapsed() < work {
                            core::hint::spin_loop();
                        }
                    }
                    if drained == 0 || !io.saturated() {
                        break;
                    }
                }
                busy += pass.elapsed();
            }
            let name = match receive {
                Receive::Batched(_) => "batched",
                Receive::Single => "one a call",
            };
            let whole = bursts.values().filter(|(n, _, _)| *n == 2550).count();
            let mut spans: Vec<f64> = bursts
                .values()
                .map(|(_, first, last)| last - first)
                .collect();
            let (p50, p95, p99) = if spans.is_empty() {
                (0.0, 0.0, 0.0)
            } else {
                quantiles(&mut spans)
            };
            let short: Vec<u32> = bursts
                .values()
                .map(|(n, _, _)| *n)
                .filter(|n| *n != 2550)
                .collect();
            println!(
                "lan_burst {name}, {} ns of work a datagram: {} bursts, {whole} whole, short: {short:?}; \
                 first to last p50 {p50:.1} p95 {p95:.1} p99 {p99:.1} ms; receive thread busy {:.0} ms",
                work.as_nanos(),
                bursts.len(),
                busy.as_secs_f64() * 1e3
            );
        }
    }
}
