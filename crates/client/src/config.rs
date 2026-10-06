//! What a client asks for, and the initialization it becomes.

use std::fmt;
use std::net::SocketAddr;
use std::num::NonZeroU16;

use lowlat_core::init::{self, Init};
pub use lowlat_decode::Caps;
use zeroize::Zeroizing;

/// The largest picture the current client generation declares it will take.
/// Not a decoder limit read from anything: it is the figure every peer of
/// that generation sends, and a host clamps a size request against it. It is
/// also what the picture slots are sized for, unless the application says
/// smaller.
pub const MAX_DIMENSION: u32 = 4096;

/// Which decoder to build.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Backend {
    /// The first that opens on the device named: the system's own
    /// interface, then the vendor's, then software.
    #[default]
    Auto,
    /// The system's own decoding interface: the open stack on Linux, the
    /// system's video decoding interface on Windows.
    Vaapi,
    /// The vendor interface, on the card behind the render node named, or
    /// the first.
    Nvdec,
    /// The machine's own codec library, an LGPL build of it -- or a GPL one
    /// as well, in a build with the `gpl-libavcodec` feature -- or none;
    /// the device names the directory it is taken from, or is empty for the
    /// search of its own.
    Software,
    /// The system's own decoder, in software: the media framework's on
    /// Windows, H.264 and, where its extension is installed, HEVC at eight
    /// and ten bits; on no other system.
    System,
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
    /// The device: a render node on Linux, an adapter's identity
    /// (`luid:HIGH:LOW`) on Windows; or empty for the first that opens.
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

/// The port every attempt binds, settled at creation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Port {
    /// One the system picks, per attempt.
    #[default]
    Any,
    /// This one, or the first free one above it, at every attempt; and, with
    /// a description, kept open on the gateway under it
    /// (docs/03-connectivity.md 6).
    Stable {
        first: NonZeroU16,
        mapping: Option<String>,
    },
}

impl Port {
    /// A stable port from what an application names: the port itself, or the
    /// one its seed picks -- the seed its own, or the machine's name when it
    /// gives none -- and, when it asks, the mapping listed under the seed.
    pub fn seeded(port: u16, seed: &str, mapping: bool) -> Self {
        let seed = if seed.is_empty() {
            lowlat_portmap::seed::machine_name()
        } else {
            seed.to_owned()
        };
        let first = if port == 0 {
            lowlat_portmap::seed::port(&seed)
        } else {
            port
        };
        // A seed's port is never zero, so this is the stable port.
        match NonZeroU16::new(first) {
            Some(first) => Port::Stable {
                first,
                mapping: mapping.then(|| lowlat_portmap::seed::description(&seed)),
            },
            None => Port::Any,
        }
    }

    /// The port an attempt asks for first: zero for any.
    pub fn first(&self) -> u16 {
        match self {
            Port::Any => 0,
            Port::Stable { first, .. } => first.get(),
        }
    }
}

/// What the application would like of the picture, for the one stream.
///
/// **Preferences, not requirements.** Each is "this if the host has it"; the
/// library masks the codec and colour ones with what its decoder was verified
/// to decode before declaring anything, so a stream the decoder cannot take is
/// never asked for, and follows whatever the host then sends. Defaults off: a
/// client at its defaults asks for H.264 in the video range.
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
    /// The application's renderer takes the full range: its conversion reads
    /// each picture's range, so the host may send samples spanning the whole
    /// of their depth. **Declared as asked, never masked by the decoder**,
    /// which decodes either range alike and converts nothing; off asks for
    /// the video range, which a renderer that assumes it draws right.
    pub full_range: bool,
}

impl Video {
    /// The declaration's flags: the preferences masked by what the decoder
    /// takes, with the wire's own implication that depth and chroma are the
    /// second codec's, so either declares it and neither is declared without
    /// it.
    ///
    /// **Everything a host may send under the declaration must decode.** A
    /// host that cannot meet a declaration takes its axes off in order --
    /// ten-bit, then full chroma, then the full range, then the second codec
    /// -- so a declared pair needs its eight-bit row and its subsampled row
    /// as well as itself. The range needs nothing of the decoder.
    pub fn flags(&self, caps: &Caps) -> u32 {
        let mut flags = if self.full_range {
            init::FLAG_FULL_RANGE
        } else {
            0
        };
        if !caps.h264 {
            // Nothing decodes: the declaration is the range alone, and the
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

/// A relay to go through, and the credential it takes
/// (docs/03-connectivity.md 7).
///
/// **The credential is never rendered and is cleared when dropped.** Every
/// copy of a configuration carries its own, and each goes when it does.
#[derive(Clone)]
pub struct Relay {
    pub server: SocketAddr,
    pub username: Zeroizing<String>,
    pub password: Zeroizing<String>,
}

impl Relay {
    pub fn new(server: SocketAddr, username: &str, password: &str) -> Self {
        Self {
            server,
            username: Zeroizing::new(username.to_owned()),
            password: Zeroizing::new(password.to_owned()),
        }
    }
}

impl fmt::Debug for Relay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Relay")
            .field("server", &self.server)
            .finish_non_exhaustive()
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
    /// Offer and check IPv4 addresses only: no IPv6 host candidate, no IPv6
    /// reflexive server, and a peer's IPv6 candidates declined. For a path
    /// whose IPv6 would connect directly, so the attempt crosses the
    /// translation on its IPv4 path.
    pub ipv4_only: bool,
    /// A relay to go through. Set, the attempt is a relay attempt: it offers
    /// the relayed address and nothing else, and every check goes through
    /// the relay.
    pub relay: Option<Relay>,
}

impl Config {
    /// Whether an address may be offered or checked. **The family is the
    /// address's, never its notation's**: an IPv4 address in its v4-mapped
    /// form is IPv4.
    pub fn admits(&self, addr: SocketAddr) -> bool {
        !self.ipv4_only || lowlat_core::stun::canonical(addr).is_ipv4()
    }

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
    use lowlat_core::init::{FLAG_10BIT, FLAG_COLOR444, FLAG_FULL_RANGE, FLAG_HEVC};

    const EVERYTHING: Caps = Caps {
        h264: true,
        hevc: true,
        hevc_10: true,
        hevc_444: true,
        hevc_444_10: true,
    };

    #[test]
    fn the_defaults_declare_the_first_codec_in_the_video_range() {
        assert_eq!(Video::default().flags(&EVERYTHING), 0);
    }

    /// The range is the renderer's to take, so it is declared as asked: a
    /// decoder that takes nothing, or only the first codec, leaves it on.
    #[test]
    fn the_full_range_is_declared_as_asked_whatever_the_decoder_takes() {
        let full = Video {
            full_range: true,
            ..Video::default()
        };
        assert_eq!(full.flags(&EVERYTHING), FLAG_FULL_RANGE);
        assert_eq!(full.flags(&Caps::default()), FLAG_FULL_RANGE);
        let all = Video {
            hevc: true,
            ten_bit: true,
            chroma_444: true,
            full_range: true,
            ..Video::default()
        };
        assert_eq!(
            all.flags(&EVERYTHING),
            FLAG_FULL_RANGE | FLAG_HEVC | FLAG_10BIT | FLAG_COLOR444
        );
        let h264_only = Caps {
            h264: true,
            ..Caps::default()
        };
        assert_eq!(all.flags(&h264_only), FLAG_FULL_RANGE);
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
            FLAG_HEVC | FLAG_10BIT | FLAG_COLOR444
        );
        // A device with the second codec at eight-bit 4:2:0 and ten bits,
        // as the open stack here: chroma comes off, depth stays.
        let ten_only = Caps {
            hevc_444: false,
            hevc_444_10: false,
            ..EVERYTHING
        };
        assert_eq!(all.flags(&ten_only), FLAG_HEVC | FLAG_10BIT);
        // Full chroma at eight bits only: depth comes off, chroma stays.
        let no_deep_chroma = Caps {
            hevc_444_10: false,
            ..EVERYTHING
        };
        assert_eq!(all.flags(&no_deep_chroma), FLAG_HEVC | FLAG_COLOR444);
        // No second codec at all: the first codec, declared as nothing.
        let h264_only = Caps {
            h264: true,
            ..Caps::default()
        };
        assert_eq!(all.flags(&h264_only), 0);
    }

    /// A port named is kept; none named is the seed's, and the machine's
    /// name is the seed when the application gives none. The mapping is
    /// listed under the seed and the machine, and only when asked for.
    #[test]
    fn a_seeded_port_is_the_one_named_or_the_seeds() {
        let first = |port: &Port| port.first();
        assert_eq!(first(&Port::seeded(30_000, "", false)), 30_000);
        assert_eq!(first(&Port::seeded(0, "HOST-1", false)), 24097);
        assert_eq!(
            Port::seeded(0, "", false),
            Port::seeded(0, &lowlat_portmap::seed::machine_name(), false)
        );
        let Port::Stable { mapping, .. } = Port::seeded(30_000, "HOST-1", true) else {
            panic!("a named port is a stable one");
        };
        let listed = mapping.unwrap();
        assert_eq!(listed, lowlat_portmap::seed::description("HOST-1"));
        assert!(listed.starts_with("ll-b596f0a1"), "{listed}");
        assert!(matches!(
            Port::seeded(0, "HOST-1", false),
            Port::Stable { mapping: None, .. }
        ));
        assert_eq!(first(&Port::default()), 0);
    }

    /// IPv4 only refuses an IPv6 address and keeps an IPv4 one in either
    /// form; by default both families pass.
    #[test]
    fn ipv4_only_refuses_ipv6_and_keeps_a_v4_mapped_address() {
        let v4: SocketAddr = "192.0.2.1:9".parse().unwrap();
        let mapped: SocketAddr = "[::ffff:192.0.2.1]:9".parse().unwrap();
        let v6: SocketAddr = "[2001:db8::1]:9".parse().unwrap();
        let only = Config {
            ipv4_only: true,
            ..Config::default()
        };
        assert!(only.admits(v4));
        assert!(only.admits(mapped), "a v4-mapped address was read as IPv6");
        assert!(!only.admits(v6));
        let both = Config::default();
        assert!(both.admits(v4) && both.admits(mapped) && both.admits(v6));
    }

    #[test]
    fn depth_or_chroma_alone_declares_the_second_codec() {
        let ten = Video {
            ten_bit: true,
            ..Video::default()
        };
        assert_eq!(ten.flags(&EVERYTHING), FLAG_HEVC | FLAG_10BIT);
        let full = Video {
            chroma_444: true,
            ..Video::default()
        };
        assert_eq!(full.flags(&EVERYTHING), FLAG_HEVC | FLAG_COLOR444);
    }
}
