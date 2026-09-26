//! What a stream is built on, on Linux: the display's device, the encoder
//! that can take its frames, and the census of what those could code. The
//! loop above is written once; a platform writes this module.

use std::sync::Arc;
use std::sync::mpsc;

use lowlat_core::control::status;

use super::{
    Backend, Codec, Config, DISPLAY_WAIT, ENCODE_DEPTH, Exit, FromDevice, Join, Roster, Shared,
    await_display, encode_loop, settle_pace, start_bps,
};

/// Build the stream's device and encoder for `config` and run it until it
/// exits: the encoder that shares the capture's device where that is asked
/// for and the device can serve it, else the one that serves the display's
/// device.
pub(super) fn build(
    shared: &Arc<Shared>,
    arrivals: &mpsc::Receiver<Join>,
    config: &Config,
    roster: &mut Roster,
) -> Exit {
    // **Resolved on every rebuild, not once.** A display can move to
    // another card while this is running, and the rebuild that follows it
    // is the only chance to build an encoder that can take its frames.
    let backend = match config.backend {
        Some(chosen) => chosen,
        None => follow_display(config),
    };

    lowlat_common::log_info!(
        "stream: encoding w={} h={} fps={} ceiling_mbps={:.1} codec={:?} backend={:?}",
        config.width,
        config.height,
        config.fps,
        config.configured_mbps,
        config.codec,
        backend
    );
    // **The third encoder is preferred only when asked, and never wins an
    // argument with a named backend.** A device that cannot serve it has
    // already logged why by the time this falls through to the pair that
    // always could.
    let prefer_vulkan = (config.prefer_vulkan
        || std::env::var("LOWLAT_VULKAN_ENCODE").is_ok_and(|value| value == "1"))
        && config.backend.is_none()
        && config.display;
    let vulkan_exit = if prefer_vulkan {
        run_vulkan(shared, arrivals, config.clone(), roster)
    } else {
        None
    };
    match vulkan_exit {
        Some(exit) => exit,
        None => match backend {
            Backend::Open => run_open(shared, arrivals, config.clone(), roster),
            Backend::Vendor => run_vendor(shared, arrivals, config.clone(), roster),
        },
    }
}

/// Which encoder can take frames from the device the display is on.
///
/// **A display and its encoder have to be on one device**, so this is read
/// from the display rather than configured. A machine with one card answers
/// the same thing every time and nothing about it is visible; a machine with
/// two answers differently depending on which screen is being captured, and
/// getting it wrong is an encoder that refuses every frame it is handed.
///
/// Anything that is not the vendor's own driver is served by the open stack,
/// including the open driver for the same hardware.
fn follow_display(config: &Config) -> Backend {
    if !config.display {
        return Backend::Open;
    }
    let driver = crate::display::Display::driver(config.output.as_deref());
    let backend = match driver.as_deref() {
        Some("nvidia") => Backend::Vendor,
        _ => Backend::Open,
    };
    lowlat_common::log_info!(
        "stream: the display is on {}, encoding with {backend:?}",
        driver.as_deref().unwrap_or("a device that did not say")
    );
    backend
}

/// Capability bits a peer can declare that this pipeline does not emit.
///
/// A request for one is refused and reported rather than quietly treated as
/// granted, which would leave the peer building a decoder for a stream it will
/// never receive.
///
/// **The full-range bit is not listed here.** This host codes the video range
/// whatever is declared, and the bit is a preference, so it is never refused;
/// testing it as a capability reported a refusal on every ordinary request,
/// which is what it did while it was taken for a base flag.
/// The depth the conversion targets are allocated at, which must be the depth
/// the encoder was built for.
///
/// **One place, because the two are read separately and a disagreement is not
/// refused anywhere.** A target allocated at one depth and read at the other
/// has a consistent pitch and a consistent size; only the samples inside it
/// are the wrong width.
fn colour_of(config: &Config) -> lowlat_capture::convert::Depth {
    if config.ten_bit {
        lowlat_capture::convert::Depth::Ten
    } else {
        lowlat_capture::convert::Depth::Eight
    }
}

/// Whether every encoder this host could select codes full chroma at the
/// running depth, or which gate refused it.
///
/// **A census, not a capability list.** One encode serves every seat and the
/// configuration is settled once (D11), so a part this host might later land
/// on that cannot code full chroma would end the session rather than degrade.
/// The offer is refused up front on such a machine, with the gate named, so a
/// guest that asks is answered without an encoder being built for a promise
/// the machine cannot keep. Checked once per run: the answers are device
/// properties and nothing here changes them.
pub(super) fn chroma_census(config: &Config) -> Result<(), &'static str> {
    let ten_bit = config.ten_bit;
    // **A probe knob, loudly on purpose.** A mixed machine refuses the offer
    // by design, which makes the granted path unexercisable there; this opens
    // it for a measurement or a test and says so rather than pretending.
    if std::env::var("LOWLAT_PROBE_CENSUS").is_ok_and(|value| value == "pass") {
        lowlat_common::log_warn!("stream: the full-chroma census is forced open by a probe knob");
        return Ok(());
    }
    match config.backend {
        Some(Backend::Vendor) => vendor_census(ten_bit),
        Some(Backend::Open) => open_census(ten_bit),
        None => {
            // **The third interface codes no full chroma on any part measured
            // here**, so preferring it is a refusal and the preference is the
            // gate. Asked before the device censuses, because it makes them
            // moot and it is the one answer that needs no device.
            if config.prefer_vulkan
                || std::env::var("LOWLAT_VULKAN_ENCODE").is_ok_and(|value| value == "1")
            {
                return Err("the third encoder, which no measured part serves full chroma");
            }
            // **Both display-following backends are candidates wherever the
            // output may move**, not just where it is now: a move onto a part
            // that cannot code full chroma would end the session, which is
            // the exact failure this census exists to keep unannounced.
            open_census(ten_bit)?;
            vendor_census(ten_bit)
        }
    }
}

/// The open backend, on every node an output could move onto.
///
/// **Only nodes that would serve the stream are asked about full chroma.** A
/// node with no second-codec encoder at this depth cannot serve the stream at
/// all, full chroma or not, which is a refusal the subsampled stream already
/// owns; the census adds only what full chroma adds.
fn open_census(ten_bit: bool) -> Result<(), &'static str> {
    let Ok(runtime) = lowlat_encode::vaapi::Vaapi::load() else {
        return Err("the open runtime, which could not be loaded");
    };
    let Ok(nodes) = std::fs::read_dir("/dev/dri") else {
        return Err("the node directory, which could not be walked");
    };
    for entry in nodes.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with("renderD") {
            continue;
        }
        let path = entry.path();
        // **A node the vendor driver owns is served by the vendor encoder**,
        // never the open one, so the open census does not ask it; the vendor
        // census covers it.
        if crate::display::driver_of(&path).as_deref() == Some("nvidia") {
            continue;
        }
        let Ok(asked) = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()) else {
            continue;
        };
        let Ok(display) = lowlat_encode::vaapi::Display::open(&runtime, &asked) else {
            continue;
        };
        // **A node without the second codec at this depth cannot serve the
        // stream at all**, so it is not asked about full chroma; the refusal
        // there belongs to the subsampled stream it cannot code either.
        if display
            .caps_at(lowlat_encode::vaapi::Codec::H265, ten_bit)
            .is_err()
        {
            continue;
        }
        if display.caps_444(ten_bit).is_err() {
            return Err("the open encoder on a node the output could move onto");
        }
    }
    Ok(())
}

/// The vendor backend, on the device it would encode with.
fn vendor_census(ten_bit: bool) -> Result<(), &'static str> {
    let Ok(cuda) = lowlat_encode::cuda::Cuda::load() else {
        // **Absent is not a refusal.** A machine without the vendor runtime
        // never selects that backend, so there is no part to fail the census.
        return Ok(());
    };
    let Ok(device) = cuda.any_device() else {
        return Ok(());
    };
    let Ok(compute) = cuda.retain_primary(&device) else {
        return Ok(());
    };
    let Ok(api) = lowlat_encode::nvenc::Api::load() else {
        return Ok(());
    };
    let Ok(session) = api.open_session(compute) else {
        return Ok(());
    };
    let Ok(caps) = session.caps(lowlat_encode::nvenc::Codec::H265) else {
        return Err("the vendor encoder, which would not report its capabilities");
    };
    if !caps.yuv444 || (ten_bit && !caps.ten_bit) {
        return Err("the vendor encoder, which codes no full chroma at this depth");
    }
    Ok(())
}

/// How many pictures the third encoder's ring holds.
///
/// The same figure as the other backends' target rotation: the conversion
/// writes the next picture while the previous one encodes, and the loop's
/// collect discipline never lets a slot be rewritten while an encode holds
/// it.
const VULKAN_SLOTS: usize = 4;

/// Run the stream on the encoder that shares the capture's device, or say
/// why it cannot.
///
/// **Nothing is what "follow the display instead" looks like from here.**
/// Every reason a device cannot serve this path is logged once and answered
/// with `None`, and the caller falls through to the pair that always could.
/// A device that can serve it and then fails building keeps its refusal:
/// masking a real fault with a slower path is how a rig measures the wrong
/// encoder.
fn run_vulkan(
    shared: &Arc<Shared>,
    arrivals: &mpsc::Receiver<Join>,
    config: Config,
    roster: &mut Roster,
) -> Option<Exit> {
    let codec = match config.codec {
        Codec::H264 => lowlat_encode::vulkan::Codec::H264,
        Codec::H265 => lowlat_encode::vulkan::Codec::H265,
    };
    let depth = if config.ten_bit {
        lowlat_encode::vulkan::Depth::Ten
    } else {
        lowlat_encode::vulkan::Depth::Eight
    };
    // The display decides the size, exactly as on the other paths.
    let (width, height, refresh_hz) = match await_display(config.output.as_deref()) {
        Some(shape) => shape,
        None => {
            lowlat_common::log_error!(
                "stream: nothing has been scanning out for {:.0}s, ending {} guest(s)",
                DISPLAY_WAIT.as_secs_f64(),
                roster.active.len()
            );
            return Some(Exit::Failed(status::CAPTURE_UNAVAILABLE));
        }
    };
    shared.publish_picture(width, height);
    let config = Config {
        width,
        height,
        fps: settle_pace(shared, config.fps, refresh_hz),
        ..config
    };

    let node = match crate::display::Display::node_of(config.output.as_deref()) {
        Ok(node) => node,
        Err(error) => {
            lowlat_common::log_info!(
                "stream: no node for the vulkan encoder, following the display, {error}"
            );
            return None;
        }
    };
    let capture = match lowlat_capture::vulkan::Device::for_display_and_encode(&node) {
        Ok(device) => device,
        Err(error) => {
            lowlat_common::log_info!(
                "stream: the device does not open for encode, following the display, {error}"
            );
            return None;
        }
    };
    let Some((queue, family)) = capture.encode_queue() else {
        lowlat_common::log_info!("stream: the device has no encode queue, following the display");
        return None;
    };
    let device = match lowlat_encode::vulkan::Device::shared(capture.clone(), queue, family) {
        Ok(device) => device,
        Err(error) => {
            lowlat_common::log_info!(
                "stream: the encode interface refused the device, following the display, {error}"
            );
            return None;
        }
    };
    let caps = match device.caps_at(codec, depth) {
        Ok(caps) => caps,
        Err(error) => {
            lowlat_common::log_info!(
                "stream: the device does not encode over this interface, following the display, \
                 {error}"
            );
            return None;
        }
    };
    // **Asked of the interface rather than known here.** This backend codes no
    // full chroma on any part, and the refusal belongs where the pairing is
    // decided rather than three steps later where a target is allocated: left
    // to be discovered there it ends every guest on the stream, where falling
    // through to a backend that can code it costs nobody anything.
    if config.chroma_444 && !caps.chroma_444 {
        lowlat_common::log_info!(
            "stream: this interface codes no full chroma, following the display"
        );
        return None;
    }
    if !caps.shared_picture {
        // A copy stage between conversion and encode is not written, and
        // silently inserting one is exactly what this backend must not do.
        lowlat_common::log_info!(
            "stream: a copy stands between conversion and encode here, following the display"
        );
        return None;
    }

    // From here on the device said it could, so a failure is reported
    // rather than masked.
    let mut encoder = match device.encoder(
        &caps,
        width,
        height,
        start_bps(&config),
        config.fps,
        VULKAN_SLOTS,
    ) {
        Ok(encoder) => encoder,
        Err(error) => {
            lowlat_common::log_error!("stream: the vulkan encoder failed to build, {error}");
            return Some(Exit::Failed(status::ENCODER_UNAVAILABLE));
        }
    };
    encoder.set_quality_setting(config.quality);
    // **The floor only on this backend**, because the effort level is fixed
    // when the session is created and moving it is not what a live setting may
    // cost. What is asked for is logged; nothing here reports what was done
    // with it.
    lowlat_common::log_info!(
        "stream: quality={:?}, quantiser floor {}, no effort lever on this backend",
        config.quality,
        encoder.min_qp()
    );
    let mut targets = Vec::with_capacity(VULKAN_SLOTS);
    for slot in 0..VULKAN_SLOTS {
        let (Some(planes), Some(image)) = (encoder.planes(slot), encoder.source(slot)) else {
            lowlat_common::log_error!("stream: the encoder lends no planes for slot {slot}");
            return Some(Exit::Failed(status::ENCODER_UNAVAILABLE));
        };
        // **At the depth the encoder built its pictures for.** The target
        // descriptor is what tells the conversion which range constants to
        // use and what to quantise against, and nothing downstream re-reads
        // the depth from anywhere else: handed the eight-bit shorthand, a
        // ten-bit session converts against 255 and writes each result into the
        // low eight bits of a sixteen-bit sample. The encode succeeds, the
        // stream decodes, and every picture is dark and wrongly ranged.
        targets.push(lowlat_capture::convert::TargetRef::lent_to_encoder_at(
            image,
            planes,
            colour_of(&config),
        ));
    }
    let mut desktop = match crate::display::Display::open(
        VULKAN_SLOTS,
        colour_of(&config),
        config.chroma_444,
        config.output.as_deref(),
        config.convert,
        crate::display::Register::VulkanRing {
            node,
            width,
            height,
            device: capture,
            targets,
        },
    ) {
        Ok(desktop) => desktop,
        Err(error) => {
            lowlat_common::log_error!("stream: the display could not be opened, {error}");
            return Some(Exit::Failed(status::CAPTURE_UNAVAILABLE));
        }
    };
    // **The line above named the backend the display would have chosen, and
    // this one is what took the stream.** Nothing announced the third encoder
    // building, so a log could only be read for which encoder ran by the
    // shape of what the display was registered with -- which is describing
    // the system from something that merely correlates with it.
    lowlat_common::log_info!(
        "stream: the shared-device encoder took it instead, codec={:?} backend=Vulkan on {}",
        config.codec,
        device.name()
    );
    Some(encode_loop(
        shared,
        arrivals,
        config,
        roster,
        &mut encoder,
        Some(&mut desktop),
    ))
}

fn run_open(
    shared: &Arc<Shared>,
    arrivals: &mpsc::Receiver<Join>,
    config: Config,
    roster: &mut Roster,
) -> Exit {
    // **The size the display settled on, before anything is built for it.**
    // Same rule as the vendor path: a display decides its own size and
    // everything downstream has to be told the same answer.
    let (width, height, refresh_hz) = if config.display {
        match await_display(config.output.as_deref()) {
            Some(shape) => {
                if (shape.0, shape.1) != (config.width, config.height) {
                    lowlat_common::log_info!(
                        "stream: the display is {}x{}, not the configured {}x{}; following it",
                        shape.0,
                        shape.1,
                        config.width,
                        config.height
                    );
                }
                shape
            }
            None => {
                lowlat_common::log_error!(
                    "stream: nothing has been scanning out for {:.0}s, ending {} guest(s)",
                    DISPLAY_WAIT.as_secs_f64(),
                    roster.active.len()
                );
                return Exit::Failed(status::CAPTURE_UNAVAILABLE);
            }
        }
    } else {
        (config.width, config.height, 0)
    };
    shared.publish_picture(width, height);
    let config = Config {
        width,
        height,
        fps: settle_pace(shared, config.fps, refresh_hz),
        ..config
    };

    let (codec, params) = match config.codec {
        Codec::H264 => (
            lowlat_encode::vaapi::Codec::H264,
            lowlat_encode::vaapi::Params::H264(lowlat_encode::h264::Params {
                width: config.width,
                height: config.height,
                fps: config.fps,
                level_idc: H264_LEVEL,
                log2_max_frame_num_minus4: 4,
                log2_max_poc_lsb_minus4: 4,
                max_num_ref_frames: 1,
            }),
        ),
        Codec::H265 => (
            lowlat_encode::vaapi::Codec::H265,
            lowlat_encode::vaapi::Params::H265(lowlat_encode::h265::Params {
                width: config.width,
                height: config.height,
                fps: config.fps,
                level_idc: H265_LEVEL,
                log2_max_poc_lsb_minus4: 4,
                max_num_ref_frames: 1,
                transform_depth: lowlat_encode::h265::TRANSFORM_HIERARCHY_DEPTH,
                bit_depth_minus8: if config.ten_bit { 2 } else { 0 },
                chroma_444: config.chroma_444,
            }),
        ),
    };
    let Ok(display) = lowlat_encode::vaapi::Vaapi::load() else {
        lowlat_common::log_error!("stream: display runtime unavailable, nothing will encode");
        return Exit::Failed(status::ENCODER_UNAVAILABLE);
    };
    // **The encoder is built on the device that drew the picture.** The
    // backend already follows the display; the node it encodes through has to
    // follow it as well, and a constant only agrees with the display while the
    // machine has one card. With two it depends on the order the kernel probed
    // them, so adding or moving a card silently repoints the encoder at the
    // other device: the picture is converted on one and coded on the other,
    // which works, crosses the bus every frame, and costs the difference with
    // nothing in the log to say so.
    // Without a display there is nothing to follow, and the first node is as
    // good an answer as any.
    let node = crate::display::Display::render_node_of(config.output.as_deref())
        .filter(|_| config.display);
    let named = node
        .as_deref()
        .and_then(|node| std::ffi::CString::new(node.as_os_str().as_encoded_bytes()).ok());
    let asked = named.as_deref().unwrap_or(c"/dev/dri/renderD128");
    lowlat_common::log_info!(
        "stream: encoding through {}, {}",
        asked.to_string_lossy(),
        if named.is_some() {
            "the device the display is on"
        } else {
            "which is the default; no display to follow"
        }
    );
    let Ok(display) = lowlat_encode::vaapi::Display::open(&display, asked) else {
        lowlat_common::log_error!("stream: render node could not be opened");
        return Exit::Failed(status::ENCODER_UNAVAILABLE);
    };
    let caps = if config.chroma_444 {
        display.caps_444(config.ten_bit)
    } else {
        display.caps_at(codec, config.ten_bit)
    };
    let Ok(caps) = caps else {
        lowlat_common::log_error!("stream: render node reports no encode for codec={codec:?}");
        return Exit::Failed(status::ENCODER_CAPABILITIES);
    };
    let Ok(context) = display.create_context(caps, config.width, config.height, ENCODE_DEPTH)
    else {
        lowlat_common::log_error!("stream: encode context could not be created");
        return Exit::Failed(status::ENCODER_UNAVAILABLE);
    };
    let Ok(mut encoder) = context.encoder(params, start_bps(&config)) else {
        lowlat_common::log_error!("stream: encoder could not be configured");
        return Exit::Failed(status::ENCODER_UNAVAILABLE);
    };
    encoder.set_quality_setting(config.quality);
    // **What was asked for, not what the device did.** Nothing in this
    // interface reports whether a driver honoured either lever, and one
    // measured here takes the quantiser floor on one codec and ignores it on
    // the other, so this is a record of the request.
    lowlat_common::log_info!(
        "stream: quality={:?}, quantiser floor {}, effort {} of {}",
        config.quality,
        encoder.min_qp(),
        encoder.quality(),
        caps.quality_range
    );
    let mut desktop = if config.display {
        match crate::display::Display::open(
            ENCODE_DEPTH,
            colour_of(&config),
            config.chroma_444,
            config.output.as_deref(),
            config.convert,
            crate::display::Register::Open(&display),
        ) {
            Ok(desktop) => Some(desktop),
            Err(error) => {
                lowlat_common::log_error!("stream: the display could not be opened, {error}");
                return Exit::Failed(status::CAPTURE_UNAVAILABLE);
            }
        }
    } else {
        None
    };
    encode_loop(
        shared,
        arrivals,
        config,
        roster,
        &mut encoder,
        desktop.as_mut(),
    )
}

fn run_vendor(
    shared: &Arc<Shared>,
    arrivals: &mpsc::Receiver<Join>,
    config: Config,
    roster: &mut Roster,
) -> Exit {
    let Ok(cuda) = lowlat_encode::cuda::Cuda::load() else {
        lowlat_common::log_error!("stream: compute runtime unavailable, nothing will encode");
        return Exit::Failed(status::ENCODER_UNAVAILABLE);
    };
    let Ok(device) = cuda.any_device() else {
        lowlat_common::log_error!("stream: no compute device");
        return Exit::Failed(status::ENCODER_CAPABILITIES);
    };
    let Ok(compute) = cuda.retain_primary(&device) else {
        lowlat_common::log_error!("stream: compute context could not be retained");
        return Exit::Failed(status::ENCODER_UNAVAILABLE);
    };
    let Ok(api) = lowlat_encode::nvenc::Api::load() else {
        lowlat_common::log_error!("stream: encoder runtime unavailable");
        return Exit::Failed(status::ENCODER_UNAVAILABLE);
    };
    let Ok(session) = api.open_session(compute) else {
        lowlat_common::log_error!("stream: encode session could not be opened");
        return Exit::Failed(status::ENCODER_UNAVAILABLE);
    };
    // **The display decides the picture size when it is the source.** The
    // encoder is created before the source exists and its registration fixes
    // the shape, so asking the display first is the only way the two agree.
    let (width, height, refresh_hz) = if config.display {
        // **A display that is asleep is the ordinary case for this product,
        // not a fault.** Somebody connecting to a machine whose screen has
        // powered down is most of what remote access is for, so it is waited
        // for rather than refused. The wait is bounded because the only thing
        // that wakes a blanked display is somebody at the desk, and a guest
        // held indefinitely on a machine nobody is at learns nothing.
        match await_display(config.output.as_deref()) {
            Some(shape) => {
                if (shape.0, shape.1) != (config.width, config.height) {
                    lowlat_common::log_info!(
                        "stream: the display is {}x{}, not the configured {}x{}; following it",
                        shape.0,
                        shape.1,
                        config.width,
                        config.height
                    );
                }
                shape
            }
            None => {
                lowlat_common::log_error!(
                    "stream: nothing has been scanning out for {:.0}s, ending {} guest(s)",
                    DISPLAY_WAIT.as_secs_f64(),
                    roster.active.len()
                );
                return Exit::Failed(status::CAPTURE_UNAVAILABLE);
            }
        }
    } else {
        (config.width, config.height, 0)
    };
    // **Said once the size is settled and before any guest is seated.** It is
    // the coordinate space a peer's absolute input is expressed in, so a guest
    // that seated against the configured numbers would place every position
    // scaled by the ratio between the two.
    shared.publish_picture(width, height);
    // **And the configuration follows it too, from here down.** Everything
    // below this line that asks the configuration how big the picture is has
    // to get the same answer the display gave, or it judges the picture
    // against a rectangle the picture is not in. The pointer did exactly that:
    // it was tested for being inside the stream against the configured size,
    // so on a display larger than it, every update from the part of the screen
    // beyond those bounds was dropped and a guest kept whatever shape it last
    // had.
    let config = Config {
        width,
        height,
        fps: settle_pace(shared, config.fps, refresh_hz),
        ..config
    };
    let Ok(mut encoder) = session.initialize(
        &cuda,
        lowlat_encode::nvenc::Config {
            codec: match config.codec {
                Codec::H264 => lowlat_encode::nvenc::Codec::H264,
                Codec::H265 => lowlat_encode::nvenc::Codec::H265,
            },
            width,
            height,
            fps: config.fps,
            bitrate_bps: start_bps(&config),
            min_qp: config.quality.min_qp(),
            // Full chroma is granted per session, never configured; the
            // census that gates it already ran before this encoder exists.
            chroma: if config.chroma_444 {
                lowlat_encode::nvenc::Chroma::Yuv444
            } else {
                lowlat_encode::nvenc::Chroma::Yuv420
            },
            depth: if config.ten_bit {
                lowlat_encode::nvenc::Depth::Ten
            } else {
                lowlat_encode::nvenc::Depth::Eight
            },
        },
    ) else {
        lowlat_common::log_error!("stream: encoder could not be configured");
        return Exit::Failed(status::ENCODER_UNAVAILABLE);
    };
    lowlat_common::log_info!(
        "stream: quality={:?}, quantiser floor {}",
        config.quality,
        config.quality.min_qp()
    );
    let mut desktop = if config.display {
        match crate::display::Display::open(
            lowlat_encode::nvenc::IN_FLIGHT,
            colour_of(&config),
            config.chroma_444,
            config.output.as_deref(),
            config.convert,
            crate::display::Register::Vendor(&encoder),
        ) {
            Ok(desktop) => Some(desktop),
            Err(error) => {
                lowlat_common::log_error!("stream: the display could not be opened, {error}");
                return Exit::Failed(status::ENCODER_UNAVAILABLE);
            }
        }
    } else {
        None
    };
    encode_loop(
        shared,
        arrivals,
        config,
        roster,
        &mut encoder,
        desktop.as_mut(),
    )
}

/// The lowest level each codec has that carries 1080p60, which is 4.2 on the
/// first and 4.1 on the second. **They are written on different scales**: ten
/// times the level number on the first, thirty on the second. Writing the
/// first codec's scale into the second declares level 1.4, which is far below
/// what this resolution needs, and a strict decoder refuses the stream.
const H264_LEVEL: u32 = 42;
const H265_LEVEL: u32 = 123;

impl FromDevice for lowlat_encode::nvenc::Encoder<'_> {
    fn submit_from_device(
        &mut self,
        registration: &crate::display::Registration,
        force_keyframe: bool,
    ) -> bool {
        // **A registration made for the other backend is refused, not
        // reinterpreted.** It names an object this runtime has never heard of.
        let crate::display::Registration::Vendor { input, .. } = registration else {
            return false;
        };
        self.submit_registered(input, force_keyframe).is_ok()
    }
}

impl FromDevice for lowlat_encode::vaapi::Encoder<'_> {
    fn submit_from_device(
        &mut self,
        registration: &crate::display::Registration,
        force_keyframe: bool,
    ) -> bool {
        let crate::display::Registration::Open { surface } = registration else {
            return false;
        };
        self.submit_registered(*surface, force_keyframe).is_ok()
    }
}

impl FromDevice for lowlat_encode::vulkan::Encoder<'_> {
    fn submit_from_device(
        &mut self,
        registration: &crate::display::Registration,
        force_keyframe: bool,
    ) -> bool {
        let crate::display::Registration::Vulkan { slot } = registration else {
            return false;
        };
        // The conversion wrote the slot and handed it over in the layout
        // this encoder reads; the written entry point is what honours that.
        self.submit_written(*slot, force_keyframe).is_ok()
    }
}
