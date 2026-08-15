//! RFC 1982 serial number arithmetic for 32-bit sequence numbers.
//!
//! A naive `a > b` inverts at the wrap boundary, which arrives after roughly
//! fifteen days of continuous high-rate streaming. **Every** comparison of a
//! sequence, base, or cumulative acknowledgement goes through these functions.
//!
//! Valid while the two values are within 2^31 of each other, which always holds
//! here because in-flight windows are bounded far below that.

/// `a` is after `b`.
pub const fn gt(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) > 0
}

/// `a` is at or after `b`.
pub const fn ge(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) >= 0
}

/// `a` is before `b`.
pub const fn lt(a: u32, b: u32) -> bool {
    gt(b, a)
}

/// `a` is at or before `b`.
pub const fn le(a: u32, b: u32) -> bool {
    ge(b, a)
}

/// Distance from `a` forward to `b`, assuming `b` is at or after `a`.
pub const fn distance(a: u32, b: u32) -> u32 {
    b.wrapping_sub(a)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_ordering() {
        assert!(gt(5, 4));
        assert!(!gt(4, 5));
        assert!(lt(4, 5));
        assert!(ge(5, 5));
        assert!(le(5, 5));
    }

    /// The regression test. A naive comparison says `0 > u32::MAX` is false;
    /// serial arithmetic says it is true, which is what the wire means.
    #[test]
    fn ordering_survives_the_wrap_boundary() {
        assert!(gt(0, u32::MAX), "0 must be after MAX across the wrap");
        assert!(gt(1, u32::MAX));
        assert!(lt(u32::MAX, 0));
        assert!(lt(u32::MAX - 5, 5));

        // The naive form, kept as a witness to what we are avoiding. The
        // values are made opaque so the compiler cannot fold the comparison
        // away and so it reads as the runtime mistake it actually is.
        let zero = core::hint::black_box(0u32);
        let max = core::hint::black_box(u32::MAX);
        assert!(
            !(zero > max),
            "the naive comparison inverts across the wrap; that is why gt() exists"
        );
    }

    #[test]
    fn distance_across_the_wrap() {
        assert_eq!(distance(u32::MAX, 0), 1);
        assert_eq!(distance(u32::MAX - 2, 2), 5);
        assert_eq!(distance(10, 20), 10);
    }

    #[test]
    fn equality_is_neither_before_nor_after() {
        assert!(!gt(7, 7));
        assert!(!lt(7, 7));
        assert!(ge(7, 7));
        assert!(le(7, 7));
    }
}
