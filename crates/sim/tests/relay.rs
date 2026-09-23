//! The relay in the simulator, against a relay server written from the
//! standard rather than from the client's codec (docs/03-connectivity.md 7).
//!
//! A symmetric client reaches a host through the relay and not without it;
//! the host-box deployment with one forwarded port; full-size datagrams both
//! ways in both framings; 300 seconds crossed three times and a nonce rotation
//! with the path kept; a loopback candidate never relayed; and each typed
//! outcome from its cause.
//!
//! The host knows nothing of the relay. It is handed one more candidate and
//! runs exactly as it would against a peer it can reach directly.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use lowlat_core::channel::{RecvRing, SlotMeta};
use lowlat_core::conn::{self, Conn, Credentials, Kind, PROBE_TTL, Ttl};
use lowlat_core::endpoint::Endpoint;
use lowlat_core::envelope::Envelope;
use lowlat_core::relay::{Failure, Relay as ClientRelay, State as RelayState};
use lowlat_core::send::{SendRing, SendSlot};
use lowlat_core::session::{Health, Session};
use lowlat_sim::relay::Relay;
use lowlat_sim::{HostId, Nat, Sim};

const KEY: [u8; 32] = [0x2B; 32];
const CHANNEL: u8 = 1;
/// A fragment per slot at the default datagram size, as the client and the host
/// run it: what makes a full-size datagram full-size.
const SLOT: usize = lowlat_core::DEFAULT_BODY;
const SLOTS: usize = 256;
const USER: &str = "user";
const PASS: &str = "password";

/// Where each side would ask for its own mapping. Nothing needs to answer:
/// the simulator performs the translation the question would.
const STUN: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 99)), 3478);

fn public(last: u8) -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(203, 0, 113, last))
}

fn private(third: u8, last: u8, port: u16) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, third, last)), port)
}

/// The host's machine, where the relay also runs in the deployment of
/// docs/03-connectivity.md 7.1.
fn the_box(port: u16) -> SocketAddr {
    private(1, 20, port)
}

struct Arena {
    recv_bodies: Vec<u8>,
    recv_meta: Vec<SlotMeta>,
    send_bodies: Vec<u8>,
    send_meta: Vec<SendSlot>,
}

impl Arena {
    fn new() -> Self {
        Self {
            recv_bodies: vec![0u8; SLOT * SLOTS],
            recv_meta: vec![SlotMeta::default(); SLOTS],
            send_bodies: vec![0u8; SLOT * SLOTS],
            send_meta: vec![SendSlot::default(); SLOTS],
        }
    }

    fn session(&mut self) -> Session<'_> {
        let mut session = Session::new(Envelope::from_key(&KEY).unwrap(), 1, 0.0);
        session
            .attach_recv(
                CHANNEL,
                RecvRing::new(&mut self.recv_bodies, &mut self.recv_meta, SLOT).unwrap(),
            )
            .unwrap();
        session
            .attach_send(
                CHANNEL,
                SendRing::new(&mut self.send_bodies, &mut self.send_meta, SLOT, CHANNEL).unwrap(),
            )
            .unwrap();
        session
    }
}

fn conn(
    ours: (&'static str, &'static str),
    theirs: (&'static str, &'static str),
    seed: u8,
) -> Conn<'static> {
    Conn::new(
        Credentials {
            local_ufrag: ours.0,
            local_pwd: ours.1,
            remote_ufrag: theirs.0,
            remote_pwd: theirs.1,
        },
        [seed; 16],
        0.0,
    )
}

const CLIENT: (&str, &str) = ("aaaa", "passwordforaaaa");
const HOST: (&str, &str) = ("bbbb", "passwordforbbbb");

/// The network, the relay on it, and the two ends.
struct World<'a> {
    sim: Sim,
    relay: Option<Relay>,
    client: Endpoint<'a>,
    client_id: HostId,
    host: Endpoint<'a>,
    host_id: HostId,
    offered: bool,
    at_client: Vec<Vec<u8>>,
    at_host: Vec<Vec<u8>>,
    /// Every address the client sent anything to.
    client_sent_to: Vec<SocketAddr>,
    message: Vec<u8>,
}

impl World<'_> {
    fn step(&mut self) {
        let now = self.sim.now_ms();
        let mut buf = [0u8; 2100];
        let mut scratch = [0u8; 2100];
        let ttl = |ttl: Ttl| if ttl == Ttl::Probe { PROBE_TTL } else { 64 };
        while let Some(result) = self.client.get_output(now, &mut buf) {
            let egress =
                result.unwrap_or_else(|error| panic!("the client could not emit: {error}"));
            if !self.client_sent_to.contains(&egress.to) {
                self.client_sent_to.push(egress.to);
            }
            self.sim.send(
                self.client_id,
                egress.to,
                ttl(egress.ttl),
                &buf[..egress.len],
            );
        }
        while let Some(result) = self.host.get_output(now, &mut buf) {
            let egress = result.unwrap_or_else(|error| panic!("the host could not emit: {error}"));
            self.sim
                .send(self.host_id, egress.to, ttl(egress.ttl), &buf[..egress.len]);
        }
        while let Some(arrival) = self.sim.next_arrival() {
            // A datagram that is refused is dropped, not fatal.
            if arrival.host == self.client_id {
                let _ = self.client.process_input(
                    &arrival.bytes,
                    arrival.from,
                    None,
                    now,
                    &mut scratch,
                );
            } else if arrival.host == self.host_id {
                let _ =
                    self.host
                        .process_input(&arrival.bytes, arrival.from, None, now, &mut scratch);
            } else if let Some(relay) = self.relay.as_mut().filter(|relay| relay.owns(arrival.host))
            {
                relay.receive(&mut self.sim, &arrival);
            }
        }
        // Signaling, at once: the relayed address once there is one, and the
        // readiness marker after it.
        if !self.offered
            && let Some(relayed) = self.client.relay().and_then(ClientRelay::relayed)
        {
            self.host
                .conn()
                .add_candidate(relayed, Kind::marked(false, true))
                .unwrap();
            self.host.conn().set_peer_ready();
            self.offered = true;
        }
        while let Some(Ok(len)) = self
            .client
            .session()
            .take_message(CHANNEL, &mut self.message)
        {
            self.at_client.push(self.message[..len].to_vec());
        }
        while let Some(Ok(len)) = self.host.session().take_message(CHANNEL, &mut self.message) {
            self.at_host.push(self.message[..len].to_vec());
        }

        let wait = self
            .client
            .next_timer_ms(now)
            .min(self.host.next_timer_ms(now))
            .min(self.sim.next_delivery_ms().unwrap_or(f64::INFINITY))
            .clamp(1.0, 50.0);
        self.sim.advance_ms(wait);
        let now = self.sim.now_ms();
        self.client.poll(now);
        self.host.poll(now);
    }

    /// Run until `done` or `until_ms`; whether `done` came.
    fn run(&mut self, until_ms: f64, done: impl Fn(&Self) -> bool) -> bool {
        while self.sim.now_ms() < until_ms {
            if done(self) {
                return true;
            }
            self.step();
        }
        done(self)
    }

    fn established(&self) -> bool {
        self.client_path().is_some() && self.host_path().is_some()
    }

    fn client_path(&self) -> Option<SocketAddr> {
        self.client.path()
    }

    fn host_path(&self) -> Option<SocketAddr> {
        self.host.path()
    }

    fn relay_state(&self) -> RelayState {
        self.client.relay().expect("a relay attempt").state()
    }

    fn counts(&self) -> lowlat_sim::relay::Counts {
        self.relay.as_ref().expect("a relay").counts.clone()
    }
}

/// A client behind a symmetric translator, which no direct punch gets
/// through: its checks to a peer leave from a mapping the peer was never
/// told about.
fn symmetric_client(sim: &mut Sim) -> HostId {
    let nat = sim.add_nat(Nat::symmetric(public(1)));
    sim.add_host(private(9, 10, 5000), &[nat])
}

/// Everything about one run but the arenas, which the endpoints borrow.
struct Setup {
    sim: Sim,
    relay: Option<Relay>,
    client_id: HostId,
    host_id: HostId,
    /// The relay as the client is configured with it; `None` for a direct
    /// attempt.
    server: Option<SocketAddr>,
    /// What the host's answer carries.
    host_candidates: Vec<(SocketAddr, Kind)>,
    /// What the client would offer in a direct attempt.
    client_candidates: Vec<(SocketAddr, Kind)>,
}

fn world<'a>(
    setup: Setup,
    client_arena: &'a mut Arena,
    host_arena: &'a mut Arena,
    password: &'static str,
) -> World<'a> {
    let Setup {
        sim,
        relay,
        client_id,
        host_id,
        server,
        host_candidates,
        client_candidates,
    } = setup;
    let client_conn = conn(CLIENT, HOST, 0xA1);
    let mut client = match server {
        Some(server) => Endpoint::relayed(
            client_conn,
            client_arena.session(),
            ClientRelay::new(server, USER, password, [0x5E; 16], 0.0),
        ),
        None => Endpoint::new(client_conn, client_arena.session()),
    };
    let mut host = Endpoint::new(conn(HOST, CLIENT, 0xB2), host_arena.session());
    for (addr, kind) in host_candidates {
        client.add_candidate(addr, kind).unwrap();
    }
    client.conn().set_peer_ready();
    let offered = server.is_none();
    for (addr, kind) in client_candidates {
        host.conn().add_candidate(addr, kind).unwrap();
    }
    if offered {
        host.conn().set_peer_ready();
    }
    World {
        sim,
        relay,
        client,
        client_id,
        host,
        host_id,
        offered,
        at_client: Vec::new(),
        at_host: Vec::new(),
        client_sent_to: Vec::new(),
        message: vec![0u8; 16_384],
    }
}

/// A relay on a public server; the host behind a port-restricted translator.
fn public_relay(with_relay: bool) -> Setup {
    let mut sim = Sim::new(0x5EED_0001);
    let client_id = symmetric_client(&mut sim);
    let host_nat = sim.add_nat(Nat::port_restricted(public(2)));
    let host_id = sim.add_host(private(2, 20, 6000), &[host_nat]);
    let server = SocketAddr::new(public(50), 3478);
    let relay = with_relay.then(|| Relay::new(&mut sim, server, public(50), &[], USER, PASS));
    let host_reflexive = sim.reflexive(host_id, STUN);
    let client_reflexive = sim.reflexive(client_id, STUN);
    Setup {
        sim,
        relay,
        client_id,
        host_id,
        server: with_relay.then_some(server),
        host_candidates: vec![(host_reflexive, Kind::Reflexive)],
        client_candidates: if with_relay {
            Vec::new()
        } else {
            vec![(client_reflexive, Kind::Reflexive)]
        },
    }
}

/// The deployment of docs/03-connectivity.md 7.1: the relay on the host's
/// own machine behind a router that forwards its one port, the relayed
/// addresses the machine's own, the host beside it.
fn host_box() -> (Setup, SocketAddr) {
    let mut sim = Sim::new(0x5EED_0002);
    let client_id = symmetric_client(&mut sim);
    let router = sim.add_nat(Nat::port_restricted(public(2)).with_forward(50_085, the_box(3478)));
    let relay = Relay::new(
        &mut sim,
        the_box(3478),
        the_box(0).ip(),
        &[router],
        USER,
        PASS,
    );
    let host_id = sim.add_host(the_box(22_974), &[router]);
    let host_reflexive = sim.reflexive(host_id, STUN);
    let forwarded = SocketAddr::new(public(2), 50_085);
    (
        Setup {
            sim,
            relay: Some(relay),
            client_id,
            host_id,
            server: Some(forwarded),
            host_candidates: vec![
                (the_box(22_974), Kind::Direct),
                (host_reflexive, Kind::Reflexive),
            ],
            client_candidates: Vec::new(),
        },
        forwarded,
    )
}

/// Send a message each way and run until both have arrived.
fn exchange(world: &mut World<'_>, payload: &[u8], within_ms: f64) -> bool {
    let (client_had, host_had) = (world.at_client.len(), world.at_host.len());
    world
        .client
        .session()
        .send_message(CHANNEL, b"c", payload)
        .unwrap();
    world
        .host
        .session()
        .send_message(CHANNEL, b"h", payload)
        .unwrap();
    let until = world.sim.now_ms() + within_ms;
    world.run(until, |world| {
        world.at_client.len() > client_had && world.at_host.len() > host_had
    })
}

/// The case the relay exists for: a client no punch reaches, reached through
/// the relay; and the same pair without it, timing out, so this passes on the
/// relay rather than on a topology anything could cross.
#[test]
fn a_symmetric_client_reaches_the_host_through_the_relay_and_not_without_it() {
    let (mut client_arena, mut host_arena) = (Arena::new(), Arena::new());
    let mut direct = world(
        public_relay(false),
        &mut client_arena,
        &mut host_arena,
        PASS,
    );
    direct.run(conn::PUNCH_WINDOW_MS + 100.0, World::established);
    assert!(
        !direct.established(),
        "a direct punch got through a symmetric translator"
    );
    assert!(matches!(
        direct.client.conn().state(),
        conn::State::Failed(_)
    ));

    let (mut client_arena, mut host_arena) = (Arena::new(), Arena::new());
    let mut relayed = world(public_relay(true), &mut client_arena, &mut host_arena, PASS);
    assert!(
        relayed.run(conn::PUNCH_WINDOW_MS, World::established),
        "no path through the relay"
    );
    assert_eq!(
        relayed.host_path(),
        relayed.client.relay().and_then(ClientRelay::relayed)
    );
    assert!(exchange(&mut relayed, b"through the relay", 2_000.0));
    assert_eq!(relayed.client_sent_to, [SocketAddr::new(public(50), 3478)]);
}

/// The host-box deployment. The client talks to the router's one forwarded
/// port and nothing else; the relayed address is the machine's own, which
/// nothing outside can reach, and the host reaches it without leaving the
/// machine.
#[test]
fn the_host_box_deployment_with_one_forwarded_port() {
    let (setup, forwarded) = host_box();
    let (mut client_arena, mut host_arena) = (Arena::new(), Arena::new());
    let mut world = world(setup, &mut client_arena, &mut host_arena, PASS);
    assert!(
        world.run(conn::PUNCH_WINDOW_MS, World::established),
        "no path"
    );

    let relayed = world.client.relay().and_then(ClientRelay::relayed).unwrap();
    assert_eq!(
        relayed.ip(),
        the_box(0).ip(),
        "the relayed address is not the machine's own"
    );
    assert_eq!(world.host_path(), Some(relayed));
    assert_eq!(world.client_path(), Some(the_box(22_974)));
    assert!(exchange(&mut world, b"over the one port", 2_000.0));
    assert_eq!(
        world.client_sent_to,
        [forwarded],
        "the client sent outside the relay"
    );
    let counts = world.counts();
    assert_eq!(counts.allocations, 1);
    assert_eq!(counts.unpermitted, 0, "a datagram met no permission");
}

/// A host on the relay's machine that offers only its public address. The
/// relay's own machine permitted before the relayed address is offered is all
/// that lets its first check through: its checks come from the machine's own
/// address, and nothing it offered names that address.
#[test]
fn the_relays_own_machine_is_permitted_before_anything_is_offered() {
    let (mut setup, _) = host_box();
    setup
        .host_candidates
        .retain(|(_, kind)| *kind == Kind::Reflexive);
    let (mut client_arena, mut host_arena) = (Arena::new(), Arena::new());
    let mut world = world(setup, &mut client_arena, &mut host_arena, PASS);
    assert!(
        world.run(conn::PUNCH_WINDOW_MS, World::established),
        "no path"
    );
    assert_eq!(world.counts().unpermitted, 0, "a check met no permission");
    assert!(exchange(&mut world, b"from the box", 2_000.0));
}

/// Floor-sized datagrams both ways, as indications through a relay that binds
/// no channel and as channel data through one that does. A relay's framing
/// added to a full datagram is what a buffer sized for the datagram alone
/// drops, and a refused binding leaves media on indications, not stopped.
#[test]
fn full_size_datagrams_cross_both_ways_in_both_framings() {
    let floor = lowlat_core::DEFAULT_DATAGRAM;
    for channels in [false, true] {
        let (mut setup, _) = host_box();
        if !channels {
            setup.relay = setup.relay.map(Relay::refusing_channels);
        }
        let (mut client_arena, mut host_arena) = (Arena::new(), Arena::new());
        let mut world = world(setup, &mut client_arena, &mut host_arena, PASS);
        assert!(
            world.run(conn::PUNCH_WINDOW_MS, World::established),
            "no path"
        );

        let large: Vec<u8> = (0u8..=250).cycle().take(3_000).collect();
        assert!(exchange(&mut world, &large, 2_000.0));
        world.run(world.sim.now_ms() + 500.0, |_| false);
        assert!(exchange(&mut world, &large, 2_000.0));
        assert_eq!(world.at_client.len() + world.at_host.len(), 4);
        for arrived in world.at_client.iter().chain(world.at_host.iter()) {
            assert_eq!(&arrived[1..], &large[..], "a large message arrived damaged");
        }

        let counts = world.counts();
        if channels {
            assert_eq!(
                counts.largest_channel_in,
                floor + 4,
                "client to relay, channel data"
            );
            assert_eq!(
                counts.largest_channel_out,
                floor + 4,
                "relay to client, channel data"
            );
        } else {
            assert_eq!(
                counts.channel_in + counts.channel_out,
                0,
                "a channel was used"
            );
            assert_eq!(
                counts.largest_indication_in,
                floor + 36 + 3,
                "client to relay, indication"
            );
            assert_eq!(
                counts.largest_indication_out,
                floor + 36 + 3,
                "relay to client, indication"
            );
        }
    }
}

/// Sixteen minutes: every permission crosses 300 seconds three times, the
/// allocation is refreshed at each half-life, and the relay's nonce rotates
/// twice. Media every 20 ms from the host at a wide-area round trip, a message
/// each second from the client, and every one arrives: a permission renewed
/// as it lapses rather than before would drop what crosses in between.
///
/// Through a relay that binds no channel as well as one that does: a binding
/// renews its address's permission too, and can hide a permission renewed
/// late. Without one the permission is all that keeps the path.
#[test]
fn the_path_is_kept_across_three_permission_lifetimes_and_two_nonce_rotations() {
    for channels in [false, true] {
        kept_for_sixteen_minutes(channels);
    }
}

fn kept_for_sixteen_minutes(channels: bool) {
    let (mut setup, _) = host_box();
    setup.sim = setup.sim.with_link(lowlat_sim::Link {
        one_way_ms: 25.0,
        ..lowlat_sim::Link::default()
    });
    setup.relay = setup.relay.map(|relay| {
        let relay = relay.rotating_nonce_every(400_000.0);
        if channels {
            relay
        } else {
            relay.refusing_channels()
        }
    });
    let (mut client_arena, mut host_arena) = (Arena::new(), Arena::new());
    let mut world = world(setup, &mut client_arena, &mut host_arena, PASS);
    assert!(
        world.run(conn::PUNCH_WINDOW_MS, World::established),
        "no path"
    );

    let (mut to_client, mut to_host) = (0, 0);
    let (mut media_at, mut message_at) = (world.sim.now_ms(), world.sim.now_ms());
    while world.sim.now_ms() < 960_000.0 {
        let now = world.sim.now_ms();
        if now >= media_at {
            let frame = format!("frame {to_client}");
            // A full ring is media that stopped leaving: the path lapsed.
            assert!(
                world
                    .host
                    .session()
                    .send_message(CHANNEL, &[], frame.as_bytes())
                    .is_ok(),
                "the host's media stalled at {now} ms, channels={channels}"
            );
            to_client += 1;
            media_at += 20.0;
        }
        if now >= message_at {
            let tick = format!("tick {to_host}");
            assert!(
                world
                    .client
                    .session()
                    .send_message(CHANNEL, &[], tick.as_bytes())
                    .is_ok(),
                "the client's messages stalled at {now} ms, channels={channels}"
            );
            to_host += 1;
            message_at += 1_000.0;
        }
        world.step();
    }
    world.run(world.sim.now_ms() + 2_000.0, |_| false);

    let now = world.sim.now_ms();
    assert_eq!(
        world.relay_state(),
        RelayState::Ready(world.host_path().unwrap())
    );
    assert_eq!(world.client.health(now), Health::Alive);
    assert_eq!(
        world.at_client.len(),
        to_client,
        "media to the client went missing, channels={channels}"
    );
    assert_eq!(
        world.at_host.len(),
        to_host,
        "messages to the host went missing"
    );

    let counts = world.counts();
    assert_eq!(
        counts.unpermitted, 0,
        "a permission lapsed, channels={channels}: {counts:?}"
    );
    assert!(
        counts.stale_nonces >= 2,
        "the nonce never rotated: {counts:?}"
    );
    assert!(
        counts.refreshes >= 3,
        "the allocation was not refreshed: {counts:?}"
    );
    assert!(
        counts.permissions >= 2 * 4,
        "permissions were not renewed: {counts:?}"
    );
}

/// A host that offers a loopback candidate, hostile or mistaken. Relayed
/// toward, it would destroy the allocation; it is never permitted or checked,
/// and the session carries on.
#[test]
fn a_loopback_candidate_does_not_end_the_session() {
    let (mut setup, _) = host_box();
    setup.host_candidates.insert(
        0,
        (
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 22_974),
            Kind::Direct,
        ),
    );
    let (mut client_arena, mut host_arena) = (Arena::new(), Arena::new());
    let mut world = world(setup, &mut client_arena, &mut host_arena, PASS);
    assert!(
        world.run(conn::PUNCH_WINDOW_MS, World::established),
        "no path"
    );
    assert!(exchange(&mut world, b"still here", 2_000.0));
    assert_eq!(
        world.counts().destroyed,
        0,
        "an allocation was relayed toward loopback"
    );
    assert!(matches!(world.relay_state(), RelayState::Ready(_)));
}

/// Nothing answers at the configured address: unreachable, at the deadline.
#[test]
fn a_relay_that_is_not_there_is_unreachable() {
    let (mut setup, _) = host_box();
    setup.server = Some(SocketAddr::new(public(77), 3478));
    let (mut client_arena, mut host_arena) = (Arena::new(), Arena::new());
    let mut world = world(setup, &mut client_arena, &mut host_arena, PASS);
    world.run(lowlat_core::relay::SETUP_DEADLINE_MS + 100.0, |world| {
        world.relay_state() != RelayState::Setup
    });
    assert_eq!(
        world.relay_state(),
        RelayState::Failed(Failure::Unreachable)
    );
    assert!(world.sim.now_ms() >= lowlat_core::relay::SETUP_DEADLINE_MS);
}

/// The wrong password: the relay challenges the credentials it was sent, and
/// the attempt ends refused rather than asking again.
#[test]
fn wrong_credentials_are_refused() {
    let (setup, _) = host_box();
    let (mut client_arena, mut host_arena) = (Arena::new(), Arena::new());
    let mut world = world(
        setup,
        &mut client_arena,
        &mut host_arena,
        "not the password",
    );
    world.run(1_000.0, |world| world.relay_state() != RelayState::Setup);
    assert_eq!(world.relay_state(), RelayState::Failed(Failure::Refused));
    assert_eq!(world.counts().allocations, 0);
}

/// A relay that restarted has let every allocation go; the next renewal is
/// refused, and the attempt ends lost.
#[test]
fn a_relay_that_forgets_the_allocation_is_lost() {
    let (setup, _) = host_box();
    let (mut client_arena, mut host_arena) = (Arena::new(), Arena::new());
    let mut world = world(setup, &mut client_arena, &mut host_arena, PASS);
    assert!(
        world.run(conn::PUNCH_WINDOW_MS, World::established),
        "no path"
    );
    world.relay.as_mut().unwrap().forget_allocations();
    world.run(400_000.0, |world| {
        world.relay_state() != RelayState::Ready(world.host_path().unwrap())
    });
    assert_eq!(world.relay_state(), RelayState::Failed(Failure::Lost));
}
