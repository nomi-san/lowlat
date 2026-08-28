//! Encode a run of synthetic pictures and keep the source beside the stream,
//! so what came back can be compared against what went in.
//!
//!   LOWLAT_VAAPI_NODE=/dev/dri/renderD128 LOWLAT_CODEC=h265 \
//!     drift-probe /tmp/out
//!
//! `LOWLAT_ALL_INTRA` refreshes every picture, which separates a picture coded
//! wrongly from a reference that drifts: with no picture referencing another,
//! drift has nowhere to accumulate.
//!
//! Writes `/tmp/out.265` (or `.264`) and `/tmp/out.nv12`, then:
//!
//!   ffmpeg -i /tmp/out.265 -pix_fmt nv12 -f rawvideo /tmp/dec.nv12
//!
//! **A decode-error count is not a picture and a first frame is not a run.**
//! Both of this backend's wrong pictures so far encoded without error and
//! decoded without error: one was wrong from the second row of blocks, and one
//! was right in the first picture and drifted afterwards. Only a comparison
//! against the source finds either, and only a comparison over a run finds the
//! second.

use std::ffi::CString;
use std::io::Write;

use lowlat_encode::{Poll, vaapi};

const PICTURES: usize = 60;
const DEPTH: usize = 2;

fn main() {
    let stem = std::env::args().nth(1).unwrap_or("/tmp/drift".into());
    let node = std::env::var("LOWLAT_VAAPI_NODE").unwrap_or("/dev/dri/renderD128".into());
    let hevc = std::env::var("LOWLAT_CODEC").is_ok_and(|c| c == "h265" || c == "hevc");
    let width: u32 = std::env::var("LOWLAT_W")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1920);
    let height: u32 = std::env::var("LOWLAT_H")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1080);

    let va = vaapi::Vaapi::load().expect("no open-stack runtime");
    let display = va
        .open(&CString::new(node.clone()).unwrap())
        .expect("render node");
    let codec = if hevc {
        vaapi::Codec::H265
    } else {
        vaapi::Codec::H264
    };
    let caps = display.caps(codec).expect("caps");
    let context = display
        .create_context(caps, width, height, 4)
        .expect("context");
    let params = if hevc {
        vaapi::Params::H265(lowlat_encode::h265::Params {
            width,
            height,
            fps: 60,
            level_idc: 123,
            log2_max_poc_lsb_minus4: 4,
            max_num_ref_frames: 1,
            transform_depth: lowlat_encode::h265::TRANSFORM_HIERARCHY_DEPTH,
        })
    } else {
        vaapi::Params::H264(lowlat_encode::h264::Params {
            width,
            height,
            fps: 60,
            level_idc: 42,
            log2_max_frame_num_minus4: 4,
            log2_max_poc_lsb_minus4: 4,
            max_num_ref_frames: 1,
        })
    };
    let bps: u32 = std::env::var("LOWLAT_BPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20_000_000);
    let mut encoder = context.encoder(params, bps).expect("encoder");
    if let Ok(level) = std::env::var("LOWLAT_QUALITY")
        && let Ok(level) = level.parse::<u32>()
    {
        encoder.set_quality(level);
    }
    println!(
        "device offers {} effort level(s), asking for {}",
        caps.quality_range,
        encoder.quality()
    );

    let ext = if hevc { "265" } else { "264" };
    let mut stream = std::fs::File::create(format!("{stem}.{ext}")).expect("stream");
    let mut source_out = std::fs::File::create(format!("{stem}.nv12")).expect("source");

    // **A real clip where one is given**, because the synthetic source is
    // nearly flat: sixty of its pictures fit in tens of kilobytes, so no
    // ceiling ever binds and nothing about a starved encode can be measured
    // through it. LOWLAT_SOURCE names raw NV12 at the size above.
    let clip: Option<Vec<u8>> = std::env::var("LOWLAT_SOURCE")
        .ok()
        .map(|p| std::fs::read(p).expect("source clip"));
    let frame_bytes = (width as usize) * (height as usize) * 3 / 2;
    let mut source = lowlat_capture::synthetic::Synthetic::new(width, height);
    let (mut submitted, mut collected) = (0usize, 0usize);
    while collected < PICTURES {
        if submitted < PICTURES && encoder.in_flight() < DEPTH {
            let held;
            let frame = match &clip {
                Some(bytes) => {
                    let at = (submitted % (bytes.len() / frame_bytes)) * frame_bytes;
                    let luma = (width as usize) * (height as usize);
                    held = lowlat_capture::Frame {
                        width,
                        height,
                        luma: lowlat_capture::Plane {
                            bytes: &bytes[at..at + luma],
                            stride: width as usize,
                        },
                        chroma: lowlat_capture::Plane {
                            bytes: &bytes[at + luma..at + frame_bytes],
                            stride: width as usize,
                        },
                        captured_at: lowlat_common::clock::Time::now(),
                        index: submitted as u64,
                    };
                    held
                }
                None => source.acquire(),
            };
            // **Written as the encoder was handed it**, row by row, because a
            // plane's stride is not its width and a bulk write would record
            // padding the encoder never saw.
            for row in 0..height as usize {
                source_out
                    .write_all(&frame.luma.row(row).unwrap()[..width as usize])
                    .unwrap();
            }
            for row in 0..(height as usize) / 2 {
                source_out
                    .write_all(&frame.chroma.row(row).unwrap()[..width as usize])
                    .unwrap();
            }
            encoder
                .submit(
                    &frame,
                    std::env::var("LOWLAT_ALL_INTRA").is_ok() || submitted == 0,
                )
                .expect("submit");
            submitted += 1;
        }
        match encoder.poll().expect("poll") {
            Poll::Ready { bitstream, .. } => {
                stream.write_all(bitstream).unwrap();
                collected += 1;
            }
            Poll::Pending => std::hint::spin_loop(),
        }
    }
    println!("{stem}.{ext} and {stem}.nv12: {PICTURES} pictures of {width}x{height}");
}
