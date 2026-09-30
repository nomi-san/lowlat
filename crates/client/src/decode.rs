//! The decode thread: units in from the session's thread, pictures out to
//! the application's, and the keyframe policy between them.
//!
//! One per stream (one stream in v1). It never blocks the receive loop: a
//! full pool leaves the backlog in the receive ring, where the catch-up sees
//! it; a full picture queue is never full, because the queue steals. The
//! device is opened here, on this thread, and lives as long as it does.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lowlat_core::video;
use lowlat_decode::{Decoder, Fault, Fed, Format, Picture, software};
use lowlat_net::WakeHandle;

use crate::config::FrameKind;
use crate::driver::{DECODER_FAILED, Telemetry, Units};
use crate::feed::{Decision, Feed};
use crate::frames::{Filling, Frame, Frames};
use crate::report::Smoothed;
use crate::seam::Opened;

/// The decoders a stream is opened on, which are the platform's; the thread
/// around them is written once.
#[cfg(target_os = "linux")]
#[path = "decode/linux.rs"]
mod sys;
#[cfg(windows)]
#[path = "decode/windows.rs"]
mod sys;

/// How long the thread waits for a unit before looking at the stop flag.
const IDLE_WAIT: Duration = Duration::from_millis(50);

/// What either backend tells the thread beyond the decoder trait: the
/// layout of what it hands out, what the last picture cost, and the
/// device route for a backend that has one.
trait Backend: Decoder {
    fn output(&self) -> Option<(u32, u32, Format)>;
    /// The last picture's decode wait and hand-over, in microseconds.
    fn timings(&self) -> (u32, u32);
    /// Whether it hands pictures out as a handle; the rest hand out planes
    /// whatever the session asked for.
    fn exports(&self) -> bool {
        false
    }
    /// The next picture into the slot `filling` lends, as a handle, with the
    /// value its gate opens at (zero for a picture finished when it is
    /// taken). Only a backend that exports is asked.
    fn take_to_slot(
        &mut self,
        filling: &mut Filling<'_>,
        layout: (u32, u32, Format),
    ) -> Result<Option<(Picture, u64)>, Fault> {
        let _ = (filling, layout);
        Err(Fault::Fatal)
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

/// A decoder the application chose mid-session, left by the seam for the
/// decode thread to take once the session thread has said the word.
pub(crate) type Pending = Arc<Mutex<Option<Opened>>>;

/// What the thread is handed.
pub(crate) struct Attached {
    /// The decoder chosen at creation, or none: then units are taken and
    /// dropped.
    pub opened: Option<Opened>,
    /// Where the next choice arrives.
    pub pending: Pending,
    pub units: Units,
    pub frames: Arc<Frames>,
    pub telemetry: Arc<Telemetry>,
    /// The session thread's wake, for the keyframe request and for a
    /// decoder that failed for good, which the session thread acts on.
    pub shell: WakeHandle,
    pub stopping: Arc<std::sync::atomic::AtomicBool>,
}

/// Why a decoder's loop returned.
#[derive(Debug, PartialEq, Eq)]
enum Next {
    /// The session is over.
    Stop,
    /// No decoder can serve the stream.
    Failed,
    /// The decoder's device is gone; the platform finds the GPU again and
    /// goes round, or fails the stream.
    Lost,
    /// The application chose another decoder: the old one is gone, and the
    /// one named is to be opened on this thread.
    Switch(Opened),
}

/// Everything a decoder's loop shares with the thread around it.
struct Shared<'a> {
    units: &'a Units,
    frames: &'a Frames,
    telemetry: &'a Telemetry,
    shell: &'a WakeHandle,
    stopping: &'a std::sync::atomic::AtomicBool,
    pending: &'a Pending,
}

impl Shared<'_> {
    /// The choice the application left, if the session thread has said so
    /// since `seen`; `seen` follows the generation.
    fn switch(&self, seen: &mut u32) -> Option<Opened> {
        let generation = self.telemetry.switch.load(Ordering::Acquire);
        if generation == *seen {
            return None;
        }
        *seen = generation;
        self.pending.lock().ok().and_then(|mut slot| slot.take())
    }
}

pub(crate) fn run(args: Attached) {
    let Attached {
        opened,
        pending,
        units,
        frames,
        telemetry,
        shell,
        stopping,
    } = args;
    let shared = Shared {
        units: &units,
        frames: &frames,
        telemetry: &telemetry,
        shell: &shell,
        stopping: &stopping,
        pending: &pending,
    };

    // The runtimes are opened here, on this thread, and live as long as the
    // decoder built on them does; the vendor's context is made current
    // here, where every call against it is made. A choice made mid-session
    // comes back as `Switch`, and the loop goes round with it: the old
    // runtime dropped, the new one opened, the queue never closed.
    let mut opened = opened;
    // Whether the decoder is a replacement, which asks for a keyframe once
    // it can take one; the first never asks, the host's own start sends one.
    let mut replacing = false;
    loop {
        let next = match opened {
            None => idle(&shared),
            Some(opened) => sys::open(opened, &shared, replacing),
        };
        match next {
            Next::Switch(choice) => {
                opened = Some(choice);
                replacing = true;
            }
            other => break other,
        }
    }
    .finish(&telemetry, &shell, &frames);
}

impl Next {
    /// What the thread does last: the queue closed either way, and the
    /// session ended when no decoder can serve the stream.
    fn finish(self, telemetry: &Telemetry, shell: &WakeHandle, frames: &Frames) {
        match self {
            // A loss no platform recovered from ends the stream as a failure.
            Next::Failed | Next::Lost => fail(telemetry, shell, frames),
            Next::Stop => frames.close(),
            Next::Switch(_) => unreachable!("a switch is taken by the loop"),
        }
    }
}

/// No decoder: units are taken and dropped, until the session ends or the
/// application chooses a decoder after all.
fn idle(shared: &Shared<'_>) -> Next {
    let mut switched = shared.telemetry.switch.load(Ordering::Acquire);
    while !shared.stopping.load(Ordering::Acquire) {
        if let Some(choice) = shared.switch(&mut switched) {
            return Next::Switch(choice);
        }
        if shared.units.take().is_none() {
            shared.units.wait(IDLE_WAIT);
        }
    }
    Next::Stop
}

/// The loop: units in, pictures out, the policy between.
fn drive<D: Backend>(backend: D, shared: &Shared<'_>, replacing: bool) -> Next {
    let Shared {
        units,
        frames,
        telemetry,
        shell,
        stopping,
        ..
    } = *shared;
    let mut feed = Feed::new(backend);
    let mut reported = Smoothed::default();
    let mut range = None;
    let mut reconfigured = telemetry.reconfigure.load(Ordering::Acquire);
    let mut switched = telemetry.switch.load(Ordering::Acquire);

    // A replacement decoder exists now, so the keyframe it needs is asked
    // for now and not before: nothing the host sends is wasted on a decoder
    // that was still opening, and a runtime that fails to open costs the
    // host nothing.
    if replacing && feed.reconfigure() == Decision::Request {
        telemetry.request.store(true, Ordering::Release);
        let _ = shell.notify();
    }

    while !stopping.load(Ordering::Acquire) {
        // The application chose another decoder: this one is torn down
        // here, and the thread goes round to open the other. The request
        // belongs to the other, once it exists.
        if let Some(choice) = shared.switch(&mut switched) {
            feed.replaced();
            return Next::Switch(choice);
        }
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
        let stamp = unit.stamp();
        let decision = feed.feed(bytes);
        drop(unit);

        match decision {
            Decision::Request => {
                telemetry.request.store(true, Ordering::Release);
                let _ = shell.notify();
            }
            Decision::Failed => return Next::Failed,
            Decision::Lost => return Next::Lost,
            Decision::Fed(Fed::Picture) => {
                telemetry.decoder.store(1, Ordering::Relaxed);
                let lost = take_pictures(
                    &mut feed,
                    frames,
                    telemetry,
                    header.as_ref(),
                    stamp,
                    &mut reported,
                    &mut range,
                );
                if lost {
                    feed.lost();
                    return Next::Lost;
                }
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
                    let lost = take_pictures(
                        &mut feed,
                        frames,
                        telemetry,
                        header.as_ref(),
                        stamp,
                        &mut reported,
                        &mut range,
                    );
                    if lost {
                        feed.lost();
                        return Next::Lost;
                    }
                }
            }
            _ => {}
        }
        telemetry.queue_depth.store(
            u32::try_from(frames.ready()).unwrap_or(u32::MAX),
            Ordering::Relaxed,
        );
    }
    Next::Stop
}

/// Every picture the decoder has ready goes into the queue, carrying the
/// arrival stamp of the unit just fed. `range` is the last picture's, for
/// the log. **Whether the decoder's device was found lost**: a removed
/// device can go on taking decodes and fail only here, so the loss is the
/// loop's to act on, as one the feed reports is.
fn take_pictures<D: Backend>(
    feed: &mut Feed<D>,
    frames: &Frames,
    telemetry: &Telemetry,
    header: Option<&video::VideoHeader>,
    stamp: u32,
    reported: &mut Smoothed,
    range: &mut Option<bool>,
) -> bool {
    loop {
        // The layout before the take: the planes are the picture's own size.
        let Some((width, height, format)) = feed.decoder().output() else {
            return false;
        };
        let Some(mut filling) = frames.fill() else {
            return false;
        };
        // By handle when the session asks for one and the decoder can hand
        // one out; as planes otherwise, whatever was asked. Decided per
        // picture, so the kind switches at the next one with no keyframe.
        let by_handle = frames.kind() == FrameKind::Handle && feed.decoder().exports();
        let taken = if by_handle {
            feed.decoder_mut()
                .take_to_slot(&mut filling, (width, height, format))
        } else {
            let Some(mut planes) = filling.planes_for(width, height, format) else {
                return false;
            };
            lowlat_decode::Decoder::take(feed.decoder_mut(), &mut planes).map(|p| p.map(|p| (p, 0)))
        };
        match taken {
            Ok(Some((picture, gate))) => {
                // A picture behind a gate was not waited for here: its
                // decode is the backend's timing on the device of the last
                // one finished, from its take to its fence passing.
                let (decode_us, readback_us) = feed.decoder().timings();
                telemetry.decode_us.store(decode_us, Ordering::Relaxed);
                telemetry.readback_us.store(readback_us, Ordering::Relaxed);
                // What the stream is, from the picture itself: a backend
                // that reads no parameter set knows it no earlier.
                telemetry
                    .stream_format
                    .store(format_code(picture.format), Ordering::Relaxed);
                // What the host is told: decode and hand-over together,
                // smoothed, since that is the time a picture costs here. A
                // picture behind a gate is timed across its hand-over.
                let cost_us = if gate == 0 {
                    decode_us.saturating_add(readback_us)
                } else {
                    decode_us
                };
                let sample_ms = f64::from(cost_us) / 1000.0;
                telemetry
                    .decode_reported_us
                    .store(reported.push(sample_ms), Ordering::Relaxed);
                // Said when it is learned and when it changes, never per
                // picture: a renderer told the wrong range shows a picture
                // that decodes perfectly and looks wrong.
                if *range != Some(picture.full_range) {
                    *range = Some(picture.full_range);
                    lowlat_common::log_info!(
                        "client: picture range set, full_range={}",
                        picture.full_range
                    );
                }
                let frame = Frame {
                    format: picture.format,
                    width: picture.width,
                    height: picture.height,
                    rotation: header.map_or(video::Rotation::None, |h| h.rotation),
                    generation: header.map_or(0, |h| h.frame_id),
                    order: picture.order,
                    full_range: picture.full_range,
                    arrived: Some(stamp),
                    // The queue's, written at publish.
                    pitch: 0,
                    uv_offset: 0,
                    v_offset: 0,
                    handle: None,
                };
                filling.publish_gated(frame, gate);
                telemetry.decoded.fetch_add(1, Ordering::Relaxed);
            }
            Ok(None) => return false,
            Err(Fault::DeviceLost) => return true,
            Err(_) => {
                // The read-back failed: the picture is lost and the decoder
                // is left to its next unit; a fault there is the feed's to
                // judge.
                return false;
            }
        }
    }
}

/// No decoder can serve the stream: the queue is closed and the session
/// thread woken, which leaves the session -- a clean departure the host reads
/// as the guest leaving -- and then ends it as the decoder's failure. The
/// host would otherwise go on streaming to a client that shows nothing.
fn fail(telemetry: &Telemetry, shell: &WakeHandle, frames: &Frames) {
    telemetry.decoder.store(DECODER_FAILED, Ordering::Release);
    frames.close();
    let _ = shell.notify();
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

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicU32};

    use lowlat_core::video::{Codec, Rotation, VIDEO_HEADER_LEN, VideoHeader, encode};
    use lowlat_decode::Planes;

    use super::*;

    /// A backend that decodes nothing and counts what the loop does to it.
    struct Fake {
        destroyed: Arc<AtomicU32>,
    }

    impl Decoder for Fake {
        fn build(&mut self, _: &VideoHeader) -> Result<(), Fault> {
            Ok(())
        }
        fn feed(&mut self, _: &[u8]) -> Result<Fed, Fault> {
            Ok(Fed::NeedMoreData)
        }
        fn take(&mut self, _: &mut Planes<'_>) -> Result<Option<Picture>, Fault> {
            Ok(None)
        }
        fn destroy(&mut self) {
            self.destroyed.fetch_add(1, Ordering::Relaxed);
        }
    }

    impl Backend for Fake {
        fn output(&self) -> Option<(u32, u32, Format)> {
            None
        }
        fn timings(&self) -> (u32, u32) {
            (0, 0)
        }
    }

    /// A backend whose device is gone by its first take while its decodes
    /// still succeed, as a removed device was seen to behave on one driver.
    struct LostAtTake {
        destroyed: Arc<AtomicU32>,
    }

    impl Decoder for LostAtTake {
        fn build(&mut self, _: &VideoHeader) -> Result<(), Fault> {
            Ok(())
        }
        fn feed(&mut self, _: &[u8]) -> Result<Fed, Fault> {
            Ok(Fed::Picture)
        }
        fn take(&mut self, _: &mut Planes<'_>) -> Result<Option<Picture>, Fault> {
            Err(Fault::DeviceLost)
        }
        fn destroy(&mut self) {
            self.destroyed.fetch_add(1, Ordering::Relaxed);
        }
    }

    impl Backend for LostAtTake {
        fn output(&self) -> Option<(u32, u32, Format)> {
            Some((64, 64, Format::Nv12))
        }
        fn timings(&self) -> (u32, u32) {
            (0, 0)
        }
    }

    /// A unit that builds a decoder: an H.264 sequence parameter set.
    fn parameter_set() -> Vec<u8> {
        let header = VideoHeader {
            frame_id: 1,
            width: 64,
            height: 64,
            codec: Codec::H264,
            rotation: Rotation::None,
            ten_bit: false,
            locked: false,
            announced: false,
            metadata: false,
        };
        let mut out = vec![0u8; VIDEO_HEADER_LEN];
        encode(&mut out, &header).expect("a header");
        out.extend_from_slice(&[0, 0, 0, 1, 0x67, 0xAA]);
        out
    }

    struct Rig {
        units: Units,
        frames: Arc<Frames>,
        telemetry: Arc<Telemetry>,
        wake: lowlat_net::Wake,
        stopping: Arc<AtomicBool>,
        pending: Pending,
    }

    impl Rig {
        fn new() -> Self {
            Self {
                units: Units::new(),
                frames: Arc::new(Frames::new((64, 64), FrameKind::Planes)),
                telemetry: Arc::new(Telemetry::default()),
                wake: lowlat_net::Wake::new().expect("a wake"),
                stopping: Arc::new(AtomicBool::new(false)),
                pending: Arc::new(Mutex::new(None)),
            }
        }

        /// Run one decoder's loop on its own thread until it returns.
        fn drive<D: Backend + Send + 'static>(
            &self,
            fake: D,
            replacing: bool,
        ) -> std::thread::JoinHandle<Next> {
            let units = self.units.clone();
            let frames = Arc::clone(&self.frames);
            let telemetry = Arc::clone(&self.telemetry);
            let shell = self.wake.handle().expect("a handle");
            let stopping = Arc::clone(&self.stopping);
            let pending = Arc::clone(&self.pending);
            std::thread::spawn(move || {
                let shared = Shared {
                    units: &units,
                    frames: &frames,
                    telemetry: &telemetry,
                    shell: &shell,
                    stopping: &stopping,
                    pending: &pending,
                };
                super::drive(fake, &shared, replacing)
            })
        }

        /// What the seam and the session thread do for a switch: the choice
        /// left where the decode thread looks, then the word.
        fn switch_to(&self, choice: Opened) {
            *self.pending.lock().expect("the slot") = Some(choice);
            self.telemetry.switch.fetch_add(1, Ordering::Release);
            self.units.wake();
        }
    }

    /// **A switch is one act with one request, and the queue stays open.**
    /// The running loop returns the choice it was handed without asking for
    /// anything; the loop that replaces it asks once, as soon as it runs,
    /// so a keyframe never arrives for a decoder that is still opening; the
    /// queue is closed by neither.
    #[test]
    fn a_switch_returns_the_choice_and_the_replacement_asks_once() {
        let rig = Rig::new();
        let destroyed = Arc::new(AtomicU32::new(0));
        let first = rig.drive(
            Fake {
                destroyed: Arc::clone(&destroyed),
            },
            false,
        );
        std::thread::sleep(Duration::from_millis(20));
        assert!(
            !rig.telemetry.request.load(Ordering::Acquire),
            "the first loop asked"
        );

        rig.switch_to(Opened::Software(None));
        let next = first.join().expect("the first loop");
        assert_eq!(next, Next::Switch(Opened::Software(None)));
        assert!(
            !rig.telemetry.request.load(Ordering::Acquire),
            "the loop that was replaced asked for a keyframe"
        );
        assert!(!rig.frames.closed(), "the switch closed the queue");
        assert!(
            rig.pending.lock().expect("the slot").is_none(),
            "the choice was left behind"
        );

        let second = rig.drive(
            Fake {
                destroyed: Arc::clone(&destroyed),
            },
            true,
        );
        std::thread::sleep(Duration::from_millis(20));
        assert!(
            rig.telemetry.request.swap(false, Ordering::AcqRel),
            "the replacement did not ask"
        );
        std::thread::sleep(Duration::from_millis(60));
        assert!(
            !rig.telemetry.request.load(Ordering::Acquire),
            "the replacement asked twice"
        );

        rig.stopping.store(true, Ordering::Release);
        rig.units.wake();
        assert_eq!(second.join().expect("the second loop"), Next::Stop);
        assert!(
            !rig.frames.closed(),
            "the loop closes nothing; the thread does"
        );
    }

    /// A switch generation with no choice behind it moves nothing: the loop
    /// carries on with the decoder it has.
    #[test]
    fn a_word_without_a_choice_changes_nothing() {
        let rig = Rig::new();
        let destroyed = Arc::new(AtomicU32::new(0));
        let loop_ = rig.drive(
            Fake {
                destroyed: Arc::clone(&destroyed),
            },
            false,
        );
        rig.telemetry.switch.fetch_add(1, Ordering::Release);
        rig.units.wake();
        std::thread::sleep(Duration::from_millis(30));
        assert!(!loop_.is_finished(), "the loop returned on an empty word");
        rig.stopping.store(true, Ordering::Release);
        rig.units.wake();
        assert_eq!(loop_.join().expect("the loop"), Next::Stop);
    }

    /// With no decoder at all the thread idles, and a choice made then is
    /// taken the same way.
    #[test]
    fn an_idle_thread_takes_a_choice_too() {
        let rig = Rig::new();
        let units = rig.units.clone();
        let frames = Arc::clone(&rig.frames);
        let telemetry = Arc::clone(&rig.telemetry);
        let shell = rig.wake.handle().expect("a handle");
        let stopping = Arc::clone(&rig.stopping);
        let pending = Arc::clone(&rig.pending);
        let idle = std::thread::spawn(move || {
            let shared = Shared {
                units: &units,
                frames: &frames,
                telemetry: &telemetry,
                shell: &shell,
                stopping: &stopping,
                pending: &pending,
            };
            super::idle(&shared)
        });
        std::thread::sleep(Duration::from_millis(20));
        let choice = Opened::Software(Some(std::path::PathBuf::from("pair")));
        rig.switch_to(choice.clone());
        assert_eq!(idle.join().expect("the idle loop"), Next::Switch(choice));
    }

    /// **A device lost at the take ends the loop as lost**, though the decode
    /// before it succeeded, and the decoder is torn down with it. A removed
    /// device can go on taking decodes and fail only at the hand-over; left
    /// to the next unit's decode, the loss was never seen and nothing was
    /// handed out again.
    #[test]
    fn a_device_lost_at_the_take_ends_the_loop_as_lost() {
        let rig = Rig::new();
        let destroyed = Arc::new(AtomicU32::new(0));
        let loop_ = rig.drive(
            LostAtTake {
                destroyed: Arc::clone(&destroyed),
            },
            false,
        );
        rig.units.hand_over(&parameter_set());
        // Bounded so that a loop that never sees the loss fails the test
        // rather than hanging it.
        let began = std::time::Instant::now();
        while !loop_.is_finished() && began.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(5));
        }
        rig.stopping.store(true, Ordering::Release);
        rig.units.wake();
        assert_eq!(loop_.join().expect("the loop"), Next::Lost);
        assert_eq!(
            destroyed.load(Ordering::Relaxed),
            1,
            "the decoder was not torn down"
        );
    }
}
