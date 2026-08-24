//! The display source driven the way the encode loop drives it.
//!
//!   sudo desktop-probe [/tmp/desktop.h264] [frames]
//!
//! **What this checks is the wiring, not the stages.** Each stage was verified
//! on its own; what only appears once they run in a loop is whether the source
//! keeps up with a display that is moving underneath it. Two things it is here
//! to catch:
//!
//! - The display cycles through a pool of buffers, so a source that imports
//!   once reads one buffer of that rotation forever. That produces a stream
//!   that decodes perfectly and never changes, which is why the check below is
//!   that consecutive pictures differ rather than that the file decodes.
//! - One conversion target per picture in flight. Sharing fewer is invisible
//!   until the screen changes, and then the encoder emits the newest content
//!   under an older picture's timestamp.

use std::io::Write;

use lowlat::display::{Display, Registration};
use lowlat_encode::{Poll, cuda, nvenc};

fn main() {
    let mut args = std::env::args().skip(1);
    let out = args
        .next()
        .unwrap_or_else(|| "/tmp/desktop.h264".to_string());
    let frames: usize = args
        .next()
        .and_then(|value| value.parse().ok())
        .unwrap_or(120);

    let cuda = cuda::Cuda::load().unwrap_or_else(|e| fail(&format!("compute: {e}")));
    let device = cuda
        .device(0)
        .unwrap_or_else(|e| fail(&format!("compute device: {e}")));
    let context = cuda
        .retain_primary(&device)
        .unwrap_or_else(|e| fail(&format!("context: {e}")));
    context
        .make_current()
        .unwrap_or_else(|e| fail(&format!("make current: {e}")));

    // The encoder is configured for what the display is showing, so its size
    // has to be known before it exists. One scan answers that and is thrown
    // away; the source does its own.
    let card = lowlat_capture::scanout::Card::open(std::path::Path::new("/dev/dri/card1"))
        .unwrap_or_else(|e| fail(&format!("open: {e}")));
    let layout = card.scan().unwrap_or_else(|e| fail(&format!("scan: {e}")));
    let (width, height) = (layout.primary.width, layout.primary.height);
    drop(card);

    let api = nvenc::Api::load().unwrap_or_else(|e| fail(&format!("encoder: {e}")));
    let session = api
        .open_session(context)
        .unwrap_or_else(|e| fail(&format!("session: {e}")));
    let mut encoder = session
        .initialize(
            &cuda,
            nvenc::Config {
                codec: nvenc::Codec::H264,
                width,
                height,
                fps: 60,
                bitrate_bps: 20_000_000,
                min_qp: nvenc::DEFAULT_MIN_QP,
            },
        )
        .unwrap_or_else(|e| fail(&format!("configure: {e}")));

    let wanted = std::env::args().nth(1);
    let mut desktop = Display::open(
        nvenc::IN_FLIGHT,
        wanted.as_deref(),
        lowlat::capture::Backend::requested(),
        lowlat::display::Register::Vendor(&encoder),
    )
    .unwrap_or_else(|e| fail(&format!("display: {e}")));
    println!("{desktop:?}, encoding {frames} pictures");

    let mut file = std::fs::File::create(&out).unwrap_or_else(|e| fail(&format!("create: {e}")));
    let mut sizes: Vec<usize> = Vec::with_capacity(frames);
    for at in 0..frames {
        desktop
            .acquire()
            .unwrap_or_else(|e| fail(&format!("acquire: {e}")));
        let Some(Registration::Vendor { input, .. }) = desktop.presented() else {
            fail("nothing was converted")
        };
        if encoder.submit_registered(input, at == 0).is_err() {
            // Back pressure. Collect below and try this picture again.
            collect(&mut encoder, &mut file, &mut sizes, true);
            continue;
        }
        collect(&mut encoder, &mut file, &mut sizes, false);
        // Roughly a frame at sixty, so the display has time to move.
        std::thread::sleep(std::time::Duration::from_millis(16));
    }
    while sizes.len() < frames {
        if !collect(&mut encoder, &mut file, &mut sizes, true) {
            break;
        }
    }
    file.flush().ok();

    // **The stream must move.** A source that imported once decodes perfectly
    // and shows one frozen picture, and every check short of this one passes.
    let identical = sizes.windows(2).filter(|pair| pair[0] == pair[1]).count();
    println!(
        "wrote {} pictures to {out}, {} bytes; {identical} consecutive pairs identical in size",
        sizes.len(),
        sizes.iter().sum::<usize>()
    );
    if sizes.len() > 4 && identical + 4 >= sizes.len() {
        println!("SUSPECT: the picture is barely changing; is the source re-reading the display?");
    }
}

fn collect(
    encoder: &mut nvenc::Encoder<'_>,
    file: &mut std::fs::File,
    sizes: &mut Vec<usize>,
    wait: bool,
) -> bool {
    loop {
        match encoder.poll() {
            Ok(Poll::Ready { bitstream, .. }) => {
                sizes.push(bitstream.len());
                file.write_all(bitstream)
                    .unwrap_or_else(|e| fail(&format!("write: {e}")));
                return true;
            }
            Ok(Poll::Pending) => {
                if !wait {
                    return false;
                }
                std::thread::yield_now();
            }
            Err(error) => fail(&format!("collect: {error}")),
        }
    }
}

fn fail(what: &str) -> ! {
    eprintln!("{what}");
    std::process::exit(1)
}
