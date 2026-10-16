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

pub mod control;
pub mod edid;
pub mod frames;
pub mod modes;
pub mod settings;

/// How many monitors the adapter carries, each a slot (docs/11-drivers.md
/// section 1.3).
pub const MAX_MONITORS: usize = 16;

/// A GUID in the system's layout.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Guid {
    pub data1: u32,
    pub data2: u16,
    pub data3: u16,
    pub data4: [u8; 8],
}

/// The control interface, which any interactive user may open.
pub const CONTROL_INTERFACE: Guid = Guid {
    data1: 0x4a91_4f53,
    data2: 0x543f,
    data3: 0x46f9,
    data4: [0xb9, 0x9b, 0xd3, 0x49, 0xa5, 0xaa, 0xf3, 0xe5],
};

/// A monitor's container, the same for a slot across replugs and reboots, so
/// the system keeps the display's arrangement for it. The slot is the last
/// byte.
pub const fn container_id(slot: u8) -> Guid {
    Guid {
        data1: 0x3bb8_0aa8,
        data2: 0x3343,
        data3: 0x4e3d,
        data4: [0x99, 0xd4, 0xcc, 0xc1, 0x66, 0xf4, 0x62, slot],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_slots_container_carries_the_slot_and_nothing_else_differs() {
        let first = container_id(0);
        assert_eq!(
            (first.data1, first.data2, first.data3),
            (0x3bb8_0aa8, 0x3343, 0x4e3d)
        );
        assert_eq!(
            first.data4,
            [0x99, 0xd4, 0xcc, 0xc1, 0x66, 0xf4, 0x62, 0x00]
        );
        let last = container_id(15);
        assert_eq!(last.data4, [0x99, 0xd4, 0xcc, 0xc1, 0x66, 0xf4, 0x62, 0x0f]);
        assert_ne!(container_id(3), container_id(4), "each slot its own");
    }
}
