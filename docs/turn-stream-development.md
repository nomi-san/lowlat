# TURN stream development status

Fork: https://github.com/vampywiz17/lowlat

Branch: `codex/turn-stream-transport`, based on upstream `feat/win-seams`
`8674882319b24023547bbed77821c472566af69b`.

This is work in progress. TCP/TLS relay is not yet a selectable client path.
No stock-host/TURN interoperability or end-to-end latency result is claimed.

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

1. Selectable, feature-isolated TCP/TLS IO under the native shell, driven by
   nonblocking socket readiness and the same application/teardown wake. Preserve
   one merged network thread and keep the UDP offload/IOCP paths unchanged.
2. TLS needs a retained server hostname, a validated trust chain, bounded
   handshake buffers and cancellation while connecting/handshaking. The SDK's
   current no-TLS-dependency rule needs an explicit optional-feature exception;
   do not import an async runtime or the WebRTC/ICE stack for this.
3. Client config and size-versioned SDK ABI expose transport selection with
   old callers defaulting to UDP. Report the actual established transport.
4. Bound reads/writes and queued age, preserve frame boundaries on backpressure,
   and separate socket/TLS acceptance from delivery and its telemetry.
5. Verify authenticated bidirectional BUD through isolated coturn UDP/TCP/TLS,
   including blocked client UDP, renewal/nonce changes and teardown. Exercise
   Windows and Linux, then a stock Parsec host. Only after that claim a usable
   transport and prepare the full upstream contribution.

The codec, GPU decoder/renderer, BUD wire format, peer UDP allocation type,
application retry policy and ParsecWebTurn WebRTC path are outside these changes.
