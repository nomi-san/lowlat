//! The session's side of this host, on Linux: the session helper and the tray,
//! which are roles of this program, and what the service does on each pass
//! with what they say (docs/07-platforms.md section 5.1).

use std::collections::{HashMap, HashSet};

use lowlat_host::admission::Admission;

use crate::service::{Introduced, occupancy};
use crate::{app, channel, dbus, seat, sni};

/// The session agent: connect outward, say what this is, and hold the
/// connection for as long as the session does.
///
/// **It speaks for its own session and nothing else**, so it carries no
/// privilege worth taking and is never asked to do anything it could not do on
/// its own behalf (docs/07-platforms.md section 5.1). What it has to say
/// arrives with the sources of those signals; today it is announced and
/// silent, which is a helper that reports nothing rather than a helper that is
/// absent.
pub(crate) fn session() -> ! {
    // **What this session can do, announced rather than assumed.** The
    // mechanisms differ per desktop and one of them offers no protocol at all,
    // so a helper says what it found and the service answers the honest way
    // for the rest. Nothing is claimed yet: the customers land with their own
    // sources, and a helper that reports nothing is not a helper that is
    // absent.
    // **What this session offers, found rather than assumed.** A desktop with
    // no screen saver of its own, or none that answers on the bus, is a
    // session that cannot hold a screen open, and saying so is what lets the
    // service give the honest answer instead of waiting for one.
    let mut screen = match dbus::Screen::connect() {
        Ok(screen) => Some(screen),
        Err(error) => {
            lowlat_common::log_warn!("session: nothing to ask about the screen, error={error}");
            None
        }
    };
    let clip = match dbus::Clip::connect() {
        Ok(clip) => Some(clip),
        Err(error) => {
            lowlat_common::log_warn!("session: no clipboard to own, error={error}");
            None
        }
    };
    // **Watched rather than asked for, which is the whole reason this is here
    // and not in the service.** A session re-describes an output when it moves
    // and announces one that appears, but only to a client that is still
    // connected; a query that opens, reads and closes learns the layout once
    // and can never learn that it changed.
    let watching = lowlat_host::capture::Watch::open();
    // **Whether the session takes mode requests is a property of the
    // compositor**, asked once here and announced, so a desktop without the
    // mechanism refuses a request at the service rather than here.
    let can = channel::Can {
        idle: screen.is_some(),
        clipboard: clip.is_some(),
        layout: watching.is_some(),
        mode: lowlat_host::capture::mode::offered(),
        ..channel::Can::default()
    };
    // **A thread of its own, because owning a clipboard is a wait.** The
    // desktop says when its selection changed and nothing says when it will,
    // so something has to be sitting on that answer while this reads the
    // service's socket.
    let (copied, to_copy) = std::sync::mpsc::channel::<String>();
    let writer: Writer = std::sync::Arc::new(std::sync::Mutex::new(None));
    let latest: Latest = std::sync::Arc::new(std::sync::Mutex::new(None));
    if let Some((watch, first)) = watching {
        let writer = std::sync::Arc::clone(&writer);
        let latest = std::sync::Arc::clone(&latest);
        if std::thread::Builder::new()
            .name("lowlat-layout".to_string())
            .spawn(move || watch_layout(watch, first, &writer, &latest))
            .is_err()
        {
            lowlat_common::log_warn!("session: no thread for the layout");
        }
    }
    if let Some(clip) = clip {
        let writer = std::sync::Arc::clone(&writer);
        if std::thread::Builder::new()
            .name("lowlat-clip".to_string())
            .spawn(move || own_clipboard(clip, &to_copy, &writer))
            .is_err()
        {
            lowlat_common::log_warn!("session: no thread for the clipboard");
        }
    }
    // **Reconnected rather than exited.** A helper outlives the service by
    // design -- a system service restarts, a session does not -- so losing the
    // socket is a wait rather than an ending. Backed off because the common
    // reason to fail is that there is nothing there yet.
    let mut wait = FIRST_RETRY_MS;
    loop {
        match channel::connect(channel::Role::Helper, can) {
            Ok(mut stream) => {
                lowlat_common::log_info!("session: connected");
                wait = FIRST_RETRY_MS;
                if let Ok(sending) = stream.try_clone()
                    && let Ok(mut writer) = writer.lock()
                {
                    *writer = Some(sending);
                }
                // **The layout is said again on every connection.** The service
                // on the other end may have just started and read nothing, and
                // the layout thread speaks only on change.
                if let Ok(kept) = latest.lock()
                    && let Some(outputs) = kept.as_ref()
                    && let Err(error) =
                        channel::send(&mut stream, channel::layout(outputs).as_bytes())
                {
                    lowlat_common::log_warn!(
                        "session: the service did not take the layout, {error}"
                    );
                }
                let mut body = Vec::new();
                while channel::read_frame(&mut stream, &mut body).is_ok() {
                    if let Some(reason) = channel::is_bye(&body) {
                        lowlat_common::log_info!("session: sent away, reason={reason}");
                        std::process::exit(0);
                    }
                    if let Some(awake) = channel::is_awake(&body) {
                        hold_screen(screen.as_mut(), awake);
                    }
                    if let Some(text) = channel::is_clipboard(&body) {
                        let _ = copied.send(text);
                    }
                    if let Some(asked) = channel::is_mode_ask(&body) {
                        let outcome = change_mode(&asked.output, asked.size, asked.rotation);
                        let answer = channel::reply(asked.id, &outcome);
                        if let Err(error) = channel::send(&mut stream, &answer) {
                            lowlat_common::log_warn!(
                                "session: the service did not take the answer, {error}"
                            );
                        }
                    }
                }
                if let Ok(mut writer) = writer.lock() {
                    *writer = None;
                }
                // **Let go on the way out.** The lease is held while it is
                // asked for, and a socket that has gone is nobody asking. A
                // service that comes back says so again.
                hold_screen(screen.as_mut(), false);
                // **What is known, which is only that the connection ended.**
                // A service that went away and one that is still there both
                // read as a closed socket from here; the case where it is
                // deliberate is the one above, and it says so.
                lowlat_common::log_info!("session: the connection ended");
            }
            Err(error) => {
                lowlat_common::log_warn!("session: cannot reach the service, error={error}");
                wait = (wait * 2).min(LAST_RETRY_MS);
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(wait));
    }
}

/// The tray: `lowlatd tray`, run inside a session.
///
/// **Never load-bearing.** It attaches, shows what the host is doing, asks
/// for a kick or a rate, and detaches; the stream neither waits on it nor
/// notices it go (docs/07-platforms.md section 5). The desktop draws it
/// from what it describes over the session bus, so it links no toolkit and
/// is the same binary as the service for the same reason the helper is: the
/// two sides speak a private protocol, and one build cannot disagree with
/// itself.
pub(crate) fn tray() -> ! {
    let bus = match sni::Bus::open() {
        Ok(bus) => std::sync::Arc::new(bus),
        Err(error) => {
            lowlat_common::log_error!("tray: no session bus to be drawn on, {error}");
            std::process::exit(1);
        }
    };
    let shown = std::sync::Arc::new(std::sync::Mutex::new(sni::Shown::default()));
    let writer: Writer = std::sync::Arc::new(std::sync::Mutex::new(None));
    // **The service on its own thread, the bus on this one.** Each is a
    // blocking read, and what one hears the other announces.
    {
        let bus = std::sync::Arc::clone(&bus);
        let shown = std::sync::Arc::clone(&shown);
        let writer = std::sync::Arc::clone(&writer);
        if std::thread::Builder::new()
            .name("lowlat-tray".to_string())
            .spawn(move || follow_service(&bus, &shown, &writer))
            .is_err()
        {
            lowlat_common::log_error!("tray: no thread for the service");
            std::process::exit(1);
        }
    }
    let why = bus.serve(&shown, |click| act(click, &writer));
    // **The bus going away is the session ending**, and a tray outside a
    // session is nothing.
    lowlat_common::log_info!("tray: the session bus ended, {why}");
    std::process::exit(0);
}

/// Stay connected to the service, and show what it says.
///
/// **Reconnected rather than exited**, as the helper is: a system service
/// restarts and a session does not, so losing the socket is a wait. What is
/// shown meanwhile is that there is nothing to show.
fn follow_service(
    bus: &sni::Bus,
    shown: &std::sync::Mutex<sni::Shown>,
    writer: &std::sync::Mutex<Option<std::os::unix::net::UnixStream>>,
) {
    let mut wait = FIRST_RETRY_MS;
    loop {
        match channel::connect(channel::Role::Tray, channel::Can::default()) {
            Ok(mut stream) => {
                lowlat_common::log_info!("tray: connected");
                wait = FIRST_RETRY_MS;
                if let Ok(sending) = stream.try_clone()
                    && let Ok(mut writer) = writer.lock()
                {
                    *writer = Some(sending);
                }
                if let Ok(mut shown) = shown.lock() {
                    shown.connected = true;
                    bus.changed(&shown);
                }
                // **The first state after a connect is a baseline, not
                // news.** What is there when the tray first looks is shown;
                // what changes afterwards is what a person is told about.
                let mut known: Option<Vec<sni::Guest>> = None;
                let mut body = Vec::new();
                while channel::read_frame(&mut stream, &mut body).is_ok() {
                    if let Some(state) = channel::is_state(&body)
                        && let Ok(mut shown) = shown.lock()
                    {
                        shown.read(&state);
                        bus.changed(&shown);
                        if let Some(before) = known.as_deref() {
                            for (who, arrived) in sni::arrivals(before, &shown.guests) {
                                let what = if arrived { "connected" } else { "disconnected" };
                                bus.notify(&format!("{who} {what}"), "");
                            }
                        }
                        known = Some(shown.guests.clone());
                    }
                    if let Some(reason) = channel::is_bye(&body) {
                        lowlat_common::log_info!("tray: sent away, reason={reason}");
                    }
                }
                if let Ok(mut writer) = writer.lock() {
                    *writer = None;
                }
                if let Ok(mut shown) = shown.lock() {
                    shown.connected = false;
                    bus.changed(&shown);
                }
                lowlat_common::log_info!("tray: the connection ended");
            }
            Err(error) => {
                lowlat_common::log_warn!("tray: cannot reach the service, error={error}");
                wait = (wait * 2).min(LAST_RETRY_MS);
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(wait));
    }
}

/// What a click asks the service for.
///
/// **Not waited on.** The answer is the state the service pushes back: a
/// guest gone from the list, a rate marked; a request that changed nothing
/// is read off the service's own log, where it says who asked.
fn act(click: sni::Click, writer: &std::sync::Mutex<Option<std::os::unix::net::UnixStream>>) {
    let frame = match click {
        sni::Click::Quit => {
            lowlat_common::log_info!("tray: quit");
            std::process::exit(0);
        }
        sni::Click::Kick(guest) => channel::kick(guest),
        sni::Click::Bitrate(mbps) => channel::config(&serde_json::json!({
            "video": [{ "encoderMaxBitrate": mbps }],
        })),
    };
    let sent = writer
        .lock()
        .ok()
        .and_then(|mut writer| writer.as_mut().map(|stream| channel::send(stream, &frame)));
    match sent {
        Some(Ok(())) => lowlat_common::log_info!("tray: asked, {click:?}"),
        Some(Err(error)) => lowlat_common::log_warn!("tray: not asked, {click:?} error={error}"),
        None => lowlat_common::log_warn!("tray: not connected, {click:?} goes nowhere"),
    }
}

/// How often the seat is asked which session owns the display.
///
/// The question costs a few file reads and, where the owning session has no
/// helper, one round trip to its compositor -- measured under a tenth of a
/// millisecond -- so the interval is set by how stale a layout may be after a
/// switch, not by cost.
const SEAT_MS: f64 = 1000.0;

/// Take a layout as the one in force, and tell everything that reads one.
///
/// **Told to every reader, not only the stream.** The output listing, the
/// corner preference and what a guest is told all read the layout for
/// themselves. A layout of nothing withdraws the placement, so the absolute
/// axis spans the picture alone rather than a desktop that is no longer there.
fn adopt_layout(
    seam: &mut Admission,
    layout: &mut Option<Vec<lowlat_host::capture::Output>>,
    chosen: Option<Vec<lowlat_host::capture::Output>>,
) -> Option<lowlat_host::capture::Placement> {
    lowlat_host::capture::tell(chosen.clone());
    *layout = chosen;
    let placed = situate(seam, layout.as_deref());
    if placed.is_none() {
        seam.set_place(None);
    }
    placed
}

/// Tell the stream where its picture sits, from the layout a session gave.
///
/// **The connector, not the identity.** A session names an output the way the
/// display device does; the identity this host selects by carries the device
/// alongside, and the two are the same name with a prefix.
fn situate(
    seam: &mut Admission,
    layout: Option<&[lowlat_host::capture::Output]>,
) -> Option<lowlat_host::capture::Placement> {
    let outputs = layout?;
    let listed = lowlat_host::display::Display::outputs();
    let capturing = lowlat_host::display::captured(&listed, seam.captured())?;
    let place = lowlat_host::capture::place(outputs, &capturing.connector);
    match place {
        Some(place) => lowlat_common::log_info!(
            "lowlatd: {} is {}x{} at {},{} of a {}x{} desktop, rotation={}",
            capturing.connector,
            place.width,
            place.height,
            place.x,
            place.y,
            place.desktop_width,
            place.desktop_height,
            place.rotation as u8
        ),
        None => lowlat_common::log_info!(
            "lowlatd: the session describes no {}, absolute input spans the picture alone",
            capturing.connector
        ),
    }
    seam.set_place(place);
    place
}

/// Report this session's display layout, and every change to it.
///
/// **The first one is sent unasked.** A service that opened its display before
/// a helper connected read the layout once for itself; saying it again costs a
/// frame and covers the case where it read nothing at all.
fn watch_layout(
    mut watch: lowlat_host::capture::Watch,
    first: Vec<lowlat_host::capture::Output>,
    writer: &Writer,
    latest: &Latest,
) {
    let say = |outputs: &[lowlat_host::capture::Output]| {
        lowlat_common::log_info!("session: the desktop has {} output(s)", outputs.len());
        // **Kept, for the next connection.** A service that restarts has
        // read nothing, and the layout is only ever sent on change; the
        // connect loop sends what is kept here the moment it connects.
        if let Ok(mut kept) = latest.lock() {
            *kept = Some(outputs.to_vec());
        }
        let body = channel::layout(outputs);
        if let Ok(mut writer) = writer.lock()
            && let Some(stream) = writer.as_mut()
            && let Err(error) = channel::send(stream, body.as_bytes())
        {
            lowlat_common::log_warn!("session: the service did not take the layout, {error}");
        }
    };
    say(&first);
    // **Only what changed, and for as long as the session lasts.** A session
    // re-sends every field of an output it re-describes, so a report per
    // event would be a report per anything; and a quiet tick is not the
    // session ending, which is the distinction this loop once got wrong and
    // stopped watching after its first second.
    loop {
        match watch.changed(std::time::Duration::from_millis(LAYOUT_TICK_MS)) {
            Ok(Some(outputs)) => say(&outputs),
            Ok(None) => {}
            Err(_) => {
                lowlat_common::log_warn!("session: the layout can no longer be watched");
                return;
            }
        }
    }
}

/// Change an output as the service asked, and say what the session said.
///
/// **Answered inline, on the helper's own deadline**, which is shorter than
/// the service's: a session that is slow answers with a refusal rather than
/// with silence, and the service never has to drop this helper for a
/// compositor that took its time.
fn change_mode(output: &str, size: Option<(u32, u32)>, rotation: Option<u8>) -> Result<(), String> {
    // The wire's code is one-based and the session's transform is not.
    let transform = rotation.map(|rotation| u32::from(rotation.saturating_sub(1)));
    let outcome = lowlat_host::capture::mode::set(
        output,
        lowlat_host::capture::mode::Change { size, transform },
        std::time::Duration::from_millis(MODE_MS),
    );
    match &outcome {
        Ok(()) => {
            lowlat_common::log_info!("session: {output} set to {size:?} transform={transform:?}")
        }
        Err(reason) => lowlat_common::log_warn!(
            "session: {output} not set to {size:?} transform={transform:?}, {reason}"
        ),
    }
    outcome
}

/// How long the helper waits on the compositor for a mode change.
const MODE_MS: u64 = 3_000;

/// How long the layout thread waits on its session before looking again.
///
/// **A deadline on a read, not a poll.** Nothing is asked for on this
/// connection; the wait exists so a session that ends is noticed rather than
/// waited on forever.
const LAYOUT_TICK_MS: u64 = 1_000;

/// Where the clipboard thread writes, when there is a service to write to.
type Writer = std::sync::Arc<std::sync::Mutex<Option<std::os::unix::net::UnixStream>>>;

/// The layout last reported, for a service that connects after it was.
type Latest = std::sync::Arc<std::sync::Mutex<Option<Vec<lowlat_host::capture::Output>>>>;

/// Own the desktop's clipboard: report what it becomes, and set what a guest
/// sent.
///
/// **One connection doing both, on a cadence.** Waiting for the desktop to
/// speak and being told to set something are two things to be doing at once,
/// and a tick is the smaller of the two ways to have both: the other is a poll
/// over the bus and a pipe to wake it. Nothing is timed against this and no
/// data path passes through it -- a person pastes, and a fifth of a second
/// either way is not a thing anybody can see.
///
/// **One connection also keeps the echo out.** What this host sets, it
/// remembers, so the change it causes is not read back and handed to the guest
/// that caused it.
fn own_clipboard(
    mut clip: dbus::Clip,
    to_copy: &std::sync::mpsc::Receiver<String>,
    writer: &Writer,
) {
    loop {
        if let Some(text) = clip.changed(std::time::Duration::from_millis(CLIP_TICK_MS)) {
            let body = serde_json::json!({ "clipboard": text }).to_string();
            lowlat_common::log_info!("session: the desktop copied {} bytes", text.len());
            if let Ok(mut writer) = writer.lock()
                && let Some(stream) = writer.as_mut()
                && let Err(error) = crate::channel::send(stream, body.as_bytes())
            {
                lowlat_common::log_warn!("session: the service did not take it, error={error}");
            }
        }
        while let Ok(text) = to_copy.try_recv() {
            match clip.write(&text) {
                Ok(()) => {
                    lowlat_common::log_info!("session: a guest copied {} bytes here", text.len());
                }
                Err(error) => {
                    lowlat_common::log_warn!("session: the clipboard refused, error={error}");
                }
            }
        }
    }
}

/// How long the clipboard thread waits on the desktop before it looks at what
/// a guest sent.
const CLIP_TICK_MS: u64 = 200;

/// Take or let go of the screen, and say which happened.
///
/// **Reported rather than answered.** The service asked for a state rather
/// than a reply, so what comes back is a line in the log of the session that
/// owns the screen, which is where somebody looking for a screen that blanked
/// anyway would look.
fn hold_screen(screen: Option<&mut dbus::Screen>, awake: bool) {
    let Some(screen) = screen else { return };
    let held = if awake {
        screen.inhibit()
    } else {
        screen.release()
    };
    match held {
        Ok(()) => lowlat_common::log_info!(
            "session: screen asked to stay awake={} holding={}",
            u8::from(awake),
            u8::from(screen.holding())
        ),
        Err(error) => lowlat_common::log_warn!("session: the screen refused, error={error}"),
    }
}

/// How long a session agent waits before asking for the service again, and
/// how long it eventually waits.
///
/// **A cadence, not a deadline**: nothing is timed against these and no data
/// path passes through them, so the coarse sleep this uses is the right one.
const FIRST_RETRY_MS: u64 = 250;
const LAST_RETRY_MS: u64 = 5_000;

/// What the session's side keeps between the service's passes.
pub(crate) struct Side {
    // The layout in force, which output the stream was pointed at when it was
    // last acted on, and where the picture sits in that layout.
    layout: Option<Vec<lowlat_host::capture::Output>>,
    situated: u32,
    placed: Option<lowlat_host::capture::Placement>,
    // The last layout a helper pushed, and whose session it describes.
    helper_layout: Option<(u32, Vec<lowlat_host::capture::Output>)>,
    // When the seat was last asked who owns the display.
    seated: lowlat_common::clock::Time,
    // Whether the session was last told somebody is watching.
    awake: bool,
    // What a tray is shown, and the part of it that is re-read on a change.
    shown: app::Shown,
}

impl Side {
    pub(crate) fn new() -> Self {
        Self {
            layout: None,
            situated: 0,
            placed: None,
            helper_layout: None,
            seated: lowlat_common::clock::Time::now(),
            awake: false,
            shown: app::Shown::default(),
        }
    }

    /// One pass of the service's loop, on the session's side.
    pub(crate) fn pass(
        &mut self,
        seam: &mut Admission,
        settings: &app::Settings,
        peers: &HashMap<String, Introduced>,
        established: &HashSet<String>,
    ) {
        // **A lease, held while somebody is watching.** The screen blanking
        // during a session is the desktop doing exactly what it was told, and
        // nothing below the session can argue with it, so the session is asked
        // (docs/07-platforms.md section 5.1). Said on the change rather than on
        // a timer: it is a state, and a state repeated is a state that can
        // arrive out of order with the one that replaced it.
        let watched = occupancy(seam) > 0;
        if watched != self.awake {
            self.awake = watched;
            channel::screen_awake(watched);
        }

        // **Drained here rather than acted on where it arrives.** The channel
        // reads its socket on its own threads and only this loop may touch the
        // guests, so what a session says waits in a queue until this asks for
        // it (docs/07-platforms.md section 5.1).
        for (uid, said) in channel::take_said() {
            if let Some(text) = channel::is_clipboard(&said) {
                app::clipboard_to_guests(seam, settings, text.as_bytes());
            }
            if let Some(outputs) = channel::is_layout(&said) {
                // **Kept, and applied only if that session owns the display.**
                // A helper in a session that has gone inactive keeps pushing a
                // layout about a desktop nobody is scanning out.
                let owns = seat::active_uid().is_none_or(|active| active == uid);
                self.helper_layout = Some((uid, outputs.clone()));
                if owns {
                    self.placed = adopt_layout(seam, &mut self.layout, Some(outputs));
                }
            }
            // **The answer to a mode request is a line, not an action.** The
            // stream follows whatever the display became on its own.
            match channel::is_reply(&said) {
                Some(Ok(())) => lowlat_common::log_info!("lowlatd: the session set the mode"),
                Some(Err(reason)) => {
                    lowlat_common::log_info!("lowlatd: the session refused the mode, {reason}");
                }
                None => {}
            }
        }
        channel::drop_overdue();

        // **What a tray asked for, acted on here and attributed.** A tray's
        // frames wait in their own queue for the same reason a session's do:
        // only this loop may touch the guests.
        for (who, asked) in channel::take_acted() {
            app::on_action(seam, settings, who, &asked);
        }
        // **Said on every pass while somebody is looking, and sent on
        // change.** The room and the picture both move without an event this
        // loop sees, so what a tray shows is worked out each time round and
        // the channel decides whether it differs from what it last sent.
        // Nothing is worked out for nobody.
        if channel::trays() > 0 {
            channel::state(&app::state(seam, &mut self.shown, peers, established));
        }

        // **The display belongs to whichever session is in front of it, and
        // the login manager says which.** A user switch keeps the first
        // session alive with its helper connected, and puts a greeter with no
        // helper in front of the display; the layout in force has to be that
        // session's. Once a second: where the owning session has a helper,
        // its last push stands; where it has none, its own sockets are asked,
        // which root may do; where they say nothing, the picture is the
        // desktop, which is right for a greeter on one screen.
        if lowlat_common::clock::elapsed_ms(self.seated) >= SEAT_MS {
            self.seated = lowlat_common::clock::Time::now();
            // **No answer is no change.** Between one session and the next
            // the seat has no active session for a moment, and a layout
            // adopted then would be somebody's stale one over a desktop that
            // is about to be described properly.
            let active = seat::active_uid();
            let chosen = match active {
                Some(active) => match self.helper_layout.as_ref() {
                    Some((uid, outputs)) if *uid == active && channel::helper_for(active) => {
                        Some(outputs.clone())
                    }
                    _ => lowlat_host::capture::layout_of(active),
                },
                None => self.layout.clone(),
            };
            if chosen != self.layout {
                lowlat_common::log_info!(
                    "lowlatd: the display's session changed, uid={:?} outputs={}",
                    active,
                    chosen.as_ref().map_or(0, Vec::len)
                );
                self.placed = adopt_layout(seam, &mut self.layout, chosen);
            } else if self.placed.is_none() && self.layout.is_some() && seam.captured() != 0 {
                // **A layout adopted while the display was dark placed
                // nothing**, because nothing was lit to place; the display
                // coming back changes no name the pass below can see, so
                // this asks again until it is placed. **Only while something
                // is being captured**: with no guest seated nothing is, and
                // there is nothing to place -- the pass below places it when
                // a stream starts. Without this an idle host enumerated its
                // display devices and asked the session's sockets once a
                // second, forever, and said so each time.
                self.placed = situate(seam, self.layout.as_deref());
            }
        }

        // **Resolved when either half moves, not only when the desktop does.**
        // The two go stale for different reasons: a desktop that gained a
        // display, and a stream pointed at a different screen. Either leaves a
        // guest's absolute input mapped against a rectangle it is not in.
        let capturing = seam.captured();
        if capturing != self.situated {
            self.situated = capturing;
            self.placed = situate(seam, self.layout.as_deref());
        }
        // **Said every pass, though it is worked out only when it changes.**
        // The stream publishes what it read for itself when a pipeline is
        // built, and that reading is the one-shot one that goes stale; without
        // this, a rebuild for any reason at all would put the stale answer
        // back and nothing would notice. Repeating costs two stores.
        if self.placed.is_some() {
            seam.set_place(self.placed);
        }
    }
}

/// Open the channel the session's programs connect to.
pub(crate) fn listen() {
    channel::listen();
}

/// Hand a guest's clipboard to the session in front of the display; false when
/// none took it.
pub(crate) fn clipboard(body: &[u8]) -> bool {
    channel::clipboard(body)
}

/// Ask the session to set a display's mode.
pub(crate) fn ask_mode(
    output: &str,
    size: Option<(u32, u32)>,
    rotation: Option<u8>,
) -> Result<(), &'static str> {
    channel::ask_mode(channel::ModeAsk {
        output,
        size,
        rotation,
    })
}
