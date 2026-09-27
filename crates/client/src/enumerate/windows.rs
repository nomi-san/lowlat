//! The decoder table on Windows: slot 0 is the machine's own codec library.
//! The device decoders' slots come with their steps of
//! docs/impl-plan-windows.md, each ahead of this one as the automatic order
//! puts them.

use super::{Available, codec_library};

/// The system's own decode interface's slots: none yet.
pub const OPEN_SLOTS: u32 = 0;
/// The vendor's slots: none yet.
pub const VENDOR_SLOTS: u32 = 0;
/// The table's length: the codec library's one slot.
pub const SLOTS: u32 = OPEN_SLOTS + VENDOR_SLOTS + 1;

/// The slot `slot` of the table, probed now; none past the table's end.
pub fn probe(slot: u32) -> Option<Available> {
    (slot == SLOTS - 1).then(codec_library)
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use super::*;

    /// **Every slot answers, in the table's order, and nothing past it**: the
    /// one slot is the codec library, available with what it decodes or
    /// unavailable with why, and its label and words follow the grammar
    /// every platform's table uses.
    #[test]
    fn every_slot_answers_and_the_table_ends() {
        let rows: Vec<Available> = (0..SLOTS)
            .map(|slot| probe(slot).expect("a slot"))
            .collect();
        assert!(probe(SLOTS).is_none());
        assert!(probe(u32::MAX).is_none());
        for row in &rows {
            println!("{row:?}");
            assert_eq!(row.backend, Backend::Software);
            assert!(
                row.name == "libavcodec" || row.name.starts_with("libavcodec ["),
                "a label off the grammar: {}",
                row.name
            );
            assert!(!row.driver.is_empty(), "a slot without its words");
            assert!(!row.handle, "the codec library hands out no handle");
            if row.available {
                assert!(row.caps.any());
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
            } else {
                assert!(!row.caps.any(), "an unavailable slot with a capability");
            }
        }
    }
}
