//! The stable port, and how the gateway lists its mapping, from a seed.
//!
//! **A port that stays put is what makes a mapping worth keeping**: the
//! gateway's entry, and the reflexive candidate gathered through it, hold from
//! one attempt to the next only while the port does. A seed -- the
//! application's own name for this instance, or the machine's -- picks the
//! port, so two instances on one machine can hold two, and the same seed names
//! this side's entries in the gateway's table.

/// The stable ports: below both systems' ephemeral ranges, and clear of the
/// ranges an established client and host derive their own ports in.
const BASE: u16 = 24000;
const SPAN: u32 = 2000;

/// djb2: the same on every run and every platform, and nothing secret about
/// it.
fn djb2(seed: &str) -> u32 {
    seed.bytes().fold(5381u32, |hash, byte| {
        hash.wrapping_mul(33).wrapping_add(u32::from(byte))
    })
}

/// The port a seed picks.
pub fn port(seed: &str) -> u16 {
    // Below the span, so the sum stays under the top of the range.
    BASE + u16::try_from(djb2(seed) % SPAN).unwrap_or(0)
}

/// What the machine's own identifier is reduced under: the application's
/// name for the purpose, fixed.
const APPLICATION: &[u8] = b"lowlat port mapping";

/// How the gateway lists this side's mapping: the seed, and the machine.
///
/// **A seed alone is shared**: machines left with one default name, or an
/// application that names its instances the same on every machine, would
/// each take the other's entry for a leftover of its own and delete it. The
/// machine's own identifier, reduced by a keyed hash so that nothing of it
/// leaves the machine, tells them apart; this side's own entry for an
/// address it no longer has still matches.
#[cfg(any(target_os = "linux", windows))]
pub fn description(seed: &str) -> String {
    describe(seed, crate::sys::machine_id().as_deref())
}

/// The description from a seed and the machine's identifier, where the
/// system has one.
pub fn describe(seed: &str, machine: Option<&[u8]>) -> String {
    match machine {
        Some(id) => {
            let [a, b, c, ..] = lowlat_crypto::app_specific(id, APPLICATION);
            format!("ll-{:08x}-{a:02x}{b:02x}{c:02x}", djb2(seed))
        }
        None => format!("ll-{:08x}", djb2(seed)),
    }
}

/// The machine's name, the seed when the application gives none: on Windows
/// its network name, in capitals, as the system reports it; on Linux its host
/// name. Empty when the system will not say.
#[cfg(any(target_os = "linux", windows))]
pub fn machine_name() -> String {
    crate::sys::machine_name().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// djb2's values and the keyed hash's, computed apart from this code; the
    /// last seed wraps.
    #[test]
    fn a_seed_picks_its_port_and_its_description() {
        let machine = Some(&b"0123456789abcdef0123456789abcdef"[..]);
        assert_eq!(
            (port(""), describe("", None), describe("", machine)),
            (25381, "ll-00001505".into(), "ll-00001505-ccb40a".into())
        );
        assert_eq!(
            (port("HOST-1"), describe("HOST-1", None)),
            (24097, "ll-b596f0a1".into())
        );
        let long = "a much longer seed that wraps around thirty-two bits";
        assert_eq!(
            (port(long), describe(long, machine)),
            (25375, "ll-e9ca2dff-ccb40a".into())
        );
    }

    /// Two seeds that pick one port are told apart by their descriptions.
    #[test]
    fn a_description_tells_apart_seeds_that_share_a_port() {
        // Two thousand ports: a short walk finds another seed on this one.
        let first = "a";
        let other = (0u32..100_000)
            .map(|n| format!("seed-{n}"))
            .find(|seed| port(seed) == port(first))
            .unwrap();
        assert_ne!(describe(first, None), describe(&other, None));
    }

    /// One seed on two machines: one port, and two descriptions.
    #[test]
    fn a_description_tells_apart_machines_that_share_a_seed() {
        let ours = describe("debian", Some(b"one machine"));
        let theirs = describe("debian", Some(b"another machine"));
        assert_ne!(ours, theirs);
        assert!(ours.starts_with("ll-") && theirs.starts_with("ll-"));
    }

    /// The machine's identifier is read where the system keeps one, and the
    /// description carries its hash, never the identifier itself.
    #[cfg(any(target_os = "linux", windows))]
    #[test]
    fn this_machine_has_an_identifier_and_hides_it() {
        let Some(id) = crate::sys::machine_id() else {
            return;
        };
        let text = description("seed");
        assert_eq!(text, describe("seed", Some(&id)));
        assert_eq!(text.len(), "ll-00000000-000000".len());
        let id = String::from_utf8_lossy(&id).to_ascii_lowercase();
        assert!(
            !text.contains(id.as_str()),
            "the description carries the identifier"
        );
    }

    #[cfg(any(target_os = "linux", windows))]
    #[test]
    fn the_machine_has_a_name() {
        let name = machine_name();
        assert!(!name.is_empty());
        #[cfg(windows)]
        assert_eq!(name, name.to_uppercase(), "a network name is in capitals");
    }
}
