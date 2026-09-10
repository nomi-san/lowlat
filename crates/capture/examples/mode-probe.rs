//! Ask this session to change an output's mode or turn, and print what it said.
//!
//!   mode-probe <connector> [WxH] [transform]
//!
//! Run it as the person who is logged in. It is the mechanism a guest's mode
//! request goes through, driven by hand.

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(connector) = args.next() else {
        eprintln!("usage: mode-probe <connector> [WxH] [transform]");
        std::process::exit(2);
    };
    let size = args.next().and_then(|size| {
        let (w, h) = size.split_once('x')?;
        Some((w.parse().ok()?, h.parse().ok()?))
    });
    let transform = args.next().and_then(|t| t.parse().ok());
    let started = std::time::Instant::now();
    let outcome = lowlat_capture::mode::set(
        &connector,
        lowlat_capture::mode::Change { size, transform },
        std::time::Duration::from_secs(3),
    );
    println!("{outcome:?} after {:?}", started.elapsed());
}
