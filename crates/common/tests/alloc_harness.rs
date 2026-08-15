//! Phase 0 gate: the zero-allocation harness works, and it can fail.
//!
//! A check that cannot fail proves nothing. `harness_can_fail` is the more
//! important of the two tests here.

#![cfg(feature = "alloc-counter")]

use lowlat_common::alloc_counter::{self, Counting};
use lowlat_common::spsc::Ring;

#[global_allocator]
static ALLOC: Counting = Counting;

#[test]
fn allocation_free_work_passes() {
    let mut buffer = [0u8; 64];
    alloc_counter::assert_no_alloc(|| {
        let mut value: u8 = 0;
        for byte in buffer.iter_mut() {
            *byte = value;
            value = value.wrapping_add(1);
        }
    });
    assert_eq!(buffer[63], 63);
}

/// The harness self-test. If this stops failing, every zero-allocation
/// assertion in the workspace has silently stopped checking anything.
#[test]
#[should_panic(expected = "hot path allocated")]
fn harness_can_fail() {
    alloc_counter::assert_no_alloc(|| {
        let vector: Vec<u8> = Vec::with_capacity(64);
        std::hint::black_box(vector);
    });
}

/// The ring is a data-path type. Pushing and popping must not allocate.
#[test]
fn ring_operations_do_not_allocate() {
    let ring: Ring<u64, 32> = Ring::new();
    alloc_counter::assert_no_alloc(|| {
        for i in 0..32 {
            ring.push(i).unwrap();
        }
        for _ in 0..32 {
            std::hint::black_box(ring.pop());
        }
    });
}

/// Wire field access must not allocate.
#[test]
fn byte_accessors_do_not_allocate() {
    use lowlat_common::bytes;
    let mut buffer = [0u8; 32];
    alloc_counter::assert_no_alloc(|| {
        bytes::write_u32_be(&mut buffer, 0, 0xDEAD_BEEF).unwrap();
        std::hint::black_box(bytes::read_u32_be(&buffer, 0));
    });
}
