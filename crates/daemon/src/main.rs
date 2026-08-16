//! The host service: signaling on one side, the admission seam on the other.
//!
//! This is the only place the two meet, and deliberately so. The SDK does not
//! link a signaling client, so something above both has to translate one into
//! the other, and that translation is the whole of this program.
//!
//! Four inbound actions map onto the seam's four calls, and the seam's event
//! queue maps back onto two outbound actions. Nothing else here is protocol.
//!
//!   KESSEL_WS_SERVER=... KESSEL_SESSION=... lowlatd [--name NAME] [--port N]

use std::net::{IpAddr, SocketAddr, UdpSocket};

use lowlat::admission::{Admission, Config, Event, Peer};
use lowlat_kessel::message::{
    Answer, AnswerData, CancelRelay, Candex, CandexRelay, CandidateData, ConnUpdate, Credentials,
    HostDataBase, OfferRelay,
};
use lowlat_kessel::{Client, Connect, Role};

/// The generation this host implements, as the wire wants it: a string.
const APP_V: &str = "150-104a";
const SDK_V: u32 = 0x0006_0000;

/// Advertised capacity, and the only policy the seam applies. Read once and
/// handed to both the seam and the advertisement, so the listing cannot promise
/// more than admission will grant.
const MAX_GUESTS: u32 = 4;

/// Base port every guest's bind walks from.
const DEFAULT_PORT: u16 = 9000;

/// Reflexive server, for discovering our own mapped address.
const DEFAULT_STUN: &str = "74.125.250.129:19302";

/// How long the loop waits when neither side has anything, before draining the
/// seam again. Signaling carries no media and is on no hot path; a candidate
/// noticed this late is invisible against a wide-area round trip.
const IDLE_MS: u64 = 50;

/// The address on a readiness marker is ignored by the receiver, so this is a
/// placeholder rather than anywhere we can be reached. A widely deployed peer
/// sends this exact value, which is what establishes that it is ignored.
const READY_PLACEHOLDER: &str = "1.2.3.4";
const READY_PORT: u16 = 1234;

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("lowlatd: {error}");
        std::process::exit(1);
    }
}

fn flag(name: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    let at = args.iter().position(|arg| arg == name)?;
    args.get(at + 1).cloned()
}

fn read(path: &str) -> String {
    std::fs::read_to_string(path)
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// The address a peer on our own network would reach us at.
///
/// Learned by asking the routing table which source it would use, which needs
/// no packet and no interface enumeration. Without it a host on the same
/// network as its guest offers only a reflexive candidate and has to hairpin
/// to reach a peer two metres away.
fn primary_local_address() -> Option<IpAddr> {
    let probe = UdpSocket::bind("0.0.0.0:0").ok()?;
    probe.connect("1.1.1.1:80").ok()?;
    probe.local_addr().ok().map(|addr| addr.ip())
}

/// Guests currently admitted, in the width the wire uses.
///
/// Bounded by the configured limit, which is a small number, so the conversion
/// cannot lose anything; it is written as a conversion rather than a cast so
/// that stays true if the limit ever moves.
fn occupancy(seam: &Admission) -> u32 {
    u32::try_from(seam.occupancy()).unwrap_or(u32::MAX)
}

fn advertisement(name: &str, players: u32) -> ConnUpdate {
    ConnUpdate {
        loader_v: 0,
        service_v: 0,
        os: "linux".to_string(),
        os_v: read("/proc/sys/kernel/osrelease"),
        platform: "linux".to_string(),
        app_v: APP_V.to_string(),
        sdk_v: SDK_V,
        device_id: read("/etc/machine-id"),
        mode: "desktop".to_string(),
        name: name.to_string(),
        desc: String::new(),
        game_id: String::new(),
        secret: String::new(),
        max_players: MAX_GUESTS,
        players,
        is_public: false,
        guests: Vec::new(),
    }
}

/// One outbound candidate, or a readiness marker.
fn candex<'a>(
    attempt: &'a str,
    to: &'a str,
    ip: String,
    port: u16,
    lan: bool,
    sync: bool,
) -> Candex<'a> {
    Candex {
        attempt_id: attempt,
        data: CandidateData {
            base: HostDataBase::default(),
            ip,
            port,
            lan,
            from_stun: !lan && !sync,
            sync,
        },
        to,
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let configured =
        std::env::var("KESSEL_WS_SERVER").map_err(|_| "KESSEL_WS_SERVER is not set")?;
    let configured = configured.trim();
    let server = if configured.contains("://") {
        configured.to_string()
    } else {
        format!("wss://{configured}")
    };
    let session = std::env::var("KESSEL_SESSION").map_err(|_| "KESSEL_SESSION is not set")?;
    let hostname = read("/proc/sys/kernel/hostname");
    let name = flag("--name").unwrap_or(if hostname.is_empty() {
        "lowlat".to_string()
    } else {
        hostname
    });
    let base_port: u16 = flag("--port")
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_PORT);
    let stun: SocketAddr = std::env::var("LOWLAT_STUN")
        .unwrap_or_else(|_| DEFAULT_STUN.to_string())
        .parse()
        .map_err(|_| "LOWLAT_STUN is not an address")?;

    let mut seam = Admission::new(Config {
        base_port,
        max_guests: MAX_GUESTS as usize,
        servers: vec![stun],
    });

    let mut client = Client::connect(&Connect {
        server,
        session_id: session,
        role: Role::Host,
        build: APP_V.to_string(),
        sdk_version: SDK_V,
    })
    .await?;

    // The greeting the service accepts alongside the message protocol. Sent
    // once, as a peer does; it is not a keepalive and nothing depends on it.
    let _ = client.send_text("__ping__");
    client.send("conn_update", &advertisement(&name, 0))?;
    println!("lowlatd: advertised as {name:?}, base port {base_port}, capacity {MAX_GUESTS}");

    // Who each attempt is with, so an outbound message can be addressed. The
    // seam is addressed by attempt and knows nothing about peer identity.
    let mut peers: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let local = primary_local_address();

    loop {
        tokio::select! {
            message = client.recv() => {
                let Some(message) = message else {
                    println!("lowlatd: signaling closed");
                    return Ok(());
                };
                match message.action.as_str() {
                    "offer_relay" => {
                        let offer: OfferRelay = serde_json::from_value(message.payload)?;
                        println!("lowlatd: offer {} from {}", offer.attempt_id, offer.from);
                        peers.insert(offer.attempt_id.clone(), offer.from.clone());

                        // Admission is the application's decision, and this
                        // application's policy is capacity alone.
                        if let Err(error) = seam.new_attempt(&offer.attempt_id, Peer {
                            ufrag: offer.data.creds.ice_ufrag,
                            pwd: offer.data.creds.ice_pwd,
                            aes256: offer.data.creds.aes256,
                        }) {
                            // Refusal is an answer too, and a peer that never
                            // hears one waits out its whole window.
                            println!("lowlatd: refusing {}: {error}", offer.attempt_id);
                            peers.remove(&offer.attempt_id);
                            continue;
                        }
                        let host = seam.begin_p2p(&offer.attempt_id)?;

                        let creds = Credentials {
                            aes256: Some(host.aes256),
                            fingerprint: host.fingerprint,
                            ice_ufrag: host.ufrag,
                            ice_pwd: host.pwd,
                        };
                        client.send("answer", &Answer {
                            approved: true,
                            attempt_id: &offer.attempt_id,
                            data: AnswerData { base: HostDataBase::default(), creds: &creds },
                            to: &offer.from,
                        })?;
                        println!("lowlatd: answered, guest bound to port {}", host.port);

                        // The port that was bound, not the one that was asked
                        // for: they differ as soon as a second guest walks.
                        if let Some(ip) = local {
                            client.send("candex", &candex(
                                &offer.attempt_id, &offer.from, ip.to_string(), host.port, true, false,
                            ))?;
                        }
                    }
                    "candex_relay" => {
                        let relay: CandexRelay = serde_json::from_value(message.payload)?;
                        // A v4-mapped address arrives in its textual form and
                        // is IPv4; parsing it structurally is what keeps it one.
                        let text = relay.data.ip.trim_start_matches("::ffff:");
                        let Ok(ip) = text.parse::<IpAddr>() else { continue };
                        seam.add_candidate(
                            &relay.attempt_id,
                            SocketAddr::new(ip, relay.data.port),
                            relay.data.sync,
                        );
                    }
                    "offer_cancel_relay" => {
                        let cancel: CancelRelay = serde_json::from_value(message.payload)?;
                        println!("lowlatd: cancelled {}", cancel.attempt_id);
                        seam.end_connection(&cancel.attempt_id);
                        peers.remove(&cancel.attempt_id);
                        client.send("conn_update", &advertisement(&name, occupancy(&seam)))?;
                    }
                    other => println!("lowlatd: ignoring {other}"),
                }
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(IDLE_MS)) => {}
            _ = tokio::signal::ctrl_c() => {
                println!("lowlatd: stopping");
                return Ok(());
            }
        }

        while let Some(event) = seam.poll_event() {
            match event {
                Event::Candidate { attempt, addr, .. } => {
                    let Some(to) = peers.get(&attempt) else {
                        continue;
                    };
                    println!("lowlatd: local candidate {addr} for {attempt}");
                    client.send(
                        "candex",
                        &candex(
                            &attempt,
                            to,
                            addr.ip().to_string(),
                            addr.port(),
                            false,
                            false,
                        ),
                    )?;
                }
                Event::Ready { attempt } => {
                    let Some(to) = peers.get(&attempt) else {
                        continue;
                    };
                    client.send(
                        "candex",
                        &candex(
                            &attempt,
                            to,
                            READY_PLACEHOLDER.to_string(),
                            READY_PORT,
                            false,
                            true,
                        ),
                    )?;
                }
                Event::Established { attempt, addr } => {
                    println!("lowlatd: established {attempt} over {addr}");
                    client.send("conn_update", &advertisement(&name, occupancy(&seam)))?;
                }
                // The enum is non-exhaustive, so a catch-all is required across
                // the crate boundary even though it is the shape this project
                // otherwise avoids. Logged rather than dropped silently, so a
                // variant added later announces itself at runtime.
                Event::Ended { attempt, outcome } => {
                    println!("lowlatd: ended {attempt}, {outcome:?}");
                    // Reaped whatever the reason: the loop has stopped, and
                    // leaving the attempt registered holds its port for the
                    // life of the host.
                    seam.end_connection(&attempt);
                    peers.remove(&attempt);
                    client.send("conn_update", &advertisement(&name, occupancy(&seam)))?;
                }
                other => println!("lowlatd: unhandled seam event {other:?}"),
            }
        }
    }
}
