# Changelog

Newest first. One entry per phase; approach changes and gate revisions go in
[impl-plan.md](impl-plan.md) instead.

## 5: encoder and Gate A (in progress)

**Added**

- Vendored codec headers under `third_party/nvcodec/`, and the encoder FFI
  generated from them into `lowlat-encode`. No crate dependency is added.
- `lowlat-common::dynlib`: opening a shared library at runtime and resolving
  symbols from it, one implementation per platform.
- Loading the encoder runtime, with the interface version checked once at load.
- Loading the compute runtime, enumerating devices by bus address, and
  retaining a primary context.
- Opening an encode session against that context, and querying what the
  hardware will actually do.
- Configuring the encoder for low latency, with the colour description the
  wire requires, and changing its bitrate live.
- Registering an input surface, submitting a picture, and collecting the
  encoded access unit.
- Vendored display headers under `third_party/libva/`, their bindings, and the
  second backend's runtime loading, display binding and profile query.
- The second backend's capability query, encode configuration, surface pool and
  context.
- A bit-level writer for parameter sets: exponential-Golomb, fixed-width
  fields, trailing bits, and start-code escaping.
- The sequence and picture parameter sets themselves, carrying the colour
  description one backend has no other way to state.
- The second backend's encode path: a picture is submitted, the parameter sets
  and the slice header travel with it as packed headers, and a finished picture
  is collected as a probe rather than a wait.
- Live bitrate change on the second backend, carried with each picture, so it
  reinitialises nothing and forces no refresh.
- `lowlat-capture`: the synthetic frame source, planar 4:2:0 with a moving bar
  and a static colour block, and the frame type both encode backends take.
- Upload paths on both backends, each writing a frame into its own input
  surfaces at the surface's own stride.
- `scripts/check-encoded-frames.py`, which decodes a dump and checks each
  picture against the frame index that produced it.
- The encoder trait, and one shared collect result, with both hardware
  backends implementing it and one generic loop driving both.
- Predicted pictures on the second backend: reference bookkeeping, the
  slice header for both picture kinds, and parameter sets that travel
  with a refresh rather than with every picture.
- The encoded-frame pool: fixed slots, one copy per frame however many
  guests take it, and a hold per guest that releases itself.
- `lowlat-common::sync` made public, so a second crate building a
  cross-thread handoff shares the shim the model check swaps out rather
  than keeping a second copy in step.
- The per-guest delivery gate: the window ceiling, the skip-until-keyframe
  latch, the running-maximum retest, and a throttled global keyframe.
- The video packetiser: a stream's fixed facts, the ten-byte header ahead
  of each access unit, and the message the send ring takes.
- The initialization parser in the core: the eight-key body, its sentinels
  and its flag bits, allocation free.
- Session negotiation in the host: the five-second deadline, the encoder
  configuration message, and the encode-latency and generation cadences.
- The bitrate budget: a ceiling divided by the guests on a stream, the
  minimum across their controllers, and a deadband before reconfiguring.
- Two-channel ring geometry per guest, sized from the largest frame the stream
  can produce, and the control channel a guest declares itself on.

- The encode loop: one capture and one encode serving every guest, the seats
  guests take on it, and the daemon wiring that starts it.
- The two cadences a stream owes its peer, sent from the guest that carries
  them: the encode latency every thirtieth frame, and the encoder generation
  once, on the frame after the encoder is ready.
- `lowlat-host::timing`: per-stage percentiles, recorded as a store into a
  fixed ring and sorted only where a report is asked for.

- A guest's refresh request is honoured: a peer that cannot decode asks for a
  picture with no history behind it, and the request now reaches the encoder
  through the gate's throttle.
- Live diagnostics on a streaming guest: what it declared, what it is being
  sent, and what it is still sending back.

- A peer that says it is leaving is taken at its word: the control channel
  carries the notice, and the seat, the port and the share of the bitrate
  budget come back at once rather than two minutes later.

- The video header's third flag bit is the colour depth, not a keyframe
  marker, and we no longer set it.

- The refresh picture's cost, measured at each quantiser floor, as a test
  rather than as a remembered figure.

- A band of unpredictable detail in the synthetic source, off by default, so a
  frame can be made large enough to need more than one fragment.
- The frame rate is declared to the encoder, which is what a bitrate is spent
  at and was never being said.

**Notes on the frames that never fragmented**

- **Every message we had ever sent fit in one fragment**, so the fragmenting
  path, a peer's reassembly and the window arithmetic had never met a message
  that had to be split. Resolution does not fix it: a bar on a flat field is
  trivially compressible at any size. The source now takes a band of detail
  derived from the frame index, which an encoder cannot predict from the frame
  before it.
- **Off by default, and that is deliberate.** Every latency figure and every
  refresh size on record was measured against the flat picture; content that
  changed underneath them would invalidate them silently.
- **It found a defect on its first run.** The stream ran at exactly twice its
  configured rate at every setting. The encoder was never told the frame rate,
  so it budgeted bits for its default of thirty frames a second and received
  sixty. A congestion controller actuating through that is wrong by the same
  factor and would push a path into loss while believing it was inside budget.
  The flat picture could never have shown it: at a tenth of a megabit nothing
  was near the target.

**Notes on a burst that never existed**

- **The vendor backend's 2.4 MB refresh was a length, not a picture.** A
  collect racing the driver reported a size the encoder had not written; the
  same picture held 651 bytes. The race was found and fixed the following day
  and the plan paragraph was never revised, so a fixed defect stood as a gate
  condition. Measured now: 651 bytes against a raw frame of 3110400, under a
  thousandth.
- **The floor is the only bound that moves anything.** A quantiser ceiling and
  an initial quantiser were both swept and neither changed the refresh or the
  quantiser it was coded at, so neither is configured.
- A number nobody re-measures is a memory. This one is a test now.

**Notes on the flag that was not a keyframe**

- **A peer built a ten-bit decoder for our eight-bit stream and failed every
  picture**, on one decoder family out of four. We set that bit on every
  keyframe because this project's own protocol document called it a keyframe
  flag and argued that setting it was more informative and free.
- **The evidence against that reading was already in the same paragraph.**
  Across 4883 recorded video messages the flags byte was identical on every
  one, including the two whose first unit was a parameter set. A host that
  never sets the bit on its own keyframes is not describing keyframes with it.
  The document noticed the pattern, concluded that peers are unreliable about
  a keyframe flag, and then licensed us to set it anyway.
- **Only one decoder family reads it**, so three rendered our stream happily
  and the fourth reported a decode error rather than a mismatch. That is what
  made it look like a defect on the peer's side.
- Keyframes are now classified from the bitstream and from nowhere else. The
  classifier used to check the bit first, which would have called every
  ten-bit predicted frame a keyframe.

**Notes on the first stock client to render our frames**

- **The session was keyed from the media key alone**, discarding the four-byte
  nonce prefix that follows it, so every record we sealed was undecryptable by
  the peer and every record it sent failed our tag check. It presents as a path
  that establishes and then carries nothing, which is indistinguishable from a
  loop that was never wired up. The constructor used documents itself as being
  for fixtures, and the seam was its only non-fixture caller.
- **No test could have caught it**, and that is worth stating rather than
  regretting: every test builds both endpoints the same way, so the two agreed
  with each other and proved nothing about the prefix.
- **A peer builds one decoder from what it declared** and never switches on
  what arrives, so a guest asking for a codec this stream does not produce
  fails every frame and reports a decode error rather than a mismatch. Said
  plainly in the log now.
- **A peer that cannot decode asks for a refresh, and we were dropping the
  request.** It was parsed and thrown away, so the only recovery a peer has was
  dead against us.
- **Every frame we have sent fits in a single fragment**: median 129 bytes,
  largest 868, over four hundred access units. The multi-fragment path is
  covered by tests and by the corpus comparison and has never met a real peer.
- **Nothing in signalling reports a peer closing a session it was using**, but
  the peer itself does, on the control channel, and we were ignoring it. The
  seat, the port and the share of the bitrate budget were held until the media
  path's two-minute liveness deadline noticed, which is what made repeated test
  connections exhaust capacity.

**Notes on the timing, and the loop shape it forced**

- **The loop had to be restructured before it could be measured.** It
  submitted a frame and then waited for it, which is serialised by
  construction: nothing prepares the next frame while the hardware works, and
  the pipeline caps at one frame per encode however fast the encoder is. It
  now has two deadlines -- a poll for finished pictures and a frame clock for
  new ones -- so an encode overlaps the acquire and submit behind it and a
  picture leaves within a poll of being ready.
- **The measurement is what showed the difference is real**, and it is the
  arithmetic rather than the frame rate that carries it: stages sum to 10.670
  ms across a 2.665 ms interval unpaced, and holding one picture in flight
  instead of four collapses that to 3.064 ms of stages inside a 3.066 ms
  interval.
- **A stamp per picture, not one for the loop.** With more than one picture in
  flight the one that comes back is not the one that went in last, so a single
  stamp would report the wrong frame's latency for every frame after the
  first.
- **Percentiles, never averages**, and nearest rank rather than interpolated,
  so the figure reported is one an actual frame took. Its own test uses two
  slow samples in a hundred rather than one, because one lands exactly on the
  p99 boundary and says nothing either way -- the sort of check that looks
  like a measurement and is an arithmetic accident.

**Notes on the encode loop**

- **The loop is written against the encoder trait**, so the second backend is
  a construction change rather than a second loop, and the tests drive the
  same code through a fake encoder with no device and no hardware latency.
- **A seat has four states and each transition has one owner**, which is what
  keeps the handoff lock free. The loop promotes a claimed seat at the top of
  a frame, before the gate runs, so the guests a frame goes to are fixed for
  that frame and a guest arriving mid-frame waits for the next one rather than
  being handed a predicted frame it cannot decode.
- **The loop empties a leaving guest's ring, and only the loop can.** The
  guest stops touching it before marking the seat, so a push already in flight
  lands after that; every index dropped instead is a pool slot that never
  comes back, and one leak per session exhausts a host.
- **A frame a guest did not get is a broken reference chain whatever the
  reason.** Three ways to lose one, and each latches the guest and reaches the
  refresh that recovers it: the room test refusing, a publish ring that is
  full, and no pool slot free. The last needed a new entry point, because the
  pass that would otherwise ask for the refresh is the one that could not take
  a slot. Removing any of the three fails its own test and no other.
- **`publish` reports which rings took the frame, not how many.** A count
  leaves the caller knowing a frame was lost and unable to latch the guest
  that lost it, which is the silent form of the failure the gate exists to
  prevent.
- **The pool is deliberately smaller than the guests could hold between
  them.** Sizing it so exhaustion is impossible costs a slot per guest per
  queued frame at the width of the largest frame a window can carry, which is
  tens of megabytes for guests that need none of it. Exhaustion is back
  pressure with a defined answer instead.
- **The send ring counts bytes now**, because the rate controller's peak is
  tracked from measured throughput and a controller fed zero collapses to its
  floor on the first congestion rather than to a fraction of what the path was
  carrying. Mebibits per second over an interval of at least half a second,
  which is also the period the controller increases on.
- **The gate's ceiling is the divided rate, not the configured one.** A second
  guest halves both what a guest may send and the window it is measured
  against.
- **The encode latency belongs to the stream, not to a guest.** One encode
  serves them all, so they all waited the same time for it; each guest folds
  the same figure into its own smoothed value because the cadence that reports
  it is per guest.
- **The announced generation and the one in every video header are read from
  one place**, so they cannot disagree. A peer told one number and shown
  another would be tracking a reference chain that does not exist.
- The host-mode message goes out before the first frame. It is thirteen bytes
  with no body, a peer stores it and gates nothing on it, so sending it costs
  less than being wrong about that.

**Notes on the ring geometry and the control channel**

- **The control channel was not attached at all**, so a peer's declaration was
  counted as unhandled and dropped while the group acknowledgement reported
  zero for that channel for the life of the session. A peer therefore
  retransmitted its declaration until it gave up, and nothing above could see
  the message that decides whether a guest is streamable.
- **The slot width is the fragment width, so it is also the datagram width.**
  A ring sized wider than the datagram floor does not gain headroom; it emits
  datagrams no probe has justified, and a peer that cannot take one discards
  the whole datagram rather than truncating it. The previous width put every
  full fragment 207 bytes past the floor, which nothing noticed because no
  message long enough to fill a fragment had ever been sent.
- **The video ring is the peer's ring depth, which is the gate's top ceiling.**
  Those two numbers have to be the same or a frame the gate admits is refused
  by the ring it is admitted into, and the test says so rather than the
  constants agreeing by coincidence.
- **The largest frame that fits is four bytes short of the arithmetic**, since
  the length prefix rides in the first fragment.
- **A take that does not fit does not consume the message.** The channel only
  advances on a completed take, so the same message would be read again every
  pass at full speed. It ends the attempt with its own outcome instead.
- Nothing is attached for video receive. Video is host to guest only, and an
  unattached channel acknowledges zero, which is the truth about a channel the
  peer never sends on.

**Notes on the bitrate budget**

- **Two aggregations compose and they do different jobs.** Each guest's ceiling
  is the configured rate divided by the guests on its stream, which bounds what
  the host can send in total; the rate applied is the minimum of what their
  controllers return, which bounds it to what the slowest path carries. Each
  has its own test, and breaking either fails only its own.
- **The slowest guest does pull everyone down, and that is intended.** The rate
  is what the transport can actually carry, and sending a guest more than that
  produces loss rather than quality. What the slow guest must not do is break
  the others' streams, and it cannot: delivery is decided per guest by the
  gate.
- **A guest arriving and the operator changing the rate are the same event.**
  Both move a ceiling, and a controller has to be told rather than discovering
  it, because the rate it is holding may already be above the new one.
- **The tick is the frame.** The controller's periods are counted in ticks, so
  at sixty a second its thirty clean ticks are half a second. Ticking it from a
  timer would silently change what those numbers mean.
- **The deadband is what stops a reconfigure per frame.** Removing it fails its
  test, which is the point of having one for a behaviour whose absence is
  otherwise invisible.

**Notes on session initialization**

- **The recorded initialization is parsed by the code that will meet a real
  one.** The replay finds the message a stock client actually sent, runs the
  parser on it, and checks that argument 0 really is the body length. A fixture
  we wrote would only prove the parser agrees with the writer.
- **Two of the fields are sentinels, not measurements.** A maximum size of
  60000 means no limit and a resolution of zero means no preference; a host
  taking either literally tries to encode a picture nobody asked for.
- **Only the version is mandatory.** Everything else defaults, and unknown keys
  are ignored rather than refused: peers send different objects, and requiring
  a shape refuses them over fields nothing reads.
- **A body that will not parse leaves the guest on the clock** rather than
  admitting it or abandoning it early. It is the same position as a guest that
  has not spoken, and the deadline already covers that.
- **Time is a parameter, as it is in the core.** Both this and the delivery
  gate previously took a clock reading, which made two of their tests unable to
  reach the case that mattered: nothing could advance five seconds, or half a
  second, without sleeping. The deadline and the keyframe throttle are now
  driven by a millisecond figure, and both tests check the far side of the
  interval rather than only the near one.

**Notes on the packetiser**

- **Parsing proves we can read a peer; re-emitting proves a peer could read
  us.** The corpus replay now takes every recorded video header, re-encodes it
  with our own writer, and requires the bytes back byte for byte -- 4883 of
  them. It then reframes the message and requires the fragment count to match
  what the recording actually used. A field written at the wrong offset, in the
  wrong endianness, or with the rotation off by one fails there rather than in
  a client that renders nothing and says why.
- **Shown capable of failing.** Writing the rotation zero-based, which is the
  documented trap, fails the comparison against the recording.
- **Almost nothing about a video header is per frame.** Dimensions, rotation
  and the generation counter are fixed for a stream and only the keyframe flag
  moves, so the packetiser is a value that outlives a frame rather than a
  function taking six arguments that could each be got wrong per call.
- **The generation counter moves only on reconfiguration**, never per frame,
  and a bitrate change is not a reconfiguration: it neither reinitialises the
  encoder nor changes what a decoder must do. A test frames fifty pictures and
  requires the counter not to move.

**Notes on the delivery gate**

- **The cascade is the invariant and the latch is how it is kept.** A guest
  that misses one frame must miss every frame until a keyframe. Delivering a
  single dependent frame across the gap breaks the reference chain silently:
  the decoder keeps going and produces progressively wrong output rather than
  failing, which is the gray-frame failure. The regression test starves a
  guest, drains its window completely, and asserts every predicted frame is
  still withheld -- it fails the moment the latch is removed.
- **The wrong thing is unsayable.** There is no operation that withholds one
  frame without marking the guest pending, and a caller is told only which
  guests take the frame, never which were withheld from. An interface that
  exposed the other half would eventually have it called.
- **A skipping guest is retested against the largest frame the session has
  produced**, not the frame in hand. Testing against the frame in hand lets a
  guest out of the cascade on a small predicted frame, whereupon the keyframe
  it actually needs does not fit, the throttled grant is spent, and every guest
  pays the spike for a recovery that did not happen. Its own test fails, and
  only it fails, when the comparison is swapped.
- **A joining guest starts pending**, which is what produces its join keyframe.
  Nothing separate arranges one: a guest that has received nothing is in the
  same position as a guest that has fallen out of the chain, so it is the same
  state rather than a second one to keep in step.

**Notes on the collect block**

- **Reading one field wrongly is invisible; reading four is not.** The block
  is audited on every hardware run rather than trusted: the picture kind
  against the one refresh that was asked for, the frame index against the
  collect order, the quantiser against the range the codec has, the structure
  against a whole frame, and the length against the last byte that was
  actually coded. A block read at offsets the driver did not write does not
  land on five right answers at once, and each of them is something the pool
  or the packetiser is about to depend on.
- **The length assertion is the regression test for the race above**, and the
  slice count is its early warning. The count is the more sensitive of the
  two, so it is checked on every picture rather than only where it failed.
- **What was measured and proved nothing is gone.** The macroblock counts and
  the timestamp echoes stay zero under this configuration, and a field that
  cannot move cannot witness anything.

**Notes on the frame pool**

- **The refcount is the only thing that says a slot is reusable**, and it
  is the first cross-thread handoff in the workspace outside the shared
  primitives, so it carries phase 0's obligation: model checked under
  `loom`, and **shown capable of failing**. Weakening the release on a
  guest's decrement and the acquire on the producer's search makes `loom`
  report a causality violation on concurrent access to the frame storage;
  restoring them makes it pass. A model check that cannot fail proves
  nothing about the orderings it is supposed to be exercising.
- **The count is raised before any index is pushed.** Raising it after
  would let the first guest finish and take the slot to zero while later
  guests were still being handed the same index, and the producer would
  then be free to overwrite a frame nobody had sent yet. A ring that
  refuses the index gives its hold straight back, or the pool bleeds one
  slot per congested guest until it stops entirely.
- **One place releases the writer's own hold.** Publishing and abandoning
  both end in the same drop, so neither path can release twice -- which
  the first version did, and which handed slots back while a guest still
  held them.

**Notes on predicted pictures**

- **A refresh happens on request, or when there is nothing to predict
  from.** The second half is not a special case for the first picture; it
  is the same rule, because a reference we do not hold is one we cannot
  point at. That is what makes the refresh request meaningful on this
  backend at last, and with it the gate asking for zero keyframes across
  a bitrate change becomes statable: a backend that refreshed every
  picture would have satisfied any keyframe assertion trivially.
- **The reference is named twice and both are load bearing.** The slice
  points at it, and the picture parameters list it separately. A picture
  missing from that list is one the driver is entitled to release, and it
  will, while the slice still points at it.
- **The counter widths are exercised at a real value rather than zero.**
  They size fixed-width fields in every slice header, so a width the
  writer and the sequence set disagree on shifts every field after it.
  Zero would also wrap the frame number every sixteen pictures, which
  hides the question rather than answering it.

**Fixed**

- **The collect asked the driver not to wait, and the driver answered
  anyway.** `NV_ENC_LOCK_BITSTREAM::doNotWait` neither is ignored nor reports
  a busy lock: set, it returns success on a block the driver has not finished
  writing, with the coded bytes in place and the length not. A refresh picture
  came back claiming megabytes it had never coded, and the slice count came
  back as noise. Both were taken for driver defects for a day; they are one
  race, and it was ours. **A flag that is ignored is harmless and invites a
  retry on a newer driver; a flag that answers wrongly must never be set.**
  Clearing it costs nothing measurable -- the lock takes the same 0.7 to
  1.8 ms either way, and the caller is still never parked, because what gates
  the lock is a completion marker rather than the flag.
- **The quantiser floor was configured but never applied.** It was added to
  the configuration block, documented, and given a default, and nothing ever
  wrote it to the rate controller. The change that added it was verified by
  checking the encoder still encoded, which it did either way: a check that
  cannot distinguish the two states proves nothing about which one holds. It
  is applied now, to refresh and predicted pictures alike -- a floor on the
  predicted ones only would leave the largest picture in the stream, and the
  one that matters most for delay, unbounded.

**Notes**

- **A picture is read from one surface and reconstructed into another, and the
  two are never the same.** The interface takes a surface at the start of a
  picture and a second one in the picture parameters, and it is tempting to
  pass the same identifier to both, because for a stream of independently
  coded pictures there is no reconstruction to keep. That is wrong: as the
  driver takes a surface into its reference store for the first time it
  releases the buffer backing it and allocates a replacement, and the pointer
  it encodes from was captured at the start of the picture. It then encodes
  from freed memory. The first picture tends to survive, because a replacement
  allocated immediately after a release usually lands on the same block, so
  the failure presents as an intermittent fault a few pictures in with nothing
  in the call chain naming a surface. Two pools, paired by index, and the
  question does not arise. The context test asserts the pools are disjoint.
- **The bitrate does not travel in the sequence parameters.** That structure
  has a field for it, and one driver never reads that field; it takes the rate
  only from a separate rate-control parameter, and silently runs on its own
  default otherwise. The field is still filled because another driver does read
  it, but the rate-control parameter is what makes it true. This is what the
  congestion actuator will drive, so a rate that is quietly ignored would have
  been discovered much later and at much greater cost.
- **The source's content is a function of the frame index, and that is what
  makes an encoder checkable.** A bar whose left edge is `index * step` can be
  found in a decoded picture by a checker that shares nothing with the
  producer but the frame number, with an independent decoder in between. That
  catches what a structural check cannot: a wrong upload stride shears the
  picture, a wrong plane offset moves or destroys the bar, and an off-by-one
  in ordering shows as a bar one step out -- all of which otherwise produce a
  stream that parses, decodes, and reports the right resolution. The two
  chroma components of the static block deliberately differ, because equal
  ones survive being written in the wrong order.
- **Noise would have been the wrong content.** It is incompressible, so every
  frame arrives at the rate ceiling and nothing about rate control behaves as
  it will in production; it cannot be checked without reproducing the exact
  generator, which lossy coding defeats anyway; and it offers motion search
  nothing to track, so every picture is effectively intra and the predicted
  path is never exercised. Worth having later as a worst-case size stress, not
  as the default.
- **One input surface per in-flight picture, on both backends.** One surface
  shared across a queue is overwritten while the hardware is still reading it,
  so the encoder emits the newest content under an older picture's timestamp:
  output that decodes cleanly and is wrong, with no error anywhere.
- **Parameter buffers are the caller's to release.** They are read while the
  picture is being assembled and are not consumed by it, so the eight a picture
  carries accumulate for the life of the context until they are destroyed
  after the picture closes. They are released on the failing paths too: a call
  that got far enough to return a status has already read them.

- **The vendored headers are pinned to an old interface version on purpose.**
  Every encoder structure carries a version stamp taken from the header it was
  compiled against, and the compatibility runs one way: a newer driver accepts
  an older stamp, an older driver rejects a newer one on every call and reports
  only that the version is invalid. So the header chooses the binary's minimum
  driver. Pinning to the newest available would have floored us above what
  current distributions ship. Every feature the backend needs was checked
  present in the older header before the pin was taken.
- **The bindings are generated, and committed rather than built.** Generated,
  because the structures are version-stamped and bitfield-heavy and a
  hand-transcription error is not a compile error but a runtime status code
  with nothing pointing at its cause. Committed, because a build script would
  put a C toolchain on every build machine and in continuous integration, which
  installs nothing today. The generated output carries its own gate: forty-two
  compile-time assertions of size, alignment and every field offset, so
  building is the check.
- **Codec and preset identifiers cannot be linked against.** They are
  file-static constants in the header, so no exported symbol exists for any of
  them, and a generator renders each as an extern static -- a reference that
  can never resolve. They are emitted as constants instead, produced from the
  header text so that nothing is hand-copied.
- **A group-level lint allow loses to an explicitly configured lint**, on
  either side of the command line. Allowing the whole lint group over generated
  code left two hundred errors standing; the entries are named individually.
- **No function is declared in the generated output.** The libraries are opened
  at runtime, so an extern block would turn a missing driver into a failed
  start instead of a missing backend.
- **The loader lives in the common crate, not beside its first caller.** It is
  the piece that differs per platform while everything above it does not, and
  that crate is the only one in the workspace containing `unsafe`, which keeps
  that obligation in one auditable place.
- **Symbols resolve eagerly and privately.** Eagerly, so a library whose own
  dependencies are missing fails at open where a caller can fall back, rather
  than at the first call through a function pointer. Privately, because this
  ships inside a shared library loaded into other processes and publishing a
  vendor runtime's symbols can capture lookups never meant for us.
- **Both loader tests were shown capable of failing.** Making the open return
  nothing fails four of the five; making every symbol resolve fails exactly the
  one written to catch it, and no other. A pair that cannot both pass under a
  broken implementation is the point.
- **The compute device is selected by bus address and a miss is an error.** A
  machine with two GPUs has the frame source on exactly one of them, and
  encoding on the other moves every frame across the bus, which is a readback
  under another name and which section 4 of the host document requires to be
  chosen rather than discovered. There is no fallback to another device,
  because that failure would surface as a latency figure rather than as an
  error. The address is read at construction and never stored: enumeration
  order is not stable across driver reloads and neither is which card drives
  the display.
- **The hardware settled the phase's open question: there is no asynchronous
  completion on this platform.** The capability reports absent for both codecs,
  so a completion object cannot be waited on and the collect has exactly two
  honest options: the non-blocking form of the bitstream lock, or a compute
  event recorded on the encoder's own stream. A blocking collect padded with
  queue depth is the third option and is the one to avoid, because it converts
  "the encoder fell behind" into "the pipeline thread is stopped", which is the
  one moment it must not be. The gate item that requires a non-blocking poll is
  therefore load bearing rather than defensive.
- **Live bitrate change is supported, so the congestion actuator can exist.**
  Asked rather than assumed, because the only actuator the design has would
  otherwise be unimplementable and the gate that counts keyframes across a rate
  change could never pass.
- **The encoder accepts packed colour formats directly.** That is the internal
  conversion the pipeline exists to avoid, and its availability is exactly why
  the rule against it has to be explicit: it is the easy path and it is
  measurably worse. The planar format the conversion targets is accepted too,
  which is what makes the rule followable.
- **The configuration starts from the preset and overrides only what is
  required.** Building one from zero means silently accepting a default for
  every field nobody thought about, and the fields nobody thinks about in an
  encoder are the ones that add a frame of latency.
- **No B-frames, though the hardware offers up to seven.** Every one of them is
  reorder delay: latency paid on every frame to save bits on some of them,
  which is the wrong trade for this product. Output order is pinned to capture
  order for the same reason.
- **One frame of rate-control buffer.** A larger one lets the encoder smooth
  bitrate across frames, which is precisely the queueing this pipeline exists
  to avoid; those bits arrive late rather than not at all.
- **Keyframes are never scheduled.** The interval is set to infinite, so one is
  produced when the delivery gate asks and at no other time. A periodic
  keyframe on top of a throttled on-demand one is bandwidth spent to a
  timetable rather than to a need.
- **Parameter sets repeat on every keyframe.** A guest joining mid-stream is
  then decodable from the next keyframe alone, with no separate out-of-band
  step that can be got wrong.
- **The reconfigure clears both the reset and the keyframe flags.** Either one
  turns a rate change into a visible discontinuity, and congestion moves the
  rate many times a minute.
- **The initialisation block does not keep the pointer it was given.** The
  interface copies the configuration during the call, and the block it named is
  a local about to be moved into the returned value, so retaining it would
  leave a dangling pointer in a structure that is reused on every reconfigure.
- **The second backend pins its headers the opposite way round, and for a
  reason.** The codec interface stamps a version into every structure and an
  older driver rejects a newer stamp, so that pin has to be low. This one has
  no stamp, passes buffer sizes explicitly at every call, and grows its
  structures by appending, so a driver older than the header reads the prefix
  it knows. Compiling against a header older than the installed runtime is the
  safe direction either way; only the reason differs, and writing down which
  reason applies is what stops the next person applying the wrong rule.
- **Cropping is measured in chroma samples, not pixels.** A coded picture is a
  whole number of macroblocks, so 1080 rows are coded as 1088 and eight rows
  are cropped -- but the field takes four, because each unit is two rows at
  4:2:0. Writing pixels crops twice what was intended, on every decoder, and it
  presents as a capture bug rather than as a parameter-set one.
- **The parameter-set tests check structure, not meaning, and that distinction
  is worth keeping visible.** They agree with the writer because both came from
  one reading of the standard, so they would agree just as well if that reading
  were wrong. Nothing yet proves a decoder reads the colour description as
  intended, and a parameter set alone cannot be used to find out: a decoder
  reports nothing about a stream containing no picture. The check arrives with
  the first frame this backend encodes, and the dumper that will perform it is
  in place rather than left to be remembered.
- **One backend has nowhere to put the colour description.** Its sequence
  parameters carry a single aspect-ratio flag and no colour fields at all,
  where the other takes primaries, matrix, transfer and range as ordinary
  structure fields. Since the description is required and not optional, that
  backend has to write its own parameter set and hand it over as a packed
  header. This is why the packed-header attribute was worth asking about, and
  why the answer only arrived by looking at the structures rather than at the
  capability.
- **Start-code escaping is the kind of fault that arrives with content.** A
  payload may not contain a start code, so a run of two zeros followed by a low
  byte needs a marker inserted. Omit it and the stream decodes correctly until
  the day the encoded data happens to contain the pattern, which attributes
  itself to anything but the writer. Every byte that can end a start code is
  covered by a test, and the zero run restarts after each insertion.
- **The parameter-set writer is testable without hardware**, because the coding
  is fixed by the standard rather than by a vendor. Its expectations are the
  standard's own code table written as bit strings, not values captured from a
  device, so they can be checked by eye. Both halves were shown capable of
  failing: reversing the bit order fails four of the eight, and removing the
  escaping fails three, and no test overlaps both.
- **The quantiser floor is a latency control and reads backwards.** A lower
  floor lets the encoder spend more bits on a frame, and more bits is a larger
  frame, more packets, and longer in every queue between here and the far side.
  Below about five those bits buy nothing the eye resolves, so they are spent
  purely on delay. The setting with the *higher* floor is therefore the
  lowest-latency one, which is the opposite of how a quality knob reads, and it
  is the default here because latency is this product's first goal.
- **An unsupported attribute reports a sentinel, not zero.** Reading the
  interface's not-supported marker as a bit set makes every bit read as set,
  which turns "this device does nothing" into "this device does everything".
  Folded to zero at the boundary, once, rather than at each use.
- **What a driver accepts is not what it requires.** The packed-header
  attribute says which parameter sets the driver will take from us, and reading
  it as which ones we must supply is the same capability-for-behaviour mistake
  that a renderer's format list invited earlier in this phase. Whether the
  driver emits them unasked is settled by encoding a frame and looking, not by
  an attribute. The accessor is named for what it answers.
- **A profile is not an encoder.** A device may decode a codec and not encode
  it, and both are entry points against the same profile, so the query asks for
  the encode entry point specifically. The test asserts the refusal as well as
  the answer: a profile the driver cannot encode must come back empty, or the
  positive result proves nothing.
- **Generated code satisfies the safety lint rather than being exempted from
  it.** Trailing-array helpers perform unsafe operations inside unsafe
  functions, which the workspace denies. The generator now wraps them, which is
  a flag; adding the lint to the module's allow list would have been a flag
  too, and would have quietly widened what the crate permits.
- **There is no way to learn a picture is finished without waiting for it.**
  Two mechanisms were tried and measured. The interface's own no-wait flag is
  documented for exactly this case and is ignored by the driver. A completion
  marker recorded on the encoder's own stream does gate -- most polls in a
  burst come back not-ready -- but it passes before the bitstream can be
  retrieved, because the encode runs on a hardware engine rather than on that
  stream. What the marker does buy is the half that matters: a caller with
  nothing ready gets an answer in about 300 ns instead of being parked for a
  frame. The phase gate was narrowed to that, and the retrieval cost recorded
  as a number.
- **A stream handle is not a pointer to a stream.** The encoder's stream setter
  takes the address of a handle, and passing the handle makes the driver
  dereference it as memory. It faults inside the driver with nothing pointing
  back at the call site, which is the worst diagnostic shape available.
- **Field declaration order is load bearing when fields own driver objects.**
  Fields drop in declaration order, and the session owns the compute context;
  a stream destroyed after its context has been released is a use-after-free.
  The encoder therefore declares its stream and markers first and its session
  last, and the comment says why, because the next person to add a field will
  not guess it.
- **The no-wait collect is documented and not implemented.** The interface
  states that its no-wait flag returns a busy status when a picture is not
  ready, explicitly including the synchronous mode that is the only mode this
  platform offers. The driver ignores it: four collects for four pictures, not
  one busy status, and the slowest collect equal to one picture's encode time.
  A genuinely non-blocking lock spun in that loop would report busy hundreds of
  times. **The phase gate for a non-blocking collect is therefore not met by
  this path**, and is met instead by recording a completion event on the
  encoder's own stream and querying it, which is the next piece of work. The
  measurement is kept as a test rather than deleted, because it is the evidence
  that the cheap path was tried and does not work, and because a driver update
  could change the answer.
- **Nothing was allowed to pass by lowering the bar.** The obvious repair when
  the assertion failed was to relax it. That would have made the gate item pass
  against exactly the behaviour it exists to reject, so the assertion was
  replaced by a recorded number and the gate left open.
- **The test bug that hid the finding is worth naming.** The first version
  polled once to time it, then drained a full queue's worth, having already
  consumed one picture; it waited forever for a frame that could not arrive.
  The symptom was a hang, and the temptation was to treat the hang as the
  finding. The actual finding was one layer down and only visible after
  instrumenting the status the driver returned.
- **The selection test proves itself the same way the loader's does.** On a
  machine with one compute device, matching the display's address cannot
  distinguish a correct implementation from one that ignores the address
  entirely. What distinguishes them is the second assertion, that an address
  belonging to no device is refused: an implementation returning the first
  device regardless would satisfy the first check and fail that one.

## 4: signaling (in progress)

**Added**

- `lowlat-crypto`: credential generation, key material decoding, and the only
  source of randomness in the workspace.
- The admission seam in `lowlat-host`: register an attempt, add a candidate,
  approve returning host credentials, end a connection, and an event queue the
  application drains.
- `lowlat-kessel`: the connect URL, the message set, and a transport with one
  reader and one writer over a queue, so producers never touch the socket.
- The host advertisement, and a runnable endpoint that publishes a host into the
  discovery listing and holds the connection open.
- Reconnection with bounded exponential backoff and jitter, re-registering and
  re-advertising on every connection rather than only the first.

**Notes**

- **Entropy gets its own crate, below the core so the core cannot reach it.**
  The core owns no generator by construction and everything above it needs one:
  a session key, a check password, the seed a transaction identifier derives
  from. Until now every one of those was a constant supplied by a fixture, which
  is correct for a test and is not a source. One audited crate is the
  alternative to scattering it into whichever crate happened to need it first.
- **A generator that is not generating passes every length check**, so the test
  that matters asserts two draws differ rather than that one is the right size.
- **Credentials never render their contents**, whatever the format string, and
  a test asserts it. A credential reaches a log by accident, and the accident is
  worth making impossible rather than unlikely.
- **The advertisement's field order is pinned by a test.** A strict parser on
  the far side is entitled to care, and matching the order costs nothing here
  while being invisible to find later.
- **`app_v` is a string even though it holds a build number**, and a schema that
  types it as a number is wrong about the wire. Named test.
- **A candidate's `ip` is a string and its `port` is a number**, with exactly
  three booleans after them. Transposing the pair produces a candidate a peer
  accepts and silently ignores, which is the worst failure shape available, so
  the layout carries a named test rather than a comment.
- **The query hangs off exactly one root path.** Without the path the request
  line is malformed and the edge answers 400 rather than upgrading; with two the
  service has no such route. Named test, because the failure is a status code
  with no body and nothing that names the cause.
- **The credential is longer than the key.** The media key field carries far
  more material than the cipher consumes, and only its leading portion is key
  and nonce prefix. A validator written to the key's length rejects every real
  offer.
- **Both directions key from the host's material.** A client that supplies its
  own is signalling support, not proposing a key; the session is encrypted with
  what the host returns when it approves.
- **Exactly one TLS provider is chosen here**, rather than left to feature
  unification, which resolves to none and panics inside the TLS stack at the
  first connection. That reads as a crash rather than as a configuration gap.
- **Jitter is the part of a reconnect schedule that matters.** Without it, a
  service restart brings every host that was connected back on the same
  schedule, arriving together at exactly the moment the service is least able
  to take them. The draw is bounded below as well as above, so an unlucky run
  cannot hammer a service trying to come back. A fixed schedule passes every
  bound a growth test can state, so the test that matters asserts two schedules
  disagree.
- **The registering frame is resent on every connection, not just the first.**
  The service takes it as what associates the connection with the host, so a
  reconnect without it is a connection nobody has associated with anything.
- **A reconnect abandons what was negotiating and keeps what established.** An
  attempt still trading candidates is gone: the peer gave up when the
  connection carrying them dropped. A guest that already established never
  depended on that connection, and tearing it down because signaling blinked
  would drop a working session for an unrelated reason.
- **Silence is not a refusal.** A declined answer is a wire event the peer acts
  on at once; no answer at all leaves it connecting indefinitely, because
  nothing in the protocol reports a host that never replied. An offer refused
  on capacity was being dropped without a word, which is the worst failure shape
  available: neither side reports anything. Every offer is answered now,
  including the ones turned down.
- **"Still waiting for the host" is not a protocol outcome.** There is no
  message for it in either direction, so anything that needs to surface it owns
  the timer itself.
- **Two inbound actions were being dropped silently**: the service's close,
  whose reason is the only thing separating a bad session from an unknown host,
  and an opaque passthrough channel no schema lists. Both are reported now.
- **A keepalive without a deadline detects nothing; it only makes silence look
  like traffic.** A connection whose peer has gone stays established locally for
  as long as the kernel keeps retrying, so writes queue and nothing reports a
  fault. Found at ten hours: the socket up, bytes stuck in its send queue, a
  ping leaving every thirty seconds, no reply to any of them, and not one drop
  logged. Anything inbound now counts as a sign of life and two missed replies
  end the connection, which is the only thing that will ever notice.
- **Silence is not a stable state for a connection, and answering pings is not
  enough.** A host with nothing to say has to put something on the wire itself,
  on a schedule, because the path to the service closes an idle websocket after
  about a hundred seconds. Inbound pings are answered too, which is correct and
  was not the cause.
- **A working reconnect hid this for an hour.** The first diagnosis was that
  queued pongs were never flushed, and the connection dropping every two minutes
  was read as fixed because the host stayed in the discovery listing. It stayed
  there because it was reconnecting roughly thirty times an hour, fast enough
  that nothing above noticed. **A recovery mechanism masks the fault it recovers
  from**, so the measurement that settles it is drops per hour, not whether the
  host is visible.
- **The first regression test for it passed against the defect.** It asserted
  that an inbound ping was answered, which was true before and after the fix and
  therefore proved nothing. The test that discriminates asserts an idle
  connection transmits unprompted, and it fails in ten seconds against a client
  with no keepalive.
- **An established guest learns the peer left from the media path, not from
  signaling.** A peer that closes a session it was using does not withdraw its
  offer, so nothing arrives to say so. Without a liveness check the loop runs
  forever holding its socket, and the next guest walks to the next port; three
  connects in a row took three ports and freed none.
- **A withdrawal can overtake the offer it withdraws.** Observed: a cancel for
  an attempt arrived before that attempt's offer. Treating the cancel as a
  no-op for an unknown attempt then admits the offer behind it, spending a
  socket and a thread on a guest that has already gone. Withdrawals are
  remembered briefly so the offer behind one is refused.
- **The queue carries what the application did not cause.** Ending a connection
  is the application causing it, so it emits nothing; reporting it back
  produced a second terminal event for an attempt that had already reported
  one, and the reaping call would have looped.
- **A candidate marked `sync` is a readiness signal, not an address**, and the
  flag is a parameter of the call rather than the caller's business, because
  both ways of getting it wrong are silent. Adding one to the table spends
  checks on whatever the placeholder names, and a peer that sends a literal
  `1.2.3.4:1234` will be checked at that address. Never sending one is worse:
  a peer is entitled to withhold every real candidate until it sees one, so
  negotiation succeeds and then there is nothing to check. Approval queues the
  request without being prompted. Two named tests, both shown to fail.
- **The seam is polled, not called back.** A callback runs on our thread, so an
  application that blocks in one stalls a media loop and every integration has
  to reason about which thread it is on. A queue moves that decision to the
  caller and keeps our threads ours.
- **One socket per guest, so concurrent guests walk the port.** The socket
  punched for an attempt becomes that guest's media socket for the whole
  session rather than being handed back, so a second guest cannot have the
  configured port. Without the walk a host cannot admit a second guest at all,
  which turns the walk from a convenience into the thing multi-guest rests on.
  Named test over three guests taking P, P+1 and P+2.
- **The bound port goes back in the answer, not the configured one.**
  Advertising the port that was asked for when the bind walked produces a peer
  that answers checks and never establishes.
- **Credentials are generated at approval.** Earlier binds them to no socket;
  per registration leaks state for attempts that are never approved. Two
  concurrent guests are asserted not to share a media key.
- **A candidate that arrives before approval is kept.** Candidates trickle and
  the peer starts sending before the answer reaches it, so the early ones are
  buffered and handed over on approval. On a wide-area path one of them may be
  the only one that works.
- **The advertisement is emitted on state change, not on a schedule**, and it is
  driven by a stale mark rather than by a timer: something marks it dirty, the
  loop publishes and clears the mark. A capture appearing to show a ten second
  cadence was the layer above driving it, not the layer being measured -- the
  cadence was attributed before its cause was, and correcting that took two
  passes over the same document. Whether a host that advertises once stays
  discoverable over hours is open, and is a question for the listing rather than
  for argument.

## 3: io shell (2026-08-16)

**Added**

- `endpoint`: one object owning the connectivity engine and the session,
  classifying each datagram and reporting the sooner of the two deadlines.
- The acknowledgement cadence is labelled correctly: one the cadence produced
  carries the keepalive flag and a zeroed trigger.
- `lowlat-net`: the media socket with the full option set and the granted
  buffer readable at open, batched receive pulling a burst per syscall, and
  batched send with segmentation offload and a per-datagram fallback, the
  application send wake, the event loop that drives an endpoint over them, and
  the merged per-guest thread with its teardown.
- The bind walks forward when the configured port is occupied, so an occupied
  port delays a host's start rather than preventing it.
- `conn` retains the addresses reflexive servers report, so a caller that
  processes datagrams in batches can still ask for its own candidates.
- A second fixture endpoint that drives the namespace topologies through the
  real shell instead of a loop standing in for one. The topology matrix passes
  with it, as does a run between two machines on different networks.
- The sustained loopback soak, carrying gates 1, 2 and 4 in one harness: nothing
  lost, nothing allocated in steady state, and wake accounting as a number.
- The drain flushes a full staging batch instead of asking the core again with
  room it has already established is too small.
- The connect and teardown churn soak, counting descriptors, threads and
  resident memory across ten thousand cycles.

**Notes**

- **A bind failure is not a startup failure.** The walk takes 50 ports from the
  configured one, each attempt on a fresh descriptor, because the option set is
  applied before the bind and cannot be retried on the socket that carried it.
  It stops at the top of the range instead of wrapping: wrapping lands on the
  privileged ports, where the bind fails for an unrelated reason and reports it
  as though the range were occupied.
- **The fixture endpoint is swapped, not grown.** The original loop stays as the
  reflexive server, which no shell provides, and stays deliberately the simplest
  thing that works. The endpoint under test is a separate binary owning a shell,
  so the two can be run against the same topologies and compared.
- **Signaling reaches the loop through the wake, not through a poll.** The
  fixture's rendezvous is read on its own thread and injected where the
  application's work is pulled. Polling it from the loop instead ties how fast a
  candidate is noticed to how long the loop happens to be waiting, and the loop
  waits on the endpoint's deadline -- tens of milliseconds when nothing is due.
  That delay is invisible against a peer that waits and decisive against one
  that does not, which is what made the difference in three topologies.
- **A test that passes because it is fast is not passing.** The loop that stood
  in for the shell polled every 5 ms and always punched outward first. The
  event-driven loop does not, and three topologies failed until the wake carried
  the candidate. Neither result was about the topology.
- **The churn gate's exact counts were shown to fail.** Leaking a descriptor
  per cycle takes the count from 4 to 254 over two hundred cycles, and leaking
  the guest takes threads from 52 to 252. Resident memory is the one with a
  tolerance, because an allocator holds arenas back and a plateau is not a
  slope; the two that can be counted exactly are asserted exactly.
- **A full staging batch is not a malformed emission, and treating it as one
  wedges the loop.** Staging hands back the room that is left; once that is
  shorter than the next datagram the core cannot encode into it and fails.
  Asking again unchanged returns the same failure forever, so the loop spun at
  full CPU with the stream stopped the moment a pass produced more than the
  batch holds. Two messages crossed and then nothing. The drain flushes, which
  makes the whole buffer available, and asks once more; a failure with all of it
  free is ours and the emission is dropped. **Every test that sends a datagram
  or two ran straight over this**, and so did the two-machine punch: it takes a
  sustained stream to reach at all.
- **A regression test for it has to punch first.** The first attempt queued a
  burst on one shell with a candidate and no path. Media waits until a path
  exists, so nothing was emitted, the batch never filled, and the test passed
  just as happily against the loop that spins. It proves nothing without a peer.
- **The receive buffer is the deployment's to grant, and now there is a number.**
  Ten minutes at 10009 datagrams/s on a stock kernel: 6005139 messages sent,
  6005139 received, no gaps, and **1648 datagrams dropped by the kernel** for
  want of receive buffer. Recovery carried every one of them, which is the point
  of the recovery, but the datagrams were still lost on arrival. With the
  ceiling raised the same run drops none. The request has always been logged
  against the grant; what this adds is that the grant is a deployment setting
  and the service will have to raise it rather than assume it.
- **The translator that filters must not be poisoned by what it filters.** Two
  fixtures let an unsolicited inbound check commit a connection entry whose
  reply direction was exactly the one the outbound punch then needed; with the
  external port pinned there was no second choice, so the punch was dropped for
  as long as the peer kept retrying. Real equipment discards unsolicited inbound
  without keeping anything, and the fixtures now do too, dropping before
  translation so the entry is never confirmed. Until that landed, the matrix
  turned on which side transmitted first, which is not what any of the six cases
  is about. It is shown by making the endpoint slow again: the arrangement that
  failed three of six now passes all six.
- **One green run proved nothing here.** A first hypothesis about the difference
  was supported by a single passing run and refuted by repeating it three times
  each way. On a fixture with a race in it, a single pass is not evidence, and
  the repeat is what turned an answer that fit the facts into one that was true.
- **A candidate reported once is a candidate lost.** The reflexive address was
  returned from the call that processed the datagram carrying it and kept
  nowhere. A loop that handles one datagram at a time can read that return
  value; the shell pulls a burst per syscall and has nowhere to put it, so the
  candidate a wide-area path depends on was learned and discarded on every
  path that matters. It is retained and asked for instead.
- **Landing on a port nobody asked for is opt-in.** Exhausting the walk returns
  the bind error. A caller that would rather have any port than none asks for
  that by name and must read the bound port back, because a host that silently
  takes an arbitrary port advertises the one it wanted and receives nothing on
  it -- a peer that answers checks and never establishes.
- **Classification and timer merging are protocol decisions, so they live in the
  core rather than in the shell.** There they run on injected time and replay
  from a seed; in the shell they would be the untested glue written once per
  platform. The shell's job against an endpoint is four calls.
- A shell arming from the session alone misses every connectivity deadline; one
  arming from connectivity alone polls forever once the attempt is over, because
  a finished attempt asks for no wakeups. Both are one-line mistakes and neither
  is visible in a short test.
- **Media has nowhere to go before a path exists**, so it waits rather than being
  emitted to a default destination or dropped. Named test.
- **The event loop's upper clamp was 5 ms, which is shorter than every deadline
  the session actually arms.** It bound on every wake and reinstated exactly the
  fixed over-poll the rule beside it forbids, while the gate two lines down
  exists to prove the loop is event driven. Raised to 50 ms, where it never
  binds in normal operation and still catches a core returning nonsense.
- **The socket is opened by the shell, not by the connectivity engine.** Two
  documents said otherwise; the engine is sans-IO and owns nothing. The rule
  that mattered survives: options are set once at open and nothing lowers one
  afterwards.
- **`poll` rather than an event port, deliberately.** The loop waits on two
  descriptors, the socket and the send wake. At that count a readiness scan is
  free and an event port saves no syscall per wait, since both are one call;
  it would only add registration state and a third descriptor. The trade
  reverses if a thread ever multiplexes many guests, which the threading model
  does not do, so revisit it only if that changes.
- **Batched receive is not an optimisation.** A single outstanding receive plus
  a poll loses a keyframe burst outright, so the batch pulls up to 64 datagrams
  per syscall straight into slots the kernel writes.
- **The address length is in and out.** A reused descriptor whose length is not
  reset before each pass presents the previous datagram's value and truncates
  the source address. Reset every slot, every pass; a named test sends from two
  sockets in turn and checks the second is not reported as the first.
- **The message descriptors hold raw pointers into the batch's own
  allocations**, so one field exists purely to keep an allocation alive and is
  never read through its handle. Removing it because nothing reads it would
  leave the kernel writing through dangling pointers.
- **This crate adds no model-checking obligation, and that is a finding rather
  than a skip.** Its batches are single threaded by construction, the
  application seam is a call rather than a ring, and the one shared word is a
  teardown flag carrying no payload: a model of it passes under relaxed
  ordering too, so it could not fail. What closes the teardown race is the wake
  descriptor, which a model checker cannot represent, because it cannot execute
  a syscall. The primitives that do carry the obligation stay model checked.
- **ThreadSanitizer is the checked build here**, and it runs clean over every
  test in the crate. It covers what the model checker cannot: kernel-mediated
  concurrency across a real spawn boundary.
- A churn test over spawn and teardown cycles covers what a single pass cannot,
  and seeds the connect-and-teardown soak.
- **Teardown wakes before it joins.** Setting the state and joining strands the
  thread in its wait until the deadline expires, which turns a clean disconnect
  into a visible hang. The state is set, the loop's descriptor is notified, and
  only then is the thread joined; teardown also runs from `Drop`, so a caller
  who forgets is not the difference between a clean exit and a stranded loop.
- **The teardown test's threshold sits below the loop's wait cap, deliberately.**
  At or above it the test passes without the wake at all, because the thread
  times out into the same check and joins looking prompt. Only a threshold below
  the cap can distinguish being woken from timing out.
- **No thread raises its own priority**, and the crate says why where someone
  would otherwise add it: a library outranking its host process's interface
  thread is a priority inversion that has shipped as a hard hang. The process
  class is the lever that works and it belongs to the application.
- **The wake is taken before the application rings are pulled, never after.**
  Anything enqueued from that point on leaves the descriptor armed, so the next
  wait returns at once. The reverse order consumes the token belonging to an
  item that has not been read yet and leaves it sitting until the next timeout.
  It is the same shape as a notify that never reaches its waiter: the wake
  exists and the sequence around it loses it. Named test.
- **Producers own their own descriptor** rather than sharing a reference count,
  so the send path touches no atomic refcount and ownership stays single.
- **Wake accounting is a counter, not a description.** The loop records why each
  pass woke, so "event driven rather than polling" is a number: an idle loop
  wakes about once per deadline it armed, where a ticking one wakes an order of
  magnitude more. Asserted at the shell.
- **The kernel's segmentation rules shape the send API rather than hiding
  inside it.** Every segment but the last must be the same size and all go to
  one destination, so a burst closes on a size change, a destination change, or
  a datagram needing its own hop limit. Making that visible is what keeps the
  caller from silently producing an unsendable batch.
- **A probe never rides with anything else.** It carries a hop limit that cannot
  reach the peer, so it leaves alone and the socket is restored in the same
  call. Asserted at this layer as well as in the core, because this is the layer
  that holds the option.
- **Offload is a fast path, never a requirement**, and it is dropped for good
  the first time a kernel refuses it rather than paying a failed syscall per
  burst. The burst test asserts offload was still enabled afterwards; without
  that it would pass identically on the fallback and keep passing if offload
  silently stopped working.
- **The keepalive is the acknowledgement cadence, not a separate schedule.**
  Every acknowledgement resets the cadence, whatever prompted it, so the timer
  fires only when nothing else has sent one and an acknowledgement leaves at
  least every 30 ms for the life of a session. It carries the keepalive flag and
  a zeroed trigger when only the cadence prompted it, and the ordinary flag with
  the real trigger when data did. A working note had this recorded as "keepalive
  emission is not implemented, an idle session eventually trips liveness". That
  was wrong: the cadence already emitted, so an idle pair never died. Only the
  label and the stale trigger were wrong. An idle pair is now driven past the
  hard liveness deadline in a test rather than argued about.
- **The crypto per-thread leak cannot occur here.** The primitives in use keep no
  per-thread state, so the churn soak is kept for leaks that can still happen
  and the original cause is recorded as absent by construction rather than as
  covered. A gate that passes without testing anything is worse than no gate.

## 2: connectivity (2026-08-16)

The punch, sans-IO like the rest of the core. Candidates in, checks out, a path
or a typed failure.

**Added**

- `stun`: the check codec. Binding requests carrying the attribute set and order
  a peer expects, binding responses carrying the address a request was observed
  from, integrity and fingerprint over both.
- `conn`: the punch state machine. Candidate table, check schedule, the
  once-per-attempt mapping probe, first-answer-wins path selection, and a typed
  failure when the window closes.
- An output carries a destination and a send-time TTL, so a probe cannot be
  emitted without the shell being told to restore the socket afterwards.
- `hmac` and `sha1`, default features off, asserted allocation free rather than
  assumed to be.
- `demux`: the two-byte classification that lets checks and media share one
  socket.
- Reflexive discovery: bare binding requests to a server, and the address it
  reports emitted as a candidate.
- `check` fuzz target over the classifier, the parser, every accessor, and
  verification.
- `lowlat-sim`: address translation modelled as two independent behaviours,
  chained translators, hairpin, a seeded path with loss, duplication,
  reordering, jitter, and hop-limited delivery.
- The topology matrix, each case stating the outcome it expects.
- The recovery gate: ten thousand messages across a degraded path, delivered
  and in order.
- Network namespace fixtures: six topologies against a real kernel, driven by a
  fixture endpoint that runs the engine over a real socket.
- Peer-reflexive candidates: the source address of a verified check becomes a
  candidate and is checked like any other.

**Notes**

- **The check length field is written twice.** Integrity covers a message whose
  length claims to end after the integrity attribute, while the length left on
  the wire claims to end after the fingerprint. Hashing the bytes as received
  fails every message. The digest is fed a substituted value instead of the
  message being copied and edited.
- **Integrity and fingerprint must be adjacent and last**, and a message outside
  52 to 256 bytes is refused before parsing. A peer rejects both cases with no
  diagnostic, so the codec rejects at exactly the same boundaries.
- **The mapping probe is emitted once per attempt, not once per candidate.** It
  exists to open the local mapping, not to reach anyone, so repeating it per
  candidate buys nothing and spends budget.
- **There are two passwords and swapping them still authenticates.** A check we
  send is signed with the peer's password; a check we receive was signed with
  ours. Both directions carry a test that fails if they are exchanged.
- **The window is 7500 ms at a 500 ms per-candidate cadence**, so an attempt has
  about fifteen checks per candidate and no slow retry tier behind it. A
  candidate that cannot answer must therefore never be admitted, which is why a
  gateway mapping outside globally routable space is discarded rather than
  offered.
- Transaction identifiers are derived from a per-session seed rather than
  generated. The identifier is echoed rather than validated and integrity is
  what authenticates, so deriving it keeps the core free of a random number
  generator and makes a failing run replayable from its seed alone.

- **Two trust domains share the check codec.** A peer check is authenticated and
  verification is the whole of its admission. A reflexive server's answer
  carries no credentials at all, so it is admitted only on a transaction
  identifier still outstanding toward that exact address. Parsing therefore
  accepts an unauthenticated message and can never be mistaken for having
  authenticated one, which is why `is_authenticated` exists and why
  verification refuses such a message under every password.
- **Classification is asymmetric on purpose.** Anything not shaped like a check
  goes to the record layer, where authentication rejects it, so the check
  parser is never handed input that was not already check-shaped.
- **Mapping and filtering are separate knobs**, and the matrix is the pairings
  of them. Mapping decides whether the address a peer was told about is the one
  our packets leave from, which is what a punch depends on; filtering decides
  what is let back in, which is what simultaneous open defeats. A model with one
  knob cannot express the difference and the difference is the whole matrix.
- **Carrier-grade translation is decided by mapping behaviour, not by the number
  of layers.** Two layers that keep mappings endpoint independent are punchable
  and the matrix requires them to establish; a symmetric carrier translator is
  not. The plan previously assumed all carrier-grade cases fail, which would
  have made a real regression on that path look like expected behaviour.
- **Half the matrix is expected to fail**, so every case states its expected
  outcome and the timeout cases require the specific failure rather than any
  failure. Two pairs differ by a single behaviour flag and produce opposite
  outcomes, which is what shows the harness can report both.

- **Recovery figures**, ten thousand messages on one channel: a clean path
  delivers them in 440 ms of simulated time; five percent loss with five percent
  reordering takes 4530 ms and discards 1323 datagrams; twenty percent loss with
  ten percent reordering and five percent duplication takes 24705 ms and
  discards 8644, and still converges with nothing lost or reordered at the
  application. The order-of-magnitude cost at five percent is the in-order
  channel stalling behind each gap until the retransmission arrives, which is
  the expected shape and is worth remembering when reading a freeze.
- A clean run is compared against a lossy one in the same test, because a
  recovery suite where the conditions silently failed to apply would otherwise
  pass while measuring nothing.

- **A default masquerade rule is not a cone translator.** It reallocates the
  source port per destination, which is address-and-port-dependent mapping, so
  the stock configuration is symmetric. A fixture built on it looks like a
  port-restricted cone, fails to punch, and confirms the exact opposite of what
  it was written to check. Every cone topology pins the external port; only the
  symmetric case is left at the default.
- **The path between fixture endpoints must be longer than a mapping probe can
  travel.** On a short path the probe crosses the whole fabric and reaches the
  far translator before that side has sent anything, which creates an entry in
  the inbound direction; the far side's own outbound then matches that entry as
  a reply, so no inward path is ever established and both sides time out. This
  is the mechanism the reduced TTL exists to avoid, and it is only visible with
  real distance. Carrier-grade needs one hop more than a plain gateway, because
  the probe must die before reaching either of that side's two translators.
- **An endpoint that exits the moment it establishes strands the other side.**
  Answering checks is unconditional and outlives path selection, so the fixture
  keeps running for a settling period after a path is found. Without it one side
  established and the other timed out, on every topology.

- **Peer-reflexive candidates are not optional, and a wide-area run is what
  found that.** Under symmetric translation the address a peer advertised was
  created toward a reflexive server, so its packets to us leave from a different
  mapping and the advertised address is unreachable. Only the address its check
  actually arrived from is. Without this the far side answers our checks while
  never finding a path of its own, and a host that never finds a path never
  sends media. The failure is one-sided and looks like the peer connecting
  successfully.
- Neither the simulator nor the namespace fixtures caught it, because in every
  case there both sides advertised addresses that were genuinely reachable. The
  matrix now carries the symmetric-to-full-cone pairing that exposes it; it
  failed before the change with exactly the wide-area symptom.
- Admission is the check having authenticated, and nothing weaker. An
  unauthenticated source address would let anyone able to reach the socket
  decide where we send.

**Gate closed 2026-08-16.**

```
matrix:   10 topologies simulated, 6 against a real kernel, 0 unexpected
wide area: both sides established between two networks, 32 ms to first path
recovery: 10000 messages at 5 percent loss and reorder, in order, 4530 ms
fuzz:     check parser 35.3M executions, no crash
tests:    206 passed; clippy, fmt, ascii clean
```

The wide-area run is the item that earned its place: both synthetic tiers were
green while a one-sided failure sat in the engine, because in every synthetic
case both sides advertised addresses that were genuinely reachable.

## 1: protocol core (2026-08-16)

Sans-IO, `no_std`, allocation free. Bytes in, bytes out, time as a parameter.

**Added**

- `envelope`: the record layer, both ciphers, nonce derived from the credential.
- `packet`: data packets and group acknowledgements with the full flag validation matrix.
- `message`: the length-prefix framing and fragmentation arithmetic.
- `channel`: the receive ring, length-driven reassembly, and the stall escape.
- `send`: the send ring, retransmission timeout, fast retransmission, staleness scan.
- `congestion`: the host-local rate controller.
- `pmtu`: path probing.
- `control`, `video`: message headers and keyframe classification.
- `session`: the facade the shell drives.
- Fuzz targets for every surface that parses network bytes.

**Notes**

- **The nonce is not a zero prefix plus a counter.** The credential decodes to the key
  followed by a four-byte nonce prefix, which is why a recorded key is 72 hex characters
  rather than 64. Found by reading a working implementation before running the corpus, not by
  the corpus failing.
- **The cipher is a parameter, never inferred from material length.** The legacy path keys
  from a 32-byte fingerprint with a 16-byte key, so a length guess picks the wrong cipher and
  fails every packet on the one path with no corpus to catch it.
- **Reassembly is length-driven and ignores the last-fragment flag.** Keying on the flag works
  against a well-behaved sender and fails exactly when a tail is truncated or reordered.
- **The retransmission timeout is not the congestion level table.** It is per fragment and
  exponential in the retry count; the table classifies staleness, and the scan produces the
  count the controller consumes.
- **The stall escape jumps to the furthest resumable slot, never the nearest.** Jumping to the
  nearest crawls the window one gap at a time. Which slots are resumable is the caller's
  decision, because only the layer that understands the payload can tell a message start from
  the middle of one.
- **The first round-trip sample seeds the estimate outright.** Averaging against zero would
  leave it an order of magnitude low for the first dozen samples, and the retransmission
  timeout is built on it.
- **The core contains no `unsafe`.** Every path uses checked slicing. That was not a goal; it
  is what fell out of writing the parsers against hostile input, and it moves the `miri`
  obligation to `lowlat-common` where the risk actually is.

## 0: workspace and common primitives (2026-08-15)

**Added**

- Cargo workspace, edition 2024, eleven crates with the dependency direction enforced by the
  manifest. Directories are unprefixed; package names carry `lowlat-`. The shared library
  target is named `lowlat`, so it links as `liblowlat`.
- `lowlat-common`:
  - `clock`: monotonic time, **fractional-millisecond** intervals, absolute-deadline sleep
    built from `CLOCK_MONOTONIC` with a 200 us spin finish.
  - `wait`: address-based wait and wake over the raw futex on Linux, with a bucketed portable
    fallback. Wait and notify live in one module because they are one primitive.
  - `spsc`: bounded single-producer single-consumer ring, fixed capacity, no allocation after
    construction, never blocks, never grows.
  - `seq`: RFC 1982 serial comparisons.
  - `bytes`: bounds-checked fixed-width wire accessors, all returning options.
  - `log`: leveled logging with an application sink; trace compiled out in release.
  - `alloc_counter`: thread-local counting allocator behind a test-only feature.
- Deterministic swap of atomics and cells for model checking, so one body of ring code serves
  both the real build and `loom`.
- CI: ASCII check, format, clippy with warnings denied, tests, release build, model checking,
  dependency and license audit, sanitizers.
- `deny.toml`. The GPL denial is load bearing: codec libraries are loaded at runtime and never
  linked, and this is what makes a violation a build failure rather than a discipline.
- Pre-commit hook running the ASCII check on staged files.

**Notes**

- **The model check was shown capable of failing.** Weakening the producer's release store to
  relaxed makes `loom` report a causality violation. A passing check that has never failed is
  not yet evidence.
- **The ASCII checker was found silently passing.** It reported success while examining zero
  files, because a directory argument fell through its file filter. Fixed to expand
  directories, and the incident is cited in [08-testing.md](08-testing.md) 8 as the concrete
  case for the harness rule.
- Hardware encode confirmed working in the development VM by `scripts/probe-capture.sh`
  stage 3: 1080p60, 60 frames. That is what Phase 5 depends on, and it is now measured rather
  than inferred.
- Gate 3's wording was corrected from "strictly monotonic" to non-decreasing-and-advancing.
  The platform guarantees the former, not the latter, and asserting strict increase would test
  the timer's resolution rather than our contract.
