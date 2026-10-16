//! The display on Windows: one output's desktop as the system duplicates it,
//! converted on the output's own adapter straight into the encoder's inputs,
//! and the pointer the system draws over it (docs/05-host.md sections 2, 3
//! and 8).
//!
//! **The duplication answers only when something changed**, where a display's
//! vblank answers every refresh, so the loop's present is mapped onto it: a
//! new image is a present that changed the picture; a wait past the frame's
//! deadline that sees no composition for a refresh is a present of the same
//! picture, so a still desktop keeps the frame rate's refine frames and the
//! pointer's cadence; and a frame that carries only the pointer is no present
//! at all, so the wait goes on and still lands on the next composition.
//!
//! **Everything here runs on the stream's own thread.** The duplication, the
//! desktop it reads and the coordinates the pointer is read in are that
//! thread's state ([`lowlat_capture::duplication`]), taken when the display
//! opens and given back when it goes.

use std::sync::Arc;

use std::os::windows::io::AsHandle;

use lowlat_capture::d3d11::{Converter, Layout, Target};
use lowlat_capture::duplication::{
    self, Awake, Aware, Cursor, Duplication, Frame, InputDesktop, Shape,
};
use lowlat_capture::{Placement, Pointer, Reported};
use lowlat_common::clock::{self, Time};
use lowlat_core::video::Rotation;
use lowlat_drivers::d3d11::{Com, D3d11, Device, Fence, Output, Span, Timer};
use lowlat_drivers::ffi::d3d11::{
    DXGI_MODE_ROTATION, DXGI_MODE_ROTATION_ROTATE90, DXGI_MODE_ROTATION_ROTATE270,
    ID3D11ShaderResourceView, ID3D11Texture2D, IDXGIOutput,
};
use lowlat_vdd_ctl::{FramePage, ShapeKind, Snapshot, Waited};

use super::{Acquired, Capturable, Selectable, VIRTUAL};
use crate::timing::Stage;

/// How long after a refusal the duplication is asked for again, while the
/// desktop is out of reach: often enough that the lock screen's arrival
/// shows within a few frames, not so often that a desktop that stays out of
/// reach costs a call every frame.
const RETRY_MS: f64 = 50.0;

/// One refresh, where nothing says: a refresh is how long a wait past a
/// deadline goes before it is a present of the same picture.
const PERIOD_MS: f64 = 1000.0 / 60.0;

/// Pictures converted between one report of where the time went and the
/// next: ten seconds at sixty frames.
const REPORT_EVERY: u32 = 600;

/// How often the compositor's clock is asked again while a display runs.
const CLOCK_CHECK_MS: f64 = 1000.0;

/// The pointer's state must hold for longer than this, with a change pending,
/// before it is said, and is said once (docs/05-host.md section 8.3).
const SETTLE_MS: f64 = 18.0;

/// What went wrong.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub enum Error {
    /// The system's graphics interface, or a call on the device.
    Device(lowlat_drivers::d3d11::Error),
    /// The duplication.
    Duplication(duplication::Error),
    /// No output by that name is part of the desktop, or it moved to
    /// another adapter.
    NoOutput,
    /// The encoder would not take a target.
    Register,
    /// The host's own display's driver: its frame page, or a handle it names.
    Frames(lowlat_vdd_ctl::Error),
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Device(error) => write!(f, "device: {error}"),
            Self::Duplication(error) => write!(f, "duplication: {error}"),
            Self::NoOutput => f.write_str("no such output is part of the desktop"),
            Self::Register => f.write_str("the encoder refused a target on its own device"),
            Self::Frames(error) => write!(f, "the display driver's frames: {error:?}"),
        }
    }
}

impl From<lowlat_vdd_ctl::Error> for Error {
    fn from(error: lowlat_vdd_ctl::Error) -> Self {
        Self::Frames(error)
    }
}

impl std::error::Error for Error {}

impl From<lowlat_drivers::d3d11::Error> for Error {
    fn from(error: lowlat_drivers::d3d11::Error) -> Self {
        Self::Device(error)
    }
}

impl From<duplication::Error> for Error {
    fn from(error: duplication::Error) -> Self {
        Self::Duplication(error)
    }
}

/// What an encoder calls a converted frame it has been handed: one of its
/// inputs, registered once.
pub type Registration = lowlat_encode::nvenc::Input;

/// Whether `wanted` names `name`: the system's own name, its `\\.\` prefix
/// optional, in any case, as the system treats it.
fn named(wanted: &str, name: &str) -> bool {
    wanted
        .strip_prefix(r"\\.\")
        .unwrap_or(wanted)
        .eq_ignore_ascii_case(name)
}

/// Every output the system lists, read per-monitor aware so the rectangles
/// are physical pixels whatever the calling thread was, each lit only with a
/// monitor behind it ([`lit`]).
fn listed() -> Vec<Output> {
    let _aware = Aware::per_monitor();
    lit(
        D3d11::load()
            .and_then(|d3d11| d3d11.outputs())
            .unwrap_or_default(),
        duplication::monitored,
    )
}

/// The outputs as lit: part of the desktop **and** a monitor behind them.
///
/// **The last monitor's cable pulled leaves its output on the desktop**,
/// active and composed for nobody, and no event marks it; so an output whose
/// monitor has left counts as dark here, where every rule that asks whether
/// something is lit reads it -- what can be captured, what is listed, which
/// output a request lands on, and whether the host's own display is wanted.
fn lit(mut outputs: Vec<Output>, monitored: impl Fn(&str) -> bool) -> Vec<Output> {
    for output in &mut outputs {
        output.attached = output.attached && monitored(&output.name);
    }
    outputs
}

/// The size of the picture an output's duplication hands over: the panel's,
/// so on a quarter turn the desktop's sides the other way round.
fn picture_of(output: &Output) -> (u32, u32) {
    let width = u32::try_from(output.right.saturating_sub(output.left)).unwrap_or(0);
    let height = u32::try_from(output.bottom.saturating_sub(output.top)).unwrap_or(0);
    if matches!(
        output.rotation,
        DXGI_MODE_ROTATION_ROTATE90 | DXGI_MODE_ROTATION_ROTATE270
    ) {
        (height, width)
    } else {
        (width, height)
    }
}

/// The turn the system reports, as the video header carries it: the same
/// one-based numbers, unspecified read as upright.
fn rotation_of(rotation: DXGI_MODE_ROTATION) -> Rotation {
    match u8::try_from(rotation).map(Rotation::from_bits) {
        Ok(Rotation::Unknown) | Err(_) => Rotation::None,
        Ok(known) => known,
    }
}

/// Where `output` sits in the desktop the attached outputs make together,
/// measured from that desktop's own corner: a display left of the primary
/// has a negative left edge, which the corner absorbs.
fn place_of(output: &Output, listed: &[Output]) -> Placement {
    let lit = || listed.iter().filter(|o| o.attached);
    let left = lit().map(|o| o.left).min().unwrap_or(output.left);
    let top = lit().map(|o| o.top).min().unwrap_or(output.top);
    let right = lit().map(|o| o.right).max().unwrap_or(output.right);
    let bottom = lit().map(|o| o.bottom).max().unwrap_or(output.bottom);
    let span = |from: i32, to: i32| u32::try_from(to.saturating_sub(from)).unwrap_or(0);
    Placement {
        x: span(left, output.left),
        y: span(top, output.top),
        width: span(output.left, output.right),
        height: span(output.top, output.bottom),
        desktop_width: span(left, right),
        desktop_height: span(top, bottom),
        rotation: rotation_of(output.rotation),
    }
}

/// The output at the desktop's corner, which on this system is the primary
/// display; the first attached one where none sits there.
fn primary(listed: &[Output]) -> Option<&Output> {
    listed
        .iter()
        .filter(|o| o.attached)
        .find(|o| o.left == 0 && o.top == 0)
        .or_else(|| listed.iter().find(|o| o.attached))
}

/// The output a request names, or with none the primary; only one that is
/// part of the desktop. **A name the system renumbered is followed** to the
/// output now showing the monitor it named ([`renamed`]), and [`VIRTUAL`] is
/// whichever output the host's own display has now.
fn chosen<'a>(listed: &'a [Output], wanted: Option<&str>) -> Option<&'a Output> {
    match wanted {
        Some(wanted) if wanted.eq_ignore_ascii_case(VIRTUAL) => {
            let own = own()?;
            listed.iter().find(|o| o.attached && named(&own, &o.name))
        }
        Some(wanted) => listed
            .iter()
            .find(|o| o.attached && named(wanted, &o.name))
            .or_else(|| {
                let now = renamed(wanted)?;
                listed.iter().find(|o| o.attached && named(&now, &o.name))
            }),
        None => primary(listed),
    }
}

/// The monitor behind each output this process captured, by the name the
/// output had then.
///
/// **A name is the system's number for an output, and a driver restart
/// renews every number** -- two displays came back from one as DISPLAY27 and
/// DISPLAY28 -- while the monitors stay where they are. A request names the
/// monitor a person picked, so a stream asked for a name that has gone
/// follows the monitor to its new name rather than waiting for the old one.
static MONITORS: std::sync::Mutex<Vec<(String, String)>> = std::sync::Mutex::new(Vec::new());

/// Remember which monitor the output named `name` shows.
fn remember(name: &str) {
    let Some(monitor) = duplication::shown()
        .into_iter()
        .find(|shown| shown.name.eq_ignore_ascii_case(name))
        .map(|shown| shown.monitor)
        .filter(|monitor| !monitor.is_empty())
    else {
        return;
    };
    let mut held = MONITORS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    held.retain(|(was, _)| !was.eq_ignore_ascii_case(name));
    held.push((name.to_string(), monitor));
}

/// The name the monitor `wanted` named carries now, where `wanted` names no
/// output of its own any more.
fn renamed(wanted: &str) -> Option<String> {
    let held = MONITORS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    follow(&held, &duplication::shown(), wanted)
}

/// Which output shows the monitor `wanted` showed when it was remembered:
/// the monitor's own path, not the output's number.
fn follow(held: &[(String, String)], shown: &[duplication::Shown], wanted: &str) -> Option<String> {
    let monitor = held
        .iter()
        .find(|(was, _)| named(wanted, was))
        .map(|(_, monitor)| monitor)?;
    shown
        .iter()
        .find(|output| !output.monitor.is_empty() && output.monitor == *monitor)
        .map(|output| output.name.clone())
}

/// The host's own display, while one is plugged: the system's name for its
/// output now, which a driver restart or a replug renews, and the driver's
/// slot it is in.
static OWN: std::sync::Mutex<Option<(String, u8)>> = std::sync::Mutex::new(None);

/// Say which output is the host's own display and its driver's slot, or that
/// none is: `DISPLAYn`, the `\\.\` prefix optional.
pub(crate) fn publish_own(own: Option<(&str, u8)>) {
    let bare =
        own.map(|(name, slot)| (name.strip_prefix(r"\\.\").unwrap_or(name).to_string(), slot));
    *OWN.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = bare;
}

/// The output the host's own display has now, if one is plugged.
fn own() -> Option<String> {
    OWN.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .map(|(name, _)| name.clone())
}

/// The driver's slot of the host's own display, when the output named `name`
/// is it.
fn own_slot(name: &str) -> Option<u8> {
    OWN.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .filter(|(own, _)| own.eq_ignore_ascii_case(name))
        .map(|&(_, slot)| slot)
}

/// What an output is asked for by: [`VIRTUAL`] for the host's own display,
/// else the system's own name.
fn id_of(name: &str) -> String {
    if own().is_some_and(|own| own.eq_ignore_ascii_case(name)) {
        VIRTUAL.to_string()
    } else {
        name.to_string()
    }
}

/// A rate the system gave as a ratio, rounded to the nearest as a mode's is.
fn rounded((numerator, denominator): (u32, u32)) -> u32 {
    (numerator + denominator / 2) / denominator.max(1)
}

/// Presents a second, rounded to the nearest, or zero where the system will
/// not say: **the lower of the output's mode and the compositor's clock**,
/// since an output is composed on the compositor's one clock whatever its
/// mode says ([`duplication::composed_rate`]).
fn hz_of(name: &str) -> u32 {
    rate_for(duplication::refresh_of(name).unwrap_or((0, 0)))
}

/// The rate an output whose mode presents at `refresh` presents at: the
/// lower of that and the compositor's clock, zero where neither is known.
fn rate_for(refresh: (u32, u32)) -> u32 {
    let mode = if refresh.1 == 0 { 0 } else { rounded(refresh) };
    let clock = duplication::composed_rate().map_or(0, rounded);
    match (mode, clock) {
        (0, clock) => clock,
        (mode, 0) => mode,
        (mode, clock) => mode.min(clock),
    }
}

/// One refresh in milliseconds, from a rate the system gave as a ratio.
fn period_of((numerator, denominator): (u32, u32)) -> Option<f64> {
    (numerator > 0 && denominator > 0)
        .then(|| 1000.0 * f64::from(denominator) / f64::from(numerator))
}

/// The refresh the loop places its deadlines on, in milliseconds: **the
/// compositor's clock, on which every composition lands** -- an output at a
/// mode slower than the clock is still composed on it, 120 times a second
/// for a 60 Hz virtual display under a 120 Hz primary, measured -- else the
/// output's own mode where the compositor will not say.
fn period_for(refresh: (u32, u32)) -> f64 {
    duplication::composed_rate()
        .and_then(period_of)
        .or_else(|| period_of(refresh))
        .unwrap_or(PERIOD_MS)
}

/// Whether two refresh periods differ by more than a rate's rounding: a mode
/// of 59.94 against 60 is the same rate, 120 against 144 is not.
fn period_moved(was: f64, now: f64) -> bool {
    (was - now).abs() > was * 0.01
}

/// What one look at the duplication found.
#[derive(Debug)]
enum Looked {
    /// A new image, kept for the tick.
    Image(Kept),
    /// A frame of the pointer alone, taken and given back.
    Pointer,
    /// Nothing in the time.
    Nothing,
}

/// A frame a wait kept for the tick: a new image, not yet converted.
#[derive(Debug)]
struct Kept {
    image: Com<ID3D11Texture2D>,
    /// When the system presented it, by the performance counter.
    presented: i64,
    /// When the duplication handed it over.
    at: Time,
    /// How long before `at` it was presented.
    age_ms: f64,
}

/// The pointer's three bits and when they are said (docs/05-host.md section
/// 8): hidden from the duplication's own visibility of it, relative from
/// the show state or a picture of one value, suppressed from the show
/// state's touch flag; and one message once the whole state has held for
/// longer than [`SETTLE_MS`] with a change pending, never for a position
/// alone.
#[derive(Debug, Default)]
struct Settle {
    /// The state and what is pending, one word: the three bits as the wire
    /// carries them, whether the picture is one value, and below them the
    /// changes not yet said.
    word: u16,
    /// The word as last seen while a change is pending, and since when.
    last: Option<(u16, f64)>,
}

/// The duplication says the pointer is not over the picture.
const HIDDEN: u16 = 0x0200;
/// Something wants the pointer's motion.
const RELATIVE: u16 = 0x0100;
/// The system holds the pointer back while a touch or a pen drives.
const SUPPRESSED: u16 = 0x0800;
/// The picture is one value throughout.
const ONE_VALUE: u16 = 0x0400;
/// Pending: a new picture.
const PICTURE: u16 = 0x0002;
/// Pending: relative turned.
const RELATIVE_TURNED: u16 = 0x0004;
/// Pending: relative turned off.
const RELEASED: u16 = 0x0001;
/// Pending: hidden turned.
const HIDDEN_TURNED: u16 = 0x0008;
/// Everything pending.
const PENDING: u16 = 0x00FF;

impl Settle {
    /// Take one look into account: what the frames since the last said of
    /// the overlay and a new picture, and the show state now. Whether a
    /// message is owed, and if so whether it carries a new picture.
    ///
    /// **A change and its undoing inside the window say nothing**: each is
    /// a toggle of the bit and of its pending mark, so the pair cancels.
    fn look(
        &mut self,
        visible: Option<bool>,
        picture: Option<bool>,
        cursor: Cursor,
        now_ms: f64,
    ) -> Option<bool> {
        if let Some(visible) = visible
            && !visible != (self.word & HIDDEN != 0)
        {
            self.word ^= HIDDEN | HIDDEN_TURNED;
        }
        if let Some(one_value) = picture {
            self.word |= PICTURE;
            if one_value != (self.word & ONE_VALUE != 0) {
                self.word ^= ONE_VALUE;
            }
        }
        let relative = self.word & RELATIVE != 0;
        let turn = if self.word & ONE_VALUE != 0 {
            !relative
        } else {
            cursor.showing == relative
        };
        if turn {
            self.word = (self.word & !RELEASED) ^ (RELATIVE | RELATIVE_TURNED);
            if self.word & (RELATIVE | RELATIVE_TURNED) == RELATIVE_TURNED {
                self.word |= RELEASED;
            }
        }
        if cursor.suppressed {
            self.word |= SUPPRESSED;
        } else {
            self.word &= !SUPPRESSED;
        }
        if self.word & PENDING == 0 {
            self.last = None;
            return None;
        }
        let since = match self.last {
            Some((word, since)) if word == self.word => since,
            _ => {
                self.last = Some((self.word, now_ms));
                now_ms
            }
        };
        if now_ms - since <= SETTLE_MS {
            return None;
        }
        let fresh = self.word & PICTURE != 0;
        self.word &= !PENDING;
        self.last = None;
        Some(fresh)
    }

    fn hidden(&self) -> bool {
        self.word & HIDDEN != 0
    }

    fn relative(&self) -> bool {
        self.word & RELATIVE != 0
    }

    fn suppressed(&self) -> bool {
        self.word & SUPPRESSED != 0
    }
}

/// The pointer's picture and what the frames said of it since the last look.
#[derive(Debug)]
struct Watch {
    settle: Settle,
    /// The duplication's visibility of the pointer, from the last frame that
    /// said anything of it; nothing before one has.
    visible: Option<bool>,
    /// A picture not yet looked at, and whether it is one value.
    picture: Option<bool>,
    /// The shape as the system wrote it, its RGBA, and its PNG: allocated
    /// for the largest the wire carries, once.
    raw: Vec<u8>,
    rgba: Vec<u8>,
    png: Vec<u8>,
    used: usize,
    checksum: u32,
    width: u16,
    height: u16,
    hot_x: u16,
    hot_y: u16,
    /// Whether a shape the wire cannot carry has been said, once.
    refused: bool,
}

impl Watch {
    fn new() -> Self {
        let side = duplication::POINTER_LIMIT as usize;
        Self {
            settle: Settle::default(),
            visible: None,
            picture: None,
            raw: vec![0; side * side * 4],
            rgba: Vec::with_capacity(side * side * 4),
            png: vec![
                0;
                lowlat_core::png::upper_bound(
                    duplication::POINTER_LIMIT,
                    duplication::POINTER_LIMIT,
                )
            ],
            used: 0,
            checksum: 0,
            width: 0,
            height: 0,
            hot_x: 0,
            hot_y: 0,
            refused: false,
        }
    }

    /// Take a new shape the system wrote into `raw`: the picture it makes,
    /// named by its PNG, and the hotspot inside it. The same picture at the
    /// same hotspot is not new.
    fn adopt(&mut self, shape: &Shape) {
        let Some((width, height)) = duplication::pointer_rgba(shape, &self.raw, &mut self.rgba)
        else {
            if !self.refused {
                self.refused = true;
                lowlat_common::log_warn!(
                    "display: a pointer the wire cannot carry, w={} h={}, keeping the last",
                    shape.width,
                    shape.height
                );
            }
            return;
        };
        let stride = usize::try_from(width).unwrap_or(0) * 4;
        let Ok(used) = lowlat_core::png::encode(&self.rgba, width, height, stride, &mut self.png)
        else {
            return;
        };
        let checksum = lowlat_core::crc32::of(self.png.get(..used).unwrap_or_default());
        let hot = |at: i32, side: u32| {
            u16::try_from(at.clamp(0, i32::try_from(side).unwrap_or(1) - 1)).unwrap_or(0)
        };
        let (hot_x, hot_y) = (hot(shape.hot_x, width), hot(shape.hot_y, height));
        if (checksum, hot_x, hot_y) == (self.checksum, self.hot_x, self.hot_y) {
            return;
        }
        self.used = used;
        self.checksum = checksum;
        self.width = u16::try_from(width).unwrap_or(u16::MAX);
        self.height = u16::try_from(height).unwrap_or(u16::MAX);
        self.hot_x = hot_x;
        self.hot_y = hot_y;
        self.picture = Some(duplication::one_value(&self.rgba));
    }
}

/// Where the time goes between a composition and the encoder's input.
#[derive(Debug)]
struct Timing {
    /// The system's present to the duplication handing the image over.
    handed: Stage,
    /// The present to the conversion queued on the device.
    queued: Stage,
    /// The conversion on the device's own clock.
    converting: Stage,
    fence: Option<Fence>,
    timer: Option<Timer>,
    /// The last value the fence was asked to reach, and whether a span is
    /// closed and not yet read.
    signalled: u64,
    timing: bool,
    pictures: u32,
}

impl Timing {
    fn new(device: &Device) -> Self {
        Self {
            handed: Stage::new(),
            queued: Stage::new(),
            converting: Stage::new(),
            fence: device.fence(0).ok(),
            timer: device.timer().ok(),
            signalled: 0,
            timing: false,
            pictures: 0,
        }
    }

    /// Read the span last closed, once the device has passed it: whether the
    /// next conversion may be timed, since the timer holds one span.
    fn read(&mut self, device: &Device) -> bool {
        let Some(timer) = &self.timer else {
            return false;
        };
        if !self.timing {
            return true;
        }
        match device.span(timer) {
            Ok(Span::Pending) => return false,
            Ok(Span::Micros(us)) => self.converting.record(f64::from(us) / 1000.0),
            Ok(Span::Unsteady) | Err(_) => {}
        }
        self.timing = false;
        true
    }

    fn report(&mut self, name: &str) {
        self.pictures = self.pictures.wrapping_add(1);
        if self.pictures % REPORT_EVERY != 0 {
            return;
        }
        let (handed, queued, converting) = (
            self.handed.percentiles(),
            self.queued.percentiles(),
            self.converting.percentiles(),
        );
        lowlat_common::log_info!(
            "display: {name} composition to handed over ms p50/p95/p99 {:.3}/{:.3}/{:.3}, \
             to the conversion queued {:.3}/{:.3}/{:.3}, conversion on the device \
             {:.3}/{:.3}/{:.3}, pictures={}",
            handed.p50,
            handed.p95,
            handed.p99,
            queued.p50,
            queued.p95,
            queued.p99,
            converting.p50,
            converting.p95,
            converting.p99,
            self.pictures
        );
    }
}

/// The host's own display read straight from its driver (docs/11-drivers.md
/// section 1.6): the system's own buffers, taken from the driver's process
/// and opened on the stream's device once each, converted into the encoder's
/// inputs. **Nothing here holds the driver up**: it publishes whether or not
/// this reads, and a picture taken too late is caught after its conversion
/// is queued and the latest converted instead.
struct Direct {
    page: FramePage,
    /// What the buffers and the fence below belong to; zero before any.
    generation: u64,
    fence: Option<Fence>,
    /// Each buffer the page named, opened here, and the view it is read
    /// through: made once each for the generation.
    buffers: Vec<Option<(Com<ID3D11Texture2D>, Com<ID3D11ShaderResourceView>)>>,
    /// The picture last taken: its generation, sequence and frame number.
    taken: Option<(u64, u64, u32)>,
    /// A picture published since the last take, seen by a wait.
    pending: bool,
    /// Pictures converted again, their buffer possibly written meanwhile.
    again: u64,
    /// The system's number for the pointer's shape last taken; zero before
    /// any.
    shape: u32,
    /// How long a take stalls after queuing its conversion: a reader made
    /// late on purpose, which the freshness check must catch.
    #[cfg(test)]
    stall_ms: u64,
}

/// Whether a picture of frame number `frame` is the one taken last.
/// **By the frame this took, never by the driver's word that it repeats**: a
/// still desktop's marker repeats the last frame number in another buffer,
/// and when the picture it repeats was never taken, the marker is the only
/// way left for it to reach the encoder.
fn the_same_picture(taken: Option<(u64, u64, u32)>, frame: u32) -> bool {
    taken.is_some_and(|(_, _, last)| last == frame)
}

/// What a take of the direct source found.
enum Took {
    /// A picture converted: the driver's acquire time, by the performance
    /// counter.
    Picture(i64),
    /// The same picture as the last, or none yet.
    Same,
    /// Another size than the display was built for.
    Resized,
}

impl Direct {
    fn attach(slot: u8) -> Result<Self, Error> {
        let page = FramePage::open(slot)?;
        let named = Snapshot::default().buffer.len();
        Ok(Self {
            page,
            generation: 0,
            fence: None,
            buffers: (0..named).map(|_| None).collect(),
            taken: None,
            pending: false,
            again: 0,
            shape: 0,
            #[cfg(test)]
            stall_ms: 0,
        })
    }

    /// What the driver says of the pointer, into `watch`: its visibility --
    /// none with no hardware cursor, the pointer then in the pictures -- and
    /// a shape not taken yet. Nothing before the driver has said anything; a
    /// shape published again since the state was read is taken at the next
    /// look.
    fn note_pointer(&mut self, watch: &mut Watch) {
        let Some(said) = self.page.pointer() else {
            return;
        };
        watch.visible = Some(said.cursor && said.visible);
        if !said.cursor || said.shape == 0 || said.shape == self.shape {
            return;
        }
        let kind = match said.kind {
            Some(ShapeKind::MaskedColour) => duplication::Kind::MaskedColour,
            Some(ShapeKind::Alpha) => duplication::Kind::Colour,
            None => return,
        };
        if self.page.shape(said.shape, &mut watch.raw).is_none() {
            return;
        }
        self.shape = said.shape;
        watch.adopt(&Shape {
            kind,
            width: said.width,
            height: said.height,
            pitch: said.pitch,
            hot_x: i32::try_from(said.hot_x).unwrap_or(0),
            hot_y: i32::try_from(said.hot_y).unwrap_or(0),
        });
    }

    /// Whether the page names a picture this has not taken.
    fn fresh(&self, snapshot: &Snapshot) -> bool {
        snapshot.generation != 0
            && snapshot.picture.sequence != 0
            && self
                .taken
                .map(|(generation, sequence, _)| (generation, sequence))
                != Some((snapshot.generation, snapshot.picture.sequence))
    }

    /// The generation `snapshot` names, its fence taken and opened: what was
    /// open of another is let go. **Handles are taken by value and the value
    /// may have been reused by the time it is**, so the page is read again
    /// after and a generation that moved is not trusted.
    fn renew(&mut self, device: &Device, snapshot: &Snapshot) -> Result<bool, Error> {
        if self.generation == snapshot.generation && self.fence.is_some() {
            return Ok(true);
        }
        self.fence = None;
        self.buffers.iter_mut().for_each(|buffer| *buffer = None);
        self.generation = 0;
        let taken = self.page.pull(snapshot.fence)?;
        let fence = device.open_shared_fence(taken.as_handle())?;
        if self.page.read().map(|now| now.generation) != Some(snapshot.generation) {
            return Ok(false);
        }
        self.fence = Some(fence);
        self.generation = snapshot.generation;
        Ok(true)
    }

    /// Buffer `index` of the current generation, opened here the first time.
    fn view(
        &mut self,
        device: &Device,
        snapshot: &Snapshot,
        index: u32,
    ) -> Result<Option<&Com<ID3D11ShaderResourceView>>, Error> {
        let at = usize::try_from(index).unwrap_or(usize::MAX);
        let opened = self.buffers.get(at).is_some_and(Option::is_some);
        if !opened {
            let Some(value) = snapshot.buffer(index) else {
                return Ok(None);
            };
            let taken = self.page.pull(value)?;
            let texture = device.open_shared_nt(taken.as_handle())?;
            if self.page.read().map(|now| now.generation) != Some(self.generation) {
                return Ok(None);
            }
            let view = Converter::view_of(device, texture.as_ptr())?;
            if let Some(slot) = self.buffers.get_mut(at) {
                *slot = Some((texture, view));
            }
        }
        Ok(self
            .buffers
            .get(at)
            .and_then(Option::as_ref)
            .map(|(_, view)| view))
    }

    /// Convert the latest picture into `target`, unless it is the one taken
    /// last; when the driver has since acquired enough newer ones that the
    /// buffer may have been written again, the latest instead, counted.
    fn take(
        &mut self,
        device: &Device,
        converter: &Converter,
        target: &Target,
        size: (u32, u32),
    ) -> Result<Took, Error> {
        let Some(snapshot) = self.page.read() else {
            return Ok(Took::Same);
        };
        if snapshot.generation == 0 || snapshot.picture.sequence == 0 {
            return Ok(Took::Same);
        }
        if (snapshot.width, snapshot.height) != size {
            return Ok(Took::Resized);
        }
        if !self.fresh(&snapshot) {
            return Ok(Took::Same);
        }
        let picture = snapshot.picture;
        let same = the_same_picture(self.taken, picture.frame);
        self.taken = Some((snapshot.generation, picture.sequence, picture.frame));
        if same {
            return Ok(Took::Same);
        }
        let mut converted = self.convert(device, converter, target, &snapshot)?;
        #[cfg(test)]
        if self.stall_ms > 0 {
            std::thread::sleep(core::time::Duration::from_millis(self.stall_ms));
        }
        if let Some(now) = self.page.read()
            && now.overtaken(snapshot.generation, picture.sequence)
            && now.generation == snapshot.generation
        {
            self.again += 1;
            self.taken = Some((now.generation, now.picture.sequence, now.picture.frame));
            if let Some(acquired) = self.convert(device, converter, target, &now)? {
                converted = Some(acquired);
            }
        }
        Ok(converted.map_or(Took::Same, Took::Picture))
    }

    /// Queue the conversion of `snapshot`'s picture behind the driver's fence:
    /// its acquire time, or none where its buffer could not be opened.
    fn convert(
        &mut self,
        device: &Device,
        converter: &Converter,
        target: &Target,
        snapshot: &Snapshot,
    ) -> Result<Option<i64>, Error> {
        if !self.renew(device, snapshot)? {
            return Ok(None);
        }
        let picture = snapshot.picture;
        let Some(view) = self.view(device, snapshot, picture.index)? else {
            return Ok(None);
        };
        let view = view.clone_ref();
        let Some(fence) = &self.fence else {
            return Ok(None);
        };
        // **Only a value the driver has published**, and so already queued to
        // be signalled behind the system's write of this buffer.
        device.wait(fence, picture.sequence)?;
        converter.convert_view(device, &view, target)?;
        Ok(Some(i64::try_from(picture.acquired).unwrap_or(i64::MAX)))
    }
}

/// The desktop, ready to hand pictures to an encoder.
///
/// **Fields drop in the order written**: the duplication first, so the frame
/// it holds goes back before anything it lent is let go, and the thread's own
/// state last, after everything made under it.
pub struct Display {
    duplication: Option<Duplication>,
    /// The host's own display read straight from its driver, in place of the
    /// duplication: opened so ([`Display::open_direct`]), or the host's own
    /// display whose driver's frames could be taken.
    direct: Option<Direct>,
    kept: Option<Kept>,
    converter: Converter,
    targets: Vec<Target>,
    registrations: Vec<Registration>,
    device: Arc<Device>,
    d3d11: D3d11,
    interface: Com<IDXGIOutput>,
    /// The output as it was when the display opened.
    output: Output,
    /// Whether that output is the host's own display, asked for as
    /// [`VIRTUAL`].
    own: bool,
    place: Placement,
    /// The picture size the targets and the encoder were built for.
    size: (u32, u32),
    /// The refresh the deadlines are placed on ([`period_for`]).
    period_ms: f64,
    /// The rate the output's mode presents at, as the duplication reported
    /// it; and the refresh and the rate first found for this display, which
    /// the stream was paced against -- either one different later is a
    /// rebuild.
    mode_refresh: (u32, u32),
    settled: Option<(f64, u32)>,
    /// When the compositor's clock was last asked whether it moved.
    clock_checked: Time,
    /// Pictures converted, which names the next target.
    next: usize,
    /// When the wait for a present past the deadline began.
    waiting_since: Option<Time>,
    /// What the last acquire converted, for `converted` to hand back.
    flight: Option<Acquired>,
    /// When the duplication was last refused, while it is.
    refused_at: Option<Time>,
    last_refusal: Option<duplication::Error>,
    /// The duplication was lost to a switch of desktop, so the next is made
    /// on the input desktop.
    reattach: bool,
    resized: bool,
    watch: Watch,
    timing: Timing,
    /// What the pointer's settling is measured from.
    origin: Time,
    _awake: Awake,
    desktop: Option<InputDesktop>,
    /// When the pointer's read last moved this thread to the input desktop.
    desktop_tried: Option<Time>,
    _aware: Aware,
}

impl core::fmt::Debug for Display {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Display")
            .field("output", &self.output.name)
            .field("size", &self.size)
            .field("duplicated", &self.duplication.is_some())
            .finish_non_exhaustive()
    }
}

impl Display {
    /// Every output that is part of the desktop, by the name one is asked for
    /// by, at the size its picture is.
    pub fn outputs() -> Vec<Selectable> {
        let listed = listed();
        listed
            .iter()
            .filter(|o| o.attached)
            .map(|o| {
                let (width, height) = picture_of(o);
                Selectable {
                    id: id_of(&o.name),
                    connector: o.name.clone(),
                    width,
                    height,
                    refresh_hz: hz_of(&o.name),
                    place: Some(place_of(o, &listed)),
                }
            })
            .collect()
    }

    /// Whether this process could capture right now. **Nothing lit** is no
    /// output that is part of the desktop. **Not reachable** is an output
    /// whose desktop this process cannot see: one outside the console's
    /// session, which a service and a remote session are.
    pub fn capturable() -> Capturable {
        if !listed().iter().any(|o| o.attached) {
            Capturable::NothingLit
        } else if !duplication::in_console_session() {
            Capturable::NotReachable
        } else {
            Capturable::Yes
        }
    }

    /// The output this host captures when it is asked for nothing: the one
    /// at the desktop's corner, from the same rule [`Display::open`] follows.
    pub fn preferred() -> Option<String> {
        primary(&listed()).map(|o| id_of(&o.name))
    }

    /// The picture the output asked for hands over, and how many times a
    /// second it presents, before anything is built on it.
    pub fn shape_of_display(wanted: Option<&str>) -> Option<(u32, u32, u32)> {
        let listed = listed();
        let output = chosen(&listed, wanted)?;
        let (width, height) = picture_of(output);
        Some((width, height, hz_of(&output.name)))
    }

    /// Whether a request -- a name, or none for the primary -- lands on the
    /// host's own display.
    pub fn captures_own(wanted: Option<&str>) -> bool {
        let listed = listed();
        chosen(&listed, wanted).is_some_and(|output| id_of(&output.name) == VIRTUAL)
    }

    /// The output a request names, or the primary, with the adapter that
    /// scans it out: what decides the device everything is built on.
    pub fn locate(wanted: Option<&str>) -> Option<Output> {
        let found = chosen(&listed(), wanted).cloned()?;
        if let Some(wanted) = wanted
            && !wanted.eq_ignore_ascii_case(VIRTUAL)
            && !named(wanted, &found.name)
        {
            lowlat_common::log_info!(
                "display: {wanted} is {} now, the same monitor renumbered",
                found.name
            );
        }
        Some(found)
    }

    /// Prepare to capture the output named `name` on `device`, its own
    /// adapter's, into targets in `layout` for a `size` picture, each
    /// registered with `encoder` once. **The host's own display is read
    /// straight from its driver** when the driver's frames can be taken, as
    /// [`Display::open_direct`] reads it, and duplicated otherwise.
    ///
    /// **A desktop out of reach is not a failure here.** The lock screen or
    /// an elevation prompt may be up, and a process that cannot reach it
    /// answers the duplication with a refusal that passes; the display opens
    /// without it and asks again as it goes, which is the loop's held path.
    pub fn open(
        device: &Arc<Device>,
        name: &str,
        layout: Layout,
        size: (u32, u32),
        encoder: &lowlat_encode::nvenc::Encoder<'_>,
    ) -> Result<Self, Error> {
        Self::open_with(device, name, layout, size, encoder, None)
    }

    /// Prepare to capture the host's own display, whose output is `name`,
    /// straight from its driver's monitor at `slot` rather than through the
    /// system's duplication of it (docs/11-drivers.md section 1.6), into the
    /// same targets. **The process must be able to take handles from the
    /// driver's**, which the system account can.
    pub fn open_direct(
        device: &Arc<Device>,
        name: &str,
        layout: Layout,
        size: (u32, u32),
        encoder: &lowlat_encode::nvenc::Encoder<'_>,
        slot: u8,
    ) -> Result<Self, Error> {
        Self::open_with(device, name, layout, size, encoder, Some(slot))
    }

    fn open_with(
        device: &Arc<Device>,
        name: &str,
        layout: Layout,
        size: (u32, u32),
        encoder: &lowlat_encode::nvenc::Encoder<'_>,
        direct: Option<u8>,
    ) -> Result<Self, Error> {
        let aware = Aware::per_monitor();
        let d3d11 = D3d11::load()?;
        let listed = d3d11.outputs()?;
        let (interface, output) = d3d11
            .output(name)?
            .filter(|(_, o)| o.attached && o.adapter.luid == device.adapter.luid)
            .ok_or(Error::NoOutput)?;
        let converter = Converter::new(device, layout)?;
        let mut targets = Vec::with_capacity(lowlat_encode::nvenc::IN_FLIGHT);
        let mut registrations = Vec::with_capacity(lowlat_encode::nvenc::IN_FLIGHT);
        for _ in 0..lowlat_encode::nvenc::IN_FLIGHT {
            let target = Target::new(device, layout, size.0, size.1)?;
            registrations.push(
                encoder
                    .register_texture(target.texture())
                    .map_err(|_| Error::Register)?,
            );
            targets.push(target);
        }
        let own = own().is_some_and(|own| own.eq_ignore_ascii_case(&output.name));
        let mut display = Self {
            duplication: None,
            direct: None,
            kept: None,
            converter,
            targets,
            registrations,
            device: Arc::clone(device),
            place: place_of(&output, &listed),
            interface,
            output,
            own,
            d3d11,
            size,
            period_ms: PERIOD_MS,
            mode_refresh: (0, 0),
            settled: None,
            clock_checked: Time::now(),
            next: 0,
            waiting_since: None,
            flight: None,
            refused_at: None,
            last_refusal: None,
            reattach: false,
            resized: false,
            watch: Watch::new(),
            timing: Timing::new(device),
            origin: Time::now(),
            _awake: Awake::hold(),
            desktop: None,
            desktop_tried: None,
            _aware: aware,
        };
        remember(name);
        // **The host's own display is read straight from its driver whenever
        // the driver answers** -- which it does to the system account the
        // host runs as -- and duplicated otherwise, as any display is.
        let attached = match direct {
            Some(slot) => Some((slot, Direct::attach(slot)?)),
            None => own_slot(&display.output.name).and_then(|slot| match Direct::attach(slot) {
                Ok(attached) => Some((slot, attached)),
                Err(error) => {
                    lowlat_common::log_info!(
                        "display: {name} duplicated, its driver's frames not taken, {error}"
                    );
                    None
                }
            }),
        };
        if let Some((slot, attached)) = attached {
            if let Some(snapshot) = attached.page.read() {
                display.mode_refresh = snapshot.refresh;
            }
            display.direct = Some(attached);
            display.repace();
            lowlat_common::log_info!("display: {name} read straight from its driver, slot={slot}");
        } else {
            match display.duplicate() {
                Ok(()) => {}
                Err(Error::Duplication(duplication::Error::Busy(_) | duplication::Error::Lost)) => {
                    lowlat_common::log_info!(
                        "display: the desktop of {name} is out of reach for now, asking again as it goes"
                    );
                }
                Err(error) => return Err(error),
            }
        }
        lowlat_common::log_info!(
            "display: {name} {}x{} at {},{} of a {}x{} desktop, turned {:?}, {} target(s) of \
             {layout:?} on {}",
            size.0,
            size.1,
            display.place.x,
            display.place.y,
            display.place.desktop_width,
            display.place.desktop_height,
            display.place.rotation,
            display.targets.len(),
            display.device.adapter.description
        );
        Ok(display)
    }

    /// Make the duplication, where there is none: on the input desktop when
    /// the one this thread is on refuses, which is what a switch to the lock
    /// screen or an elevation prompt and back looks like. Asked at most once
    /// a [`RETRY_MS`] while it is refused.
    fn duplicate(&mut self) -> Result<(), Error> {
        if self.duplication.is_some() {
            return Ok(());
        }
        if let (Some(at), Some(refusal)) = (self.refused_at, self.last_refusal)
            && clock::elapsed_ms(at) < RETRY_MS
        {
            return Err(Error::Duplication(refusal));
        }
        // **A duplication lost to a switch is made on the input desktop**,
        // never on the one this thread was on: made there, it is lost again
        // at its first frame, for as long as the switch lasts.
        let lost = core::mem::replace(&mut self.reattach, false);
        if lost {
            match InputDesktop::attach() {
                Ok(desktop) => self.desktop = Some(desktop),
                Err(error) => {
                    lowlat_common::log_warn!(
                        "display: the input desktop could not be attached, {error}"
                    );
                }
            }
        }
        let made = match Duplication::new(&self.device, &self.interface) {
            Err(duplication::Error::Busy(_) | duplication::Error::Lost) => {
                InputDesktop::attach().and_then(|desktop| {
                    // The desktop this thread left is closed with the old
                    // handle: the thread is on the new one now.
                    self.desktop = Some(desktop);
                    Duplication::new(&self.device, &self.interface)
                })
            }
            other => other,
        };
        match made {
            Ok(made) => {
                let mode = made.mode();
                self.mode_refresh = mode.refresh;
                self.repace();
                // **A picture of another size, or turned another way, is a
                // rebuild**: the targets and the encoder are built for one,
                // and the turn is what the place declares.
                if (mode.width, mode.height) != self.size || mode.rotation != self.output.rotation {
                    lowlat_common::log_info!(
                        "display: {} is {}x{} turned {} now, was {}x{} turned {}",
                        self.output.name,
                        mode.width,
                        mode.height,
                        mode.rotation,
                        self.size.0,
                        self.size.1,
                        self.output.rotation
                    );
                    self.resized = true;
                }
                // **Said once a switch, with the desktop it landed on**: a
                // switch the duplication follows at once is otherwise silent,
                // and the lock screen and the way back look like nothing.
                let refused = self.refused_at.take().is_some();
                if lost || refused {
                    let on = self
                        .desktop
                        .as_ref()
                        .map_or_else(|| "unattached".to_string(), InputDesktop::name);
                    lowlat_common::log_info!(
                        "display: {} duplicated again, desktop={on}",
                        self.output.name
                    );
                }
                self.last_refusal = None;
                self.converter.forget();
                self.duplication = Some(made);
                Ok(())
            }
            Err(refusal) => {
                self.refused_at = Some(Time::now());
                self.last_refusal = Some(refusal);
                Err(Error::Duplication(refusal))
            }
        }
    }

    /// Take the refresh the deadlines sit on and the rate the output presents
    /// at, from its mode and the compositor's clock as they are now. **A rate
    /// or a refresh other than the one the stream was paced against is a
    /// rebuild**, as a size is: the pace was settled from the first, and a
    /// refresh changed alone -- the output's mode, or the clock under it --
    /// otherwise leaves the stream paced and budgeted for a rate nothing is
    /// composed at, its deadlines off the compositions.
    fn repace(&mut self) {
        let (period_ms, rate) = (period_for(self.mode_refresh), rate_for(self.mode_refresh));
        match self.settled {
            None => self.settled = Some((period_ms, rate)),
            Some((was, was_rate)) if period_moved(was, period_ms) || was_rate != rate => {
                lowlat_common::log_info!(
                    "display: {} presents at {rate} composed every {period_ms:.3} ms now, was \
                     {was_rate} every {was:.3}",
                    self.output.name
                );
                self.settled = Some((period_ms, rate));
                self.resized = true;
            }
            Some(_) => {}
        }
        self.period_ms = period_ms;
    }

    /// The duplication answered with a failure: let it go, and with it what
    /// it lent, to be made again as it goes.
    fn lose(&mut self, error: duplication::Error) {
        if error == duplication::Error::Lost {
            self.reattach = true;
        } else {
            lowlat_common::log_warn!("display: {} duplication failed, {error}", self.output.name);
        }
        self.kept = None;
        self.duplication = None;
        self.converter.forget();
    }

    /// What one frame says of the pointer, taken while the frame is held: its
    /// visibility, and a new shape when it carries one.
    fn note_pointer(watch: &mut Watch, source: &Duplication, frame: &Frame) {
        let Some(seen) = frame.pointer else {
            return;
        };
        watch.visible = Some(seen.visible);
        if seen.shape_bytes > 0 {
            match source.shape(&mut watch.raw, seen.shape_bytes) {
                Ok(shape) => watch.adopt(&shape),
                Err(error) => {
                    lowlat_common::log_warn!(
                        "display: the pointer's shape could not be read, {error}"
                    );
                }
            }
        }
    }

    /// Where the captured output sits in the desktop.
    pub fn place(&self) -> Option<Placement> {
        Some(self.place)
    }

    /// What is being captured, by the name [`Display::outputs`] gives.
    pub fn selected(&self) -> Option<&str> {
        Some(if self.own { VIRTUAL } else { &self.output.name })
    }

    /// The registration the last conversion wrote into.
    pub fn presented(&self) -> Option<&Registration> {
        let slot = self.next.checked_sub(1)? % self.registrations.len().max(1);
        self.registrations.get(slot)
    }

    /// Convert the image a wait kept, or one composed since, into the next
    /// target, and let the frame go at once: the conversion is queued before
    /// the release, which is the texture's own lock and so ordered on the
    /// device behind it. **Nothing new is not a failure**: the last picture
    /// converted stands, unchanged. A desktop out of reach is, and the loop
    /// holds the last picture while it is.
    pub fn acquire(&mut self) -> Result<Time, Error> {
        self.waiting_since = None;
        self.flight = None;
        // **The compositor's clock can move under an unchanged output** --
        // the primary display set to another rate -- and nothing here is
        // told, so it is asked again on the slow cadence.
        if clock::elapsed_ms(self.clock_checked) >= CLOCK_CHECK_MS {
            self.clock_checked = Time::now();
            if self.settled.is_some() {
                self.repace();
            }
        }
        if self.direct.is_some() {
            return self.acquire_direct();
        }
        self.duplicate()?;
        let kept = match self.kept.take() {
            Some(kept) => kept,
            // A composition since the wait gave up, asked for without
            // waiting.
            None => match self.look(0)? {
                Looked::Image(kept) => kept,
                Looked::Pointer | Looked::Nothing => return Ok(Time::now()),
            },
        };
        let slot = self.next % self.targets.len().max(1);
        let target = self.targets.get(slot).ok_or(Error::Register)?;
        let timed = self.timing.read(&self.device);
        if timed && let Some(timer) = &self.timing.timer {
            self.device.begin_span(timer);
        }
        let converted = self
            .converter
            .convert(&self.device, kept.image.as_ptr(), target);
        if timed
            && let (Some(fence), Some(timer)) = (&self.timing.fence, &self.timing.timer)
            && self
                .device
                .signal_timed(fence, self.timing.signalled + 1, timer)
                .is_ok()
        {
            self.timing.signalled += 1;
            self.timing.timing = true;
        }
        if let Some(duplication) = self.duplication.as_mut() {
            duplication.release();
        }
        converted?;
        self.next = self.next.wrapping_add(1);
        self.timing
            .queued
            .record(duplication::age_ms(kept.presented));
        self.timing.report(&self.output.name);
        self.flight = Some(Acquired {
            at: kept.at,
            changed: true,
            composed_before_ms: Some(kept.age_ms),
        });
        Ok(kept.at)
    }

    /// The next frame within `timeout_ms`, kept if it carries an image; what
    /// it says of the pointer is taken either way, and a frame of the pointer
    /// alone goes back at once.
    fn look(&mut self, timeout_ms: u32) -> Result<Looked, Error> {
        let Some(source) = self.duplication.as_mut() else {
            return Ok(Looked::Nothing);
        };
        match source.next(timeout_ms) {
            Ok(Some(frame)) => {
                Self::note_pointer(&mut self.watch, source, &frame);
                match frame.image {
                    Some(image) => {
                        let age_ms = duplication::age_ms(frame.presented);
                        self.timing.handed.record(age_ms);
                        Ok(Looked::Image(Kept {
                            image,
                            presented: frame.presented,
                            at: Time::now(),
                            age_ms,
                        }))
                    }
                    None => {
                        source.release();
                        Ok(Looked::Pointer)
                    }
                }
            }
            Ok(None) => Ok(Looked::Nothing),
            Err(error) => {
                self.lose(error);
                Err(Error::Duplication(error))
            }
        }
    }

    /// [`Display::acquire`] for the host's own display read from its driver:
    /// the latest picture converted into the next target, unless it is the
    /// one taken last. Another size is a rebuild, as the duplication's is.
    fn acquire_direct(&mut self) -> Result<Time, Error> {
        // **The refresh comes with each swapchain**, and one can change with
        // the size unchanged: the pace and the deadlines settle again from
        // it, as the duplication's mode does at each duplication.
        let refresh = self
            .direct
            .as_ref()
            .and_then(|direct| direct.page.read())
            .filter(|snapshot| snapshot.generation != 0 && snapshot.refresh.1 != 0)
            .map(|snapshot| snapshot.refresh);
        if let Some(refresh) = refresh
            && refresh != self.mode_refresh
        {
            self.mode_refresh = refresh;
            self.repace();
        }
        let Some(direct) = self.direct.as_mut() else {
            return Ok(Time::now());
        };
        direct.pending = false;
        let slot = self.next % self.targets.len().max(1);
        let target = self.targets.get(slot).ok_or(Error::Register)?;
        // Timed only where a conversion will be queued: the timer holds one
        // span, which an empty one would spend.
        let seen = direct.page.read().filter(|snapshot| {
            direct.fresh(snapshot)
                && !the_same_picture(direct.taken, snapshot.picture.frame)
                && (snapshot.width, snapshot.height) == self.size
        });
        let timed = seen.is_some() && self.timing.read(&self.device);
        if timed && let Some(timer) = &self.timing.timer {
            self.device.begin_span(timer);
        }
        let took = direct.take(&self.device, &self.converter, target, self.size);
        if timed
            && let (Some(fence), Some(timer)) = (&self.timing.fence, &self.timing.timer)
            && self
                .device
                .signal_timed(fence, self.timing.signalled + 1, timer)
                .is_ok()
        {
            self.timing.signalled += 1;
            self.timing.timing = true;
        }
        let acquired = match took? {
            Took::Picture(acquired) => acquired,
            Took::Same => return Ok(Time::now()),
            Took::Resized => {
                lowlat_common::log_info!(
                    "display: {} is another size now, was {}x{}",
                    self.output.name,
                    self.size.0,
                    self.size.1
                );
                self.resized = true;
                return Ok(Time::now());
            }
        };
        let at = Time::now();
        self.next = self.next.wrapping_add(1);
        if let Some(seen) = seen {
            let handed = i64::try_from(seen.picture.acquired).unwrap_or(i64::MAX);
            self.timing.handed.record(duplication::age_ms(handed));
        }
        let age_ms = duplication::age_ms(acquired);
        self.timing.queued.record(age_ms);
        self.timing.report(&self.output.name);
        if self.timing.pictures % REPORT_EVERY == 0
            && let Some(direct) = &self.direct
        {
            lowlat_common::log_info!(
                "display: {} read from its driver, converted again {} of {} pictures",
                self.output.name,
                direct.again,
                self.timing.pictures
            );
        }
        self.flight = Some(Acquired {
            at,
            changed: true,
            composed_before_ms: Some(age_ms),
        });
        Ok(at)
    }

    /// [`Display::wait_present`] for the host's own display read from its
    /// driver: a picture published that has not been taken is a present; a
    /// refresh past the deadline with none is a present of the same picture,
    /// as the duplication's wait has it -- **before the first picture too**,
    /// so the wait always answers within a refresh: unbounded, a stream built
    /// before its display had a picture (plugged again a moment before, or
    /// asleep) found no present at its start, was paced by the clock instead,
    /// and spun, each tick that took nothing waiting again at once.
    fn wait_direct(&mut self, timeout_ms: i32) -> Option<bool> {
        let period = self.period_ms;
        let direct = self.direct.as_mut()?;
        let fresh = |direct: &Direct| direct.page.read().is_some_and(|s| direct.fresh(&s));
        if direct.pending || fresh(direct) {
            direct.pending = true;
            return Some(true);
        }
        let began = *self.waiting_since.get_or_insert_with(Time::now);
        let called = Time::now();
        loop {
            let now = Time::now();
            let bound = period - clock::diff_ms(began, now);
            if bound <= 0.0 {
                return Some(true);
            }
            let budget = f64::from(timeout_ms.max(1)) - clock::diff_ms(called, now);
            if budget <= 0.0 {
                return None;
            }
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "a positive wait of at most a refresh or the loop's timeout"
            )]
            let wait = budget.min(bound).ceil().max(1.0) as u32;
            match direct.page.wait(wait) {
                Waited::Published if fresh(direct) => {
                    direct.pending = true;
                    return Some(true);
                }
                Waited::Published | Waited::TimedOut => {}
                // **The driver's process ended: nothing will be published,
                // and the process answers every wait at once.** The wait runs
                // to its bound as a still desktop's does -- answering at once
                // spun the loop until the output's departure rebuilt it.
                Waited::Ended => {
                    std::thread::sleep(core::time::Duration::from_millis(u64::from(wait)))
                }
            }
        }
    }

    /// What the last acquire converted, if it converted anything: nothing
    /// is a picture that did not change, which the loop reads as such.
    pub fn converted(&mut self) -> Result<Option<Acquired>, Error> {
        Ok(self.flight.take())
    }

    /// The pointer, when a message about it is owed: once its state has held
    /// long enough with a change pending (`Settle`), read on this thread's
    /// desktop. **Nothing outside the captured output**, and nothing is taken
    /// into account while it is there: a pointer on another screen is not
    /// this stream's to describe.
    pub fn pointer(&mut self) -> Option<Pointer> {
        // Read from its driver, the display's pointer is the driver's too:
        // its visibility and shape in place of the duplication's frames.
        if let Some(direct) = self.direct.as_mut() {
            direct.note_pointer(&mut self.watch);
        }
        let cursor = self.cursor()?;
        let output = &self.output;
        if cursor.x < output.left
            || cursor.x >= output.right
            || cursor.y < output.top
            || cursor.y >= output.bottom
        {
            return None;
        }
        let (x, y) = (cursor.x - output.left, cursor.y - output.top);
        let now_ms = clock::elapsed_ms(self.origin);
        let watch = &mut self.watch;
        let picture = watch.picture.take();
        let fresh = watch.settle.look(watch.visible, picture, cursor, now_ms)?;
        Some(Pointer {
            x: x - i32::from(watch.hot_x),
            y: y - i32::from(watch.hot_y),
            width: watch.width,
            height: watch.height,
            checksum: watch.checksum,
            fresh: fresh && watch.used > 0,
            looked: true,
            reported: Some(Reported {
                x: u16::try_from(x).unwrap_or(u16::MAX),
                y: u16::try_from(y).unwrap_or(u16::MAX),
                hot_x: watch.hot_x,
                hot_y: watch.hot_y,
                hidden: watch.settle.hidden(),
                relative: watch.settle.relative(),
                suppressed: watch.settle.suppressed(),
            }),
        })
    }

    /// Where the pointer points and how the system shows it, read on the
    /// input desktop. **Read from its driver, the display's frames need no
    /// desktop**, so nothing else moves this thread when the input desktop
    /// changes -- the lock screen, an elevation prompt -- and a read refused
    /// on the desktop it is on moves it to the input one and asks again, at
    /// most once a [`RETRY_MS`], as the duplication's re-make does.
    fn cursor(&mut self) -> Option<Cursor> {
        if let Some(cursor) = duplication::cursor() {
            return Some(cursor);
        }
        if self.direct.is_none()
            || self
                .desktop_tried
                .is_some_and(|at| clock::elapsed_ms(at) < RETRY_MS)
        {
            return None;
        }
        self.desktop_tried = Some(Time::now());
        match InputDesktop::attach() {
            Ok(desktop) => {
                lowlat_common::log_info!(
                    "display: {} reads the pointer on the input desktop, desktop={}",
                    self.output.name,
                    desktop.name()
                );
                self.desktop = Some(desktop);
            }
            Err(error) => {
                lowlat_common::log_warn!(
                    "display: the input desktop could not be attached for the pointer, {error}"
                );
                return None;
            }
        }
        duplication::cursor()
    }

    /// Wake the display with one pointer move of no distance
    /// (docs/05-host.md section 2), from the desktop that takes the input:
    /// the system inserts it from nowhere else.
    pub fn wake(&mut self) {
        if !duplication::wake() {
            match InputDesktop::attach() {
                Ok(desktop) => self.desktop = Some(desktop),
                Err(error) => {
                    lowlat_common::log_warn!(
                        "display: the input desktop could not be attached to wake {}, {error}",
                        self.output.name
                    );
                    return;
                }
            }
            if !duplication::wake() {
                lowlat_common::log_warn!("display: {} could not be woken", self.output.name);
                return;
            }
        }
        lowlat_common::log_info!("display: {} woken as the stream starts", self.output.name);
    }

    /// Whether something holds the pointer's motion, as of the last look at
    /// it: the bit as it turns, before it has held long enough to be said,
    /// which is what absolute motion is converted on (docs/05-host.md
    /// section 7).
    pub fn relative(&self) -> bool {
        self.watch.settle.relative()
    }

    /// Whether the output being captured is still part of the desktop with a
    /// monitor behind it ([`lit`]), on the adapter it was opened on, and the
    /// device still there.
    pub fn attached(&self) -> bool {
        if self.device.lost() {
            return false;
        }
        match self.d3d11.output(&self.output.name) {
            Ok(Some((_, now))) => {
                now.attached
                    && now.adapter.luid == self.output.adapter.luid
                    && duplication::monitored(&now.name)
            }
            Ok(None) => false,
            // A system that cannot answer is not evidence that the output
            // went, and ending a session on it would be worse.
            Err(_) => true,
        }
    }

    /// Nothing to arm: the duplication's wait is the present.
    pub fn arm_vblank(&self) -> Result<(), Error> {
        Ok(())
    }

    /// The refresh the desktop is composed on, which every picture here says
    /// when it was composed against: the loop places its deadlines on it.
    pub fn refresh_ms(&self) -> Option<f64> {
        Some(self.period_ms)
    }

    /// Wait up to `timeout_ms` for a present past the frame's deadline, which
    /// the loop asks about only once the deadline has passed.
    ///
    /// A new image is kept for the tick: true. A refresh past the deadline
    /// with nothing composed is a present of the same picture: true, and the
    /// tick converts nothing. A frame of the pointer alone is taken and given
    /// back, and the wait goes on to the end of the time: false if that was
    /// all. Nothing at all in the time: nothing. **A duplication lost or out
    /// of reach is a present**, so the tick runs and its acquire asks again.
    pub fn wait_present(&mut self, timeout_ms: i32) -> Option<bool> {
        if self.direct.is_some() {
            return self.wait_direct(timeout_ms);
        }
        if self.kept.is_some() || self.duplication.is_none() {
            return Some(true);
        }
        let began = *self.waiting_since.get_or_insert_with(Time::now);
        let called = Time::now();
        let mut moved = false;
        loop {
            let now = Time::now();
            let bound = self.period_ms - clock::diff_ms(began, now);
            if bound <= 0.0 {
                return Some(true);
            }
            let budget = f64::from(timeout_ms.max(1)) - clock::diff_ms(called, now);
            if budget <= 0.0 {
                return moved.then_some(false);
            }
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "a positive wait of at most a refresh or the loop's timeout"
            )]
            let wait = budget.min(bound).ceil().max(1.0) as u32;
            match self.look(wait) {
                Ok(Looked::Image(kept)) => {
                    self.kept = Some(kept);
                    return Some(true);
                }
                Ok(Looked::Pointer) => moved = true,
                Ok(Looked::Nothing) => {}
                Err(_) => return Some(true),
            }
        }
    }

    /// Nothing to wake: this device has no compute block that sleeps between
    /// frames the way an integrated one's does.
    pub fn poke(&mut self) -> Result<(), Error> {
        Ok(())
    }

    /// Whether the picture changed size or turn, clearing the answer.
    pub fn take_resize(&mut self) -> bool {
        core::mem::replace(&mut self.resized, false)
    }

    /// The picture the last message owed named, in the form the wire
    /// carries it.
    pub fn pointer_image(&self) -> &[u8] {
        self.watch.png.get(..self.watch.used).unwrap_or_default()
    }

    /// Nothing to turn: the system reports the pointer, its picture and its
    /// position in the desktop's own orientation.
    pub fn set_pointer_turn(&mut self, _turn: Rotation) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cursor(showing: bool) -> Cursor {
        Cursor {
            x: 10,
            y: 10,
            showing,
            suppressed: false,
        }
    }

    /// **One message per settled change, and only after the whole state has
    /// held for longer than the window**: the first look at a new picture
    /// owes nothing yet, the state held past the window owes one carrying
    /// the picture, and nothing after it until something else changes.
    #[test]
    fn a_change_is_said_once_after_it_has_held() {
        let mut settle = Settle::default();
        assert_eq!(
            settle.look(Some(true), Some(false), cursor(true), 0.0),
            None
        );
        assert_eq!(settle.look(Some(true), None, cursor(true), SETTLE_MS), None);
        assert_eq!(
            settle.look(Some(true), None, cursor(true), SETTLE_MS + 1.0),
            Some(true),
            "a held change with a picture"
        );
        for at in [40.0, 80.0, 400.0] {
            assert_eq!(settle.look(Some(true), None, cursor(true), at), None);
        }
        assert!(!settle.hidden() && !settle.relative());
    }

    /// **A change and its undoing inside the window say nothing**, where a
    /// change that holds is said with the bit it set.
    #[test]
    fn a_flip_and_back_inside_the_window_says_nothing() {
        let mut settle = Settle::default();
        assert_eq!(settle.look(Some(false), None, cursor(true), 0.0), None);
        assert_eq!(settle.look(Some(true), None, cursor(true), 5.0), None);
        assert_eq!(settle.look(Some(true), None, cursor(true), 100.0), None);
        assert!(!settle.hidden());

        assert_eq!(settle.look(Some(false), None, cursor(true), 200.0), None);
        assert_eq!(
            settle.look(Some(false), None, cursor(true), 230.0),
            Some(false),
            "held, and no picture with it"
        );
        assert!(settle.hidden());
    }

    /// **Relative follows the show state, and a picture of one value forces
    /// it on** whatever the show state says; showing again and a picture
    /// with something to see turn it off.
    #[test]
    fn relative_follows_the_show_state_or_a_picture_of_one_value() {
        let mut settle = Settle::default();
        settle.look(Some(true), None, cursor(false), 0.0);
        assert_eq!(
            settle.look(Some(true), None, cursor(false), 20.0),
            Some(false)
        );
        assert!(settle.relative(), "a hidden pointer is relative");

        settle.look(Some(true), None, cursor(true), 40.0);
        settle.look(Some(true), None, cursor(true), 60.0);
        assert!(!settle.relative(), "shown again it is not");

        settle.look(Some(true), Some(true), cursor(true), 100.0);
        assert_eq!(
            settle.look(Some(true), None, cursor(true), 120.0),
            Some(true)
        );
        assert!(
            settle.relative(),
            "a blank picture is relative though shown"
        );

        settle.look(Some(true), Some(false), cursor(true), 200.0);
        settle.look(Some(true), None, cursor(true), 220.0);
        assert!(!settle.relative(), "a picture to see turns it off");
    }

    /// **Suppression rides with the next message and owes none of its own**,
    /// and a position alone owes nothing either: neither is a change the rule
    /// marks.
    #[test]
    fn suppression_and_position_owe_no_message_of_their_own() {
        let mut settle = Settle::default();
        let mut touched = cursor(true);
        touched.suppressed = true;
        assert_eq!(settle.look(Some(true), None, touched, 0.0), None);
        assert_eq!(settle.look(Some(true), None, touched, 100.0), None);
        assert!(settle.suppressed());
        let mut moved = cursor(true);
        moved.x = 500;
        assert_eq!(settle.look(Some(true), None, moved, 200.0), None);
    }

    /// **A renumbered output is found again by the monitor it shows**, as a
    /// driver restart renews every number while the monitors stay: the name
    /// asked for leads to its monitor's path, and the path to the output
    /// showing it now. A name never captured, a monitor gone, and a path the
    /// system would not give all find nothing.
    #[test]
    fn a_renumbered_output_is_followed_by_its_monitor() {
        let shown = |name: &str, monitor: &str| duplication::Shown {
            name: name.to_string(),
            refresh: (60, 1),
            monitor: monitor.to_string(),
        };
        let held = vec![
            ("DISPLAY1".to_string(), r"\\?\DISPLAY#AAA1#UID1".to_string()),
            ("DISPLAY2".to_string(), r"\\?\DISPLAY#BBB2#UID2".to_string()),
        ];
        let now = [
            shown("DISPLAY28", r"\\?\DISPLAY#AAA1#UID1"),
            shown("DISPLAY27", r"\\?\DISPLAY#BBB2#UID2"),
            shown("DISPLAY29", ""),
        ];
        assert_eq!(
            follow(&held, &now, "DISPLAY2").as_deref(),
            Some("DISPLAY27")
        );
        assert_eq!(
            follow(&held, &now, r"\\.\display1").as_deref(),
            Some("DISPLAY28")
        );
        assert_eq!(follow(&held, &now, "DISPLAY3"), None, "never captured");
        assert_eq!(
            follow(&held, &now[..1], "DISPLAY2"),
            None,
            "the monitor is gone"
        );
        let blank = vec![("DISPLAY9".to_string(), String::new())];
        assert_eq!(
            follow(&blank, &now, "DISPLAY9"),
            None,
            "an empty path is no monitor"
        );
    }

    /// The name a request carries matches the system's own, prefixed or
    /// not, in any case, and nothing else.
    #[test]
    fn an_output_is_asked_for_by_the_systems_own_name() {
        assert!(named("DISPLAY1", "DISPLAY1"));
        assert!(named(r"\\.\DISPLAY1", "DISPLAY1"));
        assert!(named("display1", "DISPLAY1"));
        assert!(!named("DISPLAY12", "DISPLAY1"));
        assert!(!named("DISPLAY", "DISPLAY1"));
    }

    /// **The host's own display is asked for as `virtual`**, whatever the
    /// system numbers its output now, listed under that name and under its
    /// own number nowhere; and with none plugged, `virtual` names nothing
    /// rather than falling back to another screen.
    #[test]
    fn the_host_display_is_asked_for_as_virtual() {
        let listed = [
            output("DISPLAY1", 0, 0, 2560, 1440),
            output("DISPLAY9", 2560, 0, 1920, 1080),
        ];
        publish_own(Some((r"\\.\DISPLAY9", 15)));
        assert_eq!(own_slot("display9"), Some(15));
        assert_eq!(own_slot("DISPLAY1"), None);
        let found = chosen(&listed, Some("virtual")).map(|o| o.name.as_str());
        assert_eq!(found, Some("DISPLAY9"));
        assert_eq!(
            chosen(&listed, Some("VIRTUAL")).map(|o| o.name.as_str()),
            Some("DISPLAY9")
        );
        assert_eq!(id_of("DISPLAY9"), VIRTUAL);
        assert_eq!(id_of("DISPLAY1"), "DISPLAY1");
        publish_own(None);
        assert!(
            chosen(&listed, Some("virtual")).is_none(),
            "virtual fell back to a screen"
        );
        assert_eq!(id_of("DISPLAY9"), "DISPLAY9");
    }

    /// **A refresh moves only past a rate's rounding**: 59.94 against 60 is
    /// the same rate, 120 against 144 is a rebuild.
    #[test]
    fn a_repeat_the_reader_never_took_is_a_new_picture() {
        // The marker repeats frame 381; a reader that last took 380 takes it,
        // one that took 381 already does not.
        assert!(!the_same_picture(Some((8, 300, 380)), 381));
        assert!(the_same_picture(Some((8, 301, 381)), 381));
        assert!(!the_same_picture(None, 381));
    }

    #[test]
    fn a_refresh_moves_only_past_rounding() {
        let sixty = 1000.0 / 60.0;
        let ntsc = 1000.0 * 1001.0 / 60000.0;
        assert!(!period_moved(sixty, ntsc));
        assert!(period_moved(1000.0 / 144.0, 1000.0 / 120.0));
        assert!(period_moved(1000.0 / 120.0, 1000.0 / 60.0));
    }

    /// **An output on the desktop with no monitor behind it is dark**, as the
    /// last monitor's cable pulled leaves one: neither the primary nor
    /// anything a request can land on, and with it the only output, nothing
    /// is lit. An output off the desktop is not asked about.
    #[test]
    fn an_output_with_no_monitor_behind_it_is_dark() {
        let mut off = output("DISPLAY3", 4480, 0, 1920, 1080);
        off.attached = false;
        let listed = lit(
            vec![
                output("DISPLAY1", 0, 0, 2560, 1440),
                output("DISPLAY2", 2560, 0, 1920, 1080),
                off,
            ],
            |name| {
                assert_ne!(name, "DISPLAY3", "an output off the desktop was asked");
                name == "DISPLAY2"
            },
        );
        let shown: Vec<&str> = listed
            .iter()
            .filter(|o| o.attached)
            .map(|o| o.name.as_str())
            .collect();
        assert_eq!(shown, ["DISPLAY2"]);
        assert_eq!(
            primary(&listed).map(|o| o.name.as_str()),
            Some("DISPLAY2"),
            "the primary stayed on an output with nothing behind it"
        );

        let gone = lit(vec![output("DISPLAY1", 0, 0, 2560, 1440)], |_| false);
        assert!(
            primary(&gone).is_none(),
            "an output with no monitor was lit"
        );
        assert!(
            chosen(&gone, Some("DISPLAY1")).is_none(),
            "an output with no monitor could be asked for"
        );
    }

    fn output(name: &str, left: i32, top: i32, width: i32, height: i32) -> Output {
        Output {
            name: name.to_string(),
            left,
            top,
            right: left + width,
            bottom: top + height,
            attached: true,
            rotation: lowlat_drivers::ffi::d3d11::DXGI_MODE_ROTATION_IDENTITY,
            adapter: lowlat_drivers::d3d11::Adapter {
                luid: lowlat_drivers::d3d11::Luid { high: 0, low: 1 },
                vendor: 0x10de,
                device: 0,
                subsystem: 0,
                revision: 0,
                description: String::new(),
                driver: None,
                renders: true,
                software: false,
                indirect: false,
                integrated: false,
            },
        }
    }

    /// **A place is measured from the desktop's own corner**, so a display
    /// left of the primary -- negative on the system's own axes -- moves the
    /// corner rather than going below zero; a turned output's picture is its
    /// desktop rectangle the other way round.
    #[test]
    fn a_place_is_measured_from_the_desktops_corner() {
        let left = output("DISPLAY2", -1536, 200, 1536, 864);
        let primary_one = output("DISPLAY1", 0, 0, 2560, 1440);
        let listed = [primary_one.clone(), left.clone()];
        let place = place_of(&primary_one, &listed);
        assert_eq!((place.x, place.y), (1536, 0));
        assert_eq!((place.desktop_width, place.desktop_height), (4096, 1440));
        let place = place_of(&left, &listed);
        assert_eq!(
            (place.x, place.y, place.width, place.height),
            (0, 200, 1536, 864)
        );
        assert_eq!(primary(&listed).map(|o| o.name.as_str()), Some("DISPLAY1"));

        let mut turned = output("DISPLAY3", 0, 0, 1080, 1920);
        turned.rotation = DXGI_MODE_ROTATION_ROTATE90;
        assert_eq!(picture_of(&turned), (1920, 1080));
        assert_eq!(rotation_of(turned.rotation), Rotation::Deg90);
        assert_eq!(rotation_of(0), Rotation::None, "unspecified is upright");
    }

    /// **The desktop as the loop drives it, without a network**: the primary
    /// output opened on its own adapter's device with the vendor's encoder,
    /// the first present taken at once, then the loop's own sequence -- wait
    /// for a present, acquire, collect the conversion, submit the last
    /// converted picture, collect the encode -- for `LOWLAT_TICKS` ticks
    /// (default 240) at the display's own pace. Every tick must present
    /// within a refresh or two of asking, a still desktop included; the first
    /// picture must be the desktop and every submit an access unit.
    /// `LOWLAT_LAYOUT` names the layout (nv12, p010, ayuv, y410). Needs the
    /// vendor's GPU and an interactive desktop: `cargo test -p lowlat-host
    /// display -- --ignored --nocapture`.
    #[test]
    #[ignore = "requires the vendor's GPU, a display and an interactive desktop"]
    fn the_desktop_converts_and_encodes_at_the_displays_pace() {
        use lowlat_encode::nvenc;
        // As the library and the daemon hold it: without it every wait here
        // ends at the system's 15.6 ms tick.
        let _timer = clock::TimerResolution::raise();
        let ticks: usize = std::env::var("LOWLAT_TICKS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(240);
        let layout = match std::env::var("LOWLAT_LAYOUT").as_deref() {
            Ok("p010") => Layout::P010,
            Ok("ayuv") => Layout::Ayuv,
            Ok("y410") => Layout::Y410,
            _ => Layout::Nv12,
        };
        let output = Display::locate(None).expect("an output");
        let (width, height, hz) = Display::shape_of_display(Some(&output.name)).expect("a shape");
        println!(
            "{} {width}x{height} at {hz} Hz on {}",
            output.name, output.adapter.description
        );
        let d3d11 = D3d11::load().expect("the system's libraries");
        let device = Arc::new(d3d11.open(output.adapter.luid).expect("a device"));
        let api = nvenc::Api::load().expect("the encoder's runtime");
        let mut display: Display;
        let session = api.open_session(Arc::clone(&device)).expect("a session");
        let mut encoder = session
            .initialize(nvenc::Config {
                codec: nvenc::Codec::H265,
                width,
                height,
                fps: hz.max(1),
                bitrate_bps: 20_000_000,
                min_qp: lowlat_encode::DEFAULT_MIN_QP,
                thorough: false,
                chroma: if layout.full_chroma() {
                    nvenc::Chroma::Yuv444
                } else {
                    nvenc::Chroma::Yuv420
                },
                depth: if matches!(layout, Layout::P010 | Layout::Y410) {
                    nvenc::Depth::Ten
                } else {
                    nvenc::Depth::Eight
                },
            })
            .expect("an encoder");
        display = Display::open(&device, &output.name, layout, (width, height), &encoder)
            .expect("the display");

        let began = Time::now();
        let first = display.wait_present(150);
        println!(
            "first present {first:?} after {:.2} ms",
            clock::elapsed_ms(began)
        );
        assert_eq!(first, Some(true), "the first present did not come at once");
        let interval = 1000.0 / f64::from(hz.max(1));
        let (mut changed, mut unchanged, mut units, mut late) = (0, 0, 0, 0);
        let mut waits = Stage::new();
        let mut deadline = clock::elapsed_ms(began);
        for tick in 0..ticks {
            if tick > 0 {
                // The loop's own shape: sleep to the deadline, then wait for
                // the present.
                let until = deadline - clock::elapsed_ms(began);
                if until > 0.0 {
                    clock::precise_sleep(core::time::Duration::from_secs_f64(until / 1000.0));
                }
                let asked = Time::now();
                while display.wait_present(20) != Some(true) {}
                let waited = clock::elapsed_ms(asked);
                waits.record(waited);
                if waited > 2.0 * interval + 2.0 {
                    late += 1;
                }
            }
            deadline += interval;
            display.acquire().expect("an acquire");
            match display.converted().expect("the conversion") {
                Some(got) if got.changed => changed += 1,
                // A present before any picture is one of the same picture, so
                // the first picture's coming at once is a conversion within
                // the first ticks.
                _ => {
                    assert!(
                        changed > 0 || clock::elapsed_ms(began) < 150.0,
                        "the first picture did not come within 150 ms"
                    );
                    unchanged += 1;
                }
            }
            if let Some(input) = display.presented() {
                encoder
                    .submit_registered(input, tick == 0)
                    .expect("a submit");
                loop {
                    match encoder.poll().expect("a collect") {
                        lowlat_encode::Poll::Ready { bitstream, .. } => {
                            assert!(!bitstream.is_empty());
                            units += 1;
                            break;
                        }
                        lowlat_encode::Poll::Pending => std::hint::spin_loop(),
                    }
                }
            }
            if let Some(pointer) = display.pointer() {
                println!("  pointer message at tick {tick}: {:?}", pointer.reported);
            }
        }
        let waited = waits.percentiles();
        println!(
            "{ticks} ticks in {:.0} ms: {changed} changed, {unchanged} the same, {units} units; \
             wait for a present ms p50/p95/p99 {:.2}/{:.2}/{:.2}, {late} late",
            clock::elapsed_ms(began),
            waited.p50,
            waited.p95,
            waited.p99
        );
        let (handed, queued) = (
            display.timing.handed.percentiles(),
            display.timing.queued.percentiles(),
        );
        println!(
            "composition to handed over ms p50/p95/p99 {:.3}/{:.3}/{:.3}; to the conversion \
             queued {:.3}/{:.3}/{:.3}",
            handed.p50, handed.p95, handed.p99, queued.p50, queued.p95, queued.p99
        );
        assert!(changed >= 1, "the desktop was never converted");
        assert_eq!(units, ticks, "a tick went unencoded");
        assert_eq!(late, 0, "a present took longer than two refreshes");
    }

    /// **The host's own display, read straight from its driver, converts and
    /// encodes at the display's pace**: a monitor of the driver plugged by
    /// this test and bound to it, opened through its frame page; the first
    /// present at once, every tick an access unit, none later than two
    /// refreshes. Anything presenting on the display makes the pictures change;
    /// how many were converted again for coming too late is printed. Run as
    /// the system account, the one allowed to take the driver's handles;
    /// `LOWLAT_MODE=WxH@Hz` sets the monitor (1920x1080@60) and
    /// `LOWLAT_SETTLE_MS` how long the desktop is left to go still first
    /// (3000).
    #[test]
    #[ignore = "requires the display driver, the vendor's GPU and the system account"]
    fn the_hosts_own_display_converts_straight_from_its_driver() {
        use lowlat_encode::nvenc;
        use lowlat_vdd_ctl::{Controller, Lifetime, Mode};

        let _timer = clock::TimerResolution::raise();
        let ticks: usize = std::env::var("LOWLAT_TICKS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(240);
        let mode = std::env::var("LOWLAT_MODE")
            .ok()
            .and_then(|text| {
                let (size, hz) = text.split_once('@')?;
                let (width, height) = size.split_once('x')?;
                Some(Mode::new(
                    width.parse().ok()?,
                    height.parse().ok()?,
                    hz.parse().ok()?,
                ))
            })
            .unwrap_or(Mode::new(1920, 1080, 60));
        let mut controller = Controller::open().expect("the display driver");
        let monitor = controller
            .plug(mode, Lifetime::Bound, false, None)
            .expect("a monitor");
        println!("slot {} {} at {}", monitor.slot, monitor.name, monitor.mode);
        // **Asked for once the desktop is still**, when the system hands the
        // driver nothing new: the first picture must still come at once.
        let settle_ms: u64 = std::env::var("LOWLAT_SETTLE_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(3000);
        std::thread::sleep(core::time::Duration::from_millis(settle_ms));
        let output = Display::locate(Some(&monitor.name)).expect("its output");
        let (width, height, hz) = Display::shape_of_display(Some(&output.name)).expect("a shape");
        let d3d11 = D3d11::load().expect("the system's libraries");
        let device = Arc::new(d3d11.open(output.adapter.luid).expect("a device"));
        let api = nvenc::Api::load().expect("the encoder's runtime");
        let mut display: Display;
        let session = api.open_session(Arc::clone(&device)).expect("a session");
        let mut encoder = session
            .initialize(nvenc::Config {
                codec: nvenc::Codec::H265,
                width,
                height,
                fps: hz.max(1),
                bitrate_bps: 20_000_000,
                min_qp: lowlat_encode::DEFAULT_MIN_QP,
                thorough: false,
                chroma: nvenc::Chroma::Yuv420,
                depth: nvenc::Depth::Eight,
            })
            .expect("an encoder");
        display = Display::open_direct(
            &device,
            &output.name,
            Layout::Nv12,
            (width, height),
            &encoder,
            monitor.slot,
        )
        .expect("the display, read from its driver");

        let began = Time::now();
        let first = display.wait_present(150);
        println!(
            "first present {first:?} after {:.2} ms",
            clock::elapsed_ms(began)
        );
        assert_eq!(first, Some(true), "the first present did not come at once");
        let interval = 1000.0 / f64::from(hz.max(1));
        let (mut changed, mut unchanged, mut units, mut late) = (0, 0, 0, 0);
        let mut waits = Stage::new();
        let mut deadline = clock::elapsed_ms(began);
        for tick in 0..ticks {
            if tick > 0 {
                let until = deadline - clock::elapsed_ms(began);
                if until > 0.0 {
                    clock::precise_sleep(core::time::Duration::from_secs_f64(until / 1000.0));
                }
                let asked = Time::now();
                while display.wait_present(20) != Some(true) {}
                let waited = clock::elapsed_ms(asked);
                waits.record(waited);
                if waited > 2.0 * interval + 2.0 {
                    late += 1;
                }
            }
            deadline += interval;
            display.acquire().expect("an acquire");
            match display.converted().expect("the conversion") {
                Some(got) if got.changed => changed += 1,
                // A present before any picture is one of the same picture, so
                // the first picture's coming at once is a conversion within
                // the first ticks.
                _ => {
                    assert!(
                        changed > 0 || clock::elapsed_ms(began) < 150.0,
                        "the first picture did not come within 150 ms"
                    );
                    unchanged += 1;
                }
            }
            if let Some(input) = display.presented() {
                encoder
                    .submit_registered(input, tick == 0)
                    .expect("a submit");
                loop {
                    match encoder.poll().expect("a collect") {
                        lowlat_encode::Poll::Ready { bitstream, .. } => {
                            assert!(!bitstream.is_empty());
                            units += 1;
                            break;
                        }
                        lowlat_encode::Poll::Pending => std::hint::spin_loop(),
                    }
                }
            }
        }
        let waited = waits.percentiles();
        let again = display.direct.as_ref().map_or(0, |direct| direct.again);
        println!(
            "{ticks} ticks in {:.0} ms: {changed} changed, {unchanged} the same, {units} units, \
             {again} converted again; wait for a present ms p50/p95/p99 {:.2}/{:.2}/{:.2}, \
             {late} late",
            clock::elapsed_ms(began),
            waited.p50,
            waited.p95,
            waited.p99
        );
        let (handed, queued) = (
            display.timing.handed.percentiles(),
            display.timing.queued.percentiles(),
        );
        println!(
            "acquired to taken ms p50/p95/p99 {:.3}/{:.3}/{:.3}; to the conversion queued \
             {:.3}/{:.3}/{:.3}",
            handed.p50, handed.p95, handed.p99, queued.p50, queued.p95, queued.p99
        );
        assert!(changed >= 1, "the display was never converted");
        assert_eq!(units, ticks, "a tick went unencoded");
        assert_eq!(late, 0, "a present took longer than two refreshes");
    }

    /// **A refresh changed alone settles the pace again**, reading the host's
    /// own display straight from its driver: the monitor put from 120 to 60
    /// at the same size, the stream is asked to rebuild at 60 within two
    /// seconds of the change. *Named regression test*: the refresh was taken
    /// once, at the open, and the pace stayed at 120. Run as the system
    /// account.
    #[test]
    #[ignore = "requires the display driver, the vendor's GPU and the system account"]
    fn a_refresh_changed_alone_settles_the_direct_pace_again() {
        use lowlat_encode::nvenc;
        use lowlat_vdd_ctl::{Controller, Lifetime, Mode};

        let _timer = clock::TimerResolution::raise();
        let mut controller = Controller::open().expect("the display driver");
        let monitor = controller
            .plug(Mode::new(1920, 1080, 120), Lifetime::Bound, false, None)
            .expect("a monitor");
        std::thread::sleep(core::time::Duration::from_millis(1000));
        let output = Display::locate(Some(&monitor.name)).expect("its output");
        let d3d11 = D3d11::load().expect("the system's libraries");
        let device = Arc::new(d3d11.open(output.adapter.luid).expect("a device"));
        let api = nvenc::Api::load().expect("the encoder's runtime");
        let session = api.open_session(Arc::clone(&device)).expect("a session");
        let encoder = session
            .initialize(nvenc::Config {
                codec: nvenc::Codec::H265,
                width: 1920,
                height: 1080,
                fps: 120,
                bitrate_bps: 20_000_000,
                min_qp: lowlat_encode::DEFAULT_MIN_QP,
                thorough: false,
                chroma: nvenc::Chroma::Yuv420,
                depth: nvenc::Depth::Eight,
            })
            .expect("an encoder");
        let mut display = Display::open_direct(
            &device,
            &output.name,
            Layout::Nv12,
            (1920, 1080),
            &encoder,
            monitor.slot,
        )
        .expect("the display, read from its driver");
        let rate = |display: &Display| display.settled.map(|(_, rate)| rate);
        println!(
            "slot {} {} settled at {:?}",
            monitor.slot,
            monitor.name,
            rate(&display)
        );

        controller
            .set_mode(monitor.slot, Mode::new(1920, 1080, 60))
            .expect("the refresh changed");
        let began = Time::now();
        let mut resized = false;
        while !resized && clock::elapsed_ms(began) < 2000.0 {
            if display.wait_present(20) == Some(true) {
                display.acquire().expect("an acquire");
                display.converted().expect("the conversion");
            }
            resized = display.take_resize();
        }
        println!(
            "rebuild asked {resized} {:.0} ms after the change, settled at {:?}",
            clock::elapsed_ms(began),
            rate(&display)
        );
        assert!(resized, "a refresh changed alone left the pace as it was");
        assert_eq!(rate(&display), Some(60));
    }

    /// A load presenting on a display, from `LOWLAT_LOAD` with `{display}`
    /// replaced by the output's name: ended however the test ends, since it
    /// holds the test's output open and outlives a failed one otherwise.
    struct Load(std::process::Child);

    impl Load {
        fn on(display: &str) -> Option<Self> {
            let line = std::env::var("LOWLAT_LOAD")
                .ok()?
                .replace("{display}", display);
            let mut words = line.split_whitespace();
            let program = words.next()?;
            std::process::Command::new(program)
                .args(words)
                .spawn()
                .ok()
                .map(Self)
        }
    }

    impl Drop for Load {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// **A late reader is caught, and a still desktop's last picture is
    /// never lost**, reading the host's own display straight from its driver
    /// with a load presenting on it (`LOWLAT_LOAD`, as the comparison below).
    /// A take stalled past the system's cycle of buffers -- two refreshes,
    /// with three buffers -- converts again, counted, and one not stalled
    /// never does. A reader that stops taking while pictures land, then takes
    /// once the load has gone and the desktop is still, converts the page's
    /// latest picture: the still desktop's marker repeats its frame number in
    /// another buffer and is the only way left to it.
    #[test]
    #[ignore = "requires the display driver, the vendor's GPU, a load and the system account"]
    fn a_late_reader_is_caught_and_the_last_picture_is_never_lost() {
        use lowlat_encode::nvenc;
        use lowlat_vdd_ctl::{Controller, Lifetime, Mode};

        let _timer = clock::TimerResolution::raise();
        let mut controller = Controller::open().expect("the display driver");
        let monitor = controller
            .plug(Mode::new(1920, 1080, 120), Lifetime::Bound, false, None)
            .expect("a monitor");
        let load = Load::on(&monitor.name).expect("a load");
        std::thread::sleep(core::time::Duration::from_millis(1500));
        let output = Display::locate(Some(&monitor.name)).expect("its output");
        let (width, height, hz) = Display::shape_of_display(Some(&output.name)).expect("a shape");
        let d3d11 = D3d11::load().expect("the system's libraries");
        let device = Arc::new(d3d11.open(output.adapter.luid).expect("a device"));
        let api = nvenc::Api::load().expect("the encoder's runtime");
        let session = api.open_session(Arc::clone(&device)).expect("a session");
        let encoder = session
            .initialize(nvenc::Config {
                codec: nvenc::Codec::H265,
                width,
                height,
                fps: hz.max(1),
                bitrate_bps: 20_000_000,
                min_qp: lowlat_encode::DEFAULT_MIN_QP,
                thorough: false,
                chroma: nvenc::Chroma::Yuv420,
                depth: nvenc::Depth::Eight,
            })
            .expect("an encoder");
        let mut display = Display::open_direct(
            &device,
            &output.name,
            Layout::Nv12,
            (width, height),
            &encoder,
            monitor.slot,
        )
        .expect("the display, read from its driver");
        let take = |display: &mut Display, takes: usize| {
            let mut changed = 0;
            for _ in 0..takes {
                while display.wait_present(50) != Some(true) {}
                display.acquire().expect("an acquire");
                if display
                    .converted()
                    .expect("the conversion")
                    .is_some_and(|got| got.changed)
                {
                    changed += 1;
                }
            }
            changed
        };
        let again = |display: &Display| display.direct.as_ref().map_or(0, |d| d.again);

        // Not stalled: nothing is overtaken.
        let changed = take(&mut display, 120);
        println!(
            "on time: {changed} changed, {} converted again",
            again(&display)
        );
        assert_eq!(again(&display), 0, "a picture on time was overtaken");
        // Stalled past two refreshes after queuing: each one overtaken.
        if let Some(direct) = display.direct.as_mut() {
            direct.stall_ms = 30;
        }
        let changed = take(&mut display, 20);
        let stalled = again(&display);
        println!("stalled 30 ms: {changed} changed, {stalled} converted again");
        assert!(
            stalled >= 15,
            "a stalled reader was not caught: {stalled} of 20"
        );
        if let Some(direct) = display.direct.as_mut() {
            direct.stall_ms = 0;
        }

        // The last picture: stop taking while pictures land, then end the load
        // and let the desktop go still.
        take(&mut display, 10);
        std::thread::sleep(core::time::Duration::from_millis(100));
        drop(load);
        std::thread::sleep(core::time::Duration::from_millis(1000));
        let latest = display
            .direct
            .as_ref()
            .and_then(|direct| direct.page.read())
            .expect("the page");
        while display.wait_present(50) != Some(true) {}
        display.acquire().expect("an acquire");
        let got = display.converted().expect("the conversion");
        let taken = display.direct.as_ref().and_then(|direct| direct.taken);
        println!(
            "still: the page's latest frame {} (a repeat: {}), taken {taken:?}, converted {:?}",
            latest.picture.frame,
            latest.picture.repeat,
            got.map(|got| got.changed)
        );
        assert!(
            got.is_some_and(|got| got.changed),
            "the still desktop's last picture was not converted"
        );
        assert_eq!(
            taken.map(|(_, _, frame)| frame),
            Some(latest.picture.frame),
            "the picture converted is not the page's latest"
        );
    }

    /// **Two readers at once are both served**: one monitor of the driver with
    /// a load presenting on it (`LOWLAT_LOAD`, as the comparison below), its
    /// page read by two of the host's displays at once, each on a thread and a
    /// device of its own, each waiting for the next picture straight after
    /// converting the last, so both wait on the publish together. Its event
    /// wakes one waiter and the other learns of the picture at its own wait's
    /// bound, so neither may wait past two refreshes, and each converts the
    /// pictures as they land. Run as the system account.
    #[test]
    #[ignore = "requires the display driver, the vendor's GPU, a load and the system account"]
    fn two_readers_at_once_are_both_served() {
        use lowlat_encode::nvenc;
        use lowlat_vdd_ctl::{Controller, Lifetime, Mode};

        let _timer = clock::TimerResolution::raise();
        let mut controller = Controller::open().expect("the display driver");
        let monitor = controller
            .plug(Mode::new(1920, 1080, 120), Lifetime::Bound, false, None)
            .expect("a monitor");
        let _load = Load::on(&monitor.name).expect("a load");
        std::thread::sleep(core::time::Duration::from_millis(1500));
        let output = Display::locate(Some(&monitor.name)).expect("its output");
        let (width, height, hz) = Display::shape_of_display(Some(&output.name)).expect("a shape");
        let ticks = 240;
        let start = Arc::new(std::sync::Barrier::new(2));
        let readers: Vec<_> = (0..2)
            .map(|reader| {
                let (name, luid, slot) = (output.name.clone(), output.adapter.luid, monitor.slot);
                let start = Arc::clone(&start);
                std::thread::spawn(move || {
                    let d3d11 = D3d11::load().expect("the system's libraries");
                    let device = Arc::new(d3d11.open(luid).expect("a device"));
                    let api = nvenc::Api::load().expect("the encoder's runtime");
                    let session = api.open_session(Arc::clone(&device)).expect("a session");
                    let encoder = session
                        .initialize(nvenc::Config {
                            codec: nvenc::Codec::H265,
                            width,
                            height,
                            fps: hz.max(1),
                            bitrate_bps: 20_000_000,
                            min_qp: lowlat_encode::DEFAULT_MIN_QP,
                            thorough: false,
                            chroma: nvenc::Chroma::Yuv420,
                            depth: nvenc::Depth::Eight,
                        })
                        .expect("an encoder");
                    let mut display = Display::open_direct(
                        &device,
                        &name,
                        Layout::Nv12,
                        (width, height),
                        &encoder,
                        slot,
                    )
                    .expect("the display, read from its driver");
                    start.wait();
                    let interval = 1000.0 / f64::from(hz.max(1));
                    let (mut changed, mut late) = (0, 0);
                    let mut waits = Stage::new();
                    for _ in 0..ticks {
                        let asked = Time::now();
                        while display.wait_present(20) != Some(true) {}
                        let waited = clock::elapsed_ms(asked);
                        waits.record(waited);
                        if waited > 2.0 * interval + 2.0 {
                            late += 1;
                        }
                        display.acquire().expect("an acquire");
                        if display
                            .converted()
                            .expect("the conversion")
                            .is_some_and(|got| got.changed)
                        {
                            changed += 1;
                        }
                    }
                    let waited = waits.percentiles();
                    println!(
                        "reader {reader}: {changed} of {ticks} changed; wait for a present ms \
                         p50/p95/p99 {:.2}/{:.2}/{:.2}, {late} late",
                        waited.p50, waited.p95, waited.p99
                    );
                    (changed, late)
                })
            })
            .collect();
        for reader in readers {
            let (changed, late) = reader.join().expect("a reader");
            assert!(changed >= ticks / 2, "a reader was not handed the pictures");
            assert_eq!(late, 0, "a reader waited past two refreshes");
        }
    }

    /// **The host's own display is read from its driver where the driver's
    /// frames can be taken, and duplicated where they cannot**: a monitor of
    /// the driver plugged by this test and published as the host's own,
    /// opened as any display is. `LOWLAT_EXPECT` says which: `direct` run as
    /// the system account, `duplication` as an ordinary user.
    #[test]
    #[ignore = "requires the display driver and the vendor's GPU; run as the system account and as a user"]
    fn the_hosts_own_display_is_read_from_its_driver_where_it_can_be() {
        use lowlat_encode::nvenc;
        use lowlat_vdd_ctl::{Controller, Lifetime, Mode};

        let expect = std::env::var("LOWLAT_EXPECT").expect("LOWLAT_EXPECT, direct or duplication");
        let mut controller = Controller::open().expect("the display driver");
        let monitor = controller
            .plug(Mode::new(1920, 1080, 60), Lifetime::Bound, false, None)
            .expect("a monitor");
        std::thread::sleep(core::time::Duration::from_millis(1000));
        publish_own(Some((monitor.name.as_str(), monitor.slot)));
        let output = Display::locate(Some(&monitor.name)).expect("its output");
        let d3d11 = D3d11::load().expect("the system's libraries");
        let device = Arc::new(d3d11.open(output.adapter.luid).expect("a device"));
        let api = nvenc::Api::load().expect("the encoder's runtime");
        let session = api.open_session(Arc::clone(&device)).expect("a session");
        let encoder = session
            .initialize(nvenc::Config {
                codec: nvenc::Codec::H265,
                width: 1920,
                height: 1080,
                fps: 60,
                bitrate_bps: 20_000_000,
                min_qp: lowlat_encode::DEFAULT_MIN_QP,
                thorough: false,
                chroma: nvenc::Chroma::Yuv420,
                depth: nvenc::Depth::Eight,
            })
            .expect("an encoder");
        let mut display =
            Display::open(&device, &output.name, Layout::Nv12, (1920, 1080), &encoder)
                .expect("the display");
        let source = if display.direct.is_some() {
            "direct"
        } else {
            "duplication"
        };
        let began = Time::now();
        let first = display.wait_present(500);
        // A present before any picture is one of the same picture: the
        // picture is what the acquire converts.
        let pictured = first == Some(true)
            && display.acquire().is_ok()
            && display.converted().is_ok_and(|got| got.is_some());
        println!(
            "slot {} {} read through the {source}, first present {first:?} after {:.1} ms, \
             a picture {pictured}",
            monitor.slot,
            monitor.name,
            clock::elapsed_ms(began)
        );
        publish_own(None);
        assert_eq!(source, expect);
        assert!(pictured, "nothing presented");
    }

    /// A window over a whole output whose pointer is the test's own: made and
    /// pumped on a thread of its own, the pointer it shows set and shown or
    /// hidden on that thread, which is where the system takes both from.
    struct PointerWindow {
        window: usize,
        thread: u32,
        pump: Option<std::thread::JoinHandle<()>>,
    }

    /// The pointer the window shows, and the messages that set it and show
    /// or hide it.
    static SHOWN: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    const SET_POINTER: u32 = windows_sys::Win32::UI::WindowsAndMessaging::WM_APP + 1;
    const SHOW_POINTER: u32 = windows_sys::Win32::UI::WindowsAndMessaging::WM_APP + 2;

    unsafe extern "system" fn pointer_window(
        window: windows_sys::Win32::Foundation::HWND,
        message: u32,
        w: usize,
        l: isize,
    ) -> isize {
        use std::sync::atomic::Ordering;
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            DefWindowProcW, SetCursor, ShowCursor, WM_SETCURSOR,
        };
        // SAFETY: the system's calls, on the window's own thread.
        unsafe {
            match message {
                WM_SETCURSOR => {
                    SetCursor(SHOWN.load(Ordering::Relaxed) as _);
                    1
                }
                SET_POINTER => {
                    SHOWN.store(w, Ordering::Relaxed);
                    SetCursor(w as _);
                    0
                }
                SHOW_POINTER => {
                    ShowCursor(i32::from(w != 0));
                    0
                }
                _ => DefWindowProcW(window, message, w, l),
            }
        }
    }

    impl PointerWindow {
        fn over(output: &Output) -> Self {
            use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
            use windows_sys::Win32::System::Threading::GetCurrentThreadId;
            use windows_sys::Win32::UI::WindowsAndMessaging::{
                CreateWindowExW, DispatchMessageW, GetMessageW, MSG, RegisterClassW, WNDCLASSW,
                WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP, WS_VISIBLE,
            };
            let (left, top) = (output.left, output.top);
            let (width, height) = (output.right - output.left, output.bottom - output.top);
            let (made, taken) = std::sync::mpsc::channel();
            let pump = std::thread::spawn(move || {
                let name: Vec<u16> = "lowlat pointer test\0".encode_utf16().collect();
                // SAFETY: a class and a window of this thread's, with a
                // procedure of this module's; the messages pumped until quit.
                unsafe {
                    let instance = GetModuleHandleW(core::ptr::null());
                    let class = WNDCLASSW {
                        lpfnWndProc: Some(pointer_window),
                        hInstance: instance,
                        lpszClassName: name.as_ptr(),
                        ..core::mem::zeroed()
                    };
                    RegisterClassW(&raw const class);
                    let window = CreateWindowExW(
                        WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
                        name.as_ptr(),
                        name.as_ptr(),
                        WS_POPUP | WS_VISIBLE,
                        left,
                        top,
                        width,
                        height,
                        core::ptr::null_mut(),
                        core::ptr::null_mut(),
                        instance,
                        core::ptr::null(),
                    );
                    made.send((window as usize, GetCurrentThreadId()))
                        .expect("the test waits");
                    let mut message: MSG = core::mem::zeroed();
                    while GetMessageW(&raw mut message, core::ptr::null_mut(), 0, 0) > 0 {
                        DispatchMessageW(&raw const message);
                    }
                }
            });
            let (window, thread) = taken.recv().expect("a window");
            assert_ne!(window, 0, "no window");
            Self {
                window,
                thread,
                pump: Some(pump),
            }
        }

        fn send(&self, message: u32, w: usize) {
            // SAFETY: a live window; the message handled before this returns.
            unsafe {
                windows_sys::Win32::UI::WindowsAndMessaging::SendMessageW(
                    self.window as _,
                    message,
                    w,
                    0,
                )
            };
        }

        fn set(&self, pointer: usize) {
            self.send(SET_POINTER, pointer);
        }

        fn show(&self, shown: bool) {
            self.send(SHOW_POINTER, usize::from(shown));
        }
    }

    impl Drop for PointerWindow {
        fn drop(&mut self) {
            // SAFETY: the pump's thread, told to quit; its window goes with it.
            unsafe {
                windows_sys::Win32::UI::WindowsAndMessaging::PostThreadMessageW(
                    self.thread,
                    windows_sys::Win32::UI::WindowsAndMessaging::WM_QUIT,
                    0,
                    0,
                )
            };
            if let Some(pump) = self.pump.take() {
                let _ = pump.join();
            }
        }
    }

    /// A pointer of the test's own: `width` by `height`, its picture made by
    /// `pixel` for each point -- BGRA, alpha 0 throughout for a pointer whose
    /// mask decides, with `masked` the points the mask lets the screen
    /// through.
    fn made_pointer(
        width: i32,
        height: i32,
        pixel: impl Fn(i32, i32) -> [u8; 4],
        masked: impl Fn(i32, i32) -> bool,
    ) -> usize {
        use windows_sys::Win32::Graphics::Gdi::{
            BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateBitmap, CreateDIBSection, DIB_RGB_COLORS,
            DeleteObject,
        };
        use windows_sys::Win32::UI::WindowsAndMessaging::{CreateIconIndirect, ICONINFO};
        // A monochrome bitmap's rows are whole words.
        let rows = usize::try_from((width + 15) / 16 * 2).unwrap_or(0);
        let mut mask = vec![0u8; rows * usize::try_from(height).unwrap_or(0)];
        for y in 0..height {
            for x in 0..width {
                if masked(x, y) {
                    let at = usize::try_from(y).unwrap_or(0) * rows
                        + usize::try_from(x / 8).unwrap_or(0);
                    mask[at] |= 0x80 >> (x % 8);
                }
            }
        }
        // SAFETY: bitmaps of the sizes said, filled within them, given to the
        // pointer made of them and deleted after, as the call copies them.
        unsafe {
            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: u32::try_from(size_of::<BITMAPINFOHEADER>()).unwrap_or(0),
                    biWidth: width,
                    biHeight: -height,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB,
                    ..core::mem::zeroed()
                },
                ..core::mem::zeroed()
            };
            let mut bits: *mut core::ffi::c_void = core::ptr::null_mut();
            let colour = CreateDIBSection(
                core::ptr::null_mut(),
                &raw const info,
                DIB_RGB_COLORS,
                &raw mut bits,
                core::ptr::null_mut(),
                0,
            );
            assert!(!colour.is_null() && !bits.is_null(), "no colour bitmap");
            let pixels = core::slice::from_raw_parts_mut(
                bits.cast::<u8>(),
                usize::try_from(width * height * 4).unwrap_or(0),
            );
            for y in 0..height {
                for x in 0..width {
                    let at = usize::try_from((y * width + x) * 4).unwrap_or(0);
                    pixels[at..at + 4].copy_from_slice(&pixel(x, y));
                }
            }
            let mask_bitmap = CreateBitmap(width, height, 1, 1, mask.as_ptr().cast());
            let icon = ICONINFO {
                fIcon: 0,
                xHotspot: 2,
                yHotspot: 3,
                hbmMask: mask_bitmap,
                hbmColor: colour,
            };
            let pointer = CreateIconIndirect(&raw const icon);
            DeleteObject(colour);
            DeleteObject(mask_bitmap);
            assert!(!pointer.is_null(), "no pointer");
            pointer as usize
        }
    }

    /// What a display said of the pointer last, as a guest would see it.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct Seen {
        checksum: u32,
        size: (u16, u16),
        hot: (u16, u16),
        hidden: bool,
        relative: bool,
        at: (u16, u16),
    }

    /// Look at the pointer through both sources for `ms`, the duplication's
    /// frames taken as they come: what each said last, if anything.
    fn both_say(dup: &mut Display, direct: &mut Display, ms: f64) -> (Option<Seen>, Option<Seen>) {
        let seen = |pointer: Pointer| {
            pointer.reported.map(|reported| Seen {
                checksum: pointer.checksum,
                size: (pointer.width, pointer.height),
                hot: (reported.hot_x, reported.hot_y),
                hidden: reported.hidden,
                relative: reported.relative,
                at: (reported.x, reported.y),
            })
        };
        let began = Time::now();
        let (mut said, mut direct_said) = (None, None);
        while clock::elapsed_ms(began) < ms {
            if dup.wait_present(5) == Some(true) {
                let _ = dup.acquire();
                let _ = dup.converted();
            }
            if let Some(pointer) = dup.pointer() {
                said = seen(pointer).or(said);
            }
            if let Some(pointer) = direct.pointer() {
                direct_said = seen(pointer).or(direct_said);
            }
        }
        (said, direct_said)
    }

    /// **The pointer reads the same from the driver as from the
    /// duplication**: a monitor of the driver plugged by this test, read
    /// through both sources at once, a window over it whose pointer the test
    /// sets -- the system's arrow, text cursor and hand, a monochrome pointer
    /// of the test's own, one inverting in colour -- each drawn alike, the
    /// text cursor and the monochrome one in black with a light halo. A
    /// pointer larger than the 256 the wire carries is the system's to draw:
    /// the driver's report says hidden. Hidden by the application, the
    /// pointer turns relative on both, and shown again it is absolute with
    /// its position; taken off the display nothing is said, and back on it
    /// the state is right. Moves the desk's pointer; run as the system
    /// account.
    #[test]
    #[ignore = "requires the display driver, the vendor's GPU and the system account; moves the pointer"]
    fn the_pointer_reads_the_same_from_the_driver_as_from_the_duplication() {
        use lowlat_encode::nvenc;
        use lowlat_vdd_ctl::{Controller, Lifetime, Mode};
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            CreateCursor, IDC_ARROW, IDC_HAND, IDC_IBEAM, LoadCursorW, SetCursorPos,
        };

        let _timer = clock::TimerResolution::raise();
        let mut controller = Controller::open().expect("the display driver");
        let monitor = controller
            .plug(Mode::new(1920, 1080, 60), Lifetime::Bound, false, None)
            .expect("a monitor");
        std::thread::sleep(core::time::Duration::from_millis(1500));
        let output = Display::locate(Some(&monitor.name)).expect("its output");
        let d3d11 = D3d11::load().expect("the system's libraries");
        let device = Arc::new(d3d11.open(output.adapter.luid).expect("a device"));
        let api = nvenc::Api::load().expect("the encoder's runtime");
        let (first, second) = (
            api.open_session(Arc::clone(&device)).expect("a session"),
            api.open_session(Arc::clone(&device)).expect("a session"),
        );
        let config = nvenc::Config {
            codec: nvenc::Codec::H265,
            width: 1920,
            height: 1080,
            fps: 60,
            bitrate_bps: 20_000_000,
            min_qp: lowlat_encode::DEFAULT_MIN_QP,
            thorough: false,
            chroma: nvenc::Chroma::Yuv420,
            depth: nvenc::Depth::Eight,
        };
        let (encoder, other) = (
            first.initialize(config).expect("an encoder"),
            second.initialize(config).expect("an encoder"),
        );
        let mut dup = Display::open(&device, &output.name, Layout::Nv12, (1920, 1080), &encoder)
            .expect("the duplication");
        let mut direct = Display::open_direct(
            &device,
            &output.name,
            Layout::Nv12,
            (1920, 1080),
            &other,
            monitor.slot,
        )
        .expect("the display, read from its driver");
        let window = PointerWindow::over(&output);
        let (middle_x, middle_y) = (
            (output.left + output.right) / 2,
            (output.top + output.bottom) / 2,
        );
        // SAFETY: a point on the desktop.
        unsafe { SetCursorPos(middle_x, middle_y) };

        // SAFETY: the system's shared pointers, never destroyed.
        let system = |name| unsafe { LoadCursorW(core::ptr::null_mut(), name) } as usize;
        // Black, white, inverting and clear rows, eight each.
        let (and, xor): (Vec<u8>, Vec<u8>) = (0..32)
            .flat_map(|row| {
                let (a, x) = match row / 8 {
                    0 => (0x00, 0x00),
                    1 => (0x00, 0xFF),
                    2 => (0xFF, 0xFF),
                    _ => (0xFF, 0x00),
                };
                [(a, x); 4]
            })
            .unzip();
        // SAFETY: two planes of the size said.
        let monochrome = unsafe {
            CreateCursor(
                core::ptr::null_mut(),
                4,
                4,
                32,
                32,
                and.as_ptr().cast(),
                xor.as_ptr().cast(),
            )
        } as usize;
        let coloured = made_pointer(
            32,
            32,
            |x, y| match (x < 16, y < 16) {
                (true, true) => [0x20, 0x40, 0xE0, 0],
                (false, true) => [0x40, 0x80, 0xC0, 0],
                _ => [0, 0, 0, 0],
            },
            |x, y| !(x < 16 && y < 16),
        );
        let large = made_pointer(
            300,
            300,
            |x, y| {
                if x < 200 && y < 200 {
                    [0x30, 0x60, 0x90, 0xFF]
                } else {
                    [0, 0, 0, 0]
                }
            },
            |_, _| false,
        );

        let mut failures = Vec::new();
        for (name, pointer) in [
            ("arrow", system(IDC_ARROW)),
            ("text", system(IDC_IBEAM)),
            ("hand", system(IDC_HAND)),
            ("monochrome", monochrome),
            ("coloured inverting", coloured),
        ] {
            window.set(pointer);
            let (said, direct_said) = both_say(&mut dup, &mut direct, 600.0);
            println!("{name}: duplication {said:?}, driver {direct_said:?}");
            let (Some(said), Some(direct_said)) = (said, direct_said) else {
                failures.push(format!("{name}: a source said nothing"));
                continue;
            };
            if (said.checksum, said.size, said.hot)
                != (direct_said.checksum, direct_said.size, direct_said.hot)
                || said.hidden
                || direct_said.hidden
            {
                failures.push(format!("{name}: drawn differently or hidden"));
            }
        }

        window.set(large);
        let (said, direct_said) = both_say(&mut dup, &mut direct, 600.0);
        println!("larger than 256: duplication {said:?}, driver {direct_said:?}");
        if !direct_said.is_some_and(|seen| seen.hidden) {
            failures.push("larger than 256: the driver's report not hidden".to_string());
        }

        window.set(system(IDC_ARROW));
        both_say(&mut dup, &mut direct, 300.0);
        window.show(false);
        let (said, direct_said) = both_say(&mut dup, &mut direct, 600.0);
        println!("hidden by the application: duplication {said:?}, driver {direct_said:?}");
        if !direct_said.is_some_and(|seen| seen.relative && seen.hidden) || !dup.relative() {
            failures.push("hidden by the application: not relative".to_string());
        }
        window.show(true);
        let (said, direct_said) = both_say(&mut dup, &mut direct, 600.0);
        println!("shown again: duplication {said:?}, driver {direct_said:?}");
        let middle = (
            u16::try_from(middle_x - output.left).unwrap_or(0),
            u16::try_from(middle_y - output.top).unwrap_or(0),
        );
        if !direct_said.is_some_and(|seen| !seen.relative && !seen.hidden && seen.at == middle) {
            failures.push("shown again: not absolute at the pointer's place".to_string());
        }

        // Off the display, on the desktop's first corner, and back.
        // SAFETY: points on the desktop.
        unsafe { SetCursorPos(output.left - 100, output.top + 100) };
        let (said, direct_said) = both_say(&mut dup, &mut direct, 400.0);
        println!("off the display: duplication {said:?}, driver {direct_said:?}");
        if said.is_some() || direct_said.is_some() {
            failures.push("off the display: something said".to_string());
        }
        window.set(system(IDC_HAND));
        // SAFETY: as above.
        unsafe { SetCursorPos(middle_x, middle_y) };
        let (said, direct_said) = both_say(&mut dup, &mut direct, 600.0);
        println!("back on it: duplication {said:?}, driver {direct_said:?}");
        if !matches!((said, direct_said), (Some(a), Some(b)) if a.checksum == b.checksum && !b.hidden)
        {
            failures.push("back on it: not the hand, visible".to_string());
        }
        drop(window);
        assert!(failures.is_empty(), "{failures:#?}");
    }

    /// A desktop of the test's own made the input desktop while this lives,
    /// as the lock screen's is while it is up; the user's put back after,
    /// however the test ends.
    struct OtherDesktop {
        made: windows_sys::Win32::System::StationsAndDesktops::HDESK,
        user: windows_sys::Win32::System::StationsAndDesktops::HDESK,
    }

    impl OtherDesktop {
        fn switch() -> Self {
            use windows_sys::Win32::Foundation::GENERIC_ALL;
            use windows_sys::Win32::System::StationsAndDesktops::{
                CreateDesktopW, OpenDesktopW, SwitchDesktop,
            };
            let name: Vec<u16> = "lowlat pointer test\0".encode_utf16().collect();
            let default: Vec<u16> = "Default\0".encode_utf16().collect();
            // SAFETY: terminated names; no device, mode or attributes.
            unsafe {
                let made = CreateDesktopW(
                    name.as_ptr(),
                    core::ptr::null(),
                    core::ptr::null(),
                    0,
                    GENERIC_ALL,
                    core::ptr::null(),
                );
                let user = OpenDesktopW(default.as_ptr(), 0, 0, GENERIC_ALL);
                assert!(!made.is_null() && !user.is_null(), "no desktops");
                assert_ne!(SwitchDesktop(made), 0, "the switch was refused");
                Self { made, user }
            }
        }
    }

    impl Drop for OtherDesktop {
        fn drop(&mut self) {
            use windows_sys::Win32::System::StationsAndDesktops::{CloseDesktop, SwitchDesktop};
            // SAFETY: desktops this opened, the user's put back first, each
            // closed once; one a thread is still on closes with that thread.
            unsafe {
                SwitchDesktop(self.user);
                CloseDesktop(self.made);
                CloseDesktop(self.user);
            }
        }
    }

    /// Look at the pointer through `display` for `ms`: what it said last.
    fn says(display: &mut Display, ms: f64) -> Option<Seen> {
        let began = Time::now();
        let mut said = None;
        while clock::elapsed_ms(began) < ms {
            if let Some(pointer) = display.pointer()
                && let Some(reported) = pointer.reported
            {
                said = Some(Seen {
                    checksum: pointer.checksum,
                    size: (pointer.width, pointer.height),
                    hot: (reported.hot_x, reported.hot_y),
                    hidden: reported.hidden,
                    relative: reported.relative,
                    at: (reported.x, reported.y),
                });
            }
            std::thread::sleep(core::time::Duration::from_millis(5));
        }
        said
    }

    /// **The pointer is read on whatever desktop takes the input**: read from
    /// its driver, the host's own display needs no desktop for its frames, so
    /// nothing else moves its thread when the lock screen or an elevation
    /// prompt takes the input. Switched to a desktop of the test's own, the
    /// pointer's change there -- the hand it had becomes the system's arrow --
    /// is still said, and the hand's return on the way back. Switches the
    /// input desktop for about a second and moves the pointer; run as the
    /// system account.
    #[test]
    #[ignore = "requires the display driver, the vendor's GPU and the system account; switches the desktop"]
    fn the_pointer_is_read_on_the_desktop_that_takes_the_input() {
        use lowlat_encode::nvenc;
        use lowlat_vdd_ctl::{Controller, Lifetime, Mode};
        use windows_sys::Win32::UI::WindowsAndMessaging::{IDC_HAND, LoadCursorW, SetCursorPos};

        let _timer = clock::TimerResolution::raise();
        let mut controller = Controller::open().expect("the display driver");
        let monitor = controller
            .plug(Mode::new(1920, 1080, 60), Lifetime::Bound, false, None)
            .expect("a monitor");
        std::thread::sleep(core::time::Duration::from_millis(1500));
        let output = Display::locate(Some(&monitor.name)).expect("its output");
        let d3d11 = D3d11::load().expect("the system's libraries");
        let device = Arc::new(d3d11.open(output.adapter.luid).expect("a device"));
        let api = nvenc::Api::load().expect("the encoder's runtime");
        let session = api.open_session(Arc::clone(&device)).expect("a session");
        let encoder = session
            .initialize(nvenc::Config {
                codec: nvenc::Codec::H265,
                width: 1920,
                height: 1080,
                fps: 60,
                bitrate_bps: 20_000_000,
                min_qp: lowlat_encode::DEFAULT_MIN_QP,
                thorough: false,
                chroma: nvenc::Chroma::Yuv420,
                depth: nvenc::Depth::Eight,
            })
            .expect("an encoder");
        let mut direct = Display::open_direct(
            &device,
            &output.name,
            Layout::Nv12,
            (1920, 1080),
            &encoder,
            monitor.slot,
        )
        .expect("the display, read from its driver");
        let window = PointerWindow::over(&output);
        // SAFETY: a point on the desktop; the system's shared hand.
        unsafe {
            SetCursorPos(
                (output.left + output.right) / 2,
                (output.top + output.bottom) / 2,
            );
            window.set(LoadCursorW(core::ptr::null_mut(), IDC_HAND) as usize);
        }
        let hand = says(&mut direct, 600.0);
        println!("on the user's desktop: {hand:?}");
        let away = {
            let _other = OtherDesktop::switch();
            says(&mut direct, 800.0)
        };
        println!("on the test's desktop: {away:?}");
        let back = says(&mut direct, 800.0);
        println!("back on the user's: {back:?}");
        drop(window);
        let hand = hand.expect("nothing said on the user's desktop");
        assert!(
            away.is_some_and(|away| away.checksum != hand.checksum && !away.hidden),
            "the pointer's change on the other desktop went unsaid"
        );
        assert_eq!(
            back.map(|back| back.checksum),
            Some(hand.checksum),
            "the hand did not come back"
        );
    }

    /// **The pointer is read on the secure desktop**: the host's own display
    /// read from its driver while a person brings up the secure attention
    /// screen -- Ctrl+Alt+Del -- and leaves it, within `LOWLAT_WAIT_S` (60).
    /// The reading thread must follow the input onto the system's own
    /// desktop, and the pointer must be said there, the system's arrow
    /// in place of the hand the test's window shows. Run as the system
    /// account, with somebody at the keyboard.
    #[test]
    #[ignore = "requires the display driver, the vendor's GPU, the system account and a person"]
    fn the_pointer_is_read_on_the_secure_desktop() {
        use lowlat_encode::nvenc;
        use lowlat_vdd_ctl::{Controller, Lifetime, Mode};
        use windows_sys::Win32::UI::WindowsAndMessaging::{IDC_HAND, LoadCursorW, SetCursorPos};

        let _timer = clock::TimerResolution::raise();
        let wait_s: f64 = std::env::var("LOWLAT_WAIT_S")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(60.0);
        let mut controller = Controller::open().expect("the display driver");
        let monitor = controller
            .plug(Mode::new(1920, 1080, 60), Lifetime::Bound, false, None)
            .expect("a monitor");
        std::thread::sleep(core::time::Duration::from_millis(1500));
        let output = Display::locate(Some(&monitor.name)).expect("its output");
        let d3d11 = D3d11::load().expect("the system's libraries");
        let device = Arc::new(d3d11.open(output.adapter.luid).expect("a device"));
        let api = nvenc::Api::load().expect("the encoder's runtime");
        let session = api.open_session(Arc::clone(&device)).expect("a session");
        let encoder = session
            .initialize(nvenc::Config {
                codec: nvenc::Codec::H265,
                width: 1920,
                height: 1080,
                fps: 60,
                bitrate_bps: 20_000_000,
                min_qp: lowlat_encode::DEFAULT_MIN_QP,
                thorough: false,
                chroma: nvenc::Chroma::Yuv420,
                depth: nvenc::Depth::Eight,
            })
            .expect("an encoder");
        let mut direct = Display::open_direct(
            &device,
            &output.name,
            Layout::Nv12,
            (1920, 1080),
            &encoder,
            monitor.slot,
        )
        .expect("the display, read from its driver");
        let window = PointerWindow::over(&output);
        // SAFETY: a point on the desktop; the system's shared hand.
        unsafe {
            SetCursorPos(
                (output.left + output.right) / 2,
                (output.top + output.bottom) / 2,
            );
            window.set(LoadCursorW(core::ptr::null_mut(), IDC_HAND) as usize);
        }
        let hand = says(&mut direct, 600.0).expect("nothing said on the user's desktop");
        println!("waiting {wait_s:.0} s for Ctrl+Alt+Del and back: {hand:?}");
        let began = Time::now();
        let mut there = None;
        while clock::elapsed_ms(began) < wait_s * 1000.0 && there.is_none() {
            let said = says(&mut direct, 100.0);
            let on = direct
                .desktop
                .as_ref()
                .map(InputDesktop::name)
                .unwrap_or_default();
            if on.eq_ignore_ascii_case("Winlogon")
                && let Some(said) = said
                && said.checksum != hand.checksum
            {
                println!(
                    "on {on} after {:.1} s: {said:?}",
                    clock::elapsed_ms(began) / 1000.0
                );
                there = Some(said);
            }
        }
        // Back on the user's desktop before the window goes.
        let back = says(&mut direct, 3000.0);
        println!("afterwards: {back:?}");
        drop(window);
        assert!(
            there.is_some_and(|said| !said.hidden),
            "no pointer said on the secure desktop"
        );
    }

    /// **The driver's process ending mid-stream strands nothing**: ended right
    /// after a picture is published -- its fence's signal queued, perhaps not
    /// yet passed -- the conversion the reader queues behind it still
    /// finishes, the stream's check finds the output gone, and the driver is
    /// back for the next round. `LOWLAT_LOAD` as the comparison below;
    /// `LOWLAT_ROUNDS` (5); `LOWLAT_RESTART`, the driver's device instance,
    /// to have the system restart it instead. A round still running after fifteen seconds ends
    /// the process, since a reader stuck behind a fence that never passes is
    /// what this looks for.
    #[test]
    #[ignore = "ends the display driver's process; requires the vendor's GPU and the system account"]
    fn the_drivers_end_strands_nothing() {
        use lowlat_drivers::d3d11::Event;
        use lowlat_encode::nvenc;
        use lowlat_vdd_ctl::{Controller, Lifetime, Mode};
        use std::sync::atomic::{AtomicBool, Ordering};
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_TERMINATE, TerminateProcess,
        };

        let _timer = clock::TimerResolution::raise();
        let rounds: usize = std::env::var("LOWLAT_ROUNDS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(5);
        let (mut stranded, mut refused) = (0, 0);
        for round in 0..rounds {
            let finished = Arc::new(AtomicBool::new(false));
            let watched = Arc::clone(&finished);
            let watchdog = std::thread::spawn(move || {
                let began = Time::now();
                while !watched.load(Ordering::Relaxed) {
                    if clock::elapsed_ms(began) > 15_000.0 {
                        eprintln!("round {round}: still running after 15 s, stuck");
                        std::process::exit(3);
                    }
                    std::thread::sleep(core::time::Duration::from_millis(100));
                }
            });
            // The driver, started again after the last round ended it.
            let began = Time::now();
            let mut controller = loop {
                match Controller::open() {
                    Ok(controller) => break controller,
                    Err(_) if clock::elapsed_ms(began) < 10_000.0 => {
                        std::thread::sleep(core::time::Duration::from_millis(100));
                    }
                    Err(error) => panic!("the driver did not come back: {error:?}"),
                }
            };
            // **A restart waits for every handle open on the device**, and
            // the controller's closes itself as the driver is asked to be
            // removed (docs/11-drivers.md section 1.8): so the monitor is
            // bound and its handle held through the restart, as a host's is.
            let monitor = loop {
                match controller.plug(Mode::new(1920, 1080, 120), Lifetime::Bound, false, None) {
                    Ok(monitor) => break monitor,
                    Err(_) if clock::elapsed_ms(began) < 10_000.0 => {
                        std::thread::sleep(core::time::Duration::from_millis(100));
                    }
                    Err(error) => panic!("no monitor: {error:?}"),
                }
            };
            println!(
                "round {round}: the driver back in {:.0} ms",
                clock::elapsed_ms(began)
            );
            let load = Load::on(&monitor.name).expect("a load");
            std::thread::sleep(core::time::Duration::from_millis(1000));
            let output = Display::locate(Some(&monitor.name)).expect("its output");
            let (width, height, hz) =
                Display::shape_of_display(Some(&output.name)).expect("a shape");
            let d3d11 = D3d11::load().expect("the system's libraries");
            let device = Arc::new(d3d11.open(output.adapter.luid).expect("a device"));
            let api = nvenc::Api::load().expect("the encoder's runtime");
            let session = api.open_session(Arc::clone(&device)).expect("a session");
            let encoder = session
                .initialize(nvenc::Config {
                    codec: nvenc::Codec::H265,
                    width,
                    height,
                    fps: hz.max(1),
                    bitrate_bps: 20_000_000,
                    min_qp: lowlat_encode::DEFAULT_MIN_QP,
                    thorough: false,
                    chroma: nvenc::Chroma::Yuv420,
                    depth: nvenc::Depth::Eight,
                })
                .expect("an encoder");
            let mut display = Display::open_direct(
                &device,
                &output.name,
                Layout::Nv12,
                (width, height),
                &encoder,
                monitor.slot,
            )
            .expect("the display, read from its driver");
            let fence = device.fence(0).expect("a fence");
            let event = Event::new().expect("an event");
            let mut signalled = 0u64;
            let mut finishes = |display: &mut Display| -> Option<bool> {
                display.acquire().ok()?;
                display.converted().ok()??;
                signalled += 1;
                device.signal(&fence, signalled).ok()?;
                fence.notify_at(signalled, &event).ok()?;
                Some(event.wait(core::time::Duration::from_millis(1000)))
            };
            for _ in 0..30 + (round * 37) % 90 {
                while display.wait_present(50) != Some(true) {}
                assert_ne!(
                    finishes(&mut display),
                    Some(false),
                    "a conversion did not finish"
                );
            }
            // Right after a publish: the driver ended before the reader queues
            // its wait on the picture's fence value.
            while display.wait_present(50) != Some(true) {}
            let process = display
                .direct
                .as_ref()
                .map_or(0, |direct| direct.page.process_id());
            // `LOWLAT_RESTART`, a device instance: the driver restarted by the
            // system rather than its process ended.
            let mut restart = std::env::var("LOWLAT_RESTART").ok().map(|instance| {
                std::process::Command::new("pnputil")
                    .args(["/restart-device", &instance])
                    .stdout(std::process::Stdio::null())
                    .spawn()
                    .expect("pnputil")
            });
            if restart.is_none() {
                // SAFETY: an identifier the system checks; the handle closed
                // once.
                unsafe {
                    let handle = OpenProcess(PROCESS_TERMINATE, 0, process);
                    assert!(
                        !handle.is_null(),
                        "the driver's process could not be opened"
                    );
                    TerminateProcess(handle, 1);
                    CloseHandle(handle);
                }
            }
            match finishes(&mut display) {
                Some(true) => {}
                Some(false) => stranded += 1,
                // Its handles gone with the process: nothing queued at all.
                None => refused += 1,
            }
            let ended = Time::now();
            while display.attached() && clock::elapsed_ms(ended) < 3000.0 {
                std::thread::sleep(core::time::Duration::from_millis(20));
            }
            let gone_ms = clock::elapsed_ms(ended);
            println!(
                "round {round}: ended after a publish, the output gone in {gone_ms:.0} ms, \
                 stranded so far {stranded}, nothing queued so far {refused}"
            );
            assert!(!display.attached(), "the output outlived the driver");
            if let Some(restart) = restart.as_mut() {
                let _ = restart.wait();
            }
            drop(load);
            drop(display);
            drop(encoder);
            drop(device);
            finished.store(true, Ordering::Relaxed);
            let _ = watchdog.join();
        }
        assert_eq!(
            stranded, 0,
            "a conversion was stranded behind the ended driver's fence"
        );
    }

    /// **The comparison the host's default is decided by, offline**: a monitor
    /// of the driver plugged by this test, a load presenting on it, and the
    /// display read in interleaved blocks through the system's duplication
    /// and straight from the driver; per picture, its composition to its
    /// conversion done on the device -- the conversion's own fence, waited on
    /// the processor -- and the conversion's time on the device. Run as the
    /// system account in the console session: `LOWLAT_LOAD` is the load's
    /// command line, `{display}` replaced by the output's name;
    /// `LOWLAT_MODE` (2560x1440@120), `LOWLAT_BLOCK` pictures a block (600),
    /// `LOWLAT_ROUNDS` (3).
    #[test]
    #[ignore = "requires the display driver, the vendor's GPU, a load and the system account"]
    fn the_direct_frame_against_the_duplication() {
        use lowlat_drivers::d3d11::Event;
        use lowlat_encode::nvenc;
        use lowlat_vdd_ctl::{Controller, Lifetime, Mode};

        let _timer = clock::TimerResolution::raise();
        let number = |key: &str, default: usize| {
            std::env::var(key)
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(default)
        };
        let (block, rounds) = (number("LOWLAT_BLOCK", 600), number("LOWLAT_ROUNDS", 3));
        let mode = std::env::var("LOWLAT_MODE")
            .ok()
            .and_then(|text| {
                let (size, hz) = text.split_once('@')?;
                let (width, height) = size.split_once('x')?;
                Some(Mode::new(
                    width.parse().ok()?,
                    height.parse().ok()?,
                    hz.parse().ok()?,
                ))
            })
            .unwrap_or(Mode::new(2560, 1440, 120));
        let mut controller = Controller::open().expect("the display driver");
        let monitor = controller
            .plug(mode, Lifetime::Bound, false, None)
            .expect("a monitor");
        println!("slot {} {} at {}", monitor.slot, monitor.name, monitor.mode);
        let _load = Load::on(&monitor.name);
        std::thread::sleep(core::time::Duration::from_millis(1500));
        let output = Display::locate(Some(&monitor.name)).expect("its output");
        let (width, height, hz) = Display::shape_of_display(Some(&output.name)).expect("a shape");
        println!("{width}x{height}, {hz} a second");
        let d3d11 = D3d11::load().expect("the system's libraries");
        let device = Arc::new(d3d11.open(output.adapter.luid).expect("a device"));
        let api = nvenc::Api::load().expect("the encoder's runtime");
        let session = api.open_session(Arc::clone(&device)).expect("a session");
        let encoder = session
            .initialize(nvenc::Config {
                codec: nvenc::Codec::H265,
                width,
                height,
                fps: hz.max(1),
                bitrate_bps: 20_000_000,
                min_qp: lowlat_encode::DEFAULT_MIN_QP,
                thorough: false,
                chroma: nvenc::Chroma::Yuv420,
                depth: nvenc::Depth::Eight,
            })
            .expect("an encoder");
        let fence = device.fence(0).expect("a fence");
        let event = Event::new().expect("an event");
        let mut signalled = 0u64;
        // Composition to the conversion done per source, every picture kept;
        // the pictures converted again.
        let mut done: [Vec<f64>; 2] = [Vec::new(), Vec::new()];
        let mut again = 0u64;
        for round in 0..rounds {
            for direct in [false, true] {
                let mut display = if direct {
                    Display::open_direct(
                        &device,
                        &output.name,
                        Layout::Nv12,
                        (width, height),
                        &encoder,
                        monitor.slot,
                    )
                } else {
                    Display::open(
                        &device,
                        &output.name,
                        Layout::Nv12,
                        (width, height),
                        &encoder,
                    )
                }
                .expect("the display");
                let source = usize::from(direct);
                let began = Time::now();
                let mut taken = 0;
                while taken < block && clock::elapsed_ms(began) < 30_000.0 {
                    if display.wait_present(20) != Some(true) {
                        continue;
                    }
                    display.acquire().expect("an acquire");
                    let Some(got) = display.converted().expect("the conversion") else {
                        continue;
                    };
                    signalled += 1;
                    device.signal(&fence, signalled).expect("a signal");
                    fence.notify_at(signalled, &event).expect("an event");
                    assert!(
                        event.wait(core::time::Duration::from_millis(200)),
                        "a conversion did not finish"
                    );
                    let total = got.composed_before_ms.unwrap_or(0.0) + clock::elapsed_ms(got.at);
                    // The first pictures of a block include the source's own
                    // start: left out.
                    if taken >= 30 {
                        done[source].push(total);
                    }
                    taken += 1;
                }
                // The device's own clock for the conversion, its last span
                // read once the device has passed it.
                let _ = display.timing.read(&device);
                let gpu = display.timing.converting.percentiles();
                if let Some(direct) = &display.direct {
                    again += direct.again;
                }
                println!(
                    "round {round} {}: {taken} pictures in {:.0} ms, the conversion on the device \
                     p50/p95 {:.3}/{:.3} ms",
                    if direct { "direct" } else { "duplication" },
                    clock::elapsed_ms(began),
                    gpu.p50,
                    gpu.p95
                );
            }
        }
        let rank = |sorted: &[f64], fraction: f64| {
            #[allow(
                clippy::cast_precision_loss,
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "an index into the samples"
            )]
            let at = ((fraction * sorted.len() as f64).ceil() as usize).saturating_sub(1);
            sorted.get(at).copied().unwrap_or(0.0)
        };
        for (source, name) in [(0, "duplication"), (1, "direct")] {
            let mut sorted = done[source].clone();
            sorted.sort_unstable_by(f64::total_cmp);
            println!(
                "{name}: composition to the conversion done ms p50/p95/p99/max \
                 {:.3}/{:.3}/{:.3}/{:.3} over {} pictures",
                rank(&sorted, 0.50),
                rank(&sorted, 0.95),
                rank(&sorted, 0.99),
                sorted.last().copied().unwrap_or(0.0),
                sorted.len()
            );
        }
        println!("direct: {again} converted again");
    }

    /// **An absolute position lands on the pixel it names, on every output
    /// of the desktop**, through a guest's own devices and the place the
    /// display gives them: the corners, the middle and points between, read
    /// back where the system put the pointer, each exactly: a pixel short is
    /// an output's first column landing on its neighbour.
    #[test]
    #[ignore = "moves the desk's pointer across every display"]
    fn absolute_input_lands_on_the_pixel_it_names() {
        use lowlat_core::control::{Control, op};
        use lowlat_inject::Devices;
        use lowlat_inject::event::{Extents, Injector, Place};

        let _aware = Aware::per_monitor();
        let mut devices = Devices::create("sweep").expect("devices");
        let listed = listed();
        let mut worsts = Vec::new();
        for output in listed.iter().filter(|o| o.attached) {
            let placed = place_of(output, &listed);
            let (width, height) = picture_of(output);
            let mut inject = Injector::new(Extents::placed(
                width,
                height,
                Place {
                    x: placed.x,
                    y: placed.y,
                    width: placed.width,
                    height: placed.height,
                    desktop_width: placed.desktop_width,
                    desktop_height: placed.desktop_height,
                },
            ));
            let mut worst = (0, 0);
            for (fx, fy) in [
                (0, 0),
                (1, 1),
                (2, 2),
                (3, 3),
                (4, 4),
                (0, 4),
                (4, 0),
                (1, 3),
                (3, 1),
            ] {
                let (x, y) = ((width - 1) * fx / 4, (height - 1) * fy / 4);
                let motion = Control {
                    a0: 0,
                    a1: x,
                    a2: y,
                    opcode: op::MOUSE_MOTION,
                    body: &[],
                };
                // Three times each, so a reading disturbed by anything else
                // moving the pointer shows as a disagreement.
                let mut seen = Vec::new();
                for _ in 0..3 {
                    let mut tee = Tee {
                        devices: &mut devices,
                        sent: (0, 0),
                    };
                    inject.on_control(&motion, &mut tee);
                    let sent = tee.sent;
                    std::thread::sleep(std::time::Duration::from_millis(40));
                    let at = duplication::cursor().expect("the pointer");
                    seen.push((sent, at.x - output.left, at.y - output.top));
                }
                for &(sent, gx, gy) in &seen {
                    let (dx, dy) = (
                        gx - i32::try_from(x).unwrap(),
                        gy - i32::try_from(y).unwrap(),
                    );
                    println!(
                        "{} {x},{y} sent {},{} -> {gx},{gy} off {dx},{dy}",
                        output.name, sent.0, sent.1
                    );
                    worst = (worst.0.max(dx.abs()), worst.1.max(dy.abs()));
                }
            }
            println!(
                "{} {width}x{height} at {},{} of {}x{}: worst {worst:?}",
                output.name, placed.x, placed.y, placed.desktop_width, placed.desktop_height
            );
            worsts.push((output.name.clone(), worst));
        }
        for (name, worst) in &worsts {
            assert_eq!(*worst, (0, 0), "{name} landed off the pixel it named");
        }
    }

    /// The devices, and what the absolute axes were last sent.
    struct Tee<'a> {
        devices: &'a mut lowlat_inject::Devices,
        sent: (i32, i32),
    }

    impl lowlat_inject::event::Sink for Tee<'_> {
        fn emit(
            &mut self,
            device: lowlat_inject::event::Device,
            events: &[lowlat_inject::event::Event],
        ) {
            for event in events.iter().filter(|e| e.kind == 0x03) {
                match event.code {
                    0x00 => self.sent.0 = event.value,
                    0x01 => self.sent.1 = event.value,
                    _ => {}
                }
            }
            self.devices.emit(device, events);
        }
    }
}
