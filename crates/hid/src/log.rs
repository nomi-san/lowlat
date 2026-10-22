//! The driver's log goes to the debugger stream: the driver host has no console,
//! and no file it may write.
//!
//! Each line carries the seconds since the driver's first line, as the
//! library's own log does, since a reader of this stream has nothing else to
//! tell an interval by.

use core::fmt::Write;
use std::sync::OnceLock;

use lowlat_common::clock::{Time, elapsed_ms};
use lowlat_common::log::Level;
use windows_sys::Win32::System::Diagnostics::Debug::OutputDebugStringA;

static SINCE: OnceLock<Time> = OnceLock::new();

pub(crate) fn install() {
    // A second install, from a second `DriverEntry` in one host process, is
    // refused by the logger and the first sink goes on.
    let _ = lowlat_common::log::set_sink(sink);
    let _ = SINCE.get_or_init(Time::now);
}

/// A line under construction: what does not fit is cut, never allocated.
struct Line {
    bytes: [u8; 512],
    len: usize,
}

impl Write for Line {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        // Two bytes stay free for the newline and the terminator.
        let room = self.bytes.len().saturating_sub(self.len + 2);
        let take = s.len().min(room);
        if let (Some(dst), Some(src)) = (
            self.bytes.get_mut(self.len..self.len + take),
            s.as_bytes().get(..take),
        ) {
            dst.copy_from_slice(src);
            self.len += take;
        }
        Ok(())
    }
}

fn sink(level: Level, message: &str) {
    let since = SINCE.get_or_init(Time::now);
    let mut line = Line {
        bytes: [0; 512],
        len: 0,
    };
    let _ = write!(
        line,
        "lowlat-hid [{:9.3}] [{}] {}",
        elapsed_ms(*since) / 1000.0,
        level.tag(),
        message
    );
    if let Some(end) = line.bytes.get_mut(line.len..line.len + 2) {
        end.copy_from_slice(b"\n\0");
    }
    // SAFETY: a terminated string in a live buffer.
    unsafe { OutputDebugStringA(line.bytes.as_ptr()) };
}
