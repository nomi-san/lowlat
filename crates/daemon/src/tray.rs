//! What a tray shows and offers, on every platform that has one: the host's
//! state as a line and a menu, who arrived and who left, and what a click
//! asks for. Linux's desktop draws it from a status notifier item
//! (`sni.rs`); Windows' notification area from the host's own icon
//! (`windows_tray.rs`). One description, so the two trays cannot come to
//! offer different things.

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

    /// One line saying what the host is doing.
    ///
    /// **Nothing streams with nobody seated**, so the picture is unset then
    /// and the honest word is idle; a guest seated against a picture still
    /// unset is waiting on the display.
    pub(crate) fn line(&self) -> String {
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

/// The change a rate click asks the stream for, in the words a guest's own
/// request uses, so the desk's request goes through the same reader.
pub(crate) fn rate_change(mbps: u32) -> serde_json::Value {
    serde_json::json!({ "video": [{ "encoderMaxBitrate": mbps }] })
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
pub(crate) const STATUS: i32 = 1;
pub(crate) const QUIT: i32 = 2;
pub(crate) const RATES: i32 = 3;
pub(crate) const NOBODY: i32 = 4;
/// Separators, which need numbers too; counted up from here.
pub(crate) const SEPARATOR_AT: i32 = 10;
/// A rate entry is the rate in megabits past this, up to the next range.
pub(crate) const RATE_AT: i32 = 1000;
const OFFERED_MBPS: [u32; 4] = [5, 10, 20, 50];
/// A kick entry is the guest's number past this.
pub(crate) const KICK_AT: i32 = 100_000;

/// What a clicked entry asks for, if it asks for anything.
pub(crate) fn clicked(id: i32) -> Option<Click> {
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
pub(crate) struct Entry {
    pub(crate) id: i32,
    pub(crate) label: String,
    pub(crate) enabled: bool,
    pub(crate) separator: bool,
    /// A radio button, and whether it is the one selected.
    pub(crate) radio: Option<bool>,
    pub(crate) children: Vec<Entry>,
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

    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn find(&self, id: i32) -> Option<&Self> {
        if self.id == id {
            return Some(self);
        }
        self.children.iter().find_map(|child| child.find(id))
    }

    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn each<'a>(&'a self, into: &mut Vec<&'a Self>) {
        into.push(self);
        for child in &self.children {
            child.each(into);
        }
    }
}

/// The whole menu, from what is shown.
///
/// **Quit closes a tray that is a program of its own**, as on Linux; where
/// the tray is the host's own icon there is nothing to close but the host,
/// and the entry is left out.
pub(crate) fn menu(shown: &Shown, quit: bool) -> Entry {
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
    if quit {
        root.children.push(Entry::separator(SEPARATOR_AT + 2));
        root.children.push(Entry::item(QUIT, "Quit"));
    }
    root
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The host's own icon offers no quit**: there is no program of the
    /// tray's to close, and quitting the host from it is not something it
    /// offers. The rest of the menu is the same one.
    #[test]
    fn a_menu_without_quit_is_the_same_menu_without_it() {
        let shown = Shown {
            connected: true,
            guests: vec![Guest {
                number: 2,
                connected: true,
                ..Guest::default()
            }],
            ..Shown::default()
        };
        let ids = |tree: &Entry| {
            tree.children
                .iter()
                .map(|entry| entry.id)
                .collect::<Vec<_>>()
        };
        let with = menu(&shown, true);
        let without = menu(&shown, false);
        assert_eq!(
            ids(&without),
            vec![STATUS, SEPARATOR_AT, KICK_AT + 2, SEPARATOR_AT + 1, RATES]
        );
        assert_eq!(
            ids(&with),
            vec![
                STATUS,
                SEPARATOR_AT,
                KICK_AT + 2,
                SEPARATOR_AT + 1,
                RATES,
                SEPARATOR_AT + 2,
                QUIT
            ]
        );
    }

    /// The desk's rate is asked in a guest's own words.
    #[test]
    fn a_rate_is_asked_as_a_guest_asks_it() {
        assert_eq!(
            rate_change(20),
            serde_json::json!({ "video": [{ "encoderMaxBitrate": 20 }] })
        );
    }
}
