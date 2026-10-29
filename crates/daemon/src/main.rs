//! The host service: signaling on one side, the admission seam on the other.
//!
//! This is the only place the two meet, and deliberately so. The SDK does not
//! link a signaling client, so something above both has to translate one into
//! the other, and that translation is the whole of this program.
//!
//! Four inbound actions map onto the seam's four calls, and the seam's event
//! queue maps back onto two outbound actions. Nothing else here is protocol.
//!
//!   KESSEL_SESSION=... [KESSEL_WS_SERVER=...] lowlatd [--name NAME] [--port N]
//!   lowlatd session
//!   lowlatd display list|add|hold|remove|mode|modes|gpu|pointer|edid ...

// The service is written once; what the session's side of it is, is the
// platform's (docs/impl-plan-windows.md). The channel, the bus, the seat and
// the tray are how Linux's session side is reached. No host runs on macOS
// (docs/impl-plan-macos.md), so there it says so and exits.
#[cfg(any(target_os = "linux", windows))]
mod app;
#[cfg(target_os = "linux")]
mod channel;
#[cfg(target_os = "linux")]
mod dbus;
#[cfg(any(target_os = "linux", windows))]
mod display;
#[cfg(target_os = "linux")]
mod seat;
#[cfg(any(target_os = "linux", windows))]
mod service;
#[cfg(target_os = "linux")]
#[path = "session_side/linux.rs"]
mod session_side;
#[cfg(windows)]
#[path = "session_side/windows.rs"]
mod session_side;
#[cfg(target_os = "linux")]
mod sni;
#[cfg(windows)]
mod windows_install;
#[cfg(windows)]
mod windows_service;

#[cfg(any(target_os = "linux", windows))]
fn main() {
    service::main();
}

#[cfg(not(any(target_os = "linux", windows)))]
fn main() {
    eprintln!("lowlatd: no host runs on this platform");
    std::process::exit(1);
}
