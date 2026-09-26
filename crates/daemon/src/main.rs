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

// The program is Linux's so far (docs/impl-plan-windows.md); elsewhere the
// binary says so and exits.
#[cfg(target_os = "linux")]
mod app;
#[cfg(target_os = "linux")]
mod channel;
#[cfg(target_os = "linux")]
mod dbus;
#[cfg(target_os = "linux")]
mod seat;
#[cfg(target_os = "linux")]
mod service;
#[cfg(target_os = "linux")]
mod sni;

#[cfg(target_os = "linux")]
fn main() {
    service::main();
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("lowlatd: not built for this platform yet");
    std::process::exit(1);
}
