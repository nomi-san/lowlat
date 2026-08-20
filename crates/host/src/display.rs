//! The real desktop as a frame source for the encode loop.
//!
//! Capture, conversion and the encoder all live on one device and hand each
//! other handles; nothing here copies a pixel. What this module owns is the
//! composition, which is why it sits above all three rather than inside any of
//! them.
//!
//! Two things it exists to get right, both learned by measurement rather than
//! by design:
//!
//! - **The display cycles through a pool of buffers as it draws.** A single
//!   import reads one buffer of that rotation, not the live screen, so the
//!   plane is re-read every frame and imports are kept per buffer.
//! - **One conversion target per picture in flight.** Converting into a target
//!   the encoder has not finished reading produces the newest content under an
//!   older picture's timestamp, which is invisible until the screen changes.

use std::collections::HashMap;

use lowlat_capture::convert::{Converter, Nv12};
use lowlat_capture::cursor::{Pointer, Watcher};
use lowlat_capture::scanout::{self, Card, CursorPlane};
use lowlat_capture::vulkan::{self, Imports, PlaneLayout};
use lowlat_common::clock::Time;

/// How many buffers of the display's rotation to keep imported.
///
/// The pool is small -- a handful -- and an import that falls out of it is
/// rebuilt on its next turn rather than lost. Bounded so a compositor that
/// cycles more widely than expected cannot grow this without limit.
const IMPORTS: usize = 8;

/// How many display nodes to look at. More than any machine here has, and the
/// search stops at the first that is scanning out.
const NODES: u32 = 8;

/// What went wrong.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub enum Error {
    /// The display device could not be read.
    Capture(scanout::Error),
    /// The device that converts could not do so.
    Convert(vulkan::Error),
    /// The encoder would not take a frame that is already on the device.
    Register,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Capture(error) => write!(f, "display: {error}"),
            Self::Convert(error) => write!(f, "conversion: {error}"),
            Self::Register => f.write_str("the encoder refused a frame already on the device"),
        }
    }
}

impl std::error::Error for Error {}

impl From<scanout::Error> for Error {
    fn from(error: scanout::Error) -> Self {
        Self::Capture(error)
    }
}

impl From<vulkan::Error> for Error {
    fn from(error: vulkan::Error) -> Self {
        Self::Convert(error)
    }
}

/// One conversion target, registered with the encoder once.
struct Target {
    frame: Nv12,
    input: lowlat_encode::nvenc::Input,
    /// Kept because releasing it invalidates the address the encoder holds.
    _taken: lowlat_encode::cuda::External,
}

/// What the display is showing, described once and compared against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Shape {
    width: u32,
    height: u32,
    format: drm::buffer::DrmFourcc,
    modifier: u64,
}

impl Shape {
    fn of(fb: &scanout::Framebuffer) -> Self {
        Self {
            width: fb.width,
            height: fb.height,
            format: fb.format,
            modifier: fb.modifier.map_or(0, u64::from),
        }
    }
}

/// The desktop, ready to hand pictures to an encoder.
pub struct Display {
    card: Card,
    device: vulkan::Device,
    converter: Converter,
    plane: drm::control::plane::Handle,
    /// What the display was doing when the imports below were built.
    shape: Shape,
    /// One per buffer of the display's rotation, keyed by the kernel's
    /// identifier for it.
    imports: HashMap<u32, vulkan::Imported>,
    targets: Vec<Target>,
    next: usize,
    /// The pointer plane, when the pipeline has one. **Found once**: a machine
    /// that draws its pointer in the picture rather than on a plane has none,
    /// and looking for it every frame would be a walk of the whole pipeline
    /// for an answer that does not change.
    cursor_plane: Option<CursorPlane>,
    cursor: Watcher,
}

impl core::fmt::Debug for Display {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Display")
            .field("shape", &self.shape)
            .field("imports", &self.imports.len())
            .field("targets", &self.targets.len())
            .finish_non_exhaustive()
    }
}

impl Display {
    /// Find the display and prepare `depth` conversion targets for an encoder.
    ///
    /// **Which node the display is on is discovered, not configured.** A
    /// machine with two cards lights one of them, and it is not reliably the
    /// first: measured here, the node that scans out is the second and the
    /// first reports nothing at all. Configuring it would make that a setting
    /// somebody gets wrong once per machine, and a wrong answer is
    /// indistinguishable from no session running.
    ///
    /// **The encoder must already be configured for what the display is
    /// showing.** Its registration fixes the picture size, so a mismatch is a
    /// stream of the wrong shape rather than a refusal.
    pub fn open(depth: usize, encoder: &lowlat_encode::nvenc::Encoder<'_>) -> Result<Self, Error> {
        let (node, card, layout) = Self::find()?;
        let device = vulkan::Device::for_display(&node)?;
        let converter = Converter::new(&device)?;
        let shape = Shape::of(&layout.primary);

        let mut targets = Vec::with_capacity(depth);
        for _ in 0..depth {
            targets.push(Self::target(&device, encoder, shape.width, shape.height)?);
        }

        lowlat_common::log_info!(
            "display: {}x{} on {}, {} target(s)",
            shape.width,
            shape.height,
            device.name(),
            targets.len()
        );

        Ok(Self {
            card,
            device,
            converter,
            plane: layout.primary_plane,
            shape,
            imports: HashMap::new(),
            targets,
            next: 0,
            cursor_plane: layout.cursor_plane,
            cursor: Watcher::new(),
        })
    }

    /// What the display is showing, before anything is built on it.
    ///
    /// **The encoder has to be configured for this and is created first**, so
    /// the size has to be readable without a device, a converter or a target.
    /// Configuring it for anything else produces a stream of the wrong shape
    /// rather than a refusal, which is a whole session wasted.
    pub fn size_of_display() -> Option<(u32, u32)> {
        let (_, _, layout) = Self::find().ok()?;
        Some((layout.primary.width, layout.primary.height))
    }

    /// The first node with something on it.
    ///
    /// Nodes are tried in order and the first that reports a lit primary plane
    /// wins. A node that reports nothing is skipped rather than failed on,
    /// because that is the ordinary state of a card driving no display.
    fn find() -> Result<(std::path::PathBuf, Card, scanout::Layout), Error> {
        let mut last = scanout::Error::NoScanout;
        for index in 0..NODES {
            let node = std::path::PathBuf::from(format!("/dev/dri/card{index}"));
            if !node.exists() {
                continue;
            }
            match Card::open(&node).and_then(|card| card.scan().map(|layout| (card, layout))) {
                Ok((card, layout)) => {
                    lowlat_common::log_info!("display: capturing {}", node.display());
                    return Ok((node, card, layout));
                }
                Err(error) => last = error,
            }
        }
        Err(Error::Capture(last))
    }

    /// Build one conversion target and hand it to the encoder.
    fn target(
        device: &vulkan::Device,
        encoder: &lowlat_encode::nvenc::Encoder<'_>,
        width: u32,
        height: u32,
    ) -> Result<Target, Error> {
        let frame = device.allocate_nv12(width, height)?;
        // The encoder's runtime has no name for a display-interface descriptor,
        // so the frame leaves the other way. The allocation can produce both.
        let (fd, exported) = device.export_nv12(&frame, false)?;
        let bytes = u64::from(exported.pitch) * u64::from(exported.height) * 3 / 2;
        let cuda = lowlat_encode::cuda::Cuda::load().map_err(|_| Error::Register)?;
        // SAFETY: the encoder's context is current on this thread, the
        // descriptor was exported for the platform's opaque kind, and the size
        // is the whole allocation behind it.
        let taken = unsafe { cuda.import(fd, bytes) }.map_err(|_| Error::Register)?;
        let plane = taken
            .plane(0, bytes, exported.pitch as usize)
            .map_err(|_| Error::Register)?;
        let input = encoder
            .register_ptr(plane.ptr(), plane.pitch())
            .map_err(|_| Error::Register)?;
        Ok(Target {
            frame,
            input,
            _taken: taken,
        })
    }

    /// The registration the last [`Display::acquire`] converted into.
    ///
    /// Separate from the acquire so a caller can hold it across other work
    /// without holding the whole source borrowed.
    pub fn presented(&self) -> Option<&lowlat_encode::nvenc::Input> {
        let slot = self.next.checked_sub(1)? % self.targets.len().max(1);
        self.targets.get(slot).map(|target| &target.input)
    }

    /// Convert what the display is showing now into the next free target.
    ///
    /// Returns when the picture was taken; the registration to submit is then
    /// [`Display::presented`].
    pub fn acquire(&mut self) -> Result<Time, Error> {
        let began = Time::now();
        let fb = self.card.framebuffer_on(self.plane)?;

        // **Nothing about the buffer announces a format change**: the size, the
        // stride and the plane count are identical across one, and only the
        // meaning of the bytes moves. So it is compared rather than noticed,
        // and every import built for the old meaning is dropped.
        let shape = Shape::of(&fb);
        if shape != self.shape {
            lowlat_common::log_info!(
                "display: the picture changed, {}x{} {:?} -> {}x{} {:?}",
                self.shape.width,
                self.shape.height,
                self.shape.format,
                shape.width,
                shape.height,
                shape.format
            );
            self.forget_imports();
            self.shape = shape;
        }

        self.ensure_import(&fb)?;

        // The slot is chosen and the cursor advanced before anything is
        // borrowed out of the targets, because the registration handed back
        // borrows them until the caller is done with it.
        let slot = self.next % self.targets.len().max(1);
        self.next = self.next.wrapping_add(1);

        let source = self.imports.get(&fb.id).ok_or(Error::Register)?;
        let target = self.targets.get(slot).ok_or(Error::Register)?;
        self.converter
            .run(&self.device, source, &target.frame, false)?;
        Ok(began)
    }

    /// Make sure this buffer is imported, building it if the rotation moved on.
    fn ensure_import(&mut self, fb: &scanout::Framebuffer) -> Result<(), Error> {
        if !self.imports.contains_key(&fb.id) {
            // Bounded rather than unbounded. A compositor that cycles more
            // widely than expected costs a rebuild per turn, which is a cost;
            // an unbounded map would be a leak.
            if self.imports.len() >= IMPORTS {
                self.forget_imports();
            }
            let planes: Vec<PlaneLayout> = fb
                .planes()
                .map(|buffer| PlaneLayout {
                    offset: buffer.offset,
                    pitch: buffer.pitch,
                })
                .collect();
            let first = fb.planes().next().ok_or(scanout::Error::NoScanout)?;
            let fd = self.card.export(first)?;
            let imported = self.device.import(&Imports {
                width: fb.width,
                height: fb.height,
                format: fb.format,
                modifier: fb.modifier.map_or(0, u64::from),
                fd: <std::os::fd::OwnedFd as std::os::fd::IntoRawFd>::into_raw_fd(fd),
                planes: &planes,
            })?;
            self.imports.insert(fb.id, imported);
        }
        Ok(())
    }

    /// Drop every import, which is what a changed picture requires.
    fn forget_imports(&mut self) {
        for (_, imported) in self.imports.drain() {
            self.device.release(imported);
        }
    }

    /// Read the pointer the display is drawing, if it is drawing one.
    ///
    /// **Not on the frame path.** It reads the plane's buffer on the
    /// processor, which the rule against copying pixels does not cover: that
    /// rule is about frames, which are megabytes sixty times a second, and
    /// this is a quarter of a megabyte a few times a second that has to be
    /// compared against the last one to know whether it changed at all.
    ///
    /// Nothing drawn is a state and not a failure. It means an application hid
    /// the pointer, or the compositor drew it into the picture because it
    /// outgrew the plane, and this backend cannot tell those apart. Both mean
    /// the same thing to a peer: do not draw one.
    pub fn pointer(&mut self) -> Option<Pointer> {
        let at = self.cursor_plane.as_ref()?;
        match self.cursor.read(&self.card, at) {
            Ok(pointer) => pointer,
            Err(error) => {
                lowlat_common::log_warn!("display: the pointer could not be read, err={error}");
                None
            }
        }
    }

    /// The picture the last [`Display::pointer`] named.
    pub fn pointer_image(&self) -> &[u8] {
        self.cursor.image()
    }

    /// What the display is showing, for the encoder's configuration.
    pub fn size(&self) -> (u32, u32) {
        (self.shape.width, self.shape.height)
    }
}

impl Drop for Display {
    fn drop(&mut self) {
        self.forget_imports();
        for target in self.targets.drain(..) {
            self.device.release_nv12(target.frame);
        }
    }
}
