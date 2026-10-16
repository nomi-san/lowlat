# Implementation plan: Windows

**Status:** planned 2026-09-26. Phase W0, the platform seams, is closed; the client's phase
(W1) was planned at its interview the same day, and the host's (W2) at its own on 2026-10-14,
the day the host's own virtual display (W3) was added with its frame hand-off decided; W3 was
planned at its own interview on 2026-10-15, and its first step, the driver and its package,
built the same day.

Conventions as [impl-plan.md](impl-plan.md): a gate is a command that passes or a peer that
streams, one phase per commit, changelog entry before the checkbox.

## Why the seams come first

Everything so far was built for Linux with no platform scaffolding. The IO shell compiled
only there; the shared primitives' Windows side was a stand-in (a wait on a mutex-and-condvar
table, no timer resolution, a clock that read zero); the Linux-only dependencies were
unconditional; the generated driver bindings carried Linux type sizes; and the host's frame
loop polled a display descriptor itself. A Windows backend written on top of that would land
as conditional compilation inside the longest functions in the tree.

So W0 moves code rather than adding behaviour. Each step keeps Linux exactly as it was and
gives the Windows side a named place. Per-platform code is a module of the same name on each
platform, never a trait with one implementation per build, which leaves a compile on each
platform as the only contract keeping the two in step: CI builds both.

## Phase W0 - The platform seams

- [x] **W0.1 common**: the address wait on the system's own primitive; the timer resolution
  raised by each library handle for its life ([02 §2](02-io-shell.md)); the arrival clock on
  the performance counter; libraries loaded from the application's and the system's
  directories, never the current one or the search path. *Built 2026-09-26; the common tests
  pass on Windows, run here under a compatibility layer, and the wake test fails with the
  wake removed.*
- [x] **W0.2 net**: the system calls apart from the loop, so the loop, the attempt thread,
  the browser transport and the address filters compile everywhere and the completion-port
  receive has one module to fill ([02 §6](02-io-shell.md)). *Built 2026-09-26: the platform
  module owns the socket, the wake and the receive storage together; the receive tests now
  run through the loop's own receive path, so they are the contract the next platform's
  module meets. The browser transport and the address filters build for Windows; the loop
  waits for that platform's module.*
- [x] **W0.3 drivers and decode**: bindings generated per target; the open-stack interface
  on Linux only; the vendor runtimes by their Windows names; descriptors confined to Linux.
  *Built 2026-09-26: the vendor interfaces generated for Windows beside Linux, their layout
  assertions compiling for both; the compute runtime's descriptor interop in a module of its
  own, which is where a Windows half goes. Both crates build and lint for Windows, and their
  tests and the core's pass there, run under a compatibility layer.*
- [x] **W0.4 client**: decoder selection and the device-backed picture slots as a module per
  platform. *Built 2026-09-26: the automatic order and the devices it walks, the decoders the
  decode thread opens, and the decoder table are each a platform module; what a client
  opened says which backend it is through an accessor, which the library's status reads. The
  device-backed picture slots stay where they are: their Windows form -- shared textures and
  a fence, the handle kind the boundary appends -- is designed with the client's phase.*
- [x] **W0.5 host**: the frame loop waits for a present through the display rather than on a
  descriptor; the encoder builders move out of the frame loop's module, one module per
  platform; types every platform shares move out of the Linux modules; the guest loop takes
  its input devices from a module per platform. *Built 2026-09-26: the loop and the guest
  loop make no system call of their own; the encoder built on the display's device, the
  choice of it and the full-chroma census are the platform's; the display's shared types
  -- the outputs listed, a pre-flight's answer, what a capture produced -- sit apart from the
  Linux display, as does where an output sits in the desktop.*
- [x] **W0.6 build**: Linux-only dependencies scoped to Linux, and a Windows job in CI.
  *Built 2026-09-26, ahead of W0.4 and W0.5 because both halves' work starts from a
  workspace that builds: the crates with no Windows side yet -- capture, encode, inject,
  host, client and the library itself -- build empty there, and the service and the shell's
  fixture endpoint say they are not built for the platform and exit. `cargo build`,
  `cargo clippy` and `cargo test` over the whole workspace (`--lib --bins --tests`) pass for
  Windows, the tests run here under a compatibility layer.*

**Gate:** the Linux tests and every lint as before, nothing on Linux behaving differently,
and the Windows target building every crate that has a Windows side.

## Phase W1 - The client

**Planned 2026-09-26, interview of the same day.** What [impl-plan-client.md](impl-plan-client.md)
left for later, built on a machine that carries every vendor's device: an NVIDIA card driving
the display, an Intel card beside it and AMD's integrated graphics. The decisions are recorded
once, here; the design is [10 §4.2 and §5.2](10-client.md), the boundary
[06 §3b](06-api.md), and D14's amendment [00](00-overview.md).

### Decisions taken at the interview

- **The client decodes on the GPU the application names, which is the one it renders on.**
  On Windows a device is named by its adapter identity, where a render node names one on
  Linux. A renderer on another GPU than the display pays a copy of every presented picture
  across the bus and loses the direct flip, so the application keeps its renderer on the
  display's GPU and the library follows it; the demo names the GPU the toolkit makes its
  device on, and says so when that is not the display's.
- **A picture of the handle kind is one shared texture per plane**, in the legacy form a
  renderer in the same process opens by its handle: a luma and a two-channel chroma texture
  at eight and ten bits, three single-channel textures for full chroma. Every frame says
  which GPU its textures are on. No other graphics interface is served.
- **A picture of the handle kind is finished on the device before it can be acquired, and
  the decode thread never waits for it** (*scoped to the handle kind at W1.3*: a picture read
  back to planes is waited for in its read-back, as on Linux, the wait sleeping on the
  device's progress). The decode thread queues the decode, the split into the plane
  textures and a signal of the library's own fence, and takes the next unit; a picture
  becomes acquirable when the fence has passed it, so the newest picture is the newest
  finished one, and only an acquire, on the application's thread and within its timeout,
  ever sleeps on the fence. An earlier implementation tried both of the other ways: waiting on
  its decode thread, it fell behind the stream past 60 ms under contention on an integrated
  GPU; handing a picture out before its device work was known to be done, it showed pictures
  out of order on one vendor's driver. The release fence keeps its one kind, none: the
  application's device waits on nothing of the library's.
- **Each slot carries its own backing**, made again when it comes back free after the
  session's backing has moved on. One mechanism serves three changes: the session moved to
  another GPU by `lowlat_client_set_decoder`, which a session of the handle kind now accepts,
  a picture still held staying valid on the old GPU until it is released; planes and handles
  switched mid-session; and the device lost to a driver update or a reset, after which the
  GPU may come back under a new identity and is named again the same way.
- **`lowlat_client_set_frame_kind` switches planes and handles at the next picture**, with no
  keyframe: the decoder keeps running and keeps its references, and only how its pictures
  leave changes. A decoder that cannot hand out the kind refuses it.
- **The automatic order**: D3D11 video decoding driven by the library's own readers, NVDEC,
  AMD's AMF, Intel's VPL (with Intel's older MFX for parts the newer runtime cannot reach),
  an LGPL libavcodec pair, and the system's own software decoder. W1 builds D3D11, NVDEC, the
  pair and the system's decoder. The two vendors' own decoders wait until D3D11 has run on
  each GPU, because what they would add -- a missing profile, a driver fault, speed -- is
  known only then. (*Amended 2026-09-28, after W1.3*: on the AMD GPU D3D11 proved short on
  speed, so W1 builds AMF as well, now W1.6, and on an AMD GPU it comes first. *Amended again
  after W1.4*: W1 builds Intel's VPL too, as W1.7, though D3D11 did not prove short on the
  Intel card, so that every Intel part has its vendor's runtime, the older ones through MFX;
  on an Intel GPU it comes first only if it measures faster than D3D11. NVDEC moves ahead of
  AMF, as W1.5.)
- **The software decoders**: the pair keeps D14's rule, loaded at runtime only when it is an
  LGPL build and never shipped, and is found in the directory the application names or beside
  the application, by the platform's versioned names. The system's decoder is software only
  and eight-bit 4:2:0 only, H.264 on every edition and HEVC where the system's HEVC extension
  is installed, and it comes after the pair. (*Amended 2026-10-01, at W1.8*: HEVC at ten bits
  as well, which the extension decodes in software exactly; H.264 stays at eight bits, the
  decoder hanging on its deeper profiles.)
- **The hermetic session runs on Windows**, CI included: the host's session and packetiser
  and the injection crate's event, pad and usage modules build there, and every platform
  module stays Linux-only until W2.
- **The library and the demo link the C runtime statically**, so neither needs a
  redistributable, and the demo's makefile for the platform's compiler sits beside the GNU
  one. The client ships as a zip: the library, its import library, the header and the demo.
  (*Amended 2026-10-13*: the demo's Windows build is CMake, with the presets an application
  that links the library into itself uses, clang-cl's needing no developer environment; it
  links the library from its static build, and a release build optimises the demo's own
  code at the link. The GNU makefile stays for Linux.)
- **The demo's toolkit gains two things**: a choice of the GPU its device is made on, for a
  menu, and a pad's feature reports read and written by the toolkit's own controller id. A
  Sony pad's input reports come from the toolkit's events and its output reports go out
  through the toolkit, so the demo never opens a pad beside it. (*Amended 2026-10-13, at
  W1.9*: the demo's pads and sound are SDL 3's instead, on both platforms -- its gamepads for
  the states, its HID layer for a pad sent as reports, which is never opened as a gamepad --
  and the toolkit's second gain is a flag that leaves controllers alone, since on Windows it
  reads every pad whatever the application asks and writes a lightbar of its own on a
  PlayStation pad's first report. SDL comes from vcpkg on Windows, static; on Linux from the
  system, or where the system has none from a static build of the same release, which is what
  the build workflows link.)

### Steps

- [x] **W1.0 build**: the client and the modules the hermetic session uses build for
  Windows, and the C runtime is linked statically. Checked by the hermetic session passing
  on Windows, here and in CI. *Built 2026-09-26.* What the seam says -- its events, outcomes
  and errors, and what reaches the session loop -- moved into a module of its own, so the
  driver, the feed, the sound and the input build everywhere, and the seam, which opens the
  socket and runs the shell, is built with the shell in W1.1. The library's client half sits
  on the seam and moved there with it (*corrected at the build*: this step had it). On
  Windows the client's 40 tests, the hermetic session's 16, the negotiation and packetiser's
  23 and the injection crate's 63 pass, and a test binary imports no C runtime library. The
  environment's compiler flags replace the configuration's rather than adding to them, so
  CI's Windows job names the static runtime again.
- [x] **W1.1 net**: the completion-port platform module ([02 §5, §6](02-io-shell.md)): the
  socket and its options, never address reuse, since a second bind succeeds there; the
  source address taken through the message receive; a wake posted once until it is taken;
  receive slots pinned for the socket's life and drained up to 256 a call; cancel and drain
  before a slot is freed; the established path marked per destination. Checked by the
  shell's tests, which are the platform module's contract, and by the soak. *Built
  2026-09-26.* Decided at its interview: the system's own declarations for the socket, the
  port and the interface walk, since the system writes several of those structures after a
  call has returned; the plain completion port, with registered I/O taken only if it
  measures better, since its socket refuses the ordinary calls and would be a second module;
  and the mark, through the system's QoS service loaded at run time, since a per-socket type
  of service reaches the wire as zero there. *Corrected at the build*: the seam and the
  library's client half move to W1.2 -- the seam chooses and opens the decoders, which that
  step writes for this platform -- and the QoS service marks the wildcard-bound socket only
  connected, so the flow is asked for connected and the socket disconnected again
  ([02 §5](02-io-shell.md)). On Windows natively the net crate's 65 tests, the soak and the
  browser-shell pair pass; the churn soak holds handles, threads and the working set flat;
  the port's wait asked for 1 ms lasts 2.0 ms with the resolution raised and 16.0 without;
  the plain port hands over a queued keyframe-sized burst in 1.8 ms, 695 ns a datagram.
- [x] **W1.2 the seam, software, and the demo on Windows**: the seam and the library's
  client half on the shell, with the platform's decoder choice (*moved here from W1.1 at its
  build*); the libavcodec pair loaded there, and the demo built with the platform's compiler,
  drawing planes through the toolkit's D3D11 renderer. Checked by the first picture: ten
  minutes from an established host. *Built 2026-09-27.* Decided at its interview: the host
  slots reserve their range and commit as a picture reaches, since that system charges a
  zeroed reserve whole ([10 §4](10-client.md)); the library's gates run on Windows through
  the platform's compiler, found in its installation, and the client half gets its own
  deliberate panic, minor 18, which moves W1.4's to 19; the demo keeps a makefile per make;
  the toolkit's shader compiler comes from the path. Until the hardware backends exist the
  pair is the table's one slot ([10 §5.2](10-client.md)). On Windows the workspace's 792
  tests pass; on Linux nothing moved. Ten minutes of each codec from an established host
  over the internet at 2560x1440: H.264 decoded in 4.4 ms at the median and HEVC in 7.0,
  arrival to present 6.6 and 9.4 ms. *Found at the gate*: the demo called the library
  through a destroyed handle while its window was destroyed, which the window's own focus
  change does synchronously there (fixed: the window goes first); and three HEVC runs lost
  the path's upward direction mid-session, past this machine's network card, as an
  established client over the same path did too.
- [x] **W1.3 D3D11 planes**: the backend fed from the readers' jobs, read back to planes.
  Checked by every clip decoding bit for bit on each of the three GPUs. *Built 2026-09-27,
  closed 2026-09-28*: the Intel card was out of the machine, so the step was checked on the
  NVIDIA and AMD GPUs first and on the Intel card once refitted. Decided at its interview:
  the system's own declarations, generated from its headers and committed with their layout
  checks, the libraries loaded at run time; the backend wired into the client for planes
  (the open kind on Windows, first in the automatic order), which needs a GPU named by its
  identity, so that moved here from W1.4; a picture read back to planes is waited for in its
  read-back, the never-waits rule being the handle kind's; and, after the live runs, a
  decoder nobody placed settles on the high-performance GPU. As built: the slices go in the
  short form and the scaling lists in the coded order; a virtual display's adapter, which
  enumerates under its GPU's name, is not offered. Every committed clip decodes bit for bit
  on all three GPUs, full chroma on the NVIDIA and the Intel card and refused as fatal on
  the AMD. Ten minutes of each codec on each GPU from an established host at 2560x1440, and
  ten-bit and full chroma with the full range from a second one ([10 §5.2](10-client.md)).
  *Found at the gate*: the AMD's video engine runs at a low clock under one decode that
  leaves it idle between pictures, twice as slow as when it is kept busy -- recorded, not
  pursued there (*taken up by W1.6*).
- [x] **W1.4 the handle**: the plane split, reading the decoder's output directly where a
  driver lets a shader read it and copying it first where one does not; the shared textures;
  the library's fence and the newest finished picture; the GPU on every frame; per-slot
  backing; `lowlat_client_set_frame_kind`. Minor 19 (*corrected at W1.2*, which took 18).
  Checked by handles on all three GPUs and by each mid-session change. (*Corrected at
  W1.3*: a GPU named by its identity moved to W1.3, which needed it for its table.) *Built
  and closed 2026-09-28.* Decided at its interview: the handle kind needs Windows 10 1703,
  asked of the device; the example client's device made on the pictures' GPU, and made again
  when they move, moved here from W1.9; a lost device is found again by the library; the kind
  is a preference at `lowlat_client_set_decoder`; Linux's vendor decoder takes the same
  calls. As built: every GPU here lets a shader read the decoder's output, so the copy is the
  fallback, checked by forcing it; a move between GPUs waits for nothing, a picture of a
  backing the session has left never handed out; eight-bit full chroma read back is unpacked
  on the device. Every committed clip decodes bit for bit through the textures on all three
  GPUs by both routes, and no picture is handed out unfinished, on the integrated GPU beside a
  full engine load too. Ten minutes of each codec by handle on each GPU from an established
  host, and ten-bit and full chroma with the full range by handle on the NVIDIA and the Intel
  card; mid-session, planes to handles and back with no keyframe, a GPU move, and the device
  lost by restarting each GPU's driver, streaming again within one to three seconds. *Found
  live*: a removed device can go on accepting decodes and refuse only the hand-over, and a
  restart of one GPU can remove a renderer's device on another; both fixed
  ([10 §4.2](10-client.md)). *Measured*: where the renderer is on the GPU that drives the
  display, handles are the faster route, 2.2 to 2.7 ms from arrival to the screen against
  3.5 to 3.6 by planes in one session; a renderer moved to another GPU to open its textures
  pays for every frame's crossing to the display, 9.7 ms against 5.4 for planes drawn on the
  display's GPU on the Intel card -- the placement is W1.9's.
- [x] **W1.5 NVDEC** (*moved ahead of AMF 2026-09-28*): planes, then handles through the
  vendor's interop with D3D11, the copy's completion signalled on the library's fence rather
  than waited for. Checked by the clips and ten minutes. *Built 2026-09-29.* Decided at its
  interview: NVDEC's place on an NVIDIA GPU by measuring it against the system's interface in
  one session, both codecs and both kinds -- first, since it proved faster in all four; a
  device lost under it, which the vendor's runtime cannot come back from in the process, ends
  the session by the library's own departure and `LOWLAT_OUTCOME_DECODER_FAILED`, and so now
  does every decoder that fails past recovery, on both platforms; the driver's newer decode
  into the backend's own surfaces where it has it (the 610 series on), on Linux too, the map
  below it, textures handed out either way; W1.4's search for a lost GPU amended (below). As
  built: the textures are the queue's own, made on a device of the library's own on the GPU
  and registered with the vendor's runtime once per slot, written by copies between a map and
  its release queued on the runtime's stream, the library's fence signalled on that device
  behind them; the surfaces padded, which a small picture needs; full chroma always mapped.
  Every committed clip decodes bit for bit both ways and through the textures read back on a
  second device, and fails with the fence signalled ahead of the copies or two planes swapped;
  the queue end to end, back to back and at 120 pictures a second; nothing allocated per unit;
  the Linux clips and slots under the vendor's runtime in a Linux environment on the same
  machine. Ten minutes of each codec by handle and by planes from an established host, ten-bit
  and full chroma at both depths by handle from a second, the kind switched, the decoders
  alternating in one session, and the device lost by restarting the GPU's driver, the session
  ending as the decoder's failure. A second client beside the first on the same machine, each
  on the vendor's decoder, ran clean ([10 §5.2](10-client.md)). *Found at the gate*: one of
  eleven restarts by handle ended the process rather than the session -- the only run whose
  loss surfaced at a texture copy before any decode -- and was not reproduced in nine runs
  after it; open. *Added at the gate, from W1.4's review*: a picture of textures is timed on
  its device, one in eight, rather than where the application's acquire saw it finished,
  which carried an application's own cadence into the figure ([10 §4.2](10-client.md)).
- [x] **W1.6 AMF on AMD** (*added 2026-09-28*): AMD's own decoder, loaded at run time, in
  its low-latency mode; planes, then handles; first in the automatic order on an AMD GPU.
  Checked by the clips it decodes and ten minutes of each codec on the AMD GPU. On that GPU
  the system's interface proved short on speed: the device's power policy stretches a decode
  paced in real time to fill its interval, 9 to 10 ms of the video engine a picture for HEVC
  at 1440p, and the vendor's decoder in its default mode fares the same; its low-latency mode
  takes 3.2 ms, decoding alone at 30 and at 60 pictures a second. The mode is the asking session's
  own -- a decoder beside it is not lifted -- so only this decoder can have it
  ([10 §5.2](10-client.md)). (*Noted at W1.4*: the system's interface by handle took 13 to 15 %
  of a core on the AMD GPU against 4 to 6 by planes, unexplained; looked at here.) *Built
  2026-09-30.* Decided at its interview: AMD's decoder first on an AMD GPU only where its
  runtime has the low-latency mode for every decoder it builds, and only if it measures faster
  than the system's interface in one session, both codecs and both kinds -- it did, in all
  four. As built: the runtime hands a picture out as soon as it has read the unit, its decode
  still running, and in decode order in that mode, so the library's readers read every unit
  too, for the stream's shape and for the order pictures leave in; the split and the read-back
  are queued behind the decode on the library's own device, which the runtime is given, so
  nothing waits for it, and no more than two split pictures are left unfinished when the next
  unit goes in, since the runtime left to itself queues some thirty and then spins; ten bits
  decode into the runtime's ten-bit layout, its eight-bit one decoding every such picture
  wrongly without an error; the table's row reports the device's size limits as the system's
  interface finds them, since the runtime initialises a decoder at sizes its engine cannot
  decode; a device lost under it is found again on the same GPU and the runtime made again
  there. Every committed 4:2:0 clip decodes bit for bit by planes and through the textures read
  back on a second device, in the order the system's interface lets the same clip out in, and
  fails with the fence signalled ahead of the split, the readers' order bypassed, or the bound
  lifted; full chroma is refused, the GPU having no such profile, and the declaration masked
  by it; nothing is allocated per unit; the queue end to end, back to back and at 120 pictures
  a second. Ten minutes of each codec by handle and by planes from an established host,
  ten-bit by both kinds and full chroma asked from a second, the kind switched, the decoders
  alternating in one session, and the device lost by restarting the GPU's driver, the session
  going on at the next keyframe ([10 §5.2](10-client.md)). *Open*: a stream larger than the
  engine decodes would fail at each keyframe, where the system's interface refuses it once.
  *Found at the gate*: the processor time noted at W1.4 is the example client's. By handle,
  drawing on a GPU other than the display's, its memory grows with every picture and its
  processor time with it, from about 3 % of a core to 30 in ten minutes, through either
  decoder; the library's route alone does not. Left to W1.9.
- [x] **W1.7 Intel VPL** (*added 2026-09-28*): Intel's own decoder through its current
  runtime, loaded at run time from where the display driver installs it, and through its
  older runtime (MFX) for the Intel parts the current one does not reach; planes, then
  handles by W1.4's split. Checked by the clips it decodes and ten minutes of each codec on
  the Intel card; the older runtime's check is owed until an Intel part that needs it is at
  hand. Unlike AMD's, it is built without the system's interface having proved short: every
  clip decoded bit for bit on the Intel card (W1.3). It is built so that every Intel part has
  its vendor's runtime, the older ones included, where the system's interface may lack a
  profile; its place in the automatic order on an Intel GPU is decided by measuring it against
  the system's interface there -- first only if it is faster, as AMD's is on the AMD GPU.
  *Built 2026-09-30.* Decided at its interview: **the older runtime hands out planes alone**,
  through its own memory output on the GPU named, H.264 and 4:2:0 HEVC at both depths -- no
  textures, which would need an allocator of the library's that no part here could check; its
  check stays owed. And a fault met handing a decoded picture out is judged as one met
  decoding, on every decoder ([10 §5](10-client.md)). As built: the runtime is found by the
  GPU itself, in the folder its own display device names among the devices present -- never
  the display class's keys in order, which keep entries for removed drivers; the current
  runtime is given the library's own device with its lock on, first of all, since a device
  without it, or one handed over once the session has touched the hardware, is refused and the
  runtime decodes on a device of its own; it makes its decode calls on that device's context
  inside its decode call, so the split and the read-back queued after are ordered behind the
  decode and nothing waits; asked for decode order, it hands each unit's picture out of that
  unit's own call, the library's readers restoring the stream's order; a unit it will not take
  while its own completion lags the device is offered again a millisecond later; parameter
  sets sent in a unit of their own are kept for the next build; one runtime thread is asked for
  where it would start one a core. Every committed clip decodes bit for bit by planes and
  through the textures read back on a second device, in the order the system's interface lets
  the same clip out in, full chroma at both depths included, and fails with the fence
  signalled ahead of the split or the kept sets ignored; every 4:2:0 clip through the older
  runtime's calls, which the current runtime answers here; nothing is allocated per unit; the
  queue end to end, back to back and at 120 pictures a second. **It measured no faster than the
  system's interface on the Intel card**, within 0.1 ms in one session per codec and kind, so
  the system's interface stays first there and Intel's second ([10 §5.2](10-client.md)). Ten
  minutes of each codec by handle and by planes from an established host, ten-bit and full
  chroma at both depths by both kinds from a second, the kind switched, and the device lost by
  restarting the GPU's driver under each kind, the session going on after about a second.
  (*Checked 2026-10-01* on a laptop whose integrated GPU only the older runtime reaches, a
  2015 part on its last driver: the runtime found through its own registry list, H.264 alone
  without the plugin HEVC needs there, every H.264 clip bit for bit by planes in the readers'
  order, five minutes from an established host -- no slower than the system's interface on
  the same GPU, 15.5 ms from arrival to acquired against 15.8 at 1440p -- and the GPU's
  driver restarted mid-session, the session going on.)
- [x] **W1.8 the system's decoder**, software only. Checked by the clips it decodes. *Built
  2026-10-01.* Decided at its interview: a decoder kind of its own,
  `LOWLAT_DECODER_SYSTEM`, with its own slot, the table's last, minor 20 -- the software kind
  stays the codec library's, whose device string is its directory; and HEVC at ten bits too,
  the extension decoding it into the ten-bit planar layout bit for bit. As built: the
  framework is loaded from the system's directory alone and started once for the process,
  never shut down, with the process's multithreaded apartment kept alive, so any thread can
  drive a decoder and none of the application's is put into an apartment; H.264 is the
  system's own decoder, HEVC the extension where installed and licensed, made only through
  the framework's enumeration; no device manager is given, so both decode on the processor.
  Every sequence parameter set is read before the decoder sees its unit, and a stream the
  decoder would hang on or write wrongly is refused as fatal: H.264 other than eight-bit 4:2:0
  or cropped from the left or top, HEVC other than 4:2:0 at eight or ten bits; the depth picks
  the output's layout, since a ten-bit stream into an eight-bit output decodes wrongly without
  an error. Every unit ends with an access unit delimiter, without which the HEVC decoder hands
  each picture out a unit late, and a picture of several slices not at all; one input and one
  output buffer are reused, the output's length cleared before each call; rows are copied by
  the decoder's own stride, the chroma after its coded height. HEVC is given two worker
  threads, faster than the default of one a processor, and H.264 half the processors.
  Every committed eight-bit 4:2:0 clip and every ten-bit HEVC clip decodes bit for bit in the
  stream's order; a size change mid-stream is followed; each unit's picture comes out of its own
  call; the refused streams are refused at once; nothing is allocated per unit; each guard
  shown failing. Ten minutes of each codec from an established host at 2560x1440: a picture
  acquired 5.1 ms after its arrival for H.264 and 6.0 for HEVC, the whole process at 30 % of a
  core; in one session with every decoder alternating, against the LGPL pair 0.4 ms slower on
  H.264 and 1.1 ms faster on HEVC, so it stays after the pair; ten-bit with the full range from
  a second host in 3.6 ms. On a 2015 dual-core laptop without the HEVC extension, H.264 alone,
  every H.264 clip bit for bit and a 1440p stream kept up with at 21 ms a picture, HEVC asked
  for masked to H.264 ([10 §5.2](10-client.md)).
- [ ] **W1.9 the demo**: the GPU choice's menu, the feature reports, the Sony pads, the check
  of the display's GPU, and from it the renderer's placement: handles when the decoding GPU
  drives the display, planes drawn on the display's GPU when it does not. Checked by both Sony
  pads against an established host. (*Corrected at W1.4*: the toolkit's device made on the
  pictures' GPU, and made again when they move, moved to W1.4, whose handles on every GPU
  needed it; the placement added, W1.4 having measured a frame's crossing to the display at
  4 ms on the Intel card.) (*Added at W1.6*: by handle, drawing on a GPU other than the
  display's, the demo's memory grows with every picture and its processor time with it -- the
  library's route alone does not -- so its renderer is fixed here and checked by ten minutes
  flat. The crossing measured again in separate sessions came to 1 to 2 ms over planes drawn
  on the display's GPU, which took longer than at W1.3; the placement is measured within one
  session here.) (*2026-10-13, the pads and the sound*: on SDL 3, as W1's amended decision
  says, the Sony pads sent as reports on Windows too, their feature reports read again for
  every session. Shown against an established host with both pads on USB, as states and as
  reports; through Windows' HID stack every report comes at its own length -- the input at
  64 bytes, the feature reports at 37 and 49 for a DualShock 4 and 41 and 64 for a DualSense --
  and a 100 ms stall in the loop loses none. Checked by both pads against the established host
  in each of its pad modes: in its DualSense mode the DualSense whole, both ways -- the sixteen
  buttons, the touchpad, the motion sensors, rumble and the lights, 295 output reports put on
  the pad and none refused; in its DualShock mode the DualShock 4 but for the motion sensors,
  which that mode never reads, its 350 rumble messages each written as a motor-only report;
  and both as the sixteen-button pad with rumble.)
- [ ] **W1.10 packaging**: the zip from the build workflow; CI decodes the clips through a
  downloaded LGPL pair, since its runner has no GPU, and builds the demo. Checked by the
  artifact, built and unpacked.
- [x] **W1.11 the static library** (*2026-10-09*, ahead of W1.9, for an application that
  links the client library into its own): `lowlat.lib` from the release build, asked for by
  name, its system libraries named inside it ([06 §3b](06-api.md)). Checked by an ABI gate
  that links a C program against it and nothing else under the static release and debug
  runtimes, warnings as errors, and has its deliberate panic contained; shown failing with a
  system library left out and with the debug runtime's set-aside removed.

**Gate:** Gate C ([impl-plan-client.md](impl-plan-client.md)) on this machine, against two
established hosts:

- ten minutes each through D3D11 on each of the three GPUs, NVDEC, AMF on the AMD GPU, VPL
  on the Intel card, the pair and the system's decoder, each on its default kind and with
  both codecs, against a host that sends neither ten-bit nor full chroma;
- ten-bit and full chroma, both kinds, on every decoder that decodes them, against a host
  that sends both, and the full range;
- once each, mid-session: a GPU change from the demo's menu, planes to handles and back, a
  decoder change, and the device lost by restarting it;
- recorded rather than passed or failed: how long a released slot stays unread on the
  integrated GPU, both GPU placements' presentation mode and time to the screen, the
  toolkit's open of a texture on every draw, whether each driver lets a shader read the
  decoder's output, and full chroma on each GPU;
- both CI jobs, and one Gate C run on Linux before the phase closes, since the slots and the
  queue are shared with it.

## Phase W2 - The host

**Planned 2026-10-14, interview of the same day.** The host on a Windows machine with a
monitor: desktop duplication, a D3D11 conversion, NVENC on the capture's device, input through
the system's batch input call and a virtual pad, loopback sound, and a service. A host with no
monitor gets a display of its own in W3, measured against this one. The decisions are recorded
once, here.

### Decisions taken at the interview

- **One process, in the console session.** The whole host -- capture, the conversion and the
  encoder on one device, input, sound and the network -- is one process of the system account
  in the active console session; the service only starts it there and starts it again when the
  console's session changes. Desktop duplication and the batch input call both need the
  session's input desktop, and a process of the system account follows it onto the lock screen
  and an elevation prompt, which switch desktops rather than sessions; the frames, the encoder
  and congestion control never cross a process, as on Linux; and W3's channel to its driver
  needs the system account anyway. A logoff or a user switch restarts the host and its guests
  reconnect once it is back; the first logon after boot stays in the boot's session. An
  application that embeds the host half runs it the same way; the library starts no process.
  This replaces the split [07 §10](07-platforms.md) implied.
- **No application supplies frames** (*settled before the interview*). Desktop duplication is
  this phase's source, for a physical monitor and for a display another driver provides; W3's
  display is read by the same display module, so nothing crosses the library's boundary.
  *Corrected 2026-10-15, at W3's review:* as built, a display another driver provides is not
  duplicated. Its output sits on an adapter of its own, which renders nothing and is refused as
  an encoder's device, and the GPU that renders it is another adapter; so the stream refuses
  every such output, another vendor's virtual display and the host's own alike. W3.2 delivers
  it. *Corrected again 2026-10-15, at W3.0's build:* the system lists a virtual display's
  output under the GPU that renders it, as that GPU's own -- seen with the host's own display --
  not on an adapter of its own, so the premise above does not hold for it. Whether the stream
  as built takes such an output is W3.2's first check.
  *Answered 2026-10-16, at W3.2's review:* it does. The host's own display and an established
  host's own virtual display were each streamed by the stream as built: both outputs listed
  under the GPU that renders them, the device made there, its encoder opened, and 120 pictures
  a second delivered.
- **NVENC alone, on the capture's D3D11 device.** The encoder keeps its register-once submit and
  poll and binds its device through D3D11 rather than CUDA. AMD's and Intel's encoders are
  deferred: on the development machine only the NVIDIA card drives a display, so either would
  pay a copy of every frame between GPUs and measure the copy rather than the encoder. They are
  measured on W3's display, which renders on any GPU, or on outputs given a display. No Vulkan
  encoder and no system encoder on Windows: the vendor runtimes take D3D11 textures directly,
  and the Vulkan encoder's tails were the longest on Linux.
- **Desktop duplication, converted straight from the acquired image.** The conversion reads the
  duplicated image into the encoder's registered input and the frame is released as soon as the
  conversion is queued: no copy, so two passes after composition, the system's duplication copy
  and the conversion. A still desktop's refresh frames re-encode the last converted input. The
  duplication asks for eight-bit BGRA alone, and on a desktop in a wider format the system
  converts. A duplication lost to a desktop switch is made again on the current input desktop,
  the secure one included. The pointer's shape, hotspot and visibility are taken as reported,
  and the shared pointer type carries them where Linux goes on learning the hotspot. Rotation
  is declared in the video header, as on Linux. The device is made on the output's adapter and
  the encoder on it.
  *Amended 2026-10-14, at W2.2's review:* the duplication answers only when the image or the
  pointer changed, so a frame that carries only the pointer, and a wait that runs past the
  frame's deadline, are each a present of an unchanged picture -- a still desktop keeps the
  frame rate's refine frames and the pointer's cadence -- and whether the image changed is the
  duplication's own report, not a summary written by the conversion. The conversion's targets
  are the encoder's inputs, bound for unordered access and registered once (the encoder takes
  them, and one submitted again while it is in flight, measured). Full chroma is the packed
  layout: eight bits as AYUV, ten bits as Y410 once a decoder's round trip agrees, refused by
  the census until then (no planar layout is taken on this device). The display is kept awake
  while a capture exists. The pointer's bits each have their own source
  ([05 §8.1](05-host.md)): `hidden` the duplication's own visibility of the pointer, false
  while the system draws it into the picture; `relative` the system's show state, or a pointer
  whose picture is one value throughout; `suppressed` the show state's touch flag. The
  position is where the pointer points, from the show state; the shape and hotspot are the
  duplication's, and the pixels that invert the screen beneath them, which a picture cannot
  carry, are drawn dark grey (a masked colour pointer) or black with a light halo (a monochrome
  one); and a message goes out once the whole state has held for more than 18 ms.
  *Amended 2026-10-14, at the build:* a frame that carries only the pointer ends no wait. The
  pointer moves a thousand times a second, so ending the wait on it would tick at once on the
  deadline whenever the pointer moved, and a composition that followed a millisecond later
  would wait a whole frame interval; the frame's pointer is taken and the wait goes on. A wait
  past the deadline that sees no composition for a refresh is the present of an unchanged
  picture, so a still desktop ticks once a refresh, as a display's vblank would. Ten-bit full
  chroma is granted: its round trip through the library's decoder agreed.
  *Amended 2026-10-16, at W3.2's review:* **the rate a display presents at is the rate its
  desktop is composed at**, which on Windows 10 is one clock for every output -- the primary
  display's -- whatever an output's mode says. A virtual display at 144 or 240 was handed 120
  compositions a second under a 120 Hz primary, and the stream, which said it paced at 144,
  sent 120 while sizing each frame's budget for 144 and telling its guest 144. So the pace is
  the lowest of the ceiling, the output's mode and the clock the compositor reports, settled
  again at every change of the captured output's mode -- its refresh included, which re-made
  the duplication and settled nothing before -- and the refresh the deadlines take their phase
  from is that clock too. A ceiling below it is honoured, as `fps` always was: with the mode's
  period under the deadlines, a ceiling of 60 or 90 on that display took each picture 4.6 to
  6.2 ms after its composition at the median, against under a millisecond on a panel whose
  mode is its clock.
  *Amended 2026-10-16, at W3.4's review:* **a masked colour pointer made only of black, white,
  clear and inverting pixels is drawn as the monochrome one it is**, its inverting pixels black
  in a light halo: the system hands a display driver every monochrome pointer in that form, so
  without it the host's own display read from its driver would draw the text cursor dark grey
  where the duplication draws it with a halo. The duplication, which reports such a pointer
  monochrome, is unchanged.
  *Amended 2026-10-16, at W3.5's review:* **keeping the display awake holds it on but does not
  wake it.** A display that slept on its timeout before a guest arrived stays asleep under the
  stream's request, measured, and gives no pictures -- the system takes a virtual display's
  away while it sleeps -- until input wakes it: one injected pointer move of no distance woke it
  in 170 ms. So **a setting wakes the display as a stream starts, with that move, off by
  default**: a connection lights nothing at the desk by itself, and a guest's first input wakes
  the display otherwise.
- **The host is per-monitor DPI aware** (*added 2026-10-14, at W2.2's review*). On a thread
  that is not, the duplication call the host uses is refused outright, and every coordinate
  the system hands back is scaled at a display scale other than 100 percent (measured, at 125
  and 150). `lowlatd` declares the mode in its manifest, and the library sets its own capture
  and input threads to it, so an application embedding the host half is not refused for its
  packaging ([05 §7](05-host.md)).
- **An output is named `DISPLAY1`**, the system's own name for it, the `\\.\` prefix optional.
  It is unique on the machine, so it needs no adapter half -- an adapter's identity changes
  across reboots -- and its number can move as monitors come and go, as a Linux device's can.
  Asked for nothing, the host takes the output at the desktop's corner, which on Windows is the
  primary display; the adapter, and the encoder with it, follows the output; a switch is the
  rebuild a resolution change is.
  *Amended 2026-10-14, at W2.2's build:* a restart of the GPU's driver renumbers every output
  while the monitors stay where they are -- the development machine's two came back as
  `DISPLAY27` and `DISPLAY28` -- so a stream asked for a name that has gone follows the monitor
  it named, by the monitor's own device path, to its new name, and says so once.
- **The conversion's shaders** are HLSL compiled offline and committed as generated code, as
  the client's plane split is: no build machine needs the compiler and nothing compiles one at
  run time. The formats follow the Linux host's where NVENC's D3D11 input takes them; full
  chroma's layout there is checked first.
- **Input.** A peer's usage code maps to the scancode Windows produces for that key on a USB
  keyboard -- the translation is the platform's, as the kernel's own table is on Linux, and the
  expander that turns messages into events is shared and unchanged -- and goes out through the
  batch input call by scancode. Relative motion stays relative; an absolute position is
  normalised over the virtual desktop from the captured output's rectangle, the captured
  picture's pixels being the coordinate space as on Linux; wheels move in their standard steps.
  A pad is an Xbox 360 device on the ViGEm bus, one per peer pad, the bus addressed through its
  own control codes with declarations generated from its public header; rumble returns as the
  guest's rumble message; with no bus there are no pads, said once in the log. A pad sent as
  reports waits for a HID device of the host's own, which is deferred.
  *Amended 2026-10-15, at W2.3's review:* the translation is generated from the system's own
  translation of usage codes into scancodes, which any program can call, as the Linux table is
  copied from the kernel's. On top of it, the codes peers send that no keyboard does -- the
  context-menu key's second code, mute and volume on the keyboard page, the six media keys
  above the usage range -- land where an established host puts them, and the keys a scancode
  cannot carry go as the system's keyboard driver reports them: Pause as its virtual key,
  Ctrl+Pause as Break, Alt+PrintScreen as SysRq. **The power key is not mapped**, so a guest
  cannot put the host to sleep or turn it off. The host's own lock state is read, so a lock is
  tapped when a peer's reported state changes and the host's differs. A wheel's units go out as
  the peer counted them. Absolute motion turns into deltas while an application holds the
  pointer (`relative`, [05 §8.1](05-host.md)), not while the pointer is hidden. **A pad sent as
  reports is played from its whole states as the Xbox pad** unless the application takes the
  reports ([05 §7.2](05-host.md)), as an established host in its Xbox mode does. **Opcode 5's
  vertical axes point down on the wire** ([01 §11.1](01-protocol.md)), opposite to the whole
  state's: the host reads them so on both platforms, and the client's per-axis call converts at
  the wire so its interface keeps the state's direction. The attention chord is the system's
  own call for it, made by the service, never typed keys ([05 §7.3](05-host.md)).
- **Sound.** The loopback of the default render endpoint, event-driven, 48 kHz stereo asked and
  the system's engine converting, 20 ms a packet. **A loopback delivers nothing while nothing
  plays** -- measured on the development machine: no packet in three seconds, where a silent
  render stream open on the same endpoint kept it delivering every 10 ms -- so a silent render
  stream stays open on the captured endpoint while the loopback runs, and the source stays the
  clock ([05 §9.1](05-host.md)). The default is followed as it changes and a device that went
  away is taken again a couple of seconds later ([05 §9.3](05-host.md)); a guest's microphone
  stays the application's, as on Linux.
  *Amended 2026-10-15, at W2.4's review:* the silent stream is started once with a buffer of
  silence and never fed -- that alone kept the loopback delivering, measured -- and the
  loopback's packets then come flagged as silent, so a flagged packet counts as silence of its
  length rather than being skipped, or the clock it buys is lost again. **The host asks for
  floating point and rounds to sixteen bits itself.** A program playing digital silence reaches
  the loopback as a floor far below hearing, which the system's own conversion to sixteen bits
  dithers into the lowest bit -- a quarter of the samples, measured -- so silence, which a host
  recognises only as exact zeros ([05 §9.1](05-host.md)), would never read as silent; and the
  system limits the mix below full scale before the loopback, so the rounding never clips real
  sound (measured with a mix driven past it). The default followed is the output the system's
  sound settings name as the default device, through the system's own device notifications; a
  named output that goes away is replaced by the default, as the Linux sound server moves a
  stream; and a program that takes the output in exclusive mode wins -- both of the host's
  streams are lost the moment it opens, measured -- which is a device that went away, taken
  again once it lets go ([05 §9.3](05-host.md)). **The local mute is performed only where the
  output mutes in hardware**: the loopback hears what the system's engine applies and nothing
  the device applies, per control, and the system reports which -- measured both ways on the
  development machine ([05 §9.4](05-host.md)). **Leaving one program out of the sound is
  deferred** to W2.6: the system can capture everything but one program, and that is a feature
  of its own ([05 §9.5](05-host.md)).
- **The service.** Two roles of the daemon: the service, under the service manager as the
  system account in session 0, which only starts and watches the host; and the host above. No
  session role, since the host is in the session, and no tray in W2. The service starts the
  host from its own token moved to the console session, in a job that ends the host with the
  service; starts it again two seconds after a failed exit; and on a change of the console's
  session asks the old host to end, then terminates it after a timeout. Its configuration is an
  environment file in the program-data directory that only the system account and
  administrators can read, since it holds the signaling secret, and the host's log is a file
  beside it. `lowlatd install` and `lowlatd uninstall`, from an elevated shell, register and
  remove the service; there is no installer package, and the ViGEm bus is a prerequisite.
- **The floor is Windows 10 1703**: the duplication call the host uses and the event-driven
  loopback both need it, and the client set it already.

### Steps

- [x] **W2.0 build**: the host's crates and the library's host half build for Windows, each
  platform module in its place and the Linux ones unchanged; the hermetic host session passes
  there. Checked by both CI jobs and the Linux bar. The library on Windows stops being the
  client half alone ([06 §3b](06-api.md)). *Built 2026-10-14.* There is no hermetic host
  session (*corrected at the build*): the hermetic session is the client's, on Windows since
  W1.0, and what passes here is the host's own tests, on the synthetic source and a fake
  encoder. Each Windows module refuses until its step rather than pretending: the display
  lists no outputs and finds nothing lit (W2.2), a stream finds no encoder (W2.1), and a guest
  is admitted with its input off (W2.3). The daemon's session side -- the helper and the tray,
  which only Linux has -- is a platform module as well, so the service builds everywhere and
  refuses the two roles on Windows with a line. The Windows job's lints and tests pass here,
  1214 tests where there were 977, the library's gates among them with both halves inside it;
  on Linux the bar gives the results it gave and the list of tests is the one it was. Two port
  tests raced on Windows, which hands out ephemeral ports nearly in order, and now take theirs
  below both systems' ephemeral ranges.
- [x] **W2.1 NVENC on D3D11**: the encoder's device and inputs bound through D3D11, fed by the
  synthetic source. Checked by the library's own decoder taking the encoder's output in both
  codecs, and an established client streaming the synthetic picture over the LAN. *Built
  2026-10-14.* The encoder is one on both platforms and what it is bound to is a platform
  module. On Windows the session is opened on a D3D11 device, its inputs are NV12 and P010
  textures registered once, and it runs asynchronously, setting an event of its own as each
  picture finishes, so the lock behind a set event waits for nothing (18 to 24 us); a frame from
  ordinary memory goes in through a staging texture copied whole. Until the display exists the
  stream opens a device of its own on the first of the vendor's adapters that renders, runs the
  synthetic source alone, refuses a display as nothing to capture and refuses full chroma, whose
  layout is the conversion's. The library's D3D11 decoder takes the pictures back at 73 dB or
  better against the source in H.264, HEVC and ten-bit HEVC; two established clients, macOS
  and Android, streamed it over the LAN in both codecs, switching between them with both
  seated. **Found live and fixed**: the encode took 7.8 ms at the median where the established
  host takes 3 on this machine. Three causes, each measured: the preset every setting started
  from turned on a second
  pass, its effort was the middle of the device's range, and the loop polled for a finished
  picture on sleeps of about two milliseconds. The quality setting now picks the effort as
  [05 §4.1](05-host.md) says -- the fastest preset in one pass, or for the highest setting the
  most thorough with two -- on both platforms; the loop sleeps on the encoder's event where it
  has one; and `lowlatd` raises its process class and the timer resolution on Windows. Live, the
  encode is now 1.3 to 3.0 ms at the median and 6 to 10 at the p99; what the tail has left is
  the card lowering its clocks under a load this light, to be compared with the established host
  at its own load in W2.2.
- [x] **W2.2 desktop duplication and the conversion**: the capture, the conversion and its
  shaders, the pointer, rotation, and naming and switching outputs. Run as the system account in
  the console session by a process launcher until W2.5. Checked by the desktop streaming to an
  established client through the lock screen and an elevation prompt.
  *Reviewed 2026-10-14, before its build* (the decisions above): also the per-monitor DPI mode
  (`lowlatd`'s manifest, the library's threads), the display kept awake, a still desktop's
  presents, and full chroma's packed layouts; and checked as well by a switch between the two
  displays the development machine now has on the NVIDIA card, the second at 125 percent, with
  the pointer's three bits seen on each. The encode at this machine's load was measured
  against the encoder's own conversion first ([05 §3](05-host.md)): the same cost here, and
  HEVC at 2560x1440 and 120 a second reaches the frame interval at its p99 (~8.5 ms) while the
  card sits in a low power state.
  *Built and checked live 2026-10-14.* The duplication and the conversion are capture's,
  over the device seam the encoder uses; the display is the host's, on the output's own
  adapter, the encoder built on that device at the output's size and the conversion's four
  targets registered with it once. The conversion is one compute shader per layout, compiled
  offline: on the development card it lands on the colour reference sample for sample in all
  four (NV12, P010, AYUV, Y410), and a desktop-like picture converted, encoded and decoded by
  the library's own decoder comes back at 57 dB of chroma or better in each, the packed
  full-chroma words carrying colour that changes every pixel. On the real desktop the loop's
  own sequence runs at the display's pace in all four layouts: the first present in 6 ms, no
  wait past the deadline longer than 10.3 ms at 120 a second, a composition handed over in
  0.05 ms at the median and its conversion queued by 0.26. A duplication lost to a switch of
  desktop is made again on the input desktop, the encoder kept; one out of reach holds the last
  picture and is asked for again every 50 ms. The pointer's message is settled by the display.
  `lowlatd` declares per-monitor awareness in its manifest and lists both displays at their
  physical sizes. **Live**, as the system account in the console session, an established
  client on a remote Windows machine over the internet streamed the desktop through the lock
  screen and the secure attention screen -- which stands in for an elevation prompt, since the
  development machine raises none -- through a switch from one display to the other and back,
  and through H.264 and HEVC, the encode about 3 ms at the median, no packet asked for again.
  **Recorded at 120 a second**: composition to the conversion queued 0.35 to 1.5 ms at the
  median, 4.6 to 7.2 at the p95 and 7.5 to 8.6 at the p99, and the conversion 0.07 ms on the
  device; the tail is the frame clock, not the duplication -- a composition that lands while
  the loop sleeps to its deadline waits for it, up to a refresh.
  *Checked further the same day:* a restart of the GPU's driver under the stream -- the device
  lost -- is a rebuild on the device as it comes back, the monitor followed through the
  renumbering and the guest kept, about four seconds in all, most of it the restart; an
  encoder that fails because its device went away is rebuilt, never read as refusing the
  guests' preferences. A program taking the display in exclusive fullscreen at another mode,
  in the legacy model and in the flip model, is followed both ways: the stream rebuilds at its
  size and rate and back when it leaves.
  *Followed up 2026-10-15:* **the frame clock's tail is gone.** The deadline takes its phase
  from the compositions ([05 §2](05-host.md)) and a tick that found nothing waits again at once;
  live at 120 a second, composition to handed over is p95 0.11-0.45 and p99 0.19-0.60 ms against
  6.5-8.1 and 7.8-8.9 before, across sporadic updates and 60, 90 and 120 Hz content, the rate
  exact and the processor time on a still desktop unchanged within what the system resolves.
  A picture in flight is now polled for while the loop waits for the display, which the new
  waiting needed: without it the encode a peer was shown on a still desktop read 9-10 ms.
- [x] **W2.3 input**: the usage translation, both pointers, the wheels, and the ViGEm pad with
  rumble. Checked by every key class on the host against what a USB keyboard gives, and a pad
  with rumble, from an established client.
  *Reviewed 2026-10-15, before its build* (the decisions above): also the lock keys from the
  host's state, report pads played as the Xbox pad, opcode 5's direction on both platforms with
  the client's per-axis call, and the attention chord refused until the service makes it.
  Checked as well by a keyboard hook and a raw-input log on the host against the physical
  keyboard -- Pause, Break, SysRq, both Shifts, the keypad either side of NumLock, media and
  volume keys, the context-menu key by both codes -- by absolute positions swept across both
  displays, the second at 125 percent, against where the pointer lands, and by typing on the
  lock screen. **The attention chord is the service's** (W2.5): the system takes its call for
  the secure screen from a service alone, so until the service exists the host refuses the
  chord and logs it.
  *Built and checked live 2026-10-15.* The translation is the system's, generated and
  committed: 135 keys, with the two keys that do not answer as one code recorded in the form the
  system gives them under a modifier -- PrintScreen without its fake Shift, Pause as Break --
  and the generator checking the system's answer for every key under every modifier; with the
  peers' ten extra codes and the power, sleep and wake keys refused, 141 codes map. Pause goes
  as its virtual key, or as Break while the guest's own keyboard holds Ctrl, and PrintScreen as
  SysRq while it holds Alt, each released as it went down. The two pointers are one mouse to
  the batch input call: positions over the virtual desktop, deltas, the wheels in the units the
  peer counted, the side buttons as the extra buttons. A guest's thread is made per-monitor
  aware and attached to the input desktop with its devices, and attached again when the call
  refuses -- a thread on another desktop is answered with access denied and nothing inserted,
  measured -- after which the input goes again. The locks are read from the host's own state on
  the guest's thread, whose view of a lock follows one changed anywhere on the desk (measured
  on a thread with no window). A pad is the bus's, through its own requests from declarations
  generated from its public header: plugged in by its first message, held until the bus
  answers that it is ready, then reported whole; its rumble is a request kept outstanding for
  it and polled by the guest's loop. Checked on the development machine through the system's
  own pad interface, as a game reads it: a whole state read back exactly, up still up, the bus
  ready 3 ms after the pad was plugged in, a rumble a game raised back as the guest's rumble
  message, and the pad gone on its unplug message and with its guest's devices. The per-axis
  message's vertical axes are read pointing down on both platforms, and the client's per-axis
  call turns them over at the wire. The chord is refused on Windows with a log line.
  **Found before the live run and fixed: every absolute position landed a pixel up and to the
  left**, and an output's first column on its neighbour's last -- a sweep through a guest's own
  devices over both displays, the second at 125 percent, read back where the system put the
  pointer, three times a point. The system truncates `value * desktop / 65536` back to a pixel
  and the value named a pixel's near edge; it now names the pixel's centre
  ([05 §7](05-host.md)), and the sweep lands every position on its pixel on both displays. The
  mapping is shared, so Linux moved with it; its measurement there is owed
  ([impl-plan.md](impl-plan.md) Phase 9). **Found live and fixed: Caps Lock toggled the host
  once every two presses from a macOS client**, whose platform reports the lock as a modifier --
  the key down as the lock goes on and up as it goes off -- and an established host does the
  same; a lock key is now the peer's lock state where the host reads its own, and the peer's
  first message counts only for a lock it has on, since a keyboard with no such key reports it
  off. **Live**, as the system account in the console session, from established clients on a
  MacBook and a Windows laptop: every key class -- letters, digits, the F-row, navigation, the
  keypad either side of Num Lock, both sides' modifiers, the Windows and context-menu keys,
  Pause, PrintScreen, the media and volume keys, the locks -- landed as the scancode a keyboard
  on the machine gives, read by a low-level hook and raw input; the buttons, both wheels in the
  units the peer counted, motion either way; a pad's buttons and sticks and its rumble back,
  the bus ready 5 to 19 ms after the pad was plugged in; typing on the lock screen, the guest's
  input following the desktop there; and the attention chord refused and logged. Break and
  SysRq never reached the host from either client, so their forms are the generator's check
  alone.
- [x] **W2.4 sound**: the loopback and its silent stream. Checked by sound through a quiet
  spell with no gap at the guest.
  *Reviewed 2026-10-15, before its build* (the decisions above): also the silent stream
  started once, flagged packets as silence, the host's own rounding to sixteen bits, the
  default followed through the system's notifications, exclusive use taken again, and the local
  mute only where the output mutes in hardware. Checked as well on the development machine: a
  quiet spell delivering a frame every 20 ms, the same check failing with the silent stream
  left out; a program playing silence read back as exact silence, failing through the system's
  own sixteen-bit conversion; the default switched between the machine's two outputs
  mid-session; a program in exclusive mode mid-session, the sound back after it lets go; and
  the local mute with a guest seated, put back when the last one leaves.
  *Built and checked live 2026-10-15.* The capture is a platform module under the names
  Linux's has, and the reassembly, the settings and the handle an owner holds are written once;
  the system's sound interfaces are declared from the SDK's headers by a script and committed.
  One thread waits on the loopback's own event, the system's word of a new default, and a stop:
  the silent stream started once on the output, the loopback asked for floating point and
  rounded to sixteen bits, a flagged packet taken as zeros of its length. A lost output is
  opened again at once where it can be -- a named one that went away replaced by the default --
  and otherwise the capture ends and is asked for again; the local mute is the output's own,
  where it mutes in hardware; `lowlatd` refuses a sound server's socket on Windows with a line.
  Checked on the development machine through the library's capture, nothing playing: three
  seconds of quiet handed on 150 frames, and none with the silent stream left out or with
  flagged packets skipped; a program playing zeros read back as exact silence, and every frame
  of it loud through the system's own sixteen-bit conversion; a tone at -30 dBFS back at its
  level in each channel; a program taking the output in exclusive mode ended the capture, which
  opened again once it let go; the speakers silenced and put back; and the outputs listed by
  the identity a capture publishes. **Live**, as the system account in the console session at
  30 frames a second, from an established client on a Windows laptop: through quiet spells of
  twenty seconds with nothing playing on the host, a packet every 20 ms and none dropped, and
  a beep after each spell heard whole at the guest; the default switched between the machine's
  two outputs and back six times while sound played, each followed within one or two packets,
  the speakers' mute moved to the new output and put back on the old; and the speakers
  silenced while a guest was seated and put back as it left, three times. A program in
  exclusive mode was checked by the machine's own tests, not live.
- [ ] **W2.5 the service**: the two roles, install and uninstall, a change of console session.
  Checked by a logoff mid-session and the guest reconnecting.
  *Added 2026-10-15, at W2.3's review:* the attention chord. The host asks the service, which
  makes the system's call for the secure screen, since the system takes it from a service alone;
  `lowlatd install` lets services make it and `uninstall` puts back the setting it found.
  *Added 2026-10-16, at W3.5's review:* also a remote desktop connection to the machine, which
  moves the console to another session as a logoff does; and with each, the host's own display
  back with the new host -- the display side of both, which W3.5 checks as the host's end and a
  new host's start.
- [ ] **W2.6 a program left out of the sound** (*added 2026-10-15, at W2.4's review; not in the
  gate*): the system's capture of everything but one program's process tree, for a voice
  program whose sound would otherwise reach the guests it is talking to. Its setting through
  the configuration and the library, the system version it needs, its asynchronous activation
  and how it keeps a clock in a quiet room are its own review. Until then a person sends that
  program to another output in the system's per-application settings, or mutes it
  ([05 §9.5](05-host.md)).

**Gate:** the host on the development machine, with established clients on macOS and Android on
the same LAN:

- ten minutes each of H.264 and HEVC at 2560x1440 through NVENC, and ten-bit and full chroma
  where the encoder takes them;
- once each mid-session: the lock screen, an elevation prompt, a logoff and the reconnect after
  it, and a switch of display where a second one is attached;
- input: every key class against a USB keyboard's, both pointers, a pad with rumble;
- sound through a quiet spell with no gap;
- the wide-area leg the other way round: the host here, the library's demo and an established
  client on a remote Windows machine over the internet;
- recorded rather than passed: composition to encoder input at p50, p95 and p99, the baseline
  W3's direct frame is measured against;
- both CI jobs, and the Linux bar, since the shared pointer type and the usage translation
  change.

## Phase W3 - The host's virtual display

**Added 2026-10-14; planned 2026-10-15, interview of the same day.** A display driver of the
host's own, for a host with no monitor: an indirect display whose composed desktop the host
converts straight from the system's buffer, and a controller through which any application can
plug virtual monitors of its own ([00 §D16](00-overview.md)). Behind a virtual display, desktop
duplication costs two passes after composition -- the system's copy into its duplication image,
then the conversion -- where this costs one, the conversion itself. W2's desktop duplication is
what it is measured against, and an established host duplicates its own virtual display like
any other output, so the direct frame is the whole of what this display adds over one. The
decisions are recorded once, here; the design they add up to is [11](11-drivers.md).

### Decided when it was added

- **The host reads the system's own buffer.** The driver shares each of the swapchain's
  buffers once, read-only, as a handle the host takes from the driver's process. After each
  acquire it signals a fence on the device the swapchain was given, which orders the signal
  behind the system's write, and the host's conversion waits on that fence. The driver
  finishes the frame as soon as the signal is queued, then holds the buffer -- acquiring no
  other -- until the host's release fence passes. It reads the host's fence on the processor
  only and never waits for it on the device, so a host that dies cannot stall the display.
  *Amended 2026-10-16, at W3.3's review:* **the driver never waits for the host.** A swapchain
  hands the driver three buffers in turn, measured, and the system writes a buffer again only for
  the third composition after it, so a picture stays intact for about three refreshes after the
  driver acquires it -- 24 ms at 120 a second -- where the host converts within half a
  millisecond. The driver acquires, signals, publishes and finishes, as it would with nobody
  reading; the release fence and the hold are gone. The host converts the latest picture and,
  after queuing the conversion, checks that the driver has not acquired as many newer pictures as
  the swapchain has buffers less one; when it has, the buffer may have been written again, so the
  latest is converted instead, and counted. A still desktop's last picture stays the driver's
  until a newer one is acquired, so it is never written again while it is the latest.
- **The hold is bounded well inside a frame period**, since a held buffer holds back the
  compositor. Past the bound the driver releases the buffer anyway and counts it.
  *Replaced 2026-10-16, at W3.3's review:* nothing is held (above); the system's own cycle of
  buffers keeps a picture intact while the host reads it, six times longer than the bound would
  have.
- **The driver converts nothing.** Formats, depth, full chroma and scaling stay the host's,
  so changing them never re-signs the driver. As everywhere, the display is standard dynamic
  range only, so every buffer is eight-bit BGRA.
- **The frames never cross the library's boundary**: the host's display module reads the
  driver, as it reads a scanout buffer on Linux, and [06 §13](06-api.md) stands.

### Decisions taken at the interview

- **The monitor lives with the host.** The host plugs its display when a stream finds no lit
  output and the configuration allows it, or when the configuration names it, and keeps it for
  as long as the host runs: the driver unplugs a monitor when the handle that plugged it closes,
  so a host that ends, however it ends, leaves no display behind, and nothing has to keep one
  alive. Not one per session: a machine with no monitor then has a display for its logon screen
  and its launchers before anybody connects, a connection waits for no plug, and the desktop's
  layout does not change with every guest. Allowing it is off in the library's defaults, since
  it changes the machine, and on in `lowlatd`'s.
- **The driver is Rust, in this workspace.** A user-mode display driver built from a
  Windows-only crate here and shipped with `lowlatd`. The declarations of the user-mode driver
  framework and of the indirect display class are generated once from the driver kit's headers
  by a script, against an allowlist, and committed, as the other vendor interfaces are; the
  calls through each framework's function table are written by hand behind their availability
  checks. A callback never unwinds into the system's code: a panic ends the driver's host
  process, which the system starts again. **The channel's shared layout and its claim and
  release are written once, compiled into the driver and the host both, and model-checked**,
  as every other handoff here is. The driver kit is a pinned package restored by a script, so
  every machine and CI build against one version and nothing is installed.
- **A controller, the library's third half** ([00 §D16](00-overview.md)). The control side of
  the driver's channel is a module the host's display uses and a third half of the library
  beside the host and the client, under their conventions ([06](06-api.md)): open, close, plug a
  monitor with its modes and the preferred one, unplug, set a mode, list, and the driver's
  version. **A monitor is bound to the handle that plugged it, or kept**
  ([11 §1.3](11-drivers.md)): only its handle re-modes or unplugs a bound one, and the handle's
  end unplugs it, so an application cannot disturb the host's display, nor the host an
  application's; a kept one belongs to no handle, lives until removed, and may come back at start
  from a file the controller writes and the driver only reads, in a folder users may write.
  **Nothing keeps a monitor alive by traffic**: a ping that stalls with a debugger, a resume from
  sleep or a starved thread would take a display away mid-session, where a handle's end is the
  owner's own. Up to sixteen monitors, each a slot whose identity never changes, so the system
  keeps each one's arrangement; the host plugs one, bound. `lowlatd display` drives the
  controller from a shell. The control interface admits any interactive user -- plugging a
  display asks no more than a person at the desk may do, and ownership keeps one user's monitors
  from another's requests -- while **the frames are never in it** ([06 §13](06-api.md)): the
  driver's frame interface admits the system account alone, checked when it is opened.
  *Amended 2026-10-16, at W3.3's review:* what limits the frames is the handles. The answer to a
  request for a monitor's frames is a set of handle values in the driver's own process, which only
  a process allowed to take handles from it can use -- the system account, or an administrator
  who turns on the debugging privilege, who could find them there unasked. So the driver answers
  anyone the device admits, and nothing it does waits on whoever reads; a check at open would
  have guarded only what the handles already guard.
- **The host's display is configured like an output.** Whether a stream may plug it is settled
  at start; its name, `virtual`, and its size and refresh are live. Zero lets the host decide:
  the size an owner states as it joins an empty room, within the largest picture that guest
  accepts -- the owner's machine is not resized for a guest the owner let in -- else the size it
  already has, 1920x1080 when first plugged. The refresh is the stream's frame-rate ceiling, 60
  when that is zero: a display composing faster than the stream composes pictures nobody is
  sent, and the refresh a client states is a fixed number rather than a reading of its screen.
  A guest's later request for a size reaches it through the application, the last one winning
  ([06 §14](06-api.md)); `lowlatd` relays the settings panel's. A turn of the host's display is
  refused until its rotation has been checked. The last guest leaving changes nothing. A mode
  the monitor lists is set through the system's display configuration, never saved to the
  system's database of arrangements -- a saved arrangement that only the virtual display can
  satisfy holds a later boot waiting for the driver -- and a mode it does not list is a replug
  with that mode added first, since a monitor's modes are fixed when it arrives.
  *Amended 2026-10-16, at W3.2's review:* the display is plugged when the host starts -- the
  setting allowing it and nothing lit, or the configuration naming it -- and again whenever
  nothing is lit while the host runs, never on the first guest's arrival, since a stream exists
  only once a guest is seated; on a thread of the host's own, since the controller's calls wait
  for the system. It takes the highest free slot, so it keeps one identity and never takes an
  application's first free one. The size at a join, stated or kept, is clamped to that guest's
  largest picture and put before that guest's first picture; an established client states its
  own screen's size, so the join usually replugs the display with a mode no monitor lists.
  **The display holds its mode**: another monitor's arrival and a program leaving exclusive
  fullscreen each put a saved mode back on it, measured, so the host puts its own back
  whenever the system's differs. The enumeration lists it as `virtual`, the system's name for
  it beside that, so a request by name survives a renumbering, and reading the configuration
  back says `virtual` while it is captured. Off Windows the fields are accepted and nothing is
  plugged, and `virtual` is refused.
  *Amended 2026-10-16, at W3.2's build:* **lit means a monitor behind the output.** The last
  monitor's cable pulled leaves its output active and part of the desktop, composed for nobody,
  and no event marks it -- a DisplayPort and an HDMI panel alike, measured -- so the stream kept
  serving a still picture and the setting never found nothing lit. The monitor leaves only the
  list of monitors under the output, so an output counts as lit only with a monitor listed
  behind it: where outputs are read, and in the stream's check each second that its display is
  still there. A virtual display lists its monitor as a panel does. Any monitor's arrival clears
  the dead output.
- **The frame contract, made concrete.** Per swapchain the driver shares each distinct buffer
  once, with a fence of its device, and publishes their handles in a section the host maps; the
  host takes the handles from the driver's process and keeps a view per buffer until the
  swapchain is replaced. Per frame the driver acquires, notes the time, signals the fence,
  publishes the buffer with its sequence and frame number, finishes the frame, then waits on
  three things together -- the host's release reaching that sequence, the swapchain's end, and
  the bound -- before it acquires again. The host claims a sequence before it reads; at the bound
  the driver lets go of an unclaimed frame, which the host then skips, and of a claimed one too,
  counting it. **The swapchain's end ends a hold at once**: a frame held across a monitor's
  departure stalls the system's teardown until its watchdog ends the driver. The frame's time
  is the driver's acquire, not the time the system says the frame should be shown, which lies
  ahead of it. A buffer's reference is released at the next acquire. If the system's buffer
  will not open in the host, or the hold costs more than it saves, the driver converts into
  targets of its own, then copies, by measurement.
  *Amended 2026-10-16, at W3.3's review:* everything is the driver's -- the section, the buffers'
  handles, the fence and an event set at each publish -- and the host takes them from the
  driver's process, so nothing is ever placed in it. Per frame the driver acquires, notes the
  time, signals the fence, publishes the latest picture -- its buffer, sequence and frame number
  -- and finishes; nothing is claimed and nothing waits on the host, and with no hold there is
  none to end at a monitor's departure. The host takes the latest picture, waits for the fence on
  its device, converts, and checks the sequence after, as above. The buffer opens in the host as
  it is: no keyed mutex, read across processes, measured.
- **Signing.** In development the driver and its catalogue are signed with a test certificate
  the machine trusts: a user-mode driver needs neither test mode nor a signature from the
  system's vendor. Releases are signed by a signing service for open-source projects, from CI's
  build, so CI builds the driver. The library is never linked to demand the system vendor's own
  signature at load.
- **The floor**: the indirect display class at 1.4, the first that names the swapchain's render
  adapter, so Windows 10 1903; the user-mode framework 2.25; later calls behind their
  availability checks. The host's own floor stays 1703 for a physical display.
- **Desktop duplication of any virtual display** (W2's gap, corrected above): the device is made
  on the GPU that renders the output -- for the host's own display the one the driver reports,
  and for another vendor's whichever the encoder accepts, measured first.
  *Amended 2026-10-16, at W3.2's review:* nothing to build. The system lists a virtual display's
  output under the GPU that renders it, so the device W2 makes on the output's adapter is
  already that GPU's -- seen for the host's own display and an established host's own, each
  streamed as built. A virtual display rendered on a GPU without the encoder this host builds
  stays refused, as any output there is.
- **A kept monitor's arrangement is saved in the system's own store** (*added 2026-10-15, at
  W3.1's review*). A kept monitor is arranged like any monitor -- the system's own extend of a
  set it has not seen saves it, a person's settings save it, and the controller's activation and
  modes for it are saved too -- so it comes back after a reboot at its mode and place by the
  system's own restore, which on a replug also overrules the mode the plug asked for. **Except
  an arrangement in which only virtual displays would be active**, applied unsaved: a saved one
  that only the virtual display can satisfy holds a later boot waiting for its driver. A bound
  monitor's -- the host's display, an application's -- is never saved, as above.
- **The controller's calls wait for the system, within a bound** (*added at W3.1's review*): a
  plug until the monitor is active at its mode, a mode until it is current, an unplug until the
  monitor has gone; about three seconds, past which a status of its own says the monitor is
  plugged and the system has not finished. Calls on one handle take turns. The library's other
  calls never wait so long ([06 §8](06-api.md)); this half's are its stated exception.
- **One GPU for every monitor: the file's, else the high-performance one** (*added at W3.1's
  review*). The display class takes one render GPU for the whole adapter and re-creates every
  swapchain when it changes, so the file names one by vendor and device; with none, or one not
  in the machine, the driver names the first hardware GPU in the system's high-performance order,
  at its start and before any monitor. The host never names one: it builds on the GPU each
  swapchain reports, the encoder following the display.

### Steps

- [x] **W3.0 the driver and its package**: the crate and its generated declarations; the
  adapter, its monitors with a fixed identity, their modes and the hardware cursor; a frame loop
  that finishes every frame; the setup information, the package and its test signature; install
  and uninstall. Checked by a monitor plugged by hand arriving and composing, listed with the
  render GPU named, and the package installing silently with no test mode. *Amended at the
  build:* the system lists the output under the GPU that renders it, not on an adapter of its
  own as first written.
  *Built and checked live 2026-10-15.* Two crates: `lowlat-vdd-proto` holds what the driver and
  its controller share -- the control interface's requests and replies, each slot's identity
  and EDID, the modes -- with no platform code, tested on every platform; `lowlat-vdd` is the
  driver, a Windows library the system's user-mode driver host loads. The frameworks'
  declarations are generated from the pinned driver kit's headers by a script and committed;
  scripts restore the kit, make and trust the development certificate, package and sign the
  driver, and install and remove it. The control interface answers the version, a plug, an
  unplug and the list of slots, a monitor plugged through it bound to its handle; the driver's
  log goes to the debugger stream. Two things the build found. **A mode that asks for display
  bandwidth is never made active** when the adapter declares no budget, since the system keeps
  the active modes' sum within the adapter's: the first build's monitor arrived and never
  composed, and the target modes now ask for none. **The system makes a plugged monitor active
  by itself only in a set of monitors it has not seen**; a set it remembers it arranges as
  remembered, which left the first build's monitor dark ever after. So a monitor is made active
  by adding its path to the active ones through the system's display configuration, never saved
  -- W3.1's controller does so, and this step's check did by hand. Checked on the development
  machine, Windows 10 22H2 with the display class at 1.5: the package installing and updating
  silently with no test mode; a monitor plugged by an unelevated program in under a fifth of a
  millisecond, the system making a monitor of its EDID; made active in 164 ms, its mode
  committed, its swapchain on the RTX 5060 the system named and its pictures counted, at
  1920x1080 at 60 and at 2560x1440 at 120; a second slot's monitor, a set the system had not
  seen, extended by the system itself in 15 ms; the output listed under the render GPU; the
  first one to three swapchains after a mode is set taken back at once -- unassigned before the
  device is set, or lost at the first acquire -- and another assigned within about a quarter of
  a second, each ended and deleted without a wait; and the monitor unplugged by a request, by
  its handle's close and by its process killed mid-hold, each in under 10 ms. The protocol's
  checks were each broken once and seen to fail, and CI fetches the kit and builds the driver.
- [x] **W3.1 the controller**: the control interface, bound and kept monitors, plug, unplug, a
  mode set through the display configuration or by replug, the list; the slots, the custom
  modes and the file kept monitors come back from; the library's third half and `lowlatd
  display`. Checked by an unelevated program driving a bound monitor of its own beside another
  handle's, refused the other's, and killed with no monitor left behind; and by a kept monitor
  made unelevated coming back after a reboot at its slot, mode and position.
  *Reviewed 2026-10-15, before its build* (the last three decisions above, added at it): also
  the monitors reported as DisplayPort, as their EDID says; a replug that keeps its slot from
  departure to arrival; the file in a folder of its own, read as hostile.
  *Built and checked live 2026-10-15.* The driver's protocol at version 2:
  a plug carries the lifetime, whether a kept monitor comes back, and the slot it asks for; a
  replug adds a mode to one monitor; a reload rereads the file. Kept monitors belong to no
  handle. The file (`%ProgramData%\lowlat\display\displays.conf`) is read at the driver's start
  and on a reload -- the GPU, the pointer, the custom modes, a dumped EDID and the kept monitors
  that come back -- and written by the controller under a lock, replaced whole; the installer
  makes its folder, writable by users and readable by the driver's own account. The controller
  is `lowlat-vdd-ctl`: it plugs and waits for the system, makes the monitor active itself when
  the system has not within 400 ms, reads the mode back and sets the one asked for when a saved
  arrangement overruled it, saves as the decisions above say, and sets a listed mode through the
  display configuration and an unlisted one by a replug. The library's third half drives it
  (minor 26, Windows alone, [06 §3c](06-api.md)), and so does `lowlatd display`. Checked on the
  development machine: from an unelevated shell, a kept monitor plugged in 612 ms -- a set the
  system remembered dark, made active by the controller and saved -- set to a listed mode in
  99 ms and to an unlisted one by a replug in 170 ms, and removed in 197 ms; a bound monitor
  held by one program, its remove and its mode refused to another, and unplugged in 7 ms when
  its holder was killed; a kept monitor marked to come back plugged by the driver 7 ms after its
  device restarted, before any request, and shown by the system at its saved mode and place; the
  GPU, the pointer, the custom modes and the EDID changed through the file, and refused where
  wrong; the third half driven through the C interface for both lifetimes, its deliberate panic
  contained; the protocol's checks each broken once and seen to fail, 45 of them. **And after a
  reboot**, a kept monitor an unelevated shell made came back at its slot, mode and place:
  plugged again by the driver from the file at its start, arranged by the system from its
  store.
- [x] **W3.2 the host's display, and desktop duplication of any virtual display**: the setting,
  the name, the live mode, the owner's size at join, `lowlatd`'s relay; the device on the GPU
  that renders an indirect output. Checked by the host streaming its own display through desktop
  duplication -- the baseline -- and two other vendors' virtual displays.
  *Reviewed 2026-10-16, before its build* (the decisions above, and the pace in W2's): the first
  check passed -- the stream as built takes the host's own display and an established host's
  own -- so the device half is nothing to build, and an established host's own display stands
  for the other vendors' here, a second one left to the gate. Checked as well: the display
  plugged by name with the panels lit, and by the setting with them taken out of the topology;
  an owner's stated size, listed and not, at the first picture with no blink, a second guest's
  ignored, one past that guest's largest picture clamped; the live size and refresh through the
  library and through `lowlatd`'s relay, the blink measured; the saved mode put back by another
  monitor's arrival and by a program leaving exclusive fullscreen, each restored; the pace at
  the compositor's clock under a 144 and a 240 Hz mode, settled again when only the refresh
  changes, and a ceiling below the clock taking each picture as it lands; a turn of the host's
  display refused.
  *Built and checked live 2026-10-16.* The host's own display lives with the stream's thread: plugged at the host's start where the
  configuration names it, or allows it and nothing is lit -- asked again each second while
  nobody is seated and at every build -- bound to the host, in the highest free slot; held at the
  mode the host wants, put back whenever the system has it at another; and sized as a guest
  arrives into an empty room, before that guest's first picture is built, the guest's
  declaration travelling with its seat. Its name is `virtual` wherever outputs are listed or
  asked for. The pace is the lowest of the rate asked, the output's mode and the compositor's
  clock, which is also the refresh the deadlines sit on, and a change of either is a rebuild; the
  rate asked is kept apart from the pace, so a rebuild settles again from it rather than from
  what the last display allowed. `lowlatd` turns the setting on (`--no-virtual-fallback` turns it
  off), passes a guest's rate to the host's display unbounded, since the display follows it,
  relays a guest's size into the display's setting and refuses a turn of it, and tells the guests
  a new rate. **The controller puts a mode again when the system takes it back**: a monitor
  plugged again is restored to its set's saved arrangement as its arrival completes, which came
  after the mode was put -- more than three seconds after while a program presented on the
  display, measured -- and a bound monitor's unsaved mode did not survive it. **An output is lit
  only with a monitor behind it** (the amendment above), since the last cable pulled leaves its
  output on the desktop. Checked on the development machine: the display plugged by name with the panels lit 26 to 118 ms after the
  host started, in slot 15, streamed at 120 a second with each picture handed over 0.08 ms after
  its composition at the median, and gone when the host stopped; an owner's 2560x1440 put 163 ms
  before the first picture, which was already that size; 3420x2148, which no monitor lists, put
  by a replug in 201 ms, and in 622 ms with a program presenting on the display; a guest's rate
  re-moding the display in 109 to 116 ms, the stream rebuilt behind it; a mode set from outside
  the host put back in 119 ms; asking 144 of the display paced at 120, the compositor's clock, and
  the guests told 120; asking 60, each picture taken 0.9 ms after its composition at the median
  and 1.4 at p95, where 8.3 was measured with the deadlines on the mode; an established host's own
  display at 144 paced at 120; the default `lowlatd` with the panels lit plugging nothing; the C#
  host at minor 27; the new checks each broken once and seen to fail, nineteen of them; both
  bars. **At the desk**, an established client on a laptop through `lowlatd`: its owner's
  1366x768 put 179 ms after the declaration, the first picture at that size; the settings
  panel's 1920x1080, 2560x1440 and 1366x768 each put 121 to 127 ms after the request, the
  picture at the new size 331 to 335 ms after it; two turns refused with a line, nothing
  rebuilt. The setting with the last panel's cable pulled while streaming: the stream left the
  output with nothing behind it 0.96 s after the pull, the host's display was plugged 113 ms
  later and streamed 0.27 s after that, about 1.35 s from the pull to the picture; the panel
  plugged back, the host's display stayed and its saved 60 Hz, put back by the arrival, was put
  to the asked 30 in 154 ms. Before the rule the same pull froze the picture until a monitor
  arrived. Nothing lit cannot be made from software here -- the system refuses a desktop with no
  output, and disabling a monitor's device leaves its output lit -- hence the cable. A program
  leaving exclusive fullscreen takes the road a mode set from outside took, not checked again.
- [x] **W3.3 the direct frame**: the frame interface, the section, the shared buffers and
  fences, the claim, the hold and its bound, a view per buffer, the frame's time; the Windows
  display's second source. Checked by the gate's comparison, run offline first, and by each part
  broken and seen to fail: no wait on the fence, no release, a claimed frame past the bound.
  *Reviewed 2026-10-16, before its build* (the decisions above, amended): a probe answered what
  the design rested on, on the development machine. The system's buffer is eight-bit BGRA, read
  by a shader, shared by an NT handle and with no keyed mutex, so a D3D11 reader in another process
  opens it as it is; a swapchain has three, used in turn, at 2560x1440 at 120 and 1920x1080 at 60
  alike. A reader as the system account took the buffers and the fence out of the driver's process,
  opened them on a device of its own on the GPU that renders them, and read 1,790 pictures in
  fifteen seconds, each the one published and none torn, with the wait on the fence and without:
  on one GPU with the scheduling in its driver, the reads ran after the composition either way, so
  that the wait is needed cannot be shown here, and it stays. The driver's acquire to the reader
  seeing the picture took 0.013 ms at the median, to four pixels read back 0.39 ms. Read late on
  purpose, every picture was intact up to 24 ms after its acquire at 120 a second and none from
  25 ms -- the buffer written again for the third composition after it -- and at a 60 Hz mode none
  was wrong up to 30 ms. Hence no hold (above). **The duplication stays the host's path through
  this step**: the direct source is built into the Windows display and run by the hardware tests
  and the comparison, as the system account, and becomes the host's own display's source at W3.4.
  Its checks become: the publication model-checked; the freshness check broken by a stalled
  reader, which then converts a later picture; the driver's process killed and its device
  restarted mid-stream, twenty times each, the host never stuck behind its wait on the fence; two
  readers at once; a user's process given nothing it can use. No release, bound or claim exists to
  break.
  *Built 2026-10-16*, not ticked: three checks wait on a restart of the machine. The page is
  written once for both sides, its layout and its sequence lock, model-checked over every
  interleaving of a writer publishing twice and a reader, and shown to fail with the reader's
  second look or the writer's release removed. The driver makes a monitor's page at the first
  request for its frames and keeps it for its life; per swapchain it shares each buffer once and
  signals a fence of its device after each acquire, then publishes. **It keeps each acquired
  buffer until the next acquire**, so a page asked for while the desktop is still is given the
  picture on the display -- missing at first, when a reader that asked after the desktop went
  still waited for its next change. The request is answered to anyone the device admits: an
  ordinary user's process was answered and refused the page, the system account given it. The
  host's display reads the page as a second source, run by hardware tests and the comparison
  alone (the decision above). Found at the build as well: **a reader's same picture is its own
  last frame**, never the driver's mark of a repeat; and, by reading, **the refresh comes with
  each swapchain**: taken once at the open, the rate stays at 120 after the monitor is put at 60,
  as a test shows with the read taken out, where it now settles at 60 within a tenth of a
  second. Checked on the development machine, as the
  system account: the first picture at once on a still desktop; a reader stalled 30 ms after
  queuing each conversion caught 20 times in 20 and converted again, and never on time; a reader
  that stopped taking during a burst converting the still desktop's last picture; the driver's
  process ended right after a publish five times, each conversion queued behind its fence
  finished, the output gone in 85 to 97 ms and the driver back in 120 to 300 ms. **The comparison,
  offline**, a load presenting on the display, interleaved blocks of 600, composition to the
  conversion done in ms at p50/p95/p99/max: at 2560x1440 at 120 the duplication
  0.661/0.833/0.870/0.924 and the direct frame 0.608/0.767/0.820/1.326; at 3840x2160 at 120,
  0.943/1.139/1.189/1.462 and 0.841/0.997/1.053/1.277; at 1920x1080 at 60 under the 120 Hz clock,
  1.663/4.144/5.531/6.120 and 1.229/4.904/5.203/6.282, both tails unexplained; none converted
  again. The duplication's own copy delays the conversion's start by about 0.05 ms at 1440p and
  0.1 at 4K. **Owed, after a restart of the machine**: the driver restarted twenty times
  mid-stream -- the first restart, made while the test held a handle on the device, was refused
  and left the device waiting for a reboot ([11 §1.8](11-drivers.md)) -- the still desktop's
  picture broken on purpose in the driver, and the frame thread's priority measured with and
  without under load. The 240 Hz leg is the gate's.
  *Checked 2026-10-16, after a restart of the machine*, as the system account: the device
  restarted twenty times mid-stream with the test's handle closed first, nothing stranded, the
  output gone in 160 to 206 ms; the driver's process ended five times, not twenty -- the system
  starts a driver whose process ends again no more often in a few minutes. The driver's
  publication of the held picture switched off, the first picture on a still desktop did not
  come within 150 ms, and on, it came at once. Two readers waiting on one page together at 120 a
  second each converted all 240 pictures, neither waiting more than 9.9 ms; no break was built
  for it. **The frame thread stays at normal priority** ([11 §1.1](11-drivers.md)): under one
  busy loop per logical processor, raised through the system's multimedia scheduling it acquired
  one picture in twenty 1.4 to 1.6 ms later than the median, where at normal priority nineteen in
  twenty came within 0.09 ms. Found: an update restarts the other virtual displays' devices too,
  and one held open by its own software leaves the update waiting for a reboot, this driver
  restarted onto the new one all the same ([11 §1.8](11-drivers.md)).
- [x] **W3.4 the pointer from the hardware cursor**: shape, hotspot and visibility from the
  driver, position and show state as in W2. Checked by each kind of shape, one larger than the
  wire's 256, and the visibility while the system draws the pointer itself.
  *Amended 2026-10-16, at W3.3's review:* with its pointer, the host's own display reads the
  direct frame whenever the driver answers, and duplicates it otherwise, so W3.5's checks and the
  gate stream it; before this step its pictures would carry no pointer.
  *Reviewed 2026-10-16, before its build.* **The driver publishes the pointer in the monitor's
  frame section**, after the pictures' page ([11 §1.7](11-drivers.md)): whether a hardware cursor
  is set up and whether it is visible, and the shape -- its kind, size, row pitch, hotspot and the
  system's number for it -- with its pixels copied in only when that number moves; each part under
  a sequence lock of its own, written by the monitor's worker when the system signals new cursor
  data, model-checked as the pictures' part is. The frame channel's version becomes 2. The host's
  own display reads it in place of the duplication's frames: the visibility and a new shape into
  the pointer state W2 built, the position, the show state and the touch flag from the system as
  for any display, settled as in W2. **The frames need no desktop, the pointer's read does**: the
  reading thread follows the input desktop whenever that read fails, at the duplication's retry
  pace, so the lock screen and an elevation prompt keep their pointer. The system hands the
  driver two kinds of shape, alpha and masked colour, every monochrome pointer converted to the
  latter, which is drawn as the monochrome one it is (W2's decisions, amended). **A pointer
  larger than 256 stays the system's to draw**: drawn into the pictures and reported invisible,
  so a guest is told it is drawn and takes it over until it shrinks, as for any pointer the
  system draws; the duplication keeps its last shape, its pictures never carrying the pointer.
  No hardware cursor -- the file's setting, or a setup the system refused -- is said as such:
  the pointer is in the pictures. The cursor is read on the frame's own worker unless measured
  to move the frame's acquire. The host's own display opens the direct frame when the driver
  answers -- the host runs as the system account -- and the duplication otherwise; no
  configuration selects it. Checked by: each kind of shape on both sources from the same pointer
  -- an arrow, the text cursor, a coloured inverting pointer, a monochrome one of the test's own,
  a hand -- drawn alike, and the monochrome rule broken once; a pointer over 256 reported hidden
  with the picture carrying it; a pointer an application hides turning relative, and its showing
  again carrying the position; the pointer leaving the display and coming back; the lock screen
  and the secure attention screen keeping their pointer, broken by not following the desktop; a
  pointer swept a thousand times a second against the frame's acquire; the host's own display
  streaming direct as the system account and duplicating as a user, and back on direct after its
  driver restarts; the publication model-checked; both bars.
  *Built and checked 2026-10-16*, on the development machine, as the system account. **The
  pointer reads the same from the driver as from the duplication**: the system's arrow, text
  cursor and hand, a monochrome pointer of the test's own and one inverting in colour, each the
  same picture to the byte with the same hotspot; with the monochrome rule taken out, the text
  cursor and the monochrome pointer differed and the rest did not -- the duplication reports the
  text cursor monochrome and the driver is handed it in masked colour, as the review read. Hidden
  by an application the pointer turned relative on both, and shown again absolute at its place;
  off the display nothing was said, and back on it the state was right. **The secure desktop
  keeps the hardware cursor**: brought up by a person with Ctrl+Alt+Del, the reading thread
  followed the input onto it and the driver's pointer was said there, not hidden; with the
  following taken out, a desktop of the test's own taking the input left the pointer's change
  unsaid. The host's own display opened on the direct frame as the system account and duplicated
  as an ordinary user, said once why; opened again after its driver's device restarted, direct.
  The cursor's read costs 4 to 5 us at the median, 13 at p99, about 920 a second with the
  pointer swept, so a picture's acquire waits behind one at most that long -- one read in 48,000
  took 1.7 ms -- and the read stays on the frame's worker; the acquire's own timing could not
  show it, the compositor's clock sitting at a refresh's edge of the display's and splitting
  its frames between two refreshes, swept or not. The publication model-checked, a reader
  waiting for the state to name the shape; shown to fail with the state published before the
  pixels. Found: **a pointer over 256 is hidden on the duplication too** -- it keeps its last
  shape and reports the pointer not visible, the system drawing it (the review said otherwise,
  corrected in [05 §8.2](05-host.md)); that the picture then carries it was not read back.
- [x] **W3.5 lifecycle and no monitor**: a host killed mid-hold, the driver updated, the GPU's
  driver restarted, the display asleep, the lock screen, the secure attention screen and an
  elevation prompt, a logoff, a remote desktop connection, and a machine with no monitor.
  Checked by each once while streaming.
  *Reviewed 2026-10-16, before its build.* Nothing has held a picture since W3.3's review, so the
  first check is the host killed mid-stream and started again. **The logoff and the remote
  desktop connection move to W2.5**: each is a change of the console's session, which only the
  service follows, and for the display either is the host's end and a new host's start, checked
  here; the boot with no monitor stays the gate's. Read in the code: the host keeps its
  controller's handle for its life and never closes it when the driver is asked to be removed,
  which an update needs -- an open handle refuses the removal, and the update then waits for a
  reboot ([11 §1.8](11-drivers.md)); a handle left by a driver whose process ended is kept for
  good, so the host's display never comes back; and a stream that lost the host's display waits
  for one without plugging it again. So **the controller follows its driver**, for every user:
  it closes its handle when the driver is asked to be removed or has gone, the monitors bound to
  it going with it, and every call after answers that the driver is absent, a new open being a
  new handle; and **the host's own display follows its driver**, opened and plugged again
  whenever it went, also while the stream waits for a display, so an update, a restart or the
  driver's end costs the guests a rebuild rather than the session. Probed on the development
  machine: the GPU's driver restarted three times under a monitor of the driver, the system put
  the monitor back on the GPU by itself, on the GPU's new identity, 3.5 s after the old swapchain
  was lost twice and 17.5 s once -- the first restart since boot, the GPU itself back that late
  -- so the driver names no GPU again. **But for about 1.5 s mid-restart the system lists the
  display under the integrated GPU**, and the panels under its software renderer, where a stream
  rebuilding -- it sees its device lost within a second -- refuses the display as one without its
  encoder and ends the guests: **a display found under a GPU without the encoder is waited for**
  within the display wait, as one that went, and refused only past it. **A display asleep keeps
  its output and loses its pictures**: the system took the driver's swapchain away about five
  seconds before saying the display was off, and gave one back as it woke; the stream's request
  kept a display on past its timeout but did not wake one, and one injected pointer move of no
  distance woke it, which a setting off by default does as a stream starts (W2's decisions,
  amended). Checked by: the host killed mid-stream and started again; the driver updated
  mid-stream, through with no reboot owed and the guest kept; the driver's process ended once;
  the GPU's driver restarted, the guest kept; a guest arriving at a display asleep with the
  setting off and on, and the display held awake while streaming; the lock screen and its unlock
  typed through the guest, the secure attention screen and an elevation prompt; the host's end,
  the update and the GPU's restart again with the panels' cables out; the machine asleep with the
  host idle and its display plugged, and streaming after it wakes; both bars. The update's close
  broken leaves the device waiting for a reboot, so it is broken only at the user's word.
  *Built 2026-10-16*, not ticked: the checks that need a person at the machine wait for one.
  **The controller follows its driver**: a notification on its handle closes it as the driver is
  asked to be removed, and a request answered that the driver's process ended closes it too --
  that answer comes within 2 ms of the end, measured, with no notification before it. **The
  host's own display follows**, plugged again on a new handle, also while the stream waits for a
  display, which waits out an output under a GPU without the encoder. `wake_display` in the
  library's host configuration and `lowlatd --wake-display`, off by default. **Found live: the
  stream spun after its driver was updated** -- the wait for the direct frame had no bound before
  its first picture and answered at once once the driver's process had ended, so a stream built
  before the new driver's first picture was paced by the clock and ticked every few
  microseconds; both waits now run to the refresh ([05 §2](05-host.md)). Checked on the
  development machine, the host as the system account and the library's demo as the guest: the
  host killed mid-stream, its display gone with it and plugged again by the next host 30 ms
  after its start; the driver updated mid-stream twice, installed with no reboot owed and the
  guest kept, about 0.9 s without pictures; the driver's process ended mid-stream, the display
  plugged again 0.2 s after and the guest kept; the device restarted with a handle open and a
  monitor bound to it, through in 87 ms where it was refused before; the GPU's driver restarted
  mid-stream, the host's display read again on its GPU 3 s after the stream saw its device lost --
  the demo, decoding on the same GPU, did not survive its own device's loss; the host's display
  rendered on the integrated GPU on purpose and back, the stream waiting 6 s and going on, where
  the build before ended the guest at once; **the display asleep**, its swapchain taken away
  with the output still listed: with the setting off a guest arriving got nothing for 13 s until
  one pointer move standing in for its first input, then pictures within a quarter second; with
  it on, the stream woke the display as it started and the guest had pictures in its second
  second, and watched 90 s with no input past the 60 s timeout with the display held on; each new
  check broken once where it can be without a reboot; both bars. **Owed**: a guest on another
  machine through the GPU's restart and on the lock screen, its unlock typed through it, and the
  secure attention screen; the host's end, the update and the GPU's restart with the panels'
  cables out; the machine asleep with the host idle. **No elevation prompt could be raised on the
  development machine** as it was set: its administrators elevated with no prompt, so none
  reached the secure desktop.
  *Checked with a person 2026-10-16*, an established client on a laptop over the LAN driving the
  host's own display at the laptop's 2880x1800. The GPU's driver restarted with the panel lit:
  the rebuild landed in the integrated GPU's listing, waited it out and went on in about 3 s, the
  session kept. **The lock screen and the secure attention screen with the panel lit showed black
  to the guest** -- the system draws them on the primary display alone, and the host's display
  was a secondary; the stream followed the input onto them and back regardless ([05 §2](05-host.md)).
  With both cables out, the host's display the only one: the host killed and started again, its
  display plugged again by the next host and the guest connecting again; the driver updated, the
  session kept; the GPU's driver restarted, the session kept through about two seconds without
  pictures; the lock screen and the secure attention screen shown to the guest and unlocked by the
  password typed through it; and **the machine put to sleep** by a person, woken to the lock screen
  on the host's display, the stream reading it again on the GPU's new identity and the guest
  connecting again -- its session had ended while the machine slept. A reset of the display stack
  from the keyboard put the display's saved mode back, which the host put right in a tenth of a
  second. **An elevation prompt**, once the person turned prompting on: the stream followed the
  input onto it; the host's display beside the lit panel showed the prompt's dimmed backdrop, its
  dialog drawn on the primary alone; the guest switched its stream to the panel, answered the
  prompt through it, and switched back.
**Gate:** after W2.5, on the development machine, with established clients on macOS and Android
on the LAN and one on a Windows machine:

- with no monitor: the machine booted with its monitors unplugged, the service's host plugging
  its display, the logon screen streamed, a sign-in, the desktop at the owner's size;
- ten minutes each of H.264 and HEVC at 120 a second, ten-bit and full chroma where the encoder
  takes them;
- once each mid-session: a size change from a client's settings, its blink recorded; the lock
  screen, the secure attention screen and an elevation prompt; a logoff and the reconnect; the
  GPU's driver restarted; the driver updated; the host killed and started again -- no watchdog,
  no stop error, no blank beyond a swapchain's replacement;
- the controller: an unelevated program plugs, re-modes and unplugs a monitor of its own beside
  the host's, and is refused the host's and the frame interface; a kept monitor made through
  `lowlatd display` comes back after a reboot where it was;
- two other vendors' virtual displays streamed through desktop duplication, an established
  host's own among them;
- **the comparison decides the host's default**: in one session at 60, 120 and 240 Hz,
  composition to encoder input at p50, p95 and p99 for desktop duplication of the host's display
  and for the direct frame, with the hold's distribution and how often its bound fires; the
  direct frame becomes the host's path only at or below the duplication at every rate and
  percentile, else the duplication stays the path and the numbers are recorded;
  *amended 2026-10-16, at W3.3's review:* with no hold, how often a picture is converted again for
  coming too late in its place; for both sources, the blackout at the lock screen and at an
  elevation prompt, from the switch to the first picture of the new desktop; the 240 Hz leg with
  the host's display as the primary, unsaved and held, since the compositor's clock is the
  primary's; and the GPU scheduling mode recorded, every measurement so far having had it in the
  driver;
- the test-signed package installing silently with no test mode; CI building the driver; both
  CI jobs and the Linux bar.

## Change log

- 2026-10-16: W3.5 checked with a person and ticked: a guest on a laptop through the GPU's
  restart, the lock screen and the secure attention screen, with both panels' cables out the host's
  end, the update and the GPU's restart, and the machine's sleep; the secure screens black on the
  host's display while it is a secondary; an elevation prompt answered through the guest, its
  dialog drawn on the primary alone.
- 2026-10-16: W3.5 built, not ticked: the controller follows its driver, the host's own display
  follows it and the stream's wait plugs it again and waits out a display under a GPU without the
  encoder; `wake_display`; the stream's spin after a driver update found live and fixed. Checked
  with the demo as the guest; a guest on another machine, the cables and the machine's sleep owed.
- 2026-10-16: W3.5 reviewed: the logoff and the remote desktop connection moved to W2.5; the
  controller closes its handle when its driver is asked to be removed or goes, for every user;
  the host's own display follows its driver, also while a stream waits for a display; a display
  listed under a GPU without the encoder is waited for, as the system lists one there while a
  GPU's driver restarts; a setting, off by default, wakes a sleeping display as a stream starts.
- 2026-10-16: W3.4 built, checked and ticked: the pointer the same to the byte from the driver
  as from the duplication, the text cursor by the monochrome rule; the secure desktop keeping
  the hardware cursor, the pointer's read following the input onto it; the host's own display
  direct as the system account and duplicated as a user; a pointer over 256 hidden on both
  sources; the cursor's read 4 to 5 us, kept on the frame's worker.
- 2026-10-16: W3.4 reviewed: the driver publishes the pointer in the monitor's frame section,
  the channel's version 2; the pointer's read follows the input desktop; a masked colour pointer
  that is a monochrome one in another form is drawn as one; a pointer over 256 stays the
  system's to draw; the host's own display reads the direct frame when the driver answers.
- 2026-10-16: W3.3 checked and ticked after a restart of the machine: the device restarted twenty
  times mid-stream, the held picture's publication broken and caught, two readers at once both
  served; the frame thread left at normal priority, since raised it took a fifth of its pictures
  up to 2 ms late under load; an update restarts the other virtual displays' devices, and one
  held open leaves the update waiting for a reboot.
- 2026-10-16: W3.3 built: the frame page written once and model-checked; the driver's half -- a
  page per monitor for its life, each buffer shared, the fence signalled, the last picture kept
  -- and the host's display's second source, offline until W3.4; the comparison offline at 1440p
  and 4K at 120 and 1080p at 60; a restart refused with a handle open leaves the device waiting
  for a reboot; three checks owed after one.
- 2026-10-16: W3.3 reviewed: the driver never waits for the host -- a probe found the system's
  buffer shared with no keyed mutex and read across processes, three buffers in turn, and a
  picture intact for about three refreshes, so the hold, the release fence and the claim go; the
  frames limited by who can take handles from the driver's process; the duplication the host's
  path until W3.4; the gate's comparison without a hold to measure.
- 2026-10-16: W3.2 built and checked: the host's own display plugged, held, sized at an owner's
  join and named `virtual`; an output lit only with a monitor behind it, since the last cable
  pulled leaves its output on the desktop; the pace and the deadlines' refresh at the
  compositor's clock, the rate asked kept apart from the pace; the controller putting a mode
  again after a replug; `lowlatd`'s setting, relay and new-rate notice; minor 27.
- 2026-10-16: W3.2 reviewed: any virtual display streams through the duplication as built; the
  pace and the deadlines' refresh at the rate the desktop is composed at, amending W2's
  duplication decision; the host's display plugged at the host's start, in the highest free
  slot, sized at an owner's join within that guest's limit, holding its mode, listed as
  `virtual`.
- 2026-10-15: W3.1 checked after a reboot, a kept monitor back at its slot, mode and place;
  ticked.
- 2026-10-15: W3.1 reviewed and built, checked live but for the reboot: a kept monitor's
  arrangement saved in the system's store unless only virtual displays would be active, a
  bound one's never; the controller's calls waiting for the system within a bound; one GPU
  for every monitor, the file's or the high-performance one, never the host's; the monitors
  reported as DisplayPort; the file, its folder and its reader; the controller crate, the
  library's third half (minor 26) and `lowlatd display`.
- 2026-10-15: W3.0 built and checked live: the driver and its protocol crate, the generated
  declarations, the kit restored by a script, the package test-signed and installed silently;
  a mode asks for no display bandwidth; a plugged monitor made active through the display
  configuration, unsaved, since the system leaves a set it remembers as it was; the output
  listed under the GPU that renders it, correcting W2's correction; the first swapchains after
  a mode is set taken back while it settles.
- 2026-10-15: W3 planned at its interview: the monitor lives with the host, plugged when a
  stream finds nothing lit and the configuration allows it, or when named; the driver in Rust in
  this workspace, its channel's layout and handshake written once and model-checked; a
  controller as the library's third half, a monitor bound to its handle or kept and restored at
  start, no keepalive, `lowlatd display` for people; the design in 11; the host's display
  sized through the configuration from an owner's stated size and the stream's rate; test
  signing in development and an open-source signing service for releases; the floor Windows 10
  1903; desktop duplication of any virtual display, which W2 refused (its decision corrected);
  the gate after W2.5, its comparison deciding the default path.
- 2026-10-15: W2.4 built and checked live: the loopback and its silent stream, the host's own
  rounding, flagged packets as silence, the default followed and a lost output opened again at
  once where it can be, the local mute only where the output mutes in hardware.
- 2026-10-15: W2.4 reviewed before its build: the silent stream started once, a flagged packet
  counted as silence, the sound taken as floating point and rounded by the host, the default
  followed through the system's notifications, exclusive use a lost device, the local mute only
  where the output mutes in hardware; leaving one program out of the sound deferred to W2.6.
- 2026-10-15: W2.3 built and checked live: the generated key translation, the batch input call
  following the input desktop, the locks from the host's state with a lock key as the peer's
  state, the bus's pads with their readiness and rumble, opcode 5 pointing down on both
  platforms, the chord refused; absolute positions aimed at their pixel's centre on both
  platforms, Linux's measurement owed.
- 2026-10-15: W2.3 reviewed before its build: the key translation generated from the system's
  own, with the peers' extra codes and the keys a scancode cannot carry; the power key unmapped;
  the locks from the host's state; report pads played as the Xbox pad without the application;
  opcode 5's vertical axes pointing down on the wire; the attention chord the system's own call,
  made by the service (W2.5).
- 2026-10-14: W2.2 checked further: a lost device is a rebuild with the guest kept, and a name
  the system renumbered follows its monitor (amending the naming decision); exclusive
  fullscreen at another mode is followed in both presentation models.
- 2026-10-14: W2.2 built and checked live: the duplication and the conversion in all four
  layouts, the display on the output's own adapter, the pointer settled by the display, the
  daemon per-monitor aware; streamed as the system account through the lock screen, the secure
  attention screen and a switch of display. A frame of the pointer alone ends no wait, and
  ten-bit full chroma is granted, amending the review's two defaults.
- 2026-10-14: W2.2 reviewed before its build: the pointer's three bits each from its own
  source and one message per settled state ([05 §8](05-host.md) corrected with it); the host
  per-monitor DPI aware, its duplication call refused otherwise; a still desktop's presents;
  the display kept awake; full chroma's packed layouts; the encoder's own conversion measured
  no dearer than ours on the development card ([05 §3](05-host.md) corrected); the second
  display moved onto the NVIDIA card for the gate's switch.
- 2026-10-14: W2 planned at its interview: one process of the system account in the console
  session, the service starting and watching it; NVENC alone, AMD's and Intel's encoders
  deferred; desktop duplication converted straight from the acquired image; outputs named
  `DISPLAYn`; input by scancode through the platform's own translation, and the ViGEm pad; the
  loopback kept running by a silent stream; the floor at Windows 10 1703.
- 2026-10-14: W3 added, the host's own virtual display, with its frame hand-off decided: the
  host converts straight from the system's buffer, which the driver holds to a bound. W2's
  frames question answered by it; W2's virtual pad is the ViGEm bus's, and a HID device of
  the host's own is deferred.
- 2026-10-01: W1.8 built: the system's decoder, its own kind and slot, last in the automatic
  order; HEVC at ten bits as well, amending W1's decision.
- 2026-10-01: W1.7 reviewed before W1.8: on an Intel GPU the automatic order tries the
  system's interface first and builds Intel's decoder only where it does not open
  ([10 §5.2](10-client.md)).
- 2026-09-30: W1.7 built: Intel's decoder, second on an Intel GPU as measured; its older
  runtime planes only, its check owed until a part that needs it is at hand.
- 2026-09-30: W1.6 reviewed before W1.7: the automatic order now tries both hardware decoders
  on a GPU before it leaves it, and a fault met handing a picture out is judged as one met
  decoding, on every decoder ([10 §5](10-client.md), §5.2).
- 2026-09-30: W1.6 built: AMD's decoder first on an AMD GPU where its runtime has the
  low-latency mode, as measured; the demo's growth by handle on a GPU other than the display's
  found and left to W1.9.
- 2026-09-29: W1.5 built: NVDEC first on an NVIDIA GPU, as measured; a decoder that fails past
  recovery ends the session by the library's own departure, on both platforms.
- 2026-09-29: W1.4's search for a lost GPU amended: the same hardware alone, for five seconds
  rather than ten, a session nobody placed taking another GPU only when it has run its course
  ([10 §4.2](10-client.md)).
- 2026-09-28: NVDEC moved ahead of AMF, as W1.5, AMF W1.6; W1.7 Intel VPL added, with the
  older runtime for the parts the current one does not reach, built though the system's
  interface did not prove short on the Intel card, its place on an Intel GPU by measurement;
  the steps after it renumbered: the system's decoder W1.8, the demo W1.9, packaging W1.10.
- 2026-09-28: W1.4 closed; the renderer's placement by the display's GPU added to W1.8, from
  W1.4's measurement; the handle route's processor time on the AMD GPU noted for W1.5.
- 2026-09-28: W1.5 AMF added, in its low-latency mode and first on an AMD GPU; the steps after
  it renumbered: NVDEC W1.6, the system's decoder W1.7, the demo W1.8, packaging W1.9.
- 2026-09-28: W1.3 closed with the Intel card's pass.
- 2026-09-27: W1.3 built on two GPUs, the third's pass owed; a GPU named by its identity moved
  from W1.4 to W1.3; the never-waits decision scoped to the handle kind.
- 2026-09-27: W1.2 built; W1.4's minor corrected to 19.
- 2026-09-26: W1.1 built; the seam and the library's client half moved to W1.2.
- 2026-09-26: W1 planned at its interview.
- 2026-09-26: planned; W0 built, W0.1 to W0.6.
