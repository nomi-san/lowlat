//! AMD's encoder, on the D3D11 device of the output it encodes
//! (docs/impl-plan-windows.md W2.7).
//!
//! **What it reads is the captured picture as it is** -- eight-bit BGRA,
//! converted in the encoder's own hardware at no cost to the encode
//! ([05 §3](../../../docs/05-host.md)), into eight bits only -- **or two
//! planes of YUV**, a texture of the device's or the generator's frames
//! written from ordinary memory.
//!
//! **A thread of its own waits for each picture.** The runtime hands a
//! finished picture to a query that waits for it, but reads how long a query
//! may wait only when the encoder is initialised, so one setting cannot serve
//! a collect that never blocks and a wait on the picture both. The thread
//! queries, waiting up to [`QUERY_WAIT_MS`] at a time and sleeping when
//! nothing is in the engine, and hands each unit over a ring; the loop
//! collects from the ring without blocking and sleeps on it when it waits.
//!
//! **A picture's own properties are written with every picture.** The
//! runtime keeps a forced type, the parameter sets and the delimiter on the
//! input they were written to, and inputs are made once and submitted again.

use core::time::Duration;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::thread::JoinHandle;

use lowlat_common::spsc::Ring;
use lowlat_common::wait;
use lowlat_drivers::amf::{self as runtime, Amf, Context, Format, Names, QueryHandle, Unit};
use lowlat_drivers::d3d11::Com;
use lowlat_drivers::vcall;

use crate::ffi::d3d11::{
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING, DXGI_FORMAT_B8G8R8A8_UNORM,
    DXGI_FORMAT_NV12, DXGI_FORMAT_P010, ID3D11Texture2D,
};
use crate::upload::{describe, upload};
use crate::{Encoder as EncoderTrait, Poll};

/// How many pictures the encoder holds at once, as NVIDIA's does: as many of
/// the generator's inputs, taken in turn.
pub const IN_FLIGHT: usize = 4;

/// How long one query waits for a picture: the bound on how soon the
/// collecting thread sees that it should stop.
const QUERY_WAIT_MS: i64 = 20;

/// The same bound, for the thread's own sleep with nothing in the engine.
const IDLE_WAIT: Duration = Duration::from_millis(20);

/// Why the encoder could not be built or a call on it failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// The runtime refused a call.
    Runtime(runtime::Error),
    /// The runtime refused a setting, named.
    Setting(&'static str, runtime::Error),
    /// The encoder's engine is not the hardware's.
    NoHardware,
    /// The picture is outside the sizes the encoder takes.
    Size,
    /// The runtime offers no query that waits for a picture.
    NoWait,
    /// Ten bits asked of the first codec, which has no such profile.
    TenBitH264,
    /// Ten bits asked of the encoder's own conversion, which writes every
    /// level at the eight-bit scale.
    TenBitRgb,
    /// A texture in a layout the encoder was not built to read.
    Layout,
    /// An input the encoder was never given.
    Unregistered,
    /// As many pictures in flight as the encoder holds: back pressure.
    QueueFull,
    /// The device refused a texture or a mapping.
    Device(lowlat_drivers::d3d11::Error),
    /// The collecting thread could not be started.
    Thread,
    /// The collecting thread's query failed, with the runtime's status.
    Collect(i32),
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Runtime(error) => write!(f, "{error}"),
            Self::Setting(name, error) => write!(f, "AMD's encoder refused {name}: {error}"),
            Self::NoHardware => f.write_str("AMD's encoder runs on no hardware here"),
            Self::Size => f.write_str("the picture is outside the sizes AMD's encoder takes"),
            Self::NoWait => f.write_str("AMD's encoder cannot wait for a picture"),
            Self::TenBitH264 => f.write_str("AMD's encoder has no ten-bit H.264"),
            Self::TenBitRgb => {
                f.write_str("AMD's encoder converts RGB into ten bits at the eight-bit scale")
            }
            Self::Layout => f.write_str("a texture AMD's encoder was not built to read"),
            Self::Unregistered => f.write_str("an input AMD's encoder was never given"),
            Self::QueueFull => f.write_str("AMD's encoder holds as many pictures as it takes"),
            Self::Device(error) => write!(f, "{error}"),
            Self::Thread => f.write_str("AMD's encoder's collecting thread did not start"),
            Self::Collect(status) => write!(f, "AMD's encoder's query failed with status {status}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<runtime::Error> for Error {
    fn from(error: runtime::Error) -> Self {
        Self::Runtime(error)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    H264,
    H265,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Depth {
    Eight,
    Ten,
}

/// What the encoder reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// The captured picture as it is: eight-bit BGRA, converted by the
    /// encoder, for an eight-bit session only.
    Rgb,
    /// Two planes of YUV at the session's depth: a texture of the device's,
    /// or the generator's frames written from ordinary memory.
    Yuv,
}

/// How the encoder is built. Fixed for its life but for the rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_bps: u32,
    /// The quantiser floor, zero for none ([`crate::Quality::min_qp`]).
    pub min_qp: u32,
    /// The most thorough effort the encoder has, with its pass ahead of the
    /// encode, rather than the fastest ([`crate::Quality::thorough`]).
    pub thorough: bool,
    pub depth: Depth,
    pub source: Source,
}

/// What the loop and the collecting thread share.
struct Shared {
    /// Units collected and not yet taken by the loop. As long as the encoder
    /// holds pictures, so a push always finds room.
    ring: Ring<Unit<'static>, IN_FLIGHT>,
    /// Moved at every unit pushed and when the thread ends: what the loop
    /// sleeps on.
    ready: AtomicU32,
    /// Moved at every submit and at the stop: what the thread sleeps on with
    /// nothing in the engine.
    submitted: AtomicU32,
    /// Pictures submitted and not yet collected by the thread.
    in_engine: AtomicUsize,
    /// When the last unit came out of the engine, on the performance counter
    /// in microseconds: the end of the last read of a texture handed over.
    out_us: AtomicU64,
    /// A probe's: how much later each unit is handed over than it came out,
    /// in microseconds, as if the engine had read its picture that long.
    late_us: AtomicU64,
    stop: AtomicBool,
    /// The runtime's status when a query failed, else zero.
    failed: AtomicI32,
}

/// Whether the encoder still has pictures in its engine, for a reader of the
/// textures it was handed: one that would let a texture go, or have it
/// written again, asks first.
#[derive(Clone)]
pub struct Engine(Arc<Shared>);

impl core::fmt::Debug for Engine {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Engine")
            .field("busy", &self.busy())
            .finish()
    }
}

impl Engine {
    /// True from a picture's submit until its unit comes out: until then the
    /// engine may still be reading its texture.
    pub fn busy(&self) -> bool {
        self.0.in_engine.load(Ordering::Acquire) > 0
    }

    /// When the last unit came out, on the performance counter in
    /// microseconds ([`lowlat_common::clock::monotonic_us`]): a texture handed
    /// over was read no later than this. Zero before the first.
    pub fn out_us(&self) -> u64 {
        self.0.out_us.load(Ordering::Acquire)
    }
}

/// One of the generator's inputs: the texture the encoder reads, the one the
/// processor writes it through, and the input made over the first.
struct Slot<'c> {
    texture: Com<ID3D11Texture2D>,
    staging: Com<ID3D11Texture2D>,
    input: runtime::Input<'c>,
}

/// An encoder, in the context it borrows.
pub struct Encoder<'c> {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    /// The unit the last collect handed out, released at the next call.
    held: Option<Unit<'static>>,
    /// Submitted and not yet handed out by a collect.
    outstanding: usize,
    /// Dropped before the inputs, so the encoder has given back every
    /// picture it holds before the textures under them go. The units it made
    /// are given back before it, in the drop.
    component: runtime::Encoder<'c>,
    /// The generator's inputs, empty for an encoder that reads the capture.
    slots: Vec<Slot<'c>>,
    next_slot: usize,
    /// The inputs made over textures something else writes, each at the
    /// place [`Encoder::register_texture`] answered.
    registered: Vec<runtime::Input<'c>>,
    /// The inputs made over a capture's own textures, by the texture, as
    /// [`Encoder::submit_texture`] first saw each.
    captured: Vec<(*mut ID3D11Texture2D, runtime::Input<'c>)>,
    context: &'c Context<'c>,
    names: &'static Names,
    config: Config,
}

impl core::fmt::Debug for Encoder<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Encoder")
            .field("config", &self.config)
            .field("outstanding", &self.outstanding)
            .finish()
    }
}

impl<'c> Encoder<'c> {
    /// Build an encoder for `config` in `context`, its collecting thread
    /// started.
    pub fn new(amf: &Amf, context: &'c Context<'c>, config: Config) -> Result<Self, Error> {
        let codec = match config.codec {
            Codec::H264 => runtime::Codec::H264,
            Codec::H265 => runtime::Codec::Hevc,
        };
        if config.depth == Depth::Ten && config.codec == Codec::H264 {
            return Err(Error::TenBitH264);
        }
        // **Measured to come out wrong**: the runtime takes RGB into ten bits
        // without an error and writes every level at the eight-bit scale.
        if config.depth == Depth::Ten && config.source == Source::Rgb {
            return Err(Error::TenBitRgb);
        }
        let names = codec.names();
        let component = amf.encoder(context, codec)?;
        // **Asked of the encoder, never inferred from its initialisation**,
        // which has accepted sizes the engine cannot code.
        let caps = component.caps()?;
        if !caps.hardware() {
            return Err(Error::NoHardware);
        }
        let ((least_width, most_width), (least_height, most_height)) = caps.input_size()?;
        let fits = |value: u32, least: i32, most: i32| {
            i64::from(value) >= i64::from(least) && i64::from(value) <= i64::from(most)
        };
        if !fits(config.width, least_width, most_width)
            || !fits(config.height, least_height, most_height)
        {
            return Err(Error::Size);
        }
        if caps.get_int(names.cap_query_timeout)? != 1 {
            return Err(Error::NoWait);
        }
        drop(caps);
        configure(&component, codec, names, &config)?;
        let format = match (config.source, config.depth) {
            (Source::Rgb, _) => Format::Bgra,
            (Source::Yuv, Depth::Eight) => Format::Nv12,
            (Source::Yuv, Depth::Ten) => Format::P010,
        };
        component.init(format, config.width, config.height)?;

        let mut slots = Vec::new();
        if config.source == Source::Yuv {
            let device = context.device();
            let layout = if config.depth == Depth::Ten {
                DXGI_FORMAT_P010
            } else {
                DXGI_FORMAT_NV12
            };
            let size = (config.width, config.height);
            for _ in 0..IN_FLIGHT {
                let texture = device
                    .texture(&describe(layout, size, D3D11_USAGE_DEFAULT))
                    .map_err(Error::Device)?;
                let staging = device
                    .texture(&describe(layout, size, D3D11_USAGE_STAGING))
                    .map_err(Error::Device)?;
                let input = context.input(&texture)?;
                slots.push(Slot {
                    texture,
                    staging,
                    input,
                });
            }
        }

        let shared = Arc::new(Shared {
            ring: Ring::new(),
            ready: AtomicU32::new(0),
            submitted: AtomicU32::new(0),
            in_engine: AtomicUsize::new(0),
            out_us: AtomicU64::new(0),
            late_us: AtomicU64::new(0),
            stop: AtomicBool::new(false),
            failed: AtomicI32::new(0),
        });
        // SAFETY: the thread is joined in the drop before the encoder goes,
        // and the drop releases every unit it handed over before then.
        let query = unsafe { component.query_handle() };
        let thread = std::thread::Builder::new()
            .name("amf-collect".into())
            .spawn({
                let shared = Arc::clone(&shared);
                move || collect(&shared, &query)
            })
            .map_err(|_| Error::Thread)?;
        Ok(Self {
            shared,
            thread: Some(thread),
            slots,
            next_slot: 0,
            registered: Vec::new(),
            captured: Vec::new(),
            held: None,
            outstanding: 0,
            component,
            context,
            names,
            config,
        })
    }

    pub fn config(&self) -> Config {
        self.config
    }

    /// Pictures submitted and not yet collected.
    pub fn in_flight(&self) -> usize {
        self.outstanding
    }

    /// Whether the engine still has pictures in it, for a reader of the
    /// textures handed over.
    pub fn engine(&self) -> Engine {
        Engine(Arc::clone(&self.shared))
    }

    /// **A probe's knob**: each unit handed over `late` after it came out, as
    /// if the engine had gone on reading its picture that long -- for a check
    /// that a reader in place catches a read that ran past the picture's
    /// buffer being written again. Zero, the default, is none.
    pub fn hand_over_late(&self, late: Duration) {
        let late_us = u64::try_from(late.as_micros()).unwrap_or(u64::MAX);
        self.shared.late_us.store(late_us, Ordering::Release);
    }

    /// `texture`, which something else on the encoder's device writes -- a
    /// conversion's target, or a captured picture -- made an input the
    /// encoder keeps, answered by its place among them: submitted by that
    /// place with [`Encoder::submit_registered`] as often as it holds a
    /// picture. Kept as long as the encoder, which gives back every picture
    /// it holds before its inputs go.
    pub fn register_texture(&mut self, texture: &Com<ID3D11Texture2D>) -> Result<usize, Error> {
        if !self.reads(texture) {
            return Err(Error::Layout);
        }
        self.registered.push(self.context.input(texture)?);
        Ok(self.registered.len() - 1)
    }

    /// Whether `texture` is in the layout the encoder was built to read.
    fn reads(&self, texture: &Com<ID3D11Texture2D>) -> bool {
        // SAFETY: plain data the call fills whole.
        let mut desc: D3D11_TEXTURE2D_DESC = unsafe { core::mem::zeroed() };
        // SAFETY: a live texture; the output is a live local.
        unsafe { vcall!(texture.as_ptr(), GetDesc, &raw mut desc) };
        match (self.config.source, self.config.depth) {
            (Source::Rgb, _) => desc.Format == DXGI_FORMAT_B8G8R8A8_UNORM,
            (Source::Yuv, Depth::Eight) => desc.Format == DXGI_FORMAT_NV12,
            (Source::Yuv, Depth::Ten) => desc.Format == DXGI_FORMAT_P010,
        }
    }

    /// Hand over the picture in `texture`, a capture's own on the encoder's
    /// device, read as it is: made an input the first time it is seen and
    /// kept while it goes on being handed over. The texture must hold the
    /// picture until [`Engine::busy`] says the engine is done with it.
    pub fn submit_texture(
        &mut self,
        texture: &Com<ID3D11Texture2D>,
        force_keyframe: bool,
    ) -> Result<(), Error> {
        if self.outstanding >= IN_FLIGHT {
            return Err(Error::QueueFull);
        }
        self.failed()?;
        let raw = texture.as_ptr();
        let at = match self.captured.iter().position(|(seen, _)| *seen == raw) {
            Some(at) => at,
            None => {
                if !self.reads(texture) {
                    return Err(Error::Layout);
                }
                // **A texture not seen before replaces those that were**, once
                // nothing in the engine can be reading them: a capture keeps
                // one texture a duplication, and lets the old go with it.
                if self.shared.in_engine.load(Ordering::Acquire) == 0 {
                    self.captured.clear();
                }
                self.captured.push((raw, self.context.input(texture)?));
                self.captured.len() - 1
            }
        };
        let (_, made) = &self.captured[at];
        queue(
            &self.component,
            self.names,
            &self.shared,
            made,
            force_keyframe,
        )?;
        self.outstanding += 1;
        Ok(())
    }

    /// Hand over a picture already on the device, in the input
    /// [`Encoder::register_texture`] answered `input` for. The texture must
    /// hold the picture until the unit made from it is collected.
    pub fn submit_registered(&mut self, input: usize, force_keyframe: bool) -> Result<(), Error> {
        if self.outstanding >= IN_FLIGHT {
            return Err(Error::QueueFull);
        }
        self.failed()?;
        let made = self.registered.get(input).ok_or(Error::Unregistered)?;
        queue(
            &self.component,
            self.names,
            &self.shared,
            made,
            force_keyframe,
        )?;
        self.outstanding += 1;
        Ok(())
    }

    /// Hand over a frame from ordinary memory, written into the next of the
    /// generator's inputs first.
    pub fn submit(
        &mut self,
        frame: &lowlat_capture::Frame<'_>,
        force_keyframe: bool,
    ) -> Result<(), Error> {
        if self.outstanding >= IN_FLIGHT {
            return Err(Error::QueueFull);
        }
        self.failed()?;
        // An input is written again only once its picture was collected: as
        // many inputs as pictures may be held, taken in turn.
        let slot = self.next_slot;
        let target = self.slots.get(slot).ok_or(Error::Layout)?;
        upload(
            self.context.device(),
            &target.staging,
            &target.texture,
            frame,
            (self.config.width, self.config.height),
            self.config.depth == Depth::Ten,
        )
        .map_err(Error::Device)?;
        queue(
            &self.component,
            self.names,
            &self.shared,
            &target.input,
            force_keyframe,
        )?;
        self.next_slot = (slot + 1) % self.slots.len();
        self.outstanding += 1;
        Ok(())
    }

    /// The next unit, if the collecting thread has one; never blocks. The
    /// bytes are the runtime's buffer, valid until the next call.
    pub fn poll(&mut self) -> Result<Poll<'_>, Error> {
        self.held = None;
        if let Some(unit) = self.shared.ring.pop() {
            self.outstanding = self.outstanding.saturating_sub(1);
            let kind = unit.get_int(self.names.output_data_type);
            let keyframe = matches!(kind, Ok(runtime::OUTPUT_IDR | runtime::OUTPUT_I));
            let held = self.held.insert(unit);
            return Ok(Poll::Ready {
                bitstream: held.bytes(),
                keyframe,
            });
        }
        self.failed()?;
        Ok(Poll::Pending)
    }

    /// Change the rate, live: the target and the peak, and the buffer and
    /// the largest picture with it.
    pub fn reconfigure(&mut self, bitrate_bps: u32) -> Result<(), Error> {
        let names = self.names;
        let vbv = i64::from(crate::nvenc::vbv_bits(bitrate_bps, self.config.fps));
        let rate = i64::from(bitrate_bps);
        for (name, value) in [
            (names.target_bitrate, rate),
            (names.peak_bitrate, rate),
            (names.vbv_buffer_size, vbv),
            (names.max_au_size, vbv),
        ] {
            self.component
                .set_int(name, value)
                .map_err(|error| Error::Setting(name, error))?;
        }
        self.config.bitrate_bps = bitrate_bps;
        Ok(())
    }

    /// Sleep until the collecting thread hands a unit over or `limit`
    /// passes; true when there was a picture to sleep on.
    pub fn wait_finished(&mut self, limit: Duration) -> bool {
        if !self.shared.ring.is_empty() {
            return true;
        }
        if self.outstanding == 0 {
            return false;
        }
        let seen = self.shared.ready.load(Ordering::Acquire);
        if self.shared.ring.is_empty() && self.shared.failed.load(Ordering::Acquire) == 0 {
            wait::wait(&self.shared.ready, seen, limit);
        }
        true
    }

    /// The collecting thread's failure, once it has one.
    fn failed(&self) -> Result<(), Error> {
        match self.shared.failed.load(Ordering::Acquire) {
            0 => Ok(()),
            status => Err(Error::Collect(status)),
        }
    }
}

impl Drop for Encoder<'_> {
    /// The thread stopped and joined -- within one query's wait -- and every
    /// unit it handed over given back, before the encoder goes.
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        self.shared.submitted.fetch_add(1, Ordering::Release);
        wait::notify_all(&self.shared.submitted);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        self.held = None;
        while self.shared.ring.pop().is_some() {}
    }
}

impl EncoderTrait for Encoder<'_> {
    type Error = Error;

    fn submit(
        &mut self,
        frame: &lowlat_capture::Frame<'_>,
        force_keyframe: bool,
    ) -> Result<(), Error> {
        Encoder::submit(self, frame, force_keyframe)
    }

    fn poll(&mut self) -> Result<Poll<'_>, Error> {
        Encoder::poll(self)
    }

    fn reconfigure(&mut self, bitrate_bps: u32) -> Result<(), Error> {
        Encoder::reconfigure(self, bitrate_bps)
    }

    fn wait_finished(&mut self, limit: Duration) -> bool {
        Encoder::wait_finished(self, limit)
    }
}

/// Hand `input` to the encoder for one picture, its own properties written
/// first -- every time, since a property written once stays on the input --
/// and tell the collecting thread a picture is in the engine.
fn queue(
    encoder: &runtime::Encoder<'_>,
    names: &'static Names,
    shared: &Shared,
    input: &runtime::Input<'_>,
    force_keyframe: bool,
) -> Result<(), Error> {
    let set = |name: &'static str, result: Result<(), runtime::Error>| {
        result.map_err(|error| Error::Setting(name, error))
    };
    let kind = if force_keyframe {
        runtime::PICTURE_IDR
    } else {
        runtime::PICTURE_NONE
    };
    set(
        names.force_picture_type,
        input.set_int(names.force_picture_type, kind),
    )?;
    for header in names.insert_headers {
        set(header, input.set_bool(header, force_keyframe))?;
    }
    set(names.insert_aud, input.set_bool(names.insert_aud, false))?;
    if !encoder.submit(input)? {
        return Err(Error::QueueFull);
    }
    // Counted before the thread is told, so a thread that wakes finds it.
    shared.in_engine.fetch_add(1, Ordering::Release);
    shared.submitted.fetch_add(1, Ordering::Release);
    wait::notify_one(&shared.submitted);
    Ok(())
}

/// The collecting thread: query while something is in the engine, sleep
/// while nothing is, and hand every unit over.
fn collect(shared: &Shared, query: &QueryHandle) {
    while !shared.stop.load(Ordering::Acquire) {
        // Read before the count, so a submit between the two moves it and
        // the sleep below returns at once.
        let seen = shared.submitted.load(Ordering::Acquire);
        if shared.in_engine.load(Ordering::Acquire) == 0 {
            wait::wait(&shared.submitted, seen, IDLE_WAIT);
            continue;
        }
        match query.query() {
            Ok(Some(mut unit)) => {
                let late_us = shared.late_us.load(Ordering::Acquire);
                if late_us > 0 {
                    std::thread::sleep(Duration::from_micros(late_us));
                }
                // **The read's end before the count**, so a reader that finds
                // the engine idle finds when it went idle too.
                shared
                    .out_us
                    .store(lowlat_common::clock::monotonic_us(), Ordering::Release);
                shared.in_engine.fetch_sub(1, Ordering::AcqRel);
                // The ring holds as many as the encoder does, so this finds
                // room; a unit is never dropped, it carries the stream.
                loop {
                    match shared.ring.push(unit) {
                        Ok(()) => break,
                        Err(back) if shared.stop.load(Ordering::Acquire) => {
                            drop(back);
                            return;
                        }
                        Err(back) => {
                            unit = back;
                            std::thread::yield_now();
                        }
                    }
                }
                shared.ready.fetch_add(1, Ordering::Release);
                wait::notify_one(&shared.ready);
            }
            Ok(None) => {}
            Err(error) => {
                let status = match error {
                    runtime::Error::Status(status) => status,
                    _ => -1,
                };
                shared.failed.store(status, Ordering::Release);
                shared.ready.fetch_add(1, Ordering::Release);
                wait::notify_all(&shared.ready);
                return;
            }
        }
    }
}

/// Every setting the encoder is built with, the usage first: it is a preset
/// of everything written after it.
fn configure(
    encoder: &runtime::Encoder<'_>,
    codec: runtime::Codec,
    names: &'static Names,
    config: &Config,
) -> Result<(), Error> {
    let int = |name: &'static str, value: i64| {
        encoder
            .set_int(name, value)
            .map_err(|error| Error::Setting(name, error))
    };
    let flag = |name: &'static str, value: bool| {
        encoder
            .set_bool(name, value)
            .map_err(|error| Error::Setting(name, error))
    };
    int(names.usage, runtime::USAGE_ULTRA_LOW_LATENCY)?;
    match (config.depth, names.profile_10) {
        (Depth::Ten, Some(profile)) => {
            int(names.profile, profile)?;
            int(names.color_bit_depth, 10)?;
        }
        _ => {
            int(names.profile, names.profile_8)?;
            int(names.color_bit_depth, 8)?;
        }
    }
    encoder
        .set_size(names.frame_size, config.width, config.height)
        .map_err(|error| Error::Setting(names.frame_size, error))?;
    encoder
        .set_rate(names.frame_rate, config.fps.max(1), 1)
        .map_err(|error| Error::Setting(names.frame_rate, error))?;
    int(
        names.quality_preset,
        if config.thorough {
            names.preset_quality
        } else {
            names.preset_speed
        },
    )?;
    // **The rate that bounds each picture by the buffer**, and the largest
    // picture bounded too: the peak-constrained kind let a scene change reach
    // thirty times the buffer and the rate two and a half times the target.
    int(names.rate_control, names.rate_latency_constrained)?;
    let rate = i64::from(config.bitrate_bps);
    let vbv = i64::from(crate::nvenc::vbv_bits(config.bitrate_bps, config.fps));
    int(names.target_bitrate, rate)?;
    int(names.peak_bitrate, rate)?;
    int(names.vbv_buffer_size, vbv)?;
    int(names.vbv_initial_fullness, runtime::VBV_FULL)?;
    int(names.max_au_size, vbv)?;
    flag(names.filler, false)?;
    flag(names.skip_frame, false)?;
    int(names.max_reference_frames, 1)?;
    int(names.slices, 1)?;
    for name in names.min_qp {
        int(name, i64::from(config.min_qp))?;
    }
    // A flag on one codec and a number on the other, which is the runtime's
    // shape.
    match codec {
        runtime::Codec::H264 => int(names.preencode, i64::from(config.thorough))?,
        runtime::Codec::Hevc => flag(names.preencode, config.thorough)?,
    }
    // **The colour is the host's to state**: BT.709 in the video range, in
    // and out, whichever side converts.
    flag(names.full_range, false)?;
    for name in [
        names.in_color_profile,
        names.out_color_profile,
        names.in_primaries,
        names.out_primaries,
        names.in_transfer,
        names.out_transfer,
    ] {
        int(name, runtime::BT709)?;
    }
    // Recovery is a keyframe asked for; nothing scheduled, nothing rolling.
    int(names.intra_refresh, 0)?;
    for name in names.keyframe_period {
        int(name, 0)?;
    }
    if let Some(mode) = names.header_insertion {
        int(mode, runtime::HEADERS_AT_IDR)?;
    }
    int(names.query_timeout, QUERY_WAIT_MS)?;
    Ok(())
}
