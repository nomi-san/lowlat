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
use lowlat_capture::desktop::Placement;
use lowlat_capture::scanout::{self, Card, CursorPlane};
use lowlat_capture::vulkan::{self, Imports, PlaneLayout};
use lowlat_common::clock::Time;
use std::os::fd::{AsFd, OwnedFd};

/// One output a host can be asked to capture.
#[derive(Debug, Clone)]
pub struct Selectable {
    /// What to ask for, such as `card0:DP-2`.
    pub id: String,
    /// The connector's own name, which is what the session knows it by.
    pub connector: String,
    pub width: u32,
    pub height: u32,
    /// Where it sits in the desktop, when a session describes it.
    pub place: Option<Placement>,
}

/// What a pre-flight found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capturable {
    /// A display is lit and its framebuffer can be reached.
    Yes,
    /// Nothing is lit: no display, or no session driving one.
    NothingLit,
    /// Something is lit and its framebuffer cannot be reached, which is the
    /// privilege rather than the hardware.
    NotReachable,
}

/// Which of these outputs a published capture checksum names.
///
/// **The checksum is how the loop says what it is capturing without a lock**
/// ([`crate::stream`]), and this is the other half: a caller that can
/// enumerate the outputs gets the one being captured without ever handling the
/// encoding. Nothing when the loop has not opened a display, or when what it
/// opened is no longer in the list.
#[must_use]
pub fn captured(listed: &[Selectable], checksum: u32) -> Option<&Selectable> {
    if checksum == 0 {
        return None;
    }
    listed
        .iter()
        .find(|output| lowlat_core::crc32::of(output.id.as_bytes()) == checksum)
}

/// The driver bound to a display device, by the name the system uses.
///
/// Read from the device's own link rather than inferred from anything about
/// the node, which is only ever an ordering.
fn driver_of(node: &std::path::Path) -> Option<String> {
    let card = node.file_name()?.to_str()?;
    let link = std::fs::read_link(format!("/sys/class/drm/{card}/device/driver")).ok()?;
    Some(link.file_name()?.to_str()?.to_string())
}

/// Name one output the way it is asked for.
///
/// The device, then the connector. Both halves are the system's own names
/// rather than anything invented here, so the identity a caller stores is one
/// they can also see in the machine.
fn identity(node: &std::path::Path, connector: &str) -> String {
    let device = node
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("card");
    format!("{device}:{connector}")
}

/// Split an identity back into the device and the connector.
///
/// **The device half is optional.** A bare connector name selects it on
/// whichever device has it, which is what somebody typing one by hand means,
/// and is unambiguous on the overwhelmingly common single-device machine.
fn split(id: &str) -> (Option<&std::ffi::OsStr>, &str) {
    match id.split_once(':') {
        Some((device, connector)) => (Some(std::ffi::OsStr::new(device)), connector),
        None => (None, id),
    }
}

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
    /// The same, on the other interface.
    Gl(lowlat_capture::gl::Error),
    /// The encoder would not take a frame that is already on the device.
    Register,
    /// The interface asked for cannot hand a frame to the encoder asked for.
    ///
    /// **Refused rather than quietly served by the other interface.** A caller
    /// naming one and getting the other has measured the wrong thing and has
    /// nothing telling it so.
    NotTogether,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Capture(error) => write!(f, "display: {error}"),
            Self::Convert(error) => write!(f, "conversion: {error}"),
            Self::Gl(error) => write!(f, "conversion: {error}"),
            Self::Register => f.write_str("the encoder refused a frame already on the device"),
            Self::NotTogether => f.write_str("that conversion interface cannot feed that encoder"),
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

/// What an encoder calls a converted frame it has been handed.
///
/// **An enum rather than a type parameter**, because the set is closed and the
/// two members are genuinely different objects: one is an address the vendor
/// runtime was told about, the other is a surface the display interface
/// imported. A backend matches its own and refuses the other, which is the
/// same shape as a frame arriving for a pipeline that cannot take it.
pub enum Registration {
    /// An address registered with the vendor runtime.
    Vendor {
        input: lowlat_encode::nvenc::Input,
        /// Kept because releasing it invalidates the address the encoder
        /// holds.
        _taken: lowlat_encode::cuda::External,
    },
    /// A surface the display interface imported from the same allocation.
    Open { surface: u32 },
}

impl core::fmt::Debug for Registration {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Vendor { .. } => f.write_str("Vendor"),
            Self::Open { surface } => write!(f, "Open({surface})"),
        }
    }
}

/// What one [`Display::acquire`] and [`Display::converted`] pair produced.
#[derive(Debug, Clone, Copy)]
pub struct Acquired {
    /// When the picture was taken, which is what every latency figure is
    /// measured from.
    pub at: Time,
    /// **False means this picture is the previous one, byte for byte.** A
    /// caller may skip everything downstream of it; nothing is skipped here,
    /// because the conversion is what produced the answer.
    pub changed: bool,
}

/// One conversion target, registered with the encoder once.
///
/// **Generic because the second instantiation exists**: the two interfaces call
/// a converted frame different things, and everything around it here -- the
/// rotation, the registration, the slot -- is the same either way.
struct Target<F> {
    frame: F,
    registration: Registration,
}

/// Which encoder a conversion target is being registered with.
///
/// **Named rather than passed as a closure**, because the four pairings of
/// interface and encoder are not all buildable and the refusal belongs here.
/// A closure would put each caller in the position of knowing which pairs
/// work, which is exactly the knowledge that has to live in one place.
pub enum Register<'a> {
    /// The vendor runtime, which takes an address it was told about.
    Vendor(&'a lowlat_encode::nvenc::Encoder<'a>),
    /// The display stack's encoder, which takes a descriptor.
    Open(&'a lowlat_encode::vaapi::Display<'a>),
}

impl core::fmt::Debug for Register<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Vendor(_) => f.write_str("Vendor"),
            Self::Open(_) => f.write_str("Open"),
        }
    }
}

/// The interface the import and conversion run on.
///
/// **An enum rather than a trait, and the members are not interchangeable.**
/// Only the first can hand its result to either encoder; the second allocates
/// nothing it can export, so its targets are views of a region the display
/// device allocated. Collapsing them behind one trait would hide that, and it
/// is the difference that decides which encoders a machine can use.
enum Pipeline {
    Vulkan(Box<Vulkan>),
    Gl(Box<Gl>),
}

/// The primary interface's half.
struct Vulkan {
    device: vulkan::Device,
    converter: Converter,
    imports: HashMap<u64, vulkan::Imported>,
    targets: Vec<Target<Nv12>>,
}

/// The other interface's half.
struct Gl {
    device: lowlat_capture::gl::Device,
    converter: lowlat_capture::gl::Converter,
    imports: HashMap<u64, lowlat_capture::gl::Imported>,
    targets: Vec<Target<lowlat_capture::gl::Nv12>>,
    /// The regions the targets are views of.
    ///
    /// **Held here because they outlive nothing else.** This interface cannot
    /// allocate a frame it can export, so the allocation came from the display
    /// device and has to be given back to it; nothing in the target says where
    /// it came from.
    regions: Vec<scanout::Linear>,
    /// The digest of a conversion that ran synchronously inside `submit`,
    /// waiting to be handed out by `poll`. This interface has no split, so
    /// the answer exists the moment the submit returns.
    ready: Option<lowlat_capture::convert::Digest>,
}

impl Pipeline {
    /// How many targets there are, which is the rotation's length.
    fn depth(&self) -> usize {
        match self {
            Self::Vulkan(vk) => vk.targets.len(),
            Self::Gl(gl) => gl.targets.len(),
        }
    }

    /// Whether this buffer is already imported.
    fn holds(&self, key: u64) -> bool {
        match self {
            Self::Vulkan(vk) => vk.imports.contains_key(&key),
            Self::Gl(gl) => gl.imports.contains_key(&key),
        }
    }

    /// How many imports are cached.
    fn cached(&self) -> usize {
        match self {
            Self::Vulkan(vk) => vk.imports.len(),
            Self::Gl(gl) => gl.imports.len(),
        }
    }

    /// Import a captured buffer under a key.
    ///
    /// **The two interfaces disagree about the descriptor and it is not
    /// cosmetic.** The first takes ownership on success; the second duplicates
    /// what it needs and leaves it here. Getting it the wrong way round leaks
    /// one a frame or closes one the driver still holds.
    fn take(&mut self, key: u64, fd: OwnedFd, source: Imports<'_>) -> Result<(), Error> {
        match self {
            Self::Vulkan(vk) => {
                let source = Imports {
                    fd: <OwnedFd as std::os::fd::IntoRawFd>::into_raw_fd(fd),
                    ..source
                };
                let imported = vk.device.import(&source)?;
                vk.imports.insert(key, imported);
            }
            Self::Gl(gl) => {
                let source = Imports {
                    fd: std::os::fd::AsRawFd::as_raw_fd(&fd),
                    ..source
                };
                let imported = gl.device.import(&source).map_err(Error::Gl)?;
                drop(fd);
                gl.imports.insert(key, imported);
            }
        }
        Ok(())
    }

    /// Drop every import, which is what a changed picture requires.
    fn forget(&mut self) {
        match self {
            Self::Vulkan(vk) => {
                for (_, imported) in vk.imports.drain() {
                    vk.device.release(imported);
                }
            }
            Self::Gl(gl) => {
                for (_, imported) in gl.imports.drain() {
                    gl.device.release(imported);
                }
            }
        }
    }

    /// Submit the conversion of one imported buffer into one target slot.
    ///
    /// **Returns as soon as it is queued, not when it is done.** The fence is
    /// the converter's own and the digest arrives through [`Self::poll`]; the
    /// loop pairs the two so the conversion runs on the device while it
    /// collects the previous picture, instead of the wait landing in the
    /// acquire stage where it was measured.
    fn submit(&mut self, key: u64, slot: usize) -> Result<(), Error> {
        match self {
            Self::Vulkan(vk) => {
                let source = vk.imports.get(&key).ok_or(Error::Register)?;
                let target = vk.targets.get(slot).ok_or(Error::Register)?;
                vk.converter
                    .submit(&vk.device, source, &target.frame.target(), false)
                    .map_err(Error::Convert)
            }
            Self::Gl(gl) => {
                let source = gl.imports.get(&key).ok_or(Error::Register)?;
                let target = gl.targets.get(slot).ok_or(Error::Register)?;
                // The other interface has no split: its conversion blocks, so
                // the digest is here the moment `submit` returns. Held and
                // handed out by `poll`, so the caller sees one shape either
                // way.
                let digest = gl
                    .converter
                    .run(&gl.device, source, &target.frame, false)
                    .map_err(Error::Gl)?;
                gl.ready = Some(digest);
                Ok(())
            }
        }
    }

    /// Collect the conversion [`Pipeline::submit`] started.
    ///
    /// **Waits rather than asks**, unlike the encoder's poll: the loop needs
    /// the digest to decide whether the picture is owed before it can submit,
    /// so a not-ready answer would only send it round again. The wait is
    /// short because the submit happened before the loop's own collect, which
    /// is the whole point of the split.
    fn collect(&mut self) -> Result<Option<lowlat_capture::convert::Digest>, Error> {
        match self {
            Self::Vulkan(vk) => vk.converter.collect(&vk.device).map_err(Error::Convert),
            Self::Gl(gl) => Ok(gl.ready.take()),
        }
    }

    /// Poke the device with a trivial conversion, and wait for it.
    ///
    /// **For the wakeup cost an integrated device pays**, measured on the open
    /// stack: a conversion submitted after a few milliseconds of idle costs
    /// 1.3 ms against 0.4 ms warm, because the compute block powers down
    /// between frames. The loop calls this a moment before the display's next
    /// present, while it would otherwise be waiting for it, so the conversion
    /// that follows runs warm. The one workgroup this writes lands in a slot
    /// the next conversion overwrites whole, and the digest is discarded. The
    /// other interface has no such cost, so there is nothing to poke.
    fn poke(&mut self, key: u64, slot: usize) -> Result<(), Error> {
        match self {
            Self::Vulkan(vk) => {
                let source = vk.imports.get(&key).ok_or(Error::Register)?;
                let target = vk.targets.get(slot).ok_or(Error::Register)?;
                let groups = lowlat_capture::convert::poke_groups(source.width, source.height);
                vk.converter
                    .poke(&vk.device, source, &target.frame.target(), groups)
                    .map_err(Error::Convert)?;
                // The wait is the point: the submission above woke the block,
                // and the digest it produces is nobody's.
                let _ = vk.converter.collect(&vk.device).map_err(Error::Convert)?;
                Ok(())
            }
            Self::Gl(_) => Ok(()),
        }
    }

    /// The registration for one target slot.
    fn registration(&self, slot: usize) -> Option<&Registration> {
        match self {
            Self::Vulkan(vk) => vk.targets.get(slot).map(|target| &target.registration),
            Self::Gl(gl) => gl.targets.get(slot).map(|target| &target.registration),
        }
    }

    /// What the device calls itself, for a startup log line.
    fn name(&self) -> String {
        match self {
            Self::Vulkan(vk) => vk.device.name(),
            Self::Gl(gl) => gl.device.name().to_string(),
        }
    }

    /// Give everything back, which needs the card the regions came from.
    ///
    /// **The converters and devices are not torn down here**, as before: each
    /// device's own release waits for the queue and frees everything built on
    /// it, so naming the pieces again would be a second free.
    fn release(&mut self, card: &Card) {
        match self {
            Self::Vulkan(vk) => {
                for (_, imported) in vk.imports.drain() {
                    vk.device.release(imported);
                }
                for target in vk.targets.drain(..) {
                    vk.device.release_nv12(target.frame);
                }
            }
            Self::Gl(gl) => {
                for (_, imported) in gl.imports.drain() {
                    gl.device.release(imported);
                }
                for target in gl.targets.drain(..) {
                    gl.device.release_nv12(target.frame);
                }
                for region in gl.regions.drain(..) {
                    card.release_linear(region);
                }
            }
        }
    }
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
    /// The import and conversion, on whichever interface was asked for.
    pipeline: Pipeline,
    plane: drm::control::plane::Handle,
    /// The controller that plane is bound to.
    ///
    /// **What the vblank event is armed on.** The event fires per controller,
    /// and asking the wrong one means the events belong to another output.
    crtc_index: u32,
    /// What the display was doing when the imports below were built.
    shape: Shape,
    /// One per buffer of the display's rotation.
    ///
    /// **Keyed by what the buffer is, not by the identifier the kernel gives
    /// its framebuffer.** Identifiers are reused, and they are reused across
    /// the transition where it shows: over a monitor switched off and on, two
    /// came back naming different memory. An import cached against one then
    /// feeds the encoder the picture from before the display went dark,
    /// alternating with live frames until the cache turns over.
    next: usize,
    /// What the last conversion came to, or nothing before the first one.
    ///
    /// **The whole of the duplicate check.** Held here rather than in the loop
    /// because it belongs to the source: a display that is rebuilt starts
    /// again, which is right, and a loop that restarts around the same display
    /// does not have to.
    digest: Option<lowlat_capture::convert::Digest>,
    /// When the conversion in flight captured its picture.
    ///
    /// **The other half of the split acquire.** `acquire` submits the
    /// conversion and stores the timestamp here; `converted` waits for it and
    /// hands both back as the same `Acquired` value the old synchronous
    /// acquire returned. Nothing when no conversion is in flight.
    flight_at: Option<Time>,
    /// The import the last [`Display::acquire`] converted from.
    ///
    /// **What a poke reads.** A poke is a one-workgroup conversion through
    /// the same pipeline, and it needs a source that is already imported;
    /// this is the one the stream is converting anyway. Nothing before the
    /// first acquire, which skips the poke rather than inventing a source.
    last_key: Option<u64>,
    /// The pointer plane, when the pipeline has one. **Found once**: a machine
    /// that draws its pointer in the picture rather than on a plane has none,
    /// and looking for it every frame would be a walk of the whole pipeline
    /// for an answer that does not change.
    cursor_plane: Option<CursorPlane>,
    cursor: Watcher,
    /// Latched when the display changed size, and taken by the loop.
    resized: bool,
    /// What is being captured, by the name one is asked for by.
    selected: Option<String>,
    /// Where this output sits in the desktop, when a session says.
    ///
    /// **Read once, here, rather than per frame.** It costs a round trip to
    /// the session and it changes only when somebody rearranges their
    /// displays, which rebuilds the stream anyway.
    place: Option<Placement>,
}

impl core::fmt::Debug for Display {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Display")
            .field("shape", &self.shape)
            .field("imports", &self.pipeline.cached())
            .field("targets", &self.pipeline.depth())
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
    pub fn open(
        depth: usize,
        wanted: Option<&str>,
        backend: Option<lowlat_capture::Backend>,
        register: Register<'_>,
    ) -> Result<Self, Error> {
        let (node, card, layout) = Self::find(wanted)?;
        let shape = Shape::of(&layout.primary);
        let on = layout.connector.as_deref().unwrap_or("this output");

        let pipeline = match backend {
            Some(lowlat_capture::Backend::Vulkan) => {
                Self::build_vulkan(&node, depth, shape, &register)
            }
            Some(lowlat_capture::Backend::Gl) => {
                Self::build_gl(&node, &card, depth, shape, &register)
            }
            // **Nothing follows the device**: the compute interface where it
            // exists, the fallback where it does not (docs/05-host.md
            // section 4). Only the two errors that mean "this device has no
            // such interface" fall through. The fallback costs about a
            // millisecond more per frame, so any other failure keeps its
            // refusal rather than being masked by a slower tier -- a machine
            // quietly converting on the wrong interface is a measurement
            // nobody can trust and a latency nobody asked for.
            None => match Self::build_vulkan(&node, depth, shape, &register) {
                Err(Error::Convert(vulkan::Error::NoLoader | vulkan::Error::NoDeviceForNode)) => {
                    lowlat_common::log_info!(
                        "display: {on} has no compute interface, converting on the fallback"
                    );
                    Self::build_gl(&node, &card, depth, shape, &register)
                }
                outcome => outcome,
            },
        };
        let pipeline = pipeline.inspect_err(|error| {
            lowlat_common::log_error!(
                "display: the conversion for {on} could not be built, {error}"
            );
            // **Only where it is the likely cause.** A frame is allocated on
            // whichever device the display is on, and an encoder built against
            // another one cannot take it -- so a display that moved between
            // cards, or a backend chosen by hand for the card that is now dark,
            // both arrive here as one unexplained refusal. Every other reason
            // says what it is, and attaching this to those would name a cause
            // that is not the one.
            if matches!(error, Error::Register) {
                lowlat_common::log_error!(
                    "display: a display and its encoder have to be on one device; check which \
                     card {on} is on"
                );
            }
        })?;

        lowlat_common::log_info!(
            "display: {}x{} on {}, {} target(s)",
            shape.width,
            shape.height,
            pipeline.name(),
            pipeline.depth()
        );

        Ok(Self {
            card,
            pipeline,
            plane: layout.primary_plane,
            crtc_index: layout.crtc_index,
            shape,
            next: 0,
            cursor_plane: layout.cursor_plane,
            cursor: Watcher::new(),
            digest: None,
            flight_at: None,
            last_key: None,
            resized: false,
            place: layout
                .connector
                .as_deref()
                .and_then(lowlat_capture::desktop::placement_of),
            selected: layout
                .connector
                .as_deref()
                .map(|connector| identity(&node, connector)),
        })
    }

    /// The primary interface, which allocates its own targets and exports them.
    fn build_vulkan(
        node: &std::path::Path,
        depth: usize,
        shape: Shape,
        register: &Register<'_>,
    ) -> Result<Pipeline, Error> {
        let device = vulkan::Device::for_display(node)?;
        let converter = Converter::new(&device)?;
        let mut targets = Vec::with_capacity(depth);
        for _ in 0..depth {
            let frame = device.allocate_nv12(shape.width, shape.height)?;
            let registration = match register {
                // Each encoder is handed the descriptor kind it has a name for;
                // the allocation is built able to produce either.
                Register::Vendor(encoder) => {
                    let (fd, exported) = device.export_nv12(&frame, false)?;
                    Self::register_vendor(encoder, fd, &exported)
                }
                Register::Open(display) => {
                    let (fd, exported) = device.export_nv12(&frame, true)?;
                    Self::register_open(display, std::os::fd::AsFd::as_fd(&fd), &exported)
                }
            }?;
            targets.push(Target {
                frame,
                registration,
            });
        }
        Ok(Pipeline::Vulkan(Box::new(Vulkan {
            device,
            converter,
            imports: HashMap::new(),
            targets,
        })))
    }

    /// The other interface, whose targets are views of regions the display
    /// device allocated.
    ///
    /// **It reaches one encoder, and the other is refused here.** The vendor
    /// runtime takes an address it was told about, which means importing this
    /// allocation through that vendor's own compute interface; that interface
    /// has no name for a descriptor this one can produce.
    fn build_gl(
        node: &std::path::Path,
        card: &Card,
        depth: usize,
        shape: Shape,
        register: &Register<'_>,
    ) -> Result<Pipeline, Error> {
        let Register::Open(display) = register else {
            return Err(Error::NotTogether);
        };
        let device = lowlat_capture::gl::Device::for_display(node).map_err(Error::Gl)?;
        let converter = lowlat_capture::gl::Converter::new(&device).map_err(Error::Gl)?;

        // Rounded here rather than inside, because the region has to be tall
        // enough for both planes at the rounded height and the caller of
        // `allocate_linear` is what knows that.
        let width = shape.width.next_multiple_of(2);
        let height = shape.height.next_multiple_of(2);

        let mut targets = Vec::with_capacity(depth);
        let mut regions = Vec::with_capacity(depth);
        for _ in 0..depth {
            let (region, fd) = card.allocate_linear(width, height / 2 * 3)?;
            let frame = device
                .import_nv12(
                    std::os::fd::AsRawFd::as_raw_fd(&fd),
                    width,
                    height,
                    region.pitch,
                )
                .map_err(Error::Gl)?;
            // **The same layout the other interface reports**, computed in one
            // place so the two cannot come to describe the region differently.
            let exported = lowlat_capture::convert::Exported::packed(width, height, region.pitch);
            let registration =
                Self::register_open(display, std::os::fd::AsFd::as_fd(&fd), &exported)?;
            regions.push(region);
            targets.push(Target {
                frame,
                registration,
            });
        }
        Ok(Pipeline::Gl(Box::new(Gl {
            device,
            converter,
            imports: HashMap::new(),
            targets,
            regions,
            ready: None,
        })))
    }

    /// Where the captured output sits in the desktop, when that is knowable.
    ///
    /// **Absent is not degraded.** With one output the picture is the desktop,
    /// which is what the absolute axis already spans, so nothing at all is the
    /// right answer rather than a missing one.
    pub fn place(&self) -> Option<Placement> {
        self.place
    }

    /// What the display is showing, before anything is built on it.
    ///
    /// **The encoder has to be configured for this and is created first**, so
    /// the size has to be readable without a device, a converter or a target.
    /// Configuring it for anything else produces a stream of the wrong shape
    /// rather than a refusal, which is a whole session wasted.
    pub fn size_of_display(wanted: Option<&str>) -> Option<(u32, u32)> {
        let (_, _, layout) = Self::find(wanted).ok()?;
        Some((layout.primary.width, layout.primary.height))
    }

    /// Every output on this machine that is lit, by the name one is asked for
    /// by.
    ///
    /// **Scoped by the device it is on.** A connector name is unique within a
    /// device and not across them: two cards each present a `DP-1`, and a bare
    /// name would select whichever was found first. The device's own node name
    /// is the cheapest thing that separates them and is as stable as the
    /// machine's hardware ordering.
    pub fn outputs() -> Vec<Selectable> {
        let mut found = Vec::new();
        for index in 0..NODES {
            let node = std::path::PathBuf::from(format!("/dev/dri/card{index}"));
            if !node.exists() {
                continue;
            }
            let Ok(card) = Card::open(&node) else {
                continue;
            };
            for output in card.outputs().unwrap_or_default() {
                found.push(Selectable {
                    id: identity(&node, &output.connector),
                    place: lowlat_capture::desktop::placement_of(&output.connector),
                    connector: output.connector,
                    width: output.width,
                    height: output.height,
                });
            }
        }
        found
    }

    /// Whether this machine could capture right now, and if not, which half is
    /// missing.
    ///
    /// **Two failures that look identical from outside, told apart.** Nothing
    /// lit means there is no display to capture -- a headless machine, or one
    /// whose session has not started. A display that is lit but whose
    /// framebuffer cannot be read means the process lacks what reaching it
    /// takes, which on this platform is the elevated capability: the plane is
    /// there, the buffer handles are not, and every later stage fails with the
    /// same message as an empty desktop.
    ///
    /// **A read, and nothing more.** No device is opened for encode, no thread
    /// starts, and nothing is left behind.
    pub fn capturable() -> Capturable {
        let mut lit = false;
        for index in 0..NODES {
            let node = std::path::PathBuf::from(format!("/dev/dri/card{index}"));
            if !node.exists() {
                continue;
            }
            let Ok(card) = Card::open(&node) else {
                continue;
            };
            if card.outputs().unwrap_or_default().is_empty() {
                continue;
            }
            lit = true;
            // **Not merely that a plane is lit.** Enumerating a connector and
            // finding its framebuffer both work without the capability; what
            // does not is getting the buffer handles back out of it, and a
            // framebuffer with none is what every later stage fails on. So the
            // probe reads the handles, which is the same step capture takes.
            let Ok(layout) = card.scan() else {
                continue;
            };
            let Ok(frame) = card.framebuffer_on(layout.primary_plane) else {
                continue;
            };
            if frame.planes().next().is_some() {
                return Capturable::Yes;
            }
        }
        if lit {
            Capturable::NotReachable
        } else {
            Capturable::NothingLit
        }
    }

    /// What is being captured, by the same name [`Display::outputs`] gives.
    pub fn selected(&self) -> Option<&str> {
        self.selected.as_deref()
    }

    /// The output this host captures when it is asked for nothing.
    ///
    /// **The same answer [`Display::open`] acts on, from the same code**, so
    /// that what a peer is told it is watching cannot drift from what it is
    /// watching. Deriving it a second time is the trap: a chooser then marks
    /// one screen while the stream carries another, and picking the marked one
    /// looks like a request that changes nothing, because it is.
    pub fn preferred() -> Option<String> {
        let (node, _, layout) = Self::find(None).ok()?;
        Some(identity(&node, layout.connector.as_deref()?))
    }

    /// Which driver is behind the display this host would capture.
    ///
    /// **Asked before anything is built, because it decides what to build.** A
    /// conversion target is allocated on the device the display is on, and an
    /// encoder belonging to another device cannot take it -- so the encoder is
    /// not a preference to be configured, it is a consequence of where the
    /// display is.
    pub fn driver(wanted: Option<&str>) -> Option<String> {
        let (node, _, _) = Self::find(wanted).ok()?;
        driver_of(&node)
    }

    /// The first node with something on it.
    ///
    /// Nodes are tried in order and the first that reports a lit primary plane
    /// wins. A node that reports nothing is skipped rather than failed on,
    /// because that is the ordinary state of a card driving no display.
    fn find(wanted: Option<&str>) -> Result<(std::path::PathBuf, Card, scanout::Layout), Error> {
        // **Asked for nothing means the main screen, not the first one found.**
        // Walking the devices in order answers with whichever card the kernel
        // enumerated first, which on a machine with two is a coin flip and on
        // this one picks the secondary screen. The desktop's own corner is the
        // signal that is really there, and a session that cannot say leaves the
        // walk to decide as before.
        if wanted.is_none()
            && let Some(primary) = lowlat_capture::desktop::at_origin()
            && let Ok(found) = Self::walk(Some(&primary))
        {
            lowlat_common::log_info!("display: {primary} is at the desktop's corner, taking it");
            return Ok(found);
        }
        Self::walk(wanted)
    }

    /// Open the first device that is lighting the output asked for.
    fn walk(wanted: Option<&str>) -> Result<(std::path::PathBuf, Card, scanout::Layout), Error> {
        let mut last = scanout::Error::NoScanout;
        for index in 0..NODES {
            let node = std::path::PathBuf::from(format!("/dev/dri/card{index}"));
            if !node.exists() {
                continue;
            }
            // **The device is named too, so a device that is not the one asked
            // for is skipped without being opened.** Opening one costs the
            // capability negotiation and a walk of its planes.
            let connector = match wanted.map(|id| split(id)) {
                Some((device, connector)) => {
                    if device.is_some_and(|device| Some(device) != node.file_name()) {
                        continue;
                    }
                    Some(connector)
                }
                None => None,
            };
            let opened = Card::open(&node)
                .and_then(|card| card.scan_output(connector).map(|layout| (card, layout)));
            match opened {
                Ok((card, layout)) => {
                    lowlat_common::log_info!(
                        "display: capturing {} on {}",
                        layout.connector.as_deref().unwrap_or("an unnamed output"),
                        node.display()
                    );
                    return Ok((node, card, layout));
                }
                Err(error) => last = error,
            }
        }
        // **Refused rather than fallen back on.** Capturing a different screen
        // from the one asked for looks like the selection working, and the
        // person who asked is the one least able to see that it did not.
        if let Some(wanted) = wanted {
            lowlat_common::log_warn!("display: no output named {wanted} is lit");
        }
        Err(Error::Capture(last))
    }

    /// Register a conversion target with the vendor runtime.
    ///
    /// **The frame leaves by the handle that runtime has a name for.** It has
    /// none for a display-interface descriptor, and the allocation can produce
    /// either.
    pub fn register_vendor(
        encoder: &lowlat_encode::nvenc::Encoder<'_>,
        fd: std::os::fd::OwnedFd,
        exported: &lowlat_capture::convert::Exported,
    ) -> Result<Registration, Error> {
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
        Ok(Registration::Vendor {
            input,
            _taken: taken,
        })
    }

    /// Import a conversion target through the display interface.
    ///
    /// **The same allocation, described rather than copied.** The encoder then
    /// reads the bytes the conversion wrote, which is the whole reason this
    /// backend has a display source at all.
    pub fn register_open(
        display: &lowlat_encode::vaapi::Display<'_>,
        fd: std::os::fd::BorrowedFd<'_>,
        exported: &lowlat_capture::convert::Exported,
    ) -> Result<Registration, Error> {
        let surface = display.import(fd, exported).map_err(|_| Error::Register)?;
        Ok(Registration::Open { surface })
    }

    /// The registration the last [`Display::acquire`] converted into.
    ///
    /// Separate from the acquire so a caller can hold it across other work
    /// without holding the whole source borrowed.
    pub fn presented(&self) -> Option<&Registration> {
        let slot = self.next.checked_sub(1)? % self.pipeline.depth().max(1);
        self.pipeline.registration(slot)
    }

    /// Submit the conversion of what the display is showing now.
    ///
    /// **Returns as soon as the conversion is queued, not when it is done.**
    /// The registration is then [`Display::presented`] once [`Display::converted`]
    /// has collected the picture; the pair replaces what a synchronous acquire
    /// did in one call, so the conversion runs on the device while the loop
    /// collects the previous picture instead of the wait landing in the
    /// acquire stage.
    ///
    /// The returned time is when the picture was taken, which is what every
    /// latency figure is measured from; [`Display::converted`] hands it back
    /// alongside whether the picture changed.
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
            // **A size change is not something this can absorb.** The
            // conversion target and the encoder are both built for one size,
            // so the picture would land in a corner of a frame the rest of
            // which never changes again. Latched for the loop, which rebuilds
            // around it.
            self.resized |= (shape.width, shape.height) != (self.shape.width, self.shape.height);
            self.forget_imports();
            self.shape = shape;
        }

        let key = self.ensure_import(&fb)?;

        // The slot is chosen and the cursor advanced before anything is
        // borrowed out of the targets, because the registration handed back
        // borrows them until the caller is done with it.
        let slot = self.next % self.pipeline.depth().max(1);
        self.next = self.next.wrapping_add(1);

        self.pipeline.submit(key, slot)?;
        self.flight_at = Some(began);
        self.last_key = Some(key);
        Ok(began)
    }

    /// Collect the conversion [`Display::acquire`] submitted, waiting for it.
    ///
    /// **The wait is the point of the pair.** The submit happened before the
    /// loop's collect of the previous picture, so the device has had the whole
    /// collect to finish; what is waited on here is normally already done.
    /// Nothing when no conversion was submitted, which is the ordinary state
    /// of a display that could not be read.
    pub fn converted(&mut self) -> Result<Option<Acquired>, Error> {
        let Some(digest) = self.pipeline.collect()? else {
            return Ok(None);
        };
        let at = self.flight_at.take().unwrap_or_else(Time::now);

        // **What was drawn, not which buffer it was drawn into.** The exported
        // identity says only that the buffer was not swapped, and this
        // compositor redraws in place on most frames, so keying on it would
        // call a changed picture unchanged. The conversion reads every pixel
        // anyway and hands back a summary of what it wrote.
        //
        // **The targets rotate and that does not matter**: the summary is of
        // the content, so the same picture converted into a different slot
        // gives the same answer.
        let changed = self.digest != Some(digest);
        self.digest = Some(digest);
        Ok(Some(Acquired { at, changed }))
    }

    /// Make sure this buffer is imported, and say which import it is.
    ///
    /// **The buffer is exported every frame, and it is what identifies it.**
    /// The export costs a handful of microseconds and is the only thing that
    /// says which memory is behind a framebuffer; the identifier beside it
    /// does not, because the kernel reuses those.
    fn ensure_import(&mut self, fb: &scanout::Framebuffer) -> Result<u64, Error> {
        let first = fb.planes().next().ok_or(scanout::Error::NoScanout)?;
        let fd = self.card.export(first)?;
        let key = scanout::identity(&fd).ok_or(Error::Register)?;
        if self.pipeline.holds(key) {
            return Ok(key);
        }
        // Bounded rather than unbounded. A compositor that cycles more widely
        // than expected costs a rebuild per turn, which is a cost; an
        // unbounded map would be a leak.
        if self.pipeline.cached() >= IMPORTS {
            self.forget_imports();
        }
        let planes: Vec<PlaneLayout> = fb
            .planes()
            .map(|buffer| PlaneLayout {
                offset: buffer.offset,
                pitch: buffer.pitch,
            })
            .collect();
        self.pipeline.take(
            key,
            fd,
            Imports {
                width: fb.width,
                height: fb.height,
                format: fb.format,
                modifier: fb.modifier.map_or(0, u64::from),
                // Replaced inside, because the two interfaces differ over who
                // owns it.
                fd: -1,
                planes: &planes,
            },
        )?;
        Ok(key)
    }

    /// Drop every import, which is what a changed picture requires.
    fn forget_imports(&mut self) {
        self.pipeline.forget();
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

    /// Whether anything is still plugged into the device being captured.
    ///
    /// **A controller that lost its connector keeps scanning out**, holding
    /// the last picture it was given, so every read here succeeds and the
    /// stream carries a desktop that will never change again. Nothing else in
    /// this path can tell that from a desktop nobody is touching.
    pub fn attached(&self) -> bool {
        // A device that cannot answer is not evidence that nothing is plugged
        // into it, and tearing a session down on a failed query would be worse
        // than the state it is looking for.
        self.card.attached().unwrap_or(true)
    }

    /// The descriptor the vblank event arrives on.
    ///
    /// **Handed out for a poll, not for a read.** The loop waits on this
    /// alongside its own deadlines, then drains the event through
    /// [`Display::drain_events`] once poll says it is readable.
    pub fn poll_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.card.as_fd()
    }

    /// Arm the next vblank event on the captured output's controller.
    ///
    /// **One per call, consumed by [`Display::drain_events`].** A refused arm
    /// is not a failure: the loop's timer paces the tick alone, which is the
    /// same behaviour a display with no vblank events gets.
    pub fn arm_vblank(&self) -> Result<(), Error> {
        self.card
            .arm_vblank(self.crtc_index)
            .map_err(Error::Capture)
    }

    /// Read every pending event, saying whether a vblank was among them.
    ///
    /// **Asked only after poll says the descriptor is readable.** Events
    /// accumulate while the loop is busy, so this drains rather than reads
    /// one, and an empty queue is `false` rather than a wait.
    pub fn drain_events(&self) -> bool {
        self.card.drain_events().unwrap_or(false)
    }

    /// Poke the device with a one-workgroup conversion, for the next
    /// conversion.
    ///
    /// **The wakeup is paid here rather than by the conversion.** An
    /// integrated device powers its compute block down after a few
    /// milliseconds of idle, and the first real work after the gap pays the
    /// wakeup; a loop that already knows it will convert on the next present
    /// calls this a moment before it, so the conversion lands on a warm
    /// block. The digest it produces is discarded, and the block it writes
    /// lands in a slot the next conversion overwrites whole.
    pub fn poke(&mut self) -> Result<(), Error> {
        let Some(key) = self.last_key else {
            // Nothing converted yet: there is no source to poke with, and
            // the first conversion pays the wakeup once.
            return Ok(());
        };
        let slot = self.next % self.pipeline.depth().max(1);
        self.pipeline.poke(key, slot)
    }

    /// Whether the display changed size, clearing the answer.
    pub fn take_resize(&mut self) -> bool {
        core::mem::replace(&mut self.resized, false)
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
        self.pipeline.release(&self.card);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A connector name is unique within a device and not across them.** Two
    /// cards each present a `DP-1`, so a bare name would select whichever was
    /// walked first, which is the kernel's order and not anybody's intent.
    #[test]
    fn an_identity_names_the_device_and_the_connector() {
        let id = identity(std::path::Path::new("/dev/dri/card1"), "DP-2");
        assert_eq!(id, "card1:DP-2");
        assert_eq!(
            split(&id),
            (Some(std::ffi::OsStr::new("card1")), "DP-2"),
            "an identity must survive the round trip it is stored across"
        );
    }

    /// **A bare connector name is accepted and means "wherever it is".** It is
    /// what somebody typing one by hand writes, and it is unambiguous on a
    /// machine with one device, which is nearly all of them.
    #[test]
    fn a_bare_connector_name_names_no_device() {
        assert_eq!(split("DP-2"), (None, "DP-2"));
        assert_eq!(split("eDP-1"), (None, "eDP-1"));
    }

    /// **The driver behind a display is read from the system, not guessed from
    /// the node.** A node's number is an ordering and says nothing about what
    /// is driving it: on this machine the vendor's card is the second one, and
    /// on the next machine it will not be.
    #[test]
    fn a_driver_is_read_from_the_device_rather_than_its_number() {
        // Whatever this machine has, the answer for a node that exists must
        // come from the link and one for a node that does not must be absent.
        let missing = driver_of(std::path::Path::new("/dev/dri/card9999"));
        assert_eq!(missing, None, "a device that is not there named a driver");

        for index in 0..4 {
            let node = std::path::PathBuf::from(format!("/dev/dri/card{index}"));
            if !node.exists() {
                continue;
            }
            let named = driver_of(&node);
            assert!(
                named.as_ref().is_none_or(|driver| !driver.is_empty()),
                "a device that answered named an empty driver"
            );
        }
    }

    /// A device that is not the one asked for is skipped before it is opened,
    /// and the comparison is against the node's own file name rather than the
    /// whole path.
    #[test]
    fn a_named_device_is_matched_by_its_node_name() {
        let (device, connector) = split("card0:HDMI-A-1");
        let node = std::path::Path::new("/dev/dri/card0");
        assert_eq!(device, node.file_name());
        assert_eq!(connector, "HDMI-A-1");

        let other = std::path::Path::new("/dev/dri/card1");
        assert_ne!(device, other.file_name());
    }
}
