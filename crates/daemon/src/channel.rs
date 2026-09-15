//! The channel the session side connects to (docs/07-platforms.md section 5.1).
//!
//! **The service listens and the session connects outward**, which is what
//! removes the problem rather than solving it: nothing here discovers a
//! session, drops privilege or guesses a desktop, and a connection arrives
//! with an identity because a local socket carries the peer's credentials.
//!
//! **A thread rather than the executor.** The runtime in this program is
//! signaling's, and a socket carrying a handful of messages a second does not
//! need one to read it.

use std::io::{Read as _, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::io::AsRawFd as _;
use std::os::unix::net::{UnixListener, UnixStream};

/// Where the socket is.
///
/// **Known rather than private**, and that is a consequence rather than a
/// preference: a tray started by hand is not started by this service and has
/// nothing to be handed, so a path it cannot find is a tray that cannot
/// connect.
const SOCKET: &str = "/run/lowlatd/session";

/// The version both ends carry in their first frame.
///
/// **A version that is not understood ends the connection** rather than being
/// worked around. Both sides ship in one file, so the only way to see a
/// mismatch is a stale process, and continuing with one is how a stale process
/// becomes a wrong answer.
const VERSION: u64 = 1;

/// The most one frame may carry.
///
/// Sized for the largest thing that will cross: copied text, which is already
/// refused above the ceiling an application message carries.
const MAX_FRAME: usize = 1 << 20;

/// How long a connection has to say what it is.
///
/// **Only the first frame is on a clock.** A helper that has said hello is
/// long lived and silent by design -- it speaks when the session changes --
/// so a deadline after that would drop the quiet ones.
const HELLO_MS: u64 = 5_000;

/// How many session-side programs may be connected at once.
///
/// A helper and a tray, and room for a replacement arriving before its
/// predecessor's socket closes. **Bounded because any local user may connect**
/// (docs/07-platforms.md section 5.1): without a cap, one that opens
/// connections in a loop costs a thread each.
const CONNECTIONS: usize = 8;

/// What a connection says it is.
///
/// **Two roles on one channel, not two channels.** They differ in what they
/// are trusted with rather than in how they speak: a helper makes statements
/// about its own session, a tray acts on the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    Helper,
    Tray,
}

impl Role {
    fn named(name: &str) -> Option<Self> {
        match name {
            "helper" => Some(Self::Helper),
            "tray" => Some(Self::Tray),
            _ => None,
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Helper => "helper",
            Self::Tray => "tray",
        }
    }
}

/// What a session-side program says it can do.
///
/// **Announced rather than assumed, because it varies with the desktop.** The
/// mechanisms behind these differ per display stack and one of them offers no
/// protocol at all, so a helper that cannot do a thing says so and the service
/// answers the honest way instead. This is what makes "absent is not degraded"
/// per customer rather than all or nothing (docs/07-platforms.md section 5.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Can {
    /// Report whether an application has taken the pointer.
    pub(crate) pointer: bool,
    /// Hold the screen awake while it is asked to.
    pub(crate) idle: bool,
    /// Own a selection, in either direction.
    pub(crate) clipboard: bool,
    /// Answer where the displays are.
    pub(crate) layout: bool,
    /// Change an output's mode or turn, on request.
    pub(crate) mode: bool,
}

impl Can {
    /// **Names this build does not know are dropped, not refused.** A newer
    /// session agent naming a fifth thing is one this service will not ask
    /// for, which is not a reason to end the connection -- unlike a version,
    /// which says the framing itself may differ.
    fn named(names: &serde_json::Value) -> Self {
        let mut can = Self::default();
        for name in names.as_array().unwrap_or(&Vec::new()) {
            match name.as_str() {
                Some("pointer") => can.pointer = true,
                Some("idle") => can.idle = true,
                Some("clipboard") => can.clipboard = true,
                Some("layout") => can.layout = true,
                Some("mode") => can.mode = true,
                _ => {}
            }
        }
        can
    }

    fn names(self) -> Vec<&'static str> {
        let mut names = Vec::new();
        for (held, name) in [
            (self.pointer, "pointer"),
            (self.idle, "idle"),
            (self.clipboard, "clipboard"),
            (self.layout, "layout"),
            (self.mode, "mode"),
        ] {
            if held {
                names.push(name);
            }
        }
        names
    }
}

/// Who is on the other end, as the kernel reports them.
///
/// **Recorded on every connection even though nothing gates on them yet.** A
/// host action has to be able to say who asked for it, and the criterion that
/// would read these is deferred rather than absent (docs/07-platforms.md
/// section 5.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Peer {
    pub(crate) pid: i32,
    pub(crate) uid: u32,
    pub(crate) gid: u32,
}

/// What a connection turned out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Greeting {
    pub(crate) role: Role,
    pub(crate) peer: Peer,
    pub(crate) can: Can,
}

/// Start listening, if the socket can be made.
///
/// **Failing to listen is not failing to host.** The stream never depends on
/// anything on this channel, so a service that cannot open it says so and
/// carries on.
pub(crate) fn listen() {
    match bind(SOCKET) {
        Ok(listener) => {
            if let Err(error) = std::thread::Builder::new()
                .name("lowlat-session".to_string())
                .spawn(move || accept(&listener))
            {
                lowlat_common::log_warn!("channel: no thread for the socket, error={error}");
            }
        }
        Err(error) => {
            lowlat_common::log_warn!("channel: not listening, path={SOCKET} error={error}");
        }
    }
}

/// Bind the socket, replacing whatever a previous run left there.
///
/// **World-writable, deliberately.** Any local user may connect, which is a
/// deferral with its cost written down rather than a position
/// (docs/07-platforms.md section 5.1); a mode that said otherwise here would
/// be an authorisation scheme hidden in a file permission.
fn bind(path: &str) -> std::io::Result<UnixListener> {
    if let Some(parent) = std::path::Path::new(path).parent() {
        std::fs::create_dir_all(parent)?;
    }
    // A socket outlives the process that made it, so a restart finds its own.
    // Removing one that a live listener owns is not possible: binding would
    // have failed for that one first.
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666))?;
    lowlat_common::log_info!("channel: listening, path={path}");
    Ok(listener)
}

/// The helpers connected now, one to a session.
///
/// **Keyed by the user, which is the nearest thing to a session this end can
/// see.** Credentials carry a user and a process and nothing about a seat, and
/// a helper's claims are bounded by its credentials in any case. Two sessions
/// belonging to one person is the case this gets wrong, and only one of them
/// is in front of the screen.
static HELPERS: std::sync::Mutex<Vec<Held>> = std::sync::Mutex::new(Vec::new());

/// A helper's place, and the handle that ends it.
#[derive(Debug)]
struct Held {
    uid: u32,
    pid: i32,
    can: Can,
    /// A second reference to the same connection, so a replacement can close
    /// what it replaced. Shutting it down is what wakes that helper's own
    /// thread out of its read.
    handle: UnixStream,
}

/// Take this helper's place, ending whatever held it.
///
/// **Newest wins.** A reconnect replaces its predecessor rather than joining
/// it, because two answers to "has an application taken the pointer" is not a
/// state anything can act on.
fn take_place(peer: Peer, can: Can, stream: &UnixStream) {
    let Ok(handle) = stream.try_clone() else {
        return;
    };
    let Ok(mut live) = HELPERS.lock() else { return };
    if let Some(at) = live.iter().position(|held| held.uid == peer.uid) {
        let mut gone = live.swap_remove(at);
        lowlat_common::log_info!(
            "channel: helper replaced, pid={} by pid={}",
            gone.pid,
            peer.pid
        );
        // **Told why before it is closed.** A helper reconnects when it loses
        // the socket, because a service restarting is the ordinary reason to
        // lose one; a helper that came back after being replaced would displace
        // its own replacement, and the two would trade the place forever. Only
        // this end can tell the two closes apart, so only this end can say.
        let _ = write_frame(&mut gone.handle, BYE_REPLACED);
        let _ = gone.handle.shutdown(std::net::Shutdown::Both);
    }
    live.push(Held {
        uid: peer.uid,
        pid: peer.pid,
        can,
        handle,
    });
}

/// Give up a place, if this connection still holds one.
///
/// **By process rather than by user**, so a helper that was already replaced
/// does not take its replacement's place away on the way out.
fn leave_place(peer: Peer) {
    if let Ok(mut live) = HELPERS.lock() {
        live.retain(|held| held.pid != peer.pid);
    }
}

/// The trays connected now.
///
/// **Any number, unlike helpers.** A tray is shown state and asks for
/// actions, and two of them showing the same room is not a contradiction the
/// way two answers about one session would be.
static TRAYS: std::sync::Mutex<Vec<Held>> = std::sync::Mutex::new(Vec::new());

/// The last state the service published, for a tray that connects between
/// two changes.
static STATE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Take a tray on, telling it what the service last said.
///
/// **Told on connect rather than on the next change**, because a tray
/// started by hand against a quiet stream would otherwise show nothing until
/// something happened.
fn attach(peer: Peer, stream: &UnixStream) {
    let Ok(mut handle) = stream.try_clone() else {
        return;
    };
    if let Ok(kept) = STATE.lock()
        && let Some(body) = kept.as_ref()
    {
        let _ = write_frame(&mut handle, body.as_bytes());
    }
    if let Ok(mut live) = TRAYS.lock() {
        live.push(Held {
            uid: peer.uid,
            pid: peer.pid,
            can: Can::default(),
            handle,
        });
    }
}

fn detach(peer: Peer) {
    if let Ok(mut live) = TRAYS.lock() {
        live.retain(|held| held.pid != peer.pid);
    }
}

/// How many trays are connected, so the state is not worked out for nobody.
pub(crate) fn trays() -> usize {
    TRAYS.lock().map_or(0, |live| live.len())
}

/// Tell every tray what the host is doing, if it differs from the last time.
///
/// **Compared here rather than by the caller**, so the rule that a state
/// repeated is not a change lives in one place; the loop says what it sees
/// on every pass and this decides whether anybody needs to hear it.
pub(crate) fn state(state: &serde_json::Value) {
    let body = serde_json::json!({ "state": state }).to_string();
    let Ok(mut kept) = STATE.lock() else { return };
    if kept.as_ref() == Some(&body) {
        return;
    }
    *kept = Some(body.clone());
    let Ok(mut live) = TRAYS.lock() else { return };
    for held in live.iter_mut() {
        if let Err(error) = write_frame(&mut held.handle, body.as_bytes()) {
            lowlat_common::log_warn!("channel: tray unreachable, pid={} error={error}", held.pid);
        }
    }
}

/// What a tray has asked for that the service has not acted on yet.
///
/// **With the credentials of the connection that asked**, because a host
/// action has to be able to say who asked for it, and the criterion that
/// would refuse one is deferred rather than absent (docs/07-platforms.md
/// section 5.1).
static ACTED: std::sync::Mutex<Vec<(Peer, Vec<u8>)>> = std::sync::Mutex::new(Vec::new());

/// How many unread actions are kept. A person clicks slower than the loop
/// reads, so the cap is a bound on a tray that is not a person.
const ACTED_MAX: usize = 8;

fn act(peer: Peer, body: &[u8]) {
    let Ok(mut acted) = ACTED.lock() else { return };
    if acted.len() >= ACTED_MAX {
        acted.remove(0);
        lowlat_common::log_warn!("channel: a tray is asking faster than this reads");
    }
    acted.push((peer, body.to_vec()));
}

/// Take everything the trays have asked for since the last time this was
/// asked, each with who asked.
pub(crate) fn take_acted() -> Vec<(Peer, Vec<u8>)> {
    ACTED
        .lock()
        .map(|mut acted| std::mem::take(&mut *acted))
        .unwrap_or_default()
}

/// A tray's request to end one guest.
pub(crate) fn kick(guest: u32) -> Vec<u8> {
    serde_json::json!({ "kick": guest })
        .to_string()
        .into_bytes()
}

pub(crate) fn is_kick(body: &[u8]) -> Option<u32> {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()?
        .get("kick")?
        .as_u64()
        .and_then(|guest| u32::try_from(guest).ok())
}

/// A tray's request to change the stream, in the shape a guest's own
/// request has, so the two are answered by one rule.
pub(crate) fn config(config: &serde_json::Value) -> Vec<u8> {
    serde_json::json!({ "config": config })
        .to_string()
        .into_bytes()
}

/// The configuration a frame carries, as bytes for the same reader a guest's
/// goes to.
pub(crate) fn is_config(body: &[u8]) -> Option<Vec<u8>> {
    let config = serde_json::from_slice::<serde_json::Value>(body)
        .ok()?
        .get("config")?
        .clone();
    Some(config.to_string().into_bytes())
}

/// The state a frame carries, as the service described it.
pub(crate) fn is_state(body: &[u8]) -> Option<serde_json::Value> {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()?
        .get("state")
        .cloned()
}

fn accept(listener: &UnixListener) {
    let live = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        if live.fetch_add(1, std::sync::atomic::Ordering::Relaxed) >= CONNECTIONS {
            live.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            lowlat_common::log_warn!("channel: refused, connections={CONNECTIONS}");
            continue;
        }
        let held = std::sync::Arc::clone(&live);
        if std::thread::Builder::new()
            .name("lowlat-session".to_string())
            .spawn(move || {
                serve(stream);
                held.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            })
            .is_err()
        {
            live.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

/// One connection, from its first frame to the end of it.
fn serve(mut stream: UnixStream) {
    let Some(greeting) = greet(&mut stream) else {
        return;
    };
    lowlat_common::log_info!(
        "channel: {} connected, pid={} uid={} gid={} can={:?}",
        greeting.role.name(),
        greeting.peer.pid,
        greeting.peer.uid,
        greeting.peer.gid,
        greeting.can.names()
    );
    match greeting.role {
        Role::Helper => take_place(greeting.peer, greeting.can, &stream),
        Role::Tray => attach(greeting.peer, &stream),
    }
    let mut body = Vec::new();
    while read_frame(&mut stream, &mut body).is_ok() {
        lowlat_common::log_debug!(
            "channel: {} said {} bytes",
            greeting.role.name(),
            body.len()
        );
        // **Only a helper speaks for a session, and only a tray acts on the
        // host.** The two are different questions with different answers, so
        // what each says goes to its own queue and is read as what it is
        // (docs/07-platforms.md section 5.1).
        match greeting.role {
            Role::Helper => say(greeting.peer.uid, &body),
            Role::Tray => act(greeting.peer, &body),
        }
    }
    match greeting.role {
        Role::Helper => leave_place(greeting.peer),
        Role::Tray => detach(greeting.peer),
    }
    lowlat_common::log_info!(
        "channel: {} gone, pid={}",
        greeting.role.name(),
        greeting.peer.pid
    );
}

/// Read the first frame and decide whether there is a conversation.
///
/// Answers `None` for every way there is not, all of which end the connection:
/// no credentials, nothing said before the deadline, a version this build does
/// not speak, or a role it does not know.
pub(crate) fn greet(stream: &mut UnixStream) -> Option<Greeting> {
    let peer = peer_of(stream)?;
    stream
        .set_read_timeout(Some(std::time::Duration::from_millis(HELLO_MS)))
        .ok()?;
    let mut body = Vec::new();
    if let Err(error) = read_frame(stream, &mut body) {
        lowlat_common::log_warn!("channel: no hello, pid={} error={error}", peer.pid);
        return None;
    }
    // **Cleared once it has said what it is.** The deadline is on being
    // announced, not on having something to say.
    stream.set_read_timeout(None).ok()?;

    let Ok(hello) = serde_json::from_slice::<serde_json::Value>(&body) else {
        lowlat_common::log_warn!("channel: hello is not readable, pid={}", peer.pid);
        return None;
    };
    let version = hello.get("version").and_then(serde_json::Value::as_u64);
    if version != Some(VERSION) {
        lowlat_common::log_warn!(
            "channel: hello speaks {:?} and this build speaks {VERSION}, pid={}",
            version,
            peer.pid
        );
        return None;
    }
    let role = hello
        .get("role")
        .and_then(serde_json::Value::as_str)
        .and_then(Role::named);
    let Some(role) = role else {
        lowlat_common::log_warn!(
            "channel: hello names no role this build has, pid={}",
            peer.pid
        );
        return None;
    };
    let can = hello.get("can").map_or_else(Can::default, Can::named);
    Some(Greeting { role, peer, can })
}

/// Tell the session whether somebody is watching this machine.
///
/// **A push rather than a question**, because it is a state the service owns
/// and the session acts on: asking would mean waiting on a process in
/// somebody's session for an answer nothing here needs.
///
/// **Nothing happens when there is no helper**, which is the honest answer
/// rather than a degraded one: with nobody in the session to ask, the screen
/// does what the desktop decides.
pub(crate) fn screen_awake(awake: bool) {
    let body = serde_json::json!({ "awake": awake }).to_string();
    let Ok(mut live) = HELPERS.lock() else { return };
    for held in live.iter_mut() {
        if let Err(error) = write_frame(&mut held.handle, body.as_bytes()) {
            lowlat_common::log_warn!(
                "channel: helper unreachable, pid={} error={error}",
                held.pid
            );
        }
    }
}

/// A request the service has put to the session and not heard back on.
///
/// **One at a time, and on a clock.** The channel's only request so far is
/// a mode change, which a guest asks for rarely and a session answers in
/// well under a second; a second one arriving before the first is answered
/// is refused rather than queued, because the two would be about the same
/// display. The deadline is the rule every request carries
/// (docs/07-platforms.md section 5.1): a helper that has stopped answering
/// is dropped rather than waited for.
static ASKED: std::sync::Mutex<Option<Asked>> = std::sync::Mutex::new(None);

#[derive(Debug, Clone, Copy)]
struct Asked {
    id: u64,
    pid: i32,
    since: std::time::Instant,
}

static NEXT_ASK: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// How long a session has to answer a request.
///
/// A mode change was measured at a fifth of a second on the desktop this was
/// built against, and a display on a real link may re-train for a second or
/// two; the helper's own wait on the compositor is shorter than this, so a
/// session that is merely slow answers with a refusal rather than silence.
const ASK_MS: u64 = 5_000;

/// What a mode request names. Either half may be absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ModeAsk<'a> {
    pub(crate) output: &'a str,
    pub(crate) size: Option<(u32, u32)>,
    /// The turn as the video header spells it, one-based.
    pub(crate) rotation: Option<u8>,
}

/// Ask the session to change an output's mode or turn.
///
/// **Refused with a reason where there is nobody to ask**, which is the
/// honest answer: a session with no helper, or a helper whose desktop offers
/// no mechanism, cannot be made to answer by waiting. The answer arrives
/// later on the queue below and is read by the loop that asked.
pub(crate) fn ask_mode(ask: ModeAsk<'_>) -> Result<(), &'static str> {
    let Ok(mut asked) = ASKED.lock() else {
        return Err("the channel is poisoned");
    };
    if asked.is_some() {
        return Err("a request is already outstanding");
    }
    let Ok(mut live) = HELPERS.lock() else {
        return Err("the channel is poisoned");
    };
    let Some(held) = live.iter_mut().find(|held| held.can.mode) else {
        return Err(if live.is_empty() {
            "no session helper is connected"
        } else {
            "the session cannot set a mode"
        });
    };
    // Distinct from every earlier one, so a late answer cannot be taken for
    // a current one.
    let id = NEXT_ASK.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let body = serde_json::json!({
        "ask": id,
        "mode": {
            "output": ask.output,
            "width": ask.size.map(|(width, _)| width),
            "height": ask.size.map(|(_, height)| height),
            "rotation": ask.rotation,
        },
    })
    .to_string();
    if write_frame(&mut held.handle, body.as_bytes()).is_err() {
        return Err("the session helper is unreachable");
    }
    *asked = Some(Asked {
        id,
        pid: held.pid,
        since: std::time::Instant::now(),
    });
    Ok(())
}

/// A mode request as the session reads it, with the id its answer names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModeAsked {
    pub(crate) id: u64,
    pub(crate) output: String,
    pub(crate) size: Option<(u32, u32)>,
    pub(crate) rotation: Option<u8>,
}

/// Whether a frame is the service asking for a mode, and what it asks.
pub(crate) fn is_mode_ask(body: &[u8]) -> Option<ModeAsked> {
    let parsed = serde_json::from_slice::<serde_json::Value>(body).ok()?;
    let id = parsed.get("ask")?.as_u64()?;
    let mode = parsed.get("mode")?;
    let output = mode.get("output")?.as_str()?.to_owned();
    let dimension = |field: &str| {
        mode.get(field)
            .and_then(serde_json::Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
    };
    let size = match (dimension("width"), dimension("height")) {
        (Some(width), Some(height)) => Some((width, height)),
        _ => None,
    };
    let rotation = mode
        .get("rotation")
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| u8::try_from(value).ok());
    Some(ModeAsked {
        id,
        output,
        size,
        rotation,
    })
}

/// The session's answer to a request.
pub(crate) fn reply(id: u64, outcome: &Result<(), String>) -> Vec<u8> {
    match outcome {
        Ok(()) => serde_json::json!({ "reply": id, "ok": true }),
        Err(reason) => serde_json::json!({ "reply": id, "error": reason }),
    }
    .to_string()
    .into_bytes()
}

/// Whether a frame answers the request outstanding, and how.
///
/// **An answer to a request nobody is waiting on is dropped**, which is what
/// a reply arriving after its deadline is: the helper it came from has
/// already been let go.
pub(crate) fn is_reply(body: &[u8]) -> Option<Result<(), String>> {
    let parsed = serde_json::from_slice::<serde_json::Value>(body).ok()?;
    let id = parsed.get("reply")?.as_u64()?;
    let mut asked = ASKED.lock().ok()?;
    if asked.map(|asked| asked.id) != Some(id) {
        return None;
    }
    *asked = None;
    Some(
        match parsed.get("error").and_then(serde_json::Value::as_str) {
            Some(reason) => Err(reason.to_owned()),
            None => Ok(()),
        },
    )
}

/// Let go of a helper that has not answered in time.
///
/// **Called from the loop that asked, every pass.** The connection is shut
/// from this end, which wakes the helper's own thread out of its read and
/// records it as gone; a helper that is still alive reconnects and is a new
/// helper, with nothing outstanding.
pub(crate) fn drop_overdue() {
    let Ok(mut asked) = ASKED.lock() else { return };
    let Some(pending) = *asked else { return };
    if pending.since.elapsed() < std::time::Duration::from_millis(ASK_MS) {
        return;
    }
    *asked = None;
    lowlat_common::log_warn!(
        "channel: helper did not answer within {ASK_MS} ms, dropped, pid={}",
        pending.pid
    );
    if let Ok(mut live) = HELPERS.lock()
        && let Some(at) = live.iter().position(|held| held.pid == pending.pid)
    {
        let gone = live.swap_remove(at);
        let _ = gone.handle.shutdown(std::net::Shutdown::Both);
    }
}

/// What a session has said that the service has not acted on yet.
///
/// **A queue rather than a call, because the two ends are different threads
/// and only one of them may touch the seam.** The channel's own threads read
/// the socket; the loop that owns the guests drains this.
static SAID: std::sync::Mutex<Vec<(u32, Vec<u8>)>> = std::sync::Mutex::new(Vec::new());

/// How many unread things from a session are kept.
///
/// **Small, and the oldest is what goes.** These are clipboard contents, where
/// the newest is the only one anybody wants; a queue that grew instead would
/// let a session agent spend this program's memory.
const SAID_MAX: usize = 4;

fn say(uid: u32, body: &[u8]) {
    let Ok(mut said) = SAID.lock() else { return };
    if said.len() >= SAID_MAX {
        said.remove(0);
        lowlat_common::log_warn!("channel: the session is talking faster than this reads");
    }
    said.push((uid, body.to_vec()));
}

/// Take everything a session has said since the last time this was asked,
/// each with the account whose session said it.
///
/// **The account matters for one thing it says.** A layout describes the
/// desktop of the session that pushed it, and whether that session is the one
/// in front of the display is decided by the reader, not here.
pub(crate) fn take_said() -> Vec<(u32, Vec<u8>)> {
    SAID.lock()
        .map(|mut said| std::mem::take(&mut *said))
        .unwrap_or_default()
}

/// Whether a helper for this account holds a place right now.
pub(crate) fn helper_for(uid: u32) -> bool {
    HELPERS
        .lock()
        .map(|live| live.iter().any(|held| held.uid == uid))
        .unwrap_or(false)
}

/// Hand the session a guest's copied text to put on its clipboard.
///
/// **Nothing happens when there is no helper**, and the caller is told so: a
/// selection can only be owned from inside a session, so with nobody there the
/// text has nowhere to go and saying it arrived would be a lie.
pub(crate) fn clipboard(text: &[u8]) -> bool {
    // **Any terminator is taken off here.** The wire counts one and the layer
    // that reads it takes one off, but a peer that sent two would otherwise
    // put a stray byte on somebody's desktop clipboard, where it is invisible
    // until it is pasted into something that minds.
    let text = text.strip_suffix(&[0]).unwrap_or(text);
    let body =
        serde_json::json!({ "clipboard": String::from_utf8_lossy(text).as_ref() }).to_string();
    let Ok(mut live) = HELPERS.lock() else {
        return false;
    };
    let mut reached = false;
    for held in live.iter_mut() {
        reached |= write_frame(&mut held.handle, body.as_bytes()).is_ok();
    }
    reached
}

/// The copied text a frame carries, in either direction.
pub(crate) fn is_clipboard(body: &[u8]) -> Option<String> {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()?
        .get("clipboard")?
        .as_str()
        .map(str::to_owned)
}

/// The layout a frame carries, as the session laid it out.
///
/// **Every field is optional on the way in as it is on the way out.** A
/// session describes an output in pieces, and the reduction to a placement
/// already drops one that is not fully described rather than defaulting it.
pub(crate) fn is_layout(body: &[u8]) -> Option<Vec<lowlat_host::capture::Output>> {
    let parsed = serde_json::from_slice::<serde_json::Value>(body).ok()?;
    let listed = parsed.get("layout")?.as_array()?;
    let read = |output: &serde_json::Value, field: &str| output.get(field).cloned();
    Some(
        listed
            .iter()
            .map(|output| lowlat_host::capture::Output {
                name: read(output, "name").and_then(|value| value.as_str().map(str::to_owned)),
                x: read(output, "x")
                    .and_then(|value| value.as_i64())
                    .and_then(|value| i32::try_from(value).ok()),
                y: read(output, "y")
                    .and_then(|value| value.as_i64())
                    .and_then(|value| i32::try_from(value).ok()),
                width: read(output, "width")
                    .and_then(|value| value.as_u64())
                    .and_then(|value| u32::try_from(value).ok()),
                height: read(output, "height")
                    .and_then(|value| value.as_u64())
                    .and_then(|value| u32::try_from(value).ok()),
                transform: read(output, "transform")
                    .and_then(|value| value.as_u64())
                    .and_then(|value| u32::try_from(value).ok()),
            })
            .collect(),
    )
}

/// Say what this session's displays are and where they sit.
pub(crate) fn layout(outputs: &[lowlat_host::capture::Output]) -> String {
    let described: Vec<serde_json::Value> = outputs
        .iter()
        .map(|output| {
            serde_json::json!({
                "name": output.name,
                "x": output.x,
                "y": output.y,
                "width": output.width,
                "height": output.height,
                "transform": output.transform,
            })
        })
        .collect();
    serde_json::json!({ "layout": described }).to_string()
}

/// Whether a frame is the service saying somebody is or is not watching.
pub(crate) fn is_awake(body: &[u8]) -> Option<bool> {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()?
        .get("awake")?
        .as_bool()
}

/// What the service says before closing a connection it is ending on purpose.
///
/// **A reason rather than a bare close**, because the two closes a session
/// agent sees mean opposite things: a service that went away is one to wait
/// for, and a place given to somebody newer is not.
pub(crate) const BYE_REPLACED: &[u8] = br#"{"bye":"replaced"}"#;

/// Whether a frame is the service ending this connection on purpose.
pub(crate) fn is_bye(body: &[u8]) -> Option<String> {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()?
        .get("bye")?
        .as_str()
        .map(str::to_owned)
}

/// Connect to the service as `role` and announce this end.
///
/// **The session side connects outward**, which is the whole reason this
/// channel has the shape it does: a service cannot reach into a desktop
/// session, and a session reaching out arrives with an identity.
pub(crate) fn connect(role: Role, can: Can) -> std::io::Result<UnixStream> {
    let mut stream = UnixStream::connect(SOCKET)?;
    write_frame(&mut stream, &hello(role, can))?;
    Ok(stream)
}

/// This end's own first frame.
pub(crate) fn hello(role: Role, can: Can) -> Vec<u8> {
    serde_json::json!({ "version": VERSION, "role": role.name(), "can": can.names() })
        .to_string()
        .into_bytes()
}

/// Who opened this connection.
fn peer_of(stream: &UnixStream) -> Option<Peer> {
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    #[allow(
        clippy::cast_possible_truncation,
        reason = "the size of a ucred is far inside a socklen"
    )]
    let mut len = size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: the option writes a ucred, which is what is passed, and the
    // length says how much room it has.
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            std::ptr::from_mut(&mut cred).cast::<libc::c_void>(),
            &raw mut len,
        )
    };
    (rc == 0).then_some(Peer {
        pid: cred.pid,
        uid: cred.uid,
        gid: cred.gid,
    })
}

/// One frame in, into a buffer the caller reuses.
pub(crate) fn read_frame(stream: &mut UnixStream, into: &mut Vec<u8>) -> std::io::Result<()> {
    let mut len = [0u8; 4];
    stream.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "frame over the cap",
        ));
    }
    into.clear();
    into.resize(len, 0);
    stream.read_exact(into)
}

/// One frame out.
pub(crate) fn write_frame(stream: &mut UnixStream, body: &[u8]) -> std::io::Result<()> {
    if body.len() > MAX_FRAME {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "frame over the cap",
        ));
    }
    #[allow(
        clippy::cast_possible_truncation,
        reason = "the cap above is far inside a u32"
    )]
    let len = (body.len() as u32).to_le_bytes();
    stream.write_all(&len)?;
    stream.write_all(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair() -> (UnixStream, UnixStream) {
        UnixStream::pair().expect("a socket pair")
    }

    /// **The register of helpers is one static for the program, and what the
    /// service pushes goes to all of them.** That is right for a service and
    /// wrong for tests running side by side in one process, where one test's
    /// lease lands in another's socket. The tests that register take this
    /// first.
    static ONE_AT_A_TIME: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn alone() -> std::sync::MutexGuard<'static, ()> {
        ONE_AT_A_TIME
            .lock()
            .unwrap_or_else(|held| held.into_inner())
    }

    /// **A real accepted connection, not a socket pair.** The credentials come
    /// from the kernel's own account of who opened the socket, and a pair is
    /// this process on both ends however it is read: only a bound path
    /// exercises the bind, the mode it is left with, and the accept.
    #[test]
    fn a_bound_socket_accepts_and_is_reachable_by_anyone() {
        let path = std::env::temp_dir().join(format!("lowlat-channel-{}", std::process::id()));
        let path = path.to_str().expect("a printable path").to_string();
        let listener = bind(&path).expect("bound");

        let joined = std::thread::spawn({
            let path = path.clone();
            move || {
                let mut client = UnixStream::connect(&path).expect("connected");
                write_frame(&mut client, &hello(Role::Tray, Can::default())).expect("written");
                // Held open until the far side has read it.
                let mut ignored = Vec::new();
                let _ = read_frame(&mut client, &mut ignored);
            }
        });

        let (mut accepted, _) = listener.accept().expect("accepted");
        let greeting = greet(&mut accepted).expect("greeted");
        assert_eq!(greeting.role, Role::Tray);
        assert_eq!(greeting.peer.uid, unsafe { libc::getuid() });

        let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
        assert_eq!(mode & 0o777, 0o666, "a local user could not connect");

        drop(accepted);
        joined.join().expect("the client thread");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_frame_arrives_as_it_left() {
        let (mut a, mut b) = pair();
        write_frame(&mut a, b"hello").expect("written");
        write_frame(&mut a, &[]).expect("written");
        let mut body = Vec::new();
        read_frame(&mut b, &mut body).expect("read");
        assert_eq!(body, b"hello");
        read_frame(&mut b, &mut body).expect("read");
        assert!(body.is_empty(), "an empty frame is still a frame");
    }

    /// A length is the sender's claim, not this end's allocation plan.
    #[test]
    fn a_length_over_the_cap_is_refused_before_it_is_believed() {
        let (mut a, mut b) = pair();
        #[allow(clippy::cast_possible_truncation)]
        let len = ((MAX_FRAME + 1) as u32).to_le_bytes();
        a.write_all(&len).expect("written");
        let mut body = Vec::new();
        let error = read_frame(&mut b, &mut body).expect_err("refused");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    /// A helper that found half of what a session can offer, which is the
    /// ordinary case: the mechanisms differ per desktop and one of them offers
    /// no protocol at all.
    const ANNOUNCED: Can = Can {
        pointer: true,
        idle: false,
        clipboard: true,
        layout: false,
        mode: false,
    };

    #[test]
    fn a_hello_names_a_role_a_version_and_what_it_can_do() {
        let (mut a, mut b) = pair();
        write_frame(&mut a, &hello(Role::Helper, ANNOUNCED)).expect("written");
        let greeting = greet(&mut b).expect("greeted");
        assert_eq!(greeting.role, Role::Helper);
        assert_eq!(greeting.can, ANNOUNCED);
        // A local socket pair is this process on both ends.
        assert_eq!(greeting.peer.pid, std::process::id().cast_signed());
    }

    /// **A version that is not understood ends the connection.** Both sides
    /// ship in one file, so a mismatch is a stale process, and carrying on
    /// with one is how a stale process becomes a wrong answer.
    #[test]
    fn a_hello_from_another_version_is_refused() {
        for body in [
            serde_json::json!({ "version": VERSION + 1, "role": "helper" }),
            serde_json::json!({ "version": VERSION, "role": "something else" }),
            serde_json::json!({ "role": "helper" }),
            serde_json::json!({ "version": VERSION }),
        ] {
            let (mut a, mut b) = pair();
            write_frame(&mut a, body.to_string().as_bytes()).expect("written");
            assert!(greet(&mut b).is_none(), "accepted {body}");
        }
    }

    /// **A name this build does not know is dropped, not refused.** A newer
    /// session agent offering a fifth thing is one this service will not ask
    /// for, which is not a reason to end the connection -- unlike a version,
    /// which says the framing itself may differ.
    #[test]
    fn a_capability_this_build_does_not_know_is_passed_over() {
        let (mut a, mut b) = pair();
        let body = serde_json::json!({
            "version": VERSION,
            "role": "helper",
            "can": ["idle", "something newer"],
        });
        write_frame(&mut a, body.to_string().as_bytes()).expect("written");
        let greeting = greet(&mut b).expect("greeted");
        assert_eq!(
            greeting.can,
            Can {
                idle: true,
                ..Can::default()
            }
        );
    }

    /// **Newest wins.** A reconnect replaces its predecessor rather than
    /// joining it, and the replacement is what wakes the old one out of its
    /// read: two answers to one question about a session is not a state
    /// anything can act on.
    #[test]
    fn a_second_helper_for_one_session_replaces_the_first() {
        let _alone = alone();
        let peer = Peer {
            pid: 4242,
            uid: 1000,
            gid: 1000,
        };
        let later = Peer { pid: 4243, ..peer };
        let (first, mut first_far) = pair();
        let (second, _second_far) = pair();

        take_place(peer, Can::default(), &first);
        assert!(holds(peer.pid));
        take_place(later, Can::default(), &second);
        // **By process rather than by count.** The register is one static for
        // the program, so a test that counts what is in it counts whatever
        // another test on another thread put there too.
        assert!(!holds(peer.pid), "the replaced helper still holds a place");
        assert!(holds(later.pid));

        // **The replaced helper is told why, then closed.** Being told is what
        // stops it coming back and displacing its own replacement.
        //
        // **On a clock, and the kind is asserted**, or a replacement that
        // failed to close anything would leave this blocked forever and read
        // as a hang rather than as the failure it is.
        first_far
            .set_read_timeout(Some(std::time::Duration::from_millis(500)))
            .expect("a deadline");
        let mut body = Vec::new();
        read_frame(&mut first_far, &mut body).expect("a reason");
        assert_eq!(is_bye(&body).as_deref(), Some("replaced"));
        let ended = read_frame(&mut first_far, &mut body).expect_err("still open");
        assert_eq!(
            ended.kind(),
            std::io::ErrorKind::UnexpectedEof,
            "the replaced connection was left open"
        );

        // The one that was already replaced must not take its replacement's
        // place away on the way out.
        leave_place(peer);
        assert!(holds(later.pid), "the replacement's place was taken away");
        leave_place(later);
        assert!(!holds(later.pid));
    }

    /// **A request goes to the helper that can answer it, is refused with a
    /// reason where none can, and is answered by the frame that names it.**
    /// The refusals are the "absent is not degraded" rule for a request: a
    /// session with no helper, or one whose desktop has no mechanism, cannot
    /// be made to answer by waiting.
    #[test]
    fn a_mode_request_reaches_a_helper_that_can_and_is_refused_otherwise() {
        let _alone = alone();
        let _ = ASKED.lock().map(|mut asked| *asked = None);
        let ask = ModeAsk {
            output: "DP-1",
            size: Some((1920, 1080)),
            rotation: Some(2),
        };
        // A stale entry from another test is not "no helper" but is still
        // not one that can set a mode, so only the reason differs.
        assert!(
            ask_mode(ask).is_err(),
            "nothing connected and something was asked"
        );

        let peer = Peer {
            pid: 5151,
            uid: 1000,
            gid: 1000,
        };
        let (unable, _unable_far) = pair();
        take_place(peer, Can::default(), &unable);
        assert_eq!(ask_mode(ask), Err("the session cannot set a mode"));

        let able = Peer { pid: 5152, ..peer };
        let (stream, mut far) = pair();
        take_place(
            able,
            Can {
                mode: true,
                ..Can::default()
            },
            &stream,
        );
        ask_mode(ask).expect("asked");
        assert_eq!(
            ask_mode(ask),
            Err("a request is already outstanding"),
            "two requests about one display were both put"
        );

        far.set_read_timeout(Some(std::time::Duration::from_millis(500)))
            .expect("a deadline");
        let mut body = Vec::new();
        read_frame(&mut far, &mut body).expect("the request");
        let asked = is_mode_ask(&body).expect("a mode request");
        assert_eq!(
            (asked.output.as_str(), asked.size, asked.rotation),
            ("DP-1", Some((1920, 1080)), Some(2))
        );

        // **A stray answer names nothing outstanding and is dropped**; the
        // right one clears the request and carries the session's reason.
        assert_eq!(is_reply(&reply(asked.id + 1, &Ok(()))), None);
        assert_eq!(
            is_reply(&reply(asked.id, &Err("no such mode".to_string()))),
            Some(Err("no such mode".to_string()))
        );
        assert_eq!(is_reply(&reply(asked.id, &Ok(()))), None, "answered twice");

        // Nothing on the channel that is not one reads as one.
        assert_eq!(is_mode_ask(BYE_REPLACED), None);
        assert_eq!(is_reply(BYE_REPLACED), None);

        leave_place(peer);
        leave_place(able);
    }

    /// **A helper that stops answering is dropped rather than waited for.**
    /// The deadline is observed from the loop that asked, and dropping is a
    /// shutdown of the connection, which is what the helper's own thread
    /// notices.
    #[test]
    fn a_helper_that_does_not_answer_in_time_is_dropped() {
        let _alone = alone();
        let peer = Peer {
            pid: 5153,
            uid: 1000,
            gid: 1000,
        };
        let (stream, mut far) = pair();
        take_place(
            peer,
            Can {
                mode: true,
                ..Can::default()
            },
            &stream,
        );
        let _ = ASKED.lock().map(|mut asked| *asked = None);
        ask_mode(ModeAsk {
            output: "DP-1",
            size: None,
            rotation: Some(1),
        })
        .expect("asked");

        drop_overdue();
        assert!(holds(peer.pid), "dropped before its deadline");
        if let Ok(mut asked) = ASKED.lock()
            && let Some(pending) = asked.as_mut()
        {
            pending.since -= std::time::Duration::from_millis(ASK_MS + 1);
        }
        drop_overdue();
        assert!(!holds(peer.pid), "kept past its deadline");
        assert!(ASKED.lock().expect("held").is_none());

        far.set_read_timeout(Some(std::time::Duration::from_millis(500)))
            .expect("a deadline");
        let mut body = Vec::new();
        read_frame(&mut far, &mut body).expect("the request went out");
        let ended = read_frame(&mut far, &mut body).expect_err("still open");
        assert_eq!(ended.kind(), std::io::ErrorKind::UnexpectedEof);
    }

    /// Whether a process holds a place right now.
    fn holds(pid: i32) -> bool {
        HELPERS
            .lock()
            .expect("held")
            .iter()
            .any(|held| held.pid == pid)
    }

    /// The lease reaches whichever helper holds the place, and reads back as
    /// the state it was sent as.
    #[test]
    fn the_screen_lease_reaches_the_helper_that_holds_the_place() {
        let _alone = alone();
        let peer = Peer {
            pid: 5150,
            uid: 1001,
            gid: 1001,
        };
        let (near, mut far) = pair();
        take_place(peer, Can::default(), &near);

        let mut body = Vec::new();
        screen_awake(true);
        read_frame(&mut far, &mut body).expect("a lease");
        assert_eq!(is_awake(&body), Some(true));
        screen_awake(false);
        read_frame(&mut far, &mut body).expect("a release");
        assert_eq!(is_awake(&body), Some(false));

        // Anything else on the channel is not a lease and must not read as one.
        assert_eq!(is_awake(BYE_REPLACED), None);
        assert_eq!(is_awake(&hello(Role::Helper, Can::default())), None);

        leave_place(peer);
    }

    /// **A stray terminator does not reach a desktop's clipboard.** The wire
    /// counts one and the layer that reads it takes one off, so a peer that
    /// sent two would otherwise put a byte on somebody's clipboard that is
    /// invisible until it is pasted into something that minds.
    #[test]
    fn copied_text_reaches_the_session_without_its_terminator() {
        let _alone = alone();
        let peer = Peer {
            pid: 5151,
            uid: 1002,
            gid: 1002,
        };
        let (near, mut far) = pair();
        take_place(peer, Can::default(), &near);

        assert!(clipboard(b"a link\0"));
        let mut body = Vec::new();
        read_frame(&mut far, &mut body).expect("copied text");
        assert_eq!(is_clipboard(&body).as_deref(), Some("a link"));

        assert!(clipboard(b"no terminator"));
        read_frame(&mut far, &mut body).expect("copied text");
        assert_eq!(is_clipboard(&body).as_deref(), Some("no terminator"));

        // A lease is not copied text and copied text is not a lease.
        assert_eq!(is_clipboard(&hello(Role::Helper, Can::default())), None);
        assert_eq!(is_awake(&body), None);

        leave_place(peer);
        // With nobody in the session there is nowhere for it to go, and the
        // caller is told rather than left to assume it arrived.
        assert!(!clipboard(b"into the void"));
    }

    /// **The layout survives the crossing intact**, because what is done with
    /// it is a bounding box: an output that arrived with a field missing is
    /// dropped from that box rather than defaulted into it, and one silently
    /// invented at the origin makes the desktop bigger than it is.
    #[test]
    fn a_layout_crosses_as_the_session_described_it() {
        let described = vec![
            lowlat_host::capture::Output {
                name: Some("DP-7".to_string()),
                x: Some(0),
                y: Some(0),
                width: Some(2560),
                height: Some(1440),
                transform: Some(0),
            },
            lowlat_host::capture::Output {
                name: Some("HDMI-A-1".to_string()),
                x: Some(2560),
                y: Some(0),
                width: Some(1080),
                height: Some(1920),
                transform: Some(1),
            },
            // Half described, which is an ordinary intermediate state.
            lowlat_host::capture::Output {
                name: Some("DP-4".to_string()),
                ..lowlat_host::capture::Output::default()
            },
        ];
        let crossed = is_layout(layout(&described).as_bytes()).expect("a layout");
        assert_eq!(crossed, described);

        // And the thing it exists for: the desktop is the bounding box of what
        // is fully described, so the captured output knows how wide the axis
        // its input is spread over really is.
        let place = lowlat_host::capture::place(&crossed, "HDMI-A-1").expect("placed");
        assert_eq!((place.x, place.width), (2560, 1080));
        assert_eq!(place.desktop_width, 3640);
        assert_eq!(place.rotation, lowlat_host::video::Rotation::Deg90);

        // Nothing else on the channel reads as a layout.
        assert_eq!(is_layout(BYE_REPLACED), None);
        assert_eq!(is_layout(&hello(Role::Helper, Can::default())), None);
    }

    /// **A tray is told the state on connect and on change, and not on a
    /// repeat.** A tray started by hand against a quiet stream would otherwise
    /// show nothing until something happened, and one told every pass would
    /// redraw a menu twenty times a second.
    #[test]
    fn a_tray_is_told_the_state_on_connect_and_on_change() {
        let _alone = alone();
        let _ = STATE.lock().map(|mut kept| *kept = None);
        let peer = Peer {
            pid: 6001,
            uid: 1003,
            gid: 1003,
        };
        let mut body = Vec::new();

        // Nothing has been said yet, so a tray connecting now is told nothing.
        let (near, mut far) = pair();
        attach(peer, &near);
        assert_eq!(trays(), 1);
        far.set_read_timeout(Some(std::time::Duration::from_millis(100)))
            .expect("a deadline");
        assert!(
            read_frame(&mut far, &mut body).is_err(),
            "told a state nobody said"
        );

        let first = serde_json::json!({ "output": "DP-4", "guests": [] });
        state(&first);
        read_frame(&mut far, &mut body).expect("the state");
        assert_eq!(is_state(&body), Some(first.clone()));

        // Said again unchanged: nothing.
        state(&first);
        assert!(
            read_frame(&mut far, &mut body).is_err(),
            "a repeat was sent"
        );

        // Changed: sent.
        let second = serde_json::json!({ "output": "DP-4", "guests": [{ "id": 1 }] });
        state(&second);
        read_frame(&mut far, &mut body).expect("the change");
        assert_eq!(is_state(&body), Some(second.clone()));

        // A tray connecting later is told the last state at once.
        let later = Peer { pid: 6002, ..peer };
        let (near_later, mut far_later) = pair();
        attach(later, &near_later);
        far_later
            .set_read_timeout(Some(std::time::Duration::from_millis(500)))
            .expect("a deadline");
        read_frame(&mut far_later, &mut body).expect("the kept state");
        assert_eq!(is_state(&body), Some(second));

        detach(peer);
        detach(later);
        assert_eq!(trays(), 0);
        // Nothing else on the channel reads as a state.
        assert_eq!(is_state(BYE_REPLACED), None);
        assert_eq!(is_state(&hello(Role::Tray, Can::default())), None);
    }

    /// **What a tray says is an action with its credentials, and what a
    /// helper says is not.** Driven through the real dispatch rather than the
    /// queues alone, so a frame that took the wrong branch would show here.
    #[test]
    fn what_a_tray_asks_is_queued_with_who_asked() {
        let _alone = alone();
        let _ = take_acted();
        let _ = take_said();

        let (near, mut far) = pair();
        let served = std::thread::spawn(move || serve(near));
        write_frame(&mut far, &hello(Role::Tray, Can::default())).expect("hello");
        write_frame(&mut far, &kick(3)).expect("a kick");
        let wanted = serde_json::json!({ "video": [{ "encoderMaxBitrate": 20 }] });
        write_frame(&mut far, &config(&wanted)).expect("a change");
        drop(far);
        served.join().expect("served");

        let acted = take_acted();
        assert_eq!(acted.len(), 2, "both actions queued: {acted:?}");
        let (who, body) = &acted[0];
        assert_eq!(who.pid, std::process::id().cast_signed());
        assert_eq!(is_kick(body), Some(3));
        assert_eq!(is_config(body), None);
        let (_, body) = &acted[1];
        assert_eq!(is_kick(body), None);
        let carried: serde_json::Value =
            serde_json::from_slice(&is_config(body).expect("a config")).expect("json");
        assert_eq!(carried, wanted);
        // Nothing a tray says is taken as a session's statement.
        assert!(take_said().is_empty(), "a tray spoke for a session");

        // And the other way round: a helper's frame is not an action.
        let (near, mut far) = pair();
        let served = std::thread::spawn(move || serve(near));
        write_frame(&mut far, &hello(Role::Helper, Can::default())).expect("hello");
        write_frame(&mut far, &kick(3)).expect("a kick from the wrong role");
        drop(far);
        served.join().expect("served");
        assert!(take_acted().is_empty(), "a helper acted on the host");
        assert_eq!(take_said().len(), 1);
    }

    #[test]
    fn a_hello_that_is_not_readable_is_refused() {
        let (mut a, mut b) = pair();
        write_frame(&mut a, b"{not json").expect("written");
        assert!(greet(&mut b).is_none());
    }
}
