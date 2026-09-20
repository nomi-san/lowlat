# Implementation plan: the client

**Status:** locked 2026-09-15, interview of the same day; C5 re-planned in two halves
2026-09-19 and its decode half built and gated the same day. Phases C0 to C6 with verification gates; the design is [10-client.md](10-client.md) and the surface is [06 §3b](06-api.md).

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
- The library decodes sound to PCM and hands it out packet by packet; **the application owns
  the audio device**, whose buffer is the playback window (*C4 moved the window there from
  the library*). Input is encoded by the library from the application's events.
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
   *Corrected 2026-09-19, at C5's second half: that trace was read backwards. The toolkit
   already hands every stick over up-positive, so the demo's negation put down-positive on
   the wire and a game on the established host looked the wrong way up; the values pass
   through now, as every established client passes them.*
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
not a key injection, and is owed with it; pen and touch stay deferred. *Planned 2026-09-20
as C7 below with the host's Phase 14; the wire turned out to be whole, and the client half
goes first.*

## Phase C4 - Sound (closed 2026-09-18)

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
   so if the crystals give two the gate's number follows the crystals.* *Passed 2026-09-18
   against the Windows host on the second machine: eighty-five minutes with sound playing
   there, 254,178 packets at 50 a second, none dropped by the pool and none refused by the
   decoder, one decoder build, the packet's age between the wire and the call 0 ms at the
   median and 1 at most, the round trip 7 to 9 ms. The device's queue climbed at 39.3, 39.6
   and 40.3 ppm in the three stretches between flushes -- the two clocks' difference, read
   from the run -- and crossed the ceiling twice, at 1823 s and 3930 s: one resync per 35
   minutes, at most one in any thirty, both of them the speaker's clock running behind the
   host's. The first eleven seconds carried a seven-second hole in the host's own sound at
   the session's start, which ran the device dry once; that is the host starting, not
   drift. The run also found the demo counting one flush three times (the device's queue
   reads zero once more after playback restarts), fixed after it. From this host, thirty
   minutes the same day: 89,941 packets, none dropped or refused, the age 0 to 1 ms, one
   build, and one resync at 1710 s -- the device running dry. But that figure is the rig's,
   not the clocks': the demo on the same machine as the host has to play into a sink the
   host does not capture, so its device was a null sink on a software timer while the host
   read the sound card, and the queue wandered by fifteen milliseconds either way before it
   emptied. The run passes the gate's letter and measures nothing about drift; the
   established host's run is the one that does.*
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

**Planned 2026-09-19, interview of the same day, in two halves.** The decode half is planned
here and **built and gated the same day**, with two rows left open on the host's side; the
second half was planned the same evening, at its own interview, once the first had closed.
The decisions are recorded once, here; the rules are [10 §4](10-client.md), §5.1, §7 and §9.

### C5, first half: the decode time reported, the second backend, ten-bit and 4:4:4

- [x] **Opcode 21 out**, both kinds, on one two-second tick of the session thread from the
  moment the session is established: the video kind with the decode thread's smoothed figure
  for decode and hand-over per picture, the sound kind with the figure `acquire_audio`
  smooths on the application's thread, both zero until something has been timed and sent
  anyway, as this host sends its own. A time cadence rather than a count of pictures, so a
  still desktop still reports; the same message is what keeps the round-trip estimate
  alive, because a sample is taken only when the host acknowledges something this client
  sent.
- [x] **The declaration is a preference masked by capability.** The application names what it
  would like -- the second codec, ten-bit colour, full chroma -- in the video block of the
  attempt's configuration, one block for the one stream; the library ANDs that with what the
  decoder it opened at creation decodes and declares the result, with the wire's own
  implication (depth and chroma imply the second codec, and neither is declared without it).
  Defaults off, so a client of ours at its defaults asks a host for exactly what every
  established client asks at its defaults. `lowlat_client_set_video_config` changes it
  mid-session: the new flags go out as the encoder configuration with the reinitialisation
  argument and the decoder is torn down with it -- the first of the two request cases of
  [10 §5](10-client.md), which the state machine has had since C1. Status carries what was
  asked, what was declared and what the stream turned out to be. **One decoder is chosen at
  creation and there is no fallback to another**: a stream the built decoder cannot take,
  which can only be one the client did not declare, ends the session with the decoder's
  status and the stage named, never a quiet switch to a slower path. Minor 8.
- [x] **Full chroma in the readers and three planes out.** The HEVC reader admits the
  range-extensions profile at 4:2:0 and 4:4:4, eight and ten bits, and reads both extension
  syntaxes, which the devices' picture parameters carry; H.264 stays at eight-bit 4:2:0,
  which is all any device decodes. Two planar layouts join the two that exist, with the
  third plane already in the picture's shape; the slots' ceiling layout becomes the deepest
  of the four. On the open-stack backend the profile is asked for and refused where the
  device lacks it -- every device here -- so no read-back is written for a surface layout
  nothing here can verify. The clips: two from this host's synthetic source through its
  vendor encoder at 4:4:4, eight and ten bits, and three from an independent encoder at 128
  square with the extension syntax exercised, each checked against the independent decoder's
  per-picture sums as every clip is.
- [x] **NVDEC, driven from the library's own readers.** The second backend fills the vendor
  interface's picture and slice parameters from the same jobs the open-stack backend stages
  and submits a picture at a time; nothing in the interface's own parser is used, so there
  is one reader, one picture buffer and one reordering rule for both backends. Read-back is
  a device-to-host copy of the mapped picture. **The creation-time probe builds and destroys
  a real decoder per combination of codec, chroma and depth** rather than trusting the
  interface's capability query, which has reported a combination the device then failed to
  create; a device that fails does so at creation with the stage named, not mid-stream in
  the application's process. The device is named as a render node for both backends; for
  the vendor's it is resolved to the card behind it. Automatic selection takes the first
  node the open stack decodes on and the vendor interface only where there is none, so the
  vendor backend is chosen by index where both exist.
- [x] **The handle path, on NVDEC first.** A decoded picture on that interface is not
  exportable -- its pool is internal and a mapped picture is a transient pointer -- so the
  four slots become exportable device allocations, one file descriptor each, and the backend
  does one device-side copy into the slot in place of the host read-back; the ring, the
  latest-wins rule and the two-held rule are unchanged. The frame carries the descriptor and
  its kind (an opaque descriptor now; a buffer descriptor with its layout modifier is the
  open stack's kind and is later), with an offset and pitch per plane; acquire returns after
  the copy has completed, so a null fence stays correct and a real fence is a refinement.
  **Device slots are sized at the stream's size at the decoder's build, never at the
  ceiling**: device memory is real, and full chroma at sixteen bits is 200 MB a slot at the
  ceiling; a build at a new size or layout allocates a fresh set with fresh descriptors, the
  descriptor keys the application's import, and the old set is freed after its last hold is
  released. A spike precedes the wiring: a descriptor exported from the device runtime and
  imported by the application toolkit's GL context on this machine; if the import is
  refused, the handle kind is not viable on that renderer and the phase falls back to planes
  with the finding recorded.
- [x] **The application toolkit becomes a vendored tree** rather than a submodule, so its GL
  renderer's existing hardware-frame hook can be implemented for the descriptor: imported
  once per slot, one texture per plane, sampled through the shaders it already has for the
  planar layouts. The demo asks for the handle kind with a knob, draws through the hook,
  releases after present, and reports the hand-over time per picture beside the decode time.
- [x] The demo takes the three preferences at start and cycles them live through a chord,
  shows asked / declared / decoded in its title bar and its second's line, and draws the
  warning of [10 §4.1](10-client.md) when the reader has been thirty or more messages behind
  for sixty consecutive seconds.

**Decided here, and not built (recorded 2026-09-16 as deferred, decided 2026-09-19):** the
decode-lag keyframe request, the presentation-rate hint with its sustainability event, and
the application pacer are **none of them in v1**. Every established client's whole remedy for
a decoder slower than the stream is a warning to the person, gated on the reader being thirty
or more messages behind for sixty consecutive samples and cleared the moment it drops under;
status already carries the figure, so the warning is the application's and the library adds
no mechanism. The gate below records the decode and hand-over time per backend and format at
the display's rate and the reader's lag, which is the record if this is ever reopened.

**Built 2026-09-19, deviations from the text above:** no 4:4:4 clip could be made from this
host's synthetic source (it is two-plane and the vendor encoder refuses to upload it to a
full-chroma session), so four full-chroma clips from two independent encoders stand in, and
the independent encoder's clips are 144 square rather than 128 because that is the vendor
decoder's floor. Device slots are allocated per slot as a picture of a new layout is about to
be decoded into it rather than as a set at the build, which frees each old allocation after
its last hold by the ring's own rule; and the frame carries the allocation's ordinal beside
the descriptor, because descriptor numbers are reused once closed and the ordinal is what
keys the application's import. The decoders this machine can open are also listed
(`lowlat_enum_decoders`, [06 §6](06-api.md)), asked for during the build. **Figures so far**
(this host, 2560x1440, 120 pictures a second): vendor backend decode 0.6 ms, read-back
0.5-0.9 ms, device copy 0.09 ms; open stack 2.1 and 2.0 ms; the renderer's fill from the
imported descriptor 0.05 ms for two planes; the read-back's jitter showed as five repeats
and five skips in some seconds on the vendor backend's planes path and none on the handle
path.

**Gate, first half:**

1. [x] Every committed clip decodes bit-exact on both backends, the new 4:4:4 clips included;
   the workspace's checks and the ABI gate pass; the hermetic session's census counts the
   client's latency reports, both kinds, at the cadence. (*2026-09-19*: twenty-one clips on
   each backend, the vendor's by both routes.)
2. [x] Against this host, ten minutes each with the desktop moving independently of the demo:
   H.264 on the vendor backend, planes then handle; ten-bit HEVC on both backends; full
   chroma at eight and ten bits on the vendor backend, planes then handle; and full chroma
   asked of this host on the head that cannot code it, where the stream degrades and the
   client follows without a decoder fault. Recorded per run: decode and hand-over per
   picture, pictures, repeats and skips a second, the reader's lag, the resident set and the
   device memory, and the reported decode time as this host's roster shows it.
   (*2026-09-19*, `local/logs/2026-09-19-c5-gate2-*`, three ten-minute runs with the
   preferences walked every hundred seconds, 2560x1440: vendor handle 120 pictures a
   second, decode 0.61-0.75 ms, device copy 0.09-0.14 ms, resident 400-507 MB; vendor
   planes read-back 0.53-1.05 ms; open stack decode 2.0/3.4 ms, read-back 1.8-2.4 ms; the
   reader at most one message behind everywhere; sound 50 packets a second, nothing
   dropped. Full chroma asked degraded to eight-bit HEVC and the client followed -- on the
   vendor's head by this host's census rather than on the other head, the same client path.
   **Real full chroma was not streamed**: it needs this host started by hand with the census
   opened. Twice this host's service answered a ten-bit reconfigure with -15000 and stayed
   down until restarted; diagnosed and fixed the same day on the host's side -- the runtime
   libraries were unloaded per build and the C library's static thread-local area ran out
   ([07 §8](07-platforms.md)) -- and the walk then ran forty switches without it. The gate
   also found the toolkit's renderer freezing the picture after a switch to ten bits, fixed
   in the vendored tree. The full-chroma row stays open on the host.)
3. [x] Against an established host: at the defaults as C2's gate ran, then with the second codec
   and ten-bit asked, following what its encoder gives; the round trip moves off its seed
   within seconds of connecting; its own log shows this client's decode latency.
   (*2026-09-19*, `local/logs/2026-09-19-c5-gate3-*`, over the internet at 8-11 ms: at the
   defaults 31 pictures a second (that host's own cadence on a still desktop), the open
   stack at 2.3/1.9 ms, the reader at most one message behind, the round trip live from the
   first second; with the second codec and ten-bit asked that host gives the codec and not
   the depth, and the client follows at eight bits with no fault, by planes and by handle;
   a walk of five switches against it, each followed by its encoder rebuilding. One run
   ended after 160 s as undeliverable when that host's path went quiet and its port
   changed; the rerun completed. What its log shows is read at that machine.)
4. [x] A preference changed mid-session costs one configuration message, one teardown and one
   build, and the picture continues. (Hermetically: one restatement, one request, one
   teardown, one build. *Live 2026-09-19*: twenty timed switches across four runs, every one
   answered, the picture back within the second -- about forty repeats, the keyframe asked
   for -- and the stream's format following the declaration each time.)

### C5, second half: the cursor, rumble, the guest list, the client's metrics

**Planned 2026-09-19, evening, interview of the same day.** The decisions are recorded once,
here; the rules are [10 §7](10-client.md) and §9, the surface [06 §3b](06-api.md).

- [x] **The cursor, decoded.** The pointer message's picture is inflated and unfiltered in
  the library and handed to the application as RGBA at its native size, with the hotspot
  in the picture's own pixels, the suppressed flag, and the position the pointer reappears
  at on the way out of relative mode, in window units. **The picture is delivered from a
  buffer the handle owns, valid until the next poll** -- not through the caller's body
  buffer, which would make every application size its scratch at the picture ceiling or
  drop the cursor when it does not, as the demo did on a body too large. The library keeps
  the cache its initialization declares: pictures by checksum, a hundred of them, forgotten
  when the host says so; a name the cache does not hold delivers the position and the flags
  without a picture, and counts; a name that carries no size takes the picture's size and
  hotspot from what was stored with it. The reader takes 8-bit RGB and RGBA,
  non-interlaced, up to 512 square -- 1 MiB decoded, the same ceiling an established
  client's buffer has -- and refuses anything else with the picture dropped and the rest of
  the update delivered; it is fuzzed, and the committed pictures decode byte for byte
  against an independent decoder's output. **Scaling the pointer is the application's.** An
  established client scales it by the viewport it drew into, or by its display's scale, so
  a pointer from a host at twice the scale shows at the size it has in the picture and
  shrinks with a letterboxed window; the library does not own the toolkit's cursor, and a
  display server draws a cursor at its native size, so the library hands over the picture
  and the application resamples picture and hotspot together (*an earlier draft had the
  hotspot "scaled into the window" by the library, which scales nothing*).
- [x] **Rumble** as an event: the pad the application named and the two motors as eight-bit
  values; the application scales them to its toolkit's range.
- [x] **The guest list is handed over, not parsed.** The library needs nothing from its
  body: its own number arrives beside it and goes into status, permissions gate nothing on
  this side because the host drops what it does not permit, and the figures in it are the
  application's panel. So it is an event carrying the recipient's number, with the body
  through the caller's buffer as an application message goes; the application finds itself
  by that number and drops a body it cannot read (*an earlier draft had the library parse it
  for the client's own permissions*).
- [x] **The client's own metrics**, in a shape of their own: per channel the fragments that
  arrived, those that arrived late -- behind a later one, which on this transport is a
  retransmission or a reorder -- duplicates, out-of-window drops, the negative
  acknowledgements sent, bytes and messages, and **a recent-loss figure**, late arrivals
  over arrivals per one-second sample averaged with a thirtieth's weight so it reads over
  about thirty seconds; per session the round trip and the connected time. The host's own
  figures for this guest reach the application through the guest list, so one panel's two
  sides are the host's `lowlat_metrics` and the client's `lowlat_client_metrics`, each
  measured where it can be (*an earlier draft put the client's figures on the host's
  structure, half of whose fields a receiver cannot measure*). Minor 9.
- [x] User data both ways, host mode, stream ended and blocked were built with the session
  (minor 4) and stay as they are.
- [x] The demo sets the toolkit's cursor from the picture, resampled with its hotspot by the
  drawn ratio; hides its pointer while the host's is suppressed; rumbles the pad the host
  named; parses the guest list with its toolkit, shows owner and permissions in the title
  and draws the host's figures for this guest on its line beside its own.

**Built 2026-09-19, evening, deviations from the text above:** the picture already delivered,
named or sent again, travels as its checksum alone -- the first live run against this host
showed it naming the same picture a dozen times a second, each a decode for nothing -- so
the application keeps the picture it was given and `image_update` says when there is a new
one. The pictures the reader is checked against are six an established host sent in a
recorded session (compressed, where this host's encoder writes stored blocks), against an
independent decoder's pixels; the fuzz target ran five minutes clean on them. The receive
ring's late-arrival count is what the loss figure reads; the recorded session's downlink,
replayed, shows none at all, which is what a clean path reads.

**Gate, second half:**

1. [x] Hermetic: the harness host sends a fresh picture, the same by name, a name after a
   forget, a hidden pointer, rumble, blocked and unblocked, host mode and a guest list; the
   events come out in order with the right bytes, the miss delivers the position alone, and
   under the simulator's one percent loss the recent-loss figure reads about a hundredth on
   the video channel after sixty simulated seconds and zero at zero loss. (*2026-09-19*:
   every event in order; the miss delivers the position alone; the recent loss on the video
   channel reads 0.3 to 2 times the link's one percent after thirty simulated seconds, zero
   and no negative on a clean link, late arrivals without negatives under reorder alone.)
2. [ ] Gate C in full: ten minutes against an established host with picture, sound, input, the
   cursor and relative mode, from a cold connect, on both backends; then the same against
   this host with its ten-bit and 4:4:4 streams. (*2026-09-19, the unattended half*,
   `local/logs/2026-09-19-c5-gateC-*`: against the established host over the internet at
   8-12 ms, from a cold connect, the vendor backend by handles for 599 s and the open stack
   for 502 s -- the second ended from outside, not by a fault -- a mostly still desktop at
   14-16 pictures a second and bursts to 113; decode 0.63 ms by the vendor's route and 2.1 ms
   plus 2.0 of read-back by the open stack's; the reader at most one message behind; the
   guest list every two seconds with this client its owner; 152 and 63 pointer pictures
   decoded, none refused, no name missed; a rumble message from that host's game reached the
   event. Against this host at ten-bit HEVC, vendor handles, 599 s with the desktop moving:
   112 pictures a second at the median, decode 0.74 ms, device copy 0.13 ms, 323 pointer
   pictures, 293 guest lists, no late arrival. **The interactive items are owed at the
   desk**: typing, mouselook in and out, a drag out, the pointer changing shape under the
   pointer, rumble from a game on both backends, the output cycle; this host's rumble probe
   and its 4:4:4 stream on the by-hand host. The first desk session found two things the
   unattended runs could not: the demo's sticks were upside down (its negation of the
   toolkit's already-inverted vertical axes, since C3, corrected), and a pointer resampled
   at a two percent shrink lost a row to nearest neighbour and went soft under a box filter,
   where an established client stays sharp. Its rule is the demo's now: the target is the
   picture's size times the drawn-to-stream ratio per axis, a target within two pixels of
   the native size or of exactly half of it snaps there, and anything else is halved by box
   averaging while at least twice the target and then resampled bilinearly. The toolkit's
   own cursor-size call is empty on this platform and on Windows, so the scaling is the
   application's everywhere.)
3. [x] The demo's panel and this host's roster agree on the figures they share: the round
   trip, the rate, the decode time the host re-publishes; the negatives this client sent
   against the fragments the host resent on them. (*2026-09-19*: on the same line, this
   host's re-published decode figure 0.86-0.87 ms against the client's reported 0.86; its
   encode 3.98 ms against `encode_us` 3.9-4.2; its round trip 0.0-0.1 ms against ours 0;
   the established host's 10.5 ms against ours 9, its 0.75 ms decode against our 0.76
   reported, its 3.72 encode against `encode_us` 3.7. **The resends pair loosely by
   construction**: over the internet the established host resent 262 fragments on our
   negatives while this side counted 124 negatives sent, 123 late arrivals and 233
   duplicates -- a lost fragment costs one late arrival and several duplicates, because the
   host resends from the cumulative acknowledgement up to the named fragment, and one
   negative can name several. The recent-loss figure peaked at 1.3 percent in a burst and
   read zero for most of the run.)

## Phase C6 - Packaging and the second half of the header

- [ ] The client-only build in the build workflow; the SDK tarball says which halves it
  carries; the demo built and kept as an artifact.
- [ ] Documentation closure: [06 §3b](06-api.md) verified against the header, [10](10-client.md)
  against the code, [09](09-compatibility.md) gains what decodes on which part.

**Gate:** the workflow produces the full library, the client-only library and the demo, and
`lowlat_features()` on each says what it is.

## Phase C7 - The pad reports: DualShock 4 and DualSense (planned 2026-09-20)

**Planned 2026-09-20 with the host's Phase 14** ([impl-plan.md](impl-plan.md)), interview of
the same day. Executed **before C6** and after C5's owed desk items: packaging the demo and
the header before this phase changes both would be done twice. The decisions are recorded
once, here and under Phase 14; the rules are [10 §8](10-client.md), the wire
[01 §11](01-protocol.md), the surface [06 §3b](06-api.md).

- [x] **One call, one event** (minor 10): `lowlat_client_send_pad_report(cl, pad, type, kind,
  report, len)` with the product named and the report as the device delivered it, USB or
  Bluetooth form; `LOWLAT_EVENT_PAD_REPORT` with what the host's device was written, lent
  until the next poll, in the pad's own framing. `send_pad_state`, `button` and `axis` stay
  the sixteen-button pad's, and a pad identifier is one family until it is unplugged.
  *C7.1, 2026-09-20. The family is recorded with the attempt on the application's thread, so
  the other family is refused at the call rather than dropped on the session thread; the
  enumerations are `LOWLAT_PAD_TYPE_*` and `LOWLAT_PAD_REPORT_*`, the shorter names the plan
  used having collided with the button indices' `LOWLAT_PAD_*`.*
- [x] **The library derives the standard state from the report and sends it beside it**, the
  report first, deduplicated, so a host that does not read reports still has a pad and one
  that does has its slot; a DualShock 4 also travels as the ten-byte touch block an
  established host's DualShock mode reads, and its whole report goes without its identifier
  byte so no established host in any mode can mistake it for a DualSense's
  ([01 §11.1](01-protocol.md)). *C7.1.*
- [x] **Feature reports first, bounded**: calibration and firmware for each product, any
  subset, before the first input report; anything else refused. The pairing report is the
  host's. *C7.1; the normalisation runs on the application's thread, where the refusal is
  answered.*
- [x] **Bluetooth normalised in the library**: the wireless framing stripped on the way in, the
  transport remembered per pad, the identifier and checksum put back on the way out; the
  calibration report's wireless identifier rewritten. *C7.0/C7.1, confirmed on both pads
  paired to the desk: a DualShock 4 also groups its wireless calibration's gyro ranges where
  the USB answer interleaves them, and the rewrite reorders. A late feature report does not
  unsay the transport the input reports gave; a wireless DualSense's output reports carry a
  sequence the library advances.*
- [x] **The rumble event stays** for any pad a host rumbles that way; the application decides
  what to write (a motor-only report keeping the lightbar it last wrote). *C7.1: unchanged.*
- [x] **The demo reads the raw nodes itself**, nothing patched in the toolkit, which has no HID
  path on Linux: the Sony nodes found by identity, opened under the seat's access, polled in
  the millisecond loop, the feature reports read at open, the toolkit's controller events for
  those pads dropped, unplug when a node dies. A knob decides whether a host gets reports or
  states, defaulting to reports for this library's hosts only, known from the host list:
  **a DualSense against an established host is neither promised nor gated.** *C7.2,
  2026-09-20, with one deviation: the host list cannot tell this library's host from an
  established one, because the daemon advertises the established build string, so the knob
  is explicit (`LOWLAT_PAD_RAW`) rather than defaulted from the peer; the policy is still
  the application's. And one addition the same-machine rig needed: `only`, which also drops
  the toolkit's controllers, since the pads the host makes from these reports are
  controllers to a toolkit on the host's own machine.*
- [x] Fixtures from the pads on the desk (addresses zeroed); hermetic tests: the report stream
  in and the messages out, in order and deduplicated; the output report in and the event
  out, both framings; a host that reads no reports still receiving states. *The core half
  is in (C7.0, 2026-09-20): the framing, the reads and the fixtures, with the pads' own
  offsets confirmed against the reports they produced; the DualShock 4 answers its pairing
  report empty over the raw node, which is one more reason that report is the host's. The
  hermetic session (C7.1) sends both pads' reports through the driver to a host that reads
  none of them and counts the report, the block and the states it makes its pads from, then
  writes the pads and reads the events back framed for USB and for Bluetooth, with a write
  for an unknown pad dropped and counted.*
- [ ] Documentation closure with Phase 14's.

**Gate:**

1. **The DualShock 4 against the established host in its DualShock mode** (before the host
   half exists): the pad appears there as a DualShock 4, sticks and buttons the right way
   up, the touchpad works there, rumble comes back as the rumble message and the demo writes
   it. *Passed 2026-09-20 at the desk (`local/logs/2026-09-21-c7--ds4-run-2.log`), on the
   second run: the first found the pad identifiers too wide for that host
   ([01 §11.1](01-protocol.md)). With them below 256: the pad a DualShock 4 there, touch and
   its click working, seven rumble messages answered by the demo's motor-only writes, 900
   to 960 reports a second sent with none dropped, the two feature reports taken from each
   of the pads on the desk (a DualSense beside it over Bluetooth). **The motion sensors do
   not reach that host, and cannot**: its DualShock mode builds the pad's report from the
   sixteen-button state and the ten-byte touch block and writes nothing else into it -- the
   wire it defines for a DualShock 4 carries no motion, and its own client sends none.
   Motion is what the whole report on opcode 31 carries, which this library's host reads
   (Phase 14).*
2. **Both pads against this host** and **the established client holding the DualSense against
   this host** are Phase 14's gate, run with it.
3. The hermetic tests above; the ABI gate at minor 10; the census on the host names the two
   opcodes and nothing unexpected.

## Later, and not in v1

- **A Windows client**: the completion-port receive path in the shell ([02 §6](02-io-shell.md)),
  Media Foundation or NVDEC, shared textures with fences as the handle kind, the toolkit's
  D3D11 renderer. Its own phase when Linux is done; the design already names its handle.
- **Software decode**: a decision, not a phase, and the licence question decides it
  ([09 §9](09-compatibility.md)). Until then a machine without a hardware decoder is refused
  with the stage named.
- **A second stream, the browser pipe, pen and touch, the microphone uplink.** Each is known
  and none is needed for a client that streams.

### Deferred decisions, recorded 2026-09-16, decided 2026-09-19

Each is written up in [10-client.md](10-client.md) and was decided on the numbers the C2 gate
records. **Decided at C5's planning: none of the three is in v1** (the reasoning is in C5
above and in 10 §4.1). They stay written down here so the shape is not re-derived if a
slower decoder ever reopens them.

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

- 2026-09-20: C7 planned with Phase 14. The pair recorded at C3 as later has its wire
  already (opcode 31 in, 33 out, raw reports); the library derives the standard state
  beside the report so every host has a pad; feature reports go first and are bounded;
  Bluetooth is normalised in the library; the demo reads the raw nodes itself rather than
  patching the toolkit; a DualSense against an established host is application policy,
  neither promised nor gated; C7 runs before C6.
- 2026-09-19, evening: C5's second half planned. The cursor's picture is decoded in the
  library and delivered from a buffer the handle owns, valid until the next poll; scaling
  the pointer is the application's, because nothing in the library can; the guest list is
  handed over with the recipient's number rather than parsed, the library needing nothing
  from it; the client's metrics take a shape of their own, with a recent-loss figure read
  from late arrivals, and the host's structure stays the host's.
- 2026-09-19: C5 planned in two halves, the decode half first. The declaration is a
  preference masked by capability with no fallback to another decoder; the second backend is
  driven from the library's own readers and probed by building a real decoder per
  combination; the handle path is in this phase on Linux, the slots becoming exportable
  device allocations sized at the stream's size and the application toolkit becoming a
  vendored tree so its renderer can import them; the three deferred decisions are decided as
  none in v1, the reader's lag being the application's warning; opcode 21 goes out on a time
  cadence with both kinds.
- 2026-09-18, evening: C4 planned and built. The playback window leaves the library for the
  application's device, where every reference keeps it and where the clock is; sound is
  decoded on the application's call rather than on a thread of its own; the pool drops and
  counts; the demo reads resyncs from its device's own queue and logs them at once; "both
  ways" in the hermetic gate is read as both codecs, the uplink being deferred in the same
  phase. Gate 2 passed; gate 1 passed on the established host over eighty-five minutes (40 ppm
  of drift read from the device's queue, a flush every 35 minutes, nothing dropped or
  refused); the demo's resync counter was counting one flush three times and was fixed after
  the run. The run from this host passed the letter and measured a null sink's timer, not
  drift. C4 closed.
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
