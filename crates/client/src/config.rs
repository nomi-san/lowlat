//! What a client asks for, and the initialization it becomes.

use std::net::SocketAddr;

use lowlat_core::init::{self, FLAG_BASE, Init};

/// The largest picture the current client generation declares it will take.
/// Not a decoder limit read from anything: it is the figure every peer of
/// that generation sends, and a host clamps a size request against it.
pub const MAX_DIMENSION: u32 = 4096;

/// A client's settings.
#[derive(Debug, Clone)]
pub struct Config {
    /// The size asked of the host, or zero for no preference.
    ///
    /// **A request to change the host's display, not a description of this
    /// one.** An established host takes the owner's figure as a mode request,
    /// so the default asks for nothing and an application sets it only when
    /// it means to change the person's monitor.
    pub resolution: (u32, u32),
    /// The declaration's flags: the codec and colour a decoder can take.
    /// The base flag alone until a decoder says otherwise.
    pub flags: u32,
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

impl Default for Config {
    fn default() -> Self {
        Self {
            resolution: (0, 0),
            flags: FLAG_BASE,
            raw_audio: false,
            legacy_cipher: false,
            servers: Vec::new(),
            shared_address_space: false,
        }
    }
}

impl Config {
    /// The initialization this configuration declares.
    pub fn init(&self) -> Init {
        Init {
            version: init::VERSION,
            max_width: MAX_DIMENSION,
            max_height: MAX_DIMENSION,
            flags: self.flags,
            resolution_x: self.resolution.0,
            resolution_y: self.resolution.1,
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
