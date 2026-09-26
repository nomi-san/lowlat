//! The decoder table on Linux: slots 0 to 7 are the open stack on the
//! render nodes `renderD128` to `renderD135`, 8 to 15 the vendor's interface
//! on its devices by ordinal, 16 the machine's own codec library.

use std::ffi::CString;

use lowlat_core::video::Codec;
use lowlat_decode::vaapi::Vaapi;
use lowlat_decode::{nvdec, vaapi};
use lowlat_drivers::cuda::Cuda;
use lowlat_drivers::cuvid::Cuvid;

use super::{Available, NO_CONTEXT, NO_DEVICE, NO_NODE, PROFILE, RUNTIME, codec_library, label};
use crate::config::Backend;
use crate::seam::sys::{RENDER_NODES, node_of, vendor_of};

/// The open stack's slots, one per render node this crate looks at.
pub const OPEN_SLOTS: u32 = 8;
const _: () = assert!(RENDER_NODES.len() == OPEN_SLOTS as usize);
/// The vendor's slots, one per device ordinal.
pub const VENDOR_SLOTS: u32 = 8;
/// The table's length: the open stack's, the vendor's, and the codec
/// library's one slot.
pub const SLOTS: u32 = OPEN_SLOTS + VENDOR_SLOTS + 1;

/// The slot `slot` of the table, probed now; none past the table's end.
pub fn probe(slot: u32) -> Option<Available> {
    if let Some(node) = RENDER_NODES.get(usize::try_from(slot).ok()?) {
        return Some(open_stack(node));
    }
    if let Some(ordinal) = slot.checked_sub(OPEN_SLOTS).filter(|o| *o < VENDOR_SLOTS) {
        return Some(vendor(ordinal));
    }
    (slot == SLOTS - 1).then(codec_library)
}

/// The open stack on one render node.
fn open_stack(node: &str) -> Available {
    let name = label("VA-API", vendor_of(node));
    let Ok(va) = Vaapi::load() else {
        return Available::unavailable(Backend::Vaapi, node, name, RUNTIME);
    };
    let Ok(path) = CString::new(node) else {
        return Available::unavailable(Backend::Vaapi, node, name, NO_NODE);
    };
    let Ok(display) = va.open(&path) else {
        return Available::unavailable(Backend::Vaapi, node, name, NO_NODE);
    };
    let caps = vaapi::caps(&display).unwrap_or_default();
    if !caps.any() {
        return Available::unavailable(Backend::Vaapi, node, name, PROFILE);
    }
    Available {
        backend: Backend::Vaapi,
        available: true,
        device: node.to_string(),
        name,
        driver: display.vendor(),
        caps,
        handle: false,
        max_h264: vaapi::limits(&display, Codec::H264),
        max_hevc: vaapi::limits(&display, Codec::H265),
    }
}

/// The vendor's interface on the device at one ordinal.
fn vendor(ordinal: u32) -> Available {
    let Ok(cuda) = Cuda::load() else {
        return Available::unavailable(Backend::Nvdec, "", label("NVDEC", None), RUNTIME);
    };
    let Ok(device) = cuda.device(ordinal) else {
        return Available::unavailable(Backend::Nvdec, "", label("NVDEC", None), NO_DEVICE);
    };
    let node = node_of(device.address()).unwrap_or("");
    let name = label("NVDEC", Some("NVIDIA"));
    let Ok(context) = cuda.retain_primary(&device) else {
        return Available::unavailable(Backend::Nvdec, node, name, NO_CONTEXT);
    };
    if context.make_current().is_err() {
        return Available::unavailable(Backend::Nvdec, node, name, NO_CONTEXT);
    }
    let row = match Cuvid::load() {
        Err(_) => Available::unavailable(Backend::Nvdec, node, name, RUNTIME),
        Ok(cuvid) => {
            let caps = nvdec::caps(&cuvid);
            if caps.any() {
                let mut product = [0u8; 96];
                let len = cuda.device_name(&device, &mut product).unwrap_or(0);
                Available {
                    backend: Backend::Nvdec,
                    available: true,
                    device: node.to_string(),
                    name,
                    driver: String::from_utf8_lossy(product.get(..len).unwrap_or(&[])).into_owned(),
                    caps,
                    handle: true,
                    max_h264: nvdec::limits(&cuvid, Codec::H264),
                    max_hevc: nvdec::limits(&cuvid, Codec::H265),
                }
            } else {
                Available::unavailable(
                    Backend::Nvdec,
                    node,
                    label("NVDEC", Some("NVIDIA")),
                    PROFILE,
                )
            }
        }
    };
    // The application's thread, left as it was found.
    let _ = context.release_current();
    row
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use super::*;

    /// **Every slot answers, in the table's order, and nothing past it.** An
    /// available row names a backend creation takes and decodes something;
    /// an unavailable one decodes nothing and says why. On a machine with
    /// no decoder every slot is unavailable, which is an answer.
    #[test]
    fn every_slot_answers_and_the_table_ends() {
        let rows: Vec<Available> = (0..SLOTS)
            .map(|slot| probe(slot).expect("a slot"))
            .collect();
        assert!(probe(SLOTS).is_none());
        assert!(probe(u32::MAX).is_none());
        for (slot, row) in rows.iter().enumerate() {
            println!("[{slot}] {row:?}");
            let slot = slot as u32;
            let expected = if slot < OPEN_SLOTS {
                Backend::Vaapi
            } else if slot < OPEN_SLOTS + VENDOR_SLOTS {
                Backend::Nvdec
            } else {
                Backend::Software
            };
            assert_eq!(row.backend, expected, "slot {slot}");
            let interface = match row.backend {
                Backend::Vaapi => "VA-API",
                Backend::Nvdec => "NVDEC",
                _ => "libavcodec",
            };
            assert!(
                row.name == interface || row.name.starts_with(&format!("{interface} [")),
                "a label off the grammar: {}",
                row.name
            );
            assert!(
                !row.driver.is_empty(),
                "a slot without the driver's words or a reason"
            );
            if row.available {
                assert!(row.caps.any());
                assert_eq!(row.handle, row.backend == Backend::Nvdec);
                if row.backend == Backend::Software {
                    // The licence follows the version in the driver's words,
                    // and it is one this build loads: LGPL always, GPL only
                    // with the feature that says so; its first word is the
                    // label's.
                    let licence = row
                        .driver
                        .splitn(3, ' ')
                        .nth(2)
                        .expect("a name, a version, a licence");
                    assert_eq!(
                        row.name,
                        format!("libavcodec [{}]", licence.split(' ').next().unwrap_or(""))
                    );
                    assert!(
                        lowlat_drivers::lavc::accepts(licence),
                        "a software row of a licence this build refuses: {}",
                        row.name
                    );
                }
            } else {
                assert!(!row.caps.any(), "an unavailable slot with a capability");
                assert!(!row.handle);
            }
        }
        // The open stack's slots name their node whether or not they opened.
        for (slot, node) in RENDER_NODES.iter().enumerate() {
            assert_eq!(rows[slot].device, *node);
        }
    }
}
