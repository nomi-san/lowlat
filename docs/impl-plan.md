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

**Gate:**

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

- [ ] Socket open with the complete option set, logging the granted receive buffer.
- [ ] `poll` plus batched receive, segmentation-offload send with per-datagram fallback.
- [ ] The merged per-guest thread and its event loop, armed from the core's next timer.
- [ ] Application send wake via `eventfd`.
- [ ] Teardown that wakes every waiter, plus per-thread crypto state release.

**Gate:**

1. Sustained loopback stream at the target packet rate for 60 minutes with no loss attributable
   to the shell.
2. Steady-state allocation count is zero.
3. Granted receive buffer size appears in the log at open.
4. Wake accounting: the ratio of timeout wakes to packet wakes matches the expected profile,
   proving the loop is event driven rather than polling.
5. Teardown under load joins every thread within 100 ms with no stranded waiter.
6. Ten thousand connect and teardown cycles show no per-cycle memory growth. *This is the
   per-thread crypto state regression, which is invisible without a churn soak.*

---

## Phase 4 - Signaling and admission

- [ ] `lowlat-kessel`: transport, authentication, reconnection with backoff, host
  advertisement, and the message set from [04-signaling.md](04-signaling.md).
- [ ] Host admission seam: register attempt, add candidate, approve returning credentials,
  end connection, plus the event queue.
- [ ] Advertised capacity read from the configured guest limit, never hardcoded.

**Gate:**

1. The host appears in the service's host listing.
2. A stock client's offer is accepted through the seam, candidates exchange, and connectivity
   completes.
3. The session reaches the streaming state and holds it with no media flowing.
4. Advertisement is emitted on state change only, never on a timer.
5. Rejection and cancellation paths produce the correct typed outcomes.

**Not in scope:** any video.

---

## Phase 5 - Encoder and Gate A

- [ ] `lowlat-encode` trait: asynchronous submit and poll, force keyframe, live bitrate
  reconfigure that never reinitializes.
- [ ] **VAAPI backend**, H.264, 8-bit 4:2:0, low-latency parameters. First, not later: it is the
  encoder on the primary Linux target and on the machine this is tested against
  ([07 §3.1](07-platforms.md)).
- [ ] NVENC backend, same trait, same parameters.
- [ ] `lowlat-capture` trait plus the synthetic frame source.
- [ ] Packetizer and the control opcodes streaming requires.
- [ ] Congestion controller ([01 §10](01-protocol.md)) driving encoder bitrate.

**Gate A:**

0. Both hardware backends encode the same synthetic source, so the trait is shaped by two
   implementations rather than one.
1. **A stock client connects and renders our synthetic frames**, 1080p60, for 10 minutes, with
   no corruption and no freeze beyond the loss budget.
2. Bitrate reconfigure is observed live with no keyframe and no reinitialization.
3. Encode submit and the next frame's preparation overlap; the pipeline is not serialized.
4. Reported end-to-end latency is within the stage budget.

---

## Phase 6 - HEVC

- [ ] HEVC encode path and the two-place capability signalling
  ([01 §11.3](01-protocol.md)).

**Gate:** a stock client negotiates and decodes HEVC; both codecs are selectable; a client
without HEVC falls back to H.264 without operator intervention.

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

- 2026-08-16: Phase 2 split. The relay moves to Phase 2b, scheduled after Gate A: nothing
  before Gate A depends on it, it has no test surface until Phase 2's fixtures exist, and its
  datagram clamp already landed in Phase 1. Gateway port mapping leaves the phase entirely.
  Gate 1 now states an expected outcome per topology instead of "green", because half the
  matrix is expected to fail. Gate 5 loses its frame-level wording, which needed an encoder
  that does not exist until Phase 5. Two gate items added: the two-machine end-to-end run, and
  the TTL restore regression.
- 2026-08-15: plan created.
