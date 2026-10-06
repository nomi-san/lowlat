//! The mapper: a thread that keeps a handle's ports mapped on the gateway, and
//! deletes every mapping when it stops.
//!
//! **Cheapest first.** PCP, then NAT-PMP, then UPnP's gateway device. The
//! first two are a datagram each way to the gateway; UPnP is a search, a
//! description and an HTTP exchange per action, and is reached only when
//! neither of the others answers.
//!
//! **Never in the way.** A gateway that maps nothing is the common case: it is
//! logged once, looked at again later, and is never an error to the handle.
//! The mapper starts with its handle, so a mapping is usually in before the
//! first answer.
//!
//! **A renewal is checked, not assumed, and tried until the mapping lapses.**
//! A UPnP gateway may answer an identical add with success and keep the old
//! lease, so the lease is read back after a renewal and, when it did not
//! grow, the mapping is deleted and made again. A PCP or NAT-PMP gateway that
//! restarted shows it in its epoch. A renewal nothing answers is tried again
//! -- PCP's and NAT-PMP's at half the time left, never under a floor apart --
//! and the mapping, with the nonce that names it, is kept until it lapses: a
//! gateway that is only slow still holds it, and refuses a new nonce for it.
//!
//! **An attempt looks again.** An attempt about to begin asks the mapper to
//! look: with nothing mapped the ladder is climbed then rather than at the
//! next retry; with a mapping, the gateway and this side's address are checked
//! and the mapping renewed then, which also makes again what a gateway that
//! restarted lost. A look cuts nothing short.
//!
//! **A stop is bounded, and nothing asked is forgotten.** Every wait is cut
//! short by a stop or a move, the connect included. What is mapped, and a
//! request that went out and whose answer was cut short, is deleted within a
//! fixed bound counted from the stop: every PCP or NAT-PMP delete in one
//! exchange, UPnP's a port at a time, each entry read first so that one that
//! lapsed and was taken since is left to its device.

use core::cell::Cell;
use core::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use core::time::Duration;
use std::io::{self, Read, Write};
use std::net::UdpSocket;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Instant;

use lowlat_common::{log_info, log_warn, wait};

use crate::desc::{self, Service};
use crate::http::{self, Method};
use crate::natpmp;
use crate::pcp::{self, Nonce};
use crate::soap::{self, Answer, FaultCode};
use crate::ssdp;
use crate::sys;
use crate::url::Url;

/// Below it are the system's own services, never mapped.
const MIN_PORT: u16 = 1024;
/// A first look for PCP or NAT-PMP: an answer on the local network takes a
/// millisecond, so silence after these is absence.
const PROBE: [Duration; 2] = [Duration::from_millis(250), Duration::from_millis(500)];
/// A renewal waits longer: a mapping made is worth some patience.
const RENEW: [Duration; 4] = [
    Duration::from_millis(250),
    Duration::from_millis(500),
    Duration::from_secs(1),
    Duration::from_secs(2),
];
/// A search's wait: devices may delay an answer by up to the seconds asked.
const SEARCH: Duration = Duration::from_millis(2500);
const SEARCH_DELAY_S: u8 = 2;
/// An HTTP exchange's connect, and the whole.
const CONNECT: Duration = Duration::from_millis(500);
const EXCHANGE: Duration = Duration::from_secs(2);
/// How long a blocking wait lasts before looking for a stop.
const SLICE: Duration = Duration::from_millis(50);
/// A lifetime beyond this is read as this.
const LONGEST_LIFETIME_S: u32 = 24 * 60 * 60;

/// What the mapper keeps open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// The first port mapped, the same outside as inside.
    pub port: u16,
    /// How many ports from the first: one for a client, the guests' for a
    /// host.
    pub count: u16,
    /// How the gateway lists each mapping, and how this side tells its own.
    pub description: String,
    /// The gateway to ask instead of the system's default: none but in a
    /// test, whose fake gateway makes one.
    pub gateway: Option<Gateway>,
}

/// The protocol a mapping was made by.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Protocol {
    #[default]
    None,
    Pcp,
    NatPmp,
    Upnp,
}

impl Protocol {
    /// Stable short name, for logs.
    pub const fn as_str(self) -> &'static str {
        match self {
            Protocol::None => "none",
            Protocol::Pcp => "pcp",
            Protocol::NatPmp => "natpmp",
            Protocol::Upnp => "upnp",
        }
    }

    /// Where the protocol's last refusal is kept; none for no protocol.
    const fn slot(self) -> Option<usize> {
        match self {
            Protocol::None => None,
            Protocol::Pcp => Some(0),
            Protocol::NatPmp => Some(1),
            Protocol::Upnp => Some(2),
        }
    }
}

/// What is mapped, as the gateway states it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Status {
    pub protocol: Protocol,
    /// The gateway's external address, when it states one that can be.
    pub address: Option<Ipv4Addr>,
    /// The first port's external port; zero when nothing is mapped.
    pub port: u16,
    /// The first port as this side asked for it; zero when nothing is mapped.
    pub internal: u16,
    /// The last refusal: the protocol that refused, and its own code.
    pub refusal: Option<(Protocol, u16)>,
}

/// The intervals the mapper keeps; the tests shorten them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Timing {
    /// A UPnP lease, and how often the mapping is added again within it.
    pub(crate) lease_s: u32,
    pub(crate) readd: Duration,
    /// A PCP or NAT-PMP lifetime asked for; renewed at half what is granted.
    pub(crate) lifetime_s: u32,
    /// How long after finding nothing to look again.
    pub(crate) retry: Duration,
    /// The bound on deleting every mapping at a stop, from the stop.
    pub(crate) teardown: Duration,
    /// A renewal's waits, its request sent again after each.
    pub(crate) renew_waits: &'static [Duration],
    /// The least time between two renewals of a mapping.
    pub(crate) renew_floor: Duration,
    /// How soon a UPnP renewal nothing answered is tried again.
    pub(crate) upnp_retry: Duration,
    /// The least time between two looks an attempt asks for.
    pub(crate) look_floor: Duration,
}

const TIMING: Timing = Timing {
    lease_s: 2700,
    readd: Duration::from_secs(300),
    lifetime_s: 7200,
    retry: Duration::from_secs(300),
    teardown: Duration::from_millis(250),
    renew_waits: &RENEW,
    // A protocol's own floor between renewals.
    renew_floor: Duration::from_secs(4),
    upnp_retry: Duration::from_secs(30),
    look_floor: Duration::from_secs(10),
};

/// Intervals short enough to see renewals within a test.
#[cfg(any(test, feature = "fake"))]
pub(crate) const FAST: Timing = Timing {
    lease_s: 6,
    readd: Duration::from_secs(2),
    lifetime_s: 4,
    retry: Duration::from_millis(500),
    teardown: Duration::from_millis(250),
    renew_waits: &[Duration::from_millis(50), Duration::from_millis(100)],
    renew_floor: Duration::from_millis(200),
    upnp_retry: Duration::from_millis(300),
    look_floor: Duration::from_millis(100),
};

/// Where the gateway listens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Endpoints {
    pub(crate) gateway: Ipv4Addr,
    /// PCP and NAT-PMP.
    pub(crate) control: SocketAddrV4,
    /// A search to the gateway itself, then one to the group.
    pub(crate) search: SocketAddrV4,
    pub(crate) group: SocketAddrV4,
}

impl Endpoints {
    fn of(gateway: Ipv4Addr) -> Self {
        Self {
            gateway,
            control: SocketAddrV4::new(gateway, natpmp::PORT),
            search: SocketAddrV4::new(gateway, ssdp::PORT),
            group: ssdp::GROUP,
        }
    }
}

/// A gateway other than the system's default, and the intervals to keep with
/// it: what a test runs a mapper against. Nothing but the fake gateway makes
/// one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gateway {
    pub(crate) endpoints: Endpoints,
    pub(crate) timing: Timing,
}

/// A thread keeping a handle's ports mapped.
#[derive(Debug)]
pub struct Mapper {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

#[derive(Debug)]
struct Shared {
    /// Changed by every request the thread must see; it sleeps on it.
    generation: AtomicU32,
    /// Changed by a stop or a move alone: what cuts a wait short. A look an
    /// attempt asks for wakes the thread and cuts nothing.
    cuts: AtomicU32,
    stopping: AtomicBool,
    /// When the stop was asked: the delete's bound is counted from it.
    stopped_at: Mutex<Option<Instant>>,
    /// A look asked for and not yet taken.
    look: AtomicBool,
    port: AtomicU32,
    status: Mutex<Status>,
    /// The external address, the first port's external and internal ports,
    /// while the gateway states the address, as one word a reader takes
    /// without the lock; zero otherwise.
    external: AtomicU64,
}

impl Shared {
    fn poke(&self) {
        // Release: the request written before the change is seen with it.
        self.generation.fetch_add(1, Ordering::Release);
        wait::notify_all(&self.generation);
    }

    /// A request that cuts short whatever the thread is waiting on.
    fn cut(&self) {
        // Release: as the generation's, for the waits that read this word.
        self.cuts.fetch_add(1, Ordering::Release);
        self.poke();
    }

    fn publish(&self, change: impl FnOnce(&mut Status)) {
        let mut status = self.status.lock().unwrap_or_else(PoisonError::into_inner);
        change(&mut status);
        // Relaxed: the word is the whole message, and nothing is read with it.
        self.external.store(packed(&status), Ordering::Relaxed);
    }

    /// Nothing mapped any more; the last refusal is kept.
    fn clear(&self) {
        self.publish(|status| {
            *status = Status {
                refusal: status.refusal,
                ..Status::default()
            };
        });
    }
}

/// The external address above the external and internal ports. A port is set
/// only while something is mapped, so no mapping packs to zero.
fn packed(status: &Status) -> u64 {
    match status.address {
        Some(address) if status.port != 0 => {
            u64::from(address.to_bits()) << 32
                | u64::from(status.port) << 16
                | u64::from(status.internal)
        }
        _ => 0,
    }
}

/// An address a gateway states that cannot be its outside.
fn bogus(address: Ipv4Addr) -> bool {
    address.is_unspecified() || address.is_loopback()
}

/// The gateway's external address and port as a candidate, once a reflexive
/// server has reported the same address: none before, and none when it
/// reported another, which is a translator beyond the gateway. An address in
/// its v4-mapped form is the IPv4 address it carries.
pub fn confirmed(external: SocketAddrV4, reflexive: &[SocketAddr]) -> Option<SocketAddr> {
    let external = SocketAddr::V4(external);
    reflexive
        .iter()
        .any(|addr| addr.ip().to_canonical() == external.ip())
        .then_some(external)
}

/// What another thread reads of a mapper without taking its lock. It keeps
/// nothing running: once the mapper stops it reads nothing.
#[derive(Debug, Clone)]
pub struct Reader {
    shared: Arc<Shared>,
}

impl Reader {
    /// The gateway's external address and the first port's external port,
    /// while the gateway states both, for the port the mapper is asked to
    /// keep: nothing for a port moved from, whose mapping is about to go.
    pub fn external(&self) -> Option<SocketAddrV4> {
        let word = self.shared.external.load(Ordering::Relaxed);
        let address = u32::try_from(word >> 32).ok()?;
        let port = u16::try_from((word >> 16) & 0xffff).ok()?;
        let internal = u16::try_from(word & 0xffff).ok()?;
        // Relaxed: a port moved a moment ago is read on the next look.
        let asked = self.shared.port.load(Ordering::Relaxed);
        (word != 0 && u32::from(internal) == asked)
            .then(|| SocketAddrV4::new(Ipv4Addr::from_bits(address), port))
    }
}

impl Mapper {
    /// Start keeping `config`'s ports mapped, on a thread of its own.
    pub fn start(config: Config) -> io::Result<Self> {
        let (endpoints, timing) = match config.gateway {
            Some(gateway) => (Some(gateway.endpoints), gateway.timing),
            None => (None, TIMING),
        };
        Self::spawn(config, endpoints, timing, None)
    }

    /// The ladder held to one protocol: how each is checked against a gateway
    /// that answers all three.
    #[cfg(test)]
    pub(crate) fn start_only(
        config: Config,
        endpoints: Endpoints,
        timing: Timing,
        only: Protocol,
    ) -> io::Result<Self> {
        Self::spawn(config, Some(endpoints), timing, Some(only))
    }

    fn spawn(
        config: Config,
        endpoints: Option<Endpoints>,
        timing: Timing,
        only: Option<Protocol>,
    ) -> io::Result<Self> {
        let shared = Arc::new(Shared {
            generation: AtomicU32::new(0),
            cuts: AtomicU32::new(0),
            stopping: AtomicBool::new(false),
            stopped_at: Mutex::new(None),
            look: AtomicBool::new(false),
            port: AtomicU32::new(u32::from(config.port)),
            status: Mutex::new(Status::default()),
            external: AtomicU64::new(0),
        });
        let runner = Runner {
            shared: Arc::clone(&shared),
            count: config.count,
            description: config.description,
            endpoints,
            timing,
            quiet: Cell::new(false),
            refusals: [const { Cell::new(None) }; 3],
            only,
        };
        let thread = std::thread::Builder::new()
            .name("lowlat-portmap".into())
            .spawn(move || runner.run())?;
        Ok(Self {
            shared,
            thread: Some(thread),
        })
    }

    /// Map from another first port: what was mapped is deleted, and the new
    /// ports are mapped at once.
    pub fn set_port(&self, port: u16) {
        if self.shared.port.swap(u32::from(port), Ordering::AcqRel) != u32::from(port) {
            self.shared.cut();
        }
    }

    /// Look at the gateway again, for an attempt about to begin: with nothing
    /// mapped the ladder is climbed now, and a mapping is checked and renewed
    /// now. Cuts nothing short, and is taken at most once in a floor of time.
    pub fn refresh(&self) {
        self.shared.look.store(true, Ordering::Release);
        self.shared.poke();
    }

    pub fn status(&self) -> Status {
        *self
            .shared
            .status
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// A reader for another thread, which takes no lock.
    pub fn reader(&self) -> Reader {
        Reader {
            shared: Arc::clone(&self.shared),
        }
    }

    /// Delete every mapping and end the thread, within the stop's bound.
    pub fn stop(&mut self) {
        let Some(thread) = self.thread.take() else {
            return;
        };
        *self
            .shared
            .stopped_at
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(Instant::now());
        self.shared.stopping.store(true, Ordering::Release);
        self.shared.cut();
        // A runner that panicked has nothing left to delete.
        let _ = thread.join();
    }
}

impl Drop for Mapper {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The ports from `first`, `count` of them. A range never passes the last
/// port, so no step overflows, and the last port is a port like any other.
fn ports_from(first: u16, count: u16) -> impl Iterator<Item = u16> {
    (0..count).map_while(move |step| first.checked_add(step))
}

/// A mapping made, and how to keep it.
#[derive(Debug)]
struct Mapped {
    protocol: Protocol,
    endpoints: Endpoints,
    /// This side's address toward the gateway.
    local: Ipv4Addr,
    first: u16,
    /// How many ports from the first are mapped: the rest stopped at a
    /// refusal.
    count: u16,
    /// The port after the last mapped was asked for and its answer cut short:
    /// it may be mapped, so it goes with the rest.
    pending: bool,
    renew_at: Instant,
    /// When the lease or lifetime runs out; never, for a permanent mapping.
    expires: Option<Instant>,
    /// When it was made or last renewed.
    renewed: Instant,
    /// Renewals in a row nothing answered, and when the first of them was.
    misses: u32,
    unanswered: Option<Instant>,
    how: How,
}

#[derive(Debug)]
enum How {
    /// A nonce and the external port granted, per port: a renewal and a
    /// delete must carry the nonce, a renewal suggests the port. One more
    /// than the count while one is pending.
    Pcp {
        ports: Vec<(Nonce, u16)>,
        epoch: Epoch,
    },
    /// The external port granted, per port, which a renewal suggests.
    NatPmp { externals: Vec<u16>, epoch: Epoch },
    Upnp {
        service: Service,
        permanent: bool,
        /// Whether a renewal has had to delete and add, logged once.
        remade: bool,
    },
}

/// A gateway's epoch, and when this side read it.
#[derive(Debug, Clone, Copy)]
struct Epoch {
    server_s: u32,
    at: Instant,
}

enum Renewal {
    Renewed,
    /// Nothing answered: kept, and tried again until it lapses.
    Unanswered,
    Lost(&'static str),
    Interrupted,
}

/// What a climb of the ladder came to.
enum Looked {
    /// Mapped, or asked for and cut short before its answer: either way kept,
    /// so that it is deleted with everything else.
    Kept(Mapped),
    /// Nothing maps here.
    Nothing,
    /// Cut short by a stop or a move before anything was asked.
    Cut,
}

/// One protocol's turn on the ladder.
enum Step {
    Kept(Mapped),
    /// Not this protocol: the next is asked.
    Next,
    Cut,
}

/// Why a UPnP add has nothing to show.
enum Refusal {
    /// The gateway's own code.
    Code(u16),
    /// No answer.
    Silent,
    /// Cut short; `sent` once the add itself went out.
    Cut { sent: bool },
}

/// What came of a request to the gateway's control port.
enum Exchanged<T> {
    Answer(T),
    /// No answer in the waits, or the datagram refused: nothing listening.
    Silent,
    /// Cut short by a stop or a move; `sent` once a request had gone out, so
    /// that the gateway may have acted on it.
    Cut {
        sent: bool,
    },
}

/// Why an HTTP exchange has no answer to act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unanswered {
    /// Nothing came back in time, the connection failed, or what came back
    /// was not HTTP.
    Silent,
    /// An HTTP answer that is not the protocol's: a 401, a 404, a plain 500.
    Other,
    /// Cut short by a stop or a move; `sent` once the request had gone out,
    /// so that the gateway may have acted on it.
    Cut { sent: bool },
}

/// What cuts a wait short: a stop or a move, or a deadline.
struct Watch<'a> {
    shared: &'a Shared,
    seen: Option<u32>,
    deadline: Option<Instant>,
}

impl Watch<'_> {
    fn interrupted(&self) -> bool {
        self.seen
            .is_some_and(|seen| self.shared.cuts.load(Ordering::Acquire) != seen)
            || self
                .deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
    }

    /// The next slice of waiting before `until`; none once interrupted or
    /// past it.
    fn slice(&self, until: Instant) -> Option<Duration> {
        if self.interrupted() {
            return None;
        }
        let end = self.deadline.map_or(until, |deadline| deadline.min(until));
        let left = end.saturating_duration_since(Instant::now());
        (!left.is_zero()).then(|| left.min(SLICE))
    }
}

struct Runner {
    shared: Arc<Shared>,
    count: u16,
    description: String,
    endpoints: Option<Endpoints>,
    timing: Timing,
    /// Set once nothing was found, so finding nothing again is not logged.
    quiet: Cell<bool>,
    /// Each protocol's last refusal, so one is logged only when it changes.
    refusals: [Cell<Option<u16>>; 3],
    /// The one protocol the ladder asks; every one when none. Only a test
    /// holds it to one.
    only: Option<Protocol>,
}

impl Runner {
    fn run(self) {
        let mut mapped: Option<Mapped> = None;
        // When the ladder last found something or nothing, uncut, and for
        // which port: a port moved to is looked at at once.
        let mut looked: Option<(Instant, u16)> = None;
        loop {
            let seen = self.shared.generation.load(Ordering::Acquire);
            let cuts = self.shared.cuts.load(Ordering::Acquire);
            if self.shared.stopping.load(Ordering::Acquire) {
                break;
            }
            let first = u16::try_from(self.shared.port.load(Ordering::Acquire)).unwrap_or(0);
            // A port moved from, or a request whose answer was cut short, is
            // deleted before anything is mapped again.
            if let Some(stale) = mapped.take_if(|current| current.first != first || current.pending)
            {
                self.delete(&stale, Instant::now() + self.timing.teardown);
            }
            let look = self.shared.look.swap(false, Ordering::AcqRel);
            let now = Instant::now();
            let next = match mapped.take() {
                None => {
                    let last = looked.filter(|&(_, port)| port == first).map(|(at, _)| at);
                    let due = last.map_or(now, |at| at + self.timing.retry);
                    let asked = look && last.is_none_or(|at| now >= at + self.timing.look_floor);
                    if now < due && !asked {
                        due
                    } else {
                        match self.map(first, cuts) {
                            Looked::Kept(made) => {
                                looked = Some((now, first));
                                let at = made.renew_at;
                                mapped = Some(made);
                                at
                            }
                            Looked::Nothing => {
                                looked = Some((now, first));
                                Instant::now() + self.timing.retry
                            }
                            // The next pass sees why.
                            Looked::Cut => now,
                        }
                    }
                }
                Some(current) => {
                    let (kept, at) = self.keep(current, look, cuts);
                    if kept.is_none() {
                        looked = None;
                    }
                    mapped = kept;
                    at
                }
            };
            let left = next.saturating_duration_since(Instant::now());
            if !left.is_zero() {
                wait::wait(&self.shared.generation, seen, left);
            }
        }
        if let Some(current) = mapped {
            let stopped = *self
                .shared
                .stopped_at
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let from = stopped.unwrap_or_else(Instant::now);
            self.delete(&current, from + self.timing.teardown);
        }
    }

    /// Keep a mapping: checked when an attempt asks, renewed when due, tried
    /// again when nothing answers, forgotten when lost. What is kept, and
    /// when to look at it next.
    fn keep(&self, mut current: Mapped, look: bool, cuts: u32) -> (Option<Mapped>, Instant) {
        let now = Instant::now();
        if look && now >= current.renewed + self.timing.look_floor {
            if self.moved_away(&current) {
                log_info!(
                    "portmap: the network moved, protocol={} port={}, looking again",
                    current.protocol.as_str(),
                    current.first
                );
                self.shared.clear();
                return (None, now);
            }
            current.renew_at = now;
        }
        if now < current.renew_at {
            let at = current.renew_at;
            return (Some(current), at);
        }
        match self.renew(&mut current, cuts) {
            Renewal::Renewed => {
                if current.misses > 0 {
                    log_info!(
                        "portmap: renewed after silence, protocol={} port={} misses={}",
                        current.protocol.as_str(),
                        current.first,
                        current.misses
                    );
                    current.misses = 0;
                    current.unanswered = None;
                }
                let at = current.renew_at;
                (Some(current), at)
            }
            Renewal::Interrupted => {
                let at = current.renew_at;
                (Some(current), at)
            }
            Renewal::Unanswered => {
                let now = Instant::now();
                let since = *current.unanswered.get_or_insert(now);
                let lease = Duration::from_secs(u64::from(self.timing.lease_s));
                let end = current.expires.unwrap_or(since + lease);
                if now >= end {
                    self.lost(&current, "expired");
                    return (None, now);
                }
                if current.misses == 0 {
                    log_warn!(
                        "portmap: renewal unanswered, protocol={} port={}, kept until it lapses",
                        current.protocol.as_str(),
                        current.first
                    );
                }
                current.misses = current.misses.saturating_add(1);
                let retry = match current.protocol {
                    Protocol::Upnp => self.timing.upnp_retry,
                    _ => (end - now) / 2,
                };
                current.renew_at = (now + retry.max(self.timing.renew_floor)).min(end);
                let at = current.renew_at;
                (Some(current), at)
            }
            Renewal::Lost(why) => {
                self.lost(&current, why);
                (None, now)
            }
        }
    }

    /// The gateway is another, or this side's address toward it is: the
    /// mapping is on a network this side has left. Not knowing either is not
    /// a move.
    fn moved_away(&self, current: &Mapped) -> bool {
        let gateway = self.endpoints.is_none()
            && sys::gateway().is_some_and(|gateway| gateway != current.endpoints.gateway);
        let local =
            local_toward(current.endpoints.control).is_some_and(|local| local != current.local);
        gateway || local
    }

    fn lost(&self, current: &Mapped, why: &str) {
        log_warn!(
            "portmap: lost, protocol={} port={} reason={}",
            current.protocol.as_str(),
            current.first,
            why
        );
        self.shared.clear();
    }

    fn watch(&self, cuts: u32) -> Watch<'_> {
        Watch {
            shared: &self.shared,
            seen: Some(cuts),
            deadline: None,
        }
    }

    /// A refusal, logged when it is not that protocol's last one again: a
    /// gateway that refuses a port refuses it at every look.
    fn refused(&self, protocol: Protocol, code: u16, port: u16) {
        self.shared
            .publish(|status| status.refusal = Some((protocol, code)));
        let again = protocol
            .slot()
            .and_then(|slot| self.refusals.get(slot))
            .is_some_and(|last| last.replace(Some(code)) == Some(code));
        if !again {
            log_info!(
                "portmap: refused, protocol={} port={} code={}",
                protocol.as_str(),
                port,
                code
            );
        }
    }

    fn nothing(&self, gateway: Option<Ipv4Addr>) {
        if !self.quiet.replace(true) {
            match gateway {
                Some(gateway) => log_info!("portmap: nothing mapped, gateway={gateway}"),
                None => log_info!("portmap: nothing mapped, gateway=none"),
            }
        }
    }

    /// The ladder: the first protocol the gateway answers maps the ports.
    fn map(&self, first: u16, cuts: u32) -> Looked {
        if first < MIN_PORT || self.count == 0 {
            if !self.quiet.replace(true) {
                log_info!("portmap: nothing mapped, port={first} reason=reserved");
            }
            return Looked::Nothing;
        }
        // The range ends at the last port there is.
        let count = self.count.min(u16::MAX - first + 1);
        let Some(endpoints) = self.endpoints.or_else(|| sys::gateway().map(Endpoints::of)) else {
            self.nothing(None);
            return Looked::Nothing;
        };
        let Some(local) = local_toward(endpoints.control) else {
            self.nothing(Some(endpoints.gateway));
            return Looked::Nothing;
        };
        for protocol in [Protocol::Pcp, Protocol::NatPmp, Protocol::Upnp] {
            if self.only.is_some_and(|only| only != protocol) {
                continue;
            }
            let step = match protocol {
                Protocol::Pcp => self.pcp(endpoints, local, first, count, cuts),
                Protocol::NatPmp => self.natpmp(endpoints, local, first, count, cuts),
                Protocol::Upnp => self.upnp(endpoints, local, first, count, cuts),
                Protocol::None => Step::Next,
            };
            match step {
                Step::Kept(made) => {
                    if made.count > 0 {
                        self.quiet.set(false);
                        let status = self.status();
                        log_info!(
                            "portmap: mapped, protocol={} port={} count={} external={}:{} lease_s={}",
                            made.protocol.as_str(),
                            made.first,
                            made.count,
                            status.address.unwrap_or(Ipv4Addr::UNSPECIFIED),
                            status.port,
                            made.expires.map_or(0, |at| at
                                .saturating_duration_since(Instant::now())
                                .as_secs()
                                .saturating_add(1))
                        );
                    }
                    return Looked::Kept(made);
                }
                Step::Next => {}
                Step::Cut => return Looked::Cut,
            }
        }
        self.nothing(Some(endpoints.gateway));
        Looked::Nothing
    }

    fn status(&self) -> Status {
        *self
            .shared
            .status
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// A mapping kept: renewed after half the lifetime granted.
    #[allow(clippy::too_many_arguments)]
    fn made(
        &self,
        protocol: Protocol,
        endpoints: Endpoints,
        local: Ipv4Addr,
        first: u16,
        count: u16,
        pending: bool,
        granted_s: u32,
        how: How,
    ) -> Mapped {
        let now = Instant::now();
        let (renew_at, expires) = match &how {
            How::Upnp { permanent, .. } => (
                now + self.timing.readd,
                (!*permanent).then(|| now + Duration::from_secs(u64::from(self.timing.lease_s))),
            ),
            _ => (
                now + renew_after(granted_s, self.timing.renew_floor),
                Some(now + Duration::from_secs(u64::from(granted_s))),
            ),
        };
        Mapped {
            protocol,
            endpoints,
            local,
            first,
            count,
            pending,
            renew_at,
            expires,
            renewed: now,
            misses: 0,
            unanswered: None,
            how,
        }
    }

    fn pcp(
        &self,
        endpoints: Endpoints,
        local: Ipv4Addr,
        first: u16,
        count: u16,
        cuts: u32,
    ) -> Step {
        let watch = self.watch(cuts);
        let mut ports: Vec<(Nonce, u16)> = Vec::new();
        let mut made: u16 = 0;
        let mut pending = false;
        let mut granted = LONGEST_LIFETIME_S;
        let mut epoch = None;
        for port in ports_from(first, count) {
            let mut nonce = Nonce([0; 12]);
            if lowlat_crypto::fill(&mut nonce.0).is_err() {
                break;
            }
            let request =
                pcp::map_request(IpAddr::V4(local), nonce, port, port, self.timing.lifetime_s);
            let answer = exchange(&watch, endpoints.control, &request, &PROBE, |data| {
                pcp_answer(data, nonce)
            });
            let reply = match answer {
                Exchanged::Answer(Ok(reply)) if reply.result == pcp::ResultCode::SUCCESS => reply,
                Exchanged::Answer(Ok(reply)) => {
                    self.refused(Protocol::Pcp, u16::from(reply.result.0), port);
                    break;
                }
                // Another version, or nothing that answers: not PCP.
                Exchanged::Answer(Err(_)) | Exchanged::Silent => break,
                Exchanged::Cut { sent } => {
                    if sent {
                        ports.push((nonce, port));
                        pending = true;
                    }
                    break;
                }
            };
            let Some(map) = reply.map else {
                break;
            };
            if made == 0 {
                let address = match map.external_address {
                    IpAddr::V4(address) if !bogus(address) => Some(address),
                    _ => None,
                };
                self.shared.publish(|status| {
                    status.protocol = Protocol::Pcp;
                    status.address = address;
                    status.port = map.external_port;
                    status.internal = first;
                });
            }
            granted = granted.min(reply.lifetime_s);
            epoch = Some(Epoch {
                server_s: reply.epoch_s,
                at: Instant::now(),
            });
            ports.push((nonce, map.external_port));
            made += 1;
        }
        if made == 0 && !pending {
            return if watch.interrupted() {
                Step::Cut
            } else {
                Step::Next
            };
        }
        let epoch = epoch.unwrap_or(Epoch {
            server_s: 0,
            at: Instant::now(),
        });
        let how = How::Pcp { ports, epoch };
        Step::Kept(self.made(
            Protocol::Pcp,
            endpoints,
            local,
            first,
            made,
            pending,
            granted,
            how,
        ))
    }

    fn natpmp(
        &self,
        endpoints: Endpoints,
        local: Ipv4Addr,
        first: u16,
        count: u16,
        cuts: u32,
    ) -> Step {
        let watch = self.watch(cuts);
        // The external address first: a mapping's answer does not carry it.
        let request = natpmp::address_request();
        let (epoch_s, address) = match exchange(&watch, endpoints.control, &request, &PROBE, |d| {
            natpmp_answer(d, 0, None)
        }) {
            Exchanged::Answer(natpmp::Reply::Address { epoch_s, address }) => (epoch_s, address),
            Exchanged::Answer(natpmp::Reply::Refused { result, .. }) => {
                self.refused(Protocol::NatPmp, result.0, first);
                return Step::Next;
            }
            Exchanged::Answer(natpmp::Reply::Map { .. }) | Exchanged::Silent => {
                return Step::Next;
            }
            // Asking the address changes nothing on the gateway.
            Exchanged::Cut { .. } => return Step::Cut,
        };
        // A gateway with no external address maps nothing that works.
        if bogus(address) {
            return Step::Next;
        }
        let mut externals: Vec<u16> = Vec::new();
        let mut made: u16 = 0;
        let mut pending = false;
        let mut granted = LONGEST_LIFETIME_S;
        for port in ports_from(first, count) {
            let request = natpmp::map_request(port, port, self.timing.lifetime_s);
            match exchange(&watch, endpoints.control, &request, &PROBE, |d| {
                natpmp_answer(d, 1, Some(port))
            }) {
                Exchanged::Answer(natpmp::Reply::Map {
                    external_port,
                    lifetime_s,
                    ..
                }) => {
                    externals.push(external_port);
                    granted = granted.min(lifetime_s);
                    made += 1;
                }
                Exchanged::Answer(natpmp::Reply::Refused { result, .. }) => {
                    self.refused(Protocol::NatPmp, result.0, port);
                    break;
                }
                Exchanged::Answer(_) | Exchanged::Silent => break,
                Exchanged::Cut { sent } => {
                    if sent {
                        externals.push(port);
                        pending = true;
                    }
                    break;
                }
            }
        }
        if made == 0 && !pending {
            return if watch.interrupted() {
                Step::Cut
            } else {
                Step::Next
            };
        }
        if let Some(&external) = externals.first().filter(|_| made > 0) {
            self.shared.publish(|status| {
                status.protocol = Protocol::NatPmp;
                status.address = Some(address);
                status.port = external;
                status.internal = first;
            });
        }
        let how = How::NatPmp {
            externals,
            epoch: Epoch {
                server_s: epoch_s,
                at: Instant::now(),
            },
        };
        Step::Kept(self.made(
            Protocol::NatPmp,
            endpoints,
            local,
            first,
            made,
            pending,
            granted,
            how,
        ))
    }

    fn upnp(
        &self,
        endpoints: Endpoints,
        local: Ipv4Addr,
        first: u16,
        count: u16,
        cuts: u32,
    ) -> Step {
        let watch = self.watch(cuts);
        let stopped = |watch: &Watch<'_>| {
            if watch.interrupted() {
                Step::Cut
            } else {
                Step::Next
            }
        };
        let Some(location) = search(&watch, endpoints, local) else {
            return stopped(&watch);
        };
        let response = match http_exchange(&watch, &location, http::DESCRIPTION_CAP, |_| {
            let request = http::request(Method::Get, location.addr, &location.path, &[], &[]);
            (request, ())
        }) {
            Ok((response, ())) => response,
            Err(_) => return stopped(&watch),
        };
        if response.status != 200 {
            return Step::Next;
        }
        let Ok(found) = desc::parse(&response.body, &location) else {
            return Step::Next;
        };
        for service in found.connections() {
            // Only the gateway's own address is asked anything.
            if *service.control.addr.ip() != endpoints.gateway {
                continue;
            }
            let kind = service.service_type;
            // A connection that is down maps nothing that works; a gateway
            // that cannot say is asked on.
            match self.control(&watch, service, |_| soap::get_status_info(kind)) {
                Ok(answer @ Answer::Done(_)) => {
                    let down = answer.get("NewConnectionStatus").is_some_and(|state| {
                        !state.eq_ignore_ascii_case("Connected")
                            && !state.eq_ignore_ascii_case("Up")
                    });
                    if down {
                        continue;
                    }
                }
                Ok(Answer::Fault(_)) | Err(Unanswered::Other) => {}
                Err(Unanswered::Silent | Unanswered::Cut { .. }) => return stopped(&watch),
            }
            // The external address: stated, not stated, or one that cannot
            // be. Not stating one is a gateway behind a reserved address,
            // which maps all the same.
            let address =
                match self.control(&watch, service, |_| soap::get_external_ip_address(kind)) {
                    Ok(answer @ Answer::Done(_)) => {
                        let stated = answer
                            .get("NewExternalIPAddress")
                            .and_then(|address| address.parse::<Ipv4Addr>().ok());
                        if stated.is_some_and(bogus) {
                            continue;
                        }
                        stated
                    }
                    Ok(Answer::Fault(_)) | Err(Unanswered::Other) => continue,
                    Err(Unanswered::Silent | Unanswered::Cut { .. }) => return stopped(&watch),
                };
            let mut permanent = false;
            let mut made: u16 = 0;
            let mut pending = false;
            for port in ports_from(first, count) {
                match self.upnp_add(&watch, service, port, &mut permanent) {
                    Ok(()) => made += 1,
                    Err(Refusal::Code(code)) => {
                        self.refused(Protocol::Upnp, code, port);
                        break;
                    }
                    Err(Refusal::Silent) => break,
                    Err(Refusal::Cut { sent }) => {
                        pending = sent;
                        break;
                    }
                }
            }
            if made == 0 && !pending {
                return stopped(&watch);
            }
            if made > 0 {
                self.shared.publish(|status| {
                    status.protocol = Protocol::Upnp;
                    status.address = address;
                    status.port = first;
                    status.internal = first;
                });
            }
            let how = How::Upnp {
                service: service.clone(),
                permanent,
                remade: false,
            };
            return Step::Kept(self.made(
                Protocol::Upnp,
                endpoints,
                local,
                first,
                made,
                pending,
                0,
                how,
            ));
        }
        Step::Next
    }

    /// Map `port` on `service`: a leftover of this side's own on it is
    /// deleted first, anything else's is left alone, and a gateway that takes
    /// only permanent mappings gets one.
    fn upnp_add(
        &self,
        watch: &Watch<'_>,
        service: &Service,
        port: u16,
        permanent: &mut bool,
    ) -> Result<(), Refusal> {
        let kind = service.service_type;
        match self.control(watch, service, |_| {
            soap::get_specific_port_mapping_entry(kind, port)
        }) {
            Ok(entry @ Answer::Done(_)) => {
                if entry.get("NewPortMappingDescription") != Some(self.description.as_str()) {
                    return Err(Refusal::Code(FaultCode::CONFLICT.0));
                }
                if let Err(Unanswered::Cut { .. }) =
                    self.control(watch, service, |_| soap::delete_port_mapping(kind, port))
                {
                    return Err(Refusal::Cut { sent: false });
                }
            }
            // Nothing there, or a gateway that cannot say: the add will tell.
            Ok(Answer::Fault(_)) | Err(Unanswered::Other) => {}
            Err(Unanswered::Silent) => return Err(Refusal::Silent),
            Err(Unanswered::Cut { .. }) => return Err(Refusal::Cut { sent: false }),
        }
        loop {
            let lease = if *permanent { 0 } else { self.timing.lease_s };
            match self.control(watch, service, |local| {
                soap::add_port_mapping(kind, port, local, &self.description, lease)
            }) {
                Ok(Answer::Done(_)) => return Ok(()),
                // A lease refused: some gateways take permanent mappings
                // alone, and say so in one of two ways.
                Ok(Answer::Fault(code))
                    if !*permanent
                        && (code == FaultCode::ONLY_PERMANENT_LEASES
                            || code == FaultCode::INVALID_ARGS) =>
                {
                    *permanent = true;
                }
                Ok(Answer::Fault(code)) => return Err(Refusal::Code(code.0)),
                // An answer that says nothing: the entry says whether the add
                // was made.
                Err(Unanswered::Other) => {
                    return if self.ours(watch, service, port) {
                        Ok(())
                    } else {
                        Err(Refusal::Silent)
                    };
                }
                Err(Unanswered::Silent) => return Err(Refusal::Silent),
                Err(Unanswered::Cut { sent }) => return Err(Refusal::Cut { sent }),
            }
        }
    }

    /// Whether the gateway lists `port` under this side's description.
    fn ours(&self, watch: &Watch<'_>, service: &Service, port: u16) -> bool {
        let kind = service.service_type;
        match self.control(watch, service, |_| {
            soap::get_specific_port_mapping_entry(kind, port)
        }) {
            Ok(entry @ Answer::Done(_)) => {
                entry.get("NewPortMappingDescription") == Some(self.description.as_str())
            }
            _ => false,
        }
    }

    /// One control action on `service`, built once the connection's own
    /// address is known: an internal client must be the address a request
    /// comes from.
    fn control(
        &self,
        watch: &Watch<'_>,
        service: &Service,
        build: impl FnOnce(Ipv4Addr) -> soap::Action,
    ) -> Result<Answer, Unanswered> {
        let url = &service.control;
        let (response, name) = http_exchange(watch, url, http::CONTROL_CAP, |local| {
            let action = build(local);
            let headers = [
                ("Content-Type", soap::CONTENT_TYPE),
                (soap::ACTION_HEADER, action.header.as_str()),
            ];
            let request = http::request(
                Method::Post,
                url.addr,
                &url.path,
                &headers,
                action.body.as_bytes(),
            );
            (request, action.name)
        })?;
        soap::parse(name, response.status, &response.body).map_err(|_| Unanswered::Other)
    }

    fn renew(&self, mapped: &mut Mapped, cuts: u32) -> Renewal {
        let watch = self.watch(cuts);
        let now = Instant::now();
        // Late on schedule, as after a sleep; not a retry's own lateness.
        if mapped.misses == 0
            && let Some(late) = mapped
                .expires
                .and_then(|expires| now.checked_duration_since(expires))
        {
            log_warn!(
                "portmap: renewed late, protocol={} port={} late_ms={}",
                mapped.protocol.as_str(),
                mapped.first,
                late.as_millis()
            );
        }
        let renewal = match mapped.how {
            How::Pcp { .. } => self.pcp_renew(mapped, &watch),
            How::NatPmp { .. } => self.natpmp_renew(mapped, &watch),
            How::Upnp { .. } => self.upnp_renew(mapped, &watch),
        };
        if matches!(renewal, Renewal::Interrupted) || watch.interrupted() {
            return Renewal::Interrupted;
        }
        renewal
    }

    fn pcp_renew(&self, mapped: &mut Mapped, watch: &Watch<'_>) -> Renewal {
        let How::Pcp { ports, epoch } = &mut mapped.how else {
            return Renewal::Lost("protocol");
        };
        let mut granted = LONGEST_LIFETIME_S;
        let mut restarted = false;
        let mut stated: Option<(Option<Ipv4Addr>, u16)> = None;
        for ((nonce, external), port) in
            ports.iter_mut().zip(ports_from(mapped.first, mapped.count))
        {
            let nonce = *nonce;
            // The port granted is the one suggested, so it stays put.
            let request = pcp::map_request(
                IpAddr::V4(mapped.local),
                nonce,
                port,
                *external,
                self.timing.lifetime_s,
            );
            let reply = match exchange(
                watch,
                mapped.endpoints.control,
                &request,
                self.timing.renew_waits,
                |data| pcp_answer(data, nonce),
            ) {
                Exchanged::Answer(Ok(reply)) if reply.result == pcp::ResultCode::SUCCESS => reply,
                Exchanged::Answer(Ok(reply)) => {
                    self.refused(Protocol::Pcp, u16::from(reply.result.0), port);
                    return Renewal::Lost("refused");
                }
                Exchanged::Answer(Err(_)) => return Renewal::Lost("version"),
                Exchanged::Silent => return Renewal::Unanswered,
                Exchanged::Cut { .. } => return Renewal::Interrupted,
            };
            let now = Instant::now();
            restarted |= !continuous(*epoch, reply.epoch_s, now);
            *epoch = Epoch {
                server_s: reply.epoch_s,
                at: now,
            };
            granted = granted.min(reply.lifetime_s);
            if let Some(map) = reply.map {
                *external = map.external_port;
                if stated.is_none() {
                    let address = match map.external_address {
                        IpAddr::V4(address) if !bogus(address) => Some(address),
                        _ => None,
                    };
                    stated = Some((address, map.external_port));
                }
            }
        }
        if restarted {
            log_warn!(
                "portmap: lost, protocol=pcp port={} reason=gateway-restarted, mapped again",
                mapped.first
            );
        }
        // What the gateway states now: an address or a port it moved.
        if let Some((address, port)) = stated {
            self.shared.publish(|status| {
                status.address = address;
                status.port = port;
            });
        }
        let now = Instant::now();
        mapped.renew_at = now + renew_after(granted, self.timing.renew_floor);
        mapped.expires = Some(now + Duration::from_secs(u64::from(granted)));
        mapped.renewed = now;
        Renewal::Renewed
    }

    fn natpmp_renew(&self, mapped: &mut Mapped, watch: &Watch<'_>) -> Renewal {
        let How::NatPmp { externals, epoch } = &mut mapped.how else {
            return Renewal::Lost("protocol");
        };
        let request = natpmp::address_request();
        let (epoch_s, address) = match exchange(
            watch,
            mapped.endpoints.control,
            &request,
            self.timing.renew_waits,
            |d| natpmp_answer(d, 0, None),
        ) {
            Exchanged::Answer(natpmp::Reply::Address { epoch_s, address }) => (epoch_s, address),
            Exchanged::Answer(natpmp::Reply::Refused { result, .. }) => {
                self.refused(Protocol::NatPmp, result.0, mapped.first);
                return Renewal::Lost("refused");
            }
            Exchanged::Answer(natpmp::Reply::Map { .. }) => return Renewal::Lost("answer"),
            Exchanged::Silent => return Renewal::Unanswered,
            Exchanged::Cut { .. } => return Renewal::Interrupted,
        };
        let now = Instant::now();
        if !continuous(*epoch, epoch_s, now) {
            log_warn!(
                "portmap: lost, protocol=natpmp port={} reason=gateway-restarted, mapped again",
                mapped.first
            );
        }
        *epoch = Epoch {
            server_s: epoch_s,
            at: now,
        };
        let mut granted = LONGEST_LIFETIME_S;
        for (external, port) in externals
            .iter_mut()
            .zip(ports_from(mapped.first, mapped.count))
        {
            // The port granted is the one suggested, so it stays put.
            let request = natpmp::map_request(port, *external, self.timing.lifetime_s);
            match exchange(
                watch,
                mapped.endpoints.control,
                &request,
                self.timing.renew_waits,
                |d| natpmp_answer(d, 1, Some(port)),
            ) {
                Exchanged::Answer(natpmp::Reply::Map {
                    external_port,
                    lifetime_s,
                    ..
                }) => {
                    *external = external_port;
                    granted = granted.min(lifetime_s);
                }
                Exchanged::Answer(natpmp::Reply::Refused { result, .. }) => {
                    self.refused(Protocol::NatPmp, result.0, port);
                    return Renewal::Lost("refused");
                }
                Exchanged::Answer(natpmp::Reply::Address { .. }) => return Renewal::Lost("answer"),
                Exchanged::Silent => return Renewal::Unanswered,
                Exchanged::Cut { .. } => return Renewal::Interrupted,
            }
        }
        let first_external = externals.first().copied();
        self.shared.publish(|status| {
            status.address = (!bogus(address)).then_some(address);
            if let Some(port) = first_external {
                status.port = port;
            }
        });
        let now = Instant::now();
        mapped.renew_at = now + renew_after(granted, self.timing.renew_floor);
        mapped.expires = Some(now + Duration::from_secs(u64::from(granted)));
        mapped.renewed = now;
        Renewal::Renewed
    }

    fn upnp_renew(&self, mapped: &mut Mapped, watch: &Watch<'_>) -> Renewal {
        let How::Upnp {
            service,
            permanent,
            remade,
        } = &mut mapped.how
        else {
            return Renewal::Lost("protocol");
        };
        let kind = service.service_type;
        // The external address, which may have moved since.
        match self.control(watch, service, |_| soap::get_external_ip_address(kind)) {
            Ok(answer @ Answer::Done(_)) => {
                let stated = answer
                    .get("NewExternalIPAddress")
                    .and_then(|address| address.parse::<Ipv4Addr>().ok())
                    .filter(|address| !bogus(*address));
                self.shared.publish(|status| status.address = stated);
            }
            Ok(Answer::Fault(_)) | Err(Unanswered::Other) => {}
            Err(Unanswered::Silent) => return Renewal::Unanswered,
            Err(Unanswered::Cut { .. }) => return Renewal::Interrupted,
        }
        let lease_s = if *permanent { 0 } else { self.timing.lease_s };
        // A lease read back at least this long grew with the add.
        let slack = (self.timing.readd / 2).as_secs().max(1);
        let grown = u64::from(lease_s).saturating_sub(slack);
        for port in ports_from(mapped.first, mapped.count) {
            let add = |local| soap::add_port_mapping(kind, port, local, &self.description, lease_s);
            match self.control(watch, service, add) {
                Ok(Answer::Done(_)) => {}
                Ok(Answer::Fault(code)) => {
                    self.refused(Protocol::Upnp, code.0, port);
                    return Renewal::Lost("refused");
                }
                Err(Unanswered::Silent | Unanswered::Other) => return Renewal::Unanswered,
                Err(Unanswered::Cut { .. }) => return Renewal::Interrupted,
            }
            if *permanent {
                continue;
            }
            // A gateway may answer an identical add and keep the old lease.
            let left = match self.control(watch, service, |_| {
                soap::get_specific_port_mapping_entry(kind, port)
            }) {
                Ok(entry @ Answer::Done(_)) => entry
                    .get("NewLeaseDuration")
                    .and_then(|lease| lease.parse::<u64>().ok()),
                _ => None,
            };
            if left.is_some_and(|left| left < grown) {
                if !*remade {
                    log_info!(
                        "portmap: renewal kept the old lease, protocol=upnp port={port} remade=1"
                    );
                    *remade = true;
                }
                if let Err(Unanswered::Cut { .. }) =
                    self.control(watch, service, |_| soap::delete_port_mapping(kind, port))
                {
                    return Renewal::Interrupted;
                }
                match self.control(watch, service, add) {
                    Ok(Answer::Done(_)) => {}
                    Ok(Answer::Fault(code)) => {
                        self.refused(Protocol::Upnp, code.0, port);
                        return Renewal::Lost("refused");
                    }
                    Err(Unanswered::Silent | Unanswered::Other) => return Renewal::Unanswered,
                    Err(Unanswered::Cut { .. }) => return Renewal::Interrupted,
                }
            }
        }
        let now = Instant::now();
        mapped.renew_at = now + self.timing.readd;
        mapped.expires = (!*permanent).then(|| now + Duration::from_secs(u64::from(lease_s)));
        mapped.renewed = now;
        Renewal::Renewed
    }

    /// Delete every mapping by `deadline`, cut short by nothing else: PCP's
    /// and NAT-PMP's in one exchange, UPnP's a port at a time, each entry
    /// read first.
    fn delete(&self, mapped: &Mapped, deadline: Instant) {
        let watch = Watch {
            shared: &self.shared,
            seen: None,
            deadline: Some(deadline),
        };
        let asked = mapped.count.saturating_add(u16::from(mapped.pending));
        let ports = ports_from(mapped.first, asked);
        match &mapped.how {
            How::Pcp { ports: entries, .. } => {
                let requests: Vec<_> = ports
                    .zip(entries.iter())
                    .map(|(port, (nonce, _))| {
                        let request = pcp::delete_request(IpAddr::V4(mapped.local), *nonce, port);
                        (request.to_vec(), port)
                    })
                    .collect();
                delete_all(&watch, mapped.endpoints.control, &requests, |data| {
                    pcp::parse(data)
                        .ok()
                        .and_then(|reply| reply.map)
                        .map(|map| map.internal_port)
                });
            }
            How::NatPmp { .. } => {
                let requests: Vec<_> = ports
                    .map(|port| (natpmp::delete_request(port).to_vec(), port))
                    .collect();
                delete_all(
                    &watch,
                    mapped.endpoints.control,
                    &requests,
                    |data| match natpmp::parse(data) {
                        Ok(natpmp::Reply::Map { internal_port, .. }) => Some(internal_port),
                        _ => None,
                    },
                );
            }
            How::Upnp { service, .. } => {
                for port in ports {
                    if watch.interrupted() {
                        break;
                    }
                    // An entry that lapsed and was taken since is another
                    // device's: one delete by port would take it.
                    if self.ours(&watch, service, port) {
                        let _ = self.control(&watch, service, |_| {
                            soap::delete_port_mapping(service.service_type, port)
                        });
                    }
                }
            }
        }
        log_info!(
            "portmap: deleted, protocol={} port={} count={}",
            mapped.protocol.as_str(),
            mapped.first,
            asked
        );
        self.shared.clear();
    }
}

/// This side's address toward `to`, as the routing table picks it. Nothing is
/// sent.
pub(crate) fn local_toward(to: SocketAddrV4) -> Option<Ipv4Addr> {
    let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.connect(to).ok()?;
    match socket.local_addr().ok()? {
        SocketAddr::V4(local) if !local.ip().is_unspecified() => Some(*local.ip()),
        _ => None,
    }
}

/// A PCP mapping's answer to the request `nonce` names: the reply, or the
/// version of a gateway that does not speak this one. Anything else is
/// another request's, or noise.
fn pcp_answer(data: &[u8], nonce: Nonce) -> Option<Result<pcp::Reply, u8>> {
    match pcp::parse(data) {
        Err(crate::Error::Version(version)) => Some(Err(version)),
        Ok(reply) if reply.opcode != pcp::MAP => None,
        Ok(reply) => match reply.map {
            Some(map) if map.nonce != nonce => None,
            // A refusal may stop before the nonce.
            _ => Some(Ok(reply)),
        },
        Err(_) => None,
    }
}

/// A NAT-PMP answer to `opcode`, for `port` when it maps one.
fn natpmp_answer(data: &[u8], opcode: u8, port: Option<u16>) -> Option<natpmp::Reply> {
    match natpmp::parse(data).ok()? {
        reply @ natpmp::Reply::Address { .. } if opcode == 0 => Some(reply),
        reply @ natpmp::Reply::Map { internal_port, .. }
            if opcode == 1 && Some(internal_port) == port =>
        {
            Some(reply)
        }
        reply @ natpmp::Reply::Refused {
            opcode: refused, ..
        } if refused == opcode => Some(reply),
        _ => None,
    }
}

/// Whether a gateway kept its state between two answers: its epoch and this
/// side's clock advanced alike, within two seconds and a sixteenth, and the
/// epoch went back by no more than a second.
fn continuous(previous: Epoch, server_s: u32, now: Instant) -> bool {
    if u64::from(server_s) + 1 < u64::from(previous.server_s) {
        return false;
    }
    let server = u64::from(server_s.saturating_sub(previous.server_s));
    let client = now.saturating_duration_since(previous.at).as_secs();
    !(client + 2 < server - server / 16 || server + 2 < client - client / 16)
}

/// When to renew a lifetime: half of it, then up to an eighth more so that
/// many clients behind one gateway do not renew at once; never sooner than
/// `floor`.
fn renew_after(granted_s: u32, floor: Duration) -> Duration {
    let granted = Duration::from_secs(u64::from(granted_s.min(LONGEST_LIFETIME_S)));
    let mut draw = [0u8; 2];
    let jitter = match lowlat_crypto::fill(&mut draw) {
        Ok(()) => (granted / 8).mul_f64(f64::from(u16::from_le_bytes(draw)) / f64::from(u16::MAX)),
        Err(_) => Duration::ZERO,
    };
    (granted / 2 + jitter).max(floor)
}

/// A datagram socket that hears `to` alone.
fn connected(to: SocketAddrV4) -> Option<UdpSocket> {
    let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.connect(to).ok()?;
    Some(socket)
}

fn is_timeout(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

/// A datagram longer than the buffer, which one system refuses to read:
/// passed over like any other that is not an answer.
fn oversize(error: &io::Error) -> bool {
    /// The system's code for it, where it has one.
    const MESSAGE_SIZE: i32 = 10040;
    cfg!(windows) && error.raw_os_error() == Some(MESSAGE_SIZE)
}

/// One request to the gateway's port, sent again after each of `waits` in
/// silence; the first answer `take` takes. A refusal of the datagram itself
/// is nothing listening.
fn exchange<T>(
    watch: &Watch<'_>,
    to: SocketAddrV4,
    request: &[u8],
    waits: &[Duration],
    mut take: impl FnMut(&[u8]) -> Option<T>,
) -> Exchanged<T> {
    let Some(socket) = connected(to) else {
        return Exchanged::Silent;
    };
    // One byte past the longest message, so a longer one is seen as one.
    let mut buf = [0u8; pcp::MAX_LEN + 1];
    let mut sent = false;
    for wait in waits {
        if watch.interrupted() {
            return Exchanged::Cut { sent };
        }
        if socket.send(request).is_err() {
            return Exchanged::Silent;
        }
        sent = true;
        let until = Instant::now() + *wait;
        while let Some(slice) = watch.slice(until) {
            if socket.set_read_timeout(Some(slice)).is_err() {
                return Exchanged::Silent;
            }
            match socket.recv(&mut buf) {
                Ok(n) => {
                    if let Some(found) = take(buf.get(..n).unwrap_or_default()) {
                        return Exchanged::Answer(found);
                    }
                }
                Err(error) if is_timeout(&error) || oversize(&error) => {}
                Err(_) => return Exchanged::Silent,
            }
        }
        if watch.interrupted() {
            return Exchanged::Cut { sent };
        }
    }
    Exchanged::Silent
}

/// Every delete sent at once, and once more for those unanswered halfway
/// through the bound.
fn delete_all(
    watch: &Watch<'_>,
    to: SocketAddrV4,
    requests: &[(Vec<u8>, u16)],
    answered: impl Fn(&[u8]) -> Option<u16>,
) {
    let Some(socket) = connected(to) else {
        return;
    };
    let mut open: Vec<u16> = requests.iter().map(|&(_, port)| port).collect();
    let halfway = watch
        .deadline
        .map(|deadline| Instant::now() + deadline.saturating_duration_since(Instant::now()) / 2);
    let mut buf = [0u8; pcp::MAX_LEN + 1];
    for until in [halfway, watch.deadline] {
        let Some(until) = until else {
            return;
        };
        for (request, port) in requests {
            if open.contains(port) {
                let _ = socket.send(request);
            }
        }
        while let Some(slice) = watch.slice(until) {
            if open.is_empty() {
                return;
            }
            if socket.set_read_timeout(Some(slice)).is_err() {
                return;
            }
            match socket.recv(&mut buf) {
                Ok(n) => {
                    if let Some(port) = answered(buf.get(..n).unwrap_or_default()) {
                        open.retain(|&left| left != port);
                    }
                }
                Err(error) if is_timeout(&error) || oversize(&error) => {}
                Err(_) => return,
            }
        }
    }
}

/// One HTTP exchange with `url`'s host: a fresh connection, a request built
/// once its own address is known, the response read to its end. The connect
/// is waited for in slices like everything else.
fn http_exchange<T>(
    watch: &Watch<'_>,
    url: &Url,
    cap: usize,
    make: impl FnOnce(Ipv4Addr) -> (Vec<u8>, T),
) -> Result<(http::Response, T), Unanswered> {
    let ended = |sent| {
        if watch.interrupted() {
            Unanswered::Cut { sent }
        } else {
            Unanswered::Silent
        }
    };
    let begun = Instant::now();
    let until = begun + EXCHANGE;
    let mut connecting = sys::Connecting::start(url.addr).map_err(|_| Unanswered::Silent)?;
    let mut stream = loop {
        let Some(slice) = watch.slice((begun + CONNECT).min(until)) else {
            return Err(ended(false));
        };
        match connecting.wait(slice) {
            Ok(Ok(stream)) => break stream,
            Ok(Err(still)) => connecting = still,
            Err(_) => return Err(Unanswered::Silent),
        }
    };
    let Ok(SocketAddr::V4(local)) = stream.local_addr() else {
        return Err(Unanswered::Silent);
    };
    let (request, tag) = make(*local.ip());
    if watch.interrupted() {
        return Err(Unanswered::Cut { sent: false });
    }
    stream
        .set_write_timeout(Some(CONNECT))
        .map_err(|_| Unanswered::Silent)?;
    stream.write_all(&request).map_err(|_| Unanswered::Silent)?;
    let mut reader = http::Reader::new(cap);
    let mut buf = [0u8; 4096];
    loop {
        let Some(slice) = watch.slice(until) else {
            return Err(ended(true));
        };
        stream
            .set_read_timeout(Some(slice))
            .map_err(|_| Unanswered::Silent)?;
        match stream.read(&mut buf) {
            Ok(0) => {
                return reader
                    .finish()
                    .map(|response| (response, tag))
                    .map_err(|_| Unanswered::Silent);
            }
            Ok(n) => match reader.push(buf.get(..n).unwrap_or_default()) {
                Ok(Some(response)) => return Ok((response, tag)),
                Ok(None) => {}
                Err(_) => return Err(Unanswered::Silent),
            },
            Err(error) if is_timeout(&error) => {}
            Err(_) => return Err(Unanswered::Silent),
        }
    }
}

/// Search for the gateway's description: the gateway itself first, then the
/// group, out of the interface toward the gateway, and both again halfway
/// through the wait -- a datagram to the group can be lost on a wireless
/// link, and some gateways answer nothing else. The first answer that places
/// its description on the gateway's own address is taken.
fn search(watch: &Watch<'_>, endpoints: Endpoints, local: Ipv4Addr) -> Option<Url> {
    let socket = UdpSocket::bind(SocketAddrV4::new(local, 0)).ok()?;
    // On a machine with several interfaces the group alone names none.
    let _ = sys::multicast_from(&socket, local);
    let _ = socket.set_multicast_ttl_v4(2);
    let direct = ssdp::search(endpoints.search, ssdp::GATEWAY_1, SEARCH_DELAY_S);
    let grouped = ssdp::search(ssdp::GROUP, ssdp::GATEWAY_1, SEARCH_DELAY_S);
    let send = || {
        let _ = socket.send_to(direct.as_bytes(), endpoints.search);
        let _ = socket.send_to(grouped.as_bytes(), endpoints.group);
    };
    send();
    let begun = Instant::now();
    let until = begun + SEARCH;
    let mut again = Some(begun + SEARCH / 2);
    let mut buf = [0u8; ssdp::MAX_LEN];
    // Errors in a row: the gateway's own port refusing the direct search is
    // one, and the group's answer may still come; more is no search at all.
    let mut errors = 0u32;
    while let Some(slice) = watch.slice(until) {
        if again.is_some_and(|at| Instant::now() >= at) {
            again = None;
            send();
        }
        let slice = again.map_or(slice, |at| {
            slice.min(
                at.saturating_duration_since(Instant::now())
                    .max(Duration::from_millis(1)),
            )
        });
        socket.set_read_timeout(Some(slice)).ok()?;
        let (n, _) = match socket.recv_from(&mut buf) {
            Ok(received) => received,
            Err(error) if is_timeout(&error) => continue,
            Err(_) => {
                errors += 1;
                if errors > 2 {
                    return None;
                }
                continue;
            }
        };
        errors = 0;
        let Ok(answer) = ssdp::parse(buf.get(..n).unwrap_or_default()) else {
            continue;
        };
        if let Ok(location) = Url::parse(answer.location)
            && *location.addr.ip() == endpoints.gateway
        {
            return Some(location);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::{Behaviour, EXTERNAL, Elsewhere, Entry, Fake, Second};

    const PORT: u16 = 24137;
    const OURS: &str = "ll-test";

    fn config(port: u16, count: u16, gateway: Gateway) -> Config {
        Config {
            port,
            count,
            description: OURS.into(),
            gateway: Some(gateway),
        }
    }

    fn start(fake: &Fake, port: u16, count: u16) -> Mapper {
        Mapper::start(config(port, count, fake.gateway())).unwrap()
    }

    /// As [`start`], with intervals of the test's own.
    fn start_timed(fake: &Fake, port: u16, timing: Timing) -> Mapper {
        let gateway = Gateway {
            endpoints: fake.endpoints,
            timing,
        };
        Mapper::start(config(port, 1, gateway)).unwrap()
    }

    /// Wait for `holds`, failing with `what` after a generous bound.
    fn until(what: &str, holds: impl FnMut() -> bool) {
        until_within(
            Duration::from_secs(10),
            Duration::from_millis(10),
            what,
            holds,
        );
    }

    /// Wait for `holds`, looking every `every`, failing with `what` after
    /// `bound`.
    fn until_within(bound: Duration, every: Duration, what: &str, mut holds: impl FnMut() -> bool) {
        let end = Instant::now() + bound;
        while !holds() {
            assert!(Instant::now() < end, "never: {what}");
            std::thread::sleep(every);
        }
    }

    /// Stopped within the bound, every mapping deleted from the gateway, and
    /// nothing left for a reader.
    fn stopped(mut mapper: Mapper, fake: &Fake) {
        let reader = mapper.reader();
        let begun = Instant::now();
        mapper.stop();
        let took = begun.elapsed();
        // The delete's bound from the stop, and a slice; an exchange waited
        // out would take seconds.
        assert!(took < Duration::from_millis(600), "the stop took {took:?}");
        assert!(fake.table().is_empty(), "left behind: {:?}", fake.table());
        assert_eq!(mapper.status().protocol, Protocol::None);
        assert_eq!(reader.external(), None);
    }

    fn mapped(mapper: &Mapper, protocol: Protocol) -> impl FnMut() -> bool {
        move || mapper.status().protocol == protocol
    }

    #[test]
    fn pcp_first() {
        let fake = Fake::start(Behaviour::default());
        let mapper = start(&fake, PORT, 1);
        until("a PCP mapping", mapped(&mapper, Protocol::Pcp));
        let status = mapper.status();
        assert_eq!((status.address, status.port), (Some(EXTERNAL), PORT));
        // Read without the lock as the status states it.
        assert_eq!(
            mapper.reader().external(),
            Some(SocketAddrV4::new(EXTERNAL, PORT))
        );
        let entry = &fake.table()[&PORT];
        assert_eq!((entry.via, entry.client), ("pcp", Ipv4Addr::LOCALHOST));
        assert!(entry.nonce.is_some());
        // Neither of the others was asked anything.
        assert_eq!(fake.calls("GetExternalIPAddress", 0), 0);
        stopped(mapper, &fake);
    }

    #[test]
    fn nat_pmp_when_pcp_is_answered_in_its_version() {
        let fake = Fake::start(Behaviour {
            pcp: false,
            ..Behaviour::default()
        });
        let mapper = start(&fake, PORT, 1);
        until("a NAT-PMP mapping", mapped(&mapper, Protocol::NatPmp));
        assert_eq!(mapper.status().address, Some(EXTERNAL));
        assert_eq!(fake.table()[&PORT].via, "natpmp");
        stopped(mapper, &fake);
    }

    #[test]
    fn upnp_when_neither_answers() {
        let fake = Fake::start(Behaviour::upnp_only());
        let mapper = start(&fake, PORT, 1);
        until("a UPnP mapping", mapped(&mapper, Protocol::Upnp));
        let status = mapper.status();
        assert_eq!((status.address, status.port), (Some(EXTERNAL), PORT));
        let entry = &fake.table()[&PORT];
        assert_eq!(
            (entry.via, entry.client, entry.description.as_str()),
            ("upnp", Ipv4Addr::LOCALHOST, OURS)
        );
        assert!(entry.expires.is_some(), "a timed lease was asked for");
        stopped(mapper, &fake);
    }

    #[test]
    fn a_gateway_that_takes_only_permanent_mappings_gets_one() {
        // Said with either code a gateway uses for it.
        for code in [725, 402] {
            let fake = Fake::start(Behaviour {
                permanent_only: Some(code),
                ..Behaviour::upnp_only()
            });
            let mapper = start(&fake, PORT, 1);
            until("a permanent mapping", mapped(&mapper, Protocol::Upnp));
            assert_eq!(fake.table()[&PORT].expires, None, "code {code}");
            // Permanent, so deleted at the stop or never.
            stopped(mapper, &fake);
        }
    }

    #[test]
    fn a_renewal_that_kept_the_old_lease_is_made_again() {
        let fake = Fake::start(Behaviour {
            keeps_old_lease: true,
            ..Behaviour::upnp_only()
        });
        let mapper = start(&fake, PORT, 1);
        until("a renewal made again", || {
            fake.calls("DeletePortMapping", PORT) >= 1
        });
        until("the mapping added again", || {
            fake.table().contains_key(&PORT)
        });
        stopped(mapper, &fake);
    }

    #[test]
    fn a_renewal_that_grew_the_lease_is_left_alone() {
        let fake = Fake::start(Behaviour::upnp_only());
        let mapper = start(&fake, PORT, 1);
        // Made, then renewed twice.
        until("two renewals", || fake.calls("AddPortMapping", PORT) >= 3);
        assert_eq!(fake.calls("DeletePortMapping", PORT), 0);
        stopped(mapper, &fake);
    }

    #[test]
    fn a_gateway_that_restarted_is_mapped_again() {
        let fake = Fake::start(Behaviour::default());
        let mapper = start(&fake, PORT, 1);
        until("a PCP mapping", || fake.table().contains_key(&PORT));
        fake.restart();
        assert!(fake.table().is_empty());
        until("the mapping made again", || {
            fake.table().contains_key(&PORT)
        });
        assert_eq!(mapper.status().protocol, Protocol::Pcp);
        stopped(mapper, &fake);
    }

    #[test]
    fn a_port_that_moves_is_mapped_where_it_went() {
        // Looked at again after a minute: the moved port must not wait for it.
        let timing = Timing {
            retry: Duration::from_secs(60),
            ..FAST
        };
        for behaviour in [Behaviour::default(), Behaviour::upnp_only()] {
            let fake = Fake::start(behaviour);
            let mapper = start_timed(&fake, PORT, timing);
            until("the first port", || fake.table().contains_key(&PORT));
            mapper.set_port(PORT + 100);
            until("the moved port alone", || {
                let table = fake.table();
                table.contains_key(&(PORT + 100)) && !table.contains_key(&PORT)
            });
            assert_eq!(mapper.status().port, PORT + 100);
            // Deleted, where waiting would have seen it lapse as well.
            let deleted = fake.calls("pcp-delete", PORT) + fake.calls("DeletePortMapping", PORT);
            assert_eq!(deleted, 1);
            stopped(mapper, &fake);
        }
    }

    #[test]
    fn nothing_is_asked_of_a_host_other_than_the_gateway() {
        for elsewhere in [Elsewhere::Location, Elsewhere::Control] {
            let fake = Fake::start(Behaviour {
                elsewhere: Some(elsewhere),
                ..Behaviour::upnp_only()
            });
            let mapper = start(&fake, PORT, 1);
            until("the search answered", || fake.calls("M-SEARCH", 0) >= 1);
            // Followed, the other host would be asked within milliseconds.
            std::thread::sleep(Duration::from_millis(500));
            assert_eq!(fake.calls("elsewhere", 0), 0, "{elsewhere:?}");
            assert_eq!(mapper.status().protocol, Protocol::None);
            stopped(mapper, &fake);
        }
    }

    #[test]
    fn a_range_of_ports_is_mapped_and_deleted_whole() {
        for behaviour in [Behaviour::default(), Behaviour::upnp_only()] {
            let fake = Fake::start(behaviour);
            let mapper = start(&fake, PORT, 3);
            until("three ports", || fake.table().len() == 3);
            assert_eq!(
                fake.table().keys().copied().collect::<Vec<_>>(),
                vec![PORT, PORT + 1, PORT + 2]
            );
            assert_eq!(mapper.status().port, PORT);
            stopped(mapper, &fake);
        }
    }

    #[test]
    fn a_leftover_of_ours_is_deleted_before_the_add() {
        let fake = Fake::start(Behaviour::upnp_only());
        // Ours from an earlier run, for an address this side no longer has.
        fake.insert(
            PORT,
            Entry {
                via: "upnp",
                client: Ipv4Addr::new(192, 0, 2, 9),
                description: OURS.into(),
                expires: None,
                nonce: None,
            },
        );
        let mapper = start(&fake, PORT, 1);
        until("our mapping, for this side's address", || {
            fake.table()
                .get(&PORT)
                .is_some_and(|entry| entry.client == Ipv4Addr::LOCALHOST)
        });
        assert_eq!(fake.calls("DeletePortMapping", PORT), 1);
        stopped(mapper, &fake);
    }

    #[test]
    fn another_devices_mapping_is_left_alone() {
        let fake = Fake::start(Behaviour::upnp_only());
        let theirs = Entry {
            via: "upnp",
            client: Ipv4Addr::new(192, 0, 2, 9),
            description: "theirs".into(),
            expires: None,
            nonce: None,
        };
        fake.insert(PORT, theirs.clone());
        let mut mapper = start(&fake, PORT, 1);
        until("a conflict", || {
            mapper.status().refusal == Some((Protocol::Upnp, 718))
        });
        assert_eq!(mapper.status().protocol, Protocol::None);
        assert_eq!(fake.calls("AddPortMapping", PORT), 0);
        assert_eq!(fake.calls("DeletePortMapping", PORT), 0);
        mapper.stop();
        assert_eq!(fake.table().get(&PORT), Some(&theirs));
    }

    #[test]
    fn an_external_address_that_cannot_be_is_not_mapped() {
        for stated in ["0.0.0.0", "127.0.0.1"] {
            let fake = Fake::start(Behaviour {
                stated,
                ..Behaviour::upnp_only()
            });
            let mapper = start(&fake, PORT, 1);
            until("the address asked", || {
                fake.calls("GetExternalIPAddress", 0) >= 1
            });
            assert_eq!(fake.calls("AddPortMapping", PORT), 0, "{stated}");
            assert_eq!(mapper.status().protocol, Protocol::None);
            stopped(mapper, &fake);
        }
    }

    #[test]
    fn an_external_address_not_stated_is_mapped_all_the_same() {
        let fake = Fake::start(Behaviour {
            stated: "",
            ..Behaviour::upnp_only()
        });
        let mapper = start(&fake, PORT, 1);
        until("a UPnP mapping", mapped(&mapper, Protocol::Upnp));
        assert_eq!(mapper.status().address, None);
        // Mapped, and nothing a reader could offer anyone.
        assert_eq!(mapper.reader().external(), None);
        assert!(fake.table().contains_key(&PORT));
        stopped(mapper, &fake);
    }

    #[test]
    fn a_stop_is_bounded_while_an_answer_is_slow() {
        let fake = Fake::start(Behaviour {
            slow: Duration::from_secs(5),
            ..Behaviour::upnp_only()
        });
        let mapper = start(&fake, PORT, 1);
        // In the middle of reading the description.
        std::thread::sleep(Duration::from_millis(300));
        stopped(mapper, &fake);
    }

    #[test]
    fn a_stop_is_bounded_while_nothing_answers() {
        let fake = Fake::start(Behaviour {
            upnp: false,
            ..Behaviour::upnp_only()
        });
        let mapper = start(&fake, PORT, 1);
        // In the middle of the search's wait.
        std::thread::sleep(Duration::from_millis(300));
        stopped(mapper, &fake);
    }

    #[test]
    fn a_port_below_the_system_range_is_never_mapped() {
        let fake = Fake::start(Behaviour::default());
        let mapper = start(&fake, 80, 1);
        std::thread::sleep(Duration::from_millis(300));
        assert!(fake.table().is_empty());
        assert_eq!(mapper.status(), Status::default());
        stopped(mapper, &fake);
    }

    /// Confirmed once a reflexive server saw the gateway's address, in either
    /// notation and at whatever port; never before, and never for another.
    #[test]
    fn a_mapping_is_confirmed_once_a_reflexive_server_saw_its_address() {
        let external = SocketAddrV4::new(Ipv4Addr::new(203, 0, 113, 7), 24137);
        let seen: SocketAddr = "203.0.113.7:51461".parse().unwrap();
        let mapped_form: SocketAddr = "[::ffff:203.0.113.7]:51461".parse().unwrap();
        let beyond: SocketAddr = "198.51.100.9:24137".parse().unwrap();
        let offered = Some(SocketAddr::V4(external));
        assert_eq!(confirmed(external, &[]), None);
        assert_eq!(confirmed(external, &[beyond]), None);
        assert_eq!(confirmed(external, &[beyond, seen]), offered);
        assert_eq!(confirmed(external, &[mapped_form]), offered);
    }

    #[test]
    fn an_epoch_kept_and_an_epoch_lost() {
        let at = Instant::now();
        let then = Epoch { server_s: 1000, at };
        // An hour on both clocks, then within the sixteenth, then outside it.
        let hour = Duration::from_secs(3600);
        assert!(continuous(then, 4600, at + hour));
        assert!(continuous(then, 4600 - 200, at + hour));
        assert!(!continuous(then, 4600 - 300, at + hour));
        assert!(!continuous(then, 4600 + 300, at + hour));
        // A second back is skew; more is a restart.
        assert!(continuous(then, 999, at));
        assert!(!continuous(then, 998, at));
        assert!(!continuous(then, 3, at + Duration::from_secs(5)));
    }

    #[test]
    fn a_lifetime_is_renewed_between_a_half_and_five_eighths() {
        for _ in 0..64 {
            let after = renew_after(7200, TIMING.renew_floor);
            assert!(
                after >= Duration::from_secs(3600) && after <= Duration::from_secs(4500),
                "{after:?}"
            );
        }
        // Never sooner than the floor, whatever is granted.
        assert_eq!(renew_after(0, TIMING.renew_floor), Duration::from_secs(4));
        assert_eq!(renew_after(1, TIMING.renew_floor), Duration::from_secs(4));
        // An absurd lifetime is read as a day.
        assert!(renew_after(u32::MAX, TIMING.renew_floor) <= Duration::from_secs(54_000));
    }

    /// The last port is a port like any other: a range ending there is mapped
    /// and deleted, and nothing steps past it.
    #[test]
    fn a_range_ending_at_the_last_port_is_mapped_and_deleted() {
        for behaviour in [Behaviour::default(), Behaviour::upnp_only()] {
            let fake = Fake::start(behaviour);
            let mapper = start(&fake, u16::MAX, 1);
            until("the last port", || fake.table().contains_key(&u16::MAX));
            stopped(mapper, &fake);
        }
    }

    /// A mapping asked for and never answered may be made all the same: a
    /// stop while its answer is awaited deletes it with everything else.
    #[test]
    fn a_stop_while_an_answer_is_awaited_leaves_nothing() {
        let pcp = Behaviour {
            unanswered: Some("MAP"),
            natpmp: false,
            upnp: false,
            ..Behaviour::default()
        };
        let upnp = Behaviour {
            unanswered: Some("AddPortMapping"),
            ..Behaviour::upnp_only()
        };
        for behaviour in [pcp, upnp] {
            let fake = Fake::start(behaviour);
            let mapper = start(&fake, PORT, 1);
            // Made on the gateway, its answer withheld.
            until("the mapping made", || fake.table().contains_key(&PORT));
            assert_eq!(mapper.status().protocol, Protocol::None);
            stopped(mapper, &fake);
        }
    }

    /// A renewal nothing answers keeps the mapping and its nonce and is tried
    /// again before the lifetime ends: the gateway, back, renews it under the
    /// same nonce, where a new one would be refused for a port it holds.
    #[test]
    fn a_renewal_nothing_answers_is_kept_and_tried_again() {
        let fake = Fake::start(Behaviour {
            natpmp: false,
            upnp: false,
            ..Behaviour::default()
        });
        let mapper = start(&fake, PORT, 1);
        until("a PCP mapping", || fake.table().contains_key(&PORT));
        let nonce = fake.table()[&PORT].nonce;
        // Silent across the first renewal, due between a half and five
        // eighths of the lifetime, and back before the lifetime ends.
        std::thread::sleep(Duration::from_millis(1500));
        fake.mute(Duration::from_millis(1200));
        let lifetime = Duration::from_secs(u64::from(FAST.lifetime_s));
        let began = Instant::now();
        while began.elapsed() < lifetime + Duration::from_secs(1) {
            let at = began.elapsed();
            assert_eq!(mapper.status().protocol, Protocol::Pcp, "lost {at:?} in");
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(
            fake.table()[&PORT].nonce,
            nonce,
            "renewed under another nonce"
        );
        stopped(mapper, &fake);
    }

    /// The same for a UPnP lease: a re-add nothing answers is tried again
    /// until the lease ends, and the mapping is kept meanwhile.
    #[test]
    fn a_readd_nothing_answers_is_kept_and_tried_again() {
        let fake = Fake::start(Behaviour::upnp_only());
        let mapper = start(&fake, PORT, 1);
        until("a UPnP mapping", mapped(&mapper, Protocol::Upnp));
        // Silent across the first re-add, and back well before the lease
        // ends.
        std::thread::sleep(Duration::from_millis(1500));
        fake.mute(Duration::from_millis(1500));
        let lease = Duration::from_secs(u64::from(FAST.lease_s));
        let began = Instant::now();
        while began.elapsed() < lease {
            let at = began.elapsed();
            assert_eq!(mapper.status().protocol, Protocol::Upnp, "lost {at:?} in");
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(fake.table().contains_key(&PORT));
        stopped(mapper, &fake);
    }

    /// An entry that lapsed and was taken since is another device's: a stop
    /// reads before it deletes, and leaves it in place.
    #[test]
    fn a_stop_leaves_an_entry_taken_since() {
        let fake = Fake::start(Behaviour::upnp_only());
        let mut mapper = start(&fake, PORT, 1);
        until("a UPnP mapping", mapped(&mapper, Protocol::Upnp));
        let theirs = Entry {
            via: "upnp",
            client: Ipv4Addr::new(192, 0, 2, 9),
            description: "theirs".into(),
            expires: None,
            nonce: None,
        };
        fake.insert(PORT, theirs.clone());
        mapper.stop();
        assert_eq!(fake.table().get(&PORT), Some(&theirs));
    }

    /// An attempt's look makes again at once what a gateway that restarted
    /// lost, long before the renewal would have.
    #[test]
    fn a_look_makes_again_what_a_restarted_gateway_lost() {
        let fake = Fake::start(Behaviour::default());
        // Renewed after half a minute, unless a look comes first.
        let timing = Timing {
            lifetime_s: 60,
            ..FAST
        };
        let mapper = start_timed(&fake, PORT, timing);
        until("a PCP mapping", || fake.table().contains_key(&PORT));
        std::thread::sleep(timing.look_floor);
        fake.restart();
        mapper.refresh();
        until_within(
            Duration::from_secs(1),
            Duration::from_millis(10),
            "made again at the look",
            || fake.table().contains_key(&PORT),
        );
        stopped(mapper, &fake);
    }

    /// An attempt's look climbs the ladder as soon as the climb under way
    /// ends, rather than at the next retry.
    #[test]
    fn a_look_climbs_the_ladder_again_at_once() {
        let fake = Fake::start(Behaviour {
            natpmp: false,
            upnp: false,
            ..Behaviour::default()
        });
        fake.mute(Duration::from_secs(1));
        // Looked at again after a minute, unless a look comes first.
        let timing = Timing {
            retry: Duration::from_secs(60),
            ..FAST
        };
        let mapper = start_timed(&fake, PORT, timing);
        // The first climb's PCP went unanswered; the rest of it still runs.
        std::thread::sleep(Duration::from_millis(1200));
        assert!(fake.table().is_empty());
        mapper.refresh();
        until("mapped at the look", mapped(&mapper, Protocol::Pcp));
        stopped(mapper, &fake);
    }

    /// A gateway that restarted and grants another external port: the
    /// renewal states it, and the next renewal suggests it, so it stays.
    #[test]
    fn a_renewal_states_and_keeps_the_port_granted() {
        let fake = Fake::start(Behaviour {
            natpmp: false,
            upnp: false,
            ..Behaviour::default()
        });
        let timing = Timing {
            lifetime_s: 60,
            ..FAST
        };
        let mapper = start_timed(&fake, PORT, timing);
        until("a PCP mapping", || fake.table().contains_key(&PORT));
        assert_eq!(mapper.status().port, PORT);
        std::thread::sleep(timing.look_floor);
        fake.restart_shifted(1);
        mapper.refresh();
        until("the port granted stated", || {
            mapper.status().port == PORT + 1
        });
        assert_eq!(
            mapper.reader().external(),
            Some(SocketAddrV4::new(EXTERNAL, PORT + 1))
        );
        std::thread::sleep(timing.look_floor);
        mapper.refresh();
        until("the port granted suggested", || {
            fake.suggested(PORT) == Some(PORT + 1)
        });
        stopped(mapper, &fake);
    }

    /// A port moved from is not read: its mapping is about to go, and an
    /// attempt on the new port must not offer it.
    #[test]
    fn a_moved_port_is_not_read() {
        let fake = Fake::start(Behaviour::default());
        let mapper = start(&fake, PORT, 1);
        until("a PCP mapping", mapped(&mapper, Protocol::Pcp));
        let reader = mapper.reader();
        let old = Some(SocketAddrV4::new(EXTERNAL, PORT));
        assert_eq!(reader.external(), old);
        mapper.set_port(PORT + 100);
        assert_ne!(reader.external(), old, "the old port read after the move");
        until("the new port read", || {
            reader.external() == Some(SocketAddrV4::new(EXTERNAL, PORT + 100))
        });
        stopped(mapper, &fake);
    }

    /// A service that answers with no protocol at all, a plain 404, is passed
    /// over for the next, as one that faults is.
    #[test]
    fn a_service_that_answers_no_protocol_is_passed_over() {
        let fake = Fake::start(Behaviour {
            second: Some(Second::Broken),
            ..Behaviour::upnp_only()
        });
        let mapper = start(&fake, PORT, 1);
        until("a UPnP mapping", mapped(&mapper, Protocol::Upnp));
        assert!(fake.calls("second-GetExternalIPAddress", 0) >= 1);
        stopped(mapper, &fake);
    }

    /// A connection that says it is down is passed over, though it states no
    /// address and would take a mapping all the same: the one that is up
    /// maps.
    #[test]
    fn a_connection_that_is_down_is_passed_over() {
        let fake = Fake::start(Behaviour {
            second: Some(Second::Down),
            ..Behaviour::upnp_only()
        });
        let mapper = start(&fake, PORT, 1);
        until("a UPnP mapping", mapped(&mapper, Protocol::Upnp));
        assert_eq!(mapper.status().address, Some(EXTERNAL));
        assert_eq!(fake.calls("second-AddPortMapping", 0), 0);
        stopped(mapper, &fake);
    }

    /// A search lost once is sent again within the same wait, not at the next
    /// look.
    #[test]
    fn a_search_lost_once_is_sent_again() {
        // The direct search and the group's, both passed over.
        let fake = Fake::start(Behaviour {
            searches_ignored: 2,
            ..Behaviour::upnp_only()
        });
        let mapper = start(&fake, PORT, 1);
        until_within(
            SEARCH,
            Duration::from_millis(10),
            "mapped within the first search's wait",
            mapped(&mapper, Protocol::Upnp),
        );
        stopped(mapper, &fake);
    }

    /// A connection is made to a listener, and one to nowhere is waited for
    /// in pieces: starting returns at once, and a wait ends at its bound.
    #[test]
    fn a_connection_is_waited_for_in_pieces() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let SocketAddr::V4(to) = listener.local_addr().unwrap() else {
            unreachable!("bound to an IPv4 address");
        };
        let mut connecting = sys::Connecting::start(to).unwrap();
        let made = loop {
            match connecting.wait(Duration::from_millis(50)).unwrap() {
                Ok(stream) => break stream,
                Err(still) => connecting = still,
            }
        };
        assert_eq!(made.peer_addr().unwrap(), SocketAddr::V4(to));
        // A documentation address, which no route answers.
        let nowhere = SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 1), 9);
        let begun = Instant::now();
        let started = sys::Connecting::start(nowhere);
        // Whatever came of it, starting did not wait for it.
        assert!(
            begun.elapsed() < Duration::from_millis(100),
            "starting waited for the connection"
        );
        let Ok(connecting) = started else {
            println!("skipped: this machine has no route to {nowhere}");
            return;
        };
        let waited = Instant::now();
        match connecting.wait(Duration::from_millis(50)) {
            Ok(Err(_)) => assert!(
                waited.elapsed() < Duration::from_millis(500),
                "a wait outlasted its bound"
            ),
            _ => println!("skipped: {nowhere} was answered at once on this network"),
        }
    }

    /// The gateway's UPnP service, found as the ladder finds it, with a
    /// runner to ask it through.
    fn gateway_service(endpoints: Endpoints) -> (Runner, Service) {
        let runner = Runner {
            shared: Arc::new(Shared {
                generation: AtomicU32::new(0),
                cuts: AtomicU32::new(0),
                stopping: AtomicBool::new(false),
                stopped_at: Mutex::new(None),
                look: AtomicBool::new(false),
                port: AtomicU32::new(0),
                status: Mutex::new(Status::default()),
                external: AtomicU64::new(0),
            }),
            count: 1,
            description: OURS.into(),
            endpoints: Some(endpoints),
            timing: FAST,
            quiet: Cell::new(false),
            refusals: [const { Cell::new(None) }; 3],
            only: None,
        };
        let watch = runner.watch(0);
        let local = local_toward(endpoints.control).expect("a route to the gateway");
        let location = search(&watch, endpoints, local).expect("the gateway answers a search");
        let (response, ()) = http_exchange(&watch, &location, http::DESCRIPTION_CAP, |_| {
            let request = http::request(Method::Get, location.addr, &location.path, &[], &[]);
            (request, ())
        })
        .expect("its description");
        let found = desc::parse(&response.body, &location).unwrap();
        let service = found
            .connections()
            .find(|service| *service.control.addr.ip() == endpoints.gateway)
            .expect("a connection service on the gateway")
            .clone();
        (runner, service)
    }

    /// The internal client the gateway lists on `port`, and the lease left.
    fn listed(runner: &Runner, service: &Service, port: u16) -> Option<(String, String)> {
        let kind = service.service_type;
        let ask = |_| soap::get_specific_port_mapping_entry(kind, port);
        match runner.control(&runner.watch(0), service, ask).ok()? {
            entry @ Answer::Done(_) => Some((
                entry.get("NewInternalClient")?.to_owned(),
                entry.get("NewLeaseDuration").unwrap_or("?").to_owned(),
            )),
            Answer::Fault(_) => None,
        }
    }

    /// Asked short, so a renewal comes within a minute or two: the gateway may
    /// grant more.
    const LIVE: Timing = Timing {
        lease_s: 120,
        readd: Duration::from_secs(10),
        lifetime_s: 60,
        retry: Duration::from_secs(5),
        teardown: Duration::from_millis(250),
        ..TIMING
    };
    /// The live checks' port, and how often they look at the gateway's table.
    const FIRST: u16 = 24_791;
    const EVERY: Duration = Duration::from_secs(1);

    /// The real gateway, each protocol in turn: mapped and listed, moved, made
    /// again after the gateway lost it, and deleted within the bound. By hand,
    /// on a network whose gateway speaks all three and lists every protocol's
    /// mappings through UPnP; it takes a few minutes:
    /// `cargo test -p lowlat-portmap --lib live_each -- --ignored --nocapture`.
    #[test]
    #[ignore = "maps ports on this network's real gateway"]
    fn live_each_protocol_on_the_real_gateway() {
        let gateway = sys::gateway().expect("a default gateway");
        let endpoints = Endpoints::of(gateway);
        let (runner, service) = gateway_service(endpoints);
        let local = local_toward(endpoints.control).unwrap().to_string();
        let kind = service.service_type;
        for protocol in [Protocol::Pcp, Protocol::NatPmp, Protocol::Upnp] {
            let name = protocol.as_str();
            let config = Config {
                port: FIRST,
                count: 1,
                description: OURS.into(),
                gateway: None,
            };
            let mut mapper = Mapper::start_only(config, endpoints, LIVE, protocol).unwrap();
            let bound = Duration::from_secs(30);
            until_within(bound, EVERY, "mapped", mapped(&mapper, protocol));
            let status = mapper.status();
            println!(
                "live: {name} mapped {FIRST}, external {:?}:{}",
                status.address, status.port
            );
            assert_eq!(status.port, FIRST, "{name}: another external port");
            let (client, lease) = listed(&runner, &service, FIRST).expect("not listed");
            assert_eq!(client, local, "{name}: listed for another client");
            println!("live: {name} listed for {client}, lease {lease}");

            // Moved with the port: the old entry gone, the new one listed.
            mapper.set_port(FIRST + 1);
            until_within(bound, EVERY, "listed at the new port", || {
                listed(&runner, &service, FIRST + 1).is_some()
            });
            assert_eq!(
                listed(&runner, &service, FIRST),
                None,
                "{name}: the old entry stayed"
            );
            println!("live: {name} moved to {}", FIRST + 1);

            // Lost by the gateway, made again at the next renewal.
            let gone = |_| soap::delete_port_mapping(kind, FIRST + 1);
            let _ = runner.control(&runner.watch(0), &service, gone);
            assert_eq!(
                listed(&runner, &service, FIRST + 1),
                None,
                "{name}: not deleted"
            );
            let lost = Instant::now();
            until_within(Duration::from_secs(180), EVERY, "made again", || {
                listed(&runner, &service, FIRST + 1).is_some()
            });
            println!(
                "live: {name} made again {:?} after the gateway lost it",
                lost.elapsed()
            );

            // Deleted within the bound.
            let begun = Instant::now();
            mapper.stop();
            let took = begun.elapsed();
            assert!(
                took < Duration::from_secs(1),
                "{name}: the stop took {took:?}"
            );
            assert_eq!(
                listed(&runner, &service, FIRST + 1),
                None,
                "{name}: left listed"
            );
            println!("live: {name} stopped in {took:?}, nothing listed");
        }
    }

    /// The real gateway's UPnP service through a pause: something outside
    /// the process blocks this test's own traffic to the gateway from 15 s
    /// to 40 s in -- a firewall rule naming this binary -- and the mapping is
    /// kept through it, renewed after, and deleted at the stop. By hand, on
    /// any network whose gateway speaks UPnP:
    /// `cargo test -p lowlat-portmap --lib live_upnp_pause -- --ignored --nocapture`.
    #[test]
    #[ignore = "maps a port on this network's real gateway, and wants a pause made"]
    fn live_upnp_pause_on_the_real_gateway() {
        let gateway = sys::gateway().expect("a default gateway");
        let endpoints = Endpoints::of(gateway);
        let (runner, service) = gateway_service(endpoints);
        let config = Config {
            port: FIRST,
            count: 1,
            description: OURS.into(),
            gateway: None,
        };
        let mut mapper = Mapper::start_only(config, endpoints, LIVE, Protocol::Upnp).unwrap();
        until_within(
            Duration::from_secs(30),
            EVERY,
            "mapped",
            mapped(&mapper, Protocol::Upnp),
        );
        println!("live: upnp mapped {FIRST}; the pause runs from 15 s to 40 s");
        // Nothing asks the gateway from here but the mapper: the status alone
        // says whether the mapping was given up.
        let began = Instant::now();
        while began.elapsed() < Duration::from_secs(75) {
            std::thread::sleep(EVERY);
            let at = began.elapsed();
            assert_eq!(
                mapper.status().protocol,
                Protocol::Upnp,
                "upnp: lost {at:?} in"
            );
        }
        let (client, lease) = listed(&runner, &service, FIRST).expect("not listed after the pause");
        println!("live: upnp listed for {client}, lease {lease} after the pause");
        let begun = Instant::now();
        mapper.stop();
        let took = begun.elapsed();
        assert!(
            took < Duration::from_secs(1),
            "upnp: the stop took {took:?}"
        );
        assert_eq!(listed(&runner, &service, FIRST), None, "upnp: left listed");
        println!("live: upnp stopped in {took:?}, nothing listed");
    }

    /// The real gateway's UPnP service across renewals: the lease it lists
    /// stays near whole, so a gateway that answers an identical add and keeps
    /// the old lease has the mapping made again; then deleted within the
    /// bound. By hand, on any network whose gateway speaks UPnP:
    /// `cargo test -p lowlat-portmap --lib live_upnp -- --ignored --nocapture`.
    #[test]
    #[ignore = "maps a port on this network's real gateway"]
    fn live_upnp_renewals_on_the_real_gateway() {
        let gateway = sys::gateway().expect("a default gateway");
        let endpoints = Endpoints::of(gateway);
        let (runner, service) = gateway_service(endpoints);
        let config = Config {
            port: FIRST,
            count: 1,
            description: OURS.into(),
            gateway: None,
        };
        let mut mapper = Mapper::start_only(config, endpoints, LIVE, Protocol::Upnp).unwrap();
        let bound = Duration::from_secs(30);
        until_within(bound, EVERY, "mapped", mapped(&mapper, Protocol::Upnp));
        let status = mapper.status();
        println!(
            "live: upnp mapped {FIRST}, external {:?}:{}",
            status.address, status.port
        );
        // Six renewals.
        let began = Instant::now();
        let mut least = u64::MAX;
        while began.elapsed() < Duration::from_secs(65) {
            std::thread::sleep(Duration::from_secs(5));
            let (client, lease) = listed(&runner, &service, FIRST).expect("not listed");
            println!(
                "live: upnp listed for {client}, lease {lease} at {:?}",
                began.elapsed()
            );
            least = least.min(lease.parse().unwrap_or(0));
        }
        // A lease that kept running down would be near half by now.
        let whole = u64::from(LIVE.lease_s);
        assert!(least > whole * 3 / 4, "upnp: the lease ran down to {least}");
        let begun = Instant::now();
        mapper.stop();
        let took = begun.elapsed();
        assert!(
            took < Duration::from_secs(1),
            "upnp: the stop took {took:?}"
        );
        assert_eq!(listed(&runner, &service, FIRST), None, "upnp: left listed");
        println!("live: upnp stopped in {took:?}, nothing listed");
    }
}
