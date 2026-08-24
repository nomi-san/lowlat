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

    let outcome = bind_session(instance, built.physical, device, &video, session, built);
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
    built: &Built,
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
        parameters(instance, physical, device, video, session, built)
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
    instance: &ash::Instance,
    _physical: vk::PhysicalDevice,
    device: &ash::Device,
    video: &ash::khr::video_queue::Device,
    session: vk::VideoSessionKHR,
    built: &Built,
) -> Result<(), String> {
    let (width, height) = (2560_u32, 1440_u32);
    let physical = built.physical;
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

    let outcome = encode(instance, physical, device, video, session, handle, built);

    // SAFETY: created above and used by nothing else.
    unsafe {
        (video.fp().destroy_video_session_parameters_khr)(
            device.handle(),
            handle,
            core::ptr::null(),
        );
    }
    outcome
}

/// One picture's worth of device objects.
struct Frame {
    source: vk::Image,
    source_view: vk::ImageView,
    dpb: vk::Image,
    dpb_view: vk::ImageView,
    bitstream: vk::Buffer,
    bitstream_memory: vk::DeviceMemory,
    memories: Vec<vk::DeviceMemory>,
    pool: vk::QueryPool,
    commands: vk::CommandPool,
}

/// How much bitstream one picture may produce.
const BITSTREAM_BYTES: u64 = 4 << 20;

/// An image the video interface will accept, with its memory bound.
fn video_image(
    instance: &ash::Instance,
    physical: vk::PhysicalDevice,
    device: &ash::Device,
    format: vk::Format,
    extent: vk::Extent2D,
    usage: vk::ImageUsageFlags,
) -> Result<(vk::Image, vk::DeviceMemory), String> {
    with_profile(|profile| {
        let profiles = [*profile];
        let mut list = vk::VideoProfileListInfoKHR::default().profiles(&profiles);
        // **The profile travels with the image.** A picture the encoder will
        // read is not an ordinary image: the device lays it out for the codec
        // it was told about, and one created without that is refused at the
        // encode rather than here.
        let create = vk::ImageCreateInfo::default()
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
        // SAFETY: the chain outlives the call.
        let image = unsafe { device.create_image(&create, None) }
            .map_err(|e| format!("a video picture could not be created: {e}"))?;
        // SAFETY: created on this device just above.
        let wanted = unsafe { device.get_image_memory_requirements(image) };
        let index = memory_type(device, instance, physical, wanted)?;
        let allocate = vk::MemoryAllocateInfo::default()
            .allocation_size(wanted.size)
            .memory_type_index(index);
        // SAFETY: the allocate info outlives the call.
        let memory = unsafe { device.allocate_memory(&allocate, None) }
            .map_err(|e| format!("a video picture's memory could not be allocated: {e}"))?;
        // SAFETY: both handles are this device's and nothing is bound yet.
        unsafe { device.bind_image_memory(image, memory, 0) }
            .map_err(|e| format!("a video picture would not bind: {e}"))?;
        Ok((image, memory))
    })
}

/// A view over a whole video picture.
fn whole_view(
    device: &ash::Device,
    image: vk::Image,
    format: vk::Format,
) -> Result<vk::ImageView, String> {
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
        });
    // SAFETY: the create info outlives the call.
    unsafe { device.create_image_view(&info, None) }
        .map_err(|e| format!("a video picture's view could not be created: {e}"))
}

/// Build everything one picture needs, encode it, and answer the two
/// questions a capability query cannot.
fn encode(
    instance: &ash::Instance,
    physical: vk::PhysicalDevice,
    device: &ash::Device,
    video: &ash::khr::video_queue::Device,
    session: vk::VideoSessionKHR,
    parameters: vk::VideoSessionParametersKHR,
    built: &Built,
) -> Result<(), String> {
    let extent = vk::Extent2D {
        width: 2560,
        height: 1440,
    };
    let mut memories = Vec::new();

    let (source, memory) = video_image(
        instance,
        physical,
        device,
        built.picture,
        extent,
        vk::ImageUsageFlags::VIDEO_ENCODE_SRC_KHR,
    )?;
    memories.push(memory);
    let (dpb, memory) = video_image(
        instance,
        physical,
        device,
        built.reference,
        extent,
        vk::ImageUsageFlags::VIDEO_ENCODE_DPB_KHR,
    )?;
    memories.push(memory);
    let source_view = whole_view(device, source, built.picture)?;
    let dpb_view = whole_view(device, dpb, built.reference)?;

    // The bitstream comes back into memory the processor can read, because a
    // picture nobody can read is not an answer.
    let (bitstream, bitstream_memory) = {
        let info = vk::BufferCreateInfo::default()
            .size(BITSTREAM_BYTES)
            .usage(vk::BufferUsageFlags::VIDEO_ENCODE_DST_KHR)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let info = with_profile(|profile| {
            let profiles = [*profile];
            let mut list = vk::VideoProfileListInfoKHR::default().profiles(&profiles);
            // SAFETY: the chain outlives the call.
            unsafe { device.create_buffer(&info.push_next(&mut list), None) }
        });
        let buffer = info.map_err(|e| format!("the bitstream buffer could not be made: {e}"))?;
        // SAFETY: created on this device.
        let wanted = unsafe { device.get_buffer_memory_requirements(buffer) };
        // SAFETY: the device came from this instance.
        let properties = unsafe { instance.get_physical_device_memory_properties(physical) };
        let index = properties
            .memory_types
            .iter()
            .take(usize::try_from(properties.memory_type_count).unwrap_or(0))
            .enumerate()
            .find(|(at, kind)| {
                wanted.memory_type_bits & (1 << at) != 0
                    && kind.property_flags.contains(
                        vk::MemoryPropertyFlags::HOST_VISIBLE
                            | vk::MemoryPropertyFlags::HOST_COHERENT,
                    )
            })
            .and_then(|(at, _)| u32::try_from(at).ok())
            .ok_or_else(|| "no memory the processor can read suits the bitstream".to_string())?;
        let allocate = vk::MemoryAllocateInfo::default()
            .allocation_size(wanted.size)
            .memory_type_index(index);
        // SAFETY: the allocate info outlives the call.
        let memory = unsafe { device.allocate_memory(&allocate, None) }
            .map_err(|e| format!("the bitstream memory could not be allocated: {e}"))?;
        // SAFETY: both handles are this device's and nothing is bound yet.
        unsafe { device.bind_buffer_memory(buffer, memory, 0) }
            .map_err(|e| format!("the bitstream buffer would not bind: {e}"))?;
        (buffer, memory)
    };

    // **The feedback pool is how a length comes back.** Reading the buffer to
    // find out how much of it was written is the thing this replaces.
    let pool = {
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
            unsafe { device.create_query_pool(&create, None) }
        })
        .map_err(|e| format!("the feedback pool could not be made: {e}"))?
    };

    let commands = {
        let create = vk::CommandPoolCreateInfo::default()
            .queue_family_index(built.family)
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
        // SAFETY: the create info outlives the call.
        unsafe { device.create_command_pool(&create, None) }
            .map_err(|e| format!("a command pool could not be made: {e}"))?
    };

    let frame = Frame {
        source,
        source_view,
        dpb,
        dpb_view,
        bitstream,
        bitstream_memory,
        memories,
        pool,
        commands,
    };
    println!("  pictures and bitstream ready");

    let outcome = run(
        instance, device, video, session, parameters, &frame, built, extent,
    );
    release(device, frame);
    outcome
}

/// Give back everything one picture needed.
fn release(device: &ash::Device, frame: Frame) {
    // SAFETY: every handle is this device's, and the caller waits before this.
    unsafe {
        let _ = device.device_wait_idle();
        device.destroy_command_pool(frame.commands, None);
        device.destroy_query_pool(frame.pool, None);
        device.destroy_image_view(frame.source_view, None);
        device.destroy_image_view(frame.dpb_view, None);
        device.destroy_image(frame.source, None);
        device.destroy_image(frame.dpb, None);
        device.destroy_buffer(frame.bitstream, None);
        device.free_memory(frame.bitstream_memory, None);
        for memory in frame.memories {
            device.free_memory(memory, None);
        }
    }
}

/// Record one picture, submit it, and read what came back.
#[expect(clippy::too_many_arguments, reason = "a probe, threading device state")]
///
/// **The two questions are answered here and nowhere else.** Whether a
/// bitrate changes without a rebuild is a control command inside a running
/// session; whether completion is honest is a query asked before the fence
/// says anything.
fn run(
    instance: &ash::Instance,
    device: &ash::Device,
    video: &ash::khr::video_queue::Device,
    session: vk::VideoSessionKHR,
    parameters: vk::VideoSessionParametersKHR,
    frame: &Frame,
    built: &Built,
    extent: vk::Extent2D,
) -> Result<(), String> {
    let allocate = vk::CommandBufferAllocateInfo::default()
        .command_pool(frame.commands)
        .level(vk::CommandBufferLevel::PRIMARY)
        .command_buffer_count(1);
    // SAFETY: the allocate info outlives the call.
    let buffers = unsafe { device.allocate_command_buffers(&allocate) }
        .map_err(|e| format!("a command buffer could not be had: {e}"))?;
    let commands = *buffers.first().ok_or("no command buffer")?;

    // SAFETY: the family index came from this device's own enumeration.
    let queue = unsafe { device.get_device_queue(built.family, 0) };
    let fence = {
        let info = vk::FenceCreateInfo::default();
        // SAFETY: the create info outlives the call.
        unsafe { device.create_fence(&info, None) }
            .map_err(|e| format!("a fence could not be made: {e}"))?
    };

    let encode = ash::khr::video_encode_queue::Device::new(instance, device);
    let outcome = record_and_submit(
        device, video, &encode, session, parameters, frame, extent, commands, queue, fence,
    );
    // SAFETY: the submit above was waited on inside, so nothing refers to it.
    unsafe {
        let _ = device.device_wait_idle();
        device.destroy_fence(fence, None);
    }
    outcome
}

/// The recording itself, and the two answers.
#[expect(clippy::too_many_arguments, reason = "a probe, threading device state")]
fn record_and_submit(
    device: &ash::Device,
    video: &ash::khr::video_queue::Device,
    encode: &ash::khr::video_encode_queue::Device,
    session: vk::VideoSessionKHR,
    parameters: vk::VideoSessionParametersKHR,
    frame: &Frame,
    extent: vk::Extent2D,
    commands: vk::CommandBuffer,
    queue: vk::Queue,
    fence: vk::Fence,
) -> Result<(), String> {
    let begin =
        vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
    // SAFETY: freshly allocated and not recording.
    unsafe { device.begin_command_buffer(commands, &begin) }
        .map_err(|e| format!("recording could not begin: {e}"))?;

    let whole = vk::ImageSubresourceRange {
        aspect_mask: vk::ImageAspectFlags::COLOR,
        base_mip_level: 0,
        level_count: 1,
        base_array_layer: 0,
        layer_count: 1,
    };
    // Both pictures start as nobody's and are named for what the encoder will
    // do with them. Discarding their previous contents is correct: one has
    // never been written and the other is about to be.
    let barriers = [
        vk::ImageMemoryBarrier::default()
            .dst_access_mask(vk::AccessFlags::NONE)
            .old_layout(vk::ImageLayout::UNDEFINED)
            .new_layout(vk::ImageLayout::VIDEO_ENCODE_SRC_KHR)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(frame.source)
            .subresource_range(whole),
        vk::ImageMemoryBarrier::default()
            .dst_access_mask(vk::AccessFlags::NONE)
            .old_layout(vk::ImageLayout::UNDEFINED)
            .new_layout(vk::ImageLayout::VIDEO_ENCODE_DPB_KHR)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(frame.dpb)
            .subresource_range(whole),
    ];
    // SAFETY: recording, and every borrowed structure outlives the call.
    unsafe {
        device.cmd_pipeline_barrier(
            commands,
            vk::PipelineStageFlags::TOP_OF_PIPE,
            vk::PipelineStageFlags::BOTTOM_OF_PIPE,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &barriers,
        );
        device.cmd_reset_query_pool(commands, frame.pool, 0, 1);
    }

    let dpb_resource = vk::VideoPictureResourceInfoKHR::default()
        .coded_offset(vk::Offset2D { x: 0, y: 0 })
        .coded_extent(extent)
        .base_array_layer(0)
        .image_view_binding(frame.dpb_view);
    // **Named with no slot on the way in.** The slot holds nothing yet; the
    // encode below is what puts a picture in it.
    let opening = [vk::VideoReferenceSlotInfoKHR::default()
        .slot_index(-1)
        .picture_resource(&dpb_resource)];
    let begin_coding = vk::VideoBeginCodingInfoKHR::default()
        .video_session(session)
        .video_session_parameters(parameters)
        .reference_slots(&opening);
    // SAFETY: recording; the chain outlives the call.
    unsafe { (video.fp().cmd_begin_video_coding_khr)(commands, &begin_coding) };

    // **The rate control goes in as a control command**, which is the whole
    // question: if a bitrate can be set here it can be set again later without
    // rebuilding anything.
    //
    // **Turned off explicitly for the first picture.** A fixed quantiser is
    // only legal when the session has been told rate control is off; leaving
    // it at the default and setting one anyway is accepted by the recording
    // and hangs the queue, which is the worst way to be wrong.
    let mut off = vk::VideoEncodeRateControlInfoKHR::default()
        .rate_control_mode(vk::VideoEncodeRateControlModeFlagsKHR::DISABLED);
    let mut quality = vk::VideoEncodeQualityLevelInfoKHR::default().quality_level(0);
    let control = vk::VideoCodingControlInfoKHR::default()
        .flags(
            vk::VideoCodingControlFlagsKHR::RESET
                | vk::VideoCodingControlFlagsKHR::ENCODE_RATE_CONTROL
                | vk::VideoCodingControlFlagsKHR::ENCODE_QUALITY_LEVEL,
        )
        .push_next(&mut off)
        .push_next(&mut quality);
    // SAFETY: recording; the chain outlives the call.
    unsafe { (video.fp().cmd_control_video_coding_khr)(commands, &control) };

    // SAFETY: as above; the picture info and slice header are stack values
    // that outlive the call, and the codec structures are plain data.
    unsafe {
        let mut slice: ash::vk::native::StdVideoEncodeH264SliceHeader = core::mem::zeroed();
        slice.slice_type = ash::vk::native::StdVideoH264SliceType_STD_VIDEO_H264_SLICE_TYPE_I;
        let slices = [vk::VideoEncodeH264NaluSliceInfoKHR::default()
            .constant_qp(26)
            .std_slice_header(&slice)];

        let mut picture: ash::vk::native::StdVideoEncodeH264PictureInfo = core::mem::zeroed();
        picture.primary_pic_type =
            ash::vk::native::StdVideoH264PictureType_STD_VIDEO_H264_PICTURE_TYPE_IDR;
        picture.flags.set_IdrPicFlag(1);
        picture.flags.set_is_reference(1);
        let mut h264 = vk::VideoEncodeH264PictureInfoKHR::default()
            .nalu_slice_entries(&slices)
            .std_picture_info(&picture);

        let mut reference: ash::vk::native::StdVideoEncodeH264ReferenceInfo = core::mem::zeroed();
        reference.primary_pic_type =
            ash::vk::native::StdVideoH264PictureType_STD_VIDEO_H264_PICTURE_TYPE_IDR;
        let mut h264_setup =
            vk::VideoEncodeH264DpbSlotInfoKHR::default().std_reference_info(&reference);
        let setup = vk::VideoReferenceSlotInfoKHR::default()
            .slot_index(0)
            .picture_resource(&dpb_resource)
            .push_next(&mut h264_setup);

        let source = vk::VideoPictureResourceInfoKHR::default()
            .coded_offset(vk::Offset2D { x: 0, y: 0 })
            .coded_extent(extent)
            .base_array_layer(0)
            .image_view_binding(frame.source_view);
        let info = vk::VideoEncodeInfoKHR::default()
            .dst_buffer(frame.bitstream)
            .dst_buffer_offset(0)
            .dst_buffer_range(BITSTREAM_BYTES)
            .src_picture_resource(source)
            .setup_reference_slot(&setup)
            .push_next(&mut h264);

        device.cmd_begin_query(commands, frame.pool, 0, vk::QueryControlFlags::empty());
        (encode.fp().cmd_encode_video_khr)(commands, &info);
        device.cmd_end_query(commands, frame.pool, 0);
    }

    let end = vk::VideoEndCodingInfoKHR::default();
    // SAFETY: recording; the structure outlives the call.
    unsafe {
        (video.fp().cmd_end_video_coding_khr)(commands, &end);
        device
            .end_command_buffer(commands)
            .map_err(|e| format!("recording could not end: {e}"))?;
    }

    let buffers = [commands];
    let submits = [vk::SubmitInfo::default().command_buffers(&buffers)];
    // SAFETY: everything borrowed outlives the wait below.
    unsafe { device.queue_submit(queue, &submits, fence) }
        .map_err(|e| format!("the encode would not submit: {e}"))?;

    answer(device, frame, fence)?;

    // **Question one, asked of a session that is already running.** Nothing is
    // rebuilt between the picture above and this: the same session, the same
    // parameters, the same pictures. If a bitrate can be set here then the one
    // congestion actuator the design has exists on this path.
    reconfigure(
        device, video, session, parameters, frame, extent, commands, queue, fence,
    )
}

/// Change the bitrate on a running session and encode again.
#[expect(clippy::too_many_arguments, reason = "a probe, threading device state")]
fn reconfigure(
    device: &ash::Device,
    video: &ash::khr::video_queue::Device,
    session: vk::VideoSessionKHR,
    parameters: vk::VideoSessionParametersKHR,
    frame: &Frame,
    extent: vk::Extent2D,
    commands: vk::CommandBuffer,
    queue: vk::Queue,
    fence: vk::Fence,
) -> Result<(), String> {
    // SAFETY: the previous submit was waited on, so the buffer is idle.
    unsafe {
        device
            .reset_command_buffer(commands, vk::CommandBufferResetFlags::empty())
            .map_err(|e| format!("the command buffer would not reset: {e}"))?;
        device
            .reset_fences(&[fence])
            .map_err(|e| format!("the fence would not reset: {e}"))?;
    }
    let begin =
        vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
    // SAFETY: reset just above and not recording.
    unsafe { device.begin_command_buffer(commands, &begin) }
        .map_err(|e| format!("recording could not begin: {e}"))?;
    // SAFETY: recording; the pool is this device's.
    unsafe { device.cmd_reset_query_pool(commands, frame.pool, 0, 1) };

    let dpb_resource = vk::VideoPictureResourceInfoKHR::default()
        .coded_offset(vk::Offset2D { x: 0, y: 0 })
        .coded_extent(extent)
        .base_array_layer(0)
        .image_view_binding(frame.dpb_view);
    let opening = [vk::VideoReferenceSlotInfoKHR::default()
        .slot_index(-1)
        .picture_resource(&dpb_resource)];
    // **The state it is already in has to be restated here.** Once a session
    // has been told anything other than the default, every later opening of it
    // must carry that same configuration, or the opening is invalid -- which a
    // driver accepts silently and only the validation layer says.
    let mut current = vk::VideoEncodeRateControlInfoKHR::default()
        .rate_control_mode(vk::VideoEncodeRateControlModeFlagsKHR::DISABLED);
    let begin_coding = vk::VideoBeginCodingInfoKHR::default()
        .video_session(session)
        .video_session_parameters(parameters)
        .reference_slots(&opening)
        .push_next(&mut current);
    // SAFETY: recording; the chain outlives the call.
    unsafe { (video.fp().cmd_begin_video_coding_khr)(commands, &begin_coding) };

    // **No reset flag, which is the point.** A reset would be a rebuild in all
    // but name: it discards what the session has learned and the next picture
    // has to be one with no history behind it.
    let layers = [vk::VideoEncodeRateControlLayerInfoKHR::default()
        .average_bitrate(6_000_000)
        .max_bitrate(6_000_000)
        .frame_rate_numerator(60)
        .frame_rate_denominator(1)];
    let mut changed = vk::VideoEncodeRateControlInfoKHR::default()
        .rate_control_mode(vk::VideoEncodeRateControlModeFlagsKHR::CBR)
        .layers(&layers)
        .virtual_buffer_size_in_ms(1000)
        .initial_virtual_buffer_size_in_ms(0);
    let control = vk::VideoCodingControlInfoKHR::default()
        .flags(vk::VideoCodingControlFlagsKHR::ENCODE_RATE_CONTROL)
        .push_next(&mut changed);
    // SAFETY: recording; the chain outlives the call.
    unsafe { (video.fp().cmd_control_video_coding_khr)(commands, &control) };

    let end = vk::VideoEndCodingInfoKHR::default();
    // SAFETY: recording; the structure outlives the call.
    unsafe {
        (video.fp().cmd_end_video_coding_khr)(commands, &end);
        device
            .end_command_buffer(commands)
            .map_err(|e| format!("recording could not end: {e}"))?;
    }

    let buffers = [commands];
    let submits = [vk::SubmitInfo::default().command_buffers(&buffers)];
    // SAFETY: everything borrowed outlives the wait below.
    unsafe { device.queue_submit(queue, &submits, fence) }
        .map_err(|e| format!("the rate change would not submit: {e}"))?;
    // SAFETY: the fence was submitted with above.
    unsafe { device.wait_for_fences(&[fence], true, 5_000_000_000) }
        .map_err(|e| format!("the rate change did not finish: {e}"))?;
    println!("  bitrate changed to 6 Mbit/s on the running session, no rebuild, no reset");
    Ok(())
}

/// Ask the two questions of a submitted picture.
fn answer(device: &ash::Device, frame: &Frame, fence: vk::Fence) -> Result<(), String> {
    // **Question two.** Asked before waiting, which is the only way to know
    // whether the answer is honest: a query that blocks here is one a loop
    // cannot use, and one that reports a length for a picture the device has
    // not finished is worse.
    let mut early = [0_u64; 2];
    let asked = feedback(
        device,
        frame.pool,
        &mut early,
        vk::QueryResultFlags::empty(),
    );
    println!(
        "  asked before the fence: {}",
        match asked {
            Ok(()) => format!("answered offset={} bytes={}", early[0], early[1]),
            Err(vk::Result::NOT_READY) => "not ready, and said so".to_string(),
            Err(error) => format!("refused, {error}"),
        }
    );

    // SAFETY: the fence is this device's and was submitted with above.
    unsafe { device.wait_for_fences(&[fence], true, 5_000_000_000) }
        .map_err(|e| format!("the encode did not finish: {e}"))?;

    let mut done = [0_u64; 2];
    feedback(device, frame.pool, &mut done, vk::QueryResultFlags::WAIT)
        .map_err(|e| format!("the length would not come back: {e}"))?;
    println!(
        "  encoded: offset {} bytes {}",
        done[0],
        done.get(1).copied().unwrap_or(0)
    );
    Ok(())
}

/// Read the one feedback query, which carries two values.
///
/// **Not through the typed helper.** That one takes the number of queries
/// from the length of the destination, and this pool holds a single query that
/// answers with two numbers; asking it for two queries reads past the end of a
/// pool with one.
fn feedback(
    device: &ash::Device,
    pool: vk::QueryPool,
    into: &mut [u64; 2],
    flags: vk::QueryResultFlags,
) -> Result<(), vk::Result> {
    let bytes = size_of::<[u64; 2]>();
    // SAFETY: one query is read into a destination of exactly the size its two
    // values come to, with the stride saying the same.
    let result = unsafe {
        (device.fp_v1_0().get_query_pool_results)(
            device.handle(),
            pool,
            0,
            1,
            bytes,
            into.as_mut_ptr().cast(),
            bytes as vk::DeviceSize,
            flags | vk::QueryResultFlags::TYPE_64,
        )
    };
    match result {
        vk::Result::SUCCESS => Ok(()),
        other => Err(other),
    }
}
