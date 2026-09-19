//! Reading the committed clips and their reference checksums.

#![allow(
    dead_code,
    unreachable_pub,
    clippy::cast_possible_truncation,
    clippy::type_complexity
)]

use std::path::PathBuf;

use lowlat_decode::{Decoder, Fed, Planes};

pub fn data(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/data")
        .join(name)
}

/// The access units of a clip, in order.
pub fn units(name: &str) -> Vec<Vec<u8>> {
    let bytes = std::fs::read(data(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
    let mut out = Vec::new();
    let mut at = 0;
    while at + 4 <= bytes.len() {
        let len =
            u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]) as usize;
        at += 4;
        out.push(bytes[at..at + len].to_vec());
        at += len;
    }
    out
}

/// One reference picture: its index in output order and its plane checksums.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sum {
    pub picture: usize,
    pub y: u32,
    pub uv: u32,
}

pub fn sums(name: &str) -> Vec<Sum> {
    let text = std::fs::read_to_string(data(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let mut f = l.split_whitespace().map(|v| v.parse::<u64>().unwrap());
            Sum {
                picture: f.next().unwrap() as usize,
                y: f.next().unwrap() as u32,
                uv: f.next().unwrap() as u32,
            }
        })
        .collect()
}

/// Decode a clip through `backend`, already built, and return the `(y,
/// chroma)` checksums of every picture out in output order, plus what
/// `timed` reads off the backend after each. `drain` is called after the
/// last unit so the pictures still held come out.
pub fn decode_clip<D: Decoder>(
    backend: &mut D,
    clip: &str,
    drain: impl Fn(&mut D),
    mut timed: impl FnMut(&D) -> (u32, u32),
) -> (Vec<(u32, u32)>, Vec<(u32, u32)>) {
    let mut sums = Vec::new();
    let mut times = Vec::new();
    // Planes at the largest size a fixture has, with an odd pitch so a
    // pitch mistake shows.
    let pitch = 1280 * 2 + 64;
    let mut y = vec![0u8; pitch * 720];
    let mut u = vec![0u8; pitch * 720];
    let mut v = vec![0u8; pitch * 720];
    let all = units(clip);
    for (n, unit) in all.iter().enumerate() {
        let fed = backend
            .feed(unit)
            .unwrap_or_else(|e| panic!("{clip}: unit {n}: {e:?}"));
        if fed == Fed::FormatChanged {
            panic!("{clip}: unit {n} changed format");
        }
        if n + 1 == all.len() {
            drain(backend);
        }
        loop {
            let mut planes = Planes {
                y: &mut y,
                y_pitch: pitch,
                uv: &mut u,
                uv_pitch: pitch,
                v: &mut v,
                v_pitch: pitch,
            };
            let Some(picture) = backend
                .take(&mut planes)
                .unwrap_or_else(|e| panic!("{clip}: take: {e:?}"))
            else {
                break;
            };
            let format = picture.format;
            let w = picture.width as usize * format.sample();
            let h = picture.height as usize;
            let mut yb = Vec::with_capacity(w * h);
            for row in 0..h {
                yb.extend_from_slice(&y[row * pitch..row * pitch + w]);
            }
            let rows = format.chroma_rows(h);
            let mut cb = Vec::with_capacity(w * rows * 2);
            for row in 0..rows {
                cb.extend_from_slice(&u[row * pitch..row * pitch + w]);
            }
            if format.full_chroma() {
                for row in 0..rows {
                    cb.extend_from_slice(&v[row * pitch..row * pitch + w]);
                }
            }
            sums.push((crc32(&yb), crc32(&cb)));
            times.push(timed(backend));
        }
    }
    (sums, times)
}

/// CRC-32 as the reference decoder's checksum tool computes it.
pub fn crc32(data: &[u8]) -> u32 {
    let mut table = [0u32; 256];
    for (i, entry) in table.iter_mut().enumerate() {
        let mut c = i as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
        }
        *entry = c;
    }
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc = table[((crc ^ u32::from(b)) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc ^ 0xFFFF_FFFF
}

/// Every fixture of one codec: `(clip name, sums name)`.
pub fn fixtures(codec: &str) -> Vec<(String, String)> {
    let dir = data("fixtures");
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.starts_with(codec) && n.ends_with(".bin"))
        .map(|n| n.trim_end_matches(".bin").to_string())
        .collect();
    names.sort();
    names
        .into_iter()
        .map(|n| (format!("fixtures/{n}.bin"), format!("fixtures/{n}.sums")))
        .collect()
}
