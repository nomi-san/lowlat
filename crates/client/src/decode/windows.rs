//! The decoders a stream is opened on, on Windows: the system's video
//! decoding interface on an adapter, the vendor's interface on one of its
//! GPUs, or the machine's own codec library.
//!
//! **A device lost is looked for again** under the system's interface, not
//! the end of the stream: a driver update, a reset or a restart of the GPU's
//! driver takes the device away and brings the GPU back under a new
//! identity. The decode thread looks for the same GPU by its hardware -- and
//! only for it while the search runs, since the GPU that drives the display
//! comes back in seconds and a session moved off it would stay on the other
//! for good -- opens there, and the replacement asks for its keyframe;
//! pictures then say which GPU they are on. A session nobody placed takes
//! the first GPU the system offers once the search has run its course.
//!
//! **The vendor's interface is not looked for again**: its runtime does not
//! come back in the process that lost its device, so the stream fails and
//! the session ends. **AMD's decoder is**, on the same GPU only, since it
//! decodes on no other: its runtime is made again on the new device, and a
//! runtime that will not be ends the stream there. **So is Intel's**, the
//! same way: a new session of its runtime on the new device.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use lowlat_decode::{Fault, Format, Picture, amf, d3d11, nvdec, software, vpl};
use lowlat_drivers::amf::Amf;
use lowlat_drivers::cuda::Cuda;
use lowlat_drivers::cuvid::Cuvid;
use lowlat_drivers::d3d11::{Adapter, D3d11, Luid};
use lowlat_drivers::lavc::Lavc;
use lowlat_drivers::vpl::{Runtime, Vpl};

use super::{Backend, Next, Shared, drive};
use crate::UNIT_BYTES;
use crate::frames::{Filling, Vendor};
use crate::seam::Opened;

/// How long a lost device is looked for before the stream fails, and how
/// often the adapters are walked meanwhile. Every restart measured came back
/// within four seconds, the display's GPU the slowest.
const FIND_AGAIN: Duration = Duration::from_secs(5);
const FIND_EVERY: Duration = Duration::from_millis(250);

impl Backend for d3d11::Backend<'_> {
    fn output(&self) -> Option<(u32, u32, Format)> {
        d3d11::Backend::output(self)
    }
    fn timings(&self) -> (u32, u32) {
        (self.decode_us, self.readback_us)
    }
    fn exports(&self) -> bool {
        self.splits()
    }
    fn take_to_slot(
        &mut self,
        filling: &mut Filling<'_>,
        (width, height, format): (u32, u32, Format),
    ) -> Result<Option<(Picture, u64)>, Fault> {
        // Textures a device refused are a picture lost, as a slot that does
        // not fit is, and the decoder is left to its next unit -- unless the
        // device is gone, which no later unit's decode need report.
        let Some(planes) = filling.textures_for(width, height, format) else {
            return if self.lost() {
                Err(Fault::DeviceLost)
            } else {
                Ok(None)
            };
        };
        self.take_to_textures(planes)
    }
}

impl Backend for amf::Backend<'_> {
    fn output(&self) -> Option<(u32, u32, Format)> {
        amf::Backend::output(self)
    }
    fn timings(&self) -> (u32, u32) {
        (self.decode_us, self.readback_us)
    }
    fn exports(&self) -> bool {
        self.splits()
    }
    fn take_to_slot(
        &mut self,
        filling: &mut Filling<'_>,
        (width, height, format): (u32, u32, Format),
    ) -> Result<Option<(Picture, u64)>, Fault> {
        // As the system's interface: textures refused are a picture lost,
        // unless their device is gone.
        let Some(planes) = filling.textures_for(width, height, format) else {
            return if self.lost() {
                Err(Fault::DeviceLost)
            } else {
                Ok(None)
            };
        };
        self.take_to_textures(planes)
    }
}

impl Backend for vpl::Backend<'_> {
    fn output(&self) -> Option<(u32, u32, Format)> {
        vpl::Backend::output(self)
    }
    fn timings(&self) -> (u32, u32) {
        (self.decode_us, self.readback_us)
    }
    fn exports(&self) -> bool {
        self.splits()
    }
    fn take_to_slot(
        &mut self,
        filling: &mut Filling<'_>,
        (width, height, format): (u32, u32, Format),
    ) -> Result<Option<(Picture, u64)>, Fault> {
        // As the system's interface: textures refused are a picture lost,
        // unless their device is gone.
        let Some(planes) = filling.textures_for(width, height, format) else {
            return if self.lost() {
                Err(Fault::DeviceLost)
            } else {
                Ok(None)
            };
        };
        self.take_to_textures(planes)
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
        self.exports_textures()
    }
    fn take_to_slot(
        &mut self,
        filling: &mut Filling<'_>,
        (width, height, format): (u32, u32, Format),
    ) -> Result<Option<(Picture, u64)>, Fault> {
        // As the system's interface: textures refused are a picture lost,
        // unless their device is gone.
        let Some(planes) = filling.registered_for(width, height, format) else {
            return if self.textures_lost() {
                Err(Fault::DeviceLost)
            } else {
                Ok(None)
            };
        };
        match self.take_to_textures(planes) {
            Err(_) if self.textures_lost() => Err(Fault::DeviceLost),
            taken => taken,
        }
    }
}

/// Open the decoder `opened` names and drive it until it returns. The
/// device, or the library pair, is opened here, on the decode thread, and
/// lives as long as the decoder built on it does.
pub(super) fn open(opened: Opened, shared: &Shared<'_>, replacing: bool) -> Next {
    match opened {
        Opened::D3d11 { luid, named, .. } => system_decoder(luid, named, shared, replacing),
        Opened::Nvdec { luid, .. } => vendor_decoder(luid, shared, replacing),
        Opened::Amf { luid, .. } => amd_decoder(luid, shared, replacing),
        Opened::Vpl { luid, .. } => intel_decoder(luid, shared, replacing),
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

/// The system's interface on the adapter `luid`, driven until it returns;
/// a device lost on the way is found again and the loop goes round on it.
fn system_decoder(mut luid: Luid, named: bool, shared: &Shared<'_>, mut replacing: bool) -> Next {
    let Ok(d3d11) = D3d11::load() else {
        return Next::Failed;
    };
    loop {
        // An adapter gone since it was chosen (a reset, a driver update)
        // no longer opens under its identity.
        let Ok(device) = d3d11.open(luid) else {
            return Next::Failed;
        };
        let device = Arc::new(device);
        let backend = d3d11::Backend::new(&device, shared.frames.ceiling());
        // A device that splits takes the session's device slots, whatever
        // kind the session asks for now, so a switch to handles has them.
        if let Some(fence) = backend.fence() {
            shared.frames.open_device(Arc::clone(&device), fence, None);
        }
        let lost = device.adapter.clone();
        match drive(backend, shared, replacing) {
            Next::Lost => {}
            other => return other,
        }
        lowlat_common::log_warn!(
            "client: decoder device lost, adapter={} name={:?}",
            lost.luid,
            lost.description
        );
        match find_again(&d3d11, &lost, named, shared) {
            Ok(again) => {
                lowlat_common::log_info!("client: decoder device back, adapter={again}");
                luid = again;
                replacing = true;
            }
            Err(next) => return next,
        }
    }
}

/// The vendor's interface on the adapter `luid`, driven until it returns.
/// The runtime's context is made current here, where every call against it
/// is made, and pictures are handed out as textures of a device of the
/// library's own on the same adapter, opened here too, where its immediate
/// context is used; without one, as planes only. A device lost fails the
/// stream: nothing of the runtime comes back in this process.
fn vendor_decoder(luid: Luid, shared: &Shared<'_>, replacing: bool) -> Next {
    let Ok(cuda) = Cuda::load() else {
        return Next::Failed;
    };
    let Ok(device) = cuda.device_for_luid(luid.value()) else {
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
    let Ok(d3d11) = D3d11::load() else {
        return Next::Failed;
    };
    // The queue's slots are registered with this runtime and may outlive
    // this thread while the application holds one, so the queue keeps its
    // own references to it and to the context.
    let cuda = Arc::new(cuda);
    let context = Arc::new(context);
    let mut backend = nvdec::Backend::new(&cuda, &cuvid, shared.frames.ceiling(), UNIT_BYTES);
    // Attached whatever kind the session asks for now, so a switch to
    // handles has the textures.
    let textures = cuda
        .has_graphics()
        .then(|| d3d11.open(luid).ok())
        .flatten()
        .and_then(|device| Some((device.fence(0).ok()?, device)));
    if let Some((fence, device)) = textures {
        let (device, fence) = (Arc::new(device), Arc::new(fence));
        let vendor = Vendor {
            context: Arc::clone(&context),
            cuda: Arc::clone(&cuda),
        };
        shared
            .frames
            .open_device(Arc::clone(&device), Arc::clone(&fence), Some(vendor));
        backend.attach_textures(device, fence);
    }
    drive(backend, shared, replacing)
}

/// AMD's decoder on the adapter `luid`, on a device of the library's own
/// there, driven until it returns; a device lost on the way is found again
/// on the same GPU -- never another, where the decoder is not -- and the
/// runtime made again on it.
fn amd_decoder(mut luid: Luid, shared: &Shared<'_>, mut replacing: bool) -> Next {
    let Ok(d3d11) = D3d11::load() else {
        return Next::Failed;
    };
    loop {
        let Ok(device) = d3d11.open(luid) else {
            return Next::Failed;
        };
        let Ok(runtime) = Amf::load() else {
            return Next::Failed;
        };
        let device = Arc::new(device);
        let context = match runtime.context(&device) {
            Ok(context) => context,
            Err(e) => {
                lowlat_common::log_warn!(
                    "client: amd decoder not made, error={}",
                    amf::Error::from(e)
                );
                return Next::Failed;
            }
        };
        let backend =
            match amf::Backend::new(&runtime, &context, shared.frames.ceiling(), UNIT_BYTES) {
                Ok(backend) => backend,
                Err(e) => {
                    lowlat_common::log_warn!("client: amd decoder not made, error={e}");
                    return Next::Failed;
                }
            };
        // A device that splits takes the session's device slots, whatever
        // kind the session asks for now, so a switch to handles has them.
        if let Some(fence) = backend.fence() {
            shared.frames.open_device(Arc::clone(&device), fence, None);
        }
        let lost = device.adapter.clone();
        match drive(backend, shared, replacing) {
            Next::Lost => {}
            other => return other,
        }
        lowlat_common::log_warn!(
            "client: decoder device lost, adapter={} name={:?}",
            lost.luid,
            lost.description
        );
        match find_again(&d3d11, &lost, true, shared) {
            Ok(again) => {
                lowlat_common::log_info!("client: decoder device back, adapter={again}");
                luid = again;
                replacing = true;
            }
            Err(next) => return next,
        }
    }
}

/// Intel's decoder on the adapter `luid`, in a session of the runtime its
/// driver installs -- on a device of the library's own there for the
/// current runtime, into memory of the backend's own for the older one --
/// driven until it returns; a device lost on the way is found again on the
/// same GPU and a session made again there.
fn intel_decoder(mut luid: Luid, shared: &Shared<'_>, mut replacing: bool) -> Next {
    let Ok(d3d11) = D3d11::load() else {
        return Next::Failed;
    };
    loop {
        let Ok(device) = d3d11.open(luid) else {
            return Next::Failed;
        };
        let Ok(Some(index)) = d3d11.plain_index(luid) else {
            return Next::Failed;
        };
        let Ok(runtime) = Vpl::for_adapter(&device.adapter) else {
            return Next::Failed;
        };
        let device = Arc::new(device);
        let kind = runtime.runtime();
        let session = match kind {
            Runtime::Current => runtime.session(&device, index),
            Runtime::Older => runtime.system_session(index),
        };
        let session = match session {
            Ok(session) => session,
            Err(e) => {
                lowlat_common::log_warn!("client: intel decoder not made, error={e}");
                return Next::Failed;
            }
        };
        let decodes_on = (kind == Runtime::Current).then_some(&*device);
        let backend = match vpl::Backend::new(
            &session,
            kind,
            decodes_on,
            shared.frames.ceiling(),
            UNIT_BYTES,
        ) {
            Ok(backend) => backend,
            Err(e) => {
                lowlat_common::log_warn!("client: intel decoder not made, error={e}");
                return Next::Failed;
            }
        };
        // A device that splits takes the session's device slots, whatever
        // kind the session asks for now, so a switch to handles has them.
        if let Some(fence) = backend.fence() {
            shared.frames.open_device(Arc::clone(&device), fence, None);
        }
        let lost = device.adapter.clone();
        match drive(backend, shared, replacing) {
            Next::Lost => {}
            other => return other,
        }
        lowlat_common::log_warn!(
            "client: decoder device lost, adapter={} name={:?}",
            lost.luid,
            lost.description
        );
        match find_again(&d3d11, &lost, true, shared) {
            Ok(again) => {
                lowlat_common::log_info!("client: decoder device back, adapter={again}");
                luid = again;
                replacing = true;
            }
            Err(next) => return next,
        }
    }
}

/// The GPU a lost device is opened on again: the same hardware under
/// whatever identity it came back with, or -- with `any`, which a session
/// nobody placed asks once the search has run its course -- the first
/// adapter offered. A virtual display's adapter, which shares its GPU's
/// numbers, is never offered.
fn pick(adapters: &[Adapter], lost: &Adapter, any: bool) -> Option<Luid> {
    let mut offered = adapters.iter().filter(|a| a.decodes_here());
    let same = adapters
        .iter()
        .filter(|a| a.decodes_here())
        .find(|a| a.same_hardware(lost));
    same.or_else(|| if any { offered.next() } else { None })
        .map(|a| a.luid)
}

/// Look for the lost device's GPU until it opens, the session ends, another
/// decoder is chosen, or the time runs out -- and then, for a session nobody
/// placed, for any GPU once: the identity to open on, or what the thread
/// does instead.
fn find_again(
    d3d11: &D3d11,
    lost: &Adapter,
    named: bool,
    shared: &Shared<'_>,
) -> Result<Luid, Next> {
    let began = lowlat_common::clock::Time::now();
    let mut switched = shared.telemetry.switch.load(Ordering::Acquire);
    let mut walked: Option<lowlat_common::clock::Time> = None;
    loop {
        if shared.stopping.load(Ordering::Acquire) {
            return Err(Next::Stop);
        }
        if let Some(choice) = shared.switch(&mut switched) {
            return Err(Next::Switch(choice));
        }
        // Nothing decodes meanwhile: what arrives is dropped, and the
        // keyframe is asked for once a decoder exists again.
        while shared.units.take().is_some() {}
        let done = lowlat_common::clock::elapsed_ms(began) >= FIND_AGAIN.as_secs_f64() * 1000.0;
        let due = done
            || walked.is_none_or(|at| {
                lowlat_common::clock::elapsed_ms(at) >= FIND_EVERY.as_secs_f64() * 1000.0
            });
        if due {
            walked = Some(lowlat_common::clock::Time::now());
            let found = d3d11
                .adapters()
                .ok()
                .and_then(|adapters| pick(&adapters, lost, done && !named));
            // An adapter can be listed before its device opens.
            if let Some(luid) = found.filter(|l| d3d11.open(*l).is_ok_and(|d| !d.lost())) {
                return Ok(luid);
            }
        }
        if done {
            return Err(Next::Failed);
        }
        shared.units.wait(FIND_EVERY);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adapter(low: u32, device: u32, flags: (bool, bool)) -> Adapter {
        let (renders, indirect) = flags;
        Adapter {
            luid: Luid { high: 0, low },
            vendor: 0x10de,
            device,
            subsystem: 0x1234,
            revision: 0xa1,
            description: String::from("a GPU"),
            driver: None,
            renders,
            software: false,
            indirect,
            integrated: false,
        }
    }

    /// **The same GPU is found under its new identity**, and nothing else
    /// while the search runs: a virtual display sharing its numbers is passed
    /// over, and another GPU is taken only when the search has run its course
    /// for a session nobody placed -- a display's GPU comes back in seconds,
    /// and a session moved off it at once would stay on the other for good.
    #[test]
    fn a_lost_gpu_is_found_by_its_hardware() {
        let lost = adapter(0x10, 0x2d05, (true, false));
        let virtual_display = adapter(0x20, 0x2d05, (false, true));
        let other = adapter(0x30, 0x56a5, (true, false));
        let back = adapter(0x40, 0x2d05, (true, false));

        let listed = [virtual_display.clone(), other.clone(), back.clone()];
        assert_eq!(
            pick(&listed, &lost, false),
            Some(back.luid),
            "the same hardware"
        );
        assert_eq!(
            pick(&listed, &lost, true),
            Some(back.luid),
            "the same hardware first, at the end too"
        );

        let not_back = [virtual_display, other.clone()];
        assert_eq!(
            pick(&not_back, &lost, false),
            None,
            "moved to another GPU while the search ran"
        );
        assert_eq!(
            pick(&not_back, &lost, true),
            Some(other.luid),
            "an unplaced session left without a decoder at the end"
        );
    }
}
