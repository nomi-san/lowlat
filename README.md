# lowlat

Stream your Linux desktop to any Parsec client.

lowlat is an ultra-low-latency remote desktop host that speaks the Parsec protocol. Unmodified
Parsec clients connect to it on every platform they already run on, with no plugin, no forked
client, and no patched binary on the other end.

It targets **Linux first**. Unattended operation, headless operation, and running as a system
service are design inputs rather than afterthoughts.

## Status

**Design complete, implementation starting.** The protocol, IO shell, and host pipeline are
specified in [docs/](docs/); no code has landed yet. The phase plan and its verification gates
are in [docs/impl-plan.md](docs/impl-plan.md).

This README describes what lowlat is being built to do. Nothing here is a claim that it
currently does it.

## Why

**Parsec has never officially supported hosting on Linux.** The client ships for Linux and
always has, so a Linux machine can connect out to a host. The reverse has never been
available: a Linux machine cannot be connected *to*. lowlat closes that asymmetry, and it does
so on the existing protocol, so the clients people already have keep working unchanged.

The wider Linux picture has the same shape. Existing remote desktop tools fall into two
groups. Some capture only X11, which is a dead end on Wayland-default distributions and cannot
inject input below the display server. Others are session-bound, so they cannot serve a
machine you are not already logged into.

lowlat is built for the case where the host is a machine you connect *back* to: it runs as a
system service, injects through the kernel rather than through a display server, and is
designed so that the tray application is a convenience rather than a dependency. Close the
tray, log out, and the stream keeps running.

## Design goals

1. **Ultra low latency, before everything else.** Capture to present is budgeted per stage and
   measured, not estimated.
2. **Never display corruption.** Loss shows as a bounded micro-freeze of about one round trip,
   never as gray, torn, or smeared frames.
3. **Unmodified clients.** If a stock client cannot connect and stream, the feature is not
   done.
4. **A real Linux host.** Not a port of a Windows product.

## Architecture

```
lowlat-common    clock, futex wait, SPSC rings, byteorder, sequence arithmetic, log
lowlat-core      no_std sans-IO: wire, channels, rings, crypto, recovery, NAT, ICE, STUN, TURN
lowlat-net       IO shell: sockets, threads, timers, wakeups
lowlat-sim       deterministic simulator and network namespace fixtures
lowlat-capture   frame source trait and backends
lowlat-encode    NVENC, FFmpeg software, VAAPI
lowlat-inject    uinput
lowlat-host      orchestration and the C ABI shared library
lowlat-kessel    signaling client
lowlatd          system service
lowlat-tray      user session client
```

The protocol core is `no_std` and sans-IO: no sockets, no threads, no clock reads, no random
number generator, and no allocation. Time is a parameter and I/O is bytes in and bytes out.
That makes the transport and connectivity state machines fully deterministic, which is what
allows loss, reordering, and NAT topologies to be tested as reproducible unit tests rather
than as soak runs.

The SDK owns all of its threads and contains no async runtime, no TLS, and no JSON. Signaling
lives outside it, so an application can bring its own.

## Platform support

| | Status |
|---|---|
| Linux host | primary target |
| Windows host | planned |
| Clients | any platform with a stock Parsec client; nothing to install |

Capture backends and their privilege requirements are covered in
[docs/07-platforms.md](docs/07-platforms.md).

## Integration

The public surface is a stable C ABI: opaque handles, versioned plain structs, stable-numbered
enums, and poll-based calls. It is consumable from C, C++, C#, Rust, and anything else that
speaks C. The API shape follows the established host SDK, so porting an existing integration
is close to mechanical, but struct layouts are lowlat's own and binary drop-in compatibility is
not offered. Exported symbols carry a `lowlat_` prefix so a layout mismatch is a link error
rather than silent memory corruption.

See [docs/06-api.md](docs/06-api.md).

## Building

Requires a stable Rust toolchain, Rust 2024 edition.

```sh
cargo build --release
cargo test
cargo clippy --all-targets -- -D warnings
```

Hardware encoding requires an NVIDIA GPU with NVENC and a current driver. Software encoding
loads FFmpeg at runtime and is used where hardware encoding is unavailable, and in continuous
integration where no GPU is present.

The daemon needs access to `/dev/uinput` for input injection and to the display or GPU devices
for capture. Privilege requirements per capture backend, along with the udev rules and the
systemd unit, are documented in [docs/07-platforms.md](docs/07-platforms.md).

## Documentation

| Document | Contents |
|---|---|
| [00-overview.md](docs/00-overview.md) | decisions, lessons registry, system shape |
| [01-protocol.md](docs/01-protocol.md) | wire format, crypto, channels, recovery, opcodes |
| [02-io-shell.md](docs/02-io-shell.md) | threads, timing, wakeups, sockets |
| [03-connectivity.md](docs/03-connectivity.md) | NAT traversal, ICE, STUN, TURN |
| [04-signaling.md](docs/04-signaling.md) | signaling protocol and the application seam |
| [05-host.md](docs/05-host.md) | capture, encode, congestion, input, audio |
| [06-api.md](docs/06-api.md) | the C ABI |
| [07-platforms.md](docs/07-platforms.md) | display stacks, privileges, service topology |
| [08-testing.md](docs/08-testing.md) | test tiers, simulation, fuzzing, benchmarks |
| [impl-plan.md](docs/impl-plan.md) | phases and verification gates |

## License

MIT. See [LICENSE](LICENSE).

Third-party components retain their own licenses. FFmpeg is loaded dynamically at runtime and
is never linked, so GPL-licensed codec libraries stay out of lowlat's link graph.

## Disclaimer

lowlat is an independent implementation and is not affiliated with, endorsed by, or supported
by Parsec or Unity. "Parsec" is used only to identify the protocol that lowlat interoperates
with.
