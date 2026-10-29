# Implementation plan: macOS

**Status:** planned 2026-10-29 at its interview. macOS gets the client only: there is no host on
macOS and none is planned. The platform seams (M0) come first and the client (M1) after them;
nothing is built.

## Why the seams come first

The workspace does not build for macOS. Checked on the development Mac at `b69a338`: the common
crate stops at its absolute-deadline sleep, which uses a call macOS does not have; past it, the
audio crate's capture and the driver bindings have no macOS side, and the client, the decoders,
the IO shell and the library sit behind them. As at W0 ([impl-plan-windows.md](impl-plan-windows.md)),
the first phase moves code and adds no behaviour: Linux and Windows stay exactly as they are, and
macOS gets a named place -- a module of the same name per platform, never a trait.

## Phase M0 - The platform seams

- [x] **M0.1 common**: the absolute sleep without the call macOS lacks, the sleep's overshoot
  there measured to set the spin margin; the address wait on the system's own primitive
  ([02 §3](02-io-shell.md)); libraries loaded from the application's directory and the system's,
  and for the codec pair the package managers' ([10 §5.3](10-client.md)), never the current one or
  a search path. *Built 2026-10-29.* The sleep is a kernel-queue timer marked critical
  ([02 §2](02-io-shell.md)): the system lets every ordinary timed wait run late by an eighth to a
  quarter of its length (2 ms landed 260 to 510 us late), a critical timer 30 us at the median and
  60 at the 95th percentile, so the spin margin stays 100 us. The address wait is the public pair
  where found and the private one below it, each driven by name in a test; a wake reaches a waiter
  parked in the kernel in 17 us at the median through either, no sooner than through the portable
  table, so what they buy is a wait per address with no shared bucket. The loader refuses a bare
  name on macOS, where the system's search for one takes the working directory (measured: a
  library planted there opened). The common tests pass on arm64 and on x86_64 under the
  translation layer, 65 each, and on Linux and Windows as before; each new check failed when
  broken.
- [x] **M0.2 audio, drivers, decode**: the GPU vendors' runtimes and their bindings left out of
  macOS; the audio crate's capture, which is the host's, empty there, its codec unchanged.
  *Built 2026-10-29.* NVIDIA's compute and decode runtimes, the encoder's generated headers and
  the decoder over them, and the packed full-chroma layouts only those decoders hand back, are
  Linux's and Windows's; so are the sound capture and its test. The decoder crate's tests take
  the host's encoder and capture crates where a host runs alone, as do the tests of the
  encoders' writers. The three crates build, lint and pass their tests on arm64 and on x86_64
  under the translation layer in the Windows form (`--lib --bins --tests`): their examples probe
  one platform's hardware. Linux and Windows unchanged.
- [x] **M0.3 client and library**: a macOS module, empty, for each of the client's per-platform
  modules; the library's client half building as a shared and a static library, the host half
  and the display controller left out ([00 D14](00-overview.md)). *Built 2026-10-29, corrected
  at the build*: the client stands on the network shell's platform module -- the decode thread
  wakes the session through it, the seam opens its socket -- which macOS gets at M1.1, so the
  client and the library build empty there until the seam and the library's client half come
  with M1.2, as W0.6 had them build on Windows; the client's macOS modules move to M1.2 with
  them. The host's crates build empty on macOS for good, and the service says no host runs
  there and exits. The whole workspace builds and lints for both architectures, and its tests
  pass there -- 748 on each -- and on Linux and Windows as before. The simulator's long
  trajectory test takes 581 s on arm64 against 186 on Windows: the core's own cipher runs in
  software on arm64 unless told otherwise, which M0.4 settles.
- [ ] **M0.4 build**: dependencies scoped by target; the licence and advisory gate checking the
  Apple targets; a macOS job in CI that builds, lints and tests arm64 natively and x86_64 under
  the system's translation layer; AGENTS naming macOS's bar.

**Gate:** the Linux and Windows tests and every lint as before, nothing there behaving
differently, and every crate with a macOS side building, linting and passing its tests on both
architectures.

## Phase M1 - The client

**Planned 2026-10-29, interview of the same day.** The client half of the library on macOS, and
the demo with it, built and checked on a MacBook Air (Apple M2, macOS 26.6.2) on Wi-Fi. The
decisions are recorded once, here; the design is [10 §4.3 and §5.3](10-client.md), the boundary
[06 §3b](06-api.md), the socket [02 §5 and §6](02-io-shell.md), the platform [07 §13](07-platforms.md),
and D14's amendment [00](00-overview.md).

### Decisions taken at the interview

- **macOS 11 on arm64 and x86_64, and the library ships universal.** Each architecture is built
  and tested on its own, then the two are joined; CI tests arm64 natively and x86_64 under the
  system's translation layer. Signing is ad hoc, notarization only with a developer identity.
  Nothing planned needs a later system: VideoToolbox's HEVC and its hardware query are 10.13, an
  IOSurface imported into Metal 10.11, the kernel queue's user event 10.6, the network service
  type 10.12, and the language's own floor is 11 on arm64. An entry point newer than 11 is found
  at run time and never linked by name, since a missing symbol stops the load on an older
  system. Systems older than the development Mac's are declared, not tested.
- **The system's interfaces are generated bindings, as on Windows.** The binding generator runs
  on a Mac over the macOS SDK's VideoToolbox, Core Media, Core Video, IOSurface and Core
  Foundation headers, allowlisted, and its output is checked into the driver crate with its
  layout tests; no new dependency. One set serves both architectures, whose layouts the tests
  check on each.
- **Two private system calls are taken, each found at run time with a public fallback.** The
  address wait is `os_sync_wait_on_address` where the system has it (14.4 on) and `__ulock`
  below it, never the portable table ([02 §3](02-io-shell.md)). The receive is `recvmsg_x`, the
  system's only batched receive ([02 §6](02-io-shell.md)), which hands back each datagram's
  control messages, so the arrival address comes with the batch; a per-datagram receive is the
  fallback. Measured at the plan, draining 2000 queued datagrams of 1200 bytes: 0.39 us a
  datagram batched, 1.09 one at a time. Both are measured again at M1.1, with a burst over the
  LAN while the receive thread is busy, and the batched send beside them rather than assumed.
- **The demo keeps its toolkit, patched where the toolkit falls short on macOS**: pictures of the
  handle kind drawn by its Metal renderer, which refuses them as shipped; key releases caught
  while Command is held, which the toolkit's window never hears, so a host sees the key held
  ([05 §7](05-host.md)); and Caps Lock, which the toolkit reports as the lock's state, sent by
  the demo as a press and a release, so a host toggles once a press.
- **A picture of the handle kind is one IOSurface**, retained by the library for the lease, its
  planes described by the frame's own fields and imported by plane. Every graphics interface on
  the system imports one, so the application needs no Core Video, and the decoder's buffers,
  their pool and their colour stay inside the library. The application releases the lease once
  its own GPU work on the picture has completed. Appended to the frame, a minor when built.
- **Software decoding is the machine's own LGPL pair, as on Linux** ([00 D14](00-overview.md)):
  nothing shipped, loaded at run time and only when it answers LGPL, last after VideoToolbox. On
  a Mac the machine's own is a package manager's, and the dynamic linker's own search does not
  reach Homebrew's directory on Apple Silicon (measured), so after the variable and the
  application's directory the library walks the package managers' library directories by full
  path, the architecture's own first ([10 §5.3](10-client.md)).
- **The gate is the development Mac on Wi-Fi**, against an established host and this project's
  host on the development machine (below).

### Steps

- [ ] **M1.1 net**: the kernel-queue platform module ([02 §5, §6](02-io-shell.md)): the wait with
  a user-event wake and its deadline a critical timer, the queue's own timeout being coalesced
  (found at M0.1); the batched receive with each datagram's arrival address; sends from the
  address a path was proven at; the expedited mark on IPv6 by the socket's class and on IPv4 per
  datagram; don't-fragment for both families; the port taken free in both families, for a walk
  and for any port; the relay's stream never raising a broken-pipe signal. Checked by the
  shell's tests, which are the platform module's contract, with macOS arms where they lean on
  Linux's loopback, and by the soak; the receive, the wake and the mark measured, the mark over
  Wi-Fi under upload load.
- [ ] **M1.2 the seam, VideoToolbox and the demo**: the seam and the library's client half on the
  shell, with the client's macOS modules (moved here from M0.3); VideoToolbox decoding H.264 and HEVC, ten-bit and full chroma where the system answers
  that it decodes them, its pictures leased as planes without a copy; the pair from the package
  managers' directories; the demo built for macOS. Checked by the reference clips and by sessions
  against both hosts.
- [ ] **M1.3 the handle**: one IOSurface per picture, appended to the frame ([06 §3b](06-api.md));
  the demo's renderer importing it by plane. Measured against planes.
- [ ] **M1.4 the desk**: the toolkit's two key faults mended; relative mode; the host's cursors;
  pads, sound and the microphone through SDL, with the microphone's permission; the threads'
  quality-of-service classes and the system's app napping decided by measurement.
- [ ] **M1.5 packaging**: the universal library, the header and the demo's application bundle in
  a zip from the build workflow, signed ad hoc.

**Gate:** Gate C ([impl-plan-client.md](impl-plan-client.md)) on the development Mac over Wi-Fi,
against an established host and this project's host on the development machine:

- H.264 and HEVC, and ten-bit and full chroma where the host sends them, ten minutes each in both
  kinds, with decode and library latency at p50, p95 and p99, processor time and memory;
- the keyboard, Command's and Caps Lock's cases included, against the established host; the
  mouse in both modes; a DualSense; sound and the microphone;
- x86_64 under the translation layer, for video and input;
- the software pair: the default build refusing the machine's GPL pair by its licence, and a
  build that allows a GPL pair decoding both codecs through it from the package manager's
  directory, with no variable set;
- the same legs' numbers beside the other macOS clients' on the same host.

## Change log

- 2026-10-29: planned at its interview: M0, the platform seams, and M1, the client; no host on
  macOS.
