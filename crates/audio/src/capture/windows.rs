//! The desktop's own output on Windows: a loopback of the output, read on the
//! system's own event.
//!
//! **A loopback delivers nothing while nothing plays**, so a stream of silence
//! is started on the same output, once, and never fed: the system's engine
//! keeps running for it and the loopback delivers on the device's clock. The
//! packets it then delivers come flagged as silent, and **a flagged packet is
//! taken as zeros of its length**; skipping it would lose the clock again.
//!
//! **The engine converts, the host rounds.** The loopback is asked for 48 kHz
//! stereo in floating point, so the engine converts the rate and folds the
//! channels, and this turns it into sixteen bits itself. A program playing
//! digital silence reaches the loopback as a floor far below the lowest bit,
//! which the engine's own sixteen-bit conversion dithers into it; rounded here
//! it is exact zeros again, which is what silence is recognised by.
//!
//! **What a device name means here.** None follows the output the system's
//! sound settings name as the default, which the system says when it changes;
//! a named one is the system's own identifier for an output. A named output
//! that goes away while it is read is replaced by the default, and an output
//! another program takes in exclusive mode is a device that went away.

use core::ffi::c_void;
use core::ptr::{self, NonNull};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering, fence};
use std::sync::{Arc, Mutex, OnceLock};

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE};
use windows_sys::Win32::System::Com::StructuredStorage::{
    PROPVARIANT as SystemPropVariant, PropVariantClear,
};
use windows_sys::Win32::System::Com::{
    CLSCTX_ALL, CO_MTA_USAGE_COOKIE, CoCreateInstance, CoIncrementMTAUsage, CoTaskMemFree,
};
use windows_sys::Win32::System::Threading::{CreateEventW, SetEvent, WaitForMultipleObjects};
use windows_sys::Win32::System::Variant::VT_LPWSTR;

use super::{Frames, Output, Settings};
use crate::Error;
use crate::ffi::wasapi::{
    AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
    AUDCLNT_STREAMFLAGS_LOOPBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, DEVICE_STATE_ACTIVE,
    E_NOINTERFACE, EDataFlow, ENDPOINT_HARDWARE_SUPPORT_MUTE, ERole, GUID, HRESULT,
    IAudioCaptureClient, IAudioClient, IAudioEndpointVolume, IAudioRenderClient, IID, IMMDevice,
    IMMDeviceCollection, IMMDeviceEnumerator, IMMNotificationClient, IMMNotificationClientVtbl,
    IPropertyStore, IUnknown, LPCWSTR, PROPERTYKEY, PROPVARIANT, REFERENCE_TIME, STGM_READ, ULONG,
    WAVE_FORMAT_IEEE_FLOAT, WAVEFORMATEX, eConsole, eRender,
};
use crate::ffi::wasapi_guids::{
    CLSID_MMDeviceEnumerator, IID_IAudioCaptureClient, IID_IAudioClient, IID_IAudioEndpointVolume,
    IID_IAudioRenderClient, IID_IMMDeviceEnumerator, IID_IMMNotificationClient, IID_IUnknown,
    PKEY_Device_FriendlyName,
};

/// The loopback's buffer, in the system's 100 ns units: 200 ms.
///
/// The thread wakes every engine period (10 ms), so this only absorbs a wake
/// that came late; a wake later than this loses samples and says so.
const LOOPBACK_BUFFER: REFERENCE_TIME = 2_000_000;

/// The silent stream's buffer: 100 ms of silence, written once.
const SILENT_BUFFER: REFERENCE_TIME = 1_000_000;

/// The longest the loop waits without a packet before it looks again. A safety
/// cap, not a cadence: the loopback's own event wakes it every period.
const WAIT_CAP_MS: u32 = 500;

/// The engine converts whatever the output mixes to this.
const CONVERT: u32 = AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;

/// The format asked for: 48 kHz stereo floating point.
const FLOAT_TAG: u16 = 3;
const _: () = assert!(FLOAT_TAG as u32 == WAVE_FORMAT_IEEE_FLOAT);
const CHANNELS: u16 = 2;
const _: () = assert!(CHANNELS as usize == crate::CHANNELS);
/// Bytes of one sample, and of one frame, as the loopback delivers them.
const FLOAT_SAMPLE: usize = 4;
const FLOAT_FRAME: usize = FLOAT_SAMPLE * crate::CHANNELS;
/// Bytes of one frame once it is sixteen bits.
const PCM_FRAME: usize = 2 * crate::CHANNELS;

/// The packet flags, as the system writes them into an unsigned word.
const SILENT: u32 = 2;
const _: () = assert!(SILENT as i32 == AUDCLNT_BUFFERFLAGS_SILENT);
const DISCONTINUITY: u32 = 1;
const _: () = assert!(DISCONTINUITY as i32 == AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY);

/// No default output: `HRESULT_FROM_WIN32(ERROR_NOT_FOUND)`, which the header
/// writes as a function rather than a value.
const E_NOTFOUND: HRESULT = i32::from_ne_bytes(0x8007_0490_u32.to_ne_bytes());
/// A table with no entry for a method, which the system never hands out.
const E_NOTIMPL: HRESULT = i32::from_ne_bytes(0x8000_4001_u32.to_ne_bytes());
const S_OK: HRESULT = 0;

/// A property's value is read through the system's own declaration of it.
const _: () =
    assert!(core::mem::size_of::<SystemPropVariant>() == core::mem::size_of::<PROPVARIANT>());
/// The identifiers the system's calls take are the same sixteen bytes.
const _: () =
    assert!(core::mem::size_of::<windows_sys::core::GUID>() == core::mem::size_of::<GUID>());

/// Call through an interface's table, inside `unsafe`. A missing entry is the
/// system's own fault, answered as one.
macro_rules! com {
    ($this:expr, $method:ident $(, $arg:expr)* $(,)?) => {{
        let this = $this;
        match (*(*this).lpVtbl).$method {
            Some(method) => method(this $(, $arg)*),
            None => E_NOTIMPL,
        }
    }};
}

/// An application's own sound: declared after the call macro, which it uses.
#[path = "windows/application.rs"]
mod application;

/// The process's multithreaded apartment, kept alive once and never given
/// back, so the capture's thread and an application's thread asking for the
/// outputs both reach the system's sound interfaces **without either being put
/// into an apartment by the library**.
static APARTMENT: OnceLock<Result<(), Error>> = OnceLock::new();

pub(super) fn prepare() -> Result<(), Error> {
    *APARTMENT.get_or_init(|| {
        let mut cookie: CO_MTA_USAGE_COOKIE = ptr::null_mut();
        // SAFETY: the out-parameter is a local. The usage is never given back:
        // the apartment lives as long as the process.
        let hr = unsafe { CoIncrementMTAUsage(&raw mut cookie) };
        if hr < 0 {
            Err(Error::Refused(hr))
        } else {
            Ok(())
        }
    })
}

/// A kernel event this side made, closed on drop.
#[derive(Debug)]
struct Event(HANDLE);

// SAFETY: an event handle is a process-wide token; setting it and waiting on
// it are safe from any thread.
unsafe impl Send for Event {}
// SAFETY: as above, and nothing here writes through `&self` but the event.
unsafe impl Sync for Event {}

impl Event {
    /// `manual` stays set once set, so every wait after a stop sees it; the
    /// others are taken by the wait that sees them.
    fn new(manual: bool) -> Result<Self, Error> {
        // SAFETY: no attributes and no name; the handle is this value's.
        let handle = unsafe { CreateEventW(ptr::null(), i32::from(manual), 0, ptr::null()) };
        if handle.is_null() {
            Err(Error::Refused(last_error()))
        } else {
            Ok(Self(handle))
        }
    }

    fn set(&self) {
        // SAFETY: a live event this value owns.
        unsafe { SetEvent(self.0) };
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: made by `new` and closed once.
        unsafe { CloseHandle(self.0) };
    }
}

/// The calling thread's last error, as a result value.
fn last_error() -> HRESULT {
    // SAFETY: reads the calling thread's own value.
    let code = unsafe { GetLastError() };
    i32::from_ne_bytes((0x8007_0000_u32 | (code & 0xFFFF)).to_ne_bytes())
}

/// How an owner asks the loop to stop: a flag, and the event that wakes the
/// loop's wait at once rather than at its next packet.
#[derive(Debug)]
pub(super) struct Stop {
    asked: AtomicBool,
    event: Event,
}

impl Stop {
    pub(super) fn new() -> Result<Self, Error> {
        Ok(Self {
            asked: AtomicBool::new(false),
            event: Event::new(true)?,
        })
    }

    pub(super) fn request(&self) {
        self.asked.store(true, Ordering::Release);
        self.event.set();
    }

    fn asked(&self) -> bool {
        self.asked.load(Ordering::Acquire)
    }
}

/// An interface this side holds a reference on, released on drop.
struct Com<T>(NonNull<T>);

impl<T> Com<T> {
    /// Take ownership of a reference.
    ///
    /// # Safety
    ///
    /// `ptr` is null or a live interface of type `T` whose reference the
    /// caller hands over, so the drop's release is the caller's own.
    unsafe fn from_raw(ptr: *mut T) -> Option<Self> {
        NonNull::new(ptr).map(Self)
    }

    fn as_ptr(&self) -> *mut T {
        self.0.as_ptr()
    }
}

impl<T> Drop for Com<T> {
    fn drop(&mut self) {
        // Every interface's table begins with the base interface's three, so
        // the release is reached through the base's layout.
        let base = self.0.as_ptr().cast::<IUnknown>();
        // SAFETY: a live interface whose reference this holds, released once.
        unsafe {
            if let Some(release) = (*(*base).lpVtbl).Release {
                release(base);
            }
        }
    }
}

/// The system's device enumeration.
fn enumerator() -> Result<Com<IMMDeviceEnumerator>, Error> {
    let mut out: *mut c_void = ptr::null_mut();
    // SAFETY: the class and interface identifiers are the header's, the same
    // sixteen bytes the call's own type has; the out-parameter is a local.
    let hr = unsafe {
        CoCreateInstance(
            ptr::from_ref(&CLSID_MMDeviceEnumerator).cast(),
            ptr::null_mut(),
            CLSCTX_ALL,
            ptr::from_ref(&IID_IMMDeviceEnumerator).cast(),
            &raw mut out,
        )
    };
    if hr < 0 {
        return Err(Error::Refused(hr));
    }
    // SAFETY: the call succeeded and handed over a reference of that interface.
    unsafe { Com::from_raw(out.cast::<IMMDeviceEnumerator>()) }.ok_or(Error::Refused(hr))
}

/// The output the system's sound settings name as the default device.
fn default_output(enumerator: &Com<IMMDeviceEnumerator>) -> Result<Com<IMMDevice>, Error> {
    let mut out: *mut IMMDevice = ptr::null_mut();
    // SAFETY: a live enumeration; the out-parameter is a local.
    let hr = unsafe {
        com!(
            enumerator.as_ptr(),
            GetDefaultAudioEndpoint,
            eRender,
            eConsole,
            &raw mut out
        )
    };
    if hr == E_NOTFOUND {
        return Err(Error::NoSuchDevice);
    }
    if hr < 0 {
        return Err(Error::Refused(hr));
    }
    // SAFETY: as in `enumerator`.
    unsafe { Com::from_raw(out) }.ok_or(Error::NoSuchDevice)
}

/// An output by the system's identifier for it, if it is there and active.
fn output_by_id(enumerator: &Com<IMMDeviceEnumerator>, id: &str) -> Result<Com<IMMDevice>, Error> {
    let wide: Vec<u16> = id.encode_utf16().chain(core::iter::once(0)).collect();
    let mut out: *mut IMMDevice = ptr::null_mut();
    // SAFETY: a live enumeration, a terminated name that outlives the call, and
    // a local out-parameter.
    let hr = unsafe { com!(enumerator.as_ptr(), GetDevice, wide.as_ptr(), &raw mut out) };
    if hr < 0 {
        return Err(Error::NoSuchDevice);
    }
    // SAFETY: as in `enumerator`.
    let output = unsafe { Com::from_raw(out) }.ok_or(Error::NoSuchDevice)?;
    let mut state: u32 = 0;
    // SAFETY: a live device and a local out-parameter.
    let hr = unsafe { com!(output.as_ptr(), GetState, &raw mut state) };
    if hr < 0 || state != DEVICE_STATE_ACTIVE {
        return Err(Error::NoSuchDevice);
    }
    Ok(output)
}

/// A string the system allocated, copied out.
///
/// # Safety
///
/// `text` is a terminated wide string that stays live for the call.
unsafe fn copy_wide(text: *const u16) -> String {
    // An identifier or a name; a bound keeps a missing terminator from reading
    // on for ever.
    let mut len = 0usize;
    // SAFETY: the caller's contract, read up to the terminator.
    while len < 4096 && unsafe { *text.add(len) } != 0 {
        len += 1;
    }
    // SAFETY: `len` units were just read.
    String::from_utf16_lossy(unsafe { core::slice::from_raw_parts(text, len) })
}

/// The system's identifier for an output.
fn output_id(output: &Com<IMMDevice>) -> Result<String, Error> {
    let mut id: *mut u16 = ptr::null_mut();
    // SAFETY: a live device and a local out-parameter.
    let hr = unsafe { com!(output.as_ptr(), GetId, &raw mut id) };
    if hr < 0 || id.is_null() {
        return Err(Error::Refused(hr));
    }
    // SAFETY: a terminated string the call allocated, freed once after the copy.
    let text = unsafe { copy_wide(id) };
    // SAFETY: the system's own allocation, handed to the caller to free.
    unsafe { CoTaskMemFree(id.cast_const().cast()) };
    Ok(text)
}

/// What a person calls an output.
fn friendly_name(output: &Com<IMMDevice>) -> Option<String> {
    let mut store: *mut IPropertyStore = ptr::null_mut();
    // SAFETY: a live device and a local out-parameter.
    let hr = unsafe {
        com!(
            output.as_ptr(),
            OpenPropertyStore,
            STGM_READ,
            &raw mut store
        )
    };
    if hr < 0 {
        return None;
    }
    // SAFETY: as in `enumerator`.
    let store = unsafe { Com::from_raw(store) }?;
    let mut value = SystemPropVariant::default();
    let key: PROPERTYKEY = PKEY_Device_FriendlyName;
    // SAFETY: a live store, the key, and a value of the system's own layout,
    // the size the call's declaration has (asserted above).
    let hr = unsafe {
        com!(
            store.as_ptr(),
            GetValue,
            &raw const key,
            (&raw mut value).cast::<PROPVARIANT>()
        )
    };
    // SAFETY: the union is read only where its tag says it holds a string.
    let name = unsafe {
        let tagged = &value.Anonymous.Anonymous;
        if hr >= 0 && tagged.vt == VT_LPWSTR && !tagged.Anonymous.pwszVal.is_null() {
            Some(copy_wide(tagged.Anonymous.pwszVal))
        } else {
            None
        }
    };
    // SAFETY: the value the call filled, cleared once.
    unsafe { PropVariantClear(&raw mut value) };
    name
}

/// An interface an output makes.
fn activate<T>(output: &Com<IMMDevice>, iid: &IID) -> Result<Com<T>, Error> {
    let mut out: *mut c_void = ptr::null_mut();
    // SAFETY: a live device, the interface's identifier, no parameters, and a
    // local out-parameter.
    let hr = unsafe {
        com!(
            output.as_ptr(),
            Activate,
            ptr::from_ref(iid),
            CLSCTX_ALL,
            ptr::null_mut(),
            &raw mut out
        )
    };
    if hr < 0 {
        return Err(Error::Refused(hr));
    }
    // SAFETY: as in `enumerator`.
    unsafe { Com::from_raw(out.cast::<T>()) }.ok_or(Error::Refused(hr))
}

/// A service a stream offers: its capture or render half.
fn service<T>(client: &Com<IAudioClient>, iid: &IID) -> Result<Com<T>, Error> {
    let mut out: *mut c_void = ptr::null_mut();
    // SAFETY: a live client, the service's identifier and a local out-parameter.
    let hr = unsafe {
        com!(
            client.as_ptr(),
            GetService,
            ptr::from_ref(iid),
            &raw mut out
        )
    };
    if hr < 0 {
        return Err(Error::Refused(hr));
    }
    // SAFETY: as in `enumerator`.
    unsafe { Com::from_raw(out.cast::<T>()) }.ok_or(Error::Refused(hr))
}

/// 48 kHz stereo floating point: what both of the host's streams use.
fn format() -> WAVEFORMATEX {
    WAVEFORMATEX {
        wFormatTag: FLOAT_TAG,
        nChannels: CHANNELS,
        nSamplesPerSec: crate::SAMPLE_RATE,
        nAvgBytesPerSec: crate::SAMPLE_RATE * 8,
        nBlockAlign: 8,
        wBitsPerSample: 32,
        cbSize: 0,
    }
}

/// One output, read: its loopback, and the silent stream that keeps it running.
struct Session {
    /// The system's identifier, which is what is published and configured.
    id: String,
    /// What a person calls it, for the log.
    name: String,
    loopback: Com<IAudioClient>,
    capture: Com<IAudioCaptureClient>,
    silent: Com<IAudioClient>,
    /// The output's own mute, for the speakers at the desk.
    volume: Option<Com<IAudioEndpointVolume>>,
    /// Packets read, so the stream's start -- flagged as a discontinuity -- is
    /// not reported as a loss.
    packets: u64,
    _output: Com<IMMDevice>,
}

impl Session {
    /// Open the named output, or the default one, and start reading it.
    fn open(
        enumerator: &Com<IMMDeviceEnumerator>,
        wanted: Option<&str>,
        wake: &Event,
    ) -> Result<Self, Error> {
        let output = match wanted {
            Some(id) => output_by_id(enumerator, id)?,
            None => default_output(enumerator)?,
        };
        let id = output_id(&output)?;
        let name = friendly_name(&output).unwrap_or_else(|| id.clone());
        let format = format();

        // **The silent stream first**, so the engine is running when the
        // loopback starts. Filled once and started; never fed again, because
        // a stream that has run dry still keeps the engine running (measured).
        let silent = activate::<IAudioClient>(&output, &IID_IAudioClient)?;
        // SAFETY: a live client; the format outlives the call, which copies it.
        let hr = unsafe {
            com!(
                silent.as_ptr(),
                Initialize,
                AUDCLNT_SHAREMODE_SHARED,
                CONVERT,
                SILENT_BUFFER,
                0,
                &raw const format,
                ptr::null()
            )
        };
        if hr < 0 {
            return Err(Error::Refused(hr));
        }
        let mut frames: u32 = 0;
        // SAFETY: an initialised client and a local out-parameter.
        let hr = unsafe { com!(silent.as_ptr(), GetBufferSize, &raw mut frames) };
        if hr < 0 {
            return Err(Error::Refused(hr));
        }
        let render = service::<IAudioRenderClient>(&silent, &IID_IAudioRenderClient)?;
        let mut data: *mut u8 = ptr::null_mut();
        // SAFETY: the whole buffer, released at once as silence: the system
        // writes the silence, so nothing is written through `data`.
        let hr = unsafe {
            let hr = com!(render.as_ptr(), GetBuffer, frames, &raw mut data);
            if hr < 0 {
                hr
            } else {
                com!(render.as_ptr(), ReleaseBuffer, frames, SILENT)
            }
        };
        if hr < 0 {
            return Err(Error::Refused(hr));
        }
        // SAFETY: an initialised client with its buffer filled.
        let hr = unsafe { com!(silent.as_ptr(), Start) };
        if hr < 0 {
            return Err(Error::Refused(hr));
        }

        let loopback = activate::<IAudioClient>(&output, &IID_IAudioClient)?;
        // SAFETY: as for the silent stream.
        let hr = unsafe {
            com!(
                loopback.as_ptr(),
                Initialize,
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK | CONVERT,
                LOOPBACK_BUFFER,
                0,
                &raw const format,
                ptr::null()
            )
        };
        if hr < 0 {
            return Err(Error::Refused(hr));
        }
        // SAFETY: an initialised client and an event that outlives the stream:
        // the loop that owns this session owns the event too.
        let hr = unsafe { com!(loopback.as_ptr(), SetEventHandle, wake.0) };
        if hr < 0 {
            return Err(Error::Refused(hr));
        }
        let capture = service::<IAudioCaptureClient>(&loopback, &IID_IAudioCaptureClient)?;
        // SAFETY: an initialised client.
        let hr = unsafe { com!(loopback.as_ptr(), Start) };
        if hr < 0 {
            return Err(Error::Refused(hr));
        }
        let volume = activate::<IAudioEndpointVolume>(&output, &IID_IAudioEndpointVolume).ok();
        Ok(Self {
            id,
            name,
            loopback,
            capture,
            silent,
            volume,
            packets: 0,
            _output: output,
        })
    }

    /// Hand on everything the loopback holds.
    ///
    /// **An error means the output was lost**: unplugged, reconfigured, or
    /// taken by a program in exclusive mode. Nothing read here is ever retried.
    fn drain<S: FnMut(&[u8])>(&mut self, frames: &mut Frames<S>) -> Result<(), Error> {
        let capture = self.capture.as_ptr();
        loop {
            let mut next: u32 = 0;
            // SAFETY: a live capture half and a local out-parameter.
            let hr = unsafe { com!(capture, GetNextPacketSize, &raw mut next) };
            if hr < 0 {
                return Err(Error::Read(hr));
            }
            if next == 0 {
                return Ok(());
            }
            let mut data: *mut u8 = ptr::null_mut();
            let mut count: u32 = 0;
            let mut flags: u32 = 0;
            // SAFETY: as above; the positions are not asked for.
            let hr = unsafe {
                com!(
                    capture,
                    GetBuffer,
                    &raw mut data,
                    &raw mut count,
                    &raw mut flags,
                    ptr::null_mut(),
                    ptr::null_mut()
                )
            };
            if hr < 0 {
                return Err(Error::Read(hr));
            }
            let length = usize::try_from(count).unwrap_or(0);
            if flags & SILENT != 0 || data.is_null() {
                // **Flagged silence is zeros of its length**: the buffer holds
                // nothing to read, and the length is the clock.
                take_silence(frames, length);
            } else {
                // SAFETY: the system lent `count` frames of the asked format
                // at `data` until the release below.
                let bytes = unsafe { core::slice::from_raw_parts(data, length * FLOAT_FRAME) };
                take_floats(frames, bytes);
            }
            if flags & DISCONTINUITY != 0 && self.packets > 0 {
                lowlat_common::log_warn!(
                    "audio: the loopback lost samples before a packet, output={}",
                    self.name
                );
            }
            self.packets = self.packets.saturating_add(1);
            // SAFETY: exactly the frames taken above.
            let hr = unsafe { com!(capture, ReleaseBuffer, count) };
            if hr < 0 {
                return Err(Error::Read(hr));
            }
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: live clients; a stream already lost answers with an error,
        // which changes nothing here.
        unsafe {
            com!(self.loopback.as_ptr(), Stop);
            com!(self.silent.as_ptr(), Stop);
        }
    }
}

/// Samples converted at a time, on the stack.
const CHUNK: usize = 512;

/// One sample, rounded to sixteen bits.
///
/// **Rounded, not truncated**, so the engine's own floor -- far below the
/// lowest bit -- reads as the exact zeros silence is recognised by; saturated,
/// though the engine limits real sound below full scale before the loopback.
fn to_sixteen(value: f32) -> i16 {
    // The clamp holds the value inside the type, so the conversion is exact;
    // a value that is not a number converts to zero.
    #[allow(clippy::cast_possible_truncation)]
    let sample = (value * 32768.0).round().clamp(-32768.0, 32767.0) as i16;
    sample
}

/// A packet of the loopback's floats, into frames.
fn take_floats<S: FnMut(&[u8])>(frames: &mut Frames<S>, bytes: &[u8]) {
    let mut out = [0u8; CHUNK * 2];
    for chunk in bytes.chunks(CHUNK * FLOAT_SAMPLE) {
        let mut written = 0usize;
        for (sample, slot) in chunk
            .chunks_exact(FLOAT_SAMPLE)
            .zip(out.chunks_exact_mut(2))
        {
            let value = f32::from_le_bytes(sample.try_into().unwrap_or([0; FLOAT_SAMPLE]));
            slot.copy_from_slice(&to_sixteen(value).to_le_bytes());
            written += 2;
        }
        frames.take(out.get(..written).unwrap_or_default());
    }
}

/// `count` frames of silence, into frames.
fn take_silence<S: FnMut(&[u8])>(frames: &mut Frames<S>, count: usize) {
    static ZEROS: [u8; CHUNK * 2] = [0; CHUNK * 2];
    let mut left = count * PCM_FRAME;
    while left > 0 {
        let now = left.min(ZEROS.len());
        frames.take(ZEROS.get(..now).unwrap_or_default());
        left -= now;
    }
}

/// What the system's notification threads tell the capture's.
#[derive(Debug)]
struct Signal {
    default_changed: AtomicBool,
    /// An output came, went or changed its state.
    outputs_changed: AtomicBool,
    /// A session began or started on an output whose sessions are watched.
    sessions_changed: AtomicBool,
    event: Event,
}

impl Signal {
    fn new() -> Result<Self, Error> {
        Ok(Self {
            default_changed: AtomicBool::new(false),
            outputs_changed: AtomicBool::new(false),
            sessions_changed: AtomicBool::new(false),
            event: Event::new(false)?,
        })
    }
}

/// The notification client: the one interface this side implements.
///
/// **Its layout is the interface's**: a pointer to its table first, so the
/// system calls through it as through any other.
#[repr(C)]
struct Notifier {
    table: *const IMMNotificationClientVtbl,
    refs: AtomicU32,
    signal: Arc<Signal>,
}

static NOTIFIER: IMMNotificationClientVtbl = IMMNotificationClientVtbl {
    QueryInterface: Some(notifier_query),
    AddRef: Some(notifier_add_ref),
    Release: Some(notifier_release),
    OnDeviceStateChanged: Some(notifier_state),
    OnDeviceAdded: Some(notifier_device),
    OnDeviceRemoved: Some(notifier_device),
    OnDefaultDeviceChanged: Some(notifier_default),
    OnPropertyValueChanged: Some(notifier_property),
};

fn same(a: &GUID, b: &GUID) -> bool {
    a.Data1 == b.Data1 && a.Data2 == b.Data2 && a.Data3 == b.Data3 && a.Data4 == b.Data4
}

unsafe extern "C" fn notifier_query(
    this: *mut IMMNotificationClient,
    iid: *const IID,
    out: *mut *mut c_void,
) -> HRESULT {
    if out.is_null() || iid.is_null() {
        return E_NOINTERFACE;
    }
    // SAFETY: the system passes a live identifier and a writable slot.
    unsafe {
        let asked = &*iid;
        if same(asked, &IID_IUnknown) || same(asked, &IID_IMMNotificationClient) {
            notifier_add_ref(this);
            *out = this.cast();
            S_OK
        } else {
            *out = ptr::null_mut();
            E_NOINTERFACE
        }
    }
}

unsafe extern "C" fn notifier_add_ref(this: *mut IMMNotificationClient) -> ULONG {
    // SAFETY: `this` is a live notifier; a reference is being taken on it.
    let notifier = unsafe { &*this.cast::<Notifier>() };
    // A new reference needs no ordering: whoever passes it on synchronises.
    notifier
        .refs
        .fetch_add(1, Ordering::Relaxed)
        .wrapping_add(1)
}

unsafe extern "C" fn notifier_release(this: *mut IMMNotificationClient) -> ULONG {
    // SAFETY: `this` is a live notifier whose reference is being given back.
    let notifier = unsafe { &*this.cast::<Notifier>() };
    // Release, then an acquire fence before the free, so every use made under
    // another reference happens before it.
    let left = notifier
        .refs
        .fetch_sub(1, Ordering::Release)
        .wrapping_sub(1);
    if left == 0 {
        fence(Ordering::Acquire);
        // SAFETY: the last reference: nothing else reaches the box any more.
        drop(unsafe { Box::from_raw(this.cast::<Notifier>()) });
    }
    left
}

unsafe extern "C" fn notifier_state(
    this: *mut IMMNotificationClient,
    _id: LPCWSTR,
    _state: u32,
) -> HRESULT {
    // SAFETY: `this` is a live notifier for the call's duration.
    unsafe { outputs_moved(this) };
    S_OK
}

unsafe extern "C" fn notifier_device(this: *mut IMMNotificationClient, _id: LPCWSTR) -> HRESULT {
    // SAFETY: as above.
    unsafe { outputs_moved(this) };
    S_OK
}

/// An output came, went or changed: noted and the capture's thread woken.
///
/// # Safety
///
/// `this` is a live notifier for the call's duration.
unsafe fn outputs_moved(this: *mut IMMNotificationClient) {
    // SAFETY: the caller's contract.
    let notifier = unsafe { &*this.cast::<Notifier>() };
    notifier
        .signal
        .outputs_changed
        .store(true, Ordering::Release);
    notifier.signal.event.set();
}

unsafe extern "C" fn notifier_property(
    _this: *mut IMMNotificationClient,
    _id: LPCWSTR,
    _key: PROPERTYKEY,
) -> HRESULT {
    S_OK
}

/// A new default: noted and the capture's thread woken, nothing more. **It
/// runs on the system's own thread**, which must not be held up.
unsafe extern "C" fn notifier_default(
    this: *mut IMMNotificationClient,
    flow: EDataFlow,
    role: ERole,
    _id: LPCWSTR,
) -> HRESULT {
    if flow == eRender && role == eConsole {
        // SAFETY: `this` is a live notifier for the call's duration.
        let notifier = unsafe { &*this.cast::<Notifier>() };
        notifier
            .signal
            .default_changed
            .store(true, Ordering::Release);
        notifier.signal.event.set();
    }
    S_OK
}

/// The notification client, registered with the enumeration while it lives.
struct Registration<'a> {
    enumerator: &'a Com<IMMDeviceEnumerator>,
    client: *mut IMMNotificationClient,
}

impl<'a> Registration<'a> {
    fn new(
        enumerator: &'a Com<IMMDeviceEnumerator>,
        signal: &Arc<Signal>,
    ) -> Result<Self, HRESULT> {
        let client = Box::into_raw(Box::new(Notifier {
            table: &raw const NOTIFIER,
            refs: AtomicU32::new(1),
            signal: Arc::clone(signal),
        }))
        .cast::<IMMNotificationClient>();
        // SAFETY: a live enumeration and a live client, whose reference this
        // value holds until its drop.
        let hr = unsafe {
            com!(
                enumerator.as_ptr(),
                RegisterEndpointNotificationCallback,
                client
            )
        };
        if hr < 0 {
            // SAFETY: this side's own reference, given back once.
            unsafe { notifier_release(client) };
            return Err(hr);
        }
        Ok(Self { enumerator, client })
    }
}

impl Drop for Registration<'_> {
    fn drop(&mut self) {
        // SAFETY: registered in `new`; unregistered once, then this side's
        // reference is given back.
        unsafe {
            com!(
                self.enumerator.as_ptr(),
                UnregisterEndpointNotificationCallback,
                self.client
            );
            notifier_release(self.client);
        }
    }
}

/// Whether an output applies its own mute, which is what decides whether
/// muting it reaches its loopback.
///
/// **The loopback hears what the system's engine applies and nothing the
/// device applies** (measured), so only a mute the device performs leaves the
/// guests their sound.
fn mutes_itself(hardware: u32) -> bool {
    hardware & ENDPOINT_HARDWARE_SUPPORT_MUTE != 0
}

/// Speakers this host silenced, and whether to put them back.
#[derive(Debug, Default)]
struct LocalMute {
    applied: bool,
}

/// Silence the speakers behind the output being read, where that leaves the
/// guests their sound.
///
/// **Only what this host did is remembered**, so only that is ever undone.
fn mute_local(session: &Session) -> LocalMute {
    mute_speakers(session.volume.as_ref(), &session.name)
}

/// Silence an output's speakers where its mute is the device's own.
fn mute_speakers(volume: Option<&Com<IAudioEndpointVolume>>, name: &str) -> LocalMute {
    let Some(volume) = volume else {
        return LocalMute::default();
    };
    let mut hardware: u32 = 0;
    // SAFETY: a live volume and a local out-parameter.
    let hr = unsafe { com!(volume.as_ptr(), QueryHardwareSupport, &raw mut hardware) };
    if hr < 0 || !mutes_itself(hardware) {
        // **An output whose mute reaches the loopback is left alone**: the
        // engine applies it to the mix the guests are sent, so silencing the
        // speakers would silence every guest.
        lowlat_common::log_warn!(
            "audio: the speakers cannot be silenced without silencing the guests, output={name}, left alone"
        );
        return LocalMute::default();
    }
    let mut muted: i32 = 0;
    // SAFETY: as above.
    let hr = unsafe { com!(volume.as_ptr(), GetMute, &raw mut muted) };
    if hr < 0 || muted != 0 {
        // **Already muted, by somebody who is not us**: left, and nothing is
        // recorded, so nothing is undone later.
        return LocalMute::default();
    }
    // SAFETY: as above; no event context.
    let hr = unsafe { com!(volume.as_ptr(), SetMute, 1, ptr::null()) };
    if hr < 0 {
        return LocalMute::default();
    }
    lowlat_common::log_info!("audio: the speakers are silenced, output={name}");
    LocalMute { applied: true }
}

/// Put the speakers back, if this host is what silenced them.
///
/// **Restore, never unmute**: still muted, and by us. Somebody who unmuted
/// during the session is not muted again, and nobody's own mute is undone.
fn restore_local(session: &Session, state: &LocalMute) {
    restore_speakers(session.volume.as_ref(), &session.name, state);
}

/// Put an output's speakers back, if this host silenced them.
fn restore_speakers(volume: Option<&Com<IAudioEndpointVolume>>, name: &str, state: &LocalMute) {
    if !state.applied {
        return;
    }
    let Some(volume) = volume else {
        return;
    };
    let mut muted: i32 = 0;
    // SAFETY: a live volume and a local out-parameter; no event context.
    unsafe {
        if com!(volume.as_ptr(), GetMute, &raw mut muted) >= 0
            && muted != 0
            && com!(volume.as_ptr(), SetMute, 0, ptr::null()) >= 0
        {
            lowlat_common::log_info!("audio: the speakers are back, output={name}");
        }
    }
}

/// Record which output the loop is on, for whoever asks.
fn publish(session: &Session, device: &Mutex<String>) {
    match device.lock() {
        Ok(mut held) => held.clone_from(&session.id),
        Err(held) => held.into_inner().clone_from(&session.id),
    }
}

pub(super) fn run<S>(
    config: &Settings,
    stop: &Stop,
    device: &Mutex<String>,
    report: &std::sync::mpsc::Sender<Result<(), Error>>,
    sink: S,
) where
    S: FnMut(&[u8]),
{
    if let Some(server) = config.server.as_deref().filter(|server| !server.is_empty()) {
        lowlat_common::log_warn!(
            "audio: a sound server's socket means nothing on Windows, ignored, server={server}"
        );
    }
    let mut frames = Frames::new(sink);
    // **One loop for each way of capturing, and the asked-for one runs**:
    // each hands back when the other is asked for, so an application's sound
    // and the output's mix follow each other live on the same thread, into
    // the same frames.
    loop {
        let ended = match config.application() {
            Some(held) => application::serve(config, &held, stop, device, report, &mut frames),
            None => serve(config, stop, device, report, &mut frames),
        };
        match ended {
            Ok(Ended::Moved) => {}
            Ok(Ended::Stopped) => return,
            Err(error) => {
                lowlat_common::log_warn!("audio: capture ended, {error}");
                return;
            }
        }
    }
}

/// Why a capture's loop handed back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ended {
    /// Its owner asked it to stop.
    Stopped,
    /// The other way of capturing was asked for.
    Moved,
}

/// Everything the loop holds, from the enumeration to the session.
struct Loop<'a> {
    enumerator: &'a Com<IMMDeviceEnumerator>,
    wake: &'a Event,
    signal: &'a Signal,
    session: Session,
    /// Whether the default is what is read, as against a named output.
    following: bool,
    muted: LocalMute,
}

fn serve<S: FnMut(&[u8])>(
    config: &Settings,
    stop: &Stop,
    device: &Mutex<String>,
    report: &std::sync::mpsc::Sender<Result<(), Error>>,
    frames: &mut Frames<S>,
) -> Result<Ended, Error> {
    let opened = (|| -> Result<_, Error> {
        let enumerator = enumerator()?;
        let wake = Event::new(false)?;
        let signal = Arc::new(Signal::new()?);
        let session = Session::open(&enumerator, config.wanted_device().as_deref(), &wake)?;
        Ok((enumerator, wake, signal, session))
    })();
    let (enumerator, wake, signal, session) = match opened {
        Ok(opened) => opened,
        Err(error) => {
            let _ = report.send(Err(error));
            return Err(error);
        }
    };
    // **Following needs the system's word**; without it the capture still
    // runs, on the output it opened.
    let registration = match Registration::new(&enumerator, &signal) {
        Ok(registration) => Some(registration),
        Err(hr) => {
            lowlat_common::log_warn!(
                "audio: the default output will not be followed, error={}",
                Error::Refused(hr)
            );
            None
        }
    };
    publish(&session, device);
    lowlat_common::log_info!("audio: capturing {}, id={}", session.name, session.id);
    let _ = report.send(Ok(()));

    let mut state = Loop {
        enumerator: &enumerator,
        wake: &wake,
        signal: &signal,
        muted: if config.live().mute_local {
            mute_local(&session)
        } else {
            LocalMute::default()
        },
        following: config.wanted_device().is_none(),
        session,
    };
    // **However the loop leaves, the speakers come back.**
    let outcome = serve_loop(config, stop, device, frames, &mut state);
    restore_local(&state.session, &state.muted);
    drop(state);
    drop(registration);
    outcome
}

fn serve_loop<S: FnMut(&[u8])>(
    config: &Settings,
    stop: &Stop,
    device: &Mutex<String>,
    frames: &mut Frames<S>,
    state: &mut Loop<'_>,
) -> Result<Ended, Error> {
    let mut seen = config.wanted.generation();
    let handles = [stop.event.0, state.wake.0, state.signal.event.0];
    loop {
        // SAFETY: three live events: the stop's, the loopback's and the
        // notification's, each outliving this loop.
        unsafe {
            WaitForMultipleObjects(3, handles.as_ptr(), 0, WAIT_CAP_MS);
        }
        if stop.asked() {
            return Ok(Ended::Stopped);
        }

        if let Err(error) = state.session.drain(frames) {
            // **A lost output is taken again at once if it can be**; one that
            // cannot -- held by a program in exclusive mode, or gone with no
            // default to take its place -- ends the loop, and the owner asks
            // again a couple of seconds later.
            restore_local(&state.session, &state.muted);
            state.muted = LocalMute::default();
            let wanted = config.wanted_device();
            let fresh = match (state.following, wanted.as_deref()) {
                (false, Some(named)) => {
                    match Session::open(state.enumerator, Some(named), state.wake) {
                        Err(Error::NoSuchDevice) => {
                            // **A named output that went away is replaced by the
                            // default**, as a sound server moves a stream.
                            let fresh = Session::open(state.enumerator, None, state.wake);
                            if let Ok(fresh) = fresh.as_ref() {
                                lowlat_common::log_warn!(
                                    "audio: {named} is gone, capturing the default output {}",
                                    fresh.name
                                );
                                state.following = true;
                            }
                            fresh
                        }
                        other => other,
                    }
                }
                _ => Session::open(state.enumerator, None, state.wake),
            };
            let Ok(fresh) = fresh else {
                return Err(error);
            };
            lowlat_common::log_info!(
                "audio: the output was lost and taken again, now capturing {}, error={error}",
                fresh.name
            );
            state.session = fresh;
            publish(&state.session, device);
            if config.live().mute_local {
                state.muted = mute_local(&state.session);
            }
        }

        // **What an owner asked for, acted on between packets.** The streams
        // belong to this thread, so a change is a request rather than
        // something another thread may apply.
        let now = config.wanted.generation();
        if now != seen {
            seen = now;
            // **An application's own sound asked for ends this loop**, the
            // speakers put back on the way out by the caller.
            if config.application().is_some() {
                return Ok(Ended::Moved);
            }
            let live = config.live();
            let wanted = live.device.as_deref().filter(|name| !name.is_empty());
            let moving = match wanted {
                Some(named) => named != state.session.id,
                None => !state.following,
            };
            if moving {
                restore_local(&state.session, &state.muted);
                state.muted = LocalMute::default();
                // **A device that will not open keeps the one that is
                // working**: taking the sound from every guest to honour a
                // change that failed is the worse answer.
                match Session::open(state.enumerator, wanted, state.wake) {
                    Ok(fresh) => {
                        state.session = fresh;
                        state.following = wanted.is_none();
                        publish(&state.session, device);
                        lowlat_common::log_info!(
                            "audio: now capturing {}, id={}",
                            state.session.name,
                            state.session.id
                        );
                    }
                    Err(error) => lowlat_common::log_warn!(
                        "audio: the device asked for would not open, keeping {}, error={error}",
                        state.session.name
                    ),
                }
            }
            match (live.mute_local, state.muted.applied) {
                (true, false) => state.muted = mute_local(&state.session),
                (false, true) => {
                    restore_local(&state.session, &state.muted);
                    state.muted = LocalMute::default();
                }
                _ => {}
            }
        }

        // A new default only concerns a capture that is following one.
        if state.signal.default_changed.swap(false, Ordering::AcqRel) && state.following {
            let current = default_output(state.enumerator).and_then(|output| output_id(&output));
            if current.as_deref().is_ok_and(|id| id != state.session.id) {
                // **The speakers move with the device.** Left muted, the old
                // output would stay silent with nobody streaming from it.
                restore_local(&state.session, &state.muted);
                state.muted = LocalMute::default();
                match Session::open(state.enumerator, None, state.wake) {
                    Ok(fresh) => {
                        state.session = fresh;
                        publish(&state.session, device);
                        lowlat_common::log_info!(
                            "audio: the default output is now {}, id={}",
                            state.session.name,
                            state.session.id
                        );
                    }
                    Err(error) => lowlat_common::log_warn!(
                        "audio: the new default output would not open, keeping {}, error={error}",
                        state.session.name
                    ),
                }
                if config.live().mute_local {
                    state.muted = mute_local(&state.session);
                }
            }
        }
    }
}

/// Every output on this machine that a host could capture.
///
/// **The identity is the output itself**, which the system reads back through
/// a loopback of it; the name is the one a person sees. Answers before a host
/// is started and without disturbing one that is running. `_server` names a
/// Linux sound server and means nothing here.
pub fn outputs(_server: Option<&str>) -> Result<Vec<Output>, Error> {
    prepare()?;
    let enumerator = enumerator()?;
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
    if hr < 0 {
        return Err(Error::Refused(hr));
    }
    // SAFETY: as in `enumerator`.
    let collection = unsafe { Com::from_raw(collection) }.ok_or(Error::Refused(hr))?;
    let mut count: u32 = 0;
    // SAFETY: a live collection and a local out-parameter.
    let hr = unsafe { com!(collection.as_ptr(), GetCount, &raw mut count) };
    if hr < 0 {
        return Err(Error::Refused(hr));
    }
    let mut found = Vec::with_capacity(usize::try_from(count).unwrap_or(0));
    for index in 0..count {
        let mut output: *mut IMMDevice = ptr::null_mut();
        // SAFETY: an index the collection counted and a local out-parameter.
        let hr = unsafe { com!(collection.as_ptr(), Item, index, &raw mut output) };
        if hr < 0 {
            continue;
        }
        // SAFETY: as in `enumerator`.
        let Some(output) = (unsafe { Com::from_raw(output) }) else {
            continue;
        };
        let Ok(id) = output_id(&output) else {
            continue;
        };
        let name = friendly_name(&output).unwrap_or_else(|| id.clone());
        found.push(Output { id, name });
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FRAME_BYTES;

    /// **The engine's floor reads as silence**: a program playing zeros reaches
    /// the loopback with samples up to 1.4e-9 (measured), which the system's
    /// own sixteen-bit conversion dithers into the lowest bit.
    #[test]
    fn rounding_brings_the_engines_floor_to_silence() {
        assert_eq!(to_sixteen(1.4e-9), 0);
        assert_eq!(to_sixteen(-1.4e-9), 0);
        assert_eq!(to_sixteen(0.0), 0);
        // Half a step either side of the first.
        assert_eq!(to_sixteen(0.49 / 32768.0), 0);
        assert_eq!(to_sixteen(0.51 / 32768.0), 1);
        assert_eq!(to_sixteen(-0.51 / 32768.0), -1);
    }

    /// Full scale saturates rather than wrapping, and nothing that is not a
    /// number becomes noise.
    #[test]
    fn full_scale_saturates() {
        assert_eq!(to_sixteen(0.5), 16384);
        assert_eq!(to_sixteen(1.0), 32767);
        assert_eq!(to_sixteen(-1.0), -32768);
        assert_eq!(to_sixteen(1.5), 32767);
        assert_eq!(to_sixteen(-1.5), -32768);
        assert_eq!(to_sixteen(f32::NAN), 0);
        assert_eq!(to_sixteen(f32::INFINITY), 32767);
    }

    /// **A flagged packet is silence of its length**: two engine periods of
    /// it are one whole frame of zeros.
    #[test]
    fn a_flagged_packet_is_silence_of_its_length() {
        let mut seen: Vec<Vec<u8>> = Vec::new();
        {
            let mut frames = Frames::new(|frame: &[u8]| seen.push(frame.to_vec()));
            take_silence(&mut frames, crate::FRAME / 2);
            take_silence(&mut frames, crate::FRAME / 2);
        }
        assert_eq!(seen.len(), 1, "two half frames of silence are one frame");
        assert_eq!(seen[0].len(), FRAME_BYTES);
        assert!(crate::encode::is_silent(&seen[0]));
    }

    /// A packet of floats comes out as sixteen-bit samples, interleaved as it
    /// went in, little endian, whatever the chunking inside.
    #[test]
    fn floats_become_sixteen_bits_in_order() {
        // More than one chunk, and a frame and a half of it.
        let count = crate::FRAME + crate::FRAME / 2;
        let mut bytes = Vec::with_capacity(count * FLOAT_FRAME);
        let mut expected = Vec::with_capacity(count * PCM_FRAME);
        for i in 0..count {
            let step = f32::from(u16::try_from(i % 1000).unwrap());
            let left = step / 1000.0 - 0.5;
            let right = -left / 2.0;
            for value in [left, right] {
                bytes.extend_from_slice(&value.to_le_bytes());
                expected.extend_from_slice(&to_sixteen(value).to_le_bytes());
            }
        }
        let mut seen: Vec<u8> = Vec::new();
        {
            let mut frames = Frames::new(|frame: &[u8]| seen.extend_from_slice(frame));
            take_floats(&mut frames, &bytes);
        }
        assert_eq!(seen.len(), FRAME_BYTES, "one whole frame out, half held");
        assert_eq!(seen, expected[..FRAME_BYTES]);
    }

    /// **Only a mute the device performs** leaves the guests their sound.
    #[test]
    fn the_mute_is_the_devices_alone() {
        assert!(mutes_itself(0x2), "mute in the device");
        assert!(mutes_itself(0x3), "volume and mute in the device");
        assert!(!mutes_itself(0x1), "volume alone in the device");
        assert!(!mutes_itself(0x0), "both in the engine");
    }

    /// The per-packet path allocates nothing: what the loop runs for every
    /// packet, flagged or not, is the conversion and the reassembly.
    #[test]
    fn a_packet_allocates_nothing() {
        let bytes: Vec<u8> = (0..480 * 2)
            .flat_map(|i| (f32::from(u16::try_from(i).unwrap()) / 2000.0).to_le_bytes())
            .collect();
        let mut handed = 0usize;
        let mut frames = Frames::new(|frame: &[u8]| handed += frame.len());
        // The first frame outside the assertion, as the encoder's test does.
        take_floats(&mut frames, &bytes);
        lowlat_common::alloc_counter::assert_no_alloc(|| {
            for _ in 0..8 {
                take_floats(&mut frames, &bytes);
                take_silence(&mut frames, 480);
            }
        });
        drop(frames);
        // Half a frame first, then a whole one each turn; the last half held.
        assert_eq!(handed, FRAME_BYTES * 8);
    }

    // On the machine's own default output, by hand and one at a time:
    //   cargo test -p lowlat-audio --lib -- --ignored --test-threads=1
    // Nothing is audible: a tone plays only while the speakers are muted in
    // the device, which the loopback does not hear (measured), and every mute
    // a test changes is put back.

    use std::sync::Mutex as Shared;
    use std::time::{Duration, Instant};

    use crate::encode::is_silent;
    use crate::{Capture, Config};

    /// What a capture handed on: whole frames, and how many were not silence.
    #[derive(Default)]
    struct Seen {
        frames: usize,
        loud: usize,
        peak: [i16; 2],
    }

    /// A capture of the default output whose frames are counted from `after`.
    fn start(config: Config, after: Instant) -> (Capture, Arc<Shared<Seen>>) {
        let seen = Arc::new(Shared::new(Seen::default()));
        let sink = {
            let seen = Arc::clone(&seen);
            move |frame: &[u8]| {
                if Instant::now() < after {
                    return;
                }
                let mut seen = seen.lock().unwrap();
                seen.frames += 1;
                if !is_silent(frame) {
                    seen.loud += 1;
                }
                for (index, sample) in frame.chunks_exact(2).enumerate() {
                    let value = i16::from_le_bytes([sample[0], sample[1]]).saturating_abs();
                    let channel = &mut seen.peak[index % 2];
                    *channel = (*channel).max(value);
                }
            }
        };
        (Capture::open(config, sink).expect("a capture"), seen)
    }

    /// A stream of this test's own on the default output, fed from the test.
    struct Player {
        client: Com<IAudioClient>,
        render: Com<IAudioRenderClient>,
        frames: u32,
        phase: f64,
    }

    impl Player {
        fn open(output: &Com<IMMDevice>) -> Self {
            let client = activate::<IAudioClient>(output, &IID_IAudioClient).unwrap();
            // Its own format, floating point whatever the capture asks for:
            // `play` writes floats.
            let format = WAVEFORMATEX {
                wFormatTag: FLOAT_TAG,
                nChannels: CHANNELS,
                nSamplesPerSec: 48000,
                nAvgBytesPerSec: 48000 * 8,
                nBlockAlign: 8,
                wBitsPerSample: 32,
                cbSize: 0,
            };
            unsafe {
                let hr = com!(
                    client.as_ptr(),
                    Initialize,
                    AUDCLNT_SHAREMODE_SHARED,
                    CONVERT,
                    1_000_000,
                    0,
                    &raw const format,
                    ptr::null()
                );
                assert!(hr >= 0, "the player's stream: {hr:#x}");
            }
            let mut frames = 0u32;
            unsafe { com!(client.as_ptr(), GetBufferSize, &raw mut frames) };
            let render = service::<IAudioRenderClient>(&client, &IID_IAudioRenderClient).unwrap();
            unsafe { com!(client.as_ptr(), Start) };
            Self {
                client,
                render,
                frames,
                phase: 0.0,
            }
        }

        /// Keep the stream full for `ms`, `amplitude` at 1 kHz on the left and
        /// half of it on the right; zero amplitude writes explicit zeros, not
        /// a buffer flagged silent.
        fn play(&mut self, amplitude: f64, ms: u64) {
            let end = Instant::now() + Duration::from_millis(ms);
            while Instant::now() < end {
                let mut padding = 0u32;
                unsafe { com!(self.client.as_ptr(), GetCurrentPadding, &raw mut padding) };
                let room = self.frames - padding;
                if room > 0 {
                    let mut data: *mut u8 = ptr::null_mut();
                    unsafe {
                        assert!(com!(self.render.as_ptr(), GetBuffer, room, &raw mut data) >= 0);
                        let samples = core::slice::from_raw_parts_mut(
                            data.cast::<f32>(),
                            usize::try_from(room).unwrap() * 2,
                        );
                        for pair in samples.chunks_exact_mut(2) {
                            #[allow(clippy::cast_possible_truncation)]
                            let value = (amplitude * self.phase.sin()) as f32;
                            pair[0] = value;
                            pair[1] = value / 2.0;
                            self.phase += 2.0 * core::f64::consts::PI * 1000.0 / 48000.0;
                        }
                        com!(self.render.as_ptr(), ReleaseBuffer, room, 0);
                    }
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }

    impl Drop for Player {
        fn drop(&mut self) {
            unsafe { com!(self.client.as_ptr(), Stop) };
        }
    }

    /// The default output and its volume, with what the machine had.
    fn default_volume() -> (Com<IMMDevice>, Com<IAudioEndpointVolume>, u32, i32) {
        prepare().unwrap();
        let output = default_output(&enumerator().unwrap()).expect("a default output");
        let volume = activate::<IAudioEndpointVolume>(&output, &IID_IAudioEndpointVolume).unwrap();
        let (mut hardware, mut muted) = (0u32, 0i32);
        unsafe {
            com!(volume.as_ptr(), QueryHardwareSupport, &raw mut hardware);
            com!(volume.as_ptr(), GetMute, &raw mut muted);
        }
        (output, volume, hardware, muted)
    }

    fn set_mute(volume: &Com<IAudioEndpointVolume>, muted: i32) {
        unsafe { com!(volume.as_ptr(), SetMute, muted, ptr::null()) };
    }

    fn muted(volume: &Com<IAudioEndpointVolume>) -> i32 {
        let mut muted = 0i32;
        unsafe { com!(volume.as_ptr(), GetMute, &raw mut muted) };
        muted
    }

    /// **A quiet room keeps the clock**: a frame every 20 ms with nothing
    /// playing. Run with nothing playing; without the silent stream the same
    /// run hands on nothing.
    #[test]
    #[ignore = "reads the default output; run by hand"]
    fn a_quiet_spell_keeps_the_clock() {
        let (capture, seen) = start(
            Config::default(),
            Instant::now() + Duration::from_millis(500),
        );
        std::thread::sleep(Duration::from_millis(3500));
        drop(capture);
        let frames = seen.lock().unwrap().frames;
        assert!(
            (145..=155).contains(&frames),
            "{frames} frames in three seconds, expected 150"
        );
    }

    /// **A program playing digital silence reads as silence**: explicit zeros,
    /// not flagged, which the system's own sixteen-bit conversion would dither.
    #[test]
    #[ignore = "plays zeros on the default output; run by hand"]
    fn a_program_playing_silence_reads_as_silence() {
        prepare().unwrap();
        let output = default_output(&enumerator().unwrap()).unwrap();
        let mut player = Player::open(&output);
        let (capture, seen) = start(
            Config::default(),
            Instant::now() + Duration::from_millis(500),
        );
        player.play(0.0, 2500);
        drop(capture);
        let seen = seen.lock().unwrap();
        assert!(seen.frames >= 95, "only {} frames", seen.frames);
        assert_eq!(
            seen.loud, 0,
            "{} of {} frames were not silence",
            seen.loud, seen.frames
        );
    }

    /// A -30 dBFS tone comes back at its level and in its channels: the
    /// conversion's scale and order, through the system.
    #[test]
    #[ignore = "plays a tone on the default output with its speakers muted; run by hand"]
    fn a_tone_comes_back_at_its_level() {
        let (output, volume, hardware, was) = default_volume();
        if !mutes_itself(hardware) {
            eprintln!("the default output does not mute in the device; the tone would be heard");
            return;
        }
        set_mute(&volume, 1);
        let mut player = Player::open(&output);
        let (capture, seen) = start(
            Config::default(),
            Instant::now() + Duration::from_millis(500),
        );
        player.play(0.03, 1500);
        drop(capture);
        drop(player);
        set_mute(&volume, was);
        let seen = seen.lock().unwrap();
        // 0.03 and 0.015 of full scale; the samples land on the peak exactly.
        assert!(
            (978..=988).contains(&seen.peak[0]),
            "left peak {}",
            seen.peak[0]
        );
        assert!(
            (487..=497).contains(&seen.peak[1]),
            "right peak {}",
            seen.peak[1]
        );
    }

    /// **A program in exclusive mode is a lost device**: the capture ends, and
    /// once the program lets go the output opens again.
    #[test]
    #[ignore = "takes the default output in exclusive mode for a moment; run by hand"]
    fn an_exclusive_program_ends_the_capture_and_it_opens_again() {
        prepare().unwrap();
        let output = default_output(&enumerator().unwrap()).unwrap();
        let (capture, _) = start(Config::default(), Instant::now());
        std::thread::sleep(Duration::from_millis(300));
        assert!(capture.alive());

        let exclusive = activate::<IAudioClient>(&output, &IID_IAudioClient).unwrap();
        let format = WAVEFORMATEX {
            wFormatTag: 1,
            nChannels: 2,
            nSamplesPerSec: 48000,
            nAvgBytesPerSec: 48000 * 4,
            nBlockAlign: 4,
            wBitsPerSample: 16,
            cbSize: 0,
        };
        let (mut period, mut minimum) = (0i64, 0i64);
        unsafe {
            if com!(
                exclusive.as_ptr(),
                IsFormatSupported,
                1,
                &raw const format,
                ptr::null_mut()
            ) != 0
            {
                eprintln!("the output takes no 16-bit stereo in exclusive mode here");
                return;
            }
            com!(
                exclusive.as_ptr(),
                GetDevicePeriod,
                &raw mut period,
                &raw mut minimum
            );
            let hr = com!(
                exclusive.as_ptr(),
                Initialize,
                1,
                0,
                period * 4,
                0,
                &raw const format,
                ptr::null()
            );
            assert!(hr >= 0, "exclusive open: {hr:#x}");
            let mut frames = 0u32;
            com!(exclusive.as_ptr(), GetBufferSize, &raw mut frames);
            let render =
                service::<IAudioRenderClient>(&exclusive, &IID_IAudioRenderClient).unwrap();
            let mut data: *mut u8 = ptr::null_mut();
            com!(render.as_ptr(), GetBuffer, frames, &raw mut data);
            com!(render.as_ptr(), ReleaseBuffer, frames, SILENT);
            assert!(com!(exclusive.as_ptr(), Start) >= 0);
        }
        let gone = Instant::now() + Duration::from_secs(1);
        while capture.alive() && Instant::now() < gone {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !capture.alive(),
            "the capture outlived an exclusive program"
        );
        unsafe { com!(exclusive.as_ptr(), Stop) };
        drop(exclusive);
        drop(capture);

        let (again, seen) = start(Config::default(), Instant::now());
        std::thread::sleep(Duration::from_millis(500));
        drop(again);
        assert!(
            seen.lock().unwrap().frames > 0,
            "nothing after the program let go"
        );
    }

    /// **Restore, never unmute**, and only where the device mutes itself: the
    /// speakers are silenced while the capture runs and back after it.
    #[test]
    #[ignore = "mutes the default output's speakers for a moment; run by hand"]
    fn the_local_mute_is_put_back() {
        let (_output, volume, hardware, was) = default_volume();
        if !mutes_itself(hardware) || was != 0 {
            eprintln!("the default output does not mute in the device, or is muted already");
            return;
        }
        let config = Config {
            server: None,
            wanted: Arc::new(crate::Wanted::new(crate::Live {
                device: None,
                mute_local: true,
            })),
        };
        let (capture, _) = start(config, Instant::now());
        std::thread::sleep(Duration::from_millis(300));
        let during = muted(&volume);
        drop(capture);
        let after = muted(&volume);
        set_mute(&volume, was);
        assert_eq!(during, 1, "the speakers were not silenced");
        assert_eq!(after, 0, "the speakers were not put back");
    }

    /// The outputs listed include the default, by the identity a capture
    /// publishes, with a name a person can read.
    #[test]
    #[ignore = "reads the machine's outputs; run by hand"]
    fn the_outputs_name_the_default() {
        let (output, ..) = default_volume();
        let id = output_id(&output).unwrap();
        let listed = outputs(None).unwrap();
        let found = listed
            .iter()
            .find(|listed| listed.id == id)
            .expect("the default listed");
        assert!(!found.name.is_empty());
        let (capture, _) = start(Config::default(), Instant::now());
        assert_eq!(
            capture.device(),
            id,
            "the capture publishes another identity"
        );
    }
}
