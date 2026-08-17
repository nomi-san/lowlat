# 00 - Overview

**Status:** design locked, interview 2026-08-15.

lowlat is an ultra-low-latency remote desktop host that speaks the Parsec protocol. Unmodified
Parsec clients connect to it on every platform they already run on. The host targets Linux
first, with Windows following.

It is the third system this team has built on this problem. The first two established the
protocol ground truth and the architecture; this one merges them and moves the host to Linux
and to Rust.

## Goals

1. **Ultra low latency, before everything else.** Every design choice yields to end-to-end
   latency. Capture to present is budgeted per stage and measured, not estimated.
2. **Never display corruption.** Loss shows as a bounded micro-freeze of roughly one round
   trip, never as gray, torn, or smeared frames. A reference chain is never broken silently.
3. **Compatibility with unmodified clients.** No forked client, no plugin, no patched binary
   on the other end. If a stock client cannot connect and stream, the feature is not done.
4. **Linux as a first-class host.** Not a port of a Windows product. Unattended operation,
   headless operation, and running as a system service are design inputs, not afterthoughts.

## Non-goals

- **Protocol redesign.** The wire is fixed by compatibility. Where the protocol constrains a
  choice, the protocol wins.
- **ABI compatibility.** The API shape is familiar and the wire is compatible, but struct
  layouts are ours and binary drop-in is not offered. See D6.
- **FEC.** Recovery is retransmit based. Forward error correction cannot cover the burst loss
  patterns that actually occur here, and it costs bandwidth on every frame to insure against
  a minority of them.
- **Client-feedback congestion control.** See D8. The wire has no such message and the
  feedback loop is an anti-pattern independently.
- **Windows host at v1.** Planned, not first. The capture and inject layers are the only
  platform-specific stages.

## System shape

```
+------------------- host (lowlatd) --------------------+       +-------- client --------+
|  capture -> CSC -> encode -> packetize (per guest)    |       |  stock Parsec client,  |
|  audio   -> Opus -> fan out                           |  UDP  |  any platform,         |
|  inject  <- CH0                                       |<----->|  unmodified            |
|                    IO shell (poll + recvmmsg, GSO)    |       |                        |
+---------------------------+---------------------------+       +------------------------+
                            |
                     NAT engine (punch, TURN)
                            |
                   signaling (application owned)
```

## Crate map

```
lowlat-common    clock, futex wait, SPSC rings, byteorder, RFC 1982 seq, log,
                 runtime library loading
lowlat-core      no_std sans-IO: wire, channels, rings, crypto, recovery, NAT, ICE, STUN, TURN
lowlat-crypto    credentials, key material, and the only source of randomness
lowlat-net       IO shell: sockets, threads, timers, wakeups
lowlat-sim       deterministic simulator and network namespace fixtures (dev-dependency)
lowlat-capture   frame trait plus synthetic source; real backends at Gate B
lowlat-encode    NVENC, then FFmpeg software, then VAAPI
lowlat-inject    uinput
lowlat-host      orchestration plus the C ABI cdylib
lowlat-kessel    signaling client; async permitted; the SDK does not link it
lowlatd          system service
lowlat-tray      user session client over a Unix socket
```

Dependency direction is strictly downward. `core` depends only on `common`. Nothing depends
on `kessel`.

## Decision register

| # | Decision |
|---|---|
| D1 | **Compatibility target is the protocol family, not a pinned application version.** Framing and control opcodes are stable and additive; the `versions` block in signaling negotiates subprotocol paths, so old and new peers interoperate in both directions. The host implements the current opcode generation and advertises only what it has actually implemented on each of the six axes. |
| D2 | **Crypto mode is credential driven.** When the offer carries an `aes256` credential the session uses AES-256-GCM; when it does not, the session uses the legacy AES-128-GCM path with the fingerprint. The host implements both and the credential decides. This is the only version-variant layer. |
| D3 | **The SDK owns all threads; signaling is application owned.** `host_start` spawns capture, encode, audio, network, and inject. The application drives admission through a four-call seam (new attempt, add candidate, begin p2p returning host credentials, end connection) plus a poll-based event queue. No async runtime, no TLS, no JSON, and no HTTP inside the FFI boundary, because a shared library loaded into another process must not start a reactor. |
| D4 | **The sans-IO boundary covers the protocol core and the NAT engine.** Wire, channels, rings, ACK and NACK, PMTU, crypto, and the ICE, STUN, and TURN state machines all live behind it. The core reads no clock, owns no socket, spawns no thread, and needs no RNG, because nonces are derived and keys arrive from signaling. `no_std` makes each of these a compile error rather than a review finding. |
| D5 | **Three test tiers.** A deterministic simulator with injected time and scripted loss, reorder, duplication, and jitter is the primary surface. Network namespace fixtures with real sockets and real kernel NAT are the integration tier, synthesizing cone, restricted, port-restricted, symmetric, CGNAT, and hairpin topologies. A real wide-area path is the final gate. The simulator is not optional: a developer network cannot produce the topologies the NAT engine must handle. |
| D6 | **API shape is familiar; layout and ABI are ours.** Same call surface and sequence as the established host SDK, so porting an integration is mechanical. Structs are `repr(C)`, versioned, and free of inherited warts. Exported symbols carry the `lowlat_` prefix so a layout mismatch is a link error rather than silent memory corruption. |
| D7 | **v1 codecs are H.264 and HEVC, 8-bit 4:2:0.** Encoder backends are **VAAPI and NVENC together at v1**, plus FFmpeg software loaded dynamically; amended 2026-08-15 after the platform probe measured working hardware encode on the open stack, which is the primary Linux target and the machine the work is tested on. 4:4:4 stays deferred. **10-bit is no longer purely deferred**: the compositor scans out a 10-bit framebuffer, so the capture and conversion path must accept it even while the encoder emits 8-bit. The wire flag bits are reserved either way. |
| D8 | **Congestion control is host local.** It is computed from local transport state, window occupancy and stale counts, and actuates the encoder bitrate through a live reconfigure that never reinitializes and never forces a keyframe. There is no congestion feedback message in either direction. Client influence is limited to one-shot preferences, which the host is free to overrule. |
| D9 | **Capture and audio are Gate B, on bare metal.** Everything before Gate A runs against a synthetic frame source in a virtual machine, which still exercises the wire, both crypto modes, the NAT ladder, signaling, the encoder, and input. Capture is a trait from day one with a synthetic implementation; real backends land when the hardware does. |
| D10 | **The core is multi-guest from day one; v1 policy is single-guest simple.** Per-guest sequence spaces, rings, and crypto state are in the data model immediately, because retrofitting them is a rewrite. Pressure gating, the skip-until-keyframe cascade, and consensus actuators land at Gate B. `max_guests` defaults to 4 with a compile-time cap of 16, and advertised capacity is read from that field rather than hardcoded. |
| D11 | **One encode serves every seat, so a session has one video configuration and it is never adapted to a later seat.** Codec, colour and resolution are settled once and every seat receives that stream; there is no per-seat fallback because there is no per-seat encode. A seat that cannot decode the session's codec is **disconnected with a status rather than accommodated**, and never by silence. Downgrading instead would let one arriving seat degrade every stream already running -- unlike the bitrate minimum, which physics forces, that would be a choice made on the existing seats' behalf. |

## Lessons registry

Every entry is a real production failure from an earlier implementation of this system. Each
one maps to a MUST-level rule in the linked document. A design that re-opens an entry is wrong
by definition. This table is the reason the project has the shape it does.

### Timing and scheduling

| Lesson | Rule lives in |
|---|---|
| The default timer quantum starves millisecond-scale loops and waits. The resolution request is mandatory and is per-process, so another application making it does not help us | [02](02-io-shell.md) |
| A sleep of 200 us or less degrades into a busy spin. Sub-millisecond polling as loop cadence cost 14.7 percent idle CPU against a 1.4 percent reference | [02](02-io-shell.md) |
| A next-timer function that is implemented but never consumed leaves a fixed over-poll in place | [02](02-io-shell.md) |
| Elevating worker thread priority inside a library is a priority inversion within the host process. Four elevated threads starved a UI message pump on a two-core machine and the window was marked not responding. Raise the process class instead, and let the application decide | [02](02-io-shell.md) |
| Hardcoded worker counts oversubscribe small machines. Scale every thread-count knob by available parallelism | [02](02-io-shell.md) |
| Rate control math depends on fractional-millisecond intervals. Quantizing them to integer milliseconds silently skips updates whenever the interval rounds to zero | [02](02-io-shell.md) |

### Wakeups and concurrency

| Lesson | Rule lives in |
|---|---|
| A standard-library atomic notify keeps its own waiter registry and skips the kernel wake for a raw futex sleeper. Every waiter ran to full timeout and frames arrived in 104 ms bursts. Pair raw waits with raw wakes, always | [02](02-io-shell.md) |
| Teardown that does not wake every waiter strands blocked readers until their full deadline | [02](02-io-shell.md) |
| Blocking a publish into a frame ring stalls the transport drain and cascades into corruption. Drop oldest, never block | [05](05-host.md) |

### Sockets and buffers

| Lesson | Rule lives in |
|---|---|
| A 2 MB receive buffer drops keyframe bursts. Request 64 MB, log what was granted, and never downgrade a socket option after open. One initialization path shrank the buffer to 5 MB and it stayed that way through the whole stream | [02](02-io-shell.md) |
| A receive scratch buffer sized from the negotiated MTU silently discards whole datagrams. It presents as "control works, video does not". Size every receive buffer from the maximum payload plus envelope plus relay margin | [01](01-protocol.md), [02](02-io-shell.md) |
| Wire MTU and internal slot size are different constants. Conflating them truncates packets and every keyframe fails to decrypt | [01](01-protocol.md) |
| One outstanding receive plus a poll loses keyframe bursts entirely. Batch receives | [02](02-io-shell.md) |
| On Windows, an ICMP unreachable wedges subsequent receives unless the connection-reset behavior is disabled | [02](02-io-shell.md) |
| A v4-mapped address contains a colon. Classifying address family by searching for a colon kills v4 punch | [03](03-connectivity.md) |

### Protocol and recovery

| Lesson | Rule lives in |
|---|---|
| Naive 32-bit sequence comparison inverts at wrap, which arrives in about 15 days at 2K120. Use RFC 1982 comparisons everywhere, and put an epoch in the nonce so a wrap cannot reuse one | [01](01-protocol.md) |
| Transport acknowledgements stored in the reliable ring deadlock it. Acknowledgements are fire and forget | [01](01-protocol.md) |
| A shared receive ring with per-channel sequence numbers collides. Rings are fully per channel | [01](01-protocol.md) |
| Catching up to the nearest occupied slot crawls the flow-control window and cost a 20x regression. Jump to the furthest | [01](01-protocol.md) |
| An anti-replay window applied to a reliable channel rejects legitimate retransmits | [01](01-protocol.md) |
| Gray frames are decoder reference-buffer corruption from missing references. Never feed a post-gap dependent frame. No amount of forward error correction covers burst loss here | [01](01-protocol.md) |
| A relay framing layer adds bytes the receive path must account for. Sizing for the inner packet silently dropped every full-size video packet, independent of network | [03](03-connectivity.md) |

### Host pipeline

| Lesson | Rule lives in |
|---|---|
| A serialized acquire, convert, encode loop caps at 70 to 80 fps against a 120 fps target. Encode must overlap the next acquire | [05](05-host.md) |
| Letting the encoder convert BGRA internally cost about 3 ms per frame. Feed it the native format from a compute shader | [05](05-host.md) |
| Copying an NV12 plane with a subresource copy drops chroma on two vendors' drivers. Write planes from a shader through plane-slice views | [05](05-host.md) |
| In-flight frames must not share a conversion target. Use a per-slot ring | [05](05-host.md) |
| Dropping a single dependent frame for one guest breaks the reference chain silently. Skipping must cascade until the next keyframe, per guest | [05](05-host.md) |
| A client-feedback-driven bitrate loop oscillated between 10 and 19 Mbps and cascaded gray frames. Infer host side, move slowly, reconfigure live | [05](05-host.md) |
| Detecting relative pointer mode from cursor clip geometry misses the entire hide-without-clip class. Use the cursor visibility signal | [05](05-host.md) |
| Adapter LUIDs change across reboots and driver reloads. Discover at runtime, never persist | [07](07-platforms.md) |

### Connectivity

| Lesson | Rule lives in |
|---|---|
| A port mapping created per attempt leaks. Use a persistent, connection-lifetime runner on a stable port | [03](03-connectivity.md) |
| A relay client that does not answer relayed consent checks gets media withheld | [03](03-connectivity.md) |
| A host firewall drops symmetric-NAT replies as unsolicited. Document the inbound rule | [03](03-connectivity.md) |
| Per-thread crypto library state leaks for every thread that runs crypto unless it is released at thread exit. Invisible without a churn soak; roughly 11 KB per connect cycle | [02](02-io-shell.md) |

## Document map

| Document | Contents |
|---|---|
| [01-protocol.md](01-protocol.md) | wire format, crypto modes, channels, rings, acknowledgement and recovery, PMTU, opcode catalog |
| [02-io-shell.md](02-io-shell.md) | threads, timing, wakeups, socket hygiene, per-platform receive |
| [03-connectivity.md](03-connectivity.md) | NAT engine, ICE, STUN, TURN, the connection ladder |
| [04-signaling.md](04-signaling.md) | signaling protocol, host advertisement, the application-owned seam |
| [05-host.md](05-host.md) | capture, conversion, encode, congestion, multi-guest delivery, input, cursor, audio |
| [06-api.md](06-api.md) | the C ABI, events, configuration, integration paths |
| [07-platforms.md](07-platforms.md) | Linux display stacks, privileges, service topology, Windows notes |
| [08-testing.md](08-testing.md) | test tiers, gates, fuzzing, simulation, benchmarks |
| [impl-plan.md](impl-plan.md) | phases 0 to 12 with verification gates |
| [changelog.md](changelog.md) | working log, newest first |
