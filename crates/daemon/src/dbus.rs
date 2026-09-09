//! Just enough of the session bus to hold the screen awake.
//!
//! **Hand written rather than a library, because the whole of what is needed
//! is two calls on one interface.** A client library for this bus arrives with
//! an executor, which this program keeps to signaling, and with a code
//! generator for interfaces nothing here describes. What it would save is the
//! marshalling below, which is a page and is exercised against a real bus.
//!
//! **The connection is the lease.** Asking the screen saver to stand down
//! hands back a cookie that lives as long as the connection that asked, so a
//! program that asks and exits has not asked at all -- which is why the
//! command line tools for this bus cannot do it and why this lives in the
//! session agent, whose lifetime is the session's.

use std::io::{Read as _, Write as _};
use std::os::unix::net::UnixStream;

/// The name that answers for screen blanking on the desktops that have one.
const SERVICE: &str = "org.freedesktop.ScreenSaver";
const OBJECT: &str = "/org/freedesktop/ScreenSaver";

/// What the desktop is told is holding its screen open.
const WHO: &str = "lowlat";
const WHY: &str = "a remote session is connected";

/// The clipboard, on the desktop that offers this one.
///
/// **The mechanism is the desktop's, not a standard one.** A selection is
/// owned rather than stored, so what serves it has to still be running when
/// somebody pastes; here that is the desktop's own clipboard component, which
/// is always running and whose whole job this is. Another desktop needs
/// another mechanism behind the same capability, and one that has none
/// announces that it has none.
const CLIP: &str = "org.kde.klipper";
const CLIP_OBJECT: &str = "/klipper";
/// **Not the same as the name that owns it.** The service is reached by one
/// and its methods are on the other, and using the name for both is a call the
/// bus answers with a refusal rather than with a hint.
const CLIP_FACE: &str = "org.kde.klipper.klipper";

/// A reply that is not this size is not one this reads.
const HEADER: usize = 16;

/// The most a reply may carry, so a bus that answers nonsense cannot be
/// answered with an allocation.
const MAX_MESSAGE: usize = 64 * 1024;

/// A connection to the session bus, and the lease it is holding.
#[derive(Debug)]
pub(crate) struct Screen {
    stream: UnixStream,
    serial: u32,
    /// What the screen saver gave back, which is what releases it.
    cookie: Option<u32>,
}

impl Screen {
    /// Open the session bus, or say why not.
    ///
    /// **The address comes from the environment**, which is the session's own
    /// account of where its bus is. A session agent has one because it was
    /// started inside a session; a program that has to guess at one is a
    /// program running outside the session it is describing.
    pub(crate) fn connect() -> Result<Self, String> {
        let address =
            std::env::var("DBUS_SESSION_BUS_ADDRESS").map_err(|_| "no session bus".to_string())?;
        let path = socket_of(&address).ok_or_else(|| format!("no socket in {address}"))?;
        let mut stream = UnixStream::connect(&path).map_err(|error| format!("{path}: {error}"))?;
        authenticate(&mut stream)?;
        let mut screen = Self {
            stream,
            serial: 0,
            cookie: None,
        };
        // **Before anything else is allowed.** The bus refuses every other
        // call until it has named this connection.
        screen.call(
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "Hello",
            &[],
        )?;
        Ok(screen)
    }

    /// Whether the screen is being held open right now.
    pub(crate) const fn holding(&self) -> bool {
        self.cookie.is_some()
    }

    /// Ask the desktop to stop blanking, if it is not already being asked.
    pub(crate) fn inhibit(&mut self) -> Result<(), String> {
        if self.cookie.is_some() {
            return Ok(());
        }
        let mut body = Vec::new();
        put_string(&mut body, WHO);
        put_string(&mut body, WHY);
        let reply = self.call(SERVICE, OBJECT, SERVICE, "Inhibit", &[("ss", body)])?;
        let cookie = reply
            .get(..4)
            .and_then(|bytes| <[u8; 4]>::try_from(bytes).ok())
            .map(u32::from_le_bytes)
            .ok_or_else(|| "no cookie in the reply".to_string())?;
        self.cookie = Some(cookie);
        Ok(())
    }

    /// Let the desktop blank again, if it was being asked not to.
    pub(crate) fn release(&mut self) -> Result<(), String> {
        let Some(cookie) = self.cookie.take() else {
            return Ok(());
        };
        let mut body = Vec::new();
        body.extend_from_slice(&cookie.to_le_bytes());
        self.call(SERVICE, OBJECT, SERVICE, "UnInhibit", &[("u", body)])?;
        Ok(())
    }

    /// One method call, and the body of its reply.
    ///
    /// **Errors come back as messages rather than as failures**, so a name
    /// that is not on this bus -- a desktop with no screen saver of its own --
    /// reads as a refusal to be reported rather than as a broken connection.
    fn call(
        &mut self,
        destination: &str,
        object: &str,
        interface: &str,
        member: &str,
        body: &[(&str, Vec<u8>)],
    ) -> Result<Vec<u8>, String> {
        self.serial = self.serial.wrapping_add(1);
        let serial = self.serial;
        let signature: String = body.iter().map(|(text, _)| *text).collect();
        let payload: Vec<u8> = body.iter().flat_map(|(_, bytes)| bytes.clone()).collect();

        let mut message = Vec::new();
        message.extend_from_slice(&[b'l', 1, 0, 1]);
        #[allow(
            clippy::cast_possible_truncation,
            reason = "a body this builds is a handful of bytes"
        )]
        message.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        message.extend_from_slice(&serial.to_le_bytes());

        let mut fields = Vec::new();
        put_field(&mut fields, 1, "o", object);
        put_field(&mut fields, 2, "s", interface);
        put_field(&mut fields, 3, "s", member);
        put_field(&mut fields, 6, "s", destination);
        if !signature.is_empty() {
            put_field(&mut fields, 8, "g", &signature);
        }
        #[allow(
            clippy::cast_possible_truncation,
            reason = "five header fields are a hundred bytes at most"
        )]
        message.extend_from_slice(&(fields.len() as u32).to_le_bytes());
        message.extend_from_slice(&fields);
        align(&mut message, 8);
        message.extend_from_slice(&payload);

        self.stream
            .write_all(&message)
            .map_err(|error| format!("write: {error}"))?;

        // Anything that is not the answer to this call is somebody else's
        // signal arriving on a shared bus, and is passed over.
        loop {
            let (kind, to, body) = self.read_message()?;
            if to != Some(serial) {
                continue;
            }
            return match kind {
                2 => Ok(body),
                _ => Err(format!("{member} refused")),
            };
        }
    }

    /// One message in: its type, the serial it answers, and its body.
    fn read_message(&mut self) -> Result<(u8, Option<u32>, Vec<u8>), String> {
        let mut head = [0u8; HEADER];
        self.stream
            .read_exact(&mut head)
            .map_err(|error| format!("read: {error}"))?;
        let kind = head.get(1).copied().unwrap_or(0);
        let body_len = le32(&head, 4) as usize;
        let fields_len = le32(&head, 12) as usize;
        let padded = fields_len.next_multiple_of(8);
        if body_len > MAX_MESSAGE || padded > MAX_MESSAGE {
            return Err("a message over the cap".to_string());
        }
        let mut rest = vec![0u8; padded + body_len];
        self.stream
            .read_exact(&mut rest)
            .map_err(|error| format!("read: {error}"))?;
        let fields = rest.get(..fields_len).unwrap_or(&[]);
        let body = rest.get(padded..).unwrap_or(&[]).to_vec();
        Ok((kind, reply_serial(fields), body))
    }
}

/// A connection to the session bus that owns the clipboard.
///
/// **Its own connection, not the lease's.** One of them blocks waiting for the
/// desktop to say the clipboard changed while the other is asked to hold a
/// screen open, and a single connection would have to be two things at once.
#[derive(Debug)]
pub(crate) struct Clip {
    screen: Screen,
    /// What was last seen or set, so a change this host caused is not sent
    /// back to the guest that caused it.
    last: String,
}

impl Clip {
    /// Open a connection and ask to hear about clipboard changes.
    pub(crate) fn connect() -> Result<Self, String> {
        let mut screen = Screen::connect()?;
        // **Asked for by name.** The bus delivers a signal only to connections
        // that said they wanted it, so without this the socket is silent.
        let mut rule = Vec::new();
        put_string(
            &mut rule,
            "type='signal',interface='org.kde.klipper.klipper',member='clipboardHistoryUpdated'",
        );
        screen.call(
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "AddMatch",
            &[("s", rule)],
        )?;
        // A desktop that answers the name but not the call is one this cannot
        // use, and finding out now is what the capability is announced from.
        let mut clip = Self {
            screen,
            last: String::new(),
        };
        clip.last = clip.read()?;
        Ok(clip)
    }

    /// What is on the desktop's clipboard now.
    pub(crate) fn read(&mut self) -> Result<String, String> {
        let reply = self
            .screen
            .call(CLIP, CLIP_OBJECT, CLIP_FACE, "getClipboardContents", &[])?;
        Ok(string_at(&reply))
    }

    /// Put a guest's text on the desktop's clipboard.
    ///
    /// **Remembered as ours**, so the change it causes is not read back and
    /// sent to the guest that sent it.
    pub(crate) fn write(&mut self, text: &str) -> Result<(), String> {
        let mut body = Vec::new();
        put_string(&mut body, text);
        self.screen.call(
            CLIP,
            CLIP_OBJECT,
            CLIP_FACE,
            "setClipboardContents",
            &[("s", body)],
        )?;
        self.last = text.to_string();
        Ok(())
    }

    /// Wait for the desktop to say its clipboard changed, and answer with what
    /// it changed to.
    ///
    /// **Answers `None` when nothing arrived before the deadline**, which is
    /// how the caller gets a turn to do anything else. A deadline is not a
    /// failure here and neither is a signal about something else.
    pub(crate) fn changed(&mut self, within: std::time::Duration) -> Option<String> {
        self.screen.stream.set_read_timeout(Some(within)).ok()?;
        let arrived = self.screen.read_message().is_ok();
        self.screen.stream.set_read_timeout(None).ok()?;
        if !arrived {
            return None;
        }
        let now = self.read().ok()?;
        // **Repeats are dropped.** The desktop says its history changed rather
        // than what it changed to, and it says so for a copy this host made
        // itself.
        if now.is_empty() || now == self.last {
            return None;
        }
        self.last.clone_from(&now);
        Some(now)
    }
}

/// The first string in a reply body.
fn string_at(body: &[u8]) -> String {
    let len = le32(body, 0) as usize;
    body.get(4..4 + len)
        .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
        .unwrap_or_default()
}

impl Drop for Screen {
    fn drop(&mut self) {
        // The connection closing releases it in any case; asking first is what
        // makes a desktop's own listing stop naming us before we are gone.
        let _ = self.release();
    }
}

/// The socket a bus address names, if it names one this can open.
fn socket_of(address: &str) -> Option<String> {
    address.split(';').find_map(|part| {
        let rest = part.strip_prefix("unix:")?;
        rest.split(',').find_map(|pair| {
            let path = pair.strip_prefix("path=")?;
            Some(path.to_string())
        })
    })
}

/// The handshake, which this bus requires before any message.
///
/// **The user is the credential.** The bus reads it off the socket the same
/// way this program's own channel does, and the name offered here only has to
/// agree with it.
fn authenticate(stream: &mut UnixStream) -> Result<(), String> {
    // SAFETY: reading the calling process's own user identifier.
    let uid = unsafe { libc::getuid() };
    let hex: String = uid
        .to_string()
        .bytes()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    // The leading zero byte is the bus's own framing, not part of the line.
    stream
        .write_all(format!("\0AUTH EXTERNAL {hex}\r\n").as_bytes())
        .map_err(|error| format!("auth: {error}"))?;
    let line = read_line(stream)?;
    if !line.starts_with("OK ") {
        return Err(format!("auth refused: {line}"));
    }
    stream
        .write_all(b"BEGIN\r\n")
        .map_err(|error| format!("begin: {error}"))
}

fn read_line(stream: &mut UnixStream) -> Result<String, String> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    while line.len() < 512 {
        stream
            .read_exact(&mut byte)
            .map_err(|error| format!("auth read: {error}"))?;
        if byte[0] == b'\n' {
            break;
        }
        if byte[0] != b'\r' {
            line.push(byte[0]);
        }
    }
    String::from_utf8(line).map_err(|_| "auth line is not text".to_string())
}

/// The serial a reply answers, read out of its header fields.
///
/// Field 5 is the one that says so; the rest are skipped by their own
/// declared lengths rather than by knowing what they mean.
fn reply_serial(fields: &[u8]) -> Option<u32> {
    let mut at = 0usize;
    while at + 4 <= fields.len() {
        at = at.next_multiple_of(8);
        let code = *fields.get(at)?;
        let signature_len = usize::from(*fields.get(at + 1)?);
        let signature = fields.get(at + 2..at + 2 + signature_len)?;
        // The signature is followed by its own terminator.
        let mut value = at + 2 + signature_len + 1;
        match signature {
            b"u" => {
                value = value.next_multiple_of(4);
                let found = le32(fields, value);
                if code == 5 {
                    return Some(found);
                }
                at = value + 4;
            }
            b"s" | b"o" => {
                value = value.next_multiple_of(4);
                let len = le32(fields, value) as usize;
                at = value + 4 + len + 1;
            }
            b"g" => {
                let len = usize::from(*fields.get(value)?);
                at = value + 1 + len + 1;
            }
            // A field this does not know ends the walk rather than guessing
            // its width and reading somebody else's bytes as a serial.
            _ => return None,
        }
    }
    None
}

fn le32(bytes: &[u8], at: usize) -> u32 {
    bytes
        .get(at..at + 4)
        .and_then(|slice| <[u8; 4]>::try_from(slice).ok())
        .map_or(0, u32::from_le_bytes)
}

fn align(buf: &mut Vec<u8>, to: usize) {
    buf.resize(buf.len().next_multiple_of(to), 0);
}

fn put_string(buf: &mut Vec<u8>, text: &str) {
    align(buf, 4);
    #[allow(
        clippy::cast_possible_truncation,
        reason = "every string here is a constant of a few dozen bytes"
    )]
    buf.extend_from_slice(&(text.len() as u32).to_le_bytes());
    buf.extend_from_slice(text.as_bytes());
    buf.push(0);
}

fn put_signature(buf: &mut Vec<u8>, text: &str) {
    #[allow(
        clippy::cast_possible_truncation,
        reason = "a signature here is two characters"
    )]
    buf.push(text.len() as u8);
    buf.extend_from_slice(text.as_bytes());
    buf.push(0);
}

/// One header field: its code, then its value wrapped in its own signature.
///
/// **The value is written the way its own type says, not the way a string
/// is.** A signature counts its length in one byte where a string counts it in
/// four, so writing the field that carries the body's signature as a string
/// puts three extra bytes into a header and the bus disconnects without a word
/// about why.
fn put_field(buf: &mut Vec<u8>, code: u8, signature: &str, value: &str) {
    align(buf, 8);
    buf.push(code);
    put_signature(buf, signature);
    if signature == "g" {
        put_signature(buf, value);
    } else {
        put_string(buf, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bus_address_names_its_socket() {
        assert_eq!(
            socket_of("unix:path=/run/user/1000/bus").as_deref(),
            Some("/run/user/1000/bus")
        );
        assert_eq!(
            socket_of("unix:path=/run/user/1000/bus,guid=abc").as_deref(),
            Some("/run/user/1000/bus")
        );
        // An abstract socket is one this cannot open, and saying so is better
        // than opening a file of that name.
        assert_eq!(socket_of("unix:abstract=/tmp/dbus-x"), None);
        assert_eq!(socket_of("tcp:host=localhost,port=1"), None);
    }

    /// The walk over header fields has to skip fields it does not care about
    /// by their own widths, or it reads somebody else's bytes as a serial.
    #[test]
    fn the_serial_is_found_past_fields_of_every_width() {
        let mut fields = Vec::new();
        put_field(&mut fields, 6, "s", "org.freedesktop.DBus");
        put_field(&mut fields, 1, "o", "/org/freedesktop/DBus");
        // Field 8 is a signature, which is counted in one byte rather than
        // four, and getting that wrong loses everything after it.
        align(&mut fields, 8);
        fields.push(8);
        put_signature(&mut fields, "g");
        put_signature(&mut fields, "us");
        align(&mut fields, 8);
        fields.push(5);
        put_signature(&mut fields, "u");
        align(&mut fields, 4);
        fields.extend_from_slice(&4242u32.to_le_bytes());

        assert_eq!(reply_serial(&fields), Some(4242));
    }

    #[test]
    fn a_header_with_no_serial_answers_nothing() {
        let mut fields = Vec::new();
        put_field(&mut fields, 3, "s", "NameAcquired");
        assert_eq!(reply_serial(&fields), None);
    }

    /// **Off by default: it needs a desktop session's own bus.** Run it as the
    /// person who is logged in, with `--ignored`.
    ///
    /// **What it can assert is bounded, and the bound is the interface's.**
    /// The screen saver offers no way to read back what is holding it -- it
    /// answers whether it is active, not who asked it not to be -- and the
    /// desktop's own list of what holds the machine awake is a different
    /// interface that does not carry these. So this asserts that the real
    /// service answered a well formed call with a cookie of its own, which is
    /// exactly the assertion that caught the header field written as a string
    /// where it had to be a signature.
    /// **Off by default: it needs a desktop whose clipboard answers on the
    /// bus.** Run it as the person who is logged in, with `--ignored`. It
    /// leaves the clipboard holding what it found.
    #[test]
    #[ignore = "needs a session bus"]
    fn the_desktop_clipboard_can_be_read_and_written() {
        let mut clip = Clip::connect().expect("a clipboard");
        let held = clip.read().expect("read");

        let written = format!("lowlat probe {}", std::process::id());
        clip.write(&written).expect("written");
        assert_eq!(clip.read().expect("read back"), written);

        // **What this host set is not reported back as a change**, or a guest
        // that pasted here would be handed its own text.
        assert_eq!(
            clip.changed(std::time::Duration::from_millis(300)),
            None,
            "our own write came back as a change"
        );

        // **A change somebody else made is reported.** This is the half the
        // guests are sent, and asserting only that our own write is quiet
        // would pass just as well on a connection that hears nothing at all.
        let mut elsewhere = Clip::connect().expect("a second connection");
        let outside = format!("copied elsewhere {}", std::process::id());
        elsewhere.write(&outside).expect("written elsewhere");
        assert_eq!(
            clip.changed(std::time::Duration::from_millis(2000))
                .as_deref(),
            Some(outside.as_str()),
            "a change made outside this connection was not reported"
        );

        clip.write(&held).expect("put back");
    }

    #[test]
    #[ignore = "needs a session bus"]
    fn the_screen_can_be_held_and_let_go() {
        let mut screen = Screen::connect().expect("a session bus");
        assert!(!screen.holding());
        screen.inhibit().expect("inhibited");
        assert!(screen.holding(), "no cookie came back");
        assert_ne!(screen.cookie, Some(0), "the service answered with nothing");
        screen
            .inhibit()
            .expect("asking twice is not a second lease");
        screen.release().expect("released");
        assert!(!screen.holding());
        screen.release().expect("releasing twice is not an error");
    }
}
