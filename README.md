# lowlat

Stream your Linux desktop to any Parsec client.

lowlat is an ultra-low-latency remote desktop host that speaks the Parsec protocol. Unmodified
Parsec clients connect to it on every platform they already run on, with no plugin, no forked
client, and no patched binary on the other end.

It targets **Linux first**. Unattended operation, headless operation, and running as a system
service are design inputs rather than afterthoughts.

## Status

**Pre-release. A Linux machine hosts, and stock clients stream from it; the same library's
other half is a client, and it streams from established hosts.** Everything below has been
run against unmodified peers rather than argued for; the phase plans with each gate's result
are [docs/impl-plan.md](docs/impl-plan.md) and [docs/impl-plan-client.md](docs/impl-plan-client.md),
and the working log is [docs/changelog.md](docs/changelog.md).

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
  login, a login tool, a Debian package, and an install script for distributions without one.
- **A client**, the other half of the same library and header, on Linux: pictures decoded
  through the open stack, the vendor's interface, or the machine's own codec library where
  it is an LGPL build, and handed out as planes or as a device handle; sound, input, the
  host's pointer, a DualShock 4 or a DualSense sent as its own reports. Measured for ten
  minutes at a time against an established host at every phase
  ([docs/10-client.md](docs/10-client.md)). **A relay attempt** reaches a host no punch
  reaches, through a relay on the host's own machine behind the one port its router forwards,
  with no part taken by the host; streamed that way for twelve minutes against an established
  host at 26 ms against 27 direct, and against this project's own host with a relay beside
  it. The C demo on the application toolkit is
  [examples/client](examples/client).

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
lowlat-drivers   the device interfaces reached at runtime, shared by encode and decode
lowlat-capture   scanout capture, the desktop's layout, and the display's own modes
lowlat-encode    NVENC, VA-API, Vulkan Video
lowlat-decode    the H.264 and HEVC readers and the picture buffer; VA-API decode
lowlat-audio     sound capture, encode, and decode
lowlat-inject    uinput
lowlat-host      host orchestration
lowlat-client    the connecting side: the receive path, the decode thread, the frame queue
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
| Linux client | the client half of the library, and the demo; what decodes on which part is [docs/09-compatibility.md](docs/09-compatibility.md) section 7a |
| Windows client | planned, after Linux |
| Guests of a host | any platform with a stock Parsec client, or a browser; nothing to install |

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

The library builds in two forms: whole, and with the client half alone
(`cargo build -p lowlat-sdk --no-default-features --features client`); `lowlat_features()`
says which one was loaded, and the header is the same for both. The client demo is
`make -C examples/client`, which builds the application toolkit from its vendored tree
first; that needs a C compiler, the shader compiler (`glslang-tools`) and the ALSA, PNG
and JPEG headers. The build workflow produces both libraries as tarballs, the client one
with the demo inside, each with a `FEATURES` line read from its own copy.

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
the session lives, and the signaling server when it is not the public one; that file is a
conffile and survives upgrades. The service starts at boot and, until it has been logged in,
says so and exits; the login is the whole of the setup. Where the login screen should be
reachable too, the example under `/usr/share/doc/lowlat/` moves the greeter onto a Wayland
compositor.

Where there is no package, `packaging/install.sh` puts the same files in the same places and
enables the same units; the build is yours and the install is root's:

```sh
cargo build --release -p lowlatd
sudo packaging/install.sh
sudo lowlat-login --install
```

`--uninstall` takes it all away again but the configuration, which carries the login's
session; `DESTDIR` stages the files under a root of its own and touches nothing else, for a
distribution's package recipe. What the machine needs at run time is loaded, not linked:
`python3` for the login tool; a sound server speaking the PulseAudio protocol (PipeWire's
pulse server is one) and its client library; and the user-space driver of the GPU that
encodes -- NVIDIA's for NVENC; for VA-API, Mesa's on AMD or Intel's media driver; the Vulkan
loader for Vulkan Video. A Mesa built without the H.264 and HEVC codecs, as some
distributions ship it, offers VA-API no encoder at all.

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
| [10-client.md](docs/10-client.md) | the client: receive path, frame queue, keyframe policy, decode, sound, input |
| [impl-plan.md](docs/impl-plan.md) | phases and verification gates |
| [impl-plan-client.md](docs/impl-plan-client.md) | the client's phases and gate |
| [changelog.md](docs/changelog.md) | working log, newest first |

## License

MIT. See [LICENSE](LICENSE).

Third-party components retain their own licenses. No copyleft library is linked, and none is
loaded but one: the encoders and the hardware decoders are reached through the drivers' own
interfaces, and the client's software decoder is a libavcodec the machine already carries,
used only when that library answers that it is an LGPL build. The check is made against the
running process rather than the link graph. A library built from source with the
`gpl-libavcodec` feature accepts a GPL libavcodec as well; that build is the builder's own
combination, the GPL's terms apply to it, and it reports itself through `lowlat_features`.
No release build carries the feature.

## Disclaimer

lowlat is an independent implementation and is not affiliated with, endorsed by, or supported
by Parsec or Unity. "Parsec" is used only to identify the protocol that lowlat interoperates
with.
