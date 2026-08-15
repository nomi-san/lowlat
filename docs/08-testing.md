# 08 - Testing

**Status:** locked 2026-08-15.

The premise of this document is that **the development network can produce exactly one
connectivity topology, one machine's timing, and one vendor's driver**, while the software must
handle six topologies, adversarial timing, and hardware nobody here owns. Testing that only
runs against reality tests one point of a large space.

So the primary surface is deterministic and synthetic, reality is the final gate rather than
the first, and the two are not confused.

## §1 Tiers

| Tier | Runs | Covers | Speed |
|---|---|---|---|
| unit and property | every commit | pure functions, wire codecs, mappings | milliseconds |
| simulation | every commit | protocol and connectivity state machines under adversarial conditions | seconds |
| namespace fixtures | every commit | real sockets, real kernel address translation | seconds |
| model checking | every commit | concurrency primitives | seconds |
| fuzzing | continuous | every byte parsed from the network | continuous |
| hardware | manual and nightly | capture, encode, inject | minutes |
| soak | nightly | leaks, drift, exhaustion | hours |
| live peer | phase gates | the actual product working | manual |

Only the last two tiers require anything we cannot run in a virtual machine, which is what
makes phases 0 to 8 orderable the way [impl-plan.md](impl-plan.md) orders them.

## §2 Determinism

Determinism is not a testing preference; it is what makes the first four tiers possible at all.

- **The protocol core reads no clock, owns no socket, spawns no thread, and needs no random
  number generator** ([00-overview.md](00-overview.md) D4). Time is a parameter and nonces are
  derived.
- Therefore a test can drive a full session with injected time, replay a byte stream exactly,
  and get the same output every run.
- **`no_std` enforces this mechanically.** A test that needs to assert the absence of clock
  reads is a test that can be forgotten; a crate that cannot link `std::time` cannot regress.
- **A failing simulation is reproducible from its seed alone.** A failure that cannot be
  replayed from a seed is a bug in the harness, not a flaky test.

## §3 Unit and property tests

- Wire codecs get **round-trip property tests**: encode then decode is the identity for every
  message type, across the full range of every field.
- **Sequence arithmetic is tested across the wrap boundary**, explicitly, including the pair of
  values that a naive comparison inverts.
- **Input mapping is a pure function** and is tested exhaustively without any device present.
  This is why expansion is separated from injection in [05 §7](05-host.md).
- Validation is tested from the rejection side: every malformed input the wire permits is
  constructed deliberately and must be rejected without side effects.

## §4 The simulator

`lowlat-sim` drives the core with injected time and a scriptable network.

Controls: loss (uniform and bursty), reordering, duplication, jitter, bandwidth, and one-way
delay, each independently per direction.

What it is for:

- **Recovery behavior.** Retransmission timing, negative acknowledgement, the stall escape
  jumping to the furthest occupied slot, and keyframe resynchronization, all as reproducible
  tests rather than as a soak run that might reproduce the condition.
- **Connectivity topologies.** The six address-translation behaviors, as data.
- **Long-horizon conditions.** Sequence wrap arrives after roughly fifteen days of real
  streaming and in under a second of simulated time.
- **Adversarial conditions that a real network will not produce on demand**, such as a
  reordering window wide enough to expose an anti-replay bug on a reliable channel.

What it is not for: performance. The simulator has no realistic timing and any latency number
from it is meaningless.

## §5 Namespace fixtures

Network namespaces with kernel address translation give real sockets and real kernel behavior
under topologies the developer's network cannot produce.

Fixtures: full cone, restricted cone, port restricted, symmetric, carrier-grade double
translation, and hairpin.

- Each fixture is a script that builds the topology, runs the case, and tears it down, leaving
  no state behind.

**Two properties of the fixtures are load bearing, and both were found by a fixture reporting
the wrong answer confidently.**

**A default masquerade rule is not a cone translator.** It reallocates the source port per
destination, which is address-and-port-dependent mapping, so the stock configuration is a
symmetric translator. A fixture built on it looks like a port-restricted cone, fails to punch,
and confirms the exact opposite of what it was written to check. The external port must be
pinned for every cone topology, and only the symmetric case may be left at the default.

**The path between the endpoints must be longer than a mapping probe can travel.** A probe is
emitted at a reduced TTL precisely so it opens the local mapping without the peer's translator
seeing it. On a short path it crosses the whole fabric, arrives at the far translator before
that side has sent anything, and creates an entry in the inbound direction; the far side's own
outbound then matches that entry as a reply, so no inward path is ever established and both
sides time out. The same length makes the fixtures the real form of the TTL regression, since
media crosses more hops than a probe can and a socket left at the probe value carries nothing.
- They require elevated privilege to create, which is the one place the test suite needs it.
  The suite skips them with a clear message rather than failing when it is unavailable.
- **This tier exists because the development network produces one topology, not six.** The two
  machines available to it sit behind different consumer routers, so a live run between them
  exercises exactly one pair of translation behaviours -- whichever those two routers happen to
  implement -- and says nothing about the other five. Nor can it be made to produce them on
  demand, because the behaviour belongs to the routers rather than to us. A live run is
  therefore the strongest possible evidence for one point of the space and no evidence at all
  for the rest, which is precisely the division of labour between this tier and the simulator.

## §6 Fuzzing

Every byte that arrives from the network is parsed by a fuzz target:

- record envelope, before and after decryption
- cleartext packets and the group acknowledgement
- control messages and their bodies
- connectivity check messages
- relay framing
- cursor image decoding
- signaling payloads, at the application layer

Rules:

- **Crash reproducers are committed**, as named regression tests. A crash found once is
  covered forever. This is the part of the policy that matters.
- **Coverage corpora are committed only after minimizing.** Raw output keeps every input that
  reached a new branch, including redundant and needlessly long ones; minimizing replaces each
  with the shortest input reaching the same branches. On this project that was a 95 percent
  size reduction at identical coverage, and the gap widens with every run. A corpus committed
  raw only ever grows, and binary blobs never leave git history.
- A coverage corpus is a **speed optimization for bounded runs**, not a correctness artifact.
  It earns its keep on stateful targets, where reaching a path takes many mutations; on a
  small stateless parser the fuzzer rediscovers full coverage in milliseconds and the corpus
  is close to dead weight.
- Targets run in continuous integration for a bounded time per commit, and unbounded nightly.
- **A crash is a release blocker.** These parse hostile input from the network by definition.

## §7 Model checking and unsafe

- **Every concurrency primitive is model checked** under `loom`: every ring, every atomic
  handoff, every wait and notify pairing. The predecessors validated these by soak testing,
  which finds the failures that happen often and misses the ones that matter.
- **Every module containing `unsafe` runs under `miri`.** In practice that is the wire codecs'
  uninitialized scratch buffers, the frame handle types, and the C ABI layer.
- Unsafe blocks carry a safety comment stating the invariant that makes them sound
  ([AGENTS.md](../AGENTS.md) §7). A block without one fails review, not the test suite.

## §8 Allocation assertions

Hot-path tests run under a counting global allocator and assert the count is exactly zero.

**The harness is itself verified**: a deliberately allocating test must fail the assertion.
This is a real gate at [Phase 0](impl-plan.md), because a harness that cannot fail proves
nothing and will silently stop covering anything the day someone changes it. The same
principle applies to every check in this document, and it has already caught one live example
in this repository: a formatting check that reported success while examining zero files.

Covered paths: receive, decrypt, dispatch, reassemble, packetize, encrypt, send, and the input
path end to end.

## §9 Benchmarks and latency

- **Performance claims cite measurements**, as p50, p95, and p99. **Never an average.** The
  distribution is the product; a mean latency figure hides exactly the stalls users notice.
- Per-stage instrumentation is built in from the start ([05 §10](05-host.md)), not added when
  something feels slow.
- Microbenchmarks cover the per-packet paths: authenticated encryption, wire encode and decode,
  ring insert and drain.
- **Regressions gate merges.** A benchmark that moves adversely without an explanation in the
  commit is a failure, not a note.
- The synthetic frame source is the input for pipeline benchmarks, because a real desktop is
  not reproducible and therefore not comparable across runs.

## §10 Hardware and live tests

- **Tests requiring hardware are labeled and excluded by default.**
- **The default suite never moves the developer's pointer or presses a key.** Injection tests
  are opt-in, and the mapping layer they would otherwise cover is tested purely instead.
- Capture and encode tests skip cleanly with a stated reason when the device is absent, rather
  than failing. A skip that reads as a failure trains people to ignore failures.
- **Continuous integration has no GPU**, so it runs the software encoder path end to end. That
  is the second reason the software backend exists ([05 §4](05-host.md)).

## §11 Soak

Nightly, because these find what short runs cannot:

| Soak | Finds |
|---|---|
| ten thousand connect and teardown cycles | per-connection leaks, including per-thread library state that leaks a few kilobytes per cycle and is invisible below thousands of iterations |
| multi-hour continuous stream | drift, sequence growth, ring wrap, encoder state degradation |
| repeated loss bursts | recovery paths that work once and leak state each time |
| repeated display mode changes | capture reinitialization paths |

Each records memory, handle counts, and thread counts over time. **Flat is the pass
condition.** A slow upward slope is a failure even when nothing crashes.

## §12 Continuous integration

Every commit: formatting, lints as errors, the ASCII check, dependency and license audit, unit
and property tests, simulation, namespace fixtures where privilege allows, model checking,
sanitizers on the Linux debug build, bounded fuzzing, and benchmarks with regression
comparison.

Nightly: unbounded fuzzing, the soak matrix, and the hardware suite on a machine with a GPU.

**A red build is not merged.** There is no category of test in this repository that is
advisory.

## §13 Regression tests from the registry

**Every entry in the lessons registry has a named test**, and the name references the
behavior, not the incident.

The registry in [00-overview.md](00-overview.md) is a list of failures that already happened.
A design that re-opens one is wrong by definition, and a test is how that is enforced rather
than remembered. Where a lesson cannot be expressed as a test, the rule lives in
[AGENTS.md](../AGENTS.md) and the reason it is not testable is written down.

Tests land in the phase that would otherwise re-open the lesson, not in a testing pass at the
end. By then the code that needed the constraint is already written.

## §14 What counts as a gate

A phase gate is **a command that passes or a peer that streams**. If it can only be stated as
a judgment, it is not a gate and the phase does not have one yet.

Three gate shapes appear in [impl-plan.md](impl-plan.md):

1. **Mechanical.** A test suite, a benchmark threshold, a symbol table check, a link-graph
   check.
2. **Byte exact.** Replay of a recorded session produces identical output. This is the only
   ground truth for wire compatibility, and it is why the corpus is a prerequisite rather than
   a task.
3. **Observable behavior against a peer we do not control.** A stock client renders our
   frames. No amount of tier one and two substitutes for this, and it is why Gate A sits where
   it does rather than at the end.
