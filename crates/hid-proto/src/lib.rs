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

/// The kinds and slots that have a device, which the driver keeps across
/// every handle: **a kind and slot is one device at a time**, the one in the
/// kind's and slot's container.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Live {
    /// A word per kind, a bit per slot.
    slots: [u64; Kind::ALL.len()],
    count: usize,
}

/// Why a kind's slot was not claimed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Claim {
    /// A slot the kind does not have.
    Slot,
    /// A device of the kind exists in the slot.
    Taken,
    /// Every device the driver makes exists.
    Full,
}

impl Default for Live {
    fn default() -> Self {
        Self::new()
    }
}

impl Live {
    pub const fn new() -> Self {
        Self {
            slots: [0; Kind::ALL.len()],
            count: 0,
        }
    }

    fn bit(kind: Kind, slot: u8) -> Option<(usize, u64)> {
        if u32::from(slot) >= kind.slots() {
            return None;
        }
        Some((
            usize::from(kind as u8).checked_sub(1)?,
            1u64.checked_shl(u32::from(slot))?,
        ))
    }

    /// Claim `kind`'s `slot` for a device about to be made.
    pub fn claim(&mut self, kind: Kind, slot: u8) -> Result<(), Claim> {
        let (at, bit) = Self::bit(kind, slot).ok_or(Claim::Slot)?;
        let word = self.slots.get_mut(at).ok_or(Claim::Slot)?;
        if *word & bit != 0 {
            return Err(Claim::Taken);
        }
        if self.count >= MAX_DEVICES {
            return Err(Claim::Full);
        }
        *word |= bit;
        self.count += 1;
        Ok(())
    }

    /// Give `kind`'s `slot` back once its device is gone.
    pub fn release(&mut self, kind: Kind, slot: u8) {
        let Some((at, bit)) = Self::bit(kind, slot) else {
            return;
        };
        if let Some(word) = self.slots.get_mut(at)
            && *word & bit != 0
        {
            *word &= !bit;
            self.count -= 1;
        }
    }
}

/// A device's container, the same for a kind and slot at every connection:
/// the system shows a container as one device, whatever the virtual HID
/// framework names the device's instance.
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

    #[test]
    fn a_kind_and_slot_is_one_device_at_a_time_and_96_in_all() {
        let mut live = Live::new();
        assert_eq!(live.claim(Kind::Keyboard, 10), Ok(()));
        assert_eq!(
            live.claim(Kind::Keyboard, 10),
            Err(Claim::Taken),
            "one instance, one device"
        );
        assert_eq!(live.claim(Kind::Mouse, 10), Ok(()), "each kind its own");
        assert_eq!(live.claim(Kind::Keyboard, 11), Ok(()), "a slot apart");
        assert_eq!(live.claim(Kind::Keyboard, 16), Err(Claim::Slot));
        assert_eq!(live.claim(Kind::DualSense, 63), Ok(()), "a pad's last slot");
        assert_eq!(live.claim(Kind::DualSense, 64), Err(Claim::Slot));
        live.release(Kind::Keyboard, 10);
        assert_eq!(
            live.claim(Kind::Keyboard, 10),
            Ok(()),
            "free again once its device went"
        );

        let mut full = Live::new();
        let mut made = 0;
        for kind in Kind::ALL {
            for slot in 0..u8::try_from(kind.slots()).unwrap() {
                if full.claim(kind, slot).is_ok() {
                    made += 1;
                }
            }
        }
        assert_eq!(made, MAX_DEVICES);
        assert_eq!(full.claim(Kind::DualSense, 0), Err(Claim::Full));
        full.release(Kind::DualSense, 0);
        assert_eq!(
            full.claim(Kind::DualSense, 0),
            Err(Claim::Full),
            "a slot never claimed gives nothing back"
        );
        full.release(Kind::Keyboard, 3);
        assert_eq!(full.claim(Kind::DualSense, 0), Ok(()));
    }
}
