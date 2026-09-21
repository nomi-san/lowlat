//! The software backend's per-unit path allocates nothing on this side.
//!
//! Building the backend is free to allocate; the region inside
//! `assert_no_alloc` is what runs per unit for as long as a session lasts:
//! the unit copied into the library's packet, sent, the picture received,
//! converted into the caller's planes and released. What the library
//! allocates within itself goes through its own allocator and is its own.
//! Needs an LGPL pair, so it is off by default (`LOWLAT_FFMPEG_DIR`).

mod common;

use lowlat_common::alloc_counter::{Counting, assert_no_alloc};
use lowlat_core::video::{Codec, Rotation, VideoHeader};
use lowlat_decode::software::Backend;
use lowlat_decode::{Decoder, Fed, Planes};
use lowlat_drivers::lavc::Lavc;

#[global_allocator]
static ALLOC: Counting = Counting;

#[test]
#[ignore = "needs an LGPL codec library pair"]
fn the_per_unit_path_allocates_nothing_on_this_side() {
    let lavc = Lavc::load(None).expect("an LGPL pair: name one with LOWLAT_FFMPEG_DIR");
    let (clip, codec) = if lowlat_decode::software::caps(&lavc).hevc {
        ("synthetic-720p-hevc.bin", Codec::H265)
    } else {
        ("synthetic-720p-h264.bin", Codec::H264)
    };
    let units = common::units(clip);
    let mut backend = Backend::new(&lavc);
    backend
        .build(&VideoHeader {
            frame_id: 1,
            width: 0,
            height: 0,
            codec,
            rotation: Rotation::None,
            ten_bit: false,
            locked: false,
            announced: false,
            metadata: false,
        })
        .expect("build");
    let pitch = 1280;
    let mut y = vec![0u8; pitch * 720];
    let mut uv = vec![0u8; pitch * 360];
    let mut pictures = 0usize;
    let mut run = |backend: &mut Backend<'_>, units: &[Vec<u8>]| {
        for unit in units {
            let fed = backend.feed(unit).expect("feed");
            if fed == Fed::Picture {
                loop {
                    let mut planes = Planes {
                        y: &mut y,
                        y_pitch: pitch,
                        uv: &mut uv,
                        uv_pitch: pitch,
                        v: &mut [],
                        v_pitch: 0,
                    };
                    if backend.take(&mut planes).expect("take").is_none() {
                        break;
                    }
                    pictures += 1;
                }
            }
        }
    };
    // The first units warm the library's own pools; what follows is the
    // steady state.
    run(&mut backend, &units[..20]);
    assert_no_alloc(|| run(&mut backend, &units[20..]));
    assert!(pictures >= 100, "{pictures} pictures came out of {clip}");
    backend.destroy();
}
