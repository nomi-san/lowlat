//! Fixture endpoint driven by the real IO shell.
//!
//! Same role and same command line as `punch peer`, and deliberately so: the
//! namespace fixtures judge both by the lines they print, so swapping one for
//! the other changes what is under test and nothing else.
//!
//! What differs is everything below the command line. `punch` calls the
//! connectivity engine directly through a hand-rolled loop with a blocking read
//! and a timer read per pass. This owns a `Shell`: the real socket with its full
//! option set, batched receive, batched send, the wake descriptor, and a wait
//! armed from the endpoint's own deadline. The topologies are the same, so what
//! this adds is the shell itself.
//!
//! The reflexive candidate is polled from the engine rather than read off a
//! return value. A shell processes datagrams in batches and has nowhere to put a
//! per-datagram result, which is exactly why the engine retains it.
//!
//! With `--relay host:port`, `--relay-user` and `--relay-pass` the endpoint
//! makes a relay attempt instead (docs/03-connectivity.md 7.2): it publishes
//! the relayed address once the relay has one, takes the peer's address as the
//! host's own, and sends everything through the relay.

#[cfg(target_os = "linux")]
#[path = "shell-punch/linux.rs"]
mod linux;

#[cfg(target_os = "linux")]
fn main() {
    linux::main();
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("shell-punch: the shell is built on Linux so far");
    std::process::exit(1);
}
