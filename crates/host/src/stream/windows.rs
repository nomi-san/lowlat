//! What a stream is built on, on Windows: the vendor's encoder on the D3D11
//! device of the output being captured, fed by the display's conversion, or
//! by the synthetic source where no display is asked for. The loop above is
//! written once; a platform writes this module.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;

use lowlat_capture::d3d11::Layout;
use lowlat_common::clock::{self, Time};
use lowlat_core::control::status;
use lowlat_vdd_ctl::Mode;

use super::{
    Backend, Codec, Config, DISPLAY_WAIT, Declared, Exit, FromDevice, Join, Roster, Shared,
    encode_loop, own_refresh, own_size, settle_pace, start_bps,
};
use crate::display::own::FIRST_SIZE;
use crate::display::{Capturable, Display, VIRTUAL};

/// How often the host's own display is looked at while nobody is seated.
const OWN_LOOK_MS: f64 = 1000.0;

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
            if !self.display.hold(Mode::new(width, height, refresh)) && !self.display.plugged() {
                return;
            }
        }
        let current = self
            .display
            .wanted()
            .map_or(FIRST_SIZE, |mode| (mode.width, mode.height));
        // **Only for the display the stream is about to capture**: a guest
        // arriving to watch another output says nothing about this one.
        let joining = joining.filter(|_| Display::captures_own(config.output.as_deref()));
        let (width, height) = own_size(current, asked, joining);
        self.display.hold(Mode::new(width, height, refresh));
    }
}

/// The vendor whose encoder this platform builds.
const NVIDIA: u32 = 0x10de;

/// Whether this platform builds an encoder on `adapter`: the vendor's, on a
/// GPU that renders.
fn encodes_on(adapter: &lowlat_drivers::d3d11::Adapter) -> bool {
    adapter.vendor == NVIDIA && adapter.decodes_here()
}

/// Build the stream's device and encoder for `config` and run it until it
/// exits: with a display, everything on the device of the output's own
/// adapter; without one, the vendor's encoder on a device of its own and the
/// generator its source. The open backend by name is refused rather than
/// answered with something else.
pub(super) fn build(
    shared: &Arc<Shared>,
    arrivals: &mpsc::Receiver<Join>,
    config: &Config,
    roster: &mut Roster,
    own: &mut Own,
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
        run_display(shared, arrivals, config.clone(), roster, own)
    } else {
        run_vendor(shared, arrivals, config.clone(), roster)
    }
}

/// Whether the encoder codes full chroma at the running depth, or which gate
/// refused it.
///
/// **Only from the display's conversion**, which writes the packed words the
/// encoder takes on this device -- AYUV at eight bits, Y410 at ten, each
/// carried through a decoder and back -- where the generator's frames carry
/// two planes. Asked once per run of the device: the answers are its
/// properties.
pub(super) fn chroma_census(config: &Config) -> Result<(), &'static str> {
    if !config.display {
        return Err("the synthetic source, whose frames carry two planes");
    }
    let Ok(d3d11) = lowlat_encode::d3d11::D3d11::load() else {
        return Err("the graphics libraries, which could not be loaded");
    };
    let Some(adapter) = d3d11.adapters().ok().and_then(|adapters| {
        adapters
            .into_iter()
            .find(|adapter| adapter.decodes_here() && adapter.vendor == NVIDIA)
    }) else {
        // **Absent is not a refusal**: a machine without the vendor's GPU
        // never builds this encoder, so there is no part to fail the census.
        return Ok(());
    };
    let Ok(device) = d3d11.open(adapter.luid) else {
        return Err("the vendor's device, which would not open");
    };
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

/// The desktop: the output first, then its adapter, the device on it, the
/// encoder at the output's size, and only then the duplication and the
/// encoder's inputs on that device (docs/impl-plan-windows.md W2).
fn run_display(
    shared: &Arc<Shared>,
    arrivals: &mpsc::Receiver<Join>,
    config: Config,
    roster: &mut Roster,
    own: &mut Own,
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
        let found = Display::shape_of_display(wanted)
            .and_then(|shape| Display::locate(wanted).map(|output| (output, shape)));
        let elsewhere = match found {
            Some((output, shape)) if encodes_on(&output.adapter) => break (output, shape),
            Some((output, _)) => Some(output),
            None => None,
        };
        if clock::elapsed_ms(began) >= DISPLAY_WAIT.as_secs_f64() * 1000.0 {
            // **The encoder follows the output**, and only the vendor's is
            // built here: an output another vendor's GPU renders is refused
            // rather than copied across, which would measure the copy. A
            // virtual display's output is listed under the GPU that renders
            // it, so it is that GPU's like any other, and refused only where
            // that GPU stays another vendor's.
            if let Some(output) = elsewhere {
                lowlat_common::log_error!(
                    "stream: {} is on {}, which has no encoder this host builds",
                    output.name,
                    output.adapter.description
                );
                return Exit::Failed(status::ENCODER_UNAVAILABLE);
            }
            lowlat_common::log_error!(
                "stream: no output has been part of the desktop for {:.0}s, ending {} guest(s)",
                DISPLAY_WAIT.as_secs_f64(),
                roster.active.len()
            );
            return Exit::Failed(status::CAPTURE_UNAVAILABLE);
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
    let Ok(api) = lowlat_encode::nvenc::Api::load() else {
        lowlat_common::log_error!("stream: encoder runtime unavailable");
        return Exit::Failed(status::ENCODER_UNAVAILABLE);
    };
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
        &encoder,
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
    )
}

/// No display: the vendor's encoder on a device of its own, the generator
/// its source.
fn run_vendor(
    shared: &Arc<Shared>,
    arrivals: &mpsc::Receiver<Join>,
    config: Config,
    roster: &mut Roster,
) -> Exit {
    let Ok(d3d11) = lowlat_encode::d3d11::D3d11::load() else {
        lowlat_common::log_error!("stream: graphics libraries unavailable, nothing will encode");
        return Exit::Failed(status::ENCODER_UNAVAILABLE);
    };
    // **The first of the vendor's adapters that renders**, in the system's
    // high-performance order: with nothing captured, nothing else decides.
    let Some(adapter) = d3d11.adapters().ok().and_then(|adapters| {
        adapters
            .into_iter()
            .find(|adapter| adapter.decodes_here() && adapter.vendor == NVIDIA)
    }) else {
        lowlat_common::log_error!("stream: no adapter of the encoder's vendor renders here");
        return Exit::Failed(status::ENCODER_UNAVAILABLE);
    };
    let device = match d3d11.open(adapter.luid) {
        Ok(device) => device,
        Err(error) => {
            lowlat_common::log_error!("stream: no device on {}, {error}", adapter.luid);
            return Exit::Failed(status::ENCODER_UNAVAILABLE);
        }
    };
    let Ok(api) = lowlat_encode::nvenc::Api::load() else {
        lowlat_common::log_error!("stream: encoder runtime unavailable");
        return Exit::Failed(status::ENCODER_UNAVAILABLE);
    };
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
    encode_loop(shared, arrivals, config, roster, &mut encoder, None)
}

impl FromDevice for lowlat_encode::nvenc::Encoder<'_> {
    fn submit_from_device(
        &mut self,
        registration: &crate::display::Registration,
        force_keyframe: bool,
    ) -> bool {
        self.submit_registered(registration, force_keyframe).is_ok()
    }
}
