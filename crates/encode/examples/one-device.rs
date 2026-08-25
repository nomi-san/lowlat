//! Convert and encode on one device, with the picture never leaving it.
//!
//!   sudo one-device [/dev/dri/card0]
//!
//! **The arrangement the third encoder exists for.** The display's own device
//! opens able to encode as well; the conversion writes its two planes into the
//! very picture the encoder reads; the encoder reads it. Nothing is copied,
//! nothing is read back, and nothing crosses between two interfaces -- which
//! is what costs 1.45 ms a frame otherwise.

use std::path::PathBuf;

use lowlat_capture::vulkan::Device as Capture;
use lowlat_encode::vulkan;

fn main() {
    let node = std::env::args()
        .nth(1)
        .map_or_else(|| PathBuf::from("/dev/dri/card0"), PathBuf::from);

    let capture = match Capture::for_display_and_encode(&node) {
        Ok(device) => device,
        Err(error) => {
            eprintln!("{}: {error}", node.display());
            std::process::exit(2);
        }
    };
    println!("{}: {}", node.display(), capture.name());

    let Some((queue, family)) = capture.encode_queue() else {
        eprintln!("that device opened without an encode queue");
        std::process::exit(2);
    };
    println!("  one device, encoding on queue family {family}");

    let encoder_device = match vulkan::Device::shared(capture.shared(), queue, family) {
        Ok(device) => device,
        Err(error) => {
            eprintln!("the encoder could not be built on it: {error}");
            std::process::exit(2);
        }
    };
    let caps = match encoder_device.caps(vulkan::Codec::H264) {
        Ok(caps) => caps,
        Err(error) => {
            eprintln!("caps: {error}");
            std::process::exit(2);
        }
    };
    println!(
        "  the conversion may write the encoder's picture: {}",
        caps.shared_picture
    );

    let mut encoder = match encoder_device.encoder(&caps, 1920, 1080, 10_000_000, 1) {
        Ok(encoder) => encoder,
        Err(error) => {
            eprintln!("encoder: {error}");
            std::process::exit(2);
        }
    };
    match encoder.planes(0) {
        Some(_) => println!("  the encoder lends its picture's planes to a shader"),
        None => println!("  this device keeps a copy between the two"),
    }

    // One picture through, to prove the pair is live on one device.
    if let Err(error) = encoder.submit(0, true).and_then(|()| encoder.wait()) {
        eprintln!("submit: {error}");
        std::process::exit(1);
    }
    match encoder.poll() {
        Ok(lowlat_encode::Poll::Ready { bitstream, .. }) => {
            println!(
                "  encoded {} bytes on the display's own device",
                bitstream.len()
            );
        }
        Ok(lowlat_encode::Poll::Pending) => println!("  finished and reported nothing"),
        Err(error) => println!("  poll: {error}"),
    }
}
