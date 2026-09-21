//! The decode thread: units in from the session's thread, pictures out to
//! the application's, and the keyframe policy between them.
//!
//! One per stream (one stream in v1). It never blocks the receive loop: a
//! full pool leaves the backlog in the receive ring, where the catch-up sees
//! it; a full picture queue is never full, because the queue steals. The
//! device is opened here, on this thread, and lives as long as it does.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use lowlat_common::events;
use lowlat_core::video;
use lowlat_decode::nvdec::DevicePlanes;
use lowlat_decode::vaapi::Vaapi;
use lowlat_decode::{Decoder, Fault, Fed, Format, Picture, nvdec, software, vaapi};
use lowlat_drivers::cuda::Cuda;
use lowlat_drivers::cuvid::Cuvid;
use lowlat_drivers::lavc::Lavc;
use lowlat_net::WakeHandle;

use crate::UNIT_BYTES;
use crate::config::FrameKind;
use crate::driver::{Telemetry, Units};
use crate::feed::{Decision, Feed};
use crate::frames::{Frame, Frames};
use crate::report::Smoothed;
use crate::seam::{Event, Opened, Outcome};

/// How long the thread waits for a unit before looking at the stop flag.
const IDLE_WAIT: Duration = Duration::from_millis(50);

/// What either backend tells the thread beyond the decoder trait: the
/// layout of what it hands out, what the last picture cost, and the
/// device route for a backend that has one.
trait Backend: Decoder {
    fn output(&self) -> Option<(u32, u32, Format)>;
    /// The last picture's decode wait and hand-over, in microseconds.
    fn timings(&self) -> (u32, u32);
    /// The next picture into device memory. Only a backend that exports
    /// is ever asked, because creation refuses the handle kind for the
    /// rest; a fault here is the answer if one is asked anyway.
    fn take_to_device(&mut self, out: &DevicePlanes) -> Result<Option<Picture>, Fault> {
        let _ = out;
        Err(Fault::Fatal)
    }
}

impl Backend for vaapi::Backend<'_> {
    fn output(&self) -> Option<(u32, u32, Format)> {
        vaapi::Backend::output(self)
    }
    fn timings(&self) -> (u32, u32) {
        (self.decode_us, self.readback_us)
    }
}

impl Backend for nvdec::Backend<'_> {
    fn output(&self) -> Option<(u32, u32, Format)> {
        nvdec::Backend::output(self)
    }
    fn timings(&self) -> (u32, u32) {
        (self.decode_us, self.readback_us)
    }
    fn take_to_device(&mut self, out: &DevicePlanes) -> Result<Option<Picture>, Fault> {
        nvdec::Backend::take_to_device(self, out)
    }
}

impl Backend for software::Backend<'_> {
    fn output(&self) -> Option<(u32, u32, Format)> {
        software::Backend::output(self)
    }
    fn timings(&self) -> (u32, u32) {
        (self.decode_us, self.readback_us)
    }
}

/// What the thread is handed.
pub(crate) struct Attached {
    /// The decoder chosen at creation, or none: then units are taken and
    /// dropped.
    pub opened: Option<Opened>,
    pub units: Units,
    pub frames: Arc<Frames>,
    pub telemetry: Arc<Telemetry>,
    pub emit: events::Sender<Event>,
    /// The session thread's wake, for the keyframe request.
    pub shell: WakeHandle,
    pub stopping: Arc<std::sync::atomic::AtomicBool>,
}

pub(crate) fn run(args: Attached) {
    let Attached {
        opened,
        units,
        frames,
        telemetry,
        emit,
        shell,
        stopping,
    } = args;

    // The runtimes are opened here, on this thread, and live as long as it
    // does; the vendor's context is made current here, where every call
    // against it is made.
    match opened {
        None => {
            while !stopping.load(Ordering::Acquire) {
                if units.take().is_none() {
                    units.wait(IDLE_WAIT);
                }
            }
            frames.close();
        }
        Some(Opened::Vaapi(node)) => {
            let Ok(va) = Vaapi::load() else {
                fail(&telemetry, &emit, &frames);
                return;
            };
            let Ok(display) = va.open(&node) else {
                fail(&telemetry, &emit, &frames);
                return;
            };
            let backend = vaapi::Backend::new(&display, frames.ceiling());
            drive(
                backend, &units, &frames, &telemetry, &emit, &shell, &stopping,
            );
        }
        Some(Opened::Nvdec(address)) => {
            let Ok(cuda) = Cuda::load() else {
                fail(&telemetry, &emit, &frames);
                return;
            };
            let device = match address {
                Some(address) => cuda.device_at(address),
                None => cuda.any_device(),
            };
            let Ok(device) = device else {
                fail(&telemetry, &emit, &frames);
                return;
            };
            let Ok(context) = cuda.retain_primary(&device) else {
                fail(&telemetry, &emit, &frames);
                return;
            };
            if context.make_current().is_err() {
                fail(&telemetry, &emit, &frames);
                return;
            }
            let Ok(cuvid) = Cuvid::load() else {
                fail(&telemetry, &emit, &frames);
                return;
            };
            // The queue's device slots are made through this runtime, and
            // may outlive this thread while the application holds one, so
            // the queue keeps its own reference to it.
            let cuda = Arc::new(cuda);
            if frames.kind() == FrameKind::Handle {
                frames.open_device(Arc::clone(&cuda), device);
            }
            let backend = nvdec::Backend::new(&cuda, &cuvid, frames.ceiling(), UNIT_BYTES);
            drive(
                backend, &units, &frames, &telemetry, &emit, &shell, &stopping,
            );
        }
        Some(Opened::Software(dir)) => {
            // The same search creation ran, landing on the same pair.
            let Ok(lavc) = Lavc::load(dir.as_deref()) else {
                fail(&telemetry, &emit, &frames);
                return;
            };
            let backend = software::Backend::new(&lavc);
            drive(
                backend, &units, &frames, &telemetry, &emit, &shell, &stopping,
            );
        }
    }
}

/// The loop: units in, pictures out, the policy between.
fn drive<D: Backend>(
    backend: D,
    units: &Units,
    frames: &Frames,
    telemetry: &Telemetry,
    emit: &events::Sender<Event>,
    shell: &WakeHandle,
    stopping: &std::sync::atomic::AtomicBool,
) {
    let mut feed = Feed::new(backend);
    let mut reported = Smoothed::default();
    let mut reconfigured = telemetry.reconfigure.load(Ordering::Acquire);

    while !stopping.load(Ordering::Acquire) {
        // A declaration changed under the decoder: torn down here, and the
        // keyframe asked for, as one act.
        let generation = telemetry.reconfigure.load(Ordering::Acquire);
        if generation != reconfigured {
            reconfigured = generation;
            if feed.reconfigure() == Decision::Request {
                telemetry.request.store(true, Ordering::Release);
                let _ = shell.notify();
            }
        }
        let Some(unit) = units.take() else {
            units.wait(IDLE_WAIT);
            continue;
        };
        let bytes = unit.bytes();
        let header = video::parse(bytes).ok();
        let decision = feed.feed(bytes);
        drop(unit);

        match decision {
            Decision::Request => {
                telemetry.request.store(true, Ordering::Release);
                let _ = shell.notify();
            }
            Decision::Failed => {
                fail(telemetry, emit, frames);
                return;
            }
            Decision::Fed(Fed::Picture) => {
                telemetry.decoder.store(1, Ordering::Relaxed);
                take_pictures(&mut feed, frames, telemetry, header.as_ref(), &mut reported);
            }
            Decision::Built(fed) => {
                telemetry.decoder.store(1, Ordering::Relaxed);
                telemetry.codec.store(
                    header.as_ref().map_or(0, |h| u32::from(h.codec.wire())),
                    Ordering::Relaxed,
                );
                telemetry.stream_format.store(
                    feed.decoder()
                        .output()
                        .map_or(0, |(_, _, f)| format_code(f)),
                    Ordering::Relaxed,
                );
                if fed == Fed::Picture {
                    take_pictures(&mut feed, frames, telemetry, header.as_ref(), &mut reported);
                }
            }
            _ => {}
        }
        telemetry.queue_depth.store(
            u32::try_from(frames.ready()).unwrap_or(u32::MAX),
            Ordering::Relaxed,
        );
    }
    frames.close();
}

/// Every picture the decoder has ready goes into the queue.
fn take_pictures<D: Backend>(
    feed: &mut Feed<D>,
    frames: &Frames,
    telemetry: &Telemetry,
    header: Option<&video::VideoHeader>,
    reported: &mut Smoothed,
) {
    loop {
        // The layout before the take: the planes are the picture's own size.
        let Some((width, height, format)) = feed.decoder().output() else {
            return;
        };
        let Some(mut filling) = frames.fill() else {
            return;
        };
        let taken = match frames.kind() {
            FrameKind::Planes => {
                let Some(mut planes) = filling.planes_for(width, height, format) else {
                    return;
                };
                lowlat_decode::Decoder::take(feed.decoder_mut(), &mut planes)
            }
            FrameKind::Handle => {
                let Some(planes) = filling.device_planes_for(width, height, format) else {
                    return;
                };
                feed.decoder_mut().take_to_device(&planes)
            }
        };
        match taken {
            Ok(Some(picture)) => {
                let (decode_us, readback_us) = feed.decoder().timings();
                telemetry.decode_us.store(decode_us, Ordering::Relaxed);
                telemetry.readback_us.store(readback_us, Ordering::Relaxed);
                // What the stream is, from the picture itself: a backend
                // that reads no parameter set knows it no earlier.
                telemetry
                    .stream_format
                    .store(format_code(picture.format), Ordering::Relaxed);
                // What the host is told: decode and hand-over together,
                // smoothed, since that is the time a picture costs here.
                let sample_ms = f64::from(decode_us.saturating_add(readback_us)) / 1000.0;
                telemetry
                    .decode_reported_us
                    .store(reported.push(sample_ms), Ordering::Relaxed);
                filling.publish(Frame {
                    format: picture.format,
                    width: picture.width,
                    height: picture.height,
                    rotation: header.map_or(video::Rotation::None, |h| h.rotation),
                    generation: header.map_or(0, |h| h.frame_id),
                    order: picture.order,
                    // The queue's, written at publish.
                    pitch: 0,
                    uv_offset: 0,
                    v_offset: 0,
                    handle: None,
                });
                telemetry.decoded.fetch_add(1, Ordering::Relaxed);
            }
            Ok(None) => return,
            Err(_) => {
                // The read-back failed: the picture is lost and the decoder
                // is left to its next unit; a fault there is the feed's to
                // judge.
                return;
            }
        }
    }
}

fn fail(telemetry: &Telemetry, emit: &events::Sender<Event>, frames: &Frames) {
    telemetry.decoder.store(2, Ordering::Release);
    frames.close();
    emit.send(Event::Ended {
        outcome: Outcome::DecoderFailed,
    });
}

/// The layout a format is handed out as. Named here so the seam and the
/// boundary agree on one word.
pub fn format_code(format: Format) -> u32 {
    match format {
        Format::Nv12 => 1,
        Format::P010 => 2,
        Format::Yuv444 => 3,
        Format::Yuv444_16 => 4,
    }
}
