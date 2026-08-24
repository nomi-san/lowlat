//! What a device will do if the encode runs on the same interface as the
//! import and the conversion.
//!
//!   vulkan-encode-probe [/dev/dri/card0]
//!
//! **Three questions, in the order that decides whether this replaces
//! anything.** Each is one the two working backends answered badly or not at
//! all, and the first is fatal if the answer is no:
//!
//! 1. Does the bitrate change while a session runs, without rebuilding the
//!    encoder and without forcing a picture with no history? That is the only
//!    congestion actuator the design has.
//! 2. Can completion be asked about without blocking? One vendor path
//!    answers this dishonestly: its own no-wait flag is ignored by the driver.
//! 3. What does it cost? The figure to beat is a capture stage of 1.86 ms
//!    against a conversion that takes 0.39 ms on its own, the difference being
//!    two interfaces sharing one device.
//!
//! This stage answers what the device says about itself. Nothing here encodes
//! yet: a device that cannot reconfigure a bitrate is one this path cannot
//! use, and that is answerable before a single picture is submitted.

use std::path::{Path, PathBuf};

use ash::vk;

fn main() {
    let node = std::env::args()
        .nth(1)
        .map_or_else(|| PathBuf::from("/dev/dri/card0"), PathBuf::from);
    match probe(&node) {
        Ok(()) => {}
        Err(error) => {
            eprintln!("{}: {error}", node.display());
            std::process::exit(2);
        }
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

fn probe(node: &Path) -> Result<(), String> {
    let wanted = node_numbers(node).ok_or("that node cannot be read")?;

    // SAFETY: loads the system driver loader; the handle outlives everything
    // derived from it.
    let entry = unsafe { ash::Entry::load() }.map_err(|_| "no driver loader")?;
    let application = vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_3);
    let create = vk::InstanceCreateInfo::default().application_info(&application);
    // SAFETY: the create info outlives the call and names no extensions.
    let instance = unsafe { entry.create_instance(&create, None) }.map_err(|e| e.to_string())?;

    // SAFETY: enumerating from a live instance.
    let devices = unsafe { instance.enumerate_physical_devices() }.map_err(|e| e.to_string())?;
    for physical in devices {
        let mut drm = vk::PhysicalDeviceDrmPropertiesEXT::default();
        let mut properties = vk::PhysicalDeviceProperties2::default().push_next(&mut drm);
        // SAFETY: the chain is stack values that outlive the call.
        unsafe { instance.get_physical_device_properties2(physical, &mut properties) };
        // Taken out before the node is read, because the chain borrows the
        // node's own structure until its last use.
        let name = properties
            .properties
            .device_name_as_c_str()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        if drm.has_primary == 0
            || u32::try_from(drm.primary_major) != Ok(wanted.0)
            || u32::try_from(drm.primary_minor) != Ok(wanted.1)
        {
            continue;
        }
        println!("{}: {name}", node.display());
        report(&entry, &instance, physical)?;
        // SAFETY: nothing created from it outlives this.
        unsafe { instance.destroy_instance(None) };
        return Ok(());
    }
    // SAFETY: nothing created from it outlives this.
    unsafe { instance.destroy_instance(None) };
    Err("no device drives that node".to_string())
}

fn report(
    entry: &ash::Entry,
    instance: &ash::Instance,
    physical: vk::PhysicalDevice,
) -> Result<(), String> {
    // SAFETY: the device came from this instance.
    let available = unsafe { instance.enumerate_device_extension_properties(physical) }
        .map_err(|e| e.to_string())?;
    let has = |wanted: &std::ffi::CStr| {
        available
            .iter()
            .any(|entry| entry.extension_name_as_c_str() == Ok(wanted))
    };
    let required = [
        ash::khr::video_queue::NAME,
        ash::khr::video_encode_queue::NAME,
        ash::khr::video_encode_h264::NAME,
    ];
    for name in required {
        println!(
            "  {:<34} {}",
            name.to_string_lossy(),
            if has(name) { "yes" } else { "MISSING" }
        );
    }
    if !required.into_iter().all(has) {
        return Err("this device cannot encode on this interface".to_string());
    }

    // **The queue is the point.** A dedicated encode family is what lets a
    // conversion and an encode reach one device without changing interface
    // between them, which is the cost this path exists to remove.
    // SAFETY: the device came from this instance.
    let families = unsafe { instance.get_physical_device_queue_family_properties(physical) };
    for (at, family) in families.iter().enumerate() {
        let flags = family.queue_flags;
        if flags.contains(vk::QueueFlags::VIDEO_ENCODE_KHR) {
            println!(
                "  encode queue family {at}, {} queue(s){}",
                family.queue_count,
                if flags.contains(vk::QueueFlags::COMPUTE) {
                    ", also compute"
                } else {
                    ", encode only"
                }
            );
        }
    }

    capabilities(entry, instance, physical)
}

/// What the device says it can do, before anything is built on it.
///
/// **The rate control answer is the one that matters.** A device that cannot
/// be told a new bitrate mid-session forces the alternative the rules forbid:
/// rebuilding the encoder, which costs a picture with no history and a visible
/// hitch every time congestion moves.
fn capabilities(
    entry: &ash::Entry,
    instance: &ash::Instance,
    physical: vk::PhysicalDevice,
) -> Result<(), String> {
    let video = ash::khr::video_queue::Instance::new(entry, instance);

    // The picture this path would actually carry: eight bit, chroma at half
    // resolution in both directions, which is what every peer decodes.
    let mut h264 = vk::VideoEncodeH264ProfileInfoKHR::default()
        .std_profile_idc(ash::vk::native::StdVideoH264ProfileIdc_STD_VIDEO_H264_PROFILE_IDC_HIGH);
    let profile = vk::VideoProfileInfoKHR::default()
        .video_codec_operation(vk::VideoCodecOperationFlagsKHR::ENCODE_H264)
        .chroma_subsampling(vk::VideoChromaSubsamplingFlagsKHR::TYPE_420)
        .luma_bit_depth(vk::VideoComponentBitDepthFlagsKHR::TYPE_8)
        .chroma_bit_depth(vk::VideoComponentBitDepthFlagsKHR::TYPE_8)
        .push_next(&mut h264);

    let mut encode = vk::VideoEncodeCapabilitiesKHR::default();
    let mut h264_caps = vk::VideoEncodeH264CapabilitiesKHR::default();
    let mut caps = vk::VideoCapabilitiesKHR::default()
        .push_next(&mut encode)
        .push_next(&mut h264_caps);
    // **Called through the raw pointer**, because this interface arrives as
    // function pointers and no wrapper: the surface is young enough that the
    // binding has not grown safe forms for it yet.
    // SAFETY: the chains are stack values that outlive the call, the device
    // came from this instance, and both out structures are correctly typed.
    let result = unsafe {
        (video.fp().get_physical_device_video_capabilities_khr)(physical, &profile, &mut caps)
    };
    if result != vk::Result::SUCCESS {
        return Err(format!(
            "this device will not describe an H.264 encode: {result:?}"
        ));
    }

    // **Copied out before anything else is read.** The capability chain
    // borrows each structure it was given until its own last use, so the
    // outer one is emptied first and the extensions read after it.
    let (min, max) = (caps.min_coded_extent, caps.max_coded_extent);
    let (slots, active) = (caps.max_dpb_slots, caps.max_active_reference_pictures);
    let separate = caps
        .flags
        .contains(vk::VideoCapabilityFlagsKHR::SEPARATE_REFERENCE_IMAGES);

    println!(
        "  picture {}x{} to {}x{}, {slots} reference slot(s), {active} active",
        min.width, min.height, max.width, max.height
    );
    println!(
        "  reference pictures: {}",
        if separate {
            "one allocation each"
        } else {
            "all in one allocation"
        }
    );

    // **Question one, and it is the one that can end this.** Anything other
    // than a mode that takes a bitrate means the only congestion actuator the
    // design has cannot be built on this path.
    let modes = encode.rate_control_modes;
    let mut named: Vec<&str> = Vec::new();
    for (flag, what) in [
        (vk::VideoEncodeRateControlModeFlagsKHR::CBR, "CBR"),
        (vk::VideoEncodeRateControlModeFlagsKHR::VBR, "VBR"),
        (
            vk::VideoEncodeRateControlModeFlagsKHR::DISABLED,
            "may be turned off",
        ),
    ] {
        if modes.contains(flag) {
            named.push(what);
        }
    }
    if named.is_empty() {
        named.push("NONE");
    }
    println!("  rate control: {}", named.join(", "));
    println!(
        "  ceiling {} Mbit/s over {} layer(s), {} quality level(s)",
        encode.max_bitrate / 1_000_000,
        encode.max_rate_control_layers,
        encode.max_quality_levels
    );

    // **Question two.** The written length has to come back without reading
    // the buffer, or a caller cannot tell a finished picture from an empty one
    // except by blocking on it -- which is exactly how the vendor path lies.
    let feedback = encode.supported_encode_feedback_flags;
    println!(
        "  feedback: offset={} bytes-written={}",
        feedback.contains(vk::VideoEncodeFeedbackFlagsKHR::BITSTREAM_BUFFER_OFFSET),
        feedback.contains(vk::VideoEncodeFeedbackFlagsKHR::BITSTREAM_BYTES_WRITTEN)
    );

    Ok(())
}
