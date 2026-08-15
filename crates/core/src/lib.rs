//! Sans-IO protocol core: wire, channels, rings, crypto, recovery, connectivity.
//!
//! This crate is `no_std` by design, not by portability ambition. It removes
//! `std::time`, `std::net`, `std::thread`, and the allocator by construction,
//! so a sans-IO violation is a compile error rather than a review finding.
//! See docs/00-overview.md D4.

#![no_std]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

// Phase 1 lands the contents. See docs/impl-plan.md.
