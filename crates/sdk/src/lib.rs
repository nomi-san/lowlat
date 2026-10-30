//! The public C ABI: one shared object, one header, two halves.
//!
//! The only public surface there is ([06-api.md](../docs/06-api.md)). Every
//! `extern "C"` entry point catches unwinding (06 section 9). The host half and
//! the client half are features, so a build can carry one without the other
//! and the header says which through the same names.

// On macOS the library carries the client half alone: no host runs there.
#![cfg(any(target_os = "linux", target_os = "macos", windows))]

pub mod abi;

/// **The system libraries the library calls into, named inside it**, so an
/// MSVC link of the static library takes `lowlat.lib` and nothing beside it:
/// the linker reads these as if they were on its command line. They are the
/// ones the compiler lists for a static library of this crate, less the C
/// runtime, which is the application's to choose (06 section 3b). The shared
/// library is linked with them already, so naming them again changes nothing
/// there.
///
/// **They reach a link only inside an object the link pulls in**, which is
/// why the release profile stays fat LTO with one codegen unit: every entry
/// point then shares the one object these are in. Split into partitions -- a
/// debug build, thin LTO, more codegen units -- they can land in one nothing
/// references, and an application's link fails on the system functions named
/// here after a change that had nothing to do with them.
#[cfg(all(windows, target_env = "msvc"))]
#[used]
// SAFETY: a section of linker directives, never read by anything at run time;
// the linker drops it from the image.
#[unsafe(link_section = ".drectve")]
static SYSTEM_LIBRARIES: [u8; 262] = *b" /DEFAULTLIB:legacy_stdio_definitions.lib \
/DEFAULTLIB:winmm.lib /DEFAULTLIB:synchronization.lib /DEFAULTLIB:bcrypt.lib \
/DEFAULTLIB:advapi32.lib /DEFAULTLIB:kernel32.lib /DEFAULTLIB:ntdll.lib \
/DEFAULTLIB:userenv.lib /DEFAULTLIB:ws2_32.lib /DEFAULTLIB:dbghelp.lib ";
