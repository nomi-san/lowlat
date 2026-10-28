//! The service, on every platform; the session's side of it -- on Linux the
//! session helper and the tray -- is `session_side`'s.

use crate::{app, session_side};
use std::net::SocketAddr;

use lowlat_host::admission::{Admission, Config, Event, Peer, Transport};
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

/// The signaling service when none is named: the public one, which is the only
/// one a login's session is good for, so an install finishes with the login.
const DEFAULT_SERVER: &str = "kessel-ws.parsec.app";

/// Reflexive servers, for discovering our own mapped address.
///
/// **Names, so both address families are reachable.** One dual-stack name
/// answers with an A and an AAAA record, and both are asked; a literal can only
/// ever be one family, and a v4 literal is why this host had no v6 reflexive
/// candidate to offer.
///
/// **Two operators, because one server can only ever agree with itself.** Two
/// servers reporting different mapped ports is how a symmetric translator is
/// told from an endpoint-independent one, and two names on separate
/// infrastructure keep one outage from costing every reflexive candidate at
/// once. Two names by two families is four servers, which is exactly what the
/// engine holds.
const DEFAULT_STUN: &str = "stun.l.google.com:19302,stun.cloudflare.com:3478";

/// What the stream produces by default. The guest declares what it can decode
/// and the host is authoritative over all of it, so a declaration is a request.
const WIDTH: u32 = 1920;
const HEIGHT: u32 = 1080;
/// **Zero, which is a request to follow the captured display.** The rate a
/// display presents at is the ceiling on any stream of it, and it is not
/// knowable from here: the fallback when a device will not describe its mode
/// belongs where the display is opened, not where a flag is parsed.
const FPS: u32 = 0;

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

/// How often the room is told what everyone's numbers are.
///
/// **A wall-clock interval rather than a frame count.** The reference this
/// shape is written for re-sends every 120 frames, which is two seconds at
/// sixty; this host sends one frame a second on a still desktop, where a frame
/// count would stretch to two minutes of stale numbers exactly when a reader is
/// most likely watching. The interval is what was meant, so the interval is
/// what is used.
const ROSTER_MS: f64 = 2000.0;

/// How long an offer that needs approval is held before it is declined.
///
/// **Past the client's own wait.** An established client gives up on an
/// attempt nobody answered after 30 s, as the host not approving in time, and
/// tells the host nothing; a decline is a different answer, shown at once as
/// declined. Holding past that wait shows a guest what a host shows while
/// nobody approves, and the decline after it frees what the offer held here
/// and ends the wait of a client that keeps no timer of its own.
const HOLD_MS: f64 = 35_000.0;

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

/// What a host with no session says, in the words of the platform it runs on.
#[cfg(windows)]
const NOT_LOGGED_IN: &str = "lowlatd: not logged in, KESSEL_SESSION is read from %ProgramData%\\lowlat\\lowlatd.env and lowlat-login.py --install obtains it";
#[cfg(not(windows))]
const NOT_LOGGED_IN: &str = "lowlatd: not logged in, KESSEL_SESSION is read from /etc/lowlat/lowlatd.env and lowlat-login --install obtains it";

/// Why capture was refused, again in the platform's own terms: on Linux the
/// privilege is the answer, on Windows the session the host runs in.
#[cfg(windows)]
const CAPTURE_REFUSED: &str = "a display is lit and its framebuffer cannot be reached; the host must run in the console session, or --synth to generate pictures instead";
#[cfg(not(windows))]
const CAPTURE_REFUSED: &str = "a display is lit and its framebuffer cannot be reached; this needs the capture privilege, or --synth to generate pictures instead";

#[tokio::main]
pub(crate) async fn main() {
    if let Err(error) = run().await {
        lowlat_common::log_error!("lowlatd: {error}");
        std::process::exit(1);
    }
}

/// The `log` facade onto this program's own stream.
struct Bridge;

static BRIDGE: Bridge = Bridge;

impl log::Log for Bridge {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        lowlat_common::log::enabled(level_of(metadata.level()))
    }

    fn log(&self, record: &log::Record) {
        let level = level_of(record.level());
        if lowlat_common::log::enabled(level) {
            lowlat_common::log::emit(level, &format!("{}: {}", record.target(), record.args()));
        }
    }

    fn flush(&self) {}
}

/// The facade's levels onto this program's: a warning is a warning, and
/// everything the two crates consider informational is debug here.
fn level_of(level: log::Level) -> lowlat_common::log::Level {
    match level {
        log::Level::Error => lowlat_common::log::Level::Error,
        log::Level::Warn => lowlat_common::log::Level::Warn,
        log::Level::Info | log::Level::Debug => lowlat_common::log::Level::Debug,
        log::Level::Trace => lowlat_common::log::Level::Trace,
    }
}

/// Which program an invocation is.
///
/// **One binary, several roles**, because the sides speak a private protocol
/// that changes whenever either does and one build cannot disagree with itself
/// (docs/07-platforms.md section 5.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Program {
    /// The host itself, which is what an invocation with no role word is.
    Host,
    /// The session agent, which connects outward and speaks for its own
    /// session and no other.
    Session,
    /// The tray, which connects outward, shows the host and acts on it.
    Tray,
    /// The virtual displays from a shell: a command that runs and ends.
    Display,
    /// Windows' service manager's role: start and watch the host's processes.
    Service,
    /// Windows' install and uninstall, from an elevated shell.
    Install,
    Uninstall,
}

/// The words that select the session-side roles, in first position and
/// nowhere else.
const SESSION_ROLE: &str = "session";
const TRAY_ROLE: &str = "tray";
const DISPLAY_ROLE: &str = "display";
const SERVICE_ROLE: &str = "service";
const INSTALL_ROLE: &str = "install";
const UNINSTALL_ROLE: &str = "uninstall";

/// The program this invocation is, from the arguments after the name it was
/// run under.
///
/// **Read from the first argument alone.** The two roles run at different
/// privilege, so a file that can be talked into the wrong one is a security
/// defect rather than a bug: a flag is matched wherever it appears in a
/// command line, and an argument in first position is not. Everything else on
/// the line belongs to whichever role this answers.
fn program_of<'a>(args: impl IntoIterator<Item = &'a str>) -> Program {
    match args.into_iter().next() {
        Some(SESSION_ROLE) => Program::Session,
        Some(TRAY_ROLE) => Program::Tray,
        Some(DISPLAY_ROLE) => Program::Display,
        Some(SERVICE_ROLE) => Program::Service,
        Some(INSTALL_ROLE) => Program::Install,
        Some(UNINSTALL_ROLE) => Program::Uninstall,
        _ => Program::Host,
    }
}

/// The Windows control roles, named so the refusal off Windows can say which
/// one was asked for.
#[derive(Debug, Clone, Copy)]
enum ControlRole {
    Service,
    Install,
    Uninstall,
}

#[cfg(not(windows))]
impl ControlRole {
    fn word(self) -> &'static str {
        match self {
            Self::Service => SERVICE_ROLE,
            Self::Install => INSTALL_ROLE,
            Self::Uninstall => UNINSTALL_ROLE,
        }
    }
}

/// Run one of the control roles, or refuse it where the platform has no such
/// mechanism: the Linux unit is installed by packaging/install.sh, not by the
/// daemon.
#[cfg(windows)]
fn control_role(role: ControlRole) -> Result<(), Box<dyn std::error::Error>> {
    match role {
        ControlRole::Service => crate::windows_service::run_service(),
        ControlRole::Install => crate::windows_service::install(),
        ControlRole::Uninstall => crate::windows_service::uninstall(),
    }
}

#[cfg(not(windows))]
fn control_role(role: ControlRole) -> Result<(), Box<dyn std::error::Error>> {
    Err(format!(
        "lowlatd: the {} role is only for Windows; packaging/install.sh installs the unit on this platform",
        role.word()
    )
    .into())
}

/// A stop flag the host's loops wait on beside the console's interrupt.
///
/// The service's end event raises it on Windows; on every other platform
/// nothing does, and the interrupt is the only way out.
#[derive(Clone)]
pub(crate) struct Stop {
    fired: std::sync::Arc<std::sync::atomic::AtomicBool>,
    notify: std::sync::Arc<tokio::sync::Notify>,
}

impl Stop {
    #[cfg(not(windows))]
    pub(crate) fn never() -> Self {
        Self::new()
    }

    pub(crate) fn new() -> Self {
        Self {
            fired: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            notify: std::sync::Arc::new(tokio::sync::Notify::new()),
        }
    }

    pub(crate) fn fired(&self) -> bool {
        self.fired.load(std::sync::atomic::Ordering::Acquire)
    }

    pub(crate) async fn wait(&self) {
        if self.fired() {
            return;
        }
        self.notify.notified().await;
    }

    #[cfg(windows)]
    pub(crate) fn fire(&self) {
        self.fired.store(true, std::sync::atomic::Ordering::Release);
        self.notify.notify_waiters();
    }
}

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
fn report_microphone(heard: &lowlat_host::microphone::Receiver) {
    let mut samples = [0i16; 960];
    let mut packets = 0u64;
    let mut lost = 0u64;
    let mut loudest = 0i16;
    let mut said = lowlat_common::clock::Time::now();
    loop {
        match heard.recv_timeout_into(std::time::Duration::from_millis(500), &mut samples) {
            lowlat_host::microphone::Taken::Empty => {}
            lowlat_host::microphone::Taken::Took {
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

/// Where sound is read from.
///
/// **Always somewhere.** A host that streams a desktop streams its sound; there
/// is no reason to run one without it and no flag to. A machine with no sound
/// server says so once and the session runs regardless, which is the same
/// answer switching it off would have given.
fn audio_config() -> lowlat_audio::Config {
    lowlat_audio::Config {
        server: sound_server(),
        wanted: std::sync::Arc::new(lowlat_audio::capture::Wanted::new(
            lowlat_audio::capture::Live {
                device: flag("--audio-device"),
                // **On unless asked otherwise.** Somebody hosting their own
                // machine is in the room with it: hearing the session played
                // back at them is the surprising default, not the quiet one.
                // The tap is ahead of the mute where the device allows it, so
                // a guest hears everything either way.
                mute_local: !flag_set("--no-host-mute"),
            },
        )),
    }
}

/// The session's sound server's socket, which only Linux has: on Windows the
/// system's sound service is the one there is, so a socket named there is
/// refused with a line rather than carried.
fn sound_server() -> Option<String> {
    let named = flag("--audio-server");
    if cfg!(windows) && named.is_some() {
        lowlat_common::log_warn!(
            "lowlatd: --audio-server names a Linux sound server, ignored on Windows"
        );
        return None;
    }
    named
}

/// A small text file's contents, trimmed; empty when it cannot be read.
#[cfg(not(windows))]
fn read(path: &str) -> String {
    std::fs::read_to_string(path)
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// The machine's own name: the kernel's on Linux, the system's on Windows.
#[cfg(not(windows))]
fn host_name() -> String {
    read("/proc/sys/kernel/hostname")
}

#[cfg(windows)]
fn host_name() -> String {
    computer_name()
}

/// What the advertisement says this machine is: its system, that system's
/// version, the platform's own word for itself, and the listing's identifier.
#[cfg(not(windows))]
fn platform_fields() -> (&'static str, String, &'static str, String) {
    (
        "linux",
        read("/proc/sys/kernel/osrelease"),
        "linux",
        read("/etc/machine-id"),
    )
}

#[cfg(windows)]
fn platform_fields() -> (&'static str, String, &'static str, String) {
    ("Windows", os_version(), "windows", device_id())
}

/// The system's major version, as a client's listing shows it.
///
/// **`RtlGetVersion`, not `GetVersionExW`.** The latter answers with the
/// version the process's manifest declares unless the image opts into a newer
/// one, which would put a made-up number in a public listing; ntdll answers
/// what the kernel is.
#[cfg(windows)]
fn os_version() -> String {
    #[repr(C)]
    struct OsVersionInfo {
        size: u32,
        major: u32,
        _minor: u32,
        _build: u32,
        _platform: u32,
        _service_pack: [u16; 128],
    }
    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn RtlGetVersion(info: *mut OsVersionInfo) -> i32;
    }
    let mut info = OsVersionInfo {
        size: u32::try_from(core::mem::size_of::<OsVersionInfo>()).unwrap_or(0),
        major: 0,
        _minor: 0,
        _build: 0,
        _platform: 0,
        _service_pack: [0; 128],
    };
    // SAFETY: the structure is sized by its first field as the call requires
    // and lives across the call; the call writes within that size.
    let status = unsafe { RtlGetVersion(&raw mut info) };
    if status == 0 {
        info.major.to_string()
    } else {
        String::new()
    }
}

/// The computer's network name, as the system reports it.
#[cfg(windows)]
fn computer_name() -> String {
    unsafe extern "system" {
        fn GetComputerNameW(name: *mut u16, size: *mut u32) -> i32;
    }
    // Room for the longest such name, fifteen characters, and more.
    let mut name = [0u16; 64];
    let mut len = u32::try_from(name.len()).unwrap_or(0);
    // SAFETY: a buffer of `len` wide characters and its length, both live for
    // the call; the call writes at most that many and shortens `len`.
    let ok = unsafe { GetComputerNameW(name.as_mut_ptr(), &raw mut len) };
    if ok == 0 {
        return String::new();
    }
    let len = usize::try_from(len).unwrap_or(0).min(name.len());
    String::from_utf16_lossy(name.get(..len).unwrap_or(&[]))
}

/// The identifier a listing shows for this machine: thirty-two hex characters
/// made once from the one entropy source and kept beside the configuration.
///
/// **Random, not derived.** It need only be stable across restarts and
/// distinct between machines; deriving it from hardware or the installation's
/// identifier would make a public listing a fingerprint of the machine. A
/// file that cannot be read is made again, and one that cannot be written is
/// used for this process with a line saying so. Uninstall keeps it.
#[cfg(windows)]
fn device_id() -> String {
    static ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    ID.get_or_init(|| {
        let Some(path) = device_id_path() else {
            return fresh_device_id();
        };
        if let Ok(text) = std::fs::read_to_string(&path) {
            let text = text.trim();
            if is_device_id(text) {
                return text.to_string();
            }
        }
        let id = fresh_device_id();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(error) = std::fs::write(&path, &id) {
            lowlat_common::log_warn!("lowlatd: device id not kept at {}: {error}", path.display());
        }
        id
    })
    .clone()
}

#[cfg(windows)]
fn device_id_path() -> Option<std::path::PathBuf> {
    std::env::var_os("ProgramData").map(|base| {
        std::path::PathBuf::from(base)
            .join("lowlat")
            .join("device-id")
    })
}

#[cfg(windows)]
fn is_device_id(text: &str) -> bool {
    text.len() == 32 && text.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(windows)]
fn fresh_device_id() -> String {
    let mut bytes = [0u8; 16];
    match lowlat_crypto::fill(&mut bytes) {
        Ok(()) => lowlat_crypto::hex(&bytes),
        Err(_) => {
            lowlat_common::log_warn!("lowlatd: the platform supplied no entropy for a device id");
            String::new()
        }
    }
}

/// The signaling address from what the configuration names: the public service
/// when it names none, and a name without a scheme is a secure socket.
fn signaling_url(configured: &str) -> String {
    let named = configured.trim();
    let server = if named.is_empty() {
        DEFAULT_SERVER
    } else {
        named
    };
    if server.contains("://") {
        server.to_string()
    } else {
        format!("wss://{server}")
    }
}

/// Guests currently admitted, in the width the wire uses.
///
/// Bounded by the configured limit, which is a small number, so the conversion
/// cannot lose anything; it is written as a conversion rather than a cast so
/// that stays true if the limit ever moves.
pub(crate) fn occupancy(seam: &Admission) -> u32 {
    u32::try_from(seam.occupancy()).unwrap_or(u32::MAX)
}

fn advertisement(name: &str, capacity: u32, players: u32) -> ConnUpdate {
    let (os, os_v, platform, device_id) = platform_fields();
    ConnUpdate {
        loader_v: 0,
        service_v: 0,
        os: os.to_string(),
        os_v,
        platform: platform.to_string(),
        app_v: APP_V.to_string(),
        sdk_v: SDK_V,
        device_id,
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
    from_stun: bool,
    sync: bool,
) -> Candex<'a> {
    Candex {
        attempt_id: attempt,
        data: CandidateData {
            base: HostDataBase::default(),
            ip,
            port,
            lan,
            from_stun,
            sync,
        },
        to,
    }
}

/// What signaling said about a peer when it was introduced: where to answer,
/// and who it is.
#[derive(Debug, Clone)]
pub(crate) struct Introduced {
    /// The address an answer or a candidate goes to.
    pub(crate) from: String,
    /// The account's name, or empty for a peer the service did not name.
    /// Read by the tray, which only Linux's session side has.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) name: String,
}

/// Raise this process's priority class to high, once, as the established host
/// does for its own.
///
/// **The whole process, never a thread of it.** Every thread rises together
/// and keeps its order; a thread raised alone outranks the rest of its own
/// process, which has hung a two-core machine (docs/02-io-shell.md section 1).
/// The library never does this -- the process is its application's -- but this
/// program is the whole of its process.
#[cfg(windows)]
fn raise_process_class() {
    unsafe extern "system" {
        fn GetCurrentProcess() -> *mut core::ffi::c_void;
        fn SetPriorityClass(process: *mut core::ffi::c_void, class: u32) -> i32;
    }
    const HIGH_PRIORITY_CLASS: u32 = 0x80;
    // SAFETY: the pseudo-handle for this process, which needs no closing, and
    // a class the call documents.
    if unsafe { SetPriorityClass(GetCurrentProcess(), HIGH_PRIORITY_CLASS) } == 0 {
        lowlat_common::log_warn!("lowlatd: the process's priority class could not be raised");
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    // **Decided before any flag is read**, which is the whole of the rule: a
    // role that depends on a line having been scanned is a role a line can be
    // written to change. Everything after this belongs to whichever role it
    // answered.
    let args: Vec<String> = std::env::args().skip(1).collect();
    let program = program_of(args.iter().map(String::as_str));
    // **Off by default and worth having at all only for input.** Nothing else
    // in this program says anything at this level, so the switch is in
    // practice "log every key as it is expanded", which is the one fault that
    // cannot be read off a stream of injected events afterwards.
    if flag_set("--verbose") {
        lowlat_common::log::set_level(lowlat_common::log::Level::Debug);
    }
    // The browser pipe's two state machines say what they see through the
    // `log` facade; carried onto this program's own stream so a handshake
    // that fails is one log and not two. **Their idea of informational is a
    // hex dump of every packet**, so anything below a warning from them is
    // this program's debug, and is not even formatted unless asked for.
    let facade = if flag_set("--verbose") {
        log::LevelFilter::Debug
    } else {
        log::LevelFilter::Warn
    };
    let _ = log::set_logger(&BRIDGE).map(|()| log::set_max_level(facade));
    match program {
        Program::Session => session_side::session(),
        Program::Tray => session_side::tray(),
        Program::Display => crate::display::main(args.get(1..).unwrap_or(&[])),
        Program::Service => return control_role(ControlRole::Service),
        Program::Install => return control_role(ControlRole::Install),
        Program::Uninstall => return control_role(ControlRole::Uninstall),
        Program::Host => {}
    }
    // **The host's waits are milliseconds, and Windows rounds each one up to
    // its tick, about 15.6 ms, until the process raises it.** The library's
    // handles each hold one; this program drives the host directly, so it
    // holds its own for as long as it runs. Nothing elsewhere.
    let _timer = lowlat_common::clock::TimerResolution::raise();
    #[cfg(windows)]
    raise_process_class();
    // **The service's handles, if this host was started by it**: the log and
    // the two events, taken before anything logs so the first line reaches
    // the service's file. Nothing happens when it was not.
    #[cfg(windows)]
    crate::windows_service::host_handles();
    #[cfg(windows)]
    let stop = crate::windows_service::stop_signal();
    #[cfg(not(windows))]
    let stop = Stop::never();
    // **Before anything else is set up.** It answers the one question that has
    // to be answered before --output can be used at all, and a machine being
    // asked what it has is not a machine about to host.
    if flag_set("--outputs") {
        for output in lowlat_host::display::Display::outputs() {
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

    // **The host's own display exists on Windows alone**, and a stream asked
    // for an output that never lights waits for ever rather than failing.
    if cfg!(not(windows))
        && flag("--output")
            .is_some_and(|output| output.eq_ignore_ascii_case(lowlat_host::display::VIRTUAL))
    {
        return Err("--output virtual names the host's own display, which only Windows has".into());
    }

    // **Whether this process can reach the display, answered before anything
    // is advertised.** A display that is lit but out of reach is the
    // privilege, and a host that finds that out from its first guest has
    // advertised a stream it could never produce. Nothing lit is not refused:
    // the session may not have started yet, and the stream waits for one.
    if !flag_set("--synth") {
        match lowlat_host::display::Display::capturable() {
            lowlat_host::display::Capturable::Yes => {}
            lowlat_host::display::Capturable::NothingLit => {
                lowlat_common::log_warn!(
                    "lowlatd: nothing is scanning out yet, waiting for a display"
                );
            }
            lowlat_host::display::Capturable::NotReachable => {
                return Err(CAPTURE_REFUSED.into());
            }
        }
    }

    // **Not configured is not a failure.** A host that has been installed and
    // not yet logged in is an expected state, and a unit that failed on it
    // would be restarted into it every few seconds until somebody did; so
    // this says what is missing and where it goes, and exits cleanly.
    let session = std::env::var("KESSEL_SESSION").unwrap_or_default();
    if session.trim().is_empty() {
        lowlat_common::log_warn!("{}", NOT_LOGGED_IN);
        return Ok(());
    }
    let server = signaling_url(&std::env::var("KESSEL_WS_SERVER").unwrap_or_default());
    let hostname = host_name();
    let name = flag("--name").unwrap_or(if hostname.is_empty() {
        "lowlat".to_string()
    } else {
        hostname
    });
    // Declines every offer, which is the only way to exercise the refusal path
    // against a real peer.
    let reject_all = std::env::args().any(|arg| arg == "--reject");
    // **Opens, so off unless asked.** Admits an offer signaling marks as
    // needing approval, with the permissions it relayed: with no prompt here,
    // the one way to let an account the host was shared with in without its
    // being pre-approved.
    let approve_shared = flag_set("--approve-shared");
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
        let found = lowlat_host::admission::resolve_server(name);
        if found.is_empty() {
            lowlat_common::log_warn!("lowlatd: reflexive server did not resolve, skipped: {name}");
            continue;
        }
        for addr in found {
            if stun.len() < lowlat_host::admission::SERVERS_MAX {
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
    // The size the generated picture runs at, under --synth; a display
    // decides its own. A larger one is how a frame is made big enough to
    // need more than one fragment, which is the whole of the reassembly path
    // a peer runs and the part a small synthetic picture never reaches.
    let width: u32 = flag("--width")
        .and_then(|v| v.parse().ok())
        .unwrap_or(WIDTH);
    let height: u32 = flag("--height")
        .and_then(|v| v.parse().ok())
        .unwrap_or(HEIGHT);
    // **Absent means the captured display's own rate**, which is the answer
    // this program cannot give until it has looked at the display, so it is
    // passed on as zero and settled there. A number is a ceiling rather than a
    // promise and is clamped to the display: the loop will not run ahead of
    // the presents, so anything above the refresh is a rate the stream cannot
    // reach while still being what the encoder's budget is divided by.
    let fps: u32 = flag("--fps").and_then(|v| v.parse().ok()).unwrap_or(FPS);
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
            u32::try_from(lowlat_host::stream::MAX_SEATS).unwrap_or(MAX_GUESTS),
        );
    // **One-based, because zero means unspecified rather than upright.** The
    // coded picture stays landscape whatever this says; a quarter turn changes
    // what the peer presents and what it maps pointer coordinates against.
    // **One encode serves every guest**, so the codec is chosen here and not
    // negotiated per guest. A peer that cannot decode it has to be refused
    // rather than accommodated, which is the phase 6 refusal path.
    let codec = match flag("--codec").as_deref() {
        Some("hevc" | "h265") => lowlat_host::stream::Codec::H265,
        _ => lowlat_host::stream::Codec::H264,
    };
    // **Absent means follow the display**, which is the right answer on a
    // machine with more than one card: the encoder has to be on the device the
    // display is on, and which device that is can change while this runs.
    let backend = match flag("--encoder").as_deref() {
        Some("vendor" | "nvenc") => Some(lowlat_host::stream::Backend::Vendor),
        Some("open" | "vaapi") => Some(lowlat_host::stream::Backend::Open),
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
            .map_or_else(lowlat_host::capture::Backend::asked, |named| {
                let parsed = lowlat_host::capture::Backend::parse(named);
                if parsed.is_none() {
                    eprintln!("--convert {named} names no interface; following the device");
                }
                parsed
            });
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
    // **Named rather than numbered.** The values are a person's words for a
    // trade, and a number here would be a code somebody has to look up.
    let quality = match flag("--quality").as_deref() {
        None | Some("latency" | "lowest-latency") => lowlat_host::stream::Quality::LowestLatency,
        Some("balanced") => lowlat_host::stream::Quality::Balanced,
        Some("quality" | "highest") => lowlat_host::stream::Quality::Highest,
        Some(other) => {
            eprintln!("lowlatd: --quality {other} is not one of latency, balanced, quality");
            std::process::exit(2);
        }
    };
    let cg_level = flag("--cg-level")
        .and_then(|text| text.parse().ok())
        // 3 is adaptive, which runs the sensitive tuning until a host-local
        // signal lands behind it. See `lowlat_cg_level`.
        .filter(|level| *level <= 3)
        .unwrap_or(1);
    // **Drained by a thread of its own, which is what an application does with
    // it.** The daemon has nothing to play a guest's microphone into, so it
    // reports what arrived: that is the whole of what a live run needs to see.
    let (hear, heard) = lowlat_host::microphone::queue();
    if flag_set("--accept-microphone") {
        std::thread::Builder::new()
            .name("lowlat-mic".to_owned())
            .spawn(move || report_microphone(&heard))
            .ok();
    }
    // **Opened before the first guest and independent of one.** A session-side
    // program connects when its session starts, which is not when somebody
    // decides to stream, and nothing on the channel is load bearing either way
    // (docs/07-platforms.md section 5.1).
    session_side::listen();
    let mut seam = Admission::new(Config {
        microphone: Some(hear),
        pad_sink: None,
        exclusive_pointer,
        // The figure the pointer arbitration was tuned to. A flag exists so a
        // two-guest run can try another without a rebuild.
        exclusive_hold_ms: flag("--pointer-hold-ms")
            .and_then(|text| text.parse().ok())
            .unwrap_or(lowlat_host::floor::HOLD_MS),
        cg_level,
        rumble_probe,
        base_port,
        max_guests: max_guests as usize,
        servers: stun,
        shared_address_space: flag_set("--shared-address-space"),
        stream: Some(lowlat_host::stream::Config {
            // **Not a flag.** The depth is what the seated guests declare, so
            // the daemon starts eight-bit and the encoder is rebuilt if one
            // asks for ten; a switch here would choose for guests whose
            // decoders nobody running the daemon can see. Full chroma is the
            // same, and gated further by the census over every selectable
            // encoder, so there is nothing to configure here either.
            ten_bit: false,
            chroma_444: false,
            convert,
            // Prefer the encoder that shares the capture's device, where the
            // device can serve it; the environment (LOWLAT_VULKAN_ENCODE=1)
            // reaches the same knob without a flag.
            prefer_vulkan: flag_set("--vulkan-encode"),
            audio_kbps: flag("--audio-kbps")
                .and_then(|value| value.parse().ok())
                .unwrap_or(lowlat_audio::encode::DEFAULT_BITRATE_KBPS),
            allow_raw_audio: flag_set("--allow-raw-audio"),
            // **On, with nothing to turn it off.** A host that streams a
            // desktop streams its sound; the boundary keeps the switch because
            // an application embedding this library may have its own reasons,
            // and a daemon has none.
            audio_on: true,
            // **Off unless asked for**, like the boundary's own default: it
            // costs a packet every ten milliseconds on the control channel and
            // a peer is told to send one only because this said so.
            accept_microphone: flag_set("--accept-microphone"),
            audio: Some(audio_config()),
            codec,
            backend,
            cg_level,
            full_fps: flag_set("--full-fps"),
            quality,
            width,
            height,
            fps,
            configured_mbps: bitrate_mbps,
            min_mbps: MIN_BITRATE_MBPS,
            detail_rows,
            // **Named, not indexed.** An index is whichever order the kernel
            // enumerated in and moves when a cable does; the name is the
            // system's own and is what a session knows the output by too.
            output: flag("--output"),
            // **The display, unless asked otherwise.** A host streams a
            // desktop; the generator exists so the layers above capture can
            // be run and measured without a screen, and a run that wants it
            // says so. A host that cannot open the display refuses rather
            // than generating pictures in its place, because a stream of the
            // wrong thing is worse than none.
            display: !flag_set("--synth"),
            // **On, where the library's default is off.** A service is what
            // a machine with no monitor is remote-controlled through, and a
            // display of the host's own is what such a machine needs before
            // anybody connects. Only Windows has one.
            virtual_fallback: !flag_set("--no-virtual-fallback"),
            // **Off unless asked**, as in the library: a display that slept
            // is woken by a guest's first input, and a connection lights
            // nothing at the desk by itself.
            wake_display: flag_set("--wake-display"),
            // **On, where the library's default is off.** The service runs the
            // host as the administrator, which a hold needs; a host started by
            // hand without it says so as each stream starts, and streams on.
            hold_memory_clock: true,
            own_display: lowlat_host::stream::OwnDisplay::default(),
        }),
    });
    // **Off unless asked**, as in the library: the guests' keys and pointers
    // go to the host's own input driver only where the owner wants them
    // there. Where the driver is not installed it is said once and the batch
    // input call kept, and the start goes on: a host that failed for it would
    // be started again by the service, and fail again.
    #[cfg(windows)]
    if flag_set("--virtual-input")
        && let Err(error) = seam.set_virtual_input(true)
    {
        lowlat_common::log_warn!(
            "lowlatd: --virtual-input refused, keys and pointer go through the batch input call, error={error}"
        );
    }
    // **The same for the pads sent as their reports**: presented as their
    // products on the input driver only where the owner wants them there, a
    // pad on it reaching no game that reads only the Xbox pad's interface.
    #[cfg(windows)]
    if flag_set("--virtual-pads")
        && let Err(error) = seam.set_virtual_pads(true)
    {
        lowlat_common::log_warn!(
            "lowlatd: --virtual-pads refused, pads play as the Xbox pad, error={error}"
        );
    }

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
        // **Off unless asked for, and anything unrecognised is off.** One of
        // the two directions ships whatever the person at this machine copied,
        // and a typo must not open it.
        guest_clipboard: app::Clipboard::named(flag("--clipboard").as_deref()),
        output: flag("--output").unwrap_or_default(),
        // The ceiling is configured in whole megabits; a client reads it as
        // an integer and there is nothing below one to report.
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "a configured bitrate in megabits, rounded and floored at zero"
        )]
        bitrate_mbps: bitrate_mbps.round().max(0.0) as u32,
        fps,
        host_os: flag("--host-os").and_then(|v| v.parse().ok()).unwrap_or(0),
        fake_output: flag_set("--fake-output"),
        // **Off unless asked for.** Nothing here skips a repeated picture, so
        // this is the permission rather than the behaviour; promising to spend
        // the bitrate forever is not a default worth having.
        full_fps: flag_set("--full-fps"),
    };
    let mut peers: std::collections::HashMap<String, Introduced> = std::collections::HashMap::new();
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
                    approve_shared,
                    &settings,
                    &stop,
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
            _ = stop.wait() => {
                #[cfg(windows)]
                end_guests(&mut seam).await;
                lowlat_common::log_info!("lowlatd: stopping, asked by the service");
                return Ok(());
            }
        }
    }
}

/// The status a guest reads as the host system changing.
#[cfg(windows)]
const CHANGING_STATUS: i32 = 3;

/// How long the host waits for its kicked guests to end before it goes: a
/// guest's pass is at most 50 ms and its kick's grace 250 ms, and the service
/// waits five seconds in all.
#[cfg(windows)]
const KICKED_WAIT_MS: f64 = 1000.0;

/// Tell every seated guest the host system is changing, and wait for each to
/// end before the loop does.
///
/// **The wait is what delivers the status.** Each guest's own thread sends it
/// at its next pass and holds the session through a grace period; dropping
/// the seam at once stops those threads first, and the peer learns nothing.
///
/// **Used only where a host outlives the change** (Windows, on the service's
/// end event): a logoff ends the host with its session and gives it no chance
/// to say anything (docs/impl-plan-windows.md W2.5, D1).
#[cfg(windows)]
async fn end_guests(seam: &mut Admission) {
    let mut waiting: std::collections::HashSet<String> = seam
        .guests()
        .into_iter()
        .filter(|guest| seam.kick_guest(guest.number, CHANGING_STATUS))
        .map(|guest| guest.attempt)
        .collect();
    let kicked = waiting.len();
    let since = lowlat_common::clock::Time::now();
    while !waiting.is_empty() && lowlat_common::clock::elapsed_ms(since) < KICKED_WAIT_MS {
        while let Some(received) = seam.poll_event() {
            if let Event::Ended { attempt, .. } = received.event {
                waiting.remove(&attempt);
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    lowlat_common::log_info!(
        "lowlatd: guests told the host is changing, kicked={kicked} ended={}",
        kicked - waiting.len()
    );
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
    peers: &mut std::collections::HashMap<String, Introduced>,
    established: &mut std::collections::HashSet<String>,
    name: &str,
    capacity: u32,
    reject_all: bool,
    approve_shared: bool,
    settings: &app::Settings,
    stop: &Stop,
) -> Result<bool, Box<dyn std::error::Error>> {
    // What was being captured, and at what rate, the last time every guest was
    // told. Zero until a display has been opened, which is also what a host
    // with no stream has.
    let mut announced: (u32, u32) = (0, 0);
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

    // When the room was last told what everyone's numbers are.
    // **Said out loud at startup.** One of its two directions ships whatever
    // the person at this machine copied, so which way it is set is not
    // something anybody should have to infer from behaviour.
    lowlat_common::log_info!(
        "lowlatd: a guest's clipboard is {}",
        settings.guest_clipboard.name()
    );
    let mut rostered = lowlat_common::clock::Time::now();
    // Offers waiting for an approval nothing here gives, oldest first: the
    // attempt, who sent it, and when.
    let mut held: Vec<(String, String, lowlat_common::clock::Time)> = Vec::new();
    // What the session's side of this host keeps between passes.
    let mut side = session_side::Side::new();

    loop {
        tokio::select! {
            message = client.recv() => {
                let Some(message) = message else { return Ok(false) };
                match message.action.as_str() {
                    "offer_relay" => {
                        let offer: OfferRelay = serde_json::from_value(message.payload)?;
                        lowlat_common::log_info!(
                            "lowlatd: offer {} from {}, owner={} skip_approval={}",
                            offer.attempt_id,
                            offer.from,
                            offer.is_owner,
                            offer.skip_approval
                        );
                        peers.insert(
                            offer.attempt_id.clone(),
                            Introduced {
                                from: offer.from.clone(),
                                name: offer.user.name.clone(),
                            },
                        );
                        // **Signaling says who needs approval, and nothing here
                        // gives one.** An offer that is neither the owner's nor
                        // marked to skip it is held unanswered, as a host holds
                        // one while nobody approves, and declined after
                        // `HOLD_MS` -- unless the switch approves it.
                        if !offer.is_owner && !offer.skip_approval {
                            if approve_shared {
                                lowlat_common::log_info!(
                                    "lowlatd: approving {}, it needs approval and --approve-shared gives it",
                                    offer.attempt_id
                                );
                            } else {
                                lowlat_common::log_info!(
                                    "lowlatd: holding {}, it needs approval and nothing here approves",
                                    offer.attempt_id
                                );
                                held.push((
                                    offer.attempt_id.clone(),
                                    offer.from.clone(),
                                    lowlat_common::clock::Time::now(),
                                ));
                                continue;
                            }
                        }

                        // Admission is the application's decision, and past
                        // approval this application's policy is capacity alone.
                        // **Silence is not a refusal.** A declined answer is a
                        // wire event the peer acts on at once; no answer at all
                        // leaves a peer that keeps no timer of its own
                        // connecting indefinitely, because nothing in the
                        // protocol reports a host that never replied. Every
                        // offer gets an answer, including the ones we turn down
                        // and the ones held for approval.
                        let refusal = if reject_all {
                            Some("policy".to_string())
                        } else {
                            let transport = if offer.data.wants_web() {
                                Transport::Web
                            } else {
                                Transport::Bud
                            };
                            seam.new_attempt(&offer.attempt_id, Peer {
                                ufrag: offer.data.creds.ice_ufrag,
                                pwd: offer.data.creds.ice_pwd,
                                aes256: offer.data.creds.aes256,
                                transport,
                                fingerprint: offer.data.creds.fingerprint,
                                permissions: lowlat_host::inject::Permissions {
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
                            // Empty means the attempt keys from the
                            // fingerprint, and the answer omits the field a
                            // peer of that generation cannot read.
                            aes256: (!host.aes256.is_empty()).then_some(host.aes256),
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
                                    lowlat_host::admission::Kind::Direct,
                                );
                            }
                            Relayed::Probe(addr) => {
                                // Both of the peer's markings, classified in
                                // one place for every caller of the seam.
                                let kind = lowlat_host::admission::Kind::marked(
                                    relay.data.lan,
                                    relay.data.from_stun,
                                );
                                seam.add_candidate(&relay.attempt_id, addr, false, kind);
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
                        held.retain(|(attempt, _, _)| *attempt != cancel.attempt_id);
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
            _ = stop.wait() => {
                #[cfg(windows)]
                end_guests(seam).await;
                lowlat_common::log_info!("lowlatd: stopping, asked by the service");
                return Ok(true);
            }
        }

        // **Watched rather than reported.** A guest asking for a different
        // output and a display moving to another card both change what is
        // being captured, and only the loop that rebuilt knows which happened;
        // this notices either, once it has actually landed.
        announced = app::announce_capture(seam, settings, announced);

        // **Repeated, because what it carries moves.** The room's membership
        // changes on an event and the roster is sent then; the telemetry inside
        // it changes continuously and nothing announces that, so a reader
        // watching a rate or a round trip needs the message again. Sent only
        // while somebody is there to read it.
        if lowlat_common::clock::elapsed_ms(rostered) >= ROSTER_MS && occupancy(seam) > 0 {
            rostered = lowlat_common::clock::Time::now();
            app::announce_guests(seam);
        }

        // **A held offer gets its answer too**, once its hold is over.
        while held
            .first()
            .is_some_and(|(_, _, since)| lowlat_common::clock::elapsed_ms(*since) >= HOLD_MS)
        {
            let (attempt, to, _) = held.remove(0);
            lowlat_common::log_info!("lowlatd: declining {attempt}: not approved in time");
            let empty = no_credentials();
            client.send(
                "answer",
                &Answer {
                    approved: false,
                    attempt_id: &attempt,
                    data: AnswerData {
                        base: HostDataBase::default(),
                        creds: &empty,
                    },
                    to: &to,
                },
            )?;
            peers.remove(&attempt);
        }

        side.pass(seam, settings, peers, established);

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
                    lan,
                } => {
                    let Some(to) = peers.get(&attempt).map(|peer| &peer.from) else {
                        continue;
                    };
                    let kind = if from_stun { "reflexive" } else { "host" };
                    lowlat_common::log_info!("lowlatd: {kind} candidate {addr} for {attempt}");
                    // Both flags copied from the event verbatim: the marking
                    // is the boundary's decision, not this program's.
                    client.send(
                        "candex",
                        &candex(
                            &attempt,
                            to,
                            addr.ip().to_string(),
                            addr.port(),
                            lan,
                            from_stun,
                            false,
                        ),
                    )?;
                }
                Event::Ready { attempt } => {
                    let Some(to) = peers.get(&attempt).map(|peer| &peer.from) else {
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
                    // **The exact bytes for configuration, a length for a
                    // person's own text.** Having the bytes beside the question
                    // is what makes a wrong answer findable, and that reasoning
                    // inverts for the identifiers that carry what somebody
                    // typed or copied: the same line turns this log into a
                    // transcript of a desktop.
                    let printable: String = if app::carries_user_text(id) {
                        String::new()
                    } else {
                        text.iter()
                            .take(120)
                            .map(|byte| {
                                if byte.is_ascii_graphic() || *byte == b' ' {
                                    char::from(*byte)
                                } else {
                                    '.'
                                }
                            })
                            .collect()
                    };
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
mod role_tests {
    use super::{Program, program_of};

    /// **The role is a privilege boundary, so the word only counts in first
    /// position.** A flag is matched wherever it appears in a command line;
    /// this must not be, or a service invoked with an argument somebody else
    /// chose can be turned into a session agent, or the reverse.
    #[test]
    fn only_the_first_argument_selects_the_session_role() {
        assert_eq!(program_of(["session"]), Program::Session);
        assert_eq!(program_of(["session", "--verbose"]), Program::Session);
        assert_eq!(program_of(["tray"]), Program::Tray);
        assert_eq!(program_of(["tray", "--verbose"]), Program::Tray);
        assert_eq!(program_of(["display", "list"]), Program::Display);
        assert_eq!(program_of(["service"]), Program::Service);
        assert_eq!(program_of(["install"]), Program::Install);
        assert_eq!(program_of(["uninstall"]), Program::Uninstall);
        assert_eq!(program_of(["list", "display"]), Program::Host);
        assert_eq!(program_of(["--tray"]), Program::Host);
        assert_eq!(program_of(["--verbose", "tray"]), Program::Host);
        assert_eq!(program_of(["session", "tray"]), Program::Session);

        assert_eq!(program_of([] as [&str; 0]), Program::Host);
        assert_eq!(program_of(["--verbose"]), Program::Host);
        // The word, anywhere but first.
        assert_eq!(program_of(["--name", "session"]), Program::Host);
        assert_eq!(program_of(["--verbose", "session"]), Program::Host);
        assert_eq!(program_of(["--verbose", "service"]), Program::Host);
        // The word as a flag, which is the shape the rule exists to refuse.
        assert_eq!(program_of(["--session"]), Program::Host);
        assert_eq!(
            program_of(["--output", "card0:DP-1", "--session"]),
            Program::Host
        );
    }
}

#[cfg(all(test, windows))]
mod platform_tests {
    use super::is_device_id;

    /// The listing's identifier has the shape the established host sends:
    /// thirty-two hex characters and nothing else.
    #[test]
    fn a_device_id_is_thirty_two_hex_characters() {
        assert!(is_device_id("0123456789abcdef0123456789abcdef"));
        assert!(!is_device_id("0123456789abcdef0123456789abcde"));
        assert!(!is_device_id("0123456789abcdef0123456789abcdeg"));
        assert!(!is_device_id(""));
    }
}

#[cfg(test)]
mod signaling_tests {
    use super::signaling_url;

    /// **An install finishes with the login alone.** The configuration file
    /// ships the server empty and the login fills in the session only, so an
    /// empty server is the public service and not a service left unconfigured.
    #[test]
    fn an_empty_server_is_the_public_service() {
        assert_eq!(signaling_url(""), "wss://kessel-ws.parsec.app");
        assert_eq!(signaling_url("  "), "wss://kessel-ws.parsec.app");
        assert_eq!(signaling_url("relay.example"), "wss://relay.example");
        assert_eq!(signaling_url("ws://127.0.0.1:8080"), "ws://127.0.0.1:8080");
    }
}
