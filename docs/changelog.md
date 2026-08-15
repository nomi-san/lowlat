# Changelog

Newest first. One entry per phase; approach changes and gate revisions go in
[impl-plan.md](impl-plan.md) instead.

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
