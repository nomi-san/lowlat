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

use std::net::SocketAddr;

use lowlat::admission::{Admission, Config, Event, Peer};
use lowlat_kessel::message::{
    Answer, AnswerData, CancelRelay, Candex, CandexRelay, CandidateData, ConnUpdate, Credentials,
    HostDataBase, OfferRelay, Relayed, no_credentials,
};
use lowlat_kessel::{Backoff, Client, Connect, Role};

/// The generation this host implements, as the wire wants it: a string.
const APP_V: &str = "150-104a";
const SDK_V: u32 = 0x0006_0000;

/// Advertised capacity, and the only policy the seam applies. Read once and
/// handed to both the seam and the advertisement, so the listing cannot promise
/// more than admission will grant.
/// Seats a host offers unless told otherwise. The compile-time cap is the
/// stream's, and a request above it is clamped rather than refused.
const MAX_GUESTS: u32 = 4;

/// Base port every guest's bind walks from.
const DEFAULT_PORT: u16 = 9000;

/// Reflexive servers, for discovering our own mapped address.
///
/// **A name, so both address families are reachable.** One dual-stack name
/// answers with an A and an AAAA record, and both are asked; a literal can only
/// ever be one family, and a v4 literal is why this host had no v6 reflexive
/// candidate to offer.
const DEFAULT_STUN: &str = "stun.l.google.com:19302";

/// What the stream produces by default. The guest declares what it can decode
/// and the host is authoritative over all of it, so a declaration is a request.
const WIDTH: u32 = 1920;
const HEIGHT: u32 = 1080;
const FPS: u32 = 60;

/// The bitrate ceiling, in megabits per second, before it is divided among the
/// guests on the stream. Ten is the documented default, and higher values buy
/// picture at the cost of latency.
const DEFAULT_BITRATE_MBPS: f64 = 10.0;

/// The floor a controller may not descend below.
const MIN_BITRATE_MBPS: f64 = 1.0;

/// How long the loop waits when neither side has anything, before draining the
/// seam again. Signaling carries no media and is on no hot path; a candidate
/// noticed this late is invisible against a wide-area round trip.
const IDLE_MS: u64 = 50;

/// The address on a readiness marker is ignored by the receiver, so this is a
/// placeholder rather than anywhere we can be reached. A widely deployed peer
/// sends this exact value, which is what establishes that it is ignored.
const READY_PLACEHOLDER: &str = "1.2.3.4";
const READY_PORT: u16 = 1234;

/// Stands in for the address on an inbound readiness marker.
///
/// The marker's address is ignored and must never reach the candidate table,
/// so nothing is lost by not carrying it -- and passing a real-looking one
/// would invite somebody to start probing it.
const UNREAD_MARKER_ADDRESS: SocketAddr =
    SocketAddr::new(std::net::IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED), 0);

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        lowlat_common::log_error!("lowlatd: {error}");
        std::process::exit(1);
    }
}

mod app;

fn flag(name: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    let at = args.iter().position(|arg| arg == name)?;
    args.get(at + 1).cloned()
}

/// A switch that carries no value.
fn flag_set(name: &str) -> bool {
    std::env::args().any(|arg| arg == name)
}

/// Where sound comes from, or nothing when it is switched off.
///
/// **A service outside the session has to be told which one**, because the
/// sound server's socket lives in that session's own runtime directory. Absent,
/// the environment answers -- which is right when the daemon runs inside the
/// session and wrong nowhere else, since a machine with no sound server simply
/// reports that it has none.
/// Say what a guest's microphone delivered, once a second while it does.
///
/// **A count and a rate rather than every packet.** A hundred lines a second
/// says nothing that one line a second does not, and it buries the rest of the
/// log while it does it.
fn report_microphone(heard: &lowlat::microphone::Receiver) {
    let mut samples = [0i16; 960];
    let mut packets = 0u64;
    let mut lost = 0u64;
    let mut loudest = 0i16;
    let mut said = lowlat_common::clock::Time::now();
    loop {
        match heard.recv_timeout_into(std::time::Duration::from_millis(500), &mut samples) {
            lowlat::microphone::Taken::Empty => {}
            lowlat::microphone::Taken::Took {
                guest,
                samples: count,
                dropped,
            } => {
                packets += 1;
                lost += u64::from(dropped);
                loudest = samples
                    .iter()
                    .take(count)
                    .fold(loudest, |peak, sample| peak.max(sample.saturating_abs()));
                if lowlat_common::clock::elapsed_ms(said) >= 1000.0 {
                    said = lowlat_common::clock::Time::now();
                    lowlat_common::log_info!(
                        "lowlatd: microphone from guest {guest}, packets={packets} dropped={lost} samples={count} peak={loudest}"
                    );
                    loudest = 0;
                }
            }
        }
    }
}

fn audio_config() -> Option<lowlat_audio::Config> {
    if flag_set("--no-audio") {
        return None;
    }
    Some(lowlat_audio::Config {
        server: flag("--audio-server"),
        wanted: std::sync::Arc::new(lowlat_audio::capture::Wanted::new(
            lowlat_audio::capture::Live {
                device: flag("--audio-device"),
                // **Off unless asked for.** It silences the speakers of whoever
                // is at the machine, which is opted into rather than defaulted.
                mute_local: flag_set("--mute-local"),
            },
        )),
    })
}

fn read(path: &str) -> String {
    std::fs::read_to_string(path)
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// Guests currently admitted, in the width the wire uses.
///
/// Bounded by the configured limit, which is a small number, so the conversion
/// cannot lose anything; it is written as a conversion rather than a cast so
/// that stays true if the limit ever moves.
fn occupancy(seam: &Admission) -> u32 {
    u32::try_from(seam.occupancy()).unwrap_or(u32::MAX)
}

fn advertisement(name: &str, capacity: u32, players: u32) -> ConnUpdate {
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
        max_players: capacity,
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
    // **Before anything else is set up.** It answers the one question that has
    // to be answered before --output can be used at all, and a machine being
    // asked what it has is not a machine about to host.
    if flag_set("--outputs") {
        for output in lowlat::display::Display::outputs() {
            match output.place {
                Some(place) => println!(
                    "{}  {}x{} at {},{} of a {}x{} desktop",
                    output.id,
                    output.width,
                    output.height,
                    place.x,
                    place.y,
                    place.desktop_width,
                    place.desktop_height
                ),
                None => println!("{}  {}x{}", output.id, output.width, output.height),
            }
        }
        return Ok(());
    }

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
    // Declines every offer, which is the only way to exercise the refusal path
    // against a real peer: approval is otherwise unconditional here.
    let reject_all = std::env::args().any(|arg| arg == "--reject");
    let base_port: u16 = flag("--port")
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_PORT);
    // **A list, comma separated.** One reflexive server answers what address it
    // sees; two answering differently is how an endpoint-independent
    // translator is told from a symmetric one, and both answers travel to the
    // peer as candidates. The engine holds four.
    // Names or literals, each resolved to one address per family. **A name that
    // does not resolve is reported and skipped rather than fatal**: an attempt
    // with no reflexive server still punches on what it gathered locally, and a
    // service that refuses to start because a resolver was briefly unavailable
    // is worse than one that offers fewer candidates.
    let configured_stun = std::env::var("LOWLAT_STUN").unwrap_or_else(|_| DEFAULT_STUN.to_string());
    let mut stun: Vec<SocketAddr> = Vec::new();
    for name in configured_stun
        .split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
        let found = lowlat::admission::resolve_server(name);
        if found.is_empty() {
            lowlat_common::log_warn!("lowlatd: reflexive server did not resolve, skipped: {name}");
            continue;
        }
        for addr in found {
            if stun.len() < lowlat::abi::LOWLAT_SERVERS_MAX {
                stun.push(addr);
            } else {
                lowlat_common::log_warn!("lowlatd: reflexive servers full, dropped {addr}");
            }
        }
    }
    lowlat_common::log_info!(
        "lowlatd: reflexive servers: {}",
        stun.iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(" ")
    );

    // Gathered here, beside the reflexive servers, because the two are filtered
    // together and the servers are handed to the seam a few lines below.

    let bitrate_mbps: f64 = flag("--bitrate")
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_BITRATE_MBPS);
    // The size the stream runs at. A larger one is how a frame is made big
    // enough to need more than one fragment, which is the whole of the
    // reassembly path a peer runs and the part a small synthetic picture never
    // reaches.
    let width: u32 = flag("--width")
        .and_then(|v| v.parse().ok())
        .unwrap_or(WIDTH);
    let height: u32 = flag("--height")
        .and_then(|v| v.parse().ok())
        .unwrap_or(HEIGHT);
    // Rows of unpredictable detail in the synthetic picture. Zero is the flat
    // picture; a band makes frames large enough to need more than one
    // fragment, which is the only way a peer's reassembly is exercised.
    let detail_rows: u32 = flag("--detail").and_then(|v| v.parse().ok()).unwrap_or(0);
    // Advertised capacity, and the number of seats the stream offers. Read
    // from here rather than hardcoded, so the two cannot disagree.
    let max_guests: u32 = flag("--max-guests")
        .and_then(|v| v.parse().ok())
        .unwrap_or(MAX_GUESTS)
        .clamp(
            1,
            u32::try_from(lowlat::stream::MAX_SEATS).unwrap_or(MAX_GUESTS),
        );
    // **One-based, because zero means unspecified rather than upright.** The
    // coded picture stays landscape whatever this says; a quarter turn changes
    // what the peer presents and what it maps pointer coordinates against.
    // **One encode serves every guest**, so the codec is chosen here and not
    // negotiated per guest. A peer that cannot decode it has to be refused
    // rather than accommodated, which is the phase 6 refusal path.
    let codec = match flag("--codec").as_deref() {
        Some("hevc" | "h265") => lowlat::stream::Codec::H265,
        _ => lowlat::stream::Codec::H264,
    };
    // **Absent means follow the display**, which is the right answer on a
    // machine with more than one card: the encoder has to be on the device the
    // display is on, and which device that is can change while this runs.
    let backend = match flag("--encoder").as_deref() {
        Some("vendor" | "nvenc") => Some(lowlat::stream::Backend::Vendor),
        Some("open" | "vaapi") => Some(lowlat::stream::Backend::Open),
        _ => None,
    };
    // **A name pins the interface; absent follows the device** -- the compute
    // interface where it exists, the fallback where it does not, which is the
    // settled tier ladder (05 section 4). A name that is neither is refused
    // loudly and the stream follows the device instead, because silently
    // measuring the interface nobody asked about is worse than not pinning.
    let convert =
        flag("--convert")
            .as_deref()
            .map_or_else(lowlat::capture::Backend::asked, |named| {
                let parsed = lowlat::capture::Backend::parse(named);
                if parsed.is_none() {
                    eprintln!("--convert {named} names no interface; following the device");
                }
                parsed
            });
    let rotation = match flag("--rotate").as_deref() {
        Some("90") => lowlat::video::Rotation::Deg90,
        Some("180") => lowlat::video::Rotation::Deg180,
        Some("270") => lowlat::video::Rotation::Deg270,
        _ => lowlat::video::Rotation::None,
    };

    // **Off unless asked for.** One guest driving at a time is a room's
    // decision, not a host's, and imposing it breaks two people sharing a
    // desktop.
    let exclusive_pointer = flag_set("--exclusive-pointer");
    // A live-run aid, off unless asked for: nothing on this machine vibrates,
    // so without it the path back to a peer's controller cannot be exercised
    // without running a game that raises an effect.
    let rumble_probe = flag_set("--rumble-probe");
    // **Not zero.** Level 0 declares congestion on any stale fragment once the
    // window passes its floor; it is compatibility-only and the guest loop was
    // pinned to it.
    // 1 is the default the core names; see its LEVELS table.
    let cg_level = flag("--cg-level")
        .and_then(|text| text.parse().ok())
        .filter(|level| *level < 3)
        .unwrap_or(1);
    // **Drained by a thread of its own, which is what an application does with
    // it.** The daemon has nothing to play a guest's microphone into, so it
    // reports what arrived: that is the whole of what a live run needs to see.
    let (hear, heard) = lowlat::microphone::queue();
    if flag_set("--accept-microphone") {
        std::thread::Builder::new()
            .name("lowlat-mic".to_owned())
            .spawn(move || report_microphone(&heard))
            .ok();
    }
    let mut seam = Admission::new(Config {
        microphone: Some(hear),
        exclusive_pointer,
        // The figure the pointer arbitration was tuned to. A flag exists so a
        // two-guest run can try another without a rebuild.
        exclusive_hold_ms: flag("--pointer-hold-ms")
            .and_then(|text| text.parse().ok())
            .unwrap_or(lowlat::floor::HOLD_MS),
        cg_level,
        rumble_probe,
        base_port,
        max_guests: max_guests as usize,
        servers: stun,
        shared_address_space: flag_set("--shared-address-space"),
        stream: Some(lowlat::stream::Config {
            convert,
            // Prefer the encoder that shares the capture's device, where the
            // device can serve it; the environment (LOWLAT_VULKAN_ENCODE=1)
            // reaches the same knob without a flag.
            prefer_vulkan: flag_set("--vulkan-encode"),
            audio_kbps: flag("--audio-kbps")
                .and_then(|value| value.parse().ok())
                .unwrap_or(lowlat_audio::encode::DEFAULT_BITRATE_KBPS),
            allow_raw_audio: flag_set("--allow-raw-audio"),
            // The daemon's switch is its command line, which does not change
            // while it runs: no source means no sound, and there is nothing
            // here that could turn it back on.
            audio_on: !flag_set("--no-audio"),
            // **Off unless asked for**, like the boundary's own default: it
            // costs a packet every ten milliseconds on the control channel and
            // a peer is told to send one only because this said so.
            accept_microphone: flag_set("--accept-microphone"),
            audio: audio_config(),
            codec,
            backend,
            cg_level,
            full_fps: flag_set("--full-fps"),
            width,
            height,
            fps: FPS,
            configured_mbps: bitrate_mbps,
            min_mbps: MIN_BITRATE_MBPS,
            rotation,
            detail_rows,
            // **Named, not indexed.** An index is whichever order the kernel
            // enumerated in and moves when a cable does; the name is the
            // system's own and is what a session knows the output by too.
            output: flag("--output"),
            // Off unless asked for. Capture needs the elevated capability and
            // a display, and a run that has neither should generate pictures
            // rather than refuse to start.
            display: flag_set("--capture"),
        }),
    });

    let params = Connect {
        server,
        session_id: session,
        role: Role::Host,
        build: APP_V.to_string(),
        sdk_version: SDK_V,
        keepalive: lowlat_kessel::client::KEEPALIVE,
    };

    // Who each attempt is with, so an outbound message can be addressed. The
    // seam is addressed by attempt and knows nothing about peer identity. Both
    // outlive a signaling drop, because an established guest has its own media
    // path and does not depend on the connection that introduced it.
    // **Only what configuration decides.** What the stream is really producing
    // is the display's answer and is read where it is known; describing it from
    // here would report a stream nobody is producing the moment a display
    // turned out to be a different size from the one that was asked for.
    let settings = app::Settings {
        accept_microphone: flag_set("--accept-microphone"),
        output: flag("--output").unwrap_or_default(),
        // The ceiling is configured in whole megabits; a client reads it as
        // an integer and there is nothing below one to report.
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "a configured bitrate in megabits, rounded and floored at zero"
        )]
        bitrate_mbps: bitrate_mbps.round().max(0.0) as u32,
        fps: FPS,
        rotated: !matches!(rotation, lowlat::video::Rotation::None),
        host_os: flag("--host-os").and_then(|v| v.parse().ok()).unwrap_or(0),
        fake_output: flag_set("--fake-output"),
        // **Off unless asked for.** Nothing here skips a repeated picture, so
        // this is the permission rather than the behaviour; promising to spend
        // the bitrate forever is not a default worth having.
        full_fps: flag_set("--full-fps"),
    };
    let mut peers: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut established: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut backoff = Backoff::new();

    loop {
        match Client::connect(&params).await {
            Ok(client) => {
                backoff.reset();
                // **An error inside a session reconnects rather than exits.**
                // A host that quits on one bad frame from the service takes
                // every established guest down with it, and the guests are the
                // thing this process exists to serve. The connection is the
                // recoverable part; losing it is what the loop already handles.
                match session_loop(
                    client,
                    &mut seam,
                    &mut peers,
                    &mut established,
                    &name,
                    max_guests,
                    reject_all,
                    &settings,
                )
                .await
                {
                    Ok(true) => return Ok(()),
                    Ok(false) => lowlat_common::log_info!("lowlatd: signaling closed"),
                    Err(error) => lowlat_common::log_info!("lowlatd: session failed: {error}"),
                }
            }
            Err(error) => lowlat_common::log_info!("lowlatd: connect failed: {error}"),
        }

        // Attempts that were still negotiating are gone: the peer abandoned
        // them when the connection carrying their candidates dropped. An
        // established guest is not touched, because its media path never went
        // through here.
        let abandoned: Vec<String> = peers
            .keys()
            .filter(|id| !established.contains(*id))
            .cloned()
            .collect();
        for id in abandoned {
            lowlat_common::log_info!("lowlatd: abandoning in-flight {id}");
            seam.end_connection(&id);
            peers.remove(&id);
        }

        let delay = backoff.next_delay();
        lowlat_common::log_info!("lowlatd: reconnecting in {:.1}s", delay.as_secs_f64());
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = tokio::signal::ctrl_c() => {
                lowlat_common::log_info!("lowlatd: stopping");
                return Ok(());
            }
        }
    }
}

/// One connection's lifetime. Returns true when the operator asked to stop, and
/// false when the connection merely ended and should be retried.
#[allow(
    clippy::too_many_arguments,
    reason = "one call site, and a struct here would only rename the arguments"
)]
async fn session_loop(
    mut client: Client,
    seam: &mut Admission,
    peers: &mut std::collections::HashMap<String, String>,
    established: &mut std::collections::HashSet<String>,
    name: &str,
    capacity: u32,
    reject_all: bool,
    settings: &app::Settings,
) -> Result<bool, Box<dyn std::error::Error>> {
    // What was being captured the last time every guest was told. Zero until a
    // display has been opened, which is also what a host with no stream has.
    let mut captured: u32 = 0;
    // Resent on every connection, not just the first: the service takes it as
    // the frame that registers the session, so a reconnect without it is a
    // connection the service has not associated with this host.
    let _ = client.send_text("__ping__");
    client.send(
        "conn_update",
        &advertisement(name, capacity, occupancy(seam)),
    )?;
    lowlat_common::log_info!(
        "lowlatd: advertised as {name:?}, capacity {capacity}, {} guest(s) carried over",
        seam.occupancy()
    );

    loop {
        tokio::select! {
            message = client.recv() => {
                let Some(message) = message else { return Ok(false) };
                match message.action.as_str() {
                    "offer_relay" => {
                        let offer: OfferRelay = serde_json::from_value(message.payload)?;
                        lowlat_common::log_info!("lowlatd: offer {} from {}", offer.attempt_id, offer.from);
                        peers.insert(offer.attempt_id.clone(), offer.from.clone());

                        // Admission is the application's decision, and this
                        // application's policy is capacity alone.
                        // **Silence is not a refusal.** A declined answer is a
                        // wire event the peer acts on at once; no answer at all
                        // leaves it connecting indefinitely, because nothing in
                        // the protocol reports a host that never replied. Every
                        // offer gets an answer, including the ones we turn down.
                        let refusal = if reject_all {
                            Some("policy".to_string())
                        } else {
                            seam.new_attempt(&offer.attempt_id, Peer {
                                ufrag: offer.data.creds.ice_ufrag,
                                pwd: offer.data.creds.ice_pwd,
                                aes256: offer.data.creds.aes256,
                                permissions: lowlat::inject::Permissions {
                                    keyboard: offer.permissions.keyboard,
                                    pointer: offer.permissions.mouse,
                                    gamepad: offer.permissions.gamepad,
                                },
                                owner: offer.is_owner,
                            })
                            .err()
                            .map(|error| error.to_string())
                        };
                        if let Some(why) = refusal {
                            lowlat_common::log_info!("lowlatd: declining {}: {why}", offer.attempt_id);
                            let empty = no_credentials();
                            client.send("answer", &Answer {
                                approved: false,
                                attempt_id: &offer.attempt_id,
                                data: AnswerData {
                                    base: HostDataBase::default(),
                                    creds: &empty,
                                },
                                to: &offer.from,
                            })?;
                            seam.end_connection(&offer.attempt_id);
                            peers.remove(&offer.attempt_id);
                            continue;
                        }
                        // **Zero: no opinion.** This service manages no
                        // gateway mapping and no port pool, so the configured
                        // base is the right start and the seam reports where it
                        // landed.
                        let host = seam.begin_p2p(&offer.attempt_id, 0)?;

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
                        lowlat_common::log_info!("lowlatd: answered, guest bound to port {}", host.port);


                    }
                    "candex_relay" => {
                        let relay: CandexRelay = serde_json::from_value(message.payload)?;
                        match relay.data.read() {
                            Relayed::Ready => {
                                seam.add_candidate(
                                    &relay.attempt_id,
                                    UNREAD_MARKER_ADDRESS,
                                    true,
                                );
                            }
                            Relayed::Probe(addr) => {
                                seam.add_candidate(&relay.attempt_id, addr, false);
                            }
                            // Not every candidate is an address: a peer may
                            // anonymise a host candidate behind a `.local` name
                            // that only multicast resolution answers. Nothing
                            // here can probe one, and saying so is the
                            // difference between a candidate declined and a
                            // candidate lost.
                            Relayed::Unreadable => lowlat_common::log_info!(
                                "lowlatd: candidate not an address, ignored: {}",
                                relay.data.ip
                            ),
                        }
                    }
                    "offer_cancel_relay" => {
                        let cancel: CancelRelay = serde_json::from_value(message.payload)?;
                        lowlat_common::log_info!("lowlatd: cancelled {}", cancel.attempt_id);
                        seam.end_connection(&cancel.attempt_id);
                        peers.remove(&cancel.attempt_id);
                        client.send("conn_update", &advertisement(name, capacity, occupancy(seam)))?;
                    }
                    // The service closes with a reason, and the reason is the
                    // only thing that distinguishes a bad session from a host
                    // that is simply unknown.
                    "close" => lowlat_common::log_info!("lowlatd: closed by the service: {}", message.payload),
                    // An opaque passthrough channel the schema does not list.
                    // Reported rather than dropped, so its arrival is visible.
                    "sdk" => lowlat_common::log_info!("lowlatd: sdk message: {}", message.payload),
                    other => lowlat_common::log_info!("lowlatd: ignoring {other}"),
                }
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(IDLE_MS)) => {}
            _ = tokio::signal::ctrl_c() => {
                lowlat_common::log_info!("lowlatd: stopping");
                return Ok(true);
            }
        }

        // **Watched rather than reported.** A guest asking for a different
        // output and a display moving to another card both change what is
        // being captured, and only the loop that rebuilt knows which happened;
        // this notices either, once it has actually landed.
        captured = app::announce_capture(seam, settings, captured);

        while let Some(received) = seam.poll_event() {
            // **Said out loud, because the queue is bounded.** An application
            // that stopped polling long enough loses the oldest events, and a
            // loss nobody reports looks like a peer that never did anything.
            if received.dropped > 0 {
                lowlat_common::log_info!(
                    "lowlatd: {} event(s) were dropped before this one",
                    received.dropped
                );
            }
            match received.event {
                Event::Candidate {
                    attempt,
                    addr,
                    from_stun,
                } => {
                    let Some(to) = peers.get(&attempt) else {
                        continue;
                    };
                    let kind = if from_stun { "reflexive" } else { "host" };
                    lowlat_common::log_info!("lowlatd: {kind} candidate {addr} for {attempt}");
                    client.send(
                        "candex",
                        &candex(
                            &attempt,
                            to,
                            addr.ip().to_string(),
                            addr.port(),
                            !from_stun,
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
                    lowlat_common::log_info!("lowlatd: established {attempt} over {addr}");
                    established.insert(attempt.clone());
                    // **Everyone is told, not just the arrival.** The room the
                    // others are in changed too, and a guest that joined
                    // earlier has no way to ask.
                    app::announce_guests(seam);
                    client.send(
                        "conn_update",
                        &advertisement(name, capacity, occupancy(seam)),
                    )?;
                }
                // The enum is non-exhaustive, so a catch-all is required across
                // the crate boundary even though it is the shape this project
                // otherwise avoids. Logged rather than dropped silently, so a
                // variant added later announces itself at runtime.
                Event::Ended { attempt, outcome } => {
                    lowlat_common::log_info!("lowlatd: ended {attempt}, {outcome:?}");
                    // Reaped whatever the reason: the loop has stopped, and
                    // leaving the attempt registered holds its port for the
                    // life of the host.
                    seam.end_connection(&attempt);
                    peers.remove(&attempt);
                    established.remove(&attempt);
                    // Told after the reaping, so the roster describes the room
                    // as it is rather than as it was a moment ago.
                    app::announce_guests(seam);
                    client.send(
                        "conn_update",
                        &advertisement(name, capacity, occupancy(seam)),
                    )?;
                }
                // **Answered here and nowhere below.** The identifier and the
                // body are this application's protocol rather than the SDK's,
                // so the choice of what they mean is made at this level and
                // the layers under it stay ignorant of it.
                Event::UserData { guest, id, text } => {
                    let printable: String = text
                        .iter()
                        .take(120)
                        .map(|byte| {
                            if byte.is_ascii_graphic() || *byte == b' ' {
                                char::from(*byte)
                            } else {
                                '.'
                            }
                        })
                        .collect();
                    let spoken = app::on_message(seam, guest, id, &text, settings);
                    lowlat_common::log_info!(
                        "lowlatd: guest {guest} sent id={id} len={} {}body={printable}",
                        text.len(),
                        if spoken { "" } else { "(not ours) " }
                    );
                }
                other => lowlat_common::log_info!("lowlatd: unhandled seam event {other:?}"),
            }
        }
    }
}
