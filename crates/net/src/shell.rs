//! The event loop: wait, receive, poll, drain.
//!
//! The core decides what to send and when it next needs attention. This decides
//! how bytes reach the wire and when to wake up. Neither reaches into the other,
//! which is why the loop is short enough to read in one go.
//!
//! **There is no tick.** The wait is armed from the endpoint's own deadline.
//! The upper clamp is a safety net against a core returning nonsense, and it
//! sits well above every deadline the core actually arms -- a cap shorter than
//! the acknowledgement cadence would bind on every pass and quietly reinstate
//! the fixed poll it was meant to prevent.

use std::io;
use std::os::fd::AsRawFd;

use lowlat_core::endpoint::Endpoint;

use crate::recv;
use crate::send;
use crate::socket::Socket;
use crate::wake::Wake;

/// Shortest wait. A sub-millisecond timeout is a hot poll wearing a timeout's
/// clothes.
pub const MIN_WAIT_MS: f64 = 1.0;

/// Longest wait. Never binds in normal operation: the session's own deadline is
/// bounded by the acknowledgement cadence, well below this.
pub const MAX_WAIT_MS: f64 = 50.0;

/// Why the loop woke.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Woke {
    /// The armed deadline expired with nothing to do.
    Timeout,
    /// A datagram arrived.
    Datagram,
    /// The application enqueued work.
    Send,
}

/// Which descriptors the wait reported on.
///
/// **Not `POLLIN` alone.** An error or hangup bit is also a condition to go and
/// collect, and it is cleared by the syscall that collects it. A pass that saw
/// one and skipped the syscall would poll again immediately on the same
/// unconsumed bit, and again, which is a spin rather than the saved call it was
/// meant to be. Anything poll says about a descriptor sends the pass to it;
/// only silence is skipped.
#[derive(Debug, Default, Clone, Copy)]
struct Ready {
    socket: bool,
    wake: bool,
}

/// What one pass did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Turn {
    pub woke: Woke,
    pub received: usize,
    pub sent: usize,
}

/// Wake accounting, which is how the loop is shown to be event driven rather
/// than polling.
///
/// A loop that polls shows timeout wakes an order of magnitude above the number
/// of deadlines it armed. One that waits shows roughly one per deadline.
#[derive(Debug, Default, Clone, Copy)]
pub struct Stats {
    pub timeout_wakes: u64,
    pub datagram_wakes: u64,
    pub send_wakes: u64,
    pub datagrams_in: u64,
    pub datagrams_out: u64,
    /// Datagrams the endpoint refused: unparseable, unauthenticated, or a
    /// shape it does not accept.
    ///
    /// **Hostile and corrupt input is ordinary on a network, but a rejection
    /// that leaves no trace makes a wire mismatch and a silent path the same
    /// picture.** One peer generation's acknowledgements were refused on their
    /// length and dropped here without a word, which read as a peer that had
    /// stopped receiving and cost a session every time. Counting them does not
    /// make the drop an error; it makes it visible.
    pub rejected: u64,
}

impl Stats {
    /// Total wakes of every kind.
    pub fn wakes(&self) -> u64 {
        self.timeout_wakes + self.datagram_wakes + self.send_wakes
    }
}

/// One guest's loop: a socket, a wake, and the endpoint they drive.
#[derive(Debug)]
pub struct Shell<'a> {
    socket: Socket,
    wake: Wake,
    endpoint: Endpoint<'a>,
    inbound: recv::Batch,
    outbound: send::Batch,
    scratch: Box<[u8]>,
    stats: Stats,
}

impl<'a> Shell<'a> {
    /// Take ownership of the socket and wake for the life of the session.
    pub fn new(socket: Socket, wake: Wake, endpoint: Endpoint<'a>) -> Self {
        lowlat_common::log_info!(
            "net: socket open, rcvbuf={} sndbuf={}",
            socket.granted_recv_buffer(),
            socket.granted_send_buffer()
        );
        Self {
            socket,
            wake,
            endpoint,
            inbound: recv::Batch::new(),
            outbound: send::Batch::new(),
            scratch: vec![0u8; crate::socket::RECV_SLOT].into_boxed_slice(),
            stats: Stats::default(),
        }
    }

    /// The endpoint, for candidates and messages.
    pub fn endpoint(&mut self) -> &mut Endpoint<'a> {
        &mut self.endpoint
    }

    /// The socket, for its address and granted buffers.
    pub fn socket(&self) -> &Socket {
        &self.socket
    }

    /// A producer's end of the wake, for a thread that will enqueue work.
    pub fn wake_handle(&self) -> io::Result<crate::wake::WakeHandle> {
        self.wake.handle()
    }

    /// Wake accounting so far.
    pub fn stats(&self) -> Stats {
        self.stats
    }

    /// One pass of the loop.
    ///
    /// `app` is called on every pass, after the wake has been taken if there
    /// was one, to pull whatever the application has enqueued. Input is pulled
    /// there before receive processing produces any output, because input
    /// latency is the one budget with a human in it.
    pub fn turn(
        &mut self,
        now_ms: f64,
        mut app: impl FnMut(&mut Endpoint<'a>),
    ) -> io::Result<Turn> {
        let timeout = self
            .endpoint
            .next_timer_ms(now_ms)
            .clamp(MIN_WAIT_MS, MAX_WAIT_MS);
        let ready = self.wait(timeout)?;

        // Taken before the application is pulled, never after. Anything
        // enqueued from here on leaves the descriptor armed, so the next wait
        // returns at once rather than sitting out the timeout.
        //
        // Asked only when poll reported it. An eventfd with nothing pending
        // answers EAGAIN and a socket with nothing queued answers the same, so
        // a pass that asks both regardless spends two syscalls on every quiet
        // wake to be told what poll already said. **The application is pulled
        // either way**: a ring can be filled by a producer whose notify has
        // not landed yet, and gating that on the wake would hold the work
        // until the next one.
        let woken_by_send = if ready.wake { self.wake.take()? } else { false };
        app(&mut self.endpoint);

        let received = if ready.socket {
            self.receive(now_ms)?
        } else {
            0
        };
        self.endpoint.poll(now_ms);
        let sent = self.drain(now_ms)?;

        let woke = if received > 0 {
            self.stats.datagram_wakes += 1;
            Woke::Datagram
        } else if woken_by_send {
            self.stats.send_wakes += 1;
            Woke::Send
        } else {
            self.stats.timeout_wakes += 1;
            Woke::Timeout
        };

        self.stats.datagrams_in += received as u64;
        self.stats.datagrams_out += sent as u64;
        Ok(Turn {
            woke,
            received,
            sent,
        })
    }

    /// Wait for either descriptor, or the deadline.
    ///
    /// Reports which descriptors poll spoke about rather than a bare "something
    /// happened", so the pass can leave the quiet ones alone.
    fn wait(&self, timeout_ms: f64) -> io::Result<Ready> {
        #[allow(
            clippy::cast_possible_truncation,
            reason = "clamped to the wait bounds by the caller"
        )]
        let timeout = timeout_ms.max(MIN_WAIT_MS) as libc::c_int;
        let mut fds = [
            libc::pollfd {
                fd: self.socket.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: self.wake.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: two fully initialised descriptors are passed with a matching
        // count.
        let rc = unsafe { libc::poll(fds.as_mut_ptr(), 2, timeout) };
        if rc <= 0 {
            if rc < 0 {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
            // Quiet, and on the interrupted path the reported events are not
            // meaningful either. An interrupt is not a failure and nothing is
            // lost by treating it as quiet: whatever was pending is still
            // pending, and the descriptor is still armed for the next pass.
            return Ok(Ready::default());
        }
        let [socket, wake] = fds;
        Ok(Ready {
            socket: socket.revents != 0,
            wake: wake.revents != 0,
        })
    }

    /// Pull every queued datagram and hand each to the endpoint.
    fn receive(&mut self, now_ms: f64) -> io::Result<usize> {
        let mut total = 0;
        loop {
            let got = self.inbound.drain(&self.socket)?;
            if got == 0 {
                return Ok(total);
            }
            total += got;
            let mut refused = 0u64;
            for (from, datagram) in self.inbound.iter() {
                // A datagram that fails to parse or authenticate is dropped and
                // the loop continues. Hostile and corrupt input is the normal
                // case on a network, not an error path -- but it is counted,
                // because a drop nobody counts is indistinguishable from a path
                // that carried nothing.
                if self
                    .endpoint
                    .process_input(datagram, from, now_ms, &mut self.scratch)
                    .is_err()
                {
                    refused += 1;
                }
            }
            self.stats.rejected += refused;
            if !self.inbound.saturated() {
                return Ok(total);
            }
        }
    }

    /// Stage and send everything the endpoint has ready.
    fn drain(&mut self, now_ms: f64) -> io::Result<usize> {
        let mut sent = 0;
        loop {
            let room = self.outbound.stage();
            let Some(result) = self.endpoint.get_output(now_ms, room) else {
                break;
            };
            let egress = match result {
                Ok(egress) => egress,
                Err(_) => {
                    // Nearly always a full batch rather than a bad emission:
                    // staging hands back the room that is left, and once that
                    // is shorter than the next datagram the core cannot encode
                    // into it. Asking again unchanged returns the same failure
                    // forever, which wedges the loop at full CPU the moment a
                    // pass produces more than the batch holds -- invisible to
                    // any test that sends a datagram or two.
                    //
                    // Flush, which makes the whole buffer available, and ask
                    // once more. A failure with all of it free is our defect,
                    // and the emission is dropped rather than retried.
                    if self.outbound.staged() > 0 {
                        self.outbound.flush(&self.socket)?;
                        continue;
                    }
                    break;
                }
            };
            self.outbound.commit(&self.socket, egress)?;
            sent += 1;
        }
        self.outbound.flush(&self.socket)?;
        Ok(sent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::net::{IpAddr, Ipv6Addr, SocketAddr};
    use lowlat_core::channel::{RecvRing, SlotMeta};
    use lowlat_core::conn::{Conn, Credentials};
    use lowlat_core::envelope::Envelope;
    use lowlat_core::send::{SendRing, SendSlot};
    use lowlat_core::session::Session;

    const SLOT: usize = 256;
    const SLOTS: usize = 64;
    const KEY: [u8; 32] = [0x77u8; 32];
    const CHANNEL: u8 = 1;

    struct Arena {
        recv_bodies: Vec<u8>,
        recv_meta: Vec<SlotMeta>,
        send_bodies: Vec<u8>,
        send_meta: Vec<SendSlot>,
    }

    impl Arena {
        fn new() -> Self {
            Self {
                recv_bodies: vec![0u8; SLOT * SLOTS],
                recv_meta: vec![SlotMeta::default(); SLOTS],
                send_bodies: vec![0u8; SLOT * SLOTS],
                send_meta: vec![SendSlot::default(); SLOTS],
            }
        }
    }

    fn endpoint<'a>(
        arena: &'a mut Arena,
        ours: (&'a str, &'a str),
        theirs: (&'a str, &'a str),
        seed: u8,
    ) -> Endpoint<'a> {
        let conn = Conn::new(
            Credentials {
                local_ufrag: ours.0,
                local_pwd: ours.1,
                remote_ufrag: theirs.0,
                remote_pwd: theirs.1,
            },
            [seed; 16],
            0.0,
        );
        let mut session = Session::new(Envelope::from_key(&KEY).unwrap(), 1, 0.0);
        session
            .attach_recv(
                CHANNEL,
                RecvRing::new(&mut arena.recv_bodies, &mut arena.recv_meta, SLOT).unwrap(),
            )
            .unwrap();
        session
            .attach_send(
                CHANNEL,
                SendRing::new(&mut arena.send_bodies, &mut arena.send_meta, SLOT, CHANNEL).unwrap(),
            )
            .unwrap();
        Endpoint::new(conn, session)
    }

    fn shell<'a>(
        arena: &'a mut Arena,
        ours: (&'a str, &'a str),
        theirs: (&'a str, &'a str),
        seed: u8,
    ) -> Shell<'a> {
        let socket = Socket::open(0).expect("socket");
        let wake = Wake::new().expect("wake");
        Shell::new(socket, wake, endpoint(arena, ours, theirs, seed))
    }

    fn loopback_of(shell: &Shell<'_>) -> SocketAddr {
        let mut addr = shell.socket().local_addr().expect("addr");
        addr.set_ip(IpAddr::V6(Ipv6Addr::LOCALHOST));
        addr
    }

    const LEFT: (&str, &str) = ("aaaa", "passwordforaaaa");
    const RIGHT: (&str, &str) = ("bbbb", "passwordforbbbb");

    /// The shell drives a real punch over real sockets and then carries a
    /// message, which is the whole loop end to end.
    #[test]
    fn two_shells_punch_and_carry_a_message() {
        let mut left_arena = Arena::new();
        let mut right_arena = Arena::new();
        let mut left = shell(&mut left_arena, LEFT, RIGHT, 0xA1);
        let mut right = shell(&mut right_arena, RIGHT, LEFT, 0xB2);

        let left_addr = loopback_of(&left);
        let right_addr = loopback_of(&right);
        left.endpoint().conn().add_candidate(right_addr).unwrap();
        right.endpoint().conn().add_candidate(left_addr).unwrap();
        left.endpoint()
            .session()
            .send_message(CHANNEL, b"hdr", b"body")
            .unwrap();

        let mut now = 0.0;
        let mut out = [0u8; 512];
        let mut arrived = None;
        // Both sides must settle, not just the one that happens to establish
        // first: a message can arrive before the far side's own check has been
        // answered, and stopping there would hide a half-open path.
        while now < 4_000.0
            && (arrived.is_none()
                || left.endpoint().path().is_none()
                || right.endpoint().path().is_none())
        {
            left.turn(now, |_| {}).expect("left turn");
            right.turn(now, |_| {}).expect("right turn");
            if arrived.is_none()
                && let Some(Ok(len)) = right.endpoint().session().take_message(CHANNEL, &mut out)
            {
                arrived = Some(out[..len].to_vec());
            }
            now += 10.0;
        }

        assert_eq!(
            left.endpoint().path(),
            Some(right_addr),
            "left found no path"
        );
        assert_eq!(
            right.endpoint().path(),
            Some(left_addr),
            "right found no path"
        );
        assert_eq!(arrived.as_deref(), Some(&b"hdrbody"[..]));
    }

    /// The wake gets the loop moving without waiting out the deadline, which is
    /// the whole reason it exists.
    /// The wake gets the loop moving immediately instead of at the deadline,
    /// and the pass is attributed to it rather than to a timeout.
    #[test]
    fn an_application_send_wakes_the_loop() {
        let mut arena = Arena::new();
        let mut shell = shell(&mut arena, LEFT, RIGHT, 0xA1);
        let producer = shell.wake_handle().expect("handle");

        // Nothing enqueued: the pass is a timeout.
        assert_eq!(shell.turn(0.0, |_| {}).expect("turn").woke, Woke::Timeout);

        // A sending thread enqueues and notifies.
        std::thread::spawn(move || producer.notify().expect("notify"))
            .join()
            .expect("join");

        let started = std::time::Instant::now();
        let turn = shell
            .turn(1.0, |endpoint| {
                endpoint
                    .session()
                    .send_message(CHANNEL, &[], b"input")
                    .unwrap();
            })
            .expect("turn");

        assert_eq!(
            turn.woke,
            Woke::Send,
            "the pass was not attributed to the wake"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_millis(MAX_WAIT_MS as u64),
            "the loop waited out its deadline instead of waking"
        );
        assert_eq!(shell.stats().send_wakes, 1);
    }

    /// The pull is not gated on the wake, and a quiet pass must still make it.
    ///
    /// A producer can fill a ring and have its notify land after the wait
    /// returned, so a pass that asked the application only when the wake fired
    /// would hold that work until the next deadline. This is the one thing the
    /// readiness gating must not reach.
    #[test]
    fn a_quiet_pass_still_pulls_the_application() {
        let mut arena = Arena::new();
        let mut shell = shell(&mut arena, LEFT, RIGHT, 0xA1);

        let mut pulled = 0;
        let turn = shell.turn(0.0, |_| pulled += 1).expect("turn");

        assert_eq!(turn.woke, Woke::Timeout, "the pass was not a quiet one");
        assert_eq!(pulled, 1, "a quiet pass did not pull the application");
    }

    /// A datagram already queued when the wait returned is collected on that
    /// same pass.
    ///
    /// The bytes are rubbish on purpose: the count is of datagrams taken off
    /// the socket, before anything parses them, so this reports whether the
    /// pass went to the socket at all. It is what a receive gated on the wrong
    /// descriptor's readiness fails.
    #[test]
    fn a_queued_datagram_is_received_on_the_pass_it_woke() {
        let mut arena = Arena::new();
        let mut shell = shell(&mut arena, LEFT, RIGHT, 0xA1);
        let to = loopback_of(&shell);

        let sender = Socket::open(0).expect("sender");
        sender
            .send_to(b"neither a check nor a record", to)
            .expect("send");

        let turn = shell.turn(0.0, |_| {}).expect("turn");

        assert_eq!(
            turn.received, 1,
            "the pass woke for a datagram and never asked the socket"
        );
        assert_eq!(turn.woke, Woke::Datagram);
        // **And the refusal is counted.** The endpoint cannot make anything of
        // these bytes, which is ordinary, but a drop that leaves no trace makes
        // a wire the far side is speaking wrongly look exactly like a wire
        // carrying nothing -- and that reading has already cost sessions.
        assert_eq!(
            shell.stats().rejected,
            1,
            "a datagram the endpoint refused was dropped without a trace"
        );
    }

    /// A loop armed from the core's deadline must not wake more often than the
    /// deadlines it arms. This is the wake-accounting gate in miniature: a
    /// polling loop shows an order of magnitude more.
    #[test]
    fn an_idle_loop_wakes_on_its_deadline_not_on_a_tick() {
        let mut arena = Arena::new();
        let mut shell = shell(&mut arena, LEFT, RIGHT, 0xA1);

        // No candidate and no traffic, so the only deadline is the session's
        // acknowledgement cadence.
        let span_ms = 300.0;
        let mut now = 0.0;
        while now < span_ms {
            shell.turn(now, |_| {}).expect("turn");
            now += lowlat_core::session::ACK_CADENCE_MS;
        }

        let armed = (span_ms / lowlat_core::session::ACK_CADENCE_MS).ceil() as u64;
        assert!(
            shell.stats().wakes() <= armed + 1,
            "woke {} times for {armed} deadlines, which is a tick rather than a wait",
            shell.stats().wakes()
        );
    }
}
