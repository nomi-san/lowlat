//! The open stack's decode allocates nothing per unit on this side, by either
//! route a picture leaves: read back into the caller's planes, or lent as its
//! surface, held four deep and let go as the queue lets a slot go. The
//! context, its surfaces and the spares that stand in for lent ones are made
//! in the first units; what the driver allocates within itself is its own.
//! Needs a render node with the open-stack driver, so it is off by default:
//! `LOWLAT_VAAPI_NODE` names the node (`renderD128`).

// The open stack exists on Linux alone.
#![cfg(target_os = "linux")]

mod common;

use std::ffi::CString;

use lowlat_common::alloc_counter::{Counting, assert_no_alloc};
use lowlat_core::video::{Codec, Rotation, VideoHeader};
use lowlat_decode::vaapi::{Backend, Lent, caps, exports_surfaces};
use lowlat_decode::{Decoder, Fed, Planes};
use lowlat_drivers::va::Vaapi;

#[global_allocator]
static ALLOC: Counting = Counting;

/// Units decoded before the count: the context and the first pictures,
/// whose reuse of a lent surface's slot makes the spares.
const WARM: usize = 20;

#[test]
#[ignore = "requires the open-stack driver"]
fn the_decode_allocates_nothing_per_unit_either_way() {
    let named = std::env::var("LOWLAT_VAAPI_NODE").unwrap_or_else(|_| "/dev/dri/renderD128".into());
    let va = Vaapi::load().expect("runtime");
    let display = va.open(&CString::new(named).unwrap()).expect("render node");
    let lends = exports_surfaces(&display);
    let pitch = 1280 * 2 + 64;
    let (mut y, mut u, mut v) = (
        vec![0u8; pitch * 720],
        vec![0u8; pitch * 720],
        vec![0u8; pitch * 720],
    );
    for lent in [false, true] {
        if lent && !lends {
            println!("the display lends no surface; the lent route not exercised");
            continue;
        }
        for (clip, codec) in [
            ("synthetic-720p-h264.bin", Codec::H264),
            ("synthetic-720p-hevc.bin", Codec::H265),
        ] {
            if !decodes(&display, codec) {
                println!("{clip}: the device does not decode this codec, skipped");
                continue;
            }
            let units = common::units(clip);
            let mut backend = Backend::new(&display, (4096, 4096));
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
            // A queue's four slots, each holding its last picture's lease
            // until it takes the next: a fixed ring, as the queue is.
            let mut held: [Option<Lent>; 4] = [None, None, None, None];
            let mut next = 0usize;
            let mut numbers = 0u32;
            let mut pictures = 0usize;
            let mut run = |backend: &mut Backend<'_>, units: &[Vec<u8>]| {
                for unit in units {
                    if backend.feed(unit).expect("feed") != Fed::Picture {
                        continue;
                    }
                    loop {
                        let slot = &mut held[next % 4];
                        if let Some(old) = slot.take() {
                            backend.lease_ended(old.lease);
                        }
                        if lent {
                            let taken = backend
                                .take_lent(|| {
                                    numbers += 1;
                                    numbers
                                })
                                .expect("take lent");
                            let Some((_, picture)) = taken else {
                                break;
                            };
                            *slot = Some(picture);
                        } else {
                            let mut planes = Planes {
                                y: &mut y,
                                y_pitch: pitch,
                                uv: &mut u,
                                uv_pitch: pitch,
                                v: &mut v,
                                v_pitch: pitch,
                            };
                            if backend.take(&mut planes).expect("take").is_none() {
                                break;
                            }
                        }
                        next += 1;
                        pictures += 1;
                    }
                }
            };
            run(&mut backend, &units[..WARM]);
            let spares = backend.spares();
            assert_no_alloc(|| run(&mut backend, &units[WARM..]));
            assert_eq!(
                backend.spares(),
                spares,
                "{clip}: a spare made past the warm-up"
            );
            assert!(pictures >= 100, "{pictures} pictures came out of {clip}");
            println!("{clip}: lent={lent} {pictures} pictures, {spares} spares");
            for slot in &mut held {
                if let Some(old) = slot.take() {
                    backend.lease_ended(old.lease);
                }
            }
            backend.destroy();
        }
    }
}

/// Whether the display decodes a codec's eight-bit profile at all.
fn decodes(display: &lowlat_drivers::va::Display<'_>, codec: Codec) -> bool {
    let caps = caps(display).expect("caps");
    match codec {
        Codec::H264 => caps.h264,
        Codec::H265 => caps.hevc,
    }
}
