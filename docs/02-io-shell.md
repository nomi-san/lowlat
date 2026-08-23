# 02 - IO shell

**Status:** locked 2026-08-15. Implemented by `lowlat-net`.

The shell is a **first-class, specified, tested component**, not glue. It owns sockets,
threads, timers, and wakeups, and it drives the sans-IO core from
[01-protocol.md](01-protocol.md). Every rule here is MUST-level and most of them exist because
the alternative shipped and failed.

The core decides *what* to send and *when* it next needs attention. The shell decides *how*
bytes reach the wire and *when* to wake up. Neither reaches into the other.

## §1 Threading model

**One merged network thread per connected guest.** That thread does the whole cycle: receive,
decrypt and feed the core, deliver to pipelines, drain the core's output, send. Decryption is
inline. At high rates the authenticated decryption cost is a single-digit percentage of one
core with hardware AES, which is far cheaper than the handoff it would otherwise need.

| Thread | Owns |
|---|---|
| network, one per guest | the shell loop for that guest's session |
| capture and encode, one global | acquire, convert, submit, collect, hand to per-guest packetizers |
| audio, one global | capture, encode, fan out |
| admission, one global | attempts, candidates, session setup and teardown |

Input injection rides the delivering network thread. Injection is cheap and fire and forget;
a dedicated thread would add a hop for no gain.

**No thread in the SDK raises its own priority.** No priority class, no scheduling policy
change, no CPU affinity. We are a library inside another process, and outranking that
process's own UI thread is a priority inversion that has produced hard hangs on low-core
machines while the reference implementation ran fine on the same hardware. The correct lever
is the *process* class, which lifts every thread together and preserves ordering, and that
decision belongs to the integrating application. `lowlatd` owns its process and may make it
for itself.

**Scale every thread-count knob by available parallelism.** A hardcoded worker count
oversubscribes a two-core machine invisibly on a development box.

A split receive and decrypt design is the documented fallback if profiling ever shows
decryption starving the receive path at the highest frame rates. It is not built until
measured.

## §2 Timing discipline

Three rules. Each is a production scar.

1. **Never sleep sub-millisecond as loop cadence.** A sleep of 200 us or less degrades into a
   busy spin on every platform we target. Measured cost was 14.7 percent idle CPU against a
   1.4 percent reference. Waits are event driven per §3, with millisecond-scale timeout caps.
2. **Explicit sleeps use an absolute deadline.** On POSIX, `clock_nanosleep` with
   `CLOCK_MONOTONIC` and `TIMER_ABSTIME`, with the deadline built from `clock_gettime`. Never
   assume the language runtime's monotonic instant shares a base with `CLOCK_MONOTONIC`. On
   Windows, a high-resolution waitable timer with a bounded spin finish.
3. **On Windows, raise the timer resolution at SDK init and drop it at shutdown**, refcounted.
   It is per-process, so another application making the same request does not help us, and
   high-resolution waitable timers do not remove the need: completion-port timeouts, socket
   polls, and object waits all still quantize to the system tick. Missing this made a host
   tick at roughly 15.6 ms.

**Clock semantics.** The shell's clock exposes **fractional milliseconds as a float**. The
congestion controller ([01 §10](01-protocol.md)) measures throughput over the interval between
ticks, and quantizing that interval to whole milliseconds silently skips the peak update
whenever it rounds to zero. Monotonic only; a wall clock never appears in this crate.

## §3 Wakeups

A header-level wait and notify pair over the platform futex: `FUTEX_WAIT` and `FUTEX_WAKE` on
Linux, `WaitOnAddress` and `WakeByAddress` on Windows, `__ulock` on macOS.

- **Producers MUST use our notify, never the standard library's.** A standard-library atomic
  notify keeps its own waiter registry and skips the kernel wake when it sees no registered
  waiter, so a raw futex sleeper is never woken and escapes only on timeout. This turned a
  notify-driven pipeline into a timeout-polled one and delivered frames in 104 ms bursts. The
  wait and notify helpers live together in `lowlat-common` for exactly this reason: they are
  one primitive, not two.
- **Sub-millisecond timeouts round up to 1 ms**, so a wait can never degenerate into a hot
  poll.
- **Consumers recheck their predicate in a loop.** Spurious wakes are permitted.
- Standard pattern: producer pushes, bumps a sequence, notifies. Consumer loads the sequence,
  tries to pop, then waits on the sequence with a capped timeout.
- **Every ring and every atomic handoff in this crate is model checked under `loom`.**

## §4 The event loop

```
loop:
    timeout = clamp(endpoint.next_timer_ms(now), 1, 50)
    wait:    poll(fd, timeout) -> which descriptors spoke
    receive: if the socket spoke: batch drain -> endpoint.process_input(...)
    deliver: drain complete messages -> pipeline rings (+ notify)
    if app_send_seq changed:
        pull input and data rings -> send_message(...)        // input FIRST
    endpoint.poll(now)
    while e = endpoint.get_output(now, buf):
        apply e.ttl, send buf[..e.len] to e.to, restore the TTL
```

- **There is no tick.** The timeout comes from the core. A next-timer function that exists but
  is never consumed leaves a fixed over-poll in place, which is a real bug that shipped.
- **The upper clamp is a safety net and must sit well above every real deadline, or it becomes
  the cadence it was meant to prevent.** The session's own timer is bounded by the 30 ms
  acknowledgement cadence, so a 5 ms cap would bind on *every* wake and reinstate exactly the
  over-poll the rule above forbids. 50 ms never binds in normal operation and still catches a
  core returning nonsense.
- **One object, two state machines.** The shell drives a single endpoint, which owns both the
  connectivity engine and the session, classifies each datagram, and reports the sooner of the
  two deadlines. Classification and timer merging are protocol decisions and live in the core,
  where they are exercised with injected time; a shell that arms from the session alone misses
  every connectivity deadline, and one that arms from connectivity alone polls forever once the
  attempt is over.
- **An output carries its destination and how to send it.** A mapping probe leaves at a reduced
  TTL, and the socket must be restored immediately afterwards or the media path silently caps
  at a few hops ([03 §4](03-connectivity.md)). The obligation is in the type rather than in a
  comment, and the shell honours it per datagram.
- **A descriptor the wait said nothing about is not touched.** The wait already
  reports per descriptor, and a pass that asks both regardless spends an
  application-wake read and a receive call to be told what it has just been
  told. **The test is anything reported, not readability alone**: an error or
  hangup condition is cleared by the call that collects it, so a pass that saw
  one and skipped that call would wake again immediately on the same
  uncollected condition, which is a spin in place of a saved syscall. **The
  application ring is pulled either way**, because a producer can fill a ring
  and have its wake land after the wait returned.
- **Application sends wake the loop.** Enqueuing to an application-facing ring bumps a
  sequence and posts a dedicated wake: an `eventfd` on Linux, a completion post on Windows, a
  user event on macOS. Enqueue to wire is then microseconds rather than "next poll". Without
  it, input on an idle stream waits out the timer.
- **Input is pulled before receive processing on the send side.** Input latency is the one
  budget with a human in the loop.

## §5 Sockets

Set **once at open**. **Nothing may downgrade a socket option after open.** A connectivity
setup path that shrank a 64 MB receive buffer to 5 MB left it that way for the entire stream.

| Option | Value | Why |
|---|---|---|
| `SO_RCVBUF` | request 64 MB, log what was granted | keyframe bursts of roughly 2550 packets per 100 ms overflow 16 MB |
| `SO_SNDBUF` | 4 to 5 MB | the default drops connectivity-check and video bursts |
| `IPV6_V6ONLY` | 0, dual stack on one socket | one socket serves both families |
| `IP_PKTINFO`, `IPV6_PKTINFO` | on | source address selection parity |
| `IP_TOS`, `IPV6_TCLASS` | EF (`0xB8`) | |
| `IP_MTU_DISCOVER`, `IPV6_MTU_DISCOVER` | `IP_PMTUDISC_DO`, `IPV6_PMTUDISC_DO` | refuse to fragment, so an oversized probe fails fast instead of being split and arriving anyway ([01 §8](01-protocol.md)). **Both families: neither setting carries to the other**, and a socket left at the v6 default fragments locally, which a probe reads as the size having worked -- on a path whose minimum is 1280 and a ladder that climbs past it |
| non-blocking | on | all paths |
| `SIO_UDP_CONNRESET` | off, Windows only | otherwise an ICMP unreachable wedges every subsequent receive |

**Receive buffer sizing is derived from the protocol's absolute ceiling, never from the
current path MTU:**

```
recv_slot = 2000 (absolute datagram ceiling) + 64 (relay framing margin)
```

Sizing from the negotiated or probed size silently discards whole datagrams and presents as
"control works, video does not". The probed size and the receive slot size are **different
named constants** and must never be spelled with the same identifier.

**Address family is determined structurally, never by scanning for a colon.** A v4-mapped
address contains colons and is not IPv6; classifying it as such kills v4 connectivity.

**The shell owns the socket for the whole session and opens it before connectivity begins.**
The connectivity engine is sans-IO and cannot open anything, so an earlier description of it
handing a descriptor over does not survive contact with the boundary. What does survive is the
rule that mattered: options are set once at open and nothing lowers one afterwards.

## §6 Per-platform receive and send

| Platform | Receive |
|---|---|
| Linux | `poll` plus `recvmmsg` in batches of 64 directly into slots, looping until drained |
| Windows | overlapped receives pre-posted into a pinned slot pool, draining up to 256 completions per call, with completion-on-success skipped |
| macOS | `kevent` with a user-event teardown filter, plus batched receive |

A single outstanding receive plus a poll loses keyframe bursts outright. On one platform this
was the difference between zero and complete delivery of a keyframe burst on loopback.

**Send uses segmentation offload where available**: `UDP_SEGMENT` on Linux, the equivalent on
Windows, falling back to per-datagram send. One syscall per batch. This matters more as the
datagram size rises, since the packet rate falls but the burst size does not.

## §7 Buffers and allocation

- **The shell allocates nothing on a data path.** Receive slots, rings, and scratch are
  allocated once at session setup.
- **Per-packet scratch is deliberately uninitialized.** Writers cover exactly the bytes they
  emit. A defensive zeroing pass over a 1229-byte buffer, ten thousand times a second, is pure
  waste.
- **Handoff is by slot index**, not by copying bytes, wherever the pool allows.
- Shell hot paths satisfy the same zero-allocation assertions as the core: the counting
  allocator in the test harness must report exactly zero.
- **This crate contains `unsafe`, and it is the first that does outside the concurrency
  primitives.** Batched receive, offload send, and the wake descriptor are all syscalls. Keep
  the unsafe in thin wrappers whose safety argument is local, and note that the `miri`
  obligation in [08 §7](08-testing.md) cannot reach them, because `miri` cannot execute a
  syscall. The sanitizer build carries that weight instead.

## §8 Timers

There is no timer thread and no timer wheel. Housekeeping runs inside `session.poll(now)`
whenever the loop wakes, whether that was a packet, an application send, or the armed timeout.
Retransmission scanning, acknowledgement emission, keepalives, and path probing are all
consequences of that call, not independent schedules.

## §9 Teardown

Teardown wakes **every** waiter before joining: set the error state, notify all, and push a
sentinel wake onto the loop. Dying sessions that skip this strand blocked readers until their
full timeout expires, which turns a clean disconnect into a multi-second hang.

Per-guest state is released on teardown: rings, injector pressed-key state, and any
per-connection resources. The capture and encode pipeline is unaffected; other guests keep
streaming.

**Any thread that performed cryptographic work must release the crypto library's per-thread
state before exiting.** Some libraries keep a per-thread error queue and random state that
leaks otherwise. Because the network thread is per connection, this leaks per connect cycle
and is invisible without a churn soak; it was found at roughly 11 KB per cycle only after
thousands of connections.

## §10 Diagnostics

- Log for diagnosis from logs alone. Lifecycle at info, recoverable at warning, session-fatal
  at error, hot-path detail at trace and compiled out in release.
- Every log line carries the identifiers needed to correlate across threads: guest, channel,
  sequence.
- Counters the shell owns and exposes: datagrams received and sent, bytes, batch sizes, wake
  reasons, poll timeouts hit versus packet wakes, granted socket buffer sizes, probe outcomes,
  and **datagrams the endpoint refused**.
- **A datagram dropped for being unparseable or unauthenticated is counted.** Dropping it is
  right -- hostile and corrupt input is ordinary on a network -- but a drop that leaves no
  trace makes a peer speaking the wire differently and a path carrying nothing into the same
  picture, and the counters that describe a channel never see it, because a rejected datagram
  reached no channel. That gap hid a real wire mismatch through several rounds of diagnosis.
- The granted `SO_RCVBUF` is logged at open, every time. A silently clamped request is
  otherwise invisible until a burst is lost.
