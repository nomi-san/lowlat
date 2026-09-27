//! The system decoding interface's per-unit path allocates nothing on this
//! side.
//!
//! Building the decoder is free to allocate; the region inside
//! `assert_no_alloc` is what runs per unit for as long as a session lasts:
//! the unit read, its parameters staged and handed to the device with its
//! slices, the picture copied out through the staging texture into the
//! caller's planes. What the system's driver allocates within itself is its
//! own. Needs a GPU, so it is off by default.

#![cfg(windows)]

mod common;

use lowlat_common::alloc_counter::{Counting, assert_no_alloc};
use lowlat_core::video::{Codec, Rotation, VideoHeader};
use lowlat_decode::d3d11::Backend;
use lowlat_decode::{Decoder, Fed, Planes};
use lowlat_drivers::d3d11::D3d11;

#[global_allocator]
static ALLOC: Counting = Counting;

#[test]
#[ignore = "requires a GPU"]
fn the_per_unit_path_allocates_nothing_on_this_side() {
    let d3d11 = D3d11::load().expect("the system's libraries");
    let adapters = d3d11.adapters().expect("the walk");
    let luid = adapters
        .iter()
        .find(|a| a.decodes_here())
        .expect("an adapter")
        .luid;
    let device = d3d11.open(luid).expect("a device");
    for (clip, codec) in [
        ("synthetic-720p-h264.bin", Codec::H264),
        ("synthetic-720p-hevc.bin", Codec::H265),
    ] {
        let units = common::units(clip);
        let mut backend = Backend::new(&device, (4096, 4096));
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
        // The first unit builds the decoder and its surfaces; what follows
        // is the steady state.
        run(&mut backend, &units[..2]);
        assert_no_alloc(|| run(&mut backend, &units[2..]));
        assert!(pictures >= 100, "{pictures} pictures came out of {clip}");
        backend.destroy();
    }
}
