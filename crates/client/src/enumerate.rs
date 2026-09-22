//! The decoders this machine can open, listed for an application that
//! wants to choose one or show them.
//!
//! **A fixed table of slots, each probed alone.** Slots 0 to 7 are the open
//! stack on the render nodes `renderD128` to `renderD135`, 8 to 15 the
//! vendor's interface on its devices by ordinal, 16 the machine's own codec
//! library. The same slot means the same thing on every machine and on
//! every call, and a call probes its one slot the way creation probes it
//! and nothing else -- so a loop over the table costs each slot once, and
//! nothing is remembered between calls. A slot with nothing usable behind
//! it is still a row, with `available` clear and the reason in its name,
//! so a loop runs to the end of the table and skips what it cannot use.
//! An available row is a decoder creation would open, named by the same
//! two values creation takes: the backend and the render node -- or, for
//! software, the directory the pair was found in. Each row carries a
//! label for a menu -- the interface and the card's maker, `VA-API [Intel]`
//! -- and the driver's own words beside it.

use std::ffi::CString;

use lowlat_core::video::Codec;
use lowlat_decode::vaapi::Vaapi;
use lowlat_decode::{Caps, nvdec, software, vaapi};
use lowlat_drivers::cuda::Cuda;
use lowlat_drivers::cuvid::Cuvid;
use lowlat_drivers::lavc::{Lavc, Origin, Refusal};

use crate::config::Backend;
use crate::seam::{RENDER_NODES, node_of, vendor_of};

/// The open stack's slots, one per render node this crate looks at.
pub const OPEN_SLOTS: u32 = 8;
const _: () = assert!(RENDER_NODES.len() == OPEN_SLOTS as usize);
/// The vendor's slots, one per device ordinal.
pub const VENDOR_SLOTS: u32 = 8;
/// The table's length: the open stack's, the vendor's, and the codec
/// library's one slot.
pub const SLOTS: u32 = OPEN_SLOTS + VENDOR_SLOTS + 1;

/// Why a slot is not available, in the words its row carries.
const RUNTIME: &str = "the runtime library is not on the machine";
const NO_NODE: &str = "no such render node, or it did not open";
const NO_DEVICE: &str = "no such device";
const NO_CONTEXT: &str = "the device did not open";
const PROFILE: &str = "decodes none of the profiles a stream could use";
const NO_PAIR: &str = "no codec library pair found";
const LICENCE: &str = "the codec library answers a licence this build does not load";
const UNUSABLE: &str = "the codec library is not one this library can use";

/// One slot of the table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Available {
    pub backend: Backend,
    /// Whether a decoder opened here. Clear, the capabilities are all
    /// false and `driver` says why.
    pub available: bool,
    /// The render node, as creation names the device; empty when the
    /// vendor's device has no node this crate looks at, which creation
    /// takes as the first device.
    pub device: String,
    /// A label for a menu: the interface, and the card's maker in brackets
    /// where it is known -- `VA-API [AMD]`, `NVDEC [NVIDIA]`,
    /// `libavcodec [LGPL]`.
    pub name: String,
    /// The driver's own words: its banner, the device's product name, the
    /// library's version and licence; for a slot that is not available,
    /// the reason.
    pub driver: String,
    pub caps: Caps,
    /// Whether it hands pictures out as a handle.
    pub handle: bool,
    /// The largest coded picture per codec, or zero where the device does
    /// not say.
    pub max_h264: (u32, u32),
    pub max_hevc: (u32, u32),
}

impl Available {
    /// A slot with nothing usable behind it.
    fn unavailable(backend: Backend, device: &str, label: String, why: &str) -> Self {
        Self {
            backend,
            available: false,
            device: device.to_string(),
            name: label,
            driver: why.to_string(),
            caps: Caps::default(),
            handle: false,
            max_h264: (0, 0),
            max_hevc: (0, 0),
        }
    }
}

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

/// The interface's label with a maker in brackets, or alone.
fn label(interface: &str, maker: Option<&str>) -> String {
    match maker {
        Some(maker) => format!("{interface} [{maker}]"),
        None => interface.to_string(),
    }
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

/// The machine's own codec library, from the loader's own search.
fn codec_library() -> Available {
    match Lavc::load(None) {
        Ok(lavc) => Available {
            backend: Backend::Software,
            available: true,
            device: match &lavc.origin {
                Origin::Directory(dir) => dir.display().to_string(),
                Origin::Default => String::new(),
            },
            // The licence's first word: LGPL, or GPL under the opt-in.
            name: label("libavcodec", lavc.licence.split(' ').next()),
            driver: format!(
                "libavcodec {}.{}.{} {}",
                lavc.version.0, lavc.version.1, lavc.version.2, lavc.licence
            ),
            caps: software::caps(&lavc),
            handle: false,
            max_h264: (0, 0),
            max_hevc: (0, 0),
        },
        Err(refusal) => Available::unavailable(
            Backend::Software,
            "",
            label("libavcodec", None),
            match refusal {
                Refusal::Absent => NO_PAIR,
                Refusal::Licence => LICENCE,
                Refusal::NoDecoder => PROFILE,
                _ => UNUSABLE,
            },
        ),
    }
}

#[cfg(test)]
mod tests {
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
