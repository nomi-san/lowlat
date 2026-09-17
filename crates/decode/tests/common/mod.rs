//! Reading the committed clips and their reference checksums.

#![allow(dead_code, unreachable_pub, clippy::cast_possible_truncation)]

use std::path::PathBuf;

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
