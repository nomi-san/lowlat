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

## §6 Channels

19 channels, index 0 to 18, each an independent reliable ordered stream with its own sequence
space, rings, and cumulative acknowledgement.

| Channel | Use |
|---|---|
| 0 | control and input |
| 1 | video, stream 0 |
| 2 | audio |
| 3 and above | additional streams and application data |

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

Messages larger than one payload are fragmented across consecutive sequence numbers on the
channel, with `0x20` marking the final fragment. Reassembly walks forward from the base.

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
- **Retransmission timeout** is derived from the round trip estimate scaled and offset by the
  active congestion level (§10).
- **Negative acknowledgement** (`0x10` with `0x02`) names a specific gap and triggers fast
  retransmission without waiting for the timeout.
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

| Level | Threshold | Retransmit timing | Notes |
|---|---|---|---|
| 0 | 0.0 | legacy | Fires on any stale packet once the window exceeds 100. This is the most aggressive setting, not a disabled one. Do not use it as a fallback for an out-of-range value. |
| 1 | 0.15 | `rtt * 1.1 + 20 ms` | Default. Quick to retransmit. |
| 2 | 0.35 | `rtt * 1.5 + 50 ms` | Allows more time for acknowledgement. |

The resulting rate actuates the **encoder bitrate** through a live reconfigure. It does not
pace the socket. The reconfigure MUST NOT reinitialize the encoder and MUST NOT force a
keyframe.

Throughput is measured over the interval between ticks and requires **fractional millisecond**
resolution. Quantizing the interval to whole milliseconds silently skips the peak update
whenever it rounds to zero.

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
| 4 | gamepad button | pad, button, pressed | deferred |
| 5 | gamepad axis | pad, axis, value | deferred |
| 6 | gamepad unplug | 0, 0, pad | deferred |
| 11 | init | header plus JSON body | v1 |
| 13 | encoder configuration | stream, flags, reinit | v1 |
| 17 | user data | length, sub-id, 0, plus body | v1, opaque pass through |
| 23 | gamepad state | 28-byte body | deferred |
| 24 | release all input | 0, 0, 0 | v1 |
| 26 | mouse motion, stream 1 and above | packed | v1 |
| 30 | pen and touch | packed | deferred |

### §11.2 Sent by the host

| Op | Name | Arguments | Status |
|---|---|---|---|
| 9 | cursor update | 21-byte fixed body plus optional PNG image | v1 |
| 10 | disconnect | status, 0, 0 | v1 |
| 16 | input blocked | 1 blocked, 0 unblocked | later |
| 17 | user data | length, sub-id, 0, plus body | v1 |
| 20 | rumble | pad, large motor, small motor | deferred |
| 21 | encode latency | 0, microseconds, stream | v1 |
| 25 | guest list | JSON body | later |
| 28 | host mode | mode | later |

Cursor images on the wire are **PNG, not raw pixels**. Cursor position is in stream space and
requires the host-to-client transform, including a width and height swap on rotated displays.

### §11.3 Session initialization

The connecting side sends opcode 11 with a JSON body declaring its preferences: maximum
resolution, codec capability, colour mode, and feature flags. The host is authoritative and
may ignore any of it (D8).

Codec selection is carried in two places and **both are required**. The capability bit in the
init flags declares support; opcode 13 argument 1 must also carry the video flags. Setting
only one produces a stream the peer will not decode. Colour depth uses a base flag that is
always set, with an additional bit selecting 10-bit. 10-bit and 4:4:4 are reserved and unused
in v1 (D7).

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
| channel count | 19 | §6 |
| peer ring depth | 4000 | §7, bounds the send window |
| peer slot payload capacity | 2000 | §7 |
| datagram size, floor and default | 1229 | §8, yields 1193 payload |
| datagram size, absolute ceiling | 2000 | §8, MUST NOT exceed |
| direct path clamp | 1472 | §8 |
| ack cadence | 30 ms | §9 |
| soft liveness timeout | 60 s | §9 |
| hard liveness timeout | 120 s | §9 |
| congestion window floor | 100 | §10 |
