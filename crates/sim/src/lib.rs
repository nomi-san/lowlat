//! Deterministic simulator for the protocol and connectivity state machines.
//!
//! The primary test surface, per docs/08-testing.md sections 4 and 5. It exists
//! because a development network produces one topology and one machine's
//! timing, while the engine must handle six topologies and adversarial timing.
//!
//! Everything here is reproducible from a seed. Time is a counter, port
//! allocation is a counter, and every random decision comes from one seeded
//! generator consulted in a fixed order. A failure that cannot be replayed from
//! its seed is a bug in this crate, not a flaky test.
//!
//! It is not for performance. There is no realistic timing here and any latency
//! number taken from it is meaningless.
//!
//! The simulator moves datagrams; it does not drive the engine. Tests own their
//! endpoints and hand outbound datagrams over, which keeps this crate
//! independent of the shape of whatever is being tested.

pub mod nat;

use std::net::SocketAddr;

pub use nat::{Filtering, Mapping, Nat};

/// Path conditions, applied per datagram.
///
/// One set for the whole network rather than per link. A second set would be
/// speculative until a test needs asymmetry.
#[derive(Debug, Clone, Copy)]
pub struct Link {
    /// One-way delay before delivery.
    pub one_way_ms: f64,
    /// Uniform jitter added on top, up to this much.
    pub jitter_ms: f64,
    /// Probability a datagram is discarded outright.
    pub loss: f64,
    /// Probability a datagram is delivered twice.
    pub duplicate: f64,
    /// Probability a datagram is held back behind later ones.
    pub reorder: f64,
    /// How long a reordered datagram is held.
    pub reorder_ms: f64,
    /// Routers between the endpoints, each consuming one unit of TTL. A
    /// datagram whose TTL does not exceed this never arrives.
    pub hops: u8,
}

impl Default for Link {
    fn default() -> Self {
        Self {
            one_way_ms: 10.0,
            jitter_ms: 0.0,
            loss: 0.0,
            duplicate: 0.0,
            reorder: 0.0,
            reorder_ms: 0.0,
            hops: 8,
        }
    }
}

/// Reproducible pseudorandom source.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    /// Seed it. The same seed replays the same run, exactly.
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A value in `[0, 1)`.
    pub fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / ((1u64 << 53) as f64)
    }

    /// True with the given probability.
    pub fn chance(&mut self, probability: f64) -> bool {
        probability > 0.0 && self.unit() < probability
    }
}

/// Milliseconds to microseconds, saturating at both ends.
///
/// Time is kept as an integer count so that ordering is total and a replay is
/// bit-identical. A negative, infinite, or absurd interval clamps rather than
/// wrapping into an enormous delay that would silently stall a run.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the bounds check above makes the conversion exact"
)]
fn us_from_ms(ms: f64) -> u64 {
    let us = ms * 1000.0;
    if !us.is_finite() || us <= 0.0 {
        0
    } else if us >= u64::MAX as f64 {
        u64::MAX
    } else {
        us as u64
    }
}

/// Handle to a translator held by the simulator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NatId(usize);

/// Handle to an endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostId(usize);

#[derive(Debug)]
struct Host {
    internal: SocketAddr,
    /// Translators between this host and the public network, innermost first.
    /// Empty means the host is directly reachable.
    chain: Vec<NatId>,
}

#[derive(Debug)]
struct Delivery {
    at_us: u64,
    /// Tiebreak for datagrams due at the same instant, so ordering is total and
    /// therefore reproducible.
    seq: u64,
    to: HostId,
    from: SocketAddr,
    bytes: Vec<u8>,
}

/// One datagram handed to a host.
#[derive(Debug, Clone)]
pub struct Arrival {
    /// Which endpoint received it.
    pub host: HostId,
    /// The source address as that endpoint sees it, already translated.
    pub from: SocketAddr,
    /// The bytes.
    pub bytes: Vec<u8>,
}

/// Why a datagram did not arrive. Recorded so a test can assert the mechanism
/// rather than merely the absence of delivery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Dropped {
    /// The TTL did not survive the hops between the endpoints. The outbound
    /// mapping was still created, which is the point of a probe.
    TtlExpired,
    /// The path lost it.
    Lost,
    /// No translator mapping matched, or filtering refused the sender.
    Filtered,
    /// Addressed to a translator that does not loop back to its own inside.
    NoHairpin,
    /// Nothing is listening on that address.
    Unroutable,
}

/// The network.
#[derive(Debug)]
pub struct Sim {
    now_us: u64,
    seq: u64,
    rng: Rng,
    link: Link,
    nats: Vec<Nat>,
    hosts: Vec<Host>,
    queue: Vec<Delivery>,
    dropped: Vec<Dropped>,
}

impl Sim {
    /// A network with default path conditions.
    pub fn new(seed: u64) -> Self {
        Self {
            now_us: 0,
            seq: 0,
            rng: Rng::new(seed),
            link: Link::default(),
            nats: Vec::new(),
            hosts: Vec::new(),
            queue: Vec::new(),
            dropped: Vec::new(),
        }
    }

    /// Replace the path conditions.
    pub fn with_link(mut self, link: Link) -> Self {
        self.link = link;
        self
    }

    /// Install a translator. Several hosts may sit behind the same one, which
    /// is what makes hairpin expressible.
    pub fn add_nat(&mut self, nat: Nat) -> NatId {
        self.nats.push(nat);
        NatId(self.nats.len() - 1)
    }

    /// Add an endpoint behind `chain`, innermost translator first. An empty
    /// chain is a directly reachable host.
    pub fn add_host(&mut self, internal: SocketAddr, chain: &[NatId]) -> HostId {
        self.hosts.push(Host {
            internal,
            chain: chain.to_vec(),
        });
        HostId(self.hosts.len() - 1)
    }

    /// Current time, in the fractional milliseconds the core expects.
    pub fn now_ms(&self) -> f64 {
        self.now_us as f64 / 1000.0
    }

    /// Move time forward.
    pub fn advance_ms(&mut self, delta_ms: f64) {
        self.now_us = self.now_us.saturating_add(us_from_ms(delta_ms));
    }

    /// Everything that failed to arrive since the last call, and why.
    pub fn take_drops(&mut self) -> Vec<Dropped> {
        core::mem::take(&mut self.dropped)
    }

    /// The address a host learns for itself by asking `server`.
    ///
    /// This performs the outbound translation a reflexive probe would, so the
    /// answer is subject to the same mapping behaviour as any other datagram.
    /// Under endpoint-independent mapping it is the address a peer will reach;
    /// under symmetric translation it is not, and that difference is the whole
    /// reason a direct punch fails there.
    pub fn reflexive(&mut self, host: HostId, server: SocketAddr) -> SocketAddr {
        self.translate_out(host, server)
    }

    /// Send a datagram. `ttl` is the value the sending socket carried.
    ///
    /// Translation happens before anything can discard the datagram, because a
    /// mapping opened by a datagram that never arrives is precisely the effect
    /// a reduced-TTL probe is after.
    pub fn send(&mut self, from: HostId, to: SocketAddr, ttl: u8, bytes: &[u8]) {
        let source = self.translate_out(from, to);

        if u16::from(ttl) <= u16::from(self.link.hops) {
            self.dropped.push(Dropped::TtlExpired);
            return;
        }

        // Hairpin is decided by the sender's own translators: a datagram
        // addressed to one of them never leaves the local network.
        if let Some(nat) = self
            .hosts
            .get(from.0)
            .into_iter()
            .flat_map(|host| host.chain.iter())
            .filter_map(|id| self.nats.get(id.0))
            .find(|nat| nat.external_ip() == to.ip())
            && !nat.hairpins()
        {
            self.dropped.push(Dropped::NoHairpin);
            return;
        }

        let Some((target, delivered_from)) = self.resolve(source, to) else {
            return;
        };

        if self.rng.chance(self.link.loss) {
            self.dropped.push(Dropped::Lost);
            return;
        }

        let duplicate = self.rng.chance(self.link.duplicate);
        let held = self.rng.chance(self.link.reorder);
        let jitter = if self.link.jitter_ms > 0.0 {
            self.rng.unit() * self.link.jitter_ms
        } else {
            0.0
        };

        let mut delay = self.link.one_way_ms + jitter;
        if held {
            delay += self.link.reorder_ms;
        }
        self.enqueue(target, delivered_from, bytes, delay);
        if duplicate {
            self.enqueue(target, delivered_from, bytes, delay);
        }
    }

    /// Take the next datagram whose delivery time has arrived.
    ///
    /// Drive until `None`, then advance time. Ordering among datagrams due at
    /// the same instant is by send order, so a replay is identical.
    pub fn next_arrival(&mut self) -> Option<Arrival> {
        let now = self.now_us;
        let (index, _) = self
            .queue
            .iter()
            .enumerate()
            .filter(|(_, d)| d.at_us <= now)
            .min_by_key(|(_, d)| (d.at_us, d.seq))?;
        let delivery = self.queue.remove(index);
        Some(Arrival {
            host: delivery.to,
            from: delivery.from,
            bytes: delivery.bytes,
        })
    }

    /// Milliseconds until the next queued delivery, if any.
    pub fn next_delivery_ms(&self) -> Option<f64> {
        self.queue
            .iter()
            .map(|d| d.at_us)
            .min()
            .map(|at| at.saturating_sub(self.now_us) as f64 / 1000.0)
    }

    fn enqueue(&mut self, to: HostId, from: SocketAddr, bytes: &[u8], delay_ms: f64) {
        let at_us = self.now_us.saturating_add(us_from_ms(delay_ms));
        self.seq = self.seq.wrapping_add(1);
        self.queue.push(Delivery {
            at_us,
            seq: self.seq,
            to,
            from,
            bytes: bytes.to_vec(),
        });
    }

    /// Walk the sender's translators outward, opening mappings as it goes.
    fn translate_out(&mut self, from: HostId, to: SocketAddr) -> SocketAddr {
        let Some(host) = self.hosts.get(from.0) else {
            return to;
        };
        let mut source = host.internal;
        let chain = host.chain.clone();
        for id in chain {
            if let Some(nat) = self.nats.get_mut(id.0) {
                source = nat.outbound(source, to);
            }
        }
        source
    }

    /// Find who `to` reaches, walking translators inward.
    fn resolve(&mut self, from: SocketAddr, to: SocketAddr) -> Option<(HostId, SocketAddr)> {
        for (index, host) in self.hosts.iter().enumerate() {
            if host.chain.is_empty() {
                if host.internal == to {
                    return Some((HostId(index), from));
                }
                continue;
            }

            let mut dest = to;
            let mut admitted = true;
            for id in host.chain.iter().rev() {
                let Some(nat) = self.nats.get(id.0) else {
                    admitted = false;
                    break;
                };
                if nat.external_ip() != dest.ip() {
                    admitted = false;
                    break;
                }
                match nat.inbound(from, dest.port()) {
                    Some(inner) => dest = inner,
                    None => {
                        // The port belongs to this translator but the sender is
                        // not admitted: a real filtering drop, worth recording.
                        if nat.owns(dest.port()) {
                            self.dropped.push(Dropped::Filtered);
                        }
                        admitted = false;
                        break;
                    }
                }
            }

            if admitted && dest == host.internal {
                return Some((HostId(index), from));
            }
        }

        self.dropped.push(Dropped::Unroutable);
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    fn public(last: u8, port: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, last)), port)
    }

    fn private(last: u8, port: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, last)), port)
    }

    fn ip(last: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(203, 0, 113, last))
    }

    /// Drain everything due now.
    fn drain(sim: &mut Sim) -> Vec<Arrival> {
        let mut out = Vec::new();
        while let Some(arrival) = sim.next_arrival() {
            out.push(arrival);
        }
        out
    }

    #[test]
    fn a_datagram_crosses_between_public_hosts() {
        let mut sim = Sim::new(1);
        let a = sim.add_host(public(10, 5000), &[]);
        let b = sim.add_host(public(20, 6000), &[]);

        sim.send(a, public(20, 6000), 64, b"hello");
        assert!(
            drain(&mut sim).is_empty(),
            "delivery must respect the delay"
        );

        sim.advance_ms(10.0);
        let arrived = drain(&mut sim);
        assert_eq!(arrived.len(), 1);
        assert_eq!(arrived[0].host, b);
        assert_eq!(arrived[0].from, public(10, 5000));
        assert_eq!(arrived[0].bytes, b"hello");
    }

    /// The mechanism the mapping probe depends on: the datagram dies in transit
    /// but the translator has already opened the mapping.
    #[test]
    fn a_low_ttl_datagram_opens_the_mapping_without_arriving() {
        let mut sim = Sim::new(1);
        let nat = sim.add_nat(Nat::port_restricted(ip(1)));
        let a = sim.add_host(private(10, 5000), &[nat]);
        let b = sim.add_host(public(20, 6000), &[]);

        sim.send(a, public(20, 6000), 4, b"probe");
        sim.advance_ms(50.0);
        assert!(
            drain(&mut sim).is_empty(),
            "a probe must not reach the peer"
        );
        assert_eq!(sim.take_drops(), vec![Dropped::TtlExpired]);

        // The mapping is open, so the peer can now be let back in.
        sim.send(b, public(1, 40_000), 64, b"reply");
        sim.advance_ms(50.0);
        let arrived = drain(&mut sim);
        assert_eq!(arrived.len(), 1, "the probe did not open the mapping");
        assert_eq!(arrived[0].host, a);
    }

    #[test]
    fn symmetric_translation_hides_the_advertised_mapping() {
        let mut sim = Sim::new(1);
        let nat = sim.add_nat(Nat::symmetric(ip(1)));
        let a = sim.add_host(private(10, 5000), &[nat]);
        let server = sim.add_host(public(50, 3478), &[]);
        let b = sim.add_host(public(20, 6000), &[]);
        let _ = server;

        let advertised = sim.reflexive(a, public(50, 3478));

        // The peer checks the advertised address and reaches nothing, because
        // the mapping toward the peer is a different port.
        sim.send(b, advertised, 64, b"check");
        sim.advance_ms(50.0);
        assert!(drain(&mut sim).is_empty());
        assert!(sim.take_drops().contains(&Dropped::Filtered));
    }

    #[test]
    fn endpoint_independent_mapping_advertises_a_reachable_address() {
        let mut sim = Sim::new(1);
        let nat = sim.add_nat(Nat::full_cone(ip(1)));
        let a = sim.add_host(private(10, 5000), &[nat]);
        let b = sim.add_host(public(20, 6000), &[]);

        let advertised = sim.reflexive(a, public(50, 3478));
        sim.send(b, advertised, 64, b"check");
        sim.advance_ms(50.0);
        assert_eq!(drain(&mut sim).len(), 1);
    }

    #[test]
    fn a_translator_without_hairpin_refuses_its_own_external_address() {
        let mut sim = Sim::new(1);
        let nat = sim.add_nat(Nat::port_restricted(ip(1)).with_hairpin(false));
        let a = sim.add_host(private(10, 5000), &[nat]);
        let b = sim.add_host(private(11, 5000), &[nat]);

        let b_external = sim.reflexive(b, public(50, 3478));
        sim.send(a, b_external, 64, b"check");
        sim.advance_ms(50.0);
        assert!(drain(&mut sim).is_empty());
        assert_eq!(sim.take_drops(), vec![Dropped::NoHairpin]);
    }

    #[test]
    fn carrier_grade_translation_chains_two_layers() {
        let mut sim = Sim::new(1);
        let cpe = sim.add_nat(Nat::port_restricted(IpAddr::V4(Ipv4Addr::new(
            100, 64, 0, 5,
        ))));
        let cgn = sim.add_nat(Nat::port_restricted(ip(1)));
        let a = sim.add_host(private(10, 5000), &[cpe, cgn]);
        let b = sim.add_host(public(20, 6000), &[]);

        let advertised = sim.reflexive(a, public(50, 3478));
        assert_eq!(
            advertised.ip(),
            ip(1),
            "the advertised address must be the outermost one"
        );

        // An unsolicited check is refused, and correctly so: neither layer has
        // been told about this sender. Two layers of translation do not make a
        // host reachable that one layer would not.
        sim.send(b, advertised, 64, b"unsolicited");
        sim.advance_ms(50.0);
        assert!(drain(&mut sim).is_empty());
        assert!(sim.take_drops().contains(&Dropped::Filtered));

        // The property that makes a punch work through both layers: our own
        // check leaves from the very address the peer was told about.
        sim.send(a, public(20, 6000), 64, b"check");
        sim.advance_ms(50.0);
        let arrived = drain(&mut sim);
        assert_eq!(arrived.len(), 1);
        assert_eq!(
            arrived[0].from, advertised,
            "mapping must stay endpoint independent through both layers"
        );

        // And that check opened the filter at both layers, so the answer lands.
        sim.send(b, advertised, 64, b"answer");
        sim.advance_ms(50.0);
        let arrived = drain(&mut sim);
        assert_eq!(arrived.len(), 1);
        assert_eq!(arrived[0].host, a);
    }

    #[test]
    fn a_run_replays_exactly_from_its_seed() {
        let run = || {
            let mut sim = Sim::new(0xC0FFEE).with_link(Link {
                loss: 0.3,
                duplicate: 0.2,
                reorder: 0.3,
                reorder_ms: 25.0,
                jitter_ms: 5.0,
                ..Link::default()
            });
            let a = sim.add_host(public(10, 5000), &[]);
            let b = sim.add_host(public(20, 6000), &[]);
            for index in 0..64u8 {
                sim.send(a, public(20, 6000), 64, &[index]);
            }
            let _ = b;
            sim.advance_ms(500.0);
            drain(&mut sim)
                .into_iter()
                .map(|a| a.bytes)
                .collect::<Vec<_>>()
        };
        let first = run();
        assert_eq!(first, run(), "the same seed must replay identically");
        assert!(
            !first.is_empty() && first.len() != 64,
            "conditions did nothing"
        );
    }

    #[test]
    fn loss_and_duplication_are_applied() {
        let mut sim = Sim::new(7).with_link(Link {
            loss: 1.0,
            ..Link::default()
        });
        let a = sim.add_host(public(10, 5000), &[]);
        let b = sim.add_host(public(20, 6000), &[]);
        let _ = b;
        sim.send(a, public(20, 6000), 64, b"x");
        sim.advance_ms(100.0);
        assert!(drain(&mut sim).is_empty());
        assert_eq!(sim.take_drops(), vec![Dropped::Lost]);
    }
}
