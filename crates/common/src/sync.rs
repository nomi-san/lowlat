//! Atomics and interior mutability, swappable for model checking.
//!
//! Public because more than one crate builds a cross-thread handoff and every
//! one of them carries the obligation to be model checked. A second copy of
//! this shim would be a second thing to keep in step.
//!
//! Under `cfg(loom)` these resolve to loom's instrumented types so the
//! scheduler can explore interleavings. Otherwise they are the real thing with
//! no overhead. The closure-based cell API is loom's; the plain implementation
//! matches it so one body of code satisfies both.

#[cfg(loom)]
pub use loom::cell::UnsafeCell;
#[cfg(loom)]
pub use loom::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

#[cfg(not(loom))]
pub use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

#[cfg(not(loom))]
#[derive(Debug)]
pub struct UnsafeCell<T>(core::cell::UnsafeCell<T>);

#[cfg(not(loom))]
impl<T> UnsafeCell<T> {
    pub fn new(value: T) -> Self {
        Self(core::cell::UnsafeCell::new(value))
    }

    pub fn with<R>(&self, f: impl FnOnce(*const T) -> R) -> R {
        f(self.0.get())
    }

    pub fn with_mut<R>(&self, f: impl FnOnce(*mut T) -> R) -> R {
        f(self.0.get())
    }
}
