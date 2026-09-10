//! Where the captured output sits in the desktop around it.
//!
//! **The display device cannot answer this.** A controller reports a position
//! inside its own framebuffer, which is the corner whatever the desktop looks
//! like, and an output the compositor made up has no controller at all. So the
//! captured output's size is knowable here and its *place* is not.
//!
//! That matters because absolute input is spread over the whole desktop by the
//! layer below the display server: a coordinate normalised against the picture
//! alone lands proportionally short on any desktop wider than the picture, and
//! the last of the screen cannot be reached at all (docs/05-host.md section 7).
//!
//! The layout exists in one place, which is the session compositing the
//! desktop, so it is asked. A session that does not answer is not an error:
//! with one output the picture is the desktop and the mapping is already
//! right, which is the case this falls back to.
//!
//! **Logical units, not the picture's pixels.** The rectangle comes back in
//! whatever units the compositor arranges outputs in, and a scaled output's
//! rectangle is smaller than the framebuffer it is drawn from. Both ends of
//! the mapping have to be in one space, so the rectangle travels with the
//! desktop it is measured against and the picture's size is converted into it
//! rather than assumed equal.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use lowlat_core::video::Rotation;

use crate::wayland::{DISPLAY, put_str, put_u32, read_i32, read_str, read_u32, trailing_u32};

/// Where one output sits, in the desktop's own units.
///
/// The origin is measured from the desktop's own corner rather than from
/// wherever the compositor put zero, because that corner is what the absolute
/// axis maps its own zero to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placement {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    /// The whole desktop the input layer spreads an absolute device over.
    pub desktop_width: u32,
    pub desktop_height: u32,
    /// Which way the session has turned this output.
    ///
    /// **Followed, never set here.** A turned output is drawn turned into a
    /// framebuffer that keeps its landscape shape, so the picture captured
    /// from it is on its side and only the session can say by how much.
    pub rotation: Rotation,
}

/// Where the named output sits, as the session driving it lays it out.
///
/// The name is the display device's own, such as `DP-2`. **It is also what
/// picks the session**: several may be running, and the one that answers with
/// this output in its layout is by definition the one compositing it.
pub fn placement_of(connector: &str) -> Option<Placement> {
    for socket in sockets() {
        let Some(outputs) = query(&socket) else {
            continue;
        };
        if let Some(found) = place(&outputs, connector) {
            lowlat_common::log_info!(
                "desktop: {connector} is {}x{} at {},{} of {}x{}",
                found.width,
                found.height,
                found.x,
                found.y,
                found.desktop_width,
                found.desktop_height
            );
            return Some(found);
        }
    }
    lowlat_common::log_info!(
        "desktop: no session describes {connector}, absolute input spans the picture alone"
    );
    None
}

/// The output at the desktop's own corner, which is the one to prefer.
///
/// **A desktop has a corner and the screen at it is the one a person calls
/// their main display.** Nothing in the layout protocol says "primary", but
/// every arrangement puts one output at the origin and hangs the rest off it,
/// so the corner is the signal that is actually there rather than one invented
/// for the occasion.
///
/// Nothing when no session answers, which is the honest answer: without a
/// layout there is no origin to be at.
pub fn at_origin() -> Option<String> {
    for socket in sockets() {
        let Some(outputs) = query(&socket) else {
            continue;
        };
        let found = outputs.iter().find_map(|output| {
            let name = output.name.as_deref()?;
            let placed = place(&outputs, name)?;
            (placed.x == 0 && placed.y == 0).then(|| name.to_string())
        });
        if found.is_some() {
            return found;
        }
    }
    None
}

/// One output as the layout describes it.
///
/// **Every field is optional because a layout arrives in pieces.** An output
/// names itself in one event and describes its rectangle in others, so a
/// half-filled one is an ordinary intermediate state rather than a fault.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Output {
    pub name: Option<String>,
    pub x: Option<i32>,
    pub y: Option<i32>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// The session's transform, in its own numbering: a quarter turn per
    /// step, with the mirrored ones from four.
    pub transform: Option<u32>,
}

/// The session's transform as the wire says it.
///
/// **A quarter turn per step, and the turn is the same one.** The session
/// draws a turned desktop into the framebuffer a quarter counter-clockwise
/// per step; the code the peer receives makes it turn the picture the same
/// quarter clockwise, so the two are the same digit apart. The mirrored
/// transforms carry a flip the picture cannot express and the turn they can.
fn rotation_of(transform: Option<u32>) -> Rotation {
    match transform.map(|transform| transform & 3) {
        Some(1) => Rotation::Deg90,
        Some(2) => Rotation::Deg180,
        Some(3) => Rotation::Deg270,
        _ => Rotation::None,
    }
}

/// Reduce a layout to one output's placement within it.
///
/// **The desktop is the bounding box of every output**, which is the extent
/// the absolute axis is spread over. An output missing any of its four numbers
/// is dropped rather than defaulted: a missing rectangle contributes nothing
/// to a bounding box, while a rectangle assumed to be at the origin silently
/// makes the desktop bigger than it is.
pub fn place(outputs: &[Output], connector: &str) -> Option<Placement> {
    let rects: Vec<(&str, i32, i32, u32, u32)> = outputs
        .iter()
        .filter_map(|output| {
            Some((
                output.name.as_deref()?,
                output.x?,
                output.y?,
                output.width?,
                output.height?,
            ))
        })
        .collect();
    let (_, ours_x, ours_y, width, height) = *rects.iter().find(|(name, ..)| *name == connector)?;
    let rotation = outputs
        .iter()
        .find(|output| output.name.as_deref() == Some(connector))
        .map_or(Rotation::None, |output| rotation_of(output.transform));

    let mut left = i64::MAX;
    let mut top = i64::MAX;
    let mut right = i64::MIN;
    let mut bottom = i64::MIN;
    for (_, x, y, w, h) in &rects {
        left = left.min(i64::from(*x));
        top = top.min(i64::from(*y));
        right = right.max(i64::from(*x) + i64::from(*w));
        bottom = bottom.max(i64::from(*y) + i64::from(*h));
    }

    Some(Placement {
        x: u32::try_from(i64::from(ours_x) - left).ok()?,
        y: u32::try_from(i64::from(ours_y) - top).ok()?,
        width,
        height,
        desktop_width: u32::try_from(right - left).ok()?,
        desktop_height: u32::try_from(bottom - top).ok()?,
        rotation,
    })
}

/// A session's layout, watched rather than asked for.
///
/// **The connection is the subscription.** A session re-describes an output
/// when it moves and announces one that appears, but only to a client that is
/// still there: a query that connects, reads and closes learns the layout once
/// and can never learn that it changed. This is that query with the closing
/// left out.
///
/// **It speaks for the session it is in.** Where a one-shot query scans every
/// socket and picks whichever describes the output being captured, this is
/// held by something already inside a session and asks that one.
#[derive(Debug)]
pub struct Watch {
    session: Session,
}

impl Watch {
    /// Open the session named by the environment, and read its layout once.
    ///
    /// **Answers `None` when there is no session to watch**, which is the
    /// honest answer for a program started outside one rather than a reason to
    /// go looking for somebody else's.
    pub fn open() -> Option<(Self, Vec<Output>)> {
        let named = std::env::var("WAYLAND_DISPLAY").ok()?;
        let path = Path::new(&named);
        let socket = if path.is_absolute() {
            path.to_path_buf()
        } else {
            PathBuf::from(std::env::var("XDG_RUNTIME_DIR").ok()?).join(path)
        };
        let stream = UnixStream::connect(socket).ok()?;
        let mut session = Session::new(stream);
        let outputs = session.snapshot()?;
        Some((Self { session }, outputs))
    }

    /// Wait for the layout to change, and answer with what it became.
    ///
    /// **`Ok(None)` is the ordinary answer**, and covers both nothing arriving
    /// before the deadline and something arriving that left the layout as it
    /// was. A session re-sends every field of an output it re-describes, so
    /// events are not changes. **A quiet session is not an ended one**: the
    /// two are told apart here because a watcher that stops at the first
    /// quiet tick has watched for one tick, which is what the helper did for
    /// a day while its change detection was being proven through a probe
    /// that happened to loop.
    pub fn changed(&mut self, within: Duration) -> Result<Option<Vec<Output>>, Ended> {
        self.session
            .stream
            .set_read_timeout(Some(within))
            .map_err(|_| Ended)?;
        let mut chunk = [0u8; 4096];
        let read = match self.session.stream.read(&mut chunk) {
            Ok(0) => return Err(Ended),
            Ok(read) => read,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return Ok(None);
            }
            Err(_) => return Err(Ended),
        };
        self.session
            .pending
            .extend_from_slice(chunk.get(..read).unwrap_or_default());
        // A stream out of step, or a protocol error, is a session there is
        // nothing further to hear from.
        self.session.consume().ok_or(Ended)?;
        if !self.session.moved {
            return Ok(None);
        }
        // **Settled before it is believed.** What arrived may be half of a
        // description, and a rectangle read between two of its own events is a
        // layout nobody ever had.
        self.session
            .stream
            .set_read_timeout(Some(TIMEOUT))
            .map_err(|_| Ended)?;
        self.session.snapshot().map(Some).ok_or(Ended)
    }
}

/// The session a watch was in has ended, and there is nothing further to
/// hear from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ended;

/// Every session socket worth asking, the one named by the environment first.
///
/// **A host that owns the display need not be inside the session** whose
/// desktop it is capturing, so the environment usually says nothing and the
/// sockets have to be found. Ordering only decides which is tried first; the
/// answer is picked by whether the layout contains the output being captured.
fn sockets() -> Vec<PathBuf> {
    let mut found = Vec::new();
    if let Ok(named) = std::env::var("WAYLAND_DISPLAY") {
        let path = Path::new(&named);
        if path.is_absolute() {
            found.push(path.to_path_buf());
        } else if let Ok(runtime) = std::env::var("XDG_RUNTIME_DIR") {
            found.push(Path::new(&runtime).join(named));
        }
    }
    let Ok(users) = std::fs::read_dir("/run/user") else {
        return found;
    };
    for user in users.flatten() {
        let Ok(entries) = std::fs::read_dir(user.path()) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            // The lock beside each socket has the same stem and is not one.
            if let Some(index) = name.strip_prefix("wayland-")
                && index.bytes().all(|byte| byte.is_ascii_digit())
                && !found.contains(&path)
            {
                found.push(path);
            }
        }
    }
    found
}

// The session protocol, as much of it as one layout needs; the framing is
// shared with the other client of it in `wayland.rs`.

/// A reply that has not arrived is a session that is not answering. This runs
/// on the thread that opens the display, so it is bounded rather than waited
/// on: the fallback is correct behaviour on one output, and a wedged session
/// must not hold the stream.
const TIMEOUT: Duration = Duration::from_millis(500);

/// Refuse a layout that is obviously not one, rather than growing to meet it.
const OUTPUTS: usize = 64;

/// Ask one session for its layout.
fn query(socket: &Path) -> Option<Vec<Output>> {
    let Ok(stream) = UnixStream::connect(socket) else {
        return None;
    };
    stream.set_read_timeout(Some(TIMEOUT)).ok()?;
    Session::new(stream).layout()
}

/// One conversation, from the registry to the outputs it names.
#[derive(Debug)]
struct Session {
    stream: UnixStream,
    /// Client object identifiers, which are ours to allocate and start above
    /// the connection's own.
    next: u32,
    registry: u32,
    /// The reply that says a stage is complete, or zero between stages.
    barrier: u32,
    /// The layout manager, once the registry has named it.
    manager: Option<u32>,
    /// Outputs by the object bound for each, and which output each of their
    /// descriptions belongs to.
    outputs: HashMap<u32, Output>,
    described: HashMap<u32, u32>,
    /// The object bound for each name the registry gave, so an output that is
    /// removed can be found again: removal names what was advertised, not what
    /// was bound for it.
    named: HashMap<u32, u32>,
    /// Whatever has arrived and is not yet a whole message.
    pending: Vec<u8>,
    /// Whether anything about the layout has actually changed since this was
    /// last cleared.
    moved: bool,
}

impl Session {
    fn new(stream: UnixStream) -> Self {
        Self {
            stream,
            next: DISPLAY + 1,
            registry: 0,
            barrier: 0,
            manager: None,
            outputs: HashMap::new(),
            described: HashMap::new(),
            named: HashMap::new(),
            pending: Vec::new(),
            moved: false,
        }
    }

    /// The two rounds a layout takes: what exists, then what each one is.
    fn layout(mut self) -> Option<Vec<Output>> {
        self.snapshot()
    }

    /// Ask for everything not yet described, and wait for the answers.
    ///
    /// **Both rounds every time, because the second depends on the first.** A
    /// description is asked for per output and the outputs are not known until
    /// the round that names them has finished arriving; an output that appears
    /// later is undescribed until this runs again.
    fn snapshot(&mut self) -> Option<Vec<Output>> {
        if self.registry == 0 {
            self.registry = self.allocate();
            let mut body = Vec::new();
            put_u32(&mut body, self.registry);
            self.send(DISPLAY, 1, &body)?;
            self.settle()?;
        }

        let manager = self.manager?;
        let undescribed: Vec<u32> = self
            .outputs
            .keys()
            .copied()
            .filter(|output| !self.described.values().any(|had| had == output))
            .collect();
        for output in undescribed {
            let described = self.allocate();
            let mut body = Vec::new();
            put_u32(&mut body, described);
            put_u32(&mut body, output);
            self.send(manager, 1, &body)?;
            self.described.insert(described, output);
        }
        self.settle()?;
        self.moved = false;

        Some(self.outputs.values().cloned().collect())
    }

    fn allocate(&mut self) -> u32 {
        let id = self.next;
        self.next = self.next.saturating_add(1);
        id
    }

    fn send(&mut self, object: u32, opcode: u16, body: &[u8]) -> Option<()> {
        let size = u32::try_from(8 + body.len()).ok()?;
        let mut message = Vec::with_capacity(size as usize);
        put_u32(&mut message, object);
        put_u32(&mut message, (size << 16) | u32::from(opcode));
        message.extend_from_slice(body);
        self.stream.write_all(&message).ok()
    }

    /// Ask for a reply and read events until it arrives.
    ///
    /// **The reply is what says a round is complete.** Every event before it
    /// belongs to what was asked for previously, and there is no other signal:
    /// the connection is a stream and a quiet moment means nothing.
    fn settle(&mut self) -> Option<()> {
        self.barrier = self.allocate();
        let mut body = Vec::new();
        put_u32(&mut body, self.barrier);
        self.send(DISPLAY, 0, &body)?;

        let mut chunk = [0u8; 4096];
        while self.barrier != 0 {
            let read = self.stream.read(&mut chunk).ok()?;
            if read == 0 {
                return None;
            }
            self.pending.extend_from_slice(chunk.get(..read)?);
            self.consume()?;
        }
        Some(())
    }

    /// Take whole messages out of what has arrived.
    fn consume(&mut self) -> Option<()> {
        loop {
            let Some(header) = self.pending.get(..8) else {
                return Some(());
            };
            let object = read_u32(header.get(..4)?)?;
            let packed = read_u32(header.get(4..8)?)?;
            let size = (packed >> 16) as usize;
            let opcode = (packed & 0xFFFF) as u16;
            // A message shorter than its own header is a stream out of step,
            // and nothing after it can be trusted to be a message at all.
            if size < 8 {
                return None;
            }
            if self.pending.len() < size {
                return Some(());
            }
            let message: Vec<u8> = self.pending.drain(..size).collect();
            self.event(object, opcode, message.get(8..)?)?;
        }
    }

    /// One event, or nothing when it is not one this needs.
    fn event(&mut self, object: u32, opcode: u16, body: &[u8]) -> Option<()> {
        // The connection reports a fatal protocol error and then says nothing
        // further, so it ends the conversation rather than being stepped over.
        if object == DISPLAY {
            if opcode == 0 {
                return None;
            }
            return Some(());
        }
        if object == self.barrier && opcode == 0 {
            self.barrier = 0;
            return Some(());
        }
        if object == self.registry {
            match opcode {
                0 => self.global(body),
                // **An output that went away takes its rectangle with it.**
                // Left behind, it keeps contributing to the bounding box the
                // absolute axis is spread over, so a desktop that shrank would
                // go on being mapped at its old width.
                1 => self.gone(body),
                _ => {}
            }
            return Some(());
        }
        // An output names itself, which is the same name the display device
        // knows it by and the only thing tying the two together; and its
        // geometry ends with the transform, which nothing below the session
        // reports.
        if let Some(output) = self.outputs.get_mut(&object) {
            let before = output.clone();
            match opcode {
                0 => output.transform = trailing_u32(body),
                4 => {
                    if let Some(name) = read_str(body, 0) {
                        output.name = Some(name);
                    }
                }
                _ => {}
            }
            self.moved |= *output != before;
            return Some(());
        }
        let Some(&output) = self.described.get(&object) else {
            return Some(());
        };
        let Some(described) = self.outputs.get_mut(&output) else {
            return Some(());
        };
        let before = described.clone();
        match opcode {
            0 => {
                described.x = read_i32(body);
                described.y = read_i32(body.get(4..).unwrap_or_default());
            }
            1 => {
                described.width = extent(body);
                described.height = extent(body.get(4..).unwrap_or_default());
            }
            // Superseded by the output naming itself, and still the only name
            // a session too old to do that gives.
            3 if described.name.is_none() => described.name = read_str(body, 0),
            _ => {}
        }
        // **Compared rather than assumed.** A session re-sends every field of
        // an output it re-describes, most of them unchanged, so a watcher told
        // by the arrival of an event alone would report a layout change every
        // time anything at all was re-announced.
        self.moved |= *described != before;
        Some(())
    }

    /// Something the session offers. Two of them are wanted.
    fn global(&mut self, body: &[u8]) {
        let (Some(name), Some(interface), Some(version)) = (
            read_u32(body.get(..4).unwrap_or_default()),
            read_str(body, 4),
            trailing_u32(body),
        ) else {
            return;
        };
        match interface.as_str() {
            "wl_output" if self.outputs.len() < OUTPUTS => {
                // Bound high enough to be told the name, and no higher: a
                // version above what is offered is refused outright.
                let id = self.bind(name, &interface, version.min(4));
                self.outputs.insert(id, Output::default());
                self.named.insert(name, id);
                self.moved = true;
            }
            // The rectangles are the manager's to describe, and the version
            // that names an output alongside them is the second.
            "zxdg_output_manager_v1" if self.manager.is_none() && version >= 2 => {
                self.manager = Some(self.bind(name, &interface, version.min(3)));
            }
            _ => {}
        }
    }

    /// Something the session no longer offers.
    fn gone(&mut self, body: &[u8]) {
        let Some(name) = read_u32(body.get(..4).unwrap_or_default()) else {
            return;
        };
        let Some(object) = self.named.remove(&name) else {
            return;
        };
        if self.outputs.remove(&object).is_some() {
            self.moved = true;
        }
        self.described.retain(|_, output| *output != object);
    }

    fn bind(&mut self, name: u32, interface: &str, version: u32) -> u32 {
        let id = self.allocate();
        let mut body = Vec::new();
        put_u32(&mut body, name);
        put_str(&mut body, interface);
        put_u32(&mut body, version);
        put_u32(&mut body, id);
        let _ = self.send(self.registry, 0, &body);
        id
    }
}

/// A dimension, which the protocol signs and nothing real makes negative.
fn extent(bytes: &[u8]) -> Option<u32> {
    u32::try_from(read_i32(bytes)?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output(name: &str, x: i32, y: i32, width: u32, height: u32) -> Output {
        Output {
            name: Some(name.to_string()),
            x: Some(x),
            y: Some(y),
            width: Some(width),
            height: Some(height),
            transform: None,
        }
    }

    /// **A turned output is a change**, and the transform is the last of
    /// the geometry's arguments, after two strings of no fixed length.
    #[test]
    fn a_turn_arrives_at_the_end_of_the_geometry_and_is_a_change() {
        let mut session = session();
        session.outputs.insert(9, Output::default());

        let mut body = Vec::new();
        for value in [0u32, 0, 600, 340, 0] {
            put_u32(&mut body, value);
        }
        put_str(&mut body, "AOC");
        put_str(&mut body, "Q27B30S3");
        put_u32(&mut body, 1);
        session.event(9, 0, &body).expect("a geometry");
        assert!(session.moved, "a turn was not noticed");
        assert_eq!(session.outputs[&9].transform, Some(1));

        session.moved = false;
        session.event(9, 0, &body).expect("the same geometry");
        assert!(!session.moved, "the same geometry read as a change");
    }

    /// **The session's transform and the wire's code are one digit apart**,
    /// and the mirrored transforms keep their turn.
    #[test]
    fn the_placement_carries_the_turn_the_session_reports() {
        for (transform, expected) in [
            (None, Rotation::None),
            (Some(0), Rotation::None),
            (Some(1), Rotation::Deg90),
            (Some(2), Rotation::Deg180),
            (Some(3), Rotation::Deg270),
            (Some(4), Rotation::None),
            (Some(5), Rotation::Deg90),
            (Some(7), Rotation::Deg270),
        ] {
            let mut turned = output("DP-1", 2560, 0, 1440, 2560);
            turned.transform = transform;
            let layout = [output("DP-4", 0, 0, 2560, 1440), turned];
            let ours = place(&layout, "DP-1").expect("the turned output is placeable");
            assert_eq!(ours.rotation, expected, "transform {transform:?}");
            // **The rectangle is the desktop's, already turned.** Nothing
            // here swaps it: the session laid the output out portrait.
            assert_eq!((ours.width, ours.height), (1440, 2560));
        }
    }

    /// A session with a socket nothing is on the other end of, which is all a
    /// test of what arrives needs.
    fn session() -> Session {
        let (near, _far) = UnixStream::pair().expect("a socket pair");
        let mut session = Session::new(near);
        session.registry = 2;
        session
    }

    /// **An output that appears is a change**, and it is the case the whole
    /// watch exists for: a display added after a stream started leaves the
    /// desktop wider than the mapping believes it is.
    #[test]
    fn an_output_appearing_or_going_away_moves_the_layout() {
        let mut session = session();
        let mut body = Vec::new();
        put_u32(&mut body, 7);
        put_str(&mut body, "wl_output");
        put_u32(&mut body, 4);
        session.event(2, 0, &body).expect("a global");
        assert!(session.moved, "an output appeared and nothing said so");
        assert_eq!(session.outputs.len(), 1);

        session.moved = false;
        let mut body = Vec::new();
        put_u32(&mut body, 7);
        session.event(2, 1, &body).expect("a removal");
        assert!(session.moved, "an output went away and nothing said so");
        assert!(
            session.outputs.is_empty(),
            "a departed output still counts toward the desktop it left"
        );
    }

    /// **Off by default: it needs a session to be inside.** Run it as the
    /// person who is logged in, with `--ignored`.
    ///
    /// It asserts what can be asserted without moving somebody's screen: that
    /// the connection opens, that the layout it reads is the same one the
    /// one-shot query reports, and that a quiet desktop reports no change. The
    /// half it cannot reach is a real hotplug, which needs a display to be
    /// plugged in while it runs.
    #[test]
    #[ignore = "needs a session"]
    fn a_watch_reads_the_layout_its_session_has() {
        let (mut watch, outputs) = Watch::open().expect("a session");
        assert!(!outputs.is_empty(), "a session with no outputs at all");

        for output in &outputs {
            let name = output.name.clone().expect("every output names itself");
            assert_eq!(
                place(&outputs, &name),
                placement_of(&name),
                "the watch and the one-shot query disagree about {name}"
            );
        }

        // **A desktop nobody touched reports nothing.** Without this the watch
        // could be reporting a change on every event it receives, which is the
        // failure that looks most like working.
        assert_eq!(
            watch.changed(Duration::from_millis(300)),
            Ok(None),
            "a still desktop reported a layout change"
        );
        // **And a quiet tick is not the session ending.** Twice, because the
        // first quiet read is where a watcher that confused the two stopped.
        assert_eq!(watch.changed(Duration::from_millis(300)), Ok(None));
    }

    /// **Events are not changes.** A session re-sends every field of an output
    /// it re-describes, most of them unchanged, so a watch woken by arrival
    /// alone would report a layout change whenever anything was re-announced.
    #[test]
    fn a_description_that_says_the_same_thing_is_not_a_change() {
        let mut session = session();
        session.outputs.insert(9, Output::default());
        session.described.insert(10, 9);

        let mut body = Vec::new();
        put_u32(&mut body, 100);
        put_u32(&mut body, 200);
        session.event(10, 0, &body).expect("a position");
        assert!(session.moved, "the first position is a change");

        session.moved = false;
        session.event(10, 0, &body).expect("the same position");
        assert!(!session.moved, "the same position read as a move");

        let mut moved = Vec::new();
        put_u32(&mut moved, 101);
        put_u32(&mut moved, 200);
        session.event(10, 0, &moved).expect("a new position");
        assert!(session.moved, "a real move was not noticed");
    }

    /// **The desktop is every output, not the one being captured.** This is
    /// the whole of what the display device cannot see, and getting it from
    /// the captured output alone is the failure this exists to fix.
    #[test]
    fn the_desktop_is_the_bounding_box_of_every_output() {
        let layout = [
            output("DP-2", 0, 0, 2560, 1440),
            output("Virtual-1", 2560, 0, 1920, 1080),
        ];
        let ours = place(&layout, "DP-2").expect("the captured output is in the layout");
        assert_eq!((ours.x, ours.y), (0, 0));
        assert_eq!((ours.width, ours.height), (2560, 1440));
        assert_eq!((ours.desktop_width, ours.desktop_height), (4480, 1440));
    }

    /// **An output left of the origin makes every coordinate negative**, and
    /// the absolute axis has no negative half: zero is the desktop's own
    /// corner, so the whole layout shifts to meet it.
    #[test]
    fn the_origin_is_the_desktops_corner_and_not_the_compositors_zero() {
        let layout = [
            output("DP-2", 0, 0, 2560, 1440),
            output("HDMI-A-1", -1920, -120, 1920, 1080),
        ];
        let ours = place(&layout, "DP-2").expect("the captured output is in the layout");
        assert_eq!((ours.x, ours.y), (1920, 120));
        assert_eq!((ours.desktop_width, ours.desktop_height), (4480, 1560));

        let other = place(&layout, "HDMI-A-1").expect("both outputs are placeable");
        assert_eq!((other.x, other.y), (0, 0));
    }

    /// **The corner is the only "primary" a layout actually carries.** It is
    /// what the rest of the arrangement is measured from, and every output but
    /// one is somewhere else.
    #[test]
    fn the_output_at_the_corner_is_the_one_to_prefer() {
        let layout = [
            output("HDMI-A-1", 2560, 0, 2226, 1252),
            output("DP-4", 0, 0, 2560, 1440),
        ];
        let corner = layout
            .iter()
            .find_map(|output| {
                let name = output.name.as_deref()?;
                let placed = place(&layout, name)?;
                (placed.x == 0 && placed.y == 0).then(|| name.to_string())
            })
            .expect("some output is at the corner");
        assert_eq!(corner, "DP-4", "the corner is not the first one listed");

        // **And it is the desktop's corner, not the compositor's zero.** A
        // layout laid out into negative coordinates still has exactly one
        // output at its own corner.
        let shifted = [
            output("DP-4", 0, 0, 2560, 1440),
            output("HDMI-A-1", -2226, 0, 2226, 1252),
        ];
        let corner = shifted
            .iter()
            .find_map(|output| {
                let name = output.name.as_deref()?;
                let placed = place(&shifted, name)?;
                (placed.x == 0 && placed.y == 0).then(|| name.to_string())
            })
            .expect("some output is at the corner");
        assert_eq!(corner, "HDMI-A-1");
    }

    /// **A layout that does not contain the captured output is another
    /// session's**, and answering from it would place the pointer against a
    /// desktop nobody is looking at.
    #[test]
    fn an_output_that_is_not_in_the_layout_is_not_placed() {
        let layout = [output("DP-2", 0, 0, 2560, 1440)];
        assert_eq!(place(&layout, "DP-1"), None);
        assert_eq!(place(&[], "DP-2"), None);
    }

    /// **A half-described output is dropped rather than defaulted.** One
    /// assumed to be at the origin grows the desktop it is measured into, and
    /// every position then lands proportionally short -- which is exactly the
    /// failure this file exists to remove, arriving by a different route.
    #[test]
    fn an_output_missing_its_rectangle_is_not_counted() {
        let mut half = output("Virtual-1", 2560, 0, 1920, 1080);
        half.width = None;
        let layout = [output("DP-2", 0, 0, 2560, 1440), half];
        let ours = place(&layout, "DP-2").expect("the captured output is whole");
        assert_eq!((ours.desktop_width, ours.desktop_height), (2560, 1440));
    }

    /// A string argument is its length with the terminator counted, and the
    /// argument after it starts on the next four-byte boundary.
    #[test]
    fn a_string_argument_carries_its_terminator_and_is_padded() {
        let mut body = Vec::new();
        put_str(&mut body, "wl_output");
        assert_eq!(body.len(), 16, "nine bytes, a terminator, then padding");
        assert_eq!(read_u32(&body), Some(10));
        assert_eq!(read_str(&body, 0).as_deref(), Some("wl_output"));

        // The version follows the padding rather than the terminator, which is
        // what makes reading it from the end of the message right.
        put_u32(&mut body, 3);
        assert_eq!(trailing_u32(&body), Some(3));
    }
}
