//! The decode thread: units in from the session's thread, pictures out to
//! the application's, and the keyframe policy between them.
//!
//! One per stream (one stream in v1). It never blocks the receive loop: a
//! full pool leaves the backlog in the receive ring, where the catch-up sees
//! it; a full picture queue is never full, because the queue steals. The
//! device is opened here, on this thread, and lives as long as it does.

use std::ffi::CString;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use lowlat_common::events;
use lowlat_core::video;
use lowlat_decode::vaapi::{Backend, Vaapi};
use lowlat_decode::{Fed, Format};
use lowlat_net::WakeHandle;

use crate::driver::{Telemetry, Units};
use crate::feed::{Decision, Feed};
use crate::frames::{Frame, Frames};
use crate::seam::{Event, Outcome};

/// How long the thread waits for a unit before looking at the stop flag.
const IDLE_WAIT: Duration = Duration::from_millis(50);

/// What the thread is handed.
pub(crate) struct Attached {
    /// The render node, or none: then units are taken and dropped.
    pub node: Option<CString>,
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
        node,
        units,
        frames,
        telemetry,
        emit,
        shell,
        stopping,
    } = args;

    let Some(node) = node else {
        while !stopping.load(Ordering::Acquire) {
            if units.take().is_none() {
                units.wait(IDLE_WAIT);
            }
        }
        frames.close();
        return;
    };
    let Ok(va) = Vaapi::load() else {
        fail(&telemetry, &emit, &frames);
        return;
    };
    let Ok(display) = va.open(&node) else {
        fail(&telemetry, &emit, &frames);
        return;
    };
    let mut feed = Feed::new(Backend::new(&display, frames.ceiling()));

    while !stopping.load(Ordering::Acquire) {
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
                fail(&telemetry, &emit, &frames);
                return;
            }
            Decision::Fed(Fed::Picture) => {
                telemetry.decoder.store(1, Ordering::Relaxed);
                take_pictures(&mut feed, &frames, &telemetry, header.as_ref());
            }
            Decision::Built(fed) => {
                telemetry.decoder.store(1, Ordering::Relaxed);
                telemetry.codec.store(
                    header.as_ref().map_or(0, |h| u32::from(h.codec.wire())),
                    Ordering::Relaxed,
                );
                if fed == Fed::Picture {
                    take_pictures(&mut feed, &frames, &telemetry, header.as_ref());
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
fn take_pictures(
    feed: &mut Feed<Backend<'_>>,
    frames: &Frames,
    telemetry: &Telemetry,
    header: Option<&video::VideoHeader>,
) {
    loop {
        // The layout before the take: the planes are the picture's own size.
        let Some((width, height, format)) = feed.decoder().output() else {
            return;
        };
        let Some(mut filling) = frames.fill() else {
            return;
        };
        let Some(mut planes) = filling.planes_for(width, height, format) else {
            return;
        };
        match lowlat_decode::Decoder::take(feed.decoder_mut(), &mut planes) {
            Ok(Some(picture)) => {
                let backend = feed.decoder();
                telemetry
                    .decode_us
                    .store(backend.decode_us, Ordering::Relaxed);
                telemetry
                    .readback_us
                    .store(backend.readback_us, Ordering::Relaxed);
                filling.publish(Frame {
                    format: picture.format,
                    width: picture.width,
                    height: picture.height,
                    rotation: header.map_or(video::Rotation::None, |h| h.rotation),
                    chroma_444: false,
                    generation: header.map_or(0, |h| h.frame_id),
                    order: picture.order,
                    // The queue's, written at publish.
                    pitch: 0,
                    uv_offset: 0,
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
    }
}
