//! The decoders a stream is opened on, on Windows: the system's video
//! decoding interface on an adapter, or the machine's own codec library.
//!
//! **A device lost is looked for again**, not the end of the stream: a
//! driver update, a reset or a restart of the GPU's driver takes the device
//! away and brings the GPU back under a new identity. The decode thread
//! finds the same GPU by its hardware -- or, for a session nobody placed,
//! the first the system offers -- opens there, and the replacement asks for
//! its keyframe; pictures then say which GPU they are on.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use lowlat_decode::{Fault, Format, Picture, d3d11, software};
use lowlat_drivers::d3d11::{Adapter, D3d11, Luid};
use lowlat_drivers::lavc::Lavc;

use super::{Backend, Next, Shared, drive};
use crate::frames::Filling;
use crate::seam::Opened;

/// How long a lost device is looked for before the stream fails, and how
/// often the adapters are walked meanwhile.
const FIND_AGAIN: Duration = Duration::from_secs(10);
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

/// Open the decoder `opened` names and drive it until it returns. The
/// device, or the library pair, is opened here, on the decode thread, and
/// lives as long as the decoder built on it does.
pub(super) fn open(opened: Opened, shared: &Shared<'_>, replacing: bool) -> Next {
    match opened {
        Opened::D3d11 { luid, named, .. } => system_decoder(luid, named, shared, replacing),
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
            shared.frames.open_device(Arc::clone(&device), fence);
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

/// The GPU a lost device is opened on again: the same hardware under
/// whatever identity it came back with, or -- for a session nobody placed --
/// the first adapter offered. A virtual display's adapter, which shares its
/// GPU's numbers, is never offered.
fn pick(adapters: &[Adapter], lost: &Adapter, named: bool) -> Option<Luid> {
    let mut offered = adapters.iter().filter(|a| a.decodes_here());
    let same = adapters
        .iter()
        .filter(|a| a.decodes_here())
        .find(|a| a.same_hardware(lost));
    same.or_else(|| if named { None } else { offered.next() })
        .map(|a| a.luid)
}

/// Look for the lost device's GPU until it opens, the session ends, another
/// decoder is chosen, or the time runs out: the identity to open on, or
/// what the thread does instead.
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
        let due = walked.is_none_or(|at| {
            lowlat_common::clock::elapsed_ms(at) >= FIND_EVERY.as_secs_f64() * 1000.0
        });
        if due {
            walked = Some(lowlat_common::clock::Time::now());
            let found = d3d11
                .adapters()
                .ok()
                .and_then(|adapters| pick(&adapters, lost, named));
            // An adapter can be listed before its device opens.
            if let Some(luid) = found.filter(|l| d3d11.open(*l).is_ok_and(|d| !d.lost())) {
                return Ok(luid);
            }
        }
        if lowlat_common::clock::elapsed_ms(began) >= FIND_AGAIN.as_secs_f64() * 1000.0 {
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

    /// **The same GPU is found under its new identity**; a virtual display
    /// sharing its numbers is passed over; a session nobody placed falls to
    /// the first adapter offered when the GPU is not back, and one placed
    /// does not.
    #[test]
    fn a_lost_gpu_is_found_by_its_hardware() {
        let lost = adapter(0x10, 0x2d05, (true, false));
        let virtual_display = adapter(0x20, 0x2d05, (false, true));
        let other = adapter(0x30, 0x56a5, (true, false));
        let back = adapter(0x40, 0x2d05, (true, false));

        let listed = [virtual_display.clone(), other.clone(), back.clone()];
        assert_eq!(
            pick(&listed, &lost, true),
            Some(back.luid),
            "the same hardware"
        );
        assert_eq!(pick(&listed, &lost, false), Some(back.luid));

        let not_back = [virtual_display, other.clone()];
        assert_eq!(pick(&not_back, &lost, true), None, "a placed session moved");
        assert_eq!(
            pick(&not_back, &lost, false),
            Some(other.luid),
            "an unplaced session stayed without a decoder"
        );
    }
}
