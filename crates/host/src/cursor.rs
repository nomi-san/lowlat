//! What one guest is told about the pointer.
//!
//! The stream reads the pointer once and every guest reports it separately,
//! because what a guest is owed depends on what it already holds
//! (docs/05-host.md section 8).
//!
//! **A peer that keeps pictures is sent a name instead of a picture**, and one
//! that does not must be sent the picture every time. Naming a picture to a
//! peer that never kept one leaves it drawing whatever it last had, which is a
//! pointer frozen in the shape it happened to be in when the guest joined.

use lowlat_core::cursor::{self, Image, Update};

use crate::stream::{PointerState, SeatHold};

/// Pictures one peer is assumed to hold.
///
/// The far side is told to forget them all when this fills, which is the only
/// way its cache and this one can be made to agree again: nothing in the
/// protocol reports what it evicted.
const CAPACITY: usize = 100;

/// What this update says about the picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Choice {
    /// Nothing about the picture changed.
    Nothing,
    /// It travels.
    Carried,
    /// The peer holds it already.
    Named,
}

/// One guest's view of the pointer.
pub struct Sender {
    /// Whether the peer said it keeps pictures. **Not assumed**: a peer that
    /// did not say so gets the picture every time.
    caching: bool,
    held: [u32; CAPACITY],
    count: usize,
    /// The generation last acted on, or `None` before the first update.
    ///
    /// **A joining guest is owed the pointer that is already on screen**, and
    /// nothing about it will change just because somebody connected, so the
    /// first update cannot wait for one.
    seen: Option<u32>,
    image: Vec<u8>,
    message: Vec<u8>,
    /// Whether the first update has been reported.
    said: bool,
}

impl core::fmt::Debug for Sender {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Sender")
            .field("caching", &self.caching)
            .field("held", &self.count)
            .finish_non_exhaustive()
    }
}

impl Default for Sender {
    fn default() -> Self {
        Self::new()
    }
}

impl Sender {
    pub fn new() -> Self {
        Self {
            caching: false,
            held: [0; CAPACITY],
            count: 0,
            seen: None,
            image: Vec::new(),
            said: false,
            // Sized once for the largest picture a pointer plane can carry, so
            // building an update never allocates.
            message: vec![0; cursor::encoded_len(lowlat_core::png::upper_bound(256, 256))],
        }
    }

    /// Record what the peer declared it can do with pointer pictures.
    pub fn caches(&mut self, caching: bool) {
        self.caching = caching;
    }

    /// The next update for this guest, if it is owed one.
    pub fn next(&mut self, seat: &SeatHold) -> Option<&[u8]> {
        let generation = seat.pointer_generation();
        // **Nothing has been published yet.** A guest can be seated before the
        // display is even open, and reporting the state that has not been read
        // tells the peer there is no pointer when the truth is that nobody has
        // looked.
        if generation == 0 || self.seen == Some(generation) {
            return None;
        }
        self.seen = Some(generation);
        let state = seat.pointer(&mut self.image)?;
        // **Once per guest.** A pointer moves constantly and logging it would
        // bury a session; logging none leaves "the guest sees no pointer" with
        // nothing to read. This is the line that says the path is live.
        if !self.said {
            self.said = true;
            lowlat_common::log_info!(
                "guest: first pointer {}x{} at ({},{}) hot=({},{}) checksum={:#010x} caching={}",
                state.width,
                state.height,
                state.x,
                state.y,
                state.hot_x,
                state.hot_y,
                state.checksum,
                u8::from(self.caching)
            );
        }
        self.update(state)
    }

    /// Build the message for one state, deciding what the peer is owed.
    ///
    /// Split from [`Sender::next`] so the decision can be exercised without a
    /// seat and without a display.
    fn update(&mut self, state: PointerState) -> Option<&[u8]> {
        let update = Update {
            stream: 0,
            x: state.x,
            y: state.y,
            hot_x: state.hot_x,
            hot_y: state.hot_y,
            // **Plane presence drives this and nothing else.** A pointer that
            // is not on the plane is either one an application hid or one the
            // compositor drew into the picture, and in both cases a peer that
            // drew its own would be wrong: there is nothing to draw, or the
            // frame already carries it.
            hidden: !state.drawn,
            // Both need a signal this backend cannot see, and inventing one
            // from what it can see traps the pointer of anybody who shook the
            // mouse (docs/07-platforms.md section 2.1).
            relative: false,
            suppressed: false,
        };

        // **Decided before anything is borrowed.** The picture is lent out of
        // this same object, so the bookkeeping has to be finished first.
        let (choice, forget) = self.decide(state.checksum);
        let image = match choice {
            Choice::Nothing => Image::Unchanged,
            Choice::Named => Image::Cached {
                checksum: state.checksum,
            },
            // **The picture's own size travels with it**, not the update's
            // idea of it: a message that disagreed with the image it carries
            // is read as the image.
            Choice::Carried => Image::Fresh {
                png: &self.image,
                width: state.width,
                height: state.height,
                checksum: state.checksum,
            },
        };
        let written = cursor::encode(&mut self.message, &update, image, forget).ok()?;
        self.message.get(..written)
    }

    /// What to say about the picture, and whether the peer must forget first.
    fn decide(&mut self, checksum: u32) -> (Choice, bool) {
        if checksum == 0 || self.image.is_empty() {
            return (Choice::Nothing, false);
        }
        if !self.caching {
            return (Choice::Carried, false);
        }
        if self
            .held
            .get(..self.count)
            .is_some_and(|held| held.contains(&checksum))
        {
            return (Choice::Named, false);
        }
        // **Full means forget everything, not evict one.** The far side is
        // told what to drop and cannot report what it dropped, so the only
        // state the two ends can agree on afterwards is an empty one.
        let forget = self.count >= CAPACITY;
        if forget {
            self.count = 0;
        }
        if let Some(slot) = self.held.get_mut(self.count) {
            *slot = checksum;
            self.count += 1;
        }
        (Choice::Carried, forget)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(checksum: u32) -> PointerState {
        PointerState {
            x: 10,
            y: 20,
            hot_x: 0,
            hot_y: 0,
            width: 21,
            height: 24,
            checksum,
            drawn: true,
        }
    }

    fn sender(caching: bool) -> Sender {
        let mut sender = Sender::new();
        sender.caches(caching);
        sender.image = std::vec![1, 2, 3, 4];
        sender
    }

    /// **The values come from the encoder's own type**, so a bit that moves
    /// moves here too. Spelling them out again is how a test agrees with
    /// itself rather than with the wire.
    fn flags(message: &[u8]) -> cursor::Flags {
        cursor::Flags::from_bits(u16::from_be_bytes([message[13 + 19], message[13 + 20]]))
    }

    use cursor::Flags;

    /// **A peer that did not say it keeps pictures gets the picture, every
    /// time.** Naming one it never kept leaves it drawing whatever it last
    /// had, and nothing in the protocol would ever correct that.
    #[test]
    fn a_peer_that_does_not_cache_is_sent_the_picture_every_time() {
        let mut sender = sender(false);
        for _ in 0..3 {
            let message = sender.update(state(7)).expect("an update");
            assert!(flags(message).contains(Flags::IMAGE));
            assert!(!flags(message).contains(Flags::CACHED));
        }
    }

    /// One that does is sent the picture once and its name afterwards.
    #[test]
    fn a_picture_travels_once_to_a_peer_that_keeps_it() {
        let mut sender = sender(true);
        let first = sender.update(state(7)).expect("an update");
        assert!(flags(first).contains(Flags::IMAGE));

        let again = sender.update(state(7)).expect("an update");
        assert!(flags(again).contains(Flags::CACHED));
        assert!(
            !flags(again).contains(Flags::IMAGE),
            "a name claimed to carry a picture"
        );

        // A shape it has not seen is a picture again.
        let other = sender.update(state(9)).expect("an update");
        assert!(flags(other).contains(Flags::IMAGE));
    }

    /// **A full cache is emptied, not evicted from.** The far side cannot
    /// report what it dropped, so an eviction here leaves the two ends
    /// disagreeing about a name that is still in use.
    #[test]
    fn a_full_cache_tells_the_peer_to_forget_everything() {
        let mut sender = sender(true);
        let capacity = u32::try_from(CAPACITY).expect("the cache is small");
        for shape in 0..capacity {
            let message = sender.update(state(shape + 1)).expect("an update");
            assert!(
                !flags(message).contains(Flags::FORGET),
                "forgot early at {shape}"
            );
        }
        let over = sender.update(state(capacity + 1)).expect("an update");
        assert!(
            flags(over).contains(Flags::FORGET),
            "the cache never filled"
        );
        assert!(
            flags(over).contains(Flags::IMAGE),
            "forgotten and not resent"
        );

        // And the one it had before the flush is a picture again, because the
        // peer was told to drop it.
        let old = sender.update(state(1)).expect("an update");
        assert!(flags(old).contains(Flags::IMAGE));
    }

    /// Nothing drawing a pointer is a state a peer has to be told about, or it
    /// keeps drawing the last one it was sent over a picture that has its own.
    #[test]
    fn a_pointer_that_is_not_drawn_is_reported_hidden() {
        let mut sender = sender(true);
        let mut gone = state(0);
        gone.drawn = false;
        let message = sender.update(gone).expect("an update");
        assert!(flags(message).contains(Flags::HIDDEN));
        assert!(!flags(message).contains(Flags::IMAGE));
    }
}
