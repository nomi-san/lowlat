//! A gateway on loopback that speaks all three protocols, for tests: its
//! behaviours are switched per test, its table read and changed, every action
//! counted, and it can restart or fall silent. Test-only: nothing in a
//! shipping build makes one.

// Test support: a fixture that cannot be built is a broken test, not input.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::cast_possible_truncation,
    missing_debug_implementations
)]

use core::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4};
use core::sync::atomic::{AtomicBool, Ordering};
use core::time::Duration;
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Instant;

use crate::mapper::{Endpoints, FAST, Gateway};

/// The address the fake states as its outside.
pub const EXTERNAL: Ipv4Addr = Ipv4Addr::new(203, 0, 113, 7);
const SERVICE: &str = "urn:schemas-upnp-org:service:WANIPConnection:1";
/// The second service's kind, which a mapper asks before the first's.
const SECOND_SERVICE: &str = "urn:schemas-upnp-org:service:WANIPConnection:2";
/// A second loopback address, standing for a host that is not the gateway.
const ELSEWHERE: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 2);
/// How long a request applied and never answered holds its connection.
const HOLD: Duration = Duration::from_secs(3);

fn description(control: &str, second: bool) -> String {
    let second = if second {
        format!(
            "<service><serviceType>{SECOND_SERVICE}</serviceType>\
             <controlURL>/ctl/Second</controlURL></service>"
        )
    } else {
        String::new()
    };
    format!(
        "<?xml version=\"1.0\"?><root xmlns=\"urn:schemas-upnp-org:device-1-0\">\
         <device><deviceType>urn:schemas-upnp-org:device:InternetGatewayDevice:1</deviceType>\
         <deviceList><device><deviceType>urn:schemas-upnp-org:device:WANDevice:1</deviceType>\
         <deviceList><device><deviceType>urn:schemas-upnp-org:device:WANConnectionDevice:1</deviceType>\
         <serviceList>{second}<service><serviceType>{SERVICE}</serviceType>\
         <controlURL>{control}</controlURL></service></serviceList></device></deviceList></device>\
         </deviceList></device></root>"
    )
}

/// What the fake places on a host that is not the gateway.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Elsewhere {
    /// The description, by the search's answer.
    Location,
    /// The control service, by the description.
    Control,
}

/// A second connection service, which a mapper asks first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Second {
    /// Answers every action with a plain 404, no fault in it.
    Broken,
    /// A connection that is down: it says so, and states no address.
    Down,
}

#[derive(Debug, Clone)]
pub struct Behaviour {
    pub pcp: bool,
    pub natpmp: bool,
    pub upnp: bool,
    /// A timed UPnP lease refused with this code: only permanent mappings.
    pub permanent_only: Option<u16>,
    /// An identical UPnP add answered with success, the old lease kept.
    pub keeps_old_lease: bool,
    /// The external address UPnP states.
    pub stated: &'static str,
    /// How long each HTTP answer takes.
    pub slow: Duration,
    /// A part of the device placed on another host, which serves it there.
    pub elsewhere: Option<Elsewhere>,
    /// One request carried out and never answered: `"MAP"`, a PCP mapping,
    /// or a UPnP action by its name. Its connection is held open a while.
    pub unanswered: Option<&'static str>,
    /// A second connection service, listed before the working one.
    pub second: Option<Second>,
    /// How many search datagrams are passed over before one is answered.
    pub searches_ignored: u32,
}

impl Default for Behaviour {
    fn default() -> Self {
        Self {
            pcp: true,
            natpmp: true,
            upnp: true,
            permanent_only: None,
            keeps_old_lease: false,
            stated: "203.0.113.7",
            slow: Duration::ZERO,
            elsewhere: None,
            unanswered: None,
            second: None,
            searches_ignored: 0,
        }
    }
}

impl Behaviour {
    pub fn upnp_only() -> Self {
        Self {
            pcp: false,
            natpmp: false,
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub via: &'static str,
    pub client: Ipv4Addr,
    pub description: String,
    /// When it lapses; never, for a permanent one.
    pub expires: Option<Instant>,
    pub nonce: Option<[u8; 12]>,
}

#[derive(Debug)]
struct State {
    behaviour: Behaviour,
    table: BTreeMap<u16, Entry>,
    started: Instant,
    /// What was asked, by name and port; port zero for what names none.
    /// UPnP's actions by their own names, a delete by the other two as
    /// `pcp-delete` and `natpmp-delete`, the description as `GET`, a
    /// search as `M-SEARCH`, and anything asked of the other host as
    /// `elsewhere`.
    calls: BTreeMap<(String, u16), u32>,
    description: String,
    /// Nothing answered until then.
    muted_until: Option<Instant>,
    /// Added to every external port granted from a restart on.
    shift: u16,
    /// Search datagrams passed over so far.
    searches_seen: u32,
    /// The external port a PCP mapping of each internal port last suggested.
    suggested: BTreeMap<u16, u16>,
}

impl State {
    fn count(&mut self, what: &str, port: u16) {
        *self.calls.entry((what.to_string(), port)).or_default() += 1;
    }

    fn epoch(&self) -> u32 {
        u32::try_from(self.started.elapsed().as_secs()).unwrap()
    }

    fn purge(&mut self) {
        let now = Instant::now();
        self.table
            .retain(|_, entry| entry.expires.is_none_or(|at| at > now));
    }

    fn muted(&self) -> bool {
        self.muted_until.is_some_and(|until| Instant::now() < until)
    }
}

pub struct Fake {
    state: Arc<Mutex<State>>,
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
    pub(crate) endpoints: Endpoints,
}

impl Fake {
    pub fn start(behaviour: Behaviour) -> Self {
        let state = Arc::new(Mutex::new(State {
            behaviour: behaviour.clone(),
            table: BTreeMap::new(),
            started: Instant::now(),
            calls: BTreeMap::new(),
            description: String::new(),
            muted_until: None,
            shift: 0,
            searches_seen: 0,
            suggested: BTreeMap::new(),
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let mut threads = Vec::new();
        let loopback = Ipv4Addr::LOCALHOST;

        // The other host serves the same device, so that only the address
        // tells the two apart.
        let mut elsewhere = None;
        if behaviour.elsewhere.is_some() {
            let listener = TcpListener::bind((ELSEWHERE, 0)).unwrap();
            listener.set_nonblocking(true).unwrap();
            elsewhere = Some(listener.local_addr().unwrap().port());
            let (state, stop) = (Arc::clone(&state), Arc::clone(&stop));
            threads.push(std::thread::spawn(move || {
                serve_http(&listener, state, stop, true);
            }));
        }

        // PCP and NAT-PMP; or, for neither, a port nothing listens on, which
        // refuses at once.
        let socket = UdpSocket::bind((loopback, 0)).unwrap();
        let control = v4(socket.local_addr().unwrap());
        if behaviour.pcp || behaviour.natpmp {
            socket
                .set_read_timeout(Some(Duration::from_millis(20)))
                .unwrap();
            let (state, stop) = (Arc::clone(&state), Arc::clone(&stop));
            threads.push(std::thread::spawn(move || {
                serve_control(&socket, &state, &stop);
            }));
        }

        let listener = TcpListener::bind((loopback, 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let http_port = listener.local_addr().unwrap().port();
        let (location, control_url) = match (behaviour.elsewhere, elsewhere) {
            (Some(Elsewhere::Location), Some(port)) => (
                format!("http://{ELSEWHERE}:{port}/rootDesc.xml"),
                "/ctl/IPConn".to_string(),
            ),
            (Some(Elsewhere::Control), Some(port)) => (
                format!("http://{loopback}:{http_port}/rootDesc.xml"),
                format!("http://{ELSEWHERE}:{port}/ctl/IPConn"),
            ),
            _ => (
                format!("http://{loopback}:{http_port}/rootDesc.xml"),
                "/ctl/IPConn".to_string(),
            ),
        };
        state.lock().unwrap().description = description(&control_url, behaviour.second.is_some());
        {
            let (state, stop) = (Arc::clone(&state), Arc::clone(&stop));
            threads.push(std::thread::spawn(move || {
                serve_http(&listener, state, stop, false);
            }));
        }

        let socket = UdpSocket::bind((loopback, 0)).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_millis(20)))
            .unwrap();
        let search = v4(socket.local_addr().unwrap());
        {
            let (state, stop) = (Arc::clone(&state), Arc::clone(&stop));
            threads.push(std::thread::spawn(move || {
                serve_search(&socket, &location, &state, &stop);
            }));
        }

        Self {
            state,
            stop,
            threads,
            endpoints: Endpoints {
                gateway: loopback,
                control,
                search,
                group: search,
            },
        }
    }

    /// This fake as a mapper's gateway, with intervals short enough to see
    /// renewals within a test.
    pub fn gateway(&self) -> Gateway {
        Gateway {
            endpoints: self.endpoints,
            timing: FAST,
        }
    }

    /// As [`Fake::gateway`], a PCP or NAT-PMP mapping asking for
    /// `lifetime_s`, so that nothing but a look renews it within a test.
    pub fn gateway_with_lifetime(&self, lifetime_s: u32) -> Gateway {
        Gateway {
            endpoints: self.endpoints,
            timing: crate::mapper::Timing { lifetime_s, ..FAST },
        }
    }

    /// The live mappings.
    pub fn table(&self) -> BTreeMap<u16, Entry> {
        let mut state = self.state.lock().unwrap();
        state.purge();
        state.table.clone()
    }

    pub fn insert(&self, port: u16, entry: Entry) {
        self.state.lock().unwrap().table.insert(port, entry);
    }

    /// Forget every mapping and begin a new epoch, as a gateway that
    /// restarted.
    pub fn restart(&self) {
        self.restart_shifted(0);
    }

    /// Restart, and from then on grant every PCP and NAT-PMP mapping the
    /// external port `shift` above the one asked for.
    pub fn restart_shifted(&self, shift: u16) {
        let mut state = self.state.lock().unwrap();
        state.table.clear();
        state.started = Instant::now();
        state.shift = shift;
    }

    /// Answer nothing for `span`: no datagram, and every connection closed
    /// unanswered. What is asked meanwhile is not carried out.
    pub fn mute(&self, span: Duration) {
        self.state.lock().unwrap().muted_until = Some(Instant::now() + span);
    }

    /// How often `what` was asked, for `port` when it names one.
    pub fn calls(&self, action: &str, port: u16) -> u32 {
        let state = self.state.lock().unwrap();
        state
            .calls
            .get(&(action.to_string(), port))
            .copied()
            .unwrap_or(0)
    }

    /// The external port the last PCP mapping of `port` suggested.
    pub fn suggested(&self, port: u16) -> Option<u16> {
        self.state.lock().unwrap().suggested.get(&port).copied()
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

fn v4(address: SocketAddr) -> SocketAddrV4 {
    match address {
        SocketAddr::V4(address) => address,
        SocketAddr::V6(_) => unreachable!("bound to an IPv4 address"),
    }
}

fn serve_control(socket: &UdpSocket, state: &Mutex<State>, stop: &AtomicBool) {
    let mut buf = [0u8; 1100];
    while !stop.load(Ordering::Acquire) {
        let Ok((n, from)) = socket.recv_from(&mut buf) else {
            continue;
        };
        let SocketAddr::V4(sender) = from else {
            continue;
        };
        let request = &buf[..n];
        let mut state = state.lock().unwrap();
        if state.muted() {
            continue;
        }
        state.purge();
        let answer = match request.first() {
            Some(2) if state.behaviour.pcp => pcp(&mut state, request),
            // A gateway that speaks NAT-PMP alone refuses the version.
            Some(2) if state.behaviour.natpmp => {
                let mut answer = vec![0, 0x80 | (request[1] & 0x7f), 0, 1];
                answer.extend_from_slice(&state.epoch().to_be_bytes());
                Some(answer)
            }
            Some(0) if state.behaviour.natpmp => natpmp(&mut state, request, *sender.ip()),
            _ => None,
        };
        // A mapping carried out and never answered.
        let mapping = request.first() == Some(&2)
            && request.get(1) == Some(&1)
            && request.get(4..8) != Some(&[0, 0, 0, 0][..]);
        let withheld = state.behaviour.unanswered == Some("MAP") && mapping;
        drop(state);
        if let Some(answer) = answer
            && !withheld
        {
            let _ = socket.send_to(&answer, from);
        }
    }
}

fn pcp(state: &mut State, request: &[u8]) -> Option<Vec<u8>> {
    if request.len() < 24 {
        return None;
    }
    let opcode = request[1];
    let lifetime = u32::from_be_bytes(request[4..8].try_into().unwrap());
    let client = Ipv6Addr::from(<[u8; 16]>::try_from(&request[8..24]).unwrap()).to_ipv4_mapped()?;
    let mut answer = vec![2, 0x80 | opcode, 0, 0, 0, 0, 0, 0];
    answer.extend_from_slice(&state.epoch().to_be_bytes());
    answer.extend_from_slice(&[0; 12]);
    if opcode == 0 {
        return Some(answer);
    }
    if opcode != 1 || request.len() < 60 {
        return None;
    }
    let nonce: [u8; 12] = request[24..36].try_into().unwrap();
    let port = u16::from_be_bytes([request[40], request[41]]);
    let suggested = u16::from_be_bytes([request[42], request[43]]);
    let mut granted = 0;
    match state.table.get(&port) {
        // A mapping named by another nonce is not this request's to change.
        Some(held) if held.nonce != Some(nonce) => answer[3] = 2,
        _ if lifetime == 0 => {
            state.table.remove(&port);
            state.count("pcp-delete", port);
        }
        _ => {
            state.suggested.insert(port, suggested);
            state.table.insert(
                port,
                Entry {
                    via: "pcp",
                    client,
                    description: "pcp".into(),
                    expires: Some(Instant::now() + Duration::from_secs(u64::from(lifetime))),
                    nonce: Some(nonce),
                },
            );
            granted = lifetime;
        }
    }
    answer[4..8].copy_from_slice(&granted.to_be_bytes());
    answer.extend_from_slice(&nonce);
    answer.extend_from_slice(&[17, 0, 0, 0]);
    answer.extend_from_slice(&port.to_be_bytes());
    let external = if granted == 0 { 0 } else { port + state.shift };
    answer.extend_from_slice(&external.to_be_bytes());
    answer.extend_from_slice(&EXTERNAL.to_ipv6_mapped().octets());
    Some(answer)
}

fn natpmp(state: &mut State, request: &[u8], client: Ipv4Addr) -> Option<Vec<u8>> {
    let epoch = state.epoch().to_be_bytes();
    match request.get(1)? {
        0 => {
            let mut answer = vec![0, 128, 0, 0];
            answer.extend_from_slice(&epoch);
            answer.extend_from_slice(&EXTERNAL.octets());
            Some(answer)
        }
        1 if request.len() >= 12 => {
            let port = u16::from_be_bytes([request[4], request[5]]);
            let lifetime = u32::from_be_bytes(request[8..12].try_into().unwrap());
            if lifetime == 0 {
                state.table.remove(&port);
                state.count("natpmp-delete", port);
            } else {
                state.table.insert(
                    port,
                    Entry {
                        via: "natpmp",
                        client,
                        description: "natpmp".into(),
                        expires: Some(Instant::now() + Duration::from_secs(u64::from(lifetime))),
                        nonce: None,
                    },
                );
            }
            let external = if lifetime == 0 { 0 } else { port + state.shift };
            let mut answer = vec![0, 129, 0, 0];
            answer.extend_from_slice(&epoch);
            answer.extend_from_slice(&port.to_be_bytes());
            answer.extend_from_slice(&external.to_be_bytes());
            answer.extend_from_slice(&lifetime.to_be_bytes());
            Some(answer)
        }
        _ => None,
    }
}

fn serve_search(socket: &UdpSocket, location: &str, state: &Mutex<State>, stop: &AtomicBool) {
    let mut buf = [0u8; 2048];
    while !stop.load(Ordering::Acquire) {
        let Ok((n, from)) = socket.recv_from(&mut buf) else {
            continue;
        };
        let text = String::from_utf8_lossy(&buf[..n]);
        {
            let mut state = state.lock().unwrap();
            if !state.behaviour.upnp || state.muted() || !text.starts_with("M-SEARCH") {
                continue;
            }
            state.count("M-SEARCH", 0);
            if state.searches_seen < state.behaviour.searches_ignored {
                state.searches_seen += 1;
                continue;
            }
        }
        let target = text
            .lines()
            .find_map(|line| line.strip_prefix("ST: "))
            .unwrap_or("ssdp:all");
        let answer = format!(
            "HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age=120\r\nST: {target}\r\n\
             USN: uuid:00000000-0000-4000-8000-000000000000::{target}\r\n\
             LOCATION: {location}\r\n\r\n"
        );
        let _ = socket.send_to(answer.as_bytes(), from);
    }
}

/// Each connection on a thread of its own, as a gateway serves them: one held
/// open never keeps the next waiting.
fn serve_http(
    listener: &TcpListener,
    state: Arc<Mutex<State>>,
    stop: Arc<AtomicBool>,
    elsewhere: bool,
) {
    while !stop.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, _)) => {
                if elsewhere {
                    state.lock().unwrap().count("elsewhere", 0);
                }
                let (state, stop) = (Arc::clone(&state), Arc::clone(&stop));
                std::thread::spawn(move || answer(stream, &state, &stop));
            }
            Err(_) => std::thread::sleep(Duration::from_millis(5)),
        }
    }
}

/// Wait out `span` in slices, so that the fake itself still stops at once.
fn hold(span: Duration, stop: &AtomicBool) {
    let end = Instant::now() + span;
    while Instant::now() < end && !stop.load(Ordering::Acquire) {
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn answer(mut stream: TcpStream, state: &Mutex<State>, stop: &AtomicBool) {
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    // The request's head, then as much body as it states.
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let (head, path, action, length) = loop {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
        let mut headers = [httparse::EMPTY_HEADER; 32];
        let mut request = httparse::Request::new(&mut headers);
        if let Ok(httparse::Status::Complete(head)) = request.parse(&buf) {
            let header = |name: &str| {
                request
                    .headers
                    .iter()
                    .find(|h| h.name.eq_ignore_ascii_case(name))
                    .map(|h| String::from_utf8_lossy(h.value).into_owned())
            };
            let action = header("SOAPAction").map(|value| {
                let value = value.trim_matches('"');
                value.rsplit('#').next().unwrap_or(value).to_string()
            });
            let length = header("Content-Length")
                .and_then(|value| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            break (head, request.path.unwrap_or("").to_string(), action, length);
        }
    };
    while buf.len() < head + length {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
    let body = &buf[head..head + length];
    // Silent: the connection closed, nothing carried out.
    if state.lock().unwrap().muted() {
        return;
    }
    let slow = state.lock().unwrap().behaviour.slow;
    hold(slow, stop);
    let (status, body) = match (path.as_str(), action.as_deref()) {
        ("/rootDesc.xml", _) => {
            let mut state = state.lock().unwrap();
            state.count("GET", 0);
            (200, state.description.clone())
        }
        ("/ctl/IPConn", Some(action)) => control(&mut state.lock().unwrap(), action, body),
        ("/ctl/Second", Some(action)) => second(&mut state.lock().unwrap(), action),
        _ => (404, String::new()),
    };
    // Carried out, and the answer withheld while the connection stays open.
    let withheld = action
        .as_deref()
        .is_some_and(|action| state.lock().unwrap().behaviour.unanswered == Some(action));
    if withheld {
        hold(HOLD, stop);
        return;
    }
    let reason = match status {
        200 => "OK",
        500 => "Internal Server Error",
        _ => "Not Found",
    };
    let _ = write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/xml; charset=\"utf-8\"\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
}

/// The second service: a 404 with no fault for every action, or a connection
/// that says it is down.
fn second(state: &mut State, action: &str) -> (u16, String) {
    state.count(&format!("second-{action}"), 0);
    match state.behaviour.second {
        Some(Second::Down) => match action {
            "GetStatusInfo" => done(action, &[("NewConnectionStatus", "Disconnected".into())]),
            "GetExternalIPAddress" => done(action, &[("NewExternalIPAddress", String::new())]),
            _ => fault(501),
        },
        _ => (404, String::new()),
    }
}

fn control(state: &mut State, action: &str, body: &[u8]) -> (u16, String) {
    state.purge();
    let document = roxmltree::Document::parse(std::str::from_utf8(body).unwrap()).unwrap();
    let call = document
        .descendants()
        .find(|node| node.tag_name().name() == action)
        .unwrap();
    let argument = |name: &str| {
        call.children()
            .find(|node| node.tag_name().name() == name)
            .and_then(|node| node.text())
            .unwrap_or("")
            .to_string()
    };
    let port: u16 = argument("NewExternalPort").parse().unwrap_or(0);
    state.count(action, port);
    match action {
        "GetExternalIPAddress" => done(
            action,
            &[("NewExternalIPAddress", state.behaviour.stated.to_string())],
        ),
        "GetStatusInfo" => done(action, &[("NewConnectionStatus", "Connected".into())]),
        "AddPortMapping" => {
            let lease: u32 = argument("NewLeaseDuration").parse().unwrap();
            let client: Ipv4Addr = argument("NewInternalClient").parse().unwrap();
            let description = argument("NewPortMappingDescription");
            assert_eq!(argument("NewProtocol"), "UDP");
            assert_eq!(argument("NewInternalPort"), port.to_string());
            if let Some(code) = state.behaviour.permanent_only
                && lease != 0
            {
                return fault(code);
            }
            match state.table.get(&port) {
                Some(held) if held.client != client || held.description != description => {
                    return fault(718);
                }
                Some(_) if state.behaviour.keeps_old_lease => return done(action, &[]),
                _ => {}
            }
            state.table.insert(
                port,
                Entry {
                    via: "upnp",
                    client,
                    description,
                    expires: (lease != 0)
                        .then(|| Instant::now() + Duration::from_secs(u64::from(lease))),
                    nonce: None,
                },
            );
            done(action, &[])
        }
        "DeletePortMapping" => {
            if state.table.remove(&port).is_some() {
                done(action, &[])
            } else {
                fault(714)
            }
        }
        "GetSpecificPortMappingEntry" => match state.table.get(&port) {
            Some(held) => {
                // Seconds left, rounded up as a gateway that counts whole
                // seconds would state them.
                let left = held.expires.map_or(0, |at| {
                    at.saturating_duration_since(Instant::now())
                        .as_millis()
                        .div_ceil(1000)
                });
                done(
                    action,
                    &[
                        ("NewInternalPort", port.to_string()),
                        ("NewInternalClient", held.client.to_string()),
                        ("NewEnabled", "1".into()),
                        ("NewPortMappingDescription", held.description.clone()),
                        ("NewLeaseDuration", left.to_string()),
                    ],
                )
            }
            None => fault(714),
        },
        _ => fault(401),
    }
}

fn done(action: &str, arguments: &[(&str, String)]) -> (u16, String) {
    let inner: String = arguments
        .iter()
        .map(|(name, value)| format!("<{name}>{value}</{name}>"))
        .collect();
    (
        200,
        envelope(&format!(
            "<u:{action}Response xmlns:u=\"{SERVICE}\">{inner}</u:{action}Response>"
        )),
    )
}

fn fault(code: u16) -> (u16, String) {
    (
        500,
        envelope(&format!(
            "<s:Fault><faultcode>s:Client</faultcode><faultstring>UPnPError</faultstring>\
             <detail><UPnPError xmlns=\"urn:schemas-upnp-org:control-1-0\"><errorCode>{code}</errorCode>\
             <errorDescription>refused</errorDescription></UPnPError></detail></s:Fault>"
        )),
    )
}

fn envelope(inner: &str) -> String {
    format!(
        "<?xml version=\"1.0\"?>\r\n<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
         s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\"><s:Body>{inner}</s:Body></s:Envelope>\r\n"
    )
}
