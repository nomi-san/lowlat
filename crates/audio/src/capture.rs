//! Reading the desktop's own output.
//!
//! One thread owns everything: the loop, the connection and the stream. The
//! interface below is not thread safe and nothing here pretends otherwise --
//! an owner asks for a change by setting a flag the loop reads between
//! iterations, and learns what happened from a published value.
//!
//! **The source is the clock.** Fragments arrive when the sound server has
//! them, on the period its graph runs rather than the one asked for, so this
//! reassembles whole frames from whatever arrives. A reader pulling on its own
//! timer would drift against the sound device for as long as the session
//! lasted and would have to resample to hide it.
//!
//! **What a device name means here.** None follows the default output, which is
//! resolved once at connect and does **not** follow it afterwards on its own:
//! this loop is told when the server's own state changes and reconnects. A
//! named device is checked by reading back what the connection landed on,
//! because a name that does not resolve is **substituted rather than refused**,
//! so there is nothing to detect in what the call returns.

use std::ffi::{CStr, CString, c_int, c_void};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use crate::pulse::{self, Pulse};
use crate::{Error, FRAME_BYTES};

/// The device name that means "whichever output is the default one". The
/// server resolves it, which is why finding the monitor costs no enumeration.
const DEFAULT_MONITOR: &CStr = c"@DEFAULT_MONITOR@";

/// How long the loop may sleep before it looks at its flags again, and
/// therefore the worst case for noticing a stop.
const ITERATE_US: c_int = 100_000;

/// How long to wait for a connection or a stream before giving up.
const READY_MS: f64 = 5_000.0;

/// **Resolved once for the process and never unloaded.**
///
/// The interface is reached from callbacks it invokes and from the drop of
/// values it created, neither of which can be handed a reference. A process-
/// wide table costs one indirection on a path that runs fifty times a second,
/// and keeping the library mapped for the life of the process is the point
/// rather than a cost: unmapping it while one of its own callbacks might still
/// run is the failure this avoids.
static PULSE: OnceLock<Pulse> = OnceLock::new();

fn pulse() -> Option<&'static Pulse> {
    PULSE.get()
}

/// What to capture, and where from.
#[derive(Debug, Clone, Default)]
pub struct Config {
    /// The sound server's socket, when this process is outside the session that
    /// owns it. `None` takes whatever the environment names.
    pub server: Option<String>,
    /// A device to capture, or `None` for the default output's monitor.
    pub device: Option<String>,
}

impl Config {
    /// The device asked for, if one was.
    fn wanted(&self) -> Option<CString> {
        self.device
            .as_ref()
            .filter(|name| !name.is_empty())
            .and_then(|name| CString::new(name.as_str()).ok())
    }
}

/// A running capture. Dropping it stops the thread and joins it.
#[derive(Debug)]
pub struct Capture {
    stop: Arc<AtomicBool>,
    /// **The device the loop is on**, which is not always the one asked for:
    /// the server may move a stream, and the default output can change under a
    /// session.
    device: Arc<Mutex<String>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Capture {
    /// Open the sound server and start reading.
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
        if PULSE.get().is_none() {
            // A second thread racing this one loses and drops its copy, which
            // costs a load and nothing else.
            let _ = PULSE.set(Pulse::load()?);
        }
        let stop = Arc::new(AtomicBool::new(false));
        let device = Arc::new(Mutex::new(String::new()));
        let (report, opened) = std::sync::mpsc::channel();

        let thread = {
            let stop = Arc::clone(&stop);
            let device = Arc::clone(&device);
            std::thread::Builder::new()
                .name("lowlat-audio".to_owned())
                .spawn(move || run(&config, &stop, &device, &report, sink))
                .map_err(|_| Error::Unavailable)?
        };

        // The thread reports once, either way. A closed channel means it died
        // before it could, which is the same failure with less to say.
        match opened.recv() {
            Ok(Ok(())) => Ok(Self {
                stop,
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

    /// The device this capture is on now.
    pub fn device(&self) -> String {
        self.device
            .lock()
            .map_or_else(|held| held.into_inner().clone(), |held| held.clone())
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Everything the loop and its callbacks share.
///
/// **Reached only through the raw pointer given to the interface**, including
/// by the loop itself. Holding a reference to the box as well would invalidate
/// that pointer the moment it was used.
struct State<S> {
    sink: S,
    /// Bytes of a frame that have arrived, waiting for the rest.
    partial: Vec<u8>,
    /// The stream was moved to another device by somebody else.
    moved: bool,
    /// The server's own state changed, which is how a new default output
    /// arrives.
    server_changed: bool,
}

impl<S: FnMut(&[u8])> State<S> {
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

fn run<S>(
    config: &Config,
    stop: &AtomicBool,
    device: &Mutex<String>,
    report: &std::sync::mpsc::Sender<Result<(), Error>>,
    sink: S,
) where
    S: FnMut(&[u8]),
{
    let state = Box::into_raw(Box::new(State {
        sink,
        partial: Vec::with_capacity(FRAME_BYTES),
        moved: false,
        server_changed: false,
    }));
    let opaque = state.cast::<c_void>();
    let outcome = serve::<S>(config, stop, device, report, opaque);
    if let Err(error) = outcome {
        lowlat_common::log_warn!("audio: capture ended, {error}");
    }
    // SAFETY: the box was leaked above and nothing else took ownership; every
    // callback that could reach it has been cleared by the stream's drop.
    drop(unsafe { Box::from_raw(state) });
}

/// The loop, from connecting to the last frame.
fn serve<S>(
    config: &Config,
    stop: &AtomicBool,
    device: &Mutex<String>,
    report: &std::sync::mpsc::Sender<Result<(), Error>>,
    opaque: *mut c_void,
) -> Result<(), Error>
where
    S: FnMut(&[u8]),
{
    let session = match Session::open::<S>(config, opaque).and_then(|session| {
        session.wait_ready(stop)?;
        Ok(session)
    }) {
        Ok(session) => session,
        Err(error) => {
            let _ = report.send(Err(error));
            return Err(error);
        }
    };
    let mut stream = match Stream::open::<S>(&session, config, opaque, stop) {
        Ok(stream) => stream,
        Err(error) => {
            let _ = report.send(Err(error));
            return Err(error);
        }
    };
    publish(&stream, device);
    let _ = report.send(Ok(()));

    while !stop.load(Ordering::Acquire) {
        session.iterate();
        if !stream.ready() {
            return Err(Error::Read(session.errno()));
        }

        // **Read the flags after dispatching, never during it.** A callback
        // holds the state while it runs, so nothing else may.
        // SAFETY: the pointer is the leaked state, alive until `run` returns,
        // and this thread is the only one that touches it.
        let state = unsafe { &mut *opaque.cast::<State<S>>() };
        let moved = core::mem::take(&mut state.moved);
        let changed = core::mem::take(&mut state.server_changed);

        if moved {
            publish(&stream, device);
            lowlat_common::log_info!("audio: the stream was moved to {}", stream.name());
        }
        // A new default output only concerns a stream that is following one.
        if changed && config.wanted().is_none() {
            let before = stream.name();
            let fresh = Stream::open::<S>(&session, config, opaque, stop)?;
            stream = fresh;
            let after = stream.name();
            if before != after {
                lowlat_common::log_info!("audio: the default output is now {after}");
            }
            publish(&stream, device);
        }
    }
    Ok(())
}

/// Record which device the stream is on, for whoever asks.
fn publish(stream: &Stream, device: &Mutex<String>) {
    let name = stream.name();
    match device.lock() {
        Ok(mut held) => *held = name,
        Err(held) => *held.into_inner() = name,
    }
}

/// The loop and the connection to the server.
struct Session {
    mainloop: *mut pulse::MainLoop,
    context: *mut pulse::Context,
}

impl Session {
    fn open<S: FnMut(&[u8])>(config: &Config, opaque: *mut c_void) -> Result<Self, Error> {
        let pulse = pulse().ok_or(Error::Unavailable)?;
        let server = config
            .server
            .as_ref()
            .filter(|named| !named.is_empty())
            .and_then(|named| CString::new(named.as_str()).ok());
        // SAFETY: the loop and the context are created here and freed in
        // `Drop`; the server string outlives the connect call, which copies it.
        unsafe {
            let mainloop = (pulse.mainloop_new)();
            if mainloop.is_null() {
                return Err(Error::Unavailable);
            }
            let api = (pulse.mainloop_get_api)(mainloop);
            let context = (pulse.context_new)(api, c"lowlat".as_ptr());
            if context.is_null() {
                (pulse.mainloop_free)(mainloop);
                return Err(Error::Unavailable);
            }
            (pulse.context_set_subscribe_callback)(context, Some(on_subscribe::<S>), opaque);
            let named = server.as_ref().map_or(std::ptr::null(), |s| s.as_ptr());
            if (pulse.context_connect)(context, named, pulse::CONTEXT_NOAUTOSPAWN, std::ptr::null())
                < 0
            {
                let code = (pulse.context_errno)(context);
                (pulse.context_unref)(context);
                (pulse.mainloop_free)(mainloop);
                return Err(Error::Refused(code));
            }
            Ok(Self { mainloop, context })
        }
    }

    /// Run the loop until the connection is up, fails, or the wait is over.
    fn wait_ready(&self, stop: &AtomicBool) -> Result<(), Error> {
        let pulse = pulse().ok_or(Error::Unavailable)?;
        let began = lowlat_common::clock::Time::now();
        loop {
            // SAFETY: the context is live until this value is dropped.
            let state = unsafe { (pulse.context_get_state)(self.context) };
            if state == pulse::CONTEXT_READY {
                self.subscribe();
                return Ok(());
            }
            if state == pulse::CONTEXT_FAILED || state == pulse::CONTEXT_TERMINATED {
                // SAFETY: as above.
                return Err(Error::Refused(unsafe {
                    (pulse.context_errno)(self.context)
                }));
            }
            if stop.load(Ordering::Acquire) || lowlat_common::clock::elapsed_ms(began) > READY_MS {
                return Err(Error::Refused(0));
            }
            self.iterate();
        }
    }

    /// Ask to be told when the server's own state changes, which is where a new
    /// default output comes from.
    fn subscribe(&self) {
        let Some(pulse) = pulse() else {
            return;
        };
        // SAFETY: the context is ready. The operation is discarded rather than
        // waited on: nothing here depends on when the subscription takes
        // effect, only that it does.
        unsafe {
            let op = (pulse.context_subscribe)(
                self.context,
                pulse::SUBSCRIPTION_MASK_SERVER,
                std::ptr::null(),
                std::ptr::null_mut(),
            );
            if !op.is_null() {
                (pulse.operation_unref)(op);
            }
        }
    }

    /// The server's own code for the last failure.
    fn errno(&self) -> i32 {
        let Some(pulse) = pulse() else {
            return 0;
        };
        // SAFETY: the context is live until this value is dropped.
        unsafe { (pulse.context_errno)(self.context) }
    }

    /// One turn of the loop, bounded so a stop is noticed even when the server
    /// has gone quiet.
    fn iterate(&self) {
        let Some(pulse) = pulse() else {
            return;
        };
        // SAFETY: the three calls are one iteration decomposed, and they are
        // only ever made from the thread that created the loop.
        unsafe {
            if (pulse.mainloop_prepare)(self.mainloop, ITERATE_US) < 0 {
                return;
            }
            if (pulse.mainloop_poll)(self.mainloop) < 0 {
                return;
            }
            (pulse.mainloop_dispatch)(self.mainloop);
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let Some(pulse) = pulse() else {
            return;
        };
        // SAFETY: both handles were created in `open` and are dropped once.
        unsafe {
            (pulse.context_disconnect)(self.context);
            (pulse.context_unref)(self.context);
            (pulse.mainloop_free)(self.mainloop);
        }
    }
}

/// The recording stream itself.
struct Stream {
    stream: *mut pulse::Stream,
}

impl Stream {
    fn open<S: FnMut(&[u8])>(
        session: &Session,
        config: &Config,
        opaque: *mut c_void,
        stop: &AtomicBool,
    ) -> Result<Self, Error> {
        let pulse = pulse().ok_or(Error::Unavailable)?;
        let spec = pulse::SampleSpec {
            format: pulse::SAMPLE_S16LE,
            rate: crate::SAMPLE_RATE,
            channels: u8::try_from(crate::CHANNELS).unwrap_or(2),
        };
        let attr = pulse::BufferAttr::fragments_of(u32::try_from(FRAME_BYTES).unwrap_or(u32::MAX));
        let asked = config.wanted();
        let device = asked.as_deref().unwrap_or(DEFAULT_MONITOR);

        // SAFETY: the specification and the attributes outlive the calls, which
        // copy them; a null channel map asks for the default layout of the
        // channel count.
        let stream = unsafe {
            let stream = (pulse.stream_new)(
                session.context,
                c"desktop".as_ptr(),
                &spec,
                std::ptr::null(),
            );
            if stream.is_null() {
                return Err(Error::Refused((pulse.context_errno)(session.context)));
            }
            let stream = Self { stream };
            (pulse.stream_set_read_callback)(stream.stream, Some(on_read::<S>), opaque);
            (pulse.stream_set_moved_callback)(stream.stream, Some(on_moved::<S>), opaque);
            if (pulse.stream_connect_record)(
                stream.stream,
                device.as_ptr(),
                &attr,
                pulse::STREAM_ADJUST_LATENCY,
            ) < 0
            {
                return Err(Error::Refused((pulse.context_errno)(session.context)));
            }
            stream
        };

        let began = lowlat_common::clock::Time::now();
        loop {
            // SAFETY: the stream is live until it is dropped.
            let state = unsafe { (pulse.stream_get_state)(stream.stream) };
            if state == pulse::STREAM_READY {
                break;
            }
            if state == pulse::STREAM_FAILED || state == pulse::STREAM_TERMINATED {
                // SAFETY: as above.
                return Err(Error::Refused(unsafe {
                    (pulse.context_errno)(session.context)
                }));
            }
            if stop.load(Ordering::Acquire) || lowlat_common::clock::elapsed_ms(began) > READY_MS {
                return Err(Error::Refused(0));
            }
            session.iterate();
        }

        // **A name that does not resolve is substituted, not refused**, so what
        // is checked is where the stream landed rather than what the call
        // returned.
        if let Some(asked) = asked.as_deref() {
            let got = stream.name();
            if got.as_bytes() != asked.to_bytes() {
                lowlat_common::log_warn!(
                    "audio: asked for {} and the server offered {got}",
                    asked.to_string_lossy()
                );
                return Err(Error::NoSuchDevice);
            }
        }
        Ok(stream)
    }

    /// Whether the stream is still delivering.
    ///
    /// **Checked every pass rather than trusted.** A server that goes away
    /// leaves a stream that reads nothing for ever, which is indistinguishable
    /// from a desktop in silence and is the one failure a caller most needs
    /// told about.
    fn ready(&self) -> bool {
        let Some(pulse) = pulse() else {
            return false;
        };
        // SAFETY: the stream is live until it is dropped.
        unsafe { (pulse.stream_get_state)(self.stream) == pulse::STREAM_READY }
    }

    /// The device this stream is on, as the server names it.
    fn name(&self) -> String {
        let Some(pulse) = pulse() else {
            return String::new();
        };
        // SAFETY: the stream is live, and the string belongs to the interface
        // and is copied here rather than kept.
        unsafe {
            let name = (pulse.stream_get_device_name)(self.stream);
            if name.is_null() {
                return String::new();
            }
            CStr::from_ptr(name).to_string_lossy().into_owned()
        }
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        let Some(pulse) = pulse() else {
            return;
        };
        // SAFETY: created in `open`, dropped once. **The callbacks are cleared
        // first**, so nothing can be delivered into a state that is going away.
        unsafe {
            (pulse.stream_set_read_callback)(self.stream, None, std::ptr::null_mut());
            (pulse.stream_set_moved_callback)(self.stream, None, std::ptr::null_mut());
            (pulse.stream_disconnect)(self.stream);
            (pulse.stream_unref)(self.stream);
        }
    }
}

/// Fragments arrived. Take everything available and hand on whole frames.
unsafe extern "C" fn on_read<S: FnMut(&[u8])>(
    stream: *mut pulse::Stream,
    _bytes: usize,
    opaque: *mut c_void,
) {
    let Some(pulse) = pulse() else {
        return;
    };
    // SAFETY: the pointer is the leaked state, which outlives the stream, and
    // nothing else holds a reference to it while a callback runs.
    let state = unsafe { &mut *opaque.cast::<State<S>>() };
    loop {
        let mut data: *const c_void = std::ptr::null();
        let mut len: usize = 0;
        // SAFETY: both out-parameters are valid for the call.
        if unsafe { (pulse.stream_peek)(stream, &mut data, &mut len) } < 0 || len == 0 {
            return;
        }
        if data.is_null() {
            // A null pointer with a length is a gap the server could not fill.
            // Those samples are simply gone; dropping it is all there is to do.
            lowlat_common::log_warn!("audio: the source dropped {len} bytes");
        } else {
            // SAFETY: the interface guarantees `len` readable bytes at `data`
            // until the matching drop below.
            let fragment = unsafe { std::slice::from_raw_parts(data.cast::<u8>(), len) };
            state.take(fragment);
        }
        // SAFETY: exactly one drop per successful peek, which is the rule.
        unsafe {
            (pulse.stream_drop)(stream);
        }
    }
}

/// Somebody moved this stream to another device.
unsafe extern "C" fn on_moved<S: FnMut(&[u8])>(_stream: *mut pulse::Stream, opaque: *mut c_void) {
    // SAFETY: as in `on_read`.
    let state = unsafe { &mut *opaque.cast::<State<S>>() };
    state.moved = true;
}

/// The server's own state changed, which includes its default output.
unsafe extern "C" fn on_subscribe<S: FnMut(&[u8])>(
    _context: *mut pulse::Context,
    _event: u32,
    _index: u32,
    opaque: *mut c_void,
) {
    // SAFETY: as in `on_read`.
    let state = unsafe { &mut *opaque.cast::<State<S>>() };
    state.server_changed = true;
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
            let mut state = State {
                sink: |frame: &[u8]| frames.push(frame.len()),
                partial: Vec::new(),
                moved: false,
                server_changed: false,
            };
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
            let mut state = State {
                sink: |_: &[u8]| frames += 1,
                partial: Vec::new(),
                moved: false,
                server_changed: false,
            };
            for _ in 0..3 {
                state.take(&quarter);
            }
        }
        assert_eq!(frames, 0, "three quarters of a frame is not a frame");
        let mut frames = 0usize;
        {
            let mut state = State {
                sink: |_: &[u8]| frames += 1,
                partial: Vec::new(),
                moved: false,
                server_changed: false,
            };
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
            let mut state = State {
                sink: |frame: &[u8]| seen.extend_from_slice(frame),
                partial: Vec::new(),
                moved: false,
                server_changed: false,
            };
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
