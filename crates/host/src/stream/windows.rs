//! What a stream is built on, on Windows: the encoder of the captured
//! output's GPU -- NVIDIA's, AMD's or Intel's own -- on that output's D3D11
//! device, fed by the display's conversion, or by the synthetic source where
//! no display is asked for. The loop above is written once; a platform writes
//! this module.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;

use lowlat_capture::d3d11::Layout;
use lowlat_common::clock::{self, Time};
use lowlat_core::control::status;

use super::{
    Backend, Codec, Config, DISPLAY_WAIT, Declared, Exit, FromDevice, Join, Roster, Shared,
    encode_loop, own_refresh, own_size, settle_pace, start_bps,
};
use crate::display::own::FIRST_SIZE;
use crate::display::{Capturable, Display, Register, Registration, VIRTUAL};

/// How often the host's own display is looked at while nobody is seated.
const OWN_LOOK_MS: f64 = 1000.0;

/// How often a running stream asks again for a mode of the host's own display
/// the system refused.
const OWN_RETRY_MS: f64 = 2000.0;

/// How long one such asking waits for the system: one attempt, on the stream's
/// own thread between two pictures.
const OWN_RETRY_BOUND: std::time::Duration = std::time::Duration::from_millis(100);

/// The host's own display, as the stream's thread keeps it
/// (docs/impl-plan-windows.md W3): plugged when the configuration names it,
/// or allows it and nothing else is lit; held at its mode; sized for a guest
/// arriving into an empty room.
#[derive(Debug, Default)]
pub(super) struct Own {
    display: crate::display::own::Own,
    looked: Option<Time>,
}

impl Own {
    /// One pass while nobody is seated: at most once a second, so a machine
    /// with no monitor has a display before anybody connects and one whose
    /// mode the system put back is held.
    pub(super) fn idle(&mut self, shared: &Shared, config: &Config) {
        if self
            .looked
            .is_some_and(|at| clock::elapsed_ms(at) < OWN_LOOK_MS)
        {
            return;
        }
        self.looked = Some(Time::now());
        self.reconcile(shared, config, None);
    }

    /// Before a stream is built: the same, and the size `joining` stated
    /// when it arrives into an empty room and the stream captures this
    /// display.
    pub(super) fn prepare(&mut self, shared: &Shared, config: &Config, joining: Option<Declared>) {
        self.looked = Some(Time::now());
        self.reconcile(shared, config, joining);
    }

    /// One pass of a running stream: **a mode the system refused is asked
    /// again**, every [`OWN_RETRY_MS`] and one attempt each time. A built
    /// stream asks the hold nothing more, so a refusal that passes would leave
    /// the display at another mode until something rebuilt; a mode put
    /// changes the display, and the stream follows that by a rebuild of its
    /// own.
    pub(super) fn streaming(&mut self) {
        if !asks_again(self.display.pending(), self.looked.map(clock::elapsed_ms)) {
            return;
        }
        self.looked = Some(Time::now());
        self.display.retry(OWN_RETRY_BOUND);
    }

    /// One pass while a stream waits for its display: the same as nobody
    /// seated, at the wait's own pace, so a display that went with its
    /// driver is plugged again as soon as the driver is back.
    fn waiting(&mut self, shared: &Shared, config: &Config) {
        self.looked = Some(Time::now());
        self.reconcile(shared, config, None);
    }

    fn reconcile(&mut self, shared: &Shared, config: &Config, joining: Option<Declared>) {
        let named = config
            .output
            .as_deref()
            .is_some_and(|output| output.eq_ignore_ascii_case(VIRTUAL));
        let asked = shared.own_display();
        let refresh = own_refresh(shared.asked_fps(), asked);
        if !self.display.plugged() {
            if !named
                && !(config.virtual_fallback && Display::capturable() == Capturable::NothingLit)
            {
                return;
            }
            // **Plugged first**, at the size asked of it or the first size,
            // so the display a guest is about to watch exists to be asked.
            let (width, height) = own_size(FIRST_SIZE, asked, None);
            if !self.display.hold(width, height, refresh) && !self.display.plugged() {
                return;
            }
        }
        let current = self.display.wanted().unwrap_or(FIRST_SIZE);
        // **Only for the display the stream is about to capture**: a guest
        // arriving to watch another output says nothing about this one.
        let joining = joining.filter(|_| Display::captures_own(config.output.as_deref()));
        let (width, height) = own_size(current, asked, joining);
        self.display.hold(width, height, refresh);
    }
}

/// **Whether a running stream asks the hold again now**: a mode the system
/// refused is pending, and the hold was last asked `since_ms` ago, or never --
/// at most once in [`OWN_RETRY_MS`], so a system that goes on refusing costs
/// one attempt in each.
fn asks_again(pending: bool, since_ms: Option<f64>) -> bool {
    pending && since_ms.is_none_or(|ms| ms >= OWN_RETRY_MS)
}

/// The vendors whose own encoders this platform builds.
const NVIDIA: u32 = 0x10de;
const AMD: u32 = 0x1002;
const INTEL: u32 = 0x8086;

/// Whether this platform builds an encoder on `adapter`: NVIDIA's, AMD's or
/// Intel's own, on a GPU of theirs that renders -- Intel's where its driver
/// names the current runtime; the parts only the older one reaches have no
/// encoder here.
fn encodes_on(adapter: &lowlat_drivers::d3d11::Adapter) -> bool {
    adapter.decodes_here()
        && match adapter.vendor {
            NVIDIA | AMD => true,
            INTEL => lowlat_drivers::vpl::has_current(adapter),
            _ => false,
        }
}

/// **The memory clock of the NVIDIA GPU a stream runs on, held for as long as
/// the stream runs**, where the host is asked to (docs/05-host.md section
/// 4.2): NVIDIA's management library, loaded with the stream's thread and kept
/// for it, so a rebuild costs a call rather than a load.
#[derive(Debug, Default)]
pub(super) struct MemoryClock {
    nvml: Option<lowlat_drivers::nvml::Nvml>,
}

impl MemoryClock {
    /// Loaded where asked, every NVIDIA GPU's memory clock given back to its
    /// driver first: **a hold outlives the process that took it**, so a host
    /// that ended without its stream's end left one held.
    pub(super) fn new(asked: bool) -> Self {
        if !asked {
            return Self::default();
        }
        let nvml = match lowlat_drivers::nvml::Nvml::load() {
            Ok(nvml) => nvml,
            Err(error) => {
                lowlat_common::log_info!("stream: memory clock left to the driver, {error}");
                return Self::default();
            }
        };
        let gpus = nvml.count().unwrap_or(0);
        let mut refused = None;
        for index in 0..gpus {
            if let Err(error) = nvml
                .gpu(index)
                .and_then(|gpu| nvml.release_memory_clock(gpu))
            {
                refused = Some(error);
            }
        }
        match refused {
            None => lowlat_common::log_info!(
                "stream: memory clock held while a stream runs, given back first on {gpus} NVIDIA GPU(s)"
            ),
            Some(error) => lowlat_common::log_info!(
                "stream: memory clock not given back on every one of {gpus} NVIDIA GPU(s), {error}"
            ),
        }
        Self { nvml: Some(nvml) }
    }

    /// Held on `adapter`'s GPU until what is answered is dropped, between the
    /// floor and the ceiling [`memory_floor`] gives; `None`, said why, where
    /// it cannot be.
    fn hold(&self, adapter: &lowlat_drivers::d3d11::Adapter) -> Option<Held<'_>> {
        let nvml = self.nvml.as_ref()?;
        let left = |why: &dyn core::fmt::Display| {
            lowlat_common::log_info!(
                "stream: memory clock left to the driver on {}, {why}",
                adapter.description
            );
        };
        let gpu = nvml
            .matching(adapter.vendor, adapter.device, adapter.subsystem)
            .map_err(|error| left(&error))
            .ok()?;
        let mut listed = [0; 32];
        let clocks = nvml
            .memory_clocks(gpu, &mut listed)
            .map_err(|error| left(&error))
            .ok()?;
        let Some((floor, ceiling)) = memory_floor(clocks) else {
            left(&"it lists no memory clock");
            return None;
        };
        nvml.hold_memory_clock(gpu, floor, ceiling)
            .map_err(|error| left(&error))
            .ok()?;
        lowlat_common::log_info!(
            "stream: memory clock held at {floor} to {ceiling} MHz on {}",
            adapter.description
        );
        Some(Held { nvml, gpu })
    }
}

/// A stream's hold on its GPU's memory clock, given back as the stream ends.
#[derive(Debug)]
struct Held<'n> {
    nvml: &'n lowlat_drivers::nvml::Nvml,
    gpu: lowlat_drivers::nvml::Gpu,
}

impl Drop for Held<'_> {
    fn drop(&mut self) {
        match self.nvml.release_memory_clock(self.gpu) {
            Ok(()) => lowlat_common::log_info!("stream: memory clock given back to the driver"),
            Err(error) => {
                lowlat_common::log_warn!("stream: memory clock not given back, {error}");
            }
        }
    }
}

/// The floor and the ceiling a stream holds a GPU's memory clock between,
/// among the clocks it lists: the lowest at or above half its top, and the
/// top. **Below half are the light-load states, where memory bounds the
/// encoder**: at 1080p, 5 ms a picture at 810 MHz of a top of 14001 against 2
/// at 7001 (docs/05-host.md section 4.2).
fn memory_floor(clocks: &[u32]) -> Option<(u32, u32)> {
    let top = clocks.iter().copied().max()?;
    let floor = clocks
        .iter()
        .copied()
        .filter(|&clock| clock >= top / 2)
        .min()?;
    Some((floor, top))
}

/// Build the stream's device and encoder for `config` and run it until it
/// exits: with a display, everything on the device of the output's own
/// adapter, with that GPU's encoder; without one, the encoder of the first
/// GPU that has one on a device of its own, and the generator its source.
/// The open backend by name is refused rather than answered with something
/// else.
pub(super) fn build(
    shared: &Arc<Shared>,
    arrivals: &mpsc::Receiver<Join>,
    config: &Config,
    roster: &mut Roster,
    own: &mut Own,
    clock: &MemoryClock,
) -> Exit {
    lowlat_common::log_info!(
        "stream: encoding w={} h={} fps={} ceiling_mbps={:.1} codec={:?} backend={:?}",
        config.width,
        config.height,
        config.fps,
        config.configured_mbps,
        config.codec,
        Backend::Vendor
    );
    // **Said once.** The loop asks again at its own pace while a refusal is
    // young, and the answer does not change.
    static NO_OPEN: AtomicBool = AtomicBool::new(false);
    if config.backend == Some(Backend::Open) {
        if !NO_OPEN.swap(true, Ordering::Relaxed) {
            lowlat_common::log_error!("stream: the open backend is not built for this platform");
        }
        return Exit::Failed(status::ENCODER_UNAVAILABLE);
    }
    if config.display {
        run_display(shared, arrivals, config.clone(), roster, own, clock)
    } else {
        run_vendor(shared, arrivals, config.clone(), roster, clock)
    }
}

/// Whether the encoder of the output being captured codes full chroma at the
/// running depth, or which gate refused it.
///
/// **That output's encoder alone**, the output found as the run finds it: a
/// move onto an output whose encoder cannot code full chroma drops it at that
/// rebuild (D11). **Only from the display's conversion**, which writes the
/// packed words the encoder takes on this device -- AYUV at eight bits, Y410
/// at ten, each carried through a decoder and back -- where the generator's
/// frames carry two planes. Asked once per run of the device: the answers are
/// its properties.
pub(super) fn chroma_census(config: &Config) -> Result<(), &'static str> {
    if !config.display {
        return Err("the synthetic source, whose frames carry two planes");
    }
    let Some(output) = Display::locate(config.output.as_deref()) else {
        return Err("the output, which is not part of the desktop");
    };
    if !encodes_on(&output.adapter) {
        return Err("the output's GPU, which has no encoder for full chroma here");
    }
    // **The engine has no full chroma at any depth**, so AMD's answer needs
    // no device.
    if output.adapter.vendor == AMD {
        return Err("AMD's encoder on the output's GPU, which codes no full chroma");
    }
    let Ok(d3d11) = lowlat_encode::d3d11::D3d11::load() else {
        return Err("the graphics libraries, which could not be loaded");
    };
    let Ok(device) = d3d11.open(output.adapter.luid) else {
        return Err("the vendor's device, which would not open");
    };
    if output.adapter.vendor == INTEL {
        return intel_chroma(&d3d11, &device, &output.adapter, config);
    }
    let Ok(api) = lowlat_encode::nvenc::Api::load() else {
        return Ok(());
    };
    let Ok(session) = api.open_session(Arc::new(device)) else {
        return Err("the vendor encoder, which would not open a session");
    };
    let Ok(caps) = session.caps(lowlat_encode::nvenc::Codec::H265) else {
        return Err("the vendor encoder, which would not report its capabilities");
    };
    if !caps.yuv444 || (config.ten_bit && !caps.ten_bit) {
        return Err("the vendor encoder, which codes no full chroma at this depth");
    }
    Ok(())
}

/// Intel's answer to [`chroma_census`]: its encoder on `device` asked whether
/// it codes the second codec's full chroma from the packed word at the
/// running depth, at the output's size and pace, with nothing built.
fn intel_chroma(
    d3d11: &lowlat_encode::d3d11::D3d11,
    device: &lowlat_encode::d3d11::Device,
    adapter: &lowlat_drivers::d3d11::Adapter,
    config: &Config,
) -> Result<(), &'static str> {
    let Ok(runtime) = lowlat_drivers::vpl::Vpl::for_adapter(adapter) else {
        return Err("the vendor encoder, whose runtime would not load");
    };
    let Some(index) = d3d11.plain_index(adapter.luid).ok().flatten() else {
        return Err("the vendor encoder, whose GPU the system does not list");
    };
    let Ok(session) = runtime.encoder_session(device, index) else {
        return Err("the vendor encoder, which would not open a session");
    };
    let asked = lowlat_encode::vpl::Config {
        codec: lowlat_encode::vpl::Codec::H265,
        chroma: lowlat_encode::vpl::Chroma::Yuv444,
        ..intel_config(config)
    };
    if !lowlat_encode::vpl::codes(&session, &asked) {
        return Err("the vendor encoder, which codes no full chroma at this depth");
    }
    Ok(())
}

/// The encoder's configuration for `config`.
fn encoder_config(config: &Config) -> lowlat_encode::nvenc::Config {
    lowlat_encode::nvenc::Config {
        codec: match config.codec {
            Codec::H264 => lowlat_encode::nvenc::Codec::H264,
            Codec::H265 => lowlat_encode::nvenc::Codec::H265,
        },
        width: config.width,
        height: config.height,
        fps: config.fps,
        bitrate_bps: start_bps(config),
        min_qp: config.quality.min_qp(),
        thorough: config.quality.thorough(),
        // Full chroma is granted per session, never configured; the census
        // that gates it has already answered.
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
    }
}

/// AMD's encoder's configuration for `config`, reading `source`: the
/// capture's own pictures, or two planes of YUV -- the display's conversion,
/// or the generator's frames.
fn amd_config(config: &Config, source: lowlat_encode::amf::Source) -> lowlat_encode::amf::Config {
    lowlat_encode::amf::Config {
        codec: match config.codec {
            Codec::H264 => lowlat_encode::amf::Codec::H264,
            Codec::H265 => lowlat_encode::amf::Codec::H265,
        },
        width: config.width,
        height: config.height,
        fps: config.fps,
        bitrate_bps: start_bps(config),
        min_qp: config.quality.min_qp(),
        thorough: config.quality.thorough(),
        depth: if config.ten_bit {
            lowlat_encode::amf::Depth::Ten
        } else {
            lowlat_encode::amf::Depth::Eight
        },
        source,
    }
}

/// Intel's encoder's configuration for `config`: built at the configured
/// rate, the ceiling, which the rate's buffer is made from.
fn intel_config(config: &Config) -> lowlat_encode::vpl::Config {
    lowlat_encode::vpl::Config {
        codec: match config.codec {
            Codec::H264 => lowlat_encode::vpl::Codec::H264,
            Codec::H265 => lowlat_encode::vpl::Codec::H265,
        },
        width: config.width,
        height: config.height,
        fps: config.fps,
        bitrate_bps: start_bps(config),
        min_qp: config.quality.min_qp(),
        thorough: config.quality.thorough(),
        // As NVIDIA's: granted per session, past the census.
        chroma: if config.chroma_444 {
            lowlat_encode::vpl::Chroma::Yuv444
        } else {
            lowlat_encode::vpl::Chroma::Yuv420
        },
        depth: if config.ten_bit {
            lowlat_encode::vpl::Depth::Ten
        } else {
            lowlat_encode::vpl::Depth::Eight
        },
    }
}

/// The desktop: the output first, then its adapter, the device on it, the
/// encoder at the output's size, and only then the duplication and the
/// encoder's inputs on that device (docs/impl-plan-windows.md W2).
fn run_display(
    shared: &Arc<Shared>,
    arrivals: &mpsc::Receiver<Join>,
    config: Config,
    roster: &mut Roster,
    own: &mut Own,
    clock: &MemoryClock,
) -> Exit {
    let wanted = config.output.as_deref();
    // **A display that is asleep or switched off is waited for**, as on
    // Linux: somebody connecting to a machine nobody is at is most of what
    // remote access is for. **So is the host's own display gone with its
    // driver**, plugged again meanwhile, and **an output listed under a GPU
    // without the encoder**: while a GPU's driver restarts, the system lists
    // a virtual display under another GPU for about a second and a half,
    // which is just when a stream that lost its device rebuilds
    // (docs/05-host.md section 2).
    let began = Time::now();
    let mut said = None;
    let (output, (width, height, refresh_hz)) = loop {
        // **A host that is ending stops waiting**, before it plugs anything:
        // the wait runs for many seconds, and a host asked to end while its
        // session left the console would otherwise outlive the request and go
        // on plugging its own display into a desktop that is no longer its.
        if shared.stopping.load(Ordering::Acquire) != 0 {
            return Exit::Stopped;
        }
        let found = Display::shape_of_display(wanted)
            .and_then(|shape| Display::locate(wanted).map(|output| (output, shape)));
        let elsewhere = match found {
            Some((output, shape)) if encodes_on(&output.adapter) => break (output, shape),
            Some((output, _)) => Some(output),
            None => None,
        };
        if clock::elapsed_ms(began) >= DISPLAY_WAIT.as_secs_f64() * 1000.0 {
            // **The encoder follows the output**, and only the vendors' own --
            // NVIDIA's, AMD's and Intel's -- are built here: an output another
            // GPU renders is refused rather than copied across, which would
            // measure the copy. A virtual display's output is listed under the
            // GPU that renders it, so it is that GPU's like any other, and
            // refused only where that GPU stays one without an encoder here.
            if let Some(output) = elsewhere {
                lowlat_common::log_error!(
                    "stream: {} is on {}, which has no encoder this host builds",
                    output.name,
                    output.adapter.description
                );
                return Exit::Failed(status::ENCODER_UNAVAILABLE);
            }
            lowlat_common::log_warn!(
                "stream: no output has been part of the desktop for {:.0}s",
                DISPLAY_WAIT.as_secs_f64()
            );
            return Exit::NoDisplay;
        }
        if said != Some(elsewhere.is_some()) {
            said = Some(elsewhere.is_some());
            match &elsewhere {
                Some(output) => lowlat_common::log_info!(
                    "stream: {} is on {}, waiting for it on a GPU with the encoder",
                    output.name,
                    output.adapter.description
                ),
                None => lowlat_common::log_info!(
                    "stream: nothing is scanning out, waiting for a display to come back"
                ),
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
        own.waiting(shared, &config);
    };
    let Ok(d3d11) = lowlat_encode::d3d11::D3d11::load() else {
        lowlat_common::log_error!("stream: graphics libraries unavailable, nothing will encode");
        return Exit::Failed(status::ENCODER_UNAVAILABLE);
    };
    let device = match d3d11.open(output.adapter.luid) {
        Ok(device) => Arc::new(device),
        Err(error) => {
            lowlat_common::log_error!("stream: no device on {}, {error}", output.adapter.luid);
            return Exit::Failed(status::ENCODER_UNAVAILABLE);
        }
    };
    if (width, height) != (config.width, config.height) {
        lowlat_common::log_info!(
            "stream: {} is {width}x{height}, not the configured {}x{}; following it",
            output.name,
            config.width,
            config.height
        );
    }
    // **Said once the size is settled and before any guest is seated**: it
    // is the coordinate space a peer's absolute input is expressed in.
    shared.publish_picture(width, height);
    let config = Config {
        width,
        height,
        fps: settle_pace(shared, shared.asked_fps(), refresh_hz),
        ..config
    };
    if output.adapter.vendor == AMD {
        return run_amd(shared, arrivals, config, roster, own, &device, &output);
    }
    if output.adapter.vendor == INTEL {
        return run_intel(shared, arrivals, config, roster, own, &device, &output);
    }
    let Ok(api) = lowlat_encode::nvenc::Api::load() else {
        lowlat_common::log_error!("stream: encoder runtime unavailable");
        return Exit::Failed(status::ENCODER_UNAVAILABLE);
    };
    // **Before the encoder, so it is given back after it.**
    let _held = clock.hold(&output.adapter);
    // **Declared before the encoder, so it outlives it.** Its targets are
    // registered with the encoder's session, which takes the registrations
    // back as it goes; the textures must still be there when it does.
    let mut desktop: Display;
    let session = match api.open_session(Arc::clone(&device)) {
        Ok(session) => session,
        Err(error) => {
            lowlat_common::log_error!("stream: encode session could not be opened, {error}");
            return Exit::Failed(status::ENCODER_UNAVAILABLE);
        }
    };
    let mut encoder = match session.initialize(encoder_config(&config)) {
        Ok(encoder) => encoder,
        Err(error) => {
            lowlat_common::log_error!("stream: encoder could not be configured, {error}");
            return Exit::Failed(status::ENCODER_UNAVAILABLE);
        }
    };
    lowlat_common::log_info!(
        "stream: quality={:?}, quantiser floor {}",
        config.quality,
        config.quality.min_qp()
    );
    desktop = match Display::open(
        &device,
        &output.name,
        Layout::of(config.ten_bit, config.chroma_444),
        (width, height),
        Register::Nvenc(&encoder),
    ) {
        Ok(desktop) => desktop,
        Err(crate::display::Error::NoOutput) => {
            return Exit::Rediscover("the output moved before it could be opened");
        }
        Err(error) => {
            lowlat_common::log_error!("stream: the display could not be opened, {error}");
            return Exit::Failed(status::CAPTURE_UNAVAILABLE);
        }
    };
    // **Woken where asked**: the display is held on from here, which keeps
    // it on but never wakes one that slept.
    if config.wake_display {
        desktop.wake();
    }
    lowlat_common::log_info!(
        "stream: encoding {} on {} ({})",
        output.name,
        output.adapter.description,
        output.adapter.luid
    );
    encode_loop(
        shared,
        arrivals,
        config,
        roster,
        &mut encoder,
        Some(&mut desktop),
        Some(own),
    )
}

/// [`run_display`] on an AMD GPU: AMD's own encoder on the output's device,
/// reading the display's conversion.
fn run_amd(
    shared: &Arc<Shared>,
    arrivals: &mpsc::Receiver<Join>,
    config: Config,
    roster: &mut Roster,
    own: &mut Own,
    device: &Arc<lowlat_encode::d3d11::Device>,
    output: &lowlat_drivers::d3d11::Output,
) -> Exit {
    let Ok(runtime) = lowlat_drivers::amf::Amf::load() else {
        lowlat_common::log_error!("stream: AMD's encoder runtime unavailable");
        return Exit::Failed(status::ENCODER_UNAVAILABLE);
    };
    let context = match runtime.context(device) {
        Ok(context) => context,
        Err(error) => {
            lowlat_common::log_error!("stream: AMD's runtime would not take the device, {error}");
            return Exit::Failed(status::ENCODER_UNAVAILABLE);
        }
    };
    // **Eight bits read the capture as it is**, the encoder converting in its
    // own hardware; ten go through our conversion into P010, which the
    // encoder's own cannot write.
    let in_place = !config.ten_bit;
    let source = if in_place {
        lowlat_encode::amf::Source::Rgb
    } else {
        lowlat_encode::amf::Source::Yuv
    };
    let mut encoder =
        match lowlat_encode::amf::Encoder::new(&runtime, &context, amd_config(&config, source)) {
            Ok(encoder) => encoder,
            Err(error) => {
                lowlat_common::log_error!("stream: encoder could not be configured, {error}");
                return Exit::Failed(status::ENCODER_UNAVAILABLE);
            }
        };
    lowlat_common::log_info!(
        "stream: quality={:?}, quantiser floor {}, the capture read {}",
        config.quality,
        config.quality.min_qp(),
        if in_place { "in place" } else { "converted" }
    );
    let register = if in_place {
        Register::Captured(encoder.engine())
    } else {
        Register::Amd(&mut encoder)
    };
    let mut desktop = match Display::open(
        device,
        &output.name,
        Layout::of(config.ten_bit, config.chroma_444),
        (config.width, config.height),
        register,
    ) {
        Ok(desktop) => desktop,
        Err(crate::display::Error::NoOutput) => {
            return Exit::Rediscover("the output moved before it could be opened");
        }
        Err(error) => {
            lowlat_common::log_error!("stream: the display could not be opened, {error}");
            return Exit::Failed(status::CAPTURE_UNAVAILABLE);
        }
    };
    if config.wake_display {
        desktop.wake();
    }
    lowlat_common::log_info!(
        "stream: encoding {} on {} ({}) with AMD's encoder",
        output.name,
        output.adapter.description,
        output.adapter.luid
    );
    encode_loop(
        shared,
        arrivals,
        config,
        roster,
        &mut encoder,
        Some(&mut desktop),
        Some(own),
    )
}

/// Intel's runtime for `adapter` and a session of it on `device`, for an
/// encoder reading textures of ours; said why where either cannot be had.
fn intel_session<'r>(
    runtime: &'r lowlat_drivers::vpl::Vpl,
    device: &'r lowlat_encode::d3d11::Device,
    adapter: &lowlat_drivers::d3d11::Adapter,
) -> Option<lowlat_drivers::vpl::Session<'r>> {
    // **The runtime numbers adapters in the plain enumeration**, never the
    // high-performance order the adapters are found in.
    let index = match lowlat_encode::d3d11::D3d11::load()
        .and_then(|d3d11| d3d11.plain_index(adapter.luid))
    {
        Ok(Some(index)) => index,
        Ok(None) => {
            lowlat_common::log_error!("stream: {} is not enumerated", adapter.description);
            return None;
        }
        Err(error) => {
            lowlat_common::log_error!("stream: the adapters could not be enumerated, {error}");
            return None;
        }
    };
    match runtime.encoder_session(device, index) {
        Ok(session) => Some(session),
        Err(error) => {
            lowlat_common::log_error!("stream: Intel's runtime would not take the device, {error}");
            None
        }
    }
}

/// [`run_display`] on an Intel GPU: Intel's own encoder on the output's
/// device, reading the display's conversion.
fn run_intel(
    shared: &Arc<Shared>,
    arrivals: &mpsc::Receiver<Join>,
    config: Config,
    roster: &mut Roster,
    own: &mut Own,
    device: &Arc<lowlat_encode::d3d11::Device>,
    output: &lowlat_drivers::d3d11::Output,
) -> Exit {
    let runtime = match lowlat_drivers::vpl::Vpl::for_adapter(&output.adapter) {
        Ok(runtime) => runtime,
        Err(error) => {
            lowlat_common::log_error!("stream: Intel's encoder runtime unavailable, {error}");
            return Exit::Failed(status::ENCODER_UNAVAILABLE);
        }
    };
    let Some(session) = intel_session(&runtime, device, &output.adapter) else {
        return Exit::Failed(status::ENCODER_UNAVAILABLE);
    };
    let mut encoder =
        match lowlat_encode::vpl::Encoder::new(&session, device, intel_config(&config)) {
            Ok(encoder) => encoder,
            Err(error) => {
                lowlat_common::log_error!("stream: encoder could not be configured, {error}");
                return Exit::Failed(status::ENCODER_UNAVAILABLE);
            }
        };
    lowlat_common::log_info!(
        "stream: quality={:?}, quantiser floor {}",
        config.quality,
        config.quality.min_qp()
    );
    let mut desktop = match Display::open(
        device,
        &output.name,
        Layout::of(config.ten_bit, config.chroma_444),
        (config.width, config.height),
        Register::Intel(&mut encoder),
    ) {
        Ok(desktop) => desktop,
        Err(crate::display::Error::NoOutput) => {
            return Exit::Rediscover("the output moved before it could be opened");
        }
        Err(error) => {
            lowlat_common::log_error!("stream: the display could not be opened, {error}");
            return Exit::Failed(status::CAPTURE_UNAVAILABLE);
        }
    };
    if config.wake_display {
        desktop.wake();
    }
    lowlat_common::log_info!(
        "stream: encoding {} on {} ({}) with Intel's encoder",
        output.name,
        output.adapter.description,
        output.adapter.luid
    );
    encode_loop(
        shared,
        arrivals,
        config,
        roster,
        &mut encoder,
        Some(&mut desktop),
        Some(own),
    )
}

/// No display: the encoder of the first GPU with one, on a device of its
/// own, the generator its source.
fn run_vendor(
    shared: &Arc<Shared>,
    arrivals: &mpsc::Receiver<Join>,
    config: Config,
    roster: &mut Roster,
    clock: &MemoryClock,
) -> Exit {
    let Ok(d3d11) = lowlat_encode::d3d11::D3d11::load() else {
        lowlat_common::log_error!("stream: graphics libraries unavailable, nothing will encode");
        return Exit::Failed(status::ENCODER_UNAVAILABLE);
    };
    // **The first adapter with an encoder of its vendor's that renders**, in
    // the system's high-performance order: with nothing captured, nothing
    // else decides.
    let Some(adapter) = d3d11
        .adapters()
        .ok()
        .and_then(|adapters| adapters.into_iter().find(encodes_on))
    else {
        lowlat_common::log_error!(
            "stream: no adapter with an encoder this host builds renders here"
        );
        return Exit::Failed(status::ENCODER_UNAVAILABLE);
    };
    let device = match d3d11.open(adapter.luid) {
        Ok(device) => device,
        Err(error) => {
            lowlat_common::log_error!("stream: no device on {}, {error}", adapter.luid);
            return Exit::Failed(status::ENCODER_UNAVAILABLE);
        }
    };
    if adapter.vendor == AMD {
        return synthetic_amd(shared, arrivals, config, roster, &device, &adapter);
    }
    if adapter.vendor == INTEL {
        return synthetic_intel(shared, arrivals, config, roster, &device, &adapter);
    }
    let Ok(api) = lowlat_encode::nvenc::Api::load() else {
        lowlat_common::log_error!("stream: encoder runtime unavailable");
        return Exit::Failed(status::ENCODER_UNAVAILABLE);
    };
    let _held = clock.hold(&adapter);
    let session = match api.open_session(Arc::new(device)) {
        Ok(session) => session,
        Err(error) => {
            lowlat_common::log_error!("stream: encode session could not be opened, {error}");
            return Exit::Failed(status::ENCODER_UNAVAILABLE);
        }
    };
    lowlat_common::log_info!(
        "stream: encoding on {} ({})",
        adapter.description,
        adapter.luid
    );
    // **The generator's picture is the configured one**, so the size is said
    // before any guest is seated, as it is once a display has answered.
    shared.publish_picture(config.width, config.height);
    let config = Config {
        fps: settle_pace(shared, shared.asked_fps(), 0),
        ..config
    };
    let mut encoder = match session.initialize(encoder_config(&config)) {
        Ok(encoder) => encoder,
        Err(error) => {
            lowlat_common::log_error!("stream: encoder could not be configured, {error}");
            return Exit::Failed(status::ENCODER_UNAVAILABLE);
        }
    };
    lowlat_common::log_info!(
        "stream: quality={:?}, quantiser floor {}",
        config.quality,
        config.quality.min_qp()
    );
    encode_loop(shared, arrivals, config, roster, &mut encoder, None, None)
}

/// [`run_vendor`] on an AMD GPU: AMD's own encoder on `device`, the
/// generator's frames written into its inputs.
fn synthetic_amd(
    shared: &Arc<Shared>,
    arrivals: &mpsc::Receiver<Join>,
    config: Config,
    roster: &mut Roster,
    device: &lowlat_encode::d3d11::Device,
    adapter: &lowlat_drivers::d3d11::Adapter,
) -> Exit {
    let Ok(runtime) = lowlat_drivers::amf::Amf::load() else {
        lowlat_common::log_error!("stream: AMD's encoder runtime unavailable");
        return Exit::Failed(status::ENCODER_UNAVAILABLE);
    };
    let context = match runtime.context(device) {
        Ok(context) => context,
        Err(error) => {
            lowlat_common::log_error!("stream: AMD's runtime would not take the device, {error}");
            return Exit::Failed(status::ENCODER_UNAVAILABLE);
        }
    };
    lowlat_common::log_info!(
        "stream: encoding on {} ({}) with AMD's encoder",
        adapter.description,
        adapter.luid
    );
    shared.publish_picture(config.width, config.height);
    let config = Config {
        fps: settle_pace(shared, shared.asked_fps(), 0),
        ..config
    };
    let mut encoder = match lowlat_encode::amf::Encoder::new(
        &runtime,
        &context,
        amd_config(&config, lowlat_encode::amf::Source::Yuv),
    ) {
        Ok(encoder) => encoder,
        Err(error) => {
            lowlat_common::log_error!("stream: encoder could not be configured, {error}");
            return Exit::Failed(status::ENCODER_UNAVAILABLE);
        }
    };
    lowlat_common::log_info!(
        "stream: quality={:?}, quantiser floor {}",
        config.quality,
        config.quality.min_qp()
    );
    encode_loop(shared, arrivals, config, roster, &mut encoder, None, None)
}

/// [`run_vendor`] on an Intel GPU: Intel's own encoder on `device`, the
/// generator's frames written into its input.
fn synthetic_intel(
    shared: &Arc<Shared>,
    arrivals: &mpsc::Receiver<Join>,
    config: Config,
    roster: &mut Roster,
    device: &lowlat_encode::d3d11::Device,
    adapter: &lowlat_drivers::d3d11::Adapter,
) -> Exit {
    let runtime = match lowlat_drivers::vpl::Vpl::for_adapter(adapter) {
        Ok(runtime) => runtime,
        Err(error) => {
            lowlat_common::log_error!("stream: Intel's encoder runtime unavailable, {error}");
            return Exit::Failed(status::ENCODER_UNAVAILABLE);
        }
    };
    let Some(session) = intel_session(&runtime, device, adapter) else {
        return Exit::Failed(status::ENCODER_UNAVAILABLE);
    };
    lowlat_common::log_info!(
        "stream: encoding on {} ({}) with Intel's encoder",
        adapter.description,
        adapter.luid
    );
    shared.publish_picture(config.width, config.height);
    let config = Config {
        fps: settle_pace(shared, shared.asked_fps(), 0),
        ..config
    };
    let mut encoder =
        match lowlat_encode::vpl::Encoder::new(&session, device, intel_config(&config)) {
            Ok(encoder) => encoder,
            Err(error) => {
                lowlat_common::log_error!("stream: encoder could not be configured, {error}");
                return Exit::Failed(status::ENCODER_UNAVAILABLE);
            }
        };
    lowlat_common::log_info!(
        "stream: quality={:?}, quantiser floor {}",
        config.quality,
        config.quality.min_qp()
    );
    encode_loop(shared, arrivals, config, roster, &mut encoder, None, None)
}

impl FromDevice for lowlat_encode::nvenc::Encoder<'_> {
    fn submit_from_device(&mut self, registration: &Registration, force_keyframe: bool) -> bool {
        match registration {
            Registration::Nvenc(input) => self.submit_registered(input, force_keyframe).is_ok(),
            Registration::Amd(_) | Registration::Captured(_) | Registration::Intel(_) => false,
        }
    }
}

impl FromDevice for lowlat_encode::amf::Encoder<'_> {
    fn submit_from_device(&mut self, registration: &Registration, force_keyframe: bool) -> bool {
        match registration {
            Registration::Amd(input) => self.submit_registered(*input, force_keyframe).is_ok(),
            Registration::Captured(texture) => self.submit_texture(texture, force_keyframe).is_ok(),
            Registration::Nvenc(_) | Registration::Intel(_) => false,
        }
    }
}

impl FromDevice for lowlat_encode::vpl::Encoder<'_> {
    fn submit_from_device(&mut self, registration: &Registration, force_keyframe: bool) -> bool {
        match registration {
            Registration::Intel(input) => self.submit_registered(*input, force_keyframe).is_ok(),
            Registration::Nvenc(_) | Registration::Amd(_) | Registration::Captured(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A mode the system refused is asked again while the stream runs, and
    /// at its pace**: at once when the hold was never asked, never sooner than
    /// a period after the last asking, and not at all with nothing pending --
    /// so a stream whose host's display took its mode pays nothing, and one
    /// the system goes on refusing pays one short attempt a period.
    #[test]
    fn a_refused_mode_is_asked_again_once_a_period_while_streaming() {
        assert!(asks_again(true, None), "a refusal never asked again");
        assert!(
            !asks_again(true, Some(0.0)),
            "asked again on the heels of the last asking"
        );
        assert!(
            !asks_again(true, Some(OWN_RETRY_MS - 1.0)),
            "asked again inside the period"
        );
        assert!(
            asks_again(true, Some(OWN_RETRY_MS)),
            "not asked again once the period passed"
        );
        assert!(
            !asks_again(false, Some(10.0 * OWN_RETRY_MS)),
            "asked with nothing pending"
        );
        assert!(!asks_again(false, None), "asked with nothing pending");
    }

    /// **The floor is the lowest clock at or above half the top**: on the
    /// development machine's GPU its middle state's 7001 MHz under a top of
    /// 14001, never the light-load 810 and 405; another generation's 5001
    /// under 9501, whatever order the clocks are listed in.
    #[test]
    fn a_stream_floors_the_memory_clock_at_the_lowest_clock_from_half_its_top() {
        assert_eq!(
            memory_floor(&[14001, 13801, 7001, 810, 405]),
            Some((7001, 14001))
        );
        assert_eq!(
            memory_floor(&[405, 9501, 5001, 810, 9251]),
            Some((5001, 9501))
        );
        assert_eq!(memory_floor(&[7001]), Some((7001, 7001)));
        assert_eq!(memory_floor(&[]), None);
    }

    /// **Held, the memory clock stays at or above its floor under an
    /// encoder's load, and given back it falls below**: the second half is
    /// what shows the first is the hold's and not the load's. As the
    /// administrator, on an NVIDIA GPU nothing else is loading:
    /// `cargo test -p lowlat-host --lib a_held_memory_clock -- --ignored --nocapture`.
    #[test]
    #[ignore = "requires the vendor driver and the administrator"]
    fn a_held_memory_clock_stays_at_its_floor_under_an_encoder() {
        use lowlat_encode::Encoder as _;
        use std::time::{Duration, Instant};

        let d3d11 = lowlat_encode::d3d11::D3d11::load().expect("the graphics libraries");
        let adapter = d3d11
            .adapters()
            .expect("the adapters")
            .into_iter()
            .find(|adapter| adapter.vendor == NVIDIA && encodes_on(adapter))
            .expect("an NVIDIA GPU");
        let clock = MemoryClock::new(true);
        let nvml = clock.nvml.as_ref().expect("the management library");
        let gpu = nvml
            .matching(adapter.vendor, adapter.device, adapter.subsystem)
            .expect("the adapter's GPU");
        let mut listed = [0; 32];
        let (floor, _) = memory_floor(nvml.memory_clocks(gpu, &mut listed).expect("its clocks"))
            .expect("a floor");
        let device = Arc::new(d3d11.open(adapter.luid).expect("a device"));
        let api = lowlat_encode::nvenc::Api::load().expect("the encoder runtime");
        let session = api.open_session(device).expect("a session");
        let mut encoder = session
            .initialize(lowlat_encode::nvenc::Config {
                codec: lowlat_encode::nvenc::Codec::H264,
                width: 1920,
                height: 1080,
                fps: 60,
                bitrate_bps: 10_000_000,
                min_qp: 5,
                thorough: false,
                chroma: lowlat_encode::nvenc::Chroma::Yuv420,
                depth: lowlat_encode::nvenc::Depth::Eight,
            })
            .expect("an encoder");
        let mut source = lowlat_capture::synthetic::Synthetic::new(1920, 1080);
        // Forty seconds at sixty a second, the clock read every tenth of one
        // from the fifth second on, past the climb an encoder's start makes.
        let mut run = |name: &str| {
            let period = Duration::from_secs(1) / 60;
            let (mut lowest, mut times) = (u32::MAX, Vec::with_capacity(2400));
            let mut next = Instant::now();
            for index in 0..2400 {
                lowlat_common::clock::precise_sleep(next.saturating_duration_since(Instant::now()));
                next += period;
                let frame = source.acquire();
                let began = Instant::now();
                encoder.submit(&frame, index == 0).expect("a submit");
                encoder.wait_finished(Duration::from_millis(100));
                while !matches!(
                    encoder.poll().expect("a poll"),
                    lowlat_encode::Poll::Ready { .. }
                ) {
                    std::hint::spin_loop();
                }
                times.push(began.elapsed());
                if index >= 300 && index % 6 == 0 {
                    lowest = lowest.min(nvml.memory_clock(gpu).expect("the clock now"));
                }
            }
            times.sort_unstable();
            println!(
                "{name}: memory clock at least {lowest} MHz, encode p50 {:?} p99 {:?}",
                times[times.len() / 2],
                times[times.len() * 99 / 100]
            );
            lowest
        };
        let held = clock.hold(&adapter).expect("the hold");
        let lowest_held = run("held");
        drop(held);
        let lowest_free = run("given back");
        assert!(
            lowest_held >= floor,
            "held, the clock fell to {lowest_held} MHz, below the floor of {floor}"
        );
        assert!(
            lowest_free < floor,
            "given back, the clock never fell below the floor of {floor} MHz: the load held \
             it, so the held half showed nothing"
        );
    }
}
