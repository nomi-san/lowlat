//! The decoders this machine can open, listed for an application that
//! wants to choose one or show them.
//!
//! **A fixed table of slots, each probed alone**, laid out by the platform
//! (`sys`). The same slot means the same thing on every machine and on
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

use lowlat_decode::{Caps, software};
use lowlat_drivers::lavc::{Lavc, Origin, Refusal};

use crate::config::Backend;

/// The table's layout and the slots only a platform has, which are the
/// platform's own.
#[cfg(target_os = "linux")]
#[path = "enumerate/linux.rs"]
mod sys;

pub use sys::{OPEN_SLOTS, SLOTS, VENDOR_SLOTS, probe};

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

/// The interface's label with a maker in brackets, or alone.
fn label(interface: &str, maker: Option<&str>) -> String {
    match maker {
        Some(maker) => format!("{interface} [{maker}]"),
        None => interface.to_string(),
    }
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
