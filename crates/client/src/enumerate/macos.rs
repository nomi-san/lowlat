//! The decoder table on macOS: slot 0 is the system's own decoder, slot 1 the
//! machine's own codec library. No open stack and no vendor's interface
//! exists here.

use lowlat_decode::{Caps, videotoolbox};

use super::{Available, PROFILE, RUNTIME, codec_library, label};
use crate::config::Backend;

/// The open stack's slots, none here.
pub const OPEN_SLOTS: u32 = 0;
/// The vendors' slots, none here.
pub const VENDOR_SLOTS: u32 = 0;
/// The table's length: the system's decoder and the codec library.
pub const SLOTS: u32 = 2;

/// The slot `slot` of the table, probed now; none past the table's end.
pub fn probe(slot: u32) -> Option<Available> {
    match slot {
        0 => Some(system()),
        1 => Some(codec_library()),
        _ => None,
    }
}

/// The system's own decoder, asked what it decodes and what of that the
/// hardware does.
fn system() -> Available {
    let name = label("VideoToolbox", Some("Apple"));
    let Ok(vt) = lowlat_drivers::vt::load() else {
        return Available::unavailable(Backend::System, "", name, RUNTIME);
    };
    let (caps, hardware) = videotoolbox::caps(vt);
    if !caps.any() {
        return Available::unavailable(Backend::System, "", name, PROFILE);
    }
    Available {
        backend: Backend::System,
        available: true,
        device: String::new(),
        name,
        driver: words(caps, hardware),
        caps,
        // The hardware's pictures alone leave by handle.
        handle: hardware.any(),
        max_h264: (0, 0),
        max_hevc: (0, 0),
    }
}

/// What the decoder decodes, by the hardware and by the system's software
/// decoder: `hardware H.264 HEVC HEVC-10; software HEVC-444 HEVC-444-10`.
fn words(caps: Caps, hardware: Caps) -> String {
    let each = [
        ("H.264", caps.h264, hardware.h264),
        ("HEVC", caps.hevc, hardware.hevc),
        ("HEVC-10", caps.hevc_10, hardware.hevc_10),
        ("HEVC-444", caps.hevc_444, hardware.hevc_444),
        ("HEVC-444-10", caps.hevc_444_10, hardware.hevc_444_10),
    ];
    let list = |by_hardware: bool| {
        each.iter()
            .filter(|(_, decodes, hw)| *decodes && *hw == by_hardware)
            .map(|(name, ..)| *name)
            .collect::<Vec<_>>()
            .join(" ")
    };
    let (hw, sw) = (list(true), list(false));
    match (hw.is_empty(), sw.is_empty()) {
        (false, false) => format!("hardware {hw}; software {sw}"),
        (false, true) => format!("hardware {hw}"),
        (true, _) => format!("software {sw}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every slot answers, in its place, and the table ends where it says.
    #[test]
    fn every_slot_answers_and_the_table_ends() {
        let system = probe(0).expect("the system's slot");
        assert_eq!(system.backend, Backend::System);
        assert!(system.available, "{}", system.driver);
        assert_eq!(system.name, "VideoToolbox [Apple]");
        // Judged by the slot's own probe: the system offers the hardware by
        // the moment, so a second probe may be told otherwise.
        assert_eq!(
            system.handle,
            system.driver.starts_with("hardware"),
            "the handle is the hardware's: {}",
            system.driver
        );
        assert_eq!(
            probe(1).expect("the codec library's slot").backend,
            Backend::Software
        );
        assert!(probe(SLOTS).is_none());
    }

    /// The words name what each decoder serves, the hardware's first.
    #[test]
    fn the_words_say_what_the_hardware_decodes() {
        let all = Caps {
            h264: true,
            hevc: true,
            hevc_10: true,
            hevc_444: true,
            hevc_444_10: true,
        };
        let some = Caps {
            hevc_444: false,
            hevc_444_10: false,
            ..all
        };
        assert_eq!(
            words(all, all),
            "hardware H.264 HEVC HEVC-10 HEVC-444 HEVC-444-10"
        );
        assert_eq!(
            words(all, some),
            "hardware H.264 HEVC HEVC-10; software HEVC-444 HEVC-444-10"
        );
        assert_eq!(
            words(all, Caps::default()).split(' ').next(),
            Some("software")
        );
    }
}
