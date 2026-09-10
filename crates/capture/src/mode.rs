//! Ask the session to change an output's mode or turn.
//!
//! **The display device admits one client and refuses every other**, a
//! service included, so nothing here touches the device: the session that
//! holds it takes requests over its own output-management protocol, which is
//! where a person's own display settings already send them. That protocol is
//! one desktop's, so this is one mechanism behind a capability the session
//! announces, and a session with a different one needs its own here.
//!
//! **A request, with one answer, on a deadline.** The session says applied
//! or failed, and a session that says neither within the deadline is
//! reported as not answering rather than waited on: this runs in somebody's
//! session, and an unbounded wait here is an unbounded wait on that.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::wayland::{DISPLAY, put_str, put_u32, read_i32, read_str, read_u32, trailing_u32};

/// What to change on one output. Either half may be left as it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Change {
    /// The mode's size in the output's own pixels.
    pub size: Option<(u32, u32)>,
    /// The session's transform: a quarter turn per step, from zero.
    pub transform: Option<u32>,
}

/// Whether this session offers the mechanism at all.
///
/// **Asked once, when the helper announces itself.** The answer is a
/// property of the compositor and does not change while it runs.
pub fn offered() -> bool {
    let Some(mut link) = Link::open() else {
        return false;
    };
    link.settle(Duration::from_millis(500)).is_some() && link.manager.is_some()
}

/// Change the named output, and wait for the session to say it did.
///
/// **Errors are reasons, in words**, because the thing that reads them is a
/// log line beside a guest's request.
pub fn set(connector: &str, change: Change, within: Duration) -> Result<(), String> {
    let deadline = Instant::now() + within;
    let mut link = Link::open().ok_or("no session to ask")?;
    link.settle(within).ok_or("the session did not answer")?;
    let manager = link.manager.ok_or("the session cannot set a mode")?;
    // Binding the devices asks for their descriptions, which arrive before
    // the next reply does.
    link.settle(deadline.saturating_duration_since(Instant::now()))
        .ok_or("the session did not describe its outputs")?;

    let (&device, output) = link
        .outputs
        .iter()
        .find(|(_, output)| output.name.as_deref() == Some(connector))
        .ok_or_else(|| format!("the session has no {connector}"))?;
    let mode = match change.size {
        Some((width, height)) => Some(
            output
                .mode_for(width, height)
                .ok_or_else(|| format!("{connector} has no {width}x{height} mode"))?,
        ),
        None => None,
    };

    let configuration = link.allocate();
    let mut body = Vec::new();
    put_u32(&mut body, configuration);
    link.send(manager, 0, &body)?;
    if let Some(mode) = mode {
        let mut body = Vec::new();
        put_u32(&mut body, device);
        put_u32(&mut body, mode);
        link.send(configuration, 1, &body)?;
    }
    if let Some(transform) = change.transform {
        let mut body = Vec::new();
        put_u32(&mut body, device);
        put_u32(&mut body, transform);
        link.send(configuration, 2, &body)?;
    }
    link.send(configuration, 5, &[])?;
    link.configuration = Some(configuration);

    let outcome = link.wait_applied(deadline);
    // Destroyed whatever happened, so the session is not left holding a
    // configuration nobody will apply.
    let _ = link.send(configuration, 6, &[]);
    outcome
}

/// One output as the session describes it, as much as choosing a mode needs.
#[derive(Debug, Default)]
struct Device {
    name: Option<String>,
    current: Option<u32>,
    /// Every mode by its object, with its size and its refresh in mHz.
    modes: HashMap<u32, (u32, u32, u32)>,
}

impl Device {
    /// The mode of that size to ask for.
    ///
    /// **The current refresh where the size offers it, else the highest.** A
    /// request names a size and nothing else, and a display kept at the rate
    /// it was running is the change the person at the desk would notice
    /// least.
    fn mode_for(&self, width: u32, height: u32) -> Option<u32> {
        let refresh = self
            .current
            .and_then(|current| self.modes.get(&current))
            .map(|&(_, _, refresh)| refresh);
        let mut candidates: Vec<(u32, u32)> = self
            .modes
            .iter()
            .filter(|(_, mode)| (mode.0, mode.1) == (width, height))
            .map(|(&id, &(_, _, hz))| (id, hz))
            .collect();
        if let Some(refresh) = refresh
            && let Some(&(same, _)) = candidates.iter().find(|(_, hz)| *hz == refresh)
        {
            return Some(same);
        }
        candidates.sort_by_key(|&(_, hz)| std::cmp::Reverse(hz));
        candidates.first().map(|&(id, _)| id)
    }
}

/// One conversation with the session, from the registry to an answer.
#[derive(Debug)]
struct Link {
    stream: UnixStream,
    next: u32,
    registry: u32,
    barrier: u32,
    manager: Option<u32>,
    outputs: HashMap<u32, Device>,
    /// Which output each mode object belongs to.
    owned: HashMap<u32, u32>,
    configuration: Option<u32>,
    /// What the session said of the configuration, once it has.
    answered: Option<Result<(), String>>,
    pending: Vec<u8>,
}

impl Link {
    /// Open the session named by the environment, and ask it what it offers.
    fn open() -> Option<Self> {
        let named = std::env::var("WAYLAND_DISPLAY").ok()?;
        let path = Path::new(&named);
        let socket = if path.is_absolute() {
            path.to_path_buf()
        } else {
            PathBuf::from(std::env::var("XDG_RUNTIME_DIR").ok()?).join(path)
        };
        let stream = UnixStream::connect(socket).ok()?;
        let mut link = Self {
            stream,
            next: DISPLAY + 1,
            registry: 0,
            barrier: 0,
            manager: None,
            outputs: HashMap::new(),
            owned: HashMap::new(),
            configuration: None,
            answered: None,
            pending: Vec::new(),
        };
        link.registry = link.allocate();
        let mut body = Vec::new();
        put_u32(&mut body, link.registry);
        link.send(DISPLAY, 1, &body).ok()?;
        Some(link)
    }

    fn allocate(&mut self) -> u32 {
        let id = self.next;
        self.next = self.next.saturating_add(1);
        id
    }

    fn send(&mut self, object: u32, opcode: u16, body: &[u8]) -> Result<(), String> {
        let size = u32::try_from(8 + body.len()).map_err(|_| "a request too large")?;
        let mut message = Vec::with_capacity(size as usize);
        put_u32(&mut message, object);
        put_u32(&mut message, (size << 16) | u32::from(opcode));
        message.extend_from_slice(body);
        self.stream
            .write_all(&message)
            .map_err(|error| format!("the session went away, {error}"))
    }

    /// Ask for a reply and read events until it arrives.
    fn settle(&mut self, within: Duration) -> Option<()> {
        let deadline = Instant::now() + within;
        self.barrier = self.allocate();
        let mut body = Vec::new();
        put_u32(&mut body, self.barrier);
        self.send(DISPLAY, 0, &body).ok()?;
        while self.barrier != 0 {
            self.read_some(deadline)?;
        }
        Some(())
    }

    /// Read until the session has said whether the configuration took.
    fn wait_applied(&mut self, deadline: Instant) -> Result<(), String> {
        while self.answered.is_none() {
            if self.read_some(deadline).is_none() {
                return Err("the session did not say whether it applied".to_string());
            }
        }
        self.answered.take().unwrap_or(Ok(()))
    }

    /// One read, bounded by the deadline, and everything whole in it.
    fn read_some(&mut self, deadline: Instant) -> Option<()> {
        let left = deadline.checked_duration_since(Instant::now())?;
        self.stream.set_read_timeout(Some(left)).ok()?;
        let mut chunk = [0u8; 4096];
        let read = self.stream.read(&mut chunk).ok()?;
        if read == 0 {
            return None;
        }
        self.pending.extend_from_slice(chunk.get(..read)?);
        loop {
            let Some(header) = self.pending.get(..8) else {
                return Some(());
            };
            let object = read_u32(header.get(..4)?)?;
            let packed = read_u32(header.get(4..8)?)?;
            let size = (packed >> 16) as usize;
            let opcode = (packed & 0xFFFF) as u16;
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

    fn event(&mut self, object: u32, opcode: u16, body: &[u8]) -> Option<()> {
        if object == DISPLAY {
            // A protocol error ends the conversation; anything else the
            // connection says is bookkeeping.
            return (opcode != 0).then_some(());
        }
        if object == self.barrier && opcode == 0 {
            self.barrier = 0;
            return Some(());
        }
        if object == self.registry {
            if opcode == 0 {
                self.global(body);
            }
            return Some(());
        }
        if Some(object) == self.configuration {
            match opcode {
                0 => self.answered = Some(Ok(())),
                1 => {
                    if self.answered.is_none() {
                        self.answered = Some(Err("the session refused it".to_string()));
                    }
                }
                // The reason precedes the failure it explains.
                2 => {
                    if let Some(reason) = read_str(body, 0) {
                        self.answered = Some(Err(reason));
                    }
                }
                _ => {}
            }
            return Some(());
        }
        if let Some(output) = self.outputs.get_mut(&object) {
            match opcode {
                1 => output.current = read_u32(body),
                2 => {
                    if let Some(mode) = read_u32(body) {
                        output.modes.insert(mode, (0, 0, 0));
                        self.owned.insert(mode, object);
                    }
                }
                14 => output.name = read_str(body, 0),
                _ => {}
            }
            return Some(());
        }
        if let Some(&owner) = self.owned.get(&object)
            && let Some(output) = self.outputs.get_mut(&owner)
            && let Some(mode) = output.modes.get_mut(&object)
        {
            match opcode {
                0 => {
                    mode.0 = read_i32(body).and_then(|v| u32::try_from(v).ok())?;
                    mode.1 = read_i32(body.get(4..)?).and_then(|v| u32::try_from(v).ok())?;
                }
                1 => mode.2 = read_i32(body).and_then(|v| u32::try_from(v).ok())?,
                3 => {
                    output.modes.remove(&object);
                    self.owned.remove(&object);
                }
                _ => {}
            }
        }
        Some(())
    }

    /// Something the session offers. Two of them are wanted, one of them
    /// once per output.
    fn global(&mut self, body: &[u8]) {
        let (Some(name), Some(interface), Some(version)) =
            (read_u32(body), read_str(body, 4), trailing_u32(body))
        else {
            return;
        };
        match interface.as_str() {
            // High enough to be told why a configuration failed.
            "kde_output_management_v2" if self.manager.is_none() => {
                self.manager = Some(self.bind(name, &interface, version.min(12)));
            }
            // High enough to be told the output's name.
            "kde_output_device_v2" if version >= 2 => {
                let id = self.bind(name, &interface, 2);
                self.outputs.insert(id, Device::default());
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

#[cfg(test)]
mod tests {
    use super::*;

    fn device() -> Device {
        let mut device = Device::default();
        device.modes.insert(10, (2560, 1440, 60_000));
        device.modes.insert(11, (2560, 1440, 120_000));
        device.modes.insert(12, (1920, 1080, 60_000));
        device.modes.insert(13, (1920, 1080, 50_000));
        device.current = Some(10);
        device
    }

    /// **The refresh the display is running is kept where the size offers
    /// it**, and the highest is taken where it does not: a request names a
    /// size and nothing else.
    #[test]
    fn the_mode_keeps_the_refresh_where_it_can() {
        let mut device = device();
        assert_eq!(device.mode_for(1920, 1080), Some(12));
        device.current = Some(11);
        assert_eq!(device.mode_for(2560, 1440), Some(11));
        assert_eq!(
            device.mode_for(1920, 1080),
            Some(12),
            "the highest, not the first"
        );
        assert_eq!(device.mode_for(1234, 567), None);
    }

    /// **The answer is the configuration's, and the reason comes first.** A
    /// failure without a reason is still a failure, and one with a reason
    /// says it.
    #[test]
    fn a_failure_reason_precedes_the_failure_it_explains() {
        let (near, _far) = UnixStream::pair().expect("a socket pair");
        let mut link = Link {
            stream: near,
            next: 2,
            registry: 0,
            barrier: 0,
            manager: None,
            outputs: HashMap::new(),
            owned: HashMap::new(),
            configuration: Some(7),
            answered: None,
            pending: Vec::new(),
        };
        let mut reason = Vec::new();
        put_str(&mut reason, "no such mode");
        link.event(7, 2, &reason).expect("a reason");
        link.event(7, 1, &[]).expect("a failure");
        assert_eq!(link.answered, Some(Err("no such mode".to_string())));

        link.answered = None;
        link.event(7, 1, &[]).expect("a bare failure");
        assert_eq!(
            link.answered,
            Some(Err("the session refused it".to_string()))
        );

        link.answered = None;
        link.event(7, 0, &[]).expect("applied");
        assert_eq!(link.answered, Some(Ok(())));
    }

    /// **Modes belong to the output that announced them**, and a mode the
    /// session withdraws stops being one that can be asked for.
    #[test]
    fn a_mode_is_described_under_its_output_and_can_be_withdrawn() {
        let (near, _far) = UnixStream::pair().expect("a socket pair");
        let mut link = Link {
            stream: near,
            next: 2,
            registry: 0,
            barrier: 0,
            manager: None,
            outputs: HashMap::new(),
            owned: HashMap::new(),
            configuration: None,
            answered: None,
            pending: Vec::new(),
        };
        link.outputs.insert(5, Device::default());
        let mut body = Vec::new();
        put_u32(&mut body, 0xFF00_0001);
        link.event(5, 2, &body).expect("a mode announced");
        let mut size = Vec::new();
        put_u32(&mut size, 1920);
        put_u32(&mut size, 1080);
        link.event(0xFF00_0001, 0, &size).expect("a size");
        let mut refresh = Vec::new();
        put_u32(&mut refresh, 60_000);
        link.event(0xFF00_0001, 1, &refresh).expect("a refresh");
        let mut name = Vec::new();
        put_str(&mut name, "DP-1");
        link.event(5, 14, &name).expect("a name");

        let output = &link.outputs[&5];
        assert_eq!(output.name.as_deref(), Some("DP-1"));
        assert_eq!(output.mode_for(1920, 1080), Some(0xFF00_0001));

        link.event(0xFF00_0001, 3, &[]).expect("withdrawn");
        assert_eq!(link.outputs[&5].mode_for(1920, 1080), None);
    }
}
