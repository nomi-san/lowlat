//! The decoders a stream is opened on, on macOS: the system's own, whose
//! hardware's pictures stay in its buffers and are lent to the queue's slots
//! -- as planes, or by handle as their surfaces -- and whose software
//! decoder's are copied into them, or the machine's own codec library.

use lowlat_decode::{Fault, Format, Picture, software, videotoolbox};
use lowlat_drivers::lavc::Lavc;

use super::{Backend, Next, Shared, drive};
use crate::UNIT_BYTES;
use crate::frames::Filling;
use crate::seam::Opened;

impl Backend for videotoolbox::Backend {
    fn output(&self) -> Option<(u32, u32, Format)> {
        videotoolbox::Backend::output(self)
    }
    fn timings(&self) -> (u32, u32) {
        (self.decode_us, self.readback_us)
    }
    // Only the hardware's pictures stay as they were handed out while they
    // are held: the system's software decoder writes into a buffer it has
    // handed out, so its pictures are copied into the queue's own slots, and
    // come as planes to a session that asked for handles.
    fn exports(&self) -> bool {
        self.hardware() == Some(true)
    }
    fn lends(&self) -> bool {
        self.hardware() == Some(true)
    }
    fn take_to_slot(
        &mut self,
        filling: &mut Filling<'_>,
        _layout: (u32, u32, Format),
    ) -> Result<Option<(Picture, u64)>, Fault> {
        let Some((picture, buffer)) = self.take_buffer()? else {
            return Ok(None);
        };
        // In the kind the session asks for, the slot deciding per picture; a
        // buffer that will not lock or has no surface is lost, as a picture
        // that does not fit its slot is.
        Ok(filling.lend(buffer).then_some((picture, 0)))
    }
}

/// Open the decoder `opened` names and drive it until it returns. The
/// frameworks and the codec library are opened here, on the decode thread,
/// and live as long as the decoder built on them does.
pub(super) fn open(opened: Opened, shared: &Shared<'_>, replacing: bool) -> Next {
    match opened {
        Opened::VideoToolbox { .. } => {
            let Ok(vt) = lowlat_drivers::vt::load() else {
                return Next::Failed;
            };
            drive(
                videotoolbox::Backend::new(vt, UNIT_BYTES),
                shared,
                replacing,
            )
        }
        Opened::Software(dir) => {
            // The same search creation ran, landing on the same pair.
            let Ok(lavc) = Lavc::load(dir.as_deref()) else {
                return Next::Failed;
            };
            drive(software::Backend::new(&lavc), shared, replacing)
        }
    }
}

#[cfg(test)]
mod tests {
    use lowlat_core::video::{Codec, Rotation, VideoHeader};
    use lowlat_decode::{Decoder, videotoolbox};

    use super::Backend;

    /// The first picture of a committed clip through `backend`, which then
    /// has its session.
    fn first_picture(backend: &mut videotoolbox::Backend) {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../decode/tests/data/fixtures/hevc-nvenc-444-10.bin"
        );
        let bytes = std::fs::read(path).unwrap();
        backend
            .build(&VideoHeader {
                frame_id: 1,
                width: 0,
                height: 0,
                codec: Codec::H265,
                rotation: Rotation::None,
                ten_bit: true,
                locked: false,
                announced: false,
                metadata: false,
            })
            .unwrap();
        let mut at = 0;
        while backend.take_buffer().unwrap().is_none() {
            let len = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
            backend.feed(&bytes[at + 4..at + 4 + len]).unwrap();
            at += 4 + len;
        }
    }

    /// **Only the hardware's pictures are lent or exported**: the system's
    /// software decoder writes into a buffer it has handed out while it is
    /// still held, so its pictures are copied into the queue's own slots,
    /// whatever kind the session asked for.
    #[test]
    fn only_the_hardwares_pictures_are_lent_or_exported() {
        let vt = lowlat_drivers::vt::load().unwrap();
        let mut software = videotoolbox::Backend::software_only(vt, 1 << 22);
        first_picture(&mut software);
        assert_eq!(software.hardware(), Some(false));
        assert!(!Backend::lends(&software), "the software decoder's lent");
        assert!(
            !Backend::exports(&software),
            "the software decoder's exported"
        );
        let mut chosen = videotoolbox::Backend::new(vt, 1 << 22);
        first_picture(&mut chosen);
        let hardware = chosen.hardware() == Some(true);
        assert_eq!(
            (Backend::lends(&chosen), Backend::exports(&chosen)),
            (hardware, hardware)
        );
    }
}
