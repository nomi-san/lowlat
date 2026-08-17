//! Loading the vendor encoder runtime.
//!
//! The library is opened at runtime rather than linked, so a machine without
//! the driver reports a missing backend instead of failing to start
//! (docs/07-platforms.md section 8).
//!
//! **The version check is the point of this module.** Every structure the
//! interface takes carries a stamp of the header it was compiled against, and
//! the compatibility runs one way: a newer driver accepts an older stamp, an
//! older driver rejects a newer one. It rejects it on *every* call, with a
//! status that says only "invalid version" and names neither number. So the
//! check happens once, here, where both numbers are in hand and the failure can
//! say what it actually is.

use core::ffi::CStr;
use core::mem::MaybeUninit;

use lowlat_common::dynlib::Library;

use crate::ffi::nvenc::{
    NV_ENC_SUCCESS, NV_ENCODE_API_FUNCTION_LIST, NVENCAPI_MAJOR_VERSION, NVENCAPI_MINOR_VERSION,
    NVENCSTATUS,
};
use crate::ffi::versions::NV_ENCODE_API_FUNCTION_LIST_VER;

/// Versioned first. The unversioned alias belongs to the development package
/// and is absent on a machine that merely has the driver.
const SONAMES: [&CStr; 2] = [c"libnvidia-encode.so.1", c"libnvidia-encode.so"];

type CreateInstance = unsafe extern "C" fn(*mut NV_ENCODE_API_FUNCTION_LIST) -> NVENCSTATUS;
type MaxSupportedVersion = unsafe extern "C" fn(*mut u32) -> NVENCSTATUS;

/// Why the runtime could not be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// No such library. The ordinary case on a machine without the hardware,
    /// and not an error worth raising above a debug line.
    Unavailable,
    /// The library loaded but does not export what it must, which means it is
    /// not the library we think it is.
    MissingSymbol,
    /// The driver predates the interface this was built against. Both numbers
    /// are carried because the fix is to compare them.
    DriverTooOld { compiled: Version, driver: Version },
    /// The runtime refused to hand over its function table.
    Status(NVENCSTATUS),
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Unavailable => f.write_str("encoder runtime not present"),
            Self::MissingSymbol => f.write_str("encoder runtime is missing an entry point"),
            Self::DriverTooOld { compiled, driver } => write!(
                f,
                "driver supports interface {driver}, this build needs {compiled} or newer"
            ),
            Self::Status(status) => write!(f, "encoder runtime returned status {status}"),
        }
    }
}

impl std::error::Error for Error {}

/// An interface version, as major and minor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
}

impl Version {
    /// The packing used by the runtime's version query, which is **not** the
    /// packing the structure stamps use. Four bits of minor, the rest major.
    const fn from_packed(packed: u32) -> Self {
        Self {
            major: packed >> 4,
            minor: packed & 0x0F,
        }
    }

    const fn packed(self) -> u32 {
        (self.major << 4) | self.minor
    }

    /// What this build was compiled against.
    pub const COMPILED: Self = Self {
        major: NVENCAPI_MAJOR_VERSION,
        minor: NVENCAPI_MINOR_VERSION,
    };
}

impl core::fmt::Display for Version {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

/// The loaded runtime and its function table.
#[derive(Debug)]
pub struct Api {
    functions: NV_ENCODE_API_FUNCTION_LIST,
    driver: Version,
    /// Declared last so it is dropped last. The table above is a set of code
    /// addresses inside this library; unmapping it first would leave them
    /// dangling.
    _library: Library,
}

impl Api {
    /// Open the runtime, check the version, and take the function table.
    pub fn load() -> Result<Self, Error> {
        let library = Library::open_first(&SONAMES).ok_or(Error::Unavailable)?;

        // SAFETY: both signatures are transcribed from the vendored header and
        // are checked against it by the layout assertions on the types they
        // mention. Neither is called before it is resolved.
        let max_version: MaxSupportedVersion =
            unsafe { library.symbol(c"NvEncodeAPIGetMaxSupportedVersion") }
                .ok_or(Error::MissingSymbol)?;
        // SAFETY: as above.
        let create: CreateInstance =
            unsafe { library.symbol(c"NvEncodeAPICreateInstance") }.ok_or(Error::MissingSymbol)?;

        let mut packed = 0u32;
        // SAFETY: the pointer is to a live local for the duration of the call.
        let status = unsafe { max_version(&raw mut packed) };
        if status != NV_ENC_SUCCESS {
            return Err(Error::Status(status));
        }
        let driver = Version::from_packed(packed);
        if Version::COMPILED.packed() > packed {
            return Err(Error::DriverTooOld {
                compiled: Version::COMPILED,
                driver,
            });
        }

        // The table is zeroed and then stamped. The runtime reads the stamp to
        // decide how much of the structure it may write, so a zero there is a
        // rejection rather than a default.
        let mut functions = MaybeUninit::<NV_ENCODE_API_FUNCTION_LIST>::zeroed();
        // SAFETY: the type is plain data, so an all-zero bit pattern is a valid
        // value of it, and the only field read before the call is the stamp.
        let functions = unsafe {
            (&raw mut (*functions.as_mut_ptr()).version).write(NV_ENCODE_API_FUNCTION_LIST_VER);
            let status = create(functions.as_mut_ptr());
            if status != NV_ENC_SUCCESS {
                return Err(Error::Status(status));
            }
            functions.assume_init()
        };

        Ok(Self {
            functions,
            driver,
            _library: library,
        })
    }

    /// The interface version the driver supports, which is at least
    /// [`Version::COMPILED`].
    pub fn driver_version(&self) -> Version {
        self.driver
    }

    /// The function table. Every entry is optional in the wire sense: the
    /// runtime fills what it implements and leaves the rest null.
    pub fn functions(&self) -> &NV_ENCODE_API_FUNCTION_LIST {
        &self.functions
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The packing here is four bits of minor, unlike the structure stamps,
    /// and confusing the two produces a comparison that is wrong only for
    /// some driver versions. Checked in both directions.
    #[test]
    fn the_query_packing_round_trips() {
        for (major, minor) in [(11, 0), (12, 1), (13, 0), (13, 15)] {
            let version = Version { major, minor };
            assert_eq!(Version::from_packed(version.packed()), version);
        }
        // 13.1 as the runtime reports it.
        assert_eq!(
            Version::from_packed(0xD1),
            Version {
                major: 13,
                minor: 1
            }
        );
    }

    /// A driver newer than the pin must be accepted and an older one refused.
    /// This is the whole reason the header is pinned low, so the comparison
    /// gets a test rather than a comment.
    #[test]
    fn an_older_driver_is_refused_and_a_newer_one_is_not() {
        let compiled = Version::COMPILED.packed();
        assert!(
            compiled
                > Version {
                    major: 10,
                    minor: 0
                }
                .packed()
        );
        assert!(
            compiled
                <= Version {
                    major: 11,
                    minor: 0
                }
                .packed()
        );
        assert!(
            compiled
                <= Version {
                    major: 13,
                    minor: 1
                }
                .packed()
        );
    }

    /// Needs the vendor driver, so it is off by default. Run with
    /// `cargo test -p lowlat-encode -- --ignored`.
    #[test]
    #[ignore = "requires the vendor driver"]
    fn the_runtime_loads_on_this_machine() {
        match Api::load() {
            Ok(api) => {
                let driver = api.driver_version();
                assert!(driver >= Version::COMPILED);
                assert!(
                    api.functions().nvEncOpenEncodeSession.is_some(),
                    "the table came back without its entry points"
                );
                println!(
                    "driver interface {driver}, built against {}",
                    Version::COMPILED
                );
            }
            Err(Error::Unavailable) => panic!("no runtime present; this test needs the driver"),
            Err(error) => panic!("{error}"),
        }
    }
}
