# 03 - Connectivity

**Status:** locked 2026-08-15. Implemented by `lowlat-core` (state machines) and `lowlat-net`
(sockets), per [00-overview.md](00-overview.md) D4.

Connectivity is inside the sans-IO boundary. Candidates and received packets go in, packets
and events come out, and time is a parameter. This is not an aesthetic choice: the failures in
this area are state-machine failures that a real socket makes almost impossible to reproduce,
and a development network can only produce one topology. See [08-testing.md](08-testing.md).

## §1 Model

This is **not** full ICE, and implementing full ICE would be wrong. The protocol uses
ICE-shaped credentials and a candidate exchange, but the procedure is a direct hole punch with
authenticated binding checks, without ICE's nomination, priority ordering, or role conflict
resolution.

What is actually required:

- A candidate exchange over signaling ([04-signaling.md](04-signaling.md)).
- Username fragment and password credentials, used for binding-check authentication.
- Binding requests and responses on the media socket, demultiplexed from media.
- A punch procedure that opens the local mapping and detects reachability.

What is deliberately absent: candidate pairs with computed priorities, a check list with
frozen and waiting states, nominated pairs, and role conflict handling. The exchange carries a
controlling role that is always the same value, so there is no conflict to resolve.

**The credentials are ICE-shaped for a reason.** The same signaling also drives a browser
path, where the far side is a real peer connection and the media rides an encrypted stream
transport. Username fragment, password, and certificate fingerprint exist so that one
signaling exchange serves both. Candidates trickle as they are discovered for the same reason.
So the shape is worth preserving even though the native path uses almost none of the
machinery behind it.

**Emit the controlled role on every binding request.** This is fixed by the protocol, not
negotiated, and varying it breaks the peer.

## §2 One socket, two protocols

Connectivity checks and media share a single socket for the life of the session. Classification
is per [01 §2](01-protocol.md), on the first two bytes, before anything else. There is no
separate control socket and no port pair.

The socket is opened by the connectivity engine and the descriptor is handed to the IO shell
once the path is established. **Ownership transfers; options do not change across the
handoff.** A setup path that lowers a socket option and leaves it lowered has already cost a
production stream ([02 §5](02-io-shell.md)).

## §3 Candidates

Four kinds, in the order they become available:

| Kind | Source |
|---|---|
| host | local interface enumeration |
| server reflexive | binding response from a public server |
| peer reflexive | the source address of an authenticated check we received |
| mapped | a port mapping created on the gateway (§6) |

**Peer-reflexive candidates are not optional.** Under symmetric translation the address a peer
advertised was created toward a reflexive server, and its packets to us leave from a different
mapping entirely, so the advertised address is not reachable and the observed one is the only
address that is. A host that ignores it will answer such a peer's checks while never finding a
path of its own, and a host that never finds a path never sends media. The failure is
one-sided and looks like the peer connecting successfully, which is what makes it easy to
miss.

The source of a **verified** check is admitted; nothing weaker is. Authentication means the
sender holds the password from the credential exchange, and an unauthenticated source address
would let anyone able to reach the socket point us anywhere.

Gathering rules:

- Enumerate every usable interface. Exclude loopback, link-local, and interfaces that are
  down.
- **Address family is determined structurally, never by scanning the text form for a colon.**
  A v4-mapped address contains colons and is IPv4. Classifying it as IPv6 removes every v4
  candidate and kills connectivity on v4-only paths. *Named regression test,
  [impl-plan §Phase 2](impl-plan.md).*
- Candidates are emitted to the application as they are discovered, not batched at the end.
  The peer can begin probing the first candidate while later ones are still being gathered.
- A candidate arriving for an unknown attempt is discarded silently. It is not an error; it is
  a race with teardown.

## §4 Binding checks

Standard STUN binding requests and responses, with these specifics:

- **Message integrity is computed over the request using the peer's password** from the
  credential exchange. A request that fails integrity is dropped without a response.
- The mapped address is returned XOR-obfuscated, and must be decoded before use.
- **Probes are sent with a reduced IP TTL, and the socket TTL is restored immediately
  afterward.** A low-TTL probe opens the local mapping without the packet reaching the peer's
  network, so the peer's firewall never sees an unsolicited inbound datagram from an address
  it has not yet sent to. Restoring the TTL is not optional: leaving the socket at the probe
  value silently caps the media path at a few hops.
- Responses are answered from the same socket, with the same credentials, immediately on
  receipt. A peer that does not answer checks is treated as unreachable even if media is
  flowing, which matters for §7.

## §5 The punch

```
for each remote candidate, in arrival order:
    send low-TTL probe          -> opens the local mapping
    send binding request        -> full TTL, authenticated
    on binding response:
        mark the candidate reachable
        adopt it as the active path
        stop probing others
```

- Probes repeat on a bounded schedule until a response arrives or the attempt times out.
- Both sides probe simultaneously. Simultaneous open is the normal case, not an exception.
- The first candidate to answer wins. There is no priority ordering and no attempt to find a
  better path afterward; the cost of switching mid-stream exceeds the benefit.
- **Local-network candidates are probed alongside public ones**, not after. On a LAN the local
  path answers first by a wide margin and the correct path is chosen for free.

**Symmetric address translation defeats this**, by construction. When the mapping the peer was
told about is not the mapping our packets actually arrive from, the peer's probes reach a port
nobody is listening on. This is not a bug to be worked around at this layer; it is the case
§6 and §7 exist for.

## §6 Gateway port mapping

An opportunistic mapping on the gateway, when the gateway supports it, yields a candidate that
is reachable even under symmetric translation.

- **One persistent runner for the lifetime of the connection, on a stable port.** Not one
  mapping per attempt. Per-attempt mappings leak: they accumulate on the gateway across
  reconnects until its table is full, at which point mapping stops working for everything on
  the network, including other applications.
- The mapping is removed on clean shutdown and its lease is short enough that an unclean
  shutdown expires rather than persisting.
- Discovery failure is not an error. It is the common case on networks where the feature is
  disabled, and it must not delay the punch. Gathering proceeds without it.
- **A mapped candidate whose external address is not globally routable is discarded, never
  advertised.** Shared address space, private ranges, link-local, and loopback all fail that
  test. Offering one spends the whole punch budget probing an address nothing can reach.

**Deferred, and not part of the first connectivity phase** ([impl-plan.md](impl-plan.md)). The
discovery mechanism cannot sit behind the sans-IO boundary, the benefit is opportunistic by
construction, and on a carrier-grade translated upstream it is worse than absent: the gateway
returns its own WAN address, that address is itself shared address space, and the rule above
then discards the only candidate the whole mechanism produced. The escalation path in §8 does
not depend on it, because the relay is ours.

## §7 Relay

When no direct path exists, media can be forwarded through a relay allocated by us.

**This is our addition, not a protocol requirement**, and the design consequence matters: the
peer needs no relay support at all. We allocate, we advertise the relayed address as one of our
candidates, and the peer sends there as it would to any other candidate. From the peer's side
nothing is different.

That matters more than it might appear, because **most peers have no relay of their own**. The
protocol's native relay offering is a paid-tier feature, absent for free and consumer-tier
users, and it is an endpoint both sides connect out to rather than a standard allocation. So
relay availability cannot be assumed from the peer, cannot be negotiated, and must be
transparent to it. A design that expected the far side to participate in relay setup would
work for a minority of sessions.

Requirements:

- Standard relay allocation, permissions, and channel binding. Client side only; operating a
  relay server is out of scope, and no default server address is compiled in.
- **The peer must answer binding checks arriving from the relayed address.** If it does not,
  the relay withholds media and the path silently produces nothing. This is a real failure
  mode with real peers and is why §4 requires answering checks unconditionally.
- **Receive buffers must account for relay framing.** The relay wraps our datagram in its own
  header, so a buffer sized for the media datagram alone discards every full-size packet while
  small control packets pass. It presents as a working connection that never shows video, and
  it is independent of the network. Sizing is specified in [02 §5](02-io-shell.md) and is
  derived from the protocol ceiling plus a fixed relay margin.
- **Reaching the relay over a stream transport is deferred.** It cannot sit behind the sans-IO
  boundary, where there is no async runtime and no transport security, so it is a shell concern
  if it is ever wanted. The datagram transport is the whole of the first implementation.

**Scheduled after Gate A** ([impl-plan.md](impl-plan.md) Phase 2b). The host is a relay
*client* against an ordinary external server. It is not a relay server, it does not run one
alongside itself, and it does not reach one over loopback; a design that co-locates the two is
a different component solving a different problem and nothing here is derived from it.

## §8 Policy and the ladder

**Every stage beyond the direct punch is opt-in, per connection, and driven by the
application.** The SDK does not decide to escalate on its own.

The reason is structural rather than philosophical. Escalating to a relay requires a fresh
candidate exchange, which requires signaling, which the application owns
([00-overview.md](00-overview.md) D3). An SDK that decided to escalate internally would need
to reach back through a seam it does not own.

So the ladder is:

1. Direct punch with whatever candidates were gathered. Report the outcome.
2. The application, seeing a typed failure, may start a new attempt with mapping or relay
   enabled.

The active path is reported so the application can display it and decide.

## §9 Failure outcomes

Failures are **typed**, never a generic timeout, because the correct response differs
completely between them.

| Outcome | Meaning | Application response |
|---|---|---|
| peer gone | the other side abandoned the attempt | give up, inform the user |
| no permission | rejected before connectivity began | do not retry |
| probe timeout | probes sent, nothing answered | retry with mapping or relay |
| relay unreachable | allocation failed or was blocked | retry direct, or a different relay |

A probe timeout is the only one that justifies escalation. Retrying the others wastes the
user's time and, on a rejection, looks like an attack to the far side.

## §10 Host firewall

On a host behind a firewall that filters unsolicited inbound datagrams, replies from a peer
behind symmetric translation arrive from a port the firewall never saw us send to, and are
dropped as unsolicited. The low-TTL probe in §4 does not help here, because the problem is on
the receiving side.

This is a deployment requirement, not something the code can fix: the host needs an inbound
rule for its media port. It is documented in [07-platforms.md](07-platforms.md) and surfaced
in the daemon's startup diagnostics, which check for it and warn rather than failing silently.

## §11 Verification status

Per [AGENTS.md](../AGENTS.md) §14, what in this document is confirmed against a real peer
versus carried from earlier work and pending re-verification.

**Confirmed:** the shared socket and its demultiplexing rule; binding requests and responses
carrying message integrity; the fixed controlling role; TTL-scoped probes with restoration;
gateway mapping present and opportunistic.

**Carried, pending re-verification before Phase 2 closes:** probe scheduling and backoff
constants; the exact attempt timeout; candidate emission ordering; the mapping lease duration;
whether the peer imposes any ordering requirement between candidate arrival and the first
probe.

**Ours by design, with no peer-side counterpart:** the relay path in §7 in its entirety.
