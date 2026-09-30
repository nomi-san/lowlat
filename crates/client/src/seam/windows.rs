//! Which decoders a configuration opens, on Windows, and on which GPU: the
//! system's video decoding interface on an adapter named by its identity,
//! then the vendor's on its own GPUs, then the machine's own codec library.

use std::path::PathBuf;

use lowlat_decode::{d3d11, nvdec};
use lowlat_drivers::cuda::{Context, Cuda};
use lowlat_drivers::cuvid::Cuvid;
use lowlat_drivers::d3d11::{Adapter, D3d11, Luid};
use lowlat_drivers::ffi::d3d11::DXGI_FORMAT_R8_UNORM;

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
    /// The vendor's interface, on the adapter with this identity.
    Nvdec {
        luid: Luid,
        /// Its pictures can be handed out as textures: the runtime writes a
        /// texture of a device on the adapter, which has a fence.
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
            Self::Nvdec { .. } => Backend::Nvdec,
            Self::Software(_) => Backend::Software,
        }
    }

    /// Whether it hands pictures out as a handle.
    pub fn exports(&self) -> bool {
        match self {
            Self::D3d11 { handles, .. } | Self::Nvdec { handles, .. } => *handles,
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

/// Whether the vendor's runtime writes a texture of a device of the
/// library's own on the adapter `luid`, in `context`, and the device has
/// the fence that says when: a texture made and registered, not a version
/// asked.
pub(crate) fn writes_textures(cuda: &Cuda, context: &Context, luid: Luid) -> bool {
    if !cuda.has_graphics() {
        return false;
    }
    let Ok(d3d11) = D3d11::load() else {
        return false;
    };
    let Ok(device) = d3d11.open(luid) else {
        return false;
    };
    device.has_fences()
        && device
            .shared_texture(DXGI_FORMAT_R8_UNORM, 64, 64)
            .is_ok_and(|texture| {
                // SAFETY: a live texture of a device on the adapter the
                // context's device is; the registration drops at the end of
                // this statement, before the texture and the context.
                unsafe { cuda.register_texture(context, texture.texture().cast()) }.is_ok()
            })
}

/// Probe the vendor's interface on one adapter: the runtimes loaded, the
/// context made current here, a real decoder built per combination; with
/// whether it can hand pictures out as textures.
fn probe_nvdec(luid: Luid) -> Result<(Caps, bool), DecoderStage> {
    let cuda = Cuda::load().map_err(|_| DecoderStage::Runtime)?;
    let device = cuda
        .device_for_luid(luid.value())
        .map_err(|_| DecoderStage::Device)?;
    let context = cuda
        .retain_primary(&device)
        .map_err(|_| DecoderStage::Device)?;
    context.make_current().map_err(|_| DecoderStage::Device)?;
    let caps = Cuvid::load().map(|cuvid| nvdec::caps(&cuvid));
    // The application's thread, left as it was found.
    let _ = context.release_current();
    let caps = caps.map_err(|_| DecoderStage::Runtime)?;
    if !caps.any() {
        return Err(DecoderStage::Profile);
    }
    Ok((caps, writes_textures(&cuda, &context, luid)))
}

/// The vendor's interface on the adapter a device name spells, or on the
/// first of its GPUs offered, high-performance first; the stage the walk
/// met otherwise. With `handles`, one that cannot hand pictures out as
/// textures is refused as the handle kind is.
fn open_nvdec(named: Option<&str>, handles: bool) -> Result<(Opened, Caps), DecoderStage> {
    let probe = |luid| {
        let (caps, exports) = probe_nvdec(luid)?;
        if handles && !exports {
            return Err(DecoderStage::Unsupported);
        }
        let opened = Opened::Nvdec {
            luid,
            handles: exports,
        };
        Ok((opened, caps))
    };
    if let Some(named) = named {
        return probe(Luid::parse(named).ok_or(DecoderStage::Device)?);
    }
    let d3d11 = D3d11::load().map_err(|_| DecoderStage::Runtime)?;
    let adapters = d3d11.adapters().map_err(|_| DecoderStage::Runtime)?;
    let mut last = DecoderStage::Device;
    let theirs = adapters
        .iter()
        .filter(|a| a.decodes_here() && a.maker() == Some("NVIDIA"));
    for adapter in theirs {
        match probe(adapter.luid) {
            Ok(found) => return Ok(found),
            Err(stage) => last = most_telling(last, stage),
        }
    }
    Err(last)
}

/// Whether a session lands on one of the vendor's GPUs: the one named, or
/// else the first of `adapters` offered.
fn lands_on_the_vendors(adapters: &[Adapter], named: Option<&str>) -> bool {
    let mut offered = adapters.iter().filter(|a| a.decodes_here());
    let target = match named {
        Some(named) => Luid::parse(named).and_then(|luid| offered.find(|a| a.luid == luid)),
        None => offered.next(),
    };
    target.is_some_and(|a| a.maker() == Some("NVIDIA"))
}

/// The decoder a configuration settles on, probed once here, with what it
/// decodes. **Strict where a kind is named**: the kind opens on the GPU
/// named or the stage is the answer. **In order where none is**: the two
/// hardware decoders on the GPU named or the first that decodes -- the
/// vendor's first on one of its own GPUs, where it measured faster end to
/// end than the system's interface, both codecs and both kinds, and the
/// system's first everywhere else -- then software, which the handle kind,
/// needing textures, never reaches.
pub(crate) fn choose(decoding: &Decoding) -> Result<(Option<Opened>, Caps), Error> {
    let named = (!decoding.device.is_empty()).then_some(decoding.device.as_str());
    let handles = decoding.kind == FrameKind::Handle;
    let system = |named| open_d3d11(named, handles);
    let vendor = |named| open_nvdec(named, handles);
    // The two hardware decoders in order, the stage the walk met otherwise.
    let either = || {
        let vendor_first = D3d11::load()
            .and_then(|d3d11| d3d11.adapters())
            .is_ok_and(|adapters| lands_on_the_vendors(&adapters, named));
        if vendor_first {
            vendor(named).or_else(|first| system(named).map_err(|then| most_telling(first, then)))
        } else {
            system(named).or_else(|first| vendor(named).map_err(|then| most_telling(first, then)))
        }
    };
    match (decoding.kind, decoding.backend) {
        (_, Backend::None) => Ok((None, Caps::default())),
        (FrameKind::Handle, Backend::Software) => Err(Error::Decoder(DecoderStage::Unsupported)),
        (_, Backend::Vaapi) => {
            let (opened, caps) = system(named).map_err(Error::Decoder)?;
            Ok((Some(opened), caps))
        }
        (_, Backend::Nvdec) => {
            let (opened, caps) = vendor(named).map_err(Error::Decoder)?;
            Ok((Some(opened), caps))
        }
        (FrameKind::Handle, Backend::Auto) => {
            let (opened, caps) = either().map_err(Error::Decoder)?;
            Ok((Some(opened), caps))
        }
        (FrameKind::Planes, Backend::Software) => {
            let dir = software_dir(&decoding.device);
            let caps = probe_software(dir.as_deref()).map_err(Error::Decoder)?;
            Ok((Some(Opened::Software(dir)), caps))
        }
        (FrameKind::Planes, Backend::Auto) => {
            let last = match either() {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn adapter(low: u32, vendor: u32, indirect: bool) -> Adapter {
        Adapter {
            luid: Luid { high: 0, low },
            vendor,
            device: 0x1234,
            subsystem: 0x5678,
            revision: 0xa1,
            description: String::from("a GPU"),
            driver: None,
            renders: true,
            software: false,
            indirect,
            integrated: false,
        }
    }

    /// **The vendor's decoder goes first on the vendor's own GPU**: the one
    /// named, or the first offered where none is. Anywhere else the system's
    /// interface goes first, and so it does for a name no GPU has; a virtual
    /// display's adapter under the vendor's numbers is never where a session
    /// lands.
    #[test]
    fn the_vendors_decoder_goes_first_on_its_own_gpu() {
        let nvidia = adapter(0x10, 0x10de, false);
        let intel = adapter(0x20, 0x8086, false);
        let virtual_display = adapter(0x30, 0x10de, true);
        let name = |a: &Adapter| a.luid.to_string();

        let nvidia_first = [nvidia.clone(), intel.clone()];
        assert!(lands_on_the_vendors(&nvidia_first, None));
        assert!(!lands_on_the_vendors(&nvidia_first, Some(&name(&intel))));
        let intel_first = [intel.clone(), nvidia.clone()];
        assert!(!lands_on_the_vendors(&intel_first, None));
        assert!(lands_on_the_vendors(&intel_first, Some(&name(&nvidia))));
        assert!(!lands_on_the_vendors(
            &nvidia_first,
            Some("luid:7fffffff:00000001")
        ));
        let behind_a_virtual_display = [virtual_display.clone(), intel];
        assert!(!lands_on_the_vendors(&behind_a_virtual_display, None));
        assert!(!lands_on_the_vendors(
            &behind_a_virtual_display,
            Some(&name(&virtual_display))
        ));
    }
}
