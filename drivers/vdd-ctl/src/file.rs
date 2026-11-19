//! The file kept monitors come back from (docs/11-drivers.md section 1.4):
//! read and changed here, under a lock any user's controller takes, and
//! replaced whole so the driver never reads half of one.

use std::io::{ErrorKind, Read};
use std::os::windows::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use lowlat_vdd_proto::settings::{FILE_NAME, FOLDER, MAX_FILE, Settings};

use crate::Error;

/// The file's folder, which the driver's installer makes and lets users write.
fn folder() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("ProgramData")?).join(FOLDER))
}

/// What the file says; the defaults when there is none.
pub(crate) fn read() -> Result<Settings, Error> {
    let path = folder().ok_or(Error::File)?.join(FILE_NAME);
    let file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Settings::default()),
        Err(_) => return Err(Error::File),
    };
    let mut text = Vec::new();
    let limit = u64::try_from(MAX_FILE).unwrap_or(u64::MAX);
    file.take(limit)
        .read_to_end(&mut text)
        .map_err(|_| Error::File)?;
    Ok(Settings::parse(&text).0)
}

/// Change the file: read it, change it, write it whole, all under the lock.
pub(crate) fn change(
    deadline: Instant,
    change: impl FnOnce(&mut Settings),
) -> Result<Settings, Error> {
    let folder = folder().ok_or(Error::File)?;
    // The lock is a file opened by one holder at a time; the system lets it
    // go when its holder's process ends, however it ends.
    let _lock = loop {
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .share_mode(0)
            .open(folder.join("displays.lock"))
        {
            Ok(lock) => break lock,
            Err(error) if error.raw_os_error() == Some(32) && Instant::now() < deadline => {
                // Another controller holds it: a write takes milliseconds.
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(_) => return Err(Error::File),
        }
    };
    let mut settings = read()?;
    change(&mut settings);
    let mut text = vec![0u8; MAX_FILE];
    let len = settings.write(&mut text).ok_or(Error::File)?;
    let fresh = folder.join(format!("{FILE_NAME}.{}", std::process::id()));
    std::fs::write(&fresh, text.get(..len).unwrap_or(&[])).map_err(|_| Error::File)?;
    std::fs::rename(&fresh, folder.join(FILE_NAME)).map_err(|_| {
        let _ = std::fs::remove_file(&fresh);
        Error::File
    })?;
    Ok(settings)
}
