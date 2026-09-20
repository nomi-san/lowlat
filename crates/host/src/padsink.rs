//! A pad's own reports, to the application instead of to a device
//! (docs/05-host.md section 7.2).
//!
//! **The microphone's shape, for the microphone's reason.** A DualSense
//! reports several hundred times a second, which is the wrong rate for the
//! event queue, so the reports have a queue of their own that an application
//! parks a thread on. The wake is the futex pair, so a report is in the
//! application's hands one cross-thread wake after it was parsed.
//!
//! **Latest wins.** A pad's state is what it is now; when the application
//! falls behind, the oldest input report goes and is counted, never the
//! newest -- and never a feature report, of which there are at most two per
//! pad and which the application's device needs before the first input, nor
//! the pad's end, which is what the application destroys that device on.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use lowlat_common::clock;
use lowlat_core::pad::{self, Product};
use lowlat_inject::uinput::Forwarded;

/// Reports held for an application that is not draining: a quarter of a
/// second of one wired DualSense.
const MAX_REPORTS: usize = 64;

/// Which of a pad's reports this is, or that there will be no more.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Input,
    Feature,
    /// The pad is gone, unplugged by its peer or with its guest; `len` is
    /// zero. It comes after the pad's last report, so a device destroyed on
    /// it has seen them all.
    Unplug,
}

/// One report, as the guest thread handed it over.
#[derive(Debug, Clone, Copy)]
pub struct Report {
    pub guest: u32,
    pub pad: u32,
    pub product: Product,
    pub kind: Kind,
    pub len: usize,
    /// The USB form, identifier byte first.
    pub report: [u8; pad::INPUT_LEN],
}

impl Report {
    /// What the guest's injector forwarded, as this queue carries it.
    #[must_use]
    pub fn of(guest: u32, forwarded: Forwarded) -> Self {
        match forwarded {
            Forwarded::Input {
                pad,
                product,
                report,
            } => Self {
                guest,
                pad,
                product,
                kind: Kind::Input,
                len: pad::INPUT_LEN,
                report,
            },
            Forwarded::Feature {
                pad,
                product,
                len,
                report,
            } => Self {
                guest,
                pad,
                product,
                kind: Kind::Feature,
                len,
                report,
            },
            Forwarded::Unplugged { pad, product } => Self {
                guest,
                pad,
                product,
                kind: Kind::Unplug,
                len: 0,
                report: [0; pad::INPUT_LEN],
            },
        }
    }
}

#[derive(Debug, Default)]
struct State {
    queued: VecDeque<Report>,
    /// Dropped for want of room, reported with the next delivery.
    dropped: u32,
}

impl State {
    fn push(&mut self, report: Report) {
        while self.queued.len() >= MAX_REPORTS {
            // The oldest input report goes; a feature report or a pad's end
            // never does, and if only those are held the oldest of them goes
            // rather than the one arriving, which is then the newest.
            let at = self
                .queued
                .iter()
                .position(|r| r.kind == Kind::Input)
                .unwrap_or(0);
            if self.queued.remove(at).is_some() {
                self.dropped = self.dropped.saturating_add(1);
            }
        }
        self.queued.push_back(report);
    }
}

#[derive(Debug, Default)]
struct Shared {
    /// Bumped on every push, and the address a waiting consumer parks on.
    arrivals: AtomicU32,
    state: Mutex<State>,
}

impl Shared {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|held| held.into_inner())
    }
}

/// Where the reports are put. Cloned into every guest thread.
#[derive(Clone)]
pub struct Sender {
    shared: Arc<Shared>,
}

impl core::fmt::Debug for Sender {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("padsink::Sender").finish()
    }
}

impl Sender {
    pub fn send(&self, report: Report) {
        self.shared.state().push(report);
        self.shared.arrivals.fetch_add(1, Ordering::Release);
        lowlat_common::wait::notify_one(&self.shared.arrivals);
    }
}

/// Where they are taken from. **One consumer**, which is what lets the
/// dropped count be reported exactly once.
#[derive(Debug)]
pub struct Receiver {
    shared: Arc<Shared>,
}

/// What one take produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Taken {
    /// Nothing arrived before the timeout.
    Empty,
    /// A report, already copied into the caller's buffer.
    Took {
        guest: u32,
        pad: u32,
        product: Product,
        kind: Kind,
        len: usize,
        dropped: u32,
    },
}

impl Receiver {
    /// Take one report into the caller's buffer, waiting up to `timeout`.
    /// The buffer must hold [`pad::INPUT_LEN`]; a shorter one takes what
    /// fits, which is never what the caller wants.
    pub fn recv_timeout_into(&self, timeout: Duration, out: &mut [u8]) -> Taken {
        let began = clock::Time::now();
        loop {
            // Sampled before the queue is found empty, so a push landing
            // between the two cannot be slept through.
            let seen = self.shared.arrivals.load(Ordering::Acquire);
            {
                let mut state = self.shared.state();
                if let Some(report) = state.queued.pop_front() {
                    let len = report.len.min(out.len()).min(report.report.len());
                    if let (Some(target), Some(source)) =
                        (out.get_mut(..len), report.report.get(..len))
                    {
                        target.copy_from_slice(source);
                    }
                    return Taken::Took {
                        guest: report.guest,
                        pad: report.pad,
                        product: report.product,
                        kind: report.kind,
                        len,
                        dropped: core::mem::take(&mut state.dropped),
                    };
                }
            }
            let waited = Duration::from_secs_f64(clock::elapsed_ms(began) / 1000.0);
            let Some(left) = timeout.checked_sub(waited) else {
                return Taken::Empty;
            };
            if left.is_zero() {
                return Taken::Empty;
            }
            lowlat_common::wait::wait(&self.shared.arrivals, seen, left);
        }
    }
}

/// One queue, as the two ends of it.
#[must_use]
pub fn queue() -> (Sender, Receiver) {
    let shared = Arc::new(Shared::default());
    (
        Sender {
            shared: Arc::clone(&shared),
        },
        Receiver { shared },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(pad: u32, kind: Kind, first: u8) -> Report {
        let mut bytes = [0u8; pad::INPUT_LEN];
        bytes[0] = first;
        Report {
            guest: 1,
            pad,
            product: Product::DualSense,
            kind,
            len: if kind == Kind::Input { 64 } else { 41 },
            report: bytes,
        }
    }

    /// Reports come out in order, with what was copied and how long it is.
    #[test]
    fn reports_come_out_in_order() {
        let (tx, rx) = queue();
        tx.send(report(3, Kind::Feature, 0x05));
        tx.send(report(3, Kind::Input, 0x01));
        let mut out = [0u8; pad::INPUT_LEN];
        assert_eq!(
            rx.recv_timeout_into(Duration::ZERO, &mut out),
            Taken::Took {
                guest: 1,
                pad: 3,
                product: Product::DualSense,
                kind: Kind::Feature,
                len: 41,
                dropped: 0
            }
        );
        assert_eq!(out[0], 0x05);
        assert!(matches!(
            rx.recv_timeout_into(Duration::ZERO, &mut out),
            Taken::Took {
                kind: Kind::Input,
                len: 64,
                ..
            }
        ));
        assert_eq!(out[0], 0x01);
        assert_eq!(rx.recv_timeout_into(Duration::ZERO, &mut out), Taken::Empty);
    }

    /// **The oldest input report goes when nobody drains, never a feature
    /// report**, and the count travels with the next delivery.
    #[test]
    fn a_full_queue_drops_the_oldest_input_and_keeps_the_features() {
        let (tx, rx) = queue();
        tx.send(report(3, Kind::Feature, 0xF0));
        for i in 0..MAX_REPORTS {
            tx.send(report(3, Kind::Input, u8::try_from(i).unwrap()));
        }
        // 65 sent into 64 slots: the oldest input (0) went.
        let mut out = [0u8; pad::INPUT_LEN];
        let first = rx.recv_timeout_into(Duration::ZERO, &mut out);
        assert!(matches!(
            first,
            Taken::Took {
                kind: Kind::Feature,
                dropped: 1,
                ..
            }
        ));
        assert_eq!(out[0], 0xF0);
        let second = rx.recv_timeout_into(Duration::ZERO, &mut out);
        assert!(matches!(second, Taken::Took { dropped: 0, .. }));
        assert_eq!(out[0], 1, "the oldest input report should have gone");
    }

    /// **A pad's end travels as a report of its own, after the pad's last
    /// one**, and is never dropped for want of room.
    #[test]
    fn a_pads_end_comes_after_its_last_report_and_is_kept() {
        let (tx, rx) = queue();
        for i in 0..MAX_REPORTS {
            tx.send(report(3, Kind::Input, u8::try_from(i).unwrap()));
        }
        tx.send(Report::of(
            1,
            Forwarded::Unplugged {
                pad: 3,
                product: Product::DualSense,
            },
        ));
        let mut out = [0u8; pad::INPUT_LEN];
        let mut taken = Vec::new();
        while let Taken::Took { kind, len, .. } = rx.recv_timeout_into(Duration::ZERO, &mut out) {
            taken.push((kind, len));
        }
        assert_eq!(taken.len(), MAX_REPORTS);
        assert_eq!(taken.last(), Some(&(Kind::Unplug, 0)));
        assert!(
            taken[..MAX_REPORTS - 1]
                .iter()
                .all(|t| *t == (Kind::Input, 64))
        );
    }

    /// A consumer parked on the queue is woken by a push.
    #[test]
    fn a_push_wakes_a_parked_consumer() {
        let (tx, rx) = queue();
        let producer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            tx.send(report(9, Kind::Input, 0x01));
        });
        let began = std::time::Instant::now();
        let mut out = [0u8; pad::INPUT_LEN];
        let taken = rx.recv_timeout_into(Duration::from_secs(5), &mut out);
        assert!(matches!(taken, Taken::Took { pad: 9, .. }));
        assert!(began.elapsed() < Duration::from_secs(2));
        producer.join().unwrap();
    }
}
