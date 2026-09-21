//! The decoders this machine can open, listed for an application that
//! wants to choose one or show them.
//!
//! One row per backend and device that decodes anything: the open stack on
//! each render node it decodes through, then the vendor's interface on
//! each of its devices, then the machine's own codec library when it is an
//! LGPL build. Each row is probed the way creation probes it, so a row is a
//! decoder creation would open, named by the same two values creation
//! takes: the backend and the render node -- or, for software, the
//! directory the pair was found in.

use std::ffi::CString;

use lowlat_core::video::Codec;
use lowlat_decode::vaapi::Vaapi;
use lowlat_decode::{Caps, nvdec, software, vaapi};
use lowlat_drivers::cuda::Cuda;
use lowlat_drivers::cuvid::Cuvid;
use lowlat_drivers::lavc::{Lavc, Origin};

use crate::config::Backend;
use crate::seam::{RENDER_NODES, node_of};

/// One decoder this machine can open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Available {
    pub backend: Backend,
    /// The render node, as creation names the device; empty when the
    /// vendor's device has no node this crate looks at, which creation
    /// takes as the first device.
    pub device: String,
    /// The device's or driver's own name, for a label.
    pub name: String,
    pub caps: Caps,
    /// Whether it hands pictures out as a handle.
    pub handle: bool,
    /// The largest coded picture per codec, or zero where the device does
    /// not say.
    pub max_h264: (u32, u32),
    pub max_hevc: (u32, u32),
}

/// Every decoder this machine can open, in a fixed order. Each call
/// probes afresh: a few milliseconds, for a call made once at startup.
pub fn enumerate() -> Vec<Available> {
    let mut rows = Vec::new();
    if let Ok(va) = Vaapi::load() {
        for node in RENDER_NODES {
            let Ok(path) = CString::new(node) else {
                continue;
            };
            let Ok(display) = va.open(&path) else {
                continue;
            };
            let Ok(caps) = vaapi::caps(&display) else {
                continue;
            };
            if !caps.any() {
                continue;
            }
            rows.push(Available {
                backend: Backend::Vaapi,
                device: node.to_string(),
                name: display.vendor(),
                caps,
                handle: false,
                max_h264: vaapi::limits(&display, Codec::H264),
                max_hevc: vaapi::limits(&display, Codec::H265),
            });
        }
    }
    if let Ok(cuda) = Cuda::load() {
        for ordinal in 0..cuda.device_count().unwrap_or(0) {
            let Ok(device) = cuda.device(ordinal) else {
                continue;
            };
            let Ok(context) = cuda.retain_primary(&device) else {
                continue;
            };
            if context.make_current().is_err() {
                continue;
            }
            let row = Cuvid::load().ok().and_then(|cuvid| {
                let caps = nvdec::caps(&cuvid);
                caps.any().then(|| {
                    let mut name = [0u8; 96];
                    let len = cuda.device_name(&device, &mut name).unwrap_or(0);
                    Available {
                        backend: Backend::Nvdec,
                        device: node_of(device.address()).unwrap_or("").to_string(),
                        name: String::from_utf8_lossy(name.get(..len).unwrap_or(&[])).into_owned(),
                        caps,
                        handle: true,
                        max_h264: nvdec::limits(&cuvid, Codec::H264),
                        max_hevc: nvdec::limits(&cuvid, Codec::H265),
                    }
                })
            });
            // The application's thread, left as it was found.
            let _ = context.release_current();
            rows.extend(row);
        }
    }
    if let Ok(lavc) = Lavc::load(None) {
        rows.push(Available {
            backend: Backend::Software,
            device: match &lavc.origin {
                Origin::Directory(dir) => dir.display().to_string(),
                Origin::Default => String::new(),
            },
            name: format!(
                "libavcodec {}.{}.{} {}",
                lavc.version.0, lavc.version.1, lavc.version.2, lavc.licence
            ),
            caps: software::caps(&lavc),
            handle: false,
            max_h264: (0, 0),
            max_hevc: (0, 0),
        });
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every row names a backend creation takes and decodes something;
    /// the rows come in the documented order. On a machine with no
    /// decoder the list is empty, which is an answer.
    #[test]
    fn the_rows_are_openable_and_ordered() {
        let rows = enumerate();
        for row in &rows {
            println!("{row:?}");
            assert!(row.caps.any());
            assert!(matches!(
                row.backend,
                Backend::Vaapi | Backend::Nvdec | Backend::Software
            ));
            assert_eq!(row.handle, row.backend == Backend::Nvdec);
            if !row.device.is_empty() && row.backend != Backend::Software {
                assert!(RENDER_NODES.contains(&row.device.as_str()));
            }
            if row.backend == Backend::Software {
                assert!(row.name.contains("LGPL"), "a software row not LGPL");
            }
        }
        let rank = |b: Backend| match b {
            Backend::Vaapi => 0,
            Backend::Nvdec => 1,
            _ => 2,
        };
        assert!(
            rows.windows(2)
                .all(|w| rank(w[0].backend) <= rank(w[1].backend)),
            "the open stack's rows, then the vendor's, then software"
        );
        assert!(
            rows.iter()
                .filter(|r| r.backend == Backend::Software)
                .count()
                <= 1,
            "one software row at most"
        );
    }
}
