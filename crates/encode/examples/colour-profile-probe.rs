//! What colour a device will encode, asked of the device rather than assumed.
//!
//!   colour-profile-probe            every device this machine has
//!   colour-profile-probe /dev/dri/card0
//!
//! **Three questions the shipped backend cannot currently ask.**
//! [`vulkan::Device::caps`] builds one profile -- eight bit, chroma at half
//! resolution in both directions -- so nothing here has ever been asked
//! whether it would encode anything else. Enabling ten-bit or 4:4:4 on this
//! backend starts with the answers below, and two of them are not obvious:
//!
//! 1. Is the profile supported at all? A codec extension being present says
//!    nothing about which depths or chroma layouts the device implements
//!    under it.
//! 2. What layout does a picture have to be in? The conversion writes that
//!    layout, so it decides whether one shader variant covers a depth or two
//!    are needed.
//! 3. **Can a shader still write the very picture the encoder reads?** This is
//!    `shared_picture`, and it is the whole reason this backend exists: false
//!    means a copy stands between the conversion and the encode. It is asked
//!    per profile, because a device may share an eight-bit picture and refuse
//!    a ten-bit one, and finding that out after building the ring is finding
//!    it out too late.
//!
//! Nothing here creates a logical device or encodes a picture. These are
//! physical-device queries, so a claim is all this can collect -- and a claim
//! is what decides whether the work is worth starting, not whether it works.

use std::path::{Path, PathBuf};

use ash::vk;

/// The codecs the shipped backend produces, which is what this asks about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Codec {
    H264,
    H265,
}

/// One colour a device might encode, named the way a profile names it.
struct Spec {
    label: &'static str,
    codec: Codec,
    /// The codec's own profile number. Both standards' identifiers are plain
    /// integers, so one field carries either.
    idc: u32,
    chroma: vk::VideoChromaSubsamplingFlagsKHR,
    depth: vk::VideoComponentBitDepthFlagsKHR,
}

/// Every colour worth asking about, and why this is the whole list.
///
/// **H.264 has no ten-bit entry because the standard header cannot express
/// one.** `StdVideoH264ProfileIdc` offers baseline, main, high and high 4:4:4
/// predictive -- there is no High 10 -- so a ten-bit H.264 encode is not
/// merely unsupported on some device, it is unsayable through this interface.
/// That is worth seeing as an absence rather than as a row that fails.
const SPECS: &[Spec] = &[
    Spec {
        label: "H.264  4:2:0  8-bit",
        codec: Codec::H264,
        idc: ash::vk::native::StdVideoH264ProfileIdc_STD_VIDEO_H264_PROFILE_IDC_HIGH,
        chroma: vk::VideoChromaSubsamplingFlagsKHR::TYPE_420,
        depth: vk::VideoComponentBitDepthFlagsKHR::TYPE_8,
    },
    Spec {
        label: "H.264  4:4:4  8-bit",
        codec: Codec::H264,
        idc: ash::vk::native::StdVideoH264ProfileIdc_STD_VIDEO_H264_PROFILE_IDC_HIGH_444_PREDICTIVE,
        chroma: vk::VideoChromaSubsamplingFlagsKHR::TYPE_444,
        depth: vk::VideoComponentBitDepthFlagsKHR::TYPE_8,
    },
    Spec {
        label: "HEVC   4:2:0  8-bit",
        codec: Codec::H265,
        idc: ash::vk::native::StdVideoH265ProfileIdc_STD_VIDEO_H265_PROFILE_IDC_MAIN,
        chroma: vk::VideoChromaSubsamplingFlagsKHR::TYPE_420,
        depth: vk::VideoComponentBitDepthFlagsKHR::TYPE_8,
    },
    Spec {
        label: "HEVC   4:2:0 10-bit",
        codec: Codec::H265,
        idc: ash::vk::native::StdVideoH265ProfileIdc_STD_VIDEO_H265_PROFILE_IDC_MAIN_10,
        chroma: vk::VideoChromaSubsamplingFlagsKHR::TYPE_420,
        depth: vk::VideoComponentBitDepthFlagsKHR::TYPE_10,
    },
    Spec {
        label: "HEVC   4:4:4  8-bit",
        codec: Codec::H265,
        idc: ash::vk::native::StdVideoH265ProfileIdc_STD_VIDEO_H265_PROFILE_IDC_FORMAT_RANGE_EXTENSIONS,
        chroma: vk::VideoChromaSubsamplingFlagsKHR::TYPE_444,
        depth: vk::VideoComponentBitDepthFlagsKHR::TYPE_8,
    },
    Spec {
        label: "HEVC   4:4:4 10-bit",
        codec: Codec::H265,
        idc: ash::vk::native::StdVideoH265ProfileIdc_STD_VIDEO_H265_PROFILE_IDC_FORMAT_RANGE_EXTENSIONS,
        chroma: vk::VideoChromaSubsamplingFlagsKHR::TYPE_444,
        depth: vk::VideoComponentBitDepthFlagsKHR::TYPE_10,
    },
];

fn main() {
    let node = std::env::args().nth(1).map(PathBuf::from);
    if let Err(error) = probe(node.as_deref()) {
        eprintln!("{error}");
        std::process::exit(2);
    }
}

/// A device node's major and minor numbers.
fn node_numbers(path: &Path) -> Option<(u32, u32)> {
    use std::os::unix::fs::MetadataExt;
    let rdev = std::fs::metadata(path).ok()?.rdev();
    let major = u32::try_from(((rdev >> 8) & 0xfff) | ((rdev >> 32) & !0xfff)).ok()?;
    let minor = u32::try_from((rdev & 0xff) | ((rdev >> 12) & !0xff)).ok()?;
    Some((major, minor))
}

fn probe(node: Option<&Path>) -> Result<(), String> {
    let wanted = match node {
        Some(path) => Some(node_numbers(path).ok_or("that node cannot be read")?),
        None => None,
    };

    // SAFETY: loads the system driver loader; the handle outlives everything
    // derived from it.
    let entry = unsafe { ash::Entry::load() }.map_err(|_| "no driver loader")?;
    let application = vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_3);
    let create = vk::InstanceCreateInfo::default().application_info(&application);
    // SAFETY: the create info outlives the call and names no extensions.
    let instance = unsafe { entry.create_instance(&create, None) }.map_err(|e| e.to_string())?;
    let video = ash::khr::video_queue::Instance::new(&entry, &instance);

    // SAFETY: enumerating from a live instance.
    let devices = unsafe { instance.enumerate_physical_devices() }.map_err(|e| e.to_string())?;
    let mut examined = 0_usize;
    for physical in devices {
        if let Some(wanted) = wanted
            && !drives_node(&instance, physical, wanted)
        {
            continue;
        }
        examined += 1;
        report(&instance, &video, physical);
    }

    if examined == 0 {
        return Err(match node {
            Some(path) => format!("no Vulkan device drives {}", path.display()),
            None => "this machine offers no Vulkan device".to_owned(),
        });
    }
    Ok(())
}

/// Whether this device is the one behind a node, matched on the numbers the
/// driver reports rather than on a name or an index.
fn drives_node(instance: &ash::Instance, physical: vk::PhysicalDevice, wanted: (u32, u32)) -> bool {
    let mut drm = vk::PhysicalDeviceDrmPropertiesEXT::default();
    let mut properties = vk::PhysicalDeviceProperties2::default().push_next(&mut drm);
    // SAFETY: the chain outlives the call and the device came from this
    // instance.
    unsafe { instance.get_physical_device_properties2(physical, &mut properties) };
    let primary = (
        u32::try_from(drm.primary_major).unwrap_or(u32::MAX),
        u32::try_from(drm.primary_minor).unwrap_or(u32::MAX),
    );
    let render = (
        u32::try_from(drm.render_major).unwrap_or(u32::MAX),
        u32::try_from(drm.render_minor).unwrap_or(u32::MAX),
    );
    primary == wanted || render == wanted
}

fn report(
    instance: &ash::Instance,
    video: &ash::khr::video_queue::Instance,
    physical: vk::PhysicalDevice,
) {
    // SAFETY: the device came from this instance.
    let properties = unsafe { instance.get_physical_device_properties(physical) };
    let name = properties
        .device_name_as_c_str()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    println!("{name}");

    // SAFETY: the device came from this instance.
    let available =
        unsafe { instance.enumerate_device_extension_properties(physical) }.unwrap_or_default();
    let has = |wanted: &std::ffi::CStr| {
        available.iter().any(|entry| {
            entry
                .extension_name_as_c_str()
                .is_ok_and(|name| name == wanted)
        })
    };
    if !has(ash::khr::video_queue::NAME) || !has(ash::khr::video_encode_queue::NAME) {
        println!("  no video encode queue extension; nothing to ask\n");
        return;
    }
    let codecs = (
        has(ash::khr::video_encode_h264::NAME),
        has(ash::khr::video_encode_h265::NAME),
    );
    println!(
        "  encode extensions: h264 {} h265 {}",
        yes_no(codecs.0),
        yes_no(codecs.1)
    );

    for spec in SPECS {
        // **A profile whose codec extension is absent is not a refusal.**
        // Asking anyway would report "unsupported" for a device that was never
        // asked, which reads as a capability answer and is not one.
        let present = match spec.codec {
            Codec::H264 => codecs.0,
            Codec::H265 => codecs.1,
        };
        if !present {
            println!("  {}  -- codec extension absent", spec.label);
            continue;
        }
        match capabilities(video, physical, spec) {
            Err(result) => println!("  {}  no      {result:?}", spec.label),
            Ok(granularity) => {
                let picture = first_format(
                    video,
                    physical,
                    spec,
                    vk::ImageUsageFlags::VIDEO_ENCODE_SRC_KHR | vk::ImageUsageFlags::TRANSFER_DST,
                );
                // The question this probe exists for: a picture a shader may
                // write is a picture the conversion writes in place.
                let shared = first_format(
                    video,
                    physical,
                    spec,
                    vk::ImageUsageFlags::VIDEO_ENCODE_SRC_KHR | vk::ImageUsageFlags::STORAGE,
                );
                let shared = match (&picture, &shared) {
                    (Some(picture), Some(shared)) => yes_no(picture == shared),
                    _ => "no",
                };
                println!(
                    "  {}  YES     picture {:?}  shader-writable {}  granularity {}x{}",
                    spec.label,
                    picture.map_or(vk::Format::UNDEFINED, |format| format),
                    shared,
                    granularity.width,
                    granularity.height,
                );
            }
        }
    }
    println!();
}

/// Build this colour as one profile chain and hand it to a caller.
///
/// **Built inside a call rather than returned.** Each structure borrows the one
/// it is pushed onto for as long as that one lives, so the chain cannot outlive
/// the frame that made it.
fn with_profile<R>(spec: &Spec, f: impl FnOnce(&vk::VideoProfileInfoKHR<'_>) -> R) -> R {
    let base = vk::VideoProfileInfoKHR::default()
        .chroma_subsampling(spec.chroma)
        .luma_bit_depth(spec.depth)
        .chroma_bit_depth(spec.depth);
    match spec.codec {
        Codec::H264 => {
            let mut h264 = vk::VideoEncodeH264ProfileInfoKHR::default().std_profile_idc(spec.idc);
            let profile = base
                .video_codec_operation(vk::VideoCodecOperationFlagsKHR::ENCODE_H264)
                .push_next(&mut h264);
            f(&profile)
        }
        Codec::H265 => {
            let mut h265 = vk::VideoEncodeH265ProfileInfoKHR::default().std_profile_idc(spec.idc);
            let profile = base
                .video_codec_operation(vk::VideoCodecOperationFlagsKHR::ENCODE_H265)
                .push_next(&mut h265);
            f(&profile)
        }
    }
}

/// Whether the device encodes this colour, and at what granularity.
fn capabilities(
    video: &ash::khr::video_queue::Instance,
    physical: vk::PhysicalDevice,
    spec: &Spec,
) -> Result<vk::Extent2D, vk::Result> {
    // **The codec's own capabilities have to be in the chain.** Asking about
    // an encode without them is invalid, and a driver answers anyway with a
    // structure it never filled.
    let mut encode = vk::VideoEncodeCapabilitiesKHR::default();
    let mut h264 = vk::VideoEncodeH264CapabilitiesKHR::default();
    let mut h265 = vk::VideoEncodeH265CapabilitiesKHR::default();
    let mut caps = vk::VideoCapabilitiesKHR::default().push_next(&mut encode);
    caps = match spec.codec {
        Codec::H264 => caps.push_next(&mut h264),
        Codec::H265 => caps.push_next(&mut h265),
    };
    with_profile(spec, |profile| {
        // SAFETY: the chain outlives the call and the device came from this
        // instance.
        let result = unsafe {
            (video.fp().get_physical_device_video_capabilities_khr)(physical, profile, &mut caps)
        };
        if result == vk::Result::SUCCESS {
            Ok(caps.picture_access_granularity)
        } else {
            Err(result)
        }
    })
}

/// The first layout the device will take for one use of a picture in this
/// colour, or nothing if it will take none.
fn first_format(
    video: &ash::khr::video_queue::Instance,
    physical: vk::PhysicalDevice,
    spec: &Spec,
    usage: vk::ImageUsageFlags,
) -> Option<vk::Format> {
    with_profile(spec, |profile| {
        let profiles = [*profile];
        let mut list = vk::VideoProfileListInfoKHR::default().profiles(&profiles);
        let info = vk::PhysicalDeviceVideoFormatInfoKHR::default()
            .image_usage(usage)
            .push_next(&mut list);
        let mut count = 0_u32;
        // SAFETY: asking for the count writes only the counter.
        let result = unsafe {
            (video.fp().get_physical_device_video_format_properties_khr)(
                physical,
                &info,
                &raw mut count,
                core::ptr::null_mut(),
            )
        };
        if result != vk::Result::SUCCESS || count == 0 {
            return None;
        }
        let mut properties =
            vec![vk::VideoFormatPropertiesKHR::default(); usize::try_from(count).ok()?];
        // SAFETY: the destination holds `count` entries, which is what the
        // count query reported and what is passed back as the capacity.
        let result = unsafe {
            (video.fp().get_physical_device_video_format_properties_khr)(
                physical,
                &info,
                &raw mut count,
                properties.as_mut_ptr(),
            )
        };
        if result != vk::Result::SUCCESS {
            return None;
        }
        properties.first().map(|entry| entry.format)
    })
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}
