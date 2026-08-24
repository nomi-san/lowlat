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
//! What a device says about itself is a claim. A session is what turns the
//! first two into answers: a mode can be advertised and only honoured at
//! creation, and a completion query can be offered and still block.

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
    let mut encode_family = None;
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
            encode_family = encode_family.or_else(|| u32::try_from(at).ok());
        }
    }
    let Some(encode_family) = encode_family else {
        return Err("this device exposes no encode queue".to_string());
    };

    capabilities(entry, instance, physical, encode_family)
}

/// What the device says it can do, before anything is built on it.
///
/// **The rate control answer is the one that matters.** A device that cannot
/// be told a new bitrate mid-session forces the alternative the rules forbid:
/// rebuilding the encoder, which costs a picture with no history and a visible
/// hitch every time congestion moves.
/// The picture this path would carry, as one chain.
///
/// **Built inside a call rather than returned.** Every structure here borrows
/// the one it is pushed onto for as long as that one lives, so the chain
/// cannot outlive the frame that made it and is handed to a closure instead.
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

/// Which layouts the device will take for one use of a video picture.
fn formats(
    video: &ash::khr::video_queue::Instance,
    physical: vk::PhysicalDevice,
    usage: vk::ImageUsageFlags,
) -> Result<Vec<vk::Format>, String> {
    with_profile(|profile| {
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
        if result != vk::Result::SUCCESS {
            return Err(format!("no layouts for that use: {result:?}"));
        }
        let mut properties =
            vec![vk::VideoFormatPropertiesKHR::default(); usize::try_from(count).unwrap_or(0)];
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
            return Err(format!("no layouts for that use: {result:?}"));
        }
        Ok(properties.into_iter().map(|entry| entry.format).collect())
    })
}

fn capabilities(
    entry: &ash::Entry,
    instance: &ash::Instance,
    physical: vk::PhysicalDevice,
    family: u32,
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
    // Taken here with the rest: a session is built against the exact codec
    // header revision the device names, and asking later would keep the outer
    // structure alive across every read of the extensions hanging off it.
    let header = caps.std_header_version;

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

    // **What the pictures have to be laid out as.** A session is built for one
    // layout for the source and one for the references, and the two are asked
    // for separately because a device is free to want different ones.
    let source = formats(&video, physical, vk::ImageUsageFlags::VIDEO_ENCODE_SRC_KHR)?;
    let dpb = formats(&video, physical, vk::ImageUsageFlags::VIDEO_ENCODE_DPB_KHR)?;
    println!("  source layout(s): {source:?}");
    println!("  reference layout(s): {dpb:?}");

    let (Some(picture), Some(reference)) = (source.first().copied(), dpb.first().copied()) else {
        return Err("this device names no layout for a picture it would encode".to_string());
    };
    session(
        entry,
        instance,
        physical,
        Built {
            physical,
            family,
            picture,
            reference,
            slots,
            active,
            header,
        },
    )?;

    Ok(())
}

/// What the device said, gathered so a session can be asked for.
struct Built {
    physical: vk::PhysicalDevice,
    family: u32,
    picture: vk::Format,
    reference: vk::Format,
    slots: u32,
    active: u32,
    header: vk::ExtensionProperties,
}

/// Open a device on the encode queue and build a session on it.
///
/// **The session is where a claim becomes an answer.** Everything above this
/// is the device describing itself; from here it either does the thing or
/// refuses.
fn session(
    entry: &ash::Entry,
    instance: &ash::Instance,
    physical: vk::PhysicalDevice,
    built: Built,
) -> Result<(), String> {
    let priorities = [1.0_f32];
    let queues = [vk::DeviceQueueCreateInfo::default()
        .queue_family_index(built.family)
        .queue_priorities(&priorities)];
    let names = [
        ash::khr::video_queue::NAME.as_ptr(),
        ash::khr::video_encode_queue::NAME.as_ptr(),
        ash::khr::video_encode_h264::NAME.as_ptr(),
    ];
    // The video interface is specified against the newer synchronisation, so
    // it has to be turned on even though nothing here names it directly.
    let mut sync = vk::PhysicalDeviceVulkan13Features::default().synchronization2(true);
    let create = vk::DeviceCreateInfo::default()
        .queue_create_infos(&queues)
        .enabled_extension_names(&names)
        .push_next(&mut sync);
    // SAFETY: every borrowed slice outlives the call.
    let device = unsafe { instance.create_device(physical, &create, None) }
        .map_err(|e| format!("the encode queue could not be opened: {e}"))?;

    let outcome = build_session(instance, &device, &built);

    // SAFETY: nothing built on it outlives this, because `build_session`
    // releases whatever it made before returning.
    unsafe {
        let _ = device.device_wait_idle();
        device.destroy_device(None);
    }
    let _ = entry;
    outcome
}

/// Create the session, bind its memory, and give it back.
fn build_session(
    instance: &ash::Instance,
    device: &ash::Device,
    built: &Built,
) -> Result<(), String> {
    let video = ash::khr::video_queue::Device::new(instance, device);

    let session = with_profile(|profile| {
        let create = vk::VideoSessionCreateInfoKHR::default()
            .queue_family_index(built.family)
            .video_profile(profile)
            .picture_format(built.picture)
            // The size a session is built for is a ceiling, not the picture:
            // one built small refuses a larger frame later, and the display
            // this would capture changes size while it runs.
            .max_coded_extent(vk::Extent2D {
                width: 2560,
                height: 1440,
            })
            .reference_picture_format(built.reference)
            .max_dpb_slots(built.slots.min(2))
            .max_active_reference_pictures(built.active.min(1))
            .std_header_version(&built.header);
        let mut session = vk::VideoSessionKHR::null();
        // SAFETY: the chain outlives the call and the out handle is a live
        // local.
        let result = unsafe {
            (video.fp().create_video_session_khr)(
                device.handle(),
                &create,
                core::ptr::null(),
                &raw mut session,
            )
        };
        if result == vk::Result::SUCCESS {
            Ok(session)
        } else {
            Err(format!("a session could not be created: {result:?}"))
        }
    })?;
    println!("  session created");

    let outcome = bind_session(instance, built.physical, device, &video, session);
    // SAFETY: created above; whatever was bound to it is released by the
    // caller's device teardown.
    unsafe { (video.fp().destroy_video_session_khr)(device.handle(), session, core::ptr::null()) };
    outcome
}

/// A session has its own memory, in as many pieces as it asks for.
fn bind_session(
    instance: &ash::Instance,
    physical: vk::PhysicalDevice,
    device: &ash::Device,
    video: &ash::khr::video_queue::Device,
    session: vk::VideoSessionKHR,
) -> Result<(), String> {
    let mut count = 0_u32;
    // SAFETY: asking for the count writes only the counter.
    let result = unsafe {
        (video.fp().get_video_session_memory_requirements_khr)(
            device.handle(),
            session,
            &raw mut count,
            core::ptr::null_mut(),
        )
    };
    if result != vk::Result::SUCCESS {
        return Err(format!(
            "the session will not say what it needs: {result:?}"
        ));
    }
    let mut wanted =
        vec![vk::VideoSessionMemoryRequirementsKHR::default(); usize::try_from(count).unwrap_or(0)];
    // SAFETY: the destination holds `count` entries, as just reported.
    let result = unsafe {
        (video.fp().get_video_session_memory_requirements_khr)(
            device.handle(),
            session,
            &raw mut count,
            wanted.as_mut_ptr(),
        )
    };
    if result != vk::Result::SUCCESS {
        return Err(format!(
            "the session will not say what it needs: {result:?}"
        ));
    }
    println!("  session wants {count} allocation(s)");

    // **Bound before anything else, in as many pieces as it asked for.** The
    // count is the device's own and differs between the two here, so the
    // allocation walks the list rather than assuming one.
    let mut memories = Vec::with_capacity(wanted.len());
    let mut binds = Vec::with_capacity(wanted.len());
    for entry in &wanted {
        let index = memory_type(device, instance, physical, entry.memory_requirements)?;
        let allocate = vk::MemoryAllocateInfo::default()
            .allocation_size(entry.memory_requirements.size)
            .memory_type_index(index);
        // SAFETY: the allocate info outlives the call.
        let memory = unsafe { device.allocate_memory(&allocate, None) }
            .map_err(|e| format!("the session's memory could not be allocated: {e}"))?;
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
    let result = unsafe {
        (video.fp().bind_video_session_memory_khr)(
            device.handle(),
            session,
            u32::try_from(binds.len()).unwrap_or(0),
            binds.as_ptr(),
        )
    };
    let outcome = if result == vk::Result::SUCCESS {
        println!("  session memory bound");
        parameters(device, video, session)
    } else {
        Err(format!("the session's memory would not bind: {result:?}"))
    };

    for memory in memories {
        // SAFETY: nothing refers to it once the session is torn down by the
        // caller, and the wait there is what makes that true.
        unsafe { device.free_memory(memory, None) };
    }
    outcome
}

/// The first memory type that suits a requirement.
fn memory_type(
    _device: &ash::Device,
    instance: &ash::Instance,
    physical: vk::PhysicalDevice,
    wanted: vk::MemoryRequirements,
) -> Result<u32, String> {
    // SAFETY: the device came from this instance.
    let memory = unsafe { instance.get_physical_device_memory_properties(physical) };
    // **Preferred, not required.** A session's own memory is the driver's
    // business and the two devices here do not want the same thing: one takes
    // a single allocation the card can read fastest, the other asks for five
    // and refuses outright if that property is demanded of all of them.
    let usable = memory
        .memory_types
        .iter()
        .take(usize::try_from(memory.memory_type_count).unwrap_or(0))
        .enumerate()
        .filter(|(at, _)| wanted.memory_type_bits & (1 << at) != 0);
    let mut fallback = None;
    for (at, kind) in usable {
        if kind
            .property_flags
            .contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)
        {
            return u32::try_from(at).map_err(|_| "memory type out of range".to_string());
        }
        fallback = fallback.or(Some(at));
    }
    fallback
        .and_then(|at| u32::try_from(at).ok())
        .ok_or_else(|| "no memory type suits the session".to_string())
}

/// The parameter sets, and whether the driver will write them for us.
///
/// **Both working backends had to write these by hand**, and a hand-written
/// set encodes without error and decodes to nothing in more than one way. If
/// this interface emits them, that whole class of fault goes away.
fn parameters(
    device: &ash::Device,
    video: &ash::khr::video_queue::Device,
    session: vk::VideoSessionKHR,
) -> Result<(), String> {
    let (width, height) = (2560_u32, 1440_u32);
    // **Zeroed and then filled, because these come from the codec's own
    // headers and carry pointers**, so there is no derived empty value. Zero
    // is the right start: every optional table is absent as a null pointer and
    // every flag is off.
    // SAFETY: the structure is plain data from the codec headers with no
    // invariant beyond its layout; all-zero is a valid parameter set with no
    // optional tables attached, and every field that matters is set below.
    let mut sps: ash::vk::native::StdVideoH264SequenceParameterSet = unsafe { core::mem::zeroed() };
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
    sps.pic_width_in_mbs_minus1 = width.div_ceil(16) - 1;
    sps.pic_height_in_map_units_minus1 = height.div_ceil(16) - 1;
    sps.frame_crop_right_offset = (width.next_multiple_of(16) - width) / 2;
    sps.frame_crop_bottom_offset = (height.next_multiple_of(16) - height) / 2;
    sps.flags.set_frame_mbs_only_flag(1);
    sps.flags.set_direct_8x8_inference_flag(1);
    if sps.frame_crop_right_offset > 0 || sps.frame_crop_bottom_offset > 0 {
        sps.flags.set_frame_cropping_flag(1);
    }

    // SAFETY: as above; a zeroed picture parameter set names sequence zero and
    // attaches no scaling lists.
    let mut pps: ash::vk::native::StdVideoH264PictureParameterSet = unsafe { core::mem::zeroed() };
    pps.flags.set_entropy_coding_mode_flag(1);
    pps.flags.set_deblocking_filter_control_present_flag(1);

    let sps_list = [sps];
    let pps_list = [pps];
    let mut h264 = vk::VideoEncodeH264SessionParametersAddInfoKHR::default()
        .std_sp_ss(&sps_list)
        .std_pp_ss(&pps_list);
    let mut create_h264 = vk::VideoEncodeH264SessionParametersCreateInfoKHR::default()
        .max_std_sps_count(1)
        .max_std_pps_count(1)
        .parameters_add_info(&h264);
    let create = vk::VideoSessionParametersCreateInfoKHR::default()
        .video_session(session)
        .push_next(&mut create_h264);

    let mut handle = vk::VideoSessionParametersKHR::null();
    // SAFETY: the chain outlives the call and the out handle is a live local.
    let result = unsafe {
        (video.fp().create_video_session_parameters_khr)(
            device.handle(),
            &create,
            core::ptr::null(),
            &raw mut handle,
        )
    };
    if result != vk::Result::SUCCESS {
        return Err(format!("the parameter sets were refused: {result:?}"));
    }
    println!("  parameter sets accepted");
    let _ = &mut h264;

    // SAFETY: created above and used by nothing else.
    unsafe {
        (video.fp().destroy_video_session_parameters_khr)(
            device.handle(),
            handle,
            core::ptr::null(),
        );
    }
    Ok(())
}
