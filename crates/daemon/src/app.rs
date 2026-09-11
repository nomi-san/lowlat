//! The application protocol this daemon speaks over application messages.
//!
//! **None of this is the protocol's, and none of it belongs in the SDK.** The
//! wire carries a sub-identifier and a body and says nothing about either
//! (docs/01-protocol.md 11.2a); what they mean is an agreement between an
//! application and the clients it serves. Two applications using the same
//! opcode are speaking different languages over one channel, so the SDK hands
//! the body over untouched and this is where a language is chosen.
//!
//! The one an established client already speaks is the one implemented here,
//! because it is the one a client asks in without being told to.
//!
//! ```text
//!   client -> host   9   ""              what is the video configuration
//!   host   -> client 11  JSON            this is
//!   client -> host   10  ""              what outputs are there
//!   host   -> client 12  JSON array      these
//!   client -> host   11  JSON            use this configuration
//! ```
//!
//! A client asks 9 and 10 on connecting and again after it acts, so both
//! answers have to be cheap and neither may block.

use lowlat::admission::Admission;
use lowlat::display::{Display, Selectable};

/// Sub-identifiers, as the client that speaks this uses them.
mod id {
    /// What the client asks with.
    pub(crate) const QUERY_CONFIG: u32 = 9;
    pub(crate) const QUERY_OUTPUTS: u32 = 10;
    /// The configuration, in both directions: the host's answer to a query,
    /// and a client's request to change it.
    pub(crate) const CONFIG: u32 = 11;
    /// The outputs, host to client only.
    pub(crate) const OUTPUTS: u32 = 12;
    /// The attention chord, client to host, with an empty body.
    ///
    /// **A message exists for it because a client cannot type it.** The
    /// combination is taken by the operating system the client is running on
    /// before any application sees it, so a remote user physically cannot send
    /// it as keystrokes and asks the host to produce it instead.
    pub(crate) const SECURE_ATTENTION: u32 = 14;
    /// Copied text, in both directions.
    pub(crate) const CLIPBOARD: u32 = 7;
}

/// Which application messages carry a person's own text rather than
/// configuration.
///
/// **What is logged for these is a length and an identifier, never the body.**
/// The exact bytes beside the question are what make a wrong answer findable,
/// and that reasoning holds for configuration and inverts for user text: the
/// same line that would help turns the log into a transcript of everything
/// anybody copies on this desktop. Applies to what is sent as well as to what
/// arrives.
pub(crate) const fn carries_user_text(id: u32) -> bool {
    matches!(id, id::CLIPBOARD)
}

/// Which way copied text may travel for one guest.
///
/// **An owner is not a guest.** Ownership arrives relayed from signaling and
/// is never read from the peer, and the setting names what a *guest* may do
/// (docs/07-platforms.md section 5.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Clipboard {
    #[default]
    Off,
    /// A guest's clipboard reaches the desktop.
    Send,
    /// That, and the desktop's reaches the guest.
    Both,
}

impl Clipboard {
    /// **Anything unrecognised is `off`** -- absent, empty, misspelled, or a
    /// value from a newer version -- so that a typo cannot open a clipboard
    /// and a configuration this build does not understand fails closed.
    pub(crate) fn named(name: Option<&str>) -> Self {
        match name {
            Some("send") => Self::Send,
            Some("both") => Self::Both,
            _ => Self::Off,
        }
    }

    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Send => "send",
            Self::Both => "both",
        }
    }

    /// Whether this guest's own clipboard may reach the desktop.
    const fn takes_from(self, owner: bool) -> bool {
        owner || matches!(self, Self::Send | Self::Both)
    }

    /// Whether the desktop's clipboard may reach this guest.
    const fn gives_to(self, owner: bool) -> bool {
        owner || matches!(self, Self::Both)
    }
}

/// The settings this host was started with.
///
/// **Only what configuration really decides.** The size the stream produces
/// and the output it came from are the display's answers, not these, and are
/// read where they are known rather than carried alongside a request for them.
#[derive(Debug, Clone)]
pub(crate) struct Settings {
    /// The output asked for, if one was. Empty means whichever was lit first.
    pub(crate) output: String,
    pub(crate) bitrate_mbps: u32,
    pub(crate) fps: u32,
    pub(crate) full_fps: bool,
    /// What to stamp into the platform field a host declares itself with.
    ///
    /// **Zero, and a knob rather than a constant, because the value is not
    /// known.** An established host stamps one here and every client stamps
    /// nothing; only a client reads it, and what it does with it has not been
    /// found. It is exposed so the question can be answered by trying values
    /// against a real client rather than by guessing one onto the wire.
    pub(crate) host_os: u32,
    /// Whether this host takes a guest's microphone.
    ///
    /// **A client reads it from the configuration this host publishes, not
    /// from the message that enables it.** The two are one decision and both
    /// have to say the same thing: the message tells a connected peer whether
    /// to send, and this is what its settings panel reads to know the feature
    /// exists at all. Published as zero while it does not, which is what a
    /// host with no microphone support has always said.
    pub(crate) accept_microphone: bool,
    /// Which way copied text may travel for a guest that does not own the
    /// machine.
    pub(crate) guest_clipboard: Clipboard,
    /// Offer one output that does not exist.
    ///
    /// **A probe, off by default.** Whether a reader draws a chooser at all
    /// depends on how many outputs it is offered, and this machine can capture
    /// exactly one -- the second display here is the compositor's own and has no
    /// controller, so nothing below the session can see it. This makes the
    /// count testable without a second physical head. Selecting it is refused.
    pub(crate) fake_output: bool,
}

/// What this host would tell a client it is doing, right now.
#[derive(Debug, Clone)]
struct Video {
    output: String,
    bitrate_mbps: u32,
    fps: u32,
    width: u32,
    height: u32,
    rotated: bool,
    full_fps: bool,
    host_os: u32,
}

/// Describe the stream as it actually is, at the moment of the asking.
///
/// **Built per query rather than once.** A display decides its own size and a
/// host follows it, so a description made when the process started reports a
/// stream nobody is producing -- which is the same fault that once told a peer
/// its pointer was in a 1920x1080 space while the picture was 2560x1440.
///
/// **The output must not be empty.** A client shown a stream with no output
/// has nothing to name and nothing to switch away from, so where none was
/// asked for the one being captured is reported: the first that is lit, which
/// is the same one the display opens.
fn describe(
    picture: Option<(u32, u32)>,
    listed: &[Selectable],
    preferred: Option<&str>,
    captured: u32,
    rotation: lowlat::video::Rotation,
    settings: &Settings,
    live: Option<lowlat::stream::LiveVideo>,
) -> Video {
    // **What is being captured beats what was asked for.** A guest can switch
    // outputs and a display can move to another card by itself, and a reader
    // told the request rather than the result marks the wrong screen -- then
    // picking the right one changes nothing, because the host already believes
    // it is there.
    // **The turn is the display's, as the session told the stream.** Reported
    // as the one flag a reader has for it, which is whether the picture is on
    // its side at all.
    let rotated = rotation != lowlat::video::Rotation::None;
    let running = lowlat::display::captured(listed, captured).map(|output| output.id.clone());
    let output = if let Some(running) = running {
        running
    } else if settings.output.is_empty() {
        // **Asked, not guessed.** Which output a host takes when nobody asked
        // is a decision with rules -- the desktop's corner, then whatever is
        // lit -- and repeating them here would be a second answer to one
        // question. It drifted exactly that way once: a chooser marked the
        // screen this listed first while the stream carried the one at the
        // corner, and picking the marked screen changed nothing because the
        // host already believed it was there.
        preferred.map(str::to_string).unwrap_or_default()
    } else {
        settings.output.clone()
    };
    // **Falling back to the output's own size is not a second opinion.** The
    // stream follows the display, so before it has opened one the display's
    // size is what it is about to produce; it is the *configured* size that
    // would be a different answer, and that is the one never consulted here.
    let (width, height) = picture
        .or_else(|| {
            listed
                .iter()
                .find(|candidate| candidate.id == output)
                .map(|found| (found.width, found.height))
        })
        .unwrap_or((0, 0));
    // **What the stream is running at beats what it was started with.** The
    // rate, the frame rate and the permission to send a repeated picture are
    // all live, so a guest may have changed one a moment ago; a panel told the
    // startup value shows the change never happened and asks for it again.
    // Same rule as the output above, one field along.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a live bitrate in megabits, rounded and floored at zero"
    )]
    let (bitrate_mbps, fps, full_fps) = live.map_or(
        (settings.bitrate_mbps, settings.fps, settings.full_fps),
        |live| {
            (
                live.bitrate_mbps.round().max(0.0) as u32,
                live.fps,
                live.full_fps,
            )
        },
    );
    // **Zero is "follow the display", and a reader has no use for it.** Same
    // rule as the size two fields up: before the stream has opened a display,
    // the display's own rate is what it is about to produce, so that is what
    // is reported rather than the request that has not been answered yet.
    // **Only when nothing has settled one.** A running stream has already
    // resolved its rate against the display it opened, and asking the question
    // a second time here would answer it from a listing that may describe a
    // different output -- reporting a rate the stream is not running at, which
    // is the one thing this whole field exists to avoid.
    let fps = if fps == 0 {
        lowlat::stream::paced(0, refresh_of(listed, &output))
    } else {
        fps
    };
    Video {
        output,
        bitrate_mbps,
        fps,
        width,
        height,
        rotated,
        full_fps,
        host_os: settings.host_os,
    }
}

/// Answer one application message, if it is one this speaks.
///
/// Answers whether it was handled, so a body meant for something else is
/// visible rather than silently swallowed.
pub(crate) fn on_message(
    seam: &mut Admission,
    guest: u32,
    id: u32,
    body: &[u8],
    settings: &Settings,
) -> bool {
    match id {
        id::QUERY_CONFIG => {
            let described = describe(
                seam.picture(),
                &Display::outputs(),
                Display::preferred().as_deref(),
                seam.captured(),
                seam.rotation(),
                settings,
                seam.video(),
            );
            let body = config(&described, settings.accept_microphone);
            answered(seam, guest, id::CONFIG, &body);
            true
        }
        id::QUERY_OUTPUTS => {
            let body = outputs(settings.fake_output);
            answered(seam, guest, id::OUTPUTS, &body);
            true
        }
        id::CONFIG => {
            // **Enumerated once and used twice.** Describing what is running
            // and checking what a guest asked for are the same question about
            // the same machine, and asking the devices twice is how the two
            // come to disagree about what is lit.
            let listed = Display::outputs();
            let described = describe(
                seam.picture(),
                &listed,
                Display::preferred().as_deref(),
                seam.captured(),
                seam.rotation(),
                settings,
                seam.video(),
            );
            apply(seam, body, &described, &listed, &format!("guest {guest}"));
            // **Not answered.** The client asks again with 9 the moment it has
            // sent one of these, so an answer here would arrive beside the one
            // it is about to ask for.
            true
        }
        // **Not answered either**, and there is nothing to answer with: what a
        // guest asked for is a keystroke, and it either happened on this
        // machine or it did not. The guest's own thread says which on its log
        // line, because that is where the keyboard permission is read.
        //
        // **Refused here rather than where it is typed**, because what the
        // chord is worth is a property of this machine rather than of the
        // keyboard it goes out on. In front of a graphical session it reaches
        // the compositor and produces its leave dialog, which is what asking
        // for it means. In front of a text console the terminal translates it
        // itself and the machine restarts, which is not.
        id::SECURE_ATTENTION => {
            if lowlat::inject::console_takes_the_chord() {
                lowlat_common::log_warn!(
                    "lowlatd: guest {guest} asked for the attention chord, refused=console"
                );
            } else {
                seam.secure_attention(guest);
            }
            true
        }
        // **Handed to the session, which is the only thing that can hold a
        // selection.** Putting text on a clipboard is announcing that you own
        // it, and the bytes are asked for later when somebody pastes, so
        // nothing outside a session can do it (docs/07-platforms.md 5.1).
        //
        // **Claimed even when it is refused**, because a refusal is this
        // host's answer rather than a message it did not understand.
        id::CLIPBOARD => {
            let owner = seam.guests().iter().any(|g| g.number == guest && g.owner);
            if settings.guest_clipboard.takes_from(owner) {
                let reached = crate::channel::clipboard(body);
                lowlat_common::log_info!(
                    "lowlatd: guest {guest} sent a clipboard of {} bytes, session={}",
                    body.len(),
                    u8::from(reached)
                );
            } else {
                lowlat_common::log_info!(
                    "lowlatd: guest {guest} sent a clipboard of {} bytes, refused={}",
                    body.len(),
                    settings.guest_clipboard.name()
                );
            }
            true
        }
        _ => false,
    }
}

/// The status a peer renders as having been kicked.
///
/// **Non-zero, and one the peer already has words for.** A peer carries on
/// through a zero, and a value outside its own enumeration shows as a blank
/// reason rather than as one.
const KICKED: i32 = 5;

/// Act on what a tray asked for, saying who asked.
///
/// **The credentials are on the line, and nothing else gates on them yet.**
/// Any local user may act, which is a deferral with its cost written down
/// (docs/07-platforms.md section 5.1); what keeps it a deferral is that a
/// kick or a change can be attributed afterwards.
pub(crate) fn on_action(
    seam: &mut Admission,
    settings: &Settings,
    who: crate::channel::Peer,
    body: &[u8],
) {
    let by = format!("tray pid={} uid={}", who.pid, who.uid);
    if let Some(guest) = crate::channel::is_kick(body) {
        if seam.kick_guest(guest, KICKED) {
            lowlat_common::log_info!("lowlatd: guest {guest} kicked, asked by {by}");
        } else {
            lowlat_common::log_info!("lowlatd: {by} asked to kick guest {guest}, who is not here");
        }
    }
    // **The same reader a guest's request goes to**, so what a tray may change
    // and what a guest may change are one rule rather than two that drift.
    if let Some(config) = crate::channel::is_config(body) {
        let listed = Display::outputs();
        let described = describe(
            seam.picture(),
            &listed,
            Display::preferred().as_deref(),
            seam.captured(),
            seam.rotation(),
            settings,
            seam.video(),
        );
        apply(seam, &config, &described, &listed, &by);
    }
}

/// What a tray is shown, and the one part of it that costs something to
/// find out.
///
/// **The output's name is re-read only when the capture moves.** Everything
/// else a tray shows is a field the seam already holds; the name needs the
/// display devices enumerated, which is not something to do twenty times a
/// second for an icon.
#[derive(Debug, Default)]
pub(crate) struct Shown {
    captured: u32,
    output: String,
}

/// The host as a tray would show it: the picture, its rate, and who is here.
///
/// **Whether anybody needs to hear it is the channel's decision**, which
/// compares against what it last sent; this only says what is true now.
pub(crate) fn state(
    seam: &Admission,
    shown: &mut Shown,
    peers: &std::collections::HashMap<String, crate::Introduced>,
    established: &std::collections::HashSet<String>,
) -> serde_json::Value {
    let captured = seam.captured();
    if captured != shown.captured {
        shown.captured = captured;
        shown.output = lowlat::display::captured(&Display::outputs(), captured)
            .map(|output| output.connector.clone())
            .unwrap_or_default();
    }
    let (width, height) = seam.picture().unwrap_or((0, 0));
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a live bitrate in megabits, rounded and floored at zero"
    )]
    let (fps, bitrate_mbps) = seam.video().map_or((0, 0), |live| {
        (live.fps, live.bitrate_mbps.round().max(0.0) as u32)
    });
    let codec = seam.colour().map_or("", |(codec, _, _)| match codec {
        lowlat::stream::Codec::H264 => "h264",
        lowlat::stream::Codec::H265 => "h265",
    });
    // **Seated is not connected.** A guest has a number from the answer
    // onward, before its media path exists, and a tray telling somebody that
    // a guest arrived should mean the guest is there.
    let guests: Vec<serde_json::Value> = seam
        .guests()
        .iter()
        .map(|guest| {
            serde_json::json!({
                "id": guest.number,
                "owner": guest.owner,
                "name": peers.get(&guest.attempt).map_or("", |peer| peer.name.as_str()),
                "connected": established.contains(&guest.attempt),
            })
        })
        .collect();
    serde_json::json!({
        "output": shown.output,
        "width": width,
        "height": height,
        "fps": fps,
        "bitrate": bitrate_mbps,
        "codec": codec,
        "guests": guests,
    })
}

/// Pass the desktop's own clipboard to the guests that may have it.
///
/// **Sent rather than offered, because that is the shape the message has.**
/// Copied text travels as an application message with no request behind it, so
/// a host that waited to be asked would never send one.
pub(crate) fn clipboard_to_guests(seam: &mut Admission, settings: &Settings, text: &[u8]) {
    let mut reached = 0u32;
    for guest in seam.guests() {
        if !settings.guest_clipboard.gives_to(guest.owner) {
            continue;
        }
        if seam.send_user_data(guest.number, id::CLIPBOARD, text) {
            reached += 1;
        }
    }
    lowlat_common::log_info!(
        "lowlatd: the desktop's clipboard of {} bytes reached {reached} guest(s)",
        text.len()
    );
}

/// Tell every guest what changed about the capture, if anything did.
///
/// **Nobody asked, and that is the point.** A reader asks after it acts, so a
/// change it did not cause -- a display moving to another card, another guest
/// switching outputs -- reaches it only if the host says so. And a change it
/// *did* cause takes a moment to land, so the answer to its own question can
/// still describe the world it was leaving.
///
/// **Compared rather than told, and it no longer has to be.** The stream now
/// raises a capture-changed event from the one place that knows both the size
/// and the output; this remains because the daemon drives the seam directly
/// rather than through the boundary, and an application on the boundary should
/// use the event.
///
/// Answers what is being captured now, so the caller can hold it and call
/// again.
pub(crate) fn announce_capture(seam: &mut Admission, settings: &Settings, last: u32) -> u32 {
    let captured = seam.captured();
    if captured == last {
        return last;
    }
    let listed = Display::outputs();
    let described = describe(
        seam.picture(),
        &listed,
        Display::preferred().as_deref(),
        captured,
        seam.rotation(),
        settings,
        seam.video(),
    );
    let config = config(&described, settings.accept_microphone);
    let outputs = outputs(settings.fake_output);
    for guest in seam.guests() {
        seam.send_user_data(guest.number, id::CONFIG, config.as_bytes());
        seam.send_user_data(guest.number, id::OUTPUTS, outputs.as_bytes());
    }
    lowlat_common::log_info!("lowlatd: capture changed, told every guest: {config}");
    captured
}

/// Tell every guest who is in the room.
///
/// **Sent whenever the room changes, not on a timer and not on request.** A
/// peer has no way to ask, and it needs this to find itself: it matches its own
/// number against the list and takes that entry as what it is allowed to do.
/// A client that never receives one does not know what it is.
///
/// **The shape is the reader's, not ours**, down to details that look
/// pointless from here: a version stamp of two, an always-empty external
/// identifier, and exactly three per-stream metric blocks whether or not there
/// are three streams. A reader that requires a field it does not find falls
/// back to its own idea of the world, and the failure is silence rather than
/// an error.
pub(crate) fn announce_guests(seam: &mut Admission) {
    let body = roster(&seam.guests());
    let reached = seam.send_roster(body.as_bytes());
    lowlat_common::log_info!("lowlatd: told {reached} guest(s) the roster: {body}");
}

/// The roster body, built where it can be read back.
///
/// **Separate from the sending so the shape can be tested**, which is the
/// whole reason it exists: what a reader does with one of these is invisible
/// from here -- it parses it or falls back to its own defaults, and both look
/// like silence.
fn roster(guests: &[lowlat::admission::GuestInfo]) -> String {
    let guests: Vec<serde_json::Value> = guests
        .iter()
        .map(|guest| {
            serde_json::json!({
                "_version": 2,
                "id": guest.number,
                "userID": 0,
                "name": format!("guest {}", guest.number),
                // Always empty in the reader this shape came from.
                "externalID": "",
                "has_avatar": false,
                "owner": guest.owner,
                "perms": {
                    "gamepad": guest.permissions.gamepad,
                    "keyboard": guest.permissions.keyboard,
                    "mouse": guest.permissions.pointer,
                },
                // **One block per channel, in the slots the reader expects
                // them.** The array is the video streams and this host runs
                // one, so the second and third stay zeroed: a reader indexing
                // them has no reason to expect a shorter array, and a zeroed
                // entry is what a stream that never ran looks like.
                "audio": block(&guest.metrics.audio, guest.metrics.network_ms, 0),
                "control": block(&guest.metrics.control, guest.metrics.network_ms, 0),
                "metrics": [
                    block(
                        &guest.metrics.video,
                        guest.metrics.network_ms,
                        guest.metrics.cg_events,
                    ),
                    empty(),
                    empty(),
                ],
            })
        })
        .collect();
    serde_json::Value::Array(guests).to_string()
}

/// One channel's block of telemetry.
///
/// **The round trip is the session's and is repeated into every block**, which
/// is the shape a reader expects: there is one path under all the channels, and
/// a block that left the field out would be read as a round trip of zero.
///
/// **Congestion events are passed in rather than taken from the channel**,
/// because only the video channel is rate controlled and a count reported
/// against sound or control would be a number with no meaning behind it.
fn block(
    channel: &lowlat::admission::ChannelMetrics,
    network_ms: f32,
    cg_events: u32,
) -> serde_json::Value {
    serde_json::json!({
        "packetsSent": channel.packets_sent,
        "fastRTs": channel.fast_rts,
        "slowRTs": channel.slow_rts,
        "cgEvents": cg_events,
        "encodeLatency": number(channel.encode_ms),
        "decodeLatency": number(channel.decode_ms),
        "networkLatency": number(network_ms),
        "bitrate": number(channel.bitrate_mbps),
    })
}

/// A figure a reader can parse, whatever arrived here.
///
/// **A number that is not finite is written as a null, and a null costs the
/// whole roster.** The readers this body is written for require every one of
/// these keys to be a JSON number and abandon the entire guest list -- every
/// guest, not the one bad block -- when one is not, taking with it everything
/// the roster gates. Nothing upstream produces a NaN or an infinity today, and
/// this is what keeps that from being load bearing.
fn number(value: f32) -> f32 {
    if value.is_finite() { value } else { 0.0 }
}

/// A stream that never ran, which is every field zero including the round trip.
///
/// **Not `block` with a zeroed channel.** A stream this host never opened has
/// no path of its own to report a round trip for, and the reader it is written
/// for zero-fills the whole entry rather than half of it.
fn empty() -> serde_json::Value {
    serde_json::json!({
        "packetsSent": 0,
        "fastRTs": 0,
        "slowRTs": 0,
        "cgEvents": 0,
        "encodeLatency": 0.0,
        "decodeLatency": 0.0,
        "networkLatency": 0.0,
        "bitrate": 0.0,
    })
}

/// Send an answer and say what was sent.
///
/// **The body is printed, not summarised.** What a client does with one of
/// these is invisible from here -- it parses it or falls back to its own
/// defaults, and both look like silence -- so the only thing that makes a
/// wrong answer findable is having the exact bytes in the log beside the
/// question they answered.
fn answered(seam: &mut Admission, guest: u32, id: u32, body: &str) {
    let sent = seam.send_user_data(guest, id, body.as_bytes());
    if sent {
        lowlat_common::log_info!("lowlatd: answered guest {guest} id={id} {body}");
    } else {
        lowlat_common::log_info!("lowlatd: guest {guest} could not be answered with id={id}");
    }
}

/// How many streams the configuration describes.
///
/// **One, because this host produces one.** A reader takes as many as the
/// array holds and keeps its own defaults for the rest, so padding it out with
/// streams that do not exist describes streams nobody is producing. Three was
/// tried while the panel was missing for an unrelated reason, and it was not
/// what fixed it.
const STREAMS: usize = 1;

/// The video configuration, as this host would describe itself.
fn config(video: &Video, accept_microphone: bool) -> String {
    let streams: Vec<serde_json::Value> = (0..STREAMS)
        .map(|_| {
            serde_json::json!({
                "output": video.output,
                "encoderMaxBitrate": video.bitrate_mbps,
                "encoderFPS": video.fps,
                "resolutionX": video.width,
                "resolutionY": video.height,
                "rotated": video.rotated,
                "fullFPS": video.full_fps,
                // The platform a host declares itself as. See Settings.
                "hostOS": video.host_os,
            })
        })
        .collect();
    serde_json::json!({
        "virtualTablet": 0,
        // **Not a boolean, and zero is "there is none".** A reader takes this
        // as the mode a virtual microphone runs in; one means it exists while
        // the session does, which is what this host offers. A host that says
        // zero here is a host whose client will never offer the feature,
        // however willing the rest of it is.
        "virtualMicrophone": u32::from(accept_microphone),
        "video": streams,
    })
    .to_string()
}

/// What a probe output is called, and what makes it refusable.
const FAKE_OUTPUT: &str = "fake:NOT-A-DISPLAY";

/// The name a reader sends for "choose for me".
///
/// **The same word means two things in the two directions**, which is not a
/// contradiction: a stream with no output has none, and a request with no
/// output wants whichever one this host would have picked anyway. Observed
/// from a real client: its `Auto` entry sends exactly this.
const AUTO: &str = "none";

/// Every output this host could be asked to capture.
fn outputs(fake: bool) -> String {
    // **The name is what a person picks from and the identity is what comes
    // back**, so they are allowed to differ: the size is in the name because
    // that is what distinguishes two identical monitors on a list, and it must
    // not be in the identity, which has to survive a mode change.
    //
    // **A connector and a size beat a model name here.** The established host
    // reports what the display calls itself, and two identical monitors -- or
    // two of anything a driver names generically -- then appear under the same
    // label with no way to tell which is which. The connector is unique by
    // construction and the size says which screen a person is looking at.
    let listed: Vec<serde_json::Value> = Display::outputs()
        .into_iter()
        .map(|output| {
            serde_json::json!({
                "id": output.id,
                "name": format!("{} ({}x{})", output.connector, output.width, output.height),
                "adapterName": output.id.split(':').next().unwrap_or_default(),
            })
        })
        .collect();
    let mut listed = listed;
    if fake {
        listed.push(serde_json::json!({
            "id": FAKE_OUTPUT,
            "name": "not a display (probe)",
            "adapterName": "fake",
        }));
    }
    serde_json::Value::Array(listed).to_string()
}

/// The frame rate a configuration message asks for, against the display it
/// would run on.
///
/// **A ceiling, and clamped to the display rather than merely bounded by it.**
/// The loop will not run ahead of the display's own present, so a rate above
/// the refresh was never going to be reached -- but it is also what the
/// encoder's per-frame budget is divided by, so honouring the request as asked
/// spends a quarter of each frame on a display presenting a quarter as often.
/// Same rule the pipeline is built with, asked here because this is the other
/// place a rate is chosen.
///
/// **Zero is no change**, as it is for the rate ceiling beside it: a panel
/// that leaves a field alone sends zero, and reading that as a request to
/// follow the display would change something nobody touched.
fn asked_fps(first: &serde_json::Value, current: u32, refresh_hz: u32) -> u32 {
    match first.get("encoderFPS").and_then(serde_json::Value::as_u64) {
        Some(0) | None => current,
        Some(asked) => {
            u32::try_from(asked).map_or(current, |asked| lowlat::stream::paced(asked, refresh_hz))
        }
    }
}

/// How often the output being captured presents, or zero when nothing here
/// knows.
fn refresh_of(listed: &[Selectable], output: &str) -> u32 {
    listed
        .iter()
        .find(|candidate| candidate.id == output)
        .map_or(0, |found| found.refresh_hz)
}

/// Take what a client asked for, and act on the part of it that is ours.
///
/// **`who` is on every line this writes**, because a change to the stream is
/// a host action and a host action says who asked for it: a guest by number,
/// or a tray by the credentials of its connection.
fn apply(seam: &mut Admission, body: &[u8], video: &Video, listed: &[Selectable], who: &str) {
    let Ok(parsed) = serde_json::from_slice::<serde_json::Value>(body) else {
        lowlat_common::log_info!("lowlatd: a configuration arrived that is not JSON, ignoring it");
        return;
    };
    let Some(first) = parsed.get("video").and_then(|v| v.get(0)) else {
        lowlat_common::log_info!(
            "lowlatd: a configuration arrived describing no stream, ignoring it"
        );
        return;
    };

    // **An empty name means no change**, which is how a client asks for
    // everything else in the message without touching the output.
    let wanted = first.get("output").and_then(|v| v.as_str()).unwrap_or("");
    match wanted {
        // Nothing said, so nothing about the capture changes.
        "" => {}
        // **Choose for me.** The selection is cleared rather than pointed
        // somewhere, so the host goes back to whichever output it would have
        // taken on its own.
        AUTO if !video.output.is_empty() => {
            lowlat_common::log_info!("lowlatd: {who} asked for whichever output this host picks");
            seam.select_output(None);
        }
        AUTO => {}
        chosen if chosen == video.output => {}
        // **Checked against what is really there before it is forwarded.** A
        // name nothing is lighting is refused where the display is opened, and
        // that refusal is the display failing to open at all -- which ends every
        // guest on the stream, including the one that asked. A guest naming
        // something that is not there must cost nothing.
        chosen if listed.iter().any(|real| real.id == chosen) => {
            lowlat_common::log_info!("lowlatd: {who} asked to capture {chosen}");
            seam.select_output(Some(chosen.to_string()));
        }
        chosen => {
            lowlat_common::log_info!(
                "lowlatd: {who} asked to capture {chosen}, which nothing here is lighting"
            );
        }
    }

    // **Asked of the session, which owns the display.** A size or a turn is
    // the display's own mode, and on a display this host did not create
    // that belongs to whoever holds it; the session takes requests for it
    // where a person's own display settings already send them
    // (docs/07-platforms.md section 5.1). The stream is not told: it follows
    // whatever the display becomes, as it does when the mode is changed by
    // hand. A request that cannot be put is refused with the reason, so a
    // guest whose request went nowhere can be told why by whoever reads the
    // log rather than by nothing changing.
    //
    // **The output asked about is the one being captured now**, which is
    // also the one the size and the turn describe: a request naming another
    // output arrives above as a switch, and the mode it names is applied to
    // that output on the next request, once the stream has moved.
    let asked_size = match (
        first.get("resolutionX").and_then(serde_json::Value::as_u64),
        first.get("resolutionY").and_then(serde_json::Value::as_u64),
    ) {
        (Some(width), Some(height))
            if width != 0
                && height != 0
                && (width, height) != (u64::from(video.width), u64::from(video.height)) =>
        {
            match (u32::try_from(width), u32::try_from(height)) {
                (Ok(width), Ok(height)) => Some((width, height)),
                _ => None,
            }
        }
        _ => None,
    };
    // **A turn is one flag, and it means a quarter.** The established host
    // turns an upright display a quarter and turns any turned one back, and
    // leaves a display already turned some other way alone; the same here.
    let turned = seam.rotation();
    let asked_rotation = match (
        first.get("rotated").and_then(serde_json::Value::as_bool),
        turned,
    ) {
        (Some(true), lowlat::video::Rotation::None) => Some(lowlat::video::Rotation::Deg90 as u8),
        (Some(false), lowlat::video::Rotation::None) | (None, _) => None,
        (Some(false), _) => Some(lowlat::video::Rotation::None as u8),
        (Some(true), _) => None,
    };
    if asked_size.is_some() || asked_rotation.is_some() {
        let output = lowlat::display::captured(listed, seam.captured())
            .map(|output| output.connector.clone())
            .unwrap_or_default();
        match crate::channel::ask_mode(crate::channel::ModeAsk {
            output: &output,
            size: asked_size,
            rotation: asked_rotation,
        }) {
            Ok(()) => lowlat_common::log_info!(
                "lowlatd: {who} asked for {output} at {:?} rotation={:?}, asking the session",
                asked_size,
                asked_rotation
            ),
            Err(reason) => lowlat_common::log_info!(
                "lowlatd: {who} asked for {output} at {:?} rotation={:?}, refused: {reason}",
                asked_size,
                asked_rotation
            ),
        }
    }

    // **The rest is live and is applied.** The frame rate, the rate ceiling
    // and the permission to send a repeated picture all reach the running
    // loop without rebuilding anything, so a guest asking for one gets it
    // rather than a log line saying it was heard.
    let Some(running) = seam.video() else {
        lowlat_common::log_info!("lowlatd: nothing is streaming, so there is nothing to change");
        return;
    };
    let mut wanted = running;
    // **Not bounded by what this host was started with.** The figure the
    // daemon was launched with is where the stream begins, not a limit on it:
    // the boundary itself takes any positive rate from an application, and a
    // guest's panel is the same request arriving by another road. Clamping to
    // the startup value silently defeats the one thing a guest most wants to
    // do with it, and does so invisibly -- asking for more than it started
    // with produced no change and no message.
    if let Some(asked) = first
        .get("encoderMaxBitrate")
        .and_then(serde_json::Value::as_u64)
        && asked != 0
    {
        #[allow(
            clippy::cast_precision_loss,
            reason = "a bitrate in megabits, exact far past any real one"
        )]
        let asked_mbps = asked as f64;
        wanted.bitrate_mbps = asked_mbps;
    }
    wanted.fps = asked_fps(first, running.fps, refresh_of(listed, &video.output));
    if let Some(asked) = first.get("fullFPS").and_then(serde_json::Value::as_bool) {
        wanted.full_fps = asked;
    }

    // **The floor follows the ceiling down.** A rate lowered under the floor
    // the controller was given leaves it unable to reach what it was told.
    wanted.min_mbps = running.min_mbps.min(wanted.bitrate_mbps);
    if (wanted.bitrate_mbps - running.bitrate_mbps).abs() < f64::EPSILON
        && wanted.fps == running.fps
        && wanted.full_fps == running.full_fps
    {
        return;
    }
    lowlat_common::log_info!(
        "lowlatd: {who} changed the stream, fps={} bitrate={:.1} full_fps={}",
        wanted.fps,
        wanted.bitrate_mbps,
        u8::from(wanted.full_fps)
    );
    seam.set_video(wanted);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one_guest() -> lowlat::admission::GuestInfo {
        lowlat::admission::GuestInfo {
            number: 3,
            attempt: "an-attempt".to_string(),
            permissions: lowlat::inject::Permissions::default(),
            owner: false,
            metrics: lowlat::admission::Metrics {
                cg_events: 7,
                network_ms: 12.5,
                video: lowlat::admission::ChannelMetrics {
                    packets_sent: 900,
                    fast_rts: 5,
                    slow_rts: 2,
                    bitrate_mbps: 18.5,
                    encode_ms: 4.25,
                    decode_ms: 1.75,
                },
                audio: lowlat::admission::ChannelMetrics {
                    packets_sent: 120,
                    fast_rts: 1,
                    slow_rts: 0,
                    bitrate_mbps: 0.125,
                    encode_ms: 0.05,
                    decode_ms: 0.5,
                },
                control: lowlat::admission::ChannelMetrics {
                    packets_sent: 40,
                    fast_rts: 0,
                    slow_rts: 1,
                    bitrate_mbps: 0.01,
                    ..lowlat::admission::ChannelMetrics::default()
                },
                ..lowlat::admission::Metrics::default()
            },
        }
    }

    /// **Compared with a tolerance, because the fields are `f32`.** A rate
    /// widened to `f64` for serialisation is not the decimal literal it was
    /// written as, and a test that demanded it would fail on a correct value.
    #[track_caller]
    fn near(found: &serde_json::Value, want: f64) {
        let got = found.as_f64().unwrap_or(f64::NAN);
        assert!(
            (got - want).abs() < 1e-6,
            "expected about {want}, found {got}"
        );
    }

    /// **The roster carries what the guest is doing, not a row of zeros.** A
    /// reader paints these over the figures its own messages gave it, so a
    /// zeroed block does not read as "not reported yet" -- it reads as a
    /// stream that has stopped, and it alternates with the truth once a
    /// second.
    #[test]
    fn the_roster_carries_each_channels_own_numbers() {
        let body: serde_json::Value =
            serde_json::from_str(&roster(&[one_guest()])).expect("valid JSON");
        let guest = &body[0];

        assert_eq!(guest["metrics"][0]["packetsSent"], 900);
        near(&guest["metrics"][0]["encodeLatency"], 4.25);
        near(&guest["metrics"][0]["decodeLatency"], 1.75);
        near(&guest["metrics"][0]["bitrate"], 18.5);
        assert_eq!(guest["audio"]["packetsSent"], 120);
        near(&guest["audio"]["encodeLatency"], 0.05);
        assert_eq!(guest["control"]["packetsSent"], 40);
        assert_eq!(guest["control"]["slowRTs"], 1);

        // The round trip is the session's, so every block that describes a
        // live channel repeats it.
        near(&guest["audio"]["networkLatency"], 12.5);
        near(&guest["control"]["networkLatency"], 12.5);
        near(&guest["metrics"][0]["networkLatency"], 12.5);

        // Congestion is video's alone: nothing else is rate controlled, so a
        // count against it would be a number with nothing behind it.
        assert_eq!(guest["metrics"][0]["cgEvents"], 7);
        assert_eq!(guest["audio"]["cgEvents"], 0);
        assert_eq!(guest["control"]["cgEvents"], 0);
    }

    /// **A number the reader cannot parse costs the whole roster, not one
    /// field.** Its parser requires seven of the eight keys to be JSON
    /// numbers and abandons the entire guest list -- every guest, not just the
    /// bad block -- when one is not. A non-finite float serialises as `null`,
    /// which is a token and not a number, so one NaN anywhere would delete the
    /// room from the reader's view and take the panels the roster gates with
    /// it. Nothing upstream produces one today; this is here so that nothing
    /// upstream ever can.
    #[test]
    fn a_figure_that_is_not_a_number_never_reaches_the_body() {
        let mut guest = one_guest();
        guest.metrics.network_ms = f32::NAN;
        guest.metrics.video.bitrate_mbps = f32::INFINITY;
        guest.metrics.audio.encode_ms = f32::NEG_INFINITY;
        guest.metrics.control.decode_ms = f32::NAN;

        let body: serde_json::Value = serde_json::from_str(&roster(&[guest])).expect("valid JSON");
        let g = &body[0];
        let mut checked = 0;
        for block in [&g["audio"], &g["control"]]
            .into_iter()
            .chain(g["metrics"].as_array().expect("an array"))
        {
            for (key, value) in block.as_object().expect("an object") {
                assert!(value.is_number(), "{key} is {value}, which is not a number");
                checked += 1;
            }
        }
        assert_eq!(
            checked, 40,
            "five blocks of eight keys were not all checked"
        );
    }

    /// **Three entries whether or not there are three streams.** The reader
    /// this shape is written for indexes the array and has no reason to expect
    /// a shorter one; a stream that never ran is every field zero, the round
    /// trip included, because it had no path of its own to measure one on.
    #[test]
    fn the_stream_array_stays_three_long_with_the_unused_entries_zeroed() {
        let body: serde_json::Value =
            serde_json::from_str(&roster(&[one_guest()])).expect("valid JSON");
        let streams = body[0]["metrics"].as_array().expect("an array");
        assert_eq!(streams.len(), 3);
        for stream in &streams[1..] {
            for key in [
                "packetsSent",
                "fastRTs",
                "slowRTs",
                "cgEvents",
                "encodeLatency",
                "decodeLatency",
                "networkLatency",
                "bitrate",
            ] {
                assert_eq!(stream[key].as_f64(), Some(0.0), "{key} on an unused stream");
            }
        }
    }

    fn video() -> Video {
        Video {
            output: "card0:DP-2".to_string(),
            bitrate_mbps: 10,
            fps: 60,
            width: 2560,
            height: 1440,
            rotated: false,
            full_fps: true,
            host_os: 0,
        }
    }

    fn listed() -> Vec<Selectable> {
        vec![Selectable {
            id: "card0:DP-2".to_string(),
            connector: "DP-2".to_string(),
            width: 2560,
            height: 1440,
            refresh_hz: 60,
            place: None,
        }]
    }

    fn settings() -> Settings {
        Settings {
            accept_microphone: false,
            guest_clipboard: Clipboard::Off,
            output: "card0:DP-2".to_string(),
            bitrate_mbps: 10,
            fps: 60,
            full_fps: true,
            host_os: 0,
            fake_output: false,
        }
    }

    /// **A panel told the startup value shows a change that never happened.**
    /// The rate, the frame rate and the repeated-picture permission are live,
    /// so a guest may have moved one a moment ago; describing from the
    /// settings answers the question that was asked at boot.
    #[test]
    fn a_stream_is_described_by_what_it_runs_at_and_not_by_what_it_started_at() {
        let started = settings();
        let running = lowlat::stream::LiveVideo {
            fps: 120,
            bitrate_mbps: 7.0,
            min_mbps: 1.0,
            full_fps: true,
        };
        assert_ne!(started.fps, running.fps, "the test needs the two to differ");
        let described = describe(
            None,
            &listed(),
            Some("card0:DP-2"),
            0,
            lowlat::video::Rotation::None,
            &started,
            Some(running),
        );
        assert_eq!(described.fps, 120, "the frame rate came from the settings");
        assert_eq!(described.bitrate_mbps, 7, "the rate came from the settings");
        assert!(described.full_fps, "the permission came from the settings");

        // With nothing streaming there is nothing to read, and the settings
        // are the only answer there is.
        let early = describe(
            None,
            &listed(),
            Some("card0:DP-2"),
            0,
            lowlat::video::Rotation::None,
            &started,
            None,
        );
        assert_eq!(early.fps, started.fps);
        assert_eq!(early.bitrate_mbps, started.bitrate_mbps);
    }

    /// **A stream is described by what it produces, not by what was asked
    /// for.** A display decides its own size and a host follows it, so a
    /// description built from configuration reports a stream nobody is making
    /// -- which once told a peer its pointer was in a 1920x1080 space while the
    /// picture was 2560x1440, and every position landed short by the ratio.
    #[test]
    fn a_stream_is_described_by_the_picture_and_never_by_the_request() {
        // The picture wins whenever there is one, even against the display it
        // came from: a display that changed size mid-session is a picture the
        // encoder is still producing at the old one.
        let live = describe(
            Some((3840, 2160)),
            &listed(),
            Some("card0:DP-2"),
            0,
            lowlat::video::Rotation::None,
            &settings(),
            None,
        );
        assert_eq!((live.width, live.height), (3840, 2160));

        // And before a display has been opened, the output's own size is what
        // the stream is about to produce. What is never consulted is the
        // configuration, which carries no size at all.
        let early = describe(
            None,
            &listed(),
            Some("card0:DP-2"),
            0,
            lowlat::video::Rotation::None,
            &settings(),
            None,
        );
        assert_eq!((early.width, early.height), (2560, 1440));
    }

    /// **What a chooser marks has to be what the stream carries.** They were
    /// derived separately once: this reported whichever output enumerated
    /// first while the host captured the one at the desktop's corner, so the
    /// check sat on the wrong screen and picking that screen changed nothing,
    /// because the host already believed it was there.
    #[test]
    fn the_output_reported_is_the_one_the_host_would_capture() {
        let asked = Settings {
            accept_microphone: false,
            guest_clipboard: Clipboard::Off,
            output: String::new(),
            ..settings()
        };
        let listed = vec![
            Selectable {
                id: "card0:HDMI-A-1".to_string(),
                connector: "HDMI-A-1".to_string(),
                width: 2560,
                height: 1440,
                refresh_hz: 60,
                place: None,
            },
            Selectable {
                id: "card1:DP-4".to_string(),
                connector: "DP-4".to_string(),
                width: 2560,
                height: 1440,
                refresh_hz: 60,
                place: None,
            },
        ];
        // The host would take the second; the first is merely first.
        let described = describe(
            None,
            &listed,
            Some("card1:DP-4"),
            0,
            lowlat::video::Rotation::None,
            &asked,
            None,
        );
        assert_eq!(
            described.output, "card1:DP-4",
            "the enumeration order was reported instead of the choice"
        );

        // An explicit request still wins over both.
        let told = Settings {
            accept_microphone: false,
            guest_clipboard: Clipboard::Off,
            output: "card0:HDMI-A-1".to_string(),
            ..settings()
        };
        assert_eq!(
            describe(
                None,
                &listed,
                Some("card1:DP-4"),
                0,
                lowlat::video::Rotation::None,
                &told,
                None
            )
            .output,
            "card0:HDMI-A-1"
        );
    }

    /// **The output is never empty while one is being captured.** A client
    /// shown a stream with no output has nothing to name and nothing to switch
    /// away from, and it shows nothing at all -- which is what a live run
    /// against a stock client did, in silence.
    #[test]
    fn an_output_is_named_even_when_none_was_asked_for() {
        let asked = Settings {
            accept_microphone: false,
            guest_clipboard: Clipboard::Off,
            output: String::new(),
            ..settings()
        };
        assert_eq!(
            describe(
                None,
                &listed(),
                Some("card0:DP-2"),
                0,
                lowlat::video::Rotation::None,
                &asked,
                None
            )
            .output,
            "card0:DP-2"
        );
        assert_eq!(
            describe(
                None,
                &listed(),
                Some("card0:DP-2"),
                0,
                lowlat::video::Rotation::None,
                &settings(),
                None
            )
            .output,
            "card0:DP-2"
        );

        // Nothing lit is the one case where there is honestly nothing to name.
        assert_eq!(
            describe(
                None,
                &[],
                None,
                0,
                lowlat::video::Rotation::None,
                &asked,
                None
            )
            .output,
            ""
        );
    }

    /// **The shape is the client's, not ours.** It reads named fields and
    /// refuses the whole element when one it requires is missing, falling back
    /// to a configuration nobody asked for -- so a renamed or dropped field is
    /// a silent revert rather than an error.
    #[test]
    fn the_configuration_carries_every_field_the_client_requires() {
        let parsed: serde_json::Value =
            serde_json::from_str(&config(&video(), true)).expect("json");
        assert!(parsed.get("virtualTablet").is_some());
        // **Not merely present: it has to say yes when the host takes one.**
        // A client reads this to know the feature exists at all, so a host
        // that publishes zero here has a client that never offers it, however
        // willing the rest of the host is.
        assert_eq!(
            parsed.get("virtualMicrophone").and_then(|v| v.as_u64()),
            Some(1)
        );
        let without =
            serde_json::from_str::<serde_json::Value>(&config(&video(), false)).expect("json");
        assert_eq!(
            without.get("virtualMicrophone").and_then(|v| v.as_u64()),
            Some(0)
        );
        let streams = parsed
            .get("video")
            .and_then(serde_json::Value::as_array)
            .expect("an array of streams");
        assert_eq!(streams.len(), STREAMS);
        let first = streams.first().expect("one stream");
        for field in [
            "output",
            "encoderMaxBitrate",
            "encoderFPS",
            "resolutionX",
            "resolutionY",
            "rotated",
            "fullFPS",
            "hostOS",
        ] {
            assert!(first.get(field).is_some(), "missing {field}");
        }
        assert_eq!(
            first.get("hostOS").and_then(serde_json::Value::as_u64),
            Some(0),
            "the default must stay zero, whatever a run was told to try"
        );
        assert_eq!(
            first.get("rotated").and_then(serde_json::Value::as_bool),
            Some(false),
            "the two flags are booleans and the client reads them as booleans"
        );
    }

    /// **Every stream described is one that exists.** A reader keeps its own
    /// defaults for the elements an array does not hold, so padding it out
    /// describes streams nobody is producing and invites a request to
    /// configure one of them.
    #[test]
    fn only_streams_that_exist_are_described() {
        let parsed: serde_json::Value =
            serde_json::from_str(&config(&video(), true)).expect("json");
        let streams = parsed
            .get("video")
            .and_then(serde_json::Value::as_array)
            .expect("streams");
        assert_eq!(streams.len(), 1);
        for stream in streams {
            let output = stream
                .get("output")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            assert!(!output.is_empty(), "a stream that exists names its output");
        }
    }

    /// **The probe output is offered and never captured.** It exists to make
    /// the number of outputs testable on a machine that has one, and a guest
    /// that picks it must cost nothing: a name nothing is lighting is refused
    /// where the display is opened, and that refusal is the display failing to
    /// open at all, which ends every guest on the stream.
    #[test]
    fn the_probe_output_is_offered_only_when_asked_for() {
        let plain: serde_json::Value = serde_json::from_str(&outputs(false)).expect("json");
        assert!(
            !plain.to_string().contains(FAKE_OUTPUT),
            "a machine offered something that is not there"
        );

        let probed: serde_json::Value = serde_json::from_str(&outputs(true)).expect("json");
        let entries = probed.as_array().expect("an array").len();
        assert_eq!(
            entries,
            plain.as_array().expect("an array").len() + 1,
            "the probe must add exactly one"
        );
        assert!(probed.to_string().contains(FAKE_OUTPUT));
    }

    /// **An identity must not carry anything that changes.** It is stored by
    /// the far side and handed back later, so a size baked into it stops
    /// matching the moment the display changes mode.
    #[test]
    fn an_output_identity_is_not_its_label() {
        let listed: serde_json::Value = serde_json::from_str(&outputs(false)).expect("json");
        assert!(listed.is_array(), "the client reads this as an array");
        for output in listed.as_array().unwrap_or(&Vec::new()) {
            let id = output
                .get("id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            assert!(!id.contains('x'), "an identity carrying a size: {id}");
            assert!(output.get("name").is_some());
            assert!(output.get("adapterName").is_some());
        }
    }

    /// **A typo cannot open a clipboard.** Absent, empty, misspelled or a
    /// value from a newer version all mean off, so a configuration this build
    /// does not understand fails closed.
    #[test]
    fn an_unrecognised_clipboard_setting_is_off() {
        assert_eq!(Clipboard::named(Some("send")), Clipboard::Send);
        assert_eq!(Clipboard::named(Some("both")), Clipboard::Both);
        for named in [
            None,
            Some(""),
            Some("off"),
            Some("Send"),
            Some("recv"),
            Some("all"),
        ] {
            assert_eq!(Clipboard::named(named), Clipboard::Off, "{named:?}");
        }
        assert_eq!(Clipboard::default(), Clipboard::Off);
    }

    /// **The milder direction is available on its own and the dangerous one is
    /// not**, and an owner is `both` whatever the setting says.
    #[test]
    fn the_setting_names_what_a_guest_may_do_and_an_owner_is_not_a_guest() {
        let guest = false;
        assert!(!Clipboard::Off.takes_from(guest));
        assert!(!Clipboard::Off.gives_to(guest));
        // Sending is a guest's text arriving here, where a person still has to
        // choose to paste it.
        assert!(Clipboard::Send.takes_from(guest));
        assert!(!Clipboard::Send.gives_to(guest));
        assert!(Clipboard::Both.takes_from(guest));
        assert!(Clipboard::Both.gives_to(guest));

        let owner = true;
        for setting in [Clipboard::Off, Clipboard::Send, Clipboard::Both] {
            assert!(setting.takes_from(owner), "{setting:?}");
            assert!(setting.gives_to(owner), "{setting:?}");
        }
    }

    /// **A guest cannot ask for more frames than the display presents.** The
    /// loop would not have reached the rate anyway, but the number is also
    /// what the encoder's per-frame budget is divided by, so honouring the
    /// request as asked spends a fraction of each frame for no more frames.
    #[test]
    fn a_rate_a_guest_asks_for_is_clamped_to_the_display() {
        let display = refresh_of(&listed(), "card0:DP-2");
        assert_eq!(display, 60, "the fixture is what makes the clamp visible");
        let asked = |json: &str| {
            let parsed: serde_json::Value = serde_json::from_str(json).expect("json");
            asked_fps(&parsed, 90, display)
        };

        assert_eq!(asked(r#"{"encoderFPS":120}"#), 60, "asked past the display");
        assert_eq!(asked(r#"{"encoderFPS":30}"#), 30, "asked under it");
        assert_eq!(asked(r#"{"encoderFPS":60}"#), 60, "asked for exactly it");

        // **Zero and absent are no change, not a request to follow.** A panel
        // that leaves the field alone sends one of the two, and acting on it
        // would change something nobody touched.
        assert_eq!(asked(r#"{"encoderFPS":0}"#), 90);
        assert_eq!(asked(r#"{}"#), 90);
        assert_eq!(asked(r#"{"encoderFPS":"fast"}"#), 90, "not a number");
        assert_eq!(asked(r#"{"encoderFPS":4294967296}"#), 90, "past a u32");

        // A display that will not say leaves the request standing.
        let parsed: serde_json::Value =
            serde_json::from_str(r#"{"encoderFPS":144}"#).expect("json");
        assert_eq!(asked_fps(&parsed, 90, 0), 144);
    }

    /// A body for a language this host does not speak is reported as
    /// unhandled rather than swallowed.
    #[test]
    fn an_unknown_sub_identifier_is_not_claimed() {
        let mut seam = super::Admission::new(lowlat::admission::Config {
            microphone: None,
            exclusive_pointer: false,
            rumble_probe: false,
            exclusive_hold_ms: lowlat::floor::HOLD_MS,
            cg_level: 1,
            base_port: 0,
            shared_address_space: false,
            max_guests: 1,
            servers: Vec::new(),
            stream: None,
        });
        assert!(!on_message(&mut seam, 1, 0, b"Hello host", &settings()));
        assert!(!on_message(
            &mut seam,
            1,
            99,
            b"from a newer client",
            &settings()
        ));
        assert!(on_message(&mut seam, 1, 9, b"", &settings()));
        // **Claimed even with the clipboard off**, because refusing is this
        // host's answer rather than a message it did not understand.
        assert!(on_message(&mut seam, 1, 7, b"copied", &settings()));
        // **Claimed with no guest of that number to ask.** The identifier is
        // one this host speaks; whether there was anybody to type it for is a
        // different question and not what this answer means.
        assert!(on_message(&mut seam, 1, 14, b"", &settings()));
    }
}
