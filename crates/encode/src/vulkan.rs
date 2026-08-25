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
    /// A source slot that was never allocated. The count is fixed when the
    /// encoder is built.
    BadSlot,
    /// A picture is already in flight. Back pressure, not a fault: collect
    /// one and try again.
    Busy,
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
            Self::BadSlot => f.write_str("no such source slot"),
            Self::Busy => f.write_str("a picture is already in flight"),
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

/// What is enabled where the device has it, and never required.
///
/// **A codec the device does not carry must not close the path to the one it
/// does.** Naming this alongside the list above would refuse a device that
/// encodes one codec and not the other; asked for here, an absent codec is
/// refused later by [`Device::caps`], which can say which one and why.
const OPTIONAL: [&CStr; 1] = [ash::khr::video_encode_h265::NAME];

/// The codecs this backend produces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    H264,
    H265,
}

/// What a device says it will do.
#[derive(Debug, Clone, Copy)]
pub struct Caps {
    /// Which codec these answers are about. Every later call needs the same
    /// profile these were queried under, and carrying it here is what stops
    /// a session being built against one codec's answers under the other's.
    pub codec: Codec,
    /// Rate control modes offered. Anything but a mode taking a bitrate means
    /// the only congestion actuator the design has cannot be built here.
    pub rate_control: vk::VideoEncodeRateControlModeFlagsKHR,
    /// The largest picture a session may be built for.
    pub max_extent: vk::Extent2D,
    /// The granularity the device accesses a picture at. A coded size that is
    /// not a whole number of these is still read and written in whole ones.
    pub picture_granularity: vk::Extent2D,
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
fn with_profile<R>(codec: Codec, f: impl FnOnce(&vk::VideoProfileInfoKHR<'_>) -> R) -> R {
    let base = vk::VideoProfileInfoKHR::default()
        .chroma_subsampling(vk::VideoChromaSubsamplingFlagsKHR::TYPE_420)
        .luma_bit_depth(vk::VideoComponentBitDepthFlagsKHR::TYPE_8)
        .chroma_bit_depth(vk::VideoComponentBitDepthFlagsKHR::TYPE_8);
    match codec {
        Codec::H264 => {
            let mut h264 = vk::VideoEncodeH264ProfileInfoKHR::default().std_profile_idc(
                ash::vk::native::StdVideoH264ProfileIdc_STD_VIDEO_H264_PROFILE_IDC_HIGH,
            );
            let profile = base
                .video_codec_operation(vk::VideoCodecOperationFlagsKHR::ENCODE_H264)
                .push_next(&mut h264);
            f(&profile)
        }
        // Main, which is eight-bit 4:2:0 -- what the conversion produces.
        Codec::H265 => {
            let mut h265 = vk::VideoEncodeH265ProfileInfoKHR::default().std_profile_idc(
                ash::vk::native::StdVideoH265ProfileIdc_STD_VIDEO_H265_PROFILE_IDC_MAIN,
            );
            let profile = base
                .video_codec_operation(vk::VideoCodecOperationFlagsKHR::ENCODE_H265)
                .push_next(&mut h265);
            f(&profile)
        }
    }
}

/// The loaded interface. Held for as long as anything built on it.
pub struct Vulkan {
    _entry: ash::Entry,
    instance: ash::Instance,
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
        Ok(Self {
            _entry: entry,
            instance,
        })
    }

    /// The device that drives a display node, ready to encode.
    ///
    /// **Matched on the node numbers the driver reports**, which is exact.
    /// Matching on a name or an index breaks the moment a machine has two cards
    /// from one vendor, or reorders them across a reboot.
    pub fn open(&self, node: &Path) -> Result<Device> {
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
            // What the device also has is turned on with it: a session is
            // built for one codec, but which one is not known here.
            let extra: Vec<&CStr> = OPTIONAL.into_iter().filter(|name| has(name)).collect();
            return Device::open(&self.instance, &self._entry, physical, &extra);
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
/// A device this owns, or one lent by whoever opened it.
///
/// **Which it is decides only who releases it.** Everything else reads through
/// it identically, which is what lets an encoder be built on a device the
/// capture already opened.
enum Held {
    /// Boxed only so the two arms stay comparable in size; the table a device
    /// is made of is large.
    Owned(Box<ash::Device>),
    /// A clone of the capture's device wrapper, which is what keeps the
    /// underlying device alive for as long as this encoder needs it -- the
    /// last clone dropped releases it, so drop order stops mattering.
    Shared(lowlat_capture::vulkan::Device),
}

impl core::ops::Deref for Held {
    type Target = ash::Device;

    fn deref(&self) -> &Self::Target {
        match self {
            Self::Owned(device) => device,
            Self::Shared(device) => device.ash(),
        }
    }
}

pub struct Device {
    instance: ash::Instance,
    /// The physical-device queries, which live above any device.
    queries: ash::khr::video_queue::Instance,
    physical: vk::PhysicalDevice,
    device: Held,
    video: ash::khr::video_queue::Device,
    encode: ash::khr::video_encode_queue::Device,
    queue: vk::Queue,
    /// The family that encodes.
    pub family: u32,
    /// A family that can write a picture, which the encode family may not be.
    pub writer_family: u32,
    name: String,
}

impl core::fmt::Debug for Device {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Device")
            .field("name", &self.name)
            .field("family", &self.family)
            .finish_non_exhaustive()
    }
}

impl Device {
    /// Build on a device somebody else opened.
    ///
    /// **This is the arrangement the path exists for.** The capture, the
    /// conversion and the encode are then one device and one interface, and a
    /// picture is handed between them as a picture rather than as a descriptor
    /// somebody has to re-import.
    ///
    /// The caller is the owner: nothing here releases the device, and the
    /// borrow is what stops this outliving it.
    pub fn shared(
        capture: lowlat_capture::vulkan::Device,
        queue: vk::Queue,
        family: u32,
    ) -> Result<Self> {
        let video = ash::khr::video_queue::Device::new(capture.ash_instance(), capture.ash());
        let encode =
            ash::khr::video_encode_queue::Device::new(capture.ash_instance(), capture.ash());
        let mut properties = vk::PhysicalDeviceProperties2::default();
        // SAFETY: the device came from this instance.
        unsafe {
            capture
                .ash_instance()
                .get_physical_device_properties2(capture.physical(), &mut properties);
        }
        let name = properties
            .properties
            .device_name_as_c_str()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "an unnamed device".to_string());
        Ok(Self {
            instance: capture.ash_instance().clone(),
            queries: ash::khr::video_queue::Instance::new(capture.entry(), capture.ash_instance()),
            physical: capture.physical(),
            // The conversion's own family writes the pictures this device
            // encodes, and the clone below is what keeps the shared device
            // alive for as long as this one is.
            writer_family: capture.family(),
            device: Held::Shared(capture),
            video,
            encode,
            queue,
            family,
            name,
        })
    }

    fn open(
        instance: &ash::Instance,
        entry: &ash::Entry,
        physical: vk::PhysicalDevice,
        extra: &[&CStr],
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
        let names: Vec<*const core::ffi::c_char> = REQUIRED
            .iter()
            .chain(extra.iter())
            .map(|name| name.as_ptr())
            .collect();
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
            instance: instance.clone(),
            queries: ash::khr::video_queue::Instance::new(entry, instance),
            physical,
            device: Held::Owned(Box::new(device)),
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
        let mut encode = vk::VideoEncodeCapabilitiesKHR::default();
        // **The codec's own capabilities have to be in the chain.** Asking
        // about an encode without them is invalid, and a driver answers anyway
        // with a structure it never filled.
        let mut h264 = vk::VideoEncodeH264CapabilitiesKHR::default();
        let mut h265 = vk::VideoEncodeH265CapabilitiesKHR::default();
        let mut caps = vk::VideoCapabilitiesKHR::default().push_next(&mut encode);
        caps = match codec {
            Codec::H264 => caps.push_next(&mut h264),
            Codec::H265 => caps.push_next(&mut h265),
        };
        with_profile(codec, |profile| {
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
        let picture_granularity = caps.picture_access_granularity;
        let max_dpb_slots = caps.max_dpb_slots;
        let max_active_references = caps.max_active_reference_pictures;
        let std_header = caps.std_header_version;
        let bitstream_alignment = caps.min_bitstream_buffer_offset_alignment;
        let rate_control = encode.rate_control_modes;

        let picture = self
            .formats(
                codec,
                vk::ImageUsageFlags::VIDEO_ENCODE_SRC_KHR | vk::ImageUsageFlags::TRANSFER_DST,
            )?
            .first()
            .copied()
            .ok_or(Error::Unsupported("a layout for a picture it would encode"))?;
        let reference = self
            .formats(codec, vk::ImageUsageFlags::VIDEO_ENCODE_DPB_KHR)?
            .first()
            .copied()
            .ok_or(Error::Unsupported("a layout for a reference picture"))?;
        // **Asked, never assumed.** One vendor here will let a shader write the
        // very picture the encoder reads and the other will not, and building
        // on the wrong answer is building a copy that cannot be removed or one
        // that is missing.
        let shared_picture = self
            .formats(
                codec,
                vk::ImageUsageFlags::VIDEO_ENCODE_SRC_KHR | vk::ImageUsageFlags::STORAGE,
            )
            .is_ok_and(|formats| formats.contains(&picture));

        Ok(Caps {
            codec,
            rate_control,
            max_extent,
            picture_granularity,
            max_dpb_slots,
            max_active_references,
            std_header,
            picture,
            reference,
            shared_picture,
            bitstream_alignment,
        })
    }

    /// The block sizes and level this codec's sets may declare.
    ///
    /// **The device's answer, not a preference.** The sets name the blocks and
    /// the encoder codes to them, so a size the device does not implement is
    /// coded at one it does with the declaration left standing -- a stream
    /// whose syntax does not match its own description. The largest offered is
    /// taken, which is the fewest blocks and the fewest headers with them; the
    /// level is capped at what the device admits to, because a set naming a
    /// level above it describes a device that is not there.
    fn h265_limits(&self) -> Result<(u32, u32, u32)> {
        let mut h265 = vk::VideoEncodeH265CapabilitiesKHR::default();
        // **The encode capabilities belong in the chain even when nothing
        // here reads them.** Asking about an encode profile without them is
        // invalid, and a driver answers anyway.
        let mut encode = vk::VideoEncodeCapabilitiesKHR::default();
        let mut caps = vk::VideoCapabilitiesKHR::default()
            .push_next(&mut encode)
            .push_next(&mut h265);
        with_profile(Codec::H265, |profile| {
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
        let ctb = [
            (vk::VideoEncodeH265CtbSizeFlagsKHR::TYPE_64, 6),
            (vk::VideoEncodeH265CtbSizeFlagsKHR::TYPE_32, 5),
            (vk::VideoEncodeH265CtbSizeFlagsKHR::TYPE_16, 4),
        ]
        .into_iter()
        .find(|(flag, _)| h265.ctb_sizes.contains(*flag))
        .map(|(_, log2)| log2)
        .ok_or(Error::Unsupported("any coding tree block size"))?;
        let transform = [
            (vk::VideoEncodeH265TransformBlockSizeFlagsKHR::TYPE_32, 5),
            (vk::VideoEncodeH265TransformBlockSizeFlagsKHR::TYPE_16, 4),
            (vk::VideoEncodeH265TransformBlockSizeFlagsKHR::TYPE_8, 3),
        ]
        .into_iter()
        .find(|(flag, _)| h265.transform_block_sizes.contains(*flag))
        .map(|(_, log2)| log2)
        .ok_or(Error::Unsupported("any transform block size"))?;
        let level = h265
            .max_level_idc
            .min(ash::vk::native::StdVideoH265LevelIdc_STD_VIDEO_H265_LEVEL_IDC_5_2);
        Ok((ctb, transform, level))
    }

    /// Which layouts the device takes for one use of a video picture.
    fn formats(&self, codec: Codec, usage: vk::ImageUsageFlags) -> Result<Vec<vk::Format>> {
        with_profile(codec, |profile| {
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

impl Drop for Device {
    fn drop(&mut self) {
        // **Only what this opened is released.** A lent device belongs to
        // whoever opened it, and destroying it here would be the second free.
        let Held::Owned(device) = &self.device else {
            return;
        };
        // SAFETY: everything built on it is released before this, and the wait
        // is what makes that true for work still running.
        unsafe {
            let _ = device.device_wait_idle();
            device.destroy_device(None);
        }
    }
}

/// The smallest coding block this codec's sets declare, as a power of two.
/// It is also what the coded picture size rounds up to.
const LOG2_MIN_CODING_BLOCK: u32 = 3;

/// The smallest transform, as a power of two.
const LOG2_MIN_TRANSFORM_BLOCK: u32 = 2;

/// Bits of picture order count before it wraps, less four.
const LOG2_MAX_POC_LSB_MINUS4: u8 = 4;

/// The size a sequence set declares, given what the device accesses.
///
/// **Whole units of the device's granularity, not of the codec's smallest
/// coding block.** The block is all the codec asks for, and a set that
/// declares it is legal and decodes to a picture whose last partial row of
/// blocks is wrong: the device reads and writes whole units whatever the set
/// says, so the set has to name them and the conformance window has to carry
/// the difference. One device here accesses H.265 64x16 at a time where it
/// accesses H.264 16x16, which is why rounding to the coding block was enough
/// on one path and is not on the other.
fn coded_size(extent: vk::Extent2D, granularity: vk::Extent2D) -> vk::Extent2D {
    let unit = |granularity: u32| granularity.max(1 << LOG2_MIN_CODING_BLOCK);
    vk::Extent2D {
        width: extent.width.next_multiple_of(unit(granularity.width)),
        height: extent.height.next_multiple_of(unit(granularity.height)),
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
    device: &'a Device,
    /// Which codec this session codes. Every recording's chain is the codec's
    /// own, and a session cannot change it.
    codec: Codec,
    /// **The rate the loop feeds this encoder at, which the rate control has
    /// to be told.** A bitrate is a budget per second and the device spends it
    /// per picture, so an encoder told sixty and fed a hundred and twenty is
    /// working to twice the budget it was given.
    fps: u32,
    session: vk::VideoSessionKHR,
    session_memory: Vec<vk::DeviceMemory>,
    parameters: vk::VideoSessionParametersKHR,
    /// The pictures a caller writes and a submit encodes, one per slot.
    ///
    /// **A ring, so a conversion can fill the next picture while the encode
    /// reads the previous one.** One source would serialise the two: the
    /// writer cannot touch the picture an encode in flight is reading.
    sources: Vec<Picture>,
    /// The reconstructed pictures, one layer per slot of one image, written
    /// and read in alternation: the picture being encoded writes one layer
    /// while its reference is read from the other. One layer cannot be both
    /// -- the driver would read what it is overwriting, the same rule the
    /// open backend's surfaces have -- and the slots are layers rather than
    /// separate images because the separate form is a capability only one
    /// vendor here reports, while the layered form works everywhere.
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
    /// Pictures coded since the last coded refresh, which is what the frame
    /// number and picture order derive from. Continuous rather than wrapped:
    /// the wire field wraps at the size the sequence set names, and the order
    /// count is defined over the unwrapped value.
    frames_since_idr: u32,
    /// Which refresh this is. A decoder uses it to tell two adjacent
    /// refreshes apart, so it moves on every one.
    idr_id: u16,
    /// The reconstruction slot the next picture writes.
    recon: usize,
    /// The picture the next predicted picture references, or nothing when the
    /// next picture must refresh.
    previous: Option<Reference>,
    /// Whether the picture in flight is a refresh, reported by the poll.
    submitted_keyframe: bool,
    /// Whether a picture is in flight at all. One command buffer and one
    /// fence mean one at a time; a second submit is refused as back pressure
    /// rather than racing the first.
    in_flight: bool,
    /// The encoded sequence and picture sets, fetched from the driver once.
    ///
    /// **The bitstream carries slices only**: nothing on this path emits the
    /// sets into the stream, so a caller opens its stream with these -- and
    /// they come from the driver rather than being written by hand, because
    /// the driver is the authority on what it encodes against.
    sets: Vec<u8>,
}

/// One reconstructed picture a later picture may predict from.
#[derive(Debug, Clone, Copy)]
struct Reference {
    slot: usize,
    frame_num: u32,
    poc: i32,
    /// Whether the picture standing in the slot is a refresh, because its
    /// description travels with every scope that binds it.
    idr: bool,
}

impl core::fmt::Debug for Encoder<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Encoder")
            .field("extent", &self.extent)
            .field("bitrate_bps", &self.bitrate_bps)
            .finish_non_exhaustive()
    }
}

impl Device {
    /// Build a session and everything one picture needs.
    pub fn encoder<'a>(
        &'a self,
        caps: &Caps,
        width: u32,
        height: u32,
        bitrate_bps: u32,
        fps: u32,
        sources: usize,
    ) -> Result<Encoder<'a>> {
        let extent = vk::Extent2D {
            width: width.next_multiple_of(2),
            height: height.next_multiple_of(2),
        };
        let session = self.session(caps, extent)?;
        let built = self
            .finish_encoder(
                caps,
                extent,
                bitrate_bps,
                fps.max(1),
                session,
                sources.clamp(1, 4),
            )
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
        with_profile(caps.codec, |profile| {
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

impl Device {
    /// Everything after the session exists, so a failure has one place to undo.
    fn finish_encoder<'a>(
        &'a self,
        caps: &Caps,
        extent: vk::Extent2D,
        bitrate_bps: u32,
        fps: u32,
        session: vk::VideoSessionKHR,
        sources: usize,
    ) -> Result<Encoder<'a>> {
        let session_memory = self.bind_session(session)?;
        let parameters = self.parameters(caps, session, extent)?;
        let sets = self.encoded_parameters(caps.codec, parameters)?;

        // **The picture the encoder reads, and where it comes from.** Where a
        // shader may write it, the conversion writes here and nothing moves;
        // where it may not, a caller copies into it.
        let mut source_ring = Vec::with_capacity(sources);
        for _ in 0..sources {
            let shared_families = [self.writer_family, self.family];
            source_ring.push(self.picture(
                caps.codec,
                caps.picture,
                extent,
                if caps.shared_picture {
                    vk::ImageUsageFlags::VIDEO_ENCODE_SRC_KHR | vk::ImageUsageFlags::STORAGE
                } else {
                    vk::ImageUsageFlags::VIDEO_ENCODE_SRC_KHR | vk::ImageUsageFlags::TRANSFER_DST
                },
                caps.shared_picture,
                1,
                if caps.shared_picture && self.writer_family != self.family {
                    &shared_families
                } else {
                    &[]
                },
            )?);
        }
        let dpb = self.picture(
            caps.codec,
            caps.reference,
            extent,
            vk::ImageUsageFlags::VIDEO_ENCODE_DPB_KHR,
            false,
            DPB_SLOTS,
            &[],
        )?;

        let (bitstream, bitstream_memory) = self.bitstream(caps.codec)?;
        let pool = self.feedback_pool(caps.codec)?;
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
            codec: caps.codec,
            fps,
            session,
            session_memory,
            parameters,
            sources: source_ring,
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
            frames_since_idr: 0,
            idr_id: 0,
            recon: 0,
            previous: None,
            submitted_keyframe: false,
            in_flight: false,
            sets,
        })
    }

    /// The encoded sequence and picture sets, from the driver.
    ///
    /// Two calls: the first asks how many bytes, the second fills them. The
    /// driver encodes the sets it will actually code against, which is the
    /// property a hand-written copy cannot promise.
    fn encoded_parameters(
        &self,
        codec: Codec,
        parameters: vk::VideoSessionParametersKHR,
    ) -> Result<Vec<u8>> {
        let mut h264 = vk::VideoEncodeH264SessionParametersGetInfoKHR::default()
            .write_std_sps(true)
            .write_std_pps(true)
            .std_sps_id(0)
            .std_pps_id(0);
        // **Three sets on this codec, not two.** The extra one describes the
        // sequence's layers, and a decoder that never sees it has nothing to
        // attach the sequence to.
        let mut h265 = vk::VideoEncodeH265SessionParametersGetInfoKHR::default()
            .write_std_vps(true)
            .write_std_sps(true)
            .write_std_pps(true)
            .std_vps_id(0)
            .std_sps_id(0)
            .std_pps_id(0);
        let info = vk::VideoEncodeSessionParametersGetInfoKHR::default()
            .video_session_parameters(parameters);
        let info = match codec {
            Codec::H264 => info.push_next(&mut h264),
            Codec::H265 => info.push_next(&mut h265),
        };
        let mut size = 0usize;
        // SAFETY: asking for the size writes only the counter.
        checked(unsafe {
            (self.encode.fp().get_encoded_video_session_parameters_khr)(
                self.device.handle(),
                &info,
                core::ptr::null_mut(),
                &raw mut size,
                core::ptr::null_mut(),
            )
        })?;
        let mut data = vec![0u8; size];
        // SAFETY: the destination is exactly as large as the driver asked.
        checked(unsafe {
            (self.encode.fp().get_encoded_video_session_parameters_khr)(
                self.device.handle(),
                &info,
                core::ptr::null_mut(),
                &raw mut size,
                data.as_mut_ptr().cast(),
            )
        })?;
        data.truncate(size);
        Ok(data)
    }

    /// The parameter sets, written here because no driver emits them.
    fn parameters(
        &self,
        caps: &Caps,
        session: vk::VideoSessionKHR,
        extent: vk::Extent2D,
    ) -> Result<vk::VideoSessionParametersKHR> {
        match caps.codec {
            Codec::H264 => self.parameters_h264(session, extent),
            Codec::H265 => self.parameters_h265(caps, session, extent),
        }
    }

    /// Hand the built chain over, whichever codec wrote it.
    fn create_parameters(
        &self,
        create: &vk::VideoSessionParametersCreateInfoKHR<'_>,
    ) -> Result<vk::VideoSessionParametersKHR> {
        let mut handle = vk::VideoSessionParametersKHR::null();
        // SAFETY: the chain outlives the call and the out handle is live.
        let result = unsafe {
            (self.video.fp().create_video_session_parameters_khr)(
                self.device.handle(),
                create,
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

    fn parameters_h264(
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
        self.create_parameters(&create)
    }

    /// The three sets this codec opens with.
    ///
    /// **The block sizes and the level are the device's**, read rather than
    /// assumed: the sets declare what the encoder codes to, and a declaration
    /// the device does not implement is coded around silently.
    fn parameters_h265(
        &self,
        caps: &Caps,
        session: vk::VideoSessionKHR,
        extent: vk::Extent2D,
    ) -> Result<vk::VideoSessionParametersKHR> {
        let (ctb_log2, transform_log2_max, level) = self.h265_limits()?;
        let small = |value: u32| u8::try_from(value).unwrap_or(0);

        // SAFETY: plain data from the codec headers with no invariant beyond
        // its layout; all-zero is a valid set with no optional tables, and
        // every field that matters is written below. The same holds for the
        // three that follow.
        let mut tier: ash::vk::native::StdVideoH265ProfileTierLevel =
            unsafe { core::mem::zeroed() };
        tier.general_profile_idc =
            ash::vk::native::StdVideoH265ProfileIdc_STD_VIDEO_H265_PROFILE_IDC_MAIN;
        tier.general_level_idc = level;
        // **A decoder matches on these as well as on the profile.** Left
        // clear, a main-profile stream looks like one no profile claims.
        tier.flags.set_general_progressive_source_flag(1);
        tier.flags.set_general_frame_only_constraint_flag(1);

        // SAFETY: as above.
        let mut buffering: ash::vk::native::StdVideoH265DecPicBufMgr =
            unsafe { core::mem::zeroed() };
        // One reference behind the picture being coded, and nothing reordered:
        // every picture here predicts from the one before it.
        buffering.max_dec_pic_buffering_minus1[0] = 1;
        buffering.max_num_reorder_pics[0] = 0;
        buffering.max_latency_increase_plus1[0] = 0;

        // SAFETY: as above.
        let mut vps: ash::vk::native::StdVideoH265VideoParameterSet =
            unsafe { core::mem::zeroed() };
        vps.vps_video_parameter_set_id = 0;
        vps.vps_max_sub_layers_minus1 = 0;
        vps.flags.set_vps_temporal_id_nesting_flag(1);
        vps.flags.set_vps_sub_layer_ordering_info_present_flag(1);
        vps.pDecPicBufMgr = &raw const buffering;
        vps.pProfileTierLevel = &raw const tier;

        // **The coded size rounds up to whole units of what the device
        // accesses**, and the conformance window carries the remainder. The
        // codec itself asks only for a whole number of the smallest coding
        // block, and a set declaring that is legal and decodes to a picture
        // whose last partial row of blocks is wrong: the device reads and
        // writes whole units whatever the set says, so the set has to name
        // them. One device here accesses this codec 64x16 at a time where it
        // accesses the other 16x16, which is why the other path's rounding to
        // its own block size was enough and this one's is not.
        //
        // The window is in chroma units for a 4:2:0 stream, so a crop of six
        // luma rows is written as three.
        let coded = coded_size(extent, caps.picture_granularity);
        let (coded_width, coded_height) = (coded.width, coded.height);

        // SAFETY: as above.
        let mut sps: ash::vk::native::StdVideoH265SequenceParameterSet =
            unsafe { core::mem::zeroed() };
        sps.chroma_format_idc =
            ash::vk::native::StdVideoH265ChromaFormatIdc_STD_VIDEO_H265_CHROMA_FORMAT_IDC_420;
        sps.pic_width_in_luma_samples = coded_width;
        sps.pic_height_in_luma_samples = coded_height;
        sps.sps_video_parameter_set_id = 0;
        sps.sps_max_sub_layers_minus1 = 0;
        sps.sps_seq_parameter_set_id = 0;
        sps.log2_max_pic_order_cnt_lsb_minus4 = LOG2_MAX_POC_LSB_MINUS4;
        sps.log2_min_luma_coding_block_size_minus3 = small(LOG2_MIN_CODING_BLOCK - 3);
        sps.log2_diff_max_min_luma_coding_block_size = small(ctb_log2 - LOG2_MIN_CODING_BLOCK);
        sps.log2_min_luma_transform_block_size_minus2 = small(LOG2_MIN_TRANSFORM_BLOCK - 2);
        sps.log2_diff_max_min_luma_transform_block_size =
            small(transform_log2_max - LOG2_MIN_TRANSFORM_BLOCK);
        // As far below the coding block as the transform can go, which is what
        // the two sizes above already bound.
        sps.max_transform_hierarchy_depth_inter = small(ctb_log2 - LOG2_MIN_TRANSFORM_BLOCK);
        sps.max_transform_hierarchy_depth_intra = sps.max_transform_hierarchy_depth_inter;
        // **No set is stored here**, so every predicted picture carries its
        // own inline: one reference and one delta is shorter written out than
        // a table would be, and it keeps the set beside the picture that uses
        // it rather than in two places that have to agree.
        sps.num_short_term_ref_pic_sets = 0;
        sps.num_long_term_ref_pics_sps = 0;
        sps.conf_win_right_offset = (coded_width - extent.width) / 2;
        sps.conf_win_bottom_offset = (coded_height - extent.height) / 2;
        sps.flags.set_sps_temporal_id_nesting_flag(1);
        sps.flags.set_sps_sub_layer_ordering_info_present_flag(1);
        sps.flags.set_amp_enabled_flag(1);
        sps.flags.set_sample_adaptive_offset_enabled_flag(1);
        if sps.conf_win_right_offset > 0 || sps.conf_win_bottom_offset > 0 {
            sps.flags.set_conformance_window_flag(1);
        }
        sps.pProfileTierLevel = &raw const tier;
        sps.pDecPicBufMgr = &raw const buffering;

        // SAFETY: as above.
        let mut pps: ash::vk::native::StdVideoH265PictureParameterSet =
            unsafe { core::mem::zeroed() };
        pps.pps_pic_parameter_set_id = 0;
        pps.pps_seq_parameter_set_id = 0;
        pps.sps_video_parameter_set_id = 0;
        pps.init_qp_minus26 = 0;
        pps.flags.set_transform_skip_enabled_flag(1);
        // **Rate control has no other handle on this codec.** The other one
        // lets every block carry a quantiser delta unconditionally; here the
        // delta exists only if this turns it on, so a stream without it is
        // stuck at the slice quantiser and the configured bitrate does
        // nothing.
        pps.flags.set_cu_qp_delta_enabled_flag(1);
        pps.flags.set_pps_loop_filter_across_slices_enabled_flag(1);

        let vps_list = [vps];
        let sps_list = [sps];
        let pps_list = [pps];
        let add = vk::VideoEncodeH265SessionParametersAddInfoKHR::default()
            .std_vp_ss(&vps_list)
            .std_sp_ss(&sps_list)
            .std_pp_ss(&pps_list);
        let mut h265 = vk::VideoEncodeH265SessionParametersCreateInfoKHR::default()
            .max_std_vps_count(1)
            .max_std_sps_count(1)
            .max_std_pps_count(1)
            .parameters_add_info(&add);
        let create = vk::VideoSessionParametersCreateInfoKHR::default()
            .video_session(session)
            .push_next(&mut h265);
        self.create_parameters(&create)
    }

    /// One video picture with its memory and views.
    ///
    /// `layers` is the reconstruction arrangement: the slots live as layers
    /// of one image, because the separate-images form is a capability only
    /// one vendor here reports and the layered form works everywhere.
    #[expect(
        clippy::too_many_arguments,
        reason = "one picture's whole description; a struct for it would be a type with one use"
    )]
    fn picture(
        &self,
        codec: Codec,
        format: vk::Format,
        extent: vk::Extent2D,
        usage: vk::ImageUsageFlags,
        planes: bool,
        layers: u32,
        families: &[u32],
    ) -> Result<Picture> {
        let image = with_profile(codec, |profile| {
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
                .array_layers(layers)
                .samples(vk::SampleCountFlags::TYPE_1)
                .tiling(vk::ImageTiling::OPTIMAL)
                .usage(usage)
                .initial_layout(vk::ImageLayout::UNDEFINED)
                .push_next(&mut list);
            // **Shared between the family that writes and the family that
            // encodes**, where they differ: exclusive ownership would make
            // the contents undefined across the handoff without a transfer
            // barrier on each side, and the transfer is the one piece of
            // per-frame ceremony this path can do without.
            create = if families.len() >= 2 {
                create
                    .sharing_mode(vk::SharingMode::CONCURRENT)
                    .queue_family_indices(families)
            } else {
                create.sharing_mode(vk::SharingMode::EXCLUSIVE)
            };
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
            .view_type(if layers > 1 {
                vk::ImageViewType::TYPE_2D_ARRAY
            } else {
                vk::ImageViewType::TYPE_2D
            })
            .format(format)
            .subresource_range(vk::ImageSubresourceRange {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                base_mip_level: 0,
                level_count: 1,
                base_array_layer: 0,
                layer_count: layers,
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
    fn bitstream(&self, codec: Codec) -> Result<(vk::Buffer, vk::DeviceMemory)> {
        let buffer = with_profile(codec, |profile| {
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
    fn feedback_pool(&self, codec: Codec) -> Result<vk::QueryPool> {
        let mut feedback = vk::QueryPoolVideoEncodeFeedbackCreateInfoKHR::default()
            .encode_feedback_flags(
                vk::VideoEncodeFeedbackFlagsKHR::BITSTREAM_BUFFER_OFFSET
                    | vk::VideoEncodeFeedbackFlagsKHR::BITSTREAM_BYTES_WRITTEN,
            );
        with_profile(codec, |profile| {
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
    /// The encoded sequence and picture sets a stream opens with.
    ///
    /// The bitstream a collect hands out carries slices only; a decoder that
    /// has not seen these decodes nothing.
    pub fn parameter_sets(&self) -> &[u8] {
        &self.sets
    }

    /// The planes a conversion writes into one slot, when this device lets it
    /// write them.
    ///
    /// **`None` for a slot that exists means a copy stands between conversion
    /// and encode here.** Writability is a property of the device, not of the
    /// configuration, and a caller that treats absence as an error refuses a
    /// machine that works.
    pub fn planes(&self, slot: usize) -> Option<[vk::ImageView; 2]> {
        self.sources.get(slot)?.planes
    }

    /// The picture a submit of this slot encodes, for a caller that fills it
    /// by copying.
    pub fn source(&self, slot: usize) -> Option<vk::Image> {
        Some(self.sources.get(slot)?.image)
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
    pub fn submit(&mut self, slot: usize, force_keyframe: bool) -> Result<()> {
        self.encode_slot(slot, force_keyframe, false)
    }

    /// Encode a slot whose picture a writer filled and handed over.
    ///
    /// **The writer owns the picture's layout on this path.** A conversion
    /// ends its recording by moving the picture into the layout the encoder
    /// reads, so nothing here may touch it: the discard [`Self::submit`]
    /// performs on its first pass would throw away the very picture it was
    /// handed. A session drives its sources through one of the two entry
    /// points, never both.
    pub fn submit_written(&mut self, slot: usize, force_keyframe: bool) -> Result<()> {
        self.encode_slot(slot, force_keyframe, true)
    }

    fn encode_slot(&mut self, slot: usize, force_keyframe: bool, writer_owned: bool) -> Result<()> {
        if self.in_flight {
            return Err(Error::Busy);
        }
        let source_view = self.sources.get(slot).ok_or(Error::BadSlot)?.view;
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
            // The sources are named only where this encoder owns their
            // layout; a writer-owned picture arrives already in the layout
            // the encode reads, and a discard here would empty it.
            let mut barriers: Vec<vk::ImageMemoryBarrier<'_>> = if writer_owned {
                Vec::with_capacity(1)
            } else {
                self.sources
                    .iter()
                    .map(|picture| {
                        vk::ImageMemoryBarrier::default()
                            .old_layout(vk::ImageLayout::UNDEFINED)
                            .new_layout(vk::ImageLayout::VIDEO_ENCODE_SRC_KHR)
                            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                            .image(picture.image)
                            .subresource_range(whole)
                    })
                    .collect()
            };
            barriers.push(
                vk::ImageMemoryBarrier::default()
                    .old_layout(vk::ImageLayout::UNDEFINED)
                    .new_layout(vk::ImageLayout::VIDEO_ENCODE_DPB_KHR)
                    .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .image(self.dpb.image)
                    .subresource_range(vk::ImageSubresourceRange {
                        aspect_mask: vk::ImageAspectFlags::COLOR,
                        base_mip_level: 0,
                        level_count: 1,
                        base_array_layer: 0,
                        layer_count: DPB_SLOTS,
                    }),
            );
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

        // **The picture decides its own kind.** A refresh when asked for and
        // whenever there is nothing to predict from, which covers the first
        // picture; predicted otherwise. Nothing here emits an unrequested
        // refresh: the frame gate's recovery depends on a keyframe meaning
        // somebody asked for one.
        let idr = force_keyframe || self.previous.is_none();
        if idr {
            self.frames_since_idr = 0;
        }
        // The wire field wraps at the size the sequence set names; the order
        // count is type 2, derived from the unwrapped counter, so keeping the
        // counter continuous is exactly the codec's own derivation.
        let frame_num = self.frames_since_idr % (1 << 8);
        // **One codec counts its order in twos, the other in ones.** The first
        // derives the count from a frame number that advances by two a
        // picture; the second carries the count itself, and a reference one
        // picture back has to read as one step back.
        #[allow(
            clippy::cast_possible_wrap,
            reason = "wraps after two to the thirty-one pictures, which the codec's own \
                      arithmetic tolerates"
        )]
        let poc = match self.codec {
            Codec::H264 => self.frames_since_idr.wrapping_mul(2) as i32,
            Codec::H265 => self.frames_since_idr as i32,
        };
        let previous = if idr { None } else { self.previous };

        let recon_resource = vk::VideoPictureResourceInfoKHR::default()
            .coded_offset(vk::Offset2D { x: 0, y: 0 })
            .coded_extent(self.extent)
            .base_array_layer(u32::try_from(self.recon).unwrap_or(0))
            .image_view_binding(self.dpb.view);
        let previous_resource = previous.map(|reference| {
            vk::VideoPictureResourceInfoKHR::default()
                .coded_offset(vk::Offset2D { x: 0, y: 0 })
                .coded_extent(self.extent)
                .base_array_layer(u32::try_from(reference.slot).unwrap_or(0))
                .image_view_binding(self.dpb.view)
        });

        // **Both codecs' structures stand, and one of each pair is chained.**
        // A chain is a list of pointers into structures that have to outlive
        // the call, so the pair is declared here and the match below decides
        // which one the driver is handed. The alternative -- one recording per
        // codec -- is two control flows to keep identical, and they would not
        // stay identical.
        //
        // The slot being activated carries the codec's description of what
        // will stand in it, exactly as a bound reference's does: the scope
        // marks it inactive with index -1 and chains the description all the
        // same.
        // SAFETY: plain data from the codec headers; all-zero is valid and the
        // fields that matter are written below. The same holds for every
        // codec structure in this function.
        let mut setup_open_std_h264: ash::vk::native::StdVideoEncodeH264ReferenceInfo =
            unsafe { core::mem::zeroed() };
        setup_open_std_h264.primary_pic_type = if idr {
            ash::vk::native::StdVideoH264PictureType_STD_VIDEO_H264_PICTURE_TYPE_IDR
        } else {
            ash::vk::native::StdVideoH264PictureType_STD_VIDEO_H264_PICTURE_TYPE_P
        };
        setup_open_std_h264.FrameNum = frame_num;
        setup_open_std_h264.PicOrderCnt = poc;
        // SAFETY: as above.
        let mut setup_open_std_h265: ash::vk::native::StdVideoEncodeH265ReferenceInfo =
            unsafe { core::mem::zeroed() };
        setup_open_std_h265.pic_type = if idr {
            ash::vk::native::StdVideoH265PictureType_STD_VIDEO_H265_PICTURE_TYPE_IDR
        } else {
            ash::vk::native::StdVideoH265PictureType_STD_VIDEO_H265_PICTURE_TYPE_P
        };
        setup_open_std_h265.PicOrderCntVal = poc;
        let mut setup_open_h264 =
            vk::VideoEncodeH264DpbSlotInfoKHR::default().std_reference_info(&setup_open_std_h264);
        let mut setup_open_h265 =
            vk::VideoEncodeH265DpbSlotInfoKHR::default().std_reference_info(&setup_open_std_h265);
        let opening_setup = vk::VideoReferenceSlotInfoKHR::default()
            .slot_index(-1)
            .picture_resource(&recon_resource);
        let opening_setup = match self.codec {
            Codec::H264 => opening_setup.push_next(&mut setup_open_h264),
            Codec::H265 => opening_setup.push_next(&mut setup_open_h265),
        };

        // The scope names every slot it touches: the one being written, not
        // yet active so index -1, and for a predicted picture the one being
        // read.
        // SAFETY: as above.
        let mut opening_std_h264: ash::vk::native::StdVideoEncodeH264ReferenceInfo =
            unsafe { core::mem::zeroed() };
        // SAFETY: as above.
        let mut opening_std_h265: ash::vk::native::StdVideoEncodeH265ReferenceInfo =
            unsafe { core::mem::zeroed() };
        let mut opening_h264 = vk::VideoEncodeH264DpbSlotInfoKHR::default();
        let mut opening_h265 = vk::VideoEncodeH265DpbSlotInfoKHR::default();
        let opening_refresh;
        let opening_predicted;
        let opening: &[vk::VideoReferenceSlotInfoKHR] = match (previous, &previous_resource) {
            (Some(reference), Some(resource)) => {
                // The bound slot carries the codec's description of what
                // stands in it, exactly as the encode command's does.
                opening_std_h264.primary_pic_type = if reference.idr {
                    ash::vk::native::StdVideoH264PictureType_STD_VIDEO_H264_PICTURE_TYPE_IDR
                } else {
                    ash::vk::native::StdVideoH264PictureType_STD_VIDEO_H264_PICTURE_TYPE_P
                };
                opening_std_h264.FrameNum = reference.frame_num;
                opening_std_h264.PicOrderCnt = reference.poc;
                opening_std_h265.pic_type = if reference.idr {
                    ash::vk::native::StdVideoH265PictureType_STD_VIDEO_H265_PICTURE_TYPE_IDR
                } else {
                    ash::vk::native::StdVideoH265PictureType_STD_VIDEO_H265_PICTURE_TYPE_P
                };
                opening_std_h265.PicOrderCntVal = reference.poc;
                opening_h264 = opening_h264.std_reference_info(&opening_std_h264);
                opening_h265 = opening_h265.std_reference_info(&opening_std_h265);
                let bound = vk::VideoReferenceSlotInfoKHR::default()
                    .slot_index(i32::try_from(reference.slot).unwrap_or(0))
                    .picture_resource(resource);
                let bound = match self.codec {
                    Codec::H264 => bound.push_next(&mut opening_h264),
                    Codec::H265 => bound.push_next(&mut opening_h265),
                };
                opening_predicted = [opening_setup, bound];
                &opening_predicted
            }
            _ => {
                opening_refresh = [opening_setup];
                &opening_refresh
            }
        };

        // **The codec's half of the layer, which is not optional.** One vendor
        // accepts a layer without it and the other refuses the whole recording,
        // at the end of the buffer rather than at the command.
        let mut layer_h264 = vk::VideoEncodeH264RateControlLayerInfoKHR::default();
        let mut layer_h265 = vk::VideoEncodeH265RateControlLayerInfoKHR::default();
        let layer = vk::VideoEncodeRateControlLayerInfoKHR::default()
            .average_bitrate(u64::from(self.bitrate_bps))
            .max_bitrate(u64::from(self.bitrate_bps))
            .frame_rate_numerator(self.fps)
            .frame_rate_denominator(1);
        let layers = [match self.codec {
            Codec::H264 => layer.push_next(&mut layer_h264),
            Codec::H265 => layer.push_next(&mut layer_h265),
        }];
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
        let mut rate_h264 = vk::VideoEncodeH264RateControlInfoKHR::default()
            .gop_frame_count(self.fps)
            .idr_period(self.fps)
            .consecutive_b_frame_count(0)
            .temporal_layer_count(1);
        let mut rate_h265 = vk::VideoEncodeH265RateControlInfoKHR::default()
            .gop_frame_count(self.fps)
            .idr_period(self.fps)
            .consecutive_b_frame_count(0)
            .sub_layer_count(1);
        // **Its own structures, not the control's.** A chain is a list of
        // pointers, so pushing one structure onto two of them makes a shape
        // neither call can read; it reports as a duplicate kind rather than as
        // a cycle.
        let mut applied_h264 = vk::VideoEncodeH264RateControlLayerInfoKHR::default();
        let mut applied_h265 = vk::VideoEncodeH265RateControlLayerInfoKHR::default();
        let applied_layer = vk::VideoEncodeRateControlLayerInfoKHR::default()
            .average_bitrate(u64::from(self.applied_bps))
            .max_bitrate(u64::from(self.applied_bps))
            .frame_rate_numerator(self.fps)
            .frame_rate_denominator(1);
        let applied = [match self.codec {
            Codec::H264 => applied_layer.push_next(&mut applied_h264),
            Codec::H265 => applied_layer.push_next(&mut applied_h265),
        }];
        let mut begin_rate =
            vk::VideoEncodeRateControlInfoKHR::default().rate_control_mode(self.mode);
        if self.mode == vk::VideoEncodeRateControlModeFlagsKHR::CBR {
            begin_rate = begin_rate
                .layers(&applied)
                .virtual_buffer_size_in_ms(1000)
                .initial_virtual_buffer_size_in_ms(0);
        }
        let mut begin_h264 = vk::VideoEncodeH264RateControlInfoKHR::default()
            .gop_frame_count(self.fps)
            .idr_period(self.fps)
            .consecutive_b_frame_count(0)
            .temporal_layer_count(1);
        let mut begin_h265 = vk::VideoEncodeH265RateControlInfoKHR::default()
            .gop_frame_count(self.fps)
            .idr_period(self.fps)
            .consecutive_b_frame_count(0)
            .sub_layer_count(1);
        let mut begin_coding = vk::VideoBeginCodingInfoKHR::default()
            .video_session(self.session)
            .video_session_parameters(self.parameters)
            .reference_slots(opening);
        // **Only once the session has been told something.** An opening must
        // carry the configuration the session is actually in, and before the
        // first control command that is the default -- so naming a rate here
        // describes a session that does not exist yet.
        if self.mode != vk::VideoEncodeRateControlModeFlagsKHR::DEFAULT {
            begin_coding = begin_coding.push_next(&mut begin_rate);
            begin_coding = match self.codec {
                Codec::H264 => begin_coding.push_next(&mut begin_h264),
                Codec::H265 => begin_coding.push_next(&mut begin_h265),
            };
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
            .push_next(&mut rate);
        control = match self.codec {
            Codec::H264 => control.push_next(&mut rate_h264),
            Codec::H265 => control.push_next(&mut rate_h265),
        };
        if !self.started {
            control = control.push_next(&mut quality);
        }
        // SAFETY: recording; the chain outlives the call.
        unsafe { (self.device.video.fp().cmd_control_video_coding_khr)(self.buffer, &control) };

        // SAFETY: recording; every structure is a stack value that outlives
        // the call, and the codec structures are plain data.
        unsafe {
            let mut slice_h264: ash::vk::native::StdVideoEncodeH264SliceHeader =
                core::mem::zeroed();
            slice_h264.slice_type = if idr {
                ash::vk::native::StdVideoH264SliceType_STD_VIDEO_H264_SLICE_TYPE_I
            } else {
                ash::vk::native::StdVideoH264SliceType_STD_VIDEO_H264_SLICE_TYPE_P
            };
            let mut slice_h265: ash::vk::native::StdVideoEncodeH265SliceSegmentHeader =
                core::mem::zeroed();
            slice_h265.slice_type = if idr {
                ash::vk::native::StdVideoH265SliceType_STD_VIDEO_H265_SLICE_TYPE_I
            } else {
                ash::vk::native::StdVideoH265SliceType_STD_VIDEO_H265_SLICE_TYPE_P
            };
            slice_h265.slice_segment_address = 0;
            // The most the merge list may hold, which is the codec's own
            // maximum; a smaller number is written as a subtraction from it.
            slice_h265.MaxNumMergeCand = 5;
            slice_h265.flags.set_first_slice_segment_in_pic_flag(1);
            // **The sequence set turned the offset filter on**, and a slice
            // that does not say it applies leaves it off for its own picture.
            slice_h265.flags.set_slice_sao_luma_flag(1);
            slice_h265.flags.set_slice_sao_chroma_flag(1);
            slice_h265
                .flags
                .set_slice_loop_filter_across_slices_enabled_flag(1);

            // **Zero, because the rate is set.** A fixed quantiser and a
            // bitrate are two ways of saying the same thing and a device takes
            // only one; naming both is refused, and on one vendor the refusal
            // arrives as the whole recording failing to end.
            let quantiser = if rated { 0 } else { 26 };
            let slices_h264 = [vk::VideoEncodeH264NaluSliceInfoKHR::default()
                .constant_qp(quantiser)
                .std_slice_header(&slice_h264)];
            let slices_h265 = [vk::VideoEncodeH265NaluSliceSegmentInfoKHR::default()
                .constant_qp(quantiser)
                .std_slice_segment_header(&slice_h265)];

            // A predicted picture names its one reference in list zero;
            // every other entry is the no-reference sentinel.
            let mut lists_h264: ash::vk::native::StdVideoEncodeH264ReferenceListsInfo =
                core::mem::zeroed();
            lists_h264.RefPicList0 = [0xff; 32];
            lists_h264.RefPicList1 = [0xff; 32];
            let mut lists_h265: ash::vk::native::StdVideoEncodeH265ReferenceListsInfo =
                core::mem::zeroed();
            lists_h265.RefPicList0 = [0xff; 15];
            lists_h265.RefPicList1 = [0xff; 15];
            if let Some(reference) = previous {
                let slot = u8::try_from(reference.slot).unwrap_or(0);
                lists_h264.RefPicList0[0] = slot;
                lists_h265.RefPicList0[0] = slot;
            }

            // **The set of pictures this one may predict from, carried by the
            // picture rather than stored in the sequence set.** One reference,
            // one picture back: the sequence set declares no stored sets, so
            // there is nothing for an index to select and this is what says
            // what the reference is.
            let mut short_term: ash::vk::native::StdVideoH265ShortTermRefPicSet =
                core::mem::zeroed();
            short_term.num_negative_pics = 1;
            short_term.num_positive_pics = 0;
            short_term.delta_poc_s0_minus1[0] = 0;
            short_term.used_by_curr_pic_s0_flag = 1;

            let mut picture_h264: ash::vk::native::StdVideoEncodeH264PictureInfo =
                core::mem::zeroed();
            picture_h264.primary_pic_type = if idr {
                ash::vk::native::StdVideoH264PictureType_STD_VIDEO_H264_PICTURE_TYPE_IDR
            } else {
                ash::vk::native::StdVideoH264PictureType_STD_VIDEO_H264_PICTURE_TYPE_P
            };
            picture_h264.flags.set_IdrPicFlag(u32::from(idr));
            picture_h264.flags.set_is_reference(1);
            picture_h264.frame_num = frame_num;
            picture_h264.PicOrderCnt = poc;
            picture_h264.idr_pic_id = self.idr_id;
            if previous.is_some() {
                picture_h264.pRefLists = &raw const lists_h264;
            }

            let mut picture_h265: ash::vk::native::StdVideoEncodeH265PictureInfo =
                core::mem::zeroed();
            picture_h265.pic_type = if idr {
                ash::vk::native::StdVideoH265PictureType_STD_VIDEO_H265_PICTURE_TYPE_IDR
            } else {
                ash::vk::native::StdVideoH265PictureType_STD_VIDEO_H265_PICTURE_TYPE_P
            };
            picture_h265.PicOrderCntVal = poc;
            picture_h265.TemporalId = 0;
            picture_h265.sps_video_parameter_set_id = 0;
            picture_h265.pps_seq_parameter_set_id = 0;
            picture_h265.pps_pic_parameter_set_id = 0;
            picture_h265.flags.set_is_reference(1);
            picture_h265.flags.set_pic_output_flag(1);
            // **A refresh on this codec is a random-access point**, which is
            // the flag a decoder looks for to know it may start here.
            picture_h265.flags.set_IrapPicFlag(u32::from(idr));
            if previous.is_some() {
                picture_h265.pRefLists = &raw const lists_h265;
                picture_h265.pShortTermRefPicSet = &raw const short_term;
            }

            let mut encode_h264 = vk::VideoEncodeH264PictureInfoKHR::default()
                .nalu_slice_entries(&slices_h264)
                .std_picture_info(&picture_h264);
            let mut encode_h265 = vk::VideoEncodeH265PictureInfoKHR::default()
                .nalu_slice_segment_entries(&slices_h265)
                .std_picture_info(&picture_h265);

            // The slot being written carries the description a later picture
            // will predict from: this picture's own numbers.
            let mut setup_std_h264: ash::vk::native::StdVideoEncodeH264ReferenceInfo =
                core::mem::zeroed();
            setup_std_h264.primary_pic_type = picture_h264.primary_pic_type;
            setup_std_h264.FrameNum = frame_num;
            setup_std_h264.PicOrderCnt = poc;
            let mut setup_std_h265: ash::vk::native::StdVideoEncodeH265ReferenceInfo =
                core::mem::zeroed();
            setup_std_h265.pic_type = picture_h265.pic_type;
            setup_std_h265.PicOrderCntVal = poc;
            let mut setup_h264 =
                vk::VideoEncodeH264DpbSlotInfoKHR::default().std_reference_info(&setup_std_h264);
            let mut setup_h265 =
                vk::VideoEncodeH265DpbSlotInfoKHR::default().std_reference_info(&setup_std_h265);
            let setup = vk::VideoReferenceSlotInfoKHR::default()
                .slot_index(i32::try_from(self.recon).unwrap_or(0))
                .picture_resource(&recon_resource);
            let setup = match self.codec {
                Codec::H264 => setup.push_next(&mut setup_h264),
                Codec::H265 => setup.push_next(&mut setup_h265),
            };

            let source_resource = vk::VideoPictureResourceInfoKHR::default()
                .coded_offset(vk::Offset2D { x: 0, y: 0 })
                .coded_extent(self.extent)
                .base_array_layer(0)
                .image_view_binding(source_view);
            let mut info = vk::VideoEncodeInfoKHR::default()
                .dst_buffer(self.bitstream)
                .dst_buffer_offset(0)
                .dst_buffer_range(BITSTREAM_BYTES)
                .src_picture_resource(source_resource)
                .setup_reference_slot(&setup);
            info = match self.codec {
                Codec::H264 => info.push_next(&mut encode_h264),
                Codec::H265 => info.push_next(&mut encode_h265),
            };

            // The reference being read, described to the encode itself. The
            // scope named the slot; this is what tells the codec which
            // numbers stand in it.
            let mut previous_std_h264: ash::vk::native::StdVideoEncodeH264ReferenceInfo =
                core::mem::zeroed();
            let mut previous_std_h265: ash::vk::native::StdVideoEncodeH265ReferenceInfo =
                core::mem::zeroed();
            let mut previous_h264 = vk::VideoEncodeH264DpbSlotInfoKHR::default();
            let mut previous_h265 = vk::VideoEncodeH265DpbSlotInfoKHR::default();
            let active;
            if let (Some(reference), Some(resource)) = (previous, &previous_resource) {
                previous_std_h264.primary_pic_type = if reference.idr {
                    ash::vk::native::StdVideoH264PictureType_STD_VIDEO_H264_PICTURE_TYPE_IDR
                } else {
                    ash::vk::native::StdVideoH264PictureType_STD_VIDEO_H264_PICTURE_TYPE_P
                };
                previous_std_h264.FrameNum = reference.frame_num;
                previous_std_h264.PicOrderCnt = reference.poc;
                previous_std_h265.pic_type = if reference.idr {
                    ash::vk::native::StdVideoH265PictureType_STD_VIDEO_H265_PICTURE_TYPE_IDR
                } else {
                    ash::vk::native::StdVideoH265PictureType_STD_VIDEO_H265_PICTURE_TYPE_P
                };
                previous_std_h265.PicOrderCntVal = reference.poc;
                previous_h264 = previous_h264.std_reference_info(&previous_std_h264);
                previous_h265 = previous_h265.std_reference_info(&previous_std_h265);
                let bound = vk::VideoReferenceSlotInfoKHR::default()
                    .slot_index(i32::try_from(reference.slot).unwrap_or(0))
                    .picture_resource(resource);
                active = [match self.codec {
                    Codec::H264 => bound.push_next(&mut previous_h264),
                    Codec::H265 => bound.push_next(&mut previous_h265),
                }];
                info = info.reference_slots(&active);
            }
            device.cmd_begin_query(self.buffer, self.pool, 0, vk::QueryControlFlags::empty());
            (self.device.encode.fp().cmd_encode_video_khr)(self.buffer, &info);
            device.cmd_end_query(self.buffer, self.pool, 0);
        }

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
        // The picture just submitted becomes the reference, the other slot
        // becomes the next reconstruction, and the counters move.
        self.previous = Some(Reference {
            slot: self.recon,
            frame_num,
            poc,
            idr,
        });
        self.recon ^= 1;
        self.frames_since_idr = self.frames_since_idr.wrapping_add(1);
        if idr {
            self.idr_id = self.idr_id.wrapping_add(1);
        }
        self.submitted_keyframe = idr;
        self.in_flight = true;
        Ok(())
    }

    /// Ask for a finished picture without waiting for one.
    ///
    /// **Honest, unlike the vendor path.** A picture that is not finished is
    /// reported as not ready rather than as a length that is not there yet.
    pub fn poll(&mut self) -> Result<Poll<'_>> {
        let device = &self.device.device;
        // **Nothing in flight is nothing to report**, and the query would say
        // otherwise: it keeps the previous submission's result until the next
        // recording's reset executes, so an unguarded read answers with a
        // picture that was already collected.
        if !self.in_flight {
            return Ok(Poll::Pending);
        }
        // **The fence gates the query.** The reset that clears the previous
        // result rides inside the new recording, so a query read before the
        // fence fires reads the old submission's block -- instantly, with the
        // old bytes -- which is exactly the dishonest collect the vendor path
        // is documented for. The fence firing is what says the reset, the
        // encode and the query write have all executed.
        // SAFETY: the fence is this device's and was submitted with.
        let done = unsafe { device.get_fence_status(self.fence) }.map_err(driver)?;
        if !done {
            return Ok(Poll::Pending);
        }
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
        //
        // **A refresh opens with the parameter sets**, exactly as the other
        // backends' do: the encode itself produces slices only, and a decoder
        // that joins at a refresh without them decodes nothing -- it reports
        // a picture of no size at all.
        self.collected.clear();
        if self.submitted_keyframe {
            self.collected.extend_from_slice(&self.sets);
        }
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
        self.in_flight = false;
        Ok(Poll::Ready {
            bitstream: &self.collected,
            keyframe: self.submitted_keyframe,
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

impl crate::Encoder for Encoder<'_> {
    type Error = Error;

    /// The generator's path, which this backend does not carry: its pictures
    /// are written on the device by the conversion, and nothing here uploads
    /// bytes. The stream never pairs this backend with a source that
    /// delivers them.
    fn submit(
        &mut self,
        _frame: &lowlat_capture::Frame<'_>,
        _force_keyframe: bool,
    ) -> Result<()> {
        Err(Error::Unsupported("a frame delivered as bytes"))
    }

    fn poll(&mut self) -> Result<Poll<'_>> {
        Encoder::poll(self)
    }

    /// The rate is applied by the next picture's control command, which is
    /// also the only way one vendor here accepts it.
    fn reconfigure(&mut self, bitrate_bps: u32) -> Result<()> {
        self.set_bitrate(bitrate_bps);
        Ok(())
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
            for picture in self.sources.iter().chain([&self.dpb]) {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn extent(width: u32, height: u32) -> vk::Extent2D {
        vk::Extent2D { width, height }
    }

    /// **The defect this catches produced a picture whose bottom rows were
    /// wrong and whose every other row was right**, on a stream that decoded
    /// without a single error and reported the size that was asked for.
    #[test]
    fn the_coded_size_is_whole_units_of_what_the_device_accesses() {
        // The height a display of this size really has: a whole number of the
        // smallest coding block, and not of the sixteen rows this device
        // reads at a time.
        let coded = coded_size(extent(1920, 1080), extent(64, 16));
        assert_eq!(
            (coded.width, coded.height),
            (1920, 1088),
            "1080 is not a whole number of sixteen-row units"
        );
        // What the other codec on the same device asks for, where rounding to
        // the coding block alone would have been enough.
        let coded = coded_size(extent(1920, 1080), extent(16, 16));
        assert_eq!((coded.width, coded.height), (1920, 1088));
        // A size that needs both axes rounded.
        let coded = coded_size(extent(1916, 1076), extent(64, 16));
        assert_eq!((coded.width, coded.height), (1920, 1088));
        // One already whole is left alone.
        let coded = coded_size(extent(1280, 720), extent(64, 16));
        assert_eq!((coded.width, coded.height), (1280, 720));
    }

    /// A device reporting a granularity finer than the codec's own block must
    /// not talk this into declaring a size the codec forbids.
    #[test]
    fn the_coding_block_is_the_floor_whatever_the_device_reports() {
        let coded = coded_size(extent(1918, 1074), extent(1, 1));
        assert_eq!(
            (coded.width, coded.height),
            (1920, 1080),
            "the smallest coding block is eight, whatever the device accesses"
        );
    }
}
