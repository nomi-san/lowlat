//! What the pre-flight says this machine can do, and at what privilege.
//!
//!   can-host
//!
//! **The answer that matters is the middle one.** Opening the display device
//! and reading its modes needs group membership; reading the buffer handles of
//! what it is scanning out needs more, and everything after that fails on
//! their absence in a way that looks exactly like an empty desktop.

fn main() {
    println!("capturable: {:?}", lowlat::display::Display::capturable());
}
