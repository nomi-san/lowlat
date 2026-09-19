//! The device interfaces reached at runtime, and nothing above them.
//!
//! Every codec backend on this platform speaks to its device through a
//! library that is opened by name when the backend is built, never linked:
//! a machine without a driver has a missing backend rather than a process
//! that will not start. What is here is exactly that seam -- the generated
//! bindings, the loaders that resolve the entry points, and the handles that
//! bind a device -- shared by the encoders and the decoders so the two halves
//! resolve one table rather than two that drift apart.
//!
//! Nothing here encodes or decodes a picture. The pipelines that do live
//! above, in `lowlat-encode` and `lowlat-decode`.

pub mod cuda;
pub mod cuvid;
pub mod ffi;
pub mod va;
