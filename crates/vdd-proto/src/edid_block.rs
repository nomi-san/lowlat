//! An EDID's blocks: what makes one valid, and the block a build generates
//! for a brand that dumps none (docs/11-drivers.md sections 1.3 and 1.10).
//!
//! **Shared with the build script**, which generates the driver's own block
//! here and checks a dumped one; the crate checks every EDID a file or a
//! request hands it with the same code. So nothing here names the crate.

use core::ops::Range;

/// One block.
pub(crate) const LEN: usize = 128;
/// The most an EDID here carries: the base block and one extension.
pub(crate) const MAX_LEN: usize = 2 * LEN;

const HEADER: [u8; 8] = [0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00];
/// The maker's three letters, packed into two bytes.
const MAKER: Range<usize> = 8..10;
/// The product's number, low byte first.
const PRODUCT: Range<usize> = 10..12;
/// The extension count, in the base block.
const EXTENSIONS: usize = 126;
/// The name a person sees: the text of the base block's second descriptor.
const NAME: Range<usize> = 77..90;
/// The longest name: its descriptor's thirteen characters.
const MAX_NAME: usize = NAME.end - NAME.start;

/// The generated block, before its maker, product and name.
const TEMPLATE: [u8; LEN] = [
    // Header.
    0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00, //
    // Manufacturer and product (the brand's), serial (patched per slot).
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
    // Week 1 of 2026; version 1.4.
    0x01, 36, 0x01, 0x04, //
    // Digital input, eight bits a component, over DisplayPort; size unknown,
    // so no physical size scales the desktop; gamma 2.2; sRGB, the preferred
    // timing native.
    0xa5, 0x00, 0x00, 0x78, 0x06, //
    // The sRGB primaries and white point.
    0xee, 0x91, 0xa3, 0x54, 0x4c, 0x99, 0x26, 0x0f, 0x50, 0x54, //
    // No established timings; the eight standard timings unused.
    0x00, 0x00, 0x00, //
    0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, //
    0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, //
    // Detailed timing: 1920x1080 at 60, 148.5 MHz, positive syncs.
    0x02, 0x3a, 0x80, 0x18, 0x71, 0x38, 0x2d, 0x40, 0x58, 0x2c, 0x45, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x1e, //
    // The name a person sees (the brand's).
    0x00, 0x00, 0x00, 0xfc, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, //
    // Two descriptors left unused.
    0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, //
    0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, //
    // No extension; the checksum.
    0x00, 0x00,
];

/// The sum of a block's bytes, which a valid block's checksum makes zero.
pub(crate) fn sum(block: &[u8]) -> u8 {
    block.iter().fold(0u8, |sum, b| sum.wrapping_add(*b))
}

/// Whether these bytes are an EDID of one block or two: the header first,
/// each block's checksum right, and the extension count saying how many
/// follow.
pub(crate) fn valid(bytes: &[u8]) -> bool {
    (bytes.len() == LEN || bytes.len() == MAX_LEN)
        && bytes.get(..HEADER.len()) == Some(&HEADER[..])
        && bytes.chunks(LEN).all(|block| sum(block) == 0)
        && bytes
            .get(EXTENSIONS)
            .is_some_and(|count| usize::from(*count) == bytes.len() / LEN - 1)
}

/// Whether a maker is three capital letters, as a block packs them.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn maker_valid(letters: &[u8]) -> bool {
    letters.len() == 3 && letters.iter().all(u8::is_ascii_uppercase)
}

/// Whether a name fits its descriptor: one to thirteen printable characters.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn name_valid(name: &[u8]) -> bool {
    (1..=MAX_NAME).contains(&name.len()) && name.iter().all(|c| (0x20..=0x7e).contains(c))
}

/// The three letters a block's maker packs, five bits each, or `None` where
/// one is not a letter.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn maker(block: &[u8]) -> Option<[u8; 3]> {
    let packed = u16::from_be_bytes(block.get(MAKER)?.try_into().ok()?);
    let letter = |shift: u16| {
        let value = u8::try_from((packed >> shift) & 31).ok()?;
        (1..=26).contains(&value).then_some(b'@' + value)
    };
    Some([letter(10)?, letter(5)?, letter(0)?])
}

/// Whether two EDIDs name the same monitor to the system: the same maker and
/// product.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn same_monitor(a: &[u8], b: &[u8]) -> bool {
    let range = MAKER.start..PRODUCT.end;
    a.get(range.clone())
        .is_some_and(|a| Some(a) == b.get(range))
}

/// The block generated for a maker, a product and a name, its checksum
/// right; `None` for a maker or a name [`maker_valid`] or [`name_valid`]
/// refuses. A name shorter than its field ends with a line feed, the rest
/// spaces.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn generate(maker: &[u8], product: u16, name: &[u8]) -> Option<[u8; LEN]> {
    if !maker_valid(maker) || !name_valid(name) {
        return None;
    }
    let mut packed = 0u16;
    for letter in maker {
        packed = (packed << 5) | u16::from(letter - b'@');
    }
    let mut block = TEMPLATE;
    block.get_mut(MAKER)?.copy_from_slice(&packed.to_be_bytes());
    block
        .get_mut(PRODUCT)?
        .copy_from_slice(&product.to_le_bytes());
    let field = block.get_mut(NAME)?;
    field.fill(b' ');
    field.get_mut(..name.len())?.copy_from_slice(name);
    if let Some(end) = field.get_mut(name.len()) {
        *end = b'\n';
    }
    let checksum = 0u8.wrapping_sub(sum(block.get(..LEN - 1)?));
    *block.get_mut(LEN - 1)? = checksum;
    Some(block)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The name descriptor's text in a block.
    fn name_of(block: &[u8]) -> &[u8] {
        let descriptor = &block[NAME.start - 5..NAME.end];
        assert_eq!(
            &descriptor[..5],
            &[0x00, 0x00, 0x00, 0xfc, 0x00],
            "the name's tag"
        );
        &descriptor[5..]
    }

    #[test]
    fn a_generated_block_is_valid_and_carries_its_maker_product_and_name() {
        let block = generate(b"ACM", 0x1a2b, b"Acme Display").expect("generated");
        assert!(valid(&block));
        assert_eq!(maker(&block), Some(*b"ACM"));
        assert_eq!(&block[PRODUCT], &[0x2b, 0x1a], "low byte first");
        assert_eq!(name_of(&block), b"Acme Display\n");
        // Everything else is the template's.
        for (i, (got, template)) in block.iter().zip(TEMPLATE.iter()).enumerate() {
            if !MAKER.contains(&i) && !PRODUCT.contains(&i) && !NAME.contains(&i) && i != LEN - 1 {
                assert_eq!(got, template, "byte {i}");
            }
        }
    }

    /// **The maker packs five bits a letter**, and reads back the same: the
    /// letters at each end of the alphabet, and lowlat's own `LWL`, which a
    /// block has carried as `32 ec` since its first monitor.
    #[test]
    fn the_maker_packs_three_letters_and_reads_back() {
        for letters in [*b"AAA", *b"ZZZ", *b"ACM", *b"LWL"] {
            let block = generate(&letters, 1, b"x").expect("generated");
            assert_eq!(maker(&block), Some(letters));
            assert_eq!(
                u16::from_be_bytes([block[8], block[9]]) >> 15,
                0,
                "the top bit is clear"
            );
        }
        assert_eq!(
            &generate(b"LWL", 1, b"x").expect("generated")[MAKER],
            &[0x32, 0xec]
        );
    }

    /// **A name pads to its field**: thirteen characters fill it with no line
    /// feed, a shorter one ends with a line feed and spaces.
    #[test]
    fn a_name_fills_its_descriptor_or_ends_with_a_line_feed() {
        let full = generate(b"LWL", 1, b"LowlatDisplay").expect("generated");
        assert_eq!(name_of(&full), b"LowlatDisplay");
        let one = generate(b"LWL", 1, b"A").expect("generated");
        assert_eq!(name_of(&one), b"A\n           ");
        let twelve = generate(b"LWL", 1, b"Twelve chars").expect("generated");
        assert_eq!(name_of(&twelve), b"Twelve chars\n");
    }

    #[test]
    fn a_maker_or_name_out_of_form_is_refused() {
        for maker in [&b"AC"[..], b"ACME", b"acm", b"A1M", b"AC@", b"AC["] {
            assert!(generate(maker, 1, b"x").is_none(), "{maker:?}");
        }
        for name in [&b""[..], b"Fourteen chars", b"tab\there", b"caf\xc3\xa9"] {
            assert!(generate(b"ACM", 1, name).is_none(), "{name:?}");
        }
        assert!(generate(b"ACM", 1, b"Thirteen char").is_some(), "thirteen");
    }

    #[test]
    fn a_block_whose_maker_is_no_letter_reads_none() {
        let mut block = generate(b"ACM", 1, b"x").expect("generated");
        block[8] = 0;
        block[9] &= 0x1f;
        assert_eq!(maker(&block), None, "a zero first letter");
        block[8] = 0x6f;
        assert_eq!(maker(&block), None, "a letter past Z");
        assert_eq!(maker(&block[..9]), None, "no maker at all");
    }

    #[test]
    fn the_same_monitor_is_the_same_maker_and_product() {
        let a = generate(b"ACM", 1, b"One").expect("generated");
        let b = generate(b"ACM", 1, b"Two").expect("generated");
        let c = generate(b"ACM", 2, b"One").expect("generated");
        let d = generate(b"ACN", 1, b"One").expect("generated");
        assert!(same_monitor(&a, &b), "the name is not the identity");
        assert!(!same_monitor(&a, &c), "another product");
        assert!(!same_monitor(&a, &d), "another maker");
        assert!(!same_monitor(&a, &a[..11]), "a block too short");
    }
}
