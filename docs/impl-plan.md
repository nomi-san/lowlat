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

- [ ] **Session corpus captured.** A recorded exchange between a stock host and a stock
  client, with the session key, sufficient to replay both directions offline. Phase 1's gate
  depends on it. Capturing it before Phase 1 rather than during is deliberate: it is the only
  ground truth that catches wire drift at the moment the code is written rather than weeks
  later.
- [ ] Test peer available: a stock client on a second machine or VM, reachable over the
  development network.

---

## Phase 0 - Workspace and common

Foundation. Nothing protocol-specific.

- [ ] Cargo workspace, edition 2024, all crates from [00-overview.md](00-overview.md) present
  as skeletons with the dependency direction enforced.
- [ ] `lowlat-common`: monotonic clock exposing **fractional milliseconds**, absolute-deadline
  sleep, the futex wait and notify pair as one primitive, bounded SPSC ring, byte order
  helpers, RFC 1982 sequence comparisons, logging.
- [ ] Counting global allocator behind a test-only feature, plus the assertion helper that
  hot-path tests use.
- [ ] `loom` configuration for the concurrency crate.
- [ ] CI: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`,
  `python scripts/check-ascii.py`, `cargo deny check`.
- [ ] Pre-commit hook running the ASCII check.

**Gate:**

1. CI green on Linux.
2. `loom` passes on the SPSC ring.
3. Clock: one million samples are strictly monotonic, and an interval shorter than one
   millisecond returns a nonzero fractional value. *This is the regression test for the
   quantization bug; an integer-millisecond clock fails it.*
4. **The zero-allocation harness is itself verified**: a deliberately allocating test fails
   the assertion. A harness that cannot fail proves nothing.

**Not in scope:** anything that parses a packet.

---

## Phase 1 - Protocol core

`lowlat-core`, sans-IO, `no_std`. The whole of [01-protocol.md](01-protocol.md) except
connectivity.

- [ ] Record envelope encode and decode, both crypto modes, nonce derivation.
- [ ] Cleartext data packets and the group acknowledgement, with the full flag validation
  matrix.
- [ ] Channels, per-channel rings, reassembly, base advance.
- [ ] Acknowledgement emission, negative acknowledgement, retransmission timeout, stall
  escape jumping to the furthest occupied slot.
- [ ] Send window bounded by the peer ring depth ([01 §7](01-protocol.md)).
- [ ] Path probe state machine ([01 §8](01-protocol.md)), including the compile-time assertion
  that no emitted datagram can exceed the absolute ceiling.
- [ ] Control message framing and the opcode table ([01 §11](01-protocol.md)).
- [ ] Fuzz targets: envelope, cleartext packet, group acknowledgement, control message.

**Gate:**

1. **Corpus replay is byte exact in both directions.** Feed the recorded bytes with a fake
   clock; every emitted packet matches the recording.
2. Property test: encode then decode is the identity for every message type, ten thousand
   cases.
3. Fuzz targets run 15 minutes each with no crash and no timeout.
4. `miri` clean over every module containing `unsafe`.
5. Zero-allocation assertions pass on the receive and send paths.
6. The crate compiles as `no_std` with no `alloc` on any data path.

**Not in scope:** sockets, threads, connectivity.

---

## Phase 2 - Connectivity and the simulator

- [ ] Connectivity checks on the shared socket, demultiplexed per [01 §2](01-protocol.md).
- [ ] Candidate gathering, server-reflexive discovery, punch state machine, all sans-IO.
- [ ] Relay client (RFC 5766) including allocation, permissions, channel binding, and consent.
- [ ] `lowlat-sim`: injected time, scripted loss, reordering, duplication, jitter, and a
  topology model.
- [ ] Network namespace fixtures with real kernel address translation.

**Gate:**

1. **Topology matrix green in the simulator and in namespaces:** full cone, restricted cone,
   port restricted, symmetric, carrier-grade, and hairpin.
2. Relay fallback succeeds when direct connectivity fails.
3. A v4-mapped peer address is classified as IPv4. *Named regression test.*
4. Relay framing overhead is accounted for in receive sizing; a full-size datagram survives
   the relay path. *Named regression test.*
5. Five percent uniform loss and five percent reordering: the stream recovers with bounded
   freeze and no reference-chain break, over ten thousand simulated frames.

**Not in scope:** real sockets outside the fixtures.

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
- [ ] NVENC backend, H.264, 8-bit 4:2:0, low-latency parameters.
- [ ] `lowlat-capture` trait plus the synthetic frame source.
- [ ] Packetizer and the control opcodes streaming requires.
- [ ] Congestion controller ([01 §10](01-protocol.md)) driving encoder bitrate.

**Gate A:**

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
- [ ] VAAPI backend.

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

- 2026-08-15: plan created.
