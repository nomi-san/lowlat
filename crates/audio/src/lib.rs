//! Sound: capture from the desktop's own output, encode it, and later decode
//! what a guest sends back.
//!
//! **Separate from `lowlat-capture` and `lowlat-encode` on purpose.** Those two
//! carry a display stack and two vendor runtimes between them, and none of it
//! has anything to do with sound; a machine with no graphics device still has
//! audio and the reverse is true as well. What they share is the shape of the
//! problem rather than any code.
//!
//! The format is fixed and is not a configuration: 48 kHz, stereo, 20 ms a
//! packet ([docs/05-host.md](../../../docs/05-host.md) section 9). The wire
//! carries the sample count, so a receiver would follow a different frame; the
//! reason to keep one is that every buffer, every packet and every measurement
//! here is expressed in it.

#![deny(unsafe_op_in_unsafe_fn)]

pub mod capture;
pub mod encode;
pub mod microphone;
mod pulse;

pub use capture::{Capture, Config, Live, Output, Wanted, outputs};
pub use encode::Encoder;
pub use microphone::Decoder;

/// Samples a second, per channel. **The only rate the protocol carries**, so
/// it comes from there rather than being declared again here.
pub use lowlat_core::audio::SAMPLE_RATE;

/// Channels captured, encoded and sent.
///
/// Stereo is what a desktop mixes to and what every peer expects; the wire can
/// describe more and nothing here produces it.
pub const CHANNELS: usize = 2;

/// Samples per channel in one packet: **20 ms**.
///
/// A host decision rather than a wire constant. Shorter frames cost bitrate for
/// latency the picture does not have, and longer ones save little: the sound
/// server delivers on its own period regardless, which is not this one.
pub const FRAME: usize = 960;

/// Bytes one captured frame occupies as interleaved sixteen-bit samples, which
/// is also exactly what an uncompressed packet carries.
pub const FRAME_BYTES: usize = FRAME * CHANNELS * 2;

/// What went wrong, as codes and small values rather than as messages.
///
/// **No `String` anywhere in it** (AGENTS section 9): a caller logs the variant
/// and the number the platform gave, which is what identifies the fault.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// The sound server's client library is not installed.
    Unavailable,
    /// It is installed and does not export what this needs, which means a
    /// version older than the calls used here.
    Incomplete,
    /// The session's sound server refused the connection, or there is none.
    /// Carries the server's own code.
    Refused(i32),
    /// A read failed. Carries the server's own code.
    Read(i32),
    /// The codec refused a frame, or could not be built for this format.
    Encode,
    /// The named device is not among those the server offers. **Checked before
    /// opening rather than after**, because a name that does not resolve is
    /// substituted rather than refused.
    NoSuchDevice,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Error::Unavailable => f.write_str("the sound server's client library is not installed"),
            Error::Incomplete => {
                f.write_str("the sound client library is missing calls this needs")
            }
            Error::Refused(code) => {
                write!(f, "the sound server refused the connection, code={code}")
            }
            Error::Read(code) => write!(f, "reading sound failed, code={code}"),
            Error::Encode => f.write_str("the codec refused a frame"),
            Error::NoSuchDevice => f.write_str("no such sound device"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The packet this host sends is 20 ms, and the arithmetic that follows
    /// from it is what every buffer here is sized by.
    #[test]
    fn the_frame_is_twenty_milliseconds() {
        assert_eq!(FRAME * 1000 / SAMPLE_RATE as usize, 20);
        assert_eq!(FRAME_BYTES, 3840);
        // Far inside what a receiver accepts uncompressed. Both sides are
        // constants, so this holds at compile time or not at all.
        const { assert!(FRAME_BYTES < lowlat_core::audio::PCM_PAYLOAD_MAX) };
    }

    /// A header built for this frame describes exactly the bytes it carries.
    #[test]
    fn an_uncompressed_packet_matches_the_frame() {
        let header = lowlat_core::audio::AudioHeader::stereo(
            u32::try_from(FRAME).expect("a frame count"),
            lowlat_core::audio::Codec::Pcm,
        );
        assert_eq!(header.payload_len(), Some(FRAME_BYTES));
        assert_eq!(usize::from(header.channels), CHANNELS);
    }
}
