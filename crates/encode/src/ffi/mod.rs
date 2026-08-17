//! Generated bindings for the vendored encoder headers.
//!
//! Produced by `scripts/gen-nvenc-bindings.sh` and committed rather than built,
//! so no build machine needs a C toolchain. Regenerate only when the vendored
//! headers move; see third_party/nvcodec/PROVENANCE.md for the pin and why it
//! is deliberately not the newest.
//!
//! **Nothing here is linked.** The libraries are opened at runtime, so these are
//! types and function-pointer typedefs only, with no extern block anywhere. That
//! is what makes a missing driver a missing backend rather than a failed start.
//!
//! **Layout is checked by the compiler.** Each generated struct carries an
//! assertion of its size, alignment, and every field offset, derived from the
//! header by clang. A transcription error is therefore a build failure, which is
//! the whole reason these are generated rather than written.

// Generated code follows C naming and declares the full surface of each header
// while we call a subset of it, so the usual lints do not apply. Scoped to the
// generated modules alone rather than set at the crate root.
//
// The entries are named individually rather than as a group. A group allow
// loses to an explicitly configured lint whichever side it is written on, and
// the workspace configures several of these explicitly, so `clippy::all` alone
// leaves two hundred errors standing.
#[allow(
    dead_code,
    unused_imports,
    unnecessary_transmutes,
    non_camel_case_types,
    non_snake_case,
    non_upper_case_globals,
    missing_debug_implementations,
    unreachable_pub,
    clippy::all,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::float_cmp,
    clippy::useless_transmute
)]
pub(crate) mod nvenc;

#[allow(
    dead_code,
    unused_imports,
    unnecessary_transmutes,
    non_camel_case_types,
    non_snake_case,
    non_upper_case_globals,
    missing_debug_implementations,
    unreachable_pub,
    clippy::all,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::float_cmp,
    clippy::useless_transmute
)]
pub(crate) mod cuda;

#[allow(dead_code, non_upper_case_globals, unreachable_pub)]
pub(crate) mod guids;

#[allow(dead_code, non_upper_case_globals, unreachable_pub)]
pub(crate) mod versions;
