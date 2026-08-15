//! The reassembler, driven by adversarial fragments.
//!
//! The input is chopped into fragments and offered at attacker-chosen
//! sequences, so gaps, duplicates, out-of-window writes, and lying length
//! prefixes all get exercised. Nothing here may panic and no message may be
//! delivered longer than the caller's buffer.
#![no_main]

use libfuzzer_sys::fuzz_target;
use lowlat_core::channel::{RecvRing, SlotMeta};

const SLOT: usize = 64;
const SLOTS: usize = 16;

fuzz_target!(|data: &[u8]| {
    let mut bodies = [0u8; SLOT * SLOTS];
    let mut meta = [SlotMeta::default(); SLOTS];
    let Ok(mut ring) = RecvRing::new(&mut bodies, &mut meta, SLOT) else {
        return;
    };
    let mut out = [0u8; SLOT * SLOTS];

    // First byte picks a sequence, the rest is the fragment; repeat until the
    // input is consumed.
    let mut rest = data;
    while let Some((&head, tail)) = rest.split_first() {
        let take = usize::from(head).min(SLOT).min(tail.len());
        let (body, next) = tail.split_at(take);
        ring.store(u32::from(head) % (SLOTS as u32 * 2), body);
        while let Some(result) = ring.take_message(&mut out) {
            if let Ok(len) = result {
                assert!(len <= out.len(), "delivered past the caller's buffer");
            } else {
                break;
            }
        }
        if head % 7 == 0 {
            let _ = ring.escape_stall(|_| false);
        }
        rest = next;
    }
});
