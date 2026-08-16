# Changelog

Newest first. One entry per phase; approach changes and gate revisions go in
[impl-plan.md](impl-plan.md) instead.

## 3: io shell (in progress)

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

**Notes**

- **A bind failure is not a startup failure.** The walk takes 50 ports from the
  configured one, each attempt on a fresh descriptor, because the option set is
  applied before the bind and cannot be retried on the socket that carried it.
  It stops at the top of the range instead of wrapping: wrapping lands on the
  privileged ports, where the bind fails for an unrelated reason and reports it
  as though the range were occupied.
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
