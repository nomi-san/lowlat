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
    /// The frame-rate ceiling asked for; zero is the display's own rate.
    pub(crate) fps: u32,
    /// The rate the stream runs at under that ceiling.
    pub(crate) pace: u32,
    /// The captured display's refresh, which bounds the ceiling; zero where
    /// the display follows the ceiling instead, as the host's own does.
    pub(crate) refresh: u32,
    pub(crate) bitrate_mbps: u32,
    /// One of [`QUALITIES`]' words.
    pub(crate) quality: String,
    pub(crate) cg_level: u32,
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
        self.width = number("width");
        self.fps = number("fps");
        self.pace = number("pace");
        self.refresh = number("refresh");
        self.bitrate_mbps = number("bitrate");
        self.quality = text("quality");
        self.cg_level = number("cgLevel");
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
    ///
    /// **Short, since it is a tooltip and a notice's text too**: the rate,
    /// the frame rate, the quality and the level are each the marked entry of
    /// their own submenu, the guests each an entry of the menu, and the
    /// picture is on the guest's own screen.
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
        format!("Streaming to {guests}")
    }
}

/// What a click on the menu asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Click {
    Kick(u32),
    Bitrate(u32),
    /// A frame-rate ceiling; zero is the display's own rate.
    Fps(u32),
    /// An index into [`QUALITIES`].
    Quality(u8),
    /// A congestion level.
    CgLevel(u8),
    Quit,
}

/// The qualities offered, as the menu names them and as the desk's request
/// and the state carry them.
pub(crate) const QUALITIES: [(&str, &str); 3] = [
    ("Lowest latency", "latency"),
    ("Balanced", "balanced"),
    ("Highest", "quality"),
];

/// The congestion levels offered, by name. **Adaptive is not among them**:
/// it runs as the sensitive level until a signal of the host's own is behind
/// it, so offering it would offer the same thing twice.
const LEVEL_NAMES: [&str; 3] = ["Aggressive", "Sensitive", "Relaxed"];

/// The frame-rate ceilings offered beside the display's own rate.
const OFFERED_FPS: [u32; 5] = [30, 60, 120, 144, 240];

/// What the desk asks of the stream that no guest's message carries, in the
/// words both platforms' trays send it in: the frame rate (the display's own
/// among them), the quality and the congestion level.
pub(crate) fn desk_ask(click: Click) -> Option<serde_json::Value> {
    match click {
        Click::Fps(fps) => Some(serde_json::json!({ "fps": fps })),
        Click::Quality(at) => QUALITIES
            .get(usize::from(at))
            .map(|(_, word)| serde_json::json!({ "quality": word })),
        Click::CgLevel(level) => Some(serde_json::json!({ "cgLevel": level })),
        Click::Kick(_) | Click::Bitrate(_) | Click::Quit => None,
    }
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
pub(crate) const FRAMES: i32 = 5;
pub(crate) const QUALITY: i32 = 6;
pub(crate) const LEVELS: i32 = 7;
/// Separators, which need numbers too; counted up from here.
pub(crate) const SEPARATOR_AT: i32 = 10;
/// A quality entry is its place in [`QUALITIES`] past this.
pub(crate) const QUALITY_AT: i32 = 100;
/// A congestion level's entry is the level past this.
pub(crate) const LEVEL_AT: i32 = 200;
/// A frame rate's entry is the rate past this, the display's own at zero, up
/// to the next range.
pub(crate) const FPS_AT: i32 = 300;
/// A rate entry is the rate in megabits past this, up to the next range.
pub(crate) const RATE_AT: i32 = 1000;
const OFFERED_MBPS: [u32; 5] = [5, 10, 20, 30, 50];
/// A kick entry is the guest's number past this.
pub(crate) const KICK_AT: i32 = 100_000;

/// What a clicked entry asks for, if it asks for anything.
pub(crate) fn clicked(id: i32) -> Option<Click> {
    let past = |at: i32| u32::try_from(id - at).unwrap_or(0);
    let small = |at: i32| u8::try_from(id - at).unwrap_or(0);
    match id {
        QUIT => Some(Click::Quit),
        id if id >= KICK_AT => Some(Click::Kick(past(KICK_AT))),
        id if (RATE_AT..KICK_AT).contains(&id) => Some(Click::Bitrate(past(RATE_AT))),
        id if (FPS_AT..RATE_AT).contains(&id) => Some(Click::Fps(past(FPS_AT))),
        id if (LEVEL_AT..LEVEL_AT + 3).contains(&id) => Some(Click::CgLevel(small(LEVEL_AT))),
        id if (QUALITY_AT..QUALITY_AT + 3).contains(&id) => Some(Click::Quality(small(QUALITY_AT))),
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
    let mut frames = Entry {
        enabled: shown.connected,
        ..Entry::item(FRAMES, "Frame rate")
    };
    // **The display's own rate, said where it is the one in force.**
    let own = if shown.fps == 0 && shown.pace != 0 {
        format!("Display's own ({} fps)", shown.pace)
    } else {
        "Display's own".to_string()
    };
    frames.children.push(Entry {
        radio: Some(shown.fps == 0),
        ..Entry::item(FPS_AT, own)
    });
    for fps in OFFERED_FPS {
        // **Greyed past what the display presents**, since the ceiling is
        // clamped to it; never where the display follows the ceiling.
        frames.children.push(Entry {
            radio: Some(shown.fps == fps),
            enabled: shown.refresh == 0 || fps <= shown.refresh,
            ..Entry::item(
                FPS_AT + i32::try_from(fps).unwrap_or(0),
                format!("{fps} fps"),
            )
        });
    }
    root.children.push(frames);
    let mut quality = Entry {
        enabled: shown.connected,
        ..Entry::item(QUALITY, "Quality")
    };
    for (at, (name, word)) in (0..).zip(QUALITIES) {
        quality.children.push(Entry {
            radio: Some(shown.quality == word),
            ..Entry::item(QUALITY_AT + at, name)
        });
    }
    root.children.push(quality);
    let mut levels = Entry {
        enabled: shown.connected,
        ..Entry::item(LEVELS, "Congestion")
    };
    for (level, name) in (0..).zip(LEVEL_NAMES) {
        levels.children.push(Entry {
            radio: Some(shown.cg_level == level),
            ..Entry::item(LEVEL_AT + i32::try_from(level).unwrap_or(0), name)
        });
    }
    root.children.push(levels);
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
        let common = [
            STATUS,
            SEPARATOR_AT,
            KICK_AT + 2,
            SEPARATOR_AT + 1,
            RATES,
            FRAMES,
            QUALITY,
            LEVELS,
        ];
        assert_eq!(ids(&without), common.to_vec());
        assert_eq!(
            ids(&with),
            [common.as_slice(), &[SEPARATOR_AT + 2, QUIT]].concat()
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

    fn streaming() -> Shown {
        Shown {
            connected: true,
            output: "DISPLAY1".to_string(),
            width: 2560,
            fps: 0,
            pace: 60,
            refresh: 60,
            bitrate_mbps: 30,
            quality: "balanced".to_string(),
            cg_level: 2,
            guests: vec![Guest {
                number: 1,
                connected: true,
                ..Guest::default()
            }],
        }
    }

    /// The submenu's entries as (number, label, enabled, marked).
    fn entries(tree: &Entry, parent: i32) -> Vec<(i32, String, bool, Option<bool>)> {
        let parent = tree
            .children
            .iter()
            .find(|entry| entry.id == parent)
            .expect("the submenu");
        parent
            .children
            .iter()
            .map(|entry| (entry.id, entry.label.clone(), entry.enabled, entry.radio))
            .collect()
    }

    /// **Each setting's submenu marks the one in force**: 30 Mbps among the
    /// rates; the display's own frame rate, its rate said, with the ceilings
    /// past the panel's refresh greyed; the quality by its word; the level by
    /// its number. And the line is short.
    #[test]
    fn each_submenu_marks_the_setting_in_force() {
        let shown = streaming();
        assert_eq!(shown.line(), "Streaming to 1 guest");
        let tree = menu(&shown, false);
        let marked = |parent| {
            entries(&tree, parent)
                .into_iter()
                .filter(|(_, _, _, radio)| *radio == Some(true))
                .map(|(id, ..)| id)
                .collect::<Vec<_>>()
        };
        assert_eq!(marked(RATES), vec![RATE_AT + 30]);
        assert_eq!(marked(FRAMES), vec![FPS_AT]);
        assert_eq!(marked(QUALITY), vec![QUALITY_AT + 1]);
        assert_eq!(marked(LEVELS), vec![LEVEL_AT + 2]);
        let frames = entries(&tree, FRAMES);
        assert_eq!(frames[0].1, "Display's own (60 fps)");
        let enabled: Vec<(i32, bool)> = frames.iter().map(|(id, _, on, _)| (*id, *on)).collect();
        assert_eq!(
            enabled,
            vec![
                (FPS_AT, true),
                (FPS_AT + 30, true),
                (FPS_AT + 60, true),
                (FPS_AT + 120, false),
                (FPS_AT + 144, false),
                (FPS_AT + 240, false),
            ]
        );
        let names: Vec<String> = entries(&tree, LEVELS).into_iter().map(|e| e.1).collect();
        assert_eq!(names, ["Aggressive", "Sensitive", "Relaxed"]);

        // **Where the display follows the ceiling, nothing is greyed**, and a
        // ceiling asked is the one marked.
        let follows = Shown {
            fps: 144,
            refresh: 0,
            ..streaming()
        };
        let tree = menu(&follows, false);
        let frames = entries(&tree, FRAMES);
        assert!(frames.iter().all(|(_, _, on, _)| *on), "{frames:?}");
        assert_eq!(frames[0].1, "Display's own");
        assert_eq!(
            frames.iter().find(|e| e.3 == Some(true)).map(|e| e.0),
            Some(FPS_AT + 144)
        );
    }

    /// **Every entry asks for what it names, and only its own range answers**:
    /// the ends of each new range, and the numbers just past them, which are
    /// nothing to click.
    #[test]
    fn every_entry_asks_for_what_it_names() {
        assert_eq!(clicked(FPS_AT), Some(Click::Fps(0)));
        assert_eq!(clicked(FPS_AT + 240), Some(Click::Fps(240)));
        assert_eq!(clicked(RATE_AT + 30), Some(Click::Bitrate(30)));
        assert_eq!(clicked(QUALITY_AT), Some(Click::Quality(0)));
        assert_eq!(clicked(QUALITY_AT + 2), Some(Click::Quality(2)));
        assert_eq!(clicked(QUALITY_AT + 3), None);
        assert_eq!(clicked(LEVEL_AT), Some(Click::CgLevel(0)));
        assert_eq!(clicked(LEVEL_AT + 2), Some(Click::CgLevel(2)));
        assert_eq!(clicked(LEVEL_AT + 3), None);
        for parent in [STATUS, RATES, NOBODY, FRAMES, QUALITY, LEVELS] {
            assert_eq!(clicked(parent), None, "{parent}");
        }
        for separator in SEPARATOR_AT..SEPARATOR_AT + 3 {
            assert_eq!(clicked(separator), None, "{separator}");
        }
    }

    /// **The desk asks in the words its state is told in**, each a setting no
    /// guest's message carries; a kick and a rate are asked otherwise.
    #[test]
    fn the_desk_asks_in_words() {
        let ask = |click| desk_ask(click).map(|ask| ask.to_string());
        assert_eq!(ask(Click::Fps(0)).as_deref(), Some(r#"{"fps":0}"#));
        assert_eq!(ask(Click::Fps(120)).as_deref(), Some(r#"{"fps":120}"#));
        assert_eq!(
            ask(Click::Quality(0)).as_deref(),
            Some(r#"{"quality":"latency"}"#)
        );
        assert_eq!(
            ask(Click::Quality(2)).as_deref(),
            Some(r#"{"quality":"quality"}"#)
        );
        assert_eq!(ask(Click::Quality(3)), None);
        assert_eq!(ask(Click::CgLevel(2)).as_deref(), Some(r#"{"cgLevel":2}"#));
        assert_eq!(ask(Click::Kick(1)), None);
        assert_eq!(ask(Click::Bitrate(10)), None);
    }
}
