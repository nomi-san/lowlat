# Implementation plan: the client

**Status:** locked 2026-09-15, interview of the same day. Phases C0 to C6 with verification
gates; the design is [10-client.md](10-client.md) and the surface is [06 §3b](06-api.md).

Conventions as [impl-plan.md](impl-plan.md): a gate is a command that passes or a peer that
streams, one phase per commit, changelog entry before the checkbox. Phase numbers are `C`
so references into the host plan keep resolving.

## The gate that matters

**Gate C: the C demo, on the application toolkit it will ship with, streams from an
established host it does not control.** Picture first; then input; then sound; then the rest.
It is Gate A's mirror. Everything before it is unverified in the only way that counts, and it
is passed **at every phase from C2 on** rather than once at the end: each phase adds a feature
and re-runs the same ten minutes against the same host, so what ships after each phase is a
client that connects to a real host with the features it has so far.

Two peers serve it. **This host**, on the development machine, is the first: it is the
strongest reference this project has for what a host does, it runs beside the client, and
under the simulator it makes the whole session hermetic -- the test peer the host plan has
listed as a prerequisite since Phase 0. **An established host** on the second machine is the
one that counts.

## Decisions taken at the interview

Recorded once, here; the reasoning is in [10-client.md](10-client.md) and [00 D14](00-overview.md).

- One library and one header, the client half beside the host half as a feature, with its own
  handle type (`lowlat_client`) so a host call on a client handle does not compile. **Done
  in C0.**
- Pictures leave the library by **acquire and release**, at most two held, with an optional
  fence on release; acquire is the poll. Planes or a device handle, the library saying which.
- **Decoders: VA-API first, then NVDEC**, both loaded at runtime. No Vulkan Video decode.
  **Software decode is deferred**, with its licence question attached; not in v1.
- The library decodes sound to PCM and hands it out through the playback window; **the
  application owns the audio device.** Input is encoded by the library from the application's
  events.
- **Signaling stays out of the library** (D3). The demo speaks the signaling service itself
  over the toolkit's WebSocket and JSON, in one file.
- **One stream on channel 1, the native transport only.** No second stream, no browser pipe.

## Phase C0 - The boundary in two halves (closed 2026-09-15)

- [x] `lowlat-sdk` builds the shared object and the header; `lowlat-host` is the host beneath
  it. Features `host` and `client`; the header's guards generated from them, opt-out;
  `lowlat_features()`; the handle renamed `lowlat_host`. Minor 3.

**Gate:** the ABI gate passes with both halves; `--no-default-features --features client`
builds a library of the shared entry points alone; the gate's harness names a half-built
object. *Passed 2026-09-15.*

## Phase C1 - The client core, and a hermetic session (closed 2026-09-17)

The connecting side of everything below the media, written against the core that already
plays both roles.

- [x] `lowlat-client`: the client session -- the offering side of connectivity (one socket,
  the offer's credentials and certificate digest produced for the application, candidates
  out as events, the answer's credentials in, the session keyed from the answer under either
  cipher with a setting that asks for the legacy one), the session's receive half with the
  video ring at 4000 fragments and an access-unit buffer sized from it (the ring bounds a
  message at its depth times a fragment's body, about 4.8 MB; nothing larger can ever
  complete, so the 16 MiB an established client allocates is room nothing fills), the
  initialization of [01 §11.5](01-protocol.md) with fourteen keys, the diagnostics message,
  opcode 13 for the two secondary streams (stream 0 declares through the initialization),
  and the control vocabulary of [10 §7](10-client.md) as events.
- [x] The **catch-up over arrived messages** ([10 §3](10-client.md)), on the receive thread,
  which owns the ring: keyframe metadata found ahead is skipped to; nothing is ever skipped
  over a gap; and nothing is skipped when no keyframe is ahead, so a slow reader against a
  host without periodic keyframes decodes in order. Named regression tests for the last two.
  The reader's lag -- messages behind, and how long there has been anything unconsumed -- is
  a metric from here, because the deferred decisions are decided on it. The hermetic census
  counts a picture the catch-up discarded as skipped, not lost.
- [x] The **keyframe policy** ([10 §5](10-client.md)) as a state machine over a decoder
  interface, with only a test fake behind it until C2: two triggers, no timer, no start-up
  kick, one request per fault paired with the teardown. Named regression tests that a decoder
  starved of a keyframe never fires a request, that a fault fires one immediately, and that a
  burst of bad units after it fires no second one until a decoder exists again; also the
  announced bit, the stale generation, the rebuild bit and the format-change re-feed.
- [x] The C ABI: `lowlat_client_create/destroy`, the four-call seam mirrored, events, status,
  user data out. The seam's types leave the host's guard for one both halves share; the
  client half of the header behind `LOWLAT_CLIENT`. Minor 4. Pictures, sound, input, the
  video configuration and the metrics panel arrive with their phases.
- [x] **A hermetic full session under the simulator**: this host's own framing and
  negotiation against this client's driver, fake clock, scripted loss, reorder and
  duplication, sound and a synthetic picture from the host, the client reporting what it
  received. The picture is not decoded here -- there is no decoder yet -- so the check is on
  access units: every one the host sent arrives whole, in order, keyframes where the host
  said. And the real threads against this host's own admission over loopback, under both
  ciphers.

**Gate:**

1. The workspace tests, the lints, the dependency policy and the ABI gate pass; the zero
   allocation checks on the native transport still read zero, from the client's side too.
   *Passed 2026-09-17.*
2. The hermetic session runs clean at zero loss, one percent loss, and five milliseconds of
   reorder, and the client's control census matches the host's message for message. *Passed
   2026-09-17, thirty simulated seconds each; the lossy two also at three hundred.*
3. `lowlat_features()` reports both halves; the header compiles alone with `LOWLAT_NO_HOST`
   and with `LOWLAT_NO_CLIENT`. *Passed 2026-09-17.*

## Phase C2 - A picture from a real host (closed 2026-09-17)

- [x] `lowlat-decode`: the decoder trait and the **VA-API backend**, and beneath it **the
  library's own reading of both bitstreams** -- the device interfaces here decode a picture
  from its parameters and its slices, so the parameter sets, the slice headers, the picture
  order, the reference lists and the picture buffer that holds them are the library's job,
  in full syntax for H.264 (fields and MBAFF, B slices, reference-list modification,
  weighted prediction, the marking process with long-term references, gaps in the frame
  count) and HEVC (short- and long-term reference sets, dependent slices, tiles and
  wavefront entry points, the leading pictures dropped after a stream-starting random
  access point); eight and ten bit; planes by read-back. The driver's interface is loaded at
  runtime through `lowlat-drivers`, one crate for every interface reached that way, shared
  with the encoders. Every committed clip -- three from this host's synthetic source and
  fifteen from two other encoders at 128 and 256 square, B pyramids, MBAFF, CAVLC, slices,
  scaling lists, ten bit -- decodes bit-exact against an independent decoder's per-picture
  checksums; both readers are fuzzed, with two crash inputs kept as regression tests.
- [x] The **frame queue** of [10 §4](10-client.md): four slots, latest wins, the producer
  steals the oldest ready slot and never a held one, `acquire_frame` / `release_frame` with
  the two-held rule and the fence (planes only in this minor, so the fence is null-only).
  The slots are sized at the configuration's ceiling and backed on the decode thread at the
  first picture, demand-zero; each picture is laid out at its own pitch. The ring is model
  checked; named regression tests: the producer never blocks without a consumer; a held
  slot is never overwritten; a third acquire is refused; a held slot keeps its layout.
- [x] The decoder built from the stream ([10 §5](10-client.md)) on **the decode thread**,
  which takes access units from the receive thread's pool, runs the policy of C1 over the
  real backend, publishes pictures into the queue, and carries the one keyframe request to
  the session thread. The decoder is opened at creation, so a machine without one is refused
  there with the stage named; a client with nowhere to draw may ask for none.
- [x] **`examples/client`**: the C demo on the toolkit, one file for signaling, one for the
  session; window, present, nothing else, and a line of figures a second. It logs in with
  the same tool the host uses.

**Gate:**

1. **The demo shows this host's desktop**, on this machine, through VA-API on the second card,
   at the display's rate for ten minutes; decode time and queue depth on the log, and the
   presentation cadence recorded as numbers rather than judged: repeats and skips per second
   between consecutive presents with the stream at the display's rate, above it and below
   it, and the lag the picture reaches with the decoder slowed to half the stream's rate
   under the simulator ([10 §4.1](10-client.md)). The deferred decisions below wait on them.
   *Passed 2026-09-17, on the open-stack decoder of the second card, 1080p H.264 at 120
   pictures a second, the desktop moving independently of the demo's own window: ten
   minutes at the display's rate with 120.6 pictures decoded a second, decode 2.0 ms at the
   median and 2.3 at the ninety-fifth percentile, read-back 2.0 and 2.2, the queue at one,
   the reader at most one message behind (8 ms at the ninety-fifth percentile, 25 at most),
   6.2 Mbit/s, 208 MB resident, one decoder build and no keyframe request. Cadence, per
   second between consecutive presents: at the display's rate 118 new pictures, 3 repeats
   and 3 skips at the median (11 and 11 at the ninety-fifth percentile) -- the beat of two
   unsynchronised clocks; with the stream below the display's rate (60 against 120) 60
   pictures, 60 repeats, no skips; with the stream above the presentation rate (120 against
   a 60-a-second poll) 60 pictures, 60 skips, no repeats. The half-rate lag under the
   simulator: 159 messages at the deepest, 611 pictures discarded by the catch-up over 1317
   frames at a keyframe every 300.*
2. **The demo shows an established host's desktop** on the second machine for ten minutes from
   a cold connect, with the initialization and the per-stream declaration accepted as the
   established client's are, and a clean departure read as such on both sides.
   *Passed 2026-09-17: ten minutes from a cold connect to an established host on the second
   machine, over the wide area under the current cipher, the initialization accepted with
   the video protocol honoured (the host's own log reads the guest as connected with
   `vp_supported = 1` and the encoder built for it), one decoder build over the ten minutes,
   the host's encode time read at 3.7 ms, 30 pictures a second from a mostly still desktop,
   and the departure read as a clean disconnect on both logs. The process map during the
   run held 89 libraries and no copyleft codec. The first offer was refused outright: an
   established host requires the offer's `mode` ([04 §4](04-signaling.md)).*
3. A keyframe with unchanged parameter sets from a host that marks it does not rebuild the
   decoder; from a host that does not, it does, and the picture continues. *Passed
   2026-09-17: one build over the ten minutes of item 1, this host marking every keyframe;
   the other half hermetically, this host's own framing with the marking off against the
   real decoder -- a rebuild per keyframe, 477 pictures across four of them, every one the
   reference decoder's.*
4. The hermetic session of C1 now decodes: the picture out matches the synthetic picture in,
   frame for frame, at zero loss. *Passed 2026-09-17: 477 pictures, three loops of the clip
   with two announced keyframes, every picture the reference decoder's, one decoder build.*

## Phase C3 - Input (closed 2026-09-18)

**Planned 2026-09-18, interview of the same day.** The decisions are recorded once, here;
the rules are [10 §8](10-client.md).

- [x] `lowlat_client_set_viewport(x, y, w, h)`: the rectangle the application drew the
  picture into, in the same units as the positions it reports. The library never computes a
  fit: stretch, shrink, a percent scale and rotation are all the application's ways of
  producing one rectangle, and DPI does not enter because the rectangle and the positions
  share a space by construction. A zero rectangle means no picture area, and absolute motion
  is not sent until one is set; the picture's size comes from the stream's own header, so
  the two ends of the ratio come from different owners and the application cannot describe
  the picture wrongly.
- [x] The `lowlat_client_send_*` calls, one per kind (*a tagged structure until C3.5, split so a call site is checked where it is written*), and the rules of [10 §8](10-client.md): the transform into
  the picture's pixels with the edge bump and the clamp, the rotation swapped back, relative
  deltas scaled by the picture-to-drawn ratio, the press-outside guard evaluated at the
  press's own position, the keyboard code guard, pad state deduplicated per identifier,
  release-all on the application's word. Keyboard codes are usage codes and the modifier
  mask is the event's own, in the wire's bit numbering (`LOWLAT_MOD_*`); the application
  supplies both, because only its toolkit knows the lock state. The path from the handle to
  the session thread is a fixed ring of 1024 entries; a full ring drops the newest message
  and counts it in status, and never blocks the application's thread. Every push wakes the
  loop: a wake only when the ring was empty loses the one that lands between the consumer's
  last pop and its sleep. Minor 6.
- [x] The relative-mode event on the cursor message's transition (either the relative or
  the hidden bit), carrying the position to warp to on the way out, in window coordinates
  through the inverse of the same viewport mapping. The cursor body is read for that alone
  here; the image, the hotspot and the suppressed flag are C5's.
- [x] The demo takes keyboard, mouse and pad from the toolkit and sends them: the key table
  generated from the toolkit's own map crossed with the kernel's usage table (two
  directions, so a disagreement shows); a bare GUI key is dropped, as a chord modifier it
  is sent; repeats are forwarded as presses; both attached pads as the standard state,
  unplug on removal. Ctrl+Alt chords are the demo's own: stretch or shrink (the rectangle
  re-sent), letting go of the pointer, and cycling the streamed output through the
  application protocol -- ids 10 and 9 asked, the answers 12 and 11 read from the user-data
  events, 11 sent back with the next `output` in the host's own configuration, whole, as a
  host reads it. **Presenting is on a thread of its own, paced by the display; the
  toolkit's event loop runs at its own cadence** (*added at the gate*): the toolkit reads
  one pad event per pass of its loop, so a loop bound to the display's rate drained a
  moving stick slower than it moved and the kernel's queue played on for seconds after the
  hand stopped. The shape every established client has, and the shape sound will take.
- [x] Relative mode in the demo: confine and hide on the event, warp on the way out if
  focused.

**Gate:**

1. Against an established host with two monitors, one of them streamed: typing lands, the
   pointer lands where it is aimed on whichever output is streamed, switched from the demo
   both ways; a drag that leaves the window releases on the host; each of the two attached
   pads drives a game; mouselook works through relative mode. *Passed 2026-09-18, the
   Windows host on the second machine over the wide area (8 ms round trip): typing, aiming
   on both outputs through the chord (`803140-2315383105` and `803140-4239277026`, each way
   twice), aiming stretched and at the picture's own size, the drag out of the window, both
   pads in a game, and mouselook entered and left seven times in the first run -- the host
   captures the pointer for a window drag too, which the toolkit reports as relative motion.
   The first pad run found the event-loop cadence above: the sticks lagged and drained for
   seconds; with presenting on its own thread the toolkit delivered up to 610 pad reports
   a second and none were late. The stick's vertical sign is the wire's up-is-positive, read
   live: a stick pushed down arrived from the toolkit as +32767 and left as -32767.*
2. Against this host: the host's own input log agrees with the demo's, message for message,
   for one minute of mixed input; the census shows every opcode the established client sends
   and nothing it does not. *Passed 2026-09-18, sixty seconds of scripted keys, clicks, wheel,
   motion and a drag out of the window: keys 321 = 321, buttons 150 = 150, wheel 30 = 30,
   pad states 56 handed over and 55 received (one unchanged state not repeated), and motion
   340 handed over against 150 received, the 190 being the second before the first picture,
   when the rule drops absolute motion -- from the first picture on the two lines agree
   second by second. The host's census: init, diagnostics, the encoder configuration, then
   keyboard, mouse button, wheel, motion, gamepad state and release, and nothing else.*

**Not in C3, recorded here so it is not re-decided:** the DualSense pair -- touchpad
contacts, motion, lightbar, adaptive triggers and haptics -- is its own phase after C5 on
both plans, host `uhid` backend first, and the touchpad already has a wire the client will
have to learn there; a Unicode key message for an input method needs a host half that is
not a key injection, and is owed with it; pen and touch stay deferred.

## Phase C4 - Sound (built 2026-09-18; gate 1 owed)

**Planned 2026-09-18, interview of the same day.** The decisions are recorded once, here;
the rules are [10 §6](10-client.md).

- [x] **The library orders and decodes; the device paces.** No playback window in the
  library (*an earlier draft had one, with a 40 ms cap and a flush at either edge*): every
  reference keeps the window in the application's device, where the clock is, and a second
  window over it would only flush against it. The receive loop stamps each packet and parks
  it in a pool of 32; a full pool drops the newest and counts it. **Decode on acquire, no
  sound thread**: `lowlat_client_acquire_audio` takes the next packet in order and decodes it
  on the caller's thread into the caller's buffer -- one packet a call, stereo at 48 kHz, up
  to 8000 frames (the uncompressed ceiling; the 40 ms figure was one client build's slot),
  the need reported and the packet kept when the buffer is short, the packet's age at
  hand-over in status. The decoder is the one the host reads a microphone through,
  generalised over channels and capacity, contained and rebuilt after a caught panic, fuzzed
  with a stereo decoder beside the mono one (the target's own panic hook had aborted before
  unwinding, so it had never run); a stream that is not stereo at the protocol's rate is
  refused per packet; a change of mask, codec or channel count rebuilds. Minor 7.
- [x] The demo plays it through the toolkit's device at the desktop client's window, 75 ms
  to 150, from a listening thread beside the presenter; a resync is read from the device's
  own queue and logged the moment it is seen; the second's line carries the packets, the
  device's queue, the packet age, the pool's drops and the decoder's refusals; a knob traces
  every packet, another asks for uncompressed. Against a host on the same machine the demo
  plays into a sink the host does not capture, or it echoes.
- [x] The microphone the other way is **deferred**: the enable and the uplink are known, the
  capture device is the application's, and nothing in v1 needs it.

**Gate:**

1. Thirty minutes of sound from an established host with no audible gap and at most one
   resync, then the same from this host. *The resync figure is read from the run, not
   picked: the demo logs the device's queue every second and its slope is the drift; the
   desktop window drifts 75 ms to an edge, which is 25 minutes at 50 ppm and 12.5 at 100,
   so if the crystals give two the gate's number follows the crystals.* **Owed:** the user
   drives the run.
2. The hermetic session carries sound both codecs and the samples out match the samples in
   (*"both ways" was the wording; the uplink is deferred in the same phase*). *Passed
   2026-09-18: the harness host encodes a real stereo tone at 20 ms and sends it
   uncompressed in a second configuration; every packet acquired, none dropped or refused,
   at zero loss, one percent loss and five milliseconds of reorder over thirty simulated
   seconds; uncompressed sound equal sample for sample across the run (three hundred seconds
   too), compressed sound at each channel's level over the last second. A smoke run against
   this host: 50 packets a second, the age between the wire and the call 0 to 1 ms, the
   device's queue 56 to 76 ms, no drop, refusal or resync in twenty-five seconds.*

## Phase C5 - The rest of the client, and NVDEC

- [ ] The cursor: image, hotspot scaled into the window, the suppressed flag; the demo sets the
  toolkit's cursor from it.
- [ ] User data both ways, the guest list read for the client's own permissions, host mode,
  stream ended, blocked, rumble; status and metrics with the named channels.
- [ ] Opcode 21 every two seconds with the decode time.
- [ ] **NVDEC**: the second backend, on the first card, planes first; the handle path on
  whichever backend exports first.
- [ ] Ten-bit and 4:4:4 declared when the decoder has them, following what the stream turns
  out to be ([05 §6.1](05-host.md) from the other side).

**Gate:**

1. Gate C in full: ten minutes against an established host with picture, sound, input, the
   cursor and relative mode, from a cold connect, on both backends; then the same against
   this host with its ten-bit and 4:4:4 streams.
2. The demo's panel and this host's roster agree on the figures they share.

## Phase C6 - Packaging and the second half of the header

- [ ] The client-only build in the build workflow; the SDK tarball says which halves it
  carries; the demo built and kept as an artifact.
- [ ] Documentation closure: [06 §3b](06-api.md) verified against the header, [10](10-client.md)
  against the code, [09](09-compatibility.md) gains what decodes on which part.

**Gate:** the workflow produces the full library, the client-only library and the demo, and
`lowlat_features()` on each says what it is.

## Later, and not in v1

- **A Windows client**: the completion-port receive path in the shell ([02 §6](02-io-shell.md)),
  Media Foundation or NVDEC, shared textures with fences as the handle kind, the toolkit's
  D3D11 renderer. Its own phase when Linux is done; the design already names its handle.
- **Software decode**: a decision, not a phase, and the licence question decides it
  ([09 §9](09-compatibility.md)). Until then a machine without a hardware decoder is refused
  with the stage named.
- **A second stream, the browser pipe, pen and touch, the microphone uplink.** Each is known
  and none is needed for a client that streams.

### Deferred decisions, recorded 2026-09-16

Each is written up in [10-client.md](10-client.md) and decided on the numbers the C2 gate
records, not before; none is in v1. **The numbers are recorded** (2026-09-17, in 10 §4.1, §5
and §9); the decisions are taken at C5's planning, where the surface two of them need is
built.

- **A decode-lag keyframe request** ([10 §5](10-client.md)): a third trigger for the request,
  when the reader is behind by more than a threshold with no announced keyframe ahead, rate
  limited; the catch-up then lands on it. A divergence from the established client, correct
  on a reliable channel, an encoder rebuild every two seconds on the established host while
  the lag lasts. Thresholds measured, not picked.
- **A presentation-rate hint and a sustainability event** ([10 §9](10-client.md)): the
  application's display rate in `lowlat_client_set_config`, the sustainable rate in status,
  an event when the decoder cannot keep up with the rate the library recommends; the
  application relays it as `encoderFPS` in its own protocol, which is the one lever every
  host honours. The library never sends that message.
- **A presentation pacer in the application** ([10 §4.1](10-client.md)): a deeper hold count
  on acquire for an application that wants evenness over currency, as Moonlight offers
  pacing as an option beside a client-set frame rate. The library stays latest-wins.
- **Temporal layering on the host** is the only per-guest frame-rate lever, and belongs to a
  host phase; noted here because every client-side answer above ends at it.

## Change log

Newest first.

- 2026-09-18, evening: C4 planned and built. The playback window leaves the library for the
  application's device, where every reference keeps it and where the clock is; sound is
  decoded on the application's call rather than on a thread of its own; the pool drops and
  counts; the demo reads resyncs from its device's own queue and logs them at once; "both
  ways" in the hermetic gate is read as both codecs, the uplink being deferred in the same
  phase. Gate 2 passed; gate 1 is the user's thirty minutes.
- 2026-09-18, later: C3 closed. The gate found the demo's event loop bound to the display,
  which is the wrong cadence for a toolkit that reads one pad event a pass; presenting moved
  to its own thread, as every established client has it. The wire's vertical stick sign was
  read live rather than argued.
- 2026-09-18: C3 planned. The application hands over the rectangle it drew into and the
  library computes no fit; keys are usage codes with the event's own modifier mask; the
  input ring drops and counts rather than blocks; the relative-mode event carries the warp
  position in window coordinates; the two-monitor gate item is driven from the demo through
  the application protocol; the DualSense pair and a Unicode key message are recorded as
  later, not lost.
- 2026-09-17, later: C2 closed. The library reads both bitstreams itself in full syntax and
  hands the device the picture and slice parameters, because the interfaces here decode from
  those and the licence rule keeps every other reader out; the clips are checked against an
  independent decoder, never against ourselves; the slots are sized at the ceiling, backed at
  the first picture and laid out at the picture's own pitch; the read-back's cost is the
  driver's mapping and was measured rather than optimised; the gate's cadence figures were
  taken with the demo's own knobs on one display, the old-framing half of item 3 hermetically,
  and an established host turned out to require the offer's `mode`. The deferred decisions
  carry their numbers and are decided at C5's planning.
- 2026-09-17: C1 closed. The receive thread owns the ring and therefore the catch-up; the
  access-unit buffer is sized from the ring rather than at 16 MiB; the keyframe policy is a
  state machine tested against a fake until C2 brings a decoder; both ciphers, with a setting
  for the legacy one; no C# client, ever -- the demo is C on the toolkit.
- 2026-09-16: the keyframe request is one act with the teardown, stream 0 declares through
  the initialization, C2 records the cadence and lag numbers, and four deferred decisions are
  written down with what decides each.
- 2026-09-15: the plan is written, after C0 landed the same day.
