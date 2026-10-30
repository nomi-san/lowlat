//! Which decoder a configuration opens, on macOS: the system's own, through
//! the hardware where the Mac has it or the system's software decoder, then
//! the machine's own codec library from a package manager's directory.

use std::path::PathBuf;

use lowlat_decode::videotoolbox;

use super::{DecoderStage, Error, most_telling, probe_software, software_dir};
use crate::config::{Backend, Caps, Decoding, FrameKind};

/// The decoder settled at creation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Opened {
    /// The system's own decoder. Made again on the decode thread.
    VideoToolbox,
    /// The machine's own codec library, from the directory named or from
    /// the search of its own. Opened again on the decode thread, which
    /// finds the same pair the probe found.
    Software(Option<PathBuf>),
}

impl Opened {
    /// The backend this is, as a configuration names it.
    pub fn backend(&self) -> Backend {
        match self {
            Self::VideoToolbox => Backend::System,
            Self::Software(_) => Backend::Software,
        }
    }

    /// Whether it hands pictures out as a handle: the system's decoder does,
    /// each its surface.
    pub fn exports(&self) -> bool {
        matches!(self, Self::VideoToolbox)
    }
}

/// The system's own decoder: what it decodes here, a session made and
/// dropped for each combination to ask.
pub(crate) fn probe_system() -> Result<Caps, DecoderStage> {
    let vt = lowlat_drivers::vt::load().map_err(|_| DecoderStage::Runtime)?;
    let (caps, _) = videotoolbox::caps(vt);
    if caps.any() {
        Ok(caps)
    } else {
        Err(DecoderStage::Profile)
    }
}

/// The decoder a configuration settles on, probed once here, with what it
/// decodes. **Strict where a kind is named**: the system's decoder or the
/// codec library, or the stage is the answer. **In order where none is**:
/// the system's decoder, which every Mac has, then the codec library -- the
/// system's alone for a handle, which only it hands out.
pub(crate) fn choose(decoding: &Decoding) -> Result<(Option<Opened>, Caps), Error> {
    match (decoding.kind, decoding.backend) {
        (_, Backend::None) => Ok((None, Caps::default())),
        // The open stack and the vendors' interfaces are other systems'.
        (_, Backend::Vaapi | Backend::Nvdec) => Err(Error::Decoder(DecoderStage::Unsupported)),
        // The codec library's pictures are planes in the queue's own slots.
        (FrameKind::Handle, Backend::Software) => Err(Error::Decoder(DecoderStage::Unsupported)),
        (_, Backend::System) | (FrameKind::Handle, Backend::Auto) => {
            let caps = probe_system().map_err(Error::Decoder)?;
            Ok((Some(Opened::VideoToolbox), caps))
        }
        (FrameKind::Planes, Backend::Software) => {
            let dir = software_dir(&decoding.device);
            let caps = probe_software(dir.as_deref()).map_err(Error::Decoder)?;
            Ok((Some(Opened::Software(dir)), caps))
        }
        (FrameKind::Planes, Backend::Auto) => {
            let last = match probe_system() {
                Ok(caps) => return Ok((Some(Opened::VideoToolbox), caps)),
                Err(stage) => stage,
            };
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

    /// **The other systems' interfaces are not in a macOS build**, and the
    /// codec library hands out no handle: each refused as such, before
    /// anything is probed.
    #[test]
    fn the_other_systems_kinds_and_the_libraries_handle_are_refused() {
        for (backend, kind) in [
            (Backend::Vaapi, FrameKind::Planes),
            (Backend::Nvdec, FrameKind::Handle),
            (Backend::Software, FrameKind::Handle),
        ] {
            let decoding = Decoding {
                backend,
                kind,
                ..Decoding::default()
            };
            assert!(
                matches!(
                    choose(&decoding),
                    Err(Error::Decoder(DecoderStage::Unsupported))
                ),
                "{backend:?} {kind:?}"
            );
        }
    }

    /// **A handle is the system's decoder's**, asked for by name or settled
    /// on by the automatic order, and it says it hands one out; the codec
    /// library says not.
    #[test]
    fn a_handle_is_the_systems_decoders() {
        for backend in [Backend::Auto, Backend::System] {
            let decoding = Decoding {
                backend,
                kind: FrameKind::Handle,
                ..Decoding::default()
            };
            let (opened, _) = choose(&decoding).expect("a decoder");
            assert_eq!(opened, Some(Opened::VideoToolbox), "{backend:?}");
        }
        assert!(Opened::VideoToolbox.exports());
        assert!(!Opened::Software(None).exports());
    }

    /// **The automatic order settles on the system's decoder**, which every
    /// Mac has, and says what it decodes.
    #[test]
    fn the_automatic_order_settles_on_the_systems_decoder() {
        let (opened, caps) = choose(&Decoding::default()).expect("a decoder");
        assert_eq!(opened, Some(Opened::VideoToolbox));
        assert!(caps.h264 && caps.hevc, "{caps:?}");
    }
}
