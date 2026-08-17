# Implementation plan

**Status:** locked 2026-08-15. Phases 0 to 12 with verification gates.

Conventions, per [AGENTS.md](../AGENTS.md) §2:

- Check items off with `- [x]` as they land.
- Every phase has a verification gate. **A gate is not opinion.** It is a command that passes
  or a peer that streams. If a gate cannot be stated as something observable, it is not a
  gate.
- One phase per commit. Changelog entry precedes the checkbox flip. Do not start a phase
  before the previous one is committed.
- Phases 0 to 8 are testable without display hardware. Phases 9 and later require bare metal.

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
- [ ] **Per-guest delivery gate** ([05 §6](05-host.md)): the window ceiling test, the
  skip-until-keyframe latch, the running-maximum retest that releases a skipping guest, and a
  throttled global keyframe. A fresh guest starts pending, which is what produces its join
  keyframe rather than a separate arrangement.
- [ ] Two-channel ring geometry per guest, sized from the largest frame the stream can produce
  rather than from control traffic.
- [ ] Session initialization: accept the guest's preferences and encoder configuration, honour
  the 5-second deadline ([01 §12](01-protocol.md)), and emit the encode-latency and encoder
  generation messages ([01 §11.2](01-protocol.md)).
- [ ] Packetizer and the video message framing ([01 §11.3](01-protocol.md)).
- [ ] Congestion controller ([01 §10](01-protocol.md)) driving encoder bitrate, ticked once per
  guest per frame from the encode loop, **each guest's ceiling being the configured rate divided
  by the active count**, and the rate applied being the minimum across guests
  ([05 §5](05-host.md)).

**Gate A:**

0. Both hardware backends encode the same synthetic source, so the trait is shaped by two
   implementations rather than one.
1. **A stock client connects and renders our synthetic frames**, 1080p60, for 10 minutes, with
   no corruption and no freeze beyond the loss budget.
2. **Our emitted video stream is structurally indistinguishable from the session corpus.**
   Header layout, fragment sizing, message framing, and flags checked offline against the
   corpus, and our initialization parser accepts the corpus's client messages verbatim.
   *This runs without hardware and without a peer, and it is what makes item 1 diagnosable: a
   client that renders nothing after this passes has a negotiation fault, not a framing one.*
3. **Zero keyframes across a bitrate reconfigure**, counted at the encoder over a run that
   forces repeated rate changes, and the encoder is not reinitialized. This replaces
   "reconfigure is observed live", which named no observation and could not fail.
4. **A guest that is starved and recovers never receives a dependent frame across the gap.**
   Driven in the simulator by withholding acknowledgements until the window fills, then
   releasing them. *Named regression test; this is the gray-frame lesson and it is the reason
   the gate moved into this phase.*
5. **`poll` reports "not ready" without waiting**, on both backends. Submit a burst deep
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
6. **Encode overlaps the next frame's preparation**, asserted as measurement rather than as
   throughput: the per-stage times from [05 §10](05-host.md) sum to more than the wall-clock
   interval between frames. Stages that sum past the interval they fit inside can only have
   run concurrently. *A frame-rate target proves nothing here: the hardware on this machine
   encodes 1080p far faster than 60 fps, so a fully serialized pipeline would hold the frame
   rate and pass. The lesson behind this gate came from a 120 fps target, and the arithmetic,
   not the frame rate, is what carries it to 60.*
7. Every stage in [05 §10](05-host.md) reports p50, p95 and p99, and the host-side stages sum
   to less than one frame interval at the negotiated rate. A pipeline that cannot clear a frame
   within a frame interval cannot hold the frame rate, so this is the floor rather than the
   target; a tighter budget is set once the first measurement exists.

**Open, found by running both backends over one source, 2026-08-17.** The vendor backend spends
**2.4 MB on its first picture where the open-stack one spends 513 bytes**, for identical input at
the same target rate, and another encoder driving the same interface on the same hardware and the
same frames spends 680 bytes. Both decode bit-exact, so this is cost rather than corruption: the
refresh is coded at quantiser 9 where the reference reaches the same result at 22. The rate
controller is not constraining that picture, and at 60 frames per second a 2.4 MB refresh is
roughly 2000 packets in one frame interval -- the exact burst shape that has already cost this
project a receive-buffer lesson, so it must be understood before Gate A rather than tuned around.

Ruled out by measurement, so they are not re-tried: the quantiser floor, which was genuinely
unapplied and is now applied and moved the figure by six percent; the rate-control buffer
override, which changes nothing when removed; and the forced-refresh flag, which changes nothing
when withheld. **Raising the floor would shrink the picture and hide the question**, which is why
it has not been done.

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

## Phase 6 - HEVC

- [ ] HEVC encode path and the two-place capability signalling
  ([01 §11.3](01-protocol.md)).

- [ ] **The refusal path for a seat that cannot decode the session's codec** (D11): read the
  capability from session initialization, and disconnect with a status before any video is sent.

**Gate:**

1. A stock client negotiates and decodes HEVC, and both codecs are selectable.
2. **A guest that cannot decode the session's codec is disconnected with a status, and no video
   is ever sent to it.** Observed against a stock client: the disconnect arrives, the client
   reports it, and the other guests' streams are undisturbed across the whole episode.

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

**Open before this gate can be written as a number.** Opcode 10 carries a status
([01 §11.1](01-protocol.md)) and **the values it takes are not yet known**. We must send one the
stock client already renders rather than inventing a value, so the enumeration has to be read
before the disconnect can name a reason. Until then the refusal is expressible but its status
is not chosen.

---

## Phase 7 - Input injection

- [ ] `lowlat-inject` over `uinput`: keyboard, mouse buttons, wheel, absolute and relative
  motion.
- [ ] Usage-code to kernel-code mapping as a pure, unit-tested function.
- [ ] Per-guest pressed-state tracking with release-all on disconnect.

**Gate:**

1. Keyboard and mouse round-trip from a stock client to a real input device.
2. Absolute coordinates land on the correct pixel, including on a rotated output.
3. **No stuck keys after an abrupt disconnect** with keys held. *Named regression test.*
4. Mapping tests run without a device; injection tests are labeled and excluded by default.

---

## Phase 8 - Public C ABI

- [ ] `lowlat-host` orchestration and the `extern "C"` surface from
  [06-api.md](06-api.md).
- [ ] Generated header, opaque handles, versioned structs, stable-numbered enums.
- [ ] `catch_unwind` at every entry point; unwinding enabled for the shared library.

**Gate:**

1. A C# application drives a full session end to end using its own signaling.
2. The generated header compiles standalone under C and C++ with warnings as errors.
3. **A deliberately panicking call returns a status code rather than unwinding.** *Named test;
   this is undefined behavior if it regresses.*
4. Every exported symbol carries the project prefix. *Checked mechanically against the
   symbol table.*

---

*Everything below requires bare metal.*

---

## Phase 9 - Capture and Gate B

- [ ] Capture backend selection and the display-stack decision from
  [07-platforms.md](07-platforms.md).
- [ ] Colour conversion by compute shader, writing planes directly, per-slot targets.
- [ ] Zero-copy import from the capture handle into the encoder on the same device.
- [ ] Cursor extraction, classification, and the visibility and relative-mode signals.

**Gate B:**

1. **Real desktop streaming to a stock client**, 10 minutes, no corruption.
2. No host-visible copy between capture and encode; a readback stage would appear in the log
   and does not.
3. Cursor shape changes and relative mode both behave correctly, including entering and
   leaving a window drag.
4. Capture survives resolution change and display hotplug.

---

## Phase 10 - Audio

- [ ] Capture from the system monitor source, encode, fan out on the audio channel.

**Gate:** audio downlink to a stock client with no drift over 30 minutes, and clean recovery
from a source change.

---

## Phase 11 - Multi-guest, software encode, and VAAPI

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
