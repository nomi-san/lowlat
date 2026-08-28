//! The device that imports a captured framebuffer and converts it.
//!
//! Import and conversion are one interface on one device, chosen because it is
//! the only one that takes a buffer's tiling modifier explicitly and runs the
//! same compute shader on every driver here. Encoding stays where it is; this
//! hands it a plain untiled result.
//!
//! **The device is picked to match the display, not by preference.** A frame
//! captured from one card cannot be imported by another without a copy through
//! system memory, which is the thing this whole path exists to avoid.

use std::ffi::CStr;
use std::os::fd::RawFd;
use std::path::Path;

use ash::vk;
use drm::buffer::DrmFourcc;

/// What went wrong.
///
/// Driver results are carried as their raw code rather than as a formatted
/// message, so the error type stays `Copy` and allocation free.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// The loader is absent or refused to load.
    NoLoader,
    /// A call into the driver failed.
    Driver(i32),
    /// No device on this machine drives the display node that was asked for.
    /// Either the node is wrong or its driver does not report which one it is.
    NoDeviceForNode,
    /// The device that drives the display cannot import a captured buffer.
    /// Named individually because the answer differs per extension and a
    /// missing one is a deployment fact rather than a bug.
    Unsupported(&'static str),
    /// The device exposes no queue that can run the conversion.
    NoQueue,
    /// The captured framebuffer is in a layout this interface has no name for.
    UnknownFormat,
    /// No memory type satisfies both the image and the imported descriptor.
    NoMemoryType,
    /// The device will export a picture in neither of the two ways a frame is
    /// handed on.
    ///
    /// **Asked rather than assumed.** Which handle kinds a device exports, and
    /// whether it exports a pair from one allocation, is a property of the
    /// exact image being made and has to be queried; a device that offers
    /// neither cannot hand a picture to any encoder here, so it is refused
    /// while there is still nothing waiting on it.
    NoExport,
    /// The committed shader is not a whole number of words, or the driver
    /// built nothing from it. Either way the file beside the source is wrong.
    BadShader,
    /// The two planes of a conversion target cannot be laid out the way an
    /// encoder reads them: their rows came back different lengths, or the
    /// colour plane cannot start where the encoder expects. Refused rather
    /// than worked around, because the layout is what an encoder is told and
    /// there is no way to tell it anything else.
    PlanesDisagree,
    /// A conversion was submitted while another is still in flight. The
    /// converter holds one fence and one command buffer, so two in flight is
    /// not a state it can serve; the caller has not collected the previous
    /// picture yet.
    Busy,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NoLoader => f.write_str("no usable driver loader on this system"),
            Self::Driver(code) => write!(f, "driver call failed, result {code}"),
            Self::NoDeviceForNode => f.write_str("no device reports driving that display node"),
            Self::Unsupported(what) => write!(f, "the display's device does not support {what}"),
            Self::NoQueue => f.write_str("the display's device exposes no usable queue"),
            Self::UnknownFormat => f.write_str("captured pixel layout has no equivalent here"),
            Self::NoMemoryType => f.write_str("no memory type suits both image and descriptor"),
            Self::NoExport => f.write_str("the device exports a picture in neither way"),
            Self::BadShader => f.write_str("the committed conversion shader is unusable"),
            Self::PlanesDisagree => {
                f.write_str("the frame's planes cannot be laid out for an encoder")
            }
            Self::Busy => f.write_str("a conversion is already in flight"),
        }
    }
}

impl std::error::Error for Error {}

pub(crate) fn driver(result: vk::Result) -> Error {
    Error::Driver(result.as_raw())
}

/// Everything the import needs from the device, and nothing it does not.
///
/// Each is checked by name at startup so a machine that cannot do this says
/// which part is missing, rather than failing at the first import with a
/// result code that names no cause.
const REQUIRED: [&CStr; 5] = [
    // Take a buffer in by file descriptor.
    ash::khr::external_memory_fd::NAME,
    // ...and specifically one shared the way a display buffer is shared.
    ash::ext::external_memory_dma_buf::NAME,
    // Describe that buffer's tiling rather than assuming it is untiled. This
    // is the one that makes the whole approach possible: both drivers here
    // hand out a tiled or compressed buffer and neither can be read without
    // being told how.
    ash::ext::image_drm_format_modifier::NAME,
    // Take ownership of an image that was last written by something outside
    // this interface entirely, which is what the display is.
    ash::ext::queue_family_foreign::NAME,
    // **Required by the tiling one and not implied by it.** It is part of the
    // core from a later version than this asks for, so here it is an extension
    // like any other. Leaving it out builds a device every driver accepts and
    // the specification does not, which is invisible without a validation
    // layer and is exactly the kind of thing that stops working on an upgrade.
    ash::khr::image_format_list::NAME,
];

/// What an encoder on this same device needs, on top of [`REQUIRED`].
///
/// **Asked for only when a caller says it wants one.** Every one of these is
/// newer than the list above and narrows the machines that open at all, so a
/// device built for capture alone must not carry them.
const REQUIRED_ENCODE: [&CStr; 4] = [
    ash::khr::video_queue::NAME,
    ash::khr::video_encode_queue::NAME,
    ash::khr::video_encode_h264::NAME,
    // **The video interface is specified against this and does not imply it.**
    // It is part of the core from a later version than this asks for, so on
    // this instance it is an extension like any other; leaving it out builds a
    // device the driver accepts and the specification does not.
    ash::khr::synchronization2::NAME,
];

/// What an encoder on this device may also use, where the device has it.
///
/// **Enabled where advertised and never required.** A device that encodes one
/// codec and not the other has to keep the path for the one it has, so this
/// list narrows nothing: a codec the device never advertised is refused later
/// by the encoder, with a reason, rather than here by the device failing to
/// open at all.
const OPTIONAL_ENCODE: [&CStr; 1] = [ash::khr::video_encode_h265::NAME];

/// What lets a device say which display node it drives.
///
/// **Apart from the list above, because it is read rather than enabled.** It
/// supplies a property and no device-level behaviour, so it is checked where
/// the property is read and never named at device creation.
const REPORTS_NODE: &CStr = ash::ext::physical_device_drm::NAME;

/// Whether a device offers an extension by name.
fn advertises(available: &[vk::ExtensionProperties], wanted: &CStr) -> bool {
    available
        .iter()
        .any(|extension| extension.extension_name_as_c_str() == Ok(wanted))
}

/// The device the display is on, ready to import from it.
///
/// **Cheaply cloneable, one underlying device.** The display pipeline holds a
/// clone, and an encoder sharing the device holds another; the last clone
/// dropped releases the device, so drop order between them stops mattering.
/// Cloning happens at session construction, never on a frame path.
#[derive(Clone)]
pub struct Device(std::sync::Arc<DeviceInner>);

impl core::ops::Deref for Device {
    type Target = DeviceInner;

    fn deref(&self) -> &DeviceInner {
        &self.0
    }
}

impl core::fmt::Debug for Device {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        self.0.fmt(f)
    }
}

/// What one device holds. Reached through [`Device`], never owned directly.
pub struct DeviceInner {
    /// Dropped last. Every handle below is scoped to it.
    _entry: ash::Entry,
    pub(crate) instance: ash::Instance,
    pub(crate) physical: vk::PhysicalDevice,
    pub(crate) device: ash::Device,
    pub(crate) queue: vk::Queue,
    pub(crate) queue_family: u32,
    /// The queue an encoder on this same device would submit to, when one was
    /// asked for.
    ///
    /// **Absent is the normal case.** A device is opened for capture and
    /// conversion; only a caller that intends to encode here as well asks for
    /// this, and asking narrows which machines open at all.
    encode: Option<(vk::Queue, u32)>,
}

impl core::fmt::Debug for DeviceInner {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Device")
            .field("queue_family", &self.queue_family)
            .finish_non_exhaustive()
    }
}

/// A device node's major and minor numbers.
///
/// Decomposed here rather than through the C helpers, which is four lines of
/// shifting against one more dependency.
fn node_numbers(path: &Path) -> Option<(u32, u32)> {
    use std::os::unix::fs::MetadataExt;
    let rdev = std::fs::metadata(path).ok()?.rdev();
    let major = u32::try_from(((rdev >> 8) & 0xfff) | ((rdev >> 32) & !0xfff)).ok()?;
    let minor = u32::try_from((rdev & 0xff) | ((rdev >> 12) & !0xff)).ok()?;
    Some((major, minor))
}

impl Device {
    /// Open the device that drives a given display node.
    pub fn for_display(node: &Path) -> Result<Self, Error> {
        Self::opened(node, false)
    }

    /// The same device, also able to encode.
    ///
    /// **This is what makes a frame never leave the device.** A conversion and
    /// an encode on one device through one interface hand a picture over as a
    /// picture; on two interfaces the handover costs more per frame than the
    /// conversion itself. It refuses on a machine whose display device cannot
    /// encode, which is a different and smaller set than the one that can
    /// convert.
    pub fn for_display_and_encode(node: &Path) -> Result<Self, Error> {
        Self::opened(node, true)
    }

    /// The queue and family an encoder on this device submits to.
    pub fn encode_queue(&self) -> Option<(vk::Queue, u32)> {
        self.encode
    }

    /// The loader, for an extension table built above this device.
    pub fn entry(&self) -> &ash::Entry {
        &self._entry
    }

    /// The instance, for whatever builds on this device.
    pub fn ash_instance(&self) -> &ash::Instance {
        &self.instance
    }

    /// The device itself, for whatever builds on it.
    ///
    /// An encoder sharing this device holds a [`Device`] clone, which is what
    /// keeps these handles alive for as long as it needs them.
    pub fn ash(&self) -> &ash::Device {
        &self.device
    }

    /// The physical device behind it.
    pub fn physical(&self) -> vk::PhysicalDevice {
        self.physical
    }

    /// The family the conversion submits on, which is the family that writes
    /// an encoder's picture when the two share the device.
    pub fn family(&self) -> u32 {
        self.queue_family
    }

    fn opened(node: &Path, encode: bool) -> Result<Self, Error> {
        let (major, minor) = node_numbers(node).ok_or(Error::NoDeviceForNode)?;

        // SAFETY: loads the system driver loader. Nothing is passed in and the
        // handle is kept for the lifetime of everything derived from it.
        let entry = unsafe { ash::Entry::load() }.map_err(|_| Error::NoLoader)?;

        let application = vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_1);
        let create = vk::InstanceCreateInfo::default().application_info(&application);
        // SAFETY: the create info borrows `application`, which outlives it, and
        // names no extensions or layers.
        let instance = unsafe { entry.create_instance(&create, None) }.map_err(driver)?;

        // Everything after the instance exists can fail, and the instance has
        // to be released exactly once on that path. Resolving it all into one
        // result keeps that to a single place.
        let opened = Self::find(&instance, major, minor).and_then(|physical| {
            Self::open(&instance, physical, encode)
                .map(|(device, queue, family, encode)| (physical, device, queue, family, encode))
        });

        match opened {
            Ok((physical, device, queue, queue_family, encode)) => {
                Ok(Self(std::sync::Arc::new(DeviceInner {
                    _entry: entry,
                    instance,
                    physical,
                    device,
                    queue,
                    queue_family,
                    encode,
                })))
            }
            Err(error) => {
                // SAFETY: nothing created from this instance outlives the
                // failed call, so it is the only thing left to release.
                unsafe { instance.destroy_instance(None) };
                Err(error)
            }
        }
    }

    /// Open any device that can do this, for a test with no display attached.
    ///
    /// **Not for the product.** A conversion has to run on the device the frame
    /// is on, and picking whichever came first would be exactly the
    /// cross-device copy the whole path avoids. It exists so the colour
    /// transform can be checked against a reference without a screen, which is
    /// the only check of it that cannot be fooled by the contents of a desktop.
    pub fn any() -> Result<Self, Error> {
        // SAFETY: as in for_display.
        let entry = unsafe { ash::Entry::load() }.map_err(|_| Error::NoLoader)?;
        let application = vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_1);
        let create = vk::InstanceCreateInfo::default().application_info(&application);
        // SAFETY: the create info outlives the call.
        let instance = unsafe { entry.create_instance(&create, None) }.map_err(driver)?;

        // SAFETY: enumerating from a live instance.
        let candidates = unsafe { instance.enumerate_physical_devices() }
            .map_err(driver)
            .unwrap_or_default();
        let opened = candidates
            .into_iter()
            .find_map(|physical| {
                Self::open(&instance, physical, false)
                    .ok()
                    .map(|(device, queue, family, _)| (physical, device, queue, family))
            })
            .ok_or(Error::NoDeviceForNode);

        match opened {
            Ok((physical, device, queue, queue_family)) => {
                Ok(Self(std::sync::Arc::new(DeviceInner {
                    _entry: entry,
                    instance,
                    physical,
                    device,
                    queue,
                    queue_family,
                    encode: None,
                })))
            }
            Err(error) => {
                // SAFETY: nothing created from it outlives the failed call.
                unsafe { instance.destroy_instance(None) };
                Err(error)
            }
        }
    }

    /// The device that reports driving this display node.
    ///
    /// Matched on the node numbers the driver itself reports, which is exact.
    /// Matching on a name or an index instead breaks the moment a machine has
    /// two cards from the same vendor, or reorders them across a reboot.
    fn find(instance: &ash::Instance, major: u32, minor: u32) -> Result<vk::PhysicalDevice, Error> {
        // SAFETY: enumerating from a live instance.
        let candidates = unsafe { instance.enumerate_physical_devices() }.map_err(driver)?;
        // **A device that cannot say which node it drives is indistinguishable
        // here from one that drives a different node**: the property comes back
        // zeroed either way. Left alone, a driver too old to answer the
        // question reads as a machine with nothing on its display, which sends
        // the next person to look at the display. Counting the ones that
        // answered is what separates the two.
        let mut answered = false;
        for physical in candidates {
            // SAFETY: the device came from this instance.
            let available = unsafe { instance.enumerate_device_extension_properties(physical) }
                .map_err(driver)?;
            if !advertises(&available, REPORTS_NODE) {
                continue;
            }
            answered = true;
            let mut drm = vk::PhysicalDeviceDrmPropertiesEXT::default();
            let mut properties = vk::PhysicalDeviceProperties2::default().push_next(&mut drm);
            // SAFETY: the chain is built from stack values that outlive the
            // call, and the device came from this instance.
            unsafe { instance.get_physical_device_properties2(physical, &mut properties) };
            if drm.has_primary != 0
                && u32::try_from(drm.primary_major) == Ok(major)
                && u32::try_from(drm.primary_minor) == Ok(minor)
            {
                return Ok(physical);
            }
        }
        if answered {
            Err(Error::NoDeviceForNode)
        } else {
            Err(Error::Unsupported(
                REPORTS_NODE.to_str().unwrap_or("a required interface"),
            ))
        }
    }

    /// Check what the chosen device can do, then open it.
    #[expect(
        clippy::type_complexity,
        reason = "the pieces a device is made of, resolved once"
    )]
    fn open(
        instance: &ash::Instance,
        physical: vk::PhysicalDevice,
        encode: bool,
    ) -> Result<(ash::Device, vk::Queue, u32, Option<(vk::Queue, u32)>), Error> {
        // SAFETY: the device came from this instance.
        let available =
            unsafe { instance.enumerate_device_extension_properties(physical) }.map_err(driver)?;
        for wanted in REQUIRED {
            if !advertises(&available, wanted) {
                return Err(Error::Unsupported(
                    wanted.to_str().unwrap_or("a required interface"),
                ));
            }
        }
        if encode {
            for wanted in REQUIRED_ENCODE {
                if !advertises(&available, wanted) {
                    return Err(Error::Unsupported(
                        wanted.to_str().unwrap_or("a required interface"),
                    ));
                }
            }
        }

        // **Asked for before they are requested.** Naming an unsupported
        // feature at device creation is refused with one result code covering
        // every feature in the chain, so the two below are checked here where
        // the answer can say which one; the reasons they are needed are at the
        // request itself.
        let mut supported_ycbcr = vk::PhysicalDeviceSamplerYcbcrConversionFeatures::default();
        let mut supported = vk::PhysicalDeviceFeatures2::default().push_next(&mut supported_ycbcr);
        // SAFETY: the chain is built from stack values that outlive the call,
        // and the device came from this instance.
        unsafe { instance.get_physical_device_features2(physical, &mut supported) };
        if supported.features.shader_storage_image_extended_formats != vk::TRUE {
            return Err(Error::Unsupported("extended storage image formats"));
        }
        if supported_ycbcr.sampler_ycbcr_conversion != vk::TRUE {
            return Err(Error::Unsupported("two-plane image layouts"));
        }

        // Compute alone. The conversion is a shader and a copy; nothing here
        // draws, so asking for graphics would reject devices that could serve
        // us perfectly well.
        // SAFETY: the device came from this instance.
        let families = unsafe { instance.get_physical_device_queue_family_properties(physical) };
        let queue_family = families
            .iter()
            .position(|family| family.queue_flags.contains(vk::QueueFlags::COMPUTE))
            .and_then(|at| u32::try_from(at).ok())
            .ok_or(Error::NoQueue)?;

        // The family that encodes, which is not required to be the one that
        // converts and on one vendor here cannot even copy.
        let encode_family = if encode {
            let found = families
                .iter()
                .position(|family| {
                    family
                        .queue_flags
                        .contains(vk::QueueFlags::VIDEO_ENCODE_KHR)
                })
                .and_then(|at| u32::try_from(at).ok())
                .ok_or(Error::NoQueue)?;
            Some(found)
        } else {
            None
        };

        let priorities = [1.0_f32];
        let mut queues = vec![
            vk::DeviceQueueCreateInfo::default()
                .queue_family_index(queue_family)
                .queue_priorities(&priorities),
        ];
        if let Some(family) = encode_family.filter(|family| *family != queue_family) {
            queues.push(
                vk::DeviceQueueCreateInfo::default()
                    .queue_family_index(family)
                    .queue_priorities(&priorities),
            );
        }
        let mut names: Vec<*const core::ffi::c_char> =
            REQUIRED.iter().map(|name| name.as_ptr()).collect();
        if encode {
            names.extend(REQUIRED_ENCODE.iter().map(|name| name.as_ptr()));
            for wanted in OPTIONAL_ENCODE {
                if advertises(&available, wanted) {
                    names.push(wanted.as_ptr());
                }
            }
        }
        // The single-byte and two-byte storage formats the conversion writes
        // are not in the set every device must support unasked.
        let mut features = vk::PhysicalDeviceFeatures2::default();
        features.features.shader_storage_image_extended_formats = vk::TRUE;
        // Creating an image in a two-plane layout needs this even though
        // nothing here samples one: the conversion reaches its planes through
        // views, because the two-plane format itself cannot be written to on
        // any device seen here while each of its planes can.
        let mut ycbcr = vk::PhysicalDeviceSamplerYcbcrConversionFeatures::default()
            .sampler_ycbcr_conversion(true);
        // Turned on with the video interface and never otherwise.
        let mut sync = vk::PhysicalDeviceSynchronization2Features::default().synchronization2(true);
        let mut create = vk::DeviceCreateInfo::default()
            .queue_create_infos(&queues)
            .enabled_extension_names(&names)
            .push_next(&mut features)
            .push_next(&mut ycbcr);
        if encode {
            create = create.push_next(&mut sync);
        }
        // SAFETY: every borrowed slice outlives the call, and the extension
        // names are static.
        let device = unsafe { instance.create_device(physical, &create, None) }.map_err(driver)?;
        // SAFETY: the family index came from this device's own enumeration and
        // one queue was requested from it.
        let queue = unsafe { device.get_device_queue(queue_family, 0) };
        let encode = encode_family.map(|family| {
            // SAFETY: the family was requested at device creation just above.
            let queue = unsafe { device.get_device_queue(family, 0) };
            (queue, family)
        });
        Ok((device, queue, queue_family, encode))
    }

    /// What the driver calls itself, for a startup log line.
    pub fn name(&self) -> String {
        let mut properties = vk::PhysicalDeviceProperties2::default();
        // SAFETY: the device came from this instance and the chain is one
        // stack value that outlives the call.
        unsafe {
            self.instance
                .get_physical_device_properties2(self.physical, &mut properties)
        };
        properties
            .properties
            .device_name_as_c_str()
            .ok()
            .and_then(|name| name.to_str().ok())
            .unwrap_or("unknown")
            .to_string()
    }

    /// The queue conversion work is submitted to.
    pub fn queue(&self) -> vk::Queue {
        self.queue
    }

    /// The family that queue belongs to, which image ownership transfers name.
    pub fn queue_family(&self) -> u32 {
        self.queue_family
    }
}

impl Drop for DeviceInner {
    fn drop(&mut self) {
        // **On the inner value, so the last clone is what releases.** A drop
        // on the wrapper would destroy the device the first time any clone
        // went away, with the others still holding it.
        // SAFETY: both handles are live until here, and nothing derived from
        // them outlives this type. The wait is what makes that true: work still
        // running would otherwise be holding memory that is about to go.
        unsafe {
            let _ = self.device.device_wait_idle();
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}

/// The pixel layout a captured buffer is in, as this interface names it.
///
/// **The mapping is by byte order, not by the name's letters.** A format whose
/// name reads one way round is often stored the other way, and getting it wrong
/// produces a picture with red and blue exchanged that looks close enough to be
/// missed on a desktop and obvious on a face.
fn pixel_format(fourcc: DrmFourcc) -> Option<vk::Format> {
    Some(match fourcc {
        // Bytes in memory are R, G, B, A.
        DrmFourcc::Abgr8888 => vk::Format::R8G8B8A8_UNORM,
        DrmFourcc::Xbgr8888 => vk::Format::R8G8B8A8_UNORM,
        // Bytes in memory are B, G, R, A. This is the pointer's format.
        DrmFourcc::Argb8888 => vk::Format::B8G8R8A8_UNORM,
        DrmFourcc::Xrgb8888 => vk::Format::B8G8R8A8_UNORM,
        // One packed word, alpha in the top two bits then blue, green, red.
        DrmFourcc::Abgr2101010 => vk::Format::A2B10G10R10_UNORM_PACK32,
        DrmFourcc::Xbgr2101010 => vk::Format::A2B10G10R10_UNORM_PACK32,
        _ => return None,
    })
}

/// A captured framebuffer, imported without a copy.
///
/// Holds the memory it was imported into, so dropping it releases both.
pub struct Imported {
    pub(crate) image: vk::Image,
    memory: vk::DeviceMemory,
    pub width: u32,
    pub height: u32,
    pub format: vk::Format,
    /// Whether the last thing to write this was outside this interface.
    ///
    /// True for a capture, which the display wrote. False for anything built
    /// here. It decides which previous owner a barrier names, and naming the
    /// wrong one is not cosmetic: claiming a foreign owner for an image nobody
    /// else has touched is an error the validation layers report, and claiming
    /// ours for a captured one lets a driver assume a layout it never had.
    pub(crate) foreign: bool,
}

impl core::fmt::Debug for Imported {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Imported")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("format", &self.format)
            .finish_non_exhaustive()
    }
}

impl Device {
    /// Import a captured framebuffer.
    ///
    /// **The descriptor is consumed on success.** This interface takes
    /// ownership of an imported descriptor, so closing it here as well would
    /// close a descriptor the driver still holds.
    ///
    /// The tiling modifier and the per-plane pitches are handed over rather
    /// than inferred. That is the whole reason for choosing this interface:
    /// both drivers here scan out of something tiled or compressed, and a
    /// buffer read as though it were plain rows is garbage.
    pub fn import(&self, source: &Imports<'_>) -> Result<Imported, Error> {
        let format = pixel_format(source.format).ok_or(Error::UnknownFormat)?;

        let layouts: Vec<vk::SubresourceLayout> = source
            .planes
            .iter()
            .map(|plane| {
                vk::SubresourceLayout::default()
                    .offset(u64::from(plane.offset))
                    .row_pitch(u64::from(plane.pitch))
            })
            .collect();

        let mut external = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let mut explicit = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
            .drm_format_modifier(source.modifier)
            .plane_layouts(&layouts);
        let create = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .extent(vk::Extent3D {
                width: source.width,
                height: source.height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            // Read by the conversion shader, and copied out by the diagnostic
            // that checks this import is producing real pixels.
            .usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_SRC)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .push_next(&mut external)
            .push_next(&mut explicit);

        // SAFETY: every borrowed structure outlives the call.
        let image = unsafe { self.device.create_image(&create, None) }.map_err(driver)?;

        match self.bind_imported(image, source.fd) {
            Ok(memory) => Ok(Imported {
                image,
                memory,
                width: source.width,
                height: source.height,
                format,
                foreign: true,
            }),
            Err(error) => {
                // SAFETY: the image was created above and nothing else refers
                // to it, because binding is what failed.
                unsafe { self.device.destroy_image(image, None) };
                Err(error)
            }
        }
    }

    /// Take the descriptor's memory and bind the image to it.
    fn bind_imported(&self, image: vk::Image, fd: RawFd) -> Result<vk::DeviceMemory, Error> {
        let external = ash::khr::external_memory_fd::Device::new(&self.instance, &self.device);
        // What the driver will accept for this descriptor, intersected with
        // what the image needs. Choosing from either alone picks a type the
        // other rejects.
        let mut properties = vk::MemoryFdPropertiesKHR::default();
        // SAFETY: the descriptor is live and owned by the caller until this
        // call succeeds.
        unsafe {
            external.get_memory_fd_properties(
                vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT,
                fd,
                &mut properties,
            )
        }
        .map_err(driver)?;

        // SAFETY: the image was created on this device.
        let requirements = unsafe { self.device.get_image_memory_requirements(image) };
        let allowed = requirements.memory_type_bits & properties.memory_type_bits;
        let index = (0..32)
            .find(|bit| allowed & (1 << bit) != 0)
            .ok_or(Error::NoMemoryType)?;

        // The image is the only thing in this allocation, which is what an
        // imported buffer is by construction.
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
        let mut import = vk::ImportMemoryFdInfoKHR::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
            .fd(fd);
        let allocate = vk::MemoryAllocateInfo::default()
            .allocation_size(requirements.size)
            .memory_type_index(index)
            .push_next(&mut dedicated)
            .push_next(&mut import);

        // SAFETY: the chain outlives the call. On success the driver owns the
        // descriptor; on failure it does not, and the caller still holds it.
        let memory = unsafe { self.device.allocate_memory(&allocate, None) }.map_err(driver)?;
        // SAFETY: both handles are this device's and neither is bound yet.
        match unsafe { self.device.bind_image_memory(image, memory, 0) } {
            Ok(()) => Ok(memory),
            Err(result) => {
                // SAFETY: nothing is bound to it.
                unsafe { self.device.free_memory(memory, None) };
                Err(driver(result))
            }
        }
    }

    /// Release an imported image.
    ///
    /// Not a `Drop` on [`Imported`] itself, because freeing needs the device
    /// and an image that outlived its device would be worse than an explicit
    /// call.
    pub fn release(&self, imported: Imported) {
        // SAFETY: both handles came from this device, and the wait means no
        // submitted work still refers to them.
        unsafe {
            let _ = self.device.device_wait_idle();
            self.device.destroy_image(imported.image, None);
            self.device.free_memory(imported.memory, None);
        }
    }
}

impl Device {
    /// Copy an imported image out into ordinary memory.
    ///
    /// **A diagnostic, and the only place in this crate that moves pixels to
    /// the processor.** The conversion never does this; it reads the image on
    /// the device and writes its result there. This exists because an import
    /// that is subtly wrong -- a tiling described incorrectly, a channel order
    /// reversed -- still succeeds at every call, and the only thing that says
    /// so is the picture.
    ///
    /// Returns tightly packed rows, four bytes per pixel, in the imported
    /// format's own channel order.
    pub fn read_back(&self, imported: &Imported) -> Result<Vec<u8>, Error> {
        let bytes = u64::from(imported.width) * u64::from(imported.height) * 4;

        let buffer_info = vk::BufferCreateInfo::default()
            .size(bytes)
            .usage(vk::BufferUsageFlags::TRANSFER_DST)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        // SAFETY: the create info outlives the call.
        let buffer = unsafe { self.device.create_buffer(&buffer_info, None) }.map_err(driver)?;

        let result = self.read_back_into(imported, buffer, bytes);
        // SAFETY: created just above; the wait inside the helper means nothing
        // submitted still refers to it.
        unsafe { self.device.destroy_buffer(buffer, None) };
        result
    }

    fn read_back_into(
        &self,
        imported: &Imported,
        buffer: vk::Buffer,
        bytes: u64,
    ) -> Result<Vec<u8>, Error> {
        // SAFETY: the buffer was created on this device.
        let requirements = unsafe { self.device.get_buffer_memory_requirements(buffer) };
        let index = self.host_visible_memory(requirements.memory_type_bits)?;
        let allocate = vk::MemoryAllocateInfo::default()
            .allocation_size(requirements.size)
            .memory_type_index(index);
        // SAFETY: the allocate info outlives the call.
        let memory = unsafe { self.device.allocate_memory(&allocate, None) }.map_err(driver)?;

        let outcome = (|| -> Result<Vec<u8>, Error> {
            // SAFETY: neither handle is bound yet and both are this device's.
            unsafe { self.device.bind_buffer_memory(buffer, memory, 0) }.map_err(driver)?;
            self.copy_image_to_buffer(imported, buffer)?;

            // SAFETY: the memory is host visible, nothing else maps it, and the
            // copy above completed before this call returns.
            let mapped = unsafe {
                self.device
                    .map_memory(memory, 0, bytes, vk::MemoryMapFlags::empty())
            }
            .map_err(driver)?;
            let length = usize::try_from(bytes).unwrap_or(0);
            // SAFETY: the driver returned a mapping of at least `bytes`, and it
            // stays valid until the unmap below.
            let pixels = unsafe { core::slice::from_raw_parts(mapped.cast::<u8>(), length) };
            let copied = pixels.to_vec();
            // SAFETY: mapped by the call above and not referenced after this.
            unsafe { self.device.unmap_memory(memory) };
            Ok(copied)
        })();

        // SAFETY: nothing submitted still refers to it; the copy waited.
        unsafe { self.device.free_memory(memory, None) };
        outcome
    }

    /// A memory type the processor can read.
    pub(crate) fn host_visible_memory(&self, allowed: u32) -> Result<u32, Error> {
        // SAFETY: the device came from this instance.
        let memory = unsafe {
            self.instance
                .get_physical_device_memory_properties(self.physical)
        };
        let wanted = vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
        memory
            .memory_types
            .iter()
            .take(usize::try_from(memory.memory_type_count).unwrap_or(0))
            .enumerate()
            .find(|(at, kind)| allowed & (1 << at) != 0 && kind.property_flags.contains(wanted))
            .and_then(|(at, _)| u32::try_from(at).ok())
            .ok_or(Error::NoMemoryType)
    }

    /// Record and run the copy, waiting for it to finish.
    fn copy_image_to_buffer(&self, imported: &Imported, buffer: vk::Buffer) -> Result<(), Error> {
        let pool_info = vk::CommandPoolCreateInfo::default().queue_family_index(self.queue_family);
        // SAFETY: the create info outlives the call.
        let pool = unsafe { self.device.create_command_pool(&pool_info, None) }.map_err(driver)?;

        let outcome = self.record_and_submit(imported, buffer, pool);

        // SAFETY: the submit waited, so no recorded work is still running.
        unsafe { self.device.destroy_command_pool(pool, None) };
        outcome
    }

    fn record_and_submit(
        &self,
        imported: &Imported,
        buffer: vk::Buffer,
        pool: vk::CommandPool,
    ) -> Result<(), Error> {
        let allocate = vk::CommandBufferAllocateInfo::default()
            .command_pool(pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1);
        // SAFETY: the allocate info outlives the call.
        let buffers = unsafe { self.device.allocate_command_buffers(&allocate) }.map_err(driver)?;
        let commands = *buffers.first().ok_or(Error::NoQueue)?;

        let begin = vk::CommandBufferBeginInfo::default()
            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
        // SAFETY: the command buffer was just allocated and is not recording.
        unsafe { self.device.begin_command_buffer(commands, &begin) }.map_err(driver)?;

        // **The image was last written by the display, which is not this
        // interface at all.** Naming that as the previous owner is what makes
        // the contents legible; claiming it was ours lets a driver assume a
        // layout it never had.
        let acquire = vk::ImageMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::empty())
            .dst_access_mask(vk::AccessFlags::TRANSFER_READ)
            .old_layout(vk::ImageLayout::UNDEFINED)
            .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT)
            .dst_queue_family_index(self.queue_family)
            .image(imported.image)
            .subresource_range(vk::ImageSubresourceRange {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                base_mip_level: 0,
                level_count: 1,
                base_array_layer: 0,
                layer_count: 1,
            });
        // SAFETY: recording into a command buffer that has begun, with a
        // barrier that outlives the call.
        unsafe {
            self.device.cmd_pipeline_barrier(
                commands,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[acquire],
            );
        }

        let region = vk::BufferImageCopy::default()
            .image_subresource(vk::ImageSubresourceLayers {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                mip_level: 0,
                base_array_layer: 0,
                layer_count: 1,
            })
            .image_extent(vk::Extent3D {
                width: imported.width,
                height: imported.height,
                depth: 1,
            });
        // SAFETY: as above; the buffer is large enough for the whole image.
        unsafe {
            self.device.cmd_copy_image_to_buffer(
                commands,
                imported.image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                buffer,
                &[region],
            );
            self.device.end_command_buffer(commands).map_err(driver)?;
        }

        // SAFETY: the create info outlives the call.
        let fence = unsafe {
            self.device
                .create_fence(&vk::FenceCreateInfo::default(), None)
        }
        .map_err(driver)?;
        let submits = [vk::SubmitInfo::default().command_buffers(&buffers)];
        // SAFETY: everything borrowed outlives the wait below, which is what
        // makes it safe to free them afterwards.
        let waited = unsafe {
            self.device
                .queue_submit(self.queue, &submits, fence)
                .map_err(driver)
                .and_then(|()| {
                    self.device
                        .wait_for_fences(&[fence], true, u64::MAX)
                        .map_err(driver)
                })
        };
        // SAFETY: signalled or never submitted; either way nothing waits on it.
        unsafe { self.device.destroy_fence(fence, None) };
        waited
    }
}

/// What to import, gathered from a captured framebuffer.
///
/// One descriptor and one or more planes within it. Several distinct
/// descriptors are not handled: no display seen here scans out that way, and
/// guessing at the arrangement is worse than saying so.
#[derive(Debug)]
pub struct Imports<'a> {
    pub width: u32,
    pub height: u32,
    pub format: DrmFourcc,
    pub modifier: u64,
    pub fd: RawFd,
    pub planes: &'a [PlaneLayout],
}

/// Where one plane sits inside the imported descriptor.
#[derive(Debug, Clone, Copy)]
pub struct PlaneLayout {
    pub offset: u32,
    pub pitch: u32,
}

impl Device {
    /// Build a frame from bytes, for a test with a known answer.
    ///
    /// **Not a capture path.** Nothing in the product moves pixels to the
    /// device this way; a captured frame is already there. This exists so the
    /// conversion can be fed an input whose correct output is known in advance.
    ///
    /// Takes four bytes a pixel, red first, tightly packed.
    pub fn upload_rgba(&self, width: u32, height: u32, pixels: &[u8]) -> Result<Imported, Error> {
        let create = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(vk::Format::R8G8B8A8_UNORM)
            .extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        // SAFETY: the create info outlives the call.
        let image = unsafe { self.device.create_image(&create, None) }.map_err(driver)?;

        // SAFETY: the image was created on this device.
        let requirements = unsafe { self.device.get_image_memory_requirements(image) };
        // The image lives on the device like any other. Only the staging
        // buffer below is memory the processor can see.
        let index = self.device_local_memory(requirements.memory_type_bits)?;
        let allocate = vk::MemoryAllocateInfo::default()
            .allocation_size(requirements.size)
            .memory_type_index(index);
        // SAFETY: the allocate info outlives the call.
        let memory = match unsafe { self.device.allocate_memory(&allocate, None) } {
            Ok(memory) => memory,
            Err(result) => {
                // SAFETY: nothing refers to it.
                unsafe { self.device.destroy_image(image, None) };
                return Err(driver(result));
            }
        };

        let built = (|| -> Result<(), Error> {
            // SAFETY: neither handle is bound yet.
            unsafe { self.device.bind_image_memory(image, memory, 0) }.map_err(driver)?;
            self.stage_into(image, width, height, pixels)
        })();

        match built {
            Ok(()) => Ok(Imported {
                image,
                memory,
                width,
                height,
                format: vk::Format::R8G8B8A8_UNORM,
                // Written here, so a barrier must not claim otherwise.
                foreign: false,
            }),
            Err(error) => {
                // SAFETY: nothing submitted refers to either handle.
                unsafe {
                    self.device.destroy_image(image, None);
                    self.device.free_memory(memory, None);
                }
                Err(error)
            }
        }
    }

    /// Copy bytes through a staging buffer into an image.
    fn stage_into(
        &self,
        image: vk::Image,
        width: u32,
        height: u32,
        pixels: &[u8],
    ) -> Result<(), Error> {
        let bytes = u64::try_from(pixels.len()).unwrap_or(0);
        let info = vk::BufferCreateInfo::default()
            .size(bytes.max(4))
            .usage(vk::BufferUsageFlags::TRANSFER_SRC)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        // SAFETY: the create info outlives the call.
        let staging = unsafe { self.device.create_buffer(&info, None) }.map_err(driver)?;
        // SAFETY: created on this device.
        let requirements = unsafe { self.device.get_buffer_memory_requirements(staging) };
        let index = self.host_visible_memory(requirements.memory_type_bits)?;
        let allocate = vk::MemoryAllocateInfo::default()
            .allocation_size(requirements.size)
            .memory_type_index(index);
        // SAFETY: the allocate info outlives the call.
        let memory = unsafe { self.device.allocate_memory(&allocate, None) }.map_err(driver)?;

        let done = (|| -> Result<(), Error> {
            // SAFETY: neither handle is bound yet.
            unsafe { self.device.bind_buffer_memory(staging, memory, 0) }.map_err(driver)?;
            // SAFETY: host visible and nothing else maps it.
            let mapped = unsafe {
                self.device
                    .map_memory(memory, 0, bytes, vk::MemoryMapFlags::empty())
            }
            .map_err(driver)?;
            // SAFETY: the mapping is at least `pixels.len()` bytes and the
            // source is a live slice that does not overlap it.
            unsafe {
                core::ptr::copy_nonoverlapping(pixels.as_ptr(), mapped.cast::<u8>(), pixels.len());
                self.device.unmap_memory(memory);
            }
            self.copy_buffer_to_image(staging, image, width, height)
        })();

        // SAFETY: the copy waited before returning.
        unsafe {
            self.device.destroy_buffer(staging, None);
            self.device.free_memory(memory, None);
        }
        done
    }

    fn copy_buffer_to_image(
        &self,
        buffer: vk::Buffer,
        image: vk::Image,
        width: u32,
        height: u32,
    ) -> Result<(), Error> {
        let pool_info = vk::CommandPoolCreateInfo::default().queue_family_index(self.queue_family);
        // SAFETY: the create info outlives the call.
        let pool = unsafe { self.device.create_command_pool(&pool_info, None) }.map_err(driver)?;

        let done = (|| -> Result<(), Error> {
            let allocate = vk::CommandBufferAllocateInfo::default()
                .command_pool(pool)
                .level(vk::CommandBufferLevel::PRIMARY)
                .command_buffer_count(1);
            // SAFETY: the allocate info outlives the call.
            let buffers =
                unsafe { self.device.allocate_command_buffers(&allocate) }.map_err(driver)?;
            let commands = *buffers.first().ok_or(Error::NoQueue)?;
            let begin = vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
            // SAFETY: freshly allocated and not recording.
            unsafe { self.device.begin_command_buffer(commands, &begin) }.map_err(driver)?;

            let whole = vk::ImageSubresourceRange {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                base_mip_level: 0,
                level_count: 1,
                base_array_layer: 0,
                layer_count: 1,
            };
            let to_dst = vk::ImageMemoryBarrier::default()
                .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(image)
                .subresource_range(whole);
            let region = vk::BufferImageCopy::default()
                .image_subresource(vk::ImageSubresourceLayers {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    mip_level: 0,
                    base_array_layer: 0,
                    layer_count: 1,
                })
                .image_extent(vk::Extent3D {
                    width,
                    height,
                    depth: 1,
                });

            // SAFETY: recording into a begun buffer; everything borrowed
            // outlives the call.
            unsafe {
                self.device.cmd_pipeline_barrier(
                    commands,
                    vk::PipelineStageFlags::TOP_OF_PIPE,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[to_dst],
                );
                self.device.cmd_copy_buffer_to_image(
                    commands,
                    buffer,
                    image,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &[region],
                );
                self.device.end_command_buffer(commands).map_err(driver)?;
            }

            // SAFETY: the create info outlives the call.
            let fence = unsafe {
                self.device
                    .create_fence(&vk::FenceCreateInfo::default(), None)
            }
            .map_err(driver)?;
            let submits = [vk::SubmitInfo::default().command_buffers(&buffers)];
            // SAFETY: borrowed structures outlive the wait below.
            let waited = unsafe {
                self.device
                    .queue_submit(self.queue, &submits, fence)
                    .map_err(driver)
                    .and_then(|()| {
                        self.device
                            .wait_for_fences(&[fence], true, u64::MAX)
                            .map_err(driver)
                    })
            };
            // SAFETY: signalled or never submitted.
            unsafe { self.device.destroy_fence(fence, None) };
            waited
        })();

        // SAFETY: the submit waited.
        unsafe { self.device.destroy_command_pool(pool, None) };
        done
    }
}

#[cfg(test)]
mod tests {
    use ash::vk;

    /// **The two names read as opposites and that is correct.** A captured
    /// buffer is named by the order of its channels within a little-endian
    /// word, most significant first; this interface names a byte-order format
    /// by the order of the bytes in memory. Those are reverses of each other,
    /// so a buffer called ABGR is imported as an RGBA format and one called
    /// ARGB as BGRA. Anyone "fixing" the apparent mismatch exchanges red and
    /// blue in every frame, which on a desktop looks plausible until something
    /// in the picture is a known colour.
    ///
    /// The packed layout is the exception and does not reverse: it is one word
    /// rather than four bytes, so both names describe the same bit positions.
    #[test]
    fn channel_order_is_by_byte_not_by_name() {
        use drm::buffer::DrmFourcc;

        assert_eq!(
            super::pixel_format(DrmFourcc::Abgr8888),
            Some(vk::Format::R8G8B8A8_UNORM)
        );
        assert_eq!(
            super::pixel_format(DrmFourcc::Argb8888),
            Some(vk::Format::B8G8R8A8_UNORM)
        );
        assert_eq!(
            super::pixel_format(DrmFourcc::Abgr2101010),
            Some(vk::Format::A2B10G10R10_UNORM_PACK32)
        );
        // Something nothing here scans out has no import rather than a guess.
        assert_eq!(super::pixel_format(DrmFourcc::Yuyv), None);
    }

    /// The node numbers have to come out of the encoding the kernel uses, which
    /// is not a plain pair of bytes. The values here are the display and render
    /// nodes on a machine with two cards: 226:0, 226:1, 226:128, 226:129.
    #[test]
    fn node_numbers_decompose() {
        // Built the way the kernel builds them, so the test does not merely
        // restate the implementation.
        fn makedev(major: u64, minor: u64) -> u64 {
            ((major & 0xfff) << 8)
                | ((major & !0xfff) << 32)
                | (minor & 0xff)
                | ((minor & !0xff) << 12)
        }
        for (major, minor) in [(226, 0), (226, 1), (226, 128), (226, 129), (4095, 1048575)] {
            let rdev = makedev(major, minor);
            let got = (
                u32::try_from(((rdev >> 8) & 0xfff) | ((rdev >> 32) & !0xfff)).unwrap(),
                u32::try_from((rdev & 0xff) | ((rdev >> 12) & !0xff)).unwrap(),
            );
            assert_eq!(
                got,
                (u32::try_from(major).unwrap(), u32::try_from(minor).unwrap())
            );
        }
    }
}
