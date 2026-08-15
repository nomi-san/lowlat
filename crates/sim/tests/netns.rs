//! Phase 2 gate 1, integration tier: the topology matrix against a real kernel.
//!
//! The simulator proves the state machine against translation we described.
//! This proves it against translation somebody else implemented, which is the
//! only thing that checks the description rather than restating it. It has
//! already earned that: two behaviours the simulator models correctly turned out
//! to need real arrangement to reproduce here, and one of them would have made
//! a fixture confirm the opposite of what it was written to check.
//!
//! Creating namespaces needs privilege, which is the one place the suite wants
//! it. Without it this skips with a stated reason rather than failing, because a
//! skip that reads as a failure trains people to ignore failures.

#![cfg(target_os = "linux")]

use std::path::Path;
use std::process::Command;

/// The script reports this exact line when every topology behaved as expected.
const SUCCESS: &str = "0 failed";

/// And this when it could not run at all.
const SKIPPED: &str = "skipped:";

#[test]
fn the_topology_matrix_holds_against_a_real_kernel() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root");

    let output = Command::new("bash")
        .arg("scripts/netns-fixtures.sh")
        .current_dir(root)
        // Cargo built the endpoint for this test, so the script never has to
        // guess where it is or whether it is current.
        .env("PUNCH", env!("CARGO_BIN_EXE_punch"))
        .output()
        .expect("bash is required to run the namespace fixtures");

    let report = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if report.contains(SKIPPED) {
        eprintln!("namespace fixtures skipped: {}", report.trim());
        return;
    }

    assert!(
        report.contains(SUCCESS),
        "namespace fixtures reported failures\n--- stdout ---\n{report}\n--- stderr ---\n{stderr}"
    );
    println!("{}", report.trim());
}
