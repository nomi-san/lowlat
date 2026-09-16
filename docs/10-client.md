# 10 - The client

**Status:** designed 2026-09-15, interview of the same day. Built by
[impl-plan-client.md](impl-plan-client.md).

The client is the other half of the same protocol: it receives what [05](05-host.md) produces.
Everything below the media -- the wire, the rings, acknowledgement and recovery, connectivity,
the credentials -- is [01](01-protocol.md) to [04](04-signaling.md) read from the connecting
side, and the code is the same code: the session is symmetric, and the connectivity engine
already plays both roles. What is new is what a client does with bytes once they are in order:
decode them, hand pictures and sound to an application at the right moment, and send its input
back. This document is that half.

## §1 Scope

**v1 is one video stream, over the native transport, on Linux.** Stream 0 on channel 1, sound
on channel 2, control and input on channel 0. The client declares all three streams, as every
peer does, and takes pictures from channel 1 alone; a second stream is display switching, an
application-level feature over user data ([01 §11.2a](01-protocol.md)) that v1 does not
build. The second pipe -- the browser transport of [01 §14](01-protocol.md) -- exists so a
browser can be a guest; a native client has no reason to be one, and the page in
`examples/web-client` is already the browser client. The attempt information keeps its
`transport` field for symmetry; a client refuses the web value with a status.

**Windows follows Linux**, as it does for the host: the decoders and the picture handles are
the platform-specific stages, and the shell's receive path ([02 §6](02-io-shell.md)) is the one
piece of shared code that changes.

## §2 Where a client is different from a host, in one table

| | host | client |
|---|---|---|
| connectivity role | answers an offer; walks a base port per guest | makes the offer; one socket |
| session | one per guest, all fed by one encode | one |
| the media path | capture, convert, encode, packetize | reassemble, decode, hand out |
| the clock | the display's refresh paces the loop | there is no clock: the newest picture is shown when asked for |
| reference-chain repair | the host's job: a guest that misses one frame is cascaded to the next keyframe ([05 §5](05-host.md)) | none: a client never sees a gap that retransmission will not fill |
| keyframe requests | answered | sent in exactly two cases (§5) |
| input | injected | encoded from the application's events (§8) |
| threads | capture, encode, one guest loop per seat, audio | one receive loop, one decode thread per stream, one sound thread |

The row that matters most is the fourth: **the client has no pacer.** Every client generation
compared here decodes as fast as pictures arrive, keeps a queue two deep, hands out the newest
and discards the rest, and aligns to no display. Latency comes from doing that promptly, not
from scheduling.

## §3 The receive path

The session and its rings are the core's, unchanged ([01 §7](01-protocol.md)). Two sizes are
the client's to choose, and both follow from what a host is allowed to have outstanding:

- **The video receive ring holds at least 4000 fragments.** A host's per-guest ceiling on
  outstanding fragments tops out at the peer's ring depth ([05 §5](05-host.md)), and the ring
  *is* the flow-control window: a ring smaller than the ceiling drops arriving fragments whose
  slots still hold undelivered data, which stalls the cumulative count on a healthy link and
  reads as loss. Shrinking it to save memory has been measured to cut throughput threefold.
- **The read buffer holds a whole keyframe: 16 MiB.** A message is one access unit and it
  arrives whole or not at all; a buffer smaller than the largest keyframe refuses the message
  without consuming it and the stream is over. Older client generations hold two megabytes,
  which is the ceiling a host must keep its keyframes under for them.

**There is no skip.** A gap in the ring is a fragment in flight or in retransmission, and the
reader waits for it; the sender never frees an unacknowledged fragment short of ending the
session, and it ends the session on its own delivery deadline before the client's liveness
deadline would ([01 §9](01-protocol.md)). A client that skipped forward would discard data
that is about to arrive, feed the decoder a broken chain, and then have to ask the host to
rebuild its encoder to recover -- the expensive request of §5 -- to repair damage it did
itself. The core's stall-escape mechanism stays available to the caller and the client's policy
is to leave it unused.

**Catch-up is over messages that have arrived, and it is keyframe-aligned.** When the decode
thread is more than one message behind on channel 1, it looks ahead through the messages the
ring holds for keyframe metadata ([01 §11.3](01-protocol.md), bit 6) whose picture has also
arrived, and skips to that picture, discarding the pictures before it. That is the newest
client generation's behaviour and it is the transport-level form of §4's latest-wins: the
reader recovers in one step rather than decoding a backlog it will never show. Against a host
that sends no metadata messages the look-ahead finds nothing and the reader decodes in order.

**The catch-up is a mechanism, not a remedy: it needs a keyframe in the backlog** (*added
2026-09-16*). This host announces every keyframe it sends, and those are the seating
keyframe, the ones a request or the gate's cascade produce, and no periodic ones; the
established host's periodic keyframes exist only when it is configured with an interval. So a
reader that is behind because its decoder is slower than the stream ordinarily finds nothing
ahead and decodes in order, the case [§4.1](#41-what-latest-wins-costs-and-what-it-cannot-do)
describes and the deferred request of [§5](#5-the-decoder-and-when-a-client-asks-for-a-keyframe)
would answer. What the receive path owes those decisions is the measurement: how many
messages the reader is behind, and how long the oldest of them has waited, both reported
([§9](#9-events-status-and-metrics)) rather than acted on.

## §4 Pictures: the queue, and acquire and release

**Depth two, latest wins, the producer never blocks.** The decode thread publishes into a ring
of four slots; the application holds at most two -- the one it is presenting and the one it
has just acquired, so a swap has no gap -- one is being decoded into, and one is ready. When a
decoded picture finds no free slot the decoder overwrites the oldest *ready* slot, never a held
one: the picture the application has not looked at yet is the one nothing will miss. The
decode thread is also what drains the transport, so a decode thread that waited for the
application would back the whole session up into the receive ring; a minimised window must
cost nothing but the pictures it does not show.

**Acquire is the poll.** `lowlat_client_acquire_frame` waits up to its timeout for a picture
newer than the last one handed out, discards any older ready pictures on the way, and lends
the newest. A picture stays valid until it is released; the application presents it as often
as it likes in between, which is what a renderer that re-presents on every iteration needs.
**Release carries an optional fence**: a synchronisation object the application's device
signals when it has finished reading the picture, so a decoder writing straight into shared
memory waits on the application's GPU rather than on its CPU. A null fence means "reusable
now", which is the right answer for a picture that was copied.

**A picture leaves the library one of two ways, and the library chooses which it can offer.**
As **planes**: pointers, pitches and a format (`NV12`, `P010`, or the 4:4:4 layouts) into
memory the library owns for the lease, which every renderer can take and which is the path a
software or read-back decoder produces anyway. Or as a **handle**: a device-level reference --
a buffer file descriptor and its layout modifier here, a shared texture and a fence on Windows
-- that the application imports into its own device with no copy. The application asks for a
kind at creation and is told which it got; a decoder that cannot export hands out planes.

Each picture carries what the header and the bitstream said about it: size, rotation (applied
by the renderer, not the decoder -- the picture arrives as the display was encoded, and a
quarter turn is a transform at present time), colour depth, chroma layout, and the generation
it belongs to.

### §4.1 What latest-wins costs, and what it cannot do

**Latest-wins trades motion evenness for currency, and the trade is deliberate** (*added
2026-09-16*). The application presents on every refresh whatever it is shown; what moves is
the source-time step between consecutive presents. With the stream above the display's rate
the step is two frames with a jitter of one, from the phase between decode completion and
the poll; with the two rates equal and their clocks unsynchronised, their beat skips or
repeats one picture per beat period, a periodic hitch; below the display's rate there are
only repeats. A burst -- a decode stall followed by three pictures in three milliseconds --
shows the last and drops two. A first-in-first-out queue would smooth every one of these by
holding pictures, and would then be that many frames late for the rest of the session. The
wire carries no presentation timestamps (the header has size, rotation and generation), so
any pacer here can only pace on arrival time, which the network jitters; it would buy
evenness with latency and nothing else. The rates are the real lever: a stream at the
display's rate, or an integer multiple above it, is the smoothest a client without a pacer
gets.

**And it cannot help a decoder that is slower than the stream.** A predicted picture needs
the one before it, so an undecoded backlog cannot be thinned; it sits in the receive ring,
grows until the ring is full, and from then on the picture is the ring's depth in the past
while input acts on the present. No client generation compared here has a remedy: the
established client warns the user when its queue stays above thirty messages and leaves the
rate to the host's panel; the newest one can fast-forward only to a keyframe that is already
in the backlog, which without a periodic keyframe on the host is never there.

**Deferred decisions, recorded here so they are decided once** (the plan lists them):

- *A presentation pacer in the application, not the library.* Moonlight, an open client for
  a different protocol, sets the frame rate from the client and offers frame pacing as an
  option with its trade-off named -- lowest latency or smoothest motion; the same shape fits
  here as a deeper hold count on acquire, so an application that wants evenness over
  currency can buffer. Decided on the cadence numbers Phase C2 records, not before.
- *A decode-lag keyframe request*, §5 below.
- *A presentation-rate hint and a sustainability event*, §9 below.

## §5 The decoder, and when a client asks for a keyframe

**A decoder is built from the stream, not from the configuration.** The first access unit led
by a sequence or video parameter set builds it, for the codec the unit names, at the depth the
video header's bit 3 names, and any later unit led by parameter sets rebuilds it -- unless the
header's bit 5 is set, in which case the unit is fed to the decoder that exists and the
generation rule below is not applied to it ([01 §11.3](01-protocol.md)). A host that repeats
parameter sets on every keyframe without setting that bit costs a rebuild per keyframe, and
the two hosts compared here both repeat them. **Under the video protocol the rebuild is
explicit instead**: a keyframe-metadata message whose rebuild bit is set tears the decoder
down before the keyframe it announces, and every keyframe carries bit 5; a metadata message
is consumed and never decoded (*amended 2026-09-16*). This host sends the pair to a guest
that declares the protocol ([05 §6.1a](05-host.md)).

**A client asks a host for a keyframe in exactly two cases**, and both are sent as opcode 13
with the reinitialisation argument set:

1. Its video configuration changed -- a different codec, colour or decoder was requested -- and
   the decoder was torn down for it.
2. Its decoder reported a fault it cannot recover from, and was destroyed before the request
   went out, so the next access unit starts a fresh one.

There is no periodic refresh, no request at start-up (the host's own start sends a keyframe),
and no request on a gap (§3). Above all, **the request is not gated by time**: a request that
is dropped rather than deferred while a throttle is active leaves the decoder waiting for a
keyframe that never comes, because a decoder starved of one reports "need more data", not a
fault, and nothing re-arms the request. **And the request is an encoder rebuild on the other
side**, not a picture: the established host tears its encoder down and builds it again on
every one, with a rate reset and a keyframe after ([01 §11.5](01-protocol.md)). This host
answers a request that changes nothing with a keyframe alone; a client cannot assume that of
any other host, so it asks only when it must.

**The request and the teardown are one act, and there is at most one per fault** (*corrected
2026-09-16*). A decoder that reports a fault is destroyed and the request goes out once; after
that, every access unit that cannot build a decoder -- anything not led by a parameter set --
is ignored without a request, and an access unit that decodes to nothing is not a fault. So a
burst of bad units costs one request, which on the established host is one encoder rebuild,
never a storm; and a request is never sent while there is no decoder to be at fault. An
earlier draft here asked on the first fault and rebuilt only when faults persisted, which
sends a request per bad unit; against a host that rebuilds its encoder on every request that
is the storm, and it is not what any client does.

**Deferred: a third trigger, for a reader that has fallen behind** (*recorded 2026-09-16*,
not in v1). The two cases above are the established rule and they leave the slow-decoder
case of [§4.1](#41-what-latest-wins-costs-and-what-it-cannot-do) without a way out: the
backlog has all arrived, the decoder is sound, and nothing in it is a keyframe to skip to. A
request sent when the reader is more than a threshold behind, no announced keyframe is ahead
in the ring, and none went out in the last two seconds, gives the catch-up of
[§3](#3-the-receive-path) something to land on: the picture snaps to the present at the cost
of a keyframe from this host and of an encoder rebuild, every two seconds while the lag
lasts, from the established one. It is a divergence, and a correct one on this protocol --
an instantaneous refresh resets references, so nothing is lost by abandoning arrived
pictures -- but it does not cure a decoder that is slower than the stream; only fewer frames
do, which is §9's event. Decided when the C2 lag numbers exist, with the thresholds measured
rather than picked.

**A picture whose generation is older than the one the host last announced (opcode 29) is
stale**, from an encoder that no longer exists; the decoder is torn down before it is fed, so
that the new generation's keyframe, which carries its own parameter sets, builds a fresh one.

### §5.1 Backends, Linux, in order

| backend | reached through | hands out |
|---|---|---|
| VA-API | the driver's own interface, loaded at runtime | planes by read-back; a handle where the surface exports |
| NVDEC | the vendor's decode interface, loaded at runtime | planes by read-back; a device pointer or handle where the application's device can take it |

The choice is the application's by index, as the host's encoder is; unset, the first backend
that opens on the device the application named. Nothing is linked: a machine without either
interface refuses with the stage named, exactly as a host without an encoder does. **Software
decode is a decision deferred**, with its licence question attached ([09 §9](09-compatibility.md));
v1 is hardware or nothing.

## §6 Sound

Channel 2 carries fifteen bytes of header and one packet: the channel mask, samples per
channel, the rate, a codec byte, and the channel count ([01 §11.4](01-protocol.md)). The
codec byte is read: **2 is uncompressed PCM and is handed out as it is; anything else is Opus**
and is decoded. A change in mask, codec or count rebuilds the decoder.

**The library decodes; the application owns the device.** `lowlat_client_acquire_audio` hands
out signed sixteen-bit stereo at 48 kHz, in order, up to 20 ms at a time; playing it is the
application's, on whatever device and clock it has. What sits between the decoder and that
call is the **playback window**, which is the thing that decides whether sound stutters: a
bounded queue that is flushed at either edge -- too empty is a gap, too full is latency -- with
a cap of 40 ms on any one decoded packet. The host's source is the clock ([05 §9](05-host.md)),
so the window drifts against the device's clock at the rate of their difference and resyncs on
the order of once every twelve to fifty minutes; no client can remove that, and a client that
tries by resampling to the device makes a feedback loop.

## §7 Initialization and the control vocabulary

The client sends the fourteen-key initialization of [01 §11.5](01-protocol.md), whose `_flags`
is stream 0's declaration, then the diagnostics opcode with every bit clear, then opcode 13
for **streams 1 and 2 only**, with the same flags and the reinitialisation argument clear --
which is what the host stores per stream; stream 0 never sends one at start (*corrected
2026-09-16*; an earlier draft said all three). Nothing waits on an acknowledgement: the
secondary declarations follow the initialization in order on the same reliable channel, and
the host's own seating of the guest produces the first keyframe. From then on the client
reads channel 0 for:

| opcode | what the client does |
|---|---|
| 9 cursor | decodes the image if one came, scales the hotspot into the window, and raises a cursor event; the *suppressed* flag hides the local pointer |
| 10 disconnect | records the status the host gave and ends; zero is not an ending ([01 §11.2](01-protocol.md)) |
| 16 blocked | raises the blocked or unblocked event |
| 17 user data | hands the body to the application with its sub-identifier, opaque ([01 §11.2a](01-protocol.md)) |
| 20 rumble | raises the rumble event for the pad named |
| 21 encode latency | stores the host's figure for the stream named |
| 25 guest list | parses it whole, finds its own entry by number, and takes its permissions from it; a body that does not parse is dropped whole |
| 27 stream ended | raises the stream event with the reason |
| 28 host mode | stores it |
| 29 encoder generation | stores it per stream, for §5's staleness rule |

Anything else is ignored. The client sends opcode 21 every two seconds with its decode time,
in the argument order a client uses ([01 §11.1](01-protocol.md)).

## §8 Input

The application reports what happened in its window; the library turns it into the wire's
vocabulary ([01 §11.1](01-protocol.md)) and applies the rules every client applies:

- **Pointer positions are transformed into the picture's own pixels.** The coordinate a host
  expects is in the frame it sent, so a window position is mapped through the letterbox and
  the scale the renderer used, then clamped to the picture; a position one short of the edge
  is bumped onto it so the far edge is reachable. Relative motion is sent as deltas unchanged.
- **A button press outside the picture is not sent; its release always is**, so a drag that
  leaves the window ends cleanly on the host.
- **Losing focus sends the release-all message**, so nothing stays held on a host whose window
  is no longer in front. Pads are centred by that, not unplugged ([05 §7](05-host.md)).
- A keyboard code of zero is dropped. Modifier state travels in the mask the host reads for
  lock-key synchronisation.
- Pads are sent as whole states at the application's cadence, one message per poll, or as
  single button and axis changes; the host takes both and does not require one.
- Pen and touch are deferred, as they are on the host.

**Relative mode is the host's to announce and the client's to enter.** The cursor message
carries it; on the transition the library raises an event and the application confines and
hides its pointer -- a warp on the transition, not on every update, or the pointer fights the
application's own motion.

## §9 Events, status and metrics

Events are polled ([06 §5](06-api.md)): candidate found, established, ended with an outcome,
cursor, relative mode, blocked, rumble, user data, stream ended, host mode. Status carries
what the session is doing and what the decoder is; metrics carry the client's side of the
same named channels the host reports ([06 §3](06-api.md)) -- what arrived, what was
retransmitted to it, its decode time, its queue depth -- so an application can draw the same
panel either side.

**Deferred: a presentation-rate hint, and an event when the decoder cannot sustain the
stream** (*recorded 2026-09-16*, not in v1). There is no wire-level channel for the rate a
client can take that any host honours: the initialization's `refreshRate` is a literal 60
from every established client and no host reads it, the decode time of opcode 21 is only
re-published, and one encode serves every seat, so a host cannot thin frames for one guest
without breaking its reference chain -- temporal layering in the encoder is the only
per-guest frame-rate lever, and that is a host phase of its own. The lever every host
honours is `encoderFPS` in the application protocol, which is the application's message
([01 §11.2a](01-protocol.md)) and stays so. What the library can own is the decision:
`lowlat_client_set_config` takes the application's presentation rate (its display's refresh,
or zero for unknown), status carries the rate the decoder can sustain, and an event says when
it cannot sustain the stream, with the rate the library recommends; the application relays
that as `encoderFPS` in one line, and it works against an established host exactly as it
does against this one. Decided with the lag trigger of [§5](#5-the-decoder-and-when-a-client-asks-for-a-keyframe).

## §10 Threads

One receive loop (the shell's), one decode thread per stream that also drains that stream's
ring, one sound thread, and nothing that presents: presentation is the application's thread
calling acquire. Every rule of [02](02-io-shell.md) applies -- raw wakes for raw waits, no
elevated priority inside the library, teardown that wakes every waiter.
