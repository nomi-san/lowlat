//! An application's own sound on Windows ([05 §9.7](../../../../../docs/05-host.md)):
//! a capture of each of its processes that sounds, mixed on the captures' own
//! clock.
//!
//! **Which processes sound is the sessions' to say.** Every output keeps a
//! session for a process from its first stream to its exit, and the system
//! tells of one as its stream is made and again as it starts (measured), so a
//! capture opened on the notice runs before the first sample does. A process
//! is the application's when it runs one of the application's executables, or
//! one of its live ancestors does: a client built on a browser plays from a
//! helper of its window's process.
//!
//! **A capture takes its process and the processes that process started, and
//! no deeper** (measured), so each sound is taken once: a sounding process is
//! captured unless its parent is, whose capture holds it.
//!
//! **The mix keeps the captures' own clock.** Each delivers a packet of 10 ms
//! on the performance counter at exactly the rate, at a phase of its own,
//! holding that one packet and no more (measured). So every packet is placed
//! by its stamp onto one timeline, each capture's packets ending on the
//! timeline's 10 ms marks, and a frame is handed on as soon as every capture
//! has filled it, or a few milliseconds past its end at the latest. A step in
//! a capture's stamps is a pause in what it delivered, left as silence of its
//! length. The timeline never stops, so a guest hears silence while nothing of
//! the application sounds.

use core::ffi::c_void;
use core::ptr::{self, NonNull};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering, fence};
use std::sync::{Arc, Mutex, OnceLock};

use lowlat_common::dynlib::Library;
use windows_sys::Win32::Foundation::{
    CloseHandle, FILETIME, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
};
use windows_sys::Win32::System::Com::StructuredStorage::PROPVARIANT as SystemPropVariant;
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcessId, GetProcessTimes, OpenProcess, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, QueryFullProcessImageNameW,
    WaitForMultipleObjects, WaitForSingleObject,
};
use windows_sys::Win32::System::Variant::VT_BLOB;

use super::{
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK,
    CONVERT, Com, E_NOINTERFACE, E_NOTIMPL, Ended, Event, FLOAT_FRAME, Frames, IAudioCaptureClient,
    IAudioClient, IAudioEndpointVolume, IID, IID_IAudioCaptureClient, IID_IAudioClient,
    IID_IAudioEndpointVolume, IID_IUnknown, IMMDevice, IMMDeviceCollection, IMMDeviceEnumerator,
    LocalMute, PCM_FRAME, Registration, S_OK, SILENT, Settings, Signal, Stop, ULONG, activate,
    default_output, eRender, enumerator, format, friendly_name, mute_speakers, restore_speakers,
    same, service, to_sixteen,
};
use crate::capture::Application;
use crate::ffi::wasapi::{
    AUDIOCLIENT_ACTIVATION_PARAMS, AUDIOCLIENT_ACTIVATION_PARAMS__bindgen_ty_1,
    AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK, AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS,
    AudioSessionState, AudioSessionStateExpired, DEVICE_STATE_ACTIVE, GUID, HRESULT,
    IActivateAudioInterfaceAsyncOperation, IActivateAudioInterfaceCompletionHandler,
    IActivateAudioInterfaceCompletionHandlerVtbl, IAudioSessionControl, IAudioSessionControl2,
    IAudioSessionEnumerator, IAudioSessionManager2, IAudioSessionNotification,
    IAudioSessionNotificationVtbl, IUnknown, PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
};
use crate::ffi::wasapi_guids::{
    IID_IActivateAudioInterfaceCompletionHandler, IID_IAgileObject, IID_IAudioSessionControl2,
    IID_IAudioSessionManager2, IID_IAudioSessionNotification,
};
use crate::{Error, FRAME, FRAME_BYTES};

/// Captures held at once, those still opening included. Each is a handle in
/// the loop's one wait, beside the stop and the notices, of the sixty-four a
/// wait takes.
const CAPTURES_MAX: usize = 30;

/// Processes whose answers are kept at once: a few hold sessions at a time.
const KNOWN_MAX: usize = 128;

/// Ancestors walked for an executable of the application.
const ANCESTORS_MAX: usize = 32;

/// The longest path the system names an image by, in UTF-16 units.
const PATH_MAX: usize = 32_768;

/// How often the sessions are read again with no notice to say so: the net
/// under the notices, never how a sound is found.
const SWEEP_MS: f64 = 2000.0;

/// The longest the loop waits for anything: a safety cap, not a cadence.
const WAIT_CAP_MS: u32 = 500;

/// A capture's packet, in frames: 10 ms, whatever is asked for (measured).
const PACKET: u64 = 480;

/// A frame of the timeline: what one encoded packet carries.
const FRAME_FRAMES: u64 = FRAME as u64;

/// The timeline held at once, in frames: 85 ms. A packet lands at most a
/// frame and a packet past what was handed on, with room for a reader held up.
const RING: u64 = 4096;

/// A packet lands no earlier than this past its own stamped end: its delivery
/// trails the stamp by 0.2-2 ms (measured).
const LEAD_US: i64 = 1000;

/// A frame waits this long past its end for a capture that has not filled
/// it, and is then handed on without the rest.
const WAIT_US: i64 = 4000;

/// A stamp this close to where the last packet said the next one would be is
/// the next one.
const TOLERANCE_US: i64 = 100;

/// Frames past their time handed on at once after the loop was held up;
/// beyond, the timeline starts again from now, so a guest is not sent a
/// backlog of silence it would have to play through.
const CATCH_UP_MAX: i64 = 10;

/// Microseconds `frames` last at the sample rate. A packet's 480 are exactly
/// 10000; the timeline's marks are packets, so its times are exact.
fn span_us(frames: u64) -> i64 {
    i64::try_from(frames.saturating_mul(125) / 6).unwrap_or(i64::MAX)
}

/// The frames `us` microseconds hold, rounded; none for a time before.
fn frames_in(us: i64) -> u64 {
    u64::try_from(us).map_or(0, |us| (us.saturating_mul(6) + 62) / 125)
}

/// Now, on the performance counter, as the captures' stamps are.
fn now_us() -> i64 {
    i64::try_from(lowlat_common::clock::monotonic_us()).unwrap_or(i64::MAX)
}

/// One capture's place on the timeline.
#[derive(Debug, Default)]
struct Lane {
    /// Where its next packet goes, once it has placed one.
    next: Option<u64>,
    /// Where its stamps say its next packet begins.
    expect: i64,
}

/// The captures' timeline: what each has added, until it is handed on.
#[derive(Debug)]
struct Mix {
    /// Interleaved stereo, `RING` frames, position modulo its length.
    ring: Vec<f32>,
    /// The first position not yet handed on, at a frame's start.
    emitted: u64,
    /// When position zero was, in microseconds.
    anchor: i64,
}

impl Mix {
    fn new(now: i64) -> Self {
        Self {
            ring: vec![0.0; usize::try_from(RING).unwrap_or(0) * crate::CHANNELS],
            emitted: 0,
            anchor: now,
        }
    }

    fn time_of(&self, position: u64) -> i64 {
        self.anchor.saturating_add(span_us(position))
    }

    /// The first position whose time is `time` or later.
    fn position_at(&self, time: i64) -> u64 {
        let since = u64::try_from(time.saturating_sub(self.anchor)).unwrap_or(0);
        since.saturating_mul(6).div_ceil(125)
    }

    /// When the frame being filled is handed on, whatever filled it.
    fn deadline(&self) -> i64 {
        self.time_of(self.emitted + FRAME_FRAMES)
            .saturating_add(WAIT_US)
    }

    /// Where a lane's packet of `frames`, its first stamped `stamp`, goes.
    ///
    /// **After the lane's last packet** while the stamps run on from it.
    /// Otherwise -- its first packet, or the first after a step -- **at the
    /// earliest 10 ms mark it is sure to reach in time**, never over what the
    /// lane or the timeline already has: what lies between is silence, the
    /// step's length to within a mark.
    ///
    /// **A lane alone moves the timeline to itself instead**, so the one
    /// capture of an application adds nothing to its delay: its first packet
    /// sets the timeline, a step is still silence of its length, and only the
    /// rounding to the nearest mark moves the timeline -- by under 5 ms,
    /// either way, so the samples handed on keep to the clock over a session.
    /// One that has fallen behind what was handed on -- a reader held up -- is
    /// placed as one beside another is, the timeline left where it is: moved
    /// back to such a lane, it would hand on again time it already had.
    fn place(&mut self, lane: &mut Lane, stamp: i64, frames: u64, alone: bool) -> u64 {
        let length = span_us(frames);
        if let Some(next) = lane.next
            && (stamp - lane.expect).abs() <= TOLERANCE_US
        {
            lane.next = Some(next + frames);
            lane.expect = stamp.saturating_add(length);
            return next;
        }
        let floor = lane.next.unwrap_or(0).max(self.emitted);
        let least = (floor + frames).div_ceil(PACKET) * PACKET;
        let due = stamp.saturating_add(length).saturating_add(LEAD_US);
        let natural = match lane.next {
            Some(next) => next + frames_in(stamp.saturating_sub(lane.expect)),
            None => floor,
        };
        let end = if alone && natural >= self.emitted {
            let end = ((natural + frames + PACKET / 2) / PACKET * PACKET).max(least);
            self.anchor = due.saturating_sub(span_us(end));
            end
        } else {
            self.position_at(due).max(least).div_ceil(PACKET) * PACKET
        };
        lane.next = Some(end);
        lane.expect = stamp.saturating_add(length);
        end - frames
    }

    /// Whether a packet ending at `end` fits on the timeline as it stands.
    fn fits(&self, end: u64) -> bool {
        end <= self.emitted + RING
    }

    /// Add a packet's frames -- interleaved stereo floats, little endian --
    /// at `at`, and say how many came after their frame was handed on.
    fn add(&mut self, at: u64, bytes: &[u8]) -> u64 {
        let mut late = 0u64;
        for (index, frame) in (at..).zip(bytes.chunks_exact(FLOAT_FRAME)) {
            if index < self.emitted {
                late += 1;
                continue;
            }
            if index >= self.emitted + RING {
                break;
            }
            let slot = usize::try_from(index % RING).unwrap_or(0) * crate::CHANNELS;
            for (value, sample) in self
                .ring
                .get_mut(slot..slot + crate::CHANNELS)
                .into_iter()
                .flatten()
                .zip(frame.chunks_exact(FLOAT_FRAME / crate::CHANNELS))
            {
                *value += f32::from_le_bytes(sample.try_into().unwrap_or([0; 4]));
            }
        }
        late
    }

    /// Whether every lane that has placed anything has filled the frame
    /// being filled; none has not.
    fn filled<'a>(&self, lanes: impl Iterator<Item = &'a Lane>) -> bool {
        let end = self.emitted + FRAME_FRAMES;
        let mut any = false;
        for next in lanes.filter_map(|lane| lane.next) {
            if next < end {
                return false;
            }
            any = true;
        }
        any
    }

    /// Hand the frame being filled on as sixteen bits, and clear it.
    fn emit<S: FnMut(&[u8])>(&mut self, frames: &mut Frames<S>) {
        let mut out = [0u8; FRAME_BYTES];
        for (index, pair) in (self.emitted..).zip(out.chunks_exact_mut(PCM_FRAME)) {
            let slot = usize::try_from(index % RING).unwrap_or(0) * crate::CHANNELS;
            if let Some(values) = self.ring.get_mut(slot..slot + crate::CHANNELS) {
                for (value, bytes) in values.iter_mut().zip(pair.chunks_exact_mut(2)) {
                    bytes.copy_from_slice(&to_sixteen(*value).to_le_bytes());
                    *value = 0.0;
                }
            }
        }
        self.emitted += FRAME_FRAMES;
        frames.take(&out);
    }

    /// Start the timeline again from now: what it held dropped, every lane
    /// placed afresh.
    fn restart<'a>(&mut self, now: i64, lanes: impl Iterator<Item = &'a mut Lane>) {
        self.ring.fill(0.0);
        self.anchor = now.saturating_sub(span_us(self.emitted));
        for lane in lanes {
            lane.next = None;
        }
    }
}

/// The captures to hold, from the application's processes that sound -- each
/// its creation time, number and parent: **one unless its parent is one**,
/// whose capture holds it.
///
/// **Oldest first**, so a parent is decided before its children, and a
/// number that passed to a later process -- the parent having exited -- is
/// decided after the child it is not the parent of, and holds nothing.
fn targets(sounding: &mut [(u64, u32, Option<u32>)]) -> Vec<(u32, u64)> {
    sounding.sort_unstable();
    let mut wanted: Vec<(u32, u64)> = Vec::with_capacity(sounding.len());
    for &(created, pid, parent) in sounding.iter() {
        let covered = parent.is_some_and(|parent| wanted.iter().any(|&(held, _)| held == parent));
        if !covered {
            wanted.push((pid, created));
        }
    }
    wanted
}

/// A process this side holds open: **its number cannot pass to another
/// process while the handle lives**, so what is kept for it cannot outlive it.
#[derive(Debug)]
struct Process(HANDLE);

impl Process {
    fn open(pid: u32, rights: u32) -> Option<Self> {
        // SAFETY: rights the system defines, not inherited; checked.
        let handle = unsafe { OpenProcess(rights, 0, pid) };
        (!handle.is_null()).then_some(Self(handle))
    }

    /// The full path of the image it runs.
    fn image(&self) -> Option<Vec<u16>> {
        let mut path = vec![0u16; PATH_MAX];
        let mut len = u32::try_from(path.len()).ok()?;
        // SAFETY: a buffer of `len` units and its length; the call writes no
        // more and sets `len` to what it wrote.
        if unsafe {
            QueryFullProcessImageNameW(self.0, PROCESS_NAME_WIN32, path.as_mut_ptr(), &mut len)
        } == 0
        {
            return None;
        }
        path.truncate(usize::try_from(len).ok()?);
        Some(path)
    }

    /// When it was made, in the system's 100 ns units.
    fn created(&self) -> Option<u64> {
        let zero = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
        // SAFETY: a process handle this owns and four times the call writes.
        let ok = unsafe {
            GetProcessTimes(
                self.0,
                &raw mut created,
                &raw mut exited,
                &raw mut kernel,
                &raw mut user,
            )
        };
        (ok != 0)
            .then(|| (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
    }

    fn exited(&self) -> bool {
        // SAFETY: a process handle this owns, not waited on.
        unsafe { WaitForSingleObject(self.0, 0) == WAIT_OBJECT_0 }
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        // SAFETY: the handle `open` gave, closed once.
        unsafe { CloseHandle(self.0) };
    }
}

/// Each process's parent as the system says it now, by number alone: a
/// number may have passed to a later process since, which creation times
/// tell.
fn parents() -> Vec<(u32, u32)> {
    let mut found = Vec::new();
    // SAFETY: a snapshot of every process; checked, and closed below.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE || snapshot.is_null() {
        return found;
    }
    let mut entry = PROCESSENTRY32W {
        dwSize: u32::try_from(core::mem::size_of::<PROCESSENTRY32W>()).unwrap_or(0),
        ..PROCESSENTRY32W::default()
    };
    // SAFETY: a live snapshot and an entry whose size is set, as the calls ask.
    let mut more = unsafe { Process32FirstW(snapshot, &raw mut entry) } != 0;
    while more {
        found.push((entry.th32ProcessID, entry.th32ParentProcessID));
        // SAFETY: as above.
        more = unsafe { Process32NextW(snapshot, &raw mut entry) } != 0;
    }
    // SAFETY: the snapshot made above, closed once.
    unsafe { CloseHandle(snapshot) };
    found
}

/// What is kept for a process that holds a session.
#[derive(Debug)]
struct Known {
    pid: u32,
    process: Process,
    created: u64,
    holds: bool,
    /// Its parent, if that still runs and was made before it.
    parent: Option<u32>,
}

/// Which processes are the application's, each asked once while it lives.
#[derive(Debug)]
struct Judge {
    application: Application,
    known: Vec<Known>,
}

impl Judge {
    fn new(application: Application) -> Self {
        Self {
            application,
            known: Vec::new(),
        }
    }

    fn forget_exited(&mut self) {
        self.known.retain(|known| !known.process.exited());
    }

    /// The answer for a process, kept under its handle. **None for one that
    /// cannot be opened or named**, asked again next time rather than kept.
    fn judge(&mut self, pid: u32, parents: &mut Option<Vec<(u32, u32)>>) -> Option<&Known> {
        if let Some(index) = self.known.iter().position(|known| known.pid == pid) {
            return self.known.get(index);
        }
        let process = Process::open(pid, PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE)?;
        let created = process.created()?;
        let image = process.image()?;
        let listed = self.application.holds(&image);
        let parents = parents.get_or_insert_with(self::parents);
        let (parent, ancestor) = walk(&self.application, pid, created, parents, listed);
        if self.known.len() >= KNOWN_MAX {
            self.known.remove(0);
        }
        self.known.push(Known {
            pid,
            process,
            created,
            holds: listed || ancestor,
            parent,
        });
        self.known.last()
    }
}

/// A process's parent, and whether a live ancestor of it runs one of the
/// application's executables -- each link checked by creation time, the walk
/// ending at the first process that cannot be opened. `parent_only` stops at
/// the parent.
fn walk(
    application: &Application,
    pid: u32,
    created: u64,
    parents: &[(u32, u32)],
    parent_only: bool,
) -> (Option<u32>, bool) {
    let mut child = (pid, created);
    let mut parent = None;
    for depth in 0..ANCESTORS_MAX {
        let Some(&(_, up)) = parents.iter().find(|&&(of, _)| of == child.0) else {
            break;
        };
        if up == 0 || up == child.0 {
            break;
        }
        let Some(process) = Process::open(up, PROCESS_QUERY_LIMITED_INFORMATION) else {
            break;
        };
        let Some(made) = process.created() else {
            break;
        };
        if made > child.1 {
            break;
        }
        if depth == 0 {
            parent = Some(up);
            if parent_only {
                break;
            }
        }
        if process
            .image()
            .is_some_and(|image| application.holds(&image))
        {
            return (parent, true);
        }
        child = (up, made);
    }
    (parent, false)
}

/// What the session manager's notice of a new session tells the loop.
///
/// **Its layout is the interface's**: a pointer to its table first.
#[repr(C)]
struct Notice {
    table: *const IAudioSessionNotificationVtbl,
    refs: AtomicU32,
    signal: Arc<Signal>,
}

static NOTICE: IAudioSessionNotificationVtbl = IAudioSessionNotificationVtbl {
    QueryInterface: Some(notice_query),
    AddRef: Some(notice_add_ref),
    Release: Some(notice_release),
    OnSessionCreated: Some(notice_created),
};

unsafe extern "C" fn notice_query(
    this: *mut IAudioSessionNotification,
    iid: *const IID,
    out: *mut *mut c_void,
) -> HRESULT {
    if out.is_null() || iid.is_null() {
        return E_NOINTERFACE;
    }
    // SAFETY: the system passes a live identifier and a writable slot.
    unsafe {
        let asked = &*iid;
        if same(asked, &IID_IUnknown) || same(asked, &IID_IAudioSessionNotification) {
            notice_add_ref(this);
            *out = this.cast();
            S_OK
        } else {
            *out = ptr::null_mut();
            E_NOINTERFACE
        }
    }
}

unsafe extern "C" fn notice_add_ref(this: *mut IAudioSessionNotification) -> ULONG {
    // SAFETY: `this` is a live notice; a reference is being taken on it.
    let notice = unsafe { &*this.cast::<Notice>() };
    // A new reference needs no ordering: whoever passes it on synchronises.
    notice.refs.fetch_add(1, Ordering::Relaxed).wrapping_add(1)
}

unsafe extern "C" fn notice_release(this: *mut IAudioSessionNotification) -> ULONG {
    // SAFETY: `this` is a live notice whose reference is being given back.
    let notice = unsafe { &*this.cast::<Notice>() };
    // Release, then an acquire fence before the free, as the notifier does.
    let left = notice.refs.fetch_sub(1, Ordering::Release).wrapping_sub(1);
    if left == 0 {
        fence(Ordering::Acquire);
        // SAFETY: the last reference: nothing else reaches the box any more.
        drop(unsafe { Box::from_raw(this.cast::<Notice>()) });
    }
    left
}

/// A session made or started: noted and the loop woken, nothing more. **It
/// runs on the system's own thread**, which must not be held up.
unsafe extern "C" fn notice_created(
    this: *mut IAudioSessionNotification,
    _session: *mut IAudioSessionControl,
) -> HRESULT {
    // SAFETY: `this` is a live notice for the call's duration.
    let notice = unsafe { &*this.cast::<Notice>() };
    notice
        .signal
        .sessions_changed
        .store(true, Ordering::Release);
    notice.signal.event.set();
    S_OK
}

/// One output's sessions, and the notice registered with them while it lives.
struct Watched {
    manager: Com<IAudioSessionManager2>,
    notice: *mut IAudioSessionNotification,
}

impl Drop for Watched {
    fn drop(&mut self) {
        // SAFETY: registered when this was made; unregistered once, outside
        // any notice, then this side's reference given back.
        unsafe {
            com!(
                self.manager.as_ptr(),
                UnregisterSessionNotification,
                self.notice
            );
            notice_release(self.notice);
        }
    }
}

/// The sessions of every active output.
struct Sessions {
    watched: Vec<Watched>,
}

impl Sessions {
    /// Every active output's sessions, each with the notice registered.
    /// **An output that will not give them is left out**, and the sweep below
    /// reads the others.
    fn open(enumerator: &Com<IMMDeviceEnumerator>, signal: &Arc<Signal>) -> Self {
        let mut watched = Vec::new();
        let mut collection: *mut IMMDeviceCollection = ptr::null_mut();
        // SAFETY: a live enumeration and a local out-parameter.
        let hr = unsafe {
            com!(
                enumerator.as_ptr(),
                EnumAudioEndpoints,
                eRender,
                DEVICE_STATE_ACTIVE,
                &raw mut collection
            )
        };
        // SAFETY: as in `enumerator`.
        let Some(collection) = (hr >= 0)
            .then(|| unsafe { Com::from_raw(collection) })
            .flatten()
        else {
            return Self { watched };
        };
        let mut count: u32 = 0;
        // SAFETY: a live collection and a local out-parameter.
        unsafe { com!(collection.as_ptr(), GetCount, &raw mut count) };
        for index in 0..count {
            let mut output: *mut IMMDevice = ptr::null_mut();
            // SAFETY: an index the collection counted and a local out-parameter.
            if unsafe { com!(collection.as_ptr(), Item, index, &raw mut output) } < 0 {
                continue;
            }
            // SAFETY: as in `enumerator`.
            let Some(output) = (unsafe { Com::from_raw(output) }) else {
                continue;
            };
            let Ok(manager) =
                activate::<IAudioSessionManager2>(&output, &IID_IAudioSessionManager2)
            else {
                continue;
            };
            let notice = Box::into_raw(Box::new(Notice {
                table: &raw const NOTICE,
                refs: AtomicU32::new(1),
                signal: Arc::clone(signal),
            }))
            .cast::<IAudioSessionNotification>();
            // SAFETY: a live manager and a live notice, whose reference the
            // registration below holds until its drop.
            let hr = unsafe { com!(manager.as_ptr(), RegisterSessionNotification, notice) };
            if hr < 0 {
                // SAFETY: this side's own reference, given back once.
                unsafe { notice_release(notice) };
                lowlat_common::log_warn!(
                    "audio: an output's new sessions will not be told, error={}",
                    Error::Refused(hr)
                );
                continue;
            }
            watched.push(Watched { manager, notice });
        }
        Self { watched }
    }

    /// The processes holding a session on any of them, but this one, once
    /// each.
    fn sweep(&self, own: u32, into: &mut Vec<u32>) {
        into.clear();
        for watched in &self.watched {
            let mut sessions: *mut IAudioSessionEnumerator = ptr::null_mut();
            // SAFETY: a live manager and a local out-parameter.
            let hr = unsafe {
                com!(
                    watched.manager.as_ptr(),
                    GetSessionEnumerator,
                    &raw mut sessions
                )
            };
            // SAFETY: as in `enumerator`.
            let Some(sessions) = (hr >= 0)
                .then(|| unsafe { Com::from_raw(sessions) })
                .flatten()
            else {
                continue;
            };
            let mut count: i32 = 0;
            // SAFETY: a live enumeration and a local out-parameter.
            unsafe { com!(sessions.as_ptr(), GetCount, &raw mut count) };
            for index in 0..count {
                if let Some(pid) = session_process(&sessions, index)
                    && pid != 0
                    && pid != own
                    && !into.contains(&pid)
                {
                    into.push(pid);
                }
            }
        }
    }
}

/// The process of one session that has not expired, if it is one process's.
fn session_process(sessions: &Com<IAudioSessionEnumerator>, index: i32) -> Option<u32> {
    let mut session: *mut IAudioSessionControl = ptr::null_mut();
    // SAFETY: an index the enumeration counted and a local out-parameter.
    if unsafe { com!(sessions.as_ptr(), GetSession, index, &raw mut session) } < 0 {
        return None;
    }
    // SAFETY: as in `enumerator`.
    let session = unsafe { Com::from_raw(session) }?;
    let mut second: *mut c_void = ptr::null_mut();
    // SAFETY: a live session, the identifier of its second interface and a
    // local out-parameter.
    let hr = unsafe {
        com!(
            session.as_ptr(),
            QueryInterface,
            ptr::from_ref(&IID_IAudioSessionControl2),
            &raw mut second
        )
    };
    if hr < 0 {
        return None;
    }
    // SAFETY: the call succeeded and handed over a reference.
    let second = unsafe { Com::from_raw(second.cast::<IAudioSessionControl2>()) }?;
    let mut state: AudioSessionState = AudioSessionStateExpired;
    let mut pid: u32 = 0;
    // SAFETY: a live session and local out-parameters. **Only a session of
    // one process names it**: one shared by several answers otherwise.
    unsafe {
        if com!(second.as_ptr(), GetState, &raw mut state) < 0
            || com!(second.as_ptr(), GetProcessId, &raw mut pid) != S_OK
        {
            return None;
        }
    }
    (state != AudioSessionStateExpired).then_some(pid)
}

/// The system's call that opens a capture of one process's sound, from its
/// own directory alone: the library is not one every process maps.
type ActivateAsync = unsafe extern "system" fn(
    *const u16,
    *const GUID,
    *const SystemPropVariant,
    *mut IActivateAudioInterfaceCompletionHandler,
    *mut *mut IActivateAudioInterfaceAsyncOperation,
) -> HRESULT;

static ACTIVATE: OnceLock<Option<(Library, ActivateAsync)>> = OnceLock::new();

fn activate_async() -> Option<ActivateAsync> {
    ACTIVATE
        .get_or_init(|| {
            let library = Library::open_system(c"mmdevapi.dll")?;
            // SAFETY: the signature is the header's for this name.
            let call = unsafe { library.symbol::<ActivateAsync>(c"ActivateAudioInterfaceAsync") }?;
            Some((library, call))
        })
        .as_ref()
        .map(|&(_, call)| call)
}

/// The completion of a capture's opening, which the system calls on a thread
/// of its own. **Agile**, as such a completion must be.
#[repr(C)]
struct Completion {
    table: *const IActivateAudioInterfaceCompletionHandlerVtbl,
    refs: AtomicU32,
    done: AtomicBool,
    event: Event,
}

static COMPLETION: IActivateAudioInterfaceCompletionHandlerVtbl =
    IActivateAudioInterfaceCompletionHandlerVtbl {
        QueryInterface: Some(completion_query),
        AddRef: Some(completion_add_ref),
        Release: Some(completion_release),
        ActivateCompleted: Some(completion_done),
    };

unsafe extern "C" fn completion_query(
    this: *mut IActivateAudioInterfaceCompletionHandler,
    iid: *const IID,
    out: *mut *mut c_void,
) -> HRESULT {
    if out.is_null() || iid.is_null() {
        return E_NOINTERFACE;
    }
    // SAFETY: the system passes a live identifier and a writable slot.
    unsafe {
        let asked = &*iid;
        if same(asked, &IID_IUnknown)
            || same(asked, &IID_IActivateAudioInterfaceCompletionHandler)
            || same(asked, &IID_IAgileObject)
        {
            completion_add_ref(this);
            *out = this.cast();
            S_OK
        } else {
            *out = ptr::null_mut();
            E_NOINTERFACE
        }
    }
}

unsafe extern "C" fn completion_add_ref(
    this: *mut IActivateAudioInterfaceCompletionHandler,
) -> ULONG {
    // SAFETY: `this` is a live completion; a reference is being taken on it.
    let completion = unsafe { &*this.cast::<Completion>() };
    completion
        .refs
        .fetch_add(1, Ordering::Relaxed)
        .wrapping_add(1)
}

unsafe extern "C" fn completion_release(
    this: *mut IActivateAudioInterfaceCompletionHandler,
) -> ULONG {
    // SAFETY: `this` is a live completion whose reference is being given back.
    let completion = unsafe { &*this.cast::<Completion>() };
    let left = completion
        .refs
        .fetch_sub(1, Ordering::Release)
        .wrapping_sub(1);
    if left == 0 {
        fence(Ordering::Acquire);
        // SAFETY: the last reference: nothing else reaches the box any more.
        drop(unsafe { Box::from_raw(this.cast::<Completion>()) });
    }
    left
}

/// The opening is done: marked and the loop woken. The result is read on the
/// loop's thread.
unsafe extern "C" fn completion_done(
    this: *mut IActivateAudioInterfaceCompletionHandler,
    _operation: *mut IActivateAudioInterfaceAsyncOperation,
) -> HRESULT {
    // SAFETY: `this` is a live completion for the call's duration.
    let completion = unsafe { &*this.cast::<Completion>() };
    // Release: the loop reads the operation's result after it sees this.
    completion.done.store(true, Ordering::Release);
    completion.event.set();
    S_OK
}

/// This side's reference on a completion, given back on drop.
struct CompletionRef(NonNull<Completion>);

impl CompletionRef {
    fn new() -> Result<Self, Error> {
        let completion = Box::new(Completion {
            table: &raw const COMPLETION,
            refs: AtomicU32::new(1),
            done: AtomicBool::new(false),
            event: Event::new(false)?,
        });
        // SAFETY: a box is never null.
        Ok(Self(unsafe {
            NonNull::new_unchecked(Box::into_raw(completion))
        }))
    }

    fn get(&self) -> &Completion {
        // SAFETY: this side's reference keeps it alive.
        unsafe { self.0.as_ref() }
    }

    fn as_handler(&self) -> *mut IActivateAudioInterfaceCompletionHandler {
        self.0.as_ptr().cast()
    }
}

impl Drop for CompletionRef {
    fn drop(&mut self) {
        // SAFETY: this side's own reference, given back once; the system's,
        // if it still holds one, keeps the completion alive past this.
        unsafe { completion_release(self.as_handler()) };
    }
}

/// A capture of one process, from its opening to its end.
enum Capturing {
    /// Asked for; the system has not answered yet.
    Opening {
        completion: CompletionRef,
        operation: Com<IActivateAudioInterfaceAsyncOperation>,
        _request: Box<Request>,
    },
    /// Delivering.
    Running {
        client: Com<IAudioClient>,
        capture: Com<IAudioCaptureClient>,
        wake: Event,
    },
    /// Refused: kept so the process is not asked about again while it runs.
    Refused,
}

impl Drop for Capturing {
    fn drop(&mut self) {
        if let Capturing::Running { client, .. } = self {
            // SAFETY: a live client; one already lost answers with an error,
            // which changes nothing here.
            unsafe { com!(client.as_ptr(), Stop) };
        }
    }
}

/// A process of the application whose sound is captured.
struct Target {
    pid: u32,
    created: u64,
    process: Process,
    state: Capturing,
    lane: Lane,
}

impl Target {
    /// Ask for a capture of a process and the processes it started.
    fn open(pid: u32, created: u64) -> Option<Self> {
        let process = Process::open(pid, PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE)?;
        // The number may have passed to another process since it was judged.
        if process.created()? != created {
            return None;
        }
        let state = match begin(pid) {
            Ok(state) => state,
            Err(error) => {
                lowlat_common::log_warn!(
                    "audio: a process of the application cannot be captured, pid={pid}, error={error}"
                );
                Capturing::Refused
            }
        };
        Some(Self {
            pid,
            created,
            process,
            state,
            lane: Lane::default(),
        })
    }

    /// The handle the loop waits on for this capture, if it has one.
    fn handle(&self) -> Option<HANDLE> {
        match &self.state {
            Capturing::Opening { completion, .. } => Some(completion.get().event.0),
            Capturing::Running { wake, .. } => Some(wake.0),
            Capturing::Refused => None,
        }
    }

    /// Take the capture the system opened, once it has.
    fn finish(&mut self) {
        let Capturing::Opening {
            completion,
            operation,
            ..
        } = &self.state
        else {
            return;
        };
        // Acquire: pairs with the completion's release.
        if !completion.get().done.load(Ordering::Acquire) {
            return;
        }
        self.state = match running(operation) {
            Ok(state) => {
                lowlat_common::log_info!(
                    "audio: capturing a process of the application, pid={}",
                    self.pid
                );
                state
            }
            Err(error) => {
                lowlat_common::log_warn!(
                    "audio: a process of the application cannot be captured, pid={}, error={error}",
                    self.pid
                );
                Capturing::Refused
            }
        };
    }
}

/// What a capture's opening is asked with: a value holding the parameters.
///
/// **Kept until the opening completes**, boxed so neither moves: the call
/// that takes them answers later, and nothing says it has copied them.
struct Request {
    params: AUDIOCLIENT_ACTIVATION_PARAMS,
    value: SystemPropVariant,
}

/// Ask the system to open a capture of a process and its children.
fn begin(pid: u32) -> Result<Capturing, Error> {
    let call = activate_async().ok_or(Error::Unavailable)?;
    let completion = CompletionRef::new()?;
    let mut request = Box::new(Request {
        params: AUDIOCLIENT_ACTIVATION_PARAMS {
            ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
            __bindgen_anon_1: AUDIOCLIENT_ACTIVATION_PARAMS__bindgen_ty_1 {
                ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                    TargetProcessId: pid,
                    ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
                },
            },
        },
        value: SystemPropVariant::default(),
    });
    let params = (&raw mut request.params).cast::<u8>();
    // SAFETY: the union written through its tag's member: a blob over the
    // parameters beside it, in the same box.
    unsafe {
        let tagged = &mut request.value.Anonymous.Anonymous;
        tagged.vt = VT_BLOB;
        tagged.Anonymous.blob.cbSize =
            u32::try_from(core::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>()).unwrap_or(0);
        tagged.Anonymous.blob.pBlobData = params;
    }
    let mut operation: *mut IActivateAudioInterfaceAsyncOperation = ptr::null_mut();
    // SAFETY: the system's own name for the capture of a process, the client's
    // identifier, the request above (kept until the opening is done), a
    // completion whose reference the call takes for itself, and a local
    // out-parameter.
    let hr = unsafe {
        call(
            windows_sys::core::w!("VAD\\Process_Loopback"),
            ptr::from_ref(&IID_IAudioClient),
            &raw const request.value,
            completion.as_handler(),
            &raw mut operation,
        )
    };
    if hr < 0 {
        return Err(Error::Refused(hr));
    }
    // SAFETY: the call succeeded and handed over a reference.
    let operation = unsafe { Com::from_raw(operation) }.ok_or(Error::Refused(hr))?;
    Ok(Capturing::Opening {
        completion,
        operation,
        _request: request,
    })
}

/// The opened capture, started: 48 kHz stereo floats on its own event.
fn running(operation: &Com<IActivateAudioInterfaceAsyncOperation>) -> Result<Capturing, Error> {
    let mut result: HRESULT = E_NOTIMPL;
    let mut unknown: *mut IUnknown = ptr::null_mut();
    // SAFETY: a completed operation and local out-parameters.
    let hr = unsafe {
        com!(
            operation.as_ptr(),
            GetActivateResult,
            &raw mut result,
            &raw mut unknown
        )
    };
    // SAFETY: a reference handed over, or null.
    let unknown = unsafe { Com::from_raw(unknown) };
    if hr < 0 {
        return Err(Error::Refused(hr));
    }
    if result < 0 {
        return Err(Error::Refused(result));
    }
    let unknown = unknown.ok_or(Error::Refused(result))?;
    let mut out: *mut c_void = ptr::null_mut();
    // SAFETY: a live object, the client's identifier and a local out-parameter.
    let hr = unsafe {
        com!(
            unknown.as_ptr(),
            QueryInterface,
            ptr::from_ref(&IID_IAudioClient),
            &raw mut out
        )
    };
    if hr < 0 {
        return Err(Error::Refused(hr));
    }
    // SAFETY: the call succeeded and handed over a reference.
    let client = unsafe { Com::from_raw(out.cast::<IAudioClient>()) }.ok_or(Error::Refused(hr))?;
    let format = format();
    // SAFETY: a live client; the format outlives the call, which copies it.
    // The buffer asked for is the endpoint loopback's; this capture holds one
    // packet whatever is asked.
    let hr = unsafe {
        com!(
            client.as_ptr(),
            Initialize,
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK | CONVERT,
            super::LOOPBACK_BUFFER,
            0,
            &raw const format,
            ptr::null()
        )
    };
    if hr < 0 {
        return Err(Error::Refused(hr));
    }
    let wake = Event::new(false)?;
    // SAFETY: an initialised client and an event that outlives the stream:
    // both are this capture's.
    let hr = unsafe { com!(client.as_ptr(), SetEventHandle, wake.0) };
    if hr < 0 {
        return Err(Error::Refused(hr));
    }
    let capture = service::<IAudioCaptureClient>(&client, &IID_IAudioCaptureClient)?;
    // SAFETY: an initialised client.
    let hr = unsafe { com!(client.as_ptr(), Start) };
    if hr < 0 {
        return Err(Error::Refused(hr));
    }
    Ok(Capturing::Running {
        client,
        capture,
        wake,
    })
}

/// The speakers at the desk, which the local mute silences while an
/// application's sound is captured: **the output's own mute is behind the
/// capture of a process** (measured), as its level is.
struct Speakers {
    volume: Option<Com<IAudioEndpointVolume>>,
    name: String,
    muted: LocalMute,
}

impl Speakers {
    fn none() -> Self {
        Self {
            volume: None,
            name: String::new(),
            muted: LocalMute::default(),
        }
    }

    /// The default output's, silenced.
    fn take(enumerator: &Com<IMMDeviceEnumerator>) -> Self {
        let Ok(output) = default_output(enumerator) else {
            return Self::none();
        };
        let volume = activate::<IAudioEndpointVolume>(&output, &IID_IAudioEndpointVolume).ok();
        let name = friendly_name(&output).unwrap_or_default();
        let muted = mute_speakers(volume.as_ref(), &name);
        Self {
            volume,
            name,
            muted,
        }
    }

    fn restore(&mut self) {
        restore_speakers(self.volume.as_ref(), &self.name, &self.muted);
        self.muted = LocalMute::default();
    }
}

/// Everything the loop holds.
struct App<'a> {
    enumerator: &'a Com<IMMDeviceEnumerator>,
    signal: &'a Arc<Signal>,
    sessions: Sessions,
    judge: Judge,
    targets: Vec<Target>,
    mix: Mix,
    speakers: Speakers,
    own: u32,
    swept: lowlat_common::clock::Time,
    /// Frames that came after their frame was handed on, and steps; for the
    /// end's line.
    late: u64,
    steps: u64,
    /// Scratch for a sweep, kept.
    pids: Vec<u32>,
}

pub(in crate::capture) fn serve<S: FnMut(&[u8])>(
    config: &Settings,
    application: &Application,
    stop: &Stop,
    device: &Mutex<String>,
    report: &std::sync::mpsc::Sender<Result<(), Error>>,
    frames: &mut Frames<S>,
) -> Result<Ended, Error> {
    let opened = (|| -> Result<_, Error> {
        let enumerator = enumerator()?;
        let signal = Arc::new(Signal::new()?);
        Ok((enumerator, signal))
    })();
    let (enumerator, signal) = match opened {
        Ok(opened) => opened,
        Err(error) => {
            let _ = report.send(Err(error));
            return Err(error);
        }
    };
    // **Outputs coming and going need the system's word**; without it the
    // sessions of the outputs there now are still read.
    let registration = match Registration::new(&enumerator, &signal) {
        Ok(registration) => Some(registration),
        Err(hr) => {
            lowlat_common::log_warn!(
                "audio: outputs will not be followed, error={}",
                Error::Refused(hr)
            );
            None
        }
    };
    // **No output is read**: an application's sound is taken from its
    // processes, on whichever output each plays.
    match device.lock() {
        Ok(mut held) => held.clear(),
        Err(held) => held.into_inner().clear(),
    }
    lowlat_common::log_info!("audio: capturing an application's own sound");
    let _ = report.send(Ok(()));

    let mut app = App {
        enumerator: &enumerator,
        signal: &signal,
        sessions: Sessions::open(&enumerator, &signal),
        judge: Judge::new(application.clone()),
        targets: Vec::new(),
        mix: Mix::new(now_us()),
        speakers: if config.live().mute_local {
            Speakers::take(&enumerator)
        } else {
            Speakers::none()
        },
        // SAFETY: no arguments.
        own: unsafe { GetCurrentProcessId() },
        swept: lowlat_common::clock::Time::now(),
        late: 0,
        steps: 0,
        pids: Vec::new(),
    };
    app.reconcile();
    // **However the loop leaves, the speakers come back.**
    let outcome = app.run(config, application, stop, frames);
    app.speakers.restore();
    lowlat_common::log_info!(
        "audio: an application's sound is no longer captured, late={} steps={}",
        app.late,
        app.steps
    );
    drop(app);
    drop(registration);
    outcome
}

impl App<'_> {
    fn run<S: FnMut(&[u8])>(
        &mut self,
        config: &Settings,
        application: &Application,
        stop: &Stop,
        frames: &mut Frames<S>,
    ) -> Result<Ended, Error> {
        let mut seen = config.wanted.generation();
        let mut handles: [HANDLE; CAPTURES_MAX + 2] = [ptr::null_mut(); CAPTURES_MAX + 2];
        loop {
            let mut count = 0usize;
            for handle in [stop.event.0, self.signal.event.0]
                .into_iter()
                .chain(self.targets.iter().filter_map(Target::handle))
            {
                if let Some(slot) = handles.get_mut(count) {
                    *slot = handle;
                    count += 1;
                }
            }
            let until = u64::try_from(self.mix.deadline() - now_us()).unwrap_or(0);
            let sweep_left = (SWEEP_MS - lowlat_common::clock::elapsed_ms(self.swept)).max(0.0);
            // Rounded up, at least a millisecond: a wait never becomes a poll.
            let timeout = u32::try_from(until.div_ceil(1000))
                .unwrap_or(WAIT_CAP_MS)
                .min(sweep_left_ms(sweep_left))
                .clamp(1, WAIT_CAP_MS);
            // SAFETY: `count` live handles: the stop's, the notices' and each
            // capture's, each outliving the wait.
            unsafe {
                WaitForMultipleObjects(
                    u32::try_from(count).unwrap_or(0),
                    handles.as_ptr(),
                    0,
                    timeout,
                );
            }
            if stop.asked() {
                return Ok(Ended::Stopped);
            }

            for target in &mut self.targets {
                target.finish();
            }
            self.drain(frames);
            self.hand_on(frames);

            let mut reconcile = false;
            let before = self.targets.len();
            self.targets.retain(|target| {
                let gone = target.process.exited();
                if gone {
                    lowlat_common::log_info!(
                        "audio: a process of the application has exited, pid={}",
                        target.pid
                    );
                }
                !gone
            });
            reconcile |= self.targets.len() != before;
            if self.signal.outputs_changed.swap(false, Ordering::AcqRel) {
                // Dropped first, so the outputs' notices are given back
                // before new ones are registered.
                self.sessions = Sessions {
                    watched: Vec::new(),
                };
                self.sessions = Sessions::open(self.enumerator, self.signal);
                reconcile = true;
            }
            reconcile |= self.signal.sessions_changed.swap(false, Ordering::AcqRel);
            reconcile |= lowlat_common::clock::elapsed_ms(self.swept) >= SWEEP_MS;
            if reconcile {
                self.reconcile();
            }

            // **The speakers move with the default**, as they do when the
            // output is read.
            if self.signal.default_changed.swap(false, Ordering::AcqRel) && config.live().mute_local
            {
                self.speakers.restore();
                self.speakers = Speakers::take(self.enumerator);
            }

            let now = config.wanted.generation();
            if now != seen {
                seen = now;
                // **Another application, or none, ends this loop.**
                if !config
                    .application()
                    .is_some_and(|asked| asked.same(application))
                {
                    return Ok(Ended::Moved);
                }
                match (config.live().mute_local, self.speakers.muted.applied) {
                    (true, false) => self.speakers = Speakers::take(self.enumerator),
                    (false, true) => self.speakers.restore(),
                    _ => {}
                }
            }
        }
    }

    /// Everything each capture holds, onto the timeline.
    fn drain<S: FnMut(&[u8])>(&mut self, frames: &mut Frames<S>) {
        for index in 0..self.targets.len() {
            // **A lane alone moves the timeline to itself**: no other has
            // placed anything.
            let alone = !self
                .targets
                .iter()
                .enumerate()
                .any(|(other, target)| other != index && target.lane.next.is_some());
            let Some(target) = self.targets.get_mut(index) else {
                continue;
            };
            let Capturing::Running { capture, .. } = &target.state else {
                continue;
            };
            let capture = capture.as_ptr();
            let failed = loop {
                let mut next: u32 = 0;
                // SAFETY: a live capture half and a local out-parameter.
                let hr = unsafe { com!(capture, GetNextPacketSize, &raw mut next) };
                if hr < 0 {
                    break Some(hr);
                }
                if next == 0 {
                    break None;
                }
                let mut data: *mut u8 = ptr::null_mut();
                let (mut count, mut flags, mut stamp): (u32, u32, u64) = (0, 0, 0);
                // SAFETY: as above; the device position is not asked for.
                let hr = unsafe {
                    com!(
                        capture,
                        GetBuffer,
                        &raw mut data,
                        &raw mut count,
                        &raw mut flags,
                        ptr::null_mut(),
                        &raw mut stamp
                    )
                };
                if hr < 0 {
                    break Some(hr);
                }
                let length = u64::from(count);
                let had = target.lane.next;
                // The stamp is in the system's 100 ns units.
                let at = self.mix.place(
                    &mut target.lane,
                    i64::try_from(stamp / 10).unwrap_or(i64::MAX),
                    length,
                    alone,
                );
                if target.lane.next != had.map(|had| had + length) {
                    self.steps += u64::from(had.is_some());
                    lowlat_common::log_debug!(
                        "audio: a capture placed afresh, pid={} at={at} emitted={} ahead_us={} alone={alone}",
                        target.pid,
                        self.mix.emitted,
                        self.mix.time_of(at) - now_us()
                    );
                }
                // A reader held up long enough hands on what it can first.
                while !self.mix.fits(at + length) {
                    self.mix.emit(frames);
                }
                if flags & SILENT == 0 && !data.is_null() {
                    // SAFETY: the system lent `count` frames of the asked
                    // format at `data` until the release below.
                    let bytes = unsafe {
                        core::slice::from_raw_parts(
                            data,
                            usize::try_from(count).unwrap_or(0) * FLOAT_FRAME,
                        )
                    };
                    self.late += self.mix.add(at, bytes);
                }
                // SAFETY: exactly the frames taken above.
                let hr = unsafe { com!(capture, ReleaseBuffer, count) };
                if hr < 0 {
                    break Some(hr);
                }
            };
            if let Some(hr) = failed {
                // **A capture that fails is taken again** by the next
                // reconcile, the process being the application's still.
                lowlat_common::log_warn!(
                    "audio: a process's capture failed, pid={}, error={}",
                    target.pid,
                    Error::Read(hr)
                );
                target.state = Capturing::Refused;
                target.lane = Lane::default();
            }
        }
    }

    /// Every frame that is filled or due, to the sink.
    fn hand_on<S: FnMut(&[u8])>(&mut self, frames: &mut Frames<S>) {
        let now = now_us();
        let overdue = (now - self.mix.deadline()) / span_us(FRAME_FRAMES).max(1);
        if overdue > CATCH_UP_MAX {
            // **Held up too long to catch up**: the timeline starts again
            // from now and every capture is placed afresh.
            lowlat_common::log_warn!(
                "audio: the application's mix was held up, frames={overdue}, started again"
            );
            self.mix
                .restart(now, self.targets.iter_mut().map(|target| &mut target.lane));
            return;
        }
        while self
            .mix
            .filled(self.targets.iter().map(|target| &target.lane))
        {
            self.mix.emit(frames);
        }
        // **One frame on its deadline a pass**, the captures drained before
        // the next: a loop held up -- in the sink, say -- would otherwise hand
        // on as silence what the captures still hold.
        if now_us() >= self.mix.deadline() {
            self.mix.emit(frames);
        }
    }

    /// The captures as the sessions say they should be: **one for each
    /// process of the application that sounds, unless its parent's holds it**.
    fn reconcile(&mut self) {
        self.swept = lowlat_common::clock::Time::now();
        let mut pids = core::mem::take(&mut self.pids);
        self.sessions.sweep(self.own, &mut pids);
        self.judge.forget_exited();
        let mut parents = None;
        let mut sounding: Vec<(u64, u32, Option<u32>)> = Vec::with_capacity(pids.len());
        for &pid in &pids {
            if let Some(known) = self.judge.judge(pid, &mut parents)
                && known.holds
            {
                sounding.push((known.created, pid, known.parent));
            }
        }
        self.pids = pids;
        let wanted = targets(&mut sounding);
        self.targets.retain(|target| {
            let keep = wanted
                .iter()
                .any(|&(pid, created)| pid == target.pid && created == target.created);
            if !keep {
                lowlat_common::log_info!(
                    "audio: a process of the application is captured with its parent, pid={}",
                    target.pid
                );
            }
            keep
        });
        for (pid, created) in wanted {
            if self
                .targets
                .iter()
                .any(|target| target.pid == pid && target.created == created)
            {
                continue;
            }
            if self.targets.len() >= CAPTURES_MAX {
                lowlat_common::log_warn!(
                    "audio: too many processes of the application sound, pid={pid} not captured"
                );
                break;
            }
            if let Some(target) = Target::open(pid, created) {
                self.targets.push(target);
            }
        }
        // A failed capture of a process still the application's, asked for
        // again: the sweep's two seconds pace it.
        for target in &mut self.targets {
            if matches!(target.state, Capturing::Refused)
                && !target.process.exited()
                && let Ok(state) = begin(target.pid)
            {
                target.state = state;
            }
        }
    }
}

/// The time left to the next sweep, in whole milliseconds, rounded up.
fn sweep_left_ms(left: f64) -> u32 {
    if left >= f64::from(WAIT_CAP_MS) {
        WAIT_CAP_MS
    } else {
        // Bounded above, so the conversion is exact.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let ms = left.ceil() as u32;
        ms
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A packet of `frames` frames whose samples count up from `from`, left
    /// and right the same.
    fn packet(from: u32, frames: u64) -> Vec<u8> {
        (0..u32::try_from(frames).unwrap())
            .flat_map(|i| {
                let value = f32::from(u16::try_from((from + i) % 30000).unwrap()) / 32768.0;
                [value.to_le_bytes(), value.to_le_bytes()].concat()
            })
            .collect()
    }

    /// The left channel of every frame handed on, in order.
    fn handed(mix: &mut Mix, count: usize) -> Vec<i16> {
        let mut seen = Vec::new();
        {
            let mut frames = Frames::new(|frame: &[u8]| {
                seen.extend(
                    frame
                        .chunks_exact(PCM_FRAME)
                        .map(|pair| i16::from_le_bytes([pair[0], pair[1]])),
                );
            });
            for _ in 0..count {
                mix.emit(&mut frames);
            }
        }
        seen
    }

    /// **One capture, alone, adds nothing to its delay**: its packets end on
    /// the timeline's marks as they arrive, run on contiguously, and every
    /// sample comes out once, in order.
    #[test]
    fn one_capture_moves_the_timeline_to_itself() {
        let mut mix = Mix::new(1_000_000);
        let mut lane = Lane::default();
        // A capture whose phase is nowhere near the timeline's.
        let first = 1_234_567;
        let mut at = Vec::new();
        for n in 0..4u32 {
            let stamp = first + i64::from(n) * 10_000;
            let place = mix.place(&mut lane, stamp, PACKET, true);
            mix.add(place, &packet(n * 480, PACKET));
            at.push(place);
        }
        assert_eq!(at, vec![0, 480, 960, 1440], "the packets did not run on");
        // The first packet's end lands at its stamped end and the lead.
        assert_eq!(mix.time_of(480), first + 10_000 + LEAD_US);
        assert!(mix.filled([&lane].into_iter()));
        let seen = handed(&mut mix, 2);
        let expected: Vec<i16> = (0..1920u32)
            .map(|i| to_sixteen(f32::from(u16::try_from(i).unwrap()) / 32768.0))
            .collect();
        assert_eq!(seen, expected, "a sample was lost, doubled or moved");
    }

    /// **Two captures at their own phases are both heard, whole**: the
    /// second lands on the first's timeline at the earliest mark it reaches
    /// in time, less than a packet later than it arrives, and a frame is
    /// handed on once both have filled it, the two summed.
    #[test]
    fn two_captures_at_their_own_phases_mix_whole() {
        let mut mix = Mix::new(0);
        let (mut a, mut b) = (Lane::default(), Lane::default());
        let (a0, b0) = (50_000, 53_700);
        let pa = mix.place(&mut a, a0, PACKET, true);
        assert_eq!(pa, 0);
        let pb = mix.place(&mut b, b0, PACKET, false);
        assert_eq!(pb % PACKET, 0, "the second capture is off the marks");
        let waits = mix.time_of(pb + PACKET) - (b0 + 10_000 + LEAD_US);
        assert!(
            (0..10_000).contains(&waits),
            "the second capture waits {waits} us"
        );
        mix.add(pa, &packet(0, PACKET));
        mix.add(pb, &packet(1000, PACKET));
        assert!(
            !mix.filled([&a, &b].into_iter()),
            "a frame was taken as filled with a capture short of it"
        );
        let pa2 = mix.place(&mut a, a0 + 10_000, PACKET, false);
        assert_eq!(pa2, PACKET);
        mix.add(pa2, &packet(480, PACKET));
        assert!(mix.filled([&a, &b].into_iter()));
        let value = |i: u32| f32::from(u16::try_from(i).unwrap()) / 32768.0;
        let seen = handed(&mut mix, 1);
        let b_at = u32::try_from(pb).unwrap();
        for (i, &sample) in (0u32..).zip(&seen) {
            let mut expected = value(i);
            if i >= b_at && i < b_at + 480 {
                expected += value(1000 + i - b_at);
            }
            assert_eq!(sample, to_sixteen(expected), "frame {i}");
        }
    }

    /// **A step in a capture's stamps is silence of its length**, to within
    /// half a mark for a capture alone (the timeline moves by the rest) and a
    /// mark for one beside another; never over what came before.
    #[test]
    fn a_step_is_silence_of_its_length() {
        let mut mix = Mix::new(0);
        let mut lane = Lane::default();
        let p0 = mix.place(&mut lane, 100_000, PACKET, true);
        let p1 = mix.place(&mut lane, 110_000, PACKET, true);
        assert_eq!(p1, p0 + PACKET);
        // 7 ms of stamps with no frames: a pause in what it delivered.
        let p2 = mix.place(&mut lane, 127_000, PACKET, true);
        let gap = p2 - (p1 + PACKET);
        assert!(
            (336 - PACKET / 2..=336 + PACKET / 2).contains(&gap),
            "a 7 ms step alone left {gap} frames"
        );
        assert_eq!(p2 % PACKET, 0, "realigned off the marks");
        assert_eq!(mix.time_of(p2 + PACKET), 127_000 + 10_000 + LEAD_US);
        // A 2 ms step alone is taken by the timeline: no silence at all.
        let p3 = mix.place(&mut lane, 139_000, PACKET, true);
        assert_eq!(p3, p2 + PACKET, "a 2 ms step alone left silence");
        // Beside another capture the timeline stays, and the step is silence
        // of its length to within a mark.
        let before = mix.anchor;
        let p4 = mix.place(&mut lane, 149_000 + 9_000, PACKET, false);
        assert_eq!(
            mix.anchor, before,
            "the timeline moved under another capture"
        );
        let gap = p4 - (p3 + PACKET);
        assert!(
            (0..=432 + PACKET).contains(&gap),
            "a 9 ms step beside another left {gap} frames"
        );
        // And a step back never lands over what the lane has.
        let p5 = mix.place(&mut lane, 120_000, PACKET, false);
        assert!(p5 >= p4 + PACKET, "a step back overwrote the lane");
    }

    /// **A lane alone that fell behind what was handed on leaves the timeline
    /// where it is**: placed at the earliest mark it reaches in time, as one
    /// beside another is, not by moving the timeline back to it.
    #[test]
    fn a_lane_behind_what_was_handed_on_leaves_the_timeline_be() {
        let mut mix = Mix::new(0);
        let mut lane = Lane::default();
        let p0 = mix.place(&mut lane, 0, PACKET, true);
        mix.add(p0, &packet(0, PACKET));
        // Handed on well past it while the reader was held up.
        handed(&mut mix, 3);
        let before = mix.anchor;
        // The next packet, 20 ms of it lost: still behind what was handed on.
        let p1 = mix.place(&mut lane, 30_000, PACKET, true);
        assert_eq!(
            mix.anchor, before,
            "the timeline moved back to a lane behind it"
        );
        assert!(p1 >= mix.emitted, "placed behind what was handed on");
        assert_eq!((p1 + PACKET) % PACKET, 0);
    }

    /// **Frames already handed on are not written**, and are counted: a
    /// late frame written would come round the ring into a later frame.
    #[test]
    fn a_late_packet_is_dropped_and_counted() {
        let mut mix = Mix::new(0);
        handed(&mut mix, 1);
        assert_eq!(
            mix.add(0, &packet(0, PACKET)),
            PACKET,
            "late frames not counted"
        );
        // Straddling what was handed on: the part before it alone is late.
        assert_eq!(mix.add(FRAME_FRAMES - 80, &packet(0, PACKET)), 80);
        // Round the ring: the part in time where it was put, nothing else.
        let laps = usize::try_from(RING / FRAME_FRAMES).unwrap() + 1;
        let seen = handed(&mut mix, laps);
        for (i, &sample) in (0u32..).zip(&seen) {
            let expected = if i < 400 {
                to_sixteen(f32::from(u16::try_from(80 + i).unwrap()) / 32768.0)
            } else {
                0
            };
            assert_eq!(sample, expected, "frame {} after the first", i);
        }
        let mut lane = Lane::default();
        let at = mix.place(&mut lane, 0, PACKET, true);
        assert!(at >= mix.emitted, "placed behind what was handed on");
    }

    /// **Nothing sounding is silence on time**: with no capture the frames
    /// are due at their end and the wait, and they are zeros.
    #[test]
    fn nothing_sounding_is_silence_on_time() {
        let mut mix = Mix::new(5_000_000);
        assert_eq!(mix.deadline(), 5_000_000 + 20_000 + WAIT_US);
        assert!(!mix.filled([].into_iter()), "an empty room filled a frame");
        let seen = handed(&mut mix, 3);
        assert!(seen.iter().all(|&s| s == 0));
        assert_eq!(mix.deadline(), 5_000_000 + 80_000 + WAIT_US);
    }

    /// **A reader held up past the ring loses what it missed and nothing
    /// else**: the capture's stamps jump, it is placed afresh ahead, and the
    /// frames before are handed on as they stand.
    #[test]
    fn a_reader_held_up_places_the_capture_afresh() {
        let mut mix = Mix::new(0);
        let mut lane = Lane::default();
        let p0 = mix.place(&mut lane, 0, PACKET, true);
        mix.add(p0, &packet(0, PACKET));
        // Held up 60 ms: the capture kept its last packet only.
        let p1 = mix.place(&mut lane, 60_000, PACKET, false);
        assert!(p1 >= p0 + PACKET + 48 * 40, "the 50 ms lost was not left");
        assert!(
            mix.fits(p1 + PACKET),
            "a held-up reader's packet did not fit"
        );
    }

    /// **A frame handed on leaves its slots clear**: round the ring again, the
    /// timeline holds silence there, not what was handed on before.
    #[test]
    fn the_ring_is_clear_once_a_frame_is_handed_on() {
        let mut mix = Mix::new(0);
        let mut lane = Lane::default();
        let at = mix.place(&mut lane, 0, PACKET, true);
        mix.add(at, &packet(5000, PACKET));
        assert!(handed(&mut mix, 1).iter().any(|&s| s != 0));
        let laps = usize::try_from(RING / FRAME_FRAMES).unwrap();
        let seen = handed(&mut mix, laps);
        assert!(
            seen.iter().all(|&s| s == 0),
            "a frame handed on came round again"
        );
    }

    /// **Each sound is taken once**: a process is captured unless its parent
    /// is, and a parent is one only if it was made before its child.
    #[test]
    fn a_process_is_captured_unless_its_parent_is() {
        // A (1) started B (2), B started C (3), C started D (4): A, C.
        let mut chain = vec![
            (10, 1, None),
            (20, 2, Some(1)),
            (30, 3, Some(2)),
            (40, 4, Some(3)),
        ];
        assert_eq!(targets(&mut chain), vec![(1, 10), (3, 30)]);
        // B sounds alone under a silent A: B.
        let mut alone = vec![(20, 2, Some(1))];
        assert_eq!(targets(&mut alone), vec![(2, 20)]);
        // A number passed on: "parent" 5 made after its "child" 6, listed
        // first.
        let mut reused = vec![(50, 5, None), (40, 6, Some(5))];
        assert_eq!(targets(&mut reused), vec![(6, 40), (5, 50)]);
        // Two of the application's own, unrelated: both.
        let mut two = vec![(10, 1, Some(100)), (11, 7, Some(200))];
        assert_eq!(targets(&mut two), vec![(1, 10), (7, 11)]);
    }

    /// The per-packet path allocates nothing: placing, adding and handing on.
    #[test]
    fn a_packet_allocates_nothing() {
        let mut mix = Mix::new(0);
        let mut lane = Lane::default();
        let bytes = packet(0, PACKET);
        let mut handed = 0usize;
        let mut frames = Frames::new(|frame: &[u8]| handed += frame.len());
        let mut stamp = 0i64;
        lowlat_common::alloc_counter::assert_no_alloc(|| {
            for _ in 0..16 {
                let at = mix.place(&mut lane, stamp, PACKET, true);
                while !mix.fits(at + PACKET) {
                    mix.emit(&mut frames);
                }
                mix.add(at, &bytes);
                stamp += 10_000;
                if mix.filled([&lane].into_iter()) {
                    mix.emit(&mut frames);
                }
            }
        });
        drop(frames);
        assert_eq!(handed, FRAME_BYTES * 8);
    }
}
