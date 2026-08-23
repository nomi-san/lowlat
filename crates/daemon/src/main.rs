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
    HostDataBase, OfferRelay, no_credentials,
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
    SocketAddr::new(IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED), 0);

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

/// Private address space: the ranges a reflexive probe can never discover.
///
/// A host candidate exists to advertise what a reflexive server cannot see. A
/// publicly routable address is already discoverable that way, so offering it
/// here as well is a duplicate that costs the peer part of a bounded check
/// budget; a private one is invisible to any server and is the only way to
/// reach us across a shared segment.
const PRIVATE_V4: [(u32, u32); 3] = [
    (0x0A00_0000, 8),  // 10.0.0.0/8
    (0xAC10_0000, 12), // 172.16.0.0/12
    (0xC0A8_0000, 16), // 192.168.0.0/16
];

/// Shared address space, offered only when asked for.
///
/// Reachable when both ends sit behind the same carrier translation or the same
/// overlay network, and unreachable otherwise, so it is opted into rather than
/// assumed. Offered blindly it is a candidate the far side spends checks on and
/// never answers.
const SHARED_V4: (u32, u32) = (0x6440_0000, 10); // 100.64.0.0/10

/// Host candidates offered at most, per family.
///
/// A machine with several bridges can present a long list, and every entry
/// costs the peer part of a check budget bounded in both attempts and time.
const MAX_HOST_CANDIDATES: usize = 8;

/// Whether an address falls inside `base/bits`.
fn in_network(addr: core::net::Ipv4Addr, base: u32, bits: u32) -> bool {
    let mask = u32::MAX.checked_shl(32 - bits).unwrap_or(0);
    (u32::from(addr) & mask) == base
}

/// Whether an address is one this host should offer as a host candidate.
///
/// Separate from the enumeration so the decision can be checked without a
/// machine that happens to carry the right interfaces.
fn wanted_host_address(addr: core::net::Ipv4Addr, shared: bool) -> bool {
    if PRIVATE_V4
        .iter()
        .any(|(base, bits)| in_network(addr, *base, *bits))
    {
        return true;
    }
    shared && in_network(addr, SHARED_V4.0, SHARED_V4.1)
}

/// Every private IPv4 address on an interface that is up.
///
/// **Enumerated rather than probed, because one route answers one question.**
/// Asking the routing table which source it would use for a public destination
/// names a single address, and a machine on one segment through two interfaces
/// -- a wired and a wireless leg of the same network, say -- then advertises one
/// of them and hides the other. A peer that could only reach the hidden one
/// sees a host offering nothing it can use.
fn private_v4_addresses(shared: bool) -> Vec<IpAddr> {
    let mut list: *mut libc::ifaddrs = core::ptr::null_mut();
    // SAFETY: getifaddrs writes one pointer to a list it allocates and owns.
    // Failure leaves nothing to release.
    if unsafe { libc::getifaddrs(&raw mut list) } != 0 {
        return Vec::new();
    }

    let mut found: Vec<IpAddr> = Vec::new();
    let mut node = list;
    while !node.is_null() {
        // SAFETY: the walk stops at null, so this node is one getifaddrs built
        // and it stays alive until freeifaddrs below.
        let entry = unsafe { &*node };
        node = entry.ifa_next;

        if entry.ifa_addr.is_null() || entry.ifa_flags & (libc::IFF_UP as u32) == 0 {
            continue;
        }
        // SAFETY: a non-null ifa_addr points at a sockaddr, whose family field
        // is present for every family.
        if i32::from(unsafe { (*entry.ifa_addr).sa_family }) != libc::AF_INET {
            continue;
        }
        // SAFETY: the family says AF_INET, so the address is a sockaddr_in.
        let sin = unsafe { &*entry.ifa_addr.cast::<libc::sockaddr_in>() };
        let addr = core::net::Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));

        if !wanted_host_address(addr, shared) {
            continue;
        }
        let addr = IpAddr::V4(addr);
        // One address can appear on more than one entry.
        if !found.contains(&addr) {
            found.push(addr);
        }
    }

    // SAFETY: `list` came from the successful getifaddrs above and has not been
    // released; the walk copied out of it and kept no pointers into it.
    unsafe { libc::freeifaddrs(list) };
    found
}

/// The IPv6 address a peer would reach us at directly.
///
/// **Probed rather than enumerated, which is the opposite of the v4 side and
/// for the same reason.** There is no translation on this family, so the
/// address a peer sees is the one we would send from -- and an interface
/// commonly carries three global addresses at once, a stable one, a temporary
/// one and a link route, of which only the source the kernel would actually
/// pick is worth advertising. Enumerating offers all three and makes the peer
/// spend checks discovering which.
fn primary_v6_address() -> Option<IpAddr> {
    // A connected datagram socket sends nothing. It only asks the routing table
    // which source address it would pick, so the destination is arbitrary among
    // the routable ones.
    let probe = UdpSocket::bind("[::]:0").ok()?;
    probe.connect("[2606:4700:4700::1111]:80").ok()?;
    let ip = probe.local_addr().ok()?.ip();
    // A source a peer cannot reach is worse than no candidate: it spends probe
    // budget and answers nothing.
    (!ip.is_loopback() && !ip.is_unspecified()).then_some(ip)
}

/// The addresses a peer would reach us at directly.
///
/// A family the machine does not have contributes nothing, which is the
/// ordinary outcome for IPv6 and is not an error. These are host candidates
/// whichever family they are in: the flag beside them separates a host
/// candidate from a reflexive one and says nothing about scope.
fn primary_local_addresses(shared: bool) -> Vec<IpAddr> {
    let mut found = private_v4_addresses(shared);
    if found.len() > MAX_HOST_CANDIDATES {
        lowlat_common::log_warn!(
            "lowlatd: {} host candidates found, offering {MAX_HOST_CANDIDATES}",
            found.len()
        );
        found.truncate(MAX_HOST_CANDIDATES);
    }
    found.extend(primary_v6_address());
    found
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

/// A peer's candidate, as an address to probe.
///
/// **Parsed, never edited as text.** A v4-mapped address is IPv4 and has two
/// textual forms: a peer may write the trailing bytes dotted, as
/// `::ffff:192.0.2.7`, or in hex, as `::ffff:c000:207`. Stripping the prefix as
/// text handles the first and turns the second into a fragment that parses as
/// nothing, so the candidate is dropped without a word and that address is
/// never probed. The parser knows both forms.
///
/// Collapsing a mapped address to the IPv4 it really is belongs to the
/// connectivity engine, which does it to every address it is handed. Nothing
/// here reads the family, so a second copy would be a second place for that
/// rule to drift.
fn peer_candidate(ip: &str, port: u16) -> Option<SocketAddr> {
    let ip: IpAddr = ip.trim().parse().ok()?;
    Some(SocketAddr::new(ip, port))
}

/// What an inbound candidate exchange asks this host to do.
#[derive(Debug, PartialEq, Eq)]
enum Relayed {
    /// A readiness barrier. **Its address is ignored and is not parsed**, which
    /// is the whole reason this is decided before the address is looked at.
    Ready,
    /// An address to probe.
    Probe(SocketAddr),
    /// Not an address this host can probe.
    Unreadable,
}

/// Read one relayed candidate exchange.
///
/// **The barrier is decided first.** A peer sends one candidate marked ready
/// and the receiver ignores the address on it, so peers put different things
/// there: a capture carries both the well-known placeholder and a peer's own
/// reflexive address. Parsing before checking the flag makes the barrier depend
/// on a field nothing reads, and a barrier that is dropped leaves a peer that
/// withholds its real candidates waiting for something that already arrived --
/// silent at both ends.
fn relayed(sync: bool, ip: &str, port: u16) -> Relayed {
    if sync {
        return Relayed::Ready;
    }
    match peer_candidate(ip, port) {
        Some(addr) => Relayed::Probe(addr),
        None => Relayed::Unreadable,
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
    // **Off unless asked for.** A shared-address-space candidate is reachable
    // only when both ends sit behind the same carrier translation or on the
    // same overlay network, and it is a wasted check for every peer that does
    // not.
    let local = primary_local_addresses(flag_set("--shared-address-space"));

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
    let mut seam = Admission::new(Config {
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
        stream: Some(lowlat::stream::Config {
            audio_kbps: flag("--audio-kbps")
                .and_then(|value| value.parse().ok())
                .unwrap_or(lowlat_audio::encode::DEFAULT_BITRATE_KBPS),
            allow_raw_audio: flag_set("--allow-raw-audio"),
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
    lowlat_common::log_info!(
        "lowlatd: host candidates: {}",
        local
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(" ")
    );
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
                    &local,
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
    local: &[IpAddr],
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
                        lowlat_common::log_info!("lowlatd: answered, guest bound to port {}", host.port);

                        // The port that was bound, not the one that was asked
                        // for: they differ as soon as a second guest walks. One
                        // per family, since the socket is dual stack and the
                        // same port answers on both.
                        for ip in local {
                            client.send("candex", &candex(
                                &offer.attempt_id, &offer.from, ip.to_string(), host.port, true, false,
                            ))?;
                        }
                    }
                    "candex_relay" => {
                        let relay: CandexRelay = serde_json::from_value(message.payload)?;
                        match relayed(relay.data.sync, &relay.data.ip, relay.data.port) {
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
                Event::Candidate { attempt, addr, .. } => {
                    let Some(to) = peers.get(&attempt) else {
                        continue;
                    };
                    lowlat_common::log_info!("lowlatd: local candidate {addr} for {attempt}");
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

#[cfg(test)]
mod tests {
    use super::*;

    /// **A v4-mapped address has two textual forms and both are IPv4.**
    ///
    /// The hex form is the one a textual strip loses: taking `::ffff:` off the
    /// front of it leaves `c000:207`, which is not an address, so the candidate
    /// went into the bin without a log line and the peer was never probed
    /// there. Both forms are asserted to reach the same address, which is what
    /// a strip cannot do.
    #[test]
    fn a_v4_mapped_candidate_parses_in_either_textual_form() {
        let dotted = peer_candidate("::ffff:192.0.2.7", 41000).expect("dotted form");
        let hex = peer_candidate("::ffff:c000:207", 41000).expect("hex form");

        assert_eq!(dotted, hex, "the two spellings named different addresses");
        assert_eq!(
            dotted.ip(),
            "::ffff:192.0.2.7".parse::<IpAddr>().expect("reference"),
            "a v4-mapped candidate did not survive the parse"
        );
    }

    /// **A readiness marker is not a candidate and must not need to parse.**
    ///
    /// The receiver ignores the address on one, and peers put different things
    /// there -- a capture has both the well-known placeholder and a peer's own
    /// reflexive address. The barrier must survive one this cannot read, so the
    /// address the seam is handed for a marker is deliberately nowhere.
    #[test]
    fn the_marker_address_is_never_somewhere_to_probe() {
        assert!(
            UNREAD_MARKER_ADDRESS.ip().is_unspecified(),
            "the marker stands in for an address that must never be probed"
        );
        assert_eq!(UNREAD_MARKER_ADDRESS.port(), 0);
    }

    /// **The barrier survives an address this host cannot read.**
    ///
    /// The receiver ignores the address on a readiness marker, so peers put
    /// different things there -- a capture carries both the well-known
    /// placeholder and a peer's own reflexive address, and a peer that
    /// anonymises its host candidates behind a `.local` name could put one of
    /// those there too. Deciding the barrier after parsing drops it, and a
    /// peer that withholds its real candidates until the barrier arrives then
    /// waits for something that already came. Nothing logs either half.
    #[test]
    fn a_readiness_barrier_does_not_depend_on_its_address() {
        assert_eq!(
            relayed(true, "1c4d9ae8-f7a8-4513-affb-dcbb40048922.local", 58667),
            Relayed::Ready,
            "a marker with an unreadable address lost the barrier"
        );
        // The two spellings a real peer has actually sent, both markers.
        assert_eq!(relayed(true, READY_PLACEHOLDER, READY_PORT), Relayed::Ready);
        assert_eq!(
            relayed(true, "::ffff:171.246.76.160", 56730),
            Relayed::Ready
        );
    }

    /// An ordinary candidate is an address to probe, and an unreadable one is
    /// declined rather than mistaken for a barrier.
    #[test]
    fn an_ordinary_candidate_is_probed_or_declined() {
        assert_eq!(
            relayed(false, "2405:4802:d0f5:6ec0:c048:4183:5759:8357", 31064),
            Relayed::Probe(SocketAddr::new(
                "2405:4802:d0f5:6ec0:c048:4183:5759:8357"
                    .parse::<IpAddr>()
                    .expect("reference"),
                31064
            ))
        );
        assert_eq!(
            relayed(false, "1c4d9ae8-f7a8-4513-affb-dcbb40048922.local", 58667),
            Relayed::Unreadable
        );
    }

    /// **Only the ranges a reflexive probe cannot discover.**
    ///
    /// Written out rather than derived from the constants they pin, so a
    /// mistyped base or prefix fails here instead of agreeing with itself.
    #[test]
    fn only_private_space_counts_as_a_host_candidate() {
        let private = |text: &str| {
            let addr: core::net::Ipv4Addr = text.parse().expect("address");
            PRIVATE_V4
                .iter()
                .any(|(base, bits)| in_network(addr, *base, *bits))
        };

        for inside in [
            "10.0.0.1",
            "10.255.255.254",
            "172.16.0.1",
            "172.31.255.254",
            "192.168.0.1",
            "192.168.1.192",
            "192.168.72.1",
        ] {
            assert!(private(inside), "{inside} should be private space");
        }
        for outside in [
            "9.255.255.255",
            "11.0.0.1",
            "172.15.255.255",
            "172.32.0.1",
            "192.167.255.255",
            "192.169.0.0",
            "42.119.87.246",
            "127.0.0.1",
            "169.254.1.1",
            "100.102.226.42",
        ] {
            assert!(!private(outside), "{outside} should not be private space");
        }
    }

    /// Shared address space is its own range and its own decision.
    #[test]
    fn shared_address_space_is_separate_and_opt_in() {
        let shared = |text: &str| {
            let addr: core::net::Ipv4Addr = text.parse().expect("address");
            in_network(addr, SHARED_V4.0, SHARED_V4.1)
        };

        assert!(shared("100.64.0.1"), "the bottom of the range");
        assert!(
            shared("100.102.226.42"),
            "an overlay address on this machine"
        );
        assert!(shared("100.127.255.254"), "the top of the range");
        assert!(!shared("100.63.255.255"), "just below the range");
        assert!(!shared("100.128.0.0"), "just above the range");

        // **And the gate is what decides it.** Checking the two ranges apart
        // from each other passes just as well when the gate is ignored and
        // shared space is offered to everyone, which is the failure this is
        // here to catch.
        for text in ["100.64.0.1", "100.102.226.42", "100.127.255.254"] {
            let addr: core::net::Ipv4Addr = text.parse().expect("address");
            assert!(
                !wanted_host_address(addr, false),
                "{text} was offered without being asked for"
            );
            assert!(
                wanted_host_address(addr, true),
                "{text} was withheld after being asked for"
            );
        }

        // Private space does not depend on the gate either way.
        for text in ["10.0.0.1", "192.168.1.192"] {
            let addr: core::net::Ipv4Addr = text.parse().expect("address");
            assert!(wanted_host_address(addr, false));
            assert!(wanted_host_address(addr, true));
        }
    }

    /// Nothing unreachable is ever offered, whichever way it was gathered.
    #[test]
    fn host_candidates_are_reachable_addresses() {
        for shared in [false, true] {
            for ip in primary_local_addresses(shared) {
                assert!(!ip.is_loopback(), "offered a loopback host candidate: {ip}");
                assert!(
                    !ip.is_unspecified(),
                    "offered an unspecified host candidate: {ip}"
                );
            }
        }
        assert!(
            primary_local_addresses(false)
                .iter()
                .filter(|ip| ip.is_ipv6())
                .count()
                <= 1,
            "the v6 side is probed, so it names one address"
        );
    }

    /// The ordinary forms still work, and rubbish is refused rather than
    /// turning into an address that gets probed.
    #[test]
    fn a_candidate_is_parsed_or_refused() {
        assert_eq!(
            peer_candidate("203.0.113.9", 41001),
            Some(SocketAddr::new(
                IpAddr::V4(std::net::Ipv4Addr::new(203, 0, 113, 9)),
                41001
            ))
        );
        assert_eq!(
            peer_candidate(" 2001:db8::1 ", 41002).map(|a| a.ip()),
            Some("2001:db8::1".parse::<IpAddr>().expect("reference")),
            "a v6 candidate must survive, whitespace and all"
        );
        assert_eq!(peer_candidate("not-an-address", 41003), None);
        assert_eq!(peer_candidate("", 41004), None);
    }
}
