//! The control interface: version, plug, unplug, replug, list and reload,
//! and the request for a monitor's frame page.
//!
//! Every request and reply is a fixed sequence of little-endian words with its
//! own length first, read and written field by field. A request comes from any
//! interactive user (docs/11-drivers.md section 1.2), so the driver parses it
//! and never casts the buffer; a request whose length is not the one this
//! version defines is refused rather than read in part.

use crate::MAX_MONITORS;
use crate::modes::Mode;

/// The protocol a driver and a controller speak. A controller asks first and
/// refuses a driver whose version is not its own: the two ship together.
pub const VERSION: u32 = 2;

const FILE_DEVICE_UNKNOWN: u32 = 0x22;
const METHOD_BUFFERED: u32 = 0;
/// Read and write access, so a handle opened for neither cannot ask.
const FILE_READ_WRITE_ACCESS: u32 = 3;

const fn code(function: u32) -> u32 {
    (FILE_DEVICE_UNKNOWN << 16)
        | (FILE_READ_WRITE_ACCESS << 14)
        | ((0x800 + function) << 2)
        | METHOD_BUFFERED
}

/// Answers [`Version`].
pub const IOCTL_VERSION: u32 = code(1);
/// Takes [`Plug`], answers [`Plugged`].
pub const IOCTL_PLUG: u32 = code(2);
/// Takes [`Unplug`].
pub const IOCTL_UNPLUG: u32 = code(3);
/// Answers [`List`].
pub const IOCTL_LIST: u32 = code(4);
/// Takes [`Replug`], answers [`Plugged`].
pub const IOCTL_REPLUG: u32 = code(5);
/// Takes nothing: read the file again (section 1.4).
pub const IOCTL_RELOAD: u32 = code(6);
/// Takes [`Frames`], answers [`FrameChannel`] (section 1.6).
pub const IOCTL_FRAMES: u32 = code(7);

/// Why a request was not read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// The buffer is shorter than its length word says, or than the request.
    Short,
    /// The length word is not this version's.
    Length,
    /// A value outside what a monitor can be.
    Range,
    /// The reply does not fit the caller's buffer.
    Room,
}

fn get_u32(bytes: &[u8], at: usize) -> Option<u32> {
    let word = bytes.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes(word.try_into().ok()?))
}

fn put_u32(out: &mut [u8], at: usize, value: u32) -> Option<()> {
    out.get_mut(at..at.checked_add(4)?)?
        .copy_from_slice(&value.to_le_bytes());
    Some(())
}

fn put_u64(out: &mut [u8], at: usize, value: u64) -> Option<()> {
    out.get_mut(at..at.checked_add(8)?)?
        .copy_from_slice(&value.to_le_bytes());
    Some(())
}

fn get_u64(bytes: &[u8], at: usize) -> Option<u64> {
    let word = bytes.get(at..at.checked_add(8)?)?;
    Some(u64::from_le_bytes(word.try_into().ok()?))
}

/// The length word a request must carry, checked against the buffer too.
fn sized(bytes: &[u8], len: usize) -> Result<(), Refused> {
    let said = get_u32(bytes, 0).ok_or(Refused::Short)?;
    if usize::try_from(said).ok() != Some(len) {
        return Err(Refused::Length);
    }
    if bytes.len() < len {
        return Err(Refused::Short);
    }
    Ok(())
}

fn length(len: usize) -> u32 {
    u32::try_from(len).unwrap_or(u32::MAX)
}

fn get_mode(bytes: &[u8], at: usize) -> Result<Mode, Refused> {
    let mode = Mode {
        width: get_u32(bytes, at).ok_or(Refused::Short)?,
        height: get_u32(bytes, at + 4).ok_or(Refused::Short)?,
        refresh_hz: get_u32(bytes, at + 8).ok_or(Refused::Short)?,
    };
    if !mode.is_valid() {
        return Err(Refused::Range);
    }
    Ok(mode)
}

fn put_mode(out: &mut [u8], at: usize, mode: Mode) -> Option<()> {
    put_u32(out, at, mode.width)?;
    put_u32(out, at + 4, mode.height)?;
    put_u32(out, at + 8, mode.refresh_hz)
}

fn get_slot(bytes: &[u8], at: usize) -> Result<u8, Refused> {
    let slot = get_u32(bytes, at).ok_or(Refused::Short)?;
    usize::try_from(slot)
        .ok()
        .filter(|slot| *slot < MAX_MONITORS)
        .and_then(|slot| u8::try_from(slot).ok())
        .ok_or(Refused::Range)
}

/// The driver's answer to [`IOCTL_VERSION`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Version {
    pub version: u32,
    pub max_monitors: u32,
}

impl Version {
    pub const LEN: usize = 12;

    pub fn encode(&self, out: &mut [u8]) -> Result<usize, Refused> {
        put_u32(out, 0, length(Self::LEN)).ok_or(Refused::Room)?;
        put_u32(out, 4, self.version).ok_or(Refused::Room)?;
        put_u32(out, 8, self.max_monitors).ok_or(Refused::Room)?;
        Ok(Self::LEN)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, Refused> {
        sized(bytes, Self::LEN)?;
        Ok(Self {
            version: get_u32(bytes, 4).ok_or(Refused::Short)?,
            max_monitors: get_u32(bytes, 8).ok_or(Refused::Short)?,
        })
    }
}

/// How long a monitor lives (section 1.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lifetime {
    /// Owned by the handle that plugged it, and unplugged by that handle's
    /// end.
    Bound,
    /// Owned by no handle; it lives until removed or until the driver stops.
    Kept,
}

impl Lifetime {
    const fn word(self) -> u32 {
        match self {
            Self::Bound => 0,
            Self::Kept => 1,
        }
    }

    const fn from_word(word: u32) -> Option<Self> {
        match word {
            0 => Some(Self::Bound),
            1 => Some(Self::Kept),
            _ => None,
        }
    }
}

/// Plug a monitor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plug {
    /// The mode it arrives in, first and preferred in its list.
    pub mode: Mode,
    pub lifetime: Lifetime,
    /// A kept monitor the driver plugs again when it starts. The controller
    /// writes it into the file (section 1.4); the driver only reports it.
    pub restore: bool,
    /// The slot asked for, whose identity the system knows a monitor by; any
    /// free one when none.
    pub slot: Option<u8>,
}

impl Plug {
    pub const LEN: usize = 28;
    /// [`Plug::restore`], in the flags word.
    const RESTORE: u32 = 1;
    /// No slot asked for.
    const ANY_SLOT: u32 = u32::MAX;

    pub fn encode(&self, out: &mut [u8]) -> Result<usize, Refused> {
        put_u32(out, 0, length(Self::LEN)).ok_or(Refused::Room)?;
        put_mode(out, 4, self.mode).ok_or(Refused::Room)?;
        put_u32(out, 16, self.lifetime.word()).ok_or(Refused::Room)?;
        let flags = if self.restore { Self::RESTORE } else { 0 };
        put_u32(out, 20, flags).ok_or(Refused::Room)?;
        let slot = self.slot.map_or(Self::ANY_SLOT, u32::from);
        put_u32(out, 24, slot).ok_or(Refused::Room)?;
        Ok(Self::LEN)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, Refused> {
        sized(bytes, Self::LEN)?;
        let mode = get_mode(bytes, 4)?;
        let lifetime =
            Lifetime::from_word(get_u32(bytes, 16).ok_or(Refused::Short)?).ok_or(Refused::Range)?;
        let flags = get_u32(bytes, 20).ok_or(Refused::Short)?;
        if flags & !Self::RESTORE != 0 {
            return Err(Refused::Range);
        }
        let restore = flags & Self::RESTORE != 0;
        // A monitor bound to a handle has nothing to come back to.
        if restore && lifetime == Lifetime::Bound {
            return Err(Refused::Range);
        }
        let slot = match get_u32(bytes, 24).ok_or(Refused::Short)? {
            Self::ANY_SLOT => None,
            _ => Some(get_slot(bytes, 24)?),
        };
        Ok(Self {
            mode,
            lifetime,
            restore,
            slot,
        })
    }
}

/// The driver's answer to [`Plug`] and [`Replug`]: the slot, and where the
/// system put the monitor -- its adapter and its target -- so the controller
/// can find the display the system made of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plugged {
    pub slot: u32,
    pub os_target_id: u32,
    pub os_adapter_luid: u64,
}

impl Plugged {
    pub const LEN: usize = 20;

    pub fn encode(&self, out: &mut [u8]) -> Result<usize, Refused> {
        put_u32(out, 0, length(Self::LEN)).ok_or(Refused::Room)?;
        put_u32(out, 4, self.slot).ok_or(Refused::Room)?;
        put_u32(out, 8, self.os_target_id).ok_or(Refused::Room)?;
        put_u64(out, 12, self.os_adapter_luid).ok_or(Refused::Room)?;
        Ok(Self::LEN)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, Refused> {
        sized(bytes, Self::LEN)?;
        Ok(Self {
            slot: get_u32(bytes, 4).ok_or(Refused::Short)?,
            os_target_id: get_u32(bytes, 8).ok_or(Refused::Short)?,
            os_adapter_luid: get_u64(bytes, 12).ok_or(Refused::Short)?,
        })
    }
}

/// Unplug a monitor: one this handle plugged, or a kept one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unplug {
    pub slot: u8,
}

impl Unplug {
    pub const LEN: usize = 8;

    pub fn encode(&self, out: &mut [u8]) -> Result<usize, Refused> {
        put_u32(out, 0, length(Self::LEN)).ok_or(Refused::Room)?;
        put_u32(out, 4, u32::from(self.slot)).ok_or(Refused::Room)?;
        Ok(Self::LEN)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, Refused> {
        sized(bytes, Self::LEN)?;
        Ok(Self {
            slot: get_slot(bytes, 4)?,
        })
    }
}

/// Plug a monitor again with a mode its list lacks, first and preferred: the
/// same slot and identity, the mode added for this monitor's life (section
/// 1.5). One this handle plugged, or a kept one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Replug {
    pub slot: u8,
    pub mode: Mode,
}

impl Replug {
    pub const LEN: usize = 20;

    pub fn encode(&self, out: &mut [u8]) -> Result<usize, Refused> {
        put_u32(out, 0, length(Self::LEN)).ok_or(Refused::Room)?;
        put_u32(out, 4, u32::from(self.slot)).ok_or(Refused::Room)?;
        put_mode(out, 8, self.mode).ok_or(Refused::Room)?;
        Ok(Self::LEN)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, Refused> {
        sized(bytes, Self::LEN)?;
        Ok(Self {
            slot: get_slot(bytes, 4)?,
            mode: get_mode(bytes, 8)?,
        })
    }
}

/// Ask for a monitor's frame page (section 1.6). **Answered to any caller the
/// device admits**: the answer is handle values in the driver's own process,
/// which only a process allowed to take handles from it can use (section 1.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frames {
    pub slot: u8,
}

impl Frames {
    pub const LEN: usize = 8;

    pub fn encode(&self, out: &mut [u8]) -> Result<usize, Refused> {
        put_u32(out, 0, length(Self::LEN)).ok_or(Refused::Room)?;
        put_u32(out, 4, u32::from(self.slot)).ok_or(Refused::Room)?;
        Ok(Self::LEN)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, Refused> {
        sized(bytes, Self::LEN)?;
        Ok(Self {
            slot: get_slot(bytes, 4)?,
        })
    }
}

/// The driver's answer to [`Frames`]: the page's section, a handle value in
/// the driver's process, and which process that is -- its identifier and the
/// time it started, so a reader never takes handles from another that came
/// to carry the same identifier -- with the frame channel's own version
/// ([`crate::frames::VERSION`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameChannel {
    pub version: u32,
    pub process: u32,
    pub size: u32,
    /// When the process started, in the system's file-time units.
    pub started: u64,
    pub section: u64,
}

impl FrameChannel {
    pub const LEN: usize = 32;

    pub fn encode(&self, out: &mut [u8]) -> Result<usize, Refused> {
        put_u32(out, 0, length(Self::LEN)).ok_or(Refused::Room)?;
        put_u32(out, 4, self.version).ok_or(Refused::Room)?;
        put_u32(out, 8, self.process).ok_or(Refused::Room)?;
        put_u32(out, 12, self.size).ok_or(Refused::Room)?;
        put_u64(out, 16, self.started).ok_or(Refused::Room)?;
        put_u64(out, 24, self.section).ok_or(Refused::Room)?;
        Ok(Self::LEN)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, Refused> {
        sized(bytes, Self::LEN)?;
        Ok(Self {
            version: get_u32(bytes, 4).ok_or(Refused::Short)?,
            process: get_u32(bytes, 8).ok_or(Refused::Short)?,
            size: get_u32(bytes, 12).ok_or(Refused::Short)?,
            started: get_u64(bytes, 16).ok_or(Refused::Short)?,
            section: get_u64(bytes, 24).ok_or(Refused::Short)?,
        })
    }
}

/// One slot as [`IOCTL_LIST`] reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Entry {
    pub slot: u32,
    /// [`Entry::IN_USE`], [`Entry::OWNED`], [`Entry::SWAPCHAIN`],
    /// [`Entry::KEPT`], [`Entry::RESTORE`].
    pub flags: u32,
    /// The mode the system committed, or the plug's before it has.
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
    pub os_target_id: u32,
    /// The adapter the system made the monitor's target on, with
    /// [`Entry::os_target_id`] where a controller finds its display.
    pub os_adapter_luid: u64,
    /// The GPU the system renders the monitor's desktop on, from its last
    /// swapchain; zero before one.
    pub render_luid: u64,
    /// Pictures the system handed over, from every swapchain the monitor had.
    pub frames: u64,
}

impl Entry {
    pub const IN_USE: u32 = 1;
    /// Plugged through the handle asking.
    pub const OWNED: u32 = 2;
    /// A swapchain is assigned now.
    pub const SWAPCHAIN: u32 = 4;
    /// Owned by no handle.
    pub const KEPT: u32 = 8;
    /// Plugged again when the driver starts.
    pub const RESTORE: u32 = 16;
    pub const LEN: usize = 48;

    fn encode(&self, out: &mut [u8]) -> Option<()> {
        put_u32(out, 0, self.slot)?;
        put_u32(out, 4, self.flags)?;
        put_u32(out, 8, self.width)?;
        put_u32(out, 12, self.height)?;
        put_u32(out, 16, self.refresh_hz)?;
        put_u32(out, 20, self.os_target_id)?;
        put_u64(out, 24, self.os_adapter_luid)?;
        put_u64(out, 32, self.render_luid)?;
        put_u64(out, 40, self.frames)
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        Some(Self {
            slot: get_u32(bytes, 0)?,
            flags: get_u32(bytes, 4)?,
            width: get_u32(bytes, 8)?,
            height: get_u32(bytes, 12)?,
            refresh_hz: get_u32(bytes, 16)?,
            os_target_id: get_u32(bytes, 20)?,
            os_adapter_luid: get_u64(bytes, 24)?,
            render_luid: get_u64(bytes, 32)?,
            frames: get_u64(bytes, 40)?,
        })
    }
}

/// The driver's answer to [`IOCTL_LIST`]: every slot, in use or not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct List {
    pub entries: [Entry; MAX_MONITORS],
}

impl List {
    pub const LEN: usize = 8 + MAX_MONITORS * Entry::LEN;

    pub fn encode(&self, out: &mut [u8]) -> Result<usize, Refused> {
        if out.len() < Self::LEN {
            return Err(Refused::Room);
        }
        put_u32(out, 0, length(Self::LEN)).ok_or(Refused::Room)?;
        put_u32(out, 4, length(MAX_MONITORS)).ok_or(Refused::Room)?;
        for (entry, at) in self.entries.iter().zip((8..).step_by(Entry::LEN)) {
            let room = out.get_mut(at..).ok_or(Refused::Room)?;
            entry.encode(room).ok_or(Refused::Room)?;
        }
        Ok(Self::LEN)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, Refused> {
        sized(bytes, Self::LEN)?;
        let count = get_u32(bytes, 4).ok_or(Refused::Short)?;
        if usize::try_from(count).ok() != Some(MAX_MONITORS) {
            return Err(Refused::Length);
        }
        let mut list = Self::default();
        for (entry, at) in list.entries.iter_mut().zip((8..).step_by(Entry::LEN)) {
            let bytes = bytes.get(at..).ok_or(Refused::Short)?;
            *entry = Entry::decode(bytes).ok_or(Refused::Short)?;
        }
        Ok(list)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_codes_are_the_systems_buffered_read_write_form() {
        // CTL_CODE(FILE_DEVICE_UNKNOWN, 0x801.., METHOD_BUFFERED,
        // FILE_READ_ACCESS | FILE_WRITE_ACCESS), as the system's macro makes it.
        assert_eq!(IOCTL_VERSION, 0x0022_E004);
        assert_eq!(IOCTL_PLUG, 0x0022_E008);
        assert_eq!(IOCTL_UNPLUG, 0x0022_E00C);
        assert_eq!(IOCTL_LIST, 0x0022_E010);
        assert_eq!(IOCTL_REPLUG, 0x0022_E014);
        assert_eq!(IOCTL_RELOAD, 0x0022_E018);
        assert_eq!(IOCTL_FRAMES, 0x0022_E01C);
    }

    #[test]
    fn a_request_for_frames_and_its_answer_round_trip() {
        let mut buf = [0u8; FrameChannel::LEN];
        for slot in [0, 7, 15] {
            let asked = Frames { slot };
            assert_eq!(asked.encode(&mut buf), Ok(Frames::LEN));
            assert_eq!(Frames::decode(buf.get(..Frames::LEN).unwrap()), Ok(asked));
        }
        // Past the last slot.
        let mut past = [0u8; Frames::LEN];
        Frames { slot: 0 }.encode(&mut past).unwrap();
        past[4] = 16;
        assert_eq!(Frames::decode(&past), Err(Refused::Range));

        let answer = FrameChannel {
            version: crate::frames::VERSION,
            process: 10_484,
            size: 4096,
            started: 0x01dc_3e9a_1234_5678,
            section: 0x650,
        };
        assert_eq!(answer.encode(&mut buf), Ok(FrameChannel::LEN));
        assert_eq!(FrameChannel::decode(&buf), Ok(answer));
        // Every field lands where the other side reads it: the time is not
        // the section's word, nor the section the time's.
        assert_eq!(buf[16..24], 0x01dc_3e9a_1234_5678u64.to_le_bytes());
        assert_eq!(buf[24..32], 0x650u64.to_le_bytes());
        assert_eq!(
            FrameChannel::decode(&buf[..FrameChannel::LEN - 1]),
            Err(Refused::Short)
        );
        assert_eq!(answer.encode(&mut buf[..8]), Err(Refused::Room));
    }

    fn plug(lifetime: Lifetime, restore: bool, slot: Option<u8>) -> Plug {
        Plug {
            mode: Mode::new(2560, 1440, 120),
            lifetime,
            restore,
            slot,
        }
    }

    #[test]
    fn a_plug_round_trips_and_its_mode_is_checked() {
        let mut buf = [0u8; Plug::LEN];
        for asked in [
            plug(Lifetime::Bound, false, None),
            plug(Lifetime::Kept, false, Some(0)),
            plug(Lifetime::Kept, true, Some(15)),
        ] {
            assert_eq!(asked.encode(&mut buf), Ok(Plug::LEN));
            assert_eq!(Plug::decode(&buf), Ok(asked));
        }

        let tiny = Plug {
            mode: Mode::new(16, 16, 60),
            ..plug(Lifetime::Bound, false, None)
        };
        tiny.encode(&mut buf).expect("room");
        assert_eq!(Plug::decode(&buf), Err(Refused::Range));
    }

    #[test]
    fn a_plug_refuses_what_no_monitor_can_be() {
        let mut buf = [0u8; Plug::LEN];
        let at = |buf: &mut [u8; Plug::LEN], offset: usize, word: u32| {
            buf[offset..offset + 4].copy_from_slice(&word.to_le_bytes());
        };
        plug(Lifetime::Kept, true, Some(3))
            .encode(&mut buf)
            .expect("room");
        let good = buf;

        at(&mut buf, 16, 2);
        assert_eq!(Plug::decode(&buf), Err(Refused::Range), "a third lifetime");
        buf = good;
        at(&mut buf, 20, 3);
        assert_eq!(
            Plug::decode(&buf),
            Err(Refused::Range),
            "a flag not defined"
        );
        buf = good;
        at(&mut buf, 24, 16);
        assert_eq!(
            Plug::decode(&buf),
            Err(Refused::Range),
            "a slot past the last"
        );
        buf = good;
        at(&mut buf, 16, 0);
        assert_eq!(
            Plug::decode(&buf),
            Err(Refused::Range),
            "a bound monitor restored"
        );
        buf = good;
        at(&mut buf, 24, u32::MAX);
        assert_eq!(
            Plug::decode(&buf).map(|p| p.slot),
            Ok(None),
            "any free slot"
        );
    }

    #[test]
    fn a_request_of_another_length_is_refused_not_read_in_part() {
        let mut buf = [0u8; Plug::LEN + 4];
        plug(Lifetime::Bound, false, None)
            .encode(&mut buf)
            .expect("room");
        assert!(Plug::decode(&buf).is_ok(), "trailing room is the caller's");
        buf[0] = 16;
        assert_eq!(Plug::decode(&buf), Err(Refused::Length), "version 1's plug");
        assert_eq!(Plug::decode(&buf[..10]), Err(Refused::Length));
        let short = [28u8, 0, 0, 0, 0, 0];
        assert_eq!(Plug::decode(&short), Err(Refused::Short));
        assert_eq!(Plug::decode(&[]), Err(Refused::Short));
    }

    #[test]
    fn an_unplug_or_replug_past_the_last_slot_is_refused() {
        let mut buf = [0u8; Unplug::LEN];
        Unplug { slot: 15 }.encode(&mut buf).expect("room");
        assert_eq!(Unplug::decode(&buf), Ok(Unplug { slot: 15 }));
        Unplug { slot: 16 }.encode(&mut buf).expect("room");
        assert_eq!(Unplug::decode(&buf), Err(Refused::Range));

        let mut buf = [0u8; Replug::LEN];
        let replug = Replug {
            slot: 15,
            mode: Mode::new(3000, 2000, 120),
        };
        assert_eq!(replug.encode(&mut buf), Ok(Replug::LEN));
        assert_eq!(Replug::decode(&buf), Ok(replug));
        Replug { slot: 16, ..replug }
            .encode(&mut buf)
            .expect("room");
        assert_eq!(Replug::decode(&buf), Err(Refused::Range));
        Replug {
            mode: Mode::new(1920, 1080, 500),
            ..replug
        }
        .encode(&mut buf)
        .expect("room");
        assert_eq!(
            Replug::decode(&buf),
            Err(Refused::Range),
            "a mode no monitor is"
        );
    }

    #[test]
    fn replies_round_trip_and_refuse_a_short_buffer() {
        let version = Version {
            version: VERSION,
            max_monitors: 16,
        };
        let mut buf = [0u8; 64];
        assert_eq!(version.encode(&mut buf), Ok(Version::LEN));
        assert_eq!(Version::decode(&buf), Ok(version));
        assert_eq!(version.encode(&mut buf[..8]), Err(Refused::Room));

        let plugged = Plugged {
            slot: 3,
            os_target_id: 0x1234,
            os_adapter_luid: 0x0000_0001_0002_0003,
        };
        assert_eq!(plugged.encode(&mut buf), Ok(Plugged::LEN));
        assert_eq!(Plugged::decode(&buf), Ok(plugged));

        let mut list = List::default();
        list.entries[2] = Entry {
            slot: 2,
            flags: Entry::IN_USE | Entry::KEPT | Entry::RESTORE,
            width: 1920,
            height: 1080,
            refresh_hz: 60,
            os_target_id: 7,
            os_adapter_luid: 0x0000_0001_1a2b_3c4d,
            render_luid: 0xdead_beef,
            frames: 1 << 40,
        };
        let mut big = [0u8; List::LEN];
        assert_eq!(list.encode(&mut big), Ok(List::LEN));
        assert_eq!(List::decode(&big), Ok(list));
        let mut fewer = big;
        fewer[4] = 15;
        assert_eq!(
            List::decode(&fewer),
            Err(Refused::Length),
            "a list of another count"
        );
        assert_eq!(list.encode(&mut big[..List::LEN - 1]), Err(Refused::Room));
    }
}
