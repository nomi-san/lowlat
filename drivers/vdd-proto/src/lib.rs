//! What the display driver and its controller share: the control interface's
//! requests and replies, the monitors' identity and their modes, the file
//! kept monitors come back from, and the page each monitor's pictures are
//! published in ([docs/11-drivers.md](../../../docs/11-drivers.md)).
//!
//! **No platform code and no allocation.** The driver is a library loaded into
//! the system's user-mode driver host; the controller is the host's display
//! and the library's third half. Both compile this, so a request's layout and
//! a monitor's identity are written once, and everything here is tested
//! without the driver kit.

#![no_std]
#![deny(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
// Tests may panic freely: a failing assertion is the point. AGENTS.md 7.
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic
    )
)]

#[cfg(test)]
extern crate std;

/// A GUID in the system's layout.
pub use lowlat_brand::Guid;
/// The display driver's names and identifiers, as the build's brand gives them
/// (docs/11-drivers.md section 1.10).
pub use lowlat_brand::display as brand;

pub mod control;
pub mod edid;
pub mod frames;
pub mod modes;
pub mod settings;

/// How many monitors the adapter carries, each a slot (docs/11-drivers.md
/// section 1.3).
pub const MAX_MONITORS: usize = 16;

/// The control interface, which any interactive user may open: the brand's.
pub const CONTROL_INTERFACE: Guid = brand::CONTROL_INTERFACE;

/// A monitor's container, the same for a slot across replugs and reboots, so
/// the system keeps the display's arrangement for it: the brand's, the slot
/// its last byte.
pub const fn container_id(slot: u8) -> Guid {
    let base = brand::CONTAINER;
    let [a, b, c, d, e, f, g, _] = base.data4;
    Guid {
        data4: [a, b, c, d, e, f, g, slot],
        ..base
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_slots_container_carries_the_slot_and_nothing_else_differs() {
        let base = brand::CONTAINER;
        for slot in [0, 15] {
            let id = container_id(slot);
            assert_eq!(
                (id.data1, id.data2, id.data3),
                (base.data1, base.data2, base.data3)
            );
            assert_eq!(id.data4[..7], base.data4[..7]);
            assert_eq!(id.data4[7], slot);
        }
        assert_ne!(container_id(3), container_id(4), "each slot its own");
    }
}
