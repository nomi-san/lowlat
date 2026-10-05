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

/// How the gateway lists this side's mapping: by its seed, so another
/// machine's entry on the same port is never taken for a leftover of ours.
pub fn description(seed: &str) -> String {
    format!("lowlat-{:08x}", djb2(seed))
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

    /// djb2's values, computed apart from this code; the last one wraps.
    #[test]
    fn a_seed_picks_its_port_and_its_description() {
        assert_eq!(
            (port(""), description("")),
            (25381, "lowlat-00001505".into())
        );
        assert_eq!(
            (port("HOST-1"), description("HOST-1")),
            (24097, "lowlat-b596f0a1".into())
        );
        let long = "a much longer seed that wraps around thirty-two bits";
        assert_eq!(
            (port(long), description(long)),
            (25375, "lowlat-e9ca2dff".into())
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
        assert_ne!(description(first), description(&other));
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
