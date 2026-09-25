# 01 - Protocol

**Status:** locked 2026-08-15. Implemented by `lowlat-core`, which is `no_std`, sans-IO, and
allocation free on every path described here. See [00-overview.md](00-overview.md) D4.

This document is normative. Where it and an implementation disagree, this document is
corrected only by measurement against a real peer, never by convenience.

## §1 Layering

```
UDP datagram
  +- STUN            connectivity checks, demultiplexed per §2
  +- record envelope 29 bytes, authenticated encryption (§3, §4)
       +- cleartext packet
            +- data packet    7-byte header + payload  (§5.1)
            +- group ack      7 + 4n bytes             (§5.2)
                 +- channel stream, reassembled per §7
                      +- control message  13-byte header + body (§11)
```

Two transports share one socket for the lifetime of a session: connectivity checks and
encrypted media. There is no separate control connection.

**A browser is served on a second stack under the same signaling and the same checks**
([§14](#14-the-browser-transport)): a datagram transport layer session over the
attempt socket, carrying a stream-oriented association whose streams are the channels
above. Everything from the channel stream upward -- the control messages, the media
payloads, the declaration -- is the same on both; the record envelope, the rings, the
acknowledgements and the congestion signals of §3 to §10 belong to the native stack alone.

## §2 Datagram demultiplexing

A received datagram is classified on its first two bytes before anything else happens.

```
if len <= 2 or byte[0] > 1 or byte[1] != 1:
    treat as a record envelope (§3)
else:
    treat as STUN
    if byte[0] == 0: it is a Binding Request; answer with a Binding Response
```

This works because a record envelope always begins `0x17`, which is outside the range a STUN
message type can occupy in its first byte. Implementations MUST NOT introduce a record type
whose first byte is `0x00` or `0x01`.

**The bytes cannot choose between the two media stacks.** The record magic `17 FE FD` *is*
the header of an application-data record on the browser's transport layer, so a datagram of
either stack classifies the same way here. Which stack an endpoint runs is fixed per attempt
by the offer's transport field ([04 §4](04-signaling.md)), never inferred from traffic, and an
endpoint on the browser stack hands everything that is not a check to its record layer
(§14).

## §3 Record envelope

Every encrypted datagram carries a fixed 29-byte envelope. The shape is deliberately that of a
DTLS 1.2 application-data record, which is why connectivity-check credentials carry a
certificate fingerprint alongside the media key.

| Offset | Size | Field |
|---|---|---|
| 0 | 3 | magic `17 FE FD` |
| 3 | 8 | nonce counter, big endian, monotonically increasing per sender |
| 11 | 2 | size field, big endian |
| 13 | 16 | AEAD authentication tag |
| 29 | n | ciphertext |

Two details are easy to get wrong and both are load bearing.

**The tag precedes the ciphertext.** It is not appended as in TLS. An implementation that
appends will fail every decryption.

**The size field is written but never validated.** Senders set it to
`htons(plaintext_len + 45)`. Receivers MUST ignore it and derive the plaintext length from the
datagram length instead: `plaintext_len = datagram_len - 29`. Trusting it is a parsing
vulnerability and it is not what peers do.

**The counter occupies the record's epoch and sequence-number positions**, two bytes then six,
so it carries a 48-bit sequence space rather than a 64-bit one. A sender MUST stop emitting once
the counter would set any bit above 48; peers refuse to send past that point rather than
wrapping. At ten thousand packets a second the limit is nine centuries away, so this is a
correctness statement and not an operational concern -- but a counter that wraps silently
reuses a nonce, which is the one failure this rule exists to prevent.

A datagram shorter than 29 bytes is rejected before decryption.

## §4 Cryptography

The media key arrives out of band through signaling, hex encoded, as a session credential. It
is symmetric: **both directions encrypt and decrypt with the host's key.** A credential
offered by the connecting side is a capability signal only and is never used as a key. Getting
this wrong produces a clean handshake followed by universal decryption failure.

Two modes, selected by the credential and never negotiated on the wire:

| Credential | Cipher | Key |
|---|---|---|
| `aes256` present | AES-256-GCM | 32 bytes |
| `aes256` absent | AES-128-GCM | 16 bytes |

The AEAD nonce is 12 bytes: a 4-byte prefix followed by the 8-byte big-endian counter from
envelope offset 3.

**Correction (2026-08-28).** This section previously said the prefix is four zero bytes. It is
not: the credential's decoded material is the key followed by the 4-byte prefix, so the prefix
sits immediately after the key -- offset 32 under AES-256, offset 16 under AES-128 -- and a
session sealed under a zero prefix produces records no peer opens.

Nonces are **derived, never generated**, which is why the protocol core requires no random
number generator and remains deterministic under replay and simulation (D4).

There is no associated data. The envelope header is not authenticated.

The counter MUST increase monotonically per sender for the life of a session. A session that
would wrap it is torn down rather than reusing a nonce.

### §4.1 The cipher is lent to the core

The core owns the envelope -- the layout, the derived nonce, the counter's limit -- and a
portable implementation of the cipher, which is the reference and what runs inside the
core's own tests, simulator and fuzzing. **A live session's records are sealed and opened by
`ring`**, keyed by the thread that runs the session and lent to its envelope for the
session's life. That library picks the processor's widest AES and carry-less multiply
instructions at run time and constant-time software where there are none. It cannot live in
the core, because it always brings a random source (D4).

The two are interchangeable and tested to be: the same ciphertext and the same tag for every
length through a full datagram under both ciphers, and each opens what the other sealed. A
peer cannot tell which one sealed a record.

Measured on this project's development machine, through the envelope, AES-256: a 1200-byte
datagram seals in 147 ns and opens in 133 lent, against 692 and 689 portable; a 64-byte one in
59 and 58, against 84 and 68. For a host serving four guests at 40 Mbps that is a quarter of
one percent of a core rather than 1.2 percent.

## §5 Cleartext packets

### §5.1 Data packet

| Offset | Size | Field |
|---|---|---|
| 0 | 1 | marker, always `0x01` |
| 1 | 1 | flags |
| 2 | 1 | channel index, 0 to 18 |
| 3 | 4 | sequence number, big endian |
| 7 | n | payload |

Flag bits:

| Bit | Mask | Meaning |
|---|---|---|
| 0 | `0x01` | data |
| 1 | `0x02` | acknowledgement |
| 3 | `0x08` | keepalive |
| 4 | `0x10` | negative acknowledgement, valid only with `0x02` |
| 5 | `0x20` | last fragment of a message, valid only with `0x01` and not with `0x10` |

Validation, in order, all mandatory:

1. `flags & 0xC4` must be zero. Bits 2, 6, and 7 are reserved.
2. `flags & 0x0B` must equal exactly `0x01`, `0x02`, or `0x08`. Any other combination is
   malformed.
3. `0x10` requires `flags & 0x0B == 0x02`.
4. `0x20` requires `flags & 0x0B == 0x01` and `0x10` clear.
5. Marker must be `0x01`, channel must be below 19, sequence must not be `0xFFFFFFFF`.
6. Plaintext must be at least 7 bytes.

A packet failing any check is discarded without affecting session state. It is never a
protocol error.

### §5.2 Group acknowledgement

83 bytes as this implementation writes one. One packet acknowledges every channel at once,
which is why there is no per-channel ack traffic.

**The length is not fixed across peers, and a receiver must not require it to be.** The count
of cumulative entries is the number of channels the *sender* carries, and peer generations
differ: one in current use sends **four** entries, so its acknowledgement is 23 bytes. A
receiver that refuses anything shorter than its own length drops every acknowledgement that
peer sends, and the result is indistinguishable from a peer that has stopped receiving -- a
send window that only grows, every fragment stale, retransmission of the whole window, and the
session ended for undeliverability while the peer is decoding perfectly well. Read what is
present, derive the count from the packet length, and treat a channel the sender did not report
as unreported rather than as an acknowledgement of nothing.

| Offset | Size | Field |
|---|---|---|
| 0 | 1 | marker `0x01` |
| 1 | 1 | flags, `0x02` optionally with `0x10` or `0x08` |
| 2 | 1 | triggering channel |
| 3 | 4 | triggering sequence, big endian |
| 7 | 4n | n cumulative acknowledgements, big endian, one per channel the sender carries |

Each entry is the next sequence number the sender expects on that channel, so it acknowledges
everything below it. Acknowledgements are **fire and forget**. They are never placed in a
reliable ring and never retransmitted; doing so deadlocks the ring.

### §5.3 Message framing and fragmentation

A channel carries **messages**, not packets. A message is laid out across one or more
consecutive sequence numbers:

```
first fragment body:  [u32 be total_length][caller header][payload ...]
later fragment bodies: [payload continues ...]
```

`total_length` counts the caller's header plus its payload, and **excludes its own four
bytes**. Fragment count is `ceil((total_length + 4) / body_capacity)`.

`body_capacity` is the per-fragment budget for everything after the 7-byte packet header. At
the default datagram size it is 1193, which is why 1193 and 1200 both appear: 1193 of body
plus 7 of header is the 1200-byte cleartext.

The last fragment carries flag `0x20`; earlier ones do not (§5.1). **Emit this correctly, but
never depend on it when reassembling** (§7).

## §6 Channels

19 channels, index 0 to 18, each an independent reliable ordered stream with its own sequence
space, rings, and cumulative acknowledgement.

| Channel | Use |
|---|---|
| 0 | control and input |
| 1 | video, stream 0 |
| 2 | audio |
| 3 | video, stream 1 |
| 4 | video, stream 2 |
| 5 to 18 | unused |

Video stream `n` maps to channel 1 for `n = 0` and `n + 2` otherwise. v1 runs stream 0 only,
but the mapping is fixed and channels 3 and 4 are reserved for it rather than being general
purpose.

**Three video streams is the ceiling, and it is a peer-side constant.** A peer allocates its
decoders, its metrics and its per-stream configuration as arrays of three, so the index is
bounded before it reaches any host: a message naming stream 3 or above is out of range on the
peer's own terms. An older generation of the same peer allocated two, which is the one place
this number has moved.

**They are streams, not a fallback ladder.** The reason to have three is more than one thing to
show at once -- additional displays, a cursor or overlay layer carried apart from the picture it
sits on, and a peer asking for a specific view rather than taking whatever the host sends.
Everything per stream is therefore per stream on both sides: the flags a peer declares
(§11.5), the encoder generation, the encode latency, and the
mouse motion opcode that exists only to name one. A host that collapses them to a single value
records what a peer said about a picture it is not being sent.

Sequence spaces are strictly per channel. A shared receive ring keyed by a per-channel sequence
collides and corrupts.

## §7 Rings, reassembly, and flow control

Each channel holds a fixed ring per direction. The slot for a sequence number is
`seq mod depth`, so the ring is a direct-mapped window rather than a queue, and a sender that
gets more than `depth` sequence numbers ahead of the peer's cumulative acknowledgement wraps
onto occupied slots and destroys data that was already delivered.

**Correction (2026-08-28).** This section previously gave the ring depth as 4000 and the slot
payload capacity as 2000, and called both protocol constants rather than implementation
choices. **Neither is a constant.** Each peer generation picks its own, and the three in
circulation disagree:

| Generation | Slots per channel | Slot payload capacity | Channels |
|---|---|---|---|
| oldest | 1500 | 3000 | 4 |
| current | 4000 | 2000 | 19 |
| newest | 4000 | 1232 | 19 |

Only the 1193-byte body budget (§8) is common to all three, and it is the one figure that may be
relied on.

**So the safe send window is 1500 outstanding sequence numbers on a channel, not 4000**, and an
implementation MUST NOT assume more of a peer it has not identified.

**A peer identifies its generation in its group acknowledgements.** The acknowledgement's entry
count is the sender's channel count (§5.2), so a peer reporting the full 19 channels carries the
deep ring, and a peer reporting fewer -- or one not yet heard from -- is held to the 1500 floor.
The window opens on the first full-count acknowledgement, which arrives during the control
handshake, before any media channel is under load. The outstanding fragment cap of 100 (§9)
holds a conforming sender an order of magnitude below either figure, so a frame that fits at all
fits both rings; the bound matters for a future change that lifts the cap, not for anything
shipping.

The same caution applies to slot payload capacity. Sizing emissions to 2000 overruns the newest
generation, which is what §8's ceiling now reflects.

A receiver drops a packet whose sequence is below the current base, or whose slot is already
occupied, and counts it. Otherwise it stores the payload, marks the slot ready, and advances
the base across every contiguous ready slot.

**Reassembly is length-driven, not flag-driven.** A reader at the base reads the four-byte
length prefix from that slot's body, computes how many fragments the message occupies, waits
until all of them are present, then concatenates their bodies, skipping the prefix on the first
and clearing each slot as it advances.

The `0x20` flag plays no part in this. A reassembler that stopped at the flag instead of at the
declared length would work against a well-behaved sender and fail on a truncated or reordered
tail, which is exactly when it matters. Emit the flag for the peer's validation; ignore it on
receive.

## §8 MTU and path probing

Both predecessor implementations treated the wire MTU as a fixed constant. It is not, and this
is a deliberate divergence.

**Terminology, because three different sizes get called "MTU" and confusing them is how this
goes wrong.** The probed quantity is the **datagram size**, meaning the UDP payload length,
because that is what the path constrains.

| Quantity | Relation | Default | Emission ceiling |
|---|---|---|---|
| datagram (UDP payload) | `M` | 1229 | 2000 |
| plaintext | `M - 29` | 1200 | 1971 |
| payload | `M - 36` | 1193 | 1964 |

The ceiling column is what an implementation may **emit**, not what a peer will receive; see the
correction below.

On IPv4 the on-wire IP packet is `M + 28`, so the default occupies 1257 bytes and a 1500-byte
path allows `M` up to 1472.

**Default and floor: a 1229-byte datagram.** Every peer accepts this and it survives PPPoE,
tunnels, and relay framing.

**Correction (2026-08-28).** This section previously gave the absolute ceiling as a 2000-byte
datagram and said a peer that cannot accept a size discards the whole datagram rather than
truncating it. Both were read from one peer generation and neither generalises.

**The newest generation posts a 1229-byte receive buffer** -- exactly one default-sized
datagram -- where the current one posts 2000 and the oldest 3000. It does not check whether the
read was truncated, so a larger datagram is silently cut short, fails authentication, and is
counted as a corrupt packet. The observable effect is the same as a discard, which is why this
went unnoticed: the datagram is lost either way, and nothing distinguishes it from ordinary loss.

Two consequences:

- **1229 is the only datagram size every peer accepts.** It is the floor, the default, and
  against an unidentified peer it is also the ceiling.
- **The probe ladder below is unchanged and still correct**, because a probe is judged by
  whether it is acknowledged. Against the newest generation the first step simply fails and the
  session stays at the floor for its lifetime, which is the intended outcome. Expect probing to
  buy nothing against a current client and do not read its failure as a defect.
- **A probe rides a live data fragment, and staying at the floor after a loss requires
  re-emitting that fragment at the floor once the probe is judged lost.** An implementation
  whose retransmission re-emits stored bytes verbatim MUST NOT probe: the oversized fragment
  is retransmitted at the probe size for as long as the channel lives, and the channel wedges
  at that sequence instead of settling.

**Emission ceiling: 2000 bytes.** No implementation may emit more under any circumstance,
including after a successful probe -- the current generation's slot capacity is the binding
limit and there is no path to discovering headroom beyond it.

**The MTU is not negotiated and cannot be.** No field in signaling or on the wire carries it.
An endpoint's configured MTU bounds only what that endpoint emits. This means peer capacity is
unknowable a priori, and the only sound way to use headroom is to probe for it.

Probing:

1. Start at 1229. Stream at 1229 until a probe succeeds.
2. Probe upward on the active path at 1280, 1350, then 1400, all datagram sizes.
3. A probe is successful when it is cumulatively acknowledged. A probe that is not
   acknowledged while smaller packets on the same channel are acknowledged is a failure at
   that size, and probing stops there for the session.
4. Clamp at 1472 on a direct path.
5. When relayed, subtract the relay framing before clamping: 36 bytes for a data indication
   toward an IPv4 peer and 48 toward an IPv6 one, each padded to four bytes, and 4 bytes for
   channel data. The relay is the client's ([03 §7](03-connectivity.md)), so only the client
   knows its path is relayed; a host's probes cross the relay's framing on the far leg without
   knowing it. The ladder already fits: its top rung, 1400, plus the largest framing, 51
   bytes, is under 1472.
6. On any path change, reset to 1229 and probe again.

A failed probe is indistinguishable from a peer with a smaller receive buffer, and the correct
response is the same in both cases, which is why one mechanism covers both.

A 1400-byte datagram carries 1364 bytes of payload against the default's 1193, about 14
percent more per packet. A 100 KB keyframe drops from 86 packets to 76. The benefit is fewer
packets per frame, which lowers per-packet authentication cost and reduces loss amplification
on large keyframes. It is a worthwhile optimization and not a transformative one; correctness
of the fallback matters more than the gain.

**Receive buffers are never sized from the negotiated or probed MTU.** Every receive buffer is
sized from the absolute ceiling plus envelope plus relay margin. Sizing from the current MTU
silently discards whole datagrams and presents as "control works, video does not".

## §9 Acknowledgement, retransmission, and recovery

**Correction (2026-08-29).** The cadence this section stated -- a 30 ms timer plus an
immediate answer to any accepted receive -- was wrong. There are two floors sharing one
timestamp, and the cadence bullet below is the rewrite.

**Correction (2026-09-07).** This section called the retransmission timeout exponential in the
retry count. It is linear. The formula was always stated correctly; only the description of it
was wrong, so no implementation changed -- but "exponential" implies a backoff that
self-limits, and this one does not.

- **Sequence arithmetic is RFC 1982 everywhere.** A naive 32-bit comparison inverts at wrap,
  which arrives in roughly 15 days of continuous high-rate video. Every comparison of
  sequence, base, and cumulative acknowledgement uses signed difference.
- **Acknowledgement cadence has two floors on one timestamp.** A receive whose store is
  accepted is answered when 10 ms have passed since the last acknowledgement of either
  kind, and at once when the fragment reveals a gap or ends its message. Anything in
  between waits, and what it advanced rides the next acknowledgement's cumulative counts,
  so nothing is lost by waiting. A session with nothing to answer is held open by a
  keepalive 30 ms after the last acknowledgement of either kind, and every
  acknowledgement sent resets the one clock both floors read.
- **Round trip estimate** is an exponentially weighted moving average, `rtt = rtt * 0.9 +
  sample * 0.1`, sampled when an acknowledgement clears a slot carrying a send timestamp.
- **Retransmission timeout** is per fragment and linear in its retry count:

  ```
  rto = clamp(2 * (retransmissions + 1) * srtt, 50 ms, 1000 ms)
  resend when time_since_last_send > rto + 30 ms
  ```

  The 30 ms is a flat grace on top of the clamp, not part of it. Note this is **not** derived
  from the congestion level table; that table serves a different purpose (§10).

  **Each retry adds one `2 * srtt`, so the series is 2, 4, 6, 8 times the round trip and not a
  doubling.** On a fast path the 50 ms floor swallows the multiply until `(n + 1) * srtt`
  passes 25 ms, so the timeout barely backs off at all and the outstanding cap above is what
  actually bounds retransmission.
- **Negative acknowledgement** (`0x10` with `0x02`) triggers fast retransmission of everything
  below the named sequence, without waiting for the timeout. **Once per fragment per
  acknowledgement**, tracked by a latch on the fragment, so a burst of nacks cannot turn into a
  retransmission storm.
- **Outstanding fragments are capped at 100 per channel, and the cap gates the whole scan.** A
  sender at the cap does not send; it marks the fragment deferred and lets the retransmission
  scan release it as the window drains. Past the cap the scan classifies and emits nothing, so
  the fragments nearest the base are the ones that get the bandwidth. The same 100 is the
  congestion controller's window floor (§10), so it is one constant with three consumers and
  must not be tuned in one place alone.
- **A fragment in a retransmitting state counts against the cap whether or not it is due**, as
  does one released from deferral. **This is the only ceiling on retransmission there is.**
  Counting emissions alone lets a window of stale fragments admit another hundred sends on
  every pass, which is a peer that has stopped acknowledging being sent several times the
  configured bitrate for as long as the session lasts.
- **Stall escape:** when a gap cannot be filled and the window is starving, the reader skips
  forward. It MUST jump to the **furthest** occupied slot, never the nearest. Jumping to the
  nearest crawls the flow-control window and has cost a 20x throughput regression.
- **Anti-replay windows MUST NOT be applied to reliable channels.** They reject legitimate
  retransmissions.
- **Liveness:** 60 seconds without progress is a soft failure; 120 seconds is a hard failure.
  Keepalives (`0x08`) hold an idle session open.
- **Delivery deadline:** a channel holding outstanding fragments, none of which the peer has
  acknowledged for 15 seconds, is a hard failure of the session. **Judged per channel and on
  the acknowledged count, never on the window.** The liveness deadlines above measure the
  inbound direction only, which a peer that keeps acknowledging while it has stopped receiving
  satisfies indefinitely, and its whole send window is retransmitted for exactly as long. A
  congested path fills a window identically and acknowledges throughout, so the window cannot
  distinguish them. A peer that has stopped draining one ring keeps acknowledging the others,
  so a count summed across channels cannot either.

## §10 Congestion control

Host local, computed from local transport state only. **No congestion feedback message exists
in either direction and none may be added** (D8).

Inputs, per channel, per tick: the outstanding window (`send_next - send_base`) and the count
of stale slots.

```
congested = window > 100 and stale / window > threshold[level]

if congested:
    on the first congested tick and every 60th thereafter:
        peak = peak * 0.7
        current = peak
else:
    every 30th clean tick:
        peak = max(peak, measured_throughput)
        current += min(step, 5) * 0.15
        step += 2

rate = clamp(current, min_rate, max_rate)
```

Levels:

| Level | Stale ratio | Staleness threshold | Notes |
|---|---|---|---|
| 0 | 0.0 | none | **Stale by construction.** Both its multiplier and its constant are zero, so every occupied fragment classifies stale and congestion is declared on every pass once the window exceeds 100. The most aggressive setting, not a disabled one. Do not use it as a fallback for an out-of-range value. |
| 1 | 0.15 | `srtt * 1.1 + 20 ms` | Default. |
| 2 | 0.35 | `srtt * 1.5 + 50 ms` | Tolerates more delay before counting a fragment stale. |

**The three levels above are the whole of the detector**, and a host that runs one of them
behaves the same way a peer of the same generation does. A fourth setting, *adaptive*, selects
level 1's tuning and reserves a place for host-local signals that see what the window floor
hides. **Nothing is behind it yet**, so it behaves exactly as level 1; it exists so that a
signal which earns its measurement becomes a setting rather than a rebuild.

**The distinction it draws is between an addition and a correction.** A signal that goes beyond
the three levels sits behind *adaptive*. A defect found in the three levels is fixed in them,
because a correction that has to be asked for is a defect left on by default.

**The staleness threshold is not a retransmission timer** (§9). It classifies an outstanding
fragment as stale for the purpose of the ratio above. A fragment counts as stale on any of:

- it is older than the threshold, measured from its most recent send;
- **the smoothed round trip has grown past the round trip that held when this fragment was
  queued**, scaled by the same multiplier and constant;
- it has already been retransmitted, fast-retransmitted, or deferred.

**The second clause asks whether the path got slower, not whether it is slow.** Each fragment
carries the round trip as it stood at the moment it was queued, and is judged against that.
The baseline is stamped once and never restamped, so a fragment the outstanding cap held back
is compared against the path from before the queue built -- which is the comparison worth
making about it. A fixed budget in that place answers a different question, one that is true
of a bad path from its first frame and never true of a good path going bad; the second is the
case this clause exists for.

It also carries a counterweight the first clause needs. Round-trip samples are taken from a
fragment's first send and are not filtered, so retransmissions inflate the smoothed figure
during congestion, which loosens the first clause exactly when it should tighten. An inflating
round trip tightens this one by the same motion, because it is measured against its own past.

**Where `stale` comes from.** The retransmission scan produces it as a side effect of walking
the outstanding fragments, and writes it where the controller reads it. The two are one loop
split across two functions, not independent subsystems, and changing the scan changes the
controller's input.

The resulting rate actuates the **encoder bitrate** through a live reconfigure. It does not
pace the socket. The reconfigure MUST NOT reinitialize the encoder and MUST NOT force a
keyframe.

**A tick is a frame, not a timer.** The controller runs once per guest per encoded frame, from
the pipeline that produced the frame. That fixes the periods above in wall-clock terms: at 60
frames per second the 30 clean ticks between increases are half a second and the 60 congested
ticks between decreases are one second. Ticking it from a timer instead decouples the rate from
the thing the rate actuates, and ticking it per channel from a receive path makes the period
depend on inbound traffic.

Throughput is measured over the interval between ticks and requires **fractional millisecond**
resolution. Quantizing the interval to whole milliseconds silently skips the peak update
whenever it rounds to zero. The quantity measured is bytes sent on that channel since the
previous increase tick, and the unit is **mebibits per second**: bytes times eight, divided by
1048576, divided by the interval in seconds. Dividing by 1000000 instead reads about five
percent high, which is a silent bias in the peak the controller creeps back toward.

With more than one guest, the rate applied to the encoder is the **minimum** across guests, and
it is applied only when it moves by more than 0.01 Mbps, so a rate that oscillates in the noise
does not produce a reconfigure per frame.

## §11 Control messages

Control and input ride channel 0 as a stream of messages with a 13-byte header:

| Offset | Size | Field |
|---|---|---|
| 0 | 4 | argument 0, big endian |
| 4 | 4 | argument 1, big endian |
| 8 | 4 | argument 2, big endian |
| 12 | 1 | opcode |

Some opcodes append a body after the header. String bodies are NUL terminated and the declared
length **includes the terminator**; omitting it causes a silent parse failure on the peer.

### §11.1 Received by the host

| Op | Name | Arguments | Status |
|---|---|---|---|
| 0 | keyboard | usage code, modifier mask, pressed | v1 |
| 1 | mouse button | button, pressed, 0 | v1 |
| 2 | mouse wheel | x, y, 0 | v1 |
| 3 | mouse motion, stream 0 | relative flag, x, y | v1 |
| 4 | gamepad button | button, pressed, pad | v1 |
| 5 | gamepad axis | axis, value, pad | v1 |
| 6 | gamepad unplug | 0, 0, pad | v1 |
| 11 | init | header plus JSON body | v1 |
| 13 | encoder configuration | stream, flags, reinit | v1 |
| 17 | user data | length, sub-id, 0, plus body | v1, opaque pass through |
| 21 | decode latency | microseconds, kind, stream | v1, diagnostic |
| 23 | gamepad state | pad, 0, 0, plus 15-byte body | v1 |
| 24 | release all input | 0, 0, 0 | v1 |
| 26 | mouse motion, stream 1 and above | packed | v1 |
| 30 | pen and touch | packed | deferred |
| 31 | pad report | length, pad, product and flags, plus the report | v1 (*Phase 14*) |
| 35 | diagnostics | bit flags | v1 |

**Opcode 21 travels in both directions and its arguments are transposed between them.** A host
sends `(kind, microseconds, stream)`; a peer sends `(microseconds, kind, stream)`. Reading the
inbound one with the outbound layout gives a stream index of one and a latency of nothing.
Kind 1 is per-stream and kind 2 has a slot of its own. What a peer reports here is its decode
time, and a host that does not want it can drop the message.

**Opcode 35 turns diagnostics on.** Bit 0 of argument 0 enables the per-frame timing of opcode
34, which is what makes that message's "behind a diagnostic flag" concrete; bit 1 enables a
second thing that has not been read. A peer sends it with both clear in an ordinary session,
which is a request to send nothing extra rather than a message to ignore.

**The pad identifier is the peer's, and it is arbitrary.** Opcodes 4, 5, 6 and 23 all carry a
32-bit value the peer chose; it is not an index and nothing bounds it. A host maps it to a slot
and caps how many slots one guest may occupy, or a peer that varies the field creates a device
per distinct value. **Keep it below 256** (*2026-09-20*): an established host keys those four
messages' pads on the low eight bits of the identifier and the report message's (opcode 31)
on the whole of it, so a report under a wider identifier finds no pad and is dropped, and the
rumble it sends back names the eight-bit one. Found live: a peer naming its pads from 0x5000
up got sixteen-button pads and nothing else from a host in its DualShock mode.

**Opcode 23's body is fifteen bytes and the first three of them are padding**: a big-endian
`u16` of button bits, four big-endian `i16` thumbstick axes, then two single-byte triggers. The
padding is whatever the peer's stack held. Skip it; never validate it, and never treat a
nonzero value there as a different message.

**A gamepad reports itself two ways and a host takes both.** Opcodes 4 and 5 carry one button or
one axis; opcode 23 carries a whole pad in one message. Which a peer sends is its own choice,
and a peer may change it mid-session.

**The two forms number the buttons differently and they do not line up.** Opcode 4's argument
is an index into one ordering; opcode 23's body is a bit field in another, and the bit field
carries a touchpad button the index has no value for. Neither is derivable from the other, so a
host that maps one and shifts it into the other produces a pad whose face buttons are its
direction pad. **Opcode 5's axis value is signed sixteen-bit** carried in an unsigned
thirty-two-bit argument, so it needs a narrowing cast and not a comparison against zero.

**Opcode 31 carries a pad's own report, raw** (*added 2026-09-20, Phase 14*). A peer holding
a controller with more to say than the sixteen-button layout -- a DualShock 4 or a DualSense:
touch contacts, motion, and on the way back the lightbar and the trigger effects -- sends the
report the device produced, and the host presents a device of the same model that produces
the same report. The first argument is the body's length, the second the same pad identifier
opcodes 4, 5, 6 and 23 carry, and the third names the product: the low sixteen bits are the
product identifier (`0x09CC` or `0x05C4` for a DualShock 4, `0x0CE6` for a DualSense) and bit
16 marks a **feature report** rather than an input report. Established peers write zero
there, and a host reading zero tells the two apart by the length.

The body is the report in its wired form, and the two products are framed differently on
purpose:

- **A DualSense input report is the 64 bytes the device produces over USB**, report
  identifier included. That is the form an established host writes into its own virtual
  DualSense, so the body cannot be anything else.
- **A DualShock 4 input report travels without its identifier byte: 63 bytes.** An
  established host in DualShock mode reads only the ten-byte touch block at offset 33 of that
  report, and one in DualSense mode writes *any* 64-byte body into its virtual DualSense --
  so a DualShock 4 report at 64 bytes would drive a game with garbage on a host set to the
  other pad. Sixty-three bytes match nothing an established host reads. **A peer that wants
  an established host's DualShock mode to see the touchpad sends the ten-byte block as a
  second message**, the established peer's own form: no product in the third argument, and a
  host that reads products ignores it.
- **A feature report** (bit 16) is the device's answer to one of the reads a host's driver
  makes when the device appears -- calibration and firmware, by their identifiers -- and
  travels ahead of the first input report so the host has it when it creates the device. A
  host answers from what it was given and from a plausible default for what it was not.

**A pad's family is fixed by the first message that can create it**, per identifier. A
message of opcode 23, 4 or 5 makes the sixteen-button pad; an opcode 31 message naming a
product, or carrying an established peer's 64-byte DualSense report, makes that product. A
pad created from its report ignores every 23, 4 and 5 that follows -- the report is the
superset -- and a ten-byte block with no product cannot create anything, so an established
peer's DualShock 4 is the sixteen-button pad. A peer sends opcode 23 beside opcode 31
anyway (the report first), because a host that does not read opcode 31 creates its slot from
23 alone and a host that does gets a fallback it can ignore. A pad is destroyed by opcode 6
and by the peer leaving, as any pad is.

**Bluetooth is the peer's problem, not the wire's.** Both products frame their reports
differently over Bluetooth (a leading identifier and a checksum, the DualShock 4's at offset
3, the DualSense's at 2); a peer converts to the USB form before sending and converts what
comes back, so a host sees one form of each report and never a checksum.

**Opcode 32 is not a pad's.** It is the device passthrough of [§11.4b](#114b-the-guest-microphone),
one opcode for several kinds of device with a fixed body, and a controller has the pair
above.

### §11.2 Sent by the host

| Op | Name | Arguments | Status |
|---|---|---|---|
| 9 | cursor update | 21-byte fixed body plus optional PNG image | v1 |
| 10 | disconnect | status, 0, 0 | v1 |
| 27 | stream ended | stream, 0, status | later |
| 16 | input blocked | 1 blocked, 0 unblocked | later |
| 17 | user data | length, sub-id, 0, plus body | v1 |
| 20 | rumble | pad, large motor, small motor | v1 |
| 21 | encode latency | 1, microseconds, stream | v1 |
| 25 | guest list | length, the recipient's own guest number, 0, plus a JSON body | v1 |
| 28 | host mode | mode | later |
| 29 | encoder generation | stream, generation, 0 | v1 |
| 33 | pad output | length, kind, pad, plus the report | v1 (*Phase 14*) |
| 34 | frame timing | 0, stream, 0, plus 16-byte body | diagnostic |

Two of these have cadences rather than triggers. Encode latency goes out **every two seconds
on the clock, from the moment the path exists** -- not every thirtieth frame, which it was until
2026-09-12: a frame count is a cadence only while frames flow, a still desktop sends one a
second, and a browser page reads five seconds of silence on the control channel as a dead link
([§14](#14-the-browser-transport)). The figure is zero until a picture has been timed and the
message goes out anyway, on every transport; a native peer reads the extra reports as it reads
any. The guest list is sent on a change of membership and repeated on an interval; see
[§11.2b](#112b-the-guest-list) for why the interval is measured in time rather than in frames.

**Opcode 10's argument is a status the peer already renders**, from the same enumeration its own
API reports. Sending a value outside it shows as a blank reason rather than as an error, so a
host picks from the set rather than inventing one. **Zero is not one of them**: a peer stores
the status and stops on a non-zero one, so a disconnect carrying zero fires the peer's callback
and leaves the session running.

**Opcodes 10 and 27 are the same event at two scopes.** A host that can no longer serve a
stream ends the whole session with opcode 10 when the stream is the primary one, and reports
just that stream with opcode 27 when it is not, leaving the session up. v1 produces the primary
stream only, so it sends opcode 10 and never opcode 27; the pair is documented together because
a host that grows a second stream needs the distinction and it is not guessable.

**Opcode 21's first argument is 1, not 0.** The value it carries is an exponentially weighted
average of capture to bitstream-collected, `latency = 0.9 * latency + 0.1 * sample`, converted
to microseconds at emission.

**Opcode 29 announces an encoder generation**, carrying the same value the video header's frame
identifier will report (§11.3). It is emitted on the frame following an encoder
initialization, which is how a peer learns the reference chain restarted rather than inferring
it.

**Opcode 34 is per-frame timing telemetry** behind the diagnostic flag that opcode 11's
counterpart, opcode 35, carries (§11.1), so an ordinary session never emits it: four big-endian floats covering loop start to encode complete, capture start to
encode start, the frame interval, and the encode duration. Documented so its arrival is not
mistaken for something else.

**Opcode 33 carries back what the host's virtual pad was written** (*added 2026-09-20, Phase
14*): the output report an application on the host sent to the device -- motors, lightbar,
player lights, and on a DualSense the two trigger effects -- or a feature report it wrote.
The first argument is the length, the second the kind (`1` an output report, `0` a feature
write), the third the pad identifier the peer chose; the body is the report in the USB form
with its identifier byte, which a peer holding a wireless pad re-frames itself. **A pad
created from its report is never rumbled with opcode 20**: its motors are two bytes of the
output report, and a peer's toolkit that answers opcode 20 with a report of its own would
paint its constant lightbar over the one the host just sent. The sixteen-button pad keeps
opcode 20. An established peer applies kind 1 to whatever pad it holds, so a host may send it
for either product.

Cursor images on the wire are **PNG, not raw pixels**. Cursor position is in stream space and
requires the host-to-client transform, including a width and height swap on rotated displays.
**A cached name that carries no size means the picture's size and hotspot are the ones sent
with it**: a reader keeps the two with the picture and takes the hotspot from the header's
arguments only when the message carries a size, so a sender naming a picture repeats what it
sent or sends nothing for both (*2026-09-19*).

### §11.2a Application messages

Opcode 17 travels in both directions and the SDK never looks inside it. The arguments are the
body's **length**, a **sub-identifier** the application chose, and zero; the body follows.

**The length counts a terminating zero byte, and the body carries one.** Established peers read
the body as a C string, so a body sized by its text alone leaves the reader running one byte
past what arrived. A sender MUST write the terminator and count it; a receiver MUST NOT require
it, because this is a pass-through and refusing a message because a peer framed its own payload
differently discards something the SDK was never entitled to judge. A trailing zero is stripped
before the body is handed on, so an application is given the text and not the byte that ended
it.

**A declared length bounds the body and never extends it.** A peer claiming more than it sent is
taken at what it sent.

**There is a ceiling of 1 MiB including the terminator**, and it is the receiver's: one byte
over is dropped at the far end with nothing said, so a sender that does not check loses the
message and cannot find out. Refuse it locally instead.

**The sub-identifier space belongs to the application, not to this protocol.** Two applications
that both use opcode 17 are speaking different languages over the same channel, and a host that
acted on a sub-identifier would be choosing one of them ([05 §5](05-host.md)).

### §11.2b The guest list

Opcode 25 tells one guest who else is connected. **The body is the same for everyone and the
second argument is not**: each guest is sent its own number alongside, because that is how a peer
finds itself in the list and learns what it is permitted to do. A roster carrying nobody's own
number describes a room the reader is not in.

**A peer cannot ask for this**, so it is sent whenever the room changes -- a guest joining or
leaving -- and every guest is told, not only the one that moved.

**And repeated on an interval, because what it carries moves.** Membership changes on an event;
the per-guest telemetry in the body changes continuously and nothing announces it, so a reader
watching a rate or a round trip needs the message again. **Two seconds, measured in time and not
in frames.** A frame count is the obvious way to space this and it is wrong on an idle host: a
still desktop is coded at a frame a second, so a count that means two seconds under load stretches
to two minutes of stale numbers exactly when a reader is most likely watching. Nothing is sent
while the room is empty.

**Repeating it is only worth doing once the body is true.** A block of zeros repeated faster is a
reader painting "no data" over a figure its own messages gave it, more often. The telemetry has to
be filled first; the interval is what makes it useful afterwards.

**It is load bearing beyond the obvious.** A peer that never receives one does not know what it
is, and hides whatever depends on knowing, which can be far more than a list of names. Treating
it as decoration because a stream renders without it is a mistake this project made and paid
for.

The body is UTF-8 JSON, NUL-terminated and counted with the terminator, exactly as
[§11.2a](#112a-application-messages) requires. Its shape is an application's, not this
protocol's: what belongs here is that one exists per guest and carries at least that guest's
number, its permissions and whether it owns the machine.

**Where the body carries per-guest telemetry, the blocks are per channel and the readers in
circulation expect a fixed count of them.** Each block describes one channel; the round trip is
the session's and is repeated into every block that describes a live one. A host that runs fewer
video streams than the shape allows still emits the full array, with the entries for streams that
never ran left entirely zero -- the round trip included, since a stream that never opened had no
path of its own to measure one on. Shortening the array is a shape no reader has a reason to
expect.

### §11.3 Video framing

Video is not a control message. It rides the ordinary message framing of §5.3 on its own
channel, with a 10-byte header ahead of the bitstream:

| Offset | Size | Field |
|---|---|---|
| 0 | 4 | frame identifier, **little endian** |
| 4 | 2 | width, little endian |
| 6 | 2 | height, little endian |
| 8 | 1 | codec: `1` H.264, `2` HEVC (*corrected 2026-09-16*; older hosts wrote `1` for both) |
| 9 | 1 | flags |

Flags: bits 0 to 2 rotation, **bit 3 ten-bit colour**, bit 4 the host session is locked
(*corrected 2026-09-16*; below), bits 5 and 6 the video protocol (below).

Note the endianness change. Sequence numbers and message lengths are big endian; these fields
are little endian. Getting this backwards produces a plausible-looking frame with absurd
dimensions.

Three traps, all of which have cost time already.

**Rotation is one-based.** `0` means unspecified, and upright is `1`. A host that emits `0x00`
for an unrotated display is emitting "unknown", not "none". Conversely a receiver that reads
`0x01` and concludes the display is rotated has misread it by one; a quarter turn is `2`.

**Nothing in the header says which pictures are keyframes.** A receiver classifies them from
the bitstream, by finding the start code and testing the unit type for a parameter set or an
instantaneous refresh. There is no bit to check first.

**Correction (2026-08-18).** This section previously called bit 3 a keyframe flag and said a
host should set it because doing so was more informative and free. **Bit 3 is the colour
depth: set means ten-bit.** A receiver builds its decoder for the depth that bit names before
parsing any bitstream, so setting it on an eight-bit stream makes one decoder family
initialise for ten-bit and fail every picture, and hardware with no ten-bit support fails at
the first submission. It is reported as a decode failure rather than as a mismatch, and only
that one decoder family is affected, so it presents as a peer-specific defect.

The evidence was already in this section and was read backwards: across 4883 video messages
the flags byte was `0x01` on every one, **including both messages whose first unit was a
parameter set**. A host that never sets the bit on its own keyframes cannot be describing
keyframes with it; an eight-bit host never setting a ten-bit flag is exactly what it looks
like.

**Amended 2026-08-30: we set it when, and only when, the stream really is ten-bit.** The rule
that mattered was never "never set it" but "the bit must describe the pixels": a receiver builds
its decoder from this before parsing any bitstream, so a bit that disagrees with the stream
fails every picture whichever way it disagrees. An eight-bit stream still clears it, which is
every stream until a guest asks otherwise.

**Bit 4 was called full screen here until 2026-09-16, and is the lock state of the host's
session**: set while the desktop is locked or the session the host runs in is not the active
one, sampled every frame, and handed by a newer client to its application with the picture.
Nothing turns on it; a host with no such state leaves it clear, which reads as unlocked.

**Amended 2026-09-15: bits 5 and 6 exist, and a newer client generation reads them.** The
established host of the same generation sets both when the declaration below holds, and so
does this one, per guest (*2026-09-16*, [05 §6.1a](05-host.md)); a client that understands
them treats their absence as the older behaviour.

- **Bit 5 (`0x20`): this picture was announced by a metadata message, and its parameter
  sets rebuild nothing.** The rule a client has always applied is that an access unit led by a
  sequence or video parameter set tears its decoder down and builds it again -- that is how a
  codec or size change has always been carried. Since parameter sets are repeated on every
  keyframe (this host does so, and so does the established one), every keyframe is a decoder
  rebuild for every client that does not see this bit. With it set, a client that has a
  decoder feeds the unit to it and skips the generation check as well; a client with none
  builds one from the unit whatever the bit says. An older client ignores it.
- **Bit 6 (`0x40`): this message is keyframe metadata, not a picture.** It is 21 bytes: the
  ten-byte header with the same identifier, size and codec as the picture it announces and
  bits 5 and 6 both set, then a four-byte word of `1`, a four-byte word at offset 14 whose
  **bit 0 says the encoder was rebuilt** and **bit 1 says a keyframe follows**, the colour
  depth as one byte at 18 (`1` eight-bit, `2` ten-bit), the chroma layout at 19 (`2` for
  4:2:0, `0` for 4:4:4), and the rotation at 20. The picture follows as the next message on
  the channel. **The rebuild bit is what replaces the parameter-set rule**: a client tears
  its decoder down on it, before the keyframe arrives. A client that has fallen behind on
  the channel looks ahead through the messages it holds for one of these whose keyframe has
  already arrived and **skips forward to it** -- a catch-up over data that has all arrived,
  aligned to a keyframe, which is what makes a lagging reader recover in one step instead of
  decoding its way through the backlog. Nothing is skipped over a gap: the channel is
  reliable and a missing fragment is retransmitted, never abandoned
  ([§9](#9-acknowledgement-retransmission-and-recovery)).

Whether a host emits them is negotiated by the `_VideoProtocolVersion` key of the
initialization ([§11.5](#115-session-initialization)). **Corrected 2026-09-16: the value is
read, and so is what a host does with it.** Every client generation that understands the bits
sends the literal `1`; a host treats any nonzero value as that declaration and zero or
absence as the older framing. No other value exists. The established host applies the
declaration to the room as a whole -- every seated guest must have declared it -- and
rebuilds its encoder whenever that answer changes, so a guest without the key leaving a room
that had one costs the others a rebuild. A host that writes the header per guest, as this
one does, may decide per guest instead and owes no rebuild on either edge.

**What a host does when the declaration holds, and it is all-or-nothing:** a metadata message
before every keyframe, with bit 0 set on the first keyframe after an encoder build and clear
on every other; bit 5 on every keyframe and on no other picture; and, where the host is
configured with a keyframe interval, a periodic keyframe at that interval, which is what the
catch-up lands on -- without the declaration the interval is ignored and the stream has no
keyframes but those a rebuild or a request produces. When it does not hold, none of this is
sent and the framing is the older one exactly. Setting bit 5 without the metadata is safe (a
client with no decoder builds one from any parameter-set-led unit, and a client with one is
only wrong if the sets changed), but no host does it; the pair is the protocol.

**The frame identifier is not a frame counter.** It is an encoder generation counter and stays
constant across a whole session's frames, incrementing only when the encoder is reconfigured.
It went from 1 to 2 across the same 112 second recording. Anything using it to order or
deduplicate frames is broken.

Within the first fragment's body, and remembering the four-byte length prefix from §5.3, the
absolute offsets are: length at 0, frame identifier at 4, dimensions at 8 and 10, the codec at
12, flags at 13, start code at 14, and the first unit's type byte at 18.

### §11.4 Audio framing

Audio rides channel 2 with a fifteen-byte header ahead of the payload, inside the ordinary
message framing, so the offsets below are relative to the message **content**: what follows the
four-byte length prefix. Every field is little endian, as in §11.3.

| Offset | Size | Field |
|---|---|---|
| 0 | 4 | channel mask |
| 4 | 4 | samples per channel in this packet |
| 8 | 4 | sample rate, always 48000 |
| 12 | 1 | codec: `2` is uncompressed, anything else is Opus |
| 13 | 1 | written as `2` and never read |
| 14 | 1 | channel count |

**The codec is per packet, not per session.** A receiver rebuilds its decoder when the codec,
the channel count or the channel mask changes, and on nothing else -- so a host may change its
sound device mid-session without renegotiating anything, and must not change the layout without
expecting a rebuild.

**Uncompressed means interleaved sixteen-bit samples**, and a receiver derives their count from
the payload length rather than from the header: bytes over two over channels. The header's own
sample count is what a compressed packet declares.

Two traps, both of which read as something else.

**The channel mask is not a reserved zero.** It selects the stream layout a compressed decoder
is built with, and **only the low-frequency bit within it is consulted**: for two channels
without that bit the answer is one stream, one coupled pair, which is what an ordinary stereo
encoder produces. So zero and the stereo mask of `3` decode identically, and a host that emits
zero is describing a layout it does not have while getting away with it. **We emit the mask.**

**Bytes 13 and 14 are not one field.** Both are `2` for stereo, which makes the pair look like a
single tag with a self-verifying value; it is not, and the second byte is the channel count a
receiver builds its decoder from. A host that hard-codes the pair and later grows a layout with
a different channel count will emit a header that says stereo and a payload that is not.

**There is a ceiling on an uncompressed packet.** A receiver refuses one whose payload exceeds
32000 bytes, so a frame longer than about 160 ms of stereo is not deliverable uncompressed. At
the 20 ms this host sends, a packet is 3840 bytes.

### §11.4b The guest microphone

**The other direction, and it is not on the audio channel.** Sound to a guest rides channel 2
with the framing above; sound from one rides the **control channel** as a virtual device: one
opcode carrying several kinds of device, told apart by the header's own arguments rather than by
the opcode.

| Field | Value |
|---|---|
| opcode | 32 |
| argument 0 | the declared length, `1932` |
| argument 1 | `1` |
| argument 2 | `0xF055F055` |
| body | a fixed 1932 bytes |

**The first argument is the declared length, and the selection is the two after it.** That is the
same convention an application message follows (§11.2a), and reading the selectors one position
early finds the length where a selector should be: nothing matches, every packet is passed over
as another device's, and a peer that is sending a hundred packets a second looks like one sending
nothing.

**Both selectors select together.** Another device on the same opcode uses `0` where the
microphone uses `1`, so neither alone identifies it, and a body whose own kind disagrees with the
header is refused rather than believed: one sender writes both in one call.

The body is little endian and is **the same 1932 bytes whatever it carries**, with the payload's
real length inside it:

| Offset | Size | Field |
|---|---|---|
| 0 | 4 | device kind, `12` for a microphone |
| 4 | 1920 | payload |
| 1924 | 4 | payload length in bytes |
| 1928 | 1 | encoding: `1` compressed, `0` uncompressed |
| 1929 | 3 | padding |

**The encoding byte is not the audio channel's codec tag.** That one spells uncompressed `2`;
this one spells it `0`. The two are close enough to swap without noticing until a listener hears
static.

**48 kHz mono, ten milliseconds a packet.** A sender folds whatever its device captured down to
one channel before encoding, so the layout question is settled before it leaves; compressed
packets are the codec's voice mode, and uncompressed is 960 bytes of sixteen-bit samples. A
receiver bounds what it will decode rather than trusting the length: the codec can be asked for
far longer frames than ten milliseconds, and the length is the sender's to write.

**A peer sends nothing until it is told it may.** The host announces whether it will take a
microphone as an application message (§11.2a) under sub-identifier 18, whose body is a decimal
string: `"0"` means no and anything else means yes. A peer told no -- or never told at all --
keeps its microphone muted however it is configured itself, so a host that merely listens
receives silence.

**It costs what it costs, on the channel that carries control.** A packet every ten milliseconds
of 1932 bytes is around 193 kB/s in two fragments, roughly 200 fragments a second, reliable and
ordered, sharing head-of-line with the guest list, the cursor and the latency reports. A lost
fragment delays whatever is queued behind it. That is the reason a host decides whether to take
one rather than always taking it.

### §11.4a Audio latency reports

Both ends volunteer a decode or encode figure on the control channel as opcode 21, on a cadence
rather than per packet: **microseconds in one argument and the media kind in the other**, `1` for
video and `2` for audio. The two directions carry the pair in opposite order -- a host sends the
kind first, a peer sends the figure first. Nothing depends on receiving one, and a host that
ignores every one it receives is not missing anything a stream needs.

**The cadences differ by generation and by kind, and none of them is required.** A host
sends its video figure on a two-second clock here and every thirtieth frame elsewhere, its
audio figure every hundredth packet; the current client generation sends its video figure
every thirtieth decoded picture and its audio figure every twenty-fifth packet, and the older
one sends the video figure alone, with the kind argument zero -- so a host that accepts only
kinds 1 and 2 discards an older peer's report, and one that reads zero as video recovers it.
A client of ours sends both kinds on a two-second clock ([10 §7](10-client.md)). Every
figure is a smoothed average, a tenth's weight on the newest sample, in microseconds.

### §11.5 Session initialization

The connecting side sends opcode 11 with a JSON body declaring its preferences: maximum
resolution, codec capability, colour mode, and feature flags. The host is authoritative and
may ignore any of it (D8).

The message's own arguments are not empty: **argument 0 is the body length including the
terminating NUL**, and the other two are zero. The body is a NUL-terminated JSON object with
exactly these eight keys, in this order:

```
_version  _max_w  _max_h  _flags  resolutionX  resolutionY  mediaContainer  refreshRate
```

**Only `_version` is mandatory and it must be 1.** Every other key has a default, so a missing
one is a default rather than a refusal. Two of them carry sentinels rather than sizes:
`_max_w` and `_max_h` arrive as 60000 to mean **no limit**, and `resolutionX` and `resolutionY`
arrive as 0 to mean **no preference**. A host reading either as a dimension tries to encode a
picture nobody asked for.

**A maximum of zero is also no limit**, whether the key was absent or sent as zero. Peers exist
that state neither maximum, and a host that reads the absence as a ceiling has a ceiling of
nothing.

**Eight keys is the smallest object seen, not the only one.** One peer sends exactly those
eight in about 124 bytes; the current client generation sends fourteen in about 306. A host
reads the keys it knows and ignores the rest.

**The fourteen-key body (amended 2026-09-15)**, in order, as the current client generation
sends it:

```
_version 1   _max_w   _max_h   _flags   resolutionX   resolutionY   mediaContainer
refreshRate 60   channels 2   channelMask 3   rawAudio   _cache_cursor true
_VideoProtocolVersion   resolutions [ {width, height} x 3 ]
```

`channels` and `channelMask` describe the sound the client wants (two channels, front left and
right); `rawAudio` asks for uncompressed sound; `_cache_cursor` says the client keeps cursor
shapes by identifier; `resolutions` is the size request per stream, `{width, height}` for each
of the three, and its first entry repeats `resolutionX` and `resolutionY` and takes precedence
over them (*corrected 2026-09-16: it is a request, not a list of what the client can
display*); `_VideoProtocolVersion` declares the video framing extensions of
[§11.3](#113-video-framing), and its value is `1` -- a host reads it as an integer, treats
nonzero as the declaration and zero or absence as its lack, and no other value exists
(*corrected 2026-09-16*). **A client of ours sends the fourteen**, because the host generation
that reads them behaves differently when they are absent; a host of ours accepts either shape.

**Do not add keys beyond those.** Peers exist that behave differently when the object carries
keys they do not know, taking different encoder-warmup or session-setup paths, so a host must
not require extras and a client must not invent them.

Codec selection is carried in two places. The capability bit in the init flags declares
support, and opcode 13 argument 1 carries the same video flags again. Opcode 13's other
arguments are the stream index in argument 0 and a reinitialization request in argument 2.

**A host reads both, and the later one wins.** The two are not a pair that must agree: a peer
may declare in the initialization and never send opcode 13 at all, and a host that required
both would leave every such peer declaring nothing. A peer that does send opcode 13 is
restating its capability, and when the value differs it is **changing its mind mid-session** --
which is the whole point of the second place, and a host that kept the first would never hear
it.

**Argument 2 asks for a different stream, not only for a keyframe.** A host that can code what
the new flags name reinitializes its encoder: new parameter sets, a new reference chain, and a
new generation announced on opcode 29. Where nothing about the request changes what is already
being produced, a keyframe is what it is owed. See [05 §6.1](05-host.md).

**Argument 0 is a stream index, and a peer declares per stream rather than per session.** A
peer holds up to three and sends one of these for each. Observed: a client sends them for
streams 2 and 1 before it ever sends one for stream 0. A host that ignores the index records
what the peer can decode on a stream it is not being sent, and then acts on it; the flags for
streams it does not produce belong to those streams and to nothing else.

The flag bits:

| Bit | Mask | Meaning |
|---|---|---|
| 0 | `0x01` | HEVC |
| 1 | `0x02` | 4:4:4 chroma, which implies HEVC |
| 3 | `0x08` | full range: the sender's renderer takes samples across the whole of their depth |
| 4 | `0x10` | 10-bit, which implies HEVC |

**Bit 2 is not 10-bit**, and reading it as such is a mistake that has been made.

**Bit 3 is full range** (*corrected 2026-09-24*; this table called it a base flag, set on every
offer and meaning nothing). A peer sets it when its renderer converts with the stream's own
range, and a host may then code the samples across the whole of their depth, saying so in the
parameter set's range flag. It is a preference like bits 1 and 4, met through the room's
intersection, so one peer that cannot draw the full range keeps the room in the video range.
Current established clients set it in every declaration, which is why `_flags` of 8 alone is
their ordinary case -- H.264, 8-bit, 4:2:0, full range -- and older ones never set it.
Measured against an established host on the vendor's encoder, the same picture minutes apart:
declared, the full range, 8 percent of luma below 16; clear, the video range, 0.2 percent.
Not every host acts on it, and a host whose build fails takes it off after full chroma and
before the second codec.

**Both "implies HEVC" notes are the sender's own rule and the receiver enforces it too**, so
neither is advisory. A host that codes what these name promotes the codec with them rather than
honouring one and not the other; a peer asking for depth on H.264 is asking for something no
hardware produces.

**Both bits are read as preferences, and a host that cannot meet one degrades** (*corrected
2026-09-19*; this paragraph said from 2026-08-30 that 4:4:4 was refused outright, which was
true for one day). A guest declaring bit 4 gets a ten-bit stream where the built encoder can
produce one, and bit 1 full chroma where it can and the host's census allows it
([05 §6.1](05-host.md)); where it cannot, the axis is taken off and the stream carries on,
because a declaration is what the peer would like and not what it requires: an established
client follows whatever arrives, and a client of ours declares only what its decoder was
verified to decode ([10 §7](10-client.md)), so what arrives is always within what was
declared. The earlier reasoning -- that a peer builds one decoder from its declaration and
fails every picture on anything else -- described a peer that does not exist.

## §12 Session lifecycle

1. Signaling exchanges credentials and candidates ([04-signaling.md](04-signaling.md)).
2. Connectivity checks run on the shared socket (§2,
   [03-connectivity.md](03-connectivity.md)).
3. The media context is constructed on the punched socket with the key from §4.
4. The connecting side sends opcode 11 within 5 seconds or the attempt is abandoned.
5. The host acknowledges by beginning the video stream, then streams until either side sends
   opcode 10 or liveness expires (§9).

## §13 Constants

| Name | Value | Note |
|---|---|---|
| envelope size | 29 | §3 |
| data header size | 7 | §5.1 |
| group ack size | 3 + 4 + 4 x channels | §5.2, **not fixed**: the count is the sender's channel count, so 23 and 83 are both valid |
| message length prefix | 4 | §5.3, big endian, first fragment only |
| body capacity per fragment | 1229 - 36 = 1193 at the default | §5.3, tracks the datagram size |
| channel count | 19 | §6 |
| outstanding fragment cap | 100 | §9, also the congestion window floor |
| retransmission floor | 50 ms | §9 |
| retransmission ceiling | 1000 ms | §9 |
| retransmission grace | 30 ms | §9, added after the clamp |
| peer ring depth | 1500 to 4000 | §7, **generation dependent**; assume 1500 of an unidentified peer |
| peer slot payload capacity | 1232 to 3000 | §7, **generation dependent** |
| datagram size, floor and default | 1229 | §8, yields 1193 payload |
| datagram size, emission ceiling | 2000 | §8, MUST NOT exceed |
| datagram size, universally accepted | 1229 | §8, the newest peers receive no more |
| direct path clamp | 1472 | §8 |
| ack cadence | 30 ms | §9 |
| soft liveness timeout | 60 s | §9 |
| hard liveness timeout | 120 s | §9 |
| delivery deadline | 15 s | §9 |
| congestion window floor | 100 | §10 |
| browser: association port | 5000 | §14, both sides |
| browser: payload protocol identifier | 53 | §14, binary; the only one accepted |
| browser: record layer datagram | 1229 | §14, the native default |
| browser: association packet | 1191 | §14, the datagram less the record overhead of 37 |
| browser: largest message | 4 MiB | §14 |
| browser: send queue per stream | 4000 fragments | §14, a refusal coincides with the gate's |
| browser: control silence read as dead by a page | 5 s | §14, the reason for the two-second cadence in §11.2 |

## §14 The browser transport

A browser cannot open a socket, so it is served over the transport its own data channels
speak: a datagram transport layer security session (version 1.2) on the attempt socket, and
inside it a stream control transmission association whose streams carry the channels. Both
are standard; what this section fixes is the mapping, which follows the browser client that
already exists and is therefore not a matter of choice ([00 D13](00-overview.md)).

### §14.1 Roles and trust

**The host is the client of the handshake** and sends the first flight the moment the path
exists; the browser answers as the server. A browser's description therefore names the host
as the active side. Each side presents a self-signed certificate and the only trust is the
digest: the credential exchange carries each side's SHA-256 digest with the hash name in
front (`sha-256 AA:BB:...`, uppercase pairs joined by colons), and a handshake whose peer
certificate does not digest to the value signaled is a fault that ends the attempt. The host's
certificate is one per process, P-256, with no extensions; it is a container for a key and
nothing a browser might refuse ([04 §4](04-signaling.md)).

The record layer is bounded to the native datagram size, 1229 bytes, and the association's
packet to 1191, which is that less the 37 bytes a record costs. There is no path probing on
this stack (§8 does not apply); the first size is the only size.

### §14.2 Streams

The association listens on port 5000 on both sides. **The stream number is the channel
number**, and the streams are agreed in advance: a browser opens them as negotiated with the
channel number as the identifier, and the host never receives or answers an in-band open.
Only channels 0, 1 and 2 are served.

| Stream | Carries | Reliability | Priority |
|---|---|---|---|
| 0 | control messages (§11) | reliable, ordered | highest |
| 1 | video | reliable, ordered | lowest |
| 2 | audio | reliable, ordered | between |

**Every stream is reliable and ordered**, as every native channel is (§6), and the drop
that keeps sound from arriving late happens in the same place on both stacks: before a
message is queued, never after ([05 §9.2](05-host.md)). What differs is who retransmits --
the association, on its own timers, rather than the acknowledgements of §9 -- and that the
streams are interleaved, so a large picture in flight holds neither control nor sound; video
shares a head-of-line with nothing but itself.

### §14.3 Messages

**One protocol message is one association message.** There is no length prefix -- that
prefix is a property of the native rings (§5.3), not of the protocol -- and the only payload
protocol identifier accepted is 53, the binary one; a message under any other identifier is
dropped and counted. A message larger than 4 MiB is refused at the sender, and the send queue
per stream holds 4000 fragments' worth, chosen so that a refusal there coincides with what the
frame gate would refuse anyway ([05 §5](05-host.md)).

**The control channel keeps its 13-byte header; video and audio lose theirs.** A message on
stream 0 is exactly the bytes of §11, header first. A message on stream 1 is the access unit
alone -- the ten-byte video header of §11.3 is not sent -- and a message on stream 2 is the
codec payload alone, without the fifteen-byte audio header of §11.4. So a browser learns
nothing per frame: no dimensions, no rotation, no keyframe flag, no encoder generation, no
sample rate. It reads the codec from its own declaration, the size from the decoded picture,
and whether a unit is a keyframe from the unit types in the bitstream. Anything a host
signals through a media header is invisible on this stack, which is why the encoder
generation and the pointer travel on the control channel and why the H.264 sequence set must
state everything a decoder needs ([05 §4](05-host.md)).

The pointer message (§11.2) never uses its cached form here, because a browser declares no
pointer cache; every update that carries a picture carries the whole picture.

### §14.4 Liveness and pressure

**A page reads five seconds of silence on the control channel as a dead link.** The
encode-latency report of §11.2 goes out every two seconds on the clock from the moment the
path exists, whatever the frame rate, so a still desktop does not look dead. The host's own
liveness on this stack is progress: an authenticated data record arriving, or a message the
association reports delivered, stamps it, with the same soft and hard timeouts as §9, and a
message sent and not delivered past the delivery deadline of §9 is undeliverable. A closed record layer, or an association the peer aborts, ends the
attempt with a fault the host reports as a failed handshake or an aborted association.

The congestion controller of §10 runs unchanged on this stack; what it reads is synthesized
from the association rather than from the acknowledgements of §9 ([05 §5](05-host.md)).
There is no retransmission timeout count to report, because the association retransmits on
its own timers, so that figure reads zero here.

### §14.5 What a page must do

The stack is symmetric enough that the requirements on a browser page are short:

1. Send the offer with the transport field set to 2 and the triple its browser generated;
   apply the host's triple as its answer, naming the host as the active side of the handshake
   and stating the port and the message ceiling above.
2. Add the host's candidates only after that answer is set -- one browser family refuses a
   candidate offered before the remote description exists -- and send its own as it gathers
   them, followed by the readiness marker ([03 §3](03-connectivity.md)); without the marker
   the host checks only directly routable addresses.
3. Send opcode 11 on channel 0 as soon as the channel opens, with the eight keys of §11.5 and
   the flags reduced to what it can decode.
4. Feed each message on stream 1 to its decoder as one access unit, marking keyframes from
   the bitstream; ask for a fresh reference chain with opcode 13 (same flags, third argument
   set) rather than replaying a picture it saw earlier.
5. Treat five seconds without a control message as the end of the session; leave with
   opcode 10 and a zero status.

`examples/web-client` is the smallest page that does all five.
