# 05 - Host pipeline

**Status:** locked 2026-08-15. Implemented by `lowlat-capture`, `lowlat-encode`,
`lowlat-inject`, and the orchestration in `lowlat-host`.

Capture and audio land at Gate B on bare metal ([00-overview.md](00-overview.md) D9). The
traits, the pipeline shape, and every rule below are fixed now, because they are what the
synthetic path is built against and what the real backends must satisfy without renegotiation.

## §1 Threads

| Thread | Owns | Count |
|---|---|---|
| capture and encode | acquire, convert, submit, collect, hand to packetizers | one |
| audio | capture, encode, fan out | one |
| network | one session's shell loop | one per guest |
| admission | attempts, candidates, setup, teardown | one |

Input injection rides the delivering network thread. Injection is cheap and fire and forget,
and a dedicated thread would add a hop to the one budget with a human in it.

**Single encode, broadcast fan-out.** One capture and one encode serve every guest. Per-guest
work begins at packetization, where sequence spaces and rings are already separate.

No thread here raises its own priority ([02 §1](02-io-shell.md)).

## §2 Capture

The capture trait is a source of GPU frames plus cursor state. Backends are selected at
startup, never mid-session.

```
trait FrameSource:
    acquire(timeout) -> Frame | Timeout | Recoverable | Lost
    cursor_state()   -> position, visibility, shape-if-changed
```

Rules:

- **A frame is a GPU handle, never bytes.** The variant is platform and backend specific: a
  device texture, or an imported buffer handle. Nothing in the pipeline copies pixels to
  system memory unless §4 explicitly selected a software encoder.
- **Capture is timestamped at acquire**, and that timestamp travels with the frame through
  conversion, encode, and packetization. It is the origin of every latency measurement in
  §10.
- **`Lost` triggers backend reinitialization, not session teardown.** Display mode changes,
  resolution changes, and compositor restarts are normal events. Guests see at most a brief
  freeze and a keyframe.
- The acquire timeout is one frame interval. A timeout is not an error; it means the screen
  did not change, and the correct response is to skip the iteration rather than to encode a
  duplicate.
- **Dirty-rectangle awareness is deferred.** It is a real optimization for desktop work and it
  interacts with the reference chain in ways that need measurement first.

The **synthetic source** implements the same trait and is not a test double. It generates
frames with known content and controlled motion, and it is how everything up to Gate A is
built and verified. It remains in the tree afterward as the deterministic input for
performance work, since a real desktop is not reproducible.

Backend selection, privilege requirements, and the display-stack decision are in
[07-platforms.md](07-platforms.md).

## §3 Colour conversion

Encoders want a planar format. Captures deliver packed. The conversion is ours.

- **Never let the encoder convert internally.** Doing so cost roughly 3 ms per frame on
  contemporary hardware, which at 120 fps is more than a third of the entire frame budget.
- **Planes are written by a compute shader**, addressing each plane as a separate view.
  Copying between subresources to move plane data drops chroma on more than one vendor's
  driver, produces green or smeared output, and looks like an encoder bug for as long as it
  takes to find.
- **One conversion target per in-flight slot.** A ring of four. Frames in flight must not share
  a destination, or the encoder reads a surface that the next iteration is already
  overwriting.
- Conversion runs on the same device as capture and encode. A cross-device path means a
  readback, which §4 forbids implicitly.

## §4 Encode

```
trait Encoder:
    submit(frame, force_keyframe) -> ()
    poll() -> Bitstream | Pending
    reconfigure(bitrate) -> ()
    accepts() -> [FrameVariant]
```

**Submit and poll are separate.** A blocking `encode(frame) -> bitstream` is the single
biggest structural mistake available here: a serialized acquire, convert, encode loop capped at
70 to 80 fps against a 120 fps target. Encode must overlap the next acquire, so the trait
cannot express the serialized form.

- **`reconfigure` changes bitrate live.** It never reinitializes the encoder and never emits a
  keyframe. Congestion response happens many times a minute (§5); an encoder reinitialization
  at that cadence would be visible as a stutter every time the network hiccuped.
- **`force_keyframe` is cheap and is used freely.** Recovery policy in §6 depends on it.
- **`accepts` drives pipeline construction.** The backend declares which frame variants it can
  take. Construction selects a compatible capture, conversion, and encode triple. A mismatch
  is a hard error, logged with both sides named. **A readback is never inserted automatically**
  to resolve one; it must be requested by selecting a software encoder. Otherwise a driver
  quirk turns into a silent per-frame cost that survives to production.

Backends:

| Backend | Status | Notes |
|---|---|---|
| hardware, NVIDIA | v1 | H.264 first, HEVC at Phase 6; low-latency preset, variable rate with a one-frame buffer, non-reference frames enabled |
| software | v1 | dynamically loaded, resolved by codec name; the path for machines without hardware encode, and the path continuous integration runs |
| hardware, open stack | later | for AMD and Intel parts, once there is hardware to test on |

Codec and colour scope is [00-overview.md](00-overview.md) D7: H.264 and HEVC, 8-bit 4:2:0.
The wire bits for 10-bit and 4:4:4 are reserved now so enabling them later is not a wire
change.

## §5 Congestion and the frame gate

Two actuators, driven entirely by host-local signals ([01 §10](01-protocol.md)). There is no
feedback message and none may be added.

**1. The frame gate, instant.** Before each acquire: if **every** active guest lacks window
for an estimated frame, skip the acquire entirely. No capture, no conversion, no encode, no
wasted power, and no damage to the reference chain because nothing was encoded to damage it.
It recovers the moment any guest drains.

**2. Bitrate, slow.** The controller from [01 §10](01-protocol.md) multiplies down on
sustained congestion and creeps back up, applied through `reconfigure`. Its interval
measurement needs fractional-millisecond resolution ([02 §2](02-io-shell.md)).

Policy, exposed as host configuration:

| Mode | Behavior |
|---|---|
| latency (default) | yield frame rate early, hold bitrate; smallest queues |
| quality | hold frame rate, cut bitrate first; for desktop and cinematic work |

Global actuators fire on **consensus only**. The gate requires every guest to be pressured;
the bitrate controller follows the healthy majority. One chronically slow guest must never
degrade the others; it self-paces through §6 instead.

## §6 Multi-guest delivery

v1 policy is single-guest simple, but the data model is multi-guest from the first line
([00-overview.md](00-overview.md) D10).

```
on encoded frame F, keyframe K:
    for each guest G:
        if G.pending_keyframe and not K:    continue
        if G.window_free < estimate(F) * 2:
            G.mark_skipping()               // latches pending_keyframe
            continue
        G.packetize(F)
    if any guest is skipping and has drained, and the throttle allows:
        encoder.force_keyframe_next()
```

**The cascade is the invariant.** A guest that misses one frame must miss every frame until
the next keyframe. Dropping a single dependent frame breaks the reference chain silently: the
decoder keeps going and produces progressively wrong output rather than failing. That is the
gray-frame failure, and it is why `mark_skipping` latches rather than the caller being trusted
to remember.

**The API must make the wrong thing unsayable.** There is no operation that skips one frame
without setting the pending-keyframe state. If the ring exposed one, it would eventually be
called.

Other rules:

- Skipping is per guest. Sequence spaces stay contiguous, so the transport never sees a gap
  and never retransmits a frame that guest was never sent.
- The recovery keyframe is global, since encode is shared. That costs a bounded bitrate spike
  for everyone, which is acceptable because our keyframe is cheap. Throttled to roughly twice
  a second.
- A guest that cannot keep up cycles through skip and resync and emits a degraded event.
  **Disconnecting it is application policy, never SDK policy.**
- Default limit four guests, compile-time cap sixteen. Ring memory scales per guest.

## §7 Input injection

Injection goes through the kernel input layer, below the display server. That is what makes it
work identically on X11, on Wayland, and at the login screen, and it is the capability that
display-server-level injection cannot match.

- **Batches are expanded and injected as a unit.** The wire delivers input batches; expansion
  to device events is a pure, unit-tested function with no device dependency, so the mapping
  is testable everywhere and only the final write needs hardware.
- **Keyboard arrives as usage codes** and maps to kernel key codes through a static table.
  Layout is the far side's concern; we inject physical keys.
- **Absolute coordinates arrive in stream space** and are mapped to the output's geometry at
  injection, accounting for rotation. Relative deltas pass through.
- **One injector per guest**, holding that guest's pressed-key state. On disconnect it
  releases everything it is holding. Without this, a guest that vanishes mid-keystroke leaves
  a key held down on a machine nobody is sitting at.
- **While the host cursor is hidden, absolute motion converts to relative deltas at
  injection.** This is a safety net for a peer that ignores the mode signal, so aiming works
  regardless.
- A per-guest enable flag gates dispatch. The SDK invents no permission model beyond it;
  permissions arrive from signaling ([04 §3](04-signaling.md)).
- Injection tests are labeled and excluded by default. The default suite must never move the
  developer's pointer.

## §8 Cursor

The cursor travels out of band, not composited into the frame. That is a protocol property and
it is the right one: the far side renders it at its own frame rate, so pointer motion feels
local even when video stutters.

- **Shape changes are sent when they change**, encoded as an image, with the classified
  standard shape and hotspot alongside so the far side can use a native cursor where it can.
  One last-sent hash per guest, so a late joiner receives the image on its first update by
  construction.
- Positions are stream space and need the inverse of the far side's fit transform, including a
  width and height swap on rotated outputs.

### §8.1 Three states, never conflated

This is the part with the most history behind it, and the three states below are genuinely
distinct. Folding any two together produces a specific, known bug.

| State | Means | Used for |
|---|---|---|
| **hidden** | the application asked for the pointer not to be drawn | **relative intent** |
| **plane present** | the pointer was carried on a hardware cursor plane this frame | whether the far side must draw it |
| **suppressed** | the pointer was suppressed by touch input | neither; reported separately |

**Relative intent comes from hidden, and from nothing else.** When an application takes over
the pointer for mouselook it hides it. That is its own visible statement of intent, and it is
the only signal that captures the whole class.

**Never infer relative mode from pointer clipping geometry.** Clipping the pointer to a tiny
rectangle is how a *client* enforces relative capture after being told to, so reading it on the
host inverts the data flow. It also misses every application that hides without clipping,
which is most of them, and it silently fails on applications that clip to a rectangle slightly
larger than the threshold. This trap has been implemented and removed once already; it must
not come back.

**Plane presence is not hidden.** A compositor that scales, rotates, or exceeds the plane's
size limit will composite the pointer into the primary plane instead, so the plane is absent
while the pointer is plainly visible. On a scanout capture that is the common case rather than
an edge case. Plane presence answers only one question: did the captured frame already contain
the pointer, in which case the far side must not draw it again.

**Suppressed is its own signal.** A pointer suppressed by touch input is not a mouselook grab
and must never be folded into relative.

### §8.2 Source of the hidden signal per backend

Windows exposes the requested show-state directly as a global flag. Linux has no single
equivalent, so it is per backend, and this is an open item for Gate B:

| Backend | Source |
|---|---|
| X11 | cursor visibility notifications from the fixes extension |
| compositor-mediated | the cursor mode reported alongside the stream |
| scanout | no direct source; plane presence is the only observable |

The scanout row is the problem case, because it is the one backend where the two states
collapse and the false-positive rate is highest. It is flagged in
[07-platforms.md](07-platforms.md) as a backend selection input, not something to paper over
here.

### §8.3 The state machine

- `relative` is a **debounced, latched echo of hidden**. Rising hidden with relative clear sets
  it; clear hidden with relative set clears it.
- **Debounce is roughly 18 ms**, an absolute figure and not a frame count, so a pointer that
  blinks hidden for one frame does not flap the far side between modes.
- **Both `hidden` and `relative` are sent as separate flags**, since the far side needs the
  raw state as well as the decision.
- **On the falling edge, send the reappear position**, so the far side warps its pointer to
  where the host's actually is rather than leaving it wherever capture released it. Leaving a
  drag and leaving mouselook then behave identically.
- **Poll the cursor state from the same context as capture.** It is a property of the
  interactive session, and a daemon thread outside that session either sees nothing or sees
  the wrong seat.

**Known false positive, accepted:** a fullscreen video player or idle interface that hides the
pointer reads as relative. It self-corrects on the next genuine pointer motion, and the
debounce keeps the transient invisible. A stricter form would gate the hidden arm on the
foreground application being fullscreen, but that is extra state for a case the self-correction
already handles. Start without it.

## §9 Audio

Gate B, with capture ([00-overview.md](00-overview.md) D9).

Shape is fixed now: capture from the system monitor source, encode at 48 kHz stereo in short
frames, fan out on the audio channel, newest-wins under pressure. Audio is never retransmitted;
a late packet is worse than a missing one.

The capture surface choice carries the same session-versus-system-daemon question as video and
is decided with it in [07-platforms.md](07-platforms.md). Uplink is deferred.

## §10 Latency budget and instrumentation

The capture timestamp from §2 travels with the frame, and each stage stamps its completion.
Every measurement below is per frame, reported as p50, p95, and p99, never as an average.

| Stage | Measured from |
|---|---|
| acquire | previous present to frame available |
| convert | acquire to conversion complete |
| encode | submit to bitstream collected |
| packetize | bitstream to last packet enqueued |
| wire | first packet sent to acknowledgement |

Stages are instrumented from the start, not added when something feels slow. A budget that
cannot be attributed to a stage produces guesses, and every optimization in this pipeline that
was based on a guess was wrong.

Counters the pipeline owns: frames captured, skipped by the gate, skipped per guest, keyframes
forced and why, reconfigures applied, encoder queue depth, conversion ring occupancy.

## §11 Verification status

**Fixed by protocol, confirmed:** cursor is out of band and image-encoded; congestion is host
local and actuates encoder bitrate through a live reconfigure; input arrives as usage codes and
stream-space coordinates.

**Ours by design:** the capture and encoder trait shapes, the conversion strategy, the frame
gate, the fan-out policy in §6, and everything in §10.

**Pending, and deliberately not yet decided:** the capture backend and therefore the concrete
frame variant, the audio capture surface, and whether dirty rectangles earn their complexity.
All three land at Gate B against real hardware ([impl-plan §Phase 9](impl-plan.md)).
