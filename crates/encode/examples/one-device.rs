//! Convert and encode on one device, with the picture never leaving it.
//!
//!   sudo one-device [/dev/dri/card0]
//!   LOWLAT_CODEC=h265 sudo -E one-device [/dev/dri/card0]
//!
//! **The arrangement the third encoder exists for.** The display's own device
//! opens able to encode as well; the conversion writes its two planes into the
//! very picture the encoder reads; the encoder reads it. Nothing is copied,
//! nothing is read back, and nothing crosses between two interfaces -- which
//! is what costs 1.45 ms a frame otherwise.

use std::path::PathBuf;

use lowlat_capture::convert::Converter;
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

    let encoder_device = match vulkan::Device::shared(capture.clone(), queue, family) {
        Ok(device) => device,
        Err(error) => {
            eprintln!("the encoder could not be built on it: {error}");
            std::process::exit(2);
        }
    };
    let codec = match std::env::var("LOWLAT_CODEC").as_deref() {
        Ok("h265" | "hevc") => vulkan::Codec::H265,
        _ => vulkan::Codec::H264,
    };
    let caps = match encoder_device.caps(codec) {
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

    let mut encoder = match encoder_device.encoder(&caps, 1920, 1080, 10_000_000, 60, 2) {
        Ok(encoder) => encoder,
        Err(error) => {
            eprintln!("encoder: {error}");
            std::process::exit(2);
        }
    };
    if encoder.planes(0).is_none() {
        // A device that keeps a copy between the two proves the pair with one
        // picture of whatever the source holds; the ring is the shared path's.
        println!("  this device keeps a copy between the two");
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
        return;
    }
    println!("  the encoder lends its picture's planes to a shader");

    // **The whole arrangement, end to end**: the conversion writes each of
    // the encoder's pictures in turn and hands it over in the layout the
    // encoder reads; the encoder codes a refresh once and predicts the rest.
    let mut converter = match Converter::new(&capture) {
        Ok(converter) => converter,
        Err(error) => {
            eprintln!("converter: {error}");
            std::process::exit(2);
        }
    };
    let (width, height) = (1920u32, 1080u32);
    let mut pixels = vec![0u8; width as usize * height as usize * 4];
    for y in 0..height as usize {
        for x in 0..width as usize {
            let at = (y * width as usize + x) * 4;
            pixels[at] = u8::try_from(x % 256).unwrap_or(0);
            pixels[at + 1] = u8::try_from(y % 256).unwrap_or(0);
            pixels[at + 2] = u8::try_from((x + y) % 256).unwrap_or(0);
            pixels[at + 3] = 255;
        }
    }
    let source = match capture.upload_rgba(width, height, &pixels) {
        Ok(source) => source,
        Err(error) => {
            eprintln!("upload: {error}");
            std::process::exit(2);
        }
    };

    const FRAMES: usize = 60;
    let mut stream = Vec::new();
    for at in 0..FRAMES {
        let slot = at % 2;
        let (Some(planes), Some(image)) = (encoder.planes(slot), encoder.source(slot)) else {
            eprintln!("slot {slot} lends nothing");
            std::process::exit(1);
        };
        let target = lowlat_capture::convert::TargetRef {
            luma_image: image,
            chroma_image: image,
            planes,
            final_layout: ash::vk::ImageLayout::VIDEO_ENCODE_SRC_KHR,
        };
        if let Err(error) = converter.run(&capture, &source, &target, false) {
            eprintln!("convert {at}: {error}");
            std::process::exit(1);
        }
        if let Err(error) = encoder
            .submit_written(slot, at == 0)
            .and_then(|()| encoder.wait())
        {
            eprintln!("encode {at}: {error}");
            std::process::exit(1);
        }
        match encoder.poll() {
            Ok(lowlat_encode::Poll::Ready { bitstream, .. }) => {
                stream.extend_from_slice(bitstream);
            }
            Ok(lowlat_encode::Poll::Pending) => {
                eprintln!("picture {at} finished and reported nothing");
                std::process::exit(1);
            }
            Err(error) => {
                eprintln!("poll {at}: {error}");
                std::process::exit(1);
            }
        }
    }
    // The two codecs put the unit type in different bits of different bytes.
    let units_of = |kinds: &[u8]| {
        let kinds = kinds.to_vec();
        stream
            .windows(4)
            .filter(|window| {
                window[..3] == [0, 0, 1] && {
                    let kind = match codec {
                        vulkan::Codec::H264 => window[3] & 0x1F,
                        vulkan::Codec::H265 => (window[3] >> 1) & 0x3F,
                    };
                    kinds.contains(&kind)
                }
            })
            .count()
    };
    let (refreshes, predicted) = match codec {
        vulkan::Codec::H264 => (units_of(&[5]), units_of(&[1])),
        vulkan::Codec::H265 => (units_of(&[19, 20]), units_of(&[0, 1])),
    };
    println!(
        "  {FRAMES} converted pictures encoded: {refreshes} coded refresh(es), {predicted} \
         predicted, {} bytes",
        stream.len()
    );
    if refreshes != 1 || predicted != FRAMES - 1 {
        eprintln!("the stream does not carry the picture kinds that were asked for");
        std::process::exit(1);
    }
    let out = std::env::temp_dir().join(match codec {
        vulkan::Codec::H264 => "lowlat-one-device.h264",
        vulkan::Codec::H265 => "lowlat-one-device.h265",
    });
    if let Err(error) = std::fs::write(&out, &stream) {
        eprintln!("write: {error}");
        std::process::exit(1);
    }
    println!("  wrote {}", out.display());
    converter.destroy(&capture);
    capture.release(source);
}
