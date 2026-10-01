# Implementation plan: Windows

**Status:** planned 2026-09-26. Phase W0, the platform seams, is closed; the client's phase
(W1) was planned at its interview the same day, and the host's (W2) is planned at its own.

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
- **The demo's toolkit gains two things**: a choice of the GPU its device is made on, for a
  menu, and a pad's feature reports read and written by the toolkit's own controller id. A
  Sony pad's input reports come from the toolkit's events and its output reports go out
  through the toolkit, so the demo never opens a pad beside it.

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
  session here.)
- [ ] **W1.10 packaging**: the zip from the build workflow; CI decodes the clips through a
  downloaded LGPL pair, since its runner has no GPU, and builds the demo. Checked by the
  artifact, built and unpacked.

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

## Phase W2 - The host (to be planned)

Desktop duplication, a D3D11 conversion, the vendor encoder on the capture's device, input
through the system's batch input call and a virtual pad device, loopback sound, and a
service. Open before it is planned: the process topology ([07 §10](07-platforms.md)), and
whether an application may supply the frames, for a virtual display that already holds them.

## Change log

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
