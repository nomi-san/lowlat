//! The decoders a stream is opened on, on Windows: the system's video
//! decoding interface on an adapter, or the machine's own codec library.

use lowlat_decode::{Format, d3d11, software};
use lowlat_drivers::d3d11::D3d11;
use lowlat_drivers::lavc::Lavc;

use super::{Backend, Next, Shared, drive};
use crate::seam::Opened;

impl Backend for d3d11::Backend<'_> {
    fn output(&self) -> Option<(u32, u32, Format)> {
        d3d11::Backend::output(self)
    }
    fn timings(&self) -> (u32, u32) {
        (self.decode_us, self.readback_us)
    }
}

/// Open the decoder `opened` names and drive it until it returns. The
/// device, or the library pair, is opened here, on the decode thread, and
/// lives as long as the decoder built on it does.
pub(super) fn open(opened: Opened, shared: &Shared<'_>, replacing: bool) -> Next {
    match opened {
        Opened::D3d11(luid) => {
            let Ok(d3d11) = D3d11::load() else {
                return Next::Failed;
            };
            // An adapter gone since creation (a reset, a driver update)
            // no longer opens under its identity.
            let Ok(device) = d3d11.open(luid) else {
                return Next::Failed;
            };
            let backend = d3d11::Backend::new(&device, shared.frames.ceiling());
            drive(backend, shared, replacing)
        }
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
