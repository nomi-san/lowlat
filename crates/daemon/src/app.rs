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
    pub(crate) rotated: bool,
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
    settings: &Settings,
) -> Video {
    // **What is being captured beats what was asked for.** A guest can switch
    // outputs and a display can move to another card by itself, and a reader
    // told the request rather than the result marks the wrong screen -- then
    // picking the right one changes nothing, because the host already believes
    // it is there.
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
    Video {
        output,
        bitrate_mbps: settings.bitrate_mbps,
        fps: settings.fps,
        width,
        height,
        rotated: settings.rotated,
        full_fps: settings.full_fps,
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
                settings,
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
            let described = describe(
                seam.picture(),
                &Display::outputs(),
                Display::preferred().as_deref(),
                seam.captured(),
                settings,
            );
            apply(seam, body, &described);
            // **Not answered.** The client asks again with 9 the moment it has
            // sent one of these, so an answer here would arrive beside the one
            // it is about to ask for.
            true
        }
        _ => false,
    }
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
        settings,
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
    let guests: Vec<serde_json::Value> = seam
        .guests()
        .into_iter()
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
                // **Reported as nothing rather than omitted.** What these
                // carry is per-guest telemetry this host does not publish yet;
                // leaving the fields out risks the whole roster being refused.
                "audio": metrics(),
                "control": metrics(),
                "metrics": [metrics(), metrics(), metrics()],
            })
        })
        .collect();
    let body = serde_json::Value::Array(guests).to_string();
    let reached = seam.send_roster(body.as_bytes());
    lowlat_common::log_info!("lowlatd: told {reached} guest(s) the roster: {body}");
}

/// One block of per-guest telemetry, all of it zero.
fn metrics() -> serde_json::Value {
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

/// Take what a client asked for, and act on the part of it that is ours.
fn apply(seam: &mut Admission, body: &[u8], video: &Video) {
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
            lowlat_common::log_info!("lowlatd: guest asked for whichever output this host picks");
            seam.select_output(None);
        }
        AUTO => {}
        chosen if chosen == video.output => {}
        // **Checked against what is really there before it is forwarded.** A
        // name nothing is lighting is refused where the display is opened, and
        // that refusal is the display failing to open at all -- which ends every
        // guest on the stream, including the one that asked. A guest naming
        // something that is not there must cost nothing.
        chosen if Display::outputs().iter().any(|real| real.id == chosen) => {
            lowlat_common::log_info!("lowlatd: guest asked to capture {chosen}");
            seam.select_output(Some(chosen.to_string()));
        }
        chosen => {
            lowlat_common::log_info!(
                "lowlatd: guest asked to capture {chosen}, which nothing here is lighting"
            );
        }
    }

    // **Said rather than done.** Both change the display's own mode, which
    // belongs to whoever owns the display, and on a display this host did not
    // create that is the session (docs/impl-plan.md, output selection). A
    // request that is quietly dropped looks like a host that ignored its
    // guest, so it is reported.
    for (field, current) in [
        ("resolutionX", video.width),
        ("resolutionY", video.height),
        ("encoderFPS", video.fps),
    ] {
        if let Some(asked) = first.get(field).and_then(serde_json::Value::as_u64)
            && asked != 0
            && asked != u64::from(current)
        {
            lowlat_common::log_info!(
                "lowlatd: guest asked for {field}={asked}, which this host cannot set yet"
            );
        }
    }
    if let Some(asked) = first
        .get("encoderMaxBitrate")
        .and_then(serde_json::Value::as_u64)
        && asked != 0
        && asked != u64::from(video.bitrate_mbps)
    {
        lowlat_common::log_info!(
            "lowlatd: guest asked for {asked} Mbps, which needs a live reconfigure"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            place: None,
        }]
    }

    fn settings() -> Settings {
        Settings {
            accept_microphone: false,
            output: "card0:DP-2".to_string(),
            bitrate_mbps: 10,
            fps: 60,
            rotated: false,
            full_fps: true,
            host_os: 0,
            fake_output: false,
        }
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
            &settings(),
        );
        assert_eq!((live.width, live.height), (3840, 2160));

        // And before a display has been opened, the output's own size is what
        // the stream is about to produce. What is never consulted is the
        // configuration, which carries no size at all.
        let early = describe(None, &listed(), Some("card0:DP-2"), 0, &settings());
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
            output: String::new(),
            ..settings()
        };
        let listed = vec![
            Selectable {
                id: "card0:HDMI-A-1".to_string(),
                connector: "HDMI-A-1".to_string(),
                width: 2560,
                height: 1440,
                place: None,
            },
            Selectable {
                id: "card1:DP-4".to_string(),
                connector: "DP-4".to_string(),
                width: 2560,
                height: 1440,
                place: None,
            },
        ];
        // The host would take the second; the first is merely first.
        let described = describe(None, &listed, Some("card1:DP-4"), 0, &asked);
        assert_eq!(
            described.output, "card1:DP-4",
            "the enumeration order was reported instead of the choice"
        );

        // An explicit request still wins over both.
        let told = Settings {
            accept_microphone: false,
            output: "card0:HDMI-A-1".to_string(),
            ..settings()
        };
        assert_eq!(
            describe(None, &listed, Some("card1:DP-4"), 0, &told).output,
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
            output: String::new(),
            ..settings()
        };
        assert_eq!(
            describe(None, &listed(), Some("card0:DP-2"), 0, &asked).output,
            "card0:DP-2"
        );
        assert_eq!(
            describe(None, &listed(), Some("card0:DP-2"), 0, &settings()).output,
            "card0:DP-2"
        );

        // Nothing lit is the one case where there is honestly nothing to name.
        assert_eq!(describe(None, &[], None, 0, &asked).output, "");
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
        assert!(!on_message(&mut seam, 1, 7, b"clipboard", &settings()));
        assert!(on_message(&mut seam, 1, 9, b"", &settings()));
    }
}
