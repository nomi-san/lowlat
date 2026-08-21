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

/// One output as the layout describes it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Output {
    name: Option<String>,
    x: Option<i32>,
    y: Option<i32>,
    width: Option<u32>,
    height: Option<u32>,
}

/// Reduce a layout to one output's placement within it.
///
/// **The desktop is the bounding box of every output**, which is the extent
/// the absolute axis is spread over. An output missing any of its four numbers
/// is dropped rather than defaulted: a missing rectangle contributes nothing
/// to a bounding box, while a rectangle assumed to be at the origin silently
/// makes the desktop bigger than it is.
fn place(outputs: &[Output], connector: &str) -> Option<Placement> {
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
    })
}

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

// The session protocol, as much of it as one layout needs.
//
// Requests are a header of the object and a packed size and opcode, then
// arguments of four bytes each; a string is its length including a terminator,
// then the bytes, padded out to four. Every reply is the same shape, which is
// what lets an event nothing here understands be stepped over by its size
// rather than having to be described.

/// The connection itself, which is object one and never allocated.
const DISPLAY: u32 = 1;

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
    /// Whatever has arrived and is not yet a whole message.
    pending: Vec<u8>,
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
            pending: Vec::new(),
        }
    }

    /// The two rounds a layout takes: what exists, then what each one is.
    fn layout(mut self) -> Option<Vec<Output>> {
        self.registry = self.allocate();
        let mut body = Vec::new();
        put_u32(&mut body, self.registry);
        self.send(DISPLAY, 1, &body)?;
        self.settle()?;

        // **Nothing is asked for until everything has been named**, because a
        // description is requested per output and the outputs are not known
        // until the first round has finished arriving.
        let manager = self.manager?;
        let bound: Vec<u32> = self.outputs.keys().copied().collect();
        for output in bound {
            let described = self.allocate();
            let mut body = Vec::new();
            put_u32(&mut body, described);
            put_u32(&mut body, output);
            self.send(manager, 1, &body)?;
            self.described.insert(described, output);
        }
        self.settle()?;

        Some(self.outputs.into_values().collect())
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
        if object == self.registry && opcode == 0 {
            self.global(body);
            return Some(());
        }
        // An output names itself, which is the same name the display device
        // knows it by and the only thing tying the two together.
        if let Some(output) = self.outputs.get_mut(&object) {
            if opcode == 4
                && let Some(name) = read_str(body, 0)
            {
                output.name = Some(name);
            }
            return Some(());
        }
        let Some(&output) = self.described.get(&object) else {
            return Some(());
        };
        let Some(described) = self.outputs.get_mut(&output) else {
            return Some(());
        };
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
            }
            // The rectangles are the manager's to describe, and the version
            // that names an output alongside them is the second.
            "zxdg_output_manager_v1" if self.manager.is_none() && version >= 2 => {
                self.manager = Some(self.bind(name, &interface, version.min(3)));
            }
            _ => {}
        }
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

fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_ne_bytes());
}

/// A string is its length including a terminator, then the bytes, padded to
/// the four-byte alignment every argument sits on.
fn put_str(out: &mut Vec<u8>, value: &str) {
    let bytes = value.as_bytes();
    put_u32(out, u32::try_from(bytes.len() + 1).unwrap_or(1));
    out.extend_from_slice(bytes);
    out.push(0);
    while out.len() % 4 != 0 {
        out.push(0);
    }
}

fn read_u32(bytes: &[u8]) -> Option<u32> {
    Some(u32::from_ne_bytes(bytes.get(..4)?.try_into().ok()?))
}

fn read_i32(bytes: &[u8]) -> Option<i32> {
    Some(i32::from_ne_bytes(bytes.get(..4)?.try_into().ok()?))
}

/// A dimension, which the protocol signs and nothing real makes negative.
fn extent(bytes: &[u8]) -> Option<u32> {
    u32::try_from(read_i32(bytes)?).ok()
}

/// The string at an offset, without its terminator.
fn read_str(body: &[u8], at: usize) -> Option<String> {
    let length = read_u32(body.get(at..)?)? as usize;
    let bytes = body.get(at + 4..at + 4 + length.checked_sub(1)?)?;
    String::from_utf8(bytes.to_vec()).ok()
}

/// The last argument of a message whose string length is not known in advance.
///
/// A padded string is followed by whatever comes after it, and stepping over
/// one to reach a fixed final argument costs the same arithmetic twice; the
/// final four bytes are the argument either way.
fn trailing_u32(body: &[u8]) -> Option<u32> {
    read_u32(body.get(body.len().checked_sub(4)?..)?)
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
        }
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
