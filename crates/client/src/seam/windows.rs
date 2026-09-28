//! Which decoders a configuration opens, on Windows, and on which GPU: the
//! system's video decoding interface on an adapter named by its identity,
//! then the machine's own codec library. The vendor's decoder comes with
//! its step of docs/impl-plan-windows.md, at its place in the automatic
//! order.

use std::path::PathBuf;

use lowlat_decode::d3d11;
use lowlat_drivers::d3d11::{D3d11, Luid};

use super::{DecoderStage, Error, most_telling, probe_software, software_dir};
use crate::config::{Backend, Caps, Decoding, FrameKind};

/// The decoder settled at creation: which backend, on which GPU.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Opened {
    /// The system's decoding interface, on the adapter with this identity.
    D3d11 {
        luid: Luid,
        /// The application named the GPU; one it did not is found again
        /// anywhere if its own does not come back after a loss.
        named: bool,
        /// The device can hand pictures out as textures: it has a fence
        /// (Windows 10 1703 on).
        handles: bool,
    },
    /// The machine's own codec library, from the directory named or from
    /// the search of its own. Opened again on the decode thread, which
    /// finds the same pair the probe found.
    Software(Option<PathBuf>),
}

impl Opened {
    /// The backend this is, as a configuration names it.
    pub fn backend(&self) -> Backend {
        match self {
            Self::D3d11 { .. } => Backend::Vaapi,
            Self::Software(_) => Backend::Software,
        }
    }

    /// Whether it hands pictures out as a handle.
    pub fn exports(&self) -> bool {
        match self {
            Self::D3d11 { handles, .. } => *handles,
            Self::Software(_) => false,
        }
    }
}

/// Probe the system's interface on one adapter: a device of its own made
/// there, and a real decoder built per combination; with whether the device
/// can hand pictures out as textures.
fn probe_d3d11(d3d11: &D3d11, luid: Luid) -> Result<(Caps, bool), DecoderStage> {
    let device = d3d11.open(luid).map_err(|_| DecoderStage::Device)?;
    let caps = d3d11::caps(&device);
    if caps.any() {
        Ok((caps, device.has_fences()))
    } else {
        Err(DecoderStage::Profile)
    }
}

/// The system's interface on the adapter a device name spells, or on the
/// first adapter offered that decodes, high-performance first; the stage the
/// walk met otherwise. With `handles`, an adapter whose device cannot hand
/// pictures out as textures is refused as the handle kind is.
fn open_d3d11(named: Option<&str>, handles: bool) -> Result<(Opened, Caps), DecoderStage> {
    let d3d11 = D3d11::load().map_err(|_| DecoderStage::Runtime)?;
    let probe = |luid| {
        let (caps, exports) = probe_d3d11(&d3d11, luid)?;
        if handles && !exports {
            return Err(DecoderStage::Unsupported);
        }
        let opened = Opened::D3d11 {
            luid,
            named: named.is_some(),
            handles: exports,
        };
        Ok((opened, caps))
    };
    if let Some(named) = named {
        return probe(Luid::parse(named).ok_or(DecoderStage::Device)?);
    }
    let adapters = d3d11.adapters().map_err(|_| DecoderStage::Runtime)?;
    let mut last = DecoderStage::Device;
    for adapter in adapters.iter().filter(|a| a.decodes_here()) {
        match probe(adapter.luid) {
            Ok(found) => return Ok(found),
            Err(stage) => last = most_telling(last, stage),
        }
    }
    Err(last)
}

/// The decoder a configuration settles on, probed once here, with what it
/// decodes. **Strict where a kind is named**: the kind opens on the GPU
/// named or the stage is the answer. **In order where none is**: the
/// system's interface on the GPU named or the first that decodes, then
/// software. The handle kind needs a device that can hand pictures out as
/// textures, which only the system's interface does here; the vendor's
/// decoder is refused as not in the build.
pub(crate) fn choose(decoding: &Decoding) -> Result<(Option<Opened>, Caps), Error> {
    let named = (!decoding.device.is_empty()).then_some(decoding.device.as_str());
    let system = |named| open_d3d11(named, decoding.kind == FrameKind::Handle);
    match (decoding.kind, decoding.backend) {
        (_, Backend::None) => Ok((None, Caps::default())),
        (_, Backend::Nvdec) | (FrameKind::Handle, Backend::Software) => {
            Err(Error::Decoder(DecoderStage::Unsupported))
        }
        (_, Backend::Vaapi) => {
            let (opened, caps) = system(named).map_err(Error::Decoder)?;
            Ok((Some(opened), caps))
        }
        (FrameKind::Handle, Backend::Auto) => {
            // Only the system's interface hands out textures here.
            let (opened, caps) = system(named).map_err(Error::Decoder)?;
            Ok((Some(opened), caps))
        }
        (FrameKind::Planes, Backend::Software) => {
            let dir = software_dir(&decoding.device);
            let caps = probe_software(dir.as_deref()).map_err(Error::Decoder)?;
            Ok((Some(Opened::Software(dir)), caps))
        }
        (FrameKind::Planes, Backend::Auto) => {
            let last = match system(named) {
                Ok((opened, caps)) => return Ok((Some(opened), caps)),
                Err(stage) => stage,
            };
            // Then software, which knows nothing of GPUs.
            match probe_software(None) {
                Ok(caps) => Ok((Some(Opened::Software(None)), caps)),
                Err(stage) => Err(Error::Decoder(most_telling(last, stage))),
            }
        }
    }
}
