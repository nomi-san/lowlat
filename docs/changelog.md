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

**Notes**

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
