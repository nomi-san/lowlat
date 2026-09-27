//! Which decoders a configuration opens, on Windows: the machine's own codec
//! library. The device decoders come with their steps of
//! docs/impl-plan-windows.md, each at its place in the automatic order.

use std::path::PathBuf;

use super::{DecoderStage, Error, probe_software, software_dir};
use crate::config::{Backend, Caps, Decoding, FrameKind};

/// The decoder settled at creation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Opened {
    /// The machine's own codec library, from the directory named or from
    /// the search of its own. Opened again on the decode thread, which
    /// finds the same pair the probe found.
    Software(Option<PathBuf>),
}

impl Opened {
    /// The backend this is, as a configuration names it.
    pub fn backend(&self) -> Backend {
        match self {
            Self::Software(_) => Backend::Software,
        }
    }
}

/// The decoder a configuration settles on, probed once here, with what it
/// decodes. A kind this build has no decoder for is refused as not in the
/// build, and so is the handle kind, which no decoder here hands out.
pub(crate) fn choose(decoding: &Decoding) -> Result<(Option<Opened>, Caps), Error> {
    match (decoding.kind, decoding.backend) {
        (_, Backend::None) => Ok((None, Caps::default())),
        (FrameKind::Handle, _) | (_, Backend::Vaapi | Backend::Nvdec) => {
            Err(Error::Decoder(DecoderStage::Unsupported))
        }
        (FrameKind::Planes, Backend::Software | Backend::Auto) => {
            let dir = software_dir(&decoding.device);
            let caps = probe_software(dir.as_deref()).map_err(Error::Decoder)?;
            Ok((Some(Opened::Software(dir)), caps))
        }
    }
}
