//! The desktop's clipboard through the session's X server, for a desktop whose
//! clipboard has no service on the bus (docs/07-platforms.md section 5.1).
//!
//! **Why an X client, on a Wayland desktop.** Owning the clipboard is a
//! standing promise to answer whoever pastes, and Wayland gives that only to a
//! focused window or to a compositor's privileged selection protocol, which
//! GNOME's does not offer. X has no such rule: any client may own the
//! clipboard or ask for its contents at any time. GNOME's compositor, like
//! KDE's, carries the X clipboard to its Wayland clients and theirs to X, so a
//! background program holding a window nobody sees can do what the desktop's
//! own clipboard manager does on KDE.
//!
//! **Text only, as the guests' clipboard is.** Offered as `UTF8_STRING`, the
//! MIME name, `TEXT` and `STRING`; read as UTF-8 and, from an owner that offers
//! nothing better, Latin-1.

use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xfixes::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ConnectionExt as _, CreateWindowAux, EventMask, PropMode, Property,
    SelectionNotifyEvent, SelectionRequestEvent, Window, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

/// The most text taken from or given to the desktop. The service's channel
/// carries no more than this in one message, so a larger copy could not reach
/// a guest anyway.
const MAX_TEXT: usize = 1 << 20;

/// How long one answer from another client is waited for: an owner that does
/// not reply within it is taken to have nothing to give.
const STEP: Duration = Duration::from_secs(1);

/// How often a lost connection to the X server is tried again.
const RECONNECT: Duration = Duration::from_secs(5);

/// The names this speaks in, asked of the server once.
#[derive(Debug, Clone, Copy)]
struct Atoms {
    clipboard: Atom,
    utf8: Atom,
    plain: Atom,
    text: Atom,
    string: Atom,
    targets: Atom,
    incr: Atom,
    /// The property on our own window that answers are written to.
    property: Atom,
}

/// A connection to the session's X server that owns or reads its clipboard.
pub(crate) struct Clip {
    conn: RustConnection,
    window: Window,
    atoms: Atoms,
    display: String,
    /// What this has put on the clipboard and still owns.
    owned: Option<String>,
    /// What was last seen or set, so a change this host caused is not sent
    /// back to the guest that caused it.
    last: String,
    /// Another client took the clipboard and it has not been read yet.
    dirty: bool,
    /// The server went away; when it is next asked for.
    lost: Option<Instant>,
}

impl std::fmt::Debug for Clip {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Clip")
            .field("display", &self.display)
            .field("window", &self.window)
            .field("owned", &self.owned.as_ref().map(String::len))
            .finish_non_exhaustive()
    }
}

fn says(error: impl std::fmt::Display) -> String {
    error.to_string()
}

impl Clip {
    /// Connect to the session's X server and ask to hear about the clipboard
    /// changing hands.
    pub(crate) fn connect() -> Result<Self, String> {
        let display = std::env::var("DISPLAY")
            .map_err(|_| "no DISPLAY in the session's environment".to_string())?;
        let (conn, screen) =
            x11rb::connect(Some(&display)).map_err(|error| format!("{display}: {error}"))?;
        let root = conn
            .setup()
            .roots
            .get(screen)
            .ok_or_else(|| format!("{display}: no screen {screen}"))?
            .root;
        // **XFixes is what says the clipboard changed hands.** Without it a
        // copy could only be found by asking over and over.
        conn.xfixes_query_version(5, 0)
            .map_err(says)?
            .reply()
            .map_err(|error| format!("{display}: no XFixes, {error}"))?;
        let window = conn.generate_id().map_err(says)?;
        // Never mapped: it exists to own the selection and to receive answers.
        conn.create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            window,
            root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            x11rb::COPY_FROM_PARENT,
            &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )
        .map_err(says)?
        .check()
        .map_err(says)?;
        let atoms = intern(&conn)?;
        conn.xfixes_select_selection_input(
            window,
            atoms.clipboard,
            xfixes::SelectionEventMask::SET_SELECTION_OWNER
                | xfixes::SelectionEventMask::SELECTION_WINDOW_DESTROY
                | xfixes::SelectionEventMask::SELECTION_CLIENT_CLOSE,
        )
        .map_err(says)?
        .check()
        .map_err(says)?;
        let mut clip = Self {
            conn,
            window,
            atoms,
            display,
            owned: None,
            last: String::new(),
            dirty: false,
            lost: None,
        };
        // What is there now is the baseline, not a copy to report.
        clip.last = clip.read()?;
        Ok(clip)
    }

    /// The display this is connected to, for the log.
    pub(crate) fn display(&self) -> &str {
        &self.display
    }

    /// What is on the clipboard now: ours if we own it, otherwise asked of
    /// the owner. Empty when nobody owns it or the owner gave nothing.
    pub(crate) fn read(&mut self) -> Result<String, String> {
        if let Some(owned) = &self.owned {
            return Ok(owned.clone());
        }
        let atoms = self.atoms;
        for target in [atoms.utf8, atoms.string] {
            self.conn
                .delete_property(self.window, atoms.property)
                .map_err(says)?;
            self.conn
                .convert_selection(
                    self.window,
                    atoms.clipboard,
                    target,
                    atoms.property,
                    x11rb::CURRENT_TIME,
                )
                .map_err(says)?;
            if self.answered()? {
                return self.take(target);
            }
        }
        Ok(String::new())
    }

    /// Put a guest's text on the clipboard, owned by this connection until
    /// somebody else copies.
    ///
    /// **Remembered as ours**, so the change it causes is not read back and
    /// sent to the guest that sent it.
    pub(crate) fn write(&mut self, text: &str) -> Result<(), String> {
        if self.lost.is_some() {
            return Err(format!("{}: the X server went away", self.display));
        }
        let text: String = text.chars().take(MAX_TEXT).collect();
        self.conn
            .set_selection_owner(self.window, self.atoms.clipboard, x11rb::CURRENT_TIME)
            .map_err(says)?;
        let owner = self
            .conn
            .get_selection_owner(self.atoms.clipboard)
            .map_err(says)?
            .reply()
            .map_err(says)?
            .owner;
        if owner != self.window {
            self.owned = None;
            return Err(format!(
                "{}: the clipboard stayed with its owner",
                self.display
            ));
        }
        self.owned = Some(text.clone());
        self.last = text;
        Ok(())
    }

    /// Wait for somebody else to copy, and answer with what they copied.
    ///
    /// **Answers `None` when nothing arrived before the deadline**, which is
    /// how the caller gets a turn to do anything else. While it waits it also
    /// answers anybody pasting what this owns.
    pub(crate) fn changed(&mut self, within: Duration) -> Option<String> {
        if let Some(at) = self.lost {
            std::thread::sleep(within);
            if Instant::now() >= at {
                self.reconnect();
            }
            return None;
        }
        let deadline = Instant::now() + within;
        while !self.dirty {
            let event = self.next_event(deadline)?;
            self.handle(event);
        }
        self.dirty = false;
        let now = self.read().ok()?;
        // **Repeats are dropped**, as the bus's clipboard does: an owner that
        // takes the clipboard again with the same text is not a new copy.
        if now.is_empty() || now == self.last {
            return None;
        }
        self.last.clone_from(&now);
        Some(now)
    }

    fn reconnect(&mut self) {
        match Self::connect() {
            Ok(fresh) => {
                lowlat_common::log_info!(
                    "session: the X server is back, display={}",
                    fresh.display
                );
                let last = std::mem::take(&mut self.last);
                *self = fresh;
                self.last = last;
            }
            Err(_) => self.lost = Some(Instant::now() + RECONNECT),
        }
    }

    /// Wait for the owner's answer to a conversion; true when it wrote one.
    fn answered(&mut self) -> Result<bool, String> {
        let deadline = Instant::now() + STEP;
        loop {
            let Some(event) = self.next_event(deadline) else {
                return if self.lost.is_some() {
                    Err(format!("{}: the X server went away", self.display))
                } else {
                    Ok(false)
                };
            };
            if let Event::SelectionNotify(notify) = &event
                && notify.requestor == self.window
                && notify.selection == self.atoms.clipboard
            {
                return Ok(notify.property != x11rb::NONE);
            }
            self.handle(event);
        }
    }

    /// Read the answer the owner wrote to our property, in pieces if it sent
    /// it that way.
    fn take(&mut self, target: Atom) -> Result<String, String> {
        let reply = self.get()?;
        let mut bytes = if reply.type_ == self.atoms.incr {
            self.take_in_pieces()?
        } else {
            reply.value
        };
        bytes.truncate(MAX_TEXT);
        Ok(if target == self.atoms.string {
            bytes.iter().copied().map(char::from).collect()
        } else {
            String::from_utf8_lossy(&bytes).into_owned()
        })
    }

    fn get(&mut self) -> Result<x11rb::protocol::xproto::GetPropertyReply, String> {
        #[allow(
            clippy::cast_possible_truncation,
            reason = "a mebibyte in four-byte units fits a u32"
        )]
        let units = (MAX_TEXT / 4 + 1) as u32;
        self.conn
            .get_property(
                true,
                self.window,
                self.atoms.property,
                AtomEnum::ANY,
                0,
                units,
            )
            .map_err(says)?
            .reply()
            .map_err(says)
    }

    /// **A large copy arrives in pieces** (the ICCCM's INCR): each piece is
    /// written to the property after the last was read and deleted, and an
    /// empty one ends it.
    fn take_in_pieces(&mut self) -> Result<Vec<u8>, String> {
        let mut bytes = Vec::new();
        loop {
            let deadline = Instant::now() + STEP;
            let arrived = loop {
                let Some(event) = self.next_event(deadline) else {
                    break false;
                };
                if let Event::PropertyNotify(note) = &event
                    && note.window == self.window
                    && note.atom == self.atoms.property
                    && note.state == Property::NEW_VALUE
                {
                    break true;
                }
                self.handle(event);
            };
            if !arrived {
                return Ok(bytes);
            }
            let piece = self.get()?.value;
            if piece.is_empty() {
                return Ok(bytes);
            }
            let room = MAX_TEXT.saturating_sub(bytes.len());
            bytes.extend(piece.iter().take(room));
        }
    }

    /// What every wait does with an event that is not the one it waits for.
    fn handle(&mut self, event: Event) {
        match event {
            Event::SelectionRequest(request) => self.serve(&request),
            Event::SelectionClear(clear) if clear.selection == self.atoms.clipboard => {
                self.owned = None;
            }
            Event::XfixesSelectionNotify(note)
                if note.selection == self.atoms.clipboard && note.owner != self.window =>
            {
                // Somebody else has it now, whatever order the server tells
                // us that in.
                self.owned = None;
                self.dirty = true;
            }
            _ => {}
        }
    }

    /// Answer somebody pasting what this owns.
    fn serve(&mut self, request: &SelectionRequestEvent) {
        let atoms = self.atoms;
        // A requestor from before the ICCCM names no property; the target is
        // used in its place.
        let property = if request.property == x11rb::NONE {
            request.target
        } else {
            request.property
        };
        let served = match &self.owned {
            Some(_) if request.selection != atoms.clipboard => false,
            None => false,
            Some(_) if request.target == atoms.targets => self
                .conn
                .change_property32(
                    PropMode::REPLACE,
                    request.requestor,
                    property,
                    AtomEnum::ATOM,
                    &[
                        atoms.targets,
                        atoms.utf8,
                        atoms.plain,
                        atoms.text,
                        atoms.string,
                    ],
                )
                .is_ok(),
            Some(text)
                if request.target == atoms.utf8
                    || request.target == atoms.plain
                    || request.target == atoms.text =>
            {
                let kind = if request.target == atoms.plain {
                    atoms.plain
                } else {
                    atoms.utf8
                };
                self.conn
                    .change_property8(
                        PropMode::REPLACE,
                        request.requestor,
                        property,
                        kind,
                        text.as_bytes(),
                    )
                    .is_ok()
            }
            Some(text) if request.target == atoms.string => {
                let latin: Vec<u8> = text
                    .chars()
                    .map(|c| u8::try_from(u32::from(c)).unwrap_or(b'?'))
                    .collect();
                self.conn
                    .change_property8(
                        PropMode::REPLACE,
                        request.requestor,
                        property,
                        AtomEnum::STRING,
                        &latin,
                    )
                    .is_ok()
            }
            Some(_) => false,
        };
        let notify = SelectionNotifyEvent {
            response_type: x11rb::protocol::xproto::SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: request.time,
            requestor: request.requestor,
            selection: request.selection,
            target: request.target,
            property: if served { property } else { x11rb::NONE },
        };
        let _ = self
            .conn
            .send_event(false, request.requestor, EventMask::NO_EVENT, notify);
        let _ = self.conn.flush();
    }

    /// The next event from the server, or `None` at the deadline or when the
    /// server has gone, which marks the connection lost.
    fn next_event(&mut self, deadline: Instant) -> Option<Event> {
        if self.conn.flush().is_err() {
            self.lose();
            return None;
        }
        loop {
            match self.conn.poll_for_event() {
                Ok(Some(event)) => return Some(event),
                Ok(None) => {}
                Err(_) => {
                    self.lose();
                    return None;
                }
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            let mut ready = libc::pollfd {
                fd: self.conn.stream().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let ms = i32::try_from(left.as_millis()).unwrap_or(i32::MAX).max(1);
            // SAFETY: one pollfd, owned by this frame and alive for the call;
            // the descriptor is the connection's and stays open while `self`
            // is borrowed.
            let answer = unsafe { libc::poll(&raw mut ready, 1, ms) };
            if answer < 0
                && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
            {
                self.lose();
                return None;
            }
        }
    }

    fn lose(&mut self) {
        if self.lost.is_none() {
            lowlat_common::log_warn!(
                "session: the X server went away, display={}; the clipboard waits for it",
                self.display
            );
        }
        self.owned = None;
        self.lost = Some(Instant::now() + RECONNECT);
    }
}

fn intern(conn: &RustConnection) -> Result<Atoms, String> {
    let names: [&[u8]; 8] = [
        b"CLIPBOARD",
        b"UTF8_STRING",
        b"text/plain;charset=utf-8",
        b"TEXT",
        b"STRING",
        b"TARGETS",
        b"INCR",
        b"LOWLAT_CLIPBOARD",
    ];
    let mut cookies = Vec::with_capacity(names.len());
    for name in names {
        cookies.push(conn.intern_atom(false, name).map_err(says)?);
    }
    let mut atoms = [0; 8];
    for (slot, cookie) in atoms.iter_mut().zip(cookies) {
        *slot = cookie.reply().map_err(says)?.atom;
    }
    let [
        clipboard,
        utf8,
        plain,
        text,
        string,
        targets,
        incr,
        property,
    ] = atoms;
    Ok(Atoms {
        clipboard,
        utf8,
        plain,
        text,
        string,
        targets,
        incr,
        property,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **Off by default: it needs a session's X server.** Run it as the person
    /// who is logged in, with `--ignored`. It leaves the clipboard holding what
    /// it found, owned by nobody once the test ends.
    ///
    /// **Two connections on two threads**, because a paste is a conversation:
    /// whoever reads waits for the owner to answer, and the owner answers only
    /// while it waits on its own connection.
    #[test]
    #[ignore = "needs a session's X server"]
    fn the_clipboard_can_be_owned_read_and_watched_through_x() {
        let mut clip = Clip::connect().expect("an X server");
        let held = clip.read().expect("read");

        let written = format!("lowlat x probe {}", std::process::id());
        let outside = format!("copied elsewhere {}", std::process::id());
        clip.write(&written).expect("written");
        assert_eq!(clip.read().expect("read back"), written);

        let (told, heard) = std::sync::mpsc::channel::<String>();
        let other = outside.clone();
        let elsewhere = std::thread::spawn(move || {
            // Its baseline is what the first connection owns: the paste.
            let mut elsewhere = Clip::connect().expect("a second connection");
            told.send(elsewhere.last.clone()).expect("told");
            elsewhere.write(&other).expect("written elsewhere");
            // Answer the first connection's read of it.
            let until = Instant::now() + Duration::from_secs(3);
            while Instant::now() < until {
                let _ = elsewhere.changed(Duration::from_millis(100));
            }
        });

        // **Our own write is not a change; somebody else's is.** Waiting here
        // is also what answers the second connection's read.
        let mut seen = None;
        let until = Instant::now() + Duration::from_secs(3);
        while seen.is_none() && Instant::now() < until {
            seen = clip.changed(Duration::from_millis(100));
        }
        assert_eq!(
            heard.recv().expect("heard").as_str(),
            written,
            "the owner's text was not served to a reader"
        );
        assert_eq!(
            seen.as_deref(),
            Some(outside.as_str()),
            "a copy made elsewhere was not reported"
        );
        elsewhere.join().expect("joined");

        clip.write(&held).expect("put back");
    }

    /// **A Wayland client's copy reaches the X clipboard**, which is the half
    /// a guest is sent. Off by default: it needs a session with Xwayland and
    /// `wl-clipboard`, run with `--ignored`. Whether it passes is the
    /// compositor's to say: on KWin 6.8.3 it fails (measured; most likely
    /// KWin hands its Wayland clipboard to X only while an X window has
    /// focus), which is one reason KDE is read through Klipper instead.
    #[test]
    #[ignore = "needs a session with Xwayland and wl-clipboard"]
    fn a_wayland_copy_reaches_x() {
        let mut clip = Clip::connect().expect("an X server");
        let from_wayland = format!("copied on wayland {}", std::process::id());
        // Its server stays behind to answer pastes; it is given nothing of
        // ours to hold open.
        let copied = std::process::Command::new("wl-copy")
            .arg(&from_wayland)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("wl-copy runs (is wl-clipboard installed?)");
        assert!(copied.success());
        let mut seen = None;
        let until = Instant::now() + Duration::from_secs(3);
        while seen.is_none() && Instant::now() < until {
            seen = clip.changed(Duration::from_millis(100));
        }
        assert_eq!(
            seen.as_deref(),
            Some(from_wayland.as_str()),
            "a Wayland copy was not seen"
        );
    }

    /// **What X owns is what a Wayland client pastes**, which is the half a
    /// guest's copy lands in. Same conditions as the one above.
    #[test]
    #[ignore = "needs a session with Xwayland and wl-clipboard"]
    fn what_x_owns_is_pasted_on_wayland() {
        let mut clip = Clip::connect().expect("an X server");
        let from_x = format!("owned on x {}", std::process::id());
        clip.write(&from_x).expect("written");
        // A moment for the compositor to take the new owner.
        let _ = clip.changed(Duration::from_millis(300));
        let mut paste = std::process::Command::new("wl-paste")
            .arg("--no-newline")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("wl-paste runs (is wl-clipboard installed?)");
        // Answer the compositor's read while the paste waits on it.
        let until = Instant::now() + Duration::from_secs(5);
        while paste.try_wait().expect("waited").is_none() && Instant::now() < until {
            let _ = clip.changed(Duration::from_millis(50));
        }
        let _ = paste.kill();
        let pasted = paste.wait_with_output().expect("pasted");
        assert_eq!(
            String::from_utf8_lossy(&pasted.stdout),
            from_x,
            "a Wayland paste did not get what X owns"
        );
    }
}
