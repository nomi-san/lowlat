//! A monitor's modes, fixed when it arrives (docs/11-drivers.md section 1.5).

use core::fmt;

/// A size and a whole refresh rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Mode {
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
}

impl Mode {
    pub const fn new(width: u32, height: u32, refresh_hz: u32) -> Self {
        Self {
            width,
            height,
            refresh_hz,
        }
    }

    /// Within what a monitor of this display may be: no smaller than the
    /// system's own floor for a desktop, no larger than the largest picture an
    /// encoder here takes, and a rate a compositor can keep.
    pub const fn is_valid(&self) -> bool {
        self.width >= 640
            && self.width <= 7680
            && self.height >= 480
            && self.height <= 4320
            && self.refresh_hz >= 24
            && self.refresh_hz <= 480
    }

    /// `WxH@Hz`, the form the file and the command line write. Whether the
    /// mode is one a monitor may be is the caller's question.
    pub fn parse(text: &str) -> Option<Self> {
        let (size, rate) = text.split_once('@')?;
        let (width, height) = size.split_once('x')?;
        Some(Self::new(number(width)?, number(height)?, number(rate)?))
    }
}

/// Decimal digits alone: no sign, no space.
fn number(text: &str) -> Option<u32> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}x{}@{}", self.width, self.height, self.refresh_hz)
    }
}

/// What a monitor is plugged at when nothing is asked.
pub const DEFAULT: Mode = Mode::new(1920, 1080, 60);

/// The common sizes every monitor lists.
pub const SIZES: [(u32, u32); 12] = [
    (3840, 2160),
    (3440, 1440),
    (2560, 1600),
    (2560, 1440),
    (2560, 1080),
    (1920, 1200),
    (1920, 1080),
    (1600, 900),
    (1440, 900),
    (1366, 768),
    (1280, 800),
    (1280, 720),
];

/// The rates every listed size comes at. **30 among them**, so a stream asked
/// for 30 a second puts the host's own display there without plugging it
/// again, as one unlisted did.
pub const RATES: [u32; 5] = [30, 60, 120, 144, 165];

/// The custom modes the file may name (section 1.4), each listed by every
/// monitor plugged after.
pub const MAX_CUSTOM: usize = 16;

/// The most modes a monitor lists. **The system takes a monitor listing a
/// hundred and refuses one listing a hundred and one**, after asking for all of
/// them, measured on Windows 10; ninety keeps a margin for a build not
/// measured. The plug's mode, its size at the listed rates, every custom mode
/// and every listed size at every rate fit.
pub const MAX_MODES: usize = 90;

// Within what the system takes (above).
const _: () = assert!(MAX_MODES <= 100);

/// The modes a monitor arrives with: the plug's own first and preferred, the
/// plug's size at every listed rate, the custom modes, then every listed size
/// at every listed rate, each once.
#[derive(Debug, Clone, Copy)]
pub struct ModeList {
    modes: [Mode; MAX_MODES],
    len: usize,
}

impl ModeList {
    pub fn for_plug(preferred: Mode, custom: &[Mode]) -> Self {
        let mut list = Self {
            modes: [Mode::default(); MAX_MODES],
            len: 0,
        };
        list.push(preferred);
        for rate in RATES {
            list.push(Mode::new(preferred.width, preferred.height, rate));
        }
        for mode in custom.iter().take(MAX_CUSTOM) {
            if mode.is_valid() {
                list.push(*mode);
            }
        }
        for (width, height) in SIZES {
            for rate in RATES {
                list.push(Mode::new(width, height, rate));
            }
        }
        list
    }

    fn push(&mut self, mode: Mode) {
        if self.as_slice().contains(&mode) {
            return;
        }
        if let Some(free) = self.modes.get_mut(self.len) {
            *free = mode;
            self.len += 1;
        }
    }

    pub fn as_slice(&self) -> &[Mode] {
        self.modes.get(..self.len).unwrap_or(&[])
    }

    /// The plug's own mode, which the monitor prefers.
    pub fn preferred(&self) -> Mode {
        self.as_slice().first().copied().unwrap_or(DEFAULT)
    }

    /// Whether the monitor lists this mode, so the system can be asked for it
    /// without a replug.
    pub fn lists(&self, mode: Mode) -> bool {
        self.as_slice().contains(&mode)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_plugs_mode_comes_first_and_every_mode_once() {
        let asked = Mode::new(2880, 1800, 90);
        let custom = [Mode::new(1920, 1080, 75), Mode::new(2880, 1800, 90)];
        let list = ModeList::for_plug(asked, &custom);
        let modes = list.as_slice();
        assert_eq!(modes.first(), Some(&asked));
        assert_eq!(list.preferred(), asked);
        for (i, a) in modes.iter().enumerate() {
            for b in modes.iter().skip(i + 1) {
                assert_ne!(a, b, "{a:?} listed twice");
            }
        }
        // The plug's size at the listed rates, then the custom ones, then
        // the listed sizes.
        assert!(modes.contains(&Mode::new(2880, 1800, 165)));
        assert!(list.lists(Mode::new(1920, 1080, 75)));
        assert!(list.lists(DEFAULT));
        assert!(!list.lists(Mode::new(1920, 1080, 75 + 1)));
        assert!(modes.len() <= MAX_MODES);
    }

    #[test]
    fn custom_modes_come_before_the_listed_sizes() {
        let custom = [Mode::new(3000, 2000, 120)];
        let list = ModeList::for_plug(DEFAULT, &custom);
        let at = |mode: Mode| list.as_slice().iter().position(|m| *m == mode);
        let custom_at = at(Mode::new(3000, 2000, 120)).expect("listed");
        let first_size = at(Mode::new(3840, 2160, 60)).expect("listed");
        assert!(custom_at < first_size, "{custom_at} before {first_size}");
        // An invalid custom mode is not listed.
        let list = ModeList::for_plug(DEFAULT, &[Mode::new(100, 100, 60)]);
        assert!(!list.lists(Mode::new(100, 100, 60)));
    }

    #[test]
    fn a_listed_mode_is_not_listed_again() {
        let list = ModeList::for_plug(DEFAULT, &[]);
        assert_eq!(list.as_slice().len(), SIZES.len() * RATES.len());
        // A size and a rate no listed entry has.
        let fresh = ModeList::for_plug(Mode::new(3000, 2000, 75), &[]);
        assert_eq!(
            fresh.as_slice().len(),
            1 + RATES.len() + SIZES.len() * RATES.len()
        );
    }

    #[test]
    fn the_cap_holds_a_fresh_plug_and_every_custom_mode() {
        let custom: [Mode; MAX_CUSTOM] =
            core::array::from_fn(|i| Mode::new(700 + u32::try_from(i).expect("small"), 500, 50));
        let list = ModeList::for_plug(Mode::new(3000, 2000, 75), &custom);
        assert_eq!(
            list.as_slice().len(),
            1 + RATES.len() + MAX_CUSTOM + SIZES.len() * RATES.len()
        );
        assert!(list.as_slice().len() <= MAX_MODES);
        assert!(list.lists(Mode::new(715, 500, 50)), "the last custom mode");
    }

    /// **30 a second is listed at every size**: unlisted, the first stream
    /// asked for it plugged the host's own display again, and windows moved
    /// onto it.
    #[test]
    fn thirty_a_second_is_listed_at_every_size() {
        let list = ModeList::for_plug(DEFAULT, &[]);
        for (width, height) in SIZES {
            assert!(
                list.lists(Mode::new(width, height, 30)),
                "{width}x{height}@30"
            );
        }
    }

    #[test]
    fn every_listed_mode_is_one_a_monitor_may_be() {
        for (width, height) in SIZES {
            for rate in RATES {
                assert!(Mode::new(width, height, rate).is_valid());
            }
        }
        assert!(DEFAULT.is_valid());
        // Each bound, on both sides.
        assert!(Mode::new(640, 480, 24).is_valid());
        assert!(Mode::new(7680, 4320, 480).is_valid());
        assert!(!Mode::new(639, 480, 60).is_valid());
        assert!(!Mode::new(640, 479, 60).is_valid());
        assert!(!Mode::new(1920, 1080, 23).is_valid());
        assert!(!Mode::new(7681, 4320, 60).is_valid());
        assert!(!Mode::new(7680, 4321, 60).is_valid());
        assert!(!Mode::new(1920, 1080, 481).is_valid());
    }

    #[test]
    fn a_mode_reads_and_writes_as_its_three_numbers() {
        assert_eq!(
            Mode::parse("2560x1440@120"),
            Some(Mode::new(2560, 1440, 120))
        );
        for bad in [
            "2560x1440",
            "2560@120",
            "x1440@120",
            "2560x@120",
            "2560x1440@",
            "+2560x1440@60",
            "2560 x1440@60",
            "2560x1440@60x",
            "99999999999x1@1",
        ] {
            assert_eq!(Mode::parse(bad), None, "{bad}");
        }
        let mut text = [0u8; 32];
        let mut out = Writer::new(&mut text);
        core::fmt::Write::write_fmt(&mut out, format_args!("{}", Mode::new(1920, 1080, 60)))
            .expect("room");
        assert_eq!(out.written(), b"1920x1080@60");
    }

    /// A buffer to format into, for the test above.
    struct Writer<'a> {
        out: &'a mut [u8],
        len: usize,
    }

    impl<'a> Writer<'a> {
        fn new(out: &'a mut [u8]) -> Self {
            Self { out, len: 0 }
        }

        fn written(&self) -> &[u8] {
            &self.out[..self.len]
        }
    }

    impl core::fmt::Write for Writer<'_> {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            let end = self.len + s.len();
            self.out
                .get_mut(self.len..end)
                .ok_or(core::fmt::Error)?
                .copy_from_slice(s.as_bytes());
            self.len = end;
            Ok(())
        }
    }
}
