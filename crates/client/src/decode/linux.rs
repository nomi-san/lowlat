//! The decoders a stream is opened on, on Linux: the open stack on a render
//! node, the vendor's interface on a card, or the machine's own codec
//! library.

use std::sync::Arc;

use lowlat_decode::vaapi::{self, Vaapi};
use lowlat_decode::{Format, nvdec, software};
use lowlat_drivers::cuda::Cuda;
use lowlat_drivers::cuvid::Cuvid;
use lowlat_drivers::lavc::Lavc;

use super::{Backend, Next, Shared, drive};
use crate::UNIT_BYTES;
use crate::config::FrameKind;
use crate::seam::Opened;

impl Backend for vaapi::Backend<'_> {
    fn output(&self) -> Option<(u32, u32, Format)> {
        vaapi::Backend::output(self)
    }
    fn timings(&self) -> (u32, u32) {
        (self.decode_us, self.readback_us)
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
            // the queue keeps its own reference to it.
            let cuda = Arc::new(cuda);
            if shared.frames.kind() == FrameKind::Handle {
                shared.frames.open_device(Arc::clone(&cuda), device);
            }
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
