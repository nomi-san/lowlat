//! A peer's controller, as the kernel's own pad layout.
//!
//! **One layout, the Xbox 360 pad** (docs/00-overview.md D12). A peer sends
//! that button set whatever it is holding, so a second emulation would buy
//! identity rather than capability.
//!
//! Everything here is a pure mapping from what a peer says to what a device
//! reports. The device itself is in [`crate::uinput`].

/// How many pads one guest may hold.
///
/// **The peer's pad identifier is arbitrary** (docs/01-protocol.md 11.1), so
/// without a bound a peer that varies the field creates a device per value.
pub const MAX_PADS: usize = 4;

/// The whole-pad message's bit for each button.
///
/// One representation is kept for both messages, and this is it: the
/// per-button message converts into these bits on the way in, so a guest that
/// uses both forms has one held state rather than two that disagree.
pub mod bit {
    pub const DPAD_UP: u16 = 0x0001;
    pub const DPAD_DOWN: u16 = 0x0002;
    pub const DPAD_LEFT: u16 = 0x0004;
    pub const DPAD_RIGHT: u16 = 0x0008;
    pub const START: u16 = 0x0010;
    pub const BACK: u16 = 0x0020;
    pub const LEFT_THUMB: u16 = 0x0040;
    pub const RIGHT_THUMB: u16 = 0x0080;
    pub const LEFT_SHOULDER: u16 = 0x0100;
    pub const RIGHT_SHOULDER: u16 = 0x0200;
    pub const GUIDE: u16 = 0x0400;
    /// **No key code and no index.** A peer can report a touchpad press and
    /// the pad being emulated has no such button, so it is dropped rather than
    /// mapped onto something plausible.
    pub const TOUCHPAD: u16 = 0x0800;
    pub const A: u16 = 0x1000;
    pub const B: u16 = 0x2000;
    pub const X: u16 = 0x4000;
    pub const Y: u16 = 0x8000;
}

/// Kernel button codes.
pub mod key {
    pub const SOUTH: u16 = 0x130;
    pub const EAST: u16 = 0x131;
    pub const NORTH: u16 = 0x133;
    pub const WEST: u16 = 0x134;
    pub const TL: u16 = 0x136;
    pub const TR: u16 = 0x137;
    pub const SELECT: u16 = 0x13a;
    pub const START: u16 = 0x13b;
    pub const MODE: u16 = 0x13c;
    pub const THUMBL: u16 = 0x13d;
    pub const THUMBR: u16 = 0x13e;
}

/// Kernel axis codes.
pub mod axis {
    pub const X: u16 = 0x00;
    pub const Y: u16 = 0x01;
    pub const Z: u16 = 0x02;
    pub const RX: u16 = 0x03;
    pub const RY: u16 = 0x04;
    pub const RZ: u16 = 0x05;
    pub const HAT0X: u16 = 0x10;
    pub const HAT0Y: u16 = 0x11;
}

/// Sticks report this far from centre in each direction.
pub const STICK_RANGE: i32 = 32767;
/// Triggers report this far.
pub const TRIGGER_RANGE: i32 = 255;

/// The bit a per-button message's index names, or `None`.
///
/// **The per-button message indexes buttons in one order and the whole-pad
/// message packs them in another**, and the two do not line up at any point.
/// So this is a table rather than arithmetic, and a host that shifts one into
/// the other produces a controller whose face buttons are its direction pad.
#[must_use]
pub fn bit_for_index(index: u32) -> Option<u16> {
    let bit = match index {
        0 => bit::A,
        1 => bit::B,
        2 => bit::X,
        3 => bit::Y,
        4 => bit::BACK,
        5 => bit::GUIDE,
        6 => bit::START,
        7 => bit::LEFT_THUMB,
        8 => bit::RIGHT_THUMB,
        9 => bit::LEFT_SHOULDER,
        10 => bit::RIGHT_SHOULDER,
        11 => bit::DPAD_UP,
        12 => bit::DPAD_DOWN,
        13 => bit::DPAD_LEFT,
        14 => bit::DPAD_RIGHT,
        _ => return None,
    };
    Some(bit)
}

/// The kernel button code a bit names, or `None` when the pad has no such
/// button.
///
/// The direction pad is absent on purpose: it is reported as two axes rather
/// than as four buttons, because that is how the pad being emulated reports
/// it and consumers key on that.
#[must_use]
pub fn key_for_bit(button: u16) -> Option<u16> {
    let code = match button {
        bit::A => key::SOUTH,
        bit::B => key::EAST,
        bit::X => key::NORTH,
        bit::Y => key::WEST,
        bit::LEFT_SHOULDER => key::TL,
        bit::RIGHT_SHOULDER => key::TR,
        bit::BACK => key::SELECT,
        bit::START => key::START,
        bit::GUIDE => key::MODE,
        bit::LEFT_THUMB => key::THUMBL,
        bit::RIGHT_THUMB => key::THUMBR,
        _ => return None,
    };
    Some(code)
}

/// Every bit that maps to a button, so a change can be walked without
/// inventing an ordering.
pub const KEYED_BITS: [u16; 11] = [
    bit::A,
    bit::B,
    bit::X,
    bit::Y,
    bit::LEFT_SHOULDER,
    bit::RIGHT_SHOULDER,
    bit::BACK,
    bit::START,
    bit::GUIDE,
    bit::LEFT_THUMB,
    bit::RIGHT_THUMB,
];

/// The direction pad's horizontal axis value for a button state.
#[must_use]
pub fn hat_x(buttons: u16) -> i32 {
    axis_of(buttons, bit::DPAD_LEFT, bit::DPAD_RIGHT)
}

/// The direction pad's vertical axis value.
///
/// **Up is negative**, as it is on every kernel pad axis.
#[must_use]
pub fn hat_y(buttons: u16) -> i32 {
    axis_of(buttons, bit::DPAD_UP, bit::DPAD_DOWN)
}

fn axis_of(buttons: u16, negative: u16, positive: u16) -> i32 {
    match (buttons & negative != 0, buttons & positive != 0) {
        // Both at once is what a peer sends when a direction pad is being
        // rocked between two directions, and the pad it is emulating cannot
        // express it. Centre rather than pick one.
        (true, true) | (false, false) => 0,
        (true, false) => -1,
        (false, true) => 1,
    }
}

/// A stick position, as the axis reports it.
///
/// **The vertical axes are inverted and the horizontal ones are not.** A peer
/// reports a stick pushed away from the player as positive, the way the pad's
/// own protocol does; a kernel axis reports it as negative, the way every
/// joystick does. Passing the value through unchanged gives a controller that
/// steers correctly and looks the wrong way up, which reads as a game's own
/// inverted-look setting rather than as a bug here.
#[must_use]
pub fn stick(value: i16, vertical: bool) -> i32 {
    let value = i32::from(value);
    if vertical {
        // Negating the far end would leave the range one short of itself, so
        // the clamp comes first.
        -value.max(-STICK_RANGE)
    } else {
        value
    }
}

/// A trigger position, as the axis reports it.
///
/// **The whole-pad message and the per-axis message do not agree on units**:
/// one carries a byte and the other carries the same pull as a signed
/// sixteen-bit value, so the second is scaled rather than truncated. A shift
/// would be close and would never quite reach the end of the axis.
#[must_use]
pub fn trigger_from_axis(value: i16) -> i32 {
    (TRIGGER_RANGE * i32::from(value) / STICK_RANGE).clamp(0, TRIGGER_RANGE)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The two button enumerations do not line up anywhere.** If they ever
    /// appear to, somebody has mapped one onto the other.
    #[test]
    fn the_two_button_enumerations_are_unrelated() {
        let by_index: Vec<u16> = (0..=14).filter_map(bit_for_index).collect();
        assert_eq!(by_index.len(), 15, "an index lost its bit");
        // Sorted by bit, the order is nothing like the index order.
        let mut sorted = by_index.clone();
        sorted.sort_unstable();
        assert_ne!(by_index, sorted);
        // The first index is a face button and the first bit is a direction.
        assert_eq!(bit_for_index(0), Some(bit::A));
        assert_eq!(bit_for_index(11), Some(bit::DPAD_UP));
    }

    #[test]
    fn an_index_the_pad_does_not_have_is_dropped() {
        assert_eq!(bit_for_index(15), None);
        assert_eq!(bit_for_index(u32::MAX), None);
    }

    /// The touchpad press exists in the peer's vocabulary and not on this pad.
    #[test]
    fn the_touchpad_press_maps_to_nothing() {
        assert_eq!(key_for_bit(bit::TOUCHPAD), None);
        assert!(!KEYED_BITS.contains(&bit::TOUCHPAD));
    }

    /// The direction pad is axes, not buttons, so its bits have no key.
    #[test]
    fn the_direction_pad_is_not_buttons() {
        for bit in [
            bit::DPAD_UP,
            bit::DPAD_DOWN,
            bit::DPAD_LEFT,
            bit::DPAD_RIGHT,
        ] {
            assert_eq!(key_for_bit(bit), None, "{bit:#x}");
        }
        assert_eq!(hat_y(bit::DPAD_UP), -1);
        assert_eq!(hat_y(bit::DPAD_DOWN), 1);
        assert_eq!(hat_x(bit::DPAD_LEFT), -1);
        assert_eq!(hat_x(bit::DPAD_RIGHT), 1);
        assert_eq!(hat_x(0), 0);
        // Opposite directions at once centre rather than pick.
        assert_eq!(hat_x(bit::DPAD_LEFT | bit::DPAD_RIGHT), 0);
        assert_eq!(hat_y(bit::DPAD_UP | bit::DPAD_DOWN), 0);
    }

    /// Every bit that has a key is in the walk list, and nothing else is.
    #[test]
    fn the_walk_list_is_exactly_the_keyed_bits() {
        for bit in KEYED_BITS {
            assert!(key_for_bit(bit).is_some(), "{bit:#x} has no key");
        }
        let mut counted = 0;
        for shift in 0..16 {
            let bit = 1u16 << shift;
            if key_for_bit(bit).is_some() {
                assert!(KEYED_BITS.contains(&bit), "{bit:#x} is not walked");
                counted += 1;
            }
        }
        assert_eq!(counted, KEYED_BITS.len());
    }

    /// **Vertical inverts, horizontal does not.** The commonest way to get
    /// this wrong is to invert both, which passes a casual look because
    /// left and right still feel right until somebody aims.
    #[test]
    fn the_vertical_stick_axes_invert_and_the_horizontal_ones_do_not() {
        assert_eq!(stick(20_000, false), 20_000);
        assert_eq!(stick(-20_000, false), -20_000);
        assert_eq!(stick(20_000, true), -20_000);
        assert_eq!(stick(-20_000, true), 20_000);
        assert_eq!(stick(0, true), 0);
    }

    /// Negating the far end of a signed range overflows it by one.
    #[test]
    fn the_far_end_of_an_inverted_axis_stays_in_range() {
        assert_eq!(stick(i16::MIN, true), STICK_RANGE);
        assert_eq!(stick(i16::MAX, true), -STICK_RANGE);
        assert_eq!(stick(i16::MIN, false), i32::from(i16::MIN));
    }

    /// **The obvious shortcut agrees almost everywhere.** Dividing by 128
    /// instead of scaling by 255/32767 matches at both ends and at the
    /// midpoint, so a test picking round numbers cannot tell them apart. The
    /// values below are chosen where they differ, because the scale is the
    /// one the far side computes and a trigger that is one step out of step
    /// with it is not something anybody would ever notice or report.
    #[test]
    fn a_trigger_uses_the_scale_and_not_the_shortcut() {
        assert_eq!(trigger_from_axis(0), 0);
        assert_eq!(trigger_from_axis(i16::MAX), TRIGGER_RANGE);
        for (pull, scaled, shortcut) in [(16_000, 124, 125), (20_000, 155, 156), (32_640, 254, 255)]
        {
            assert_eq!(trigger_from_axis(pull), scaled);
            assert_ne!(scaled, shortcut, "the fixture stopped distinguishing them");
        }
        // A pull cannot be negative, whatever arrives.
        assert_eq!(trigger_from_axis(-1), 0);
        assert_eq!(trigger_from_axis(i16::MIN), 0);
    }
}
