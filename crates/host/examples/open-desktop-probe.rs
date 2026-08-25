//! The display source driven the way the encode loop drives it, on the display
//! stack's own encoder.
//!
//!   sudo open-desktop-probe [card0:HDMI-A-1] [/tmp/open-desktop.h264] [frames]
//!
//! **The sibling of `desktop-probe`, for the other encoder and either
//! conversion interface.** It exists because the conversion interface is chosen
//! by name and only one of the two can feed the vendor runtime, so the pairing
//! this one covers has no other way to be run without a peer and a signaling
//! server.
//!
//! `LOWLAT_CONVERT=gl` picks the other interface. What that combination proves
//! is the piece with no unit test: an allocation the display device made, whose
//! planes one interface wrote through and the encoder reads, with three
//! separate parties agreeing on where the colour plane starts.

use std::io::Write;

use lowlat::display::{Display, Register, Registration};
use lowlat_encode::{Poll, vaapi};

fn main() {
    let mut args = std::env::args().skip(1);
    let wanted = args.next().filter(|arg| !arg.is_empty());
    let out = args
        .next()
        .unwrap_or_else(|| "/tmp/open-desktop.h264".to_string());
    let frames: usize = args
        .next()
        .and_then(|value| value.parse().ok())
        .unwrap_or(120);

    // The encoder is configured for what the display is showing, so its size
    // has to be known before it exists. The source does its own scan.
    let Some((width, height)) = Display::size_of_display(wanted.as_deref()) else {
        fail("no output of that name is lit");
    };
    let backend = lowlat::capture::Backend::requested();
    println!("{width}x{height}, converting on {backend:?}");

    let api = vaapi::Vaapi::load().unwrap_or_else(|e| fail(&format!("encoder: {e}")));
    let display = api
        .open(c"/dev/dri/renderD128")
        .unwrap_or_else(|e| fail(&format!("render node: {e}")));
    let caps = display
        .caps(vaapi::Codec::H264)
        .unwrap_or_else(|e| fail(&format!("caps: {e}")));
    let context = display
        .create_context(caps, width, height, 4)
        .unwrap_or_else(|e| fail(&format!("context: {e}")));
    let params = vaapi::Params::H264(lowlat_encode::h264::Params {
        width,
        height,
        fps: 60,
        level_idc: 42,
        log2_max_frame_num_minus4: 4,
        log2_max_poc_lsb_minus4: 4,
        max_num_ref_frames: 1,
    });
    let mut encoder = context
        .encoder(params, 20_000_000)
        .unwrap_or_else(|e| fail(&format!("configure: {e}")));

    let mut desktop = Display::open(4, wanted.as_deref(), backend, Register::Open(&display))
        .unwrap_or_else(|e| fail(&format!("display: {e}")));
    println!("{desktop:?}, encoding {frames} pictures");

    let mut file = std::fs::File::create(&out).unwrap_or_else(|e| fail(&format!("create: {e}")));
    let mut sizes: Vec<usize> = Vec::with_capacity(frames);
    let mut submits: Vec<f64> = Vec::with_capacity(frames);
    let mut converts: Vec<f64> = Vec::with_capacity(frames);
    for at in 0..frames {
        let began = std::time::Instant::now();
        desktop
            .acquire()
            .unwrap_or_else(|e| fail(&format!("acquire {at}: {e}")));
        submits.push(began.elapsed().as_secs_f64() * 1000.0);
        // **The split is the point of the probe now.** The submit above costs
        // the read and the import; the wait below costs the conversion itself
        // plus whatever sharing a device with the encoder adds. With
        // `LOWLAT_NO_ENCODE` the encoder is skipped below, so the convert
        // figure is then the conversion alone and the difference between the
        // two runs is what the encoder adds to it.
        let convert_began = std::time::Instant::now();
        desktop
            .converted()
            .unwrap_or_else(|e| fail(&format!("converted {at}: {e}")));
        converts.push(convert_began.elapsed().as_secs_f64() * 1000.0);
        // **`LOWLAT_NO_ENCODE` acquires and does not encode**, which is the
        // measurement that separates the conversion's own cost from what it
        // pays for sharing a device with the encoder.
        if std::env::var("LOWLAT_NO_ENCODE").is_ok() {
            continue;
        }
        let Some(Registration::Open { surface }) = desktop.presented() else {
            fail(&format!("no registration for picture {at}"));
        };
        encoder
            .submit_registered(*surface, at == 0)
            .unwrap_or_else(|e| fail(&format!("submit {at}: {e}")));
        // **Drained before the next submit**, which the real loop does not have
        // to do: it has a frame clock to come back on and this has none, so
        // without it every surface is in flight by the fifth picture.
        let mut waited = 0;
        loop {
            match encoder.poll() {
                Ok(Poll::Ready { bitstream, .. }) => {
                    sizes.push(bitstream.len());
                    file.write_all(bitstream)
                        .unwrap_or_else(|e| fail(&format!("write: {e}")));
                    break;
                }
                Ok(Poll::Pending) => {
                    waited += 1;
                    if waited > 100_000 {
                        fail(&format!("picture {at} never finished"));
                    }
                }
                Err(error) => fail(&format!("poll {at}: {error}")),
            }
        }
    }

    // **The check is that pictures differ, not that the file decodes.** A
    // source that imports once reads one buffer of the display's rotation
    // forever, which decodes perfectly and never changes.
    submits.sort_by(f64::total_cmp);
    converts.sort_by(f64::total_cmp);
    let rank = |list: &[f64], num: usize, den: usize| {
        list.get((list.len().saturating_sub(1)) * num / den)
            .copied()
            .unwrap_or(0.0)
    };
    println!(
        "acquire p50 {:.3} ms  p95 {:.3} ms  p99 {:.3} ms  max {:.3} ms",
        rank(&submits, 50, 100),
        rank(&submits, 95, 100),
        rank(&submits, 99, 100),
        rank(&submits, 1, 1)
    );
    println!(
        "convert p50 {:.3} ms  p95 {:.3} ms  p99 {:.3} ms  max {:.3} ms",
        rank(&converts, 50, 100),
        rank(&converts, 95, 100),
        rank(&converts, 99, 100),
        rank(&converts, 1, 1)
    );

    let distinct = sizes.windows(2).filter(|pair| pair[0] != pair[1]).count();
    if std::env::var("LOWLAT_NO_ENCODE").is_ok() {
        return;
    }
    println!(
        "wrote {} pictures to {out}, {} bytes, {distinct} consecutive pairs differ",
        sizes.len(),
        sizes.iter().sum::<usize>()
    );
    if distinct == 0 {
        fail("every picture was the same size; the source is probably stuck on one buffer");
    }
}

fn fail(why: &str) -> ! {
    eprintln!("{why}");
    std::process::exit(2);
}
