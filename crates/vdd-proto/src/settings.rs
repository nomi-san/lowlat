//! The file kept monitors come back from, and the adapter's settings
//! (docs/11-drivers.md section 1.4).
//!
//! **Written by the controller, read by the driver, and writable by any user**,
//! so the driver reads it as it reads a request: a line at a time, every length
//! bounded, a line it cannot read skipped and counted, and nothing in it able
//! to stop the driver starting. One setting a line:
//!
//! ```text
//! gpu 10de:2d05             the GPU to render on, by vendor and device;
//!                           `10de` the first of that vendor's in the
//!                           high-performance order, `igpu` the first in the
//!                           minimum-power order; `auto` or absent: the
//!                           high-performance one
//! pointer drawn             the pointer drawn into the pictures;
//!                           `hardware` or absent: the hardware cursor
//! mode 3000x2000@120        a custom mode every monitor plugged after lists
//! monitor 3 2560x1440@120   a kept monitor the driver plugs at that slot when
//!                           it starts
//! edid 00ffffffffffff00...  a dumped EDID, one block or two in hexadecimal,
//!                           for every slot
//! ```
//!
//! A line starting `#` is a comment. A word this version does not know is
//! skipped without complaint, so an older driver reads a newer file.

use core::fmt::{self, Write};

use crate::MAX_MONITORS;
use crate::edid::{self, Edid};
use crate::modes::{MAX_CUSTOM, Mode};

/// The most of the file the driver reads.
pub const MAX_FILE: usize = 16 * 1024;
/// The longest line read: an EDID of two blocks in hexadecimal and its word,
/// with room for spaces.
const MAX_LINE: usize = 2 * edid::MAX_LEN + 32;
/// The file's folder, under the system's program-data directory: users may
/// write it, the driver's account read it.
pub const FOLDER: &str = "lowlat\\display";
/// The file's name, in that folder.
pub const FILE_NAME: &str = "displays.conf";

/// A GPU by its maker's and its own number, which outlive a boot where its
/// adapter's identifier does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gpu {
    pub vendor: u16,
    pub device: u16,
}

/// Which GPU the monitors render on, when the file names one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuChoice {
    /// One GPU, by its maker's and its own number.
    Exact(Gpu),
    /// The first GPU of a maker, by the maker's number, in the system's
    /// high-performance order: a card replaced by another of its maker's
    /// keeps the choice, where an exact one falls back to the default.
    Maker(u16),
    /// The first GPU in the system's minimum-power order: the power-saving
    /// one, which is the integrated GPU where a machine has one.
    PowerSaving,
}

impl GpuChoice {
    /// Whether the GPU of this maker's and its own number is the one named,
    /// walking the order [`Self::power_saving`] says: the first it takes is
    /// the choice.
    pub fn takes(self, vendor: u16, device: u16) -> bool {
        match self {
            Self::Exact(gpu) => gpu.vendor == vendor && gpu.device == device,
            Self::Maker(maker) => maker == vendor,
            Self::PowerSaving => true,
        }
    }

    /// Whether the system's minimum-power order is walked rather than its
    /// high-performance one.
    pub fn power_saving(self) -> bool {
        self == Self::PowerSaving
    }
}

/// As the file writes it.
impl fmt::Display for GpuChoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Exact(gpu) => write!(f, "{:04x}:{:04x}", gpu.vendor, gpu.device),
            Self::Maker(maker) => write!(f, "{maker:04x}"),
            Self::PowerSaving => f.write_str("igpu"),
        }
    }
}

/// Where the pointer is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Pointer {
    /// Reported beside the pictures, never in them (section 1.7).
    #[default]
    Hardware,
    /// Drawn into the pictures by the system, for software that wants it
    /// there.
    Drawn,
}

/// What the file says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    /// The GPU to render on; the high-performance one when none.
    pub gpu: Option<GpuChoice>,
    pub pointer: Pointer,
    modes: [Mode; MAX_CUSTOM],
    mode_count: usize,
    /// The kept monitors the driver plugs when it starts, by slot, each at
    /// its mode.
    pub restore: [Option<Mode>; MAX_MONITORS],
    /// A dumped EDID for every slot; the driver's own when none.
    pub edid: Option<Edid>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            gpu: None,
            pointer: Pointer::Hardware,
            modes: [Mode::default(); MAX_CUSTOM],
            mode_count: 0,
            restore: [None; MAX_MONITORS],
            edid: None,
        }
    }
}

/// A buffer formatted into, refusing whatever does not fit.
struct Out<'a> {
    out: &'a mut [u8],
    len: usize,
}

impl Write for Out<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let end = self.len.checked_add(s.len()).ok_or(fmt::Error)?;
        self.out
            .get_mut(self.len..end)
            .ok_or(fmt::Error)?
            .copy_from_slice(s.as_bytes());
        self.len = end;
        Ok(())
    }
}

/// Hexadecimal digits alone, of a value that fits sixteen bits.
fn hex16(text: &str) -> Option<u16> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    u16::from_str_radix(text, 16).ok()
}

fn nibble(digit: u8) -> Option<u8> {
    match digit {
        b'0'..=b'9' => Some(digit - b'0'),
        b'a'..=b'f' => Some(digit - b'a' + 10),
        b'A'..=b'F' => Some(digit - b'A' + 10),
        _ => None,
    }
}

impl Settings {
    /// The custom modes, in the order they were added.
    pub fn modes(&self) -> &[Mode] {
        self.modes.get(..self.mode_count).unwrap_or(&[])
    }

    /// Add a custom mode. False when it is no mode a monitor may be, or when
    /// the file holds as many as it takes; one already there is not added
    /// twice and answers true.
    pub fn add_mode(&mut self, mode: Mode) -> bool {
        if !mode.is_valid() {
            return false;
        }
        if self.modes().contains(&mode) {
            return true;
        }
        match self.modes.get_mut(self.mode_count) {
            Some(free) => {
                *free = mode;
                self.mode_count += 1;
                true
            }
            None => false,
        }
    }

    /// Remove a custom mode; false when it was not there.
    pub fn remove_mode(&mut self, mode: Mode) -> bool {
        let Some(at) = self.modes().iter().position(|m| *m == mode) else {
            return false;
        };
        let count = self.mode_count;
        if let Some(tail) = self.modes.get_mut(at..count) {
            tail.rotate_left(1);
        }
        self.mode_count -= 1;
        true
    }

    /// Read a file: what it says, and how many lines could not be read. Only
    /// the first [`MAX_FILE`] bytes are looked at.
    pub fn parse(text: &[u8]) -> (Self, u32) {
        let mut settings = Self::default();
        let mut skipped = 0u32;
        let text = text.get(..MAX_FILE).unwrap_or(text);
        for line in text.split(|b| *b == b'\n') {
            let read = line.len() <= MAX_LINE
                && core::str::from_utf8(line).is_ok_and(|line| settings.read_line(line));
            if !read {
                skipped = skipped.saturating_add(1);
            }
        }
        (settings, skipped)
    }

    /// One line: true when it was read or was nothing to read.
    fn read_line(&mut self, line: &str) -> bool {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            return true;
        }
        let (word, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        let rest = rest.trim();
        match word {
            "gpu" => self.read_gpu(rest),
            "pointer" => self.read_pointer(rest),
            "mode" => Mode::parse(rest).is_some_and(|mode| self.add_mode(mode)),
            "monitor" => self.read_monitor(rest),
            "edid" => self.read_edid(rest),
            // A word from a newer version.
            _ => true,
        }
    }

    /// **Numbers alone, and the one word.** A maker's name is the commands'
    /// to turn into its number; a driver from before the maker and the word
    /// reads either as a line it cannot read, and keeps the default.
    fn read_gpu(&mut self, text: &str) -> bool {
        self.gpu = match text {
            "auto" => None,
            "igpu" => Some(GpuChoice::PowerSaving),
            _ => match text.split_once(':') {
                Some((vendor, device)) => match (hex16(vendor), hex16(device)) {
                    (Some(vendor), Some(device)) => Some(GpuChoice::Exact(Gpu { vendor, device })),
                    _ => return false,
                },
                None => match hex16(text) {
                    Some(maker) => Some(GpuChoice::Maker(maker)),
                    None => return false,
                },
            },
        };
        true
    }

    fn read_pointer(&mut self, text: &str) -> bool {
        self.pointer = match text {
            "hardware" => Pointer::Hardware,
            "drawn" => Pointer::Drawn,
            _ => return false,
        };
        true
    }

    fn read_monitor(&mut self, text: &str) -> bool {
        let Some((slot, mode)) = text.split_once(char::is_whitespace) else {
            return false;
        };
        if slot.is_empty() || slot.len() > 2 || !slot.bytes().all(|b| b.is_ascii_digit()) {
            return false;
        }
        let (Ok(slot), Some(mode)) = (slot.parse::<usize>(), Mode::parse(mode.trim())) else {
            return false;
        };
        if !mode.is_valid() {
            return false;
        }
        match self.restore.get_mut(slot) {
            Some(entry) => {
                *entry = Some(mode);
                true
            }
            None => false,
        }
    }

    fn read_edid(&mut self, text: &str) -> bool {
        let digits = text.as_bytes();
        if digits.len() % 2 != 0 || digits.len() > 2 * edid::MAX_LEN {
            return false;
        }
        let mut bytes = [0u8; edid::MAX_LEN];
        let len = digits.len() / 2;
        for (byte, pair) in bytes.iter_mut().zip(digits.chunks_exact(2)) {
            let (Some(&high), Some(&low)) = (pair.first(), pair.get(1)) else {
                return false;
            };
            let (Some(high), Some(low)) = (nibble(high), nibble(low)) else {
                return false;
            };
            *byte = high << 4 | low;
        }
        match bytes.get(..len).and_then(Edid::parse) {
            Some(edid) => {
                self.edid = Some(edid);
                true
            }
            None => false,
        }
    }

    /// Write the file: the length written, or `None` when it does not fit.
    pub fn write(&self, out: &mut [u8]) -> Option<usize> {
        let mut out = Out { out, len: 0 };
        self.write_to(&mut out).ok()?;
        Some(out.len)
    }

    fn write_to(&self, out: &mut Out<'_>) -> fmt::Result {
        out.write_str(
            "# lowlat's virtual displays: written by `lowlat-vdd`, `lowlatd display`\n\
             # and the library's controller, read by the display driver when it starts.\n",
        )?;
        match self.gpu {
            Some(choice) => writeln!(out, "gpu {choice}")?,
            None => out.write_str("gpu auto\n")?,
        }
        match self.pointer {
            Pointer::Hardware => out.write_str("pointer hardware\n")?,
            Pointer::Drawn => out.write_str("pointer drawn\n")?,
        }
        for mode in self.modes() {
            writeln!(out, "mode {mode}")?;
        }
        for (slot, mode) in self.restore.iter().enumerate() {
            if let Some(mode) = mode {
                writeln!(out, "monitor {slot} {mode}")?;
            }
        }
        if let Some(edid) = &self.edid {
            out.write_str("edid ")?;
            for byte in edid.as_slice() {
                write!(out, "{byte:02x}")?;
            }
            out.write_str("\n")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::string::String;

    use super::*;

    fn written(settings: &Settings) -> ([u8; MAX_FILE], usize) {
        let mut buf = [0u8; MAX_FILE];
        let len = settings.write(&mut buf).expect("fits");
        (buf, len)
    }

    const RTX: GpuChoice = GpuChoice::Exact(Gpu {
        vendor: 0x10de,
        device: 0x2d05,
    });

    fn full() -> Settings {
        let mut settings = Settings {
            gpu: Some(RTX),
            pointer: Pointer::Drawn,
            edid: Some(Edid::generated().for_slot(4)),
            ..Settings::default()
        };
        assert!(settings.add_mode(Mode::new(3000, 2000, 120)));
        assert!(settings.add_mode(Mode::new(1920, 1080, 75)));
        settings.restore[0] = Some(Mode::new(2560, 1440, 120));
        settings.restore[15] = Some(Mode::new(1920, 1080, 60));
        settings
    }

    #[test]
    fn nothing_says_the_defaults() {
        let (settings, skipped) = Settings::parse(b"");
        assert_eq!(settings, Settings::default());
        assert_eq!(skipped, 0);
        assert_eq!(settings.gpu, None);
        assert_eq!(settings.pointer, Pointer::Hardware);
        assert!(settings.modes().is_empty());
        assert!(settings.restore.iter().all(Option::is_none));
    }

    #[test]
    fn what_is_written_reads_back_the_same() {
        let choosing = |gpu| Settings {
            gpu: Some(gpu),
            ..Settings::default()
        };
        for settings in [
            Settings::default(),
            full(),
            choosing(GpuChoice::Maker(0x8086)),
            choosing(GpuChoice::PowerSaving),
        ] {
            let (buf, len) = written(&settings);
            let (back, skipped) = Settings::parse(&buf[..len]);
            assert_eq!(back, settings);
            assert_eq!(
                skipped,
                0,
                "{}",
                core::str::from_utf8(&buf[..len]).expect("text")
            );
        }
    }

    #[test]
    fn the_written_lines_are_the_documented_ones() {
        let (buf, len) = written(&full());
        let text = core::str::from_utf8(&buf[..len]).expect("text");
        for line in [
            "gpu 10de:2d05",
            "pointer drawn",
            "mode 3000x2000@120",
            "mode 1920x1080@75",
            "monitor 0 2560x1440@120",
            "monitor 15 1920x1080@60",
        ] {
            assert!(text.lines().any(|l| l == line), "{line} in\n{text}");
        }
        assert!(text.lines().any(|l| l.starts_with("edid 00ffffffffffff00")));
        let (buf, len) = written(&Settings::default());
        let text = core::str::from_utf8(&buf[..len]).expect("text");
        assert!(text.lines().any(|l| l == "gpu auto"));
        assert!(text.lines().any(|l| l == "pointer hardware"));
        for (gpu, line) in [
            (GpuChoice::Maker(0x1002), "gpu 1002"),
            (GpuChoice::PowerSaving, "gpu igpu"),
        ] {
            let (buf, len) = written(&Settings {
                gpu: Some(gpu),
                ..Settings::default()
            });
            let text = core::str::from_utf8(&buf[..len]).expect("text");
            assert!(text.lines().any(|l| l == line), "{line} in\n{text}");
        }
    }

    /// **The first GPU a choice takes, in the order it says, is the one
    /// named.** An exact choice takes its own pair alone, a maker's every
    /// GPU of that maker, and the power-saving one the first of the
    /// minimum-power order, whoever made it.
    #[test]
    fn each_choice_takes_the_gpus_it_names() {
        assert!(RTX.takes(0x10de, 0x2d05));
        assert!(!RTX.takes(0x10de, 0x2c05), "another of the maker's");
        assert!(
            !RTX.takes(0x8086, 0x2d05),
            "another maker's with the number"
        );
        let maker = GpuChoice::Maker(0x8086);
        assert!(maker.takes(0x8086, 0x56a5) && maker.takes(0x8086, 0x9a49));
        assert!(!maker.takes(0x10de, 0x2d05));
        assert!(GpuChoice::PowerSaving.takes(0x1002, 0x13c0));
        assert!(GpuChoice::PowerSaving.takes(0x10de, 0x2d05));
        assert!(GpuChoice::PowerSaving.power_saving());
        assert!(!RTX.power_saving() && !maker.power_saving());
    }

    #[test]
    fn comments_blank_lines_and_line_ends_are_nothing() {
        let text = b"# a comment\r\n\r\n   \n\tgpu  10DE:2d05 \r\n pointer drawn\nmonitor 2   1920x1080@60\r\n";
        let (settings, skipped) = Settings::parse(text);
        assert_eq!(skipped, 0);
        assert_eq!(settings.gpu, Some(RTX));
        assert_eq!(settings.pointer, Pointer::Drawn);
        assert_eq!(settings.restore[2], Some(Mode::new(1920, 1080, 60)));
    }

    #[test]
    fn a_word_from_a_newer_version_is_skipped_quietly() {
        let (settings, skipped) = Settings::parse(b"brightness 50\nmode 3000x2000@120\n");
        assert_eq!(skipped, 0);
        assert_eq!(settings.modes(), &[Mode::new(3000, 2000, 120)]);
    }

    #[test]
    fn a_line_that_cannot_be_read_is_skipped_and_counted() {
        let bad: [&[u8]; 19] = [
            b"gpu nvidia",
            b"gpu igpus",
            b"gpu 12345",
            b"gpu 10de:12345",
            b"gpu xyz:2d05",
            b"gpu :2d05",
            b"pointer sometimes",
            b"mode 100x100@60",
            b"mode 1920x1080",
            b"monitor 16 1920x1080@60",
            b"monitor x 1920x1080@60",
            b"monitor 3",
            b"monitor 3 1920x1080@500",
            b"monitor 003 1920x1080@60",
            b"edid 00ff",
            b"edid zz",
            b"edid 0",
            b"\xff\xfe gpu auto",
            b"",
        ];
        for line in bad.iter().filter(|line| !line.is_empty()) {
            let (settings, skipped) = Settings::parse(line);
            assert_eq!(skipped, 1, "{}", String::from_utf8_lossy(line));
            assert_eq!(
                settings,
                Settings::default(),
                "{}",
                String::from_utf8_lossy(line)
            );
        }
        let long = [b'#'; MAX_LINE + 1];
        assert_eq!(Settings::parse(&long).1, 1, "an over-long line");
        // The rest of a file is still read past a bad line.
        let (settings, skipped) = Settings::parse(b"gpu nope\npointer drawn\n");
        assert_eq!(skipped, 1);
        assert_eq!(settings.pointer, Pointer::Drawn);
    }

    #[test]
    fn a_damaged_edid_is_refused_and_a_good_one_taken() {
        let good = Edid::generated().for_slot(0);
        let mut line = [0u8; 5 + 2 * edid::LEN];
        line[..5].copy_from_slice(b"edid ");
        for (i, byte) in good.as_slice().iter().enumerate() {
            let digits = b"0123456789abcdef";
            line[5 + 2 * i] = digits[usize::from(byte >> 4)];
            line[6 + 2 * i] = digits[usize::from(byte & 15)];
        }
        let (settings, skipped) = Settings::parse(&line);
        assert_eq!((settings.edid, skipped), (Some(good), 0));
        // A good block and a stray digit after it.
        let mut stray = [b'0'; 5 + 2 * edid::LEN + 1];
        stray[..line.len()].copy_from_slice(&line);
        let (settings, skipped) = Settings::parse(&stray);
        assert_eq!(
            (settings.edid, skipped),
            (None, 1),
            "an odd count of digits"
        );
        line[40] = if line[40] == b'0' { b'1' } else { b'0' };
        let (settings, skipped) = Settings::parse(&line);
        assert_eq!((settings.edid, skipped), (None, 1), "a checksum broken");
    }

    #[test]
    fn custom_modes_stop_at_the_cap_and_are_not_doubled() {
        let mut settings = Settings::default();
        for i in 0..MAX_CUSTOM {
            assert!(settings.add_mode(Mode::new(700 + u32::try_from(i).expect("small"), 500, 50)));
        }
        assert!(!settings.add_mode(Mode::new(3000, 2000, 120)), "full");
        assert!(settings.add_mode(Mode::new(700, 500, 50)), "already there");
        assert_eq!(settings.modes().len(), MAX_CUSTOM);
        assert!(settings.remove_mode(Mode::new(703, 500, 50)));
        assert!(!settings.remove_mode(Mode::new(703, 500, 50)), "gone");
        assert_eq!(settings.modes().len(), MAX_CUSTOM - 1);
        assert_eq!(
            settings.modes()[3],
            Mode::new(704, 500, 50),
            "the order kept"
        );
        assert!(
            !settings.add_mode(Mode::new(100, 100, 60)),
            "no mode a monitor is"
        );

        let mut text = [0u8; 64 * (MAX_CUSTOM + 1)];
        let mut out = Out {
            out: &mut text,
            len: 0,
        };
        for i in 0..=MAX_CUSTOM {
            writeln!(out, "mode {}x500@50", 700 + i).expect("room");
        }
        let len = out.len;
        let (settings, skipped) = Settings::parse(&text[..len]);
        assert_eq!(settings.modes().len(), MAX_CUSTOM);
        assert_eq!(skipped, 1, "the one past the cap");
    }

    #[test]
    fn only_the_first_part_of_a_huge_file_is_read() {
        let mut text = [b'\n'; MAX_FILE + 64];
        text[MAX_FILE + 1..MAX_FILE + 14].copy_from_slice(b"pointer drawn");
        let (settings, _) = Settings::parse(&text);
        assert_eq!(settings.pointer, Pointer::Hardware);
    }

    #[test]
    fn a_file_that_does_not_fit_is_not_written() {
        let mut small = [0u8; 64];
        assert_eq!(full().write(&mut small), None);
    }
}
