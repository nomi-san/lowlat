# Implementation plan

**Status:** locked 2026-08-15. Phases 0 to 12 with verification gates.

Conventions, per [AGENTS.md](../AGENTS.md) §2:

- Check items off with `- [x]` as they land.
- Every phase has a verification gate. **A gate is not opinion.** It is a command that passes
  or a peer that streams. If a gate cannot be stated as something observable, it is not a
  gate.
- One phase per commit. Changelog entry precedes the checkbox flip. Do not start a phase
  before the previous one is committed.
- Phases 9 and later require bare metal and a display. **Phase 9 runs before Phase 8**; the
  numbers are stable so that references to them keep resolving, and the order of execution is
  the change log's to state.

## The two gates that matter

**Gate A (end of Phase 5): a stock client renders frames from us.** Synthetic video, no
capture, no audio, no input. It proves the wire, both crypto modes, connectivity, signaling,
the encoder, and the packetizer all work together against a peer we do not control. Everything
before it is unverified in the only way that counts.

**Gate B (end of Phase 9): real desktop streaming.** Capture is the only stage between them.

## Prerequisites

- [x] **Session corpus captured** (2026-08-15). WAN 1080p60, 112 s, 438506 records, full
  coverage including a 529-fragment message, nack-flagged acknowledgements, and 11130
  retransmissions. Held outside the repo; the path and its handling rules are in project
  memory, not here, because the file carries a live session key.
- [ ] Test peer available: a stock client on a second machine or VM, reachable over the
  development network.

**Gap to close later:** the recording covers the 256-bit cipher only. The legacy 128-bit path
has no corpus and its Phase 1 coverage is therefore structural rather than byte-exact.

---

## Phase 0 - Workspace and common

Foundation. Nothing protocol-specific.

- [x] Cargo workspace, edition 2024, all crates from [00-overview.md](00-overview.md) present
  as skeletons with the dependency direction enforced.
- [x] `lowlat-common`: monotonic clock exposing **fractional milliseconds**, absolute-deadline
  sleep, the futex wait and notify pair as one primitive, bounded SPSC ring, byte order
  helpers, RFC 1982 sequence comparisons, logging.
- [x] Counting global allocator behind a test-only feature, plus the assertion helper that
  hot-path tests use.
- [x] `loom` configuration for the concurrency crate.
- [x] CI: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`,
  `python scripts/check-ascii.py`, `cargo deny check`.
- [x] Pre-commit hook running the ASCII check.

**Gate:** passed 2026-08-15.

1. [x] CI green on Linux. Format clean, clippy clean with warnings denied, 29 tests passing,
   ASCII check clean over 16 files.
2. [x] `loom` passes on the SPSC ring. **And was shown capable of failing**: weakening the
   producer's release store to relaxed makes loom report a causality violation, so the model
   check is exercising the orderings rather than passing vacuously.
3. [x] Clock: one million samples never go backwards and do advance, and a sub-millisecond
   interval returns a nonzero fractional value. *This is the regression test for the
   quantization bug; an integer-millisecond clock fails it.*
4. [x] **The zero-allocation harness is itself verified**: `harness_can_fail` allocates on
   purpose and must panic. A harness that cannot fail proves nothing.

**Not in scope:** anything that parses a packet.

**Note on gate 3.** The original wording said "strictly monotonic". The platform guarantees
non-decreasing, not strictly increasing, so the test asserts that the clock never goes
backwards and that it does advance across the sample set. Asserting strict increase would be
testing the resolution of the underlying timer, not our contract.

---

## Phase 1 - Protocol core

`lowlat-core`, sans-IO, `no_std`. The whole of [01-protocol.md](01-protocol.md) except
connectivity.

- [x] Record envelope encode and decode, both crypto modes, nonce derivation.
- [x] Cleartext data packets and the group acknowledgement, with the full flag validation
  matrix.
- [x] Channels, per-channel rings, reassembly, base advance.
- [x] Acknowledgement emission, negative acknowledgement, retransmission timeout, stall
  escape jumping to the furthest occupied slot.
- [x] Send window bounded by the peer ring depth ([01 §7](01-protocol.md)).
- [x] Path probe state machine ([01 §8](01-protocol.md)), including the compile-time assertion
  that no emitted datagram can exceed the absolute ceiling.
- [x] Control message framing and the opcode table ([01 §11](01-protocol.md)).
- [x] Fuzz targets: envelope, cleartext packet, control message, video header, reassembler.

**Gate:** passed 2026-08-16.

1. [x] **Corpus replay is byte exact.** 219253 recorded datagrams decrypt to their recorded
   cleartext, parse, and re-encode byte for byte; all 41437 message spans match their length
   prefix with zero resyncs. A second replay drives the received direction through a full
   session and lands on **exactly** the contiguous frontier the recorded peer claimed on every
   channel.
2. [x] Property tests: ten thousand cases per message type from a fixed seed, biased toward
   field boundaries, plus fragmentation and reassembly through a real ring and records on both
   ciphers.
3. [x] Fuzz targets clean: envelope 5.7M executions, packet 21.9M, control 29.2M, video 25.1M,
   reassembler 7.9M. No crash, no timeout, no leak.
4. [x] `miri` clean. **The core contains no `unsafe` at all**, so this reduces to
   `lowlat-common`, where the ring's uninitialized storage lives; that passes including the
   cross-thread case.
5. [x] Zero-allocation assertions on envelope, packet, receive ring, send ring, and a full
   session round.
6. [x] The crate compiles as `no_std` with no `alloc` on any data path.

**Not in scope:** sockets, threads, connectivity.

**Note on gate 3.** The stated budget is the continuous-integration one. These figures are a
20-second run of each target locally; nightly runs them unbounded
([08-testing.md §6](08-testing.md)).

**Note on gate 4.** The wording assumed the core would contain `unsafe`. It does not: every
path uses checked slicing, and the only `unsafe` in the workspace is in `lowlat-common`. The
gate is satisfied where the risk actually is.

---

## Phase 2 - Connectivity and the simulator

- [x] Connectivity checks on the shared socket, demultiplexed per [01 §2](01-protocol.md).
- [x] Candidate intake, server-reflexive discovery, punch state machine, all sans-IO.
- [x] Transaction identifiers derived from a per-session seed, so the core still needs no
  random number generator ([00-overview.md](00-overview.md) D4).
- [x] `lowlat-sim`: injected time, scripted loss, reordering, duplication, jitter, and a
  topology model.
- [x] Network namespace fixtures with real kernel address translation.
- [x] Fuzz targets: binding request, binding response, attribute parsing
  ([08 §6](08-testing.md)).

**Gate:** passed 2026-08-16.

1. [x] **The topology matrix, each case with its expected outcome stated**, in the simulator and in
   namespaces. Full cone, restricted cone, port restricted, and hairpin establish a direct
   path; symmetric reports `probe timeout` and must not report success, as does a pairing where
   only one side is symmetric. **Carrier-grade translation is decided by its mapping behaviour,
   not by the number of layers**: two layers that keep mappings endpoint independent are
   punchable and must establish, while a symmetric carrier translator must time out. **A
   symmetric side facing a peer that filters nothing must establish in both directions**, which
   it can only do by using the address a check arrived from rather than the one advertised.
   Every
   fixture is shown capable of failing before it is trusted, because "green" over a matrix
   whose interesting cases are failures is otherwise indistinguishable from a broken harness;
   the paired cases that differ by one behaviour flag are what demonstrate it.
2. [x] **A real session between two machines on different networks**: reflexive discovery against a
   public server, candidates exchange, checks pass, media flows. This is a genuine traversal
   through whatever the two routers impose, and it is the strongest evidence available for the
   one topology pair it happens to exercise. It is no evidence at all about the other five,
   which is why it does not replace gate 1 ([08 §5](08-testing.md)).
3. [x] A v4-mapped peer address is classified as IPv4. *Named regression test.*
4. [x] **A probe leaves the socket TTL at the value it found.** *Named regression test.* A
   probe-scoped TTL that is never restored caps the media path at a few hops, which presents as
   a connection that establishes and then carries nothing over any distance.
5. [x] Five percent uniform loss and five percent reordering across ten thousand simulated
   messages: every message is delivered, in order, and recovery stays inside the
   retransmission bound. *The frame-level form of this, bounded freeze with no reference chain
   broken, belongs to Gate A, where frames exist.*

**Not in scope:** the relay (Phase 2b); gateway port mapping ([03 §6](03-connectivity.md));
real sockets outside the fixtures and the two-machine run.

---

## Phase 2b - Relay

**Scheduled after Gate A, not after Phase 2.** Nothing before Gate A depends on it, and it has
no test surface until Phase 2's fixtures exist. Kept here because it is connectivity work and
belongs beside the rest of it; the plan's document order and its commit order differ for this
one item only.

- [ ] Relay client (RFC 5766): allocation, permissions, channel binding, refresh, and consent.
- [ ] The relayed address is advertised as an ordinary candidate, so the peer needs no relay
  support of its own ([03 §7](03-connectivity.md)).

**Gate:**

1. Relay fallback succeeds when direct connectivity fails, in the simulator and in the
   symmetric namespace fixture.
2. Relay framing overhead is accounted for in receive sizing; a full-size datagram survives the
   relay path. *Named regression test.*
3. A peer that does not answer checks arriving from the relayed address produces a typed
   outcome rather than a path that silently carries nothing.

**Scope:** client side only, against an ordinary external relay server. No server, no default
address compiled in, and reaching the relay over a stream transport is deferred. The datagram
clamp for both framings already landed in Phase 1 and is not re-litigated here.

---

## Phase 3 - IO shell

`lowlat-net`. [02-io-shell.md](02-io-shell.md) in full.

**Gate passed 2026-08-16.**

- [x] `endpoint`: one object owning both state machines, classifying each datagram and
  reporting the sooner of the two deadlines. Landed in `lowlat-core` ahead of the shell,
  because classification and timer merging are protocol decisions rather than IO ones.
- [x] Socket open with the complete option set, logging the granted receive buffer.
- [x] The bind walks forward over a bounded range when the configured port is
  occupied, with a kernel-chosen port only on explicit request.
- [x] `poll` plus batched receive, segmentation-offload send with per-datagram fallback.
- [x] The merged per-guest thread and its event loop, armed from `next_timer_ms`.
- [x] Per-datagram TTL applied and **restored** around a mapping probe.
- [x] Application send wake via `eventfd`.
- [x] Teardown that wakes every waiter.
- [x] `unsafe` confined to thin syscall wrappers, with the crate stating why `miri` cannot
  reach them.
- [x] Concurrency assurance, per the note below.

**Gate:**

1. [x] **A sustained loopback stream at the target packet rate.** Every datagram sent is received,
   and the granted receive buffer is never overrun. Ten minutes per commit, sixty nightly
   ([08 §11](08-testing.md)). The load is a synthetic generator; there is no encoder until
   Phase 5. *Ten minutes at 10009 datagrams/s: 6005082 messages sent, 6005082 received, no
   gaps, no kernel drops. See the note below on what the buffer had to be raised to.*
2. [x] Steady-state allocation count is zero. *Both loops, over the whole run.*
3. [x] Granted receive buffer size appears in the log at open.
4. [x] **Wake accounting, as a number.** Under a steady stream, timeout wakes do not exceed the
   count of armed deadlines by more than ten percent. A loop that polls instead of waiting
   shows an order of magnitude more, so the check distinguishes the two rather than describing
   a profile. *Sender 72/s, receiver 50/s, against the 1000/s a loop bound by the minimum wait
   would show. Stated against that ceiling rather than against the armed count, because every
   pass arms one and the comparison is otherwise vacuous.*
5. [x] **A probe leaves the socket TTL at the value it found**, asserted in the shell as well as in
   the core. *Named regression test.* *Confirmed on the wire as well: the probe leaves at the
   probe hop limit and the next check at the default.*
6. [x] Teardown under load joins every thread within 100 ms with no stranded waiter.
7. [x] **The namespace fixtures pass with the shell driving**, replacing the fixture loop in
   `crates/sim/src/bin/punch.rs`. That loop is deliberately the simplest thing that works and
   is not a preview of the shell; this is where the real one takes over.
8. [x] Ten thousand connect and teardown cycles show no per-cycle growth in memory, threads, or
   descriptors. Nightly. *Ten thousand cycles: descriptors 4 to 4, threads 2 to 2, resident
   memory unchanged. Both exact counts were shown capable of failing, by leaking a descriptor
   and by leaking a thread per cycle.*

**Note on concurrency assurance.** The rule is that every ring and every atomic handoff is
model checked. **This crate adds neither.** Its receive and send batches are single threaded by
construction, the application seam is a call rather than a ring, and the one shared word is a
teardown flag that publishes no payload -- a model of it passes under relaxed ordering too, so
it could not fail and would prove nothing. What actually closes the teardown race is the wake
descriptor, which a model checker cannot represent at all, because it cannot execute a syscall.

So the obligation is met where it applies rather than performed where it does not. The
primitives in `lowlat-common` remain model checked and were shown capable of failing at
[Phase 0](#phase-0---workspace-and-common); the checked build here is **ThreadSanitizer**,
which does cover kernel-mediated concurrency, and a churn test over spawn and teardown cycles
covers what a single pass cannot.

**Note on gate 8.** The lesson behind this soak is a crypto library leaking per-thread state
for every thread that touched it, at roughly 11 KB per connect cycle. **That mechanism cannot
occur here**: the primitives in use keep no per-thread state, so a gate phrased around
releasing it would pass without testing anything. The soak is kept for the leaks that can
still happen -- rings, threads, descriptors -- and the original cause is recorded as absent by
construction rather than as covered.

## Phase 4 - Signaling and admission

**Gate passed 2026-08-17.**

- [x] `lowlat-kessel`: transport, authentication, reconnection with backoff, host
  advertisement, and the message set from [04-signaling.md](04-signaling.md).
- [x] Host admission seam: register attempt, add candidate, approve returning credentials,
  end connection, plus the event queue.
- [x] Advertised capacity read from the configured guest limit, never hardcoded.

**Gate:**

1. [x] The host appears in the service's host listing.
2. [x] A stock client's offer is accepted through the seam, candidates exchange, and connectivity
   completes. *Established over the peer's LAN candidate against a stock client.*
3. [x] The session reaches the streaming state and holds it with no media flowing. *The client
   enters its remote view and holds it, reporting only that nothing is arriving.*
4. [x] Advertisement is emitted on state change only, never on a timer.
5. [x] Rejection and cancellation paths produce the correct typed outcomes. *A declined answer
   reaches the client as its declined status; a withdrawal and a connectivity failure each
   produce their own outcome.*

**Not in scope:** any video.

---

## Phase 5 - Encoder and Gate A

**Gate passed 2026-08-18.** All eight items, a stock client rendering for ten minutes, and the
two open questions this phase carried closed by measurement rather than argument: the refresh
burst never existed, and the fragmenting path now runs against a real peer. Three defects stood
between the loop and a picture and two were ours -- a session keyed without its nonce prefix, a
video flag set that names the colour depth, and a peer's refresh request dropped. None of them
could have been found without a peer we do not control.

- [x] `lowlat-encode` trait: asynchronous submit and poll, force keyframe, live bitrate
  reconfigure that never reinitializes. **`poll` never blocks**, which is a real constraint on
  both backends rather than a preference; see the note below. *Both backends implement it and a
  single generic loop drives both, so a difference either one still carried in its shape would
  be a compile error rather than an interface describing whichever was written first.*
- [x] **VAAPI backend**, H.264, 8-bit 4:2:0, low-latency parameters. First, not later: it is the
  encoder on the primary Linux target and on the machine this is tested against
  ([07 §3.1](07-platforms.md)). *Refresh and predicted pictures, one reference, parameter sets
  travelling with a refresh; content verified against the source frame by frame through an
  independent decoder.*
- [x] NVENC backend, same trait, same parameters. *Collect block audited on every hardware run
  rather than trusted; the length it reports is the length that was coded.*
- [x] `lowlat-capture` synthetic frame source, emitting planar frames directly so no conversion
  stage is needed before Phase 9. **The trait is deferred to Phase 9**; see the note on ordering.
- [x] **Frame pool and per-guest publish ring.** One encode serves every guest, and a backend's
  bitstream is only valid until its next poll, so the collected frame is copied once into a
  preallocated pool slot and the slot index is published on a bounded ring per guest, released
  by refcount after packetization. This is the first cross-thread handoff added since
  [Phase 0](#phase-0---workspace-and-common) and carries that phase's model-checking obligation.
- [x] **Per-guest delivery gate** ([05 §6](05-host.md)): the window ceiling test, the
  skip-until-keyframe latch, the running-maximum retest that releases a skipping guest, and a
  throttled global keyframe. A fresh guest starts pending, which is what produces its join
  keyframe rather than a separate arrangement. *Gate A item 4's named regression test lives
  here and was shown capable of failing: removing the latch fails it, and swapping the retest
  to the frame in hand fails that test alone.*
- [x] Two-channel ring geometry per guest, sized from the largest frame the stream can produce
  rather than from control traffic. *The video ring is the peer's ring depth, which is also the
  gate's top ceiling, and a test holds the two together. The fragment width is the datagram
  floor, because the slot width is the fragment width: a wider ring emits datagrams no probe
  has justified. Control receive was not attached at all until now, so a peer's declaration was
  dropped as unhandled while the acknowledgement reported zero for that channel forever.*
- [x] Session initialization: accept the guest's preferences and encoder configuration, honour
  the 5-second deadline ([01 §12](01-protocol.md)), and emit the encode-latency and encoder
  generation messages ([01 §11.2](01-protocol.md)). *The recording's own initialization is
  parsed in the replay, which is the rest of Gate A item 2.*
- [x] Packetizer and the video message framing ([01 §11.3](01-protocol.md)). *Every recorded
  video header re-emits byte for byte and reframes to the same fragment count, which is most of
  Gate A item 2; writing the rotation zero-based fails it.*
- [x] Congestion controller ([01 §10](01-protocol.md)) driving encoder bitrate, ticked once per
  guest per frame from the encode loop, **each guest's ceiling being the configured rate divided
  by the count of guests on that stream**, and the rate applied being the minimum across guests
  ([05 §5](05-host.md)). *Each of the three behaviours fails its own test when removed: the
  division, the minimum, and the deadband.*

**The loop that joins them, 2026-08-17.** Every item above existed and nothing called
them in order. `lowlat-host` gains `stream`: the capture and encode loop, the seats guests
take on it, and the lifecycle that starts an encoder on the first guest and holds none
before that. `lowlatd` gains the two dependencies and starts one stream. The loop is
generic over the encoder trait, so its tests drive it through a fake encoder with no
device, and the three ways a frame can be lost each latch their guest and each ask for the
refresh that recovers it.

**Gate A:**

0. [x] Both hardware backends encode the same synthetic source, so the trait is shaped by two
   implementations rather than one. *One generic loop drives both, so a difference either still
   carried in its shape is a compile error.*
1. [x] **A stock client connects and renders our synthetic frames**, 1080p60, for 10 minutes, with
   no corruption and no freeze beyond the loss budget. *Passed 2026-08-18, 640 seconds
   continuous:*

   | Measured at the client | |
   |---|---|
   | frame rate | min 59.6, median 60.0, **not one sample below 59** |
   | decode | 0.9 to 1.3 ms |
   | packet loss over 30 s | 0.00 percent, throughout |
   | retransmissions | zero, fast and slow both |
   | frames dropped from its queue | zero |
   | shutdown | clean |

   | Measured at the host, same run | |
   |---|---|
   | frames delivered | 39235 |
   | send window | 0 to 1 for the whole run |
   | stale fragments | zero |
   | refreshes the peer asked for | zero |
   | encode, capture to collected | 3.3 ms median |

   *A send window of zero to one across ten minutes is the strongest single line here: every
   fragment was acknowledged before the next frame was ready, so nothing in the delivery gate,
   the pool or the retransmission path was ever under pressure. Three decoder backends render
   the stream; a fourth refuses it, which is the open item below.*

   **Three faults stood between the loop and a picture, and only the first was ours.**

   - **The session was keyed from the media key alone, discarding the four-byte nonce prefix
     that follows it.** Every record we sealed was undecryptable by the peer and every record
     it sent failed our tag check, which presents as a path that establishes and carries
     nothing. The constructor used says in its own documentation that it is for fixtures and
     that a real session always carries a prefix; the seam was its only non-fixture caller.
   - **A peer builds one decoder from what it declared and never switches on what arrives.** A
     client declaring the codec we do not produce fails every frame and reports a decode error
     rather than a mismatch, so the host now says so plainly in its log.
   - **One decoder backend refuses our stream where two others accept it.** Same bytes, same
     client, same machine: the vendor backend fails and the software one renders. See the note
     below, because this is a real divergence rather than a peer defect.
2. [x] **Our emitted video stream is structurally indistinguishable from the session corpus.**
   Header layout, fragment sizing, message framing, and flags checked offline against the
   corpus, and our initialization parser accepts the corpus's client messages verbatim.
   *This runs without hardware and without a peer, and it is what makes item 1 diagnosable: a
   client that renders nothing after this passes has a negotiation fault, not a framing one.*
3. [x] **Zero keyframes across a bitrate reconfigure**, counted at the encoder over a run that
   forces repeated rate changes, and the encoder is not reinitialized. This replaces
   "reconfigure is observed live", which named no observation and could not fail. *120
   pictures and 119 rate changes on each backend, spanning 2 to 40 Mbps: exactly one coded
   refresh, the forced first picture, and one reported. Counted both ways, because a backend
   could report the flag correctly and still code a refresh. Forcing a refresh every thirtieth
   picture fails it.*
4. [x] **A guest that is starved and recovers never receives a dependent frame across the gap.**
   Driven in the simulator by withholding acknowledgements until the window fills, then
   releasing them. *Named regression test; this is the gray-frame lesson and it is the reason
   the gate moved into this phase.*
5. [x] **`poll` reports "not ready" without waiting**, on both backends. Submit a burst deep
   enough that the queued encode time is far longer than one frame, then time the polls. A
   not-ready answer must cost a driver round trip, not a frame; and most polls in the burst
   must be not-ready, or the probe is gating nothing.

   **Revised 2026-08-17, against hardware.** The original wording required the whole collect
   never to block, and on one backend that is not achievable: its own no-wait flag is
   documented for exactly this case and **is worse than useless** -- see the correction below
   -- and a completion marker on the encoder's stream passes before the bitstream is
   retrievable, because the encode does not run on that stream. Measured, a not-ready answer costs about 300 ns while retrieving a
   finished picture costs about one frame's encode time. So the gate now covers the part that
   is both achievable and load bearing -- a caller with nothing ready is never parked -- and
   the retrieval cost is recorded rather than wished away. **A gate nobody can pass teaches
   nothing; a gate that measures the real boundary teaches where it is.**
6. [x] **Encode overlaps the next frame's preparation**, asserted as measurement rather than as
   throughput: the per-stage times from [05 §10](05-host.md) sum to more than the wall-clock
   interval between frames. Stages that sum past the interval they fit inside can only have
   run concurrently. *A frame-rate target proves nothing here: the hardware on this machine
   encodes 1080p far faster than 60 fps, so a fully serialized pipeline would hold the frame
   rate and pass. The lesson behind this gate came from a 120 fps target, and the arithmetic,
   not the frame rate, is what carries it to 60.*

   **Measured unpaced, 2026-08-17**, because at 60 fps the pipeline idles most of the interval
   and the sum is under it whether the stages overlap or not. With the frame clock removed the
   interval is the pipeline's own throughput, which is where the question has an answer:
   stages sum to **10.670 ms across a 2.665 ms interval, 8.005 ms of overlap**, 375 pictures a
   second. Holding one picture in flight instead of four fails it, and prints the serialized
   shape exactly -- 3.064 ms of stages inside a 3.066 ms interval.
7. [x] Every stage in [05 §10](05-host.md) reports p50, p95 and p99, and the host-side stages sum
   to less than one frame interval at the negotiated rate. A pipeline that cannot clear a frame
   within a frame interval cannot hold the frame rate, so this is the floor rather than the
   target; a tighter budget is set once the first measurement exists.

   **Measured 2026-08-17**, 1080p60 on the open-stack backend, 660 frames:

   | Stage | p50 | p95 | p99 |
   |---|---|---|---|
   | acquire | 0.100 ms | 0.110 ms | 0.150 ms |
   | encode | 3.311 ms | 3.349 ms | 3.723 ms |
   | publish | 0.000 ms | 0.001 ms | 0.001 ms |
   | interval | 16.666 ms | 16.678 ms | 16.690 ms |

   **Host stages 3.411 ms at p50 and 3.874 ms at p99**, against a 16.667 ms frame: four times
   the headroom the floor asks for. The frame clock holds to 24 us of jitter at p99. Encode is
   the whole budget; the acquire is a generator and will grow when real capture replaces it,
   and publish is a copy and a counter. **The tighter budget this gate defers can now be set
   from a number**: one frame interval is 16.667 ms, the pipeline uses a fifth of it, and the
   figure to defend as capture and conversion arrive is the 3.9 ms tail rather than the floor.

**Open, found against a stock client, 2026-08-17.** One decoder backend refuses our stream
where the software one renders it. Our parameter sets differ from a recorded peer's in five
places, and two of them are the kind a strict decoder cares about: **picture order counts are
sent explicitly where the reference derives them from the frame number**, and our frame number
field is eight bits where the reference uses four. The reference's choice removes a wrapping
counter from every slice header. Wire compatibility is bug-for-bug, so the divergence is worth
closing on its own terms; that it is also the most likely explanation for the backend that
refuses us is what moves it up the list. The other three differences -- a constraint flag, the
video format code, and the declared time scale -- are cosmetic and recorded for completeness.

**Closed 2026-08-18: the multi-fragment path now runs against a real peer.** Until this the
synthetic picture coded to a few hundred bytes at any resolution, so every message we had ever
sent fit one fragment and the fragmenting path, a peer's reassembly, and the window arithmetic
had never met a message that had to be split. Raising the resolution does not help -- a bar on a
flat field is trivially compressible at any size -- so the source gained a band of detail
derived from the frame index, off by default and clear of the row the frame checker samples.

With 200 rows of it: every message spans more than one fragment, the largest is 112 of them, and
a stock client renders the result at 60 fps with zero loss, zero retransmissions and a send
window of zero.

**It found a real defect immediately.** The stream ran at exactly twice its configured rate,
at every setting: 3 Mbps produced 6.6, 10 produced 20.1, 30 produced 59.3. The encoder was never
told the frame rate, so it budgeted bits for the driver's default of thirty frames a second and
received sixty. A congestion controller actuating through that is wrong by the same factor and
would drive a path into loss while believing it was well inside budget. **The flat picture could
not have shown it**: at a tenth of a megabit nothing was ever near the target. Ten megabits now
measures ten.

**Closed by measurement, 2026-08-18.** This section carried an open question: the vendor
backend was said to spend 2.4 MB on its first picture where the open-stack one spends 513
bytes, and the plan required understanding it before Gate A because at sixty frames a second a
2.4 MB refresh is roughly 2000 packets in one frame interval and exceeds the delivery gate's
own ceiling at ordinary rates.

**There were never 2.4 MB of coded bits.** The figure was a length reported by a collect that
raced the driver, and the same picture had six hundred and fifty-one bytes in it. The race was
found and fixed the following day; this paragraph was simply never revised, so a fixed defect
stood as a gate condition for a day.

Measured now, at 1080p, 20 Mbps target, identical synthetic input:

| Quantiser floor | Refresh | Predicted mean |
|---|---|---|
| 5, the default | 651 bytes | 793 bytes |
| 10 | 651 bytes | 553 bytes |
| 22 | 499 bytes | 184 bytes |

A raw frame at this size is 3110400 bytes, so the refresh is under a thousandth of one. There
is no burst to design around. **The floor is the only bound that moves anything**: a ceiling
and an initial quantiser were both swept and neither changed the refresh or its quantiser, so
neither is configured -- a knob that does nothing invites tuning that does nothing.

The lesson is not about the encoder. **A number that has never been re-measured is a memory,
and this one gated a phase for a day after it stopped being true.** The measurement now lives
in the test suite rather than in a paragraph.

**Note on ordering, 2026-08-17.** The frame source was built before the encoder trait, which
inverts the order the items are listed in. The trait's shape is already settled by
[05 §4](05-host.md); what was not settled is what a frame *is* at this boundary, and that is
the only part of the trait a second implementation could still have moved. Writing the trait
first would have fixed the frame type against two backends that took no frame at all, which is
the degeneracy having two backends exists to prevent. Both now take the same frames, so the
trait can be written against something real.

**Note on the frame variant, 2026-08-17.** [05 §2](05-host.md) requires a captured frame to
move as a device handle and never as bytes, and the synthetic source produces system memory
instead. That is a deliberate narrowing rather than a readback creeping in. The rule protects
frames that arrive from a compositor already resident on a device; nothing is captured yet, and
a generator's output has to reach **both** hardware backends, which on a machine with two
vendors' parts are two devices sharing no allocation. No single device handle can satisfy that.
Each backend uploads into its own surfaces, the upload is explicit and per backend, and it
disappears when real capture arrives carrying a handle of its own.

Two pieces of the trait shapes in [05 §2](05-host.md) and [05 §4](05-host.md) are deferred with
their second implementations, rather than being written against nothing: `cursor_state` and the
acquire outcomes other than a frame, which only a real capture backend can produce, and
`accepts`/`FrameVariant`, which needs a second variant before a match against it can mean
anything.

**Note on the non-blocking poll.** Neither backend offers a completion callback on this
platform. One offers a usable non-blocking probe, a surface status query before mapping the
coded buffer. **The other's no-wait flag on the bitstream lock must not be used at all**; see
the correction below. The trait is only honest if the probing is done where it can be. Building against the blocking form and adding depth to
hide it produces a pipeline that stalls the moment the encoder falls behind, which is exactly
when it must not.

---

## Phase 6 - HEVC (closed 2026-08-18)

- [x] HEVC encode path on the vendor backend, and the codec and encoder chosen at startup
  rather than per guest, because one encode serves every guest. *A stock client decodes it:
  1920x1080 H265 at 60 fps, decode 1.2 ms, zero loss, and encode 2.6 ms against H.264's 3.3.
  Reached by selection rather than by a second pipeline -- the encoder trait means one generic
  loop drives either backend and either codec.*
- [x] HEVC on the open backend. *One generic loop over both codecs on both backends: a stock
  client decoded 2072 frames of 1920x1080 H265 from the open backend at 60 fps, decode 1.1 ms,
  encode 3.3 ms, zero loss and zero retransmissions. Three faults sat between a configured
  encoder and a decodable picture, and all three produce a stream that encodes without error
  (see the notes below).*
- [x] The two-place capability signalling ([01 §11.5](01-protocol.md), which is where it is
  described; the link here said §11.3 and was wrong). *Both places are read and the later wins.
  A peer may send only the first, so requiring both would leave every such peer declaring
  nothing -- confirmed against a live client, which declared H.264 in the initialization and
  H.265 in the encoder configuration eight seconds later.*
- [x] **A guest's reinitialization request changes what the stream codes** ([05 §6.1](05-host.md)).
  *A live client switched the session between the two codecs in both directions, mid-stream,
  with zero loss and zero retransmissions and one frame-rate sample below sixty across the
  change. The request is answered against the intersection of every seated guest's
  declaration, which is what the reference host does and what keeps one encode able to serve
  every seat. 4:4:4 and 10-bit are carried through the same path and reported as not emitted.*

- [x] **Ending a session with a reason** ([05 §6.2](05-host.md)). *Four reasons on one
  mechanism: no room, no encoder for what was asked, the device would not report its
  capabilities, and an encoder that stopped answering. Verified live: a host that cannot build
  an encoder ends the guest with that status and the peer reports it immediately, where before
  it waited ten seconds for its own no-video timeout and then blamed the network.*

  **D11's original refusal is not built, and should not be.** It said a seat that cannot decode
  the session's codec is disconnected by the host. A peer is the only party that can tell its
  decoder failed; it raises a decode error of its own and reports it through its own API. A host
  cannot detect the condition, so a refusal for it would be a guess. D11 is amended
  ([00](00-overview.md)) and the gate below with it.

**What the open backend's codec cost, and what it would cost again.** Three faults, each of
which encodes without error and is only visible in what a decoder makes of the output.

1. **The device codes at a sixteen-sample alignment and rewrites the size in the parameter set
   it is handed.** The standard allows eight, 1080 is a whole number of eights, and a set
   written that way therefore carries no conformance window. The device corrects the size to
   1088 in place and leaves the absent window absent, so the picture arrives eight rows too
   tall with nothing to crop it. The alignment cannot be read off one resolution -- 1080 and
   1000 both round up and 1200 does not -- so it is measured across three.
2. **Rate control has no handle on this codec unless the picture set gives it one.** The other
   codec carries a per-block quantiser delta unconditionally; here it exists only if a flag
   enables it, and without that flag the whole picture is stuck at the slice quantiser and the
   configured bitrate does nothing.
3. **Wavefront parallelism, declared and not wanted, puts entry point offsets in every slice
   header** -- byte counts into slice data that the side writing the header never sees.

Two of the three were found by encoding the same input with a second encoder on the same
device and comparing the two streams field by field. That is the cheapest way to separate what
the standard permits from what the hardware actually does, and it is the method to reach for
first next time.

**Gate:**

1. [x] A stock client negotiates and decodes HEVC, and both codecs are selectable. *Both codecs
   on both backends through one loop, and the negotiation half is done: a stock client moved a
   live session between them in both directions, mid-stream, and with two guests seated the
   move waited until both agreed. Zero loss and one frame-rate sample below sixty across each
   change.*

   **A test that looked like a pass and was not.** A client declaring H.264 also decoded the
   HEVC stream, which appears to make the refusal path unnecessary. It does not: the client
   used for that run sniffs the first parameter set and reconfigures its decoder, a behaviour
   it carries because a host can change codec underneath it. A peer without that sniff builds
   the decoder it declared and fails every picture, which is exactly the failure this project
   spent a day on from the other direction. The refusal is still required.
2. [x] **A guest the host cannot serve is disconnected with a status, and the peer reports that
   status rather than a timeout.** *Done: an unbuildable encoder ends the guest with the
   encoder-unavailable status, and the client logs `host sent opcode 10, status=-15000` and
   reports it, in place of the ten-second no-video timeout it used to blame the network for.*

   **Rewritten 2026-08-18, from "a guest that cannot decode".** That gate could only be passed
   by the host guessing at a peer's decoder, and the peer already reports its own decode
   failures. What is worth gating is the half a host can actually know: that it could not serve
   the guest, and that it said so.

**The original third clause was removed, 2026-08-17.** It read "a client without HEVC falls
back to H.264 without operator intervention", and **one encode serves every guest**
([05 §6](05-host.md)), so there is no per-guest fallback to have. The gate as written could
only have been passed by running a second encoder, which contradicts the design it was meant to
verify. D11 settles the question the other way and this gate is written from it.

**Where the refusal happens is fixed by the protocol, not chosen.** Codec capability does not
arrive with the offer; it arrives in session initialization ([01 §11.5](01-protocol.md)), which
is opcode 11 on the media path **after** connectivity has completed. So a signalling refusal is
not available -- at answer time we do not yet know what the peer can decode. The disconnect is
sent instead in the window between initialization and the first video message, which is the
first moment the capability is known and the last moment before anything has been sent that the
peer cannot use.

**The status enumeration is known and the values are chosen.** Opcode 10's argument 0
([01 §11.2](01-protocol.md)) is a status the peer already renders, and the four a host sends are
in [05 §6.2](05-host.md). They are the encode-side statuses rather than the decode-side ones,
because what a host can honestly report is what it could not do. **A status of zero does not
disconnect** -- the peer's control loop treats zero as "carry on" -- so none of them is zero.

**Closed 2026-08-18, verified against stock clients and two guests at once.** Both codecs on
both backends; a guest's declaration read from both places it arrives in; a reinitialization
request answered against what every seated guest can decode; and four reasons a host can end a
session, each reaching the peer as a status rather than as a timeout.

**One thing was found and not closed here**, because it belongs to multi-guest delivery
(Phase 11) and to the retransmission scan rather than to this phase: two guests on a wide-area
path behind a single uplink fill their send windows and retransmit at several times the
configured rate, recovering each time. Two guests on a local path do not come close. The
delivery gate does what it is meant to throughout, and no guest ever saw a broken picture.

---

## Phase 7 - Input injection (closed 2026-08-19)

- [x] `lowlat-inject` over the kernel input layer: keyboard, pointer buttons, wheel, and
  absolute and relative motion, as **separate relative and absolute pointer devices**
  ([07 §4](07-platforms.md)).
- [x] Usage-code to kernel-code mapping as a pure, unit-tested function.
- [x] Per-guest pressed-state tracking with release-all on disconnect and on the peer's own
  release message.
- [x] A **pure expansion from one control message to a batch of device events**, so the state
  machine is testable with no device and the write is one call per batch rather than one per
  axis.
- [x] The three permissions gated **inside the injector**, releasing what a permission holds
  when it is revoked ([05 §7](05-host.md)).
- [x] Virtual gamepads, **Xbox 360 layout only** ([07 §4.2](07-platforms.md)): one device per
  guest per pad identifier, capped, created on first use, destroyed on unplug and on
  disconnect.
- [x] Force feedback as the simple magnitude effect, reported back to the owning guest as a
  rumble message.
- [x] Events queued rather than dropped until a freshly created device is usable
  ([07 §4.1](07-platforms.md)), with a bounded queue and a stated overflow rule.
- [x] The three device-node failures told apart: module absent, group or rule missing,
  confinement refusing the create.
- [x] **One guest drives the pointer at a time**, optional and off by default
  ([05 §7.1](05-host.md)): the pointer belongs to whoever last moved it, lapses after a fixed
  hold, and an owner takes it without waiting. Keyboards and pads are not arbitrated.
- [x] Permissions and the owner flag read off the relayed offer, which carries both.

**Gate:**

1. Keyboard and pointer round-trip from a stock client to a real input device.
2. Absolute coordinates land on the correct pixel, including on a rotated output.
3. **No stuck keys after an abrupt disconnect** with keys held. *Named regression test.*
4. Mapping tests run without a device; injection tests are labeled and excluded by default.
5. **Revoking a permission releases what it was holding**, with nothing else disturbed.
   *Named regression test, no device required.*
6. A stock client's gamepad drives a real device: both sticks, both triggers, the direction
   pad, and every button, read back through an independent reader.
7. **No held buttons and no orphaned devices** after an abrupt disconnect and after an unplug.
   *Named regression test.*
8. A local application's rumble reaches the peer.
9. The readiness delay is not measured against one display stack alone, and the queue's
   deadline is set from the worst figure rather than the friendliest. *Answered by measuring
   where the delay actually is rather than by measuring twice: it is in device discovery, which
   every stack shares, and the display server's own share of it is nil
   ([07 §4.1](07-platforms.md)). A compositor session would still confirm its own share is as
   small; it no longer sets the figure.*
10. **A guest that loses the pointer while holding a button does not leave it down.** *Named
    regression test; the obvious implementation has this defect and it is invisible until two
    people share a session.*
11. Two guests type at once with the pointer arbitrated, and each drives its own pad.

---

## Phase 8 - Public C ABI

**Was deferred until Phase 9 closed; started 2026-08-21.** Capture is the last phase that can
force a header rewrite: the concrete `lowlat_host_config` field set and the output enumeration
both depend on the capture backend ([06 §14](06-api.md)), while everything after Phase 9 adds
only appendable surface, which [06 §11](06-api.md) permits without a version change.

**What actually held it was one field's meaning, not Phase 9's remaining work.** Appending a
field later is permitted; redefining one is not, so the only thing that had to be settled first
was whether a requested resolution exists and what it would mean. It does not -- see *Output
selection* above -- and what is left in Phase 9 touches no field that does exist.

**The size of the picture is reported, never requested.** The display decides it, the encoder
follows it, and a peer adapts to what it is sent, so the configuration selects an **output** and
caps a **frame rate** and says nothing about a resolution. A host that creates its own display
is the one case where a size is ours to choose, and it arrives with that display rather than as
a field that spends every other configuration reporting itself refused.

- [x] `lowlat-host` orchestration and the `extern "C"` surface from
  [06-api.md](06-api.md).
- [x] **Application messaging and output selection at the C boundary.** Both are wrappers over
  seams Phase 9 built and drove live -- `Admission::send_user_data` and the `UserData` event, the
  output listing and the selected output -- so what belongs here is the surface and nothing else:
  an event type in the tagged union, a send call, a listing call, and a configuration field
  settable while a session runs. The machinery is Phase 9's (see *Output selection*
  there); what belongs here is the surface: an identity an application can store and hand back,
  and each output's rectangle, which is the same quantity the input mapping is expressed
  against.
- [x] Generated header, opaque handles, versioned structs, stable-numbered enums.
- [x] `catch_unwind` at every entry point; unwinding enabled for the shared library.

**Six things the surface needs that the seam does not have yet.** The phase was written as a
wrapper over something live, and most of it is; these are the exceptions, found by reading
[06-api.md](06-api.md) against the code.

- [x] **A poll that blocks for its timeout, over a bounded queue.** Today's is a non-blocking
  read behind the same lock as every other call, on a queue that grows without limit.
  [06 §5](06-api.md) wants drop-oldest with a dropped count on the next event and `fatal` never
  dropped, and [06 §8](06-api.md) wants every other call to stay answerable while a poll is
  waiting -- so the queue has to come out from behind that lock. **This decides the locking
  discipline for the whole surface and is the first thing to design**, ahead of the header.
  Settled with it: **one lock for the seam**, with approving an attempt holding it, rather than
  a second lock and an ordering to get wrong; **the queue bounded in bytes as well as entries**,
  because a body reaches a megabyte; and **the body delivered into a buffer the caller passes to
  the poll**, so nothing is allocated on the application's behalf and no lookup key exists to go
  stale. A buffer too small reports the length it needed and leaves the event queued.
- [x] **Three of the four event types**: input owner changed, capture changed, and fatal, each
  raised where its change happens because that is the only place that can tell a change from a
  repetition. **`guest degraded` is deliberately not built**: the skip-and-resync cycle is the
  signal ([05 §6](05-host.md)) and what is missing is the threshold that makes a cycle chronic.
  Firing on every skip, or choosing a number nothing measured, would both be worse than the gap.
- [x] **Ending one guest with a reason.** Ending an attempt exists; it is addressed by attempt
  and carries no status, while the surface ends a *guest* and tells it why. The status has to
  reach the peer before the session goes, which the stream already knows how to wait for.
- [x] **Permissions mid-session.** Both are set once when a guest's thread
  starts and never revisited. The injector already takes a change; nothing above it can ask.
- [x] **A live configuration change.** Bitrate is the one an established client already asks for
  and is refused. The rate budget can be re-based and the encoder reconfigures without a
  keyframe ([00 §D8](00-overview.md)); what is missing is the path from a caller to either.
- [ ] **Encoder enumeration**, deferred for the same reason audio outputs are: the encoder
  follows the display, so today the answer is a consequence rather than a menu and the call would
  report nothing worth reading. Adding a function later is additive; a stub that always answers
  none is a worse answer than no call.

**Gate:**

1. A C# application drives a full session end to end using its own signaling. **It imports the
   shared library and nothing else**: the signaling is its own, written against its own runtime's
   sockets and JSON, because a seam proven by borrowing ours is not proven at all.
   *Written, and the boundary half passes* (`examples/csharp`): every call an integration makes
   runs from C# against the built shared object -- pre-flight, enumeration, start, the four-call
   seam, the roster, messages, permissions, a kick, the event pump and the log callback -- with
   **no marshalling directives in any structure**.
   **Passed 2026-08-22 against a stock client**: offer to established to a clean end, 2465 frames
   at 1920x1200, the codec renegotiated twice live (H.264 to HEVC and back, one reinit each,
   nobody reseated), input landing, and `acquire p50 1.85 / encode 3.24 / interval 16.68`. The
   client's own settings panel drove the host through the application protocol the example
   speaks, and **the bitrate changed from 10 to 30 Mbps while streaming**.
2. [x] The generated header compiles standalone under C and C++ with warnings as errors.
   *Included twice on purpose, which is the only thing a guard has to survive.*
3. [x] **A deliberately panicking call returns a status code rather than unwinding.** *Named
   test; this is undefined behavior if it regresses.* **It has to load the built shared library**, not
   link the same code as a Rust library: the thing being tested is that the shipped object still
   unwinds, and a test that links the library form inherits the test profile's answer instead of
   the shipped one.
4. [x] Every exported symbol carries the project prefix. *Checked mechanically against the
   symbol table, and against the header's own names, which is a second mechanism for the same
   rule: a constant is not a symbol and would otherwise pollute an application's namespace
   unchecked.*

*Two, three and four are one small C program*: include the generated header with warnings as
errors, open the shared library, call the panicking entry, and walk the symbol table.

---

*Everything below requires bare metal.*

---

## Phase 9 - Capture and Gate B

- [x] Capture backend selection and the display-stack decision from
  [07-platforms.md](07-platforms.md). *Closed by measurement; [07 §11](07-platforms.md) carries
  the result for each.*
- [x] Colour conversion by compute shader, writing planes directly. *One shader for every
  input depth, because a normalized format reaches a shader as float whatever its depth; the
  two-plane result is written through a view per plane, which measurement showed is the only
  way in.* Per-slot targets remain, with the frame ring.
- [x] **A conversion test that can fail without a display.** *Eight saturated colours against
  the transform computed on the processor from its own definitions. Exact agreement on both a
  card and the software driver; wrong coefficients move pure red's luma by eighteen levels.
  Runs by default, because it needs a driver and not a card.* The round trip against a real
  desktop is kept as a diagnostic but is nearly blind to the matrix: a desktop is mostly grey
  and a grey pixel carries no chroma for one to act on.
- [x] Zero-copy import from the capture handle into the conversion, and the converted frame
  back out as a descriptor. *Laid out so the colour plane begins exactly one luma plane in,
  because an encoder registering by pointer assumes that and has no field in which to be told
  otherwise; a driver left to lay out a two-plane image put it 49152 bytes further on.*
- [x] The encoder importing that descriptor and producing a decodable picture. *Done: thirty
  pictures at 2560x1440 from a real desktop, decoded outside the project as yuv420p, limited
  range, BT.709. That settles the layout, which nothing short of it could.*
- [x] Capture replacing the synthetic source in the stream loop, so the path reaches a guest
  rather than a file. *Two things only a loop shows: the display cycles through a pool of
  buffers, so a source that imports once reads one of them forever and produces a stream that
  decodes perfectly and never changes; and a conversion target per picture in flight, for the
  reason the encoder's own input surfaces are. Which node the display is on is discovered
  rather than configured, because a wrong setting is indistinguishable from no session.*
- [x] Cursor extraction and the visibility signal, **closed 2026-08-20**, verified against a
  stock client. Reading, encoding, the wire message, the per-guest cache gated on the client
  having advertised it, the forget flag, the cadence, the skip when the pointer is outside the
  stream, and the hotspot. *Classification is struck from this item: the shape is never
  classified, only the bitmap travels, so there is nothing to classify against.*
  **The shape has to be read and compared, not detected from metadata**: the pointer buffer's
  identity turns over as the pointer moves and says nothing about what it looks like -- and it
  is not usable as a trigger either, because a compositor redraws a pointer into the buffer it
  already had, measured at thirteen of nineteen shape changes in twenty seconds of ordinary
  hovering. The buffer is linear and maps directly, so this is a compare against the previous
  read rather than a device readback. **The mapping is uncached, so it is copied out in bulk
  before anything scans it**: walking it with a stride to find the drawn part costs an order of
  magnitude more than copying the same bytes and walking the copy, and only the rows a pointer
  occupies are copied. **Crop to the opaque extent**: the allocation is a fixed 256x256 whatever
  the pointer is, and almost all of it is transparent.
- [x] **The hotspot, which nothing reports**, closed 2026-08-20. The far side draws the picture
  against its own pointer, so the offset it applies is the one the host sends, and zero draws
  every pointer down and to the right of where it is. It is derived from the host's own
  injection: a guest commands a position, the display draws the shape with its point on it, and
  the difference is the hotspot. Sampled once per command on the read after it, refused unless
  it lands inside its own shape, and cached per shape.
- [ ] **Relative mode**, which this backend cannot supply on its own and is not blocked on
  anything else here. It needs the intent signal, which is session state above this backend;
  see the item below and [07-platforms.md](07-platforms.md).
- [x] **Measure whether the composited pointer disappears when a client takes the pointer.**
  *Answered 2026-08-19 against a real desktop: it does, and it also disappears for things that
  are not that.* Mouselook and a video player both remove it, which is the wanted behaviour and
  matches what the pointer's requested visibility would say. But it also disappears when the
  pointer merely grows past what the hardware pointer plane can carry, at which point the
  pointer is still on screen and simply drawn into the main image instead. **So the signal is
  the rendering one, not the intent one**, and relative mode cannot be driven from it: a guest
  shaking the mouse to find the cursor would have their pointer locked. The session-side helper
  is a prerequisite rather than an improvement ([07 §2.1](07-platforms.md)).
- [ ] **Rebuild the import when the scanout format changes**, without restarting the encoder or
  costing a keyframe. Not a rare event: see [07 §3.3](07-platforms.md).
- [x] **Absolute input placed within the captured output**, not spread over the whole desktop
  ([05 §7](05-host.md)). *The mapping is three steps -- clamp into the picture, convert into the
  captured output's rectangle, place that rectangle in the desktop -- and it collapses to what it
  was when there is one output.* **The rectangle and the desktop come from the session**, which
  is the only thing that knows them: a controller reports its position inside its own
  framebuffer, which reads as the corner whatever the desktop looks like, and a compositor's own
  virtual output has no controller at all. Matched to the captured output by the name both sides
  know it by, read once when the display opens, and absent is not degraded because one output
  already spans the axis. **The clamp is part of the fix, not tidiness**: a coordinate past the
  picture puts the pointer on the neighbouring output, where this host's pointer plane goes
  empty, which it cannot tell from an application hiding the pointer -- so the peer is told to
  switch to relative motion and has to walk its cursor back by hand.
- [x] **A guest without the pointer is shown that it does not have it**, rather than finding
  out by nothing happening ([05 §7.1](05-host.md)). Cursor updates are already per guest, so
  this is a different image to one guest and not a new mechanism.

### Application messaging

**Not in Gate B, and it is not capture.** It sits here because this is where it was needed and
where it can be proven: switching outputs for an established client requires the request to
reach the *application*, since the application is what interprets it
([05 §5](05-host.md)), and until this exists the SDK would have to learn a protocol that is not
its own. The C surface over it is Phase 8's.

- [x] **Both directions, uninterpreted** ([01 §11.2a](01-protocol.md) opcode 17). The framing had
  existed since Phase 1 and nothing used it: a message arriving was counted and dropped, and
  there was no way to send one.
- [x] Inbound, the body reaches the application as an event with its sub-identifier and the guest
  it came from. Outbound, to one guest or to all.
- [x] **The terminator is written on the way out and not required on the way in.** A peer reads
  the body as a C string, so one that ends without it is read past; but this is a pass-through,
  and refusing a message because a peer framed its own payload differently discards something
  the SDK was never entitled to judge.
- [x] **The body is built on the caller's thread**, so the thread serving a guest allocates
  nothing to send one.
- [x] **Answer what an established client asks for**, and tell it who is connected. The queries
  it sends, the configuration and output listing it expects back, and the roster it is never
  able to ask for are an application's protocol rather than this one's, so they are the
  daemon's. *Verified against a stock client: its own settings panel drives this host.*
  **The roster is what makes that panel exist at all** ([01 §11.2b](01-protocol.md)); the
  configuration exchange only keeps its values current, which is the opposite of what it looks
  like and cost a day to establish.

**Gate:**

1. [x] A message from a stock client reaches the application with its sub-identifier and body
   intact. *Chat, clipboard and both queries, with lengths exact.*
2. [x] A message sent to that client arrives, and one sent to every guest reaches each of them.
3. [x] **A body one byte over the ceiling is refused locally** rather than sent and dropped in
   silence at the far end.
4. [x] **A client's own settings drive this host.** *A stock client changed the output through
   its panel and the stream rebuilt under it: same guest, same channel, one coded refresh.*

---

### Output selection, and switching mid-session

**Not in Gate B.** It is written down now because absolute input placement made half of it
exist: an output has a name and a rectangle in the desktop, which is what selecting one needs.
The stream stays on the same channel throughout -- a guest is never reseated and nothing on the
wire is renumbered; only what feeds the picture changes.

- [x] **Enumerate the outputs.** Every display node, every lit controller on it, with its
  connector name, its picture size and its rectangle in the desktop. That last one already
  exists for the captured output; this generalises it to all of them. **The identity has to be
  stable and unique across cards**, because a connector name is only unique within one: two
  cards can each present a `DP-1`. Scoping the name by its node is the cheap answer and is as
  stable as the machine's own hardware ordering; an identity derived from the display's own
  serial is the alternative and costs a read this backend does not do today.
- [x] **Select one at open**, by that identity. **No selection keeps today's behaviour** -- the
  first controller found lit -- and a selection naming an output that is not present falls back
  to it and says so, because refusing presents to the far side as a machine with no session.
- [x] **Switch mid-session, through the mechanism a resolution change already uses.** A change
  of output is latched and the loop rebuilds around it, exactly as a mode change is
  ([Gate B item 6](#)). It costs one coded refresh; a picture cannot be absorbed into a stream
  built for another one, and a switch between two outputs of the same size is not a special
  case worth having -- the content is entirely different, so the refresh is owed either way.
  What must be republished with it is the picture's size **and its rectangle**, because both
  are the coordinate space absolute input comes back in ([05 §7](05-host.md)).
- [x] **A switch to an output on another card**, which is the same work as following a display
  that moved. *Done 2026-08-21 and verified across an AMD and an NVIDIA head on one desktop.*
  **The encoder is not a preference, it is a consequence of where the display is**: a conversion
  target is allocated on that device and an encoder belonging to another cannot take it. It is
  read from the driver behind the display and resolved on **every** rebuild, so a display that
  moves is followed rather than only noticed, and configuring one is an override.
  The device, the conversion and the encoder's registration are all bound to the node the
  display is on, so crossing cards rebuilds the encoder rather than the plane. **This is the
  same runtime backend re-selection the display-moved-to-another-card item needs**, and the two
  should be built once.
- [x] **The request reaches the SDK through the configuration, never off the wire.** An
  application asks for an output the same way it asks for a bitrate. **Nothing here interprets
  application traffic** to decide what to capture ([05 §5](05-host.md)); a host that did would
  be inventing a protocol on behalf of its application.

**Setting the mode is a different problem from selecting the output, and only one of them is
ours.** The established model does not scale a copy of the desktop, it *changes the display
mode*: a requested width, height and refresh are applied to the output and capture then follows
whatever the display became ([05 §7](05-host.md) already says the stream and the display are the
same size for this reason).

- [ ] **Selecting which output to capture is a read**, and this backend can do it alone: walk
  every device, take the lit controllers, and pick one. No permission beyond what capture
  already needs.
- [x] **Changing the mode of an output the session owns is not ours to do, and privilege is not
  what is missing.** Measured on this machine: one client at a time holds a display device, the
  session's compositor holds it, and a mode commit from anyone else is refused before the request
  is even examined -- **a root service is refused identically to an ordinary user**. That is a
  different kind of restriction from the one established hosts work around on other platforms,
  where display configuration is a call any privileged process may make; the experience does not
  transfer.

  **Decided 2026-08-21: this host does not set the mode of a display it does not own, and does
  not relay a request to do so either.** The mode of somebody's desk is changed where it is
  already changed, in their own display settings, and capture follows whatever the display
  became -- which is the behaviour Gate B item 6 already passes and the same behaviour an
  established host has. A peer adapts to the size it is sent, so nothing on the far side needs
  the change to have come from it. That removes the per-compositor output-management protocol
  from the plan entirely, and with it one of the session helper's customers.
- [ ] **A display this host creates is the exception, and it is the more important case.** A
  virtual display has exactly one client, which is us, so its mode is ours to set with no
  session involved at all -- which is what makes a requested resolution and refresh rate work
  properly for a headless host, and it is the product [07 §2.2](07-platforms.md) already
  separates out. **The two paths differ in who owns the display, not in what capture does with
  it.**
- [ ] **A frame-rate cap needs none of that.** Capping the encoder at a requested rate while the
  display runs at its own is already what this does, and it is the useful half of the request in
  every case where the mode cannot be set.

**So the split to hold:** the output is selected here, the mode is followed rather than set, and
a display this host creates is the one case where a requested size is ours to apply. That keeps
the whole feature working at the greeter, and it keeps a requested size out of the public
configuration ([06 §14](06-api.md)): a field that only ever reports itself refused describes a
stream nobody is producing, which is the one mistake this phase has already made four times.

**Gate:**

1. Enumeration lists every connected output on every card, with a rectangle that agrees with
   what the session reports.
2. A host told to capture the second output streams it.
3. [x] **A mid-session switch keeps the session**: same guest, same channel, one coded refresh,
   the peer follows the new size. *Passed 2026-08-21, driven from a stock client's own display
   chooser.* **A requested output MUST be checked against what is lit before it is acted on**: an
   unknown name is refused by failing to open a display, which ends every guest on the stream
   including the one that asked.
3a. **A requested mode is applied on a display this host created**, and the stream follows it.
   *Deferred with the virtual display itself.* On a display the session owns there is nothing to
   gate: a mode set anywhere is followed, which is Gate B item 6.
4. **Absolute input lands on the newly selected output**, which is Gate B item 5 arriving from
   the other direction and fails the same way if the rectangle is not republished with the size.
5. [x] A switch across cards either works or ends with a reason, and never streams a frozen
   picture. *Passed: an encoder built for the wrong device refuses every frame, and the refusal
   used to say nothing about why; it now names the device and the output.*
6. [x] **Capturing nothing in particular takes the main screen**, which is the output at the
   desktop's own corner rather than the first device enumerated.
7. [x] **What a peer is told it is watching is what the loop is capturing.** *Failed three ways
   before it held: described from configuration, from nothing, and from enumeration order.*

---

**Gate B:**

1. [x] **Real desktop streaming to a stock client**, 10 minutes, no corruption. *Passed
   2026-08-20: 10 to 12 minutes of a real desktop at 2560x1440, a 120 Hz host to a 60 fps
   client, nothing wrong.*
2. [x] No host-visible copy between capture and encode; a readback stage would appear in the log
   and does not.
3. [x] Cursor shape changes behave correctly, including entering and leaving a window drag.
   *Passed 2026-08-20 against a stock client, hotspot included: an I-beam and an arrow land on
   the same pixel as the host's.* **A shape change is detected from the pixels on a cadence and
   never from the buffer's identity**, which was tried and does not work: a compositor redraws a
   pointer into the buffer it already had, measured at thirteen of nineteen changes in twenty
   seconds of ordinary use.
   *Relative mode is held out of this gate*, and the measurement above is why. The two signals
   were shown to differ on real hardware: the pointer leaves the hardware plane both when an
   application hides it and when it is merely too large to sit there, and only the first means
   relative. Relative mode gets its own gate once the session helper exists
   ([07 §2.1](07-platforms.md)).
4. [x] With the pointer arbitrated, the guest that does not have it can see that it does not.
   *Passed 2026-08-20 with two guests: the one that does not hold the pointer is shown the
   desktop theme's refused shape, and the real cursor returns when the hold lapses.*
5. [x] **Absolute input lands on the captured display with a second display attached**, and on
   the correct one. *This cannot fail with one display, which is why it is a gate item.*
   **Passed 2026-08-21** against a stock client with a 4480x1440 desktop and a 2560x1440
   captured output: all four edges reached, and the pointer held at the border rather than
   crossing to the other output. *It failed first at exactly 57 percent of the width, which is
   the ratio of the two, and only on the horizontal, because both outputs were the same height.*
   **Two faults sat behind it and only the first was the mapping.** The second was that a peer
   which keeps pictures is sent a name, a name carries no hotspot, and the hotspot is derived
   after the picture has already travelled -- so every shape was frozen at no offset for any peer
   that caches ([05 §8.5](05-host.md)). Every earlier run used a peer that does not cache, which
   is sent the picture every time and corrected itself on the next frame.
6. [x] Capture survives resolution change and display hotplug. *Passed 2026-08-20: a mode
   change is followed, the stream rebuilt around it and the peer told, and the display link
   pulled and restored resumes the same session.* **A display that changes size cannot be
   absorbed**: the encoder and the conversion target are built for one size, so the new picture
   lands in a corner of a frame the rest of which never changes again and the peer is never
   told.

---

## Phase 10 - Audio

**The shape was decided 2026-08-22, before anything was written**, from one probe against the
real sound server and from what a client's own settings already offer. It is in
[05 §9](05-host.md); the framing is [01 §11.4](01-protocol.md); the platform question that used
to sit here is closed in [07 §7](07-platforms.md).

**Downlink first, in one phase. The guest microphone is a separate step below**, because nothing
can prove it end to end until an application owns a capture device in somebody's session.

**It lives in `lowlat-audio`**, its own crate. `lowlat-capture` carries a display stack and
`lowlat-encode` two vendor runtimes, and sound needs none of it: a machine with no graphics
device still has audio, and the reverse holds too. What the three share is the shape of the
problem and no code at all.

- [x] **The framing**, in the protocol core: fifteen bytes, encode and parse, with the channel
  mask and the channel count as fields rather than as constants. *Both are read by a receiver and
  a change of either rebuilds its decoder, so a host that hard-codes the pair is one layout change
  away from a header that disagrees with its own payload.*
- [x] **Capture from the default output's monitor**, over the sound server's own socket, with the
  library loaded at runtime. *A service is admitted to the session's socket without a credential,
  which is what makes this a stream rather than a helper. A named device is checked against the
  enumeration before it is opened, because one that does not resolve is substituted rather than
  refused.*
- [x] **Paced by the source, never by a timer.** *A read returns when the server has a fragment.
  Nothing here holds a frame clock, and nothing resamples, because there is no second clock to
  drift against.*
- [x] **Encode, with both codecs.** Opus for a guest that did not ask for anything else, and
  uncompressed for one that did and is allowed to. *The uncompressed payload is what capture
  already delivered, so the second encoding costs nothing to produce and only the bitrate is
  real.*
- [x] **One encode, fanned out**, with the per-guest choice made where the packet is handed over
  rather than where it is produced. *The same shape as a picture: a pool slot and an index, never
  a copy per guest.*
- [x] **Send whole or not at all**, and drop before numbering. *The channel is reliable and
  ordered, so a packet dropped after it is numbered is a hole the receiver waits on. A full window
  discards the packet instead, and the next one takes its place.*
- [x] **Decide what silence costs, per guest, and do not stop the instant sound does.** *A peer
  plays only once it has queued its minimum and waits that out again if it ever reaches zero, so
  the uncompressed path holds for two seconds after sound stops: past any pause inside speech,
  and still short enough that a quiet desktop stops paying for silence.* *A monitor delivers zeros rather than stopping.
  Measured: the compressed path already collapses them to 1.2 kbit/s against 128, so skipping is
  worth almost nothing there and worth the whole 1.54 Mbit/s on the uncompressed one -- and a peer
  whose buffer drains pays for it when sound returns. The encoder reports silence; the sender
  decides.*
- [x] **Follow the source, live.** The default output changing, the application naming a device,
  and something else in the session moving this host's stream all resolve to one path.
  *Silent on the wire: a receiver rebuilds only for a codec, channel-count or mask change, and a
  device switch is none of them.*
- [x] **Publish the source the host is on**, not the one it asked for.
- [x] **A capture that stops is taken again.** *A sound server that restarts takes every capture
  with it, and a capture ends on its own thread without telling anybody, so a host that opened
  one once held a thread that had already returned. Asked on the pass that already decides
  whether the room wants sound, which is the same rule one level down. Only a device that was
  once held is retried, because a machine with no sound server answers the same way every time
  and the attempt blocks the loop that encodes.*
- [x] **What sound costs a guest is reported**, in the line a live run is read from: packets sent,
  packets dropped, and the rate the channel is carrying. *Sound appeared nowhere in it, and the
  gate below cannot be measured without it.*
- [x] **The uncompressed bitrate comes out of the video ceiling** for that guest, not out of
  nothing. *The video rate controller cannot see it, and 1.54 Mbit/s is five percent of a thirty
  megabit session.*
- [x] **Open the sound device when the first guest arrives, close it when the last leaves.**
  *Nothing should hold a capture nobody is listening to, and it is what the setting below rides.*
- [x] **Silence the speakers at the desk while a guest is connected**, off by default
  ([05 §9.4](05-host.md)). *The tap is ahead of the device's own mute, so the speakers go quiet
  and the guest still hears everything.* **Restore rather than unmute**: read the state first and
  undo only what this host did, or somebody who muted their own speakers has them switched back on
  by a guest leaving -- in their absence, with the sound as the first warning. Read from the live
  configuration at every open, so the second guest of a session behaves like the first.
- [x] **The boundary**: enable, bitrate, permit-uncompressed, device and the local mute --
  **all of them live**, so unlike video there is no settled half and one structure serves as both
  what a host starts with and what the setter takes. Enumeration behind
  `lowlat_get_audio_outputs`, whose identity is the **monitor** of an output rather than the
  output, because that is the device a host reads. *Live includes a host that started with sound
  off: having a source and being switched on are two things, or `enabled` is the one field of
  that structure which is not live.*
- [x] **And the state beside the settings**, in the host status: whether a device is being read
  right now and which one it landed on. *The settings are the request. An empty device asks for
  the default output, and `enabled` goes on saying yes after a capture has died, so neither can
  answer what sound is doing -- and the settings are not rewritten to the resolved name, or an
  application that reads them and writes them back would pin a host that was following the
  default.*

**Gate:**

1. [x] Audio reaches a stock client and plays, from the real desktop. *Passed 2026-08-23, both
   codecs, on a peer that chose each in its own settings.*
2. [ ] **Thirty minutes with no drift**, measured as the packets a host sent against the samples
   a client played, not as an impression; the host half of that is the `snd` count on the guest
   line. **Expect at most one resync rather than none**: a peer plays out of a buffer it flushes
   at either edge, and two sound-card clocks differ by tens of parts per million, so one edge is
   reached every twelve to fifty minutes depending on the peer. A host that produced none would
   be one resampling to a feedback loop, and **a resync is not a failure of this item** --
   confirmed 2026-08-23. *Fifteen minutes with two guests that day: no drift, nothing audible.
   That is half the run, and the half not done is the half where the resync is expected, so what
   it has shown is that there is no fast drift rather than that the slow one behaves.*
3. [x] **A source change is survived cleanly** -- the person switches their output device
   mid-session and audio follows it, with no reconnection and no picture disturbed. *Passed
   2026-08-23 against a second output, with the two ways it can fail made survivable first: a
   switch that will not open keeps the device in use, and a device that stops delivering is taken
   again.*
4. [x] **A guest that asks for uncompressed gets it**, and a guest that did not is unaffected in
   the same room. *Passed 2026-08-23, including a guest of either kind joining a room that
   already held the other: nothing the seated guest was hearing was disturbed.*
5. [x] **Silence costs nothing**, checked on the wire rather than by listening. *Closed
   2026-08-23 on the host's own accounting rather than on a capture of the wire: the encoder
   reports silence, a measured run of a silent desktop priced it at 1.2 kbit/s against the 128 it
   carries with sound, and the uncompressed path stops after its two-second hold. `snd_mbps` on
   the guest line is the number if it is ever doubted; the compressed path keeps sending, so the
   packet count cannot tell a quiet desktop from a loud one and the rate can.*
6. [x] **The local mute silences the speakers and not the stream**, and a device the person had
   already muted is still muted after the last guest leaves. *Passed 2026-08-23 -- and then
   qualified the same day: it holds on a device that applies its own mute and cannot hold on one
   whose mute the sound server applies, because there the mute reaches the mix the capture reads.
   Measured both ways. The host refuses on the second kind rather than silencing every guest, so
   what this item now says is that the promise is kept or declined, never broken*
   ([05 §9.4](05-host.md)).

**A packet must decode to less than about 40 ms**, whatever the codec would allow: a peer holds
one packet per slot of its decoded queue and the slot is 8000 bytes, which is 41.6 ms of stereo.
The 20 ms this host sends is half of it, and that is the reason not to trade latency for bitrate
with longer frames later.

### The guest microphone, after the gate

Its own message rather than the audio channel, mono at 48 kHz in 10 ms packets, compressed or
not. **A shared library has no business creating a capture device in somebody's session**
([06 §13](06-api.md)), so the SDK decodes and the application receives sixteen-bit samples and
decides what to do with them.

- [x] **The enable, which is what makes a peer send at all.** A peer streams its microphone only
  when it has been told this host will take it; told nothing, it mutes itself and sends nothing,
  however its own settings are configured. *This is the half that was missing from this section:
  a host cannot simply listen. The willingness to receive and the message announcing it are one
  switch, and a host that will not take microphone audio must not claim it will.*
- [x] Its own poll, not the event queue. *The queue is bounded and drops oldest; a hundred audio
  packets a second competing with control events would evict what must not be dropped.*
- [x] Decoded at the boundary. *The codec is already loaded for the downlink, and an application
  that has to learn one is an application that will get it wrong.*
- [x] **Contained, because these are a guest's bytes.** The decoder is the first thing in this
  system to parse something a peer chose, and it is a port that panics on a malformed packet
  often enough to be found by fuzzing rather than by luck. *Measured: seventeen of forty thousand
  random payloads panicked rather than erroring, and **the failure is a property of the decoder's
  accumulated state rather than of any one packet** -- the same bytes decode cleanly on a fresh
  decoder, so the regression test replays a sequence and the fuzz target feeds sequences. A
  contained panic throws the state away and counts itself; the count travels in the guest line
  beside the ordinary refusals.*
- [x] **Live against a real peer.** *Passed 2026-08-23: 11,266 packets from a stock client with
  nothing dropped, nothing refused and no contained panic. It took one fix -- the selectors are
  the header's second and third arguments, not its first two, the first being the declared length
  -- and the mistake was invisible until a census of what a peer really sends was added, because
  passing over another device's message is correct behaviour for this opcode.*
- [ ] The example grows a capture device, or the uplink cannot be shown to work at all.

---

## Phase 11 - Multi-guest, software encode, and VAAPI

- [ ] **A guest whose peer stops answering with a full send window is ended.** It is not today:
  the window climbs to its cap, every fragment goes stale, and the host retransmits at three
  times the configured rate indefinitely while the guest is never reaped. **This is a
  correctness bug rather than a throughput one and does not belong behind the rest of this
  phase.** A window at its cap with every fragment stale is enough to end a guest on its own,
  without waiting on a deadline.
- [ ] Per-guest pressure gate and the skip-until-keyframe cascade, expressed so a skip cannot
  be issued without latching the pending-keyframe state.
- [ ] Consensus actuators and the degraded-guest event.
- [ ] FFmpeg software encoder, dynamically loaded, resolved by name.

**Gate:**

1. Two stock clients stream from one encode simultaneously.
2. A guest starved deliberately recovers through a keyframe resync without disturbing the
   other.
3. **The software path runs the full pipeline with no GPU present**, which is the continuous
   integration path.
4. No GPL-licensed library appears in the link graph. *Checked mechanically.*

---

## Phase 12 - Daemon and tray

- [ ] `lowlatd` as a system service, with the unit file and device access rules.
- [ ] `lowlat-tray` over a Unix socket with peer-credential authentication.

**Gate:**

1. The service starts at boot and accepts a connection with no user logged in.
2. **A stream survives the tray exiting and the user logging out.**
3. The tray attaches and detaches repeatedly against a running stream.
4. Peer-credential authentication rejects an unauthorized local user.

---

## Change log

Newest first. Record approach changes and gate revisions here; per-commit detail belongs in
[changelog.md](changelog.md).

- 2026-08-25: **Copied text is the helper's fifth customer, and its gate has four values.** A
  clipboard is an ownership rather than a value: setting one announces that you own the
  selection and the bytes are asked for later, when somebody pastes, so a program that writes
  it and exits takes it with it. There is no one-shot form, on either display stack, which
  makes this the customer that most needs something living in the session -- it can appear to
  work anyway where a desktop's clipboard manager keeps a copy, and that is somebody's
  configuration rather than a design. **The policy is the service's, not the boundary's**,
  because copied text travels as an opaque application message the library never sees.
  `guest_clipboard` takes `off`, `send`, `recv` or `both`, named from the host's point of view,
  with anything unrecognised meaning `off` so a typo cannot open a clipboard. **Four values
  rather than a switch, because the directions are not equally dangerous**: sending ships
  whatever the person copied, including what a password manager put there, while receiving
  leaves a person to choose whether to paste. A peer that owns the machine is not a guest and
  is not subject to it. Written up in [07 §5.1](07-platforms.md).

- 2026-08-25: **The session helper is one binary in a second role, and the session connects
  outward.** Everything that needs session state -- relative mode, the idle inhibitor, the
  display layout, and display mode or rotation -- was blocked on a system service being unable
  to reach a desktop session. Measured: the session's message bus refuses a service outright;
  a compositor's own socket is reachable but differs per compositor and one major desktop
  offers no such protocol at all; starting a process inside the session works but makes the
  service discover a session, drop privilege and guess a desktop. So the direction inverts.
  The session side connects to the socket the service already listens on, which removes the
  problem rather than solving it and arrives with an identity, since a local socket carries
  the peer's credentials. **The helper is `lowlatd` in a session role rather than a second
  program**, because the two sides speak a private protocol and one build cannot disagree with
  itself; the role is chosen by the first argument and never by a flag, since a file that can
  be talked into the wrong privilege is a security defect. The tray stays separate: it links a
  user-interface toolkit that has no business in a system service. Shape and rules in
  [07 §5.1](07-platforms.md). **Relative mode is the customer it gets built around**, because
  its signal is continuous and will shape the channel properly, where a single request and
  reply would shape it wrongly and be bent later.

- 2026-08-25: **The third encoder carries both codecs, and what it owed is paid.** It was
  offered as an explicit choice that owed HEVC, predicted pictures and a live session; all
  three are done, and a stock client has streamed HEVC on it. The rule that made it correct
  is worth keeping: **a sequence set declares whole units of what the device says it
  accesses, not of the codec's smallest coding block.** The block is all the codec asks for,
  and declaring it produced a stream that decoded without one error, reported the size that
  was asked for, and had every row right except the last partial row of blocks. What remains
  for this backend is the coverage question -- which hardware it strands -- rather than a
  missing piece, and that is a product decision rather than a technical one.

- 2026-08-25: **The encoder set is settled: two defaults, one opt-in, and a fallback
  conversion tier.** The vendor and open backends stay the shipped pair, chosen by following
  the display. The Vulkan Video backend becomes a third, explicitly selected encoder and is
  never chosen on a machine's behalf. The GL conversion becomes the fallback for devices
  without the compute interface, rather than a measurement knob. What forced the question
  was the third encoder existing; what settled it was measurement: the 1.45 ms a frame once
  attributed to two interfaces sharing one device was mostly the integrated device's compute
  wakeup, which the poke now pays, leaving on the order of a tenth of a millisecond -- not
  enough to pay for stranding the hardware the shipped pair covers. Wiring order: selection
  collapses into one place as part of adding the third member, not before; the fallback tier
  logs which interface it chose; the third backend is offered only once HEVC, predicted
  pictures and a live session pass on its path, and a device whose format query refuses a
  storage-writable encode source is refused rather than served through a hidden copy.

- 2026-08-19: **Phase 9 runs before Phase 8, and relative mode leaves Gate B.** The premise for
  putting the ABI first was that everything up to it needs no display hardware, which stopped
  being true when the development machine became bare metal with two drivers and the capture
  probes ran on it. Capture is now the last phase that can force a header rewrite, and the ABI
  is the piece with no unresolved question left in it, so holding the ABI costs nothing and
  holding capture costs the whole product. Numbers stay as they are, because renumbering
  invalidates every reference to them from the other documents.

  **Relative mode comes out of Gate B** and gains a measurement instead. It is driven by the
  pointer's requested visibility, which lives in session state that a backend below the
  compositor cannot read, and the composited-pointer signal the backend *can* read is a
  different state wearing a similar name. The question worth answering first is not how to
  build the helper but whether the two signals coincide on this stack, which one mouselook
  application answers in a minute. Cursor shape and the per-guest arbitration image are
  unaffected and stay in the gate.
  have.** The readiness figure is a device-count figure and not a per-device one, so three devices
  cost nearly double what one was measured at. A virtual pad has to borrow a real controller's
  identity or nothing maps it, and the mapping that identity selects has to be checked against
  what the device emits rather than assumed. A guest holding a pointer button has to keep the
  pointer, because a gesture made of a held button sends nothing and so cannot be told from a
  finished one by elapsed time -- the obvious implementation takes the button away mid-drag.
  And gate item 9 was answered by asking a better question: rather than measure the same delay
  against a second display stack, measure **where** it is. It is in device discovery, which every
  stack shares, and the display server's own share of it is nil, so the figure travels rather than
  two figures happening to agree.
- 2026-08-18: **Phase 7 gains pointer arbitration, and signaling's half of permissions.** The
  relayed offer turns out to carry both the permission set and an owner flag, so the gate built
  for Phase 7 is no longer half-connected and the owner override has a real source. Arbitration
  was not in the plan and belongs in it: with two guests on one desktop the pointer is the single
  device they genuinely contend for, because the display stack merges every pointer on a seat
  into one cursor. **It covers the pointer only.** Keyboards do not conflict that way and pads
  are each their own device, so arbitrating either would stop two people using one session and
  buy nothing. **The decision is host-wide and the enforcement is per guest**, which is not where
  the obvious implementation puts it: gating before injection loses the release of a button whose
  holder has already gone quiet, and leaves it down on a machine somebody else is driving. Gate
  item 10 exists for exactly that, because it is invisible with one guest.
- 2026-08-18: **Phase 7 takes on gamepads, permissions and force feedback, and gains five gate
  items.** Four of the additions were already required by [05 §7](05-host.md) and
  [07 §4.1](07-platforms.md) and were simply unwritten in the plan: the permission gate, the
  queue that covers a freshly created device's unusable window, the split between the three
  ways the device node can refuse, and the pure expansion that makes the rest testable without
  hardware. Gamepads are the genuine addition. They were listed as deferred against four
  received opcodes and one sent one, which cannot stay: a peer sends pad input in an ordinary
  session, and a host that accepts and drops it presents as a controller that does not work
  rather than as a feature that is missing. **Only the Xbox 360 layout is emulated**, on the
  kernel input layer; the controller families that need the HID layer buy identity rather than
  capability, since a peer sends the same button layout whichever it is holding, and that
  identity costs a device-setup handshake with the kernel's own driver
  ([07 §4.2](07-platforms.md)). **Force feedback is included rather than deferred** because the
  device must declare it or applications see a pad that cannot rumble, and declaring only the
  simple magnitude effect removes the envelope simulation that the shaped effects would drag
  in. The **permission gate is placed in the injector rather than in the message loop**: it is
  not a filter, since revoking a permission releases what it holds, and that is the same
  invariant a disconnect uses. Gate item 9 exists because every readiness figure so far comes
  from one display server on one machine, and the deadline the queue is built around must not
  be set from the friendlier of two numbers.
- 2026-08-17: **Correction to the entry below: the no-wait flag is not ignored, it answers
  wrongly.** Recorded first as "ignored by the driver", which was measured from the outside and
  read one way too kindly. Set, the lock returns success on a block the driver has not finished
  writing: the coded bytes are in place and the length is not. That produced a refresh picture
  claiming megabytes it had not coded and a slice count that was noise, both of which were taken
  for driver defects for a day. **A flag that is ignored is harmless and invites a retry on a
  newer driver; a flag that returns a wrong answer must never be set.** It also buys nothing:
  measured both ways the lock costs the same 0.7 to 1.8 ms. The narrowing below still stands,
  because the collect still cannot be made non-blocking -- only the reason changes.
- 2026-08-17: **Gate A item 5 is revised against hardware, and narrowed to what is
  achievable.** It required a collect that never blocks. One backend cannot provide it: the
  no-wait flag its own header documents for this exact case cannot be used, and a
  completion marker recorded on the encoder's stream passes early, because the encode runs on
  a hardware engine rather than on that stream. Both were measured rather than reasoned about.
  What is achievable, and is what the pipeline actually needs, is that a caller with nothing
  ready is never parked: a not-ready answer costs about 300 ns against a frame time of about
  1.8 ms. The gate now requires that, plus evidence the probe is gating at all, since a probe
  that always says ready would satisfy a latency bound while proving nothing. The retrieval
  cost is recorded as a number instead of asserted, because pretending otherwise would make
  the item pass against the behaviour it exists to reject.
- 2026-08-17: **Phase 5 gains the delivery gate, and Gate A gains four checks that can
  fail.** The gate was scheduled for Phase 11, which cannot be right: Gate A item 1 asserts no
  corruption over ten minutes, and the moment a send window fills the packetizer must choose
  between blocking the drain and breaking a reference chain. Both are already forbidden, so
  the third answer -- skip this guest and latch until the next keyframe -- is not a
  multi-guest refinement but the only defined behaviour, and a fresh guest starting in that
  latched state is also what produces its join keyframe. Phase 11 keeps what is genuinely
  multi-guest: consensus actuators, the acquire-time frame gate, and the degraded-guest event.
  Three items were added alongside it because they are load bearing and were unwritten: the
  frame pool and per-guest publish ring, which is the first cross-thread handoff since Phase 0
  and inherits its model-checking obligation; ring geometry sized from the largest frame the
  stream can produce, since the corpus's largest video message needs more slots than the
  provisional ring has; and the session-initialization exchange, without which nothing
  streams. **Two gate items could not fail as written.** "Reconfigure observed live" is now a
  keyframe count of zero across forced rate changes, and the end-to-end item, which cited a
  budget that exists in no document, is now the frame interval as a floor with the real budget
  set from the first measurement. The corpus check is new and is the one that runs with no
  hardware and no peer, which is what makes the live run diagnosable rather than a guess.
- 2026-08-17: Phase 4 closed. Five gates, all against a stock client on a real
  service. The phase's lasting finding is not in the gates: a connection that
  drops every two minutes and reconnects inside a second looks identical, from
  the listing, to one that never drops. Recovery masks the fault it recovers
  from, so the check is drops per hour rather than whether the host is visible.
- 2026-08-16: Phase 4 gate 4 was briefly inverted and is restored. A capture showed a ten
  second advertisement cadence, which was read as the SDK's own timer; it was the application
  driving it from above. The emission is dirty-flag driven and there is no periodic caller.
  **The lesson is about the measurement, not the gate:** a cadence was attributed to the layer
  being studied without controlling for the layer driving it, which is the same error as
  inferring a mechanism from an absence and cost a document correction in both directions.
- 2026-08-16: Phase 3 gate 1 records a deployment requirement. On a stock kernel
  the granted receive buffer is a fraction of what is asked for, and ten minutes
  at the target rate loses 1648 datagrams to it. Recovery carried all of them and
  no message was lost, but the gate is about the buffer and not about recovery,
  so it is measured with the ceiling raised and the stock figure kept as the
  reason the service must raise it. Gate 4 is stated against the rate a loop
  bound by the minimum wait would show, because every pass arms a deadline and
  comparing timeout wakes against the armed count can never fail.
- 2026-08-16: Phase 3 gains the port walk. `Socket::open` bound once and returned
  the error, so a host whose configured port was occupied failed to start. The
  walk is bounded, the ephemeral fallback is a separate call rather than the
  default, and the bound port is read back when it is taken. Added to the phase
  because it is behaviour the shell owes rather than a gate revision.
- 2026-08-16: Phase 2 split. The relay moves to Phase 2b, scheduled after Gate A: nothing
  before Gate A depends on it, it has no test surface until Phase 2's fixtures exist, and its
  datagram clamp already landed in Phase 1. Gateway port mapping leaves the phase entirely.
  Gate 1 now states an expected outcome per topology instead of "green", because half the
  matrix is expected to fail. Gate 5 loses its frame-level wording, which needed an encoder
  that does not exist until Phase 5. Two gate items added: the two-machine end-to-end run, and
  the TTL restore regression.
- 2026-08-15: plan created.
