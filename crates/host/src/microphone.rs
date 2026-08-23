//! A guest's microphone, from the wire to the application.
//!
//! **Its own queue, not the event queue.** That one is bounded and drops the
//! oldest when nobody polls; a hundred packets a second of sound competing
//! with control events would evict exactly what must not be dropped
//! ([06 §13](../../../docs/06-api.md)).
//!
//! **The application receives samples, never a codec.** A guest picks how it
//! encodes and this decodes whichever it picked, so an application that wants
//! a microphone does not have to learn one -- and the codec that reads a
//! guest's bytes stays on this side of the boundary, where it is contained.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use lowlat_common::clock;
use lowlat_core::control::{self, Control};
use lowlat_core::microphone::{self, SAMPLES_MAX};

/// Packets held for an application that is not draining.
///
/// **Two hundred milliseconds of sound.** Long enough to ride out a scheduling
/// hiccup in whatever is consuming it, short enough that what is delivered
/// after a stall is recent: sound that arrives late is worth less than the
/// sound behind it, which is the same reason the send side never retransmits.
const MAX_PACKETS: usize = 20;

/// One packet, decoded.
#[derive(Debug)]
struct Packet {
    guest: u32,
    samples: [i16; SAMPLES_MAX],
    count: usize,
}

#[derive(Debug, Default)]
struct State {
    queued: VecDeque<Packet>,
    /// Dropped for want of room, reported with the next delivery.
    dropped: u32,
}

impl State {
    fn push(&mut self, packet: Packet) {
        // **The oldest goes, not the newest.** What just arrived is the sound
        // closest to now, and it is the one an application wants if it is only
        // going to get one.
        while self.queued.len() >= MAX_PACKETS {
            if self.queued.pop_front().is_some() {
                self.dropped = self.dropped.saturating_add(1);
            }
        }
        self.queued.push_back(packet);
    }
}

#[derive(Debug, Default)]
struct Shared {
    /// Bumped on every push, and the address a waiting consumer parks on.
    arrivals: AtomicU32,
    state: Mutex<State>,
}

impl Shared {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|held| held.into_inner())
    }
}

/// Where decoded sound is put. Cloned into every guest thread.
#[derive(Clone)]
pub struct Sender {
    shared: Arc<Shared>,
}

impl core::fmt::Debug for Sender {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("microphone::Sender").finish()
    }
}

impl Sender {
    fn send(&self, guest: u32, samples: &[i16]) {
        let mut packet = Packet {
            guest,
            samples: [0; SAMPLES_MAX],
            count: samples.len().min(SAMPLES_MAX),
        };
        let Some(target) = packet.samples.get_mut(..packet.count) else {
            return;
        };
        let Some(source) = samples.get(..packet.count) else {
            return;
        };
        target.copy_from_slice(source);
        self.shared.state().push(packet);
        self.shared.arrivals.fetch_add(1, Ordering::Release);
        lowlat_common::wait::notify_one(&self.shared.arrivals);
    }
}

/// Where it is taken from. **One consumer**, which is what lets the dropped
/// count be reported exactly once.
#[derive(Debug)]
pub struct Receiver {
    shared: Arc<Shared>,
}

/// What one take produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Taken {
    /// Nothing arrived before the timeout.
    Empty,
    /// Sound, already copied into the caller's buffer.
    Took {
        guest: u32,
        samples: usize,
        dropped: u32,
    },
}

impl Receiver {
    /// Take one packet into the caller's buffer, waiting up to `timeout`.
    ///
    /// The buffer must hold [`SAMPLES_MAX`]; a packet cannot be larger, so
    /// there is no partial delivery and nothing to call back for.
    pub fn recv_timeout_into(&self, timeout: Duration, out: &mut [i16]) -> Taken {
        let began = clock::Time::now();
        loop {
            // Sampled before the queue is found empty, so a push landing
            // between the two cannot be slept through.
            let seen = self.shared.arrivals.load(Ordering::Acquire);
            {
                let mut state = self.shared.state();
                if let Some(packet) = state.queued.pop_front() {
                    let count = packet.count.min(out.len());
                    if let (Some(target), Some(source)) =
                        (out.get_mut(..count), packet.samples.get(..count))
                    {
                        target.copy_from_slice(source);
                    }
                    return Taken::Took {
                        guest: packet.guest,
                        samples: count,
                        dropped: core::mem::take(&mut state.dropped),
                    };
                }
            }
            let waited = Duration::from_secs_f64(clock::elapsed_ms(began) / 1000.0);
            let Some(left) = timeout.checked_sub(waited) else {
                return Taken::Empty;
            };
            if left.is_zero() {
                return Taken::Empty;
            }
            lowlat_common::wait::wait(&self.shared.arrivals, seen, left);
        }
    }
}

/// One queue, as the two ends of it.
pub fn queue() -> (Sender, Receiver) {
    let shared = Arc::new(Shared::default());
    (
        Sender {
            shared: Arc::clone(&shared),
        },
        Receiver { shared },
    )
}

/// One guest's microphone, on the thread that receives it.
///
/// **One decoder per guest, because a codec carries state between packets.**
/// Feeding two guests' packets to one produces sound that is neither guest's.
pub(crate) struct Ear {
    guest: u32,
    decoder: lowlat_audio::Decoder,
    out: Sender,
    samples: Box<[i16; SAMPLES_MAX]>,
    /// Packets taken, for the line a live run is read from.
    taken: u64,
}

impl core::fmt::Debug for Ear {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Ear")
            .field("guest", &self.guest)
            .field("taken", &self.taken)
            .finish()
    }
}

impl Ear {
    /// Build one, or nothing if the codec will not start.
    pub(crate) fn new(guest: u32, out: Sender) -> Option<Self> {
        match lowlat_audio::Decoder::new() {
            Ok(decoder) => Some(Self {
                guest,
                decoder,
                out,
                samples: Box::new([0; SAMPLES_MAX]),
                taken: 0,
            }),
            Err(error) => {
                lowlat_common::log_warn!("guest: no microphone decoder, error={error}");
                None
            }
        }
    }

    /// How many packets this guest's microphone has delivered.
    pub(crate) fn taken(&self) -> u64 {
        self.taken
    }

    /// How many were refused, and how many of those ended in a panic.
    pub(crate) fn refused(&self) -> (u64, u64) {
        (self.decoder.refused(), self.decoder.panicked())
    }

    /// Take one control message, if it is a microphone packet.
    ///
    /// **Everything else on this opcode is passed over.** It carries several
    /// kinds of virtual device and a host answers the ones it offers; a tablet
    /// arriving here is not an error and not ours.
    pub(crate) fn hear(&mut self, message: &Control<'_>) {
        if message.opcode != control::op::VIRTUAL_DEVICE {
            return;
        }
        let packet = match microphone::parse(message.a0, message.a1, message.a2, message.body) {
            Ok(Some(packet)) => packet,
            Ok(None) => return,
            Err(_) => {
                // A malformed body is dropped rather than fatal: it has been
                // consumed and the channel is still moving.
                return;
            }
        };
        let Ok(count) = self.decoder.decode(&packet, self.samples.as_mut_slice()) else {
            return;
        };
        let Some(samples) = self.samples.get(..count) else {
            return;
        };
        self.taken = self.taken.saturating_add(1);
        self.out.send(self.guest, samples);
    }
}

/// The message that tells a peer whether this host will take its microphone.
///
/// **A peer sends nothing until it is told.** Its own settings decide whether
/// it offers one at all, and this decides whether it may; told nothing, it
/// keeps the microphone muted and a host that only listened would receive
/// silence and look broken from both ends ([05 §9.6](../../../docs/05-host.md)).
pub(crate) fn announcement(accepting: bool) -> (u32, Vec<u8>) {
    let body = if accepting {
        b"1\0".to_vec()
    } else {
        b"0\0".to_vec()
    };
    (ANNOUNCE_ID, body)
}

/// The sub-identifier the announcement travels under.
const ANNOUNCE_ID: u32 = 18;

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(payload: &[u8]) -> Vec<u8> {
        let mut body = vec![0u8; microphone::BODY_LEN];
        microphone::encode(
            &mut body,
            &lowlat_core::microphone::Packet {
                payload,
                encoding: lowlat_core::microphone::Encoding::Raw,
            },
        )
        .expect("a body");
        body
    }

    fn samples_of(values: &[i16]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    /// Sound taken off the wire reaches whoever is polling, with the guest it
    /// came from.
    #[test]
    fn a_packet_reaches_the_consumer_with_its_guest() {
        let (into, from) = queue();
        let mut ear = Ear::new(7, into).expect("an ear");
        let body = packet(&samples_of(&[1, -2, 3]));
        ear.hear(&Control {
            a0: u32::try_from(microphone::BODY_LEN).unwrap_or(0),
            a1: microphone::MICROPHONE_ARGUMENT,
            a2: microphone::MICROPHONE_SELECTOR,
            opcode: control::op::VIRTUAL_DEVICE,
            body: &body,
        });

        let mut out = [0i16; SAMPLES_MAX];
        let taken = from.recv_timeout_into(Duration::from_millis(50), &mut out);
        assert_eq!(
            taken,
            Taken::Took {
                guest: 7,
                samples: 3,
                dropped: 0
            }
        );
        assert_eq!(&out[..3], &[1, -2, 3]);
    }

    /// **Another virtual device is passed over rather than refused.** The
    /// opcode carries several kinds and a host answers the ones it offers.
    #[test]
    fn another_virtual_device_is_ignored() {
        let (into, from) = queue();
        let mut ear = Ear::new(1, into).expect("an ear");
        let body = vec![0u8; microphone::BODY_LEN];
        ear.hear(&Control {
            a0: u32::try_from(microphone::BODY_LEN).unwrap_or(0),
            a1: 0,
            a2: 0x056A_0357,
            opcode: control::op::VIRTUAL_DEVICE,
            body: &body,
        });
        let mut out = [0i16; SAMPLES_MAX];
        assert_eq!(
            from.recv_timeout_into(Duration::ZERO, &mut out),
            Taken::Empty
        );
        assert_eq!(ear.taken(), 0);
    }

    /// **An application that stops draining loses the oldest sound, not the
    /// newest**, and is told how much went.
    #[test]
    fn a_consumer_that_stops_draining_loses_the_oldest() {
        let (into, from) = queue();
        let mut ear = Ear::new(2, into).expect("an ear");
        for value in 0..(MAX_PACKETS + 3) {
            let body = packet(&samples_of(&[i16::try_from(value).unwrap_or(0)]));
            ear.hear(&Control {
                a0: u32::try_from(microphone::BODY_LEN).unwrap_or(0),
                a1: microphone::MICROPHONE_ARGUMENT,
                a2: microphone::MICROPHONE_SELECTOR,
                opcode: control::op::VIRTUAL_DEVICE,
                body: &body,
            });
        }
        let mut out = [0i16; SAMPLES_MAX];
        let taken = from.recv_timeout_into(Duration::ZERO, &mut out);
        let Taken::Took {
            samples, dropped, ..
        } = taken
        else {
            panic!("nothing was delivered");
        };
        assert_eq!(samples, 1);
        assert_eq!(dropped, 3, "the count of what went was not reported");
        // The head is the oldest that survived, not the first that arrived.
        assert_eq!(out[0], 3, "the oldest survivor was not at the head");
    }

    /// The announcement is a decimal string, and "0" is what stops a peer.
    #[test]
    fn the_announcement_says_yes_or_no() {
        assert_eq!(announcement(true), (18, b"1\0".to_vec()));
        assert_eq!(announcement(false), (18, b"0\0".to_vec()));
    }
}
