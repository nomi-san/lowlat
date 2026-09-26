//! The addresses this host can be reached at directly.
//!
//! **A host candidate exists to advertise what a reflexive probe cannot see.**
//! A publicly routable address is discoverable by asking a server, so offering
//! it here as well is a duplicate that costs the peer part of a check budget
//! bounded in both attempts and time. A private address is invisible to every
//! server and is the only way to reach us across a shared segment.
//!
//! The two families are gathered in opposite ways, and that is the same rule
//! applied twice rather than an inconsistency:
//!
//! - **IPv4 is enumerated.** A machine can sit on one segment through several
//!   interfaces -- a wired and a wireless leg of the same network -- and asking
//!   the routing table names only one of them, hiding a path a peer may be able
//!   to reach when the other is filtered.
//! - **IPv6 is probed.** There is no translation on that family, so the address
//!   a peer sees is the source we would send from. One interface commonly
//!   carries a stable, a temporary and a route-local global address at once,
//!   and offering all three makes the peer spend checks discovering which of
//!   them answers.

use core::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::net::UdpSocket;

/// Private address space, always offered.
const PRIVATE_V4: [(u32, u32); 3] = [
    (0x0A00_0000, 8),  // 10.0.0.0/8
    (0xAC10_0000, 12), // 172.16.0.0/12
    (0xC0A8_0000, 16), // 192.168.0.0/16
];

/// Shared address space, offered only when asked for.
///
/// Reachable when both ends are behind the same carrier translation or on the
/// same overlay network, and unreachable otherwise, so it is opted into rather
/// than assumed. Offered blindly it is a candidate the far side spends checks
/// on and never answers.
const SHARED_V4: (u32, u32) = (0x6440_0000, 10); // 100.64.0.0/10

/// Host candidates returned at most.
///
/// A machine with several bridges can present a long list, and every entry
/// costs the peer part of that bounded budget.
pub const MAX_HOST_ADDRESSES: usize = 8;

/// Destination used to ask the routing table which IPv6 source it would pick.
///
/// A connected datagram socket sends nothing, so this is a question rather than
/// traffic and any routable address serves.
const V6_ROUTE_QUESTION: &str = "[2606:4700:4700::1111]:80";

/// Whether an address falls inside `base/bits`.
fn in_network(addr: Ipv4Addr, base: u32, bits: u32) -> bool {
    let mask = u32::MAX.checked_shl(32 - bits).unwrap_or(0);
    (u32::from(addr) & mask) == base
}

/// Whether an address is one to offer as a host candidate.
///
/// Separate from the enumeration so the decision can be checked without a
/// machine that happens to carry the right interfaces.
fn wanted(addr: Ipv4Addr, shared: bool) -> bool {
    if PRIVATE_V4
        .iter()
        .any(|(base, bits)| in_network(addr, *base, *bits))
    {
        return true;
    }
    shared && in_network(addr, SHARED_V4.0, SHARED_V4.1)
}

/// Every private IPv4 address on an interface that is up.
#[cfg(target_os = "linux")]
fn enumerated_v4(shared: bool) -> Vec<IpAddr> {
    let mut found: Vec<IpAddr> = Vec::new();
    for addr in crate::sys::interface_v4() {
        if !wanted(addr, shared) {
            continue;
        }
        // One address can appear on more than one entry.
        let addr = IpAddr::V4(addr);
        if !found.contains(&addr) {
            found.push(addr);
        }
    }
    found
}

/// Whether a probed IPv6 source is one to offer as a host candidate.
///
/// Separate from the probe so the decision can be checked without a machine
/// that happens to carry the wrong addresses. A source a peer cannot reach is
/// worse than no candidate: it spends part of a bounded check budget and
/// answers nothing.
///
/// Only global unicast is offered. The routing table answers the probe with
/// whatever source it would use, and on a network built on unique-local
/// addressing that answer is a ULA: routable here, invisible everywhere
/// else. Link-local would need a scope the candidate cannot carry. Loopback
/// and unspecified are what a probe that went nowhere reports.
fn wanted_v6(ip: Ipv6Addr) -> bool {
    !ip.is_loopback()
        && !ip.is_unspecified()
        && !ip.is_unique_local()
        && !ip.is_unicast_link_local()
}

/// The IPv6 source address a peer would see us arrive from, if any.
fn probed_v6() -> Option<IpAddr> {
    let probe = UdpSocket::bind("[::]:0").ok()?;
    probe.connect(V6_ROUTE_QUESTION).ok()?;
    match probe.local_addr().ok()?.ip() {
        IpAddr::V6(v6) => wanted_v6(v6).then_some(IpAddr::V6(v6)),
        // A route question asked over v6 cannot be answered by a v4 source.
        IpAddr::V4(_) => None,
    }
}

/// The addresses to offer as host candidates, IPv4 first.
///
/// A family this machine does not have contributes nothing, which is the
/// ordinary outcome for IPv6 and is not an error. The IPv4 list is capped at
/// [`MAX_HOST_ADDRESSES`]; a caller that wants to report a cap that bound can
/// compare the length against it.
#[cfg(target_os = "linux")]
pub fn host_addresses(shared: bool) -> Vec<IpAddr> {
    let mut found = enumerated_v4(shared);
    found.truncate(MAX_HOST_ADDRESSES);
    found.extend(probed_v6());
    found
}

/// One reflexive server name, resolved to one address per address family.
///
/// **One of each, not whichever the resolver put first.** A dual-stack name
/// answers with both, ordered by what this machine's own addressing prefers, so
/// taking the head alone gives a host with global IPv6 a v6 reflexive address
/// and no v4 one -- and a v4-only peer is then offered nothing from us that it
/// can reach.
///
/// Empty when the name does not resolve at all. What to do about that is the
/// caller's: a configuration call refuses while the caller can still fix it, a
/// service carries on, because an attempt with no reflexive server still
/// punches on whatever it gathered locally.
pub fn resolve_server(name: &str) -> Vec<SocketAddr> {
    let Ok(resolved) = std::net::ToSocketAddrs::to_socket_addrs(name) else {
        return Vec::new();
    };
    let resolved: Vec<SocketAddr> = resolved.collect();
    [
        resolved.iter().find(|addr| addr.is_ipv4()),
        resolved.iter().find(|addr| addr.is_ipv6()),
    ]
    .into_iter()
    .flatten()
    .copied()
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **Only the ranges a reflexive probe cannot discover.**
    ///
    /// The addresses are written out rather than derived from the constants
    /// they pin, so a mistyped base or prefix fails here instead of agreeing
    /// with itself.
    #[test]
    fn only_private_space_is_offered() {
        let private = |text: &str| {
            let addr: Ipv4Addr = text.parse().expect("address");
            PRIVATE_V4
                .iter()
                .any(|(base, bits)| in_network(addr, *base, *bits))
        };

        for inside in [
            "10.0.0.1",
            "10.255.255.254",
            "172.16.0.1",
            "172.31.255.254",
            "192.168.0.1",
            "192.168.1.192",
            "192.168.72.1",
        ] {
            assert!(private(inside), "{inside} should be private space");
        }
        for outside in [
            "9.255.255.255",
            "11.0.0.1",
            "172.15.255.255",
            "172.32.0.1",
            "192.167.255.255",
            "192.169.0.0",
            "42.119.87.246",
            "127.0.0.1",
            "169.254.1.1",
            "100.102.226.42",
        ] {
            assert!(!private(outside), "{outside} should not be private space");
        }
    }

    /// **Shared address space is its own range and its own decision.**
    ///
    /// Checking the two ranges apart from each other passes just as well when
    /// the gate is ignored and shared space is offered to everyone, so the gate
    /// itself is what is pinned here.
    #[test]
    fn shared_address_space_is_opt_in() {
        for text in ["100.64.0.1", "100.102.226.42", "100.127.255.254"] {
            let addr: Ipv4Addr = text.parse().expect("address");
            assert!(
                !wanted(addr, false),
                "{text} was offered without being asked for"
            );
            assert!(
                wanted(addr, true),
                "{text} was withheld after being asked for"
            );
        }
        for text in ["100.63.255.255", "100.128.0.0"] {
            let addr: Ipv4Addr = text.parse().expect("address");
            assert!(!wanted(addr, true), "{text} is outside the range");
        }
        // Private space does not depend on the gate either way.
        for text in ["10.0.0.1", "192.168.1.192"] {
            let addr: Ipv4Addr = text.parse().expect("address");
            assert!(wanted(addr, false));
            assert!(wanted(addr, true));
        }
    }

    /// Only a source the world can route back to is offered. The probe asks
    /// the routing table which source would reach a global destination, and on
    /// a network built on unique-local addressing the answer is a ULA:
    /// routable here, invisible everywhere else. Link-local would need a scope
    /// the candidate does not carry, and loopback and unspecified are what a
    /// probe that went nowhere reports.
    #[test]
    fn only_a_global_unicast_v6_source_is_offered() {
        for outside in ["::1", "::", "fc00::1", "fd12:3456:789a::1", "fe80::1"] {
            let ip: Ipv6Addr = outside.parse().expect("address");
            assert!(
                !wanted_v6(ip),
                "{outside} cannot be reached from outside and was offered"
            );
        }
        for inside in ["2001:db8::7", "2a01:4f8::2", "2606:4700:4700::1111"] {
            let ip: Ipv6Addr = inside.parse().expect("address");
            assert!(wanted_v6(ip), "{inside} is global unicast and was withheld");
        }
    }

    /// Nothing unreachable is offered, and the cap is honoured.
    #[cfg(target_os = "linux")]
    #[test]
    fn what_is_offered_is_reachable_and_bounded() {
        for shared in [false, true] {
            let found = host_addresses(shared);
            for ip in &found {
                assert!(!ip.is_loopback(), "offered a loopback address: {ip}");
                assert!(!ip.is_unspecified(), "offered an unspecified address: {ip}");
            }
            assert!(
                found.iter().filter(|ip| ip.is_ipv4()).count() <= MAX_HOST_ADDRESSES,
                "the cap did not bind: {found:?}"
            );
            assert!(
                found.iter().filter(|ip| ip.is_ipv6()).count() <= 1,
                "the v6 side is probed, so it names one address: {found:?}"
            );
        }
    }
}
