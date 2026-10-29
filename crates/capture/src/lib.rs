//! Frame sources. Synthetic now; real backends at Gate B.
//!
//! See docs/05-host.md section 2.
//!
//! **A frame here is planar 4:2:0 in system memory, and that is a deliberate
//! narrowing of what section 2 describes.** The rule there is that a captured
//! frame moves as a device handle and is never copied to system memory,
//! because a real capture backend receives one from the compositor and a
//! readback would be pure loss. Nothing is captured yet. What exists is a
//! generator, and the frames it makes have to reach **both** encode backends,
//! which on a machine with two vendors' hardware are two different devices
//! with no shared allocation between them. A device handle cannot satisfy
//! that; system memory can, and each backend uploads into its own surfaces.
//!
//! So the copy is not a readback that crept in. It is a generator writing its
//! output where every consumer can read it, and it disappears when real
//! capture arrives with a handle of its own.

// The host's alone, so it builds empty where no host runs (macOS,
// docs/impl-plan-macos.md).
#![cfg(any(target_os = "linux", windows))]

// Each platform has its own backends (docs/impl-plan-windows.md); the frame,
// the generator and what an output and its pointer are build everywhere.
#[cfg(target_os = "linux")]
pub mod convert;
#[cfg(target_os = "linux")]
pub mod cursor;
/// The conversion on Windows, over the system's graphics interface.
#[cfg(windows)]
pub mod d3d11;
#[cfg(target_os = "linux")]
pub mod desktop;
/// The desktop as Windows duplicates it, and the pointer over it.
#[cfg(windows)]
pub mod duplication;
#[cfg(target_os = "linux")]
pub mod gl;
#[cfg(target_os = "linux")]
pub mod mode;
#[cfg(test)]
mod reference;
#[cfg(target_os = "linux")]
pub mod scanout;
pub mod synthetic;
#[cfg(target_os = "linux")]
pub mod vulkan;
#[cfg(target_os = "linux")]
mod wayland;

use lowlat_common::clock::Time;

/// Which display interface the import and conversion run on.
///
/// **The first where the device has it, the second where it does not, and a
/// name pins the choice** (docs/05-host.md section 4). The two are not
/// equivalent: the second costs about a millisecond more per frame and cannot
/// hand a frame to the vendor encoder, so it is a floor for old devices and a
/// measurement target, never a substitute where the first exists. A device
/// that refuses the first for any reason other than not having it keeps its
/// refusal: swapping in a working pipeline of unknown provenance is the
/// opposite of what the refusal is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Backend {
    /// The interface that takes a buffer's tiling explicitly and exports its
    /// result to either encoder.
    #[default]
    Vulkan,
    /// The fallback for devices without the first interface, and the
    /// measurement target it started as. Slower by about a millisecond a
    /// frame, and its result reaches only the open encoder.
    Gl,
}

impl Backend {
    /// One of the two by name, or nothing if the name is neither.
    ///
    /// **Nothing rather than the default**, so a caller can tell "not asked
    /// for" from "asked for by a name that is not one of these" and refuse the
    /// second. Silently running the default because a flag was misspelled is
    /// how a measurement gets attributed to the wrong interface.
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "vulkan" | "vk" => Some(Self::Vulkan),
            "gl" | "opengl" | "egl" => Some(Self::Gl),
            _ => None,
        }
    }

    /// What the environment asks for, or nothing when it asks for nothing.
    ///
    /// The stream treats nothing as "follow the device", so unlike
    /// [`Self::requested`] there is no default to stand in: a name that is
    /// not one of the two is refused loudly and the answer stays nothing.
    pub fn asked() -> Option<Self> {
        let named = std::env::var("LOWLAT_CONVERT").ok()?;
        if named.is_empty() {
            return None;
        }
        let parsed = Self::parse(&named);
        if parsed.is_none() {
            lowlat_common::log_error!(
                "capture: LOWLAT_CONVERT={named} names no interface, following the device"
            );
        }
        parsed
    }

    /// What the environment asks for, and the default when it asks for nothing.
    ///
    /// Reads `LOWLAT_CONVERT`. A name that is not one of the two is refused
    /// loudly and the default stands, because the alternative is a run that
    /// silently measured the interface nobody asked about.
    pub fn requested() -> Self {
        let Ok(named) = std::env::var("LOWLAT_CONVERT") else {
            return Self::default();
        };
        // Set and empty is how a shell spells "not set", and refusing it would
        // put an error in the log of every run that unset it that way.
        if named.is_empty() {
            return Self::default();
        }
        match Self::parse(&named) {
            Some(backend) => backend,
            None => {
                lowlat_common::log_error!(
                    "capture: LOWLAT_CONVERT={named} names no interface, using the default"
                );
                Self::default()
            }
        }
    }
}

/// Where one output sits, in the desktop's own units.
///
/// The origin is measured from the desktop's own corner rather than from
/// wherever the compositor put zero, because that corner is what the absolute
/// axis maps its own zero to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placement {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    /// The whole desktop the input layer spreads an absolute device over.
    pub desktop_width: u32,
    pub desktop_height: u32,
    /// Which way the session has turned this output.
    ///
    /// **Followed, never set here.** A turned output is drawn turned into a
    /// framebuffer that keeps its landscape shape, so the picture captured
    /// from it is on its side and only the session can say by how much.
    pub rotation: lowlat_core::video::Rotation,
}

/// The pointer as it should reach a peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pointer {
    /// Top-left of the **drawn part** on the display: the plane's own corner
    /// plus where the drawing begins inside it.
    ///
    /// Signed, and it goes negative at the left and top edges. The drawn part
    /// is what travels, so sending the plane's corner instead would place
    /// every pointer up and to the left of itself by however much padding the
    /// compositor happened to leave.
    pub x: i32,
    pub y: i32,
    pub width: u16,
    pub height: u16,
    /// Names the image, and is what a peer that keeps pointers holds it by.
    pub checksum: u32,
    /// True when this read produced a picture that has not been seen before.
    pub fresh: bool,
    /// True when this read actually examined the pixels.
    ///
    /// **Only a read that looked can say whether a pointer is being drawn.**
    /// The picture is read on a cadence, so most reads report the one they
    /// already had; taking those as evidence that a pointer is still on screen
    /// resets any timer watching for one that has gone, three times out of
    /// four, so it never expires.
    pub looked: bool,
    /// What the system itself reports of the pointer, where it reports it:
    /// a read that carries this is a message owed to the guests, settled by
    /// the display. Nothing where the pointer is inferred from what is drawn,
    /// which the stream settles instead.
    pub reported: Option<Reported>,
}

/// The pointer as a system that reports it says it is, in the desktop's own
/// orientation (docs/05-host.md section 8.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reported {
    /// Where the pointer points -- its hotspot -- on the captured output.
    pub x: u16,
    pub y: u16,
    /// The hotspot inside the picture.
    pub hot_x: u16,
    pub hot_y: u16,
    /// The system draws no pointer of its own over the picture: it draws one
    /// into the picture, or none at all.
    pub hidden: bool,
    /// Something wants the pointer's motion rather than its position: an
    /// application hid it, or set one with nothing to see.
    pub relative: bool,
    /// The system holds the pointer back while a touch or a pen drives.
    pub suppressed: bool,
}

/// Where a rectangle drawn in a turned framebuffer sits in the desktop the
/// framebuffer shows.
///
/// `frame` is the framebuffer's own size and the rectangle is in its pixels;
/// the answer is in the desktop's, which for a quarter turn has the frame's
/// sides swapped. Signed, because a pointer straddles the edges.
pub fn upright_rect(
    turn: lowlat_core::video::Rotation,
    frame: (u32, u32),
    rect: (i32, i32, u32, u32),
) -> (i32, i32, u32, u32) {
    use lowlat_core::video::Rotation;
    let (fw, fh) = (i64::from(frame.0), i64::from(frame.1));
    let (x, y, w, h) = (
        i64::from(rect.0),
        i64::from(rect.1),
        i64::from(rect.2),
        i64::from(rect.3),
    );
    let (x, y, w, h) = match turn {
        Rotation::Deg90 => (fh - y - h, x, h, w),
        Rotation::Deg270 => (y, fw - x - w, h, w),
        Rotation::Deg180 => (fw - x - w, fh - y - h, w, h),
        _ => (x, y, w, h),
    };
    (
        i32::try_from(x).unwrap_or(i32::MAX),
        i32::try_from(y).unwrap_or(i32::MAX),
        u32::try_from(w).unwrap_or(u32::MAX),
        u32::try_from(h).unwrap_or(u32::MAX),
    )
}

/// One plane of a frame.
///
/// The stride is carried rather than assumed equal to the width: a surface
/// laid out by a driver is padded to its own alignment, and a consumer that
/// walks rows by width reads progressively further into the wrong place.
#[derive(Debug, Clone, Copy)]
pub struct Plane<'a> {
    pub bytes: &'a [u8],
    pub stride: usize,
}

impl Plane<'_> {
    /// One row, or `None` past the end.
    pub fn row(&self, index: usize) -> Option<&[u8]> {
        self.bytes
            .get(index * self.stride..(index + 1) * self.stride)
    }
}

/// A frame, planar 4:2:0 with interleaved chroma.
///
/// Chroma is interleaved rather than in two planes because that is the layout
/// both encode backends take without a conversion, which is the whole point of
/// generating planar output rather than packed colour.
#[derive(Debug, Clone, Copy)]
pub struct Frame<'a> {
    pub width: u32,
    pub height: u32,
    /// Full resolution, one byte per sample.
    pub luma: Plane<'a>,
    /// Half resolution in both directions, two bytes per sample, chroma
    /// difference blue first.
    pub chroma: Plane<'a>,
    /// **Stamped when the frame was produced, and it travels from here to the
    /// wire.** Every latency figure in docs/05-host.md section 10 is measured
    /// against this, so a stage that restamps it destroys the measurement
    /// rather than merely losing it.
    pub captured_at: Time,
    /// Counts from zero and never repeats. The content is a function of it,
    /// which is what makes a decoded frame checkable against its source.
    pub index: u64,
}
