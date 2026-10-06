//! The translation in front of the handle's port, as the application asks
//! for it: probed on a thread of its own, or read off an attempt's own
//! reflexive answers, into one result.
//!
//! **What was seen is kept; what the gateway says is read with it.** The
//! answers are a moment's, and are stored; the gateway's mapping comes and
//! goes, so its view -- the number among it, which a confirmed mapping
//! raises -- is taken from the mapper each time the result is read.

use std::net::{IpAddr, SocketAddr, SocketAddrV4};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use lowlat_core::nat::{self, Mapping, Verdict};
use lowlat_net::Socket;
use lowlat_portmap::{Protocol, Status};

/// Where the result stands.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum State {
    /// Nothing asked yet.
    #[default]
    None,
    /// A probe is running.
    Probing,
    /// A result is in.
    Done,
}

/// What the result was read from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Source {
    /// A probe the application asked for.
    #[default]
    Probe,
    /// An attempt's own reflexive answers.
    Attempt,
}

/// What was seen, as kept between the threads that see it and the reader.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Observed {
    pub state: State,
    pub source: Source,
    pub verdict: Option<Verdict>,
    pub asked: usize,
    pub answered: usize,
}

pub(crate) type Shared = Arc<Mutex<Observed>>;

pub(crate) fn publish(shared: &Shared, observed: Observed) {
    *shared.lock().unwrap_or_else(PoisonError::into_inner) = observed;
}

/// The result as the application reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Nat {
    pub state: State,
    pub source: Source,
    /// How the translation maps the port; none before a result.
    pub mapping: Option<Mapping>,
    /// The console numbering, 1 to 3; none when the answers cannot tell.
    pub number: Option<u8>,
    /// Where the first server to answer saw the port.
    pub public: Option<SocketAddr>,
    pub port_preserved: bool,
    /// Servers asked, and how many answered.
    pub asked: usize,
    pub answered: usize,
    /// What keeps the port mapped on the gateway, as it stands.
    pub gateway: Protocol,
    /// The gateway's own address is where the servers saw the port: nothing
    /// translates beyond it, and its mapping is open to anyone.
    pub confirmed: bool,
    /// The gateway's own address is not where the servers saw the port: a
    /// second translator beyond it, its mapping reaching only that far.
    pub double: bool,
    /// The gateway's own address is in a carrier's shared space.
    pub carrier: bool,
}

/// The result, with the gateway's view as `mapping` states it now.
pub(crate) fn read(observed: &Observed, mapping: Option<Status>) -> Nat {
    let wan = mapping.and_then(|status| status.address);
    let public = observed
        .verdict
        .and_then(|verdict| verdict.public)
        .and_then(|public| match lowlat_core::stun::canonical(public).ip() {
            IpAddr::V4(ip) => Some(ip),
            IpAddr::V6(_) => None,
        });
    let (confirmed, double) = match (wan, public) {
        (Some(wan), Some(public)) => (wan == public, wan != public),
        _ => (false, false),
    };
    Nat {
        state: observed.state,
        source: observed.source,
        mapping: observed.verdict.map(|verdict| verdict.mapping),
        number: observed
            .verdict
            .and_then(|verdict| nat::number(verdict.mapping, confirmed)),
        public: observed.verdict.and_then(|verdict| verdict.public),
        port_preserved: observed
            .verdict
            .is_some_and(|verdict| verdict.port_preserved),
        asked: observed.asked,
        answered: observed.answered,
        gateway: mapping.map_or(Protocol::None, |status| status.protocol),
        confirmed,
        double,
        carrier: wan.is_some_and(lowlat_net::addrs::is_shared),
    }
}

/// A probe on its own thread.
#[derive(Debug)]
pub(crate) struct Probing {
    thread: JoinHandle<()>,
    stop: Arc<AtomicBool>,
}

impl Probing {
    /// Ask `servers` from `socket` on a thread of its own, the result into
    /// `shared`. The socket goes back into `held` when it came from there,
    /// and is closed otherwise; either before the result is published, so a
    /// result that is in is a port that is free.
    pub(crate) fn start(
        socket: Socket,
        held: Option<Arc<Mutex<Option<Socket>>>>,
        servers: Vec<SocketAddrV4>,
        seed: [u8; 16],
        timeout: Duration,
        shared: &Shared,
    ) -> std::io::Result<Self> {
        let before = *shared.lock().unwrap_or_else(PoisonError::into_inner);
        publish(
            shared,
            Observed {
                state: State::Probing,
                ..before
            },
        );
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let result = Arc::clone(shared);
        let spawned = std::thread::Builder::new()
            .name("lowlat-nat".into())
            .spawn(move || {
                let probed = lowlat_net::nat::probe(&socket, &servers, seed, timeout, &stopping);
                // Acquire: a stop is an attempt taking the port, and a probe it
                // cut short says nothing; what was there before stands.
                let observed = match probed {
                    Ok(_) if stopping.load(Ordering::Acquire) => before,
                    Ok(probed) => {
                        lowlat_common::log_info!(
                            "client: the translation probed, mapping={:?} public={:?} answered={}/{}",
                            probed.verdict.mapping,
                            probed.verdict.public,
                            probed.answered,
                            probed.asked
                        );
                        Observed {
                            state: State::Done,
                            source: Source::Probe,
                            verdict: Some(probed.verdict),
                            asked: probed.asked,
                            answered: probed.answered,
                        }
                    }
                    Err(error) => {
                        lowlat_common::log_warn!("client: the translation probe failed, err={error}");
                        before
                    }
                };
                if let Some(held) = held {
                    let mut slot = held.lock().unwrap_or_else(PoisonError::into_inner);
                    if slot.is_none() {
                        *slot = Some(socket);
                    }
                }
                publish(&result, observed);
            });
        match spawned {
            Ok(thread) => Ok(Self { thread, stop }),
            Err(error) => {
                publish(shared, before);
                Err(error)
            }
        }
    }

    /// Stop it and wait for it: one of its waits at most.
    pub(crate) fn finish(self) {
        // Release: pairs with the thread's Acquire.
        self.stop.store(true, Ordering::Release);
        let _ = self.thread.join();
    }
}

/// What an attempt's reflexive answers say, once two server addresses have
/// answered: less says nothing a probe's result should give way to.
pub(crate) fn attempt(
    answers: &[nat::Answer],
    servers: &[SocketAddr],
    port: u16,
) -> Option<Observed> {
    let local = answers.iter().find_map(|answer| match answer.server {
        SocketAddr::V4(server) => {
            lowlat_net::addrs::local_toward(server).map(|ip| SocketAddr::new(IpAddr::V4(ip), port))
        }
        SocketAddr::V6(_) => None,
    });
    let verdict = nat::classify(local, answers);
    if matches!(verdict.mapping, Mapping::Unknown | Mapping::NoAnswer) {
        return None;
    }
    Some(Observed {
        state: State::Done,
        source: Source::Attempt,
        verdict: Some(verdict),
        asked: servers.iter().filter(|server| server.is_ipv4()).count(),
        answered: answers
            .iter()
            .filter(|answer| answer.server.is_ipv4())
            .count(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn verdict(mapping: Mapping, public: &str) -> Verdict {
        Verdict {
            mapping,
            public: Some(public.parse().unwrap()),
            port_preserved: false,
        }
    }

    fn mapped(protocol: Protocol, address: Ipv4Addr) -> Status {
        Status {
            protocol,
            address: Some(address),
            port: 25637,
            internal: 25637,
            refusal: None,
        }
    }

    fn done(verdict: Verdict) -> Observed {
        Observed {
            state: State::Done,
            source: Source::Probe,
            verdict: Some(verdict),
            asked: 2,
            answered: 2,
        }
    }

    /// The gateway's view against the answers: its own address where they
    /// saw the port confirms its mapping, which numbers a port per
    /// destination 2; another address is a second translator, and a
    /// carrier's space says whose.
    #[test]
    fn the_gateways_view_is_read_against_the_answers() {
        let symmetric = done(verdict(Mapping::Dependent, "203.0.113.7:40001"));
        let alone = read(&symmetric, None);
        assert_eq!(
            (alone.number, alone.confirmed, alone.double),
            (Some(3), false, false)
        );
        assert_eq!(alone.gateway, Protocol::None);

        let ours = read(
            &symmetric,
            Some(mapped(Protocol::Pcp, Ipv4Addr::new(203, 0, 113, 7))),
        );
        assert_eq!(
            (ours.number, ours.confirmed, ours.double),
            (Some(2), true, false)
        );
        assert_eq!(ours.gateway, Protocol::Pcp);
        assert_eq!(
            ours.mapping,
            Some(Mapping::Dependent),
            "the raw mapping is kept beside it"
        );

        let beyond = read(
            &symmetric,
            Some(mapped(Protocol::Upnp, Ipv4Addr::new(100, 64, 3, 9))),
        );
        assert_eq!(
            (beyond.number, beyond.confirmed, beyond.double),
            (Some(3), false, true)
        );
        assert!(beyond.carrier);

        let unmapped = Status {
            address: None,
            port: 0,
            internal: 0,
            ..mapped(Protocol::None, Ipv4Addr::UNSPECIFIED)
        };
        let refused = read(&symmetric, Some(unmapped));
        assert_eq!(
            (refused.confirmed, refused.double, refused.carrier),
            (false, false, false)
        );
    }

    /// Before anything is asked there is nothing to number.
    #[test]
    fn nothing_asked_is_nothing_known() {
        let nat = read(&Observed::default(), None);
        assert_eq!(nat.state, State::None);
        assert_eq!((nat.mapping, nat.number, nat.public), (None, None, None));
    }

    /// An attempt's answers count once two server addresses have answered,
    /// and IPv4 alone is counted.
    #[test]
    fn an_attempt_counts_once_two_addresses_answered() {
        let a: SocketAddr = "198.51.1.1:3478".parse().unwrap();
        let b: SocketAddr = "198.51.2.2:3478".parse().unwrap();
        let six: SocketAddr = "[2001:db8::1]:3478".parse().unwrap();
        let seen: SocketAddr = "203.0.113.7:25637".parse().unwrap();
        let one = [nat::Answer {
            server: a,
            mapped: seen,
        }];
        assert_eq!(attempt(&one, &[a, b, six], 25637), None);
        let two = [
            one[0],
            nat::Answer {
                server: b,
                mapped: seen,
            },
        ];
        let observed = attempt(&two, &[a, b, six], 25637).unwrap();
        assert_eq!(observed.source, Source::Attempt);
        assert_eq!(
            observed.verdict.map(|v| v.mapping),
            Some(Mapping::Independent)
        );
        assert_eq!((observed.asked, observed.answered), (2, 2));
    }
}
