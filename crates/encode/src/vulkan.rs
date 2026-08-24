//! The encoder that runs on the same interface as the capture and the
//! conversion.
//!
//! **Its reason is not speed, it is the boundary.** The other two backends
//! reach a device through an interface that is not the one the frame was
//! converted on, and switching between the two costs more per frame than the
//! conversion itself. This one has no boundary to cross: the picture a shader
//! finishes writing is the picture it encodes.
//!
//! **Selected by name, never inferred.** It is newer and less proven than
//! either of the others, it covers less hardware, and on one vendor the
//! conversion still cannot write its picture directly. Choosing it on a
//! machine's behalf would replace two backends that work with one that might.
//!
//! What it takes is a `VkImage`, not an address and a row length, which is why
//! the arrangement the other two need -- two images bound into one allocation
//! at an offset an encoder was told about -- has no equivalent here.

use core::ffi::CStr;
use std::path::Path;

use ash::vk;

use crate::Poll;

/// What went wrong.
///
/// Driver results are carried as their raw code rather than a formatted
/// message, so the type stays `Copy` and allocation free.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// The loader is absent or refused to load.
    NoLoader,
    /// A call into the driver failed.
    Driver(i32),
    /// No device on this machine drives the display node that was asked for.
    NoDeviceForNode,
    /// The device cannot encode over this interface. Named individually,
    /// because a missing piece is a deployment fact rather than a bug.
    Unsupported(&'static str),
    /// The device exposes no queue that can encode.
    NoQueue,
    /// No memory type satisfies what the session or a picture needs.
    NoMemoryType,
    /// The device refused the parameter sets it was given.
    BadParameters,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NoLoader => f.write_str("no usable driver loader on this system"),
            Self::Driver(code) => write!(f, "driver call failed, result {code}"),
            Self::NoDeviceForNode => f.write_str("no device reports driving that display node"),
            Self::Unsupported(what) => write!(f, "the device does not support {what}"),
            Self::NoQueue => f.write_str("the device exposes no queue that can encode"),
            Self::NoMemoryType => f.write_str("no memory type suits what this needs"),
            Self::BadParameters => f.write_str("the device refused the parameter sets"),
        }
    }
}

impl std::error::Error for Error {}

type Result<T> = core::result::Result<T, Error>;

fn driver(result: vk::Result) -> Error {
    Error::Driver(result.as_raw())
}

fn checked(result: vk::Result) -> Result<()> {
    if result == vk::Result::SUCCESS {
        Ok(())
    } else {
        Err(driver(result))
    }
}

/// Everything this needs from a device, checked by name so a machine that
/// cannot do it says which part is missing.
const REQUIRED: [&CStr; 3] = [
    ash::khr::video_queue::NAME,
    ash::khr::video_encode_queue::NAME,
    ash::khr::video_encode_h264::NAME,
];

/// The codecs this backend produces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    H264,
}

/// What a device says it will do.
#[derive(Debug, Clone, Copy)]
pub struct Caps {
    /// Rate control modes offered. Anything but a mode taking a bitrate means
    /// the only congestion actuator the design has cannot be built here.
    pub rate_control: vk::VideoEncodeRateControlModeFlagsKHR,
    /// The largest picture a session may be built for.
    pub max_extent: vk::Extent2D,
    /// How many reference slots a session may hold.
    pub max_dpb_slots: u32,
    /// How many of those may be active at once.
    pub max_active_references: u32,
    /// The codec header revision a session must be built against.
    pub std_header: vk::ExtensionProperties,
    /// The layout a picture the encoder reads must be in.
    pub picture: vk::Format,
    /// The layout a reference picture must be in.
    pub reference: vk::Format,
    /// **Whether a shader may write the very picture the encoder reads.**
    /// False means a copy stands between conversion and encode on this device.
    pub shared_picture: bool,
    /// Bytes a bitstream buffer's offset and size must be a multiple of.
    pub bitstream_alignment: u64,
}

/// A device node's major and minor numbers.
fn node_numbers(path: &Path) -> Option<(u32, u32)> {
    use std::os::unix::fs::MetadataExt;
    let rdev = std::fs::metadata(path).ok()?.rdev();
    let major = u32::try_from(((rdev >> 8) & 0xfff) | ((rdev >> 32) & !0xfff)).ok()?;
    let minor = u32::try_from((rdev & 0xff) | ((rdev >> 12) & !0xff)).ok()?;
    Some((major, minor))
}

/// The picture this backend encodes, as one chain.
///
/// **Built inside a call rather than returned.** Each structure borrows the one
/// it is pushed onto for as long as that one lives, so the chain cannot outlive
/// the frame that made it and is handed to a closure instead.
fn with_profile<R>(f: impl FnOnce(&vk::VideoProfileInfoKHR<'_>) -> R) -> R {
    let mut h264 = vk::VideoEncodeH264ProfileInfoKHR::default()
        .std_profile_idc(ash::vk::native::StdVideoH264ProfileIdc_STD_VIDEO_H264_PROFILE_IDC_HIGH);
    let profile = vk::VideoProfileInfoKHR::default()
        .video_codec_operation(vk::VideoCodecOperationFlagsKHR::ENCODE_H264)
        .chroma_subsampling(vk::VideoChromaSubsamplingFlagsKHR::TYPE_420)
        .luma_bit_depth(vk::VideoComponentBitDepthFlagsKHR::TYPE_8)
        .chroma_bit_depth(vk::VideoComponentBitDepthFlagsKHR::TYPE_8)
        .push_next(&mut h264);
    f(&profile)
}

/// The loaded interface. Held for as long as anything built on it.
pub struct Vulkan {
    _entry: ash::Entry,
    instance: ash::Instance,
    /// The physical-device queries, which live on the instance rather than on
    /// the device: they are asked before any device exists.
    video: ash::khr::video_queue::Instance,
}

impl core::fmt::Debug for Vulkan {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("Vulkan(loaded)")
    }
}

impl Vulkan {
    /// Load the driver, or say it is not here.
    pub fn load() -> Result<Self> {
        // SAFETY: loads the system driver loader; the handle is kept for the
        // lifetime of everything derived from it.
        let entry = unsafe { ash::Entry::load() }.map_err(|_| Error::NoLoader)?;
        let application = vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_3);
        let create = vk::InstanceCreateInfo::default().application_info(&application);
        // SAFETY: the create info outlives the call and names no extensions.
        let instance = unsafe { entry.create_instance(&create, None) }.map_err(driver)?;
        let video = ash::khr::video_queue::Instance::new(&entry, &instance);
        Ok(Self {
            _entry: entry,
            instance,
            video,
        })
    }

    /// The device that drives a display node, ready to encode.
    ///
    /// **Matched on the node numbers the driver reports**, which is exact.
    /// Matching on a name or an index breaks the moment a machine has two cards
    /// from one vendor, or reorders them across a reboot.
    pub fn open(&self, node: &Path) -> Result<Device<'_>> {
        let wanted = node_numbers(node).ok_or(Error::NoDeviceForNode)?;
        // SAFETY: enumerating from a live instance.
        let candidates = unsafe { self.instance.enumerate_physical_devices() }.map_err(driver)?;
        let mut answered = false;
        for physical in candidates {
            // SAFETY: the device came from this instance.
            let available = unsafe {
                self.instance
                    .enumerate_device_extension_properties(physical)
            }
            .map_err(driver)?;
            let has = |name: &CStr| {
                available
                    .iter()
                    .any(|entry| entry.extension_name_as_c_str() == Ok(name))
            };
            if !has(ash::ext::physical_device_drm::NAME) {
                continue;
            }
            answered = true;
            let mut drm = vk::PhysicalDeviceDrmPropertiesEXT::default();
            let mut properties = vk::PhysicalDeviceProperties2::default().push_next(&mut drm);
            // SAFETY: the chain is stack values that outlive the call.
            unsafe {
                self.instance
                    .get_physical_device_properties2(physical, &mut properties);
            }
            let matched = drm.has_primary != 0
                && u32::try_from(drm.primary_major) == Ok(wanted.0)
                && u32::try_from(drm.primary_minor) == Ok(wanted.1);
            if !matched {
                continue;
            }
            for name in REQUIRED {
                if !has(name) {
                    return Err(Error::Unsupported(
                        name.to_str().unwrap_or("a required interface"),
                    ));
                }
            }
            return Device::open(&self.instance, &self.video, physical);
        }
        if answered {
            Err(Error::NoDeviceForNode)
        } else {
            Err(Error::Unsupported("VK_EXT_physical_device_drm"))
        }
    }
}

impl Drop for Vulkan {
    fn drop(&mut self) {
        // SAFETY: nothing built on it outlives this type.
        unsafe { self.instance.destroy_instance(None) };
    }
}

/// One device, opened on its encode queue.
pub struct Device<'a> {
    instance: &'a ash::Instance,
    /// The physical-device queries, borrowed from what loaded the interface.
    queries: &'a ash::khr::video_queue::Instance,
    physical: vk::PhysicalDevice,
    device: ash::Device,
    video: ash::khr::video_queue::Device,
    encode: ash::khr::video_encode_queue::Device,
    queue: vk::Queue,
    /// The family that encodes.
    pub family: u32,
    /// A family that can write a picture, which the encode family may not be.
    pub writer_family: u32,
    name: String,
}

impl core::fmt::Debug for Device<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Device")
            .field("name", &self.name)
            .field("family", &self.family)
            .finish_non_exhaustive()
    }
}

impl<'a> Device<'a> {
    fn open(
        instance: &'a ash::Instance,
        queries: &'a ash::khr::video_queue::Instance,
        physical: vk::PhysicalDevice,
    ) -> Result<Self> {
        // SAFETY: the device came from this instance.
        let families = unsafe { instance.get_physical_device_queue_family_properties(physical) };
        let family = families
            .iter()
            .position(|family| {
                family
                    .queue_flags
                    .contains(vk::QueueFlags::VIDEO_ENCODE_KHR)
            })
            .and_then(|at| u32::try_from(at).ok())
            .ok_or(Error::NoQueue)?;
        // **A second family, because one device is not one queue.** An encode
        // family is not required to be able to write a picture, and on one
        // vendor here it cannot; whoever fills the picture needs a queue that
        // can.
        let writer_family = families
            .iter()
            .position(|family| {
                family
                    .queue_flags
                    .intersects(vk::QueueFlags::COMPUTE | vk::QueueFlags::GRAPHICS)
            })
            .and_then(|at| u32::try_from(at).ok())
            .ok_or(Error::NoQueue)?;

        let priorities = [1.0_f32];
        let mut queues = vec![
            vk::DeviceQueueCreateInfo::default()
                .queue_family_index(family)
                .queue_priorities(&priorities),
        ];
        if writer_family != family {
            queues.push(
                vk::DeviceQueueCreateInfo::default()
                    .queue_family_index(writer_family)
                    .queue_priorities(&priorities),
            );
        }
        let names: Vec<*const core::ffi::c_char> =
            REQUIRED.iter().map(|name| name.as_ptr()).collect();
        // The video interface is specified against the newer synchronisation,
        // so it is turned on even though nothing here names it directly.
        let mut sync = vk::PhysicalDeviceVulkan13Features::default().synchronization2(true);
        // The single-byte and two-byte storage formats a conversion writes
        // through are not in the set every device must support unasked.
        let mut features = vk::PhysicalDeviceFeatures2::default();
        features.features.shader_storage_image_extended_formats = vk::TRUE;
        let mut ycbcr = vk::PhysicalDeviceSamplerYcbcrConversionFeatures::default()
            .sampler_ycbcr_conversion(true);
        let create = vk::DeviceCreateInfo::default()
            .queue_create_infos(&queues)
            .enabled_extension_names(&names)
            .push_next(&mut sync)
            .push_next(&mut features)
            .push_next(&mut ycbcr);
        // SAFETY: every borrowed slice outlives the call.
        let device = unsafe { instance.create_device(physical, &create, None) }.map_err(driver)?;
        // SAFETY: the family came from this device's own enumeration.
        let queue = unsafe { device.get_device_queue(family, 0) };
        let video = ash::khr::video_queue::Device::new(instance, &device);
        let encode = ash::khr::video_encode_queue::Device::new(instance, &device);

        let mut properties = vk::PhysicalDeviceProperties2::default();
        // SAFETY: the device came from this instance.
        unsafe { instance.get_physical_device_properties2(physical, &mut properties) };
        let name = properties
            .properties
            .device_name_as_c_str()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "an unnamed device".to_string());

        Ok(Self {
            instance,
            queries,
            physical,
            device,
            video,
            encode,
            queue,
            family,
            writer_family,
            name,
        })
    }

    /// What the device calls itself, for a startup log line.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// What this device will do for a codec.
    pub fn caps(&self, codec: Codec) -> Result<Caps> {
        let Codec::H264 = codec;
        let mut encode = vk::VideoEncodeCapabilitiesKHR::default();
        // **The codec's own capabilities have to be in the chain.** Asking for
        // an H.264 encode without them is invalid, and a driver answers anyway
        // with a structure it never filled.
        let mut h264 = vk::VideoEncodeH264CapabilitiesKHR::default();
        let mut caps = vk::VideoCapabilitiesKHR::default()
            .push_next(&mut encode)
            .push_next(&mut h264);
        with_profile(|profile| {
            // SAFETY: the chain outlives the call and the device came from
            // this instance.
            checked(unsafe {
                (self.queries.fp().get_physical_device_video_capabilities_khr)(
                    self.physical,
                    profile,
                    &mut caps,
                )
            })
        })?;
        let max_extent = caps.max_coded_extent;
        let max_dpb_slots = caps.max_dpb_slots;
        let max_active_references = caps.max_active_reference_pictures;
        let std_header = caps.std_header_version;
        let bitstream_alignment = caps.min_bitstream_buffer_offset_alignment;
        let rate_control = encode.rate_control_modes;

        let picture = self
            .formats(vk::ImageUsageFlags::VIDEO_ENCODE_SRC_KHR | vk::ImageUsageFlags::TRANSFER_DST)?
            .first()
            .copied()
            .ok_or(Error::Unsupported("a layout for a picture it would encode"))?;
        let reference = self
            .formats(vk::ImageUsageFlags::VIDEO_ENCODE_DPB_KHR)?
            .first()
            .copied()
            .ok_or(Error::Unsupported("a layout for a reference picture"))?;
        // **Asked, never assumed.** One vendor here will let a shader write the
        // very picture the encoder reads and the other will not, and building
        // on the wrong answer is building a copy that cannot be removed or one
        // that is missing.
        let shared_picture = self
            .formats(vk::ImageUsageFlags::VIDEO_ENCODE_SRC_KHR | vk::ImageUsageFlags::STORAGE)
            .is_ok_and(|formats| formats.contains(&picture));

        Ok(Caps {
            rate_control,
            max_extent,
            max_dpb_slots,
            max_active_references,
            std_header,
            picture,
            reference,
            shared_picture,
            bitstream_alignment,
        })
    }

    /// Which layouts the device takes for one use of a video picture.
    fn formats(&self, usage: vk::ImageUsageFlags) -> Result<Vec<vk::Format>> {
        with_profile(|profile| {
            let profiles = [*profile];
            let mut list = vk::VideoProfileListInfoKHR::default().profiles(&profiles);
            let info = vk::PhysicalDeviceVideoFormatInfoKHR::default()
                .image_usage(usage)
                .push_next(&mut list);
            let mut count = 0_u32;
            // SAFETY: asking for the count writes only the counter.
            checked(unsafe {
                (self
                    .queries
                    .fp()
                    .get_physical_device_video_format_properties_khr)(
                    self.physical,
                    &info,
                    &raw mut count,
                    core::ptr::null_mut(),
                )
            })?;
            let mut properties =
                vec![vk::VideoFormatPropertiesKHR::default(); usize::try_from(count).unwrap_or(0)];
            // SAFETY: the destination holds `count` entries, as just reported.
            checked(unsafe {
                (self
                    .queries
                    .fp()
                    .get_physical_device_video_format_properties_khr)(
                    self.physical,
                    &info,
                    &raw mut count,
                    properties.as_mut_ptr(),
                )
            })?;
            Ok(properties.into_iter().map(|entry| entry.format).collect())
        })
    }

    /// The first memory type that suits a requirement.
    ///
    /// **Device-local is preferred and not required.** Demanding it makes one
    /// vendor refuse every allocation a session asks for.
    fn memory_type(&self, wanted: vk::MemoryRequirements, host: bool) -> Result<u32> {
        // SAFETY: the device came from this instance.
        let memory = unsafe {
            self.instance
                .get_physical_device_memory_properties(self.physical)
        };
        let needed = if host {
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT
        } else {
            vk::MemoryPropertyFlags::DEVICE_LOCAL
        };
        let usable = memory
            .memory_types
            .iter()
            .take(usize::try_from(memory.memory_type_count).unwrap_or(0))
            .enumerate()
            .filter(|(at, _)| wanted.memory_type_bits & (1 << at) != 0);
        let mut fallback = None;
        for (at, kind) in usable {
            if kind.property_flags.contains(needed) {
                return u32::try_from(at).map_err(|_| Error::NoMemoryType);
            }
            if !host {
                fallback = fallback.or(Some(at));
            }
        }
        fallback
            .and_then(|at| u32::try_from(at).ok())
            .ok_or(Error::NoMemoryType)
    }
}

impl Drop for Device<'_> {
    fn drop(&mut self) {
        // SAFETY: everything built on it is released before this, and the wait
        // is what makes that true for work still running.
        unsafe {
            let _ = self.device.device_wait_idle();
            self.device.destroy_device(None);
        }
    }
}

/// How many pictures the session keeps.
const DPB_SLOTS: u32 = 2;

/// How much bitstream one picture may produce.
const BITSTREAM_BYTES: u64 = 4 << 20;

/// One picture, and the views onto it.
struct Picture {
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
    /// Present only when a shader may write this picture directly.
    planes: Option<[vk::ImageView; 2]>,
}

/// A configured encoder on one device.
///
/// **It owns the picture it encodes.** A conversion writes into it through
/// [`Encoder::planes`] where the device allows that, which is what makes the
/// path zero copy; where it does not, a caller copies into it instead.
pub struct Encoder<'a> {
    device: &'a Device<'a>,
    session: vk::VideoSessionKHR,
    session_memory: Vec<vk::DeviceMemory>,
    parameters: vk::VideoSessionParametersKHR,
    source: Picture,
    dpb: Picture,
    bitstream: vk::Buffer,
    bitstream_memory: vk::DeviceMemory,
    pool: vk::QueryPool,
    commands: vk::CommandPool,
    buffer: vk::CommandBuffer,
    fence: vk::Fence,
    extent: vk::Extent2D,
    bitrate_bps: u32,
    /// **What the session is actually in, which is not what was asked for.**
    /// An opening has to describe the configuration the session already has,
    /// and the control command in the same recording is what moves it -- so on
    /// the pass that changes a rate the two differ, and using one figure for
    /// both describes a session that does not exist.
    applied_bps: u32,
    /// Whether anything has been submitted since the session was configured.
    started: bool,
    /// **What the session is in, not what this pass wants.** An opening has to
    /// describe the mode the session already has; the control command in the
    /// same recording is what moves it. Keeping one flag for both is how a
    /// recording comes to ask for a bitrate and a fixed quantiser at once.
    mode: vk::VideoEncodeRateControlModeFlagsKHR,
    /// Where a finished picture is copied to, so the slice a caller reads is
    /// not the driver's own mapping.
    collected: Vec<u8>,
}

impl core::fmt::Debug for Encoder<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Encoder")
            .field("extent", &self.extent)
            .field("bitrate_bps", &self.bitrate_bps)
            .finish_non_exhaustive()
    }
}

impl<'a> Device<'a> {
    /// Build a session and everything one picture needs.
    pub fn encoder(
        &'a self,
        caps: &Caps,
        width: u32,
        height: u32,
        bitrate_bps: u32,
    ) -> Result<Encoder<'a>> {
        let extent = vk::Extent2D {
            width: width.next_multiple_of(2),
            height: height.next_multiple_of(2),
        };
        let session = self.session(caps, extent)?;
        let built = self
            .finish_encoder(caps, extent, bitrate_bps, session)
            .inspect_err(|_| {
                // SAFETY: created above and nothing was bound to it yet.
                unsafe {
                    (self.video.fp().destroy_video_session_khr)(
                        self.device.handle(),
                        session,
                        core::ptr::null(),
                    );
                }
            })?;
        Ok(built)
    }

    fn session(&self, caps: &Caps, extent: vk::Extent2D) -> Result<vk::VideoSessionKHR> {
        with_profile(|profile| {
            let create = vk::VideoSessionCreateInfoKHR::default()
                .queue_family_index(self.family)
                .video_profile(profile)
                .picture_format(caps.picture)
                .max_coded_extent(extent)
                .reference_picture_format(caps.reference)
                .max_dpb_slots(caps.max_dpb_slots.min(DPB_SLOTS))
                .max_active_reference_pictures(caps.max_active_references.min(1))
                .std_header_version(&caps.std_header);
            let mut session = vk::VideoSessionKHR::null();
            // SAFETY: the chain outlives the call and the out handle is live.
            checked(unsafe {
                (self.video.fp().create_video_session_khr)(
                    self.device.handle(),
                    &create,
                    core::ptr::null(),
                    &raw mut session,
                )
            })?;
            Ok(session)
        })
    }

    /// Bind the session's memory, in as many pieces as it asks for.
    fn bind_session(&self, session: vk::VideoSessionKHR) -> Result<Vec<vk::DeviceMemory>> {
        let mut count = 0_u32;
        // SAFETY: asking for the count writes only the counter.
        checked(unsafe {
            (self.video.fp().get_video_session_memory_requirements_khr)(
                self.device.handle(),
                session,
                &raw mut count,
                core::ptr::null_mut(),
            )
        })?;
        let mut wanted = vec![
            vk::VideoSessionMemoryRequirementsKHR::default();
            usize::try_from(count).unwrap_or(0)
        ];
        // SAFETY: the destination holds `count` entries, as just reported.
        checked(unsafe {
            (self.video.fp().get_video_session_memory_requirements_khr)(
                self.device.handle(),
                session,
                &raw mut count,
                wanted.as_mut_ptr(),
            )
        })?;

        let mut memories = Vec::with_capacity(wanted.len());
        let mut binds = Vec::with_capacity(wanted.len());
        for entry in &wanted {
            let index = self.memory_type(entry.memory_requirements, false)?;
            let allocate = vk::MemoryAllocateInfo::default()
                .allocation_size(entry.memory_requirements.size)
                .memory_type_index(index);
            // SAFETY: the allocate info outlives the call.
            let memory = unsafe { self.device.allocate_memory(&allocate, None) }.map_err(driver)?;
            memories.push(memory);
            binds.push(
                vk::BindVideoSessionMemoryInfoKHR::default()
                    .memory_bind_index(entry.memory_bind_index)
                    .memory(memory)
                    .memory_offset(0)
                    .memory_size(entry.memory_requirements.size),
            );
        }
        // SAFETY: every handle is this device's and the slice outlives the call.
        let result = checked(unsafe {
            (self.video.fp().bind_video_session_memory_khr)(
                self.device.handle(),
                session,
                u32::try_from(binds.len()).unwrap_or(0),
                binds.as_ptr(),
            )
        });
        if let Err(error) = result {
            for memory in memories {
                // SAFETY: nothing is bound to it.
                unsafe { self.device.free_memory(memory, None) };
            }
            return Err(error);
        }
        Ok(memories)
    }
}

impl<'a> Device<'a> {
    /// Everything after the session exists, so a failure has one place to undo.
    fn finish_encoder(
        &'a self,
        caps: &Caps,
        extent: vk::Extent2D,
        bitrate_bps: u32,
        session: vk::VideoSessionKHR,
    ) -> Result<Encoder<'a>> {
        let session_memory = self.bind_session(session)?;
        let parameters = self.parameters(session, extent)?;

        // **The picture the encoder reads, and where it comes from.** Where a
        // shader may write it, the conversion writes here and nothing moves;
        // where it may not, a caller copies into it.
        let source = self.picture(
            caps.picture,
            extent,
            if caps.shared_picture {
                vk::ImageUsageFlags::VIDEO_ENCODE_SRC_KHR | vk::ImageUsageFlags::STORAGE
            } else {
                vk::ImageUsageFlags::VIDEO_ENCODE_SRC_KHR | vk::ImageUsageFlags::TRANSFER_DST
            },
            caps.shared_picture,
        )?;
        let dpb = self.picture(
            caps.reference,
            extent,
            vk::ImageUsageFlags::VIDEO_ENCODE_DPB_KHR,
            false,
        )?;

        let (bitstream, bitstream_memory) = self.bitstream()?;
        let pool = self.feedback_pool()?;
        let create = vk::CommandPoolCreateInfo::default()
            .queue_family_index(self.family)
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
        // SAFETY: the create info outlives the call.
        let commands = unsafe { self.device.create_command_pool(&create, None) }.map_err(driver)?;
        let allocate = vk::CommandBufferAllocateInfo::default()
            .command_pool(commands)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1);
        // SAFETY: the allocate info outlives the call.
        let buffers = unsafe { self.device.allocate_command_buffers(&allocate) }.map_err(driver)?;
        let buffer = buffers.first().copied().ok_or(Error::NoQueue)?;
        let info = vk::FenceCreateInfo::default();
        // SAFETY: the create info outlives the call.
        let fence = unsafe { self.device.create_fence(&info, None) }.map_err(driver)?;

        Ok(Encoder {
            device: self,
            session,
            session_memory,
            parameters,
            source,
            dpb,
            bitstream,
            bitstream_memory,
            pool,
            commands,
            buffer,
            fence,
            extent,
            bitrate_bps,
            applied_bps: bitrate_bps,
            started: false,
            mode: vk::VideoEncodeRateControlModeFlagsKHR::DEFAULT,
            collected: Vec::with_capacity(1 << 16),
        })
    }

    /// The parameter sets, written here because no driver emits them.
    fn parameters(
        &self,
        session: vk::VideoSessionKHR,
        extent: vk::Extent2D,
    ) -> Result<vk::VideoSessionParametersKHR> {
        // SAFETY: plain data from the codec headers with no invariant beyond
        // its layout; all-zero is a valid set with no optional tables, and
        // every field that matters is written below.
        let mut sps: ash::vk::native::StdVideoH264SequenceParameterSet =
            unsafe { core::mem::zeroed() };
        sps.profile_idc = ash::vk::native::StdVideoH264ProfileIdc_STD_VIDEO_H264_PROFILE_IDC_HIGH;
        sps.level_idc = ash::vk::native::StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_5_2;
        sps.chroma_format_idc =
            ash::vk::native::StdVideoH264ChromaFormatIdc_STD_VIDEO_H264_CHROMA_FORMAT_IDC_420;
        sps.log2_max_frame_num_minus4 = 4;
        sps.pic_order_cnt_type = ash::vk::native::StdVideoH264PocType_STD_VIDEO_H264_POC_TYPE_2;
        sps.log2_max_pic_order_cnt_lsb_minus4 = 4;
        sps.max_num_ref_frames = 1;
        // In macroblocks, which is why the size rounds up by sixteen and the
        // remainder comes back as a crop rather than as a smaller picture.
        sps.pic_width_in_mbs_minus1 = extent.width.div_ceil(16) - 1;
        sps.pic_height_in_map_units_minus1 = extent.height.div_ceil(16) - 1;
        sps.frame_crop_right_offset = (extent.width.next_multiple_of(16) - extent.width) / 2;
        sps.frame_crop_bottom_offset = (extent.height.next_multiple_of(16) - extent.height) / 2;
        sps.flags.set_frame_mbs_only_flag(1);
        sps.flags.set_direct_8x8_inference_flag(1);
        if sps.frame_crop_right_offset > 0 || sps.frame_crop_bottom_offset > 0 {
            sps.flags.set_frame_cropping_flag(1);
        }
        // SAFETY: as above.
        let mut pps: ash::vk::native::StdVideoH264PictureParameterSet =
            unsafe { core::mem::zeroed() };
        pps.flags.set_entropy_coding_mode_flag(1);
        pps.flags.set_deblocking_filter_control_present_flag(1);

        let sps_list = [sps];
        let pps_list = [pps];
        let add = vk::VideoEncodeH264SessionParametersAddInfoKHR::default()
            .std_sp_ss(&sps_list)
            .std_pp_ss(&pps_list);
        let mut h264 = vk::VideoEncodeH264SessionParametersCreateInfoKHR::default()
            .max_std_sps_count(1)
            .max_std_pps_count(1)
            .parameters_add_info(&add);
        let create = vk::VideoSessionParametersCreateInfoKHR::default()
            .video_session(session)
            .push_next(&mut h264);
        let mut handle = vk::VideoSessionParametersKHR::null();
        // SAFETY: the chain outlives the call and the out handle is live.
        let result = unsafe {
            (self.video.fp().create_video_session_parameters_khr)(
                self.device.handle(),
                &create,
                core::ptr::null(),
                &raw mut handle,
            )
        };
        if result == vk::Result::SUCCESS {
            Ok(handle)
        } else {
            Err(Error::BadParameters)
        }
    }

    /// One video picture with its memory and views.
    fn picture(
        &self,
        format: vk::Format,
        extent: vk::Extent2D,
        usage: vk::ImageUsageFlags,
        planes: bool,
    ) -> Result<Picture> {
        let image = with_profile(|profile| {
            let profiles = [*profile];
            let mut list = vk::VideoProfileListInfoKHR::default().profiles(&profiles);
            let mut create = vk::ImageCreateInfo::default()
                .image_type(vk::ImageType::TYPE_2D)
                .format(format)
                .extent(vk::Extent3D {
                    width: extent.width,
                    height: extent.height,
                    depth: 1,
                })
                .mip_levels(1)
                .array_layers(1)
                .samples(vk::SampleCountFlags::TYPE_1)
                .tiling(vk::ImageTiling::OPTIMAL)
                .usage(usage)
                .sharing_mode(vk::SharingMode::EXCLUSIVE)
                .initial_layout(vk::ImageLayout::UNDEFINED)
                .push_next(&mut list);
            if planes {
                // Mutable format is what lets a plane be seen as a
                // single-component picture; extended usage is what makes the
                // shader's use apply to that view rather than to the two-plane
                // format, which supports it nowhere.
                create = create.flags(
                    vk::ImageCreateFlags::MUTABLE_FORMAT | vk::ImageCreateFlags::EXTENDED_USAGE_KHR,
                );
            }
            // SAFETY: the chain outlives the call.
            unsafe { self.device.create_image(&create, None) }
        })
        .map_err(driver)?;

        // SAFETY: created on this device just above.
        let wanted = unsafe { self.device.get_image_memory_requirements(image) };
        let index = self.memory_type(wanted, false)?;
        let allocate = vk::MemoryAllocateInfo::default()
            .allocation_size(wanted.size)
            .memory_type_index(index);
        // SAFETY: the allocate info outlives the call.
        let memory = unsafe { self.device.allocate_memory(&allocate, None) }.map_err(driver)?;
        // SAFETY: both handles are this device's and nothing is bound yet.
        unsafe { self.device.bind_image_memory(image, memory, 0) }.map_err(driver)?;

        // **The whole-image view names its own use.** A view with none
        // inherits the image's, and the two-plane format supports a shader's
        // use nowhere; without saying what this view is for, the picture is
        // built and the view is refused.
        let mut whole_usage =
            vk::ImageViewUsageCreateInfo::default().usage(usage & !vk::ImageUsageFlags::STORAGE);
        let info = vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(format)
            .subresource_range(vk::ImageSubresourceRange {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                base_mip_level: 0,
                level_count: 1,
                base_array_layer: 0,
                layer_count: 1,
            })
            .push_next(&mut whole_usage);
        // SAFETY: the chain outlives the call and the image is bound.
        let view = unsafe { self.device.create_image_view(&info, None) }.map_err(driver)?;

        let plane_views = if planes {
            let mut built = [vk::ImageView::null(); 2];
            for (at, (aspect, plane_format)) in [
                (vk::ImageAspectFlags::PLANE_0, vk::Format::R8_UNORM),
                (vk::ImageAspectFlags::PLANE_1, vk::Format::R8G8_UNORM),
            ]
            .into_iter()
            .enumerate()
            {
                let mut plane_usage =
                    vk::ImageViewUsageCreateInfo::default().usage(vk::ImageUsageFlags::STORAGE);
                let info = vk::ImageViewCreateInfo::default()
                    .image(image)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(plane_format)
                    .subresource_range(vk::ImageSubresourceRange {
                        aspect_mask: aspect,
                        base_mip_level: 0,
                        level_count: 1,
                        base_array_layer: 0,
                        layer_count: 1,
                    })
                    .push_next(&mut plane_usage);
                // SAFETY: the chain outlives the call and the image is bound.
                built[at] =
                    unsafe { self.device.create_image_view(&info, None) }.map_err(driver)?;
            }
            Some(built)
        } else {
            None
        };

        Ok(Picture {
            image,
            memory,
            view,
            planes: plane_views,
        })
    }

    /// Where a finished picture lands, in memory the processor can read.
    fn bitstream(&self) -> Result<(vk::Buffer, vk::DeviceMemory)> {
        let buffer = with_profile(|profile| {
            let profiles = [*profile];
            let mut list = vk::VideoProfileListInfoKHR::default().profiles(&profiles);
            let info = vk::BufferCreateInfo::default()
                .size(BITSTREAM_BYTES)
                .usage(vk::BufferUsageFlags::VIDEO_ENCODE_DST_KHR)
                .sharing_mode(vk::SharingMode::EXCLUSIVE)
                .push_next(&mut list);
            // SAFETY: the chain outlives the call.
            unsafe { self.device.create_buffer(&info, None) }
        })
        .map_err(driver)?;
        // SAFETY: created on this device.
        let wanted = unsafe { self.device.get_buffer_memory_requirements(buffer) };
        let index = self.memory_type(wanted, true)?;
        let allocate = vk::MemoryAllocateInfo::default()
            .allocation_size(wanted.size)
            .memory_type_index(index);
        // SAFETY: the allocate info outlives the call.
        let memory = unsafe { self.device.allocate_memory(&allocate, None) }.map_err(driver)?;
        // SAFETY: both handles are this device's and nothing is bound yet.
        unsafe { self.device.bind_buffer_memory(buffer, memory, 0) }.map_err(driver)?;
        Ok((buffer, memory))
    }

    /// The pool a written length comes back through.
    fn feedback_pool(&self) -> Result<vk::QueryPool> {
        let mut feedback = vk::QueryPoolVideoEncodeFeedbackCreateInfoKHR::default()
            .encode_feedback_flags(
                vk::VideoEncodeFeedbackFlagsKHR::BITSTREAM_BUFFER_OFFSET
                    | vk::VideoEncodeFeedbackFlagsKHR::BITSTREAM_BYTES_WRITTEN,
            );
        with_profile(|profile| {
            let mut owned = *profile;
            let create = vk::QueryPoolCreateInfo::default()
                .query_type(vk::QueryType::VIDEO_ENCODE_FEEDBACK_KHR)
                .query_count(1)
                .push_next(&mut feedback)
                .push_next(&mut owned);
            // SAFETY: the chain outlives the call.
            unsafe { self.device.create_query_pool(&create, None) }
        })
        .map_err(driver)
    }
}

impl Encoder<'_> {
    /// The planes a conversion writes, when this device lets it write them.
    ///
    /// **`None` means a copy stands between conversion and encode here.** It is
    /// a property of the device, not of the configuration, and a caller that
    /// treats absence as an error refuses a machine that works.
    pub fn planes(&self) -> Option<[vk::ImageView; 2]> {
        self.source.planes
    }

    /// The picture the encoder reads, for a caller that fills it by copying.
    pub fn source(&self) -> vk::Image {
        self.source.image
    }

    /// The size a picture must be.
    pub fn extent(&self) -> vk::Extent2D {
        self.extent
    }

    /// The queue family that will read the picture.
    pub fn family(&self) -> u32 {
        self.device.family
    }

    /// Change the bitrate on the running session.
    ///
    /// **No rebuild and no reset.** The rate is the only congestion actuator
    /// the design has, and rebuilding to apply one would cost a picture with no
    /// history behind it every time congestion moved.
    pub fn set_bitrate(&mut self, bitrate_bps: u32) {
        self.bitrate_bps = bitrate_bps;
    }

    /// Encode what is in the source picture.
    ///
    /// **The rate control is restated on every opening.** Once a session has
    /// been told anything other than the default, an opening that does not
    /// carry that same configuration is invalid, and a driver accepts it
    /// silently.
    pub fn submit(&mut self, force_keyframe: bool) -> Result<()> {
        let device = &self.device.device;
        // SAFETY: the previous submit was waited on, so both are idle.
        unsafe {
            device
                .reset_command_buffer(self.buffer, vk::CommandBufferResetFlags::empty())
                .map_err(driver)?;
            device.reset_fences(&[self.fence]).map_err(driver)?;
        }
        let begin = vk::CommandBufferBeginInfo::default()
            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
        // SAFETY: reset just above and not recording.
        unsafe { device.begin_command_buffer(self.buffer, &begin) }.map_err(driver)?;
        // SAFETY: recording; the pool is this device's.
        unsafe { device.cmd_reset_query_pool(self.buffer, self.pool, 0, 1) };

        let whole = vk::ImageSubresourceRange {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            base_mip_level: 0,
            level_count: 1,
            base_array_layer: 0,
            layer_count: 1,
        };
        // The first pass names the pictures for what the encoder will do with
        // them; later passes leave them where they were.
        if !self.started {
            let barriers = [
                vk::ImageMemoryBarrier::default()
                    .old_layout(vk::ImageLayout::UNDEFINED)
                    .new_layout(vk::ImageLayout::VIDEO_ENCODE_SRC_KHR)
                    .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .image(self.source.image)
                    .subresource_range(whole),
                vk::ImageMemoryBarrier::default()
                    .old_layout(vk::ImageLayout::UNDEFINED)
                    .new_layout(vk::ImageLayout::VIDEO_ENCODE_DPB_KHR)
                    .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .image(self.dpb.image)
                    .subresource_range(whole),
            ];
            // SAFETY: recording; the barriers outlive the call.
            unsafe {
                device.cmd_pipeline_barrier(
                    self.buffer,
                    vk::PipelineStageFlags::TOP_OF_PIPE,
                    vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &barriers,
                );
            }
        }

        let dpb_resource = vk::VideoPictureResourceInfoKHR::default()
            .coded_offset(vk::Offset2D { x: 0, y: 0 })
            .coded_extent(self.extent)
            .base_array_layer(0)
            .image_view_binding(self.dpb.view);
        let opening = [vk::VideoReferenceSlotInfoKHR::default()
            .slot_index(-1)
            .picture_resource(&dpb_resource)];
        // **The codec's half of the layer, which is not optional.** One vendor
        // accepts a layer without it and the other refuses the whole recording,
        // at the end of the buffer rather than at the command.
        let mut layer_h264 = vk::VideoEncodeH264RateControlLayerInfoKHR::default();
        let layers = [vk::VideoEncodeRateControlLayerInfoKHR::default()
            .average_bitrate(u64::from(self.bitrate_bps))
            .max_bitrate(u64::from(self.bitrate_bps))
            .frame_rate_numerator(60)
            .frame_rate_denominator(1)
            .push_next(&mut layer_h264)];
        // **The first picture configures with the rate off.** One vendor here
        // refuses a recording whose first control command both resets the
        // session and gives it a rate, and says so only at the end of the
        // buffer. The rate arrives with the second picture, which costs one
        // picture at a fixed quantiser -- and that picture is the one with no
        // history behind it anyway.
        let wanted = if self.started {
            vk::VideoEncodeRateControlModeFlagsKHR::CBR
        } else {
            vk::VideoEncodeRateControlModeFlagsKHR::DISABLED
        };
        let rated = wanted == vk::VideoEncodeRateControlModeFlagsKHR::CBR;
        // **The layer list is left alone when there is no rate.** An empty
        // slice still hands over a pointer, and a driver that reads it despite
        // being told there are none takes the process with it; the default is a
        // null with a count of zero, which is what "none" means here.
        let mut rate = vk::VideoEncodeRateControlInfoKHR::default().rate_control_mode(wanted);
        if rated {
            rate = rate
                .layers(&layers)
                .virtual_buffer_size_in_ms(1000)
                .initial_virtual_buffer_size_in_ms(0);
        }
        let mut h264_rate = vk::VideoEncodeH264RateControlInfoKHR::default()
            .gop_frame_count(60)
            .idr_period(60)
            .consecutive_b_frame_count(0)
            .temporal_layer_count(1);
        // **Its own structures, not the control's.** A chain is a list of
        // pointers, so pushing one structure onto two of them makes a shape
        // neither call can read; it reports as a duplicate kind rather than as
        // a cycle.
        let mut applied_h264 = vk::VideoEncodeH264RateControlLayerInfoKHR::default();
        let applied = [vk::VideoEncodeRateControlLayerInfoKHR::default()
            .average_bitrate(u64::from(self.applied_bps))
            .max_bitrate(u64::from(self.applied_bps))
            .frame_rate_numerator(60)
            .frame_rate_denominator(1)
            .push_next(&mut applied_h264)];
        let mut begin_rate =
            vk::VideoEncodeRateControlInfoKHR::default().rate_control_mode(self.mode);
        if self.mode == vk::VideoEncodeRateControlModeFlagsKHR::CBR {
            begin_rate = begin_rate
                .layers(&applied)
                .virtual_buffer_size_in_ms(1000)
                .initial_virtual_buffer_size_in_ms(0);
        }
        let mut begin_h264 = vk::VideoEncodeH264RateControlInfoKHR::default()
            .gop_frame_count(60)
            .idr_period(60)
            .consecutive_b_frame_count(0)
            .temporal_layer_count(1);
        let mut begin_coding = vk::VideoBeginCodingInfoKHR::default()
            .video_session(self.session)
            .video_session_parameters(self.parameters)
            .reference_slots(&opening);
        // **Only once the session has been told something.** An opening must
        // carry the configuration the session is actually in, and before the
        // first control command that is the default -- so naming a rate here
        // describes a session that does not exist yet.
        if self.mode != vk::VideoEncodeRateControlModeFlagsKHR::DEFAULT {
            begin_coding = begin_coding
                .push_next(&mut begin_rate)
                .push_next(&mut begin_h264);
        }
        // SAFETY: recording; the chain outlives the call.
        unsafe { (self.device.video.fp().cmd_begin_video_coding_khr)(self.buffer, &begin_coding) };

        // The rate is set on the first pass and whenever it changed; a reset
        // rides with the first because the session has nothing to keep yet.
        let mut quality = vk::VideoEncodeQualityLevelInfoKHR::default().quality_level(0);
        let mut control = vk::VideoCodingControlInfoKHR::default()
            .flags(if self.started {
                vk::VideoCodingControlFlagsKHR::ENCODE_RATE_CONTROL
            } else {
                // **A quality level with the first one.** One vendor refuses
                // the whole recording without it, and the refusal arrives at
                // the end of the buffer rather than at the command.
                vk::VideoCodingControlFlagsKHR::RESET
                    | vk::VideoCodingControlFlagsKHR::ENCODE_RATE_CONTROL
                    | vk::VideoCodingControlFlagsKHR::ENCODE_QUALITY_LEVEL
            })
            .push_next(&mut rate)
            .push_next(&mut h264_rate);
        if !self.started {
            control = control.push_next(&mut quality);
        }
        // SAFETY: recording; the chain outlives the call.
        unsafe { (self.device.video.fp().cmd_control_video_coding_khr)(self.buffer, &control) };

        // SAFETY: recording; every structure is a stack value that outlives
        // the call, and the codec structures are plain data.
        unsafe {
            let mut slice: ash::vk::native::StdVideoEncodeH264SliceHeader = core::mem::zeroed();
            slice.slice_type = ash::vk::native::StdVideoH264SliceType_STD_VIDEO_H264_SLICE_TYPE_I;
            // **Zero, because the rate is set.** A fixed quantiser and a
            // bitrate are two ways of saying the same thing and a device takes
            // only one; naming both is refused, and on one vendor the refusal
            // arrives as the whole recording failing to end.
            let slices = [vk::VideoEncodeH264NaluSliceInfoKHR::default()
                // Zero once a rate is set, because a fixed quantiser and a
                // bitrate are two ways of saying the same thing and a device
                // takes only one.
                .constant_qp(if rated { 0 } else { 26 })
                .std_slice_header(&slice)];
            let mut picture: ash::vk::native::StdVideoEncodeH264PictureInfo = core::mem::zeroed();
            picture.primary_pic_type =
                ash::vk::native::StdVideoH264PictureType_STD_VIDEO_H264_PICTURE_TYPE_IDR;
            picture.flags.set_IdrPicFlag(1);
            picture.flags.set_is_reference(1);
            let mut h264 = vk::VideoEncodeH264PictureInfoKHR::default()
                .nalu_slice_entries(&slices)
                .std_picture_info(&picture);
            let mut reference: ash::vk::native::StdVideoEncodeH264ReferenceInfo =
                core::mem::zeroed();
            reference.primary_pic_type =
                ash::vk::native::StdVideoH264PictureType_STD_VIDEO_H264_PICTURE_TYPE_IDR;
            let mut slot_h264 =
                vk::VideoEncodeH264DpbSlotInfoKHR::default().std_reference_info(&reference);
            let setup = vk::VideoReferenceSlotInfoKHR::default()
                .slot_index(0)
                .picture_resource(&dpb_resource)
                .push_next(&mut slot_h264);
            let source = vk::VideoPictureResourceInfoKHR::default()
                .coded_offset(vk::Offset2D { x: 0, y: 0 })
                .coded_extent(self.extent)
                .base_array_layer(0)
                .image_view_binding(self.source.view);
            let info = vk::VideoEncodeInfoKHR::default()
                .dst_buffer(self.bitstream)
                .dst_buffer_offset(0)
                .dst_buffer_range(BITSTREAM_BYTES)
                .src_picture_resource(source)
                .setup_reference_slot(&setup)
                .push_next(&mut h264);
            device.cmd_begin_query(self.buffer, self.pool, 0, vk::QueryControlFlags::empty());
            (self.device.encode.fp().cmd_encode_video_khr)(self.buffer, &info);
            device.cmd_end_query(self.buffer, self.pool, 0);
        }
        let _ = force_keyframe;

        let end = vk::VideoEndCodingInfoKHR::default();
        // SAFETY: recording; the structure outlives the call.
        unsafe {
            (self.device.video.fp().cmd_end_video_coding_khr)(self.buffer, &end);
            device.end_command_buffer(self.buffer).map_err(|e| {
                Error::Unsupported(match e {
                    vk::Result::ERROR_INITIALIZATION_FAILED => {
                        "this recording; the device refused it at the end rather than at a command"
                    }
                    _ => "the recording",
                })
            })?;
        }
        let buffers = [self.buffer];
        let submits = [vk::SubmitInfo::default().command_buffers(&buffers)];
        // SAFETY: everything borrowed outlives the wait a caller does.
        unsafe { device.queue_submit(self.device.queue, &submits, self.fence) }.map_err(driver)?;
        self.mode = wanted;
        self.started = true;
        self.applied_bps = self.bitrate_bps;
        Ok(())
    }

    /// Ask for a finished picture without waiting for one.
    ///
    /// **Honest, unlike the vendor path.** A picture that is not finished is
    /// reported as not ready rather than as a length that is not there yet.
    pub fn poll(&mut self) -> Result<Poll<'_>> {
        let device = &self.device.device;
        let mut values = [0_u32; 3];
        let bytes = size_of::<[u32; 3]>();
        // **Thirty-two bit, and not by preference.** Asking for sixty-four bit
        // results is honoured by one driver here and ignored by the other,
        // which writes narrow values into a wide destination: the length lands
        // in the high half of the offset and reads as zero.
        // SAFETY: one query into a destination of exactly its three values.
        let result = unsafe {
            (device.fp_v1_0().get_query_pool_results)(
                device.handle(),
                self.pool,
                0,
                1,
                bytes,
                values.as_mut_ptr().cast(),
                bytes as vk::DeviceSize,
                vk::QueryResultFlags::WITH_STATUS_KHR,
            )
        };
        if result == vk::Result::NOT_READY {
            return Ok(Poll::Pending);
        }
        checked(result)?;

        let (offset, length) = (values[0] as usize, values[1] as usize);
        // SAFETY: host visible and coherent, and the map covers the whole
        // allocation the buffer was bound to.
        let mapped = unsafe {
            device.map_memory(
                self.bitstream_memory,
                0,
                BITSTREAM_BYTES,
                vk::MemoryMapFlags::empty(),
            )
        }
        .map_err(driver)?;
        // **Copied out rather than lent.** The mapping is the driver's and is
        // handed back on the next submit, so a caller keeping the slice would
        // read a picture that has since been overwritten.
        self.collected.clear();
        // SAFETY: mapped above for the whole buffer, and the query says these
        // bytes are the picture.
        unsafe {
            let all = core::slice::from_raw_parts(
                mapped.cast::<u8>(),
                usize::try_from(BITSTREAM_BYTES).unwrap_or(0),
            );
            if let Some(picture) = all.get(offset..offset.saturating_add(length)) {
                self.collected.extend_from_slice(picture);
            }
            device.unmap_memory(self.bitstream_memory);
        }
        Ok(Poll::Ready {
            bitstream: &self.collected,
            keyframe: true,
        })
    }

    /// Wait for the picture in flight, which a caller does before reading it.
    pub fn wait(&self) -> Result<()> {
        // SAFETY: the fence is this device's and was submitted with.
        unsafe {
            self.device
                .device
                .wait_for_fences(&[self.fence], true, 5_000_000_000)
        }
        .map_err(driver)
    }
}

impl Drop for Encoder<'_> {
    fn drop(&mut self) {
        let device = &self.device.device;
        // SAFETY: every handle is this device's, and the wait is what makes
        // releasing them safe while work may still be running.
        unsafe {
            let _ = device.device_wait_idle();
            device.destroy_fence(self.fence, None);
            device.destroy_command_pool(self.commands, None);
            device.destroy_query_pool(self.pool, None);
            device.destroy_buffer(self.bitstream, None);
            device.free_memory(self.bitstream_memory, None);
            for picture in [&self.source, &self.dpb] {
                if let Some(planes) = picture.planes {
                    for view in planes {
                        device.destroy_image_view(view, None);
                    }
                }
                device.destroy_image_view(picture.view, None);
                device.destroy_image(picture.image, None);
                device.free_memory(picture.memory, None);
            }
            (self.device.video.fp().destroy_video_session_parameters_khr)(
                device.handle(),
                self.parameters,
                core::ptr::null(),
            );
            (self.device.video.fp().destroy_video_session_khr)(
                device.handle(),
                self.session,
                core::ptr::null(),
            );
            for memory in self.session_memory.drain(..) {
                device.free_memory(memory, None);
            }
        }
    }
}
