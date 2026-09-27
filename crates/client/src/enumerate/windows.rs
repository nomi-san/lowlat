//! The decoder table on Windows: slots 0 to 7 are the system's video
//! decoding interface on the adapters offered, high-performance first, slot 8
//! the machine's own codec library. The vendor's slots come with its step
//! of docs/impl-plan-windows.md, between the two.

use lowlat_core::video::Codec;
use lowlat_decode::d3d11;
use lowlat_drivers::d3d11::D3d11;

use super::{Available, NO_CONTEXT, NO_DEVICE, PROFILE, RUNTIME, codec_library, label};
use crate::config::Backend;

/// The system interface's slots: one per adapter offered, in the system's
/// high-performance order.
pub const OPEN_SLOTS: u32 = 8;
/// The vendor's slots: none yet.
pub const VENDOR_SLOTS: u32 = 0;
/// The table's length: the system interface's, and the codec library's one
/// slot.
pub const SLOTS: u32 = OPEN_SLOTS + VENDOR_SLOTS + 1;

/// The slot `slot` of the table, probed now; none past the table's end.
pub fn probe(slot: u32) -> Option<Available> {
    if slot < OPEN_SLOTS {
        return Some(system(usize::try_from(slot).ok()?));
    }
    (slot == SLOTS - 1).then(codec_library)
}

/// The system's interface on the `nth` adapter offered.
fn system(nth: usize) -> Available {
    let Ok(d3d11) = D3d11::load() else {
        return Available::unavailable(Backend::Vaapi, "", label("D3D11", None), RUNTIME);
    };
    let adapter = d3d11
        .adapters()
        .ok()
        .and_then(|all| all.into_iter().filter(|a| a.decodes_here()).nth(nth));
    let Some(adapter) = adapter else {
        return Available::unavailable(Backend::Vaapi, "", label("D3D11", None), NO_DEVICE);
    };
    let device_name = adapter.luid.to_string();
    let name = label("D3D11", adapter.maker());
    let Ok(device) = d3d11.open(adapter.luid) else {
        return Available::unavailable(Backend::Vaapi, &device_name, name, NO_CONTEXT);
    };
    let caps = d3d11::caps(&device);
    if !caps.any() {
        return Available::unavailable(Backend::Vaapi, &device_name, name, PROFILE);
    }
    let driver = match adapter.driver {
        Some([a, b, c, d]) => format!("{} {a}.{b}.{c}.{d}", adapter.description),
        None => adapter.description.clone(),
    };
    Available {
        backend: Backend::Vaapi,
        available: true,
        device: device_name,
        name,
        driver,
        caps,
        handle: false,
        max_h264: d3d11::limits(&device, Codec::H264),
        max_hevc: d3d11::limits(&device, Codec::H265),
    }
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use super::*;
    use lowlat_drivers::d3d11::Luid;

    /// **Every slot answers, in the table's order, and nothing past it**:
    /// the system interface's slots, each an adapter by its identity or
    /// unavailable with why, no adapter twice; then the codec library,
    /// available with what it decodes or unavailable with why. Labels and
    /// words follow the grammar every platform's table uses.
    #[test]
    fn every_slot_answers_and_the_table_ends() {
        let rows: Vec<Available> = (0..SLOTS)
            .map(|slot| probe(slot).expect("a slot"))
            .collect();
        assert!(probe(SLOTS).is_none());
        assert!(probe(u32::MAX).is_none());
        let mut seen = Vec::new();
        for (slot, row) in rows.iter().enumerate() {
            println!("{slot}: {row:?}");
            assert!(!row.driver.is_empty(), "a slot without its words");
            if row.available {
                assert!(row.caps.any());
            } else {
                assert!(!row.caps.any(), "an unavailable slot with a capability");
            }
            if slot < OPEN_SLOTS as usize {
                assert_eq!(row.backend, Backend::Vaapi);
                assert!(
                    row.name == "D3D11" || row.name.starts_with("D3D11 ["),
                    "a label off the grammar: {}",
                    row.name
                );
                assert!(!row.handle, "the system interface hands out no handle yet");
                if row.available {
                    assert!(Luid::parse(&row.device).is_some(), "{}", row.device);
                    assert!(!seen.contains(&row.device), "{} twice", row.device);
                    seen.push(row.device.clone());
                }
                continue;
            }
            assert_eq!(row.backend, Backend::Software);
            assert!(
                row.name == "libavcodec" || row.name.starts_with("libavcodec ["),
                "a label off the grammar: {}",
                row.name
            );
            assert!(!row.handle, "the codec library hands out no handle");
            if row.available {
                let licence = row
                    .driver
                    .splitn(3, ' ')
                    .nth(2)
                    .expect("a name, a version, a licence");
                assert!(
                    lowlat_drivers::lavc::accepts(licence),
                    "a software row of a licence this build refuses: {}",
                    row.name
                );
            }
        }
    }
}
