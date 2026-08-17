//! The open-stack encoder.
//!
//! Second backend, and the reason the encoder interface will be a trait rather
//! than one concrete type: an interface shaped by a single implementation
//! encodes that implementation's assumptions, which is what the phase gate
//! requiring two backends exists to prevent.
//!
//! **This one is not the zero-copy pairing on every machine, and neither is the
//! other.** Which backend can take a captured frame without a copy depends on
//! which device drives the display, discovered at construction
//! ([`crate::cuda`] carries the same rule). Where they disagree the pipeline
//! refuses rather than inserting a transfer, per docs/05-host.md section 4.
//!
//! Loaded at runtime like everything else here, so a machine without the
//! driver has a missing backend rather than a process that will not start.

use core::ffi::{CStr, c_char, c_int};

use lowlat_common::dynlib::Library;

use crate::ffi::va::{
    VA_STATUS_SUCCESS, VADisplay, VAEntrypoint, VAEntrypointEncSlice, VAProfile, VAProfileH264High,
    VAProfileH264Main, VAProfileHEVCMain, VAStatus,
};

/// The core interface and its display binding, versioned.
const LIBVA: [&CStr; 2] = [c"libva.so.2", c"libva.so"];
const LIBVA_DRM: [&CStr; 2] = [c"libva-drm.so.2", c"libva-drm.so"];

type GetDisplayDrm = unsafe extern "C" fn(c_int) -> VADisplay;
type Initialize = unsafe extern "C" fn(VADisplay, *mut c_int, *mut c_int) -> VAStatus;
type Terminate = unsafe extern "C" fn(VADisplay) -> VAStatus;
type MaxNumProfiles = unsafe extern "C" fn(VADisplay) -> c_int;
type QueryConfigProfiles = unsafe extern "C" fn(VADisplay, *mut VAProfile, *mut c_int) -> VAStatus;
type MaxNumEntrypoints = unsafe extern "C" fn(VADisplay) -> c_int;
type QueryConfigEntrypoints =
    unsafe extern "C" fn(VADisplay, VAProfile, *mut VAEntrypoint, *mut c_int) -> VAStatus;
type ErrorStr = unsafe extern "C" fn(VAStatus) -> *const c_char;

/// Why the backend could not be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// No such library, which is the ordinary case without the driver.
    Unavailable,
    /// Loaded, but missing an entry point it must export.
    MissingSymbol,
    /// The render node could not be opened. Usually a permission on the device
    /// rather than an absent device.
    NoDevice,
    /// A call failed, carrying its status.
    Status(VAStatus),
    /// The driver has no encode entry point for any codec we speak. A decode
    /// only device reaches here, and it is a refusal rather than a fault.
    NoEncoder,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Unavailable => f.write_str("display runtime not present"),
            Self::MissingSymbol => f.write_str("display runtime is missing an entry point"),
            Self::NoDevice => f.write_str("render node could not be opened"),
            Self::Status(status) => write!(f, "display runtime returned status {status}"),
            Self::NoEncoder => f.write_str("device offers no encode entry point"),
        }
    }
}

impl std::error::Error for Error {}

type Result<T> = core::result::Result<T, Error>;

/// A count the interface reported, as a length.
///
/// These are signed and the interface is entitled to report a negative on
/// failure, so a cast would turn one into an enormous length. Converted rather
/// than cast, with the failure folded into zero.
fn count(reported: c_int) -> usize {
    usize::try_from(reported).unwrap_or(0)
}

/// Which bitstream, in this interface's terms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    H264,
    H265,
}

impl Codec {
    /// Profiles worth trying, best first.
    ///
    /// High before Main for H.264: every decoder that matters handles High,
    /// and it is what a peer expects to receive.
    fn profiles(self) -> &'static [VAProfile] {
        match self {
            Self::H264 => &[VAProfileH264High, VAProfileH264Main],
            Self::H265 => &[VAProfileHEVCMain],
        }
    }
}

/// The loaded runtime.
#[derive(Debug)]
pub struct Vaapi {
    initialize: Initialize,
    terminate: Terminate,
    max_num_profiles: MaxNumProfiles,
    query_config_profiles: QueryConfigProfiles,
    max_num_entrypoints: MaxNumEntrypoints,
    query_config_entrypoints: QueryConfigEntrypoints,
    error_str: ErrorStr,
    get_display_drm: GetDisplayDrm,
    /// Last, so both outlive the addresses taken from them.
    _libva_drm: Library,
    _libva: Library,
}

/// An initialised display bound to one render node.
#[derive(Debug)]
pub struct Display<'a> {
    va: &'a Vaapi,
    raw: VADisplay,
    /// Closed after the display is terminated, never before: the driver holds
    /// this descriptor for as long as the display lives.
    fd: c_int,
    version: (i32, i32),
}

impl Vaapi {
    /// Open the runtime.
    pub fn load() -> Result<Self> {
        let libva = Library::open_first(&LIBVA).ok_or(Error::Unavailable)?;
        let libva_drm = Library::open_first(&LIBVA_DRM).ok_or(Error::Unavailable)?;

        // SAFETY: every signature is transcribed from the vendored headers.
        // These names carry no version suffix, unlike the compute runtime's.
        unsafe {
            Ok(Self {
                initialize: libva.symbol(c"vaInitialize").ok_or(Error::MissingSymbol)?,
                terminate: libva.symbol(c"vaTerminate").ok_or(Error::MissingSymbol)?,
                max_num_profiles: libva
                    .symbol(c"vaMaxNumProfiles")
                    .ok_or(Error::MissingSymbol)?,
                query_config_profiles: libva
                    .symbol(c"vaQueryConfigProfiles")
                    .ok_or(Error::MissingSymbol)?,
                max_num_entrypoints: libva
                    .symbol(c"vaMaxNumEntrypoints")
                    .ok_or(Error::MissingSymbol)?,
                query_config_entrypoints: libva
                    .symbol(c"vaQueryConfigEntrypoints")
                    .ok_or(Error::MissingSymbol)?,
                error_str: libva.symbol(c"vaErrorStr").ok_or(Error::MissingSymbol)?,
                get_display_drm: libva_drm
                    .symbol(c"vaGetDisplayDRM")
                    .ok_or(Error::MissingSymbol)?,
                _libva_drm: libva_drm,
                _libva: libva,
            })
        }
    }

    /// What the runtime says a status means. Diagnostic only.
    pub fn status_text(&self, status: VAStatus) -> &str {
        // SAFETY: the interface returns a pointer to a static string it owns.
        let text = unsafe { (self.error_str)(status) };
        if text.is_null() {
            return "unknown";
        }
        // SAFETY: non-null and NUL terminated by contract.
        unsafe { CStr::from_ptr(text) }
            .to_str()
            .unwrap_or("unknown")
    }

    fn check(&self, status: VAStatus) -> Result<()> {
        if status == VA_STATUS_SUCCESS as VAStatus {
            Ok(())
        } else {
            Err(Error::Status(status))
        }
    }

    /// Bind a display to a render node, by path.
    ///
    /// The **render** node rather than the card node: encoding needs no
    /// display control, and the card node additionally needs a privilege this
    /// process should not hold for a job that does not require it.
    pub fn open(&self, node: &CStr) -> Result<Display<'_>> {
        // SAFETY: the path is NUL terminated. No mode is needed without
        // O_CREAT. Closed on the error paths below and in `Drop`.
        let fd = unsafe { libc::open(node.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(Error::NoDevice);
        }

        // SAFETY: the descriptor is open for the duration.
        let raw = unsafe { (self.get_display_drm)(fd) };
        if raw.is_null() {
            // SAFETY: the descriptor was opened above and is not yet owned by
            // anything else.
            unsafe { libc::close(fd) };
            return Err(Error::NoDevice);
        }

        let mut major: c_int = 0;
        let mut minor: c_int = 0;
        // SAFETY: both out pointers are to live locals.
        let status = unsafe { (self.initialize)(raw, &raw mut major, &raw mut minor) };
        if let Err(error) = self.check(status) {
            // SAFETY: initialise failed, so the display owns nothing; the
            // descriptor is still ours to close.
            unsafe { libc::close(fd) };
            return Err(error);
        }

        Ok(Display {
            va: self,
            raw,
            fd,
            version: (major, minor),
        })
    }
}

impl Display<'_> {
    /// The interface version the driver implements.
    pub fn version(&self) -> (i32, i32) {
        self.version
    }

    /// The raw handle, for the calls that take one.
    pub fn raw(&self) -> VADisplay {
        self.raw
    }

    /// The profile this driver will encode `codec` with, if any.
    ///
    /// **Asked, not assumed.** A device may decode a codec and not encode it,
    /// and the two are separate entry points against the same profile; taking
    /// the profile's presence as proof of encode support is the mistake this
    /// exists to avoid.
    pub fn encode_profile(&self, codec: Codec) -> Result<VAProfile> {
        let mut profiles = vec![0 as VAProfile; self.max_profiles()?];
        let mut found: c_int = 0;
        // SAFETY: the buffer is writable for the length the interface was told
        // about, and the count is written back.
        let status = unsafe {
            (self.va.query_config_profiles)(self.raw, profiles.as_mut_ptr(), &raw mut found)
        };
        self.va.check(status)?;
        let available = profiles.get(..count(found)).unwrap_or(&[]);

        for wanted in codec.profiles() {
            if !available.contains(wanted) {
                continue;
            }
            if self.has_encode_entrypoint(*wanted)? {
                return Ok(*wanted);
            }
        }
        Err(Error::NoEncoder)
    }

    fn max_profiles(&self) -> Result<usize> {
        // SAFETY: the display is live.
        Ok(count(unsafe { (self.va.max_num_profiles)(self.raw) }))
    }

    fn has_encode_entrypoint(&self, profile: VAProfile) -> Result<bool> {
        // SAFETY: the display is live.
        let capacity = count(unsafe { (self.va.max_num_entrypoints)(self.raw) });
        let mut entrypoints = vec![0 as VAEntrypoint; capacity];
        let mut found: c_int = 0;
        // SAFETY: the buffer is writable for the length passed.
        let status = unsafe {
            (self.va.query_config_entrypoints)(
                self.raw,
                profile,
                entrypoints.as_mut_ptr(),
                &raw mut found,
            )
        };
        self.va.check(status)?;
        Ok(entrypoints
            .get(..count(found))
            .unwrap_or(&[])
            .contains(&VAEntrypointEncSlice))
    }
}

impl Drop for Display<'_> {
    fn drop(&mut self) {
        // SAFETY: the display came from a successful initialise and is
        // terminated once. The descriptor is closed after, never before: the
        // driver holds it for the display's life.
        unsafe {
            (self.va.terminate)(self.raw);
            libc::close(self.fd);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The open-stack device on this machine. Not the one driving the display,
    /// which is the point: this backend is the non-zero-copy one here.
    const NODE: &CStr = c"/dev/dri/renderD128";

    #[test]
    fn profiles_are_listed_best_first() {
        // High before Main, because every decoder that matters takes High and
        // it is what a peer expects.
        assert_eq!(Codec::H264.profiles()[0], VAProfileH264High);
        assert!(Codec::H264.profiles().contains(&VAProfileH264Main));
        assert_eq!(Codec::H265.profiles(), &[VAProfileHEVCMain]);
    }

    /// Needs the open-stack driver, so it is off by default. Run with
    /// `cargo test -p lowlat-encode -- --ignored`.
    #[test]
    #[ignore = "requires the open-stack driver"]
    fn the_display_opens_and_reports_an_encoder() {
        let va = match Vaapi::load() {
            Ok(va) => va,
            Err(Error::Unavailable) => panic!("no runtime present; this test needs the driver"),
            Err(error) => panic!("{error}"),
        };
        let display = va.open(NODE).expect("render node did not open");
        let (major, minor) = display.version();
        println!("display interface {major}.{minor}");
        assert!(major >= 1);

        for codec in [Codec::H264, Codec::H265] {
            let profile = display.encode_profile(codec).expect("no encode profile");
            println!("  {codec:?} encodes with profile {profile}");
        }

        // A profile the driver does not encode must be refused rather than
        // substituted, which is what makes the answers above mean anything.
        assert!(
            !display
                .has_encode_entrypoint(crate::ffi::va::VAProfileNone)
                .expect("query"),
            "the driver claims an encode entry point for no profile at all"
        );
    }
}
