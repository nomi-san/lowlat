//! The drivers' identity as the build was given it (docs/11-drivers.md
//! section 1.10): lowlat's own, or the brand file `LOWLAT_BRAND` named, read by
//! the build script.
//!
//! **Every name the drivers, their packages and their controllers use comes
//! from here** -- [`display`] for the display driver, [`input`] for the input
//! driver -- so a company's build carries its own throughout: the names a
//! person sees, and the identifiers the system and the controllers know the
//! drivers by, which two drivers on one machine must not share. A controller
//! built with one brand finds only that brand's drivers.
//!
//! **No platform code and no allocation**: the drivers, their protocols and
//! their controllers all compile this.

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

pub mod edid;
// Shared with the build script, which reads the brand file through it; the
// crate's tests read brand files of their own.
#[cfg(test)]
mod file;

/// A GUID in the system's layout.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Guid {
    pub data1: u32,
    pub data2: u16,
    pub data3: u16,
    pub data4: [u8; 8],
}

include!(concat!(env!("OUT_DIR"), "/brand.rs"));

#[cfg(test)]
mod tests {
    use super::*;

    /// **lowlat's own build keeps every identity its drivers had** before a
    /// brand could replace them -- the files, the devices, the interfaces,
    /// the containers, the monitors' EDID to the byte -- so installed drivers
    /// update in place and the system keeps its displays and devices.
    #[test]
    #[cfg_attr(lowlat_brand, ignore = "the build carries another brand")]
    fn lowlats_own_build_keeps_its_identity() {
        use display::*;
        assert_eq!(
            (ID, PROVIDER, DEVICE),
            ("lowlat", "Lowlat", "Lowlat Virtual Display")
        );
        assert_eq!((FOLDER, LOG), ("lowlat\\display", "lowlat-vdd"));
        assert_eq!(
            (LIBRARY, INF, CATALOG, CERTIFICATE),
            (
                "lowlat_idd.dll",
                "lowlat-idd.inf",
                "lowlat-idd.cat",
                "lowlat-idd.cer"
            )
        );
        assert_eq!(
            (HARDWARE_ID, SERVICE, GROUP, COMPANION_INF),
            (
                "Root\\lowlat-vdd",
                "LowlatVdd",
                "LowlatVddGroup",
                "lowlat-hid.inf"
            )
        );
        assert_eq!(
            CONTROL_INTERFACE,
            Guid {
                data1: 0x4a91_4f53,
                data2: 0x543f,
                data3: 0x46f9,
                data4: [0xb9, 0x9b, 0xd3, 0x49, 0xa5, 0xaa, 0xf3, 0xe5],
            }
        );
        assert_eq!(
            CONTAINER,
            Guid {
                data1: 0x3bb8_0aa8,
                data2: 0x3343,
                data3: 0x4e3d,
                data4: [0x99, 0xd4, 0xcc, 0xc1, 0x66, 0xf4, 0x62, 0x00],
            }
        );
        assert_eq!(SERIAL_BASE, 0x4c57_0000);
        assert_eq!(PAGE_MAGIC, u64::from_le_bytes(*b"lowlatfc"));
        // The block the driver generated before, byte for byte but the
        // checksum, which the block now carries right.
        let mut before: [u8; 128] = [
            0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00, 0x32, 0xec, 0x01, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x01, 36, 0x01, 0x04, 0xa5, 0x00, 0x00, 0x78, 0x06, 0xee, 0x91, 0xa3, 0x54,
            0x4c, 0x99, 0x26, 0x0f, 0x50, 0x54, 0x00, 0x00, 0x00, 0x01, 0x01, 0x01, 0x01, 0x01,
            0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x02, 0x3a, 0x80,
            0x18, 0x71, 0x38, 0x2d, 0x40, 0x58, 0x2c, 0x45, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x1e, 0x00, 0x00, 0x00, 0xfc, 0x00, b'L', b'o', b'w', b'l', b'a', b't', b'D', b'i',
            b's', b'p', b'l', b'a', b'y', 0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00,
        ];
        before[127] = 0u8.wrapping_sub(edid::sum(&before[..127]));
        assert_eq!(EDID, &before[..]);

        assert_eq!(
            (input::ID, input::PROVIDER, input::DEVICE),
            ("lowlat", "Lowlat", "Lowlat Virtual Input")
        );
        assert_eq!(
            (
                input::LIBRARY,
                input::INF,
                input::CATALOG,
                input::CERTIFICATE
            ),
            (
                "lowlat_hid.dll",
                "lowlat-hid.inf",
                "lowlat-hid.cat",
                "lowlat-hid.cer"
            )
        );
        assert_eq!(
            (
                input::HARDWARE_ID,
                input::SERVICE,
                input::LOG,
                input::COMPANION_INF
            ),
            (
                "Root\\lowlat-hid",
                "LowlatHid",
                "lowlat-hid",
                "lowlat-idd.inf"
            )
        );
        assert_eq!(input::CONTEXT, c"lowlat-hid handle");
        assert_eq!(
            input::CONTROL_INTERFACE,
            Guid {
                data1: 0x32e3_9bbb,
                data2: 0xdbe3,
                data3: 0x4fb6,
                data4: [0xab, 0x8c, 0x78, 0x3e, 0x5b, 0x21, 0x4f, 0xed],
            }
        );
        assert_eq!(
            input::CONTAINER,
            Guid {
                data1: 0x5234_c8de,
                data2: 0xc3e9,
                data3: 0x4480,
                data4: [0x8d, 0x9c, 0xf7, 0x55, 0x4c, 0x66, 0x00, 0x00],
            }
        );
    }

    /// **Whatever the brand, its names hold together**: the EDID valid with
    /// a maker of three letters whose first two are the serial's upper half,
    /// both drivers' names from the one short name, each the other's
    /// companion, and four GUIDs, not fewer.
    #[test]
    fn any_brands_identity_holds_together() {
        use display::*;
        assert!(edid::valid(EDID));
        let maker = edid::maker(EDID).expect("three letters");
        assert_eq!(
            SERIAL_BASE,
            (u32::from(maker[0]) << 24) | (u32::from(maker[1]) << 16)
        );
        assert!(
            ID.bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        );
        assert_eq!(FOLDER.split_once('\\'), Some((ID, "display")));
        assert!(LIBRARY.starts_with(ID) && INF.starts_with(ID) && CATALOG.starts_with(ID));
        assert!(HARDWARE_ID.starts_with("Root\\") && HARDWARE_ID.ends_with("-vdd"));
        assert!(SERVICE.eq_ignore_ascii_case(&std::format!("{ID}vdd")));
        let magic = PAGE_MAGIC.to_le_bytes();
        assert_eq!(&magic[6..], b"fc");
        assert!(
            ID.as_bytes()
                .starts_with(magic[..6].split(|b| *b == 0).next().expect("a head"))
        );
        assert_eq!((input::ID, input::PROVIDER), (ID, PROVIDER));
        assert!(input::HARDWARE_ID.ends_with("-hid") && input::LIBRARY.starts_with(ID));
        assert!(input::SERVICE.eq_ignore_ascii_case(&std::format!("{ID}hid")));
        assert_eq!((COMPANION_INF, input::COMPANION_INF), (input::INF, INF));
        assert_eq!(
            input::CONTEXT.to_str(),
            Ok(std::format!("{ID}-hid handle").as_str())
        );
        let guids = [
            CONTROL_INTERFACE,
            CONTAINER,
            input::CONTROL_INTERFACE,
            input::CONTAINER,
        ];
        for (at, one) in guids.iter().enumerate() {
            assert!(!guids[..at].contains(one), "GUID {at} given twice");
        }
    }
}
