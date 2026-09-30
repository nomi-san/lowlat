//! AMD's decoder allocates nothing per unit on this side, by either route:
//! the unit read and handed over in the one buffer made with the backend,
//! the picture split into the plane textures and the fence signalled, or
//! read back into the caller's planes -- the views and the staging texture
//! made once, at the first pictures. What the runtime allocates within
//! itself is its own. Needs AMD's GPU and runtime, so it is off by default.

#![cfg(windows)]

mod common;

use lowlat_common::alloc_counter::{Counting, assert_no_alloc};
use lowlat_core::video::{Codec, Rotation, VideoHeader};
use lowlat_decode::amf::Backend;
use lowlat_decode::d3d11::plane_textures;
use lowlat_decode::{Decoder, Fed, Format, Planes};
use lowlat_drivers::amf::Amf;
use lowlat_drivers::d3d11::D3d11;

#[global_allocator]
static ALLOC: Counting = Counting;

#[test]
#[ignore = "requires AMD's GPU and runtime"]
fn a_unit_allocates_nothing_by_either_route() {
    let d3d11 = D3d11::load().expect("the system's libraries");
    let adapter = d3d11
        .adapters()
        .expect("the walk")
        .into_iter()
        .find(|a| a.decodes_here() && a.maker() == Some("AMD"))
        .expect("an AMD GPU");
    let device = d3d11.open(adapter.luid).expect("a device");
    let amf = Amf::load().expect("AMD's runtime");
    let textures = plane_textures(Format::Nv12, 1280, 720)
        .map(|p| p.map(|(f, w, h)| device.shared_texture(f, w, h).expect("a plane")));
    let targets = textures.each_ref().map(Option::as_ref);
    let pitch = 1280;
    let mut y = vec![0u8; pitch * 720];
    let mut uv = vec![0u8; pitch * 360];
    for by_textures in [true, false] {
        for (clip, codec) in [
            ("synthetic-720p-h264.bin", Codec::H264),
            ("synthetic-720p-hevc.bin", Codec::H265),
        ] {
            let units = common::units(clip);
            let context = amf.context(&device).expect("a context");
            let mut backend = Backend::new(&amf, &context, (4096, 4096), 1 << 20).expect("new");
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
            let mut pictures = 0usize;
            let mut run = |backend: &mut Backend<'_>, units: &[Vec<u8>]| {
                for unit in units {
                    if backend.feed(unit).expect("feed") != Fed::Picture {
                        continue;
                    }
                    if by_textures {
                        while backend.take_to_textures(targets).expect("take").is_some() {
                            pictures += 1;
                        }
                    } else {
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
            // The first units build the decoder and meet each of the
            // textures it hands out; what follows is the steady state.
            run(&mut backend, &units[..10]);
            assert_no_alloc(|| run(&mut backend, &units[10..]));
            assert!(pictures >= 100, "{pictures} pictures came out of {clip}");
            backend.destroy();
        }
    }
}
