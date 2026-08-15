//! Counting allocator for the zero-allocation assertions.
//!
//! Hot paths allocate nothing. That rule decays unless it is mechanically
//! checked, so tests wrap the path under test in [`assert_no_alloc`].
//!
//! The counter is thread-local, so tests may run in parallel without seeing
//! each other's allocations.
//!
//! **The harness is itself verified**: `tests/alloc_harness.rs` contains a test
//! that allocates on purpose and must fail the assertion. A check that cannot
//! fail proves nothing, and one in this repository already reported success
//! while examining zero files.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

thread_local! {
    static COUNT: Cell<u64> = const { Cell::new(0) };
}

fn bump() {
    // `try_with` because a thread may allocate while its locals are being torn
    // down. Const initialisation means this never allocates re-entrantly.
    let _ = COUNT.try_with(|count| count.set(count.get() + 1));
}

/// Allocations made by the current thread since it started.
pub fn count() -> u64 {
    COUNT.try_with(Cell::get).unwrap_or(0)
}

/// Run `body`, then assert it made no allocations on this thread.
///
/// # Panics
///
/// If `body` allocated. That is the point.
pub fn assert_no_alloc<R>(body: impl FnOnce() -> R) -> R {
    let before = count();
    let result = body();
    let after = count();
    assert!(
        after == before,
        "hot path allocated {} time(s); see docs/08-testing.md 8",
        after - before
    );
    result
}

/// Wraps the system allocator and counts allocations per thread.
///
/// Install in a test binary with:
///
/// ```ignore
/// #[global_allocator]
/// static ALLOC: lowlat_common::alloc_counter::Counting = lowlat_common::alloc_counter::Counting;
/// ```
#[derive(Debug)]
pub struct Counting;

// SAFETY: every method forwards to the system allocator unchanged. The counter
// is thread-local and touches no allocator state.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        bump();
        // SAFETY: the caller upholds GlobalAlloc's contract for `layout`.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // Deallocation is not counted. The rule is about allocation on a hot
        // path; a free of something allocated at setup is fine.
        // SAFETY: the caller upholds GlobalAlloc's contract.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        bump();
        // SAFETY: the caller upholds GlobalAlloc's contract.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        bump();
        // SAFETY: the caller upholds GlobalAlloc's contract.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}
