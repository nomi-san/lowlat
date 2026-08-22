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

### §3.1 The colour matrix is fixed

**Coefficients are BT.709 and are not negotiable.** `kr = 0.2126`, `kb = 0.0722`, so
`kg = 0.7152`. The forward transform the conversion owes is:

```
y = kr*r + kg*g + kb*b
u = (b - y) / (2 - 2*kb)      // 1.8556
v = (r - y) / (2 - 2*kr)      // 1.5748
```

followed by the range encoding below.

**A receiver applies the inverse unconditionally**, with no signal in either direction saying
which matrix was used and no path that selects a different one. Emitting BT.601 therefore
produces a stream that decodes perfectly and renders with the wrong colour: shifted skin tones
and greens, on every platform at once. There is nothing to notice it, so it presents as a
complaint about picture quality and gets attributed to the encoder or the bitrate.

**Range is limited, measured from a recorded stream.** The parameter set of a real 1080p60
session declares `yuv420p`, limited range, BT.709 primaries, matrix and transfer. So the far
side's renderer supporting a full-range path says only that it can be told to use one, not
that anything asks it to. Limited range is:

```
y = y * (219/255) + 16/255
u = u * (224/255) + 128/255
v = v * (224/255) + 128/255
```

with `64/1023`, `876/1023` and `512/1023`, `896/1023` for the 10-bit form. Full range applies
the chroma offset alone and leaves luma untouched. **Getting the range wrong is a subtler fault
than getting the matrix wrong** -- black lifts to dark grey and white clips, which reads as a
washed-out picture rather than as a colour error, so it is easy to mistake for a contrast
setting.

**Say all of it in the bitstream as well.** The recorded stream carries a complete video signal
type and colour description in its parameter set, so the encoder must emit the same: signal
type present, full range clear, and primaries, matrix and transfer all BT.709. Doing the
conversion correctly and then leaving the description absent produces a stream that any
decoder is entitled to interpret with a different matrix, and some do.

**Depth is 8-bit, also measured.** The same recording is `yuv420p` throughout. A 10-bit capture
path is still required ([07 §3.1](07-platforms.md)) because that is what the display hands us,
but it is converted down and the wire carries 8-bit. Nothing observed streams 10-bit, and the
reserved flag bits stay reserved (D7).

**Chroma is averaged over each 2x2 block on the way out.** This is worth stating because the
reverse direction is free and invites the assumption that this one is too: a decoder samples
the chroma plane at the luma coordinate and gets its upsample from the hardware sampler at no
cost. Writing subsampled data has no such shortcut. The two output planes are different
resolutions and the average is ours to compute.

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

- **The quantiser floor is a latency control, not a picture setting**, and it reads backwards
  until the chain is spelled out. A lower floor lets the encoder spend more bits refining a
  frame; more bits is a larger frame; a larger frame is more packets; more packets is longer on
  the wire and longer in every queue between here and the far side. Below about **5** the extra
  bits buy nothing the eye resolves, so they are spent purely on delay. **5 is therefore the
  lowest-latency setting rather than the lowest-quality one**, and it is the default for a
  product whose first goal is latency. Raising it trades visible sharpness for smaller frames.
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

The two actuators aggregate differently, and the difference is not an inconsistency.

**The frame gate requires every guest to be pressured.** Skipping the acquire helps nobody if
one guest can still take the frame, so it fires on unanimity.

**The configured bitrate is a ceiling, not a rate, and not a quality setting.** It bounds what
the controller may climb to; the controller's own answer is what the encoder is actually told to
target. On a healthy path the controller sits at the ceiling and the picture is as good as the
ceiling allows, which is why raising it improves quality. On a congested path the controller sits
well below it and raising it changes nothing. Both are the same mechanism seen from either side
of the constraint.

It is also a budget for the host's uplink rather than a per-guest allowance. Every guest receives
the same encoded stream, so N guests cost N times the encoded rate on the way out. **The ceiling
handed to each guest's controller is the configured rate divided by the number of guests
receiving that stream.** Skipping that division does not oversubscribe one guest; it
oversubscribes the host by a factor of the guest count, and the loss lands on all of them at
once.

So the two aggregations compose: **each guest's ceiling is the configured rate divided by the
count of guests on its stream, and the rate actually applied is the minimum of what the guests'
controllers return.** One term bounds what the host can send in total, the other bounds what the
slowest path can carry.

**A live change to the ceiling is application protocol, not SDK protocol.** Peers exist that
ask their host to change the rate mid-session, and to switch which output is captured, by
sending user data with a sub-identifier and a body the SDK never looks inside
([01 §11.1](01-protocol.md) calls user data an opaque pass through). That is the right layer for
it: the SDK hands the payload up and the application decides, because the request is a policy
question about what the operator permits rather than a protocol one. What the SDK owes is that
the change applies **live** -- `reconfigure` with no reinitialization and no forced refresh --
which is already required of it here and in [01 §10](01-protocol.md). Nothing about this is a
congestion feedback message, and none may be invented (§5 opening).

**Raising the ceiling costs latency, and the chain is the same one the quantiser floor runs.**
More bits is a larger frame, a larger frame is more packets, and more packets is longer on the
wire and longer in every queue between here and the far side. So the setting trades picture
against delay in both directions, and a host whose first goal is latency defaults it low.

**The bitrate is the minimum across guests**, applied only when it moves by more than 0.01
Mbps so noise does not produce a reconfigure per frame. Which means a chronically slow guest
*does* pull everyone's rate down, and that is the intended behaviour rather than a flaw in the
aggregation: the rate is what the transport can actually carry, and sending a guest more than
that produces loss, not quality. What the slow guest must not do is break the others' streams,
and it cannot, because delivery is decided per guest in §6.

Stating it as "follow the healthy majority" was wrong on both counts. A majority rule leaves
the minority receiving a rate its path cannot carry, and there is no majority at all in the
single-guest case that v1 actually ships.

## §6 Multi-guest delivery

v1 policy is single-guest simple, but the data model is multi-guest from the first line
([00-overview.md](00-overview.md) D10).

### §6.1 What the stream codes, when a guest asks for something else

A guest declares what it can decode in two places, and a host reads both
([01 §11.5](01-protocol.md)): session initialization declares it, and every encoder
configuration message restates it. **The later one wins.** A peer may send only the first, so a
host that required both would leave every such peer declaring nothing at all; a peer that sends
the second is changing its mind, and a host that kept the first would never hear it.

An encoder configuration message asking for reinitialization is a **request to code
differently**, not merely a request for a keyframe. It is answered against every seated guest
at once:

```
on reinitialization requested by any guest:
    asked = intersection of every seated guest's declared flags
    report any bit of `asked` this pipeline does not emit
    wanted = codec named by `asked`
    if wanted != running codec:
        build a new encoder for `wanted`
        latch every guest, and move the generation so each peer is told
    else:
        force one keyframe, which is what the request is owed
```

**The intersection, not the last request.** One encode serves every seat, so a capability only
some of them declared is one none of them can be sent: granting it would hand the others a
stream their decoders were not built for, and they report that as a decode failure rather than
as a mismatch. With a single seat the intersection is exactly what that seat asked for, which
is the ordinary case. No seats is no capability rather than every capability, because an empty
intersection is vacuously everything and acting on it would configure a stream from nobody's
declaration.

**Joining is not asking.** A guest that arrives declaring less than the running stream produces
does not move it; D11 settles that case by refusing the seat, and only an explicit request
moves the codec. The refusal is the other half of this and is not built yet, so until it lands
such a guest is sent a stream it cannot decode.

**The guests outlive the encoder.** A seat is announced exactly once, when the guest claims it,
so the seated set has to be owned above the encoder rather than rebuilt with it. A loop that
rebuilt it would find no guests and publish to nobody while every seat still read as streaming.

**A codec the device refuses is not the end of the stream, but it is the end for whoever asked.**
The encoder that was running a moment ago worked, so the guests that were watching keep their
picture. The guest that asked does not: a peer rebuilds its decoder the moment it asks rather
than waiting to be told the request was granted, so it is now holding a decoder for a stream
that will never arrive, and it is ended with a reason.

### §6.2 Ending a session, and saying why

Two mechanisms, and which one applies is fixed by when the host learns it cannot serve the
guest.

**Before a media path exists**, the host declines the offer in signalling
([04](04-signaling.md)). The peer never receives credentials and never punches, and its API
reports the refusal.

**After a media path exists**, the only thing that can end a session is a disconnect on the
control channel ([01 §11.2](01-protocol.md)), because signalling carries nothing once a peer is
streaming. Everything below is that message with a different reason:

| Reason | When |
|---|---|
| no room | every seat is taken when the guest becomes streamable |
| no encoder | nothing could be built for what was asked of it |
| encoder capabilities | the device would not say what it can encode |
| encode failed | the encoder stopped answering mid-stream |

**A peer that cannot decode what it is sent is not on this list.** It is the one party that can
tell, it raises a decode error of its own, and it reports that through its own API. A host
cannot detect it and must not pretend to: what a host owes a peer is the truth about what the
*host* could not do.

**The reason is left where the guest can find it, not sent by whoever decided.** The encode loop
owns no session and cannot write to any peer; each guest owns its own and is the only thing that
can. So a failure marks the seat and the guest turns it into a message.

**And the message needs time to arrive.** It rides a reliable channel, so it is retransmitted if
lost, but a session torn down on the pass that queued it throws the reason away with the ring it
is sitting in. The session stays up briefly after the decision for exactly that reason.

**A stopped encoder is a run of failures, not one.** A device can refuse a single collect and
answer the next; ending every guest over that turns a hiccup into a disconnection.

**And a failure does not end the loop.** It goes back to waiting: the seats free as their guests
read the reason, and the next arrival is owed its own attempt, because a device busy a moment
ago may not be. The wait has to retire the seats it is waiting on -- only this thread may empty
one -- or the loop sees a guest that has already left, calls it occupied, and rebuilds the
encoder that just failed at whatever rate the device refuses it.

```
on encoded frame F, fragment count N, keyframe K:
    largest = max(largest, N)                   // session high-water mark, not this frame
    want_keyframe = false
    for each guest G:
        if G.pending_keyframe:
            if not K:
                // Ask for one once the window could hold the biggest frame yet seen.
                want_keyframe |= G.outstanding + largest <= ceiling(G.rate)
                continue
            if G.outstanding + N > ceiling(G.rate):  continue
            G.packetize(F)
            G.pending_keyframe = false          // the only place this clears
            continue
        if G.outstanding + N > ceiling(G.rate):
            G.mark_skipping()                   // latches pending_keyframe
            continue
        G.packetize(F)
    if want_keyframe and the throttle allows:
        encoder.force_keyframe_next()
```

**The room test is an absolute ceiling on outstanding fragments, not a proportional margin.**

| Configured rate | Ceiling, fragments |
|---|---|
| below 20 Mbps | 1500 |
| 20 to under 30 Mbps | 2500 |
| 30 Mbps and above | 4000 |

The top step is the peer's ring depth ([01 §7](01-protocol.md)), so the highest rate is allowed
to fill the peer's ring and no rate is allowed past it. A proportional margin such as "free
slots must exceed twice the frame" is the shape this section carried before it was measured,
and it is wrong in both directions: it refuses frames at low occupancy on a deep window, and it
admits them when the window is nearly full because the remaining room still happens to be twice
a small frame.

**A skipping guest is retested against the largest frame the session has produced**, not
against the frame in hand. Testing against the current frame lets a guest out of the cascade on
a small predicted frame, whereupon the keyframe it needs does not fit, the keyframe grant is
spent, and every guest pays the bitrate spike for a recovery that did not happen. The
high-water mark costs one integer and removes the whole failure.

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
- **The mapping is a normalisation at both ends, and that is why a stream smaller than the
  display still lands on the right pixel.** A peer scales its window position into the stream's
  dimensions; the host scales that into the absolute axis, which the input stack then spreads
  across the desktop. Neither side needs to know the other's pixel count, only its shape.

  **What that hides is the offset, and it does not survive a second display.** The axis is
  spread over the *whole* desktop, so with one output it lands where it should and with two the
  stream is stretched across both: a 2560-wide picture on a 4480-wide desktop reaches its own
  right edge 57 percent of the way across, and the rest of the picture cannot be reached at
  all. **The failure is per axis**, which is the cheapest check on any fix: two outputs of the
  same height leave the vertical scale at one, so the vertical is already correct and a fix
  applied to the pair rather than to each axis breaks it.

  So the point is expressed in whole-desktop coordinates **including the captured output's own
  position**, rather than handing the input stack a bare fraction and letting it choose. Three
  steps: clamp into the picture, convert into the captured output's rectangle, place that
  rectangle within the desktop. With one output the rectangle is the desktop and the two
  conversions cancel, which is why this is one multiplication rather than a special case.

  **The clamp is load bearing and is not merely tidiness.** A coordinate past the picture would
  otherwise put the pointer on the neighbouring output, where the hardware pointer plane this
  host reads goes empty -- indistinguishable from an application hiding the pointer, so the peer
  is told to switch to relative motion, its cursor vanishes, and it has to be walked back by
  hand ([§8.1](#81-three-states-never-conflated)).

  **The rectangle and the desktop come from the session, which is the only thing that knows
  them.** A display device reports a controller's position inside its own framebuffer, which is
  the corner whatever the desktop looks like, and an output a compositor invented has no
  controller at all. So the layout is asked for once, when the display opens, and matched to the
  captured output by the name both sides know it by. **A session that does not answer is not a
  degraded case**: with one output the picture is the desktop, which is exactly what the axis
  already spans ([07-platforms.md](07-platforms.md)).

  **It also assumes the stream and the display are the same size**, which they are in the
  established model because a host changes the display mode rather than scaling a copy of it. A
  host that ever encodes a scaled or cropped view of a larger desktop has to say so here, not
  discover it through a pointer that drifts.

  **The size a peer is told and the extents its coordinates are scaled by MUST come from one
  place, and that place is the picture the stream really produces.** A display decides its own
  size; a configured size is a request. Describing the stream with one number and scaling input
  by another puts every absolute position through the ratio between them, and the failure is
  quiet and proportional rather than obviously broken: a 2560-wide display described as 1920
  reaches the right edge of the screen three quarters of the way across the picture. The size
  is therefore read from the stream, not from configuration, and re-read while the session runs
  rather than once, because a guest is seated before the stream has opened a display and a
  display can change size afterwards.

  **Both ends of the ratio MUST be in one coordinate space, and a display scale factor is the
  usual way they stop being.** Capture reports the real framebuffer; a windowing system asked
  about screen geometry may answer in scaled units instead, and at 125 or 150 percent the two
  disagree by exactly that factor. Where a platform makes that a per-process property, the
  process MUST declare the aware mode, and it is a **packaging requirement rather than a code
  one**: nothing in the source shows why an undeclared host is wrong, and it is wrong only on
  machines that are scaled. Where the platform has no such split, there is nothing to convert.

  This stops being free when a captured *sub-rectangle* has to be placed within a larger
  desktop, because then the offset and the range must also be in the same space -- which is the
  second-display case above.
- **One injector per guest**, holding that guest's pressed-key state. On disconnect it
  releases everything it is holding. Without this, a guest that vanishes mid-keystroke leaves
  a key held down on a machine nobody is sitting at.
- **While the host cursor is hidden, absolute motion converts to relative deltas at
  injection.** This is a safety net for a peer that ignores the mode signal, so aiming works
  regardless.
- **Three permissions gate dispatch -- keyboard, pointer, gamepad -- and the gate lives in the
  injector.** The SDK invents no permission model beyond them; they arrive from signaling
  ([04 §3](04-signaling.md)). The gate belongs next to the pressed state rather than in the
  message loop because **it is not a filter**: revoking a permission releases everything that
  permission is holding, which is the same guard as a disconnect and needs the same state. A
  gate placed in the loop would have to detect the transition and reach into the injector to
  service it, splitting one invariant across two places, and it would need a second switch over
  the same opcodes.
- Injection tests are labeled and excluded by default. The default suite must never move the
  developer's pointer.

### §7.1 One pointer, shared

**Every guest has its own devices and they still contend for one cursor.** The display stack
merges every pointer device on a seat into a single cursor, so two guests moving at once fight
over it and neither can aim. Keyboards do not conflict that way -- two people typing produce two
streams of keystrokes into whatever has focus, which is what a shared desktop is for -- and pads
do not conflict at all, because each guest's are its own devices. So **only the pointer is
arbitrated**, and arbitrating the rest would stop two people using one session for no gain.

**Off by default.** One person driving at a time is a room's decision, not a host's.

When it is on, the pointer belongs to whoever last moved it and lapses a fixed time after they
stop. Asking for it is the same act as using it: there is no separate claim, because moving
something is the only evidence that a guest wants it. An **owner** takes it from whoever has it
rather than waiting; everybody else waits for the lapse.

**The hold is the whole behaviour.** Too short and the pointer is taken away mid-gesture, because
there is a still moment inside any drag or held aim. Too long and taking over reads as the
session having stopped responding.

**A guest that loses the pointer releases the buttons it was holding, at once.** This is the part
that is easy to get wrong, and getting it wrong is not subtle. A guest loses the pointer by going
quiet, so it will never send a release of its own; the release it eventually does send arrives
after somebody else has taken over and is dropped along with the rest of that guest's pointer
input. The button then stays down on a machine that guest is no longer driving. So the loss is
noticed on the host's own timer rather than waiting for a message, and it releases immediately.

**A guest without the pointer is shown that it does not have it.** Otherwise it finds out by
nothing happening, which is indistinguishable from a session that has stopped responding. The
guest that does not have the pointer is sent a refused shape and gets the real cursor back when
its turn comes. It needs no new mechanism, because cursor updates are already per guest
([§8](#8-cursor)) -- it is one guest being sent a different image.

**The shape is loaded from the desktop's own theme, not drawn.** Theme files carry a picture and
its hotspot together, and that is the part that matters: a backend that derives a hotspot from
where the display drew a pointer ([§8](#8-cursor)) has no way to derive one for a shape the
display never draws, so a hand-drawn glyph would need a hotspot invented for it. It also looks
like the pointer somebody at the desk would see. A machine with no theme shows no shape, which
is the state that existed before rather than a failure.

**A change of turn owes an update even when the pointer has not moved**, or a guest that stops
moving keeps whichever shape it had at the moment its turn changed. Only the picture is
substituted: the position stays the display's to report, so a guest that cannot drive still
knows where the pointer is.

**The hold and that feedback are related, and the figure is still the one chosen without it.**
Without feedback it has to stay short enough that being ignored is over before anybody wonders;
with it, a longer hold is comfortable because the guest can see whose turn it is. **It is a
judgement made by feel with two guests and it must be settled that way**, in the same run that
checks the feedback works, rather than reasoned to a number.

The local user taking the pointer back from every guest needs a source of local input activity,
which is the same "observe the interactive session" problem as the hidden-pointer signal
([§8.2](#82-source-of-the-hidden-signal-per-backend)) and lands with it.

### §7.2 Gamepads

A peer's pad is a virtual device like its keyboard, with three differences that all follow from
one fact: **a host does not know how many pads a guest has until it sends one.**

- **A pad is created on the first message bearing a new identifier**, so unlike the keyboard and
  pointer it cannot be created at admission and its device-readiness delay
  ([07 §4.1](07-platforms.md)) sits in front of a stick somebody is already pushing. The same
  queue-until-usable rule applies, and here it is load bearing rather than a nicety.
- **The identifier is the peer's and is not an index** ([01 §11.1](01-protocol.md)). It maps to
  a slot, and the number of slots one guest may hold is capped.
- **Destroying the device is the release.** A pad that vanishes holds nothing, so unplug and
  disconnect need no held-button walk -- but only if the device is genuinely destroyed, which
  makes it something to test rather than something to assume.

**Force feedback travels back to the peer.** A local application raising a rumble effect on the
virtual pad reaches the guest that owns it as a rumble message. Only the simple magnitude effect
is offered: it is what the common controller libraries raise, and the shaped effects would
require carrying an envelope simulation for a peer that can express two motor strengths and
nothing else.

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
| scanout | no direct source; plane presence, debounced ([§8.4](#84-deriving-hidden-from-plane-presence)) |

The scanout row is the problem case, because it is the one backend where the two states
collapse and the false-positive rate is highest. It is flagged in
[07-platforms.md](07-platforms.md) as a backend selection input, not something to paper over
here.

### §8.4 Deriving hidden from plane presence

Where a backend has no direct source, the plane is the only observable and it **can** carry the
signal, but only with the rules below. Every one of them exists because its absence shipped.

**The far side derives relative mode from `hidden` as well as from `relative`** -- its test is
either bit, not both. So this is not a drawing instruction that can be set freely: setting it
takes the guest's pointer away and turns its motion into deltas.

- **Debounce before believing it.** The plane empties for an application taking the pointer,
  which is meant, and for a pointer that merely outgrew what the plane can carry, and for the
  moment a display mode change is being rebuilt, which are not. The transients pass; an
  application holding the pointer does not. **Slow to hide, immediate to show**: a quarter
  second before a pointer disappears on entering a game is imperceptible, and a quarter second
  of a guest unable to see its own pointer is not.
- **Nothing drawn means nothing until something has been.** A stream can open onto an idle
  desktop whose compositor is not using the plane at all, and asserting from that tells a guest
  its pointer was taken over before it has been shown one.
- **Only a read that examined the pixels may speak for the pointer.** Where the picture is read
  on a cadence, the reads in between report the picture already held; counting those as a
  pointer still being present clears the wait every time, so it never expires.

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

### §8.5 A cached picture keeps the offset it arrived with

**A peer that keeps pictures is sent a name instead of a picture, and a name carries no
hotspot.** The far side applies the offset when a picture arrives and has no way to be told a
new one for a picture it already holds.

**That makes the offset part of what the peer holds, not a property of the update.** The record
of what a peer has is therefore the picture *and* the offset it was sent with, and only both
together are a name; a corrected offset sends the picture again.

**This is the ordinary case rather than a corner**, on any backend that derives the hotspot
rather than being told it ([§8](#8-cursor)). The derivation needs a guest to command a position,
so every shape necessarily travels once before its hotspot is known, carrying none at all.
Naming it from then on freezes that: an I-beam draws half its own height low, while an arrow
looks correct because an arrow's offset really is near nothing, which is what makes the fault
easy to miss.

**Replace the record rather than adding one.** A shape whose offset is re-learned would
otherwise fill the peer's cache and force it to forget every picture it holds.

## §9 Audio

**48 kHz stereo, 20 ms a packet, one encode fanned out to every guest**, on the audio channel
with the framing in [01 §11.4](01-protocol.md). One encode serves the room exactly as one
picture does, and a guest is handed the packet rather than a copy of its own.

**Two codecs, because a guest chooses.** Opus by default; uncompressed sixteen-bit stereo for a
guest that asks for it in its initialization and a host that permits it. The choice is per guest
and per packet, so a room may hold both at once, and the second encoding costs nothing to
produce because it is what capture already delivered.

**Uncompressed is not free on the wire and must be paid for.** A packet is 3840 bytes against
Opus's few hundred: four fragments instead of one, 200 a second instead of 50, and 1.54 Mbit/s
per guest against 0.14. That is five percent of a thirty megabit session, taken from a budget
the video rate controller cannot see, so **it is subtracted from the video ceiling for that
guest** rather than spent on top of it.

### §9.1 The sound source is the clock

**Capture is paced by the source and never by a timer.** A read returns when the sound server
has a fragment, and the host encodes and sends what it got; a host that pulled on its own clock
would drift against the sound device for as long as the session lasted and would have to
resample to hide it. Measured over five minutes, the source's own rate and the host's sample
accounting agree to within a constant offset.

**The cadence is not the fragment.** A sound server delivers on its graph's own period, which
need not be the frame the host asked for, so fragments arrive a little early or a little late
and occasionally two at once. The rate is exact even when the spacing is not, which is the
property that matters: a packet declares how many samples it carries and a receiver plays them
in order.

**Silence is skipped.** A source with nothing playing delivers zeros rather than stopping, and
sending them costs the full bitrate to reproduce silence the far side already has. A host that
goes quiet is a case every established client handles, because a source that produces nothing is
ordinary.

### §9.2 Losing a packet, and never leaving a hole

The audio channel is reliable and ordered like every other, so **a packet cannot be dropped once
it is sent**: the gap would stall the receiver until retransmission filled it, and a
retransmitted packet arrives long after it was due. The drop therefore happens **before the
sequence number is assigned** -- if the send window cannot take the whole message, the packet is
discarded and the next one takes its place. Nothing is retransmitted late because nothing late
was ever numbered.

That also makes the size question a correctness question: a message is enqueued whole or not at
all, so an uncompressed packet's four fragments never half-arrive.

### §9.3 The source may change under the session

**A host follows the sound device rather than pinning it.** Three things move it: the person
changes their default output, the application names a device, or something else in the session
moves this host's stream. All three end in the same place -- resolve the wanted source again,
move the stream, and **publish the source the host is actually on rather than the one it asked
for**.

**A source change is silent on the wire.** A receiver rebuilds its decoder only for a codec,
channel-count or mask change ([01 §11.4](01-protocol.md)), and a device switch changes none of
them: the host keeps asking for 48 kHz stereo and the sound server converts. So unlike an output
switch on the video side there is no refresh owed, nothing is renegotiated, and the audible cost
is the few tens of milliseconds the move itself takes.

**A device that is not there is substituted, not refused.** A sound server hands out something
plausible rather than failing, so a requested device is checked against the enumeration before it
is opened -- the same rule a requested output follows in §7.

### §9.4 What this host does not do

**No per-application exclusion.** An established host on another platform can capture everything
except one named program, which is what keeps a voice call from echoing back to the person on the
other end of it. That call has no equivalent here, and imitating it means tapping each program's
own output and re-linking as programs come and go -- a mechanism with a lifetime problem, whose
failures are silent and whose symptom is somebody hearing themselves. **A person mutes the
application instead**, which is a control they already have and which works whatever sound server
is running.

**No uplink yet.** A guest's microphone reaches a host as its own message and is not part of
this: what a host would do with it is create a capture device in somebody's session, which is an
application's business rather than a shared library's ([06 §13](06-api.md)). The framing is
known and the work is scoped separately.

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

**Measured, and previously misdescribed here:** the delivery rules in §6 -- the ceiling table,
the high-water retest, and the latch -- and the aggregation in §5. All four were written as
design choices and two of them were wrong. They are now stated as the constants they are.

**Ours by design:** the capture and encoder trait shapes, the conversion strategy, the frame
gate's unanimity rule, the choice of a forced keyframe over an encoder restart for recovery,
and everything in §10.

**A note on that last one**, because it is a deliberate divergence rather than an omission. The
invariant -- a guest resuming after a gap never receives a dependent frame -- can be satisfied
either by forcing a keyframe or by restarting the encoder, since a restart emits one anyway. A
restart also costs orders of magnitude more, and it self-throttles only because it is slow. We
force the keyframe and throttle it explicitly at roughly twice a second, which reaches the same
invariant at a cost that does not have to be hidden.

**Pending, and deliberately not yet decided:** whether dirty rectangles earn their complexity.
The capture backend and the concrete frame variant were settled at Gate B against real hardware
([impl-plan §Phase 9](impl-plan.md)); the audio capture surface is settled in §9 above.
