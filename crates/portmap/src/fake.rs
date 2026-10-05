//! A gateway on loopback that speaks all three protocols, for the mapper's
//! tests: its behaviours are switched per test, its table read and changed,
//! every action counted, and it can restart.

use core::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4};
use core::sync::atomic::{AtomicBool, Ordering};
use core::time::Duration;
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Instant;

use crate::mapper::Endpoints;

/// The address the fake states as its outside.
pub(crate) const EXTERNAL: Ipv4Addr = Ipv4Addr::new(203, 0, 113, 7);
const SERVICE: &str = "urn:schemas-upnp-org:service:WANIPConnection:1";
/// A second loopback address, standing for a host that is not the gateway.
const ELSEWHERE: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 2);

fn description(control: &str) -> String {
    format!(
        "<?xml version=\"1.0\"?><root xmlns=\"urn:schemas-upnp-org:device-1-0\">\
         <device><deviceType>urn:schemas-upnp-org:device:InternetGatewayDevice:1</deviceType>\
         <deviceList><device><deviceType>urn:schemas-upnp-org:device:WANDevice:1</deviceType>\
         <deviceList><device><deviceType>urn:schemas-upnp-org:device:WANConnectionDevice:1</deviceType>\
         <serviceList><service><serviceType>{SERVICE}</serviceType>\
         <controlURL>{control}</controlURL></service></serviceList></device></deviceList></device>\
         </deviceList></device></root>"
    )
}

/// What the fake places on a host that is not the gateway.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Elsewhere {
    /// The description, by the search's answer.
    Location,
    /// The control service, by the description.
    Control,
}

#[derive(Debug, Clone)]
pub(crate) struct Behaviour {
    pub(crate) pcp: bool,
    pub(crate) natpmp: bool,
    pub(crate) upnp: bool,
    /// A timed UPnP lease refused with this code: only permanent mappings.
    pub(crate) permanent_only: Option<u16>,
    /// An identical UPnP add answered with success, the old lease kept.
    pub(crate) keeps_old_lease: bool,
    /// The external address UPnP states.
    pub(crate) stated: &'static str,
    /// How long each HTTP answer takes.
    pub(crate) slow: Duration,
    /// A part of the device placed on another host, which serves it there.
    pub(crate) elsewhere: Option<Elsewhere>,
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
        }
    }
}

impl Behaviour {
    pub(crate) fn upnp_only() -> Self {
        Self {
            pcp: false,
            natpmp: false,
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Entry {
    pub(crate) via: &'static str,
    pub(crate) client: Ipv4Addr,
    pub(crate) description: String,
    /// When it lapses; never, for a permanent one.
    pub(crate) expires: Option<Instant>,
    pub(crate) nonce: Option<[u8; 12]>,
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
}

impl State {
    fn count(&mut self, what: &str, port: u16) {
        *self.calls.entry((what.to_string(), port)).or_default() += 1;
    }
}

impl State {
    fn epoch(&self) -> u32 {
        u32::try_from(self.started.elapsed().as_secs()).unwrap()
    }

    fn purge(&mut self) {
        let now = Instant::now();
        self.table
            .retain(|_, entry| entry.expires.is_none_or(|at| at > now));
    }
}

pub(crate) struct Fake {
    state: Arc<Mutex<State>>,
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
    pub(crate) endpoints: Endpoints,
}

impl Fake {
    pub(crate) fn start(behaviour: Behaviour) -> Self {
        let state = Arc::new(Mutex::new(State {
            behaviour: behaviour.clone(),
            table: BTreeMap::new(),
            started: Instant::now(),
            calls: BTreeMap::new(),
            description: String::new(),
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
                serve_http(&listener, &state, &stop, true);
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
        state.lock().unwrap().description = description(&control_url);
        {
            let (state, stop) = (Arc::clone(&state), Arc::clone(&stop));
            threads.push(std::thread::spawn(move || {
                serve_http(&listener, &state, &stop, false);
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

    /// The live mappings.
    pub(crate) fn table(&self) -> BTreeMap<u16, Entry> {
        let mut state = self.state.lock().unwrap();
        state.purge();
        state.table.clone()
    }

    pub(crate) fn insert(&self, port: u16, entry: Entry) {
        self.state.lock().unwrap().table.insert(port, entry);
    }

    /// Forget every mapping and begin a new epoch, as a gateway that
    /// restarted.
    pub(crate) fn restart(&self) {
        let mut state = self.state.lock().unwrap();
        state.table.clear();
        state.started = Instant::now();
    }

    /// How often `what` was asked, for `port` when it names one.
    pub(crate) fn calls(&self, action: &str, port: u16) -> u32 {
        let state = self.state.lock().unwrap();
        state
            .calls
            .get(&(action.to_string(), port))
            .copied()
            .unwrap_or(0)
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
        drop(state);
        if let Some(answer) = answer {
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
    let mut granted = 0;
    match state.table.get(&port) {
        // A mapping named by another nonce is not this request's to change.
        Some(held) if held.nonce != Some(nonce) => answer[3] = 2,
        _ if lifetime == 0 => {
            state.table.remove(&port);
            state.count("pcp-delete", port);
        }
        _ => {
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
    answer.extend_from_slice(&(if granted == 0 { 0 } else { port }).to_be_bytes());
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
            let mut answer = vec![0, 129, 0, 0];
            answer.extend_from_slice(&epoch);
            answer.extend_from_slice(&port.to_be_bytes());
            answer.extend_from_slice(&(if lifetime == 0 { 0 } else { port }).to_be_bytes());
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
            if !state.behaviour.upnp || !text.starts_with("M-SEARCH") {
                continue;
            }
            state.count("M-SEARCH", 0);
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

fn serve_http(listener: &TcpListener, state: &Mutex<State>, stop: &AtomicBool, elsewhere: bool) {
    while !stop.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, _)) => {
                if elsewhere {
                    state.lock().unwrap().count("elsewhere", 0);
                }
                answer(stream, state, stop);
            }
            Err(_) => std::thread::sleep(Duration::from_millis(5)),
        }
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
    // A slow answer, in slices, so that the fake itself still stops at once.
    let slow = state.lock().unwrap().behaviour.slow;
    let end = Instant::now() + slow;
    while Instant::now() < end && !stop.load(Ordering::Acquire) {
        std::thread::sleep(Duration::from_millis(10));
    }
    let (status, body) = match (path.as_str(), action) {
        ("/rootDesc.xml", _) => {
            let mut state = state.lock().unwrap();
            state.count("GET", 0);
            (200, state.description.clone())
        }
        ("/ctl/IPConn", Some(action)) => control(&mut state.lock().unwrap(), &action, body),
        _ => (404, String::new()),
    };
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
