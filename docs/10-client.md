# 10 - The client

**Status:** designed 2026-09-15, interview of the same day; C1 and C2 built 2026-09-17, C3
and C4 2026-09-18; C5's decode half planned, built and gated 2026-09-19, its second half
planned and built the same evening; C7 (the pad reports) 2026-09-20, C8 (the software
decoder, a decoder chosen mid-session) 2026-09-21, C9 (full chroma on the open stack)
2026-09-22; C6 (packaging) closes the set, and this document was read against the code
once more at its closure. §11, the relay, planned, built and closed 2026-09-23 as C10.
Built by [impl-plan-client.md](impl-plan-client.md).

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
| connectivity role | answers an offer; walks a base port per guest | makes the offer; one socket; allocates a relay when the application configures one (§11) |
| session | one per guest, all fed by one encode | one |
| the media path | capture, convert, encode, packetize | reassemble, decode, hand out |
| the clock | the display's refresh paces the loop | there is no clock: the newest picture is shown when asked for |
| reference-chain repair | the host's job: a guest that misses one frame is cascaded to the next keyframe ([05 §5](05-host.md)) | none: a client never sees a gap that retransmission will not fill |
| keyframe requests | answered | sent in exactly two cases (§5) |
| input | injected | encoded from the application's events (§8) |
| threads | capture, encode, one guest loop per seat, audio | one receive loop, one decode thread per stream; sound is decoded on the application's call |

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
- **The access-unit buffer holds the largest message the ring can complete: its depth times
  a fragment's body, about 4.8 MB** (*corrected 2026-09-17*). A message is one access unit
  and it arrives whole or not at all; a buffer smaller than the largest keyframe refuses the
  message without consuming it and the stream is over. But the ring bounds that keyframe: a
  message has to sit entirely in the ring before it can be taken, and the ring refuses a
  fragment further than its depth past the reader, so nothing larger than the ring can ever
  complete. The 16 MiB an established client allocates is room nothing fills, on its ring as
  on this one. Older client generations hold two megabytes, which is the ceiling a host must
  keep its keyframes under for them.

**There is no skip.** A gap in the ring is a fragment in flight or in retransmission, and the
reader waits for it; the sender never frees an unacknowledged fragment short of ending the
session, and it ends the session on its own delivery deadline before the client's liveness
deadline would ([01 §9](01-protocol.md)). A client that skipped forward would discard data
that is about to arrive, feed the decoder a broken chain, and then have to ask the host to
rebuild its encoder to recover -- the expensive request of §5 -- to repair damage it did
itself. The core's stall-escape mechanism stays available to the caller and the client's policy
is to leave it unused.

**Catch-up is over messages that have arrived, and it is keyframe-aligned.** When the reader
is more than one message behind on channel 1, the receive thread looks ahead through the
messages the ring holds for keyframe metadata ([01 §11.3](01-protocol.md), bit 6) whose
picture has also arrived, and skips to the latest such picture, discarding the pictures before
it. That is the newest client generation's behaviour and it is the transport-level form of
§4's latest-wins: the reader recovers in one step rather than decoding a backlog it will never
show. Against a host that sends no metadata messages the look-ahead finds nothing and the
reader decodes in order. **It runs where the ring is** (*2026-09-17*): the receive thread owns
the ring and takes access units off it for the decoder's thread, so it is the receive thread
that looks ahead, and it does so whether or not the decoder is keeping up -- a backlog the
decoder has not taken never grows past one keyframe interval, because each keyframe's
arrival discards what came before it.

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

**The slots are sized once and backed late** (*built 2026-09-17*). Each is sized at the
configuration's ceiling (4096 square by default, lowerable at creation) in the deepest layout
a decoder here produces, so a rebuild never reallocates and a slot the application holds is
never pulled from under it; but nothing is allocated at creation or at an attempt -- the
backing is made on the decode thread at the first picture, demand-zero, and each picture is
laid out in its slot at its own pitch, so the working set is the pictures actually written
and never the reserve. An earlier implementation initialised its slots at creation and paid
a quarter of a gigabyte resident before a picture existed. Here a client at 1080p is about
140 MB above its resident set at creation, most of it the driver's.

**Acquire is the poll.** `lowlat_client_acquire_frame` waits up to its timeout for a picture
newer than the last one handed out, discards any older ready pictures on the way, and lends
the newest. A picture stays valid until it is released; the application presents it as often
as it likes in between, which is what a renderer that re-presents on every iteration needs.
**A renderer waits in acquire, not in its present** (*2026-09-24*). A present that waits
for the display's refresh holds the thread that would take the next picture, so a loop that
presents the picture on screen again and polls afterwards takes a picture arriving
mid-refresh a refresh late and shows it a refresh after that. Waiting in acquire, the
picture is drawn the moment it is published and shown at the next refresh, or at once
without vsync; the one on screen is presented again only when a wait brings nothing. Beside
an established client on one host, the demo's window trailed by 15 to 30 pixels of a
dragged window before its loop was turned round, and kept level after, but for a few
pixels in about one capture in twenty.
**Every picture says when it arrived** (*minor 16, 2026-09-24*). The session thread stamps
each message with the pass that completed it, the stamp travels with the unit to the decode
thread and with the picture into the queue, and the acquire hands it out on the monotonic
clock an application reads, so the application can time a picture from the network to its
own present. Measured that way beside the established client above, the user dragging
windows through both and presents not waiting for the refresh: from arrival to the present
returning, 1.32 ms at the median and 2.31 at the 99th percentile when the picture is handed
out as planes -- the read-back and the renderer's upload are most of it -- and 0.67 and 1.37
ms as a device handle; outside the decode and the copy, the library's own share is about 40
us. Pictures with this client's pointer motion in flight read the same as the rest, so input
sent on the session's thread does not hold the video back. A one-frame lag in one capture in
twenty needs only a mean difference of half a millisecond or so -- a twentieth of a frame at
the rates a drag runs at -- which is what the planes route's read-back and upload cost; by
handle the person at the desk saw this client's picture ahead about as often as behind.
**Release carries an optional fence**: a synchronisation object the application's device
signals when it has finished reading the picture, so a decoder writing straight into shared
memory waits on the application's GPU rather than on its CPU. A null fence means "reusable
now", which is the right answer for a picture that was copied.

**A picture leaves the library one of two ways, and the library chooses which it can offer.**
As **planes**: pointers, pitches and a format (`NV12`, `P010`, or the 4:4:4 layouts) into
memory the library owns for the lease, which every renderer can take and which is the path a
software or read-back decoder produces anyway. Or as a **handle**: a device-level reference
that the application imports into its own device, with an offset and a pitch per plane. The
application asks for a kind at creation and is told which it got; a decoder that cannot
export hands out planes.

**The handle has a kind, and the first kind is an opaque descriptor** (*built 2026-09-19*).
The vendor interface's decoded picture is not exportable: its pool is the interface's own
and a mapped picture is a transient pointer. So on that backend the four slots of the queue
are **exportable device allocations**, one descriptor each, and the backend does one
device-side copy into the slot in place of the host read-back -- the copy the read-back was
is gone, and what remains runs at the device's own bandwidth: **at 2560x1440 the read-back
was 0.5-0.9 ms a picture and the device copy is 0.09 ms**, and the repeats and skips the
read-back's jitter produced at 120 pictures a second (five of each in some seconds) are
gone with it. The copy is queued on the backend's own stream behind the interface's map and
the stream is waited for before the picture is unmapped, so acquire returns with the bytes
in place and a null fence stays correct on this kind; a real fence is a refinement rather
than a requirement. The descriptor is the kind NVIDIA's GL and Vulkan import as external
memory; the second kind, a buffer descriptor with its layout modifier, is the open stack's,
which can export the decoded surface itself with no copy at all but must then hold that
surface out of the decoder's pool for the lease, which is the fence's job; it comes later.

**Device slots are sized at the stream's size, never at the ceiling.** Device memory is
real where the host-memory slots' reserve is virtual: full chroma at sixteen bits is 200 MB a
slot at the ceiling, and a 1080p stream would leave most of it idle. A slot is allocated when
a picture of a new layout is about to be decoded into it, exported once, and the allocation
before it is freed then -- which is after the last hold on it was released, because the ring
lends a slot to the producer only once nothing holds it; so a slot the application holds is
never pulled from under it, which is the same rule the host-memory slots keep by never
reallocating at all. The frame carries its slot's descriptor and the allocation's ordinal,
and **the application's import is keyed by the ordinal**: descriptor numbers are reused once
closed, so the number alone cannot tell a new allocation from an old one, and an import whose
ordinal no longer appears may be dropped. The renderer this was built against imports the
descriptor as a buffer and fills its plane textures from that buffer at each plane's offset
and pitch, a device-side transfer costing 0.05 ms for two planes at 2560x1440 -- so the
pitch stays the library's and no tiled-image layout has to be negotiated.

Each picture carries what the header and the bitstream said about it: size, rotation (applied
by the renderer, not the decoder -- the picture arrives as the display was encoded, and a
quarter turn is a transform at present time), colour depth, chroma layout, the generation it
belongs to, and **whether its samples span the full range** (*added 2026-09-24*, minor 15).
The range is the parameter set's video signal type, read by the library's own readers for
the hardware backends and asked of the codec library for software; a set that says nothing
means the video range, as the standard infers. **The client asks and the host decides**
(*corrected 2026-09-24*; this said nothing on the wire negotiates it): the declaration's
full-range bit says the application's renderer takes it (§7, [01 §11](01-protocol.md)), and
a host that acts on the bit codes the full range when every seat declared it. One
established host does, at every codec and depth, measured at luma 0 to 255 with a fifth of
its samples outside 16 to 235, and codes the video range when the bit is clear; a host may
also leave it unmet. Nothing here converts it. The samples
leave as they were coded and the renderer is told, since a conversion here
would cost a pass over every picture and spend precision; a renderer that assumes the video
range draws such a picture with its blacks crushed and its contrast raised, which is how it
was found.

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

**Three decisions were deferred here and are now taken: none of them is in v1** (*decided
2026-09-19*, at C5's planning, on the numbers below):

- *A presentation pacer in the application, not the library.* Moonlight, an open client for
  a different protocol, sets the frame rate from the client and offers frame pacing as an
  option with its trade-off named -- lowest latency or smoothest motion; the same shape fits
  here as a deeper hold count on acquire, so an application that wants evenness over
  currency can buffer. Not built: the cadence numbers show nothing a pacer would fix at the
  rates that matter, and the library stays latest-wins.
- *A decode-lag keyframe request*, §5 below. Not built.
- *A presentation-rate hint and a sustainability event*, §9 below. Not built.

What every established client does instead, and what this one does: **the reader's lag is
the application's warning.** A client whose reader has been thirty or more messages behind
for sixty consecutive samples tells the person that the host's resolution or rate is too
high for this hardware, and clears the warning the moment the lag drops under thirty; status
carries the figure ([§9](#9-events-status-and-metrics)) and the demo draws the warning. A
decoder slower than its stream has no remedy but fewer frames, and the only lever for that
which every host honours is the application's own message.

**The numbers, recorded 2026-09-17** (this machine, this host, 1080p H.264 at 120 pictures a
second, a 120 Hz display, independent motion on the desktop; the C demo's own count of
presents, new pictures, repeats and skips a second, decided on at C5's planning):

| stream against presentation | pictures | repeats | skips |
|---|---|---|---|
| 120 against 120, ten minutes | 118 (mean 116) | 3, ninety-fifth percentile 11, at most 38 | the same |
| 60 against 120 | 60 | 60 | 0 |
| 120 against a 60-a-second poll | 60 | 0 | 60 |

The equal case is the beat above: two clocks at the same nominal rate slip one picture
against each other a few times a second, and each slip is one repeat and one skip. The
other two are exactly what the paragraph predicts, and neither drops a picture it did not
have to. The decoder's own time is [§9](#9-events-status-and-metrics)'s.

**The record the decision was made on, this phase's gate** (*2026-09-19*, this machine,
2560x1440 at 120 pictures a second, ten minutes a run, the preferences walked every hundred
seconds, two client windows and a spinning test window on the captured desktop): **the
reader was never more than one message behind** -- at most 16 ms -- on either backend, at
either depth, by either route, and against the established host over the internet; the
vendor backend by the handle route ran 120 pictures a second at 0.6-0.75 ms of decode and
0.09-0.14 ms of device copy with no skips outside the switching second; its planes route
0.5-1.0 ms of read-back with a few skips a second; the open stack 2.0-3.4 ms of decode and
1.8-2.4 ms of read-back, 115-120 pictures a second at eight bits and 19 skips a second at
ten, where the decoder is seventy percent busy and latest-wins does what it should. The
warning never fired. A switch costs the switching second about forty repeats: the keyframe
the host is asked for.

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

**A third trigger, for a reader that has fallen behind, was considered and is not built**
(*recorded 2026-09-16*, *decided 2026-09-19*; the record stays because the shape is
non-obvious). The two cases above are the established rule and they leave the slow-decoder
case of [§4.1](#41-what-latest-wins-costs-and-what-it-cannot-do) without a way out: the
backlog has all arrived, the decoder is sound, and nothing in it is a keyframe to skip to. A
request sent when the reader is more than a threshold behind, no announced keyframe is ahead
in the ring, and none went out in the last two seconds, gives the catch-up of
[§3](#3-the-receive-path) something to land on: the picture snaps to the present at the cost
of a keyframe from this host and of an encoder rebuild, every two seconds while the lag
lasts, from the established one. It is a divergence, and a correct one on this protocol --
an instantaneous refresh resets references, so nothing is lost by abandoning arrived
pictures -- but it does not cure a decoder that is slower than the stream; only fewer frames
do, which was §9's event. **The lag number, recorded 2026-09-17, which decided it**: under the simulator, against
this host's framing with a keyframe every 300 pictures, a decoder at half the stream's rate
falls 159 messages behind at the deepest and the catch-up discards 611 pictures over 1317
frames to land on each keyframe; a decoder that keeps up is never more than two behind.
Live against this host (which announces its keyframes and sends none periodically) the
reader was at most one message behind for ten minutes.

**A picture whose generation is older than the one the host last announced (opcode 29) is
stale**, from an encoder that no longer exists; the decoder is torn down before it is fed, so
that the new generation's keyframe, which carries its own parameter sets, builds a fresh one.

**A format change is a rebuild that re-feeds the unit once** (*2026-09-17*). A decoder that
reports the stream's format changed under it has not decoded that unit; it is torn down, a
fresh one is built for what the header names, and the same unit is fed again, so the picture
that changed the format is not lost. A second report on the same unit is a fault in the
backend, not a change in the stream. A backend that cannot be built at all ends the stream
with the stage named rather than asking the host for keyframes it would fail on the same way.

### §5.1 Backends, Linux, in order

| backend | reached through | hands out |
|---|---|---|
| VA-API | the driver's own interface, loaded at runtime | planes by read-back, full chroma included where the driver decodes into a layout the library reads (*2026-09-22*, C9); the surface's own buffer descriptor, later |
| NVDEC | the vendor's decode interface, loaded at runtime | planes by a device-to-host copy; an exportable slot filled by a device copy (§4) |
| software | the machine's own libavcodec, loaded at runtime and only when it is an LGPL build (*built 2026-09-21*, C8) | planes, converted from the decoder's own layout during the copy |

The choice is the application's, by kind and render node, as the listing reports them;
unset, the first render node the open stack decodes on, then the vendor interface on any of
its devices, then software, so on a machine with both hardware interfaces the vendor
backend is chosen by naming it -- or by asking for handles, which only the vendor backend
exports -- and a machine with any hardware decoder never reaches software. With a render
node named, the automatic order tries the open stack on that node, then the vendor's
interface on the card behind it, then software (*corrected 2026-09-21: it stopped after the
open stack, and the handle kind with a node named took any vendor device*). The device is
named as a render node for either hardware backend; the vendor's resolves it to the card
behind it; for software it may name the directory the pair was found in. **What can be
opened is listed** (*2026-09-19*): a fixed table of slots -- the open stack on each of eight
render nodes, the vendor's device by ordinal, the codec library last -- each probed exactly
as creation probes it and alone, with what it decodes, its size limits and whether it
exports a handle, so an application shows a menu or picks by capability rather than
guessing at a name; a slot with nothing behind it says so and why (*corrected 2026-09-22*:
the first shape re-probed the whole machine on every call). Each row carries a label for a
menu -- the interface and the card's maker, `VA-API [Intel]`, `VA-API [AMD]`, `NVDEC
[NVIDIA]`, `libavcodec [LGPL]` -- and the driver's own words beside it (*2026-09-22*, minor
13). Nothing is linked: a machine
without any of the three refuses with the stage named, exactly as a host without an encoder
does. Nothing this library loads writes to the application's standard error: the open
stack's own messages go to the library's log, per display.

**Software decode is the machine's own libavcodec, and only an LGPL one** (*decided
2026-09-21*, [impl-plan-client.md](impl-plan-client.md) C8). Nothing is shipped or built:
the pair (`libavutil`, `libavcodec`) is looked for in the environment (`LOWLAT_FFMPEG_DIR`,
`LOWLAT_FFMPEG_VERSION`), beside the running executable, then the dynamic linker's own way,
majors 4 through 9 with the highest that opens winning, and before any other entry point
is called it is asked its licence; a pair that does not answer `LGPL` is closed at once and
reported as such, so on a distribution whose build is GPL the software row does not exist
and the copyleft rule of [impl-plan.md](impl-plan.md) gate 4 holds by construction. **A
library built with the `gpl-libavcodec` feature takes a GPL pair as well** (*added
2026-09-21*): the default build never does; a build that opts in is its maker's combination,
to which the GPL's terms apply, and it says so through `lowlat_features` so an application
can tell the two builds apart; a pair answering `nonfree` is refused by every build. No
header is pinned: the surface relied on is the same on every major accepted and is checked
at load against the library that loaded; everything numbered that has moved between majors
is resolved by name. The decoder's three-plane pictures leave in the same four formats the
hardware backends hand out, converted in the copy that hands them out, so no fifth format
reaches the application and no flag says where in sixteen bits a ten-bit sample sits: it is
in the high bits whatever decoded it.

**As built** (*2026-09-21*): a codec counts as decoded only if its decoder opens, not if its
name is known -- a pair at hand carries an H.264 decoder that refuses to open without a
device behind it, and the listing must not promise it. The library's pixel formats are
resolved by name because they are numbered differently on a 4.x pair than on 5.x and later
(measured: the ten-bit formats sit two higher there). Slice threading, capped at four and by
the machine's parallelism, and nothing else: the low-delay flag was measured on every
committed clip and changed nothing, so it is not set. **The workers never outnumber the
machine's threads, and no thread of the library's is raised above the application's**: the
library runs inside the application's process, and workers past the cores, or above the
window's own thread, starve the message pump on a two-core machine and read as the
application hanging -- a rule written down (*2026-09-22*) with its test, after a client
generation compared here shipped exactly that fault. Every committed clip decodes bit-exact
through an LGPL 7.1 pair and the second codec's through an 8.x one, full chroma and ten-bit
included, with one documented exception: **a stream that reorders more than it declares
loses a picture at each depth the library discovers** -- a picture arriving for an earlier
place than the last one put out is dropped and the buffer deepened -- where the library's
own readers hold such a picture; a stream that declares its reordering, or does not reorder,
loses none, and no host compared here sends the undeclared kind. The output delay is the
stream's own: none for a stream without bidirectional pictures, its declared depth otherwise.
The conversions at 2560x1440 cost 74 us (NV12), 168 (P010), 147 (three planes at eight bits)
and 554 (three planes at sixteen), measured on the development machine; the per-unit path
allocates nothing on this side. Ten minutes from this host at 2560x1440 H.264, 120 pictures
a second with a moving scene: decode 1.8 ms at the median and 2.2 at the 95th percentile,
2.7 at most; the conversion 0.16 and 0.19 ms; the reader at most one message behind; the
whole client process at 31 percent of one core and 290 MB resident. Ten minutes of HEVC
ten-bit the same way: decode 2.98 ms at the median and 3.46 at the 95th percentile, the
P010 conversion 0.42 and 0.71 ms, 120 pictures a second decoded throughout, 55 percent of
one core, 360 MB resident. From the established
host over the internet, at its own cadence and bitrate (few, large pictures on a still
desktop, up to 60 KB each), decode 2.8 ms at the median and 7.9 at the 95th percentile:
the software decoder's time follows the bits in a picture, not the rate.

**Full chroma on the open stack, where the driver's layout is one the library reads**
(*built 2026-09-22*, C9). The open-stack backend refused every range-extended stream because
no device it was built on could verify the surface it would read back. A discrete Intel
part can: its driver decodes HEVC Main 4:4:4 at eight and ten bits into packed layouts --
V, U, Y and a fourth byte per sample, and a word per sample with ten bits each of U, Y and
V -- and the four committed full-chroma clips come back bit for bit through the
range-extension parameter structures and an unpacking in the read-back copy, into the same
two full-chroma formats the vendor's and the software backend hand out. The capability is
reported only where the driver offers, for the profile, a layout the library reads (planar,
or the two packed ones), so a part that lists the profile with another layout is not asked
for it; the stream's own parameter set settles depth and chroma, the declaration only
guesses, and a stream the range extensions allow but no profile here takes -- half chroma,
the extension tools on a 4:2:0 stream -- is refused outright as before.

**One decoder is chosen at creation and there is no automatic fallback to another**
(*2026-09-19*, *amended 2026-09-21*). An established client offers the second codec and
both colour axes as preferences and, when its hardware cannot decode what arrives, quietly
moves to a software path; here the preference is masked by capability before it is
declared (§7), so what arrives is what was declared, and a stream the built decoder cannot
take -- which can only be one the client did not declare -- ends the session with the
decoder's status and the stage named. A quiet switch to a slower decoder ships a degraded
stream without telling anyone, and the application cannot choose what it does not know
about. **The application may choose, mid-session** (*built 2026-09-21*, C8):
`lowlat_client_set_decoder` names another kind and node, is refused with the stage when it
does not open and changes nothing then, and otherwise is one act -- the declaration
re-masked by the new decoder's capability, the running decoder torn down, the new one
opened on the decode thread, and exactly one keyframe request with the reinitialisation
argument, the first of the two request cases of §5 -- so the picture resumes at the next
keyframe and the queue never closes. **The request is the replacement's, made once it
exists**: the old decoder is torn down without asking, the new runtime opened, and the new
decoder asks as its first act, so a keyframe never arrives for a decoder that is still
opening and a runtime that fails to open costs the host nothing; the units that arrive
meanwhile are dropped by the rule that ignores everything but a parameter-set-led unit
while no decoder exists. The frame kind is the queue's shape and stays the creation's: a
session of the handle kind refuses the call, because its device slots are bound to the
device. A decoder that cannot serve the frame kind is refused where it is asked for, never
answered with a frame of another kind. Measured against this host at 2560x1440, the three
backends walked every hundred seconds, five moves: each answered by exactly one keyframe
(the host's log shows one reinitialisation per move and nothing else), the picture back
within the second, the move to the vendor's costing about 35 pictures for its runtime's
opening -- some three hundred milliseconds -- the move to software 9 and to the open stack
2, and nothing behind by more than one message before or after any of them. Against the
established host over the internet the same five moves cost one encoder build each on that
host's own log -- six builds in all, the connect's and one per move, nothing else.

**The creation-time probe builds a real decoder per combination** (*2026-09-19*) of codec,
chroma and depth, and destroys it, rather than trusting a capability query: the vendor
interface's query has reported a combination the device then failed to create, inside the
driver, where nothing catches it. A device that fails a combination fails it at creation,
in the probe, with the stage named, and never mid-stream in the application's process; and
what the probe found is what the declaration is masked with.

**The library reads the bitstream itself** (*built 2026-09-17*). The device interfaces on this
platform decode a picture from its parameters and its slices; reading those out of the stream
is not theirs. So the library parses both codecs' parameter sets and slice headers in full,
derives the picture order, keeps the decoded picture buffer, builds the reference lists, and
hands the driver the picture, quantisation-matrix and slice parameters it asks for, with the
slice data beside them. That is most of the decoder; the backend beneath it is the device's
context, a fixed pool of surfaces the picture buffer indexes, and the read-back. Nothing is
allocated per unit: the parser's state, the picture buffer and the parameter staging are
fixed arrays sized by the standards, and a unit that needs more than they hold is refused,
never truncated. A stream that declares nothing about its reordering is held back only as
far as it proves it must: a bidirectional slice or a jump in the picture order holds one
picture, a picture that arrives late holds one more.

**The read-back is the cost of planes** (*measured 2026-09-17*, this machine, 1080p, the
open-stack driver): about 2 ms a picture live, as much as the decode itself, and nearly all
of it the driver's own mapping of the surface rather than the copy out of it -- the copy is a
quarter of a millisecond, and a streaming-load copy that makes it a twentieth moves the live
figure six percent, so it was measured and not taken. The lever is the handle path of §4,
which has no host copy; it arrived with the second backend (*2026-09-19*: at 2560x1440 the
vendor backend decodes in 0.6 ms and its read-back costs 0.5-0.9 ms, the open stack's 2.1
and 2.0; with the handle the read-back becomes a 0.09 ms device copy).

**The vendor backend is driven from the same readers** (*built 2026-09-19*): the picture and
slice parameters its interface takes are filled from the jobs the open-stack backend
stages, the interface's own parser unused -- it would be a second reader and a second
picture buffer beside the ones every clip is checked against. Every committed clip decodes
bit for bit on both backends, full chroma included on the vendor's.

**Full chroma is the second codec's, in the range-extensions profile, and the readers read
it** (*built 2026-09-19*): the HEVC reader admits that profile at 4:2:0 and 4:4:4, eight
and ten bits, and reads both extension syntaxes, whose fields the devices' picture
parameters carry. H.264 stays at eight-bit 4:2:0, which is every device's whole answer for
it. Two planar layouts join the two that exist, with the third plane already in the
picture's shape. On the open stack the profile is asked for and refused where the device
lacks it, which is every device this was built on; no read-back is written for a surface
layout nothing here can verify, and the capability stays false until a device says
otherwise.

## §6 Sound

Channel 2 carries fifteen bytes of header and one packet: the channel mask, samples per
channel, the rate, a codec byte, and the channel count ([01 §11.4](01-protocol.md)). The
codec byte is read: **2 is uncompressed PCM and is handed out as it is; anything else is Opus**
and is decoded. A change in mask, codec or count rebuilds the decoder.

**The library orders and decodes; the device paces** (*revised 2026-09-18*, at C4's
planning; an earlier draft put a playback window in the library). The receive loop copies
each packet once, off its ring into a pool of 32 slots, and stamps it with its arrival;
`lowlat_client_acquire_audio` takes the next one in order and decodes it **on the caller's
thread** into the caller's buffer -- one packet a call, signed sixteen-bit stereo at 48 kHz,
as many frames as the packet held (960 for a host at 20 ms; at most 8000, the uncompressed
ceiling), with the packet's age at hand-over reported in status. A full pool drops the
newest packet and counts it, which is a reader that has stopped calling; nothing here
waits for the right moment, because nothing here has the clock that decides it.

That clock is the application's device, and its buffer is the **playback window**: sound
is queued on it as packets arrive and it plays from a floor and flushes at a ceiling --
too empty is a gap, too full is latency -- so the floor is the latency and the pair is the
budget against drift and jitter, in each direction. A desktop client runs about 75 ms to
150; a phone or a browser about twice that, paying latency for coarser periods and worse
paths. The host's source is the clock ([05 §9](05-host.md)), so the window drifts against
the device's at the rate of their difference and resyncs on the order of once every twelve
to fifty minutes (*measured 2026-09-18*: 40 ppm between this machine and the established
host's, the desktop window flushing every 35 minutes); no client can remove that, a client
that tries by resampling to the device makes a feedback loop, and a library that ran a
second window over the device's would only flush against it. A decoder is built from the stream's own header and rebuilt
when its mask, codec or channel count changes; a stream that is not stereo at the
protocol's rate is refused per packet and counted. The pipeline hop between the wire and
the device -- one wake and one decode, a fraction of a millisecond -- is not where the
latency is.

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
| 9 cursor | decodes the picture if one came or is named from the cache, and raises a cursor event with it at native size, the hotspot in its pixels, the *suppressed* flag and the position in window units; a name the cache does not hold delivers the rest without a picture; the forget bit empties the cache; the first stream only |
| 10 disconnect | records the status the host gave and ends; zero is not an ending ([01 §11.2](01-protocol.md)) |
| 16 blocked | raises the blocked or unblocked event |
| 17 user data | hands the body to the application with its sub-identifier, opaque ([01 §11.2a](01-protocol.md)) |
| 20 rumble | raises the rumble event for the pad named, the two motors as eight-bit values |
| 21 encode latency | stores the host's figure for the stream named |
| 25 guest list | hands the body to the application with the recipient's own number, opaque; the number goes into status |
| 27 stream ended | raises the stream event with the reason |
| 28 host mode | stores it |
| 29 encoder generation | stores it per stream, for §5's staleness rule |

Anything else is ignored. **A clean departure is opcode 10 with a zero status**, given a
moment on the reliable channel before the session goes away; a client that breaks sends
nothing, and a host learns it from its delivery deadline.

**The pointer's picture is decoded here and scaled nowhere here** (*built 2026-09-19,
evening*). The picture travels as a PNG ([01 §11.2](01-protocol.md)); the library inflates
and unfilters it -- 8-bit RGB or RGBA, non-interlaced, up to 512 square, which decoded is
the 1 MiB an established client's buffer holds -- and anything else is refused with the
picture dropped and the position, the flags and the hotspot still delivered. The
initialization declares that this client caches, so a host names a picture it sent before
by the checksum of its bytes rather than sending it again: the library keeps a hundred by
checksum, empties the cache when the host's forget bit says its own is empty, delivers a
name it does not hold as an update without a picture (counted in status), and takes the
size and hotspot of a name that carries no size from what was stored with the picture. The
decoded picture is handed to the application from a buffer the handle owns, valid until
its next poll, so no application sizes a scratch buffer at the ceiling of a picture it
sees a few times an hour; and **the picture already delivered, named or sent again, travels
as its checksum alone** (*built 2026-09-19*): this host repeats the name on many updates
that change nothing about the picture, and decoding it on each would be work for nothing
-- the application keeps the picture it was given. **Scaling is the application's**: an established client scales
the pointer by the viewport it drew into, or by its display's scale, so a pointer from a
host at twice the scale shows at the size it has in the picture and shrinks with a
letterboxed window. The library cannot do that -- it does not own the toolkit's cursor, and
a display server draws a cursor at the size it was given -- so it hands over the native
picture with the hotspot in its pixels, and the application resamples the two together by
the ratio of its rectangle to the picture, the same ratio [§8](#8-input) maps positions
through. The suppressed flag -- the pointer withheld because the host is being driven by
touch -- is delivered as itself and is not relative mode ([§8](#8-input)); the application
hides its own pointer for it.

**The guest list is opaque to the library.** Its own number arrives in the message's
header and is all the library needs: permissions gate nothing on this side, because the
host drops what it does not permit and a stale list would only refuse input the host would
take; and the figures in the body are the application's panel ([§9](#9-events-status-and-metrics)).
So the body goes to the application unread, with the recipient's number beside it, and the
application finds itself, reads what it wants and drops a body it cannot read -- as it does
with any application message. An earlier draft had the library parse the list for the
client's own permissions; that put a reader for one application's schema into a library
whose host half sends the same schema blind, for nothing the library would act on.

**What the declaration says is a preference masked by capability** (*2026-09-19*). The
application names what it would like -- the second codec, ten-bit colour, full chroma -- in
the video block of the attempt's configuration; the library ANDs that with what the decoder
it opened at creation decodes (§5.1) and declares the result in the initialization's flags
and the two secondary declarations, with the wire's own implication that depth and chroma
imply the second codec and neither is declared without it. **The fourth preference is the
range** (*minor 17, 2026-09-24*): the application says whether its renderer takes the full
range, and the library declares that as asked, since the decoder decodes either range alike
and converts nothing, so there is nothing of it to mask. Defaults off: a client of ours at its
defaults asks for H.264 in the video range, which a renderer that never reads a picture's
range draws right, and full chroma at ten bits is nearly twice the bytes of the same picture
at eight-bit 4:2:0 -- neither is a choice the library makes for the application. A change mid-session
goes out as the encoder configuration with the reinitialisation argument, paired with the
decoder's teardown -- the first request case of §5. What the stream then turns out to be is
the decoder's to follow, from the header and the parameter sets, and status carries all
three: asked, declared, decoded.

**The client reports its decode time on a time cadence, both kinds** (*2026-09-19*). Every
two seconds from the moment the session is established, the session thread sends opcode 21
twice in the argument order a client uses ([01 §11.1](01-protocol.md)): the video kind with
the decode thread's smoothed figure for decode and hand-over per picture, and the sound kind
with the figure `acquire_audio` smooths on the application's thread, each an exponentially
weighted average with a tenth's weight on the newest sample, in microseconds, zero until
something has been timed and sent anyway. A time cadence rather than a count of pictures,
so a still desktop still reports, as this host does in the other direction. The message
does one more thing: the round-trip estimate samples only when the host acknowledges
something this client sent, and after the start-up burst a client that sends nothing keeps
its first estimate for the whole session; a report every two seconds is what keeps it live.

## §8 Input

The application reports what happened in its window; the library turns it into the wire's
vocabulary ([01 §11.1](01-protocol.md)) and applies the rules every client applies
(*revised 2026-09-18*, at C3's planning):

- **The application says where it drew the picture.** `lowlat_client_set_viewport` takes the
  rectangle, in the same units as the positions the application reports, and that is the
  whole of what the library knows about the window: no fit is computed here, because
  stretching, shrinking, a percent scale and a rotated picture are all the application's
  ways of producing one rectangle, and a display scale factor never enters because the
  rectangle and the positions share a space by construction. The picture's own size comes
  from the stream's header, so the two ends of the ratio have different owners and the
  application cannot describe the picture wrongly. A zero rectangle means there is nothing
  to aim at, and absolute motion is not sent until one is set.
- **Pointer positions are transformed into the picture's own pixels.** A window position is
  mapped through the rectangle into the picture, in the orientation the picture is shown
  (a rotated stream's coordinates are swapped back), then clamped to the picture; a position
  one short of the far edge is bumped onto it so the far edge is reachable. **Relative
  motion is sent as the device reported it**, whatever size the picture is drawn at: a
  delta is the hand's motion in the mouse's own counts, not a distance in the window, and
  the host moves its pointer by it as by a mouse of its own (*corrected 2026-09-24*: this
  said deltas were scaled by the ratio of the picture to the rectangle, and so they were
  until then; a picture stretched to a larger window dragged and aimed slower than the
  hand, which a person noticed dragging a window beside an established client. An
  established client scales only relative motion its toolkit made up from a device that
  reports positions, a tablet or a remote pointer, whose steps are the window's pixels; an
  application with such motion scales it before handing it over).
- **A button press outside the picture is not sent; its release always is**, so a drag that
  leaves the window ends cleanly on the host. The guard is evaluated at the press's own
  position, which the application reports with the press.
- **Losing focus is the application's to report**, as the release-all message, so nothing
  stays held on a host whose window is no longer in front. Pads are centred by that, not
  unplugged ([05 §7](05-host.md)).
- **Keys are usage codes and the modifier mask is the event's own**, in the wire's bit
  numbering; the application supplies both, because only its toolkit knows the lock state a
  host reads for lock-key synchronisation. A code of zero is dropped. A held key's repeats
  are forwarded as presses; a host tolerates them.
- **Pads are sent as whole states**, one message per state the application reports, with an
  unchanged state for the same identifier not repeated; or as single button and axis
  changes. The host takes both and does not require one.
- Pen and touch are deferred, as they are on the host.

**A DualShock 4 or a DualSense is sent as its own report** (*C7, 2026-09-20*; the
wire is [01 §11.1](01-protocol.md) and §11.2, the surface [06 §3b](06-api.md)). The
application hands the library the input report exactly as the device delivered it, USB or
Bluetooth form, under a pad identifier of its choosing, with the product named; everything
else is the library's:

- **The standard state is derived from the report and sent beside it**, the report first,
  deduplicated as a whole state is. A host that reads reports has its slot and drops the
  state; a host that does not has a sixteen-button pad and nothing else. The application
  never sends a state for a report pad, and the library refuses the other family for an
  identifier until it is unplugged.
- **The Bluetooth form is normalised to the USB form** on the way in (the identifier and
  checksum stripped, the calibration report's Bluetooth identifier rewritten), the transport
  remembered per pad, and what comes back is re-framed with the identifier and checksum a
  wireless pad expects. The host sees one form.
- **Feature reports go first.** Calibration and firmware, read from the pad by the
  application and sent under the same call before the first input report; the host's
  driver asks for them when the device appears and scales the motion sensors by the
  calibration one, so a default there is a gyro that drifts. Any subset is accepted; the
  pairing report is the host's ([05 §7.2](05-host.md)).
- **A DualShock 4 travels twice**: its whole report in the form only a host that reads
  products takes, and the ten-byte touch block an established host's DualShock mode reads,
  so that host gets the touchpad too ([01 §11.1](01-protocol.md) says why the whole report
  cannot be framed as the established one is).
- **What the host's device was written comes back as an event**, the output report or a
  feature write, in the pad's own framing, lent until the next poll; the application writes
  it to the pad. The rumble event still arrives when a host sends the rumble message (an
  established host in DualShock mode does), and the application decides what to write for
  it -- its own motor-only report, keeping the lightbar it last wrote, is the right answer.
- **Whether to send reports at all is the application's policy.** An established host reads
  a DualSense's report only in a mode its owner set, and the library cannot tell one host
  from another -- nor can the demo, since the host list names the same build for every
  host, so the demo sends reports when told to (`LOWLAT_PAD_RAW`) and states otherwise.
  Tried against an established host (2026-09-21): in its DualSense mode a DualSense works
  whole, both ways -- buttons, motion, touch, rumble, the lights; in its DualShock mode a
  DualShock 4 works but for the motion sensors, which that mode never reads
  ([impl-plan-client.md](impl-plan-client.md) C7). This library's own host reads the whole
  report for both products, so the motion sensors of either reach it, on the device it
  presents or through the application's sink alike ([05 §7.2](05-host.md)).

The toolkit the demo is built on has no HID path on Linux, so the demo reads the pad's raw
node itself beside the toolkit's controller events and drops those events for the pads it
reads raw; nothing in the toolkit is patched.

**Relative mode is the host's to announce and the client's to enter.** The cursor message
carries it in either of two bits; on the transition the library raises an event and the
application confines and hides its pointer -- a warp on the transition out, to the position
the event carries in window coordinates, not on every update, or the pointer fights the
application's own motion.

**Between the application's thread and the session's** the messages travel a fixed ring; a
full ring drops the newest message and counts it, and never blocks the caller. A ring that
fills is a session thread that is not running, and by then what the host holds is its own
release-all problem.

**The application's event loop must not be paced by its display** (*found at C3's gate*). A
toolkit that reads one pad event per pass of its loop delivers a moving stick at the loop's
rate, and a loop that presents with vsync runs at the display's; the kernel's queue then
fills and plays on for seconds after the hand stops. Presentation belongs on a thread of its
own, paced by the display through `acquire_frame` and present, while the event loop runs at
the toolkit's own cadence -- the shape every established client has, and the shape the
application's sound thread takes too (§6): a long wait on `acquire_audio`, the packet straight
onto the device.

## §9 Events, status and metrics

Events are polled ([06 §5](06-api.md)): candidate found, established, ended with an outcome,
cursor, relative mode, blocked, rumble, user data, the guest list, stream ended, host mode.
Status carries what the session is doing and what the decoder is: the round trip, how far
behind the reader is, the decode and hand-over times, the queue depth, the host's encode
time as it reported it, the sound figures, the declaration and the stream, and this
client's own number in the room.

**The client's metrics have a shape of their own** (*built 2026-09-19, evening*; an
earlier draft put them on the host's structure). The host's per-guest figures are a
sender's -- fragments put on the wire, the resends it took on a negative acknowledgement
and on its timeout, its congestion events, its window and stale count -- and a receiver can
measure none of them; reporting zeros under those names would describe the wrong end. What
a receiver can count, per channel: the fragments that **arrived**; those that arrived
**late**, behind a later fragment, which on this transport is a retransmission or a
reorder; **duplicates** and **out-of-window** drops; the **negative acknowledgements it
sent**; bytes and messages. And **a recent-loss figure**: late arrivals over arrivals in
each one-second sample, folded into an average with a thirtieth's weight on the newest
sample, so it reads over about thirty seconds and settles at the path's loss rate rather
than at the session's total. Per session, the round trip and how long the session has been
up. The host's own figures for this guest -- what it sent, what it resent, its rate and its
round trip -- reach the application through the guest list ([§7](#7-initialization-and-the-control-vocabulary)),
so one panel's two sides are the host's `lowlat_metrics`, as the host reports them, and the
client's `lowlat_client_metrics`, as the client measures them; the pairs that describe one
path from its two ends are the round trip, the rate, the decode time the host re-publishes
against the one the client reports, and the negatives the client sent against the resends
the host took on them (related, not equal: one negative can name several fragments).

**A presentation-rate hint, and an event when the decoder cannot sustain the stream, were
considered and are not built** (*recorded 2026-09-16*, *decided 2026-09-19*; the shape is
kept). There is no wire-level channel for the rate a
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
does against this one. Decided with the lag trigger of [§5](#5-the-decoder-and-when-a-client-asks-for-a-keyframe):
the numbers below show a decoder that keeps up, and the reader's lag in status is the
application's warning ([§4.1](#41-what-latest-wins-costs-and-what-it-cannot-do)).
**The decoder's own numbers, recorded 2026-09-17** (this machine's open-stack decoder,
1080p H.264): decode about 2.0 ms a picture at the median and 2.3 at the ninety-fifth
percentile, read-back about the same, so a 120-picture stream costs the decode thread about
half its time; the demo's cadence is the display's whatever the stream does.

## §10 Threads

One receive loop (the shell's), which owns the session and its rings and hands access units
to a decode thread per stream through a pool by index -- one copy, off the ring, and never
another by the library (the driver's interface takes the slice data as a buffer of its own,
which is a second copy inside the driver and not one this design can remove); and nothing
that presents or plays: presentation is the application's thread calling acquire
(*corrected 2026-09-17*: an earlier draft had the decode thread draining the ring, which is
the established client's shape and needs a lock on the session that nothing here takes), and
sound is decoded on the application's thread inside `acquire_audio` (*corrected 2026-09-18*:
an earlier draft had a sound thread; a decode of a twentieth of a millisecond earns no
thread, and the second hand-off it would need is the one wake and copy the design saves).
The decode thread never blocks the receive loop: a full pool leaves the backlog in the
receive ring, where the catch-up of §3 sees it; the sound pool never blocks it either, it
drops. The software decoder's slice workers are the one addition, capped by the machine's
parallelism (§5.1). Every rule of [02](02-io-shell.md) applies -- raw wakes for raw waits,
no elevated priority inside the library, teardown that wakes every waiter.

## §11 The relay

*Planned, built and closed 2026-09-23 as C10 ([impl-plan-client.md](impl-plan-client.md)),
minor 14. The rules are [03 §7](03-connectivity.md); this section is the client's side of
them.*

**The client allocates the relay, and the host never learns there is one.** An application
that cannot reach a host directly -- a probe timeout, or a host it knows sits behind one
forwarded port -- configures a relay with the attempt: the server as `host:port`, resolved like
the reflexive servers, and a username and password. The credential is never logged and is
cleared when it is dropped.

**A relay configured makes the attempt a relay attempt**, and a relay attempt is relay-only:
the socket talks to the relay alone, the attempt's candidate events carry the relayed address
and then the readiness marker and nothing else, and every check goes through the relay. The
relayed address goes out marked as a server-reflexive candidate, and its event is raised only
once the relay's own address is permitted, so a host on the relay's machine has its first
check pass.

**The relay is one more framing on the same socket**, not a second transport: recognised by
its source before anything is classified, unwrapped there, and wrapped on the way out for
everything bound to the relayed peer -- checks, their answers and records alike -- so no answer
can leave outside it. Its renewals are timers among the session's own, on the receive loop's
thread, and nothing waits on the relay.

**What the application sees.** Status carries the relayed address and whether the path is
relayed. Three outcomes end an attempt the relay failed -- unreachable, refused, lost
([03 §9](03-connectivity.md)) -- and the library retries none of them. A clean leave releases
the allocation, **after the departure**: the relay's own requests leave ahead of anything else,
so a release made at once would let the relay go before the departure crossed it, and it is
made once the departure has had its grace instead. The session thread raises the relayed
address the pass it becomes ready rather than on the reflexive candidates' slower cadence,
because the pass that readies the relay may be the last for a while.

**The media follows the host.** Once there is a path, media goes to wherever the host's
authenticated records come from, and a channel is bound to it; a host whose traffic leaves
through its own translator is checked at one address and speaks from another.

**What it costs**: one more hop, whose far leg is local to the host's machine -- 0.8 ms of
round trip against a direct session on the same pair, measured before this was written with an
earlier implementation; 27 ms at the median through the same relay with this one -- and 4 bytes
a datagram on the client's leg once the path is bound to a channel, 36 before that.
