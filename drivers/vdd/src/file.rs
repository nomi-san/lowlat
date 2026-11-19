//! The file kept monitors come back from (docs/11-drivers.md section 1.4):
//! read here when the adapter starts and when a controller asks, never
//! written -- a user-mode driver may not, and the controller does.

use std::io::Read;
use std::path::PathBuf;

use lowlat_vdd_proto::settings::{FILE_NAME, FOLDER, MAX_FILE, Settings};

/// Where the file is: the system's program-data directory, which every
/// account's environment names, the driver's own included.
fn path() -> Option<PathBuf> {
    let root = std::env::var_os("ProgramData")?;
    Some(PathBuf::from(root).join(FOLDER).join(FILE_NAME))
}

/// What the file says, and how many of its lines could not be read; the
/// defaults when there is no file.
pub(crate) fn read() -> (Settings, u32, bool) {
    let Some(path) = path() else {
        return (Settings::default(), 0, false);
    };
    let Ok(file) = std::fs::File::open(&path) else {
        return (Settings::default(), 0, false);
    };
    let mut text = Vec::new();
    let limit = u64::try_from(MAX_FILE).unwrap_or(u64::MAX);
    if file.take(limit).read_to_end(&mut text).is_err() {
        return (Settings::default(), 0, false);
    }
    let (settings, skipped) = Settings::parse(&text);
    (settings, skipped, true)
}
