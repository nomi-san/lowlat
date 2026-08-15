//! Fixture endpoint: the punch over a real socket, inside a real namespace.
//!
//! The simulator proves the state machine against topologies that can be
//! described. This proves it against a kernel that was not written to agree
//! with our model, which is the only way the model itself gets checked.
//!
//! Two roles. `server` answers binding requests with the address it saw, and
//! stands in for a reflexive server. `peer` runs the engine: it learns its own
//! address from the server, publishes it through a file, waits for the other
//! side's file, and punches. The file is standing in for signaling, which the
//! application owns anyway.
//!
//! There is no IO shell yet, so the loop here is deliberately the simplest
//! thing that works: a short blocking read and a timer read per pass. It is a
//! fixture, not a preview of the shell.

use std::env;
use std::fs;
use std::net::{SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use lowlat_core::conn::{Conn, Credentials, Inbound, PROBE_TTL, State, Ttl};
use lowlat_core::demux::{self, Datagram};
use lowlat_core::stun::{self, Message, Method};

/// TTL for everything that is not a mapping probe.
const NORMAL_TTL: u32 = 64;

/// How long a receive blocks before the loop looks at its timers again.
const READ_TIMEOUT_MS: u64 = 5;

/// How long to keep running after a path is found.
///
/// Answering checks is unconditional and outlives path selection, so an
/// endpoint that exits the instant it establishes abandons the answer it owes
/// the other side and strands a peer that was about to succeed. A real endpoint
/// keeps going; the fixture has to as well, or it reports a one-sided result
/// that says nothing about the topology.
const SETTLE_MS: f64 = 600.0;

/// The fixture server signs with a fixed password. A public server would send
/// no credentials at all; the engine admits either, because what actually
/// admits a reflexive answer is an outstanding transaction toward that address.
const SERVER_PASSWORD: &str = "fixture";

fn main() {
    let args: Vec<String> = env::args().collect();
    let mode = args.get(1).map(String::as_str).unwrap_or("");

    let result = match mode {
        "server" => server(&args),
        "peer" => peer(&args),
        _ => {
            eprintln!("usage: punch server --bind ADDR");
            eprintln!(
                "       punch peer --bind ADDR --server ADDR --publish PATH --await PATH \
                 --local-ufrag S --local-pwd S --remote-ufrag S --remote-pwd S --seed N \
                 --timeout-ms N"
            );
            std::process::exit(2);
        }
    };

    if let Err(error) = result {
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

/// Answer binding requests with the address they arrived from.
fn server(args: &[String]) -> Result<(), String> {
    let bind = required(args, "--bind")?;
    let socket = UdpSocket::bind(bind).map_err(|e| format!("bind {bind}: {e}"))?;
    println!("server listening on {bind}");

    let mut rx = [0u8; 2048];
    let mut tx = [0u8; 512];
    loop {
        let Ok((len, from)) = socket.recv_from(&mut rx) else {
            continue;
        };
        let Some(datagram) = rx.get(..len) else {
            continue;
        };
        if demux::classify(datagram) != Datagram::Check {
            continue;
        }
        let Ok(message) = Message::parse(datagram) else {
            continue;
        };
        if message.method() != Method::BindingRequest {
            continue;
        }
        let Ok(len) =
            stun::encode_binding_response(&mut tx, message.transaction_id(), from, SERVER_PASSWORD)
        else {
            continue;
        };
        if let Some(reply) = tx.get(..len) {
            let _ = socket.send_to(reply, from);
        }
    }
}

/// Run the engine until it finds a path or the window closes.
fn peer(args: &[String]) -> Result<(), String> {
    let bind = required(args, "--bind")?;
    // File rendezvous is optional. Across the wide area an exchange takes longer
    // than the punch window, so the candidate is supplied directly and the
    // window is spent punching rather than waiting for signaling.
    let publish = flag(args, "--publish").map(PathBuf::from);
    let expect = flag(args, "--await").map(PathBuf::from);
    let timeout_ms: u64 = required(args, "--timeout-ms")?
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

    let socket = UdpSocket::bind(bind).map_err(|e| format!("bind {bind}: {e}"))?;
    socket
        .set_read_timeout(Some(Duration::from_millis(READ_TIMEOUT_MS)))
        .map_err(|e| format!("read timeout: {e}"))?;
    socket
        .set_ttl(NORMAL_TTL)
        .map_err(|e| format!("set ttl: {e}"))?;

    let mut conn = Conn::new(credentials, [seed_byte; 16], 0.0);
    // Repeatable. Asking two servers from one socket is how the mapping
    // behaviour is measured: one external port for both answers is endpoint
    // independent and punchable, two is symmetric and is not.
    for pair in args.windows(2) {
        let (name, value) = (&pair[0], &pair[1]);
        if name == "--server" {
            let server: SocketAddr = value.parse().map_err(|_| "bad --server".to_string())?;
            conn.add_server(server).map_err(|e| e.to_string())?;
        }
    }
    let mut awaited = false;
    if let Some(candidate) = flag(args, "--candidate") {
        let candidate: SocketAddr = candidate
            .parse()
            .map_err(|_| "bad --candidate".to_string())?;
        conn.add_candidate(candidate).map_err(|e| e.to_string())?;
        awaited = true;
        println!("candidate {candidate}");
    }

    let started = Instant::now();
    let mut published = false;
    let mut settled_at: Option<f64> = None;
    let mut tx = [0u8; 512];
    let mut rx = [0u8; 2048];

    loop {
        let now_ms = started.elapsed().as_secs_f64() * 1000.0;
        if now_ms > timeout_ms as f64 {
            println!("timeout");
            return Ok(());
        }
        if let Some(at) = settled_at
            && now_ms > at + SETTLE_MS
        {
            return Ok(());
        }

        // The other side's candidate, once signaling has carried it.
        if !awaited
            && let Some(path) = expect.as_ref()
            && let Ok(text) = fs::read_to_string(path)
            && let Ok(addr) = text.trim().parse::<SocketAddr>()
        {
            conn.add_candidate(addr).map_err(|e| e.to_string())?;
            awaited = true;
            println!("candidate {addr}");
        }

        drain(&socket, &mut conn, now_ms, &mut tx, verbose)?;

        match socket.recv_from(&mut rx) {
            Ok((len, from)) => {
                if let Some(datagram) = rx.get(..len)
                    && demux::classify(datagram) == Datagram::Check
                {
                    if verbose {
                        let kind = match Message::parse(datagram).map(|m| m.method()) {
                            Ok(Method::BindingRequest) => "check",
                            Ok(Method::BindingSuccess) => "answer",
                            _ => "other",
                        };
                        println!("  {now_ms:.0} rx {kind} <- {from}");
                    }
                    match conn.process_input(datagram, from) {
                        Ok(Inbound::Reflexive(mapped)) => {
                            if !published && let Some(path) = publish.as_ref() {
                                fs::write(path, mapped.to_string())
                                    .map_err(|e| format!("publish: {e}"))?;
                            }
                            published = true;
                            println!("reflexive {mapped} via {from}");
                        }
                        Ok(Inbound::PathEstablished(addr)) if settled_at.is_none() => {
                            println!("established {addr}");
                            settled_at = Some(now_ms);
                        }
                        _ => {}
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) if error.kind() == std::io::ErrorKind::TimedOut => {}
            Err(error) => return Err(format!("recv: {error}")),
        }

        conn.poll(started.elapsed().as_secs_f64() * 1000.0);
        match conn.state() {
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

/// Send everything the engine has ready, honouring the per-datagram TTL.
///
/// The restore is the load-bearing part. A probe leaves at a TTL that cannot
/// reach the peer, and a socket left at that value carries media a few hops and
/// no further, so the value is read back and checked rather than assumed.
fn drain(
    socket: &UdpSocket,
    conn: &mut Conn<'_>,
    now_ms: f64,
    buf: &mut [u8],
    verbose: bool,
) -> Result<(), String> {
    while let Some(result) = conn.get_output(now_ms, buf) {
        let egress = result.map_err(|e| format!("emit: {e}"))?;
        let datagram = buf.get(..egress.len).ok_or("short emit")?;

        if verbose {
            let kind = match Message::parse(datagram).map(|m| m.method()) {
                Ok(Method::BindingRequest) if egress.ttl == Ttl::Probe => "probe",
                Ok(Method::BindingRequest) => "check",
                Ok(Method::BindingSuccess) => "answer",
                _ => "other",
            };
            println!("  {now_ms:.0} tx {kind} -> {}", egress.to);
        }

        if egress.ttl == Ttl::Probe {
            socket
                .set_ttl(u32::from(PROBE_TTL))
                .map_err(|e| format!("probe ttl: {e}"))?;
        }
        let _ = socket.send_to(datagram, egress.to);
        if egress.ttl == Ttl::Probe {
            socket
                .set_ttl(NORMAL_TTL)
                .map_err(|e| format!("restore ttl: {e}"))?;
            let restored = socket.ttl().map_err(|e| format!("read ttl: {e}"))?;
            if restored != NORMAL_TTL {
                return Err(format!("ttl left at {restored} after a probe"));
            }
        }
    }
    Ok(())
}
