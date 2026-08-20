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

use crate::stock;
use crate::stream::{PointerState, SeatHold};

/// The nominal size a stock shape is asked for.
///
/// **Matched to what the display's own pointers measure**, so a refused
/// pointer does not arrive at a noticeably different size from the one it
/// replaces.
const STOCK_SIZE: u32 = 24;

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
    /// What this guest is shown while somebody else holds the pointer, and its
    /// picture already encoded.
    ///
    /// **A shape the display never draws**, so it is loaded from the desktop's
    /// own theme rather than read off the plane, and it brings its own hotspot
    /// with it (crate::stock).
    refused: Option<Refused>,
    /// Whether the last update was the refused shape, so the real pointer is
    /// sent again the moment the guest gets it back.
    refusing: bool,
}

/// The refused pointer, encoded once.
#[derive(Debug)]
struct Refused {
    png: Vec<u8>,
    checksum: u32,
    width: u16,
    height: u16,
    hot_x: u16,
    hot_y: u16,
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
            refused: refused(),
            refusing: false,
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
    ///
    /// `holds` is whether this guest has the arbitrated pointer. A guest that
    /// does not is shown a refused shape: without it, its input is dropped and
    /// **nothing happens**, which is indistinguishable from a session that has
    /// stopped responding (docs/05-host.md section 7.1).
    pub fn next(&mut self, seat: &SeatHold, holds: bool) -> Option<&[u8]> {
        let generation = seat.pointer_generation();
        // **The change of turn is an update in its own right.** It owes a
        // message even when the pointer has not moved, or a guest that stops
        // moving keeps whichever shape it had when its turn changed.
        let turned = self.refusing != (!holds && self.refused.is_some());
        // **Nothing has been published yet.** A guest can be seated before the
        // display is even open, and reporting the state that has not been read
        // tells the peer there is no pointer when the truth is that nobody has
        // looked.
        if generation == 0 || (self.seen == Some(generation) && !turned) {
            return None;
        }
        self.seen = Some(generation);
        self.refusing = !holds && self.refused.is_some();
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

    /// Substitute the refused shape into a state, when this guest is not the
    /// one driving.
    fn refuse(&self, state: PointerState) -> PointerState {
        let Some(refused) = self.refused.as_ref() else {
            return state;
        };
        PointerState {
            width: refused.width,
            height: refused.height,
            hot_x: refused.hot_x,
            hot_y: refused.hot_y,
            checksum: refused.checksum,
            // The position and whether anything is drawn at all are still the
            // display's to say. Only the picture changes.
            ..state
        }
    }

    /// Build the message for one state, deciding what the peer is owed.
    ///
    /// Split from [`Sender::next`] so the decision can be exercised without a
    /// seat and without a display.
    fn update(&mut self, state: PointerState) -> Option<&[u8]> {
        let state = if self.refusing {
            self.refuse(state)
        } else {
            state
        };
        let update = Update {
            stream: 0,
            x: state.x,
            y: state.y,
            hot_x: state.hot_x,
            hot_y: state.hot_y,
            // **Never set from this backend, and plane presence is not it.**
            // A client derives relative mode from this bit as well as from the
            // relative one -- `relative || hidden` is the real test -- so any
            // moment the compositor stops using the pointer plane would put a
            // guest into relative mode with no pointer to see. The plane empties
            // for a pointer that was merely too big for it and for a moment
            // after a mode change, neither of which is an application taking the
            // pointer over. All three of these need the intent signal, which is
            // session state this backend sits below
            // (docs/07-platforms.md section 2.1).
            hidden: false,
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
            Choice::Carried if self.refusing => Image::Fresh {
                png: self
                    .refused
                    .as_ref()
                    .map_or(&[][..], |refused| &refused.png[..]),
                width: state.width,
                height: state.height,
                checksum: state.checksum,
            },
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
        if checksum == 0 || (self.image.is_empty() && !self.refusing) {
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

/// Load and encode the refused shape, once.
///
/// **Absent is a state, not a failure.** A machine with no icon theme has no
/// shape to show, and a guest there finds out it does not have the pointer the
/// way it does today: by nothing happening.
fn refused() -> Option<Refused> {
    let stock = stock::load(&stock::REFUSED, STOCK_SIZE)?;
    let mut png =
        vec![0; lowlat_core::png::upper_bound(u32::from(stock.width), u32::from(stock.height))];
    let used = lowlat_core::png::encode(
        &stock.rgba,
        u32::from(stock.width),
        u32::from(stock.height),
        (stock.width as usize) * 4,
        &mut png,
    )
    .ok()?;
    png.truncate(used);
    let checksum = lowlat_core::crc32::of(&png);
    Some(Refused {
        png,
        checksum,
        width: stock.width,
        height: stock.height,
        hot_x: stock.hot_x,
        hot_y: stock.hot_y,
    })
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

    fn stock_shape() -> Refused {
        Refused {
            png: std::vec![9, 8, 7, 6],
            checksum: 0x5150_5150,
            width: 24,
            height: 24,
            hot_x: 12,
            hot_y: 12,
        }
    }

    fn field(message: &[u8], at: usize) -> u16 {
        u16::from_be_bytes([message[13 + at], message[13 + at + 1]])
    }

    /// **A guest without the arbitrated pointer is shown that it does not have
    /// it.** Its input is dropped and nothing happens, which is
    /// indistinguishable from a session that has stopped responding, so it is
    /// sent a refused shape instead of the real pointer.
    #[test]
    fn a_guest_without_the_pointer_is_shown_a_refused_shape() {
        let mut sender = sender(false);
        sender.refused = Some(stock_shape());

        let real = sender.update(state(7)).expect("an update").to_vec();
        assert_eq!(field(&real, 7), 21, "the display's own shape");

        sender.refusing = true;
        let refused = sender.update(state(7)).expect("an update");
        assert_eq!(field(refused, 7), 24, "the stock shape's width");
        assert_eq!(field(refused, 15), 12, "and the hotspot that came with it");
        assert!(flags(refused).contains(Flags::IMAGE));
        assert_ne!(real, refused, "the same picture either way");

        // **The position is still the display's to say.** Only the picture
        // changes, or a guest that cannot drive also loses track of where the
        // pointer is.
        assert_eq!(field(refused, 11), 10);
        assert_eq!(field(refused, 13), 20);
    }

    /// And it gets the real pointer back the moment the turn passes to it.
    #[test]
    fn the_real_pointer_comes_back_when_the_turn_does() {
        let mut sender = sender(false);
        sender.refused = Some(stock_shape());
        sender.refusing = true;
        let refused = sender.update(state(7)).expect("an update").to_vec();

        sender.refusing = false;
        let back = sender.update(state(7)).expect("an update");
        assert_eq!(field(back, 7), 21);
        assert_ne!(refused, back);
    }

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

    /// **The hidden bit is never set from this backend, and that is not an
    /// omission.** A client's test for relative mode is `relative || hidden`,
    /// so this bit takes a guest's pointer away and locks it into relative
    /// motion. Nothing here can tell an application hiding the pointer from a
    /// pointer that merely outgrew the hardware plane, or from the moment
    /// after a mode change, so setting it on any of those traps a guest whose
    /// pointer was never taken over.
    #[test]
    fn the_bit_a_client_reads_as_relative_is_never_set() {
        let mut sender = sender(true);
        for update in [state(0), state(7)] {
            let message = sender.update(update).expect("an update");
            assert!(!flags(message).contains(Flags::HIDDEN), "hidden was set");
            assert!(!flags(message).contains(Flags::RELATIVE));
        }
    }
}
