//! Which decoders a configuration opens, on Linux, and on which device: the
//! open stack on a render node, the vendor's interface on the card behind
//! one, the machine's own codec library.

use std::ffi::CString;
use std::path::PathBuf;

use lowlat_decode::{nvdec, vaapi};
use lowlat_drivers::cuda::{self, PciAddress};
use lowlat_drivers::cuvid;

use super::{DecoderStage, Error, most_telling, probe_software, software_dir};
use crate::config::{Backend, Caps, Decoding, FrameKind};

/// The decoder settled at creation: which backend, on which device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Opened {
    /// The open stack, on a render node.
    Vaapi(CString),
    /// The vendor's interface, on the device at an address, or the first.
    Nvdec(Option<PciAddress>),
    /// The machine's own codec library, from the directory named or from
    /// the search of its own. Opened again on the decode thread, which
    /// finds the same pair the probe found.
    Software(Option<PathBuf>),
}

impl Opened {
    /// The backend this is, as a configuration names it.
    pub fn backend(&self) -> Backend {
        match self {
            Self::Vaapi(_) => Backend::Vaapi,
            Self::Nvdec(_) => Backend::Nvdec,
            Self::Software(_) => Backend::Software,
        }
    }
}

/// Which stage a probe's refusal names.
fn stage_of(error: &vaapi::Error) -> DecoderStage {
    match error {
        vaapi::Error::Runtime(
            vaapi::RuntimeError::Unavailable | vaapi::RuntimeError::MissingSymbol,
        ) => DecoderStage::Runtime,
        vaapi::Error::NoProfile => DecoderStage::Profile,
        _ => DecoderStage::Device,
    }
}

/// The card behind a render node, as the compute runtime addresses it:
/// the node's device link in the kernel's tree names the bus address.
pub(crate) fn address_of(node: &str) -> Option<PciAddress> {
    let name = std::path::Path::new(node).file_name()?.to_str()?;
    let link = std::fs::read_link(format!("/sys/class/drm/{name}/device")).ok()?;
    PciAddress::parse(link.file_name()?.to_str()?)
}

/// The maker of the card behind a render node, by the vendor number the
/// kernel's tree carries for its device; none for a node that is not there
/// or a maker this crate does not name.
pub(crate) fn vendor_of(node: &str) -> Option<&'static str> {
    let name = std::path::Path::new(node).file_name()?.to_str()?;
    let vendor = std::fs::read_to_string(format!("/sys/class/drm/{name}/device/vendor")).ok()?;
    match vendor.trim() {
        "0x8086" => Some("Intel"),
        "0x1002" => Some("AMD"),
        "0x10de" => Some("NVIDIA"),
        _ => None,
    }
}

/// The render node on a card, by its bus address: the inverse of
/// [`address_of`], over the nodes this crate looks at.
pub(crate) fn node_of(address: PciAddress) -> Option<&'static str> {
    RENDER_NODES
        .iter()
        .copied()
        .find(|node| address_of(node) == Some(address))
}

/// Probe the vendor's interface on `address`, or on the first device: the
/// runtimes loaded, the context made current here, a real decoder built per
/// combination.
fn probe_nvdec(address: Option<PciAddress>) -> Result<Caps, DecoderStage> {
    let cuda = cuda::Cuda::load().map_err(|_| DecoderStage::Runtime)?;
    let device = match address {
        Some(address) => cuda.device_at(address),
        None => cuda.any_device(),
    }
    .map_err(|_| DecoderStage::Device)?;
    let context = cuda
        .retain_primary(&device)
        .map_err(|_| DecoderStage::Device)?;
    context.make_current().map_err(|_| DecoderStage::Device)?;
    let loaded = cuvid::Cuvid::load().map_err(|_| DecoderStage::Runtime)?;
    let caps = nvdec::caps(&loaded);
    // The application's thread, left as it was found.
    let _ = context.release_current();
    if caps.any() {
        Ok(caps)
    } else {
        Err(DecoderStage::Profile)
    }
}

/// Probe the open stack on one render node.
fn probe_vaapi(node: &CString) -> Result<Caps, DecoderStage> {
    let caps = vaapi::probe(node).map_err(|e| stage_of(&e))?;
    if caps.any() {
        Ok(caps)
    } else {
        Err(DecoderStage::Profile)
    }
}

/// The decoder a configuration settles on, probed once here, with what it
/// decodes. **Strict where a kind is named**: the kind opens on the device
/// named or the stage is the answer. **In order where none is**: the open
/// stack on the node named or the first that decodes, then the vendor's
/// interface on the card behind that node or any, then software -- so a
/// machine with any hardware decoder never reaches software, and the stage
/// reported is the most telling one the walk met.
pub(crate) fn choose(decoding: &Decoding) -> Result<(Option<Opened>, Caps), Error> {
    let node = (!decoding.device.is_empty()).then_some(decoding.device.as_str());
    match (decoding.kind, decoding.backend) {
        (_, Backend::None) => Ok((None, Caps::default())),
        // Only the vendor backend exports a handle, so asking for one
        // settles the choice on it: on the card behind the node named, or
        // any, or nothing.
        (FrameKind::Handle, Backend::Vaapi | Backend::Software) => {
            Err(Error::Decoder(DecoderStage::Unsupported))
        }
        (FrameKind::Handle, Backend::Auto) => {
            let address = node
                .map(|node| address_of(node).ok_or(Error::Decoder(DecoderStage::Device)))
                .transpose()?;
            let caps =
                probe_nvdec(address).map_err(|_| Error::Decoder(DecoderStage::Unsupported))?;
            Ok((Some(Opened::Nvdec(address)), caps))
        }
        (_, Backend::Nvdec) => {
            // The device is named as a render node, as for the open stack;
            // the card behind it is what the runtime takes.
            let address = node
                .map(|node| address_of(node).ok_or(Error::Decoder(DecoderStage::Device)))
                .transpose()?;
            let caps = probe_nvdec(address).map_err(Error::Decoder)?;
            Ok((Some(Opened::Nvdec(address)), caps))
        }
        (_, Backend::Vaapi) => {
            let path = match node {
                Some(node) => vaapi_path(node)?,
                None => {
                    return first_vaapi()
                        .map(|(path, caps)| (Some(Opened::Vaapi(path)), caps))
                        .map_err(Error::Decoder);
                }
            };
            let caps = probe_vaapi(&path).map_err(Error::Decoder)?;
            Ok((Some(Opened::Vaapi(path)), caps))
        }
        (_, Backend::Software) => {
            let dir = software_dir(&decoding.device);
            let caps = probe_software(dir.as_deref()).map_err(Error::Decoder)?;
            Ok((Some(Opened::Software(dir)), caps))
        }
        (FrameKind::Planes, Backend::Auto) => {
            // The open stack first, on the node named or the first node that
            // decodes.
            let open = match node {
                Some(node) => vaapi_path(node).and_then(|path| {
                    probe_vaapi(&path)
                        .map(|caps| (path, caps))
                        .map_err(Error::Decoder)
                }),
                None => first_vaapi().map_err(Error::Decoder),
            };
            let mut last = match open {
                Ok((path, caps)) => return Ok((Some(Opened::Vaapi(path)), caps)),
                Err(Error::Decoder(stage)) => stage,
                Err(other) => return Err(other),
            };
            // Then the vendor's, on the card behind that node or any.
            let address = node.and_then(address_of);
            if node.is_none() || address.is_some() {
                match probe_nvdec(address) {
                    Ok(caps) => return Ok((Some(Opened::Nvdec(address)), caps)),
                    Err(stage) => last = most_telling(last, stage),
                }
            }
            // Then software, which knows nothing of nodes.
            match probe_software(None) {
                Ok(caps) => Ok((Some(Opened::Software(None)), caps)),
                Err(stage) => Err(Error::Decoder(most_telling(last, stage))),
            }
        }
    }
}

/// A render node's path for the open stack.
fn vaapi_path(node: &str) -> Result<CString, Error> {
    CString::new(node).map_err(|_| Error::Decoder(DecoderStage::Device))
}

/// The first render node the open stack decodes on, or the stage the walk
/// met: the runtime absent, or every node refusing or decoding nothing.
fn first_vaapi() -> Result<(CString, Caps), DecoderStage> {
    let mut last = DecoderStage::Device;
    for candidate in RENDER_NODES {
        let Ok(path) = CString::new(candidate) else {
            continue;
        };
        match probe_vaapi(&path) {
            Ok(caps) => return Ok((path, caps)),
            Err(stage) => last = most_telling(last, stage),
        }
    }
    Err(last)
}

/// Where the first render node that decodes is looked for.
pub(crate) const RENDER_NODES: [&str; 8] = [
    "/dev/dri/renderD128",
    "/dev/dri/renderD129",
    "/dev/dri/renderD130",
    "/dev/dri/renderD131",
    "/dev/dri/renderD132",
    "/dev/dri/renderD133",
    "/dev/dri/renderD134",
    "/dev/dri/renderD135",
];
