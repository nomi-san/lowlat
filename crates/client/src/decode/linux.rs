//! The decoders a stream is opened on, on Linux: the open stack on a render
//! node, the vendor's interface on a card, or the machine's own codec
//! library.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use lowlat_decode::vaapi::{self, Vaapi};
use lowlat_decode::{Fault, Format, Lease, Picture, nvdec, software};
use lowlat_drivers::cuda::Cuda;
use lowlat_drivers::cuvid::Cuvid;
use lowlat_drivers::lavc::Lavc;

use super::{Backend, Next, Shared, drive};
use crate::UNIT_BYTES;
use crate::frames::Filling;
use crate::seam::Opened;

// A queue slot holds at most one lent surface, so the spares cover every
// slot at once.
const _: () = assert!(vaapi::SPARES >= crate::frames::SLOTS);

impl Backend for vaapi::Backend<'_> {
    fn output(&self) -> Option<(u32, u32, Format)> {
        vaapi::Backend::output(self)
    }
    fn timings(&self) -> (u32, u32) {
        (self.decode_us, self.readback_us)
    }
    fn exports(&self) -> bool {
        // Its context's surfaces export in the layout lent; the trait's own
        // `lends` is the macOS kind, taken whatever is asked, which this is
        // not: as planes it reads back.
        vaapi::Backend::lends(self)
    }
    fn take_to_slot(
        &mut self,
        filling: &mut Filling<'_>,
        _layout: (u32, u32, Format),
    ) -> Result<Option<(Picture, u64)>, Fault> {
        // The surface itself, lent to the slot: its decode is waited for
        // before the take returns.
        let Some((picture, lent)) = self.take_lent(|| filling.new_allocation())? else {
            return Ok(None);
        };
        if let Some(replaced) = filling.lend_surface(lent) {
            self.lease_ended(replaced);
        }
        Ok(Some((picture, 0)))
    }
    fn lease_ended(&mut self, lease: Lease) {
        vaapi::Backend::lease_ended(self, lease);
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
///
/// **The vendor's decoder is built again in a fresh context when it
/// wedges**: a picture whose decode never finished leaves nothing in its
/// context able to finish, so that context is retired, never destroyed, and
/// the next is fresh, a keyframe asked for it. Two wedges in a row with no
/// picture between end the stream, as a lost device does.
pub(super) fn open(opened: Opened, shared: &Shared<'_>, replacing: bool) -> Next {
    match opened {
        Opened::Vaapi { node, .. } => {
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
            // The probes and the listing retain and release the primary
            // context; once a decoding context has wedged, a release that
            // is its last never returns, so one reference is kept.
            if cuda.hold_primary(&device).is_err() {
                return Next::Failed;
            }
            let Ok(cuvid) = Cuvid::load() else {
                return Next::Failed;
            };
            let cuda = Arc::new(cuda);
            let mut replacing = replacing;
            let mut fruitless = false;
            loop {
                let Ok(context) = cuda.decoding_context(&device) else {
                    return Next::Failed;
                };
                if context.make_current().is_err() {
                    return Next::Failed;
                }
                // The queue's device slots are made through this runtime,
                // and may outlive this thread while the application holds
                // one, so the queue keeps its own reference to it. Attached
                // whatever kind the session asks for now, so a switch to
                // handles has it; a fresh context is a new backing, and the
                // slots are made again in it.
                shared.frames.open_device(Arc::clone(&cuda), device);
                let backend =
                    nvdec::Backend::new(&cuda, &cuvid, shared.frames.ceiling(), UNIT_BYTES);
                let before = shared.telemetry.decoded.load(Ordering::Relaxed);
                let next = drive(backend, shared, replacing);
                let _ = context.release_current();
                // The vendor's backend loses its device only by wedging.
                if next != Next::Lost {
                    return next;
                }
                cuda.retire(context);
                let progressed = shared.telemetry.decoded.load(Ordering::Relaxed) != before;
                if fruitless && !progressed {
                    return Next::Lost;
                }
                fruitless = !progressed;
                lowlat_common::log_warn!(
                    "client: the vendor's decoder wedged, built again in a fresh context"
                );
                replacing = true;
            }
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
