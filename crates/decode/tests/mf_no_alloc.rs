//! The system's decoder's per-unit path allocates nothing on this side.
//!
//! Building the decoder is free to allocate; the region inside
//! `assert_no_alloc` is what runs per unit for as long as a session lasts:
//! the parameter sets read, the unit written into the decoder's input, its
//! picture taken out and copied into the caller's planes. What the framework
//! allocates within itself is its own. Needs the system's media framework,
//! so it is off by default.

#![cfg(windows)]

mod common;

use lowlat_common::alloc_counter::{Counting, assert_no_alloc};
use lowlat_core::video::{Codec, Rotation, VideoHeader};
use lowlat_decode::mf::Backend;
use lowlat_decode::{Decoder, Fed, Planes};

#[global_allocator]
static ALLOC: Counting = Counting;

#[test]
#[ignore = "requires the system's media framework and its HEVC extension"]
fn the_per_unit_path_allocates_nothing_on_this_side() {
    let mf = lowlat_drivers::mf::load().expect("the system's media framework");
    for (clip, codec) in [
        ("synthetic-720p-h264.bin", Codec::H264),
        ("synthetic-720p-hevc.bin", Codec::H265),
    ] {
        let units = common::units(clip);
        let mut backend = Backend::new(mf, 1 << 22);
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
        let mut run = |backend: &mut Backend, units: &[Vec<u8>]| {
            for unit in units {
                if backend.feed(unit).expect("feed") == Fed::Picture {
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
        // The first units make the decoder and its buffers; what follows is
        // the steady state.
        run(&mut backend, &units[..20]);
        assert_no_alloc(|| run(&mut backend, &units[20..]));
        assert!(pictures >= 100, "{pictures} pictures came out of {clip}");
        backend.destroy();
    }
}
