//! The tray, drawn by the desktop: a status notifier item on the session bus.
//!
//! **No toolkit.** The desktop's own panel draws the icon and the menu from
//! what this describes over the bus, so nothing here links a user-interface
//! library and nothing needs a main loop beyond a socket read. Two objects
//! are served: the item, which is a handful of properties the panel reads
//! and a signal for each that changes, and the menu, which is a tree the
//! panel asks for when it opens and an event when something in it is clicked.
//!
//! **One desktop's protocol, which another major desktop serves only through
//! an extension.** Where nothing on the bus registers items this says so and
//! keeps listening for something that will: the register is repeated when
//! the watcher's name changes hands, which is also what survives a panel
//! restart.

use std::io::Write as _;
use std::os::unix::net::UnixStream;

use crate::dbus;

const BUS: &str = "org.freedesktop.DBus";
const BUS_OBJECT: &str = "/org/freedesktop/DBus";
const WATCHER: &str = "org.kde.StatusNotifierWatcher";
const WATCHER_OBJECT: &str = "/StatusNotifierWatcher";
const ITEM: &str = "org.kde.StatusNotifierItem";
const ITEM_OBJECT: &str = "/StatusNotifierItem";
const MENU: &str = "com.canonical.dbusmenu";
const MENU_OBJECT: &str = "/MenuBar";
const PROPERTIES: &str = "org.freedesktop.DBus.Properties";
const INTROSPECTABLE: &str = "org.freedesktop.DBus.Introspectable";
const PEER: &str = "org.freedesktop.DBus.Peer";

/// The icon, by a name both major icon themes carry.
const ICON: &str = "video-display";

/// What the tray shows, as the service last described it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Shown {
    /// Whether there is a service to show at all.
    pub(crate) connected: bool,
    pub(crate) output: String,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) fps: u32,
    pub(crate) bitrate_mbps: u32,
    pub(crate) codec: String,
    pub(crate) guests: Vec<Guest>,
}

/// One seated guest, as a person would want it described.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Guest {
    pub(crate) number: u32,
    pub(crate) owner: bool,
    /// The account's name, or empty where the service gave none.
    pub(crate) name: String,
    /// Whether its media path is up: seated is not connected.
    pub(crate) connected: bool,
}

impl Guest {
    /// What to call this guest: the seat's number, which is what the log and
    /// the roster know it by, and the account's name where there is one.
    fn label(&self) -> String {
        if self.name.is_empty() {
            format!("Guest#{}", self.number)
        } else {
            format!("Guest#{} {}", self.number, self.name)
        }
    }
}

impl Shown {
    /// Take what the service said, keeping whether it is there.
    pub(crate) fn read(&mut self, state: &serde_json::Value) {
        let text = |field: &str| {
            state
                .get(field)
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        let number = |field: &str| {
            state
                .get(field)
                .and_then(serde_json::Value::as_u64)
                .and_then(|value| u32::try_from(value).ok())
                .unwrap_or(0)
        };
        self.output = text("output");
        self.codec = text("codec");
        self.width = number("width");
        self.height = number("height");
        self.fps = number("fps");
        self.bitrate_mbps = number("bitrate");
        self.guests = state
            .get("guests")
            .and_then(serde_json::Value::as_array)
            .map(|guests| {
                guests
                    .iter()
                    .filter_map(|guest| {
                        let flag = |field: &str| {
                            guest
                                .get(field)
                                .and_then(serde_json::Value::as_bool)
                                .unwrap_or(false)
                        };
                        Some(Guest {
                            number: u32::try_from(guest.get("id")?.as_u64()?).ok()?,
                            owner: flag("owner"),
                            name: guest
                                .get("name")
                                .and_then(serde_json::Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                            connected: flag("connected"),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
    }

    /// The desktop's word for whether this deserves a place in the panel.
    ///
    /// **Passive without a service**, which is the convention: the item is
    /// there but asks for nothing, so the panel may fold it away.
    fn status(&self) -> &'static str {
        if self.connected { "Active" } else { "Passive" }
    }

    /// One line saying what the host is doing.
    ///
    /// **Nothing streams with nobody seated**, so the picture is unset then
    /// and the honest word is idle; a guest seated against a picture still
    /// unset is waiting on the display.
    fn line(&self) -> String {
        if !self.connected {
            return "lowlatd is not running".to_string();
        }
        if self.guests.is_empty() {
            return "Idle, nobody connected".to_string();
        }
        let guests = format!(
            "{} guest{}",
            self.guests.len(),
            if self.guests.len() == 1 { "" } else { "s" }
        );
        if self.width == 0 || self.output.is_empty() {
            return format!("Waiting for a display, {guests}");
        }
        format!(
            "Streaming {} {}x{} at {} fps, {} Mbps {}, {guests}",
            self.output, self.width, self.height, self.fps, self.bitrate_mbps, self.codec,
        )
    }
}

/// What a click on the menu asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Click {
    Kick(u32),
    Bitrate(u32),
    Quit,
}

/// Who arrived and who left between two states, by label.
///
/// **Connected, not seated, on both edges.** An arrival is a guest whose
/// media path came up, and a departure is one that had come up and is gone;
/// an attempt that never got that far is nothing a person needs telling
/// about.
pub(crate) fn arrivals(before: &[Guest], after: &[Guest]) -> Vec<(String, bool)> {
    let was_up = |number: u32, guests: &[Guest]| {
        guests
            .iter()
            .any(|guest| guest.number == number && guest.connected)
    };
    let mut told = Vec::new();
    for guest in after {
        if guest.connected && !was_up(guest.number, before) {
            told.push((guest.label(), true));
        }
    }
    for guest in before {
        if guest.connected && !after.iter().any(|now| now.number == guest.number) {
            told.push((guest.label(), false));
        }
    }
    told
}

/// The menu's entries, by the number the panel names them with.
///
/// **The ranges must not meet**, because a click arrives as a number and
/// nothing else: a separator numbered into the rates would be a rate. The
/// kicks are open ended, since a guest's number only grows, so they are the
/// range with nothing above it.
const STATUS: i32 = 1;
const QUIT: i32 = 2;
const RATES: i32 = 3;
const NOBODY: i32 = 4;
/// Separators, which need numbers too; counted up from here.
const SEPARATOR_AT: i32 = 10;
/// A rate entry is the rate in megabits past this, up to the next range.
const RATE_AT: i32 = 1000;
const OFFERED_MBPS: [u32; 4] = [5, 10, 20, 50];
/// A kick entry is the guest's number past this.
const KICK_AT: i32 = 100_000;

/// What a clicked entry asks for, if it asks for anything.
fn clicked(id: i32) -> Option<Click> {
    match id {
        QUIT => Some(Click::Quit),
        id if id >= KICK_AT => Some(Click::Kick(u32::try_from(id - KICK_AT).unwrap_or(0))),
        id if (RATE_AT..KICK_AT).contains(&id) => {
            Some(Click::Bitrate(u32::try_from(id - RATE_AT).unwrap_or(0)))
        }
        _ => None,
    }
}

/// One entry of the menu, as the panel asks for it.
#[derive(Debug)]
struct Entry {
    id: i32,
    label: String,
    enabled: bool,
    separator: bool,
    /// A radio button, and whether it is the one selected.
    radio: Option<bool>,
    children: Vec<Entry>,
}

impl Entry {
    fn item(id: i32, label: impl Into<String>) -> Self {
        Self {
            id,
            label: label.into(),
            enabled: true,
            separator: false,
            radio: None,
            children: Vec::new(),
        }
    }

    fn separator(id: i32) -> Self {
        Self {
            separator: true,
            ..Self::item(id, "")
        }
    }

    fn find(&self, id: i32) -> Option<&Self> {
        if self.id == id {
            return Some(self);
        }
        self.children.iter().find_map(|child| child.find(id))
    }

    fn each<'a>(&'a self, into: &mut Vec<&'a Self>) {
        into.push(self);
        for child in &self.children {
            child.each(into);
        }
    }
}

/// The whole menu, from what is shown.
fn menu(shown: &Shown) -> Entry {
    let mut root = Entry::item(0, "");
    root.children.push(Entry {
        enabled: false,
        ..Entry::item(STATUS, shown.line())
    });
    root.children.push(Entry::separator(SEPARATOR_AT));
    if shown.guests.is_empty() {
        root.children.push(Entry {
            enabled: false,
            ..Entry::item(NOBODY, "No guests")
        });
    }
    for guest in &shown.guests {
        let label = format!(
            "Kick {}{}{}",
            guest.label(),
            if guest.owner { " (owner)" } else { "" },
            if guest.connected { "" } else { " (connecting)" }
        );
        root.children.push(Entry {
            enabled: shown.connected,
            ..Entry::item(KICK_AT + i32::try_from(guest.number).unwrap_or(0), label)
        });
    }
    root.children.push(Entry::separator(SEPARATOR_AT + 1));
    let mut rates = Entry {
        enabled: shown.connected,
        ..Entry::item(RATES, "Bitrate")
    };
    for mbps in OFFERED_MBPS {
        rates.children.push(Entry {
            radio: Some(shown.bitrate_mbps == mbps),
            ..Entry::item(
                RATE_AT + i32::try_from(mbps).unwrap_or(0),
                format!("{mbps} Mbps"),
            )
        });
    }
    root.children.push(rates);
    root.children.push(Entry::separator(SEPARATOR_AT + 2));
    root.children.push(Entry::item(QUIT, "Quit"));
    root
}

/// A body being written, with the alignment the bus requires.
///
/// **Offsets count from the start of the body**, which the header pads to
/// eight, so a body built on its own aligns the same as it will in place.
#[derive(Debug, Default)]
struct Out(Vec<u8>);

impl Out {
    fn pad(&mut self, to: usize) {
        dbus::align(&mut self.0, to);
    }

    fn u32(&mut self, value: u32) {
        self.pad(4);
        self.0.extend_from_slice(&value.to_le_bytes());
    }

    #[allow(
        clippy::cast_sign_loss,
        reason = "the bit pattern is what the wire carries"
    )]
    fn i32(&mut self, value: i32) {
        self.u32(value as u32);
    }

    fn bool(&mut self, value: bool) {
        self.u32(u32::from(value));
    }

    fn str(&mut self, text: &str) {
        dbus::put_string(&mut self.0, text);
    }

    fn variant(&mut self, signature: &str, write: impl FnOnce(&mut Self)) {
        dbus::put_signature(&mut self.0, signature);
        write(self);
    }

    /// An array: its byte length, then the elements from their own alignment.
    fn array(&mut self, element_align: usize, write: impl FnOnce(&mut Self)) {
        self.pad(4);
        let at = self.0.len();
        self.0.extend_from_slice(&[0; 4]);
        self.pad(element_align);
        let start = self.0.len();
        write(self);
        let len = u32::try_from(self.0.len() - start).unwrap_or(u32::MAX);
        if let Some(slot) = self.0.get_mut(at..at + 4) {
            slot.copy_from_slice(&len.to_le_bytes());
        }
    }

    fn structure(&mut self, write: impl FnOnce(&mut Self)) {
        self.pad(8);
        write(self);
    }

    /// One entry of a string-keyed dictionary of variants.
    fn entry(&mut self, key: &str, signature: &str, write: impl FnOnce(&mut Self)) {
        self.pad(8);
        self.str(key);
        self.variant(signature, write);
    }

    /// An empty list of icon pixmaps, which is what naming an icon leaves.
    fn no_pixmaps(&mut self) {
        self.array(8, |_| {});
    }
}

/// A body being read.
#[derive(Debug)]
struct In<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> In<'a> {
    fn u32(&mut self) -> Option<u32> {
        self.at = self.at.next_multiple_of(4);
        let word = self.bytes.get(self.at..self.at + 4)?;
        self.at += 4;
        Some(u32::from_le_bytes(<[u8; 4]>::try_from(word).ok()?))
    }

    #[allow(
        clippy::cast_possible_wrap,
        reason = "the bit pattern is what the wire carries"
    )]
    fn i32(&mut self) -> Option<i32> {
        self.u32().map(|value| value as i32)
    }

    fn str(&mut self) -> Option<&'a str> {
        let len = self.u32()? as usize;
        let text = self.bytes.get(self.at..self.at + len)?;
        self.at += len + 1;
        std::str::from_utf8(text).ok()
    }

    fn i32s(&mut self) -> Option<Vec<i32>> {
        let len = self.u32()? as usize;
        self.at = self.at.next_multiple_of(4);
        let end = self.at + len;
        let mut found = Vec::new();
        while self.at < end {
            found.push(self.i32()?);
        }
        Some(found)
    }
}

/// The item's properties, each as the panel reads it.
fn item_property(out: &mut Out, name: &str, shown: &Shown) -> bool {
    match name {
        "Category" => out.variant("s", |out| out.str("ApplicationStatus")),
        "Id" | "Title" => out.variant("s", |out| out.str("lowlat")),
        "Status" => out.variant("s", |out| out.str(shown.status())),
        "WindowId" => out.variant("i", |out| out.i32(0)),
        "IconName" => out.variant("s", |out| out.str(ICON)),
        "OverlayIconName" | "AttentionIconName" | "AttentionMovieName" | "IconThemePath" => {
            out.variant("s", |out| out.str(""));
        }
        "IconPixmap" | "OverlayIconPixmap" | "AttentionIconPixmap" => {
            out.variant("a(iiay)", Out::no_pixmaps);
        }
        "ToolTip" => out.variant("(sa(iiay)ss)", |out| {
            out.structure(|out| {
                out.str("");
                out.no_pixmaps();
                out.str("lowlat");
                out.str(&shown.line());
            });
        }),
        "ItemIsMenu" => out.variant("b", |out| out.bool(true)),
        "Menu" => out.variant("o", |out| out.str(MENU_OBJECT)),
        _ => return false,
    }
    true
}

const ITEM_PROPERTIES: [&str; 16] = [
    "Category",
    "Id",
    "Title",
    "Status",
    "WindowId",
    "IconName",
    "IconPixmap",
    "OverlayIconName",
    "OverlayIconPixmap",
    "AttentionIconName",
    "AttentionIconPixmap",
    "AttentionMovieName",
    "ToolTip",
    "ItemIsMenu",
    "Menu",
    "IconThemePath",
];

/// The menu's own properties, which describe the protocol rather than the
/// entries.
fn menu_property(out: &mut Out, name: &str) -> bool {
    match name {
        "Version" => out.variant("u", |out| out.u32(3)),
        "Status" => out.variant("s", |out| out.str("normal")),
        "TextDirection" => out.variant("s", |out| out.str("ltr")),
        "IconThemePath" => out.variant("as", |out| out.array(4, |_| {})),
        _ => return false,
    }
    true
}

const MENU_PROPERTIES: [&str; 4] = ["Version", "Status", "TextDirection", "IconThemePath"];

/// One entry's properties, as a dictionary.
fn entry_properties(out: &mut Out, entry: &Entry) {
    out.array(8, |out| {
        if entry.separator {
            out.entry("type", "s", |out| out.str("separator"));
            return;
        }
        if !entry.label.is_empty() {
            out.entry("label", "s", |out| out.str(&entry.label));
        }
        if !entry.enabled {
            out.entry("enabled", "b", |out| out.bool(false));
        }
        if let Some(selected) = entry.radio {
            out.entry("toggle-type", "s", |out| out.str("radio"));
            out.entry("toggle-state", "i", |out| out.i32(i32::from(selected)));
        }
        if !entry.children.is_empty() {
            out.entry("children-display", "s", |out| out.str("submenu"));
        }
    });
}

/// One entry and everything under it, as the layout names them.
fn entry_layout(out: &mut Out, entry: &Entry) {
    out.structure(|out| {
        out.i32(entry.id);
        entry_properties(out, entry);
        out.array(1, |out| {
            for child in &entry.children {
                out.variant("(ia{sv}av)", |out| entry_layout(out, child));
            }
        });
    });
}

/// What an object says it is, for anything that asks.
///
/// Neither panel needs this to draw the item; the bus's own tools do, and
/// they are what the bytes above are checked against.
fn introspection(path: &str) -> String {
    let arg = |name: &str, kind: &str, direction: &str| {
        format!("<arg name=\"{name}\" type=\"{kind}\" direction=\"{direction}\"/>")
    };
    let property = |name: &str, kind: &str| {
        format!("    <property name=\"{name}\" type=\"{kind}\" access=\"read\"/>")
    };
    let mut lines = vec![
        "<!DOCTYPE node PUBLIC \"-//freedesktop//DTD D-BUS Object Introspection 1.0//EN\" \
         \"http://www.freedesktop.org/standards/dbus/1.0/introspect.dtd\">"
            .to_string(),
        "<node>".to_string(),
    ];
    match path {
        ITEM_OBJECT => {
            lines.push(format!("  <interface name=\"{ITEM}\">"));
            for name in ITEM_PROPERTIES {
                let kind = match name {
                    "WindowId" => "i",
                    "ItemIsMenu" => "b",
                    "Menu" => "o",
                    "ToolTip" => "(sa(iiay)ss)",
                    "IconPixmap" | "OverlayIconPixmap" | "AttentionIconPixmap" => "a(iiay)",
                    _ => "s",
                };
                lines.push(property(name, kind));
            }
            for method in ["Activate", "SecondaryActivate", "ContextMenu"] {
                lines.push(format!(
                    "    <method name=\"{method}\">{}{}</method>",
                    arg("x", "i", "in"),
                    arg("y", "i", "in")
                ));
            }
            lines.push(format!(
                "    <method name=\"Scroll\">{}{}</method>",
                arg("delta", "i", "in"),
                arg("orientation", "s", "in")
            ));
            for signal in ["NewTitle", "NewIcon", "NewToolTip"] {
                lines.push(format!("    <signal name=\"{signal}\"/>"));
            }
            lines.push(
                "    <signal name=\"NewStatus\"><arg name=\"status\" type=\"s\"/></signal>"
                    .to_string(),
            );
            lines.push("  </interface>".to_string());
        }
        MENU_OBJECT => {
            lines.push(format!("  <interface name=\"{MENU}\">"));
            lines.push(property("Version", "u"));
            lines.push(property("Status", "s"));
            lines.push(property("TextDirection", "s"));
            lines.push(property("IconThemePath", "as"));
            lines.push(format!(
                "    <method name=\"GetLayout\">{}{}{}{}{}</method>",
                arg("parentId", "i", "in"),
                arg("recursionDepth", "i", "in"),
                arg("propertyNames", "as", "in"),
                arg("revision", "u", "out"),
                arg("layout", "(ia{sv}av)", "out")
            ));
            lines.push(format!(
                "    <method name=\"GetGroupProperties\">{}{}{}</method>",
                arg("ids", "ai", "in"),
                arg("propertyNames", "as", "in"),
                arg("properties", "a(ia{sv})", "out")
            ));
            lines.push(format!(
                "    <method name=\"GetProperty\">{}{}{}</method>",
                arg("id", "i", "in"),
                arg("name", "s", "in"),
                arg("value", "v", "out")
            ));
            lines.push(format!(
                "    <method name=\"Event\">{}{}{}{}</method>",
                arg("id", "i", "in"),
                arg("eventId", "s", "in"),
                arg("data", "v", "in"),
                arg("timestamp", "u", "in")
            ));
            lines.push(format!(
                "    <method name=\"AboutToShow\">{}{}</method>",
                arg("id", "i", "in"),
                arg("needUpdate", "b", "out")
            ));
            lines.push("    <signal name=\"LayoutUpdated\"><arg name=\"revision\" type=\"u\"/><arg name=\"parent\" type=\"i\"/></signal>".to_string());
            lines.push("  </interface>".to_string());
        }
        _ => {}
    }
    lines.push(format!("  <interface name=\"{PROPERTIES}\">"));
    lines.push(format!(
        "    <method name=\"Get\">{}{}{}</method>",
        arg("interface", "s", "in"),
        arg("name", "s", "in"),
        arg("value", "v", "out")
    ));
    lines.push(format!(
        "    <method name=\"GetAll\">{}{}</method>",
        arg("interface", "s", "in"),
        arg("properties", "a{sv}", "out")
    ));
    lines.push("  </interface>".to_string());
    lines.push(format!(
        "  <interface name=\"{INTROSPECTABLE}\"><method name=\"Introspect\">{}</method></interface>",
        arg("xml", "s", "out")
    ));
    lines.push("</node>".to_string());
    lines.join("\n")
}

/// The connection to the session bus, and the two objects it answers for.
///
/// **Written from two threads and read from one.** The panel's calls are
/// answered by the thread that reads the bus; what the service says arrives
/// on another and is announced from there, so the write side is shared and
/// each message goes out whole.
#[derive(Debug)]
pub(crate) struct Bus {
    reader: std::sync::Mutex<UnixStream>,
    writer: std::sync::Mutex<UnixStream>,
    serial: std::sync::atomic::AtomicU32,
    /// Bumped on every change, so a panel holding an older layout re-asks.
    revision: std::sync::atomic::AtomicU32,
}

impl Bus {
    /// Open the session bus, announce the item to whatever registers them,
    /// and ask to hear when that changes hands.
    pub(crate) fn open() -> Result<Self, String> {
        let stream = dbus::open()?;
        let writer = stream
            .try_clone()
            .map_err(|error| format!("clone: {error}"))?;
        let bus = Self {
            reader: std::sync::Mutex::new(stream),
            writer: std::sync::Mutex::new(writer),
            serial: std::sync::atomic::AtomicU32::new(0),
            revision: std::sync::atomic::AtomicU32::new(1),
        };
        // **Before anything else is allowed.** The bus refuses every other
        // call until it has named this connection; the answer is read with
        // everything else, since nothing here needs the name.
        bus.call(BUS, BUS_OBJECT, BUS, "Hello", "", &[])?;
        let mut rule = Out::default();
        rule.str(&format!(
            "type='signal',sender='{BUS}',interface='{BUS}',member='NameOwnerChanged',arg0='{WATCHER}'"
        ));
        bus.call(BUS, BUS_OBJECT, BUS, "AddMatch", "s", &rule.0)?;
        bus.register()?;
        Ok(bus)
    }

    /// Tell the watcher there is an item at our path.
    ///
    /// **The path rather than a name**, so the watcher takes the item under
    /// this connection's own unique name and nothing has to own a well-known
    /// one.
    fn register(&self) -> Result<(), String> {
        let mut body = Out::default();
        body.str(ITEM_OBJECT);
        self.call(
            WATCHER,
            WATCHER_OBJECT,
            WATCHER,
            "RegisterStatusNotifierItem",
            "s",
            &body.0,
        )
        .map(|_| ())
    }

    /// Put a notification on the desktop, the way any application does.
    ///
    /// **Fire and forget.** The server answers with an id nothing here needs,
    /// and a desktop with no notification server refuses the call, which the
    /// bus loop logs like any other refusal.
    pub(crate) fn notify(&self, summary: &str, body: &str) {
        if let Err(error) = self.notify_serial(summary, body) {
            lowlat_common::log_warn!("tray: not notified, {error}");
        }
    }

    /// The call itself, answering with its serial so a test can find the
    /// reply.
    fn notify_serial(&self, summary: &str, body: &str) -> Result<u32, String> {
        let mut out = Out::default();
        out.str("lowlat");
        out.u32(0);
        out.str(ICON);
        out.str(summary);
        out.str(body);
        out.array(4, |_| {});
        out.array(8, |_| {});
        out.i32(-1);
        self.call(
            "org.freedesktop.Notifications",
            "/org/freedesktop/Notifications",
            "org.freedesktop.Notifications",
            "Notify",
            "susssasa{sv}i",
            &out.0,
        )
    }

    fn next_serial(&self) -> u32 {
        self.serial
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            .wrapping_add(1)
    }

    fn send(&self, message: &[u8]) -> Result<(), String> {
        let Ok(mut writer) = self.writer.lock() else {
            return Err("the bus writer is poisoned".to_string());
        };
        writer
            .write_all(message)
            .map_err(|error| format!("write: {error}"))
    }

    /// A method call, sent and not waited for; the answer arrives in `serve`,
    /// naming the serial this answers with.
    fn call(
        &self,
        destination: &str,
        object: &str,
        interface: &str,
        member: &str,
        signature: &str,
        body: &[u8],
    ) -> Result<u32, String> {
        let serial = self.next_serial();
        self.send(&dbus::message(
            dbus::METHOD_CALL,
            0,
            serial,
            &[
                (dbus::PATH, "o", object),
                (dbus::INTERFACE, "s", interface),
                (dbus::MEMBER, "s", member),
                (dbus::DESTINATION, "s", destination),
            ],
            None,
            signature,
            body,
        ))
        .map(|()| serial)
    }

    fn signal(&self, object: &str, interface: &str, member: &str, signature: &str, body: &[u8]) {
        let sent = self.send(&dbus::message(
            dbus::SIGNAL,
            dbus::NO_REPLY,
            self.next_serial(),
            &[
                (dbus::PATH, "o", object),
                (dbus::INTERFACE, "s", interface),
                (dbus::MEMBER, "s", member),
            ],
            None,
            signature,
            body,
        ));
        if let Err(error) = sent {
            lowlat_common::log_warn!("tray: {member} not sent, {error}");
        }
    }

    /// The reply to a call, addressed to whoever made it.
    fn answer(&self, to: &dbus::Message, signature: &str, body: &[u8]) {
        if to.flags & dbus::NO_REPLY != 0 {
            return;
        }
        let sender = to.fields.sender.clone().unwrap_or_default();
        let sent = self.send(&dbus::message(
            dbus::METHOD_RETURN,
            dbus::NO_REPLY,
            self.next_serial(),
            &[(dbus::DESTINATION, "s", &sender)],
            Some(to.serial),
            signature,
            body,
        ));
        if let Err(error) = sent {
            lowlat_common::log_warn!("tray: a reply was not sent, {error}");
        }
    }

    fn refuse(&self, to: &dbus::Message, name: &str, why: &str) {
        if to.flags & dbus::NO_REPLY != 0 {
            return;
        }
        let sender = to.fields.sender.clone().unwrap_or_default();
        let mut body = Out::default();
        body.str(why);
        let sent = self.send(&dbus::message(
            dbus::ERROR,
            dbus::NO_REPLY,
            self.next_serial(),
            &[
                (dbus::DESTINATION, "s", &sender),
                (dbus::ERROR_NAME, "s", name),
            ],
            Some(to.serial),
            "s",
            &body.0,
        ));
        if let Err(error) = sent {
            lowlat_common::log_warn!("tray: a refusal was not sent, {error}");
        }
    }

    /// Say that what is shown changed, so the panel re-reads it.
    ///
    /// **The signals carry no values.** The panel asks for what it wants
    /// after each, which is what keeps the properties in one place.
    pub(crate) fn changed(&self, shown: &Shown) {
        let revision = self
            .revision
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            .wrapping_add(1);
        let mut status = Out::default();
        status.str(shown.status());
        self.signal(ITEM_OBJECT, ITEM, "NewStatus", "s", &status.0);
        self.signal(ITEM_OBJECT, ITEM, "NewToolTip", "", &[]);
        let mut updated = Out::default();
        updated.u32(revision);
        updated.i32(0);
        self.signal(MENU_OBJECT, MENU, "LayoutUpdated", "ui", &updated.0);
    }

    /// Answer the bus until it goes away, and say why it did.
    ///
    /// **Every call is answered, with a refusal where nothing here fits**,
    /// because a panel waiting on a reply that never comes is a panel that
    /// stops asking.
    pub(crate) fn serve(
        &self,
        shown: &std::sync::Mutex<Shown>,
        mut on_click: impl FnMut(Click),
    ) -> String {
        loop {
            let message = {
                let Ok(mut reader) = self.reader.lock() else {
                    return "the bus reader is poisoned".to_string();
                };
                match dbus::read_message(&mut reader) {
                    Ok(message) => message,
                    Err(error) => return error,
                }
            };
            match message.kind {
                dbus::METHOD_CALL => {
                    let seen = shown.lock().map(|shown| shown.clone()).unwrap_or_default();
                    self.handle(&message, &seen, &mut on_click);
                }
                dbus::SIGNAL => {
                    // **The watcher came back.** Whoever draws items has
                    // restarted and knows nothing of this one.
                    if message.fields.member.as_deref() == Some("NameOwnerChanged") {
                        let mut args = In {
                            bytes: &message.body,
                            at: 0,
                        };
                        let name = args.str();
                        let _old = args.str();
                        let new = args.str();
                        if name == Some(WATCHER) && new.is_some_and(|owner| !owner.is_empty()) {
                            lowlat_common::log_info!(
                                "tray: the watcher is back, registering again"
                            );
                            if let Err(error) = self.register() {
                                lowlat_common::log_warn!("tray: not registered, {error}");
                            }
                        }
                    }
                }
                dbus::ERROR => {
                    let mut args = In {
                        bytes: &message.body,
                        at: 0,
                    };
                    lowlat_common::log_warn!(
                        "tray: the bus refused a call, error={} reason={:?}",
                        message.fields.error_name.as_deref().unwrap_or("?"),
                        args.str().unwrap_or("")
                    );
                }
                _ => {}
            }
        }
    }

    /// One call from the panel, answered.
    fn handle(&self, call: &dbus::Message, shown: &Shown, on_click: &mut impl FnMut(Click)) {
        let path = call.fields.path.as_deref().unwrap_or("");
        let interface = call.fields.interface.as_deref().unwrap_or("");
        let member = call.fields.member.as_deref().unwrap_or("");
        let mut args = In {
            bytes: &call.body,
            at: 0,
        };
        let mut out = Out::default();
        match (interface, member) {
            (PEER, "Ping") => self.answer(call, "", &[]),
            (INTROSPECTABLE, "Introspect") => {
                out.str(&introspection(path));
                self.answer(call, "s", &out.0);
            }
            (PROPERTIES, "Get") => {
                let _interface = args.str();
                let name = args.str().unwrap_or("");
                let found = match path {
                    ITEM_OBJECT => item_property(&mut out, name, shown),
                    MENU_OBJECT => menu_property(&mut out, name),
                    _ => false,
                };
                if found {
                    self.answer(call, "v", &out.0);
                } else {
                    self.refuse(call, "org.freedesktop.DBus.Error.InvalidArgs", name);
                }
            }
            (PROPERTIES, "GetAll") => {
                out.array(8, |out| match path {
                    ITEM_OBJECT => {
                        for name in ITEM_PROPERTIES {
                            out.pad(8);
                            out.str(name);
                            item_property(out, name, shown);
                        }
                    }
                    MENU_OBJECT => {
                        for name in MENU_PROPERTIES {
                            out.pad(8);
                            out.str(name);
                            menu_property(out, name);
                        }
                    }
                    _ => {}
                });
                self.answer(call, "a{sv}", &out.0);
            }
            (
                ITEM,
                "Activate"
                | "SecondaryActivate"
                | "ContextMenu"
                | "Scroll"
                | "ProvideXdgActivationToken",
            ) => self.answer(call, "", &[]),
            (MENU, "GetLayout") => {
                let parent = args.i32().unwrap_or(0);
                let tree = menu(shown);
                let Some(from) = tree.find(parent) else {
                    self.refuse(
                        call,
                        "org.freedesktop.DBus.Error.InvalidArgs",
                        "no such entry",
                    );
                    return;
                };
                out.u32(self.revision.load(std::sync::atomic::Ordering::Relaxed));
                entry_layout(&mut out, from);
                self.answer(call, "u(ia{sv}av)", &out.0);
            }
            (MENU, "GetGroupProperties") => {
                let asked = args.i32s().unwrap_or_default();
                let tree = menu(shown);
                let mut all = Vec::new();
                tree.each(&mut all);
                out.array(8, |out| {
                    for entry in all
                        .iter()
                        .filter(|entry| asked.is_empty() || asked.contains(&entry.id))
                    {
                        out.structure(|out| {
                            out.i32(entry.id);
                            entry_properties(out, entry);
                        });
                    }
                });
                self.answer(call, "a(ia{sv})", &out.0);
            }
            (MENU, "GetProperty") => {
                let id = args.i32().unwrap_or(-1);
                let name = args.str().unwrap_or("");
                let tree = menu(shown);
                let found = match (tree.find(id), name) {
                    (Some(entry), "label") => {
                        out.variant("s", |out| out.str(&entry.label));
                        true
                    }
                    (Some(entry), "enabled") => {
                        out.variant("b", |out| out.bool(entry.enabled));
                        true
                    }
                    (Some(entry), "type") if entry.separator => {
                        out.variant("s", |out| out.str("separator"));
                        true
                    }
                    _ => false,
                };
                if found {
                    self.answer(call, "v", &out.0);
                } else {
                    self.refuse(call, "org.freedesktop.DBus.Error.InvalidArgs", name);
                }
            }
            // **Always worth re-asking**, which costs the panel one round
            // trip on open and spares this end a second bookkeeping of what
            // it last handed out.
            (MENU, "AboutToShow") => {
                out.bool(true);
                self.answer(call, "b", &out.0);
            }
            (MENU, "AboutToShowGroup") => {
                out.array(4, |_| {});
                out.array(4, |_| {});
                self.answer(call, "aiai", &out.0);
            }
            (MENU, "Event") => {
                let id = args.i32().unwrap_or(-1);
                let event = args.str().unwrap_or("");
                if event == "clicked"
                    && let Some(click) = clicked(id)
                {
                    on_click(click);
                }
                self.answer(call, "", &[]);
            }
            _ => {
                lowlat_common::log_debug!("tray: {interface}.{member} on {path} is not served");
                self.refuse(
                    call,
                    "org.freedesktop.DBus.Error.UnknownMethod",
                    "not served by this item",
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A reader for what the writer makes, so a layout can be walked back.
    /// **Self-consistency proves the pair agree with each other and nothing
    /// else**; what proves the bytes is the live test below, where the bus's
    /// own tool decodes them.
    fn read_entry(args: &mut In<'_>) -> (i32, Vec<(String, String)>, Vec<i32>) {
        args.at = args.at.next_multiple_of(8);
        let id = args.i32().expect("id");
        let props_len = args.u32().expect("a{sv} length") as usize;
        args.at = args.at.next_multiple_of(8);
        let props_end = args.at + props_len;
        let mut props = Vec::new();
        while args.at < props_end {
            args.at = args.at.next_multiple_of(8);
            let key = args.str().expect("key").to_string();
            let signature_len = usize::from(args.bytes[args.at]);
            let signature =
                std::str::from_utf8(&args.bytes[args.at + 1..args.at + 1 + signature_len])
                    .expect("signature")
                    .to_string();
            args.at += 1 + signature_len + 1;
            let value = match signature.as_str() {
                "s" => args.str().expect("s").to_string(),
                "b" | "i" => args.i32().expect("i").to_string(),
                other => panic!("a property of type {other}"),
            };
            props.push((key, value));
        }
        let children_len = args.u32().expect("av length") as usize;
        let children_end = args.at + children_len;
        let mut children = Vec::new();
        while args.at < children_end {
            let signature_len = usize::from(args.bytes[args.at]);
            args.at += 1 + signature_len + 1;
            let (child, _, _) = read_entry(args);
            children.push(child);
        }
        (id, props, children)
    }

    fn shown() -> Shown {
        Shown {
            connected: true,
            output: "DP-4".to_string(),
            width: 2560,
            height: 1440,
            fps: 120,
            bitrate_mbps: 20,
            codec: "h265".to_string(),
            guests: vec![
                Guest {
                    number: 1,
                    owner: true,
                    name: "someone@example.org".to_string(),
                    connected: true,
                },
                Guest {
                    number: 3,
                    connected: true,
                    ..Guest::default()
                },
            ],
        }
    }

    /// **The menu says what the host is doing and offers what a tray may
    /// ask for**: a kick per guest, the rate, and quitting; and the rate in
    /// force is the one marked.
    #[test]
    fn the_menu_lists_each_guest_and_marks_the_rate_in_force() {
        let shown = shown();
        let mut out = Out::default();
        entry_layout(&mut out, &menu(&shown));
        let mut args = In {
            bytes: &out.0,
            at: 0,
        };
        let (root, props, children) = read_entry(&mut args);
        assert_eq!(root, 0);
        assert_eq!(
            props,
            vec![("children-display".to_string(), "submenu".to_string())]
        );
        assert_eq!(args.at, out.0.len(), "bytes left over after the layout");

        assert_eq!(
            children,
            vec![
                STATUS,
                SEPARATOR_AT,
                KICK_AT + 1,
                KICK_AT + 3,
                SEPARATOR_AT + 1,
                RATES,
                SEPARATOR_AT + 2,
                QUIT
            ]
        );
        let tree = menu(&shown);
        assert_eq!(
            tree.find(STATUS).expect("status").label,
            "Streaming DP-4 2560x1440 at 120 fps, 20 Mbps h265, 2 guests"
        );
        assert!(!tree.find(STATUS).expect("status").enabled);
        assert_eq!(
            tree.find(KICK_AT + 1).expect("kick").label,
            "Kick Guest#1 someone@example.org (owner)"
        );
        assert_eq!(tree.find(KICK_AT + 3).expect("kick").label, "Kick Guest#3");
        let rates = tree.find(RATES).expect("rates");
        let marked: Vec<(i32, Option<bool>)> = rates
            .children
            .iter()
            .map(|rate| (rate.id, rate.radio))
            .collect();
        assert_eq!(
            marked,
            vec![
                (RATE_AT + 5, Some(false)),
                (RATE_AT + 10, Some(false)),
                (RATE_AT + 20, Some(true)),
                (RATE_AT + 50, Some(false)),
            ]
        );

        // What each entry asks for when clicked.
        assert_eq!(clicked(KICK_AT + 3), Some(Click::Kick(3)));
        assert_eq!(clicked(RATE_AT + 50), Some(Click::Bitrate(50)));
        assert_eq!(clicked(QUIT), Some(Click::Quit));
        assert_eq!(clicked(STATUS), None);
        assert_eq!(clicked(RATES), None);
        // **Every separator's number is nothing to click**, which is what the
        // ranges not meeting buys; a separator numbered into the rates was a
        // 9000 Mbps request once.
        for separator in [SEPARATOR_AT, SEPARATOR_AT + 1, SEPARATOR_AT + 2] {
            assert_eq!(clicked(separator), None, "separator {separator}");
        }
    }

    /// **Without a service there is nothing to ask for**, and the menu says
    /// so rather than offering actions that would go nowhere.
    #[test]
    fn without_a_service_the_menu_offers_nothing_to_act_on() {
        let shown = Shown::default();
        assert_eq!(shown.status(), "Passive");
        assert_eq!(shown.line(), "lowlatd is not running");
        let tree = menu(&shown);
        assert!(!tree.find(NOBODY).expect("no guests").enabled);
        assert!(!tree.find(RATES).expect("rates").enabled);
        assert!(
            tree.find(QUIT).expect("quit").enabled,
            "quitting always works"
        );
        assert!(tree.find(KICK_AT + 1).is_none());
    }

    /// The state crosses from what the service publishes to what is shown.
    #[test]
    fn what_the_service_says_is_what_is_shown() {
        let state = serde_json::json!({
            "output": "DP-4", "width": 2560, "height": 1440, "fps": 120,
            "bitrate": 20, "codec": "h265",
            "guests": [
                { "id": 1, "owner": true, "name": "someone@example.org", "connected": true },
                { "id": 3, "owner": false, "connected": true },
            ],
        });
        let mut read = Shown {
            connected: true,
            ..Shown::default()
        };
        read.read(&state);
        assert_eq!(read, shown());
        // A field that is missing reads as nothing rather than as a failure.
        read.read(&serde_json::json!({ "guests": [{ "id": 2 }] }));
        assert_eq!(read.line(), "Waiting for a display, 1 guest");
        assert_eq!(
            read.guests,
            vec![Guest {
                number: 2,
                ..Guest::default()
            }]
        );
        // A guest not yet connected is listed and said so, and is still one
        // a tray may end.
        assert_eq!(
            menu(&read).find(KICK_AT + 2).expect("kick").label,
            "Kick Guest#2 (connecting)"
        );
        read.read(&serde_json::json!({}));
        assert_eq!(read.line(), "Idle, nobody connected");
        assert!(read.guests.is_empty());
    }

    /// **A person is told about a media path coming up and going down, and
    /// nothing else.** An attempt that never connected is not an arrival when
    /// it appears nor a departure when it goes, and a guest already up when
    /// the tray first looks is neither.
    #[test]
    fn a_person_is_told_who_arrived_and_who_left() {
        let up = |number: u32, name: &str| Guest {
            number,
            name: name.to_string(),
            connected: true,
            ..Guest::default()
        };
        let seated = |number: u32| Guest {
            number,
            ..Guest::default()
        };
        // Guest 1 was up, guest 2 was only seated.
        let before = vec![up(1, "someone@example.org"), seated(2)];
        // Guest 2 came up, guest 3 is only seated, guest 1 left.
        let after = vec![up(2, ""), seated(3)];
        assert_eq!(
            arrivals(&before, &after),
            vec![
                ("Guest#2".to_string(), true),
                ("Guest#1 someone@example.org".to_string(), false),
            ]
        );
        // Nothing changed: nothing said.
        assert!(arrivals(&after, &after).is_empty());
        // A seated guest that vanishes without ever connecting: nothing said.
        assert!(arrivals(&[seated(3)], &[]).is_empty());
    }

    /// **Off by default: it needs a desktop session's own bus.** Run it as the
    /// person who is logged in, with `--ignored`; it puts one notification on
    /// that desktop. What it asserts is that the real notification server
    /// answered a well formed call with an id of its own, which is the one
    /// check that catches a body marshalled wrong.
    #[test]
    #[ignore = "needs a session bus"]
    fn a_notification_reaches_the_desktop() {
        let bus = Bus::open().expect("a bus");
        let serial = bus
            .notify_serial("lowlat probe", "this is a test notification")
            .expect("sent");
        let mut reader = bus.reader.lock().expect("the reader");
        loop {
            let message = dbus::read_message(&mut reader).expect("a message");
            if message.fields.reply_serial != Some(serial) {
                continue;
            }
            assert_eq!(
                message.kind,
                dbus::METHOD_RETURN,
                "refused: {:?} {:?}",
                message.fields.error_name,
                String::from_utf8_lossy(&message.body)
            );
            let id = dbus::le32(&message.body, 0);
            assert!(id > 0, "the server gave the notification no id");
            break;
        }
    }

    /// An array's length counts its elements from their own alignment, not
    /// from the length word, which is the one place a hand-rolled writer
    /// gets the bus's framing wrong without noticing.
    #[test]
    fn an_arrays_length_excludes_the_padding_after_it() {
        // A string first, so the array's length word lands at offset 8 and
        // the eight-aligned element starts at 12 after padding.
        let mut out = Out::default();
        out.str("ab");
        out.array(8, |out| {
            out.structure(|out| out.i32(7));
        });
        // "ab": 4 + 2 + 1 = 7 bytes, length word padded to 8, at 8..12,
        // elements padded to 16, one i32 at 16..20.
        assert_eq!(out.0.len(), 20);
        assert_eq!(dbus::le32(&out.0, 8), 4, "the padding was counted");
    }
}
