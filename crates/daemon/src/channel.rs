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
fn take_place(peer: Peer, stream: &UnixStream) {
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
    if greeting.role == Role::Helper {
        take_place(greeting.peer, &stream);
    }
    let mut body = Vec::new();
    while read_frame(&mut stream, &mut body).is_ok() {
        lowlat_common::log_debug!(
            "channel: {} said {} bytes",
            greeting.role.name(),
            body.len()
        );
    }
    if greeting.role == Role::Helper {
        leave_place(greeting.peer);
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
        let peer = Peer {
            pid: 4242,
            uid: 1000,
            gid: 1000,
        };
        let later = Peer { pid: 4243, ..peer };
        let (first, mut first_far) = pair();
        let (second, _second_far) = pair();

        take_place(peer, &first);
        assert!(holds(peer.pid));
        take_place(later, &second);
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
        let peer = Peer {
            pid: 5150,
            uid: 1001,
            gid: 1001,
        };
        let (near, mut far) = pair();
        take_place(peer, &near);

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

    #[test]
    fn a_hello_that_is_not_readable_is_refused() {
        let (mut a, mut b) = pair();
        write_frame(&mut a, b"{not json").expect("written");
        assert!(greet(&mut b).is_none());
    }
}
