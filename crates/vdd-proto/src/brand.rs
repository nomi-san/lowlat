//! The display driver's identity as the build was given it
//! (docs/11-drivers.md section 1.10): lowlat's own, or the brand file
//! `LOWLAT_VDD_BRAND` named, read by the build script.
//!
//! **Every name the driver, its package and its controller use comes from
//! here**, so a company's build carries its own throughout -- the names a
//! person sees, and the identifiers the system and the controller know the
//! driver by, which two drivers on one machine must not share -- and a
//! controller built with one brand finds only that brand's driver.

use crate::Guid;

include!(concat!(env!("OUT_DIR"), "/brand.rs"));

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edid_block;

    /// **lowlat's own build keeps every identity it had** before a brand
    /// could replace it -- the files, the device, the interfaces, the
    /// monitors' EDID to the byte -- so an installed driver updates in place
    /// and the system keeps its displays' arrangement.
    #[test]
    #[cfg_attr(lowlat_vdd_brand, ignore = "the build carries another brand")]
    fn lowlats_own_build_keeps_its_identity() {
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
            (HARDWARE_ID, SERVICE, GROUP),
            ("Root\\lowlat-vdd", "LowlatVdd", "LowlatVddGroup")
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
        before[127] = 0u8.wrapping_sub(edid_block::sum(&before[..127]));
        assert_eq!(EDID, &before[..]);
    }

    /// **Whatever the brand, its names hold together**: the EDID valid with
    /// a maker of three letters whose first two are the serial's upper half,
    /// the derived names from the short one, the adapter's names within what
    /// a setup information quotes.
    #[test]
    fn any_brands_identity_holds_together() {
        assert!(edid_block::valid(EDID));
        let maker = edid_block::maker(EDID).expect("three letters");
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
        assert_ne!(CONTROL_INTERFACE, CONTAINER);
    }
}
