//! The device interfaces reached at runtime, and nothing above them.
//!
//! Every codec backend on this platform speaks to its device through a
//! library that is opened by name when the backend is built, never linked:
//! a machine without a driver has a missing backend rather than a process
//! that will not start. What is here is exactly that seam -- the generated
//! bindings, the loaders that resolve the entry points, and the handles that
//! bind a device -- shared by the encoders and the decoders so the two halves
//! resolve one table rather than two that drift apart. The one library here
//! that is not a device's is the codec library the client decodes in
//! software through, reached the same way and trusted only on its own word
//! about its licence.
//!
//! Nothing here encodes or decodes a picture. The pipelines that do live
//! above, in `lowlat-encode` and `lowlat-decode`.

/// AMD's own video runtime, reached on Windows alone.
#[cfg(windows)]
pub mod amf;
pub mod cuda;
pub mod cuvid;
/// The system's video decoding interface, which exists on Windows alone.
#[cfg(windows)]
pub mod d3d11;
pub mod ffi;
pub mod lavc;
/// The open stack's video interface, which exists on Linux alone.
#[cfg(target_os = "linux")]
pub mod va;
/// Intel's own video runtime, reached on Windows alone.
#[cfg(windows)]
pub mod vpl;
