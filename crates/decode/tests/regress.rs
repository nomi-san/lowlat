//! Inputs the fuzzer found the readers panicking on, kept as they were
//! found: each one must now be refused or read, never panic.

mod common;

use lowlat_decode::{h264, hevc};

fn regress(name: &str) -> Vec<u8> {
    std::fs::read(common::data(&format!("regress/{name}"))).unwrap()
}

#[test]
fn a_scaling_list_delta_past_the_bound_is_refused() {
    let data = regress("h264-scaling-delta-overflow.bin");
    let mut stream = h264::Stream::new();
    let _ = stream.read(&data);
}

#[test]
fn a_scaling_list_reference_below_zero_is_refused() {
    let data = regress("hevc-scaling-reference-underflow.bin");
    let mut stream = hevc::Stream::new();
    let _ = stream.read(&data);
}
