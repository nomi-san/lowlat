//! Leveled logging with an application-supplied sink.
//!
//! Log for diagnosis from logs alone. Lifecycle at info, recoverable at warning,
//! session-fatal at error, hot-path detail at trace and compiled out in release.
//!
//! Messages are plain ASCII, formatted as
//! `subsystem: what happened, key=value key=value`, and carry the identifiers
//! needed to correlate across threads. No sentences.

use core::sync::atomic::{AtomicU8, Ordering};
use std::sync::OnceLock;

/// Severity, ordered from most to least severe.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum Level {
    Error = 0,
    Warn = 1,
    Info = 2,
    Debug = 3,
    Trace = 4,
}

impl Level {
    /// Fixed-width tag, so log lines align in a terminal.
    pub const fn tag(self) -> &'static str {
        match self {
            Level::Error => "ERR ",
            Level::Warn => "WARN",
            Level::Info => "INFO",
            Level::Debug => "DBG ",
            Level::Trace => "TRC ",
        }
    }
}

/// A sink receives already-formatted messages. It must not call back into the
/// library, and it may be invoked from any thread.
pub type Sink = fn(Level, &str);

static SINK: OnceLock<Sink> = OnceLock::new();
static LEVEL: AtomicU8 = AtomicU8::new(Level::Info as u8);

/// Install the sink. Once only, at initialisation; later calls are refused and
/// return false.
pub fn set_sink(sink: Sink) -> bool {
    SINK.set(sink).is_ok()
}

/// Set the maximum level that will be emitted.
pub fn set_level(level: Level) {
    LEVEL.store(level as u8, Ordering::Relaxed);
}

/// The current maximum level.
pub fn level() -> Level {
    match LEVEL.load(Ordering::Relaxed) {
        0 => Level::Error,
        1 => Level::Warn,
        2 => Level::Info,
        3 => Level::Debug,
        _ => Level::Trace,
    }
}

/// True if a message at `level` would be emitted. Check this before doing any
/// formatting work.
pub fn enabled(level: Level) -> bool {
    (level as u8) <= LEVEL.load(Ordering::Relaxed)
}

/// Emit a pre-formatted message. Prefer the macros.
pub fn emit(level: Level, message: &str) {
    if !enabled(level) {
        return;
    }
    match SINK.get() {
        Some(sink) => sink(level, message),
        None => eprintln!("[{}] {}", level.tag(), message),
    }
}

#[doc(hidden)]
pub fn emit_args(level: Level, args: core::fmt::Arguments<'_>) {
    if !enabled(level) {
        return;
    }
    emit(level, &args.to_string());
}

#[macro_export]
macro_rules! log_error {
    ($($arg:tt)*) => {
        $crate::log::emit_args($crate::log::Level::Error, format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_warn {
    ($($arg:tt)*) => {
        $crate::log::emit_args($crate::log::Level::Warn, format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_info {
    ($($arg:tt)*) => {
        $crate::log::emit_args($crate::log::Level::Info, format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_debug {
    ($($arg:tt)*) => {
        $crate::log::emit_args($crate::log::Level::Debug, format_args!($($arg)*))
    };
}

/// Hot-path detail. **Compiled out entirely in release builds**, so it may
/// appear on a data path without violating the allocation rules.
#[macro_export]
macro_rules! log_trace {
    ($($arg:tt)*) => {
        #[cfg(debug_assertions)]
        {
            $crate::log::emit_args($crate::log::Level::Trace, format_args!($($arg)*))
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_ordering_matches_severity() {
        assert!(Level::Error < Level::Warn);
        assert!(Level::Warn < Level::Info);
        assert!(Level::Info < Level::Debug);
        assert!(Level::Debug < Level::Trace);
    }

    #[test]
    fn level_filter_admits_more_severe_and_rejects_less() {
        set_level(Level::Warn);
        assert!(enabled(Level::Error));
        assert!(enabled(Level::Warn));
        assert!(!enabled(Level::Info));
        assert!(!enabled(Level::Trace));
        set_level(Level::Info);
    }

    #[test]
    fn tags_are_fixed_width() {
        for level in [
            Level::Error,
            Level::Warn,
            Level::Info,
            Level::Debug,
            Level::Trace,
        ] {
            assert_eq!(level.tag().len(), 4, "tag width drifted for {level:?}");
        }
    }
}
