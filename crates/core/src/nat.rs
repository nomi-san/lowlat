//! The translation in front of a socket, told from what reflexive servers
//! report of it.
//!
//! **The mapping alone decides the number.** Whether a peer reaches a socket
//! at the address it was told depends on how the translator maps, not on how
//! it filters: a mapping independent of the destination is one public port
//! for everyone, so the port a peer is told is the port its datagrams reach;
//! a dependent one is a new port per destination, and the port a peer is told
//! was made for somebody else.
//!
//! **Two server addresses are the least that tells.** One answer says where
//! the translator put the socket for one destination; only a second
//! destination says whether that place is the same for everyone. Two ports
//! of one address then split address-dependent mapping from
//! address-and-port-dependent mapping, which the number does not need and the
//! verdict carries anyway.
//!
//! The servers are plain reflexive servers, the ones an attempt asks: nothing
//! asks one to answer from elsewhere, so filtering is not measured. IPv4
//! alone is read; the other family has no translation to speak of.
//!
//! Pure like the rest of the core. The probe is sans-IO: requests out,
//! answers in, on the caller's clock, and the caller's deadline.

use core::net::SocketAddr;

use crate::conn::{CHECK_CADENCE_MS, MAX_SERVERS, PACING_MS, SentIds, derive_transaction_id};
use crate::error::{Error, Result};
use crate::stun::{self, Message, Method};

/// How a translator maps one socket's traffic, as far as the answers tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mapping {
    /// No server answered.
    NoAnswer,
    /// Answers from fewer than two server addresses: nothing to compare.
    Unknown,
    /// Nothing translates: the first server saw the socket's own address.
    None,
    /// One public address and port, whatever the destination.
    Independent,
    /// A public port per destination, the servers asked not telling whether
    /// per address or per address and port.
    Dependent,
    /// A public port per destination address: two ports of one server saw
    /// one port.
    AddressDependent,
    /// A public port per destination address and port.
    AddressAndPortDependent,
}

/// One server's answer: where the server is, and where it saw the socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Answer {
    pub server: SocketAddr,
    pub mapped: SocketAddr,
}

/// What a socket's answers say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Verdict {
    pub mapping: Mapping,
    /// Where the first server to answer saw the socket.
    pub public: Option<SocketAddr>,
    /// Whether that is the socket's own port.
    pub port_preserved: bool,
}

/// What `answers` say of the translation in front of a socket, read in the
/// order the servers were asked. `local` is the socket as this side knows
/// it: the address it sends from toward the servers, and its port.
pub fn classify(local: Option<SocketAddr>, answers: &[Answer]) -> Verdict {
    let heard = || answers.iter().filter_map(v4);
    let Some(first) = heard().next() else {
        return Verdict {
            mapping: Mapping::NoAnswer,
            public: None,
            port_preserved: false,
        };
    };
    let public = first.mapped;
    let local = local.map(stun::canonical);
    let verdict = |mapping| Verdict {
        mapping,
        public: Some(public),
        port_preserved: local.is_some_and(|local| local.port() == public.port()),
    };
    if local == Some(public) {
        return verdict(Mapping::None);
    }
    if !heard().any(|answer| answer.server.ip() != first.server.ip()) {
        return verdict(Mapping::Unknown);
    }
    if heard().all(|answer| answer.mapped == public) {
        return verdict(Mapping::Independent);
    }
    // Two ports of one address: the same public port for both is a mapping
    // kept per address, two are a mapping per address and port.
    let mut pairs = heard().flat_map(|a| {
        heard()
            .filter(move |b| b.server.ip() == a.server.ip() && b.server.port() != a.server.port())
            .map(move |b| a.mapped == b.mapped)
    });
    let Some(same) = pairs.next() else {
        return verdict(Mapping::Dependent);
    };
    if same && pairs.all(|same| same) {
        verdict(Mapping::AddressDependent)
    } else {
        verdict(Mapping::AddressAndPortDependent)
    }
}

/// An answer of the family that is read, in its plain form.
fn v4(answer: &Answer) -> Option<Answer> {
    let server = stun::canonical(answer.server);
    let mapped = stun::canonical(answer.mapped);
    (server.is_ipv4() && mapped.is_ipv4()).then_some(Answer { server, mapped })
}

/// The console numbering: 1 when nothing translates; 2 when one public port
/// serves every destination, or when the gateway's mapping of the port is
/// confirmed -- a peer reaches that whatever the translator does with the
/// rest; 3 when a port is made per destination or nothing answered; none
/// when the answers cannot tell.
pub fn number(mapping: Mapping, confirmed: bool) -> Option<u8> {
    match mapping {
        Mapping::None => Some(1),
        _ if confirmed => Some(2),
        Mapping::Independent => Some(2),
        Mapping::Dependent
        | Mapping::AddressDependent
        | Mapping::AddressAndPortDependent
        | Mapping::NoAnswer => Some(3),
        Mapping::Unknown => None,
    }
}

/// Asks each server where it sees the socket, until it says.
///
/// The request is the one an attempt sends a reflexive server, on the same
/// cadence, and an answer is admitted by the same rule: a transaction this
/// probe sent, from the address it was sent to. An answer carries no
/// credentials, so that rule is all that keeps another sender from naming
/// the socket's place.
#[derive(Debug)]
pub struct Probe {
    seed: [u8; 16],
    counter: u32,
    servers: [Option<Asked>; MAX_SERVERS],
    last_sent_ms: Option<f64>,
}

#[derive(Debug, Clone, Copy)]
struct Asked {
    addr: SocketAddr,
    last_ms: Option<f64>,
    sent: SentIds,
    mapped: Option<SocketAddr>,
}

impl Probe {
    /// A probe whose transaction identifiers derive from `seed`, which the
    /// caller draws unpredictably: the identifier is what admits an answer.
    pub fn new(seed: [u8; 16]) -> Self {
        Self {
            seed,
            counter: 0,
            servers: [None; MAX_SERVERS],
            last_sent_ms: None,
        }
    }

    /// Ask `addr` too. One already asked is ignored; past [`MAX_SERVERS`]
    /// the addition is refused.
    pub fn add_server(&mut self, addr: SocketAddr) -> Result<()> {
        let addr = stun::canonical(addr);
        if self.servers.iter().flatten().any(|s| s.addr == addr) {
            return Ok(());
        }
        let slot = self
            .servers
            .iter_mut()
            .find(|slot| slot.is_none())
            .ok_or(Error::Oversized)?;
        *slot = Some(Asked {
            addr,
            last_ms: None,
            sent: SentIds::new(),
            mapped: None,
        });
        Ok(())
    }

    /// How many servers are asked.
    pub fn asked(&self) -> usize {
        self.servers.iter().flatten().count()
    }

    /// Every server asked has answered: a probe asking none is done at once.
    pub fn done(&self) -> bool {
        self.servers.iter().flatten().all(|s| s.mapped.is_some())
    }

    /// Each server that answered and where it saw the socket, in the order
    /// the servers were added.
    pub fn answers(&self) -> impl Iterator<Item = Answer> + '_ {
        self.servers.iter().flatten().filter_map(|s| {
            s.mapped.map(|mapped| Answer {
                server: s.addr,
                mapped,
            })
        })
    }

    /// Milliseconds until a request is due; never, once every server has
    /// answered.
    pub fn next_timer_ms(&self, now_ms: f64) -> f64 {
        let pace = self
            .last_sent_ms
            .map_or(0.0, |last| (last + PACING_MS - now_ms).max(0.0));
        self.servers
            .iter()
            .flatten()
            .filter(|s| s.mapped.is_none())
            .map(|s| match s.last_ms {
                Some(last) => (last + CHECK_CADENCE_MS - now_ms).max(pace),
                None => pace,
            })
            .fold(f64::INFINITY, f64::min)
    }

    /// The next request due, written into `out`: where it goes, and its
    /// length. Drive until `None`.
    pub fn get_output(
        &mut self,
        now_ms: f64,
        out: &mut [u8],
    ) -> Option<Result<(SocketAddr, usize)>> {
        if self
            .last_sent_ms
            .is_some_and(|last| now_ms - last < PACING_MS)
        {
            return None;
        }
        let index = self.servers.iter().position(|slot| {
            slot.as_ref().is_some_and(|s| {
                s.mapped.is_none()
                    && s.last_ms
                        .is_none_or(|last| now_ms - last >= CHECK_CADENCE_MS)
            })
        })?;
        let tid = derive_transaction_id(&self.seed, self.counter);
        let len = match stun::encode_reflexive_request(out, tid) {
            Ok(len) => len,
            Err(error) => return Some(Err(error)),
        };
        let server = self.servers.get_mut(index).and_then(Option::as_mut)?;
        server.last_ms = Some(now_ms);
        server.sent.push(tid);
        self.counter = self.counter.wrapping_add(1);
        self.last_sent_ms = Some(now_ms);
        Some(Ok((server.addr, len)))
    }

    /// A datagram arrived from `from`. Admitted only as the answer to a
    /// request this probe sent there; anything else is refused and changes
    /// nothing.
    pub fn process_input(&mut self, datagram: &[u8], from: SocketAddr) -> Result<()> {
        let from = stun::canonical(from);
        let message = Message::parse(datagram)?;
        if message.method() != Method::BindingSuccess {
            return Err(Error::Malformed);
        }
        let tid = message.transaction_id();
        let server = self
            .servers
            .iter_mut()
            .flatten()
            .find(|s| s.addr == from && s.sent.contains(tid))
            .ok_or(Error::Decrypt)?;
        let mapped = message.mapped_address().ok_or(Error::Malformed)?;
        server.mapped = Some(mapped);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    fn at(a: u8, b: u8, port: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(198, 51, a, b)), port)
    }

    fn answer(server: SocketAddr, mapped: SocketAddr) -> Answer {
        Answer { server, mapped }
    }

    const LOCAL: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 20)), 25637);

    /// Each mapping from its answers, the number beside it: the table the
    /// console numbering is read from, one server pair at a time.
    #[test]
    fn each_mapping_is_told_from_its_answers() {
        let (a, a2, b, c) = (
            at(1, 1, 3478),
            at(1, 1, 19302),
            at(2, 2, 3478),
            at(3, 3, 3478),
        );
        let kept = at(9, 9, 25637);
        let moved = at(9, 9, 40001);
        let again = at(9, 9, 40002);
        let cases: &[(&[Answer], Mapping, Option<u8>)] = &[
            (&[], Mapping::NoAnswer, Some(3)),
            (&[answer(a, kept)], Mapping::Unknown, None),
            // Two ports of one address are one address: still unknown.
            (&[answer(a, kept), answer(a2, kept)], Mapping::Unknown, None),
            (
                &[answer(a, kept), answer(b, kept)],
                Mapping::Independent,
                Some(2),
            ),
            (
                &[answer(a, kept), answer(b, kept), answer(c, kept)],
                Mapping::Independent,
                Some(2),
            ),
            (
                &[answer(a, kept), answer(b, moved)],
                Mapping::Dependent,
                Some(3),
            ),
            (
                &[answer(a, kept), answer(a2, kept), answer(b, moved)],
                Mapping::AddressDependent,
                Some(3),
            ),
            (
                &[answer(a, kept), answer(a2, again), answer(b, moved)],
                Mapping::AddressAndPortDependent,
                Some(3),
            ),
        ];
        for (answers, mapping, number) in cases {
            let verdict = classify(Some(LOCAL), answers);
            assert_eq!(verdict.mapping, *mapping, "{answers:?}");
            assert_eq!(
                super::number(verdict.mapping, false),
                *number,
                "{answers:?}"
            );
        }
    }

    /// The first answer is the public address, and the port is preserved
    /// when it is the socket's own; the socket's own address is no
    /// translation at all, whatever else answered.
    #[test]
    fn the_first_answer_is_the_public_address() {
        let (a, b) = (at(1, 1, 3478), at(2, 2, 3478));
        let verdict = classify(
            Some(LOCAL),
            &[answer(a, at(9, 9, 25637)), answer(b, at(9, 9, 1))],
        );
        assert_eq!(verdict.public, Some(at(9, 9, 25637)));
        assert!(verdict.port_preserved);
        let moved = classify(Some(LOCAL), &[answer(a, at(9, 9, 40001))]);
        assert!(!moved.port_preserved);
        let direct = classify(Some(LOCAL), &[answer(a, LOCAL), answer(b, LOCAL)]);
        assert_eq!(direct.mapping, Mapping::None);
        assert_eq!(number(direct.mapping, false), Some(1));
        // Not knowing the socket's own address, nothing reads as direct.
        let unknown = classify(None, &[answer(a, LOCAL), answer(b, LOCAL)]);
        assert_eq!(unknown.mapping, Mapping::Independent);
        assert!(!unknown.port_preserved);
    }

    /// IPv4 alone is read: a v4-mapped answer is the IPv4 one it carries, and
    /// an IPv6 one is passed over, server or seen.
    #[test]
    fn only_ipv4_is_read() {
        let a = at(1, 1, 3478);
        let v6 = SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 3478);
        let mapped_form = |addr: SocketAddr| match addr {
            SocketAddr::V4(v4) => SocketAddr::new(IpAddr::V6(v4.ip().to_ipv6_mapped()), v4.port()),
            SocketAddr::V6(_) => addr,
        };
        let verdict = classify(
            Some(LOCAL),
            &[
                answer(v6, at(9, 9, 1)),
                answer(mapped_form(a), mapped_form(at(9, 9, 25637))),
                answer(at(2, 2, 3478), v6),
            ],
        );
        assert_eq!(verdict.mapping, Mapping::Unknown);
        assert_eq!(verdict.public, Some(at(9, 9, 25637)));
    }

    /// A confirmed mapping is reached whatever the translator does with the
    /// rest, so it numbers 2 -- but nothing translating stays 1, and nothing
    /// confirmed leaves the rest as it was.
    #[test]
    fn a_confirmed_mapping_numbers_two() {
        for mapping in [
            Mapping::Unknown,
            Mapping::Independent,
            Mapping::Dependent,
            Mapping::AddressDependent,
            Mapping::AddressAndPortDependent,
        ] {
            assert_eq!(number(mapping, true), Some(2), "{mapping:?}");
        }
        assert_eq!(number(Mapping::None, true), Some(1));
        assert_eq!(number(Mapping::Unknown, false), None);
    }

    /// The server's side of a request: answered from where it was asked.
    fn answered(request: &[u8], seen: SocketAddr) -> ([u8; 256], usize) {
        let message = Message::parse(request).unwrap();
        assert_eq!(message.method(), Method::BindingRequest);
        assert!(
            !message.is_authenticated(),
            "a probe carries no credentials"
        );
        let mut out = [0u8; 256];
        let len =
            stun::encode_binding_response(&mut out, message.transaction_id(), seen, "any").unwrap();
        (out, len)
    }

    /// Every request due at `now`, paced as the probe paces them.
    fn drain(probe: &mut Probe, now: f64) -> std::vec::Vec<(SocketAddr, [u8; 256], usize)> {
        let mut sent = std::vec::Vec::new();
        let mut at = now;
        while let Some(out) = {
            let mut buf = [0u8; 256];
            probe.get_output(at, &mut buf).map(|out| (out, buf))
        } {
            let (Ok((to, len)), buf) = out else {
                panic!("a request failed to encode");
            };
            sent.push((to, buf, len));
            at += PACING_MS;
        }
        sent
    }

    /// One request per server, paced; again at the cadence while it goes
    /// unanswered; never again once answered; and done when all have.
    #[test]
    fn each_server_is_asked_until_it_answers() {
        let (a, b) = (at(1, 1, 3478), at(2, 2, 3478));
        let mut probe = Probe::new([7; 16]);
        assert!(probe.done(), "a probe asking nobody is done");
        probe.add_server(a).unwrap();
        probe.add_server(b).unwrap();
        assert_eq!(probe.asked(), 2);
        assert!(!probe.done());

        let first = drain(&mut probe, 0.0);
        assert_eq!(
            first
                .iter()
                .map(|(to, ..)| *to)
                .collect::<std::vec::Vec<_>>(),
            [a, b]
        );
        assert!(first.iter().all(|(.., len)| *len == stun::HEADER_LEN));
        assert!(
            drain(&mut probe, 100.0).is_empty(),
            "asked again before the cadence"
        );

        let (to, request, len) = first[0];
        let (reply, reply_len) = answered(&request[..len], at(9, 9, 25637));
        probe.process_input(&reply[..reply_len], to).unwrap();
        assert_eq!(
            drain(&mut probe, CHECK_CADENCE_MS + 20.0)
                .iter()
                .map(|(to, ..)| *to)
                .collect::<std::vec::Vec<_>>(),
            [b],
            "an answered server was asked again, or the silent one was not"
        );
        let (to, request, len) = drain(&mut probe, 2.0 * CHECK_CADENCE_MS + 40.0)[0];
        assert_eq!(to, b);
        let (reply, reply_len) = answered(&request[..len], at(9, 9, 25637));
        probe.process_input(&reply[..reply_len], b).unwrap();
        assert!(probe.done());
        assert!(probe.next_timer_ms(5000.0).is_infinite());
        assert_eq!(
            probe.answers().collect::<std::vec::Vec<_>>(),
            [answer(a, at(9, 9, 25637)), answer(b, at(9, 9, 25637))]
        );
    }

    /// The timer says when the next request is due: at once for a server
    /// never asked, at the cadence for one asked, and never past either.
    #[test]
    fn the_timer_follows_the_cadence() {
        let due =
            |probe: &Probe, now: f64, wait: f64| (probe.next_timer_ms(now) - wait).abs() < 1e-9;
        let mut probe = Probe::new([7; 16]);
        probe.add_server(at(1, 1, 3478)).unwrap();
        assert!(due(&probe, 0.0, 0.0));
        let sent = drain(&mut probe, 0.0);
        assert_eq!(sent.len(), 1);
        assert!(due(&probe, 100.0, CHECK_CADENCE_MS - 100.0));
        assert!(due(&probe, 900.0, 0.0));
    }

    /// An answer is admitted only from where its request went, to a request
    /// this probe sent -- an earlier one included, for a round trip longer
    /// than the cadence -- and a stranger's changes nothing.
    #[test]
    fn only_an_answer_to_our_own_request_is_admitted() {
        let a = at(1, 1, 3478);
        let mut probe = Probe::new([7; 16]);
        probe.add_server(a).unwrap();
        let (_, early, early_len) = drain(&mut probe, 0.0)[0];
        let (_, late, late_len) = drain(&mut probe, CHECK_CADENCE_MS)[0];
        assert_ne!(early[8..20], late[8..20], "one transaction asked twice");

        let (reply, len) = answered(&early[..early_len], at(9, 9, 1));
        assert_eq!(
            probe.process_input(&reply[..len], at(1, 1, 3479)),
            Err(Error::Decrypt),
            "admitted from another port of the server"
        );
        let mut forged = [0u8; 256];
        let forged_len = stun::encode_binding_response(
            &mut forged,
            stun::TransactionId([0xEE; 12]),
            at(6, 6, 6),
            "any",
        )
        .unwrap();
        assert_eq!(
            probe.process_input(&forged[..forged_len], a),
            Err(Error::Decrypt),
            "admitted a transaction never sent"
        );
        assert_eq!(
            probe.process_input(&late[..late_len], a),
            Err(Error::Malformed)
        );
        assert_eq!(probe.answers().count(), 0);

        probe.process_input(&reply[..len], a).unwrap();
        assert_eq!(
            probe.answers().collect::<std::vec::Vec<_>>(),
            [answer(a, at(9, 9, 1))]
        );
    }

    /// One address is asked once, in either notation; past the limit, refused.
    #[test]
    fn a_server_is_asked_once_and_the_table_is_bounded() {
        let mut probe = Probe::new([7; 16]);
        let a = at(1, 1, 3478);
        let SocketAddr::V4(plain) = a else {
            unreachable!()
        };
        probe.add_server(a).unwrap();
        probe
            .add_server(SocketAddr::new(
                IpAddr::V6(plain.ip().to_ipv6_mapped()),
                3478,
            ))
            .unwrap();
        assert_eq!(probe.asked(), 1);
        for port in 1..MAX_SERVERS as u16 {
            probe.add_server(at(2, 2, port)).unwrap();
        }
        assert_eq!(probe.add_server(at(3, 3, 1)), Err(Error::Oversized));
        assert_eq!(probe.asked(), MAX_SERVERS);
    }
}
