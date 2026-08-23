//! Silencing the speakers must never silence the guests.
//!
//! **Whether it would depends on the device**, which is the thing this checks.
//! Where a device applies its own mute, the mix that reaches the monitor is
//! untouched and the speakers can go quiet while a capture stays at full
//! scale. Where the server applies the mute instead, the same mix feeds the
//! monitor: the speakers go quiet and so does everybody listening.
//!
//! Off by default. It needs a sound server and a device of the second kind,
//! which `scripts/audio-virtual-source.sh up` provides on a machine whose only
//! real output is of the first.

use std::sync::Arc;

use lowlat_audio::{Capture, Config, Live, Wanted};

const SINK: &str = "lowlat_virtual";

#[test]
#[ignore = "needs a sound server and scripts/audio-virtual-source.sh up"]
fn a_device_that_cannot_be_silenced_alone_is_left_alone() {
    assert_eq!(
        mute_state(SINK),
        Some(false),
        "{SINK} is not there or is already muted: run scripts/audio-virtual-source.sh up"
    );

    let capture = Capture::open(
        Config {
            server: None,
            wanted: Arc::new(Wanted::new(Live {
                device: Some(format!("{SINK}.monitor")),
                mute_local: true,
            })),
        },
        |_frame: &[u8]| {},
    )
    .expect("a capture of the virtual output");
    assert_eq!(capture.device(), format!("{SINK}.monitor"));

    // **The whole point.** A host that muted this would be sending silence to
    // every guest while reporting that sound was on.
    assert_eq!(
        mute_state(SINK),
        Some(false),
        "the speakers were silenced on a device whose mute reaches the capture"
    );

    drop(capture);
    assert_eq!(mute_state(SINK), Some(false), "it was left muted");
}

/// What the server says about a sink's mute, or `None` if there is no such
/// sink.
fn mute_state(sink: &str) -> Option<bool> {
    let out = std::process::Command::new("pactl")
        .args(["get-sink-mute", sink])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let said = String::from_utf8_lossy(&out.stdout);
    Some(said.trim().ends_with("yes"))
}
