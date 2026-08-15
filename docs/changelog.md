# Changelog

Newest first. One entry per phase; approach changes and gate revisions go in
[impl-plan.md](impl-plan.md) instead.

## 2: connectivity (in progress)

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
- `lowlat-sim`: address translation modelled as two independent behaviours,
  chained translators, hairpin, a seeded path with loss, duplication,
  reordering, jitter, and hop-limited delivery.
- The topology matrix, each case stating the outcome it expects.

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

**Still to land:** the namespace fixtures, the two-machine run, and the fuzz
targets.

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
