//! What the input driver and the host share: the kinds of device the driver
//! makes, with their identities, descriptors and reports, and the control
//! interface's requests and replies
//! ([docs/11-drivers.md](../../../docs/11-drivers.md) section 2).
//!
//! **No platform code and no allocation.** The driver is a library loaded into
//! the system's user-mode driver host; the host opens it. Both compile this,
//! so a request's layout and a device's descriptor are written once, and
//! everything here is tested without the driver kit.

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
pub mod keyboard;
pub mod kind;
pub mod mouse;
pub mod pad;
#[cfg(test)]
mod walk;

pub use kind::Kind;

/// A GUID in the system's layout.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Guid {
    pub data1: u32,
    pub data2: u16,
    pub data3: u16,
    pub data4: [u8; 8],
}

/// The control interface, which the system account and administrators alone
/// may open (docs/11-drivers.md section 2.2).
pub const CONTROL_INTERFACE: Guid = Guid {
    data1: 0x32e3_9bbb,
    data2: 0xdbe3,
    data3: 0x4fb6,
    data4: [0xab, 0x8c, 0x78, 0x3e, 0x5b, 0x21, 0x4f, 0xed],
};

/// How many guests a host seats: a keyboard and a mouse each.
pub const GUEST_SLOTS: u32 = 16;
/// How many pads one guest plugs.
pub const PADS_PER_GUEST: u32 = 4;
/// How many devices the driver makes at most: every seated guest's keyboard,
/// mouse and pads.
pub const MAX_DEVICES: usize = (GUEST_SLOTS * (2 + PADS_PER_GUEST)) as usize;

/// A device's container, the same for a kind and slot at every connection, so
/// the system sets that device up once.
pub const fn container_id(kind: Kind, slot: u8) -> Guid {
    Guid {
        data1: 0x5234_c8de,
        data2: 0xc3e9,
        data3: 0x4480,
        data4: [0x8d, 0x9c, 0xf7, 0x55, 0x4c, 0x66, kind as u8, slot],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_containers_last_bytes_are_its_kind_and_slot() {
        let first = container_id(Kind::Keyboard, 0);
        assert_eq!(
            (first.data1, first.data2, first.data3),
            (0x5234_c8de, 0xc3e9, 0x4480)
        );
        assert_eq!(first.data4[6..], [1, 0]);
        assert_eq!(container_id(Kind::DualSense, 63).data4[6..], [4, 63]);
        assert_ne!(
            container_id(Kind::Keyboard, 3),
            container_id(Kind::Mouse, 3),
            "each kind its own"
        );
        assert_eq!(MAX_DEVICES, 96);
    }
}
