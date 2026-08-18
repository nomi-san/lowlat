//! Session initialization, and the two messages a stream owes its peer.
//!
//! A guest is not streamable the moment connectivity completes. It has to say
//! what it can decode first, and that arrives on the media path rather than in
//! signalling: opcode 11 within five seconds of the session existing, or the
//! attempt is abandoned. See docs/01-protocol.md sections 11.5 and 12.
//!
//! **The deadline is why this is a state machine rather than a parse.** A peer
//! that never initializes is the ordinary shape of a peer that connected and
//! then died, and nothing else in the protocol reports it.

use lowlat_core::control::{Control, op};
use lowlat_core::init::{self, Init};

/// How long a guest has to initialize before the attempt is abandoned.
pub const INIT_DEADLINE_MS: f64 = 5000.0;

/// Encode latency goes out every thirtieth frame, which is half a second at
/// sixty.
pub const LATENCY_INTERVAL_FRAMES: u64 = 30;

/// The smoothing the latency figure carries. `latency = 0.9 * latency + 0.1 *
/// sample`, so a single slow frame moves the reported figure by a tenth of its
/// excess rather than all of it.
const LATENCY_WEIGHT: f64 = 0.1;

/// What a stream is waiting for, or what it settled on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Connectivity is up and nothing has been declared yet.
    AwaitingInit,
    /// The guest declared itself and can be streamed to.
    Ready,
    /// The deadline passed. **Not a timeout to retry**: the guest is gone or
    /// is not speaking this protocol, and either way there is nothing to wait
    /// longer for.
    Abandoned,
}

/// One guest's negotiation and the cadences that follow it.
#[derive(Debug)]
pub struct Negotiation {
    state: State,
    opened_ms: f64,
    /// What the guest asked for. The host is authoritative over all of it, so
    /// this is a request rather than a setting.
    asked: Option<Init>,
    /// What this guest can decode.
    ///
    /// **It arrives in two places and both count** (docs/01-protocol.md
    /// section 11.5): the initialization declares it, and every
    /// encoder-configuration message restates it. The later one wins, because
    /// the second is how a peer changes its mind mid-session and a host that
    /// kept the first would never hear the change.
    ///
    /// A peer may send only the first. Requiring both would leave every such
    /// peer declaring nothing at all.
    flags: u32,
    /// Set by an encoder-configuration message asking for reinitialization,
    /// and taken once by the encode loop.
    reconfigure: bool,
    /// Opcodes seen from this peer, one bit each, so each is logged once.
    ///
    /// **What a peer actually sends is worth knowing exactly once.** Logging
    /// every message would bury the session under pointer motion; logging none
    /// leaves the vocabulary a peer speaks unrecorded.
    seen: u64,
    frames: u64,
    latency_ms: f64,
    /// The generation to announce, set when the encoder is initialized and
    /// cleared by the frame that announces it.
    announce: Option<u32>,
}

impl Negotiation {
    /// Begin waiting, from the moment the media path exists.
    ///
    /// **Time is a parameter here as it is in the core**, so the deadline is
    /// testable without a clock and without a test that sleeps for five
    /// seconds to find out whether it works.
    pub fn opened(now_ms: f64) -> Self {
        Self {
            state: State::AwaitingInit,
            opened_ms: now_ms,
            asked: None,
            flags: 0,
            reconfigure: false,
            seen: 0,
            frames: 0,
            latency_ms: 0.0,
            announce: None,
        }
    }

    pub fn state(&self) -> State {
        self.state
    }

    pub fn asked(&self) -> Option<Init> {
        self.asked
    }

    pub fn flags(&self) -> u32 {
        self.flags
    }

    /// True once the guest may be sent video.
    pub fn ready(&self) -> bool {
        self.state == State::Ready
    }

    /// Abandon the attempt if the deadline has passed with nothing declared.
    ///
    /// Called from the loop that already has a clock, so this reads none.
    pub fn tick(&mut self, now_ms: f64) -> State {
        if self.state == State::AwaitingInit && now_ms - self.opened_ms >= INIT_DEADLINE_MS {
            self.state = State::Abandoned;
        }
        self.state
    }

    /// Take a control message from this guest.
    ///
    /// Returns true when the message was one this cares about, so a caller can
    /// tell "handled" from "pass it on" without inspecting the opcode twice.
    pub fn on_control(&mut self, message: &Control<'_>) -> bool {
        self.note(message);
        match message.opcode {
            op::INIT => {
                // **A second initialization is not a reset.** A peer that
                // sends one after settling is either confused or replaying;
                // either way the stream it already negotiated stands.
                if self.state != State::AwaitingInit {
                    return true;
                }
                match init::parse(message.body) {
                    Ok(asked) => {
                        self.flags = asked.flags;
                        self.asked = Some(asked);
                        self.state = State::Ready;
                    }
                    // A body we cannot read is the same position as no body at
                    // all: still waiting, and still on the clock.
                    Err(_) => return true,
                }
                true
            }
            op::ENCODER_CONFIG => {
                // Argument 0 is the stream, 1 the flags, 2 a reinitialization
                // request. One stream in v1, so the index is read and not yet
                // acted on.
                //
                // **Always logged, unlike everything else here.** It is the
                // only message a peer uses to change its mind about what it
                // can decode, it is rare, and a stream that reconfigures is
                // read backwards from this line.
                lowlat_common::log_info!(
                    "guest: encoder config stream={} flags={:#x} reinit={}",
                    message.a0,
                    message.a1,
                    u8::from(message.a2 != 0)
                );
                self.flags = message.a1;
                if message.a2 != 0 {
                    self.reconfigure = true;
                }
                true
            }
            _ => false,
        }
    }

    /// Log an opcode the first time this peer sends one.
    fn note(&mut self, message: &Control<'_>) {
        let bit = 1u64 << u32::from(message.opcode).min(63);
        if self.seen & bit != 0 {
            return;
        }
        self.seen |= bit;
        lowlat_common::log_info!(
            "guest: first op={} ({}) a0={} a1={} a2={} body={}",
            message.opcode,
            op::name(message.opcode),
            message.a0,
            message.a1,
            message.a2,
            message.body.len()
        );
    }

    /// Whether the peer asked for the encoder to be reinitialized, clearing
    /// the request.
    ///
    /// **Taken rather than read**, so two callers cannot both act on one
    /// request and two go out for one ask.
    pub fn take_reconfigure(&mut self) -> bool {
        core::mem::replace(&mut self.reconfigure, false)
    }

    /// Note that the encoder was initialized at this generation, so the next
    /// frame announces it.
    pub fn encoder_initialised(&mut self, generation: u32) {
        self.announce = Some(generation);
    }

    /// Fold one frame's capture-to-collected time in, and report what to send.
    ///
    /// Called once per frame after the picture is collected. The generation
    /// announcement goes out on the frame **following** an initialization,
    /// which is how a peer learns the reference chain restarted rather than
    /// inferring it from the stream.
    pub fn on_frame(&mut self, sample_ms: f64) -> Reports {
        self.frames += 1;
        self.latency_ms = if self.frames == 1 {
            sample_ms
        } else {
            (1.0 - LATENCY_WEIGHT) * self.latency_ms + LATENCY_WEIGHT * sample_ms
        };

        Reports {
            latency_us: (self.frames % LATENCY_INTERVAL_FRAMES == 0)
                .then(|| microseconds(self.latency_ms)),
            generation: self.announce.take(),
        }
    }

    /// The smoothed capture-to-collected time, in milliseconds.
    pub fn latency_ms(&self) -> f64 {
        self.latency_ms
    }
}

/// Milliseconds to whole microseconds, saturating rather than wrapping.
///
/// A cast would wrap a figure that should be impossible, and an impossible
/// latency reported as a small one is worse than one reported as the ceiling.
fn microseconds(ms: f64) -> u32 {
    let us = ms * 1000.0;
    if !us.is_finite() || us <= 0.0 {
        0
    } else if us >= f64::from(u32::MAX) {
        u32::MAX
    } else {
        // The bounds above put this inside the range, so nothing is lost.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        {
            us as u32
        }
    }
}

/// What a frame owes the peer, beyond the picture itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reports {
    /// Encode latency, in microseconds, on the frames that carry it.
    ///
    /// **Its first argument is 1, not 0**, which is what the message writer
    /// has to remember.
    pub latency_us: Option<u32>,
    /// An encoder generation to announce, once, after an initialization.
    pub generation: Option<u32>,
}

impl Reports {
    /// The encode-latency message, if this frame carries one.
    pub fn latency_message(&self, stream: u32) -> Option<Control<'static>> {
        self.latency_us.map(|us| Control {
            a0: 1,
            a1: us,
            a2: stream,
            opcode: op::ENCODE_LATENCY,
            body: &[],
        })
    }

    /// The generation announcement, if this frame carries one.
    pub fn generation_message(&self, stream: u32) -> Option<Control<'static>> {
        self.generation.map(|generation| Control {
            a0: stream,
            a1: generation,
            a2: 0,
            opcode: op::ENCODER_GENERATION,
            body: &[],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The recorded body, so the state machine is driven by what a peer sends.
    const RECORDED: &[u8] = b"{\"_version\":1,\"_max_w\":60000,\"_max_h\":60000,\"_flags\":8,\
\"resolutionX\":0,\"resolutionY\":0,\"mediaContainer\":0,\"refreshRate\":60}\0";

    fn control(opcode: u8, a0: u32, a1: u32, a2: u32, body: &[u8]) -> Control<'_> {
        Control {
            a0,
            a1,
            a2,
            opcode,
            body,
        }
    }

    #[test]
    fn a_recorded_initialisation_makes_a_guest_streamable() {
        let mut guest = Negotiation::opened(0.0);
        assert_eq!(guest.state(), State::AwaitingInit);
        assert!(!guest.ready());

        assert!(guest.on_control(&control(op::INIT, 124, 0, 0, RECORDED)));
        assert_eq!(guest.state(), State::Ready);
        assert_eq!(guest.flags(), lowlat_core::init::FLAG_BASE);
        assert_eq!(guest.asked().expect("asked").refresh_rate, 60);
    }

    /// **A body we cannot read leaves the guest on the clock**, rather than
    /// admitting it or abandoning it early. It is the same position as one
    /// that has not spoken.
    #[test]
    fn an_unreadable_body_neither_admits_nor_abandons() {
        let mut guest = Negotiation::opened(0.0);
        guest.on_control(&control(op::INIT, 4, 0, 0, b"{}\0"));
        assert_eq!(guest.state(), State::AwaitingInit);
        assert!(guest.asked().is_none());
    }

    /// A second initialization does not reset a settled stream.
    #[test]
    fn a_late_initialisation_does_not_renegotiate() {
        let mut guest = Negotiation::opened(0.0);
        guest.on_control(&control(op::INIT, 124, 0, 0, RECORDED));
        guest.on_control(&control(
            op::INIT,
            20,
            0,
            0,
            b"{\"_version\":1,\"_flags\":1}\0",
        ));
        assert_eq!(
            guest.flags(),
            lowlat_core::init::FLAG_BASE,
            "a late initialisation changed a settled stream"
        );
    }

    /// **The declaration arrives in two places and a peer may send only the
    /// first.** Requiring both would leave every such peer declaring nothing,
    /// which is the whole capability read backwards.
    #[test]
    fn the_initialization_alone_is_a_complete_declaration() {
        let mut guest = Negotiation::opened(0.0);
        guest.on_control(&control(
            op::INIT,
            30,
            0,
            0,
            b"{\"_version\":1,\"_flags\":9}\0",
        ));
        assert!(
            guest.ready(),
            "the guest is not streamable on the first place"
        );
        assert_eq!(
            guest.flags(),
            lowlat_core::init::FLAG_BASE | lowlat_core::init::FLAG_HEVC,
            "the first place did not declare on its own"
        );
        assert!(
            !guest.take_reconfigure(),
            "an initialization asked for a reinitialization"
        );
    }

    #[test]
    fn the_encoder_configuration_updates_flags_and_can_ask_for_a_reinitialization() {
        let mut guest = Negotiation::opened(0.0);
        guest.on_control(&control(op::INIT, 124, 0, 0, RECORDED));

        assert!(!guest.take_reconfigure());
        assert!(guest.on_control(&control(op::ENCODER_CONFIG, 0, 0x09, 1, &[])));
        assert_eq!(guest.flags(), 0x09, "the later flags did not win");
        assert!(guest.take_reconfigure(), "the refresh request was lost");
        assert!(
            !guest.take_reconfigure(),
            "one request produced two refreshes"
        );
    }

    #[test]
    fn a_message_this_does_not_own_is_passed_on() {
        let mut guest = Negotiation::opened(0.0);
        assert!(!guest.on_control(&control(op::KEYBOARD, 1, 2, 3, &[])));
    }

    /// The deadline is not a retry. A guest that says nothing is gone.
    /// The deadline is not a retry. A guest that says nothing is gone, and
    /// the whole point is what happens once it passes.
    #[test]
    fn a_guest_that_never_initialises_is_abandoned_at_the_deadline() {
        let mut guest = Negotiation::opened(0.0);
        assert_eq!(guest.tick(0.0), State::AwaitingInit);
        assert_eq!(
            guest.tick(INIT_DEADLINE_MS - 1.0),
            State::AwaitingInit,
            "abandoned before the deadline"
        );
        assert_eq!(guest.tick(INIT_DEADLINE_MS), State::Abandoned);
        // And it stays abandoned rather than recovering on a later tick.
        assert_eq!(guest.tick(INIT_DEADLINE_MS + 10_000.0), State::Abandoned);
    }

    /// A guest that did initialise is never abandoned, however long it runs.
    #[test]
    fn a_settled_guest_outlives_the_deadline() {
        let mut guest = Negotiation::opened(0.0);
        guest.on_control(&control(op::INIT, 124, 0, 0, RECORDED));
        assert_eq!(guest.state(), State::Ready);
        assert_eq!(guest.tick(INIT_DEADLINE_MS * 100.0), State::Ready);
    }

    /// An initialisation that lands after the deadline has passed does not
    /// resurrect the attempt.
    #[test]
    fn an_initialisation_after_the_deadline_does_not_resurrect() {
        let mut guest = Negotiation::opened(0.0);
        assert_eq!(guest.tick(INIT_DEADLINE_MS), State::Abandoned);
        guest.on_control(&control(op::INIT, 124, 0, 0, RECORDED));
        assert_eq!(
            guest.state(),
            State::Abandoned,
            "a late initialisation revived an abandoned attempt"
        );
    }

    /// Every thirtieth frame, and the first argument is one.
    #[test]
    fn encode_latency_goes_out_on_a_cadence_and_carries_its_marker() {
        let mut guest = Negotiation::opened(0.0);
        for frame in 1..=90u64 {
            let reports = guest.on_frame(4.0);
            let due = frame % LATENCY_INTERVAL_FRAMES == 0;
            assert_eq!(reports.latency_us.is_some(), due, "at frame {frame}");
            if due {
                let message = reports.latency_message(0).expect("a message");
                assert_eq!(message.a0, 1, "the first argument is not one");
                assert_eq!(message.opcode, op::ENCODE_LATENCY);
                assert_eq!(message.a1, 4000, "four milliseconds is not 4000 us");
            }
        }
    }

    /// The smoothing moves a tenth of the way, so one slow frame is not a
    /// spike in what the peer is told.
    #[test]
    fn the_latency_is_smoothed_rather_than_reported_raw() {
        let mut guest = Negotiation::opened(0.0);
        guest.on_frame(10.0);
        assert!(
            (guest.latency_ms() - 10.0).abs() < 1e-9,
            "the first is the sample"
        );
        guest.on_frame(20.0);
        assert!(
            (guest.latency_ms() - 11.0).abs() < 1e-9,
            "expected a tenth of the way, got {}",
            guest.latency_ms()
        );
    }

    /// The announcement rides the frame after the initialization, once.
    #[test]
    fn a_generation_is_announced_once_on_the_following_frame() {
        let mut guest = Negotiation::opened(0.0);
        assert!(guest.on_frame(3.0).generation.is_none());

        guest.encoder_initialised(2);
        let reports = guest.on_frame(3.0);
        let message = reports.generation_message(0).expect("an announcement");
        assert_eq!(message.opcode, op::ENCODER_GENERATION);
        assert_eq!(message.a0, 0, "argument zero is the stream");
        assert_eq!(message.a1, 2, "argument one is the generation");
        assert_eq!(message.a2, 0);

        assert!(
            guest.on_frame(3.0).generation.is_none(),
            "the announcement repeated"
        );
    }
}
