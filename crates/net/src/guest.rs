//! The merged per-guest thread, and the teardown that ends it.
//!
//! One thread does the whole cycle for one guest: receive, feed the core,
//! deliver, drain, send. Decryption is inline, because the authenticated
//! decryption cost at these rates is far cheaper than the handoff it would
//! otherwise need.
//!
//! **No thread here raises its own priority.** No priority class, no scheduling
//! policy, no affinity. This is a library inside somebody else's process, and
//! outranking that process's own interface thread is a priority inversion that
//! has produced hard hangs on low-core machines while a reference
//! implementation ran fine on the same hardware. The lever that works is the
//! process class, which lifts every thread together and preserves ordering, and
//! that decision belongs to the application.
//!
//! **Teardown wakes before it joins.** The state is set, then the loop's wake
//! descriptor is notified, and only then is the thread joined. A teardown that
//! sets a flag and joins strands the thread in its wait until the full deadline
//! expires, which turns a clean disconnect into a visible hang.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread::JoinHandle;

use crate::wake::{Wake, WakeHandle};

/// Shared between the loop and whoever tears it down.
#[derive(Debug, Default)]
pub struct Running {
    stop: AtomicU32,
}

impl Running {
    /// True once teardown has begun. Check it once per pass.
    ///
    /// Acquire, paired with the Release in the teardown. Nothing is published
    /// through the flag today, so Relaxed would also be correct; the pairing is
    /// the idiomatic contract for a signal and costs nothing on a path taken
    /// once per session. What actually closes the race is the wake, not the
    /// ordering: the flag alone cannot pull a thread out of its wait.
    pub fn stopping(&self) -> bool {
        self.stop.load(Ordering::Acquire) != 0
    }
}

/// A running per-guest loop.
#[derive(Debug)]
pub struct Guest {
    thread: Option<JoinHandle<()>>,
    notify: WakeHandle,
    running: Arc<Running>,
}

impl Guest {
    /// Start the loop.
    ///
    /// `body` receives the wake to hand to its shell and the state to check
    /// each pass. It builds its own storage, so the ring memory and the socket
    /// live and die with the thread rather than being borrowed across it.
    pub fn spawn<F>(wake: Wake, body: F) -> io::Result<Self>
    where
        F: FnOnce(Wake, &Running) + Send + 'static,
    {
        let running = Arc::new(Running::default());
        let notify = wake.handle()?;
        let theirs = Arc::clone(&running);

        let thread = std::thread::Builder::new()
            .name("lowlat-net".into())
            .spawn(move || body(wake, &theirs))?;

        Ok(Self {
            thread: Some(thread),
            notify,
            running,
        })
    }

    /// A producer's end of the loop's wake.
    pub fn wake_handle(&self) -> &WakeHandle {
        &self.notify
    }

    /// True while the loop is running: not stopped, and not returned on its
    /// own. A loop that ends itself -- a departure given its grace, a
    /// transport that failed -- is seen here as soon as its thread is done,
    /// not only once `stop` has been asked for.
    pub fn alive(&self) -> bool {
        !self.running.stopping()
            && self
                .thread
                .as_ref()
                .is_some_and(|thread| !thread.is_finished())
    }

    /// Signal, wake, and join.
    ///
    /// Idempotent, and called from `Drop` as well, so a caller that forgets is
    /// not the difference between a clean exit and a stranded thread.
    pub fn stop(&mut self) {
        // Release, pairing with the loop's Acquire. See `Running::stopping`:
        // the ordering is the contract, the wake is what makes it prompt.
        self.running.stop.store(1, Ordering::Release);
        // Then wake it. Setting the flag alone leaves a thread parked in its
        // wait until the deadline expires.
        let _ = self.notify.notify();

        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Guest {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::Shell;
    use crate::socket::Socket;
    use core::net::SocketAddr;
    use lowlat_core::channel::{RecvRing, SlotMeta};
    use lowlat_core::conn::{Conn, Credentials};
    use lowlat_core::endpoint::Endpoint;
    use lowlat_core::envelope::Envelope;
    use lowlat_core::send::{SendRing, SendSlot};
    use lowlat_core::session::Session;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    const SLOT: usize = 256;
    const SLOTS: usize = 32;
    const CHANNEL: u8 = 1;

    /// How a loop ended: its passes, whether the stop reached it as a wake,
    /// when it saw the stop, and when its socket and storage were gone -- so a
    /// slow teardown says which part was slow.
    struct Exit {
        passes: u64,
        woken: bool,
        stopped: Instant,
        released: Instant,
    }

    /// Build a shell with storage owned by the calling thread and run it until
    /// teardown, reporting the address it bound and how it ended.
    fn run(
        wake: Wake,
        running: &Running,
        bound: &mpsc::Sender<SocketAddr>,
        passes: &mpsc::Sender<Exit>,
    ) {
        let mut recv_bodies = vec![0u8; SLOT * SLOTS];
        let mut recv_meta = vec![SlotMeta::default(); SLOTS];
        let mut send_bodies = vec![0u8; SLOT * SLOTS];
        let mut send_meta = vec![SendSlot::default(); SLOTS];

        let conn = Conn::new(
            Credentials {
                local_ufrag: "aaaa",
                local_pwd: "passwordforaaaa",
                remote_ufrag: "bbbb",
                remote_pwd: "passwordforbbbb",
            },
            [0xA1; 16],
            0.0,
        );
        let mut session = Session::new(Envelope::from_key(&[0x11u8; 32]).unwrap(), 1, 0.0);
        session
            .attach_recv(
                CHANNEL,
                RecvRing::new(&mut recv_bodies, &mut recv_meta, SLOT).unwrap(),
            )
            .unwrap();
        session
            .attach_send(
                CHANNEL,
                SendRing::new(&mut send_bodies, &mut send_meta, SLOT, CHANNEL).unwrap(),
            )
            .unwrap();

        let socket = Socket::open(0).unwrap();
        let mut shell = Shell::new(socket, wake, Endpoint::new(conn, session));
        let _ = bound.send(shell.socket().local_addr().unwrap());

        let woke_on_send = |turn: std::io::Result<crate::shell::Turn>| {
            turn.is_ok_and(|turn| turn.woke == crate::shell::Woke::Send)
        };
        let mut count = 0u64;
        let mut woken = false;
        while !running.stopping() {
            woken = woke_on_send(shell.turn(|_| {}));
            count += 1;
        }
        // The stop can land between two passes, where the loop sees the flag
        // before its wait could take the wake the stop posted. One more pass
        // takes that wake at once if it was posted, and waits out the
        // deadline if it never was.
        if !woken {
            woken = woke_on_send(shell.turn(|_| {}));
        }
        let stopped = Instant::now();
        drop(shell);
        let _ = passes.send(Exit {
            passes: count,
            woken,
            stopped,
            released: Instant::now(),
        });
    }

    fn spawn() -> (Guest, mpsc::Receiver<SocketAddr>, mpsc::Receiver<Exit>) {
        let (bound_tx, bound_rx) = mpsc::channel();
        let (passes_tx, passes_rx) = mpsc::channel();
        let wake = Wake::new().expect("wake");
        let guest = Guest::spawn(wake, move |wake, running| {
            run(wake, running, &bound_tx, &passes_tx);
        })
        .expect("spawn");
        (guest, bound_rx, passes_rx)
    }

    #[test]
    fn a_guest_runs_and_stops() {
        let (mut guest, bound, passes) = spawn();
        let addr = bound.recv_timeout(Duration::from_secs(5)).expect("bound");
        assert_ne!(addr.port(), 0);
        assert!(guest.alive());

        guest.stop();
        assert!(!guest.alive());
        // Receiving at all is the property: the thread reached the end of its
        // loop and reported. The count is deliberately not asserted here --
        // teardown can land between the shell being built and the loop's first
        // check, which is a clean exit with zero passes and not a defect.
        passes
            .recv_timeout(Duration::from_secs(5))
            .expect("the loop did not exit cleanly");
    }

    /// The teardown gate. A thread parked in its wait must be woken, not left
    /// to time out: setting a flag and joining is the difference between a
    /// clean disconnect and a multi-second hang.
    #[test]
    fn teardown_wakes_a_parked_thread_promptly() {
        let (mut guest, bound, passes) = spawn();
        bound.recv_timeout(Duration::from_secs(5)).expect("bound");

        // Let it settle into the wait rather than catching it mid-pass.
        std::thread::sleep(Duration::from_millis(120));

        let started = Instant::now();
        guest.stop();
        let took = started.elapsed();
        let exit = passes
            .recv_timeout(Duration::from_secs(5))
            .expect("the loop did not report its end");

        // **The wake is shown by why the loop left, not by how long it
        // took.** A clock cannot tell a thread that was woken and then waited
        // for a processor from one that sat out its wait: on a busy Windows
        // machine a thread waits a whole scheduling quantum, two clock ticks,
        // for a processor, and a woken loop has been measured seeing the stop
        // later than the wait cap. The pass that saw the stop was either
        // woken by the stop's wake or it timed out, and only the first is a
        // teardown that wakes.
        assert!(
            exit.woken,
            "the loop left on its deadline: the stop never reached it as a wake"
        );

        // And no hang: a teardown that strands its thread -- a release that
        // waits out its bound, a wake that never lands -- is the multi-second
        // disconnect this exists to prevent. Where the time went is in the
        // message, because a slow teardown reads the same from outside
        // whichever part was slow.
        let hang = Duration::from_secs(1);
        assert!(
            took < hang,
            "teardown took {took:?}: the loop saw the stop after {:?}, released its socket \
             and storage in {:?}, and the thread's exit and join took the rest",
            exit.stopped.saturating_duration_since(started),
            exit.released.saturating_duration_since(exit.stopped),
        );

        // Here the count *is* guaranteed, because the sleep above is longer
        // than a full wait, so the loop completed passes before being stopped.
        assert!(
            exit.passes > 0,
            "the loop never ran a pass despite settling into its wait"
        );
    }

    /// Churn. A teardown race shows up across many cycles or not at all, and
    /// this is also the seed of the connect-and-teardown soak, which is where
    /// per-cycle leaks in descriptors and threads become visible.
    #[test]
    fn many_spawn_and_teardown_cycles_all_complete() {
        let cycles = 64;
        for _ in 0..cycles {
            let (mut guest, bound, passes) = spawn();
            bound.recv_timeout(Duration::from_secs(5)).expect("bound");
            guest.stop();
            // Clean exit is the property under churn, not how much work each
            // cycle managed. A cycle torn down before its first check exits
            // with zero passes, which is correct.
            passes
                .recv_timeout(Duration::from_secs(5))
                .expect("a cycle did not exit cleanly");
        }
    }

    #[test]
    fn stopping_twice_is_harmless() {
        let (mut guest, bound, _passes) = spawn();
        bound.recv_timeout(Duration::from_secs(5)).expect("bound");
        guest.stop();
        guest.stop();
    }

    /// Dropping without stopping must not leak the thread, because a caller
    /// that forgets is otherwise the difference between a clean exit and a
    /// stranded loop.
    #[test]
    fn dropping_tears_down() {
        let (guest, bound, passes) = spawn();
        bound.recv_timeout(Duration::from_secs(5)).expect("bound");
        drop(guest);
        passes
            .recv_timeout(Duration::from_secs(5))
            .expect("dropping did not tear the loop down");
    }
}
