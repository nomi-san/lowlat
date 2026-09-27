//! The decoders a stream is opened on, on Windows: the machine's own codec
//! library.

use lowlat_decode::software;
use lowlat_drivers::lavc::Lavc;

use super::{Next, Shared, drive};
use crate::seam::Opened;

/// Open the decoder `opened` names and drive it until it returns. The
/// library pair is opened here, on the decode thread, and lives as long as
/// the decoder built on it does.
pub(super) fn open(opened: Opened, shared: &Shared<'_>, replacing: bool) -> Next {
    match opened {
        Opened::Software(dir) => {
            // The same search creation ran, landing on the same pair.
            let Ok(lavc) = Lavc::load(dir.as_deref()) else {
                return Next::Failed;
            };
            let backend = software::Backend::new(&lavc);
            drive(backend, shared, replacing)
        }
    }
}
