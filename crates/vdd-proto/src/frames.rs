//! The frame channel: what the display driver publishes of the pictures a
//! monitor is handed, in one page the host maps (docs/11-drivers.md section
//! 1.6).
//!
//! **One writer, any number of readers, and nobody waits on anybody.** The
//! driver's worker for a monitor writes the page under a sequence lock -- the
//! sequence odd while it writes -- and a reader that overlapped a write reads
//! again. Every word is an atomic, so a reader in another process that meets
//! a write half done reads values, never torn memory, and the sequence tells
//! it to read again. A reader maps the page read-only and writes nothing.
//!
//! The page names the monitor's swapchain -- its generation, the GPU it
//! renders on, its size and refresh, the fence the driver signals after each
//! acquire, each buffer as it is first seen -- and the latest picture: its
//! buffer, its sequence (the fence's value for it), its frame number and when
//! the driver acquired it. Its handles are values in the driver's process,
//! which a reader takes from that process (section 1.2).
//!
//! **A picture stays intact while the system cycles its other buffers**: the
//! system writes a buffer again only once the driver has acquired as many
//! newer pictures as the swapchain has buffers, less one, so a reader checks
//! [`Snapshot::overtaken`] after it has queued its read, and reads the latest
//! instead when the picture it took was overtaken.
//!
//! **The pointer travels in the same section** (section 1.7): what the system
//! last said of its hardware cursor over the monitor, in the page's own words
//! under a sequence of its own, and the shape's pixels in the pages after it,
//! under another, with the system's number for the shape. The driver writes a
//! shape's pixels before the state that names it, so a reader that sees a new
//! number in the state finds those pixels, or a newer shape's.

// The crate takes no dependency, so the model checker's atomics are named
// here rather than through the shared shim; nothing else of it is needed.
#[cfg(not(loom))]
use core::sync::atomic::{AtomicU64, Ordering, fence};
#[cfg(loom)]
use loom::sync::atomic::{AtomicU64, Ordering, fence};

/// The channel's own version, in its page and in the answer to a request for
/// a monitor's frames; a reader refuses a page of another.
pub const VERSION: u32 = 2;

/// The page: one of the system's.
pub const PAGE: usize = 4096;

/// The most buffers of one swapchain the page names.
pub const MAX_BUFFERS: usize = 16;

/// What the page's first word holds once the driver has prepared it.
pub const MAGIC: u64 = u64::from_le_bytes(*b"lowlatfc");

// The page, word by word: the magic and the version, written once before any
// reader can have the page; the sequence; the snapshot's words under it.
const W_MAGIC: usize = 0;
const W_VERSION: usize = 1;
const W_SEQUENCE: usize = 2;
const W_DATA: usize = 3;

// The snapshot's words.
const D_EVENT: usize = 0;
const D_GENERATION: usize = 1;
const D_RENDER: usize = 2;
const D_SIZE: usize = 3;
const D_REFRESH: usize = 4;
const D_FENCE: usize = 5;
const D_BUFFERS: usize = 6;
const D_BUFFER: usize = 7;
const D_SEQUENCE: usize = D_BUFFER + MAX_BUFFERS;
const D_INDEX: usize = D_SEQUENCE + 1;
const D_FRAME: usize = D_SEQUENCE + 2;
const D_ACQUIRED: usize = D_SEQUENCE + 3;
const D_FLAGS: usize = D_SEQUENCE + 4;
const D_PICTURES: usize = D_SEQUENCE + 5;
const D_GAPS: usize = D_SEQUENCE + 6;
/// The snapshot's length in words.
pub const DATA: usize = D_SEQUENCE + 7;

/// The words of the page this version uses, from its start.
pub const WORDS: usize = W_DATA + DATA;

// The page holds every word this version uses.
const _: () = assert!(WORDS * 8 <= PAGE);

// The pointer's state: its sequence, then its words, in the page past the
// snapshot's.
const W_POINTER: usize = 64;
const P_FLAGS: usize = 0;
const P_SHAPE: usize = 1;
const P_KIND: usize = 2;
const P_SIZE: usize = 3;
const P_PITCH: usize = 4;
const P_HOT: usize = 5;
/// The pointer's state's length in words.
pub const POINTER_DATA: usize = 6;
const _: () = assert!(WORDS <= W_POINTER && (W_POINTER + 1 + POINTER_DATA) * 8 <= PAGE);
const CURSOR: u64 = 1;
const VISIBLE: u64 = 2;

/// The most a pointer's picture spans a side: the largest the wire carries.
pub const POINTER_LIMIT: u32 = 256;
/// The most bytes a shape's pixels take: four a pixel at the limit.
pub const SHAPE_BYTES: usize = (POINTER_LIMIT * POINTER_LIMIT * 4) as usize;
// The shape's pixels, from the second page: their sequence, the shape's
// number and length, then the bytes eight to a word.
const W_SHAPE: usize = PAGE / 8;
const S_ID: usize = 0;
const S_BYTES: usize = 1;
const S_DATA: usize = 2;

/// The section's size in bytes: the page, then the shape's, in whole pages.
pub const SIZE: usize = (8 * (W_SHAPE + 1 + S_DATA + SHAPE_BYTES / 8)).div_ceil(PAGE) * PAGE;
/// The section's length in words.
pub const SECTION_WORDS: usize = SIZE / 8;
// The section is whole pages and holds a shape at the limit after the page.
const _: () = assert!(SIZE % PAGE == 0 && W_SHAPE + 1 + S_DATA + SHAPE_BYTES / 8 <= SECTION_WORDS);

const REPEAT: u64 = 1;
const PROTECTED: u64 = 2;

/// How many times a reader tries before taking the writer to be stuck mid
/// write, which a writer that has died is.
const ATTEMPTS: u32 = 64;

/// The latest picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Picture {
    /// The fence's value for it, counted from one in each generation; zero
    /// before the first.
    pub sequence: u64,
    /// Which of the swapchain's buffers holds it.
    pub index: u32,
    /// The system's number for the picture; a repeat is no new picture.
    pub frame: u32,
    /// When the driver acquired it, on the system's performance counter.
    pub acquired: u64,
    /// Its frame number repeats the one before: the same picture again.
    pub repeat: bool,
    /// The system marked its buffer as holding protected content.
    pub protected: bool,
}

/// One consistent reading of the page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Snapshot {
    /// The event the driver sets at each publish, a value in its process.
    pub event: u64,
    /// The swapchain's, one more for each the driver is given; zero while the
    /// monitor has none.
    pub generation: u64,
    /// The GPU the swapchain renders on: its adapter identifier, the high
    /// half above the low.
    pub render: u64,
    pub width: u32,
    pub height: u32,
    /// The refresh the system committed the monitor to, as a fraction.
    pub refresh: (u32, u32),
    /// The fence the driver signals after each acquire, at the picture's
    /// sequence, a value in its process.
    pub fence: u64,
    /// The swapchain's buffers seen so far, each a handle value in the
    /// driver's process, in the order the system first handed them over.
    pub buffers: u32,
    pub buffer: [u64; MAX_BUFFERS],
    pub picture: Picture,
    /// Pictures the driver acquired since it began sharing this monitor's, and
    /// frame numbers skipped between them.
    pub pictures: u64,
    pub gaps: u64,
}

impl Snapshot {
    /// Whether the buffer of the picture a reader took at `sequence` of
    /// `generation` may have been written again since. **Only once the driver
    /// has acquired as many newer pictures as the swapchain has buffers, less
    /// one**: until then the system writes its other buffers. A swapchain
    /// whose buffers are not all seen yet counts as having fewer, which errs
    /// early. Another generation's buffers the system no longer writes at
    /// all.
    #[must_use]
    pub fn overtaken(&self, generation: u64, sequence: u64) -> bool {
        if self.generation != generation || sequence == 0 {
            return false;
        }
        let slack = u64::from(self.buffers).saturating_sub(1).max(1);
        self.picture.sequence >= sequence.saturating_add(slack)
    }

    /// The handle value of buffer `index`, when the page names one there.
    #[must_use]
    pub fn buffer(&self, index: u32) -> Option<u64> {
        if index >= self.buffers {
            return None;
        }
        self.buffer
            .get(usize::try_from(index).ok()?)
            .copied()
            .filter(|&value| value != 0)
    }

    fn encode(&self) -> [u64; DATA] {
        let mut words = [0u64; DATA];
        let mut put = |at: usize, value: u64| {
            if let Some(word) = words.get_mut(at) {
                *word = value;
            }
        };
        put(D_EVENT, self.event);
        put(D_GENERATION, self.generation);
        put(D_RENDER, self.render);
        put(
            D_SIZE,
            (u64::from(self.width) << 32) | u64::from(self.height),
        );
        put(
            D_REFRESH,
            (u64::from(self.refresh.0) << 32) | u64::from(self.refresh.1),
        );
        put(D_FENCE, self.fence);
        put(D_BUFFERS, u64::from(self.buffers));
        for (i, &value) in self.buffer.iter().enumerate() {
            put(D_BUFFER + i, value);
        }
        put(D_SEQUENCE, self.picture.sequence);
        put(D_INDEX, u64::from(self.picture.index));
        put(D_FRAME, u64::from(self.picture.frame));
        put(D_ACQUIRED, self.picture.acquired);
        put(
            D_FLAGS,
            if self.picture.repeat { REPEAT } else { 0 }
                | if self.picture.protected { PROTECTED } else { 0 },
        );
        put(D_PICTURES, self.pictures);
        put(D_GAPS, self.gaps);
        words
    }

    /// The snapshot `words` describe, or none when they cannot be one: more
    /// buffers than a page names, or a picture in a buffer it does not name.
    /// **A page is read as hostile**: another process wrote it.
    fn decode(words: &[u64; DATA]) -> Option<Self> {
        let get = |at: usize| words.get(at).copied().unwrap_or(0);
        let half = |word: u64| {
            (
                u32::try_from(word >> 32).unwrap_or(0),
                u32::try_from(word & 0xffff_ffff).unwrap_or(0),
            )
        };
        let buffers = u32::try_from(get(D_BUFFERS)).ok()?;
        if usize::try_from(buffers).ok()? > MAX_BUFFERS {
            return None;
        }
        let mut buffer = [0u64; MAX_BUFFERS];
        for (i, value) in buffer.iter_mut().enumerate() {
            *value = get(D_BUFFER + i);
        }
        let flags = get(D_FLAGS);
        let picture = Picture {
            sequence: get(D_SEQUENCE),
            index: u32::try_from(get(D_INDEX)).ok()?,
            frame: u32::try_from(get(D_FRAME)).ok()?,
            acquired: get(D_ACQUIRED),
            repeat: flags & REPEAT != 0,
            protected: flags & PROTECTED != 0,
        };
        if picture.sequence != 0 && picture.index >= buffers {
            return None;
        }
        let (width, height) = half(get(D_SIZE));
        Some(Self {
            event: get(D_EVENT),
            generation: get(D_GENERATION),
            render: get(D_RENDER),
            width,
            height,
            refresh: half(get(D_REFRESH)),
            fence: get(D_FENCE),
            buffers,
            buffer,
            picture,
            pictures: get(D_PICTURES),
            gaps: get(D_GAPS),
        })
    }
}

/// The two kinds of shape the system hands a display driver, four bytes a
/// pixel each; it converts every monochrome pointer to the first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShapeKind {
    /// BGRA whose alpha says, for each pixel, opaque (0) or inverting the
    /// screen beneath it (0xFF).
    MaskedColour,
    /// BGRA with alpha.
    Alpha,
}

/// What the system last said of the pointer over the monitor, as the driver
/// read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Pointer {
    /// A hardware cursor is set up: without one the system draws the pointer
    /// into the pictures.
    pub cursor: bool,
    /// The system shows the cursor: not while it hides it, nor while it draws
    /// the pointer into the pictures itself.
    pub visible: bool,
    /// The system's number for the shape, moved each time a shape is set,
    /// the same picture again included; zero before the first.
    pub shape: u32,
    /// The shape's kind; none before the first.
    pub kind: Option<ShapeKind>,
    pub width: u32,
    pub height: u32,
    /// Bytes from one row of the shape's pixels to the next.
    pub pitch: u32,
    /// Where in the shape the pointer points, from its top-left.
    pub hot_x: u32,
    pub hot_y: u32,
}

impl Pointer {
    fn encode(&self) -> [u64; POINTER_DATA] {
        let mut words = [0u64; POINTER_DATA];
        let mut put = |at: usize, value: u64| {
            if let Some(word) = words.get_mut(at) {
                *word = value;
            }
        };
        put(
            P_FLAGS,
            if self.cursor { CURSOR } else { 0 } | if self.visible { VISIBLE } else { 0 },
        );
        put(P_SHAPE, u64::from(self.shape));
        put(
            P_KIND,
            match self.kind {
                None => 0,
                Some(ShapeKind::MaskedColour) => 1,
                Some(ShapeKind::Alpha) => 2,
            },
        );
        put(
            P_SIZE,
            (u64::from(self.width) << 32) | u64::from(self.height),
        );
        put(P_PITCH, u64::from(self.pitch));
        put(P_HOT, (u64::from(self.hot_x) << 32) | u64::from(self.hot_y));
        words
    }

    /// The pointer `words` describe, or none when they cannot be one: a kind
    /// there is no such, a shape past the limit, or rows that would not fit
    /// the section. **Read as hostile**: another process wrote it.
    fn decode(words: &[u64; POINTER_DATA]) -> Option<Self> {
        let get = |at: usize| words.get(at).copied().unwrap_or(0);
        let half = |word: u64| {
            (
                u32::try_from(word >> 32).unwrap_or(0),
                u32::try_from(word & 0xffff_ffff).unwrap_or(0),
            )
        };
        let kind = match get(P_KIND) {
            0 => None,
            1 => Some(ShapeKind::MaskedColour),
            2 => Some(ShapeKind::Alpha),
            _ => return None,
        };
        let (width, height) = half(get(P_SIZE));
        let pitch = u32::try_from(get(P_PITCH)).ok()?;
        if width > POINTER_LIMIT
            || height > POINTER_LIMIT
            || u64::from(pitch) * u64::from(height) > SHAPE_BYTES as u64
            || (kind.is_some() && u64::from(pitch) < u64::from(width) * 4)
        {
            return None;
        }
        let flags = get(P_FLAGS);
        let (hot_x, hot_y) = half(get(P_HOT));
        Some(Self {
            cursor: flags & CURSOR != 0,
            visible: flags & VISIBLE != 0,
            shape: u32::try_from(get(P_SHAPE)).ok()?,
            kind,
            width,
            height,
            pitch,
            hot_x,
            hot_y,
        })
    }

    /// The bytes the shape's pixels take: its rows at its pitch.
    #[must_use]
    pub fn shape_bytes(&self) -> usize {
        usize::try_from(u64::from(self.pitch) * u64::from(self.height)).unwrap_or(usize::MAX)
    }
}

/// Prepare `section` before any reader can have it: its magic and version.
/// False when it is shorter than the channel's section.
pub fn prepare(section: &[AtomicU64]) -> bool {
    let (Some(magic), Some(version)) = (section.get(W_MAGIC), section.get(W_VERSION)) else {
        return false;
    };
    if section.len() < SECTION_WORDS {
        return false;
    }
    version.store(u64::from(VERSION), Ordering::Relaxed);
    // Release: a reader that sees the magic sees the version too.
    magic.store(MAGIC, Ordering::Release);
    true
}

/// Publish `snapshot` into `page`. **The one writer's alone**: two writers
/// would each take the other's sequence for their own.
pub fn publish(page: &[AtomicU64], snapshot: &Snapshot) {
    let (Some(lock), Some(words)) = (page.get(W_SEQUENCE), page.get(W_DATA..WORDS)) else {
        return;
    };
    write_locked(lock, words, snapshot.encode());
}

/// Whether `section` is prepared and of this version.
fn current(section: &[AtomicU64]) -> bool {
    let (Some(magic), Some(version)) = (section.get(W_MAGIC), section.get(W_VERSION)) else {
        return false;
    };
    // Acquire: pairs with the magic's release in `prepare`.
    magic.load(Ordering::Acquire) == MAGIC && version.load(Ordering::Relaxed) == u64::from(VERSION)
}

/// The latest snapshot of `page`: none when the page is not this version's,
/// when the writer overlapped every attempt -- a writer that died mid write
/// leaves its sequence odd for good -- or when what it says cannot be.
pub fn read(page: &[AtomicU64]) -> Option<Snapshot> {
    if !current(page) {
        return None;
    }
    let (Some(lock), Some(words)) = (page.get(W_SEQUENCE), page.get(W_DATA..WORDS)) else {
        return None;
    };
    let mut data = [0u64; DATA];
    for _ in 0..ATTEMPTS {
        if read_locked(lock, words, &mut data) {
            return Snapshot::decode(&data);
        }
        core::hint::spin_loop();
    }
    None
}

/// Publish `pointer` into `section`, after the pixels of the shape it names
/// ([`publish_shape`]). The one writer's alone, as [`publish`] is.
pub fn publish_pointer(section: &[AtomicU64], pointer: &Pointer) {
    let (Some(lock), Some(words)) = (
        section.get(W_POINTER),
        section.get(W_POINTER + 1..W_POINTER + 1 + POINTER_DATA),
    ) else {
        return;
    };
    write_locked(lock, words, pointer.encode());
}

/// The pointer as `section` last says it: none before the driver has said
/// anything of it, and as [`read`] says none.
pub fn read_pointer(section: &[AtomicU64]) -> Option<Pointer> {
    if !current(section) {
        return None;
    }
    let (Some(lock), Some(words)) = (
        section.get(W_POINTER),
        section.get(W_POINTER + 1..W_POINTER + 1 + POINTER_DATA),
    ) else {
        return None;
    };
    // Never written: nothing said yet.
    if lock.load(Ordering::Acquire) == 0 {
        return None;
    }
    let mut data = [0u64; POINTER_DATA];
    for _ in 0..ATTEMPTS {
        if read_locked(lock, words, &mut data) {
            return Pointer::decode(&data);
        }
        core::hint::spin_loop();
    }
    None
}

/// Publish the pixels of the shape the system numbered `shape` into
/// `section`, **before the pointer's state names it**. False for more bytes
/// than a shape at the limit takes.
pub fn publish_shape(section: &[AtomicU64], shape: u32, bytes: &[u8]) -> bool {
    if bytes.len() > SHAPE_BYTES {
        return false;
    }
    let (Some(lock), Some(words)) = (
        section.get(W_SHAPE),
        section.get(W_SHAPE + 1..W_SHAPE + 1 + S_DATA + bytes.len().div_ceil(8)),
    ) else {
        return false;
    };
    let header = [u64::from(shape), bytes.len() as u64];
    let pixels = bytes.chunks(8).map(|chunk| {
        let mut word = [0u8; 8];
        word.get_mut(..chunk.len())
            .unwrap_or_default()
            .copy_from_slice(chunk);
        u64::from_le_bytes(word)
    });
    write_locked(lock, words, header.into_iter().chain(pixels));
    true
}

/// Copy the pixels of the shape numbered `shape` from `section` into `into`:
/// how many bytes, or none when the section holds another shape's -- a newer
/// one, published since the state was read -- when they would not fit, or as
/// [`read`] says none.
pub fn read_shape(section: &[AtomicU64], shape: u32, into: &mut [u8]) -> Option<usize> {
    if !current(section) {
        return None;
    }
    let lock = section.get(W_SHAPE)?;
    for _ in 0..ATTEMPTS {
        // The length first, unchecked, to know how many words to read; the
        // locked read below says whether it held.
        let length = section.get(W_SHAPE + 1 + S_BYTES)?.load(Ordering::Relaxed);
        let length = usize::try_from(length)
            .ok()
            .filter(|&n| n <= SHAPE_BYTES && n <= into.len())?;
        let words = section.get(W_SHAPE + 1..W_SHAPE + 1 + S_DATA + length.div_ceil(8))?;
        let (mut id, mut bytes) = (u64::MAX, u64::MAX);
        let whole = read_locked_with(lock, words, |at, value| match at {
            S_ID => id = value,
            S_BYTES => bytes = value,
            _ => {
                let start = (at - S_DATA) * 8;
                if let Some(chunk) = into.get_mut(start..(start + 8).min(length)) {
                    chunk.copy_from_slice(
                        value.to_le_bytes().get(..chunk.len()).unwrap_or_default(),
                    );
                }
            }
        });
        if whole && bytes == length as u64 {
            return (id == u64::from(shape)).then_some(length);
        }
        core::hint::spin_loop();
    }
    None
}

/// Write `data` into `words` under `lock`. The sequence goes odd before the
/// first word and even again after the last; the release fence keeps every
/// word's store after the odd one, so a reader that sees any of them sees the
/// sequence moved.
pub(crate) fn write_locked(
    lock: &AtomicU64,
    words: &[AtomicU64],
    data: impl IntoIterator<Item = u64>,
) {
    let sequence = lock.load(Ordering::Relaxed);
    lock.store(sequence.wrapping_add(1), Ordering::Relaxed);
    fence(Ordering::Release);
    for (word, value) in words.iter().zip(data) {
        word.store(value, Ordering::Relaxed);
    }
    // Release: a reader that sees the even sequence sees every word above.
    lock.store(sequence.wrapping_add(2), Ordering::Release);
}

/// Read `words` under `lock` into `out`: true when no write overlapped.
pub(crate) fn read_locked(lock: &AtomicU64, words: &[AtomicU64], out: &mut [u64]) -> bool {
    read_locked_with(lock, words, |at, value| {
        if let Some(slot) = out.get_mut(at) {
            *slot = value;
        }
    })
}

/// Read `words` under `lock`, each handed to `take` with its place: true
/// when no write overlapped, and only then is what `take` was handed one
/// write's. The acquire fence after the words keeps the sequence's second
/// load after them, so a word from a later write is caught by the sequence
/// it moved.
pub(crate) fn read_locked_with(
    lock: &AtomicU64,
    words: &[AtomicU64],
    mut take: impl FnMut(usize, u64),
) -> bool {
    let before = lock.load(Ordering::Acquire);
    if before & 1 == 1 {
        return false;
    }
    for (at, word) in words.iter().enumerate() {
        take(at, word.load(Ordering::Relaxed));
    }
    fence(Ordering::Acquire);
    lock.load(Ordering::Relaxed) == before
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use std::vec::Vec;

    fn page() -> Vec<AtomicU64> {
        (0..SECTION_WORDS).map(|_| AtomicU64::new(0)).collect()
    }

    fn pointer() -> Pointer {
        Pointer {
            cursor: true,
            visible: true,
            shape: 0x0001_0203,
            kind: Some(ShapeKind::Alpha),
            width: 32,
            height: 48,
            pitch: 128,
            hot_x: 7,
            hot_y: 31,
        }
    }

    fn snapshot() -> Snapshot {
        let mut buffer = [0u64; MAX_BUFFERS];
        buffer[0] = 0x7b0;
        buffer[1] = 0x7bc;
        buffer[2] = 0x7c0;
        Snapshot {
            event: 0x654,
            generation: 3,
            render: (1u64 << 32) | 0x14805,
            width: 2560,
            height: 1440,
            refresh: (120, 1),
            fence: 0x7b8,
            buffers: 3,
            buffer,
            picture: Picture {
                sequence: 1234,
                index: 2,
                frame: 18_223,
                acquired: 0x0000_0123_4567_89ab,
                repeat: true,
                protected: false,
            },
            pictures: 30_304,
            gaps: 1,
        }
    }

    #[test]
    fn the_page_fits_and_its_words_do_not_overlap() {
        assert_eq!(DATA, 30);
        let words = snapshot().encode();
        assert_eq!(Snapshot::decode(&words), Some(snapshot()));
        // Each field in a word of its own: one changed changes one word.
        let mut other = snapshot();
        other.picture.frame += 1;
        let changed = words
            .iter()
            .zip(other.encode().iter())
            .filter(|(a, b)| a != b)
            .count();
        assert_eq!(changed, 1);
    }

    #[test]
    fn a_published_snapshot_is_read_back_whole() {
        let page = page();
        assert_eq!(read(&page), None, "an unprepared page reads as nothing");
        assert!(prepare(&page));
        assert_eq!(read(&page), Some(Snapshot::default()));
        publish(&page, &snapshot());
        assert_eq!(read(&page), Some(snapshot()));
        assert_eq!(page[W_SEQUENCE].load(Ordering::Relaxed), 2);
        let mut next = snapshot();
        next.picture.sequence += 1;
        next.picture.index = 0;
        publish(&page, &next);
        assert_eq!(read(&page), Some(next));
        assert_eq!(page[W_SEQUENCE].load(Ordering::Relaxed), 4);
    }

    #[test]
    fn a_page_of_another_version_or_none_is_refused() {
        let page = page();
        assert!(prepare(&page));
        publish(&page, &snapshot());
        page[W_VERSION].store(u64::from(VERSION) + 1, Ordering::Relaxed);
        assert_eq!(read(&page), None);
        page[W_VERSION].store(u64::from(VERSION), Ordering::Relaxed);
        page[W_MAGIC].store(0, Ordering::Relaxed);
        assert_eq!(read(&page), None);
        let short: Vec<AtomicU64> = (0..WORDS - 1).map(|_| AtomicU64::new(0)).collect();
        assert!(!prepare(&short));
        assert_eq!(read(&short), None);
        // A first version's page alone is short of the shape's pages.
        let one: Vec<AtomicU64> = (0..PAGE / 8).map(|_| AtomicU64::new(0)).collect();
        assert!(!prepare(&one));
    }

    #[test]
    fn the_pointer_round_trips_each_field_in_a_word_of_its_own() {
        assert_eq!(SHAPE_BYTES, 256 * 256 * 4);
        let words = pointer().encode();
        assert_eq!(Pointer::decode(&words), Some(pointer()));
        // Each field in a word of its own, a pair's halves aside.
        let mut other = pointer();
        other.pitch += 4;
        let changed = words
            .iter()
            .zip(other.encode().iter())
            .filter(|(a, b)| a != b)
            .count();
        assert_eq!(changed, 1);
        assert_eq!(pointer().shape_bytes(), 128 * 48);
    }

    #[test]
    fn the_pointer_is_nothing_until_said_and_beside_the_pictures() {
        let section = page();
        assert!(prepare(&section));
        assert_eq!(read_pointer(&section), None, "nothing said yet");
        publish(&section, &snapshot());
        assert_eq!(read_pointer(&section), None, "a picture says nothing of it");
        publish_pointer(&section, &pointer());
        assert_eq!(read_pointer(&section), Some(pointer()));
        assert_eq!(
            read(&section),
            Some(snapshot()),
            "the pictures' part untouched"
        );
        let none = Pointer::default();
        publish_pointer(&section, &none);
        assert_eq!(
            read_pointer(&section),
            Some(none),
            "no hardware cursor, said"
        );
        section[W_POINTER].store(5, Ordering::Relaxed);
        assert_eq!(read_pointer(&section), None, "a write in progress");
        section[W_VERSION].store(1, Ordering::Relaxed);
        section[W_POINTER].store(6, Ordering::Relaxed);
        assert_eq!(read_pointer(&section), None, "another version's");
    }

    #[test]
    fn a_pointer_that_cannot_be_is_refused() {
        let mut words = pointer().encode();
        words[P_KIND] = 3;
        assert_eq!(Pointer::decode(&words), None, "no such kind");
        let mut wide = pointer();
        wide.width = POINTER_LIMIT + 1;
        wide.pitch = wide.width * 4;
        assert_eq!(Pointer::decode(&wide.encode()), None, "past the limit");
        let mut narrow = pointer();
        narrow.pitch = narrow.width * 4 - 1;
        assert_eq!(
            Pointer::decode(&narrow.encode()),
            None,
            "rows shorter than their pixels"
        );
        let mut long = pointer();
        long.height = POINTER_LIMIT;
        long.pitch = POINTER_LIMIT * 4 + 4;
        assert_eq!(
            Pointer::decode(&long.encode()),
            None,
            "rows past the section"
        );
        let mut words = pointer().encode();
        words[P_SHAPE] = u64::from(u32::MAX) + 1;
        assert_eq!(Pointer::decode(&words), None);
    }

    #[test]
    fn a_shape_is_read_back_by_its_number_alone() {
        let section = page();
        assert!(prepare(&section));
        let pixels: Vec<u8> = (0..33u8).collect();
        assert!(publish_shape(&section, 7, &pixels));
        let mut into = [0u8; 64];
        assert_eq!(read_shape(&section, 7, &mut into), Some(33));
        assert_eq!(&into[..33], &pixels[..], "an odd length's last word too");
        assert_eq!(
            read_shape(&section, 8, &mut into),
            None,
            "another shape's pixels"
        );
        let mut short = [0u8; 32];
        assert_eq!(read_shape(&section, 7, &mut short), None, "more than fits");
        let largest = std::vec![0xA5u8; SHAPE_BYTES];
        assert!(publish_shape(&section, 9, &largest));
        let mut back = std::vec![0u8; SHAPE_BYTES];
        assert_eq!(read_shape(&section, 9, &mut back), Some(SHAPE_BYTES));
        assert_eq!(back, largest);
        assert!(!publish_shape(
            &section,
            10,
            &std::vec![0u8; SHAPE_BYTES + 1]
        ));
        assert_eq!(
            read_shape(&section, 9, &mut back),
            Some(SHAPE_BYTES),
            "kept"
        );
        let sequence = section[W_SHAPE].load(Ordering::Relaxed);
        section[W_SHAPE].store(sequence + 1, Ordering::Relaxed);
        assert_eq!(
            read_shape(&section, 9, &mut back),
            None,
            "a write in progress"
        );
    }

    #[test]
    fn a_write_in_progress_is_not_taken() {
        let page = page();
        assert!(prepare(&page));
        publish(&page, &snapshot());
        // A writer stopped mid write leaves the sequence odd for good.
        page[W_SEQUENCE].store(3, Ordering::Relaxed);
        assert_eq!(read(&page), None);
        page[W_SEQUENCE].store(4, Ordering::Relaxed);
        assert_eq!(read(&page), Some(snapshot()));
    }

    #[test]
    fn a_page_that_cannot_be_is_refused() {
        let mut words = snapshot().encode();
        words[D_BUFFERS] = MAX_BUFFERS as u64 + 1;
        assert_eq!(Snapshot::decode(&words), None, "more buffers than named");
        let mut words = snapshot().encode();
        words[D_INDEX] = 3;
        assert_eq!(
            Snapshot::decode(&words),
            None,
            "a picture in no named buffer"
        );
        let mut words = snapshot().encode();
        words[D_INDEX] = 3;
        words[D_SEQUENCE] = 0;
        assert!(
            Snapshot::decode(&words).is_some(),
            "no picture yet names no buffer"
        );
        let mut words = snapshot().encode();
        words[D_FRAME] = u64::from(u32::MAX) + 1;
        assert_eq!(Snapshot::decode(&words), None);
    }

    #[test]
    fn a_picture_is_overtaken_once_the_system_can_write_its_buffer_again() {
        let mut latest = snapshot();
        latest.picture.sequence = 101;
        // Three buffers: the picture taken at 100 is written again only for
        // the third composition after it, which the driver acquires as 102.
        assert!(!latest.overtaken(3, 100));
        latest.picture.sequence = 102;
        assert!(latest.overtaken(3, 100));
        assert!(!latest.overtaken(3, 101));
        // Another generation's buffer is no longer written at all.
        assert!(!latest.overtaken(2, 100));
        // Nothing taken, nothing overtaken.
        assert!(!latest.overtaken(3, 0));
        // Fewer buffers seen errs early: with one or two, one newer picture.
        latest.buffers = 2;
        latest.picture.sequence = 101;
        assert!(latest.overtaken(3, 100));
        latest.buffers = 1;
        assert!(latest.overtaken(3, 100));
        latest.buffers = 0;
        assert!(latest.overtaken(3, 100));
    }

    #[test]
    fn a_buffer_is_named_only_where_the_page_names_one() {
        let taken = snapshot();
        assert_eq!(taken.buffer(0), Some(0x7b0));
        assert_eq!(taken.buffer(2), Some(0x7c0));
        assert_eq!(taken.buffer(3), None, "past the buffers seen");
        let mut gap = snapshot();
        gap.buffer[1] = 0;
        assert_eq!(gap.buffer(1), None, "a slot with no handle");
        assert_eq!(gap.buffer(u32::MAX), None);
    }
}

/// The sequence lock under the model checker: every interleaving of a writer
/// publishing twice and a reader reading, each word written from the same
/// number, so a reading taken from two writes shows two numbers.
#[cfg(all(test, loom))]
mod model {
    use super::{AtomicU64, Ordering, read_locked, write_locked};
    use loom::sync::Arc;
    use std::vec::Vec;

    const N: usize = 3;

    #[test]
    fn a_reading_is_one_write_whole_or_refused() {
        loom::model(|| {
            let lock = Arc::new(AtomicU64::new(0));
            let words: Arc<Vec<AtomicU64>> = Arc::new((0..N).map(|_| AtomicU64::new(0)).collect());
            let writer = {
                let (lock, words) = (Arc::clone(&lock), Arc::clone(&words));
                loom::thread::spawn(move || {
                    write_locked(&lock, &words, [1; N]);
                    write_locked(&lock, &words, [2; N]);
                })
            };
            let mut out = [u64::MAX; N];
            if read_locked(&lock, &words, &mut out) {
                assert!(
                    out.iter().all(|&v| v == out[0]),
                    "a reading mixed two writes: {out:?}"
                );
            }
            writer.join().unwrap();
            assert_eq!(lock.load(Ordering::Relaxed), 4);
            let mut last = [0; N];
            assert!(read_locked(&lock, &words, &mut last));
            assert_eq!(last, [2; N]);
        });
    }

    /// **The pointer's state never names pixels that are not there yet**: the
    /// writer publishes a shape's pixels under their sequence, then the state
    /// naming the shape under its own; a reader that reads the state and then
    /// the pixels of the shape it names finds them whole. The reader waits for
    /// the state to name the shape, so every interleaving reaches the check
    /// rather than the few that happen to read after the writer.
    #[test]
    fn a_state_never_names_pixels_not_there() {
        loom::model(|| {
            let (shape_lock, state_lock) =
                (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
            // The shape's number and one word of pixels; the state's number.
            let shape: Arc<Vec<AtomicU64>> = Arc::new((0..2).map(|_| AtomicU64::new(0)).collect());
            let state: Arc<Vec<AtomicU64>> = Arc::new((0..1).map(|_| AtomicU64::new(0)).collect());
            let writer = {
                let (shape_lock, state_lock) = (Arc::clone(&shape_lock), Arc::clone(&state_lock));
                let (shape, state) = (Arc::clone(&shape), Arc::clone(&state));
                loom::thread::spawn(move || {
                    write_locked(&shape_lock, &shape, [1, 11]);
                    write_locked(&state_lock, &state, [1]);
                })
            };
            let mut named = [0u64; 1];
            while !(read_locked(&state_lock, &state, &mut named) && named[0] == 1) {
                loom::thread::yield_now();
            }
            let mut pixels = [0u64; 2];
            assert!(
                read_locked(&shape_lock, &shape, &mut pixels),
                "the pixels were still being written when the state named them"
            );
            assert_eq!(pixels, [1, 11]);
            writer.join().unwrap();
        });
    }
}
