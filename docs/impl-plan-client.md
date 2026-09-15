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

## Phase C1 - The client core, and a hermetic session

The connecting side of everything below the media, written against the core that already
plays both roles.

- [ ] `lowlat-client`: the client session -- the offering side of connectivity (one socket,
  the offer's credentials and certificate digest produced for the application, candidates
  out as events, the answer's credentials in), the session's receive half with the video
  ring at 4000 fragments and a 16 MiB read buffer, the initialization of
  [01 §11.5](01-protocol.md) with fourteen keys, opcode 13 per stream, the diagnostics
  message, and the control vocabulary of [10 §7](10-client.md) as events.
- [ ] The **catch-up over arrived messages** ([10 §3](10-client.md)): keyframe metadata found
  ahead is skipped to; nothing is ever skipped over a gap. Named regression test for the
  second half.
- [ ] The **keyframe policy** ([10 §5](10-client.md)): two triggers, no timer, no start-up
  kick. Named regression test that a decoder starved of a keyframe never fires a request and
  that a fault fires one immediately.
- [ ] The C ABI: `lowlat_client_create/destroy`, the four-call seam mirrored, events, status.
  The client half of the header behind `LOWLAT_CLIENT`; the C# mirror gains the client.
- [ ] **A hermetic full session under the simulator**: this host's session against this
  client's, fake clock, scripted loss, reorder and duplication, sound and a synthetic picture
  crossing both ways, the client reporting what it received. The picture is not decoded here
  -- there is no decoder yet -- so the check is on access units: every one the host sent
  arrives whole, in order, keyframes where the host said.

**Gate:**

1. The workspace tests, the lints, the dependency policy and the ABI gate pass; the zero
   allocation checks on the native transport still read zero, from the client's side too.
2. The hermetic session runs clean at zero loss, one percent loss, and five milliseconds of
   reorder, and the client's control census matches the host's log message for message.
3. `lowlat_features()` reports both halves; the header compiles alone with `LOWLAT_NO_HOST`.

## Phase C2 - A picture from a real host

- [ ] `lowlat-decode`: the decoder trait and the **VA-API backend** -- the driver's interface
  loaded at runtime, H.264 and HEVC, eight and ten bit, planes by read-back first.
- [ ] The **frame queue** of [10 §4](10-client.md): four slots, latest wins, the producer
  steals the oldest ready slot and never a held one, `acquire_frame` / `release_frame` with
  the two-held rule and the fence. Named regression tests: the producer never blocks without
  a consumer; a held slot is never overwritten; a third acquire is refused.
- [ ] The decoder built from the stream ([10 §5](10-client.md)): parameter sets build it, bit
  5 keeps it, a stale generation tears it down, a fault destroys it before the request.
- [ ] **`examples/client`**: the C demo on the toolkit, one file for signaling, one for the
  session; window, present, nothing else. It logs in with the same tool the host uses.

**Gate:**

1. **The demo shows this host's desktop**, on this machine, through VA-API on the second card,
   at the display's rate for ten minutes; decode time and queue depth on the log.
2. **The demo shows an established host's desktop** on the second machine for ten minutes from
   a cold connect, with the initialization and the per-stream declaration accepted as the
   established client's are, and a clean departure read as such on both sides.
3. A keyframe with unchanged parameter sets from a host that marks it does not rebuild the
   decoder; from a host that does not, it does, and the picture continues.
4. The hermetic session of C1 now decodes: the picture out matches the synthetic picture in,
   frame for frame, at zero loss.

## Phase C3 - Input

- [ ] `lowlat_client_send_input` and the rules of [10 §8](10-client.md): the pointer
  transform into the picture's pixels with the edge bump, the press-outside guard, release-all
  on focus loss, pad state per poll, the keyboard code guard.
- [ ] The demo takes keyboard, mouse and pad from the toolkit and sends them.
- [ ] Relative mode: the event on the transition, the demo confining and hiding its pointer.

**Gate:**

1. Against an established host: typing lands, the pointer lands where it is aimed on a host
   with one display and with two, a drag that leaves the window releases on the host, a pad
   drives a game, and mouselook works through relative mode.
2. Against this host: the host's own input log agrees with the demo's, message for message,
   for one minute of mixed input; the census shows every opcode the established client sends
   and nothing it does not.

## Phase C4 - Sound

- [ ] Opus decode to PCM, the raw-PCM pass-through, the playback window with the 40 ms cap and
  the flush at either edge, `lowlat_client_acquire_audio`.
- [ ] The demo plays it through the toolkit's audio device.
- [ ] The microphone the other way is **deferred**: the enable and the uplink are known, the
  capture device is the application's, and nothing in v1 needs it.

**Gate:**

1. Thirty minutes of sound from an established host with no audible gap and at most one
   resync, then the same from this host.
2. The hermetic session carries sound both ways and the samples out match the samples in.

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

## Change log

Newest first.

- 2026-09-15: the plan is written, after C0 landed the same day.
