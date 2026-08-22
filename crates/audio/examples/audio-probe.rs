//! Sound capture diagnostic. Reads the desktop's own output and reports what
//! the read cost.
//!
//! Run it on the machine under test, as the user or as the service:
//!
//!   audio-probe [device] [server] [seconds]
//!
//! `device` defaults to `@DEFAULT_MONITOR@`, which the sound server resolves
//! to whichever output is currently the default one. `server` defaults to
//! whatever the environment names, and is given explicitly when the reader is
//! outside the session that owns the sound server:
//!
//!   sudo audio-probe @DEFAULT_MONITOR@ unix:/run/user/1000/pulse/native
//!
//! A dash in place of the server keeps the environment's own.
//!
//! Play something audible while it runs, or the content line says nothing.
//!
//! **What it was written to answer, and did**, on a workstation whose sound
//! server belongs to a logged-in session:
//!
//!   - **a service is admitted to that session's socket**, with or without the
//!     session's own authentication cookie, so audio capture needs no helper
//!   - **the source is a clock**: fragments arrive on the server's graph
//!     period, p50 21.33 ms against the 20 ms asked for, p99 21.60, max 25.75
//!   - **the rate is exact even though the spacing is not** -- 300 seconds of
//!     reading came out 10 ms short of the wall clock, and the same figure
//!     appears at 10 and 60 seconds, so it is the connect and not a drift
//!   - **silence keeps flowing as zeros** rather than stopping, so skipping it
//!     is this host's decision and not the platform's
//!
//! **The cadence is the measurement that decided the design.** A source that
//! delivers when it is ready is a clock; anything read on our own timer drifts
//! against the sound device and has to be resampled to hide it.
//!
//! **Two things it also found, both by accident, and both now rules:** a device
//! name that does not resolve is **substituted rather than refused**, so a
//! requested one is checked against the enumeration first; and a capture
//! **does not follow the default output**, which is resolved once when the
//! stream connects, so following it is this host's work.
//!
//! **This is a probe and not the capture path.** The interface it uses cannot
//! say which source it landed on, cannot be told to move to another, and its
//! read cannot be cancelled -- which is why the host uses the fuller one. What
//! is proven here is the access, the clock and the format, and those do not
//! change between the two.

use std::ffi::{CStr, CString, c_char, c_int, c_void};

use lowlat_audio::{CHANNELS, FRAME, FRAME_BYTES, SAMPLE_RATE};
use lowlat_common::clock::{Time, elapsed_ms};
use lowlat_common::dynlib::Library;

/// How long to read for when nobody says, in frames of 20 ms.
const FRAMES: usize = 10 * 50;

/// Sixteen-bit little endian, which is `PA_SAMPLE_S16LE`.
const SAMPLE_S16LE: c_int = 3;
/// `PA_STREAM_RECORD`.
const DIRECTION_RECORD: c_int = 2;
/// The server's own default, for every buffer field except the one that
/// decides the cadence.
const DEFAULT: u32 = u32::MAX;

#[repr(C)]
#[derive(Debug)]
struct SampleSpec {
    format: c_int,
    rate: u32,
    channels: u8,
}

#[repr(C)]
#[derive(Debug)]
struct BufferAttr {
    maxlength: u32,
    tlength: u32,
    prebuf: u32,
    minreq: u32,
    /// **The one field that matters to a reader.** A read returns when this
    /// many bytes are available, so it is the frame size and the wakeup
    /// cadence at once.
    fragsize: u32,
}

type SimpleNew = unsafe extern "C" fn(
    *const c_char,
    *const c_char,
    c_int,
    *const c_char,
    *const c_char,
    *const SampleSpec,
    *const c_void,
    *const BufferAttr,
    *mut c_int,
) -> *mut c_void;
type SimpleRead = unsafe extern "C" fn(*mut c_void, *mut c_void, usize, *mut c_int) -> c_int;
type SimpleLatency = unsafe extern "C" fn(*mut c_void, *mut c_int) -> u64;
type SimpleFree = unsafe extern "C" fn(*mut c_void);
type StrError = unsafe extern "C" fn(c_int) -> *const c_char;

fn main() {
    let mut args = std::env::args().skip(1);
    let device = args
        .next()
        .unwrap_or_else(|| "@DEFAULT_MONITOR@".to_owned());
    // A dash stands for "whatever the environment names", so a duration can be
    // given without one.
    let server = args
        .next()
        .filter(|named| named != "-" && !named.is_empty());
    let frames = args
        .next()
        .and_then(|seconds| seconds.parse::<usize>().ok())
        .map_or(FRAMES, |seconds| seconds * 50);

    let simple = Library::open(c"libpulse-simple.so.0").expect("libpulse-simple.so.0 did not open");
    let pulse = Library::open(c"libpulse.so.0").expect("libpulse.so.0 did not open");
    // SAFETY: each signature is the library's own, transcribed from its
    // headers; the pointers borrow the two libraries, which outlive them here.
    let (new, read, latency, free, strerror): (
        SimpleNew,
        SimpleRead,
        SimpleLatency,
        SimpleFree,
        StrError,
    ) = unsafe {
        (
            simple.symbol(c"pa_simple_new").expect("pa_simple_new"),
            simple.symbol(c"pa_simple_read").expect("pa_simple_read"),
            simple
                .symbol(c"pa_simple_get_latency")
                .expect("pa_simple_get_latency"),
            simple.symbol(c"pa_simple_free").expect("pa_simple_free"),
            pulse.symbol(c"pa_strerror").expect("pa_strerror"),
        )
    };

    let say = |code: c_int| -> String {
        // SAFETY: the library returns a static string for any code.
        let text = unsafe { CStr::from_ptr(strerror(code)) };
        text.to_string_lossy().into_owned()
    };

    let spec = SampleSpec {
        format: SAMPLE_S16LE,
        rate: SAMPLE_RATE,
        channels: u8::try_from(CHANNELS).expect("a channel count"),
    };
    let attr = BufferAttr {
        maxlength: DEFAULT,
        tlength: DEFAULT,
        prebuf: DEFAULT,
        minreq: DEFAULT,
        fragsize: u32::try_from(FRAME_BYTES).expect("a frame fits"),
    };

    let device_c = CString::new(device.clone()).expect("a device name");
    let server_c = server.clone().map(|s| CString::new(s).expect("a server"));
    let mut error: c_int = 0;
    // SAFETY: every pointer is valid for the call and outlives it; a null
    // channel map asks for the default layout of `channels`.
    let stream = unsafe {
        new(
            server_c.as_ref().map_or(core::ptr::null(), |s| s.as_ptr()),
            c"lowlat-probe".as_ptr(),
            DIRECTION_RECORD,
            device_c.as_ptr(),
            c"desktop".as_ptr(),
            &spec,
            core::ptr::null(),
            &attr,
            &mut error,
        )
    };
    if stream.is_null() {
        println!(
            "open FAILED device={device} server={} error={} ({error})",
            server.unwrap_or_else(|| "<environment>".to_owned()),
            say(error)
        );
        return;
    }
    println!(
        "open ok device={device} server={} rate={SAMPLE_RATE} channels={CHANNELS} fragment={FRAME_BYTES}B",
        server.unwrap_or_else(|| "<environment>".to_owned())
    );

    // SAFETY: the stream is open until it is freed below.
    let reported = unsafe { latency(stream, &mut error) };
    println!("latency at open: {:.2} ms", reported as f64 / 1000.0);

    let mut buffer = vec![0u8; FRAME_BYTES];
    let mut gaps = Vec::with_capacity(frames);
    let mut sum_squares = 0.0f64;
    let mut peak = 0i32;
    let mut silent = 0usize;
    let began = Time::now();
    let mut previous = began;

    for _ in 0..frames {
        // SAFETY: the buffer holds exactly the fragment being asked for.
        let status = unsafe {
            read(
                stream,
                buffer.as_mut_ptr().cast::<c_void>(),
                FRAME_BYTES,
                &mut error,
            )
        };
        if status < 0 {
            println!("read FAILED error={} ({error})", say(error));
            break;
        }
        let now = Time::now();
        gaps.push(elapsed_ms(previous));
        previous = now;

        let mut loudest = 0i32;
        for chunk in buffer.chunks_exact(2) {
            let sample = i32::from(i16::from_le_bytes([chunk[0], chunk[1]]));
            sum_squares += f64::from(sample) * f64::from(sample);
            loudest = loudest.max(sample.abs());
        }
        peak = peak.max(loudest);
        if loudest == 0 {
            silent += 1;
        }
    }

    let elapsed = elapsed_ms(began);
    // SAFETY: as above.
    let closing = unsafe { latency(stream, &mut error) };
    // SAFETY: the stream came from `pa_simple_new` and is freed once.
    unsafe { free(stream) };

    gaps.sort_by(f64::total_cmp);
    // Percent rather than a fraction, so the index is integer arithmetic and
    // the rounding is not a float cast.
    let at = |percent: usize| -> f64 {
        let index = gaps.len().saturating_sub(1) * percent / 100;
        gaps.get(index).copied().unwrap_or(0.0)
    };
    let read_count = gaps.len();
    let captured_ms = (read_count * FRAME) as f64 * 1000.0 / f64::from(SAMPLE_RATE);
    let samples = (read_count * FRAME * CHANNELS) as f64;
    let rms = (sum_squares / samples.max(1.0)).sqrt();

    println!("reads: {read_count} of {frames}");
    println!(
        "cadence ms: min {:.2}  p50 {:.2}  p95 {:.2}  p99 {:.2}  max {:.2}",
        at(0),
        at(50),
        at(95),
        at(99),
        at(100)
    );
    println!(
        "clock: captured {captured_ms:.0} ms in {elapsed:.0} ms of wall time, \
         drift {:+.0} ms ({:+.1} ppm)",
        captured_ms - elapsed,
        (captured_ms - elapsed) / elapsed.max(1.0) * 1e6
    );
    println!("latency at close: {:.2} ms", closing as f64 / 1000.0);
    println!(
        "content: rms {rms:.0} peak {peak} silent fragments {silent} of {read_count}{}",
        if silent == read_count {
            "  <- nothing was playing, so this says nothing about the path"
        } else {
            ""
        }
    );
}
