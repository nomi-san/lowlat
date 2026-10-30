//! One answer handed from a thread of someone else's to the thread whose call
//! it answers.
//!
//! A system framework that takes a callback may run it on a thread of its own,
//! even for a call that waits for it, and gives the callback its answer there.
//! What the callback is given goes through here: posted on the framework's
//! thread, taken on the caller's once the call has returned. The framework's
//! own ordering between its callback and its return is not something this side
//! can see, so the hand-over carries its own: the answer is published by the
//! state word, a take that finds nothing posted takes nothing, and nothing
//! half written is ever taken.

use crate::sync::{AtomicU32, AtomicUsize, Ordering};

/// Nothing waiting.
const EMPTY: u32 = 0;
/// A poster holds the slot and is writing the answer.
const WRITING: u32 = 1;
/// An answer is waiting, whole.
const POSTED: u32 = 2;

/// A slot for one answer: a word and a status beside it.
#[derive(Debug)]
pub struct Handoff {
    state: AtomicU32,
    value: AtomicUsize,
    status: AtomicU32,
}

impl Default for Handoff {
    fn default() -> Self {
        Self::new()
    }
}

impl Handoff {
    pub fn new() -> Self {
        Self {
            state: AtomicU32::new(EMPTY),
            value: AtomicUsize::new(0),
            status: AtomicU32::new(0),
        }
    }

    /// Post `value` with `status`. False when an answer is already waiting or
    /// being written, which keeps the slot and refuses this one: its poster
    /// still owns whatever `value` names.
    pub fn post(&self, value: usize, status: u32) -> bool {
        // Acquire: the slot is free only once a take has read the last
        // answer out, which its Release store says.
        if self
            .state
            .compare_exchange(EMPTY, WRITING, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return false;
        }
        // Relaxed: published by the Release store of the state below.
        self.value.store(value, Ordering::Relaxed);
        self.status.store(status, Ordering::Relaxed);
        self.state.store(POSTED, Ordering::Release);
        true
    }

    /// The waiting answer, taken out; none when nothing is posted whole. One
    /// thread takes.
    pub fn take(&self) -> Option<(usize, u32)> {
        // Acquire pairs with the poster's Release: a POSTED state carries
        // the answer written before it.
        if self.state.load(Ordering::Acquire) != POSTED {
            return None;
        }
        let answer = (
            self.value.load(Ordering::Relaxed),
            self.status.load(Ordering::Relaxed),
        );
        // Release: the next poster's Acquire sees the slot free only after
        // this answer was read.
        self.state.store(EMPTY, Ordering::Release);
        Some(answer)
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    /// What is posted is what is taken, once; a second post while one waits
    /// is refused and leaves the first.
    #[test]
    fn an_answer_is_taken_once_and_a_second_post_is_refused() {
        let slot = Handoff::new();
        assert_eq!(slot.take(), None);
        assert!(slot.post(0x1000, 7));
        assert!(!slot.post(0x2000, 8), "a second answer was taken in");
        assert_eq!(slot.take(), Some((0x1000, 7)));
        assert_eq!(slot.take(), None, "an answer was taken twice");
        assert!(slot.post(0x3000, 9), "the slot stayed held after a take");
        assert_eq!(slot.take(), Some((0x3000, 9)));
    }
}

#[cfg(loom)]
mod loom_tests {
    use super::*;

    /// **A take sees an answer whole or not at all**, posted on another
    /// thread while the taker looks: never the state without the value
    /// written before it.
    #[test]
    fn an_answer_is_taken_whole() {
        loom::model(|| {
            let slot = loom::sync::Arc::new(Handoff::new());
            let poster = {
                let slot = slot.clone();
                loom::thread::spawn(move || {
                    assert!(slot.post(0xA5, 3));
                })
            };
            let taken = loop {
                if let Some(answer) = slot.take() {
                    break answer;
                }
                loom::thread::yield_now();
            };
            assert_eq!(taken, (0xA5, 3), "a torn answer");
            poster.join().expect("the poster");
        });
    }

    /// **Of two posters, one is taken and the other refused**, so a value is
    /// never overwritten while its owner thinks it was handed over.
    #[test]
    fn of_two_posters_one_is_taken_and_one_refused() {
        loom::model(|| {
            let slot = loom::sync::Arc::new(Handoff::new());
            let posters: Vec<_> = [(0x10, 1), (0x20, 2)]
                .into_iter()
                .map(|(value, status)| {
                    let slot = slot.clone();
                    loom::thread::spawn(move || slot.post(value, status).then_some((value, status)))
                })
                .collect();
            let posted: Vec<_> = posters
                .into_iter()
                .filter_map(|p| p.join().expect("a poster"))
                .collect();
            assert_eq!(posted.len(), 1, "both posts were taken in");
            assert_eq!(slot.take(), posted.first().copied());
        });
    }
}
