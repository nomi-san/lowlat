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
//! **A renewal is checked, not assumed.** A UPnP gateway may answer an
//! identical add with success and keep the old lease, so the lease is read
//! back after a renewal and, when it did not grow, the mapping is deleted and
//! made again. A PCP or NAT-PMP gateway that restarted shows it in its epoch.
//!
//! **A stop is bounded.** Every wait is cut short by it, and what is mapped is
//! deleted within a fixed bound, with nothing looked up again: every PCP or
//! NAT-PMP delete in one exchange, UPnP's a port at a time.

use core::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use core::time::Duration;
use std::io::{self, Read, Write};
use std::net::{TcpStream, UdpSocket};
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
/// An HTTP exchange's connect, which nothing can cut short, and the whole.
const CONNECT: Duration = Duration::from_millis(500);
const EXCHANGE: Duration = Duration::from_secs(2);
/// How long a blocking read waits before looking for a stop.
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
}

/// What is mapped, as the gateway states it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Status {
    pub protocol: Protocol,
    /// The gateway's external address, when it states one.
    pub address: Option<Ipv4Addr>,
    /// The first port's external port; zero when nothing is mapped.
    pub port: u16,
    /// The last refusal: the protocol that refused, and its own code.
    pub refusal: Option<(Protocol, u16)>,
}

/// The intervals the mapper keeps; the tests shorten them.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Timing {
    /// A UPnP lease, and how often the mapping is added again within it.
    pub(crate) lease_s: u32,
    pub(crate) readd: Duration,
    /// A PCP or NAT-PMP lifetime asked for; renewed at half what is granted.
    pub(crate) lifetime_s: u32,
    /// How long after finding nothing to look again.
    pub(crate) retry: Duration,
    /// The bound on deleting every mapping at a stop.
    pub(crate) teardown: Duration,
}

const TIMING: Timing = Timing {
    lease_s: 2700,
    readd: Duration::from_secs(300),
    lifetime_s: 7200,
    retry: Duration::from_secs(300),
    teardown: Duration::from_millis(250),
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
    stopping: AtomicBool,
    port: AtomicU32,
    status: Mutex<Status>,
}

impl Shared {
    fn poke(&self) {
        // Release: the request written before the change is seen with it.
        self.generation.fetch_add(1, Ordering::Release);
        wait::notify_all(&self.generation);
    }

    fn publish(&self, change: impl FnOnce(&mut Status)) {
        change(&mut self.status.lock().unwrap_or_else(PoisonError::into_inner));
    }
}

impl Mapper {
    /// Start keeping `config`'s ports mapped, on a thread of its own.
    pub fn start(config: Config) -> io::Result<Self> {
        Self::spawn(config, None, TIMING)
    }

    #[cfg(test)]
    pub(crate) fn start_with(
        config: Config,
        endpoints: Endpoints,
        timing: Timing,
    ) -> io::Result<Self> {
        Self::spawn(config, Some(endpoints), timing)
    }

    fn spawn(config: Config, endpoints: Option<Endpoints>, timing: Timing) -> io::Result<Self> {
        let shared = Arc::new(Shared {
            generation: AtomicU32::new(0),
            stopping: AtomicBool::new(false),
            port: AtomicU32::new(u32::from(config.port)),
            status: Mutex::new(Status::default()),
        });
        let runner = Runner {
            shared: Arc::clone(&shared),
            count: config.count,
            description: config.description,
            endpoints,
            timing,
            quiet: false,
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
            self.shared.poke();
        }
    }

    pub fn status(&self) -> Status {
        *self
            .shared
            .status
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Delete every mapping and end the thread, within the stop's bound.
    pub fn stop(&mut self) {
        let Some(thread) = self.thread.take() else {
            return;
        };
        self.shared.stopping.store(true, Ordering::Release);
        self.shared.poke();
        // A runner that panicked has nothing left to delete.
        let _ = thread.join();
    }
}

impl Drop for Mapper {
    fn drop(&mut self) {
        self.stop();
    }
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
    renew_at: Instant,
    /// When the lease or lifetime runs out; never, for a permanent mapping.
    expires: Option<Instant>,
    how: How,
}

#[derive(Debug)]
enum How {
    /// A nonce per port, which a renewal and a delete must carry.
    Pcp {
        nonces: Vec<Nonce>,
        epoch: Epoch,
    },
    NatPmp {
        epoch: Epoch,
    },
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
    Lost(&'static str),
    Interrupted,
}

/// What cuts a wait short: a request to the thread, or a deadline.
struct Watch<'a> {
    shared: &'a Shared,
    seen: Option<u32>,
    deadline: Option<Instant>,
}

impl Watch<'_> {
    fn interrupted(&self) -> bool {
        self.seen
            .is_some_and(|seen| self.shared.generation.load(Ordering::Acquire) != seen)
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
    quiet: bool,
}

impl Runner {
    fn run(mut self) {
        let mut mapped: Option<Mapped> = None;
        loop {
            let seen = self.shared.generation.load(Ordering::Acquire);
            if self.shared.stopping.load(Ordering::Acquire) {
                break;
            }
            let first = u16::try_from(self.shared.port.load(Ordering::Acquire)).unwrap_or(0);
            if let Some(moved) = mapped.take_if(|current| current.first != first) {
                self.delete(&moved);
            }
            let now = Instant::now();
            let next = match mapped.take() {
                None => match self.map(first, seen) {
                    Some(made) => {
                        let at = made.renew_at;
                        mapped = Some(made);
                        at
                    }
                    None => now + self.timing.retry,
                },
                Some(mut current) if now >= current.renew_at => {
                    match self.renew(&mut current, seen) {
                        Renewal::Renewed | Renewal::Interrupted => {
                            let at = current.renew_at;
                            mapped = Some(current);
                            at
                        }
                        Renewal::Lost(why) => {
                            log_warn!(
                                "portmap: lost, protocol={} port={} reason={}",
                                current.protocol.as_str(),
                                current.first,
                                why
                            );
                            self.shared.publish(|status| {
                                *status = Status {
                                    refusal: status.refusal,
                                    ..Status::default()
                                };
                            });
                            // Looked for again at once.
                            now
                        }
                    }
                }
                Some(current) => {
                    let at = current.renew_at;
                    mapped = Some(current);
                    at
                }
            };
            let left = next.saturating_duration_since(Instant::now());
            if !left.is_zero() {
                wait::wait(&self.shared.generation, seen, left);
            }
        }
        if let Some(current) = mapped {
            self.delete(&current);
        }
    }

    fn watch(&self, seen: u32) -> Watch<'_> {
        Watch {
            shared: &self.shared,
            seen: Some(seen),
            deadline: None,
        }
    }

    /// A refusal, logged when it is not the last one again: a gateway that
    /// refuses a port refuses it at every look.
    fn refused(&self, protocol: Protocol, code: u16, port: u16) {
        let mut again = false;
        self.shared.publish(|status| {
            again = status.refusal == Some((protocol, code));
            status.refusal = Some((protocol, code));
        });
        if !again {
            log_info!(
                "portmap: refused, protocol={} port={} code={}",
                protocol.as_str(),
                port,
                code
            );
        }
    }

    fn nothing(&mut self, gateway: Option<Ipv4Addr>) {
        if !self.quiet {
            match gateway {
                Some(gateway) => log_info!("portmap: nothing mapped, gateway={gateway}"),
                None => log_info!("portmap: nothing mapped, gateway=none"),
            }
            self.quiet = true;
        }
    }

    /// The ladder: the first protocol the gateway answers maps the ports.
    fn map(&mut self, first: u16, seen: u32) -> Option<Mapped> {
        if first < MIN_PORT || self.count == 0 {
            if !self.quiet {
                log_info!("portmap: nothing mapped, port={first} reason=reserved");
                self.quiet = true;
            }
            return None;
        }
        // The range ends at the last port there is.
        let count = self.count.min(u16::MAX - first + 1);
        let Some(endpoints) = self.endpoints.or_else(|| sys::gateway().map(Endpoints::of)) else {
            self.nothing(None);
            return None;
        };
        let Some(local) = local_toward(endpoints.control) else {
            self.nothing(Some(endpoints.gateway));
            return None;
        };
        let made = self
            .pcp(endpoints, local, first, count, seen)
            .or_else(|| self.natpmp(endpoints, local, first, count, seen))
            .or_else(|| self.upnp(endpoints, local, first, count, seen));
        match &made {
            Some(made) => {
                self.quiet = false;
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
            None => self.nothing(Some(endpoints.gateway)),
        }
        made
    }

    fn status(&self) -> Status {
        *self
            .shared
            .status
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn pcp(
        &self,
        endpoints: Endpoints,
        local: Ipv4Addr,
        first: u16,
        count: u16,
        seen: u32,
    ) -> Option<Mapped> {
        let watch = self.watch(seen);
        let mut nonces = Vec::new();
        let mut granted = LONGEST_LIFETIME_S;
        let mut epoch = None;
        for port in (first..).take(usize::from(count)) {
            let mut nonce = Nonce([0; 12]);
            lowlat_crypto::fill(&mut nonce.0).ok()?;
            let request =
                pcp::map_request(IpAddr::V4(local), nonce, port, port, self.timing.lifetime_s);
            let answer = exchange(&watch, endpoints.control, &request, &PROBE, |data| {
                pcp_answer(data, nonce)
            });
            let reply = match answer {
                Some(Ok(reply)) if reply.result == pcp::ResultCode::SUCCESS => reply,
                Some(Ok(reply)) => {
                    self.refused(Protocol::Pcp, u16::from(reply.result.0), port);
                    break;
                }
                // Another version, or nothing that answers: not PCP.
                Some(Err(_)) | None => break,
            };
            let Some(map) = reply.map else {
                break;
            };
            if nonces.is_empty() {
                let address = match map.external_address {
                    IpAddr::V4(address) => Some(address),
                    IpAddr::V6(_) => None,
                };
                self.shared.publish(|status| {
                    status.protocol = Protocol::Pcp;
                    status.address = address;
                    status.port = map.external_port;
                });
            }
            granted = granted.min(reply.lifetime_s);
            epoch = Some(Epoch {
                server_s: reply.epoch_s,
                at: Instant::now(),
            });
            nonces.push(nonce);
        }
        let epoch = epoch?;
        let count = u16::try_from(nonces.len()).ok()?;
        let now = Instant::now();
        Some(Mapped {
            protocol: Protocol::Pcp,
            endpoints,
            local,
            first,
            count,
            renew_at: now + renew_after(granted),
            expires: Some(now + Duration::from_secs(u64::from(granted))),
            how: How::Pcp { nonces, epoch },
        })
    }

    fn natpmp(
        &self,
        endpoints: Endpoints,
        local: Ipv4Addr,
        first: u16,
        count: u16,
        seen: u32,
    ) -> Option<Mapped> {
        let watch = self.watch(seen);
        // The external address first: a mapping's answer does not carry it.
        let request = natpmp::address_request();
        let (epoch_s, address) = match exchange(&watch, endpoints.control, &request, &PROBE, |d| {
            natpmp_answer(d, 0, None)
        })? {
            natpmp::Reply::Address { epoch_s, address } => (epoch_s, address),
            natpmp::Reply::Refused { result, .. } => {
                self.refused(Protocol::NatPmp, result.0, first);
                return None;
            }
            natpmp::Reply::Map { .. } => return None,
        };
        // A gateway with no external address maps nothing that works.
        if address.is_unspecified() || address.is_loopback() {
            return None;
        }
        let mut mapped: u16 = 0;
        let mut granted = LONGEST_LIFETIME_S;
        let mut external_port = 0;
        for port in (first..).take(usize::from(count)) {
            let request = natpmp::map_request(port, port, self.timing.lifetime_s);
            match exchange(&watch, endpoints.control, &request, &PROBE, |d| {
                natpmp_answer(d, 1, Some(port))
            }) {
                Some(natpmp::Reply::Map {
                    external_port: external,
                    lifetime_s,
                    ..
                }) => {
                    if mapped == 0 {
                        external_port = external;
                    }
                    granted = granted.min(lifetime_s);
                    mapped += 1;
                }
                Some(natpmp::Reply::Refused { result, .. }) => {
                    self.refused(Protocol::NatPmp, result.0, port);
                    break;
                }
                _ => break,
            }
        }
        if mapped == 0 {
            return None;
        }
        self.shared.publish(|status| {
            status.protocol = Protocol::NatPmp;
            status.address = Some(address);
            status.port = external_port;
        });
        let now = Instant::now();
        Some(Mapped {
            protocol: Protocol::NatPmp,
            endpoints,
            local,
            first,
            count: mapped,
            renew_at: now + renew_after(granted),
            expires: Some(now + Duration::from_secs(u64::from(granted))),
            how: How::NatPmp {
                epoch: Epoch {
                    server_s: epoch_s,
                    at: Instant::now(),
                },
            },
        })
    }

    fn upnp(
        &self,
        endpoints: Endpoints,
        local: Ipv4Addr,
        first: u16,
        count: u16,
        seen: u32,
    ) -> Option<Mapped> {
        let watch = self.watch(seen);
        let location = search(&watch, endpoints, local)?;
        let (response, ()) = http_exchange(&watch, &location, http::DESCRIPTION_CAP, |_| {
            (
                http::request(Method::Get, location.addr, &location.path, &[], &[]),
                (),
            )
        })?;
        if response.status != 200 {
            return None;
        }
        let found = desc::parse(&response.body, &location).ok()?;
        for service in found.connections() {
            // Only the gateway's own address is asked anything.
            if *service.control.addr.ip() != endpoints.gateway {
                continue;
            }
            // The external address: stated, not stated, or one that cannot
            // be. Not stating one is a gateway behind a reserved address,
            // which maps all the same.
            let address = match self.control(&watch, service, |_| {
                soap::get_external_ip_address(service.service_type)
            }) {
                Some(answer @ Answer::Done(_)) => {
                    let stated = answer
                        .get("NewExternalIPAddress")
                        .and_then(|address| address.parse::<Ipv4Addr>().ok());
                    if stated.is_some_and(|a| a.is_unspecified() || a.is_loopback()) {
                        continue;
                    }
                    stated
                }
                Some(Answer::Fault(_)) => continue,
                None => return None,
            };
            let mut permanent = false;
            let mut mapped: u16 = 0;
            for port in (first..).take(usize::from(count)) {
                match self.upnp_add(&watch, service, port, &mut permanent) {
                    Ok(()) => mapped += 1,
                    Err(refusal) => {
                        if let Some(code) = refusal {
                            self.refused(Protocol::Upnp, code, port);
                        }
                        break;
                    }
                }
            }
            if mapped == 0 {
                return None;
            }
            self.shared.publish(|status| {
                status.protocol = Protocol::Upnp;
                status.address = address;
                status.port = first;
            });
            let now = Instant::now();
            return Some(Mapped {
                protocol: Protocol::Upnp,
                endpoints,
                local,
                first,
                count: mapped,
                renew_at: now + self.timing.readd,
                expires: (!permanent)
                    .then(|| now + Duration::from_secs(u64::from(self.timing.lease_s))),
                how: How::Upnp {
                    service: service.clone(),
                    permanent,
                    remade: false,
                },
            });
        }
        None
    }

    /// Map `port` on `service`: a leftover of this side's own on it is
    /// deleted first, anything else's is left alone, and a gateway that takes
    /// only permanent mappings gets one. The error carries the gateway's code,
    /// if it gave one.
    fn upnp_add(
        &self,
        watch: &Watch<'_>,
        service: &Service,
        port: u16,
        permanent: &mut bool,
    ) -> Result<(), Option<u16>> {
        let kind = service.service_type;
        match self.control(watch, service, |_| {
            soap::get_specific_port_mapping_entry(kind, port)
        }) {
            Some(entry @ Answer::Done(_)) => {
                if entry.get("NewPortMappingDescription") != Some(self.description.as_str()) {
                    return Err(Some(FaultCode::CONFLICT.0));
                }
                self.control(watch, service, |_| soap::delete_port_mapping(kind, port));
            }
            // Nothing there, or a gateway that cannot say: the add will tell.
            Some(Answer::Fault(_)) => {}
            None => return Err(None),
        }
        loop {
            let lease = if *permanent { 0 } else { self.timing.lease_s };
            match self.control(watch, service, |local| {
                soap::add_port_mapping(kind, port, local, &self.description, lease)
            }) {
                Some(Answer::Done(_)) => return Ok(()),
                // A lease refused: some gateways take permanent mappings
                // alone, and say so in one of two ways.
                Some(Answer::Fault(code))
                    if !*permanent
                        && (code == FaultCode::ONLY_PERMANENT_LEASES
                            || code == FaultCode::INVALID_ARGS) =>
                {
                    *permanent = true;
                }
                Some(Answer::Fault(code)) => return Err(Some(code.0)),
                None => return Err(None),
            }
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
    ) -> Option<Answer> {
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
        soap::parse(name, response.status, &response.body).ok()
    }

    fn renew(&self, mapped: &mut Mapped, seen: u32) -> Renewal {
        let watch = self.watch(seen);
        let now = Instant::now();
        if let Some(late) = mapped
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
        let How::Pcp { nonces, epoch } = &mut mapped.how else {
            return Renewal::Lost("protocol");
        };
        let mut granted = LONGEST_LIFETIME_S;
        let mut restarted = false;
        for (port, nonce) in (mapped.first..).zip(nonces.iter().copied()) {
            let request = pcp::map_request(
                IpAddr::V4(mapped.local),
                nonce,
                port,
                port,
                self.timing.lifetime_s,
            );
            let reply = match exchange(watch, mapped.endpoints.control, &request, &RENEW, |data| {
                pcp_answer(data, nonce)
            }) {
                Some(Ok(reply)) if reply.result == pcp::ResultCode::SUCCESS => reply,
                Some(Ok(reply)) => {
                    self.refused(Protocol::Pcp, u16::from(reply.result.0), port);
                    return Renewal::Lost("refused");
                }
                Some(Err(_)) => return Renewal::Lost("version"),
                None if watch.interrupted() => return Renewal::Interrupted,
                None => return Renewal::Lost("no-answer"),
            };
            let now = Instant::now();
            restarted |= !continuous(*epoch, reply.epoch_s, now);
            *epoch = Epoch {
                server_s: reply.epoch_s,
                at: now,
            };
            granted = granted.min(reply.lifetime_s);
        }
        if restarted {
            log_warn!(
                "portmap: lost, protocol=pcp port={} reason=gateway-restarted, mapped again",
                mapped.first
            );
        }
        let now = Instant::now();
        mapped.renew_at = now + renew_after(granted);
        mapped.expires = Some(now + Duration::from_secs(u64::from(granted)));
        Renewal::Renewed
    }

    fn natpmp_renew(&self, mapped: &mut Mapped, watch: &Watch<'_>) -> Renewal {
        let How::NatPmp { epoch } = &mut mapped.how else {
            return Renewal::Lost("protocol");
        };
        let request = natpmp::address_request();
        let (epoch_s, address) =
            match exchange(watch, mapped.endpoints.control, &request, &RENEW, |d| {
                natpmp_answer(d, 0, None)
            }) {
                Some(natpmp::Reply::Address { epoch_s, address }) => (epoch_s, address),
                Some(natpmp::Reply::Refused { result, .. }) => {
                    self.refused(Protocol::NatPmp, result.0, mapped.first);
                    return Renewal::Lost("refused");
                }
                Some(natpmp::Reply::Map { .. }) => return Renewal::Lost("answer"),
                None if watch.interrupted() => return Renewal::Interrupted,
                None => return Renewal::Lost("no-answer"),
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
        self.shared.publish(|status| status.address = Some(address));
        let mut granted = LONGEST_LIFETIME_S;
        for port in (mapped.first..).take(usize::from(mapped.count)) {
            let request = natpmp::map_request(port, port, self.timing.lifetime_s);
            match exchange(watch, mapped.endpoints.control, &request, &RENEW, |d| {
                natpmp_answer(d, 1, Some(port))
            }) {
                Some(natpmp::Reply::Map { lifetime_s, .. }) => granted = granted.min(lifetime_s),
                Some(natpmp::Reply::Refused { result, .. }) => {
                    self.refused(Protocol::NatPmp, result.0, port);
                    return Renewal::Lost("refused");
                }
                Some(natpmp::Reply::Address { .. }) => return Renewal::Lost("answer"),
                None if watch.interrupted() => return Renewal::Interrupted,
                None => return Renewal::Lost("no-answer"),
            }
        }
        let now = Instant::now();
        mapped.renew_at = now + renew_after(granted);
        mapped.expires = Some(now + Duration::from_secs(u64::from(granted)));
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
            Some(answer @ Answer::Done(_)) => {
                let stated = answer
                    .get("NewExternalIPAddress")
                    .and_then(|address| address.parse::<Ipv4Addr>().ok());
                self.shared.publish(|status| status.address = stated);
            }
            Some(Answer::Fault(_)) => {}
            None if watch.interrupted() => return Renewal::Interrupted,
            None => return Renewal::Lost("no-answer"),
        }
        let lease_s = if *permanent { 0 } else { self.timing.lease_s };
        // A lease read back at least this long grew with the add.
        let slack = (self.timing.readd / 2).as_secs().max(1);
        let grown = u64::from(lease_s).saturating_sub(slack);
        for port in (mapped.first..).take(usize::from(mapped.count)) {
            let add = |local| soap::add_port_mapping(kind, port, local, &self.description, lease_s);
            match self.control(watch, service, add) {
                Some(Answer::Done(_)) => {}
                Some(Answer::Fault(code)) => {
                    self.refused(Protocol::Upnp, code.0, port);
                    return Renewal::Lost("refused");
                }
                None if watch.interrupted() => return Renewal::Interrupted,
                None => return Renewal::Lost("no-answer"),
            }
            if *permanent {
                continue;
            }
            // A gateway may answer an identical add and keep the old lease.
            let left = match self.control(watch, service, |_| {
                soap::get_specific_port_mapping_entry(kind, port)
            }) {
                Some(entry @ Answer::Done(_)) => entry
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
                self.control(watch, service, |_| soap::delete_port_mapping(kind, port));
                match self.control(watch, service, add) {
                    Some(Answer::Done(_)) => {}
                    Some(Answer::Fault(code)) => {
                        self.refused(Protocol::Upnp, code.0, port);
                        return Renewal::Lost("refused");
                    }
                    None if watch.interrupted() => return Renewal::Interrupted,
                    None => return Renewal::Lost("no-answer"),
                }
            }
        }
        let now = Instant::now();
        mapped.renew_at = now + self.timing.readd;
        mapped.expires = (!*permanent).then(|| now + Duration::from_secs(u64::from(lease_s)));
        Renewal::Renewed
    }

    /// Delete every mapping within the stop's bound, cut short by nothing
    /// else: PCP's and NAT-PMP's in one exchange, UPnP's a port at a time.
    fn delete(&self, mapped: &Mapped) {
        let watch = Watch {
            shared: &self.shared,
            seen: None,
            deadline: Some(Instant::now() + self.timing.teardown),
        };
        let ports = (mapped.first..).take(usize::from(mapped.count));
        match &mapped.how {
            How::Pcp { nonces, .. } => {
                let requests: Vec<_> = ports
                    .zip(nonces.iter().copied())
                    .map(|(port, nonce)| {
                        let request = pcp::delete_request(IpAddr::V4(mapped.local), nonce, port);
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
                    self.control(&watch, service, |_| {
                        soap::delete_port_mapping(service.service_type, port)
                    });
                }
            }
        }
        log_info!(
            "portmap: deleted, protocol={} port={} count={}",
            mapped.protocol.as_str(),
            mapped.first,
            mapped.count
        );
        self.shared.publish(|status| {
            *status = Status {
                refusal: status.refusal,
                ..Status::default()
            };
        });
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
/// many clients behind one gateway do not renew at once.
fn renew_after(granted_s: u32) -> Duration {
    let granted = Duration::from_secs(u64::from(granted_s.min(LONGEST_LIFETIME_S)));
    let mut draw = [0u8; 2];
    let jitter = match lowlat_crypto::fill(&mut draw) {
        Ok(()) => (granted / 8).mul_f64(f64::from(u16::from_le_bytes(draw)) / f64::from(u16::MAX)),
        Err(_) => Duration::ZERO,
    };
    (granted / 2 + jitter).max(Duration::from_secs(1))
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

/// One request to the gateway's port, sent again after each of `waits` in
/// silence; the first answer `take` takes. A refusal of the datagram itself
/// is nothing listening.
fn exchange<T>(
    watch: &Watch<'_>,
    to: SocketAddrV4,
    request: &[u8],
    waits: &[Duration],
    mut take: impl FnMut(&[u8]) -> Option<T>,
) -> Option<T> {
    let socket = connected(to)?;
    let mut buf = [0u8; pcp::MAX_LEN];
    for wait in waits {
        socket.send(request).ok()?;
        let until = Instant::now() + *wait;
        while let Some(slice) = watch.slice(until) {
            socket.set_read_timeout(Some(slice)).ok()?;
            match socket.recv(&mut buf) {
                Ok(n) => {
                    if let Some(found) = take(buf.get(..n).unwrap_or_default()) {
                        return Some(found);
                    }
                }
                Err(error) if is_timeout(&error) => {}
                Err(_) => return None,
            }
        }
        if watch.interrupted() {
            return None;
        }
    }
    None
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
    let mut buf = [0u8; pcp::MAX_LEN];
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
                Err(error) if is_timeout(&error) => {}
                Err(_) => return,
            }
        }
    }
}

/// One HTTP exchange with `url`'s host: a fresh connection, a request built
/// once its own address is known, the response read to its end.
fn http_exchange<T>(
    watch: &Watch<'_>,
    url: &Url,
    cap: usize,
    make: impl FnOnce(Ipv4Addr) -> (Vec<u8>, T),
) -> Option<(http::Response, T)> {
    let until = Instant::now() + EXCHANGE;
    // The connect is the one wait nothing cuts short, so it is bounded on
    // its own.
    let connect = watch.slice(until).map(|_| {
        let left = watch
            .deadline
            .map_or(until, |deadline| deadline.min(until))
            .saturating_duration_since(Instant::now());
        left.min(CONNECT).max(Duration::from_millis(1))
    })?;
    let mut stream = TcpStream::connect_timeout(&SocketAddr::V4(url.addr), connect).ok()?;
    let SocketAddr::V4(local) = stream.local_addr().ok()? else {
        return None;
    };
    let (request, tag) = make(*local.ip());
    stream.set_write_timeout(Some(CONNECT)).ok()?;
    stream.write_all(&request).ok()?;
    let mut reader = http::Reader::new(cap);
    let mut buf = [0u8; 4096];
    while let Some(slice) = watch.slice(until) {
        stream.set_read_timeout(Some(slice)).ok()?;
        match stream.read(&mut buf) {
            Ok(0) => return reader.finish().ok().map(|response| (response, tag)),
            Ok(n) => match reader.push(buf.get(..n).unwrap_or_default()) {
                Ok(Some(response)) => return Some((response, tag)),
                Ok(None) => {}
                Err(_) => return None,
            },
            Err(error) if is_timeout(&error) => {}
            Err(_) => return None,
        }
    }
    None
}

/// Search for the gateway's description: the gateway itself first, then the
/// group, out of the interface toward the gateway. The first answer that
/// places its description on the gateway's own address is taken.
fn search(watch: &Watch<'_>, endpoints: Endpoints, local: Ipv4Addr) -> Option<Url> {
    let socket = UdpSocket::bind(SocketAddrV4::new(local, 0)).ok()?;
    // On a machine with several interfaces the group alone names none.
    let _ = sys::multicast_from(&socket, local);
    let _ = socket.set_multicast_ttl_v4(2);
    let direct = ssdp::search(endpoints.search, ssdp::GATEWAY_1, SEARCH_DELAY_S);
    let _ = socket.send_to(direct.as_bytes(), endpoints.search);
    let grouped = ssdp::search(ssdp::GROUP, ssdp::GATEWAY_1, SEARCH_DELAY_S);
    let _ = socket.send_to(grouped.as_bytes(), endpoints.group);
    let until = Instant::now() + SEARCH;
    let mut buf = [0u8; ssdp::MAX_LEN];
    while let Some(slice) = watch.slice(until) {
        socket.set_read_timeout(Some(slice)).ok()?;
        let Ok((n, _)) = socket.recv_from(&mut buf) else {
            // A timeout, or the gateway's own port refusing the direct
            // search: the group's answer may still come.
            continue;
        };
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
    use crate::fake::{Behaviour, EXTERNAL, Elsewhere, Entry, Fake};

    /// The intervals, short enough to see renewals within a test.
    const FAST: Timing = Timing {
        lease_s: 6,
        readd: Duration::from_secs(2),
        lifetime_s: 4,
        retry: Duration::from_millis(500),
        teardown: Duration::from_millis(250),
    };
    const PORT: u16 = 24137;
    const OURS: &str = "lowlat-test";

    fn start(fake: &Fake, port: u16, count: u16) -> Mapper {
        let config = Config {
            port,
            count,
            description: OURS.into(),
        };
        Mapper::start_with(config, fake.endpoints, FAST).unwrap()
    }

    /// Wait for `holds`, failing with `what` after a generous bound.
    fn until(what: &str, mut holds: impl FnMut() -> bool) {
        let end = Instant::now() + Duration::from_secs(10);
        while !holds() {
            assert!(Instant::now() < end, "never: {what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Stopped within the bound, every mapping deleted from the gateway.
    fn stopped(mut mapper: Mapper, fake: &Fake) {
        let begun = Instant::now();
        mapper.stop();
        let took = begun.elapsed();
        // One read's slice and a delete on loopback; an exchange waited out
        // would take seconds.
        assert!(took < Duration::from_secs(1), "the stop took {took:?}");
        assert!(fake.table().is_empty(), "left behind: {:?}", fake.table());
        assert_eq!(mapper.status().protocol, Protocol::None);
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
        for behaviour in [Behaviour::default(), Behaviour::upnp_only()] {
            let fake = Fake::start(behaviour);
            let mapper = start(&fake, PORT, 1);
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
            let after = renew_after(7200);
            assert!(
                after >= Duration::from_secs(3600) && after <= Duration::from_secs(4500),
                "{after:?}"
            );
        }
        assert_eq!(renew_after(0), Duration::from_secs(1));
        // An absurd lifetime is read as a day.
        assert!(renew_after(u32::MAX) <= Duration::from_secs(54_000));
    }
}
