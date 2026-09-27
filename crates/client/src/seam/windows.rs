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
    D3d11(Luid),
    /// The machine's own codec library, from the directory named or from
    /// the search of its own. Opened again on the decode thread, which
    /// finds the same pair the probe found.
    Software(Option<PathBuf>),
}

impl Opened {
    /// The backend this is, as a configuration names it.
    pub fn backend(&self) -> Backend {
        match self {
            Self::D3d11(_) => Backend::Vaapi,
            Self::Software(_) => Backend::Software,
        }
    }
}

/// Probe the system's interface on one adapter: a device of its own made
/// there, and a real decoder built per combination.
fn probe_d3d11(d3d11: &D3d11, luid: Luid) -> Result<Caps, DecoderStage> {
    let device = d3d11.open(luid).map_err(|_| DecoderStage::Device)?;
    let caps = d3d11::caps(&device);
    if caps.any() {
        Ok(caps)
    } else {
        Err(DecoderStage::Profile)
    }
}

/// The system's interface on the adapter a device name spells, or on the
/// first adapter offered that decodes; the stage the walk met otherwise.
fn open_d3d11(named: Option<&str>) -> Result<(Luid, Caps), DecoderStage> {
    let d3d11 = D3d11::load().map_err(|_| DecoderStage::Runtime)?;
    if let Some(named) = named {
        let luid = Luid::parse(named).ok_or(DecoderStage::Device)?;
        return probe_d3d11(&d3d11, luid).map(|caps| (luid, caps));
    }
    let adapters = d3d11.adapters().map_err(|_| DecoderStage::Runtime)?;
    let mut last = DecoderStage::Device;
    for adapter in adapters.iter().filter(|a| a.decodes_here()) {
        match probe_d3d11(&d3d11, adapter.luid) {
            Ok(caps) => return Ok((adapter.luid, caps)),
            Err(stage) => last = most_telling(last, stage),
        }
    }
    Err(last)
}

/// The decoder a configuration settles on, probed once here, with what it
/// decodes. **Strict where a kind is named**: the kind opens on the GPU
/// named or the stage is the answer. **In order where none is**: the
/// system's interface on the GPU named or the first that decodes, then
/// software. The vendor's decoder and the handle kind are refused as not in
/// the build.
pub(crate) fn choose(decoding: &Decoding) -> Result<(Option<Opened>, Caps), Error> {
    let named = (!decoding.device.is_empty()).then_some(decoding.device.as_str());
    match (decoding.kind, decoding.backend) {
        (_, Backend::None) => Ok((None, Caps::default())),
        (FrameKind::Handle, _) | (_, Backend::Nvdec) => {
            Err(Error::Decoder(DecoderStage::Unsupported))
        }
        (FrameKind::Planes, Backend::Vaapi) => {
            let (luid, caps) = open_d3d11(named).map_err(Error::Decoder)?;
            Ok((Some(Opened::D3d11(luid)), caps))
        }
        (FrameKind::Planes, Backend::Software) => {
            let dir = software_dir(&decoding.device);
            let caps = probe_software(dir.as_deref()).map_err(Error::Decoder)?;
            Ok((Some(Opened::Software(dir)), caps))
        }
        (FrameKind::Planes, Backend::Auto) => {
            let last = match open_d3d11(named) {
                Ok((luid, caps)) => return Ok((Some(Opened::D3d11(luid)), caps)),
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
