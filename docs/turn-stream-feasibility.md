# BUD through TURN over TCP/TLS: source feasibility review

Reviewed upstream `feat/win-seams` at
`8674882319b24023547bbed77821c472566af69b` on 2026-10-16.
This is a design investigation, not an implemented or tested transport.

## Result

Feasible without changing the BUD wire format or the peer's implementation.
Use a UDP allocation (`REQUESTED-TRANSPORT = 17`) carried over a client-to-TURN
TCP or TLS connection. The TURN-to-peer leg remains UDP (RFC 8656 section 3.1).
This is not an RFC 6062 TCP allocation.

## Existing seams

- `crates/core/src/turn.rs`: Allocate, authentication and TURN message encoders;
  response, Data indication and ChannelData parsers. Complete messages can be
  reused after a stream framer restores boundaries. ChannelData accepts padding.
- `crates/core/src/relay.rs`: allocation, permission and channel leases, stale
  nonce handling, release, and deadlines. This is sans-IO and should remain so.
- `crates/core/src/endpoint.rs`: wraps outbound BUD/checks in TURN and unwraps
  inbound messages. Payload crypto and media channels need no stream-specific
  modification.
- `crates/net/src/shell.rs`: owns the event loop but currently concretely owns
  platform UDP IO and `send::Batch`. A stream implementation belongs here/below.
- `crates/net/src/linux/io.rs`: datagram batching and readiness wait.
- `crates/net/src/windows/io.rs`: posted datagram receives through IOCP.
- `crates/client/src/config.rs`: Relay currently carries a resolved SocketAddr
  and redacted/zeroized credentials; no transport or TLS hostname.
- `crates/client/src/shell.rs`: creates the relay endpoint and concrete shell.
- `crates/sdk/src/abi/client.rs` and `include/lowlat.h`: size-versioned client
  config. Any C API transport/hostname additions must preserve older callers.

## Proposed contribution boundary

1. Add explicit UDP/TCP/TLS client-to-relay selection, default UDP, with the
   original hostname retained for TLS identity verification independently of
   the chosen destination IP. Keep retry/escalation policy in the application.
2. Introduce a small IO seam with the current UDP implementation unchanged.
   A relay stream implementation yields complete TURN messages to the same
   endpoint and accepts complete outbound TURN messages. It must not pass BUD
   datagrams to an unframed TCP socket.
3. Frame STUN as 20-byte header plus message length, and ChannelData as four
   bytes plus payload padded to four bytes (RFC 8656 section 12.5). Handle
   fragmented/coalesced reads, partial writes, bounded storage, EOF and malformed
   lengths. Retain partially written frames until completed or close the stream;
   never discard a suffix and continue with the next frame.
4. Drive nonblocking TCP/TLS readiness from the existing single session thread.
   A TLS engine need not introduce Tokio: rustls can be driven by the shell.
   Its inclusion in the native SDK requires an explicit documented dependency
   decision: current workspace policy confines the TLS stack to signaling.
   No permissive verifier; validate server identity/chain and use supported
   public/platform trust. Isolate TLS support as a feature where appropriate.
5. Add reliable TURN transaction policy to the core without IO: don't resend
   TURN requests with UDP retransmission timers over TCP/TLS; retain bounded
   transaction failure deadlines, lease renewals and stale-nonce new requests.
   BUD retransmission remains necessary for the relay-to-host UDP leg. Reliable
   TURN control does not make end-to-end media reliable.
6. Keep queues bounded and stop/fail explicitly on prolonged backpressure.
   TCP write acceptance is not proof of peer delivery. Account for stream
   buffering when reporting send timestamps/rates and avoid unlimited old media
   behind a stalled socket. Keep read/write progress fair with protocol timers.
7. Separate dial/TLS setup deadlines from the existing five-second allocation
   deadline. Make cancellation wake every connection/handshake/wait state and
   release resources on both failure and a normal departure.

## What can transfer from ParsecWebTurn

`src-native/vendor/webrtc-ice-0.14.0/src/turn_stream.rs` supplies a working RFC
framing, TLS hostname validation, TCP_NODELAY and teardown reference. Its
Tokio/async util::Conn integration should not be transplanted into lowlat's
single-threaded synchronous shell. There is no need to import the WebRTC/ICE/
SCTP stack or a second TURN allocation engine: retain lowlat's existing engine.
ParsecWebTurn's coturn fixtures are test designs to adapt, not proof that a
lowlat BUD/TCP implementation already works.

## Acceptance tests before a usable PR

- Framer: every split point, multiple messages per read, STUN/ChannelData
  interleaving, all padding lengths, invalid/oversized headers, EOF at every
  partial frame boundary, short writes and backpressure.
- Reliable-control fake-clock cases: no UDP retransmits, bounded unanswered
  transactions, 401 authentication, 438 nonce rotation, permission/channel/
  allocation renewal, refusal and normal zero-lifetime release.
- TLS: trusted server succeeds; unknown CA, wrong name and expired certificate
  fail; cancellation during connect/handshake and a stalled peer terminate.
- Isolated coturn with a real UDP BUD peer: UDP/TCP/TLS relay modes, bidirectional
  authenticated media/control rather than allocation success alone. Block client
  UDP to prove the stream path; keep the host's UDP access to TURN available.
- Windows and Linux lifecycle/burst tests, bounded memory and teardown churn;
  run beyond permission and channel renewal boundaries, including nonce changes.
- Direct and UDP TURN regression tests. A final stock Parsec-host live session
  is separate interoperability evidence, not replaced by a synthetic peer.

## Performance limits

No decoder, GPU texture, renderer or video-copy changes are required. TLS does
add network encryption and stream framing can need CPU-side network buffers.
TCP head-of-line blocking remains: BUD cannot bypass a missing TCP segment on
the client-to-relay leg. It can still handle loss on the relay-to-host UDP leg.
TLS/443 is not HTTPS and does not guarantee traversal through an HTTP-only proxy
or a firewall requiring application-level HTTPS. Prefer UDP when it is usable;
TCP/TLS primarily extends reachability on restricted networks.

## Primary references

- RFC 8656: https://www.rfc-editor.org/rfc/rfc8656.html
- RFC 8489 section 6.2.2: https://www.rfc-editor.org/rfc/rfc8489.txt
- Upstream connectivity section 7.4 and IO shell design at the pinned revision.

The review preceded implementation. Development is now in the
`vampywiz17/lowlat` fork on `codex/turn-stream-transport`; see
[development status](turn-stream-development.md) for what has actually landed.
ParsecWebTurn application source remains unchanged.
