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

/// One conversion target, registered with the encoder once.
struct Target {
    frame: Nv12,
    registration: Registration,
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
    /// One per buffer of the display's rotation.
    ///
    /// **Keyed by what the buffer is, not by the identifier the kernel gives
    /// its framebuffer.** Identifiers are reused, and they are reused across
    /// the transition where it shows: over a monitor switched off and on, two
    /// came back naming different memory. An import cached against one then
    /// feeds the encoder the picture from before the display went dark,
    /// alternating with live frames until the cache turns over.
    imports: HashMap<u64, vulkan::Imported>,
    targets: Vec<Target>,
    next: usize,
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
    pub fn open(
        depth: usize,
        wanted: Option<&str>,
        register: impl Fn(&vulkan::Device, &Nv12) -> Result<Registration, Error>,
    ) -> Result<Self, Error> {
        let (node, card, layout) = Self::find(wanted)?;
        let device = vulkan::Device::for_display(&node)?;
        let converter = Converter::new(&device)?;
        let shape = Shape::of(&layout.primary);

        let mut targets = Vec::with_capacity(depth);
        for _ in 0..depth {
            let frame = device.allocate_nv12(shape.width, shape.height)?;
            // **A refusal here almost always means the wrong device.** The
            // frame is allocated on whichever device the display is on, and an
            // encoder built against another one cannot take it -- so a display
            // that moved between cards, or a backend chosen by hand for the
            // card that is now dark, both arrive as one unexplained refusal.
            // The device is named because it is the missing half of that.
            let registration = register(&device, &frame).inspect_err(|_| {
                lowlat_common::log_error!(
                    "display: the encoder cannot take frames from {}, which is the device {} is \
                     on; a display and its encoder have to be on one device",
                    device.name(),
                    layout.connector.as_deref().unwrap_or("this output")
                );
            })?;
            targets.push(Target {
                frame,
                registration,
            });
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
        device: &vulkan::Device,
        encoder: &lowlat_encode::nvenc::Encoder<'_>,
        frame: &Nv12,
    ) -> Result<Registration, Error> {
        let (fd, exported) = device.export_nv12(frame, false)?;
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
        device: &vulkan::Device,
        display: &lowlat_encode::vaapi::Display<'_>,
        frame: &Nv12,
    ) -> Result<Registration, Error> {
        let (fd, exported) = device.export_nv12(frame, true)?;
        let surface = display
            .import(std::os::fd::AsFd::as_fd(&fd), &exported)
            .map_err(|_| Error::Register)?;
        Ok(Registration::Open { surface })
    }

    /// The registration the last [`Display::acquire`] converted into.
    ///
    /// Separate from the acquire so a caller can hold it across other work
    /// without holding the whole source borrowed.
    pub fn presented(&self) -> Option<&Registration> {
        let slot = self.next.checked_sub(1)? % self.targets.len().max(1);
        self.targets.get(slot).map(|target| &target.registration)
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
        let slot = self.next % self.targets.len().max(1);
        self.next = self.next.wrapping_add(1);

        let source = self.imports.get(&key).ok_or(Error::Register)?;
        let target = self.targets.get(slot).ok_or(Error::Register)?;
        self.converter
            .run(&self.device, source, &target.frame, false)?;
        Ok(began)
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
        if self.imports.contains_key(&key) {
            return Ok(key);
        }
        // Bounded rather than unbounded. A compositor that cycles more widely
        // than expected costs a rebuild per turn, which is a cost; an
        // unbounded map would be a leak.
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
        let imported = self.device.import(&Imports {
            width: fb.width,
            height: fb.height,
            format: fb.format,
            modifier: fb.modifier.map_or(0, u64::from),
            fd: <std::os::fd::OwnedFd as std::os::fd::IntoRawFd>::into_raw_fd(fd),
            planes: &planes,
        })?;
        self.imports.insert(key, imported);
        Ok(key)
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
        self.forget_imports();
        for target in self.targets.drain(..) {
            self.device.release_nv12(target.frame);
        }
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
