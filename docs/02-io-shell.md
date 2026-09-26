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
   tick at roughly 15.6 ms. Measured on the completion port: a wait asked for 1 ms lasts 16.0
   ms at the default resolution and 2.0 ms with it raised, and one asked for 10 ms lasts 10.5
   -- a timeout ends at the tick after it expires -- while a posted entry reaches its waiter in
   11 us at the median either way. Each library handle holds one request for its life; the system
   counts them, so the last handle released lowers it.

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
    timeout = clamp(endpoint.next_timer_ms(now), 1, 50)   // rounded up, never down
    wait:    poll(fd, timeout) -> which descriptors spoke
    now = clock()                                         // the pass runs on this
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
- **The clock is read twice per pass, and the pass runs on the second reading** (corrected
  2026-08-29; the loop previously reused its pre-wait reading). The first reading arms the
  wait and does nothing else. A pass stamped with its pre-wait clock sees the deadline it
  woke for as not yet due, emits nothing, and pays a second wake one clamped minimum later --
  every deadline costs two wakes and fires a pass late -- and a round trip whose
  acknowledgement arrived mid-wait is stamped before it arrived, reading short by up to a
  full wait. For the same reason the wait rounds a fractional timeout **up** to the poll's
  whole-millisecond granularity: truncation wakes the loop just before the deadline it armed.
  The shell owns this clock and hands the pass's reading back to its caller, which times
  everything else in its pass with it rather than reading a clock of its own.
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
- **The endpoint is generic over its media half, and so is the shell.** The loop above reads a
  fixed set of calls from the session -- feed a datagram, poll, the next deadline, drain
  output, queue a message, take one, liveness, pressure, the round-trip figures -- and that
  set is a trait in the core with the native session as the default type. The browser session
  ([01 §14](01-protocol.md)) is the second instantiation: the same loop, the same shell,
  monomorphised twice, with no dispatch on a data path. Two things the media half decides that
  the loop does not: it is told when the path is established, because the browser's record
  layer sends its first flight only from then and must not be built before; and it may report
  a **fault** -- a handshake that did not complete, an association the peer aborted -- which
  the native session never does and which the guest loop turns into an outcome of its own
  ([06 §5](06-api.md)).
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
| `SO_RCVBUF` | request 64 MB, log what was granted | keyframe bursts of roughly 2550 packets per 100 ms overflow 16 MB. Windows grants the whole request; a buffer at its default there held 55 of 100 datagrams of a small burst |
| `SO_SNDBUF` | 4 to 5 MB | the default drops connectivity-check and video bursts |
| `IPV6_V6ONLY` | 0, dual stack on one socket | one socket serves both families. **Set first on Windows**, where a fresh socket serves one family and refuses every v4-level option until this is cleared |
| `IP_PKTINFO`, `IPV6_PKTINFO` | on, and consumed | the arrival address of every datagram is read back and claimed on sends; see below |
| `IP_TOS`, `IPV6_TCLASS` | EF (`0xB8`) on Linux; not set on Windows | Windows accepts the first and sends zero, and refuses the second; the established path is marked there instead, see below |
| `IP_MTU_DISCOVER`, `IPV6_MTU_DISCOVER` | `IP_PMTUDISC_DO`, `IPV6_PMTUDISC_DO` | refuse to fragment, so an oversized probe fails fast instead of being split and arriving anyway ([01 §8](01-protocol.md)). **Both families: neither setting carries to the other**, and a socket left at the v6 default fragments locally, which a probe reads as the size having worked -- on a path whose minimum is 1280 and a ladder that climbs past it. The same pair on Windows, whose don't-fragment options are refused on a dual-stack socket |
| non-blocking | on | all paths |
| `SIO_UDP_CONNRESET`, `SIO_UDP_NETRESET` | off, Windows only | otherwise an ICMP unreachable, or a hop limit expiring on a mapping probe, fails the next receive; the probe provokes the second by design |

**On Windows the established path is marked per destination, not per socket.** The system
accepts a per-socket type of service and sends zero, and refuses the v6 traffic class, so once
a path is established the shell asks the platform to mark it -- the relay's server on a relay
attempt, the path itself otherwise -- and Windows adds an audio-video flow through the
system's QoS service: class selector 5 on the wire and, on a wireless link, the video access
category. Measured on a wireless client beside a 250 Mbit/s upload from the same machine,
marked datagrams crossed the air in 1.0 ms at the median and 20 ms at the 99th percentile,
against 25 and 181 unmarked; on an idle link the two were the same. The service marks an
unconnected socket only when it is bound to a specific address, and this one is bound to the
wildcard so it hears every address the host holds, so the flow is asked for with the socket
connected for that one call and disconnected again: the flow keeps marking every send to the
destination, whatever source a send claims, and the wildcard comes back with the disconnect
(both measured). For the moment between the two calls the system discards what arrives from
anywhere else, which the protocol recovers from as it does any loss -- once per path, never
per datagram. The service is loaded at run time; without it, sends go unmarked and the session
says so once. Linux marks the socket at open and has nothing to do here.

**Receive buffer sizing is derived from the protocol's absolute ceiling, never from the
current path MTU:**

```
recv_slot = 2000 (absolute datagram ceiling) + 64 (relay framing margin)
```

Sizing from the negotiated or probed size silently discards whole datagrams and presents as
"control works, video does not". The probed size and the receive slot size are **different
named constants** and must never be spelled with the same identifier.

**Packet information is consumed, not merely enabled** (corrected 2026-08-29; the options
were previously set and the control messages never read, which is the worst of the three
states -- the table claimed a parity the code did not deliver). Receive reports the address
each datagram arrived at beside the address it came from. A connectivity answer leaves from
exactly the address the check arrived at, and the address the winning answer arrived at is
latched with the path and claimed on every datagram for the life of the session
([03 s4](03-connectivity.md)). On a host with several addresses the kernel's own source
selection follows the routing table, which is free to answer from a sibling address the peer
never probed; a filtering translator then drops the answer, and the punch dies on exactly the
multi-address case host candidates exist for. Only the source address is claimed -- the
interface choice stays with the routing table.

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
| Windows | overlapped message receives pre-posted into 256 slots pinned for the socket's life, taking up to 256 completions per call, with completion-on-success skipped; the wake is an entry posted to the same port |
| macOS | `kevent` with a user-event teardown filter, plus batched receive |

A single outstanding receive plus a poll loses keyframe bursts outright. On one platform this
was the difference between zero and complete delivery of a keyframe burst on loopback.

**The platform owns the socket, the wake and the receive storage together**, as one module
chosen when the shell is built, under the same names on every platform; the loop above it,
the send batching and the attempt thread are written once. Together because the completion
port joins what the readiness wait keeps apart: there the wait *is* the receive, and pre-posted
storage has to live exactly as long as the socket it was posted on. The shell asks the
platform to wait, to take the wake, to drain what arrived and to say whether more may be
queued; whether that is a poll and a batched receive or a completion drain is the platform's
own business.

**On Windows the port carries the wake beside the receives**, so a wake can be taken off it
outside a wait -- by a drain collecting what completed since. It is kept and reported by the
next wait, which then does not block; dropped, the work it announced would sit out the
timeout. A post is not a counter, so the collapse an eventfd gives is an armed flag there: the
first notify after a take posts, the rest find it set. Teardown cancels every posted receive
and takes each back before the storage may go. **Registered I/O is not used**: a socket made
for it refuses the ordinary calls, so it would be a second module, sends included, beside the
plain one a system that refuses registered I/O needs anyway, and it is taken only if it
measures better than this: the plain port hands over a keyframe-sized burst of 2550 datagrams
already queued on the socket in 1.8 ms at the median (2.6 at the 99th percentile), 695 ns a
datagram, on the development machine.

**Send uses segmentation offload where available**: `UDP_SEGMENT` on Linux, the segment size
as a control message on the message send on Windows, falling back to per-datagram send. One
syscall per batch. This matters more as the datagram size rises, since the packet rate falls
but the burst size does not. **On Windows a claimed source must be an address the host holds,
and keeps the datagram on that address's interface**: one it does not hold is refused, and the
loopback holds 127.0.0.1 and not the rest of 127/8.

**Only a capability refusal disables offload for the run** (corrected 2026-08-29; previously
any refusal was permanent). A kernel or interface that cannot segment at all says so once and
the run takes per-datagram sends from there. A refusal about one batch -- a full send buffer
in the middle of a burst is the ordinary case -- falls back for that batch alone, because the
bursts that fill the buffer are exactly the ones offload exists for, and trading the fast path
away forever on a transient is backwards. A staged batch is also bounded by what one send may
carry (one maximal UDP payload), not by the staging buffer: the two differ by twenty-nine
bytes, and a bound at the buffer let an exact-fit batch reach the kernel only to be refused
whole.

## §7 Buffers and allocation

- **The shell allocates nothing on a data path.** Receive slots, rings, and scratch are
  allocated once at session setup.
- **Per-packet scratch is deliberately uninitialized.** Writers cover exactly the bytes they
  emit. A defensive zeroing pass over a 1229-byte buffer, ten thousand times a second, is pure
  waste.
- **Handoff is by slot index**, not by copying bytes, wherever the pool allows.
- Shell hot paths satisfy the same zero-allocation assertions as the core: the counting
  allocator in the test harness must report exactly zero.
- **The browser session is the one exemption, and it is measured rather than assumed.** Its
  record layer and its association are sans-IO crates that allocate per datagram and per
  message, and rewriting either to a fixed-capacity design is not a cost this project pays for
  the second pipe. The exemption is bounded by the benchmark that stands in for the assertion
  ([08 §8](08-testing.md)): 23 allocations per received datagram and 20 per sent one, 14 us at
  p50 and 24 us at p99 per datagram on the development machine (2026-09-12). A change that
  moves those figures is reviewed as an allocation regression, and the native path's counting
  assertion still reads zero on the same build.
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
