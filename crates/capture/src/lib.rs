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

pub mod convert;
pub mod cursor;
pub mod desktop;
pub mod gl;
pub mod scanout;
pub mod synthetic;
pub mod vulkan;

use lowlat_common::clock::Time;

/// Which display interface the import and conversion run on.
///
/// **Named by whoever builds the pipeline, never inferred and never fallen back
/// to.** The two are not equivalent: only [`Self::Vulkan`] can hand its result
/// to an encoder today, and a machine that refuses it says which part it is
/// missing ([`vulkan::Error::Unsupported`]). Choosing the other automatically
/// would replace that answer with a working pipeline of unknown provenance,
/// which is the opposite of what the refusal is for.
///
/// Selecting between them on a machine's behalf is a later decision and needs
/// the refusals from real hardware first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Backend {
    /// The interface that takes a buffer's tiling explicitly and exports its
    /// result to either encoder.
    #[default]
    Vulkan,
    /// The older interface, for measuring what a device that refuses the first
    /// one can still do.
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
