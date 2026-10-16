# TURN stream development status

Fork: https://github.com/vampywiz17/lowlat

Branch: `codex/turn-stream-transport`, based on upstream `feat/win-seams`
`8674882319b24023547bbed77821c472566af69b`.

This is work in progress. TCP/TLS relay is selectable with the optional
`turn-stream` feature, in the Rust client and the size-gated minor-29 C SDK.
No stock Parsec-host playback or end-to-end latency result is claimed.

## Implemented foundations

- A no_std, allocation-free TURN stream framer in `lowlat-core::turn::stream`.
  Storage is caller supplied and explicitly bounded. Partial/coalesced input,
  ChannelData padding, STUN cookie/length checks and truncated EOF are covered.
  Messages are borrowed from the storage; padding is never exposed as payload.
- `lowlat-net::turn_stream::Output` owns one bounded reusable frame buffer.
  Partial writes retain their suffix; WouldBlock does not discard a frame;
  write-zero and hard errors poison the output. No sleeps, blocking flushes,
  timer batching, extra thread or per-message allocation are introduced.
- Write progress distinguishes a fairness yield from actual socket blocking.
  An edge-triggered caller must schedule a continuation after a yield, rather
  than wait forever on a socket whose writable edge has already fired.
- `Relay::new_on(..., Transport::Reliable)` suppresses TURN request
  retransmissions, while keeping authentication/nonce retries as new
  transactions and bounded transaction/lease deadlines. The existing constructor
  and its UDP retransmission/renewal policy remain unchanged. BUD retransmission
  is unaffected. The RFC 8489 reliable transaction default is 39.5 seconds;
  the existing five-second overall relay setup deadline still applies.
- Fake-clock tests cover reliable setup expiry, authentication, nonce rotation,
  unused-permission expiry without hot polling and failed refresh. Existing
  core tests continue to exercise the original UDP mode.
- A real loopback TCP framing roundtrip and counting-allocator test cover
  repeated encode/decode/output with no hot-path allocations.
- Focused Windows/Linux CI runs formatting, strict Clippy, all core tests and
  stream output tests. These do not substitute for coturn/BUD interoperability.

## Next integration boundary

Implemented in the second stage:

- Nonblocking `mio` TCP and `rustls` TLS on the existing merged network thread;
  no async runtime or extra network worker. The same wake cancels setup and
  wakes application input. Direct/UDP retain their platform IO implementations.
- TCP_NODELAY, bounded work per pass and explicit continuations for already
  readable/writable work. Complete inbound messages are borrowed from owned
  decoder storage without a second packet-sized copy.
- TLS hostname and chain validation using public plus OS-installed roots.
  Explicit private roots are available in the Rust transport API. TLS output
  buffering is limited to 32 KiB, separate from the one pending TURN frame.
  The framing/output allocation test does not claim rustls never allocates.
- Ten-second cancellable dial/handshake before the protocol clock starts.
  Pending-frame age and lack of physical write progress are bounded to five
  seconds; failure closes the transport rather than dropping a frame suffix.
- Rust RelayTransport selection and append-only minor-29 C config/status fields,
  complete-field size gates, UDP defaults, and unsupported-mode rejection.
- Windows/Linux TLS burst, untrusted CA, wrong-name, expired-certificate,
  teardown, EOF delivery and write-age tests, plus client-only SDK tests and
  a generated-header check.

The isolated Linux coturn fixture has passed authenticated bidirectional BUD
over UDP, TCP and TLS, with client UDP blocked for TCP/TLS. Each carrier survived
65 seconds against a 60-second allocation cap, with the server configured for
ten-second nonces, and delivered data after renewal. The extended fixture also
checks fragmented opaque video/audio bodies; it is not codec playback. A stock
Parsec-host live session, long permission/channel renewals and performance
measurements remain follow-up work. See `scripts/test-turn-stream.sh`.

Build the optional client SDK:

```sh
cargo build -p lowlat-sdk --no-default-features --features client,turn-stream --release
```

For the C API, fill the existing relay address/credentials, set `size` to the
current structure size, choose `relay_transport`, and set `relay_tls_server_name`
for TLS. In Rust, change `config::Relay::transport` from its Udp default. The
application still chooses when to retry or escalate; no automatic ladder was
added. TLS/443 is TURN, not HTTPS or an HTTP proxy tunnel.

The codec, GPU decoder/renderer, BUD wire format, peer UDP allocation type,
application retry policy and ParsecWebTurn WebRTC path are outside these changes.
