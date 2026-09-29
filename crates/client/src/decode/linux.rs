//! The decoders a stream is opened on, on Linux: the open stack on a render
//! node, the vendor's interface on a card, or the machine's own codec
//! library.

use std::sync::Arc;

use lowlat_decode::vaapi::{self, Vaapi};
use lowlat_decode::{Fault, Format, Picture, nvdec, software};
use lowlat_drivers::cuda::Cuda;
use lowlat_drivers::cuvid::Cuvid;
use lowlat_drivers::lavc::Lavc;

use super::{Backend, Next, Shared, drive};
use crate::UNIT_BYTES;
use crate::frames::Filling;
use crate::seam::Opened;

impl Backend for vaapi::Backend<'_> {
    fn output(&self) -> Option<(u32, u32, Format)> {
        vaapi::Backend::output(self)
    }
    fn timings(&self) -> (u32, u32) {
        (self.decode_us, self.readback_us)
    }
}

impl Backend for nvdec::Backend<'_> {
    fn output(&self) -> Option<(u32, u32, Format)> {
        nvdec::Backend::output(self)
    }
    fn timings(&self) -> (u32, u32) {
        (self.decode_us, self.readback_us)
    }
    fn exports(&self) -> bool {
        true
    }
    fn take_to_slot(
        &mut self,
        filling: &mut Filling<'_>,
        (width, height, format): (u32, u32, Format),
    ) -> Result<Option<(Picture, u64)>, Fault> {
        // A queue without the vendor's runtime attached lends no device
        // memory; the picture is then lost, as a slot that does not fit is.
        let Some(planes) = filling.device_planes_for(width, height, format) else {
            return Ok(None);
        };
        // The device copy is waited for before the take returns.
        Ok(nvdec::Backend::take_to_device(self, &planes)?.map(|p| (p, 0)))
    }
}

/// Open the decoder `opened` names and drive it until it returns. The
/// runtimes are opened here, on the decode thread, and live as long as the
/// decoder built on them does; the vendor's context is made current here,
/// where every call against it is made.
pub(super) fn open(opened: Opened, shared: &Shared<'_>, replacing: bool) -> Next {
    match opened {
        Opened::Vaapi(node) => {
            let Ok(va) = Vaapi::load() else {
                return Next::Failed;
            };
            let Ok(display) = va.open(&node) else {
                return Next::Failed;
            };
            let backend = vaapi::Backend::new(&display, shared.frames.ceiling());
            drive(backend, shared, replacing)
        }
        Opened::Nvdec(address) => {
            let Ok(cuda) = Cuda::load() else {
                return Next::Failed;
            };
            let device = match address {
                Some(address) => cuda.device_at(address),
                None => cuda.any_device(),
            };
            let Ok(device) = device else {
                return Next::Failed;
            };
            let Ok(context) = cuda.retain_primary(&device) else {
                return Next::Failed;
            };
            if context.make_current().is_err() {
                return Next::Failed;
            }
            let Ok(cuvid) = Cuvid::load() else {
                return Next::Failed;
            };
            // The queue's device slots are made through this runtime, and
            // may outlive this thread while the application holds one, so
            // the queue keeps its own reference to it. Attached whatever
            // kind the session asks for now, so a switch to handles has it.
            let cuda = Arc::new(cuda);
            shared.frames.open_device(Arc::clone(&cuda), device);
            let backend = nvdec::Backend::new(&cuda, &cuvid, shared.frames.ceiling(), UNIT_BYTES);
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
