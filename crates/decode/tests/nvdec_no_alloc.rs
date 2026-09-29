//! The vendor's copy into textures allocates nothing per unit on this side,
//! in either of its modes: the unit read and handed to the device, the
//! picture copied into the plane textures and the fence signalled -- the
//! textures made and registered once, before. What the vendor's runtime
//! allocates within itself is its own. Needs the vendor's GPU, so it is off
//! by default.

#![cfg(windows)]

mod common;

use std::sync::Arc;

use lowlat_common::alloc_counter::{Counting, assert_no_alloc};
use lowlat_core::video::{Codec, Rotation, VideoHeader};
use lowlat_decode::d3d11::plane_textures;
use lowlat_decode::nvdec::Backend;
use lowlat_decode::{Decoder, Fed, Format};
use lowlat_drivers::cuda::Cuda;
use lowlat_drivers::cuvid::Cuvid;
use lowlat_drivers::d3d11::D3d11;

#[global_allocator]
static ALLOC: Counting = Counting;

#[test]
#[ignore = "requires the vendor's GPU"]
fn the_copy_to_textures_allocates_nothing_per_unit() {
    let cuda = Cuda::load().expect("the vendor's runtime");
    let compute = cuda.any_device().expect("a device");
    let context = cuda.retain_primary(&compute).expect("its context");
    context.make_current().expect("current here");
    let cuvid = Cuvid::load().expect("the decode interface");
    let value = cuda.luid(&compute).expect("the device's adapter");
    let d3d11 = D3d11::load().expect("the system's libraries");
    let luid = d3d11
        .adapters()
        .expect("the walk")
        .into_iter()
        .map(|a| a.luid)
        .find(|l| l.value() == value)
        .expect("an adapter that is the device");
    let device = Arc::new(d3d11.open(luid).expect("the textures' device"));
    let planes = plane_textures(Format::Nv12, 1280, 720)
        .map(|p| p.map(|(f, w, h)| device.shared_texture(f, w, h).expect("a plane")));
    let registered = planes.each_ref().map(|p| {
        p.as_ref().map(|t| {
            // SAFETY: a live texture of a device on the context's adapter,
            // kept past the registration, which drops first.
            unsafe { cuda.register_texture(&context, t.texture().cast()) }.expect("registered")
        })
    });
    let targets = registered.each_ref().map(Option::as_ref);
    for mapped in [false, true] {
        for (clip, codec) in [
            ("synthetic-720p-h264.bin", Codec::H264),
            ("synthetic-720p-hevc.bin", Codec::H265),
        ] {
            let units = common::units(clip);
            let fence = Arc::new(device.fence(0).expect("a fence"));
            let mut backend = Backend::new(&cuda, &cuvid, (4096, 4096), 1 << 20);
            if mapped {
                backend.force_mapped();
            }
            backend.attach_textures(Arc::clone(&device), fence);
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
                    if backend.feed(unit).expect("feed") == Fed::Picture {
                        while backend.take_to_textures(targets).expect("take").is_some() {
                            pictures += 1;
                        }
                    }
                }
            };
            // The first units build the decoder, its surfaces and the
            // stream; what follows is the steady state.
            run(&mut backend, &units[..2]);
            assert_no_alloc(|| run(&mut backend, &units[2..]));
            assert!(pictures >= 100, "{pictures} pictures came out of {clip}");
            assert_eq!(
                backend.decodes_into_own_surfaces(),
                !mapped && cuvid.asynchronous(),
                "{clip}: the mode"
            );
            backend.destroy();
        }
    }
}
