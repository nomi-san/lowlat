//! The control interface: the version, the device a handle makes, and what
//! the system writes to that device, read back by a request the host keeps
//! outstanding.
//!
//! Every request and reply is a fixed sequence of little-endian words with its
//! own length first, read and written field by field: the driver parses each
//! and never casts a buffer, and a request whose length is not this version's
//! is refused rather than read in part. An input report travels as a write of
//! its own bytes, checked by [`Kind::accepts`].

use crate::Kind;

/// The protocol a driver and its host speak. The host asks first and refuses
/// a driver whose version is not its own: the two ship together.
pub const VERSION: u32 = 1;

/// The largest feature report a device answers with, and the largest report
/// the system writes that the host reads back, identifier byte first.
pub const FEATURE_MAX: usize = 64;
/// How many feature reports one device answers with.
pub const MAX_FEATURES: usize = 4;

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
/// Takes [`Create`]: the handle's one device, which goes with the handle.
pub const IOCTL_CREATE: u32 = code(2);
/// Answers [`Written`] once the system writes to the handle's device; kept
/// outstanding by the host.
pub const IOCTL_WRITTEN: u32 = code(3);

/// Why a request was not read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// The buffer is shorter than its length word says, or than the request.
    Short,
    /// The length word is not this version's.
    Length,
    /// A value outside what the request can carry.
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

/// The driver's answer to [`IOCTL_VERSION`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Version {
    pub version: u32,
    pub max_devices: u32,
}

impl Version {
    pub const LEN: usize = 12;

    pub fn encode(&self, out: &mut [u8]) -> Result<usize, Refused> {
        put_u32(out, 0, length(Self::LEN)).ok_or(Refused::Room)?;
        put_u32(out, 4, self.version).ok_or(Refused::Room)?;
        put_u32(out, 8, self.max_devices).ok_or(Refused::Room)?;
        Ok(Self::LEN)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, Refused> {
        sized(bytes, Self::LEN)?;
        Ok(Self {
            version: get_u32(bytes, 4).ok_or(Refused::Short)?,
            max_devices: get_u32(bytes, 8).ok_or(Refused::Short)?,
        })
    }
}

/// One feature report, identifier byte first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Feature {
    len: u8,
    bytes: [u8; FEATURE_MAX],
}

impl Feature {
    /// A report of its identifier and at least one byte, no longer than
    /// [`FEATURE_MAX`]; identifier 0 names no report.
    pub fn new(report: &[u8]) -> Option<Self> {
        if report.len() < 2 || report.first() == Some(&0) {
            return None;
        }
        let len = u8::try_from(report.len())
            .ok()
            .filter(|len| usize::from(*len) <= FEATURE_MAX)?;
        let mut bytes = [0u8; FEATURE_MAX];
        bytes.get_mut(..report.len())?.copy_from_slice(report);
        Some(Self { len, bytes })
    }

    pub fn id(&self) -> u8 {
        self.bytes.first().copied().unwrap_or(0)
    }

    pub fn report(&self) -> &[u8] {
        self.bytes.get(..usize::from(self.len)).unwrap_or(&[])
    }
}

/// The feature reports a device answers with, one per identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Features {
    count: usize,
    entries: [Option<Feature>; MAX_FEATURES],
}

impl Default for Features {
    fn default() -> Self {
        Self::new()
    }
}

impl Features {
    pub const fn new() -> Self {
        Self {
            count: 0,
            entries: [None; MAX_FEATURES],
        }
    }

    /// The kind's own answers ([`Kind::features`]).
    pub fn of(kind: Kind) -> Self {
        let mut features = Self::new();
        for report in kind.features() {
            // A kind's own answers are whole reports and fewer than the
            // table holds, which the tests check.
            let _ = features.set(report);
        }
        features
    }

    /// Answer `report` for its identifier from now on, replacing any answer
    /// the identifier had.
    pub fn set(&mut self, report: &[u8]) -> Result<(), Refused> {
        let feature = Feature::new(report).ok_or(Refused::Range)?;
        if let Some(slot) = self
            .entries
            .iter_mut()
            .flatten()
            .find(|held| held.id() == feature.id())
        {
            *slot = feature;
            return Ok(());
        }
        let slot = self.entries.get_mut(self.count).ok_or(Refused::Room)?;
        *slot = Some(feature);
        self.count += 1;
        Ok(())
    }

    /// The answer for identifier `id`, none where nobody defined one.
    pub fn get(&self, id: u8) -> Option<&[u8]> {
        self.iter().find(|report| report.first() == Some(&id))
    }

    pub fn iter(&self) -> impl Iterator<Item = &[u8]> {
        self.entries.iter().flatten().map(Feature::report)
    }

    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }
}

/// The device a handle makes: its kind, its slot, and the answers it gives
/// beside the kind's own -- a pad's peer's calibration and firmware, and the
/// pairing report the host makes up for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Create {
    pub kind: Kind,
    pub slot: u8,
    pub features: Features,
}

impl Create {
    /// Each feature report's room: its length word, then the report.
    const ENTRY: usize = 4 + FEATURE_MAX;
    pub const LEN: usize = 16 + MAX_FEATURES * Self::ENTRY;

    pub fn encode(&self, out: &mut [u8]) -> Result<usize, Refused> {
        let room = out.get_mut(..Self::LEN).ok_or(Refused::Room)?;
        room.fill(0);
        put_u32(room, 0, length(Self::LEN)).ok_or(Refused::Room)?;
        put_u32(room, 4, u32::from(self.kind as u8)).ok_or(Refused::Room)?;
        put_u32(room, 8, u32::from(self.slot)).ok_or(Refused::Room)?;
        put_u32(room, 12, length(self.features.len())).ok_or(Refused::Room)?;
        for (i, report) in self.features.iter().enumerate() {
            let at = 16 + i * Self::ENTRY;
            put_u32(room, at, length(report.len())).ok_or(Refused::Room)?;
            room.get_mut(at + 4..at + 4 + report.len())
                .ok_or(Refused::Room)?
                .copy_from_slice(report);
        }
        Ok(Self::LEN)
    }

    /// Refused for a kind nobody defined, a slot the kind does not have, more
    /// reports than the table holds, a report outside its room, and two
    /// reports with one identifier.
    pub fn decode(bytes: &[u8]) -> Result<Self, Refused> {
        sized(bytes, Self::LEN)?;
        let kind = get_u32(bytes, 4)
            .and_then(Kind::from_u32)
            .ok_or(Refused::Range)?;
        let slot = get_u32(bytes, 8)
            .filter(|slot| *slot < kind.slots())
            .and_then(|slot| u8::try_from(slot).ok())
            .ok_or(Refused::Range)?;
        let count = get_u32(bytes, 12)
            .and_then(|count| usize::try_from(count).ok())
            .filter(|count| *count <= MAX_FEATURES)
            .ok_or(Refused::Range)?;
        let mut features = Features::new();
        for i in 0..count {
            let at = 16 + i * Self::ENTRY;
            let len = get_u32(bytes, at)
                .and_then(|len| usize::try_from(len).ok())
                .filter(|len| *len <= FEATURE_MAX)
                .ok_or(Refused::Range)?;
            let report = bytes.get(at + 4..at + 4 + len).ok_or(Refused::Short)?;
            if report.first().is_some_and(|id| features.get(*id).is_some()) {
                return Err(Refused::Range);
            }
            features.set(report)?;
        }
        Ok(Self {
            kind,
            slot,
            features,
        })
    }
}

/// What the system wrote to a device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum WrittenKind {
    /// An output report: motors, lights, effects.
    Output = 1,
    /// A feature report written to the device.
    Feature = 2,
}

/// The reply to [`IOCTL_WRITTEN`]: one write the system made to the device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Written {
    pub kind: WrittenKind,
    len: u8,
    report: [u8; FEATURE_MAX],
}

impl Written {
    pub const LEN: usize = 12 + FEATURE_MAX;

    /// None for a report longer than [`FEATURE_MAX`], which the host could
    /// not read whole.
    pub fn new(kind: WrittenKind, report: &[u8]) -> Option<Self> {
        let len = u8::try_from(report.len())
            .ok()
            .filter(|len| usize::from(*len) <= FEATURE_MAX)?;
        let mut bytes = [0u8; FEATURE_MAX];
        bytes.get_mut(..report.len())?.copy_from_slice(report);
        Some(Self {
            kind,
            len,
            report: bytes,
        })
    }

    pub fn report(&self) -> &[u8] {
        self.report.get(..usize::from(self.len)).unwrap_or(&[])
    }

    pub fn encode(&self, out: &mut [u8]) -> Result<usize, Refused> {
        let room = out.get_mut(..Self::LEN).ok_or(Refused::Room)?;
        room.fill(0);
        put_u32(room, 0, length(Self::LEN)).ok_or(Refused::Room)?;
        put_u32(room, 4, self.kind as u32).ok_or(Refused::Room)?;
        put_u32(room, 8, u32::from(self.len)).ok_or(Refused::Room)?;
        room.get_mut(12..12 + usize::from(self.len))
            .ok_or(Refused::Room)?
            .copy_from_slice(self.report());
        Ok(Self::LEN)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, Refused> {
        sized(bytes, Self::LEN)?;
        let kind = match get_u32(bytes, 4) {
            Some(1) => WrittenKind::Output,
            Some(2) => WrittenKind::Feature,
            _ => return Err(Refused::Range),
        };
        let len = get_u32(bytes, 8)
            .and_then(|len| usize::try_from(len).ok())
            .filter(|len| *len <= FEATURE_MAX)
            .ok_or(Refused::Range)?;
        let report = bytes.get(12..12 + len).ok_or(Refused::Short)?;
        Self::new(kind, report).ok_or(Refused::Range)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{mouse, pad};

    #[test]
    fn the_codes_are_buffered_unknown_device_requests_needing_both_accesses() {
        assert_eq!(IOCTL_VERSION, 0x0022_E004);
        assert_eq!(IOCTL_CREATE, 0x0022_E008);
        assert_eq!(IOCTL_WRITTEN, 0x0022_E00C);
    }

    #[test]
    fn a_version_round_trips() {
        let version = Version {
            version: VERSION,
            max_devices: 96,
        };
        let mut out = [0u8; Version::LEN];
        assert_eq!(version.encode(&mut out), Ok(Version::LEN));
        assert_eq!(Version::decode(&out), Ok(version));
        assert_eq!(version.encode(&mut out[..11]), Err(Refused::Room));
    }

    #[test]
    fn a_create_round_trips_with_its_reports() {
        let mut features = Features::new();
        features.set(pad::DS5_FEATURES[0]).unwrap();
        features.set(&[0x09, 1, 2, 3]).unwrap();
        let create = Create {
            kind: Kind::DualSense,
            slot: 63,
            features,
        };
        let mut out = [0xAAu8; Create::LEN];
        assert_eq!(create.encode(&mut out), Ok(Create::LEN));
        assert_eq!(Create::decode(&out), Ok(create));
        assert_eq!(
            Create::decode(&out).unwrap().features.get(0x09),
            Some(&[0x09, 1, 2, 3][..])
        );
    }

    #[test]
    fn a_create_outside_what_it_can_carry_is_refused() {
        let base = Create {
            kind: Kind::Keyboard,
            slot: 0,
            features: Features::new(),
        };
        let mut good = [0u8; Create::LEN];
        base.encode(&mut good).unwrap();
        assert_eq!(
            Create::decode(&good[..Create::LEN - 1]),
            Err(Refused::Short)
        );
        let mut wrong = good;
        wrong[0] = 0;
        assert_eq!(Create::decode(&wrong), Err(Refused::Length));
        let edit = |at: usize, value: u32| {
            let mut bytes = good;
            bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
            Create::decode(&bytes)
        };
        assert_eq!(edit(4, 0), Err(Refused::Range), "no kind");
        assert_eq!(edit(4, 5), Err(Refused::Range), "an unknown kind");
        assert_eq!(
            edit(8, 16),
            Err(Refused::Range),
            "a keyboard past the guests' slots"
        );
        assert_eq!(
            edit(12, 5),
            Err(Refused::Range),
            "more reports than the table holds"
        );
        let mut long = good;
        long[12..16].copy_from_slice(&1u32.to_le_bytes());
        long[16..20].copy_from_slice(&65u32.to_le_bytes());
        assert_eq!(
            Create::decode(&long),
            Err(Refused::Range),
            "a report past its room"
        );
        let mut zero = long;
        zero[16..20].copy_from_slice(&2u32.to_le_bytes());
        assert_eq!(Create::decode(&zero), Err(Refused::Range), "identifier 0");
        let mut twice = good;
        twice[12..16].copy_from_slice(&2u32.to_le_bytes());
        for i in 0..2 {
            let at = 16 + i * Create::ENTRY;
            twice[at..at + 4].copy_from_slice(&2u32.to_le_bytes());
            twice[at + 4..at + 6].copy_from_slice(&[0x05, u8::try_from(i).unwrap()]);
        }
        assert_eq!(
            Create::decode(&twice),
            Err(Refused::Range),
            "one identifier twice"
        );
        let mut pad = good;
        pad[4..8].copy_from_slice(&3u32.to_le_bytes());
        pad[8..12].copy_from_slice(&63u32.to_le_bytes());
        assert!(
            Create::decode(&pad).is_ok(),
            "a pad has a guest's pads' slots"
        );
    }

    #[test]
    fn a_kinds_own_answers_fit_its_table_and_a_set_replaces_by_identifier() {
        for kind in Kind::ALL {
            let features = Features::of(kind);
            assert_eq!(features.len(), kind.features().len(), "{kind:?}");
            assert!(
                features.len() < MAX_FEATURES,
                "{kind:?} leaves room for the host's"
            );
        }
        let mut mouse = Features::of(Kind::Mouse);
        assert_eq!(
            mouse.get(mouse::MULTIPLIER_REPORT_ID),
            Some(&mouse::MULTIPLIER_DEFAULT[..])
        );
        mouse.set(&[mouse::MULTIPLIER_REPORT_ID, 0b0101]).unwrap();
        assert_eq!(mouse.len(), 1, "replaced, not added");
        assert_eq!(mouse.get(1), Some(&[1, 0b0101][..]));
        assert_eq!(mouse.get(2), None, "an identifier nobody defined");
        let mut full = Features::new();
        for id in 1..=4 {
            full.set(&[id, 0]).unwrap();
        }
        assert_eq!(full.set(&[5, 0]), Err(Refused::Room));
        assert_eq!(full.set(&[0, 0]), Err(Refused::Range));
        assert_eq!(full.set(&[1]), Err(Refused::Range), "an identifier alone");
    }

    #[test]
    fn a_written_report_round_trips_and_a_longer_one_is_none() {
        let written = Written::new(WrittenKind::Output, &[0x05, 0xFF, 0x00, 0x7F]).unwrap();
        let mut out = [0u8; Written::LEN];
        assert_eq!(written.encode(&mut out), Ok(Written::LEN));
        assert_eq!(Written::decode(&out), Ok(written));
        assert_eq!(
            Written::decode(&out).unwrap().report(),
            &[0x05, 0xFF, 0x00, 0x7F]
        );
        assert!(Written::new(WrittenKind::Feature, &[0u8; FEATURE_MAX]).is_some());
        assert!(Written::new(WrittenKind::Feature, &[0u8; FEATURE_MAX + 1]).is_none());
        let mut unknown = out;
        unknown[4..8].copy_from_slice(&3u32.to_le_bytes());
        assert_eq!(Written::decode(&unknown), Err(Refused::Range));
    }
}
