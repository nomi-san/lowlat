//! Peer key codes to kernel key codes.
//!
//! A peer reports a physical key by its usage code, not by the character the
//! key produces (docs/01-protocol.md section 11.1). Layout belongs to the far
//! side; this host injects the key that was pressed and lets the local layout
//! decide what it means.
//!
//! **The table below is the one the kernel itself uses for a real keyboard.**
//! It is copied from the kernel's own translation of the same usage codes,
//! unaltered and unreordered, so an injected key and a key typed on hardware
//! plugged into this machine travel the identical path from that point on. Any
//! other table -- including a correct-looking one written from the standard --
//! is a second opinion about a question the kernel has already answered, and
//! the two would diverge on exactly the entries nobody tests.
//!
//! Zero means the usage code names no key here. It covers three cases that do
//! not need telling apart: a slot the standard reserves, a key the kernel
//! declines to map, and a code past the end of the table.

/// Usage codes zero through 255, as the kernel translates them.
///
/// Indexed by usage code. Generated, not transcribed.
#[rustfmt::skip]
const KERNEL_KEY: [u16; 256] = [
       0,    0,    0,    0,   30,   48,   46,   32,   18,   33,   34,   35,   23,   36,   37,   38,  // 0
      50,   49,   24,   25,   16,   19,   31,   20,   22,   47,   17,   45,   21,   44,    2,    3,  // 16
       4,    5,    6,    7,    8,    9,   10,   11,   28,    1,   14,   15,   57,   12,   13,   26,  // 32
      27,   43,   43,   39,   40,   41,   51,   52,   53,   58,   59,   60,   61,   62,   63,   64,  // 48
      65,   66,   67,   68,   87,   88,   99,   70,  119,  110,  102,  104,  111,  107,  109,  106,  // 64
     105,  108,  103,   69,   98,   55,   74,   78,   96,   79,   80,   81,   75,   76,   77,   71,  // 80
      72,   73,   82,   83,   86,  127,  116,  117,  183,  184,  185,  186,  187,  188,  189,  190,  // 96
     191,  192,  193,  194,  134,  138,  130,  132,  128,  129,  131,  137,  133,  135,  136,  113,  // 112
     115,  114,    0,    0,    0,  121,    0,   89,   93,  124,   92,   94,   95,    0,    0,    0,  // 128
     122,  123,   90,   91,   85,    0,    0,    0,    0,    0,    0,    0,  111,    0,    0,    0,  // 144
       0,    0,    0,    0,    0,    0,    0,    0,    0,    0,    0,    0,    0,    0,    0,    0,  // 160
       0,    0,    0,    0,    0,    0,  179,  180,    0,    0,    0,    0,    0,    0,    0,    0,  // 176
       0,    0,    0,    0,    0,    0,    0,    0,    0,    0,    0,    0,    0,    0,    0,    0,  // 192
       0,    0,    0,    0,    0,    0,    0,    0,  111,    0,    0,    0,    0,    0,    0,    0,  // 208
      29,   42,   56,  125,   97,   54,  100,  126,  164,  166,  165,  163,  161,  115,  114,  113,  // 224
     150,  158,  159,  128,  136,  177,  178,  176,  142,  152,  173,  140,    0,    0,    0,    0,  // 240
];

/// The six codes a peer sends above the usage range.
///
/// These are not usage codes at all. The peer's enumeration continues past the
/// end of the standard's keyboard page with its own values for the media keys,
/// so they have no entry in the table above and are matched separately rather
/// than being written into it. Widening the table to 264 entries would put
/// them at indices that look like usage codes and are not.
const EXTRA_FIRST: u16 = 258;
const EXTRA: [u16; 6] = [
    163, // next track
    165, // previous track
    166, // stop
    164, // play and pause
    113, // mute
    226, // media select
];

/// Codes whose kernel translation is not the one a peer means by them.
///
/// **One entry, and it is not a correction to the table.** The standard has
/// two codes near the context-menu key: one it calls "application", which is
/// the key physically present between the right modifiers on a PC keyboard,
/// and one it calls "menu", which names the abstract action. The kernel
/// translates them apart and is right to -- "menu" reaches a properties code
/// that nothing on a desktop binds. Peers do not: they carry both codes and
/// send either one for the same physical key, so a host that honours the
/// distinction has a key that works from one peer and does nothing from
/// another. Both land on the context-menu key here.
const OVERRIDE: [(u16, u16); 1] = [(118, 127)];

/// The kernel key code for a peer's key code, or `None` when it names none.
#[must_use]
pub fn key_code(usage: u16) -> Option<u16> {
    if let Some(&(_, code)) = OVERRIDE.iter().find(|&&(from, _)| from == usage) {
        return Some(code);
    }
    let code = match usage {
        0..=255 => {
            // The index is in range by the arm's own bound, and the fallible
            // form keeps the crate free of panicking indexing.
            *KERNEL_KEY.get(usize::from(usage))?
        }
        _ => {
            let offset = usize::from(usage.checked_sub(EXTRA_FIRST)?);
            *EXTRA.get(offset)?
        }
    };
    (code != 0).then_some(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Spot checks across the table's shape rather than a re-transcription of
    /// it: one letter, one digit, the two ends of the modifier block, a
    /// keypad key, and a key whose usage code and kernel code are unrelated.
    #[test]
    fn known_keys_land_where_the_kernel_puts_them() {
        assert_eq!(key_code(4), Some(30)); // a
        assert_eq!(key_code(29), Some(44)); // z
        assert_eq!(key_code(30), Some(2)); // 1
        assert_eq!(key_code(40), Some(28)); // enter
        assert_eq!(key_code(41), Some(1)); // escape
        assert_eq!(key_code(44), Some(57)); // space
        assert_eq!(key_code(224), Some(29)); // left control
        assert_eq!(key_code(231), Some(126)); // right meta
        assert_eq!(key_code(89), Some(79)); // keypad 1
        assert_eq!(key_code(70), Some(99)); // print screen
    }

    /// **The two backslash usage codes map to one key**, which is the entry
    /// most likely to be "corrected" by someone reading the standard. The
    /// second is the non-US key next to the return key on those layouts, and
    /// the kernel gives both the same code.
    #[test]
    fn both_backslash_usage_codes_map_to_one_key() {
        assert_eq!(key_code(49), key_code(50));
        assert_eq!(key_code(49), Some(43));
    }

    #[test]
    fn the_media_keys_above_the_usage_range_are_reached() {
        assert_eq!(key_code(258), Some(163));
        assert_eq!(key_code(263), Some(226));
        assert_eq!(key_code(257), None);
        assert_eq!(key_code(264), None);
        assert_eq!(key_code(u16::MAX), None);
    }

    /// The reserved slots and the error codes a keyboard reports on rollover
    /// are not keys, and injecting anything for them would produce a
    /// keystroke out of a report that says a keystroke was lost.
    #[test]
    fn the_reserved_and_error_codes_name_no_key() {
        for usage in 0..=3 {
            assert_eq!(key_code(usage), None, "usage {usage}");
        }
    }

    /// A count, so a table that loses or gains entries in an edit fails here
    /// rather than in whichever key stopped working.
    #[test]
    fn the_table_maps_the_number_of_codes_it_did() {
        let mapped = (0..=u16::MAX).filter(|&u| key_code(u).is_some()).count();
        assert_eq!(mapped, 176);
    }

    /// **Both of the peer's names for the context-menu key reach it.** A peer
    /// picks one of the two and they are not interchangeable to the kernel:
    /// left alone, one of them lands on a properties code that nothing binds,
    /// so the key silently does nothing from half the peers that send it.
    #[test]
    fn both_names_for_the_context_menu_key_reach_it() {
        assert_eq!(key_code(101), Some(127));
        assert_eq!(key_code(118), Some(127));
    }

    /// An override shadows the table rather than being an entry in it, so it
    /// has to actually take effect. Read the table directly to show the two
    /// answers differ.
    #[test]
    fn an_override_shadows_the_kernel_table() {
        assert_eq!(KERNEL_KEY.get(118), Some(&130));
        assert_ne!(key_code(118), Some(130));
    }
}
