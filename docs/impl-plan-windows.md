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
- **A picture is finished on the device before it can be acquired, and the decode thread
  never waits for it.** The decode thread queues the decode, the split into the plane
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
  known only then.
- **The software decoders**: the pair keeps D14's rule, loaded at runtime only when it is an
  LGPL build and never shipped, and is found in the directory the application names or beside
  the application, by the platform's versioned names. The system's decoder is software only
  and eight-bit 4:2:0 only, H.264 on every edition and HEVC where the system's HEVC extension
  is installed, and it comes after the pair.
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
- [ ] **W1.1 net**: the completion-port platform module ([02 §5, §6](02-io-shell.md)): the
  socket and its options, never address reuse, since a second bind succeeds there; the
  source address taken through the message receive; a wake posted once until it is taken;
  receive slots pinned for the socket's life and drained up to 256 a call; cancel and drain
  before a slot is freed. The seam and the library's client half, which sit on the shell,
  build for Windows with it. Checked by the shell's receive tests, which are the platform
  module's contract. A timeout on Windows ends at the tick after it expires: the address
  wait's 1 ms lasts 2.0 ms with the resolution raised, measured; the port's own wait is
  measured when it is built.
- [ ] **W1.2 software, and the demo on Windows**: the libavcodec pair loaded there, and the
  demo built with the platform's compiler, drawing planes through the toolkit's D3D11
  renderer. Checked by the first picture: ten minutes from an established host.
- [ ] **W1.3 D3D11 planes**: the backend fed from the readers' jobs, read back to planes.
  Checked by every clip decoding bit for bit on each of the three GPUs.
- [ ] **W1.4 the handle**: the plane split, reading the decoder's output directly where a
  driver lets a shader read it and copying it first where one does not; the shared textures;
  the library's fence and the newest finished picture; the GPU on every frame; per-slot
  backing; `lowlat_client_set_frame_kind`; a GPU named by its identity. Minor 18. Checked by
  handles on all three GPUs and by each mid-session change.
- [ ] **W1.5 NVDEC**: planes, then handles through the vendor's interop with D3D11, the copy's
  completion signalled on the library's fence rather than waited for. Checked by the clips
  and ten minutes.
- [ ] **W1.6 the system's decoder**, software only. Checked by the clips it decodes.
- [ ] **W1.7 the demo**: the GPU choice and its menu, the feature reports, the Sony pads,
  the check of the display's GPU. Checked by both Sony pads against an established host.
- [ ] **W1.8 packaging**: the zip from the build workflow; CI decodes the clips through a
  downloaded LGPL pair, since its runner has no GPU, and builds the demo. Checked by the
  artifact, built and unpacked.

**Gate:** Gate C ([impl-plan-client.md](impl-plan-client.md)) on this machine, against two
established hosts:

- ten minutes each through D3D11 on each of the three GPUs, NVDEC, the pair and the system's
  decoder, each on its default kind and with both codecs, against a host that sends neither
  ten-bit nor full chroma;
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

- 2026-09-26: W1 planned at its interview.
- 2026-09-26: planned; W0 built, W0.1 to W0.6.
