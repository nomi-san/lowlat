# 04 - Signaling

**Status:** locked 2026-08-15. Implemented by `lowlat-kessel`, which **the SDK does not
link**.

Signaling is how two endpoints that cannot yet reach each other exchange credentials and
candidates. It is a control plane, not a data plane: it carries no media, it is not on any hot
path, and it stops mattering the moment connectivity completes.

## §1 Ownership

**Signaling is application owned.** The SDK contains no transport, no TLS, no JSON, and no
HTTP. `lowlat-kessel` is a separate crate that the daemon links and that an integrator may
ignore entirely.

Three reasons, in order of weight:

1. **A shared library must not start a runtime inside someone else's process.** The public
   surface is a C ABI loaded into arbitrary applications ([06-api.md](06-api.md)). Bringing an
   async executor, a TLS stack, and a JSON parser across that boundary is an imposition, and
   it is the single largest source of dependency conflict for an embedded SDK.
2. **Integrators already have this.** An application that reaches the service has an
   authenticated session, a user identity, and a transport. Making it hand those to us so we
   can duplicate the work is worse for everyone. A C# host brings its own client and never
   sees this crate.
3. **It keeps the escalation decision where the information is.** Retrying with a relay
   requires a fresh candidate exchange, which requires signaling. An SDK that escalated
   internally would have to reach back through a seam it does not own
   ([03 §8](03-connectivity.md)).

The consequence is that this document describes two things that must not be confused: the
**protocol** an application must speak to the service, and the **seam** between any signaling
implementation and the SDK. Only the second is normative for the SDK.

## §2 Transport and authentication

A persistent bidirectional connection to the service, authenticated at connect time with:

| Field | Meaning |
|---|---|
| session identifier | the account session |
| role | host or client |
| version, build | application identity |
| SDK version | protocol generation |

The role determines message direction: a host receives offers and sends answers, a client does
the reverse.

Liveness is the connection itself. The service treats a dropped connection as the host going
away, which is why §6 forbids a heartbeat.

Reconnection uses bounded exponential backoff with jitter. A reconnect re-sends the
advertisement (§6) but does not resurrect in-flight attempts, which the peer has already
abandoned.

## §3 Message set

Every message is `{ version, action, payload }`.

| Action | Direction | Meaning |
|---|---|---|
| `conn_update` | host to service | advertisement, §6 |
| `offer` | client to service | request a session |
| `offer_relay` | service to host | the offer, with the requester's identity attached |
| `offer_cancel` | client to service | withdraw |
| `offer_cancel_relay` | service to host | withdrawal |
| `answer` | host to service | approve or reject, with credentials |
| `answer_relay` | service to client | the answer |
| `candex` | either to service | a candidate |
| `candex_relay` | service to peer | the candidate |
| `close` | either | terminate, with a reason |

The `_relay` suffix means service-forwarded, not media-relayed. The two senses of the word
are unrelated and the distinction matters when reading logs.

**Candidates trickle.** They are sent as discovered and forwarded immediately, in both
directions, before and after the answer. An implementation that batches them until gathering
completes adds its slowest interface to every connection's setup time.

**A candidate marked `sync` is a readiness signal, not an address.** Each side sends one, and
the address on it is ignored by the receiver -- one implementation sends the literal
`1.2.3.4:1234`. Two rules follow, and both are silent when broken:

- **Never add one to the candidate table.** Doing so spends connectivity checks on whatever
  unrelated host the placeholder names.
- **Always send one.** A peer is entitled to withhold every real candidate until it has both
  the answer and a `sync` marker, and at least one does. Against that peer, an endpoint that
  never sends one negotiates successfully and then has nothing to check.

The two remaining flags describe where a candidate came from and map onto the standard
candidate types: `from_stun` is a server-reflexive candidate, `lan` is a host candidate.

The relayed forms carry what the direct forms cannot: the sender's identity, ownership,
whether approval may be skipped, and the requested permissions. The host reads permissions
from the relayed offer and never from the client.

## §4 Credentials

Carried in the offer and the answer:

| Field | Required | Meaning |
|---|---|---|
| `ice_ufrag` | yes | binding check username fragment |
| `ice_pwd` | yes | binding check password |
| `fingerprint` | yes | certificate fingerprint |
| `aes256` | no | media key; presence also selects the cipher |

**The key travels over the signaling transport's TLS and nowhere else.** It is never logged,
never persisted, and zeroized on session teardown.

`aes256` is the switch described in [01 §4](01-protocol.md): present selects the 256-bit
cipher, absent selects the 128-bit legacy path with the fingerprint. Both are implemented, the
credential decides, and there is no negotiation.

**Host credentials are generated at approval, not at registration.** They are bound to the
socket that was just opened for the attempt, so generating them earlier binds them to nothing
and generating them per-attempt-registration leaks state for attempts that are never approved.

## §5 Version negotiation

Every offer, answer, and candidate carries a version block:

```
ver_data: 1
versions: { bud, control, p2p, audio, init, video }
```

Six independently versioned subprotocols. This is the mechanism behind
[00-overview.md](00-overview.md) D1: framing and opcodes are additive, so a peer selects paths
based on what the other advertises, and old and new interoperate in both directions.

**Advertise only what is implemented, on each axis independently.** The number is a promise
the peer will hold us to. Advertising a higher version than we implement asks the peer to use
framing we cannot parse; advertising lower than we implement asks a current peer to fall back
unnecessarily. Both are wrong, and the second is the more tempting mistake because it looks
conservative.

`ver_data` must be non-zero on candidate messages or the peer rejects them.

## §6 Host advertisement

`conn_update` publishes the host into the service's discovery listing. Without it the host
exists but cannot be found.

Payload: device identity, display name and description, platform and version identity, mode,
visibility, an optional secret, capacity, current occupancy, and the connected guest list with
each guest's permissions.

**It is emitted on state change only. Never on a timer.** The service derives liveness from
the connection, so a periodic advertisement adds load and buys nothing. Emit on:

- connection established, so the host appears immediately
- guest connected or disconnected, updating occupancy and the guest list
- any advertised field changed by the application

The emission is driven by a dirty flag rather than by a schedule: something marks the
advertisement stale, the host loop publishes it and clears the mark. Nothing in the reference
publishes on a timer, and a capture that appears to show a ten second cadence is showing the
application above the SDK polling, not the SDK itself.

**Open, and testable by the listing rather than by argument:** whether a host that advertises
once and then goes quiet stays discoverable indefinitely. The document asserts it does. That
has not been observed over a long connection, and the service marking a host online when an
advertisement arrives is not evidence about what happens when one never arrives again.

A separate greeting frame goes out once when the connection opens. It appears exactly once per
session, so it is not a keepalive either.

**Advertised capacity is read from the configured guest limit** ([00-overview.md](00-overview.md)
D10), never a constant. A listing that promises more capacity than admission will grant is a
listing that lies to users.

## §7 Attempt lifecycle

```
client                     service                      host
  |-- offer ---------------->|                            |
  |                          |-- offer_relay ------------>|   new_attempt()
  |                          |                            |   (application decides)
  |                          |<------------- answer ------|   begin_p2p() -> credentials
  |<-- answer_relay ---------|                            |
  |                          |                            |
  |-- candex --------------->|-- candex_relay ----------->|   add_candidate()
  |<-- candex_relay ---------|<------------- candex ------|   (host candidates, as found)
  |                          |                            |
  |========== connectivity checks, then media ============|
```

An attempt is identified end to end by an identifier the client mints. Every subsequent
message references it. Unknown identifiers are dropped silently, since they are races with
teardown rather than errors.

**Admission is the application's decision.** The SDK registers the attempt and reports it; it
applies no policy of its own beyond capacity. Authentication, allow lists, and interactive
approval all live above the seam.

## §8 Outcomes

Failures are typed, and the mapping to application behavior is the whole point of typing them.

| Outcome | Meaning | Correct response |
|---|---|---|
| peer gone | the other side vanished mid-negotiation | abandon, inform |
| no permission | rejected by policy or the service | do not retry |
| host not found | not in the listing, or offline | refresh discovery |
| connectivity failed | negotiated, no path found | retry with mapping or relay |

Only the last justifies escalation ([03 §8](03-connectivity.md)). A generic timeout would
collapse all four into one and produce retry behavior that is wrong three times out of four.

**Silence is not a refusal, and every offer must be answered.** A declined answer is a wire
event the peer acts on at once. No answer at all is not a slower refusal: nothing in the
protocol reports a host that never replied, so the peer stays in its connecting state
indefinitely and neither side surfaces a reason. A host that means no must say no.

The corollary for anything above the seam: **"still waiting for the host" is not a protocol
outcome and has to be timed by whoever needs it.** There is no message for it in either
direction.

## §9 The SDK seam

**This section is normative for the SDK. Everything above is not.** Any signaling
implementation reduces to four calls and an event queue.

| Call | When | Effect |
|---|---|---|
| `new_attempt` | an offer arrived | register the attempt with the peer's credentials, permissions, and identity; state becomes waiting |
| `add_candidate` | a candidate arrived | inject it; unknown attempts no-op |
| `begin_p2p` | the application approves | bind the socket, start connectivity, **return host credentials** for the answer |
| `end_connection` | rejection or disconnect | tear down, addressed by attempt identifier |

Outbound, the SDK emits events the application forwards: local candidates as they are
gathered, and guest state changes.

The asymmetry is deliberate. `begin_p2p` returns credentials rather than the SDK sending them,
because the SDK has no transport. Approval and rejection are the same call shape so the
application's policy code has one path.

This seam is the same shape on both sides, so a client implementation built on the same core
uses the mirror image of it.

## §10 Self-hosted signaling

Nothing in the SDK requires the public service. The seam in §9 is transport agnostic, and a
minimal relay that forwards offers, answers, and candidates between two authenticated parties
is enough for a private deployment.

`packages/signaling` will carry a reference implementation: relay only, with authentication
hooks left as extension points and no policy of its own. It is a template, not a product.

Self-hosted deployments lose only discovery. Direct connection by identifier works unchanged.

## §11 Verification status

Per [AGENTS.md](../AGENTS.md) §14.

**Confirmed:** the message set and its direction rules; credential fields and the cipher
switch; the six-axis version block and the non-zero `ver_data` requirement on candidates; the
advertisement payload shape and field ordering; credentials generated at approval.

**Carried, pending re-verification before Phase 4 closes:** the authoritative advertisement
field ordering under a strict service parser; reconnection backoff expectations; whether the
service imposes any rate limit on candidate messages.

**Ours by design:** §10 in full.
