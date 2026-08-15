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

    /// True while the loop is still meant to be running.
    pub fn alive(&self) -> bool {
        !self.running.stopping()
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

    /// Build a shell with storage owned by the calling thread and run it until
    /// teardown, reporting the address it bound and how many passes it made.
    fn run(
        wake: Wake,
        running: &Running,
        bound: &mpsc::Sender<SocketAddr>,
        passes: &mpsc::Sender<u64>,
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

        let started = Instant::now();
        let mut count = 0u64;
        while !running.stopping() {
            let now = started.elapsed().as_secs_f64() * 1000.0;
            let _ = shell.turn(now, |_| {});
            count += 1;
        }
        let _ = passes.send(count);
    }

    fn spawn() -> (Guest, mpsc::Receiver<SocketAddr>, mpsc::Receiver<u64>) {
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
        assert!(
            passes.recv_timeout(Duration::from_secs(1)).expect("passes") > 0,
            "the loop never ran a pass"
        );
    }

    /// The teardown gate. A thread parked in its wait must be woken, not left
    /// to time out: setting a flag and joining is the difference between a
    /// clean disconnect and a multi-second hang.
    #[test]
    fn teardown_wakes_a_parked_thread_promptly() {
        let (mut guest, bound, _passes) = spawn();
        bound.recv_timeout(Duration::from_secs(5)).expect("bound");

        // Let it settle into the wait rather than catching it mid-pass.
        std::thread::sleep(Duration::from_millis(120));

        let started = Instant::now();
        guest.stop();
        let took = started.elapsed();

        // The threshold has to sit well below the loop's wait cap. At or above
        // it, a teardown that only set the flag would pass too: the thread
        // would time out into the same check and join looking prompt. Below it,
        // only the wake can explain the result.
        let cap = Duration::from_millis(crate::shell::MAX_WAIT_MS as u64);
        assert!(
            took < cap / 2,
            "teardown took {took:?} against a {cap:?} wait, so the thread timed out rather than being woken"
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
            assert!(
                passes.recv_timeout(Duration::from_secs(1)).expect("passes") > 0,
                "a cycle produced no passes"
            );
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
        assert!(
            passes.recv_timeout(Duration::from_secs(1)).expect("passes") > 0,
            "the loop did not finish"
        );
    }
}
