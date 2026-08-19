//! Scanout capture: the composited framebuffer, read from the display device.
//!
//! The backend sits below the compositor, which is what makes it independent of
//! the display stack and lets it run with no session at all. See
//! docs/07-platforms.md section 2.
//!
//! What this module does is enumerate the display pipeline, describe what is
//! being scanned out, and export those buffers for import elsewhere. It does
//! not read a pixel: the framebuffer is handed on as a set of file
//! descriptors, and everything downstream of here works on the device.

use std::fs::{File, OpenOptions};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::Path;

use drm::buffer::{DrmFourcc, DrmModifier};
use drm::control::{Device as ControlDevice, plane};

/// A display device opened for mode-setting queries.
///
/// Held open for the life of the capture: every handle below is scoped to this
/// descriptor and means nothing without it.
#[derive(Debug)]
pub struct Card(File);

impl AsFd for Card {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl drm::Device for Card {}
impl ControlDevice for Card {}

/// What went wrong.
///
/// Errors carry the raw error number rather than an `io::Error`, which
/// allocates and is not `Copy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// The device node could not be opened.
    Open(i32),
    /// The device answered, and said no.
    Device(i32),
    /// Nothing is being scanned out. Either no display is lit, or the caller
    /// lacks the privilege to see another client's framebuffer, which presents
    /// the same way.
    NoScanout,
    /// The framebuffer is in a format this build does not have a name for. Not
    /// fatal in itself, but nothing downstream can import what it cannot
    /// describe.
    UnknownFormat(u32),
    /// A plane did not carry a property the backend needs. Raised at the point
    /// of the read rather than defaulted, because the value that would be
    /// substituted is a plausible one and would place the pointer somewhere
    /// wrong instead of reporting that it is not known.
    MissingProperty(&'static str),
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Open(errno) => write!(f, "cannot open the display device, errno {errno}"),
            Self::Device(errno) => write!(f, "display device refused the request, errno {errno}"),
            Self::NoScanout => f.write_str("nothing is being scanned out"),
            Self::UnknownFormat(fourcc) => write!(f, "unrecognised pixel format {fourcc:#010x}"),
            Self::MissingProperty(name) => write!(f, "plane carries no {name} property"),
        }
    }
}

impl std::error::Error for Error {}

fn device_error(error: &std::io::Error) -> Error {
    Error::Device(error.raw_os_error().unwrap_or(0))
}

/// Reinterpret a plane coordinate.
///
/// The property is a signed 32-bit value carried in an unsigned field. A
/// pointer straddling the left or top edge of the display has a negative
/// coordinate, and reading the raw value as unsigned puts it several billion
/// pixels off the far side instead of slightly off the near one.
fn signed(value: u64) -> i32 {
    let low = u32::try_from(value & 0xffff_ffff).unwrap_or(0);
    i32::from_ne_bytes(low.to_ne_bytes())
}

/// One buffer behind a framebuffer.
///
/// A framebuffer is up to four of these. One vendor's compression scheme
/// presents three with differing pitches where another presents a single tiled
/// buffer, so neither shape may be assumed.
#[derive(Debug, Clone, Copy)]
pub struct Buffer {
    handle: drm::buffer::Handle,
    /// Bytes per row, which is the driver's own alignment and not the width.
    pub pitch: u32,
    /// Where this buffer's data starts, which is nonzero when several planes
    /// share one allocation.
    pub offset: u32,
}

/// What a plane is scanning out.
#[derive(Debug, Clone)]
pub struct Framebuffer {
    /// The kernel's identifier for this framebuffer.
    ///
    /// **A change here means the buffer was redrawn, and nothing more.** Both
    /// planes cycle through a small pool: the display's turns over every frame,
    /// and the pointer's alternates between a handful of buffers as it moves,
    /// carrying the same picture each time. Measured at five distinct
    /// identifiers across eighty redraws with the pointer unchanged, so this
    /// cannot tell a new pointer shape from the old one written elsewhere.
    /// It is a cheap trigger to re-read, never an identity.
    pub id: u32,
    pub width: u32,
    pub height: u32,
    pub format: DrmFourcc,
    /// **Read from the kernel on every scan, never cached.** The compositor
    /// picks both this and the format, and they change whenever a fullscreen
    /// surface takes the display over -- several times a minute in ordinary
    /// use, not only when a display is swapped or the compositor restarts.
    /// Nothing about the buffer announces it: the size, the stride and the
    /// plane count are identical across the change and only the meaning of the
    /// bytes moves (docs/07-platforms.md section 3.3).
    pub modifier: Option<DrmModifier>,
    /// Occupied entries are contiguous from zero.
    pub buffers: [Option<Buffer>; 4],
}

impl Framebuffer {
    /// Buffers actually present, in order.
    pub fn planes(&self) -> impl Iterator<Item = &Buffer> {
        self.buffers.iter().flatten()
    }
}

/// The pointer as the display is drawing it.
///
/// **This is not the pointer's requested visibility.** It says whether
/// something is compositing a pointer onto the screen, which is what a guest
/// needs in order to render one. What an application asked for lives in session
/// state that this backend sits below and cannot read
/// (docs/07-platforms.md section 2.1).
#[derive(Debug, Clone)]
pub struct Cursor {
    pub image: Framebuffer,
    /// Top-left position on the display it is drawn over.
    pub x: i32,
    pub y: i32,
}

/// One scan of the device.
#[derive(Debug, Clone)]
pub struct Layout {
    /// What the display is showing.
    pub primary: Framebuffer,
    /// Absent when nothing is drawing a pointer.
    pub cursor: Option<Cursor>,
}

/// What [`Card::export`] asks for. Close-on-exec and nothing else; see there
/// for why write access is absent.
const EXPORT_FLAGS: u32 = drm::CLOEXEC;

/// Plane classes, as the kernel numbers them.
const PLANE_TYPE_OVERLAY: u64 = 0;
const PLANE_TYPE_PRIMARY: u64 = 1;
const PLANE_TYPE_CURSOR: u64 = 2;

impl Card {
    /// Open a display device by node path, such as `/dev/dri/card0`.
    ///
    /// **Two capabilities are requested, and both are load bearing.** Without
    /// universal planes the kernel hides the cursor and overlay planes and
    /// reports only the primary one, so the pointer looks absent on every
    /// machine. Without the atomic capability a plane carries no position
    /// property at all: the pointer is then visible but its coordinates simply
    /// do not exist, which reads as a pointer parked in the corner rather than
    /// as a missing value. Both are asked for here so a driver that cannot
    /// answer says so once, at open, instead of once per frame.
    ///
    /// Nothing is ever committed through this descriptor. The capabilities
    /// change what the kernel is willing to describe, not what it will accept.
    pub fn open(path: &Path) -> Result<Self, Error> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|error| Error::Open(error.raw_os_error().unwrap_or(0)))?;
        let card = Self(file);
        for capability in [
            drm::ClientCapability::UniversalPlanes,
            drm::ClientCapability::Atomic,
        ] {
            drm::Device::set_client_capability(&card, capability, true)
                .map_err(|error| device_error(&error))?;
        }
        Ok(card)
    }

    /// Describe what the device is scanning out.
    ///
    /// **Not a per-frame call.** It walks every plane and reads their
    /// properties, which allocates in the underlying crate. The steady-state
    /// path holds on to the plane it found and re-reads only the framebuffer;
    /// this is for startup and for noticing that the pipeline changed.
    pub fn scan(&self) -> Result<Layout, Error> {
        let planes = self.plane_handles().map_err(|error| device_error(&error))?;

        let mut primary = None;
        let mut cursor = None;
        for handle in planes {
            let Ok(info) = self.get_plane(handle) else {
                continue;
            };
            // A plane with no framebuffer or no controller is configured but
            // dark, and there is nothing behind it to export.
            let (Some(fb), Some(_crtc)) = (info.framebuffer(), info.crtc()) else {
                continue;
            };
            let class = self
                .plane_property(handle, c"type")
                .unwrap_or(PLANE_TYPE_OVERLAY);
            if class != PLANE_TYPE_PRIMARY && class != PLANE_TYPE_CURSOR {
                continue;
            }
            let image = self.framebuffer(fb)?;
            if class == PLANE_TYPE_PRIMARY {
                primary = Some(image);
            } else {
                let x = self
                    .plane_property(handle, c"CRTC_X")
                    .map(signed)
                    .ok_or(Error::MissingProperty("CRTC_X"))?;
                let y = self
                    .plane_property(handle, c"CRTC_Y")
                    .map(signed)
                    .ok_or(Error::MissingProperty("CRTC_Y"))?;
                cursor = Some(Cursor { image, x, y });
            }
        }

        primary
            .map(|primary| Layout { primary, cursor })
            .ok_or(Error::NoScanout)
    }

    /// Describe one framebuffer.
    fn framebuffer(&self, handle: drm::control::framebuffer::Handle) -> Result<Framebuffer, Error> {
        let info = self
            .get_planar_framebuffer(handle)
            .map_err(|error| match error {
                drm::control::GetPlanarFramebufferError::Io(error) => device_error(&error),
                drm::control::GetPlanarFramebufferError::UnrecognizedFourcc(fourcc) => {
                    Error::UnknownFormat(fourcc.0)
                }
            })?;

        let handles = info.buffers();
        let pitches = info.pitches();
        let offsets = info.offsets();
        let buffers = core::array::from_fn(|at| {
            handles.get(at).copied().flatten().map(|handle| Buffer {
                handle,
                pitch: pitches.get(at).copied().unwrap_or(0),
                offset: offsets.get(at).copied().unwrap_or(0),
            })
        });

        let (width, height) = info.size();
        Ok(Framebuffer {
            id: u32::from(handle),
            width,
            height,
            format: info.pixel_format(),
            modifier: info.modifier(),
            buffers,
        })
    }

    /// Read one of a plane's properties by name.
    fn plane_property(&self, handle: plane::Handle, name: &core::ffi::CStr) -> Option<u64> {
        let set = self.get_properties(handle).ok()?;
        let (ids, values) = set.as_props_and_values();
        for (id, value) in ids.iter().zip(values.iter()) {
            if self.get_property(*id).is_ok_and(|info| info.name() == name) {
                return Some(*value);
            }
        }
        None
    }

    /// Export a buffer for import elsewhere.
    ///
    /// **Read-only, and the omission is load bearing.** Asking for write access
    /// makes a driver refuse a scanout buffer outright, on every plane and
    /// including a plain linear cursor buffer, and the refusal carries nothing
    /// that says why. A capture path has no reason to write to the framebuffer
    /// it is reading, so this costs nothing and is the difference between a
    /// backend that works and one that fails identically everywhere.
    pub fn export(&self, buffer: &Buffer) -> Result<OwnedFd, Error> {
        self.buffer_to_prime_fd(buffer.handle, EXPORT_FLAGS)
            .map_err(|error| device_error(&error))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the export actually asks for must never carry write access. The
    /// failure it guards against is a driver refusing every scanout buffer with
    /// nothing that says why, so it is worth a check that fails the moment the
    /// flag word gains anything.
    #[test]
    fn export_never_requests_write() {
        assert_eq!(EXPORT_FLAGS & drm::RDWR, 0);
        assert_eq!(EXPORT_FLAGS, drm::CLOEXEC);
    }

    /// A plane coordinate is a signed value in an unsigned field, and a pointer
    /// against the left or top edge really does go negative: the value here was
    /// read off a live display with the pointer two pixels past the edge. Read
    /// unsigned it becomes 4294967294, which places the pointer four billion
    /// pixels away and is a perfectly plausible-looking large number.
    #[test]
    fn plane_coordinates_go_negative() {
        assert_eq!(signed(0), 0);
        assert_eq!(signed(941), 941);
        assert_eq!(signed(0xffff_fffe), -2);
        assert_eq!(signed(0xffff_ffff), -1);
    }

    /// Occupied buffer slots are reported in order and the empty tail is
    /// skipped, so a caller iterating planes cannot walk off into unset ones.
    #[test]
    fn planes_skip_empty_slots() {
        let fb = Framebuffer {
            id: 1,
            width: 2560,
            height: 1440,
            format: DrmFourcc::Abgr2101010,
            modifier: Some(DrmModifier::Linear),
            buffers: [None, None, None, None],
        };
        assert_eq!(fb.planes().count(), 0);
    }
}
