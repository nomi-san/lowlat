//! Address translation as data.
//!
//! A translator is two independent behaviours, and every named topology is a
//! pairing of them:
//!
//! | Topology | Mapping | Filtering |
//! |---|---|---|
//! | full cone | endpoint independent | endpoint independent |
//! | restricted cone | endpoint independent | address |
//! | port restricted | endpoint independent | address and port |
//! | symmetric | address and port | address and port |
//!
//! Separating the two is what makes the model worth having. Mapping decides
//! whether the address a peer was told about is the address our packets
//! actually leave from, which is what a punch depends on. Filtering decides
//! whether an arriving packet is let through, which is what simultaneous open
//! defeats. A model with one knob cannot express the difference, and the
//! difference is the entire matrix.
//!
//! Port allocation is a counter, never a random draw, so a failing run replays
//! from its seed.

use std::net::{IpAddr, SocketAddr};

/// First external port handed out. Arbitrary, but stable across runs.
const FIRST_PORT: u16 = 40_000;

/// How the external address is chosen for an outbound datagram.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mapping {
    /// One external port per internal socket, whoever it is talking to. The
    /// address a peer was told about is the address it will be contacted from.
    EndpointIndependent,
    /// A fresh external port per destination address.
    AddressDependent,
    /// A fresh external port per destination address and port. This is what
    /// defeats a direct punch: the peer is told about a mapping created toward
    /// something else, and our packets to the peer leave from a different port
    /// entirely.
    AddressAndPortDependent,
}

/// Which arriving datagrams are let through to an existing mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Filtering {
    /// Anyone may use the mapping once it exists.
    EndpointIndependent,
    /// Only hosts the internal side has sent to.
    AddressDependent,
    /// Only the exact address and port the internal side has sent to.
    AddressAndPortDependent,
}

/// What identifies a mapping, given the translator's behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Key {
    Internal(SocketAddr),
    PerAddress(SocketAddr, IpAddr),
    PerAddressAndPort(SocketAddr, SocketAddr),
}

#[derive(Debug)]
struct Binding {
    key: Key,
    internal: SocketAddr,
    external_port: u16,
    /// Everywhere the internal side has sent through this mapping, which is
    /// what filtering consults.
    contacted: Vec<SocketAddr>,
}

/// One address translator.
#[derive(Debug)]
pub struct Nat {
    mapping: Mapping,
    filtering: Filtering,
    /// Whether a datagram addressed to this translator's own external address
    /// is looped back to an internal host.
    hairpin: bool,
    external_ip: IpAddr,
    next_port: u16,
    bindings: Vec<Binding>,
}

impl Nat {
    /// Build a translator with explicit behaviours.
    pub fn new(external_ip: IpAddr, mapping: Mapping, filtering: Filtering) -> Self {
        Self {
            mapping,
            filtering,
            hairpin: true,
            external_ip,
            next_port: FIRST_PORT,
            bindings: Vec::new(),
        }
    }

    /// Endpoint independent both ways. The easiest case and the rarest.
    pub fn full_cone(external_ip: IpAddr) -> Self {
        Self::new(
            external_ip,
            Mapping::EndpointIndependent,
            Filtering::EndpointIndependent,
        )
    }

    /// One mapping per socket, admitting only hosts already contacted.
    pub fn restricted_cone(external_ip: IpAddr) -> Self {
        Self::new(
            external_ip,
            Mapping::EndpointIndependent,
            Filtering::AddressDependent,
        )
    }

    /// One mapping per socket, admitting only the exact sockets contacted. The
    /// common consumer case, and a punch works through it.
    pub fn port_restricted(external_ip: IpAddr) -> Self {
        Self::new(
            external_ip,
            Mapping::EndpointIndependent,
            Filtering::AddressAndPortDependent,
        )
    }

    /// A fresh mapping per destination. A direct punch cannot work through
    /// this, by construction rather than by bad luck.
    pub fn symmetric(external_ip: IpAddr) -> Self {
        Self::new(
            external_ip,
            Mapping::AddressAndPortDependent,
            Filtering::AddressAndPortDependent,
        )
    }

    /// Turn loopback of the external address on or off.
    pub fn with_hairpin(mut self, hairpin: bool) -> Self {
        self.hairpin = hairpin;
        self
    }

    /// The address this translator presents to the outside.
    pub fn external_ip(&self) -> IpAddr {
        self.external_ip
    }

    /// Whether it loops a datagram addressed to its own external address back
    /// inside.
    pub fn hairpins(&self) -> bool {
        self.hairpin
    }

    fn key_for(&self, internal: SocketAddr, dest: SocketAddr) -> Key {
        match self.mapping {
            Mapping::EndpointIndependent => Key::Internal(internal),
            Mapping::AddressDependent => Key::PerAddress(internal, dest.ip()),
            Mapping::AddressAndPortDependent => Key::PerAddressAndPort(internal, dest),
        }
    }

    /// Translate an outbound datagram, creating or refreshing the mapping.
    ///
    /// Returns the source address the outside world will see. This happens even
    /// when the datagram is later discarded in flight, which is exactly what a
    /// reduced-TTL probe relies on: the mapping opens, the datagram never
    /// arrives.
    pub fn outbound(&mut self, internal: SocketAddr, dest: SocketAddr) -> SocketAddr {
        let key = self.key_for(internal, dest);

        if let Some(binding) = self.bindings.iter_mut().find(|b| b.key == key) {
            if !binding.contacted.contains(&dest) {
                binding.contacted.push(dest);
            }
            return SocketAddr::new(self.external_ip, binding.external_port);
        }

        let external_port = self.next_port;
        self.next_port = self.next_port.wrapping_add(1).max(FIRST_PORT);
        self.bindings.push(Binding {
            key,
            internal,
            external_port,
            contacted: vec![dest],
        });
        SocketAddr::new(self.external_ip, external_port)
    }

    /// Translate an arriving datagram, or refuse it.
    ///
    /// `None` means the datagram is discarded: either no mapping exists on that
    /// port, or filtering rejects the sender. Both are ordinary and both are
    /// silent, which is why a punch failure looks like nothing happening.
    pub fn inbound(&self, from: SocketAddr, external_port: u16) -> Option<SocketAddr> {
        let binding = self
            .bindings
            .iter()
            .find(|b| b.external_port == external_port)?;

        let admitted = match self.filtering {
            Filtering::EndpointIndependent => true,
            Filtering::AddressDependent => {
                binding.contacted.iter().any(|seen| seen.ip() == from.ip())
            }
            Filtering::AddressAndPortDependent => binding.contacted.contains(&from),
        };

        admitted.then_some(binding.internal)
    }

    /// Whether an internal socket already holds a mapping on `external_port`.
    pub fn owns(&self, external_port: u16) -> bool {
        self.bindings
            .iter()
            .any(|b| b.external_port == external_port)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(last: u8) -> IpAddr {
        IpAddr::V4(std::net::Ipv4Addr::new(203, 0, 113, last))
    }

    fn inside(last: u8, port: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::new(192, 168, 1, last)), port)
    }

    fn outside(last: u8, port: u16) -> SocketAddr {
        SocketAddr::new(ip(last), port)
    }

    /// The property a punch depends on: what a peer was told about is what it
    /// will be contacted from.
    #[test]
    fn endpoint_independent_mapping_reuses_one_port() {
        let mut nat = Nat::port_restricted(ip(1));
        let host = inside(10, 5000);

        let via_server = nat.outbound(host, outside(50, 3478));
        let via_peer = nat.outbound(host, outside(60, 4000));

        assert_eq!(
            via_server, via_peer,
            "the address a peer was told about must be the one it is contacted from"
        );
    }

    /// The property that defeats it, and the reason the relay exists.
    #[test]
    fn symmetric_mapping_uses_a_fresh_port_per_destination() {
        let mut nat = Nat::symmetric(ip(1));
        let host = inside(10, 5000);

        let via_server = nat.outbound(host, outside(50, 3478));
        let via_peer = nat.outbound(host, outside(60, 4000));

        assert_ne!(
            via_server, via_peer,
            "symmetric translation must not reuse a mapping across destinations"
        );

        // And the peer, checking the advertised address, reaches nothing.
        assert_eq!(
            nat.inbound(outside(60, 4000), via_server.port()),
            None,
            "a check to the advertised mapping must not arrive"
        );
    }

    #[test]
    fn endpoint_independent_filtering_admits_a_stranger() {
        let mut nat = Nat::full_cone(ip(1));
        let host = inside(10, 5000);
        let external = nat.outbound(host, outside(50, 3478));

        assert_eq!(
            nat.inbound(outside(99, 1234), external.port()),
            Some(host),
            "a full cone admits a host that was never contacted"
        );
    }

    #[test]
    fn address_dependent_filtering_admits_a_new_port_from_a_known_host() {
        let mut nat = Nat::restricted_cone(ip(1));
        let host = inside(10, 5000);
        let external = nat.outbound(host, outside(60, 4000));

        assert_eq!(
            nat.inbound(outside(60, 9999), external.port()),
            Some(host),
            "a restricted cone admits any port from a contacted address"
        );
        assert_eq!(
            nat.inbound(outside(61, 4000), external.port()),
            None,
            "but not an address that was never contacted"
        );
    }

    #[test]
    fn port_dependent_filtering_admits_only_the_exact_socket() {
        let mut nat = Nat::port_restricted(ip(1));
        let host = inside(10, 5000);
        let external = nat.outbound(host, outside(60, 4000));

        assert_eq!(nat.inbound(outside(60, 4000), external.port()), Some(host));
        assert_eq!(
            nat.inbound(outside(60, 9999), external.port()),
            None,
            "a different port from the same host must be filtered"
        );
    }

    /// Simultaneous open, which is the normal case rather than an exception.
    /// Neither side's first datagram arrives; both open the filter; the second
    /// pair gets through.
    #[test]
    fn simultaneous_open_gets_through_port_restricted_translation() {
        let mut left = Nat::port_restricted(ip(1));
        let mut right = Nat::port_restricted(ip(2));
        let left_host = inside(10, 5000);
        let right_host = inside(20, 6000);

        // Each learns its own mapping by talking to something else first.
        let left_external = left.outbound(left_host, outside(50, 3478));
        let right_external = right.outbound(right_host, outside(50, 3478));

        // First checks cross. Each opens its own filter and is dropped by the
        // other, which has not yet been told about this sender.
        left.outbound(left_host, right_external);
        assert_eq!(
            right.inbound(left_external, right_external.port()),
            None,
            "the first check must not arrive"
        );

        right.outbound(right_host, left_external);

        // Now both filters know the other side.
        assert_eq!(
            left.inbound(right_external, left_external.port()),
            Some(left_host)
        );
        assert_eq!(
            right.inbound(left_external, right_external.port()),
            Some(right_host)
        );
    }

    #[test]
    fn port_allocation_is_deterministic() {
        let ports = |()| {
            let mut nat = Nat::symmetric(ip(1));
            let host = inside(10, 5000);
            (0..4)
                .map(|index| nat.outbound(host, outside(60, 4000 + index)).port())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            ports(()),
            ports(()),
            "a replay must allocate the same ports"
        );
    }
}
