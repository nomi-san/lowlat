# lowlat

Stream your Linux desktop to any Parsec client.

lowlat is an ultra-low-latency remote desktop host that speaks the Parsec protocol. Unmodified
Parsec clients connect to it on every platform they already run on, with no plugin, no forked
client, and no patched binary on the other end.

It targets **Linux first**. Unattended operation, headless operation, and running as a system
service are design inputs rather than afterthoughts.

## Status

**Pre-release. A Linux machine hosts, and stock clients stream from it.** Everything below has
been run against unmodified clients rather than argued for; the phase plan with each gate's
result is [docs/impl-plan.md](docs/impl-plan.md), and the working log is
[docs/changelog.md](docs/changelog.md).

What works today, measured on one desktop (KDE Plasma on Wayland, Debian 13):

- **Streaming** H.264 and HEVC, eight-bit and ten-bit, 4:2:0 and (where every encoder on the
  machine can) 4:4:4, from the display device below the compositor -- so the login screen and
  an unattended machine stream too. Encoders: NVENC, VA-API on AMD and Intel, and Vulkan Video;
  which hardware hosts, and what sets each floor, is [docs/09-compatibility.md](docs/09-compatibility.md).
- **Sound**: the desktop's output to the guests, and a guest's microphone taken by the host.
- **Input** through the kernel: keyboard, pointer, and an emulated controller.
- **The session side**: the desktop's layout and orientation followed live, a guest's request
  for a display size or a turn, the screen kept awake while somebody is watching, the
  clipboard in either direction behind a setting, a tray with a kick per guest and the rate,
  and a desktop notification when a guest arrives or leaves.
- **Browsers** as guests over a second pipe on the same signaling, including a page of our own
  ([examples/web-client](examples/web-client)).
- **Packaged**: a system service that starts at boot, a helper and a tray that start with each
  login, a login tool, and a Debian package.

What does not, yet:

- A Windows host.
- A guest's microphone as a device other applications on the host can open: the route is
  measured and not built.
- Relative pointer mode: an application that takes the pointer (mouselook) is not yet put into
  it, because the session-side signal for it is deferred.
- Per-guest pressure handling with more than one guest seated: guests are admitted up to a
  capacity of four, and the cascade that protects the others from a starved one is open work.
- Any local user may act on the host through the tray; authorising on the connection's
  credentials is deferred, with its cost written down.
- Every desktop but one: what GNOME and X11 sessions would need is in
  [docs/09-compatibility.md](docs/09-compatibility.md), unmeasured.

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
lowlat-crypto    credentials, key material, and the only source of randomness
lowlat-net       IO shell: sockets, threads, timers, wakeups; the browser transport
lowlat-sim       deterministic simulator and network namespace fixtures
lowlat-capture   scanout capture, the desktop's layout, and the display's own modes
lowlat-encode    NVENC, VA-API, Vulkan Video
lowlat-audio     sound capture, encode, and decode
lowlat-inject    uinput
lowlat-host      host orchestration
lowlat-sdk       the C ABI shared library, host and client halves as features
lowlat-kessel    signaling client
lowlatd          system service; also the session helper and the tray, as its
                 second and third roles
```

The protocol core is `no_std` and sans-IO: no sockets, no threads, no clock reads, no random
number generator, and no allocation. Time is a parameter and I/O is bytes in and bytes out.
That makes the transport and connectivity state machines fully deterministic, which is what
allows loss, reordering, and NAT topologies to be tested as reproducible unit tests rather
than as soak runs.

The SDK owns all of its threads and contains no async runtime, no TLS stack, and no JSON.
Signaling lives outside it, so an application can bring its own. A browser is served on the
same signaling over a second pipe, a data channel on the attempt socket, and
[examples/web-client](examples/web-client) is the smallest page that streams from it.

## Platform support

| | Status |
|---|---|
| Linux host | primary target |
| Windows host | planned |
| Clients | any platform with a stock Parsec client, or a browser; nothing to install |

Capture backends and their privilege requirements are covered in
[docs/07-platforms.md](docs/07-platforms.md); which hardware can host, and which desktops have
been measured, in [docs/09-compatibility.md](docs/09-compatibility.md).

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

Hosting needs a GPU with a hardware encoder reached through NVENC, VA-API or Vulkan Video,
and a display device the process may read; there is no software encoder, and a machine without
a hardware path is refused with the stage that failed named. The matrix of what hosts is
[docs/09-compatibility.md](docs/09-compatibility.md). Every encoder and graphics interface is
loaded at runtime, so the build has no such requirement and the tests run without a GPU
against a synthetic source.

The daemon needs access to `/dev/uinput` for input injection and to the display device for
capture, which is a capability rather than a group. The privilege requirements, the device
rules and the service unit are documented in [docs/07-platforms.md](docs/07-platforms.md).

## Installing

On Debian, build the package and install it; `cargo-deb` is the only build-time tool needed.

```sh
cargo install cargo-deb
cargo deb -p lowlatd
sudo dpkg -i target/debian/lowlat_*.deb
sudo lowlat-login --install
```

The package carries the service and its unit, a login tool, the two user units that start the
session helper and the tray with each graphical session, and `/etc/lowlat/lowlatd.env`, where
the signaling server and the session live; that file is a conffile and survives upgrades. The
service starts at boot and, until it has been logged in, says so and exits. Where the login
screen should be reachable too, the example under `/usr/share/doc/lowlat/` moves the greeter
onto a Wayland compositor.

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
| [09-compatibility.md](docs/09-compatibility.md) | which hardware and desktops host, and how each answer was established |
| [impl-plan.md](docs/impl-plan.md) | phases and verification gates |
| [changelog.md](docs/changelog.md) | working log, newest first |

## License

MIT. See [LICENSE](LICENSE).

Third-party components retain their own licenses. No copyleft library is linked or loaded:
the encoders are reached through the drivers' own interfaces, and the check is made against
the running process rather than the link graph.

## Disclaimer

lowlat is an independent implementation and is not affiliated with, endorsed by, or supported
by Parsec or Unity. "Parsec" is used only to identify the protocol that lowlat interoperates
with.
