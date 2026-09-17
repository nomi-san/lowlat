//! The HEVC reader over an arbitrary access unit, and then a second one, so
//! the picture buffer's state after the first is exercised too. Every unit
//! must be read or refused; nothing may panic and nothing may index past a
//! list. The device is never reached: the reader is the whole surface.
#![no_main]

use libfuzzer_sys::fuzz_target;
use lowlat_decode::hevc::Stream;

fuzz_target!(|data: &[u8]| {
    let mut stream = Stream::new();
    // The input is one or two units, split at its midpoint when it is long
    // enough for two to be interesting.
    let (first, second) = if data.len() > 64 {
        data.split_at(data.len() / 2)
    } else {
        (data, &[][..])
    };
    for unit in [first, second] {
        if unit.is_empty() {
            continue;
        }
        if stream.read(unit).is_ok() {
            if let Some(job) = stream.job() {
                let _ = job.slices.as_slice().iter().flatten().count();
            }
            let _ = stream.finish();
        }
        while let Some(out) = stream.next_output() {
            stream.dpb.taken(out.slot);
        }
    }
    stream.drain();
});
