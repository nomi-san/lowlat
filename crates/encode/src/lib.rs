//! Video encoders. Submit and poll are separate operations so the trait
//! cannot express a serialized capture-to-encode loop.
//!
//! See docs/05-host.md section 4.

// Phase 5 lands the hardware backend; Phase 11 the software one.
