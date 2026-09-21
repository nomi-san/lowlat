//! What a client asks for, and the initialization it becomes.

use std::net::SocketAddr;

use lowlat_core::init::{self, Init};
pub use lowlat_decode::Caps;

/// The largest picture the current client generation declares it will take.
/// Not a decoder limit read from anything: it is the figure every peer of
/// that generation sends, and a host clamps a size request against it. It is
/// also what the picture slots are sized for, unless the application says
/// smaller.
pub const MAX_DIMENSION: u32 = 4096;

/// Which decoder to build.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Backend {
    /// The first that opens on the device named: the open stack, then the
    /// vendor's interface, then software.
    #[default]
    Auto,
    /// The open-stack interface.
    Vaapi,
    /// The vendor interface, on the card behind the render node named, or
    /// the first.
    Nvdec,
    /// The machine's own codec library, an LGPL build of it -- or a GPL one
    /// as well, in a build with the `gpl-libavcodec` feature -- or none;
    /// the device names the directory it is taken from, or is empty for the
    /// search of its own.
    Software,
    /// No decoder at all: the session carries control and sound, and every
    /// picture is taken off the wire and dropped. A test peer, or a client
    /// with nowhere to draw.
    None,
}

/// How pictures leave the library.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FrameKind {
    /// Planes in memory the library owns for the lease.
    #[default]
    Planes,
    /// A device-level handle. No backend exports one yet: refused at
    /// creation.
    Handle,
}

/// What the decoder is built on, settled at creation.
#[derive(Debug, Clone, Default)]
pub struct Decoding {
    pub backend: Backend,
    /// The render node, or empty for the first that opens.
    pub device: String,
    pub kind: FrameKind,
    /// The largest picture the slots take; zero for the generation's
    /// declared maximum.
    pub ceiling: (u32, u32),
}

impl Decoding {
    /// The ceiling in force.
    pub fn ceiling(&self) -> (u32, u32) {
        (
            if self.ceiling.0 == 0 {
                MAX_DIMENSION
            } else {
                self.ceiling.0
            },
            if self.ceiling.1 == 0 {
                MAX_DIMENSION
            } else {
                self.ceiling.1
            },
        )
    }
}

/// What the application would like of the picture, for the one stream.
///
/// **Preferences, not requirements.** Each is "this if the host has it"; the
/// library masks them with what its decoder was verified to decode before
/// declaring anything, so a stream the decoder cannot take is never asked
/// for, and follows whatever the host then sends. Defaults off: a client at
/// its defaults asks for what every established client asks at its defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Video {
    /// The size asked of the host, or zero for no preference.
    ///
    /// **A request to change the host's display, not a description of this
    /// one.** An established host takes the owner's figure as a mode request,
    /// so the default asks for nothing and an application sets it only when
    /// it means to change the person's monitor.
    pub resolution: (u32, u32),
    /// The second codec.
    pub hevc: bool,
    /// Ten-bit colour, which implies the second codec.
    pub ten_bit: bool,
    /// Full chroma, which implies the second codec.
    pub chroma_444: bool,
}

impl Video {
    /// The declaration's flags: the preferences masked by what the decoder
    /// takes, with the wire's own implication that depth and chroma are the
    /// second codec's, so either declares it and neither is declared without
    /// it.
    ///
    /// **Everything a host may send under the declaration must decode.** A
    /// host that cannot meet a declaration takes its axes off in order --
    /// ten-bit, then full chroma, then the second codec -- so a declared
    /// pair needs its eight-bit row and its subsampled row as well as itself.
    pub fn flags(&self, caps: &Caps) -> u32 {
        let mut flags = init::FLAG_BASE;
        if !caps.h264 {
            // Nothing decodes: the declaration is the base alone, and the
            // session is one with nowhere to draw.
            return flags;
        }
        let hevc = (self.hevc || self.ten_bit || self.chroma_444) && caps.hevc;
        if !hevc {
            return flags;
        }
        let chroma_444 = self.chroma_444 && caps.hevc_444;
        let ten_bit = self.ten_bit
            && if chroma_444 {
                caps.hevc_444_10
            } else {
                caps.hevc_10
            };
        flags |= init::FLAG_HEVC;
        if ten_bit {
            flags |= init::FLAG_10BIT;
        }
        if chroma_444 {
            flags |= init::FLAG_COLOR444;
        }
        flags
    }

    /// The preferences as flags, unmasked: what was asked, for the record.
    pub fn asked(&self) -> u32 {
        self.flags(&Caps {
            h264: true,
            hevc: true,
            hevc_10: true,
            hevc_444: true,
            hevc_444_10: true,
        })
    }
}

/// A client's settings.
#[derive(Debug, Clone, Default)]
pub struct Config {
    /// The picture: the size asked of the host, and what the application
    /// would prefer of the codec and the colour.
    pub video: Video,
    /// Whether uncompressed sound is acceptable.
    pub raw_audio: bool,
    /// Offer no media key, so the host answers without one and both ends key
    /// the legacy cipher from its certificate digest.
    pub legacy_cipher: bool,
    /// Reflexive servers for connectivity.
    pub servers: Vec<SocketAddr>,
    /// Offer addresses from the carrier-grade shared range.
    pub shared_address_space: bool,
}

impl Config {
    /// The initialization this configuration declares, given what the
    /// decoder takes.
    pub fn init(&self, caps: &Caps) -> Init {
        Init {
            version: init::VERSION,
            max_width: MAX_DIMENSION,
            max_height: MAX_DIMENSION,
            flags: self.video.flags(caps),
            resolution_x: self.video.resolution.0,
            resolution_y: self.video.resolution.1,
            media_container: 0,
            // A literal from every client generation; no host reads it.
            refresh_rate: 60,
            channels: 2,
            channel_mask: 3,
            caches_cursor: true,
            raw_audio: self.raw_audio,
            video_protocol_version: 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lowlat_core::init::{FLAG_10BIT, FLAG_BASE, FLAG_COLOR444, FLAG_HEVC};

    const EVERYTHING: Caps = Caps {
        h264: true,
        hevc: true,
        hevc_10: true,
        hevc_444: true,
        hevc_444_10: true,
    };

    #[test]
    fn the_defaults_declare_the_base_alone_whatever_the_decoder_takes() {
        assert_eq!(Video::default().flags(&EVERYTHING), FLAG_BASE);
    }

    #[test]
    fn a_preference_is_declared_only_where_the_decoder_takes_it() {
        let all = Video {
            hevc: true,
            ten_bit: true,
            chroma_444: true,
            ..Video::default()
        };
        assert_eq!(
            all.flags(&EVERYTHING),
            FLAG_BASE | FLAG_HEVC | FLAG_10BIT | FLAG_COLOR444
        );
        // A device with the second codec at eight-bit 4:2:0 and ten bits,
        // as the open stack here: chroma comes off, depth stays.
        let ten_only = Caps {
            hevc_444: false,
            hevc_444_10: false,
            ..EVERYTHING
        };
        assert_eq!(all.flags(&ten_only), FLAG_BASE | FLAG_HEVC | FLAG_10BIT);
        // Full chroma at eight bits only: depth comes off, chroma stays.
        let no_deep_chroma = Caps {
            hevc_444_10: false,
            ..EVERYTHING
        };
        assert_eq!(
            all.flags(&no_deep_chroma),
            FLAG_BASE | FLAG_HEVC | FLAG_COLOR444
        );
        // No second codec at all: nothing but the base.
        let h264_only = Caps {
            h264: true,
            ..Caps::default()
        };
        assert_eq!(all.flags(&h264_only), FLAG_BASE);
    }

    #[test]
    fn depth_or_chroma_alone_declares_the_second_codec() {
        let ten = Video {
            ten_bit: true,
            ..Video::default()
        };
        assert_eq!(ten.flags(&EVERYTHING), FLAG_BASE | FLAG_HEVC | FLAG_10BIT);
        let full = Video {
            chroma_444: true,
            ..Video::default()
        };
        assert_eq!(
            full.flags(&EVERYTHING),
            FLAG_BASE | FLAG_HEVC | FLAG_COLOR444
        );
    }
}
