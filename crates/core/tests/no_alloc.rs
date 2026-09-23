//! Phase 1 gate 5: the data paths allocate nothing.
//!
//! The core is `no_std` without `alloc`, so it *cannot* allocate directly.
//! That is not the whole claim. This asserts the paths behave as promised when
//! linked into a program that does have an allocator, which is how they will
//! actually run, and it catches a dependency that allocates on our behalf.
//!
//! Setup is free to allocate. Only the region inside `assert_no_alloc` is
//! covered, which is exactly the receive, send, and reassembly hot paths.

use lowlat_common::alloc_counter::{self, Counting};
use lowlat_core::channel::{RecvRing, SlotMeta};
use lowlat_core::envelope::Envelope;
use lowlat_core::message::Message;
use lowlat_core::packet::{self, Data};
use lowlat_core::send::{SendRing, SendSlot};
use lowlat_core::session::Session;
use lowlat_core::stun::{self, TransactionId};

#[global_allocator]
static ALLOC: Counting = Counting;

const KEY: [u8; 32] = [0x5Au8; 32];
const SLOT: usize = 1500;
const SLOTS: usize = 64;
const CHANNEL: u8 = 1;

#[test]
fn envelope_seal_and_open_do_not_allocate() {
    let envelope = Envelope::from_key(&KEY).unwrap();
    let plaintext = [0xABu8; 1200];
    let mut wire = [0u8; 1400];
    let mut out = [0u8; 1400];

    alloc_counter::assert_no_alloc(|| {
        for counter in 0..64u64 {
            let n = envelope.seal(counter, &plaintext, &mut wire).unwrap();
            let opened = envelope.open(&wire[..n], &mut out).unwrap();
            std::hint::black_box(opened.counter);
        }
    });
}

/// Connectivity checks are cold compared with media, but the digest crates are
/// a new dependency and this is the check that they keep their state on the
/// stack rather than allocating on our behalf.
#[test]
fn connectivity_checks_do_not_allocate() {
    let tid = TransactionId([7u8; 12]);
    let mut wire = [0u8; stun::MAX_BUILT];

    alloc_counter::assert_no_alloc(|| {
        for round in 0..64u8 {
            let len =
                stun::encode_binding_request(&mut wire, tid, "loca", "remo", [round; 8], "secret")
                    .unwrap();
            let message = stun::Message::parse(&wire[..len]).unwrap();
            std::hint::black_box(message.verify("secret"));
            std::hint::black_box(message.username());
        }
    });
}

#[test]
fn packet_parse_and_encode_do_not_allocate() {
    let body = [0x11u8; 1100];
    let data = Data {
        channel: CHANNEL,
        seq: 12345,
        last: true,
        body: &body,
    };
    let mut buf = [0u8; 1400];

    alloc_counter::assert_no_alloc(|| {
        for _ in 0..256 {
            let n = packet::encode_data(&mut buf, &data).unwrap();
            std::hint::black_box(packet::parse(&buf[..n]).unwrap());
        }
    });
}

#[test]
fn ring_store_and_reassembly_do_not_allocate() {
    let mut bodies = vec![0u8; SLOT * SLOTS];
    let mut meta = vec![SlotMeta::default(); SLOTS];
    let mut ring = RecvRing::new(&mut bodies, &mut meta, SLOT).unwrap();

    let payload = [0x7Fu8; 3000];
    let message = Message::new(&[], &payload).unwrap();
    let mut fragment = [0u8; SLOT];
    let mut out = vec![0u8; 8192];

    alloc_counter::assert_no_alloc(|| {
        let mut seq = 0u32;
        for _ in 0..16 {
            let mut index = 0;
            while let Some(result) = message.fragment(index, SLOT, &mut fragment) {
                let written = result.unwrap();
                ring.store(seq, &fragment[..written.len]);
                seq = seq.wrapping_add(1);
                index += 1;
            }
            let len = ring.take_message(&mut out).unwrap().unwrap();
            std::hint::black_box(len);
        }
    });
}

/// The reader's look-ahead: counting, peeking and skipping over messages that
/// have arrived, which a client runs on its receive thread whenever its
/// decoder falls behind.
#[test]
fn ring_look_ahead_does_not_allocate() {
    let mut bodies = vec![0u8; SLOT * SLOTS];
    let mut meta = vec![SlotMeta::default(); SLOTS];
    let mut ring = RecvRing::new(&mut bodies, &mut meta, SLOT).unwrap();

    let payload = [0x7Fu8; 3000];
    let message = Message::new(&[], &payload).unwrap();
    let mut fragment = [0u8; SLOT];
    let mut head = [0u8; 21];

    alloc_counter::assert_no_alloc(|| {
        let mut seq = 0u32;
        for _ in 0..16 {
            for _ in 0..4 {
                let mut index = 0;
                while let Some(result) = message.fragment(index, SLOT, &mut fragment) {
                    let written = result.unwrap();
                    ring.store(seq, &fragment[..written.len]);
                    seq = seq.wrapping_add(1);
                    index += 1;
                }
            }
            assert_eq!(ring.pending_messages(), 4);
            for n in 0..4 {
                std::hint::black_box(ring.peek_message(n, &mut head).unwrap());
            }
            assert_eq!(ring.skip_messages(4), 4);
        }
    });
}

/// The initialization body, written on the connecting side.
#[test]
fn init_encode_does_not_allocate() {
    let init = lowlat_core::init::parse(
        b"{\"_version\":1,\"_max_w\":4096,\"_max_h\":4096,\"_flags\":8,\"_VideoProtocolVersion\":1}",
    )
    .unwrap();
    let mut out = [0u8; 512];
    alloc_counter::assert_no_alloc(|| {
        for _ in 0..64 {
            let len = lowlat_core::init::encode(&mut out, &init).unwrap();
            std::hint::black_box(len);
        }
    });
}

#[test]
fn send_ring_enqueue_and_drain_do_not_allocate() {
    let mut bodies = vec![0u8; SLOT * SLOTS];
    let mut meta = vec![SendSlot::default(); SLOTS];
    let mut ring = SendRing::new(&mut bodies, &mut meta, SLOT, CHANNEL).unwrap();
    let payload = [0x33u8; 2000];
    let mut out = [0u8; 1600];

    alloc_counter::assert_no_alloc(|| {
        let message = Message::new(&[], &payload).unwrap();
        ring.enqueue(&message, 0.0).unwrap();
        ring.begin_pass();
        while let Some(result) = ring.poll_send(0.0, 10.0, 1, &mut out) {
            std::hint::black_box(result.unwrap());
        }
    });
}

/// The whole loop the shell will run, under the counter.
#[test]
fn a_session_round_does_not_allocate() {
    let mut recv_bodies = vec![0u8; SLOT * SLOTS];
    let mut recv_meta = vec![SlotMeta::default(); SLOTS];
    let mut send_bodies = vec![0u8; SLOT * SLOTS];
    let mut send_meta = vec![SendSlot::default(); SLOTS];

    let mut session = Session::new(Envelope::from_key(&KEY).unwrap(), 1, 0.0);
    session
        .attach_recv(
            CHANNEL,
            RecvRing::new(&mut recv_bodies, &mut recv_meta, SLOT).unwrap(),
        )
        .unwrap();
    session
        .attach_send(
            CHANNEL,
            SendRing::new(&mut send_bodies, &mut send_meta, SLOT, CHANNEL).unwrap(),
        )
        .unwrap();

    let payload = [0x42u8; 2500];
    let mut wire = [0u8; 1600];
    let mut scratch = [0u8; 1600];
    let mut message = vec![0u8; 8192];

    // Prime the paths once so any lazy one-time setup happens outside the
    // assertion rather than being blamed on the hot loop.
    session.send_message(CHANNEL, &[], &payload).unwrap();
    while let Some(result) = session.get_output(0.0, &mut wire) {
        result.unwrap();
    }

    alloc_counter::assert_no_alloc(|| {
        for round in 1..32u32 {
            let now = f64::from(round);
            session.send_message(CHANNEL, &[], &payload).unwrap();
            while let Some(result) = session.get_output(now, &mut wire) {
                let written = result.unwrap();
                session
                    .process_input(&wire[..written], now, &mut scratch)
                    .unwrap();
            }
            session.poll(now);
            while let Some(result) = session.take_message(CHANNEL, &mut message) {
                std::hint::black_box(result.unwrap());
            }
        }
    });
}

/// A relay's answers, from the layout: this tier cannot reach the crate's own
/// test helpers. A challenge to the first allocation, and a success under the
/// long-term key to everything after it.
fn relay_answer(request: &[u8], relayed: std::net::SocketAddr) -> Vec<u8> {
    use hmac::{Hmac, Mac};
    use md5::{Digest, Md5};
    use sha1::Sha1;

    fn attribute(out: &mut Vec<u8>, kind: u16, value: &[u8]) {
        out.extend_from_slice(&kind.to_be_bytes());
        out.extend_from_slice(&u16::try_from(value.len()).unwrap().to_be_bytes());
        out.extend_from_slice(value);
        out.resize(out.len().next_multiple_of(4), 0);
    }
    fn set_length(out: &mut [u8], length: usize) {
        out[2..4].copy_from_slice(&u16::try_from(length).unwrap().to_be_bytes());
    }

    let kind = u16::from_be_bytes([request[0], request[1]]);
    let mut signed = false;
    let mut at = 20;
    while at + 4 <= request.len() {
        let len = usize::from(u16::from_be_bytes([request[at + 2], request[at + 3]]));
        signed |= request[at..at + 2] == [0x00, 0x06];
        at += 4 + len.next_multiple_of(4);
    }
    let mut out = Vec::new();
    out.extend_from_slice(&(kind | 0x0100).to_be_bytes());
    out.extend_from_slice(&[0, 0]);
    out.extend_from_slice(&request[4..20]);
    if kind == 0x0003 && !signed {
        out[1] = 0x13;
        attribute(&mut out, 0x0009, &[0, 0, 4, 1]);
        attribute(&mut out, 0x0015, b"5d1b0a4f3c2e7a90");
        attribute(&mut out, 0x0014, b"relay.example");
        let length = out.len() - 20;
        set_length(&mut out, length);
        return out;
    }
    if kind == 0x0003 {
        let std::net::IpAddr::V4(ip) = relayed.ip() else {
            unreachable!()
        };
        let mut value = vec![0, 1];
        value.extend_from_slice(&(relayed.port() ^ 0x2112).to_be_bytes());
        value.extend(
            ip.octets()
                .iter()
                .zip([0x21, 0x12, 0xA4, 0x42])
                .map(|(a, b)| a ^ b),
        );
        attribute(&mut out, 0x0016, &value);
        attribute(&mut out, 0x000D, &600u32.to_be_bytes());
    }
    let length = out.len() - 20 + 24;
    set_length(&mut out, length);
    let key = Md5::digest(b"user:relay.example:password");
    let mut mac = Hmac::<Sha1>::new_from_slice(&key).unwrap();
    mac.update(&out);
    attribute(&mut out, 0x0008, &mac.finalize().into_bytes());
    out
}

/// The relayed path under the counter, both ways and in both framings: a
/// record wrapped for the relay, the relay's datagram unwrapped and
/// classified, first as indications and then on a bound channel. The endpoint
/// is its own peer here -- the same credentials both ways -- so a relay that
/// hands back what it is sent, from the peer it was sent to, is all the
/// network there is.
#[test]
fn a_relayed_round_does_not_allocate() {
    use lowlat_core::conn::{Conn, Credentials, Kind};
    use lowlat_core::endpoint::Endpoint;
    use lowlat_core::relay::Relay;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    let server = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 1)), 3478);
    let relayed = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 20)), 50_048);
    let peer = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 20)), 22_974);

    let mut recv_bodies = vec![0u8; SLOT * SLOTS];
    let mut recv_meta = vec![SlotMeta::default(); SLOTS];
    let mut send_bodies = vec![0u8; SLOT * SLOTS];
    let mut send_meta = vec![SendSlot::default(); SLOTS];
    let mut session = Session::new(Envelope::from_key(&KEY).unwrap(), 1, 0.0);
    session
        .attach_recv(
            CHANNEL,
            RecvRing::new(&mut recv_bodies, &mut recv_meta, SLOT).unwrap(),
        )
        .unwrap();
    session
        .attach_send(
            CHANNEL,
            SendRing::new(&mut send_bodies, &mut send_meta, SLOT, CHANNEL).unwrap(),
        )
        .unwrap();
    let conn = Conn::new(
        Credentials {
            local_ufrag: "aaaa",
            local_pwd: "passwordforaaaa",
            remote_ufrag: "aaaa",
            remote_pwd: "passwordforaaaa",
        },
        [7; 16],
        0.0,
    );
    let relay = Relay::new(server, "user", "password", [9; 16], 0.0);
    let mut endpoint = Endpoint::relayed(conn, session, relay);
    endpoint.add_candidate(peer, Kind::Direct).unwrap();
    endpoint.conn().set_peer_ready();

    let mut wire = [0u8; 2100];
    let mut scratch = [0u8; 2100];
    let mut message = vec![0u8; 8192];
    let payload = [0x42u8; 2500];

    // One pass: requests answered, except a channel binding while `bind` is
    // false; a datagram for the peer handed straight back as the relay
    // delivers it -- an indication under the data method, channel data as it
    // is.
    let indications = std::cell::Cell::new(0u32);
    let channels = std::cell::Cell::new(0u32);
    let taken = std::cell::Cell::new(0u32);
    let mut pass = |endpoint: &mut Endpoint<'_>, now: f64, bind: bool| {
        while let Some(result) = endpoint.get_output(now, &mut wire) {
            let len = result.unwrap().len;
            let kind = u16::from_be_bytes([wire[0], wire[1]]);
            if wire[0] >> 6 == 0b01 || kind == 0x0016 {
                if kind == 0x0016 {
                    wire[1] = 0x17;
                    indications.set(indications.get() + 1);
                } else {
                    channels.set(channels.get() + 1);
                }
                endpoint
                    .process_input(&wire[..len], server, None, now, &mut scratch)
                    .unwrap();
            } else if kind != 0x0009 || bind {
                let answer = relay_answer(&wire[..len], relayed);
                endpoint
                    .process_input(&answer, server, None, now, &mut scratch)
                    .unwrap();
            }
        }
        endpoint.poll(now);
    };

    // Setup, outside the counter: allocated, permitted, a path, and the
    // channel's binding asked for and left unanswered.
    let mut now = 0.0;
    while endpoint.path().is_none() {
        assert!(now < 2_000.0, "no path");
        pass(&mut endpoint, now, false);
        now += 10.0;
    }
    endpoint
        .session()
        .send_message(CHANNEL, &[], &payload)
        .unwrap();
    pass(&mut endpoint, now, false);

    let mut round = |endpoint: &mut Endpoint<'_>, now: f64, bind: bool| {
        endpoint
            .session()
            .send_message(CHANNEL, &[], &payload)
            .unwrap();
        pass(endpoint, now, bind);
        while let Some(result) = endpoint.session().take_message(CHANNEL, &mut message) {
            std::hint::black_box(result.unwrap());
            taken.set(taken.get() + 1);
        }
    };

    let before = (indications.get(), taken.get());
    alloc_counter::assert_no_alloc(|| {
        for step in 1..32u32 {
            round(&mut endpoint, now + f64::from(step), false);
        }
    });
    // Denominators: an assertion over nothing is not one.
    assert!(indications.get() > before.0, "no indication crossed");
    assert!(taken.get() > before.1, "no message arrived");
    assert_eq!(channels.get(), 0, "a channel was used before it was bound");

    // The binding answered when it is next sent: channel data from there on.
    let bound = now + lowlat_core::relay::RESEND_MS + 40.0;
    round(&mut endpoint, bound, true);
    let before = (indications.get(), channels.get(), taken.get());
    alloc_counter::assert_no_alloc(|| {
        for step in 1..32u32 {
            round(&mut endpoint, bound + f64::from(step), false);
        }
    });
    assert!(channels.get() > before.1, "no channel data crossed");
    assert_eq!(indications.get(), before.0, "media stayed on indications");
    assert!(taken.get() > before.2, "no message arrived");
}
