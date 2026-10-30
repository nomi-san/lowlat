# Implementation plan: macOS

**Status:** planned 2026-10-29 at its interview. macOS gets the client only: there is no host on
macOS and none is planned. The platform seams (M0) come first and the client (M1) after them.
*Amended 2026-10-29:* M0 built and closed the same day; M1 is next. *Amended 2026-10-29:* M1.1
built, the network shell and the port mapper's platform side; M1.2 is next.

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
- [x] **M0.4 build**: dependencies scoped by target; the licence and advisory gate checking the
  Apple targets; a macOS job in CI that builds, lints and tests arm64 natively and x86_64 under
  the system's translation layer; AGENTS naming macOS's bar. *Built 2026-10-29.* The host's
  crates are the decoder's and the client's test dependencies only where a host runs (M0.2,
  M0.3). The licence and advisory gate checks both Apple targets and finds nothing new: no crate
  enters the graph for macOS alone. CI's macOS job lints and tests in the Windows form, each
  architecture on its own runner. The core's own cipher keeps its software path on arm64: the
  processor's instructions would take the simulator's longest test from 581 s to 442 s, and live
  sessions seal with the vetted library, which uses them already (the maintainer's decision).

**Gate:** the Linux and Windows tests and every lint as before, nothing there behaving
differently, and every crate with a macOS side building, linting and passing its tests on both
architectures. *Passed 2026-10-29*: on the development Mac, both architectures, 748 tests each;
Windows 1468; Linux 1433, its two dual-stack tests failing on a loopback that names one family
only, as before any of this (an environment, checked on the commit before). M0 is closed.

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

- [x] **M1.1 net**: the kernel-queue platform module ([02 §5, §6](02-io-shell.md)): the wait with
  a user-event wake and its deadline a critical timer, the queue's own timeout being coalesced
  (found at M0.1); the batched receive with each datagram's arrival address; sends from the
  address a path was proven at; the expedited mark on IPv6 by the socket's class and on IPv4 per
  datagram; don't-fragment for both families; the port taken free in both families, for a walk
  and for any port; the relay's stream never raising a broken-pipe signal. Checked by the
  shell's tests, which are the platform module's contract, with macOS arms where they lean on
  Linux's loopback, and by the soak; the receive, the wake and the mark measured, the mark over
  Wi-Fi under upload load. *Built 2026-10-29.* Decided at its interview: the port mapper's
  platform side too -- the default gateway, the interface a search leaves by, a connection a stop
  can cut short, the machine's name and identifier -- which the seam starts and this plan had not
  named, so every system call the client stands on below the decoder comes in this step; and the
  mark measured under the Mac's own upload, below. The wake is an armed flag beside a user event
  on the queue, as Windows's is beside a posted entry, so a take costs no call; the deadline is a
  one-shot critical timer armed in the call that waits. The batched receive is found by name,
  with a datagram a call where it is not. **Sends are one call a datagram**: the system's batched
  send takes a destination and each datagram's control messages, but measured no cheaper per
  datagram -- 4.2 us either way, 1.7 percent less in batches of two and 7 at 64 -- and a client
  stages one or two a pass. The relay's stream opens in the relay's own family, since a
  dual-stack stream refuses the v4 mark and has no per-send control message to carry it. The
  gateway is the routing table's answer for a destination beyond the network: a tunnel lists a
  default of its own, scoped to itself, that ordinary traffic does not take. Checked by the
  shell's and the mapper's tests with macOS arms -- the loopback holds 127.0.0.1 alone, and
  delivers after the send has returned, so a test that takes a burst in one call counts it
  queued first -- and by new checks of the mark in both families, don't-fragment, the guard
  bind, the deadline's timer, both receives, the stream's family and its signal, the gateway's
  answer, each failing when broken: 853 tests on each architecture, Windows and Linux as before.
  Measured on the development Mac: a queued burst of 2550 x 1200 B drains in 3.1 ms at the
  median batched and 4.1 a datagram a call, 1.2 and 1.6 us a datagram (2.9 and 4.4 ms on x86_64
  under the translation layer); a wake reaches the parked loop in 19 us at the median and 34 at
  the 99th percentile; a 5 ms deadline lands 50 us late at the median and 59 at the 99th. Bursts
  of that size sent from the development machine over the LAN lost nothing at the Mac's socket,
  in either receive, with 2 or 20 us of work a datagram; whole runs of datagrams were lost on
  the way in both alike, on the wireless leg or before it, which is M1's gate's to look at, not
  the socket's. **The mark is EF alone, by measurement**: with the Mac saturating its own
  wireless upload, a round trip to the development machine took 10.2 ms at the median, 13.4 at
  the 95th percentile and 18.4 at the 99th marked EF, against 13.8, 17.9 and 24.7 unmarked; the
  system's network service type on top of EF, video or voice, measured no better (p95 16.4 and
  15.7), so it is not set. The mark arrives on the wire as set whatever the class, and the
  replies carried none, so the gain is the Mac's own leg. Idle, all four were the same. Another
  station's upload contending for the air is still to measure.
- [x] **M1.2 the seam, VideoToolbox and the demo**: the seam and the library's client half on the
  shell, with the client's macOS modules (moved here from M0.3); VideoToolbox decoding H.264 and HEVC, ten-bit and full chroma where the system answers
  that it decodes them, its pictures leased as planes without a copy; the pair from the package
  managers' directories; the demo built for macOS. Checked by the reference clips and by sessions
  against both hosts. *Built 2026-10-30.* Decided at its interview: full chroma as two layouts
  appended, NV24 and P410, which are what the decoder writes, so it leaves without a copy
  (minor 34, the layouts named by an enumeration at the same minor); the system's frameworks
  opened by path at run time rather than linked, so the static library still links with nothing
  named; and the sessions below. **The decoder gives pictures back in the order it decoded
  them**, whatever the session is asked, and on its own thread even from a call that waits: the
  order is the library's reader's, as for every hardware backend, and a picture crosses through
  a hand-over that orders itself, model-checked. **A picture is the decoder's own buffer**, lent
  to the queue's slot and locked for reading until the ring lends that slot again, each plane at
  its own pitch, since the decoder aligns each on its own. A session outlives a rebuild that
  brings the same parameter sets back: 50 to 78 ms for a process's first, about 4 after it. The
  library carries the client half alone, the header declaring no host there, and is named
  `@rpath/liblowlat.dylib`, which the linker had left as the path it was built at. The demo draws
  through Metal; the toolkit's upload takes each plane's own address and row length on every
  platform; the makefile builds the demo for either architecture, and the SDL script one static
  library holding both. Checked by every reference clip bit for bit and in order through the
  hardware and through the system's software decoder (the multi-slice H.264 clip, which that
  decoder gets wrong, skipped there), the library's gates in their macOS arms and the client's
  tests with macOS arms, each new check failing when broken: 979 tests on each architecture,
  Windows and Linux as before. CI's x86_64 job, a virtual Mac running the tests under the
  translation layer, found a session the system chose failing every first picture where a
  software-only one decoded every clip: such a refusal now hands the stream to the software
  decoder, as the probe does, its status logged. **Sessions**, five minutes each by planes on
  the development Mac's 60 Hz panel over the LAN, every one ending as asked, the loss at most
  0.03 percent: from
  this project's host on the development machine, a 1080p picture in every layout the decoder
  hands out -- H.264 and HEVC (NV12), Main10 (P010), 4:4:4 (NV24) and 4:4:4 ten-bit (P410) -- at
  54 to 60 pictures a second, the decode 1.9 to 2.2 ms at the median and at most 4.1 at the
  99th percentile of the per-second figures, arrival to presented 17 to 29 ms at the median, 32
  to 38 percent of a core and a footprint of 88 to 98 MB (137 once, at full chroma); from an
  established host across the Internet (13 to 15 ms away), H.264 and HEVC at the 15 pictures a
  second it sent, the decode 2.7 and 2.0 ms, arrival to presented 10 ms, 20 percent of a core;
  and HEVC from this project's host by an x86_64 demo under the translation layer, video alone,
  at 59 pictures a second, the decode 1.1 ms, 37 percent of a core and 106 MB. The host's cursor
  drawn at twice its size on the Mac's 2x display, and the system's arrow coming back on some
  moves, are M1.4's.
- [x] **M1.3 the handle**: one IOSurface per picture, appended to the frame ([06 §3b](06-api.md));
  the demo's renderer importing it by plane. Measured against planes. *Built 2026-10-30.* Decided
  at its interview: minor 35, 34 being on the remote by then; `allocation` the surface's
  identifier, unique among the surfaces alive and kept from naming another by an import, which
  retains its surface (measured: an identifier comes back the moment its surface is freed, never
  while a texture made from it lives); and the legs below. A picture asked for by handle is the
  decoder's own buffer held in its slot until the slot is lent again -- the rule the planes kind
  keeps -- the kind followed per picture; IOSurface.framework is the fifth framework opened by
  path. The toolkit's Metal renderer imports a picture's planes by index once per allocation,
  eight kept, bound for the draw alone; its present waits for the GPU on the queue it drew on, so
  the demo's release at the next acquire needs no fence. The demo's line each second gained the
  kind on screen and the process's processor time. Measured before the build: a plane imports in
  about 20 us, where the planes kind's upload of a 1440p picture costs 0.75 to 1.65 ms, 3.0 to
  5.1 under the translation layer. **Legs**, ten minutes each against this project's host on the
  development machine at 1080p and 60, alternating the kinds every 30 seconds so both ran on one
  path and one phase of the refresh: by handle the process took 25 percent of a core against 35
  by planes, for H.264 and 4:4:4 ten-bit alike, and 31 against 39 under the translation layer
  (p95 32 against 56); a picture reached the screen in 11.0 against 18.1 ms at the median for
  H.264, 20.6 against 23.2 at 4:4:4 ten-bit, 19.0 against 22.0 under the translation layer. The
  decode itself measured slower by handle -- 2.5 against 2.3 ms at the median for H.264, 2.8
  against 2.2 at 4:4:4 ten-bit, 2.5 against 1.1 under the translation layer -- likely the
  system's clocks following the processor's lighter load, not proven.
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

- 2026-10-30: M1.3 built: the handle on macOS, each picture its IOSurface (minor 35), drawn
  through Metal by the demo; three alternating legs against planes.
- 2026-10-30: M1.2 built: VideoToolbox, the client half and the demo on macOS; full chroma in
  two planes and the layouts named by an enumeration (minor 34); the library found through the
  application's run path; eight sessions against both hosts.
- 2026-10-29: M1.1 built: the network shell's macOS module and the port mapper's, the mapper
  moved here at the step's interview; one send a datagram, by measurement.
- 2026-10-29: M0.1 to M0.4 built, M0's gate passed and M0 closed; M0.3 corrected at the build,
  the library's client half moving to M1.2 with the seam it stands on.
- 2026-10-29: planned at its interview: M0, the platform seams, and M1, the client; no host on
  macOS.
