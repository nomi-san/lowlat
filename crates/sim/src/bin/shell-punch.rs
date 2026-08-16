//! Fixture endpoint driven by the real IO shell.
//!
//! Same role and same command line as `punch peer`, and deliberately so: the
//! namespace fixtures judge both by the lines they print, so swapping one for
//! the other changes what is under test and nothing else.
//!
//! What differs is everything below the command line. `punch` calls the
//! connectivity engine directly through a hand-rolled loop with a blocking read
//! and a timer read per pass. This owns a `Shell`: the real socket with its full
//! option set, batched receive, batched send, the wake descriptor, and a wait
//! armed from the endpoint's own deadline. The topologies are the same, so what
//! this adds is the shell itself.
//!
//! The reflexive candidate is polled from the engine rather than read off a
//! return value. A shell processes datagrams in batches and has nowhere to put a
//! per-datagram result, which is exactly why the engine retains it.

use std::env;
use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use lowlat_common::clock::{Time, elapsed_ms};
use lowlat_core::channel::{RecvRing, SlotMeta};
use lowlat_core::conn::{Conn, Credentials, State};
use lowlat_core::endpoint::Endpoint;
use lowlat_core::envelope::Envelope;
use lowlat_core::send::{SendRing, SendSlot};
use lowlat_core::session::Session;
use lowlat_net::{Shell, Socket, Wake};

/// How long to keep running after a path is found.
///
/// Answering checks outlives path selection, so an endpoint that exits the
/// instant it establishes abandons the answer it owes the other side and
/// strands a peer that was about to succeed. It would then report a one-sided
/// result that says nothing about the topology.
const SETTLE_MS: f64 = 600.0;

/// Ring geometry. No media crosses these fixtures; the session exists because
/// an endpoint owns one, and the shell drives the endpoint rather than the
/// connectivity engine on its own.
const SLOT: usize = 256;
const SLOTS: usize = 64;
const CHANNEL: u8 = 1;
const KEY: [u8; 32] = [0x77u8; 32];

fn main() {
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

    let socket = Socket::open(bind.port()).map_err(|e| format!("open {}: {e}", bind.port()))?;
    let wake = Wake::new().map_err(|e| format!("wake: {e}"))?;
    let mut shell = Shell::new(socket, wake, Endpoint::new(conn, session));

    for pair in args.windows(2) {
        let (name, value) = (&pair[0], &pair[1]);
        if name == "--server" {
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
    let (candidates, inbox) = mpsc::channel::<SocketAddr>();
    if let Some(path) = expect.clone() {
        let notify = shell.wake_handle().map_err(|e| format!("handle: {e}"))?;
        thread::spawn(move || {
            loop {
                if let Ok(text) = fs::read_to_string(&path)
                    && let Ok(addr) = text.trim().parse::<SocketAddr>()
                {
                    if candidates.send(addr).is_err() {
                        return;
                    }
                    let _ = notify.notify();
                    return;
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
            .add_candidate(candidate)
            .map_err(|e| e.to_string())?;
        println!("candidate {candidate}");
    }

    let started = Time::now();
    let mut published = false;
    let mut settled_at: Option<f64> = None;

    loop {
        let now_ms = elapsed_ms(started);
        if now_ms > timeout_ms {
            println!("timeout");
            return Ok(());
        }
        if let Some(at) = settled_at
            && now_ms > at + SETTLE_MS
        {
            return Ok(());
        }

        // Whatever signaling delivered, injected where the application's work is
        // pulled: after the wake has been taken, so nothing enqueued from here
        // on is lost.
        let mut arrived = None;
        let turn = shell
            .turn(now_ms, |endpoint| {
                while let Ok(addr) = inbox.try_recv() {
                    if endpoint.conn().add_candidate(addr).is_ok() {
                        arrived = Some(addr);
                    }
                }
            })
            .map_err(|e| format!("turn: {e}"))?;
        if let Some(addr) = arrived {
            println!("candidate {addr}");
        }
        if verbose && (turn.received > 0 || turn.sent > 0) {
            println!(
                "  {now_ms:.0} {:?} rx={} tx={}",
                turn.woke, turn.received, turn.sent
            );
        }

        if !published && let Some(mapped) = shell.endpoint().conn().reflexive().next() {
            if let Some(path) = publish.as_ref() {
                fs::write(path, mapped.to_string()).map_err(|e| format!("publish: {e}"))?;
            }
            published = true;
            println!("reflexive {mapped}");
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
