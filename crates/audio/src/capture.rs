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

/// How long the restore of somebody's speakers may take.
///
/// **Shorter, because it runs while everything is stopping.** A sound server
/// that has itself gone away would otherwise hold a teardown for the full wait
/// above, twice, to undo something that no longer exists.
const RESTORE_MS: f64 = 1_000.0;

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
}

impl Wanted {
    /// Start from what a host is configured with.
    pub fn new(live: Live) -> Self {
        Self {
            live: Mutex::new(live),
            asked: std::sync::atomic::AtomicU32::new(0),
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
    stop: Arc<AtomicBool>,
    wanted: Arc<Wanted>,
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
        let wanted = Arc::clone(&config.wanted);
        let (report, opened) = std::sync::mpsc::channel();

        let thread = {
            let stop = Arc::clone(&stop);
            let device = Arc::clone(&device);
            let wanted = Arc::clone(&wanted);
            std::thread::Builder::new()
                .name("lowlat-audio".to_owned())
                .spawn(move || {
                    run(
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
    fn wanted_device(&self) -> Option<CString> {
        self.live()
            .device
            .filter(|name| !name.is_empty())
            .and_then(|name| CString::new(name).ok())
    }
}

fn run<S>(
    config: &Settings,
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
    config: &Settings,
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
    let mut muted = if config.live().mute_local {
        mute_local(&session, &stream.name(), stop)
    } else {
        LocalMute::default()
    };

    // **However this loop leaves, the speakers come back.** A return on an
    // error path that skipped the restore would silence somebody's machine
    // until they noticed and fixed it by hand.
    let outcome = serve_loop::<S>(
        &session,
        &mut stream,
        config,
        stop,
        device,
        opaque,
        &mut muted,
    );
    restore_local(&session, &muted);
    outcome
}

/// The loop proper, so that whatever it returns the speakers are restored.
fn serve_loop<S>(
    session: &Session,
    stream: &mut Stream,
    config: &Settings,
    stop: &AtomicBool,
    device: &Mutex<String>,
    opaque: *mut c_void,
    muted: &mut LocalMute,
) -> Result<(), Error>
where
    S: FnMut(&[u8]),
{
    let mut seen = config.wanted.generation();
    while !stop.load(Ordering::Acquire) {
        session.iterate();
        if !stream.ready() {
            return Err(Error::Read(session.errno()));
        }

        // **What an owner asked for, acted on between iterations.** The
        // connection belongs to this thread, so a change is a request rather
        // than something another thread may apply.
        let now = config.wanted.generation();
        if now != seen {
            seen = now;
            let live = config.live();
            let before = stream.name();
            // The device is compared by what the stream landed on, not by what
            // was asked for: following the default output means those differ.
            let moving = live
                .device
                .as_deref()
                .filter(|name| !name.is_empty())
                .is_some_and(|name| name != before);
            if moving {
                restore_local(session, muted);
                // **A device that will not open keeps the one that is
                // working.** The name may have gone away between the
                // enumeration and the request, and ending the capture over it
                // would take the sound from every guest to honour a change
                // that failed.
                match Stream::open::<S>(session, config, opaque, stop) {
                    Ok(fresh) => {
                        *stream = fresh;
                        publish(stream, device);
                        lowlat_common::log_info!("audio: now capturing {}", stream.name());
                    }
                    Err(error) => lowlat_common::log_warn!(
                        "audio: the device asked for would not open, keeping {}, error={error}",
                        stream.name()
                    ),
                }
                *muted = LocalMute::default();
            }
            match (live.mute_local, muted.applied) {
                (true, false) => *muted = mute_local(session, &stream.name(), stop),
                (false, true) => {
                    restore_local(session, muted);
                    *muted = LocalMute::default();
                }
                _ => {}
            }
        }

        // **Read the flags after dispatching, never during it.** A callback
        // holds the state while it runs, so nothing else may.
        // SAFETY: the pointer is the leaked state, alive until `run` returns,
        // and this thread is the only one that touches it.
        let state = unsafe { &mut *opaque.cast::<State<S>>() };
        let moved = core::mem::take(&mut state.moved);
        let changed = core::mem::take(&mut state.server_changed);

        if moved {
            publish(stream, device);
            lowlat_common::log_info!("audio: the stream was moved to {}", stream.name());
        }
        // A new default output only concerns a stream that is following one.
        if changed && config.wanted_device().is_none() {
            let before = stream.name();
            // **The speakers move with the device.** A host that followed the
            // default output and left the old sink muted would silence a
            // machine nobody is streaming from.
            restore_local(session, muted);
            // **The new default may not be ready yet**, and a stream that is
            // still delivering is worth more than the one that was asked for:
            // a failure here leaves the old device in place, and a device that
            // has genuinely gone stops delivering on its own, which is the
            // path that ends this loop and has the capture taken again.
            match Stream::open::<S>(session, config, opaque, stop) {
                Ok(fresh) => *stream = fresh,
                Err(error) => lowlat_common::log_warn!(
                    "audio: the new default output would not open, keeping {}, error={error}",
                    stream.name()
                ),
            }
            let after = stream.name();
            if before != after {
                lowlat_common::log_info!("audio: the default output is now {after}");
            }
            publish(stream, device);
            *muted = if config.live().mute_local {
                mute_local(session, &after, stop)
            } else {
                LocalMute::default()
            };
        }
    }
    Ok(())
}

/// Silence the speakers behind the device being captured.
///
/// **Only what this host did is remembered**, so only that is ever undone. A
/// device that is not a monitor has no speakers to silence and is left alone,
/// which is the right answer for a host told to capture a microphone.
fn mute_local(session: &Session, monitor: &str, stop: &AtomicBool) -> LocalMute {
    let stop = Some(stop);
    let mut state = LocalMute::default();
    let Ok(monitor) = CString::new(monitor) else {
        return state;
    };
    let Some(source) = session.describe(false, &monitor, stop, READY_MS) else {
        lowlat_common::log_warn!("audio: the source did not describe itself, speakers left alone");
        return state;
    };
    let Some(sink) = source.paired else {
        // Not a monitor: there is nothing at the desk playing what we hear.
        return state;
    };
    let Some(described) = session.describe(true, &sink, stop, READY_MS) else {
        return state;
    };
    if !described.mutes_itself {
        // **A device whose mute reaches the capture is left alone.** Where the
        // device mutes itself the tap is ahead of it, which is what this
        // feature rests on; where the server mutes the mix instead, the same
        // mix feeds the monitor, and silencing the speakers would silence
        // every guest. That is the opposite of what was asked for, so it is
        // refused rather than done.
        lowlat_common::log_warn!(
            "audio: the speakers cannot be silenced without silencing the guests, sink={}, left alone",
            sink.to_string_lossy()
        );
        state.sink = Some(sink);
        return state;
    }
    if described.mute {
        // **Already muted, by somebody who is not us.** Leave it, and leave the
        // record empty so that nothing is undone later.
        state.sink = Some(sink);
        return state;
    }
    if session.set_sink_mute(&sink, true, stop, READY_MS).is_some() {
        lowlat_common::log_info!(
            "audio: the speakers are silenced, sink={}",
            sink.to_string_lossy()
        );
        state.applied = true;
    }
    state.sink = Some(sink);
    state
}

/// Put the speakers back, if this host is what silenced them.
fn restore_local(session: &Session, state: &LocalMute) {
    if !state.applied {
        return;
    }
    let Some(sink) = state.sink.as_deref() else {
        return;
    };
    // **Still muted, and by us.** Somebody who unmuted during the session is
    // not re-muted, and one who muted their own speakers keeps them muted.
    if session
        .describe(true, sink, None, RESTORE_MS)
        .is_some_and(|described| described.mute)
        && session
            .set_sink_mute(sink, false, None, RESTORE_MS)
            .is_some()
    {
        lowlat_common::log_info!(
            "audio: the speakers are back, sink={}",
            sink.to_string_lossy()
        );
    }
}

/// Record which device the stream is on, for whoever asks.
fn publish(stream: &Stream, device: &Mutex<String>) {
    let name = stream.name();
    match device.lock() {
        Ok(mut held) => *held = name,
        Err(held) => *held.into_inner() = name,
    }
}

/// Ask the server one question and wait for the answer.
///
/// The interface answers asynchronously and this loop is the one that drives
/// it, so the wait is turns of that loop rather than a block: an operation
/// nobody iterates never completes.
struct Ask<T> {
    answer: Option<T>,
    done: bool,
    /// The name this asked about, so the callback can check that the structure
    /// it is reading is laid out where it thinks.
    asked: Option<CString>,
}

impl<T> Ask<T> {
    fn about(name: Option<&CStr>) -> Self {
        Self {
            answer: None,
            done: false,
            asked: name.map(CStr::to_owned),
        }
    }
}

/// The device description a query answered with, reduced to what is read.
struct Described {
    mute: bool,
    /// Whether this device applies its own mute, which is what decides
    /// whether muting it reaches a capture of its monitor.
    mutes_itself: bool,
    paired: Option<CString>,
    /// Whether the name at offset zero was the one asked for, which is the
    /// check that the transcribed layout is being read correctly.
    matched: bool,
}

/// The callback both device queries use.
///
/// **It checks the layout it is reading**, because a structure transcribed from
/// a header is right until the day it is not: the name it was asked for has to
/// come back at offset zero, and anything else means the other fields are being
/// read from the wrong place and must not be acted on.
unsafe extern "C" fn on_device(
    _context: *mut pulse::Context,
    info: *const pulse::DeviceInfo,
    end: c_int,
    opaque: *mut c_void,
) {
    // SAFETY: the pointer is the `Ask` the query was issued with, which lives
    // on the stack of the call that is driving this loop.
    let ask = unsafe { &mut *opaque.cast::<Ask<Described>>() };
    if end != 0 || info.is_null() {
        ask.done = true;
        return;
    }
    // SAFETY: the interface guarantees the pointer for the call's duration.
    let info = unsafe { &*info };
    let paired = if info.paired_name.is_null() {
        None
    } else {
        // SAFETY: a non-null name from the interface is NUL terminated.
        Some(unsafe { CStr::from_ptr(info.paired_name) }.to_owned())
    };
    let name = if info.name.is_null() {
        None
    } else {
        // SAFETY: a non-null name from the interface is NUL terminated.
        Some(unsafe { CStr::from_ptr(info.name) }.to_owned())
    };
    ask.answer = Some(Described {
        mute: info.mute != 0,
        mutes_itself: info.flags & pulse::DEVICE_MUTES_ITSELF != 0,
        paired,
        matched: name == ask.asked,
    });
}

/// The callback a setting reports through. Nothing is read from it; the answer
/// is that it finished.
unsafe extern "C" fn on_done(_context: *mut pulse::Context, _success: c_int, opaque: *mut c_void) {
    // SAFETY: as in `on_device`.
    let ask = unsafe { &mut *opaque.cast::<Ask<()>>() };
    ask.done = true;
}

/// Speakers this host silenced, and what to put back.
///
/// **Restore, never unmute.** Undoing more than was done switches on the
/// speakers of somebody who muted them for their own reasons, in their absence,
/// with the sound as the first they know of it.
#[derive(Debug, Default)]
struct LocalMute {
    /// The sink behind the monitor being captured, when there is one.
    sink: Option<CString>,
    /// Whether this host is the one that muted it.
    applied: bool,
}

/// The loop and the connection to the server.
struct Session {
    mainloop: *mut pulse::MainLoop,
    context: *mut pulse::Context,
}

impl Session {
    fn open<S: FnMut(&[u8])>(config: &Settings, opaque: *mut c_void) -> Result<Self, Error> {
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

    /// Ask about a device by name, and run the loop until it answers.
    ///
    /// **The name it answers with is checked against the name asked for**,
    /// which is what makes reading a transcribed structure safe: a mismatch
    /// says the layout is wrong and the answer is discarded rather than acted
    /// on.
    fn describe(
        &self,
        sink: bool,
        name: &CStr,
        stop: Option<&AtomicBool>,
        within: f64,
    ) -> Option<Described> {
        let pulse = pulse()?;
        let mut ask = Ask::<Described>::about(Some(name));
        // **Reached only through the pointer, including from here.** A
        // reference taken to it afterwards would invalidate the one the
        // interface is writing through.
        let ask: *mut Ask<Described> = &raw mut ask;
        let query = if sink {
            pulse.sink_info_by_name
        } else {
            pulse.source_info_by_name
        };
        // SAFETY: the context is ready, the name outlives the call, and the
        // `Ask` outlives every callback because this function waits for it.
        unsafe {
            let op = query(
                self.context,
                name.as_ptr(),
                Some(on_device),
                ask.cast::<c_void>(),
            );
            if op.is_null() {
                return None;
            }
            (pulse.operation_unref)(op);
        }
        self.settle(ask, stop, within)?;
        // SAFETY: the query has finished, so nothing else holds it; it lives
        // on this stack frame until the function returns.
        let answer = unsafe { (*ask).answer.take() };
        let wanted = answer.filter(|described| described.matched);
        if wanted.is_none() {
            lowlat_common::log_warn!(
                "audio: {} did not describe itself as asked",
                name.to_string_lossy()
            );
        }
        wanted
    }

    /// Mute or unmute a sink by name, and wait for it to take effect.
    fn set_sink_mute(
        &self,
        sink: &CStr,
        mute: bool,
        stop: Option<&AtomicBool>,
        within: f64,
    ) -> Option<()> {
        let pulse = pulse()?;
        let mut ask = Ask::<()>::about(None);
        let ask: *mut Ask<()> = &raw mut ask;
        // SAFETY: as in `describe`.
        unsafe {
            let op = (pulse.set_sink_mute_by_name)(
                self.context,
                sink.as_ptr(),
                c_int::from(mute),
                Some(on_done),
                ask.cast::<c_void>(),
            );
            if op.is_null() {
                return None;
            }
            (pulse.operation_unref)(op);
        }
        self.settle(ask, stop, within)
    }

    /// Turn the loop until the answer arrives, or give up.
    ///
    /// **The flag is written by a callback the interface runs**, which is why
    /// it is read through the pointer rather than a reference and why nothing
    /// here can see it change.
    fn settle<T>(&self, ask: *mut Ask<T>, stop: Option<&AtomicBool>, within: f64) -> Option<()> {
        let began = lowlat_common::clock::Time::now();
        loop {
            // SAFETY: the caller owns the value this points at for the whole
            // of this call, and only this thread touches it -- the callbacks
            // run inside `iterate`, below, and never in parallel with it.
            if unsafe { (*ask).done } {
                return Some(());
            }
            // **A teardown is not a reason to abandon this.** Restoring what
            // this host changed runs while the loop is already stopping, so it
            // passes no flag and is bounded only by time; a wait that gave up
            // on the stop flag would leave somebody's speakers muted, which is
            // exactly what it did before this line said so.
            let cancelled = stop.is_some_and(|stop| stop.load(Ordering::Acquire));
            if cancelled || lowlat_common::clock::elapsed_ms(began) > within {
                return None;
            }
            self.iterate();
        }
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
        config: &Settings,
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
        let asked = config.wanted_device();
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

/// One output a host can capture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    /// What to put in a configuration: the **monitor** of this output, which is
    /// the device a host actually reads.
    pub id: String,
    /// What a person calls it.
    pub name: String,
}

/// Every output on this machine that a host could capture.
///
/// **The identity is the monitor and the name is the speaker's**, because those
/// are two different things and an application needs both: one goes back into
/// the configuration, and the other is what a person chooses from.
///
/// Opens its own short-lived connection, so it answers before a host is started
/// and without disturbing one that is running.
pub fn outputs(server: Option<&str>) -> Result<Vec<Output>, Error> {
    if PULSE.get().is_none() {
        let _ = PULSE.set(Pulse::load()?);
    }
    let pulse = pulse().ok_or(Error::Unavailable)?;
    let stop = AtomicBool::new(false);
    let config = Settings {
        server: server.map(str::to_owned),
        wanted: Arc::new(Wanted::default()),
    };
    let session = Session::open::<fn(&[u8])>(&config, std::ptr::null_mut())?;
    session.wait_ready(&stop)?;

    let mut ask = Listing::default();
    let ask: *mut Listing = &raw mut ask;
    // SAFETY: the listing outlives the query, because this waits for it; the
    // context is ready and belongs to this thread.
    unsafe {
        let op = (pulse.sink_info_list)(session.context, Some(on_listed), ask.cast::<c_void>());
        if op.is_null() {
            return Err(Error::Refused(session.errno()));
        }
        (pulse.operation_unref)(op);
    }
    let began = lowlat_common::clock::Time::now();
    loop {
        // SAFETY: as above.
        if unsafe { (*ask).done } {
            break;
        }
        if lowlat_common::clock::elapsed_ms(began) > READY_MS {
            return Err(Error::Refused(session.errno()));
        }
        session.iterate();
    }
    // SAFETY: the query has finished and nothing else holds the listing.
    Ok(unsafe { core::mem::take(&mut (*ask).found) })
}

/// What a listing has collected so far.
#[derive(Debug, Default)]
struct Listing {
    found: Vec<Output>,
    done: bool,
}

/// One entry of a listing, or its end.
unsafe extern "C" fn on_listed(
    _context: *mut pulse::Context,
    info: *const pulse::DeviceInfo,
    end: c_int,
    opaque: *mut c_void,
) {
    // SAFETY: the pointer is the listing the query was issued with.
    let listing = unsafe { &mut *opaque.cast::<Listing>() };
    if end != 0 || info.is_null() {
        listing.done = true;
        return;
    }
    // SAFETY: the interface guarantees the pointer for the call's duration.
    let info = unsafe { &*info };
    // **An output with no monitor cannot be captured**, so it is not offered.
    if info.paired_name.is_null() {
        return;
    }
    // SAFETY: non-null strings from the interface are NUL terminated.
    let id = unsafe { CStr::from_ptr(info.paired_name) }
        .to_string_lossy()
        .into_owned();
    let name = if info.description.is_null() {
        id.clone()
    } else {
        // SAFETY: as above.
        unsafe { CStr::from_ptr(info.description) }
            .to_string_lossy()
            .into_owned()
    };
    listing.found.push(Output { id, name });
}
