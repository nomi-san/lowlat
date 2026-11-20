//! Reading the desktop's own output.
//!
//! One thread owns everything: the loop, the connection and the stream. The
//! interface below is not thread safe and nothing here pretends otherwise --
//! an owner asks for a change by setting a flag the loop reads between
//! iterations, and learns what happened from a published value.
//!
//! **The source is the clock.** Fragments arrive when the system has them, on
//! the period its own graph runs rather than the one asked for, so this
//! reassembles whole frames from whatever arrives. A reader pulling on its own
//! timer would drift against the sound device for as long as the session
//! lasted and would have to resample to hide it.
//!
//! The loop is the platform's, under the same names on each: the session's
//! sound server on Linux, a loopback of the output on Windows. What it is
//! asked for, what it hands on and how its owner stops it are written once,
//! here.

use std::sync::{Arc, Mutex};

use crate::{Error, FRAME_BYTES};

#[cfg(target_os = "linux")]
#[path = "capture/linux.rs"]
mod sys;
#[cfg(windows)]
#[path = "capture/windows.rs"]
mod sys;

pub use sys::outputs;

/// What to capture, and where from.
#[derive(Debug, Clone, Default)]
pub struct Config {
    /// The sound server's socket, when this process is outside the session that
    /// owns it. `None` takes whatever the environment names. Linux's alone: on
    /// Windows the system's sound service is the one there is.
    pub server: Option<String>,
    /// The half that can change while a capture runs: which device, and
    /// whether the speakers at the desk are silenced.
    ///
    /// **The tap is ahead of the mute on a device that applies its own**, so
    /// what a guest hears is unaffected and it is the person in front of the
    /// machine who stops hearing what they are sending. On a device whose mute
    /// the server applies instead, the same mix feeds the monitor and the
    /// request is refused rather than obeyed
    /// ([05 §9.4](../../../docs/05-host.md)).
    pub wanted: Arc<Wanted>,
}

/// What can be changed while a capture runs.
///
/// **Both cost a reconnect or a call to the server**, so they are asked for
/// rather than applied: the loop owns the connection and nothing else may touch
/// it. A change is noticed on the pass after it is made.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Live {
    /// A device to capture, or `None` for the default output's monitor.
    pub device: Option<String>,
    /// Silence the speakers at the desk while this capture runs.
    pub mute_local: bool,
}

/// An application whose own sound is captured in place of the output's mix
/// ([05 §9.7](../../../docs/05-host.md)): whether a process is one of it,
/// asked of the image the process runs -- its full path as the system names
/// it, in UTF-16.
///
/// **A question rather than a list**, so whoever says which windows are the
/// application's says which sounds are too, by the same rule.
#[cfg(windows)]
#[derive(Clone)]
pub struct Application(Arc<Holds>);

/// The question an application answers of an image.
#[cfg(windows)]
type Holds = dyn Fn(&[u16]) -> bool + Send + Sync;

#[cfg(windows)]
impl Application {
    pub fn new<F>(holds: F) -> Self
    where
        F: Fn(&[u16]) -> bool + Send + Sync + 'static,
    {
        Self(Arc::new(holds))
    }

    pub(crate) fn holds(&self, image: &[u16]) -> bool {
        (self.0)(image)
    }

    /// Whether two are the one set, not two that answer alike.
    pub(crate) fn same(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

#[cfg(windows)]
impl core::fmt::Debug for Application {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("Application")
    }
}

/// Where those settings live, shared between whoever sets them and the loop.
///
/// **Held by the owner rather than by the capture**, so a setting survives the
/// capture being closed and reopened -- which happens when the room empties and
/// fills again, and would otherwise silently forget what an application asked
/// for.
#[derive(Debug, Default)]
pub struct Wanted {
    live: Mutex<Live>,
    asked: std::sync::atomic::AtomicU32,
    /// **Apart from the live half**, which an application sets whole: its
    /// device and its mute never clear what the host holds its guests to.
    #[cfg(windows)]
    application: Mutex<Option<Application>>,
}

impl Wanted {
    /// Start from what a host is configured with.
    pub fn new(live: Live) -> Self {
        Self {
            live: Mutex::new(live),
            asked: std::sync::atomic::AtomicU32::new(0),
            #[cfg(windows)]
            application: Mutex::new(None),
        }
    }

    /// Capture one application's own sound in place of the output's mix, or
    /// the output again. Noticed on the loop's next pass, as the rest is.
    #[cfg(windows)]
    pub fn set_application(&self, application: Option<Application>) {
        match self.application.lock() {
            Ok(mut held) => *held = application,
            Err(held) => *held.into_inner() = application,
        }
        self.asked
            .fetch_add(1, std::sync::atomic::Ordering::Release);
    }

    /// The application whose sound is asked for, if one is.
    #[cfg(windows)]
    pub fn application(&self) -> Option<Application> {
        match self.application.lock() {
            Ok(held) => held.clone(),
            Err(held) => held.into_inner().clone(),
        }
    }

    /// Ask for something different.
    ///
    /// **Repeating what is already in force costs nothing**, so an application
    /// free to call this every second does not reconnect every second.
    pub fn set(&self, live: &Live) {
        let changed = match self.live.lock() {
            Ok(mut held) => {
                let moved = *held != *live;
                *held = live.clone();
                moved
            }
            Err(held) => {
                let mut held = held.into_inner();
                let moved = *held != *live;
                *held = live.clone();
                moved
            }
        };
        if changed {
            self.asked
                .fetch_add(1, std::sync::atomic::Ordering::Release);
        }
    }

    /// What is in force now.
    pub fn read(&self) -> Live {
        match self.live.lock() {
            Ok(held) => held.clone(),
            Err(held) => held.into_inner().clone(),
        }
    }

    fn generation(&self) -> u32 {
        self.asked.load(std::sync::atomic::Ordering::Acquire)
    }
}

/// A running capture. Dropping it stops the thread and joins it.
#[derive(Debug)]
pub struct Capture {
    stop: Arc<sys::Stop>,
    wanted: Arc<Wanted>,
    /// **The device the loop is on**, which is not always the one asked for:
    /// the server may move a stream, and the default output can change under a
    /// session.
    device: Arc<Mutex<String>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Capture {
    /// Open the sound device and start reading.
    ///
    /// **Returns when the first frame can be expected**, not before. A caller
    /// told that capture started and then hearing nothing cannot tell a silent
    /// desktop from a connection that never happened.
    ///
    /// `sink` is called on the capture thread with exactly [`FRAME_BYTES`] of
    /// interleaved sixteen-bit samples, which is both what the encoder reads
    /// and what an uncompressed packet carries.
    pub fn open<S>(config: Config, sink: S) -> Result<Self, Error>
    where
        S: FnMut(&[u8]) + Send + 'static,
    {
        sys::prepare()?;
        let stop = Arc::new(sys::Stop::new()?);
        let device = Arc::new(Mutex::new(String::new()));
        let wanted = Arc::clone(&config.wanted);
        let (report, opened) = std::sync::mpsc::channel();

        let thread = {
            let stop = Arc::clone(&stop);
            let device = Arc::clone(&device);
            let wanted = Arc::clone(&wanted);
            std::thread::Builder::new()
                .name("lowlat-audio".to_owned())
                .spawn(move || {
                    sys::run(
                        &Settings {
                            server: config.server,
                            wanted,
                        },
                        &stop,
                        &device,
                        &report,
                        sink,
                    );
                })
                .map_err(|_| Error::Unavailable)?
        };

        // The thread reports once, either way. A closed channel means it died
        // before it could, which is the same failure with less to say.
        match opened.recv() {
            Ok(Ok(())) => Ok(Self {
                stop,
                wanted,
                device,
                thread: Some(thread),
            }),
            Ok(Err(error)) => {
                let _ = thread.join();
                Err(error)
            }
            Err(_) => {
                let _ = thread.join();
                Err(Error::Refused(0))
            }
        }
    }

    /// Ask for a different device, or for the speakers to be silenced or let
    /// go.
    ///
    /// **Repeating what is already in force costs nothing.** The loop compares
    /// before it acts, so an application free to call this every second does
    /// not reconnect every second.
    pub fn set_live(&self, live: &Live) {
        self.wanted.set(live);
    }

    /// The device this capture is on now.
    pub fn device(&self) -> String {
        self.device
            .lock()
            .map_or_else(|held| held.into_inner().clone(), |held| held.clone())
    }

    /// Whether the loop is still reading.
    ///
    /// **A capture stops without its owner asking it to.** A sound server that
    /// goes away leaves a stream that will never deliver again, and the loop
    /// ends there; nothing else says so, and an owner that assumes otherwise
    /// holds a device that is handing it nothing.
    pub fn alive(&self) -> bool {
        self.thread
            .as_ref()
            .is_some_and(|thread| !thread.is_finished())
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop.request();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Whole frames out of whatever the source delivers, handed to the sink.
struct Frames<S> {
    sink: S,
    /// Bytes of a frame that have arrived, waiting for the rest.
    partial: Vec<u8>,
}

impl<S: FnMut(&[u8])> Frames<S> {
    fn new(sink: S) -> Self {
        Self {
            sink,
            partial: Vec::with_capacity(FRAME_BYTES),
        }
    }

    /// Add a fragment and hand on every whole frame it completes.
    ///
    /// **The fragment is not the frame.** A sound server delivers on its own
    /// period, so this reassembles and carries the remainder to the next one.
    fn take(&mut self, mut fragment: &[u8]) {
        while !fragment.is_empty() {
            // **A full buffer at the top of this loop would spin forever**,
            // taking nothing from the fragment and handing the same frame on
            // every turn. Only the clear below prevents it, so the invariant
            // is asserted where it is relied on rather than left implied: it
            // has already cost one run that allocated until the machine
            // noticed.
            debug_assert!(
                self.partial.len() < FRAME_BYTES,
                "a completed frame is handed on and cleared before the next fragment"
            );
            let want = FRAME_BYTES - self.partial.len();
            let (head, rest) = fragment.split_at(want.min(fragment.len()));
            self.partial.extend_from_slice(head);
            fragment = rest;
            if self.partial.len() == FRAME_BYTES {
                (self.sink)(&self.partial);
                self.partial.clear();
            }
        }
    }
}

/// What the loop is given: the fixed half, and where to read the rest.
struct Settings {
    server: Option<String>,
    wanted: Arc<Wanted>,
}

impl Settings {
    /// What is asked for now.
    fn live(&self) -> Live {
        self.wanted.read()
    }

    /// The device asked for, if one was.
    fn wanted_device(&self) -> Option<String> {
        self.live().device.filter(|name| !name.is_empty())
    }

    /// The application whose own sound is asked for, if one is.
    #[cfg(windows)]
    fn application(&self) -> Option<Application> {
        self.wanted.application()
    }
}

/// One output a host can capture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    /// What to put in a configuration: on Linux the **monitor** of this output,
    /// which is the device a host actually reads; on Windows the output itself,
    /// which the system reads back through a loopback of it.
    pub id: String,
    /// What a person calls it.
    pub name: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reassembly, which is the one piece of this that runs without a sound
    /// server.
    #[test]
    fn fragments_become_whole_frames() {
        let mut frames: Vec<usize> = Vec::new();
        {
            let mut state = Frames::new(|frame: &[u8]| frames.push(frame.len()));
            // The period a server delivers on is not the frame that is wanted.
            let fragment = vec![0u8; FRAME_BYTES + 128];
            state.take(&fragment);
            state.take(&fragment);
        }
        assert_eq!(frames, vec![FRAME_BYTES, FRAME_BYTES]);
    }

    /// A fragment smaller than a frame produces nothing until the rest arrives,
    /// rather than a short frame.
    #[test]
    fn a_partial_frame_is_held_rather_than_handed_on() {
        let mut frames = 0usize;
        let quarter = vec![0u8; FRAME_BYTES / 4];
        {
            let mut state = Frames::new(|_: &[u8]| frames += 1);
            for _ in 0..3 {
                state.take(&quarter);
            }
        }
        assert_eq!(frames, 0, "three quarters of a frame is not a frame");
        let mut frames = 0usize;
        {
            let mut state = Frames::new(|_: &[u8]| frames += 1);
            for _ in 0..4 {
                state.take(&quarter);
            }
        }
        assert_eq!(frames, 1, "four quarters are one");
    }

    /// Every sample that goes in comes out, in order, whatever the fragment
    /// sizes are. **A reassembly that loses the remainder passes both tests
    /// above**, because they only count frames.
    #[test]
    fn no_sample_is_lost_between_fragments() {
        let source: Vec<u8> = (0..FRAME_BYTES * 3)
            .map(|i| u8::try_from(i % 251).unwrap_or(0))
            .collect();
        let mut seen: Vec<u8> = Vec::new();
        {
            let mut state = Frames::new(|frame: &[u8]| seen.extend_from_slice(frame));
            // Sizes that share no factor with the frame, so every boundary
            // falls somewhere different.
            for fragment in source.chunks(997) {
                state.take(fragment);
            }
        }
        assert_eq!(seen.len(), FRAME_BYTES * 3);
        assert_eq!(seen, source[..seen.len()]);
    }
}
