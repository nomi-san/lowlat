//! The endpoint on the shell, which is built on Linux so far.

use std::env;
use std::fs;
use std::net::{IpAddr, SocketAddr, SocketAddrV4};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use lowlat_core::channel::{RecvRing, SlotMeta};
use lowlat_core::conn::{Conn, Credentials, Kind, State};
use lowlat_core::endpoint::Endpoint;
use lowlat_core::envelope::Envelope;
use lowlat_core::relay::{Relay, State as RelayState};
use lowlat_core::send::{SendRing, SendSlot};
use lowlat_core::session::Session;
use lowlat_net::{Shell, Socket, Wake};
use lowlat_portmap::{Config, Mapper, Reader, confirmed};

/// How long to keep running after a path is found.
///
/// Answering checks outlives path selection, so an endpoint that exits the
/// instant it establishes abandons the answer it owes the other side and
/// strands a peer that was about to succeed. It would then report a one-sided
/// result that says nothing about the topology.
const SETTLE_MS: f64 = 600.0;

/// How long a mapped endpoint waits, once it has its reflexive address, for
/// the gateway's mapping to be confirmed before it publishes without it.
const MAP_WAIT_MS: f64 = 3000.0;

/// How long the translation probe gives its servers.
const NAT_WAIT: Duration = Duration::from_secs(3);

/// Ring geometry. No media crosses these fixtures; the session exists because
/// an endpoint owns one, and the shell drives the endpoint rather than the
/// connectivity engine on its own.
const SLOT: usize = 256;
const SLOTS: usize = 64;
const CHANNEL: u8 = 1;
const KEY: [u8; 32] = [0x77u8; 32];

pub(crate) fn main() {
    let args: Vec<String> = env::args().collect();
    if let Err(error) = peer(&args) {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    let at = args.iter().position(|arg| arg == name)?;
    args.get(at + 1).map(String::as_str)
}

fn required<'a>(args: &'a [String], name: &str) -> Result<&'a str, String> {
    flag(args, name).ok_or_else(|| format!("missing {name}"))
}

fn peer(args: &[String]) -> Result<(), String> {
    // Only the port is taken from the bind address. Every fixture namespace
    // holds exactly one host address, and the socket is dual stack and bound to
    // the wildcard, so a v4 peer arrives v4-mapped -- which is the classification
    // the shell has to get right anyway.
    let bind: SocketAddr = required(args, "--bind")?
        .parse()
        .map_err(|_| "bad --bind".to_string())?;
    let publish = flag(args, "--publish").map(PathBuf::from);
    let expect = flag(args, "--await").map(PathBuf::from);
    let timeout_ms: f64 = required(args, "--timeout-ms")?
        .parse()
        .map_err(|_| "bad --timeout-ms".to_string())?;
    let verbose = args.iter().any(|a| a == "--verbose");
    let seed_byte: u8 = required(args, "--seed")?
        .parse()
        .map_err(|_| "bad --seed".to_string())?;

    let credentials = Credentials {
        local_ufrag: required(args, "--local-ufrag")?,
        local_pwd: required(args, "--local-pwd")?,
        remote_ufrag: required(args, "--remote-ufrag")?,
        remote_pwd: required(args, "--remote-pwd")?,
    };

    let mut recv_bodies = vec![0u8; SLOT * SLOTS];
    let mut recv_meta = vec![SlotMeta::default(); SLOTS];
    let mut send_bodies = vec![0u8; SLOT * SLOTS];
    let mut send_meta = vec![SendSlot::default(); SLOTS];

    let conn = Conn::new(credentials, [seed_byte; 16], 0.0);
    let mut session = Session::new(
        Envelope::from_key(&KEY).map_err(|e| format!("key: {e}"))?,
        1,
        0.0,
    );
    session
        .attach_recv(
            CHANNEL,
            RecvRing::new(&mut recv_bodies, &mut recv_meta, SLOT).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
    session
        .attach_send(
            CHANNEL,
            SendRing::new(&mut send_bodies, &mut send_meta, SLOT, CHANNEL)
                .map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;

    let relay = match flag(args, "--relay") {
        Some(server) => Some((
            server
                .parse::<SocketAddr>()
                .map_err(|_| "bad --relay".to_string())?,
            required(args, "--relay-user")?,
            required(args, "--relay-pass")?,
        )),
        None => None,
    };
    let endpoint = match relay {
        Some((server, username, password)) => Endpoint::relayed(
            conn,
            session,
            Relay::new(server, username, password, [!seed_byte; 16], 0.0),
        ),
        None => Endpoint::new(conn, session),
    };

    let socket = Socket::open(bind.port()).map_err(|e| format!("open {}: {e}", bind.port()))?;
    // With `--map` the gateway is asked to keep the port open, as a client with
    // its mapping on does, for the run; the mapping is deleted when it ends.
    let mapper = if args.iter().any(|a| a == "--map") {
        let port = socket
            .local_addr()
            .map_err(|e| format!("local: {e}"))?
            .port();
        let config = Config {
            port,
            count: 1,
            description: "ll-fixture".into(),
            gateway: None,
        };
        Some(Mapper::start(config).map_err(|e| format!("mapper: {e}"))?)
    } else {
        None
    };
    let mapping = mapper.as_ref().map(Mapper::reader);

    // With `--nat-server` the translation in front of the port is probed
    // first, from the socket the punch then takes, as a client probes from
    // its own port -- once the gateway has mapped it, when it is asked to.
    let mut nat_servers = Vec::new();
    for pair in args.windows(2) {
        if pair[0] == "--nat-server" {
            let server: SocketAddrV4 = pair[1]
                .parse()
                .map_err(|_| "bad --nat-server".to_string())?;
            nat_servers.push(server);
        }
    }
    if !nat_servers.is_empty() {
        if let Some(reader) = &mapping {
            let began = Instant::now();
            while reader.external().is_none()
                && began.elapsed() < Duration::from_secs_f64(MAP_WAIT_MS / 1000.0)
            {
                thread::sleep(Duration::from_millis(10));
            }
        }
        // Its own identifiers, apart from the punch's, so a late answer to
        // the probe is never taken for one of the punch's.
        let seed = [seed_byte ^ 0xA5; 16];
        let probed = lowlat_net::nat::probe(
            &socket,
            &nat_servers,
            seed,
            NAT_WAIT,
            &AtomicBool::new(false),
        )
        .map_err(|e| format!("nat: {e}"))?;
        let public = probed.verdict.public;
        let confirmed = mapping
            .as_ref()
            .and_then(Reader::external)
            .zip(public)
            .is_some_and(|(external, public)| IpAddr::V4(*external.ip()) == public.ip());
        let number = lowlat_core::nat::number(probed.verdict.mapping, confirmed);
        println!(
            "nat type={} mapping={:?} public={} confirmed={} answered={}/{}",
            number.map_or_else(|| "unknown".to_string(), |number| number.to_string()),
            probed.verdict.mapping,
            public.map_or_else(|| "none".to_string(), |public| public.to_string()),
            u8::from(confirmed),
            probed.answered,
            probed.asked
        );
    }

    let wake = Wake::new().map_err(|e| format!("wake: {e}"))?;
    let mut shell = Shell::new(socket, wake, endpoint);
    // A relay attempt's peer is a host offering its own address; a direct
    // attempt's peer publishes the address a reflexive server saw.
    let peer_kind = if relay.is_some() {
        Kind::Direct
    } else {
        Kind::Reflexive
    };

    for pair in args.windows(2) {
        let (name, value) = (&pair[0], &pair[1]);
        // A relay attempt asks no reflexive server.
        if name == "--server" && relay.is_none() {
            let server: SocketAddr = value.parse().map_err(|_| "bad --server".to_string())?;
            shell
                .endpoint()
                .conn()
                .add_server(server)
                .map_err(|e| e.to_string())?;
        }
    }

    // Signaling arrives on someone else's thread and is injected through the
    // wake, which is what an application does and what the wake descriptor is
    // for. Polling the rendezvous file from the loop instead would tie how fast
    // a candidate is noticed to how long the loop happens to be waiting, and
    // the loop waits on the endpoint's deadline -- tens of milliseconds when
    // nothing is due. That delay is invisible against a peer that waits, and
    // decisive against one that does not.
    // One address a line: a mapped endpoint publishes its gateway's beside the
    // reflexive one.
    let (candidates, inbox) = mpsc::channel::<SocketAddr>();
    if let Some(path) = expect.clone() {
        let notify = shell.wake_handle().map_err(|e| format!("handle: {e}"))?;
        thread::spawn(move || {
            loop {
                if let Ok(text) = fs::read_to_string(&path) {
                    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
                    let addrs: Vec<SocketAddr> =
                        lines.iter().filter_map(|l| l.trim().parse().ok()).collect();
                    if !addrs.is_empty() && addrs.len() == lines.len() {
                        for addr in addrs {
                            if candidates.send(addr).is_err() {
                                return;
                            }
                        }
                        let _ = notify.notify();
                        return;
                    }
                }
                thread::sleep(Duration::from_millis(1));
            }
        });
    }

    if let Some(candidate) = flag(args, "--candidate") {
        let candidate: SocketAddr = candidate
            .parse()
            .map_err(|_| "bad --candidate".to_string())?;
        shell
            .endpoint()
            .conn()
            .add_candidate(candidate, Kind::Direct)
            .map_err(|e| e.to_string())?;
        println!("candidate {candidate}");
    }

    let mut published = false;
    let mut reflexive_at: Option<f64> = None;
    let mut reached = false;
    let mut settled_at: Option<f64> = None;

    loop {
        // Whatever signaling delivered, injected where the application's work is
        // pulled: after the wake has been taken, so nothing enqueued from here
        // on is lost.
        let mut arrived = Vec::new();
        let turn = shell
            .turn(|endpoint| {
                while let Ok(addr) = inbox.try_recv() {
                    if endpoint.add_candidate(addr, peer_kind).is_ok() {
                        arrived.push(addr);
                    }
                    // The rendezvous file is the whole of this fixture's
                    // signaling: a peer that published is bound and
                    // listening, so readiness rides along with its candidate.
                    endpoint.conn().set_peer_ready();
                }
            })
            .map_err(|e| format!("turn: {e}"))?;
        // The loop's own bookkeeping runs on the same clock the pass ran on.
        let now_ms = turn.now;
        if now_ms > timeout_ms {
            println!("timeout");
            return Ok(());
        }
        if let Some(at) = settled_at
            && now_ms > at + SETTLE_MS
        {
            return Ok(());
        }
        for addr in arrived {
            println!("candidate {addr}");
        }
        if verbose && (turn.received > 0 || turn.sent > 0) {
            println!(
                "  {now_ms:.0} {:?} rx={} tx={}",
                turn.woke, turn.received, turn.sent
            );
        }

        if !published && let Some(reflexive) = shell.endpoint().conn().reflexive().next() {
            // The gateway's mapping goes beside it once the reflexive server has
            // confirmed its address, as a client offers it, or not at all if
            // that has not happened within the wait.
            let found_at = *reflexive_at.get_or_insert(now_ms);
            let mapped = mapping
                .as_ref()
                .and_then(Reader::external)
                .and_then(|external| confirmed(external, &[reflexive]));
            if mapping.is_none() || mapped.is_some() || now_ms > found_at + MAP_WAIT_MS {
                let mut lines = reflexive.to_string();
                if let Some(mapped) = mapped.filter(|mapped| *mapped != reflexive) {
                    lines = format!("{lines}\n{mapped}");
                }
                if let Some(path) = publish.as_ref() {
                    // Whole or not at all, for the reader polling it.
                    let partial = path.with_extension("partial");
                    fs::write(&partial, &lines).map_err(|e| format!("publish: {e}"))?;
                    fs::rename(&partial, path).map_err(|e| format!("publish: {e}"))?;
                }
                published = true;
                println!("reflexive {reflexive}");
                if let Some(mapped) = mapped {
                    println!("mapped {mapped}");
                }
            }
        }
        if !published && let Some(relayed) = shell.endpoint().relay().and_then(Relay::relayed) {
            if let Some(path) = publish.as_ref() {
                fs::write(path, relayed.to_string()).map_err(|e| format!("publish: {e}"))?;
            }
            published = true;
            println!("relayed {relayed}");
        }
        if let Some(RelayState::Failed(failure)) = shell.endpoint().relay().map(Relay::state) {
            println!("failed {failure:?}");
            return Ok(());
        }
        if !reached && let Some(addr) = shell.endpoint().conn().reachable() {
            reached = true;
            println!("reachable {addr}");
        }

        match shell.endpoint().conn().state() {
            State::Established(addr) => {
                if settled_at.is_none() {
                    println!("established {addr}");
                    settled_at = Some(now_ms);
                }
            }
            State::Failed(failure) => {
                println!("failed {failure:?}");
                return Ok(());
            }
            _ => {}
        }
    }
}
