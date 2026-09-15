//! The public C ABI: one shared object, one header, two halves.
//!
//! The only public surface there is ([06-api.md](../docs/06-api.md)). Every
//! `extern "C"` entry point catches unwinding (06 section 9). The host half and
//! the client half are features, so a build can carry one without the other
//! and the header says which through the same names.

pub mod abi;
