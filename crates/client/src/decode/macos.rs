//! The decoders a stream is opened on, on macOS: the system's own, whose
//! pictures stay in its buffers and are lent to the queue's slots, or the
//! machine's own codec library.

use lowlat_decode::{Fault, Format, Picture, software, videotoolbox};
use lowlat_drivers::lavc::Lavc;

use super::{Backend, Next, Shared, drive};
use crate::UNIT_BYTES;
use crate::frames::Filling;
use crate::seam::Opened;

impl Backend for videotoolbox::Backend {
    fn output(&self) -> Option<(u32, u32, Format)> {
        videotoolbox::Backend::output(self)
    }
    fn timings(&self) -> (u32, u32) {
        (self.decode_us, self.readback_us)
    }
    fn lends(&self) -> bool {
        true
    }
    fn take_to_slot(
        &mut self,
        filling: &mut Filling<'_>,
        _layout: (u32, u32, Format),
    ) -> Result<Option<(Picture, u64)>, Fault> {
        let Some((picture, buffer)) = self.take_buffer()? else {
            return Ok(None);
        };
        // A buffer that will not lock is lost, as a picture that does not
        // fit its slot is.
        Ok(filling.lend(buffer).then_some((picture, 0)))
    }
}

/// Open the decoder `opened` names and drive it until it returns. The
/// frameworks and the codec library are opened here, on the decode thread,
/// and live as long as the decoder built on them does.
pub(super) fn open(opened: Opened, shared: &Shared<'_>, replacing: bool) -> Next {
    match opened {
        Opened::VideoToolbox => {
            let Ok(vt) = lowlat_drivers::vt::load() else {
                return Next::Failed;
            };
            drive(
                videotoolbox::Backend::new(vt, UNIT_BYTES),
                shared,
                replacing,
            )
        }
        Opened::Software(dir) => {
            // The same search creation ran, landing on the same pair.
            let Ok(lavc) = Lavc::load(dir.as_deref()) else {
                return Next::Failed;
            };
            drive(software::Backend::new(&lavc), shared, replacing)
        }
    }
}
