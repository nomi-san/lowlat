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
            +- group ack      83 bytes fixed           (§5.2)
                 +- channel stream, reassembled per §7
                      +- control message  13-byte header + body (§11)
```

Two transports share one socket for the lifetime of a session: connectivity checks and
encrypted media. There is no separate control connection.

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

The AEAD nonce is 12 bytes: a 4-byte zero prefix followed by the 8-byte big-endian counter
from envelope offset 3. Nonces are **derived, never generated**, which is why the protocol core
requires no random number generator and remains deterministic under replay and simulation
(D4).

There is no associated data. The envelope header is not authenticated.

The counter MUST increase monotonically per sender for the life of a session. A session that
would wrap it is torn down rather than reusing a nonce.

### §4.1 The implementation is chosen at runtime, not at build time

Two implementations of the same construction ship: a portable one, and one built on the x86-64
AES and carry-less multiply instructions. The second makes a single pass over the packet and
reduces the authentication accumulator once per eight blocks rather than once per block, which
is the whole of the difference.

**Which one runs is decided when the session key is installed, from what the processor
reports.** It is not a build flag and must not become one. A binary compiled for a processor
with these instructions will not start on one without them, and a host does not choose the
machine it is installed on. The probe is a CPUID read: both features are plain instruction-set
additions carrying no processor state, so there is no operating system handshake to consult and
the CPUID bit is the entire answer.

**The two are interchangeable by construction and are tested to be**, because anything less is
a session a peer cannot decrypt. Identical ciphertext and identical tag for the same key,
nonce and message; interoperable in both directions; checked at every length boundary the
group size creates, including empty, partial, and one past a group. A peer cannot tell which
one a host used, and a host that falls back is slower and nothing else.

Measured on a 2K120 session at 40 Mbps with four guests: sealing costs 7.6 CPU-seconds per half
hour against 20.5, and 8.8 microseconds per frame per guest against 23.7. The portable path
was never the constraint and this is not the difference between working and not working. It is
the packetize stage's share of the frame budget ([05 §10](05-host.md)), and it is headroom that
a keyframe burst spends.

*Lesson: the block counter goes into the last four bytes of the counter block as bytes, not as
a value. Writing the machine integer instead reverses them, which round-trips against itself
perfectly, passes every self-consistent test, and cannot be opened by any other implementation.
Published vectors are what catch it.*

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

Fixed 83 bytes. One packet acknowledges every channel at once, which is why there is no
per-channel ack traffic.

| Offset | Size | Field |
|---|---|---|
| 0 | 1 | marker `0x01` |
| 1 | 1 | flags, `0x02` optionally with `0x10` or `0x08` |
| 2 | 1 | triggering channel |
| 3 | 4 | triggering sequence, big endian |
| 7 | 76 | 19 cumulative acknowledgements, big endian, one per channel |

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
`seq mod ring_depth`, so the ring is a direct-mapped window rather than a queue.

**Peer ring depth is 4000 slots per channel per direction.** This is a protocol constant, not
an implementation choice, because the peer indexes by `seq mod 4000`. A sender that gets more
than 4000 sequence numbers ahead of the peer's cumulative acknowledgement wraps onto occupied
slots and destroys data that was already delivered. **The send window MUST never exceed 4000
outstanding sequence numbers on a channel.**

**Peer slot payload capacity is 2000 bytes.** Combined with §8 this is satisfied by
construction, but an implementation that raises the MTU without reading §8 will overrun it.

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

| Quantity | Relation | Default | Ceiling |
|---|---|---|---|
| datagram (UDP payload) | `M` | 1229 | 2000 |
| plaintext | `M - 29` | 1200 | 1971 |
| payload | `M - 36` | 1193 | 1964 |

On IPv4 the on-wire IP packet is `M + 28`, so the default occupies 1257 bytes and a 1500-byte
path allows `M` up to 1472.

**Default and floor: a 1229-byte datagram.** Every peer accepts this and it survives PPPoE,
tunnels, and relay framing.

**Absolute ceiling: a 2000-byte datagram.** Implementations MUST NOT emit more under any
circumstance, including after a successful probe. Peers are not required to accept more, and a
peer that cannot will discard the entire datagram rather than truncating it, so the failure is
total and silent.

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
5. When relayed, subtract the relay framing before clamping: 36 bytes for a data indication,
   4 bytes for channel data.
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

- **Sequence arithmetic is RFC 1982 everywhere.** A naive 32-bit comparison inverts at wrap,
  which arrives in roughly 15 days of continuous high-rate video. Every comparison of
  sequence, base, and cumulative acknowledgement uses signed difference.
- **Acknowledgement cadence:** a group acknowledgement is emitted when 30 ms have elapsed
  since the last one, and immediately on a receive that advances a base or reveals a gap.
- **Round trip estimate** is an exponentially weighted moving average, `rtt = rtt * 0.9 +
  sample * 0.1`, sampled when an acknowledgement clears a slot carrying a send timestamp.
- **Retransmission timeout** is per fragment and exponential in its retry count:

  ```
  rto = clamp(2 * (retransmissions + 1) * srtt, 50 ms, 1000 ms)
  resend when time_since_last_send > rto + 30 ms
  ```

  The 30 ms is a flat grace on top of the clamp, not part of it. Note this is **not** derived
  from the congestion level table; that table serves a different purpose (§10).
- **Negative acknowledgement** (`0x10` with `0x02`) triggers fast retransmission of everything
  below the named sequence, without waiting for the timeout. **Once per fragment per
  acknowledgement**, tracked by a latch on the fragment, so a burst of nacks cannot turn into a
  retransmission storm.
- **Outstanding fragments are capped at 100 per channel.** A sender at the cap does not send; it
  marks the fragment deferred and lets the retransmission scan release it as the window drains.
  The same 100 is the congestion controller's window floor (§10), so it is one constant with
  three consumers and must not be tuned in one place alone.
- **Stall escape:** when a gap cannot be filled and the window is starving, the reader skips
  forward. It MUST jump to the **furthest** occupied slot, never the nearest. Jumping to the
  nearest crawls the flow-control window and has cost a 20x throughput regression.
- **Anti-replay windows MUST NOT be applied to reliable channels.** They reject legitimate
  retransmissions.
- **Liveness:** 60 seconds without progress is a soft failure; 120 seconds is a hard failure.
  Keepalives (`0x08`) hold an idle session open.

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
| 0 | 0.0 | none | Fires on any stale fragment once the window exceeds 100. This is the most aggressive setting, not a disabled one. Do not use it as a fallback for an out-of-range value. |
| 1 | 0.15 | `srtt * 1.1 + 20 ms` | Default. |
| 2 | 0.35 | `srtt * 1.5 + 50 ms` | Tolerates more delay before counting a fragment stale. |

**The staleness threshold is not a retransmission timer** (§9). It classifies an outstanding
fragment as stale for the purpose of the ratio above. A fragment counts as stale if it is older
than the threshold, or if the smoothed round trip has grown past its own budget scaled the same
way, or if it has already been retransmitted, fast-retransmitted, or deferred.

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
per distinct value.

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
| 25 | guest list | JSON body | later |
| 28 | host mode | mode | later |
| 29 | encoder generation | stream, generation, 0 | v1 |
| 34 | frame timing | 0, stream, 0, plus 16-byte body | diagnostic |

Three of these have cadences rather than triggers, and the cadences are counted in frames:
encode latency every 30th frame, the guest list every 120th and only from stream 0.

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

Cursor images on the wire are **PNG, not raw pixels**. Cursor position is in stream space and
requires the host-to-client transform, including a width and height swap on rotated displays.

### §11.3 Video framing

Video is not a control message. It rides the ordinary message framing of §5.3 on its own
channel, with a 10-byte header ahead of the bitstream:

| Offset | Size | Field |
|---|---|---|
| 0 | 4 | frame identifier, **little endian** |
| 4 | 2 | width, little endian |
| 6 | 2 | height, little endian |
| 8 | 1 | reserved, `0x01` |
| 9 | 1 | flags |

Flags: bits 0 to 2 rotation, **bit 3 ten-bit colour**, bit 4 believed to be full screen.

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
like. **We never set it.**

Bit 4, called full screen above, has never been observed set either, and now carries the same
doubt. Nothing turns on it while we leave it clear.

**The frame identifier is not a frame counter.** It is an encoder generation counter and stays
constant across a whole session's frames, incrementing only when the encoder is reconfigured.
It went from 1 to 2 across the same 112 second recording. Anything using it to order or
deduplicate frames is broken.

Within the first fragment's body, and remembering the four-byte length prefix from §5.3, the
absolute offsets are: length at 0, frame identifier at 4, dimensions at 8 and 10, reserved at
12, flags at 13, start code at 14, and the first unit's type byte at 18.

### §11.4 Audio framing

Audio rides channel 2 with its own short header ahead of an Opus packet. The exact layout is
recorded at Phase 10 with the rest of the audio work; it is not on the Phase 1 path.

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
eight in about 124 bytes; another sends around 306. A host reads the keys it knows and ignores
the rest, which is what the "do not add keys" rule above constrains a *client* to, not a host.

**Do not add keys.** Peers exist that behave differently when the object carries more than these
eight, taking different encoder-warmup or session-setup paths, so a host must not require extras
and a client must not send them.

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
| 3 | `0x08` | base flag, **always set** |
| 4 | `0x10` | 10-bit, which implies HEVC |

**Bit 2 is not 10-bit**, and reading it as such is a mistake that has been made. The base flag at
bit 3 is set on every offer, so `_flags` of 8 alone is the ordinary case: H.264, 8-bit, 4:2:0.
10-bit and 4:4:4 are reserved and unused in v1 (D7).

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
| group ack size | 83 | §5.2 |
| message length prefix | 4 | §5.3, big endian, first fragment only |
| body capacity per fragment | 1229 - 36 = 1193 at the default | §5.3, tracks the datagram size |
| channel count | 19 | §6 |
| outstanding fragment cap | 100 | §9, also the congestion window floor |
| retransmission floor | 50 ms | §9 |
| retransmission ceiling | 1000 ms | §9 |
| retransmission grace | 30 ms | §9, added after the clamp |
| peer ring depth | 4000 | §7, bounds the send window |
| peer slot payload capacity | 2000 | §7 |
| datagram size, floor and default | 1229 | §8, yields 1193 payload |
| datagram size, absolute ceiling | 2000 | §8, MUST NOT exceed |
| direct path clamp | 1472 | §8 |
| ack cadence | 30 ms | §9 |
| soft liveness timeout | 60 s | §9 |
| hard liveness timeout | 120 s | §9 |
| congestion window floor | 100 | §10 |
