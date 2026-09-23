# 03 - Connectivity

**Status:** locked 2026-08-15; §7 rewritten 2026-09-23, when the relay moved from the host to
the client ([00-overview.md](00-overview.md) D15), and built the same day (C10), whose live gate
made §5's path both directions. Implemented
by `lowlat-core` (state machines) and `lowlat-net` (sockets), per
[00-overview.md](00-overview.md) D4.

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

**A browser is the one peer that runs the full procedure**, and the fixed role is what lets it:
its agent is always controlling, it pairs every candidate of ours with every one of its own,
checks each pair in a burst, nominates one, and keeps checking the pair it uses for as long as
the session lasts. None of that needs anything of this engine but what it already does --
answer every authenticated check, from the address it arrived at, for the life of the
attempt -- and the two figures that had to grow for it are in §4.

## §2 One socket, two protocols

Connectivity checks and media share a single socket for the life of the session. Classification
is per [01 §2](01-protocol.md), on the first two bytes, before anything else. There is no
separate control socket and no port pair.

**The IO shell owns the socket and opens it before connectivity begins**, because the engine
here is sans-IO and owns nothing. The rule that matters survives the correction: options are
set once at open and **nothing lowers one afterwards**. A setup path that lowered a receive
buffer and left it lowered has already cost a production stream ([02 §5](02-io-shell.md)).

## §3 Candidates

Four kinds, in the order they become available:

| Kind | Source |
|---|---|
| host | the source address the routing table would choose, per family |
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

**Gathering is the SDK's, not the application's.** The rules below carry reasons that are not
obvious from an address, and an integration that had to re-derive them would reach a different
answer each time. Host candidates are raised as ordinary candidate events alongside the
reflexive ones, so an application relays what it is given and decides nothing.

Gathering rules:

- **Both address families are gathered, and both are offered.** They are not substitutes: a
  peer may have only one of them, so a machine with global IPv6 that offers its v4 address
  alone hands a v6-only peer nothing to probe. A family the machine does not have contributes
  nothing, which is the ordinary outcome for v6 and is not an error. A globally routable v6
  address gathered this way is a **host** candidate, not a reflexive one
  ([04 §3](04-signaling.md)).
- **IPv4 host candidates are enumerated; the IPv6 one is probed.** The two are
  opposite on purpose. On v4 a machine can sit on one segment through several
  interfaces -- a wired and a wireless leg of the same network -- and asking the
  routing table names only one of them, so a peer that could reach the other is
  offered nothing it can use. On v6 there is no translation, so the address a
  peer sees is the source we would send from; an interface commonly carries a
  stable, a temporary and a route-local global address at once, and offering all
  three makes the peer spend checks discovering which one answers.
- **Only private address space is offered.** A host candidate exists to
  advertise what a reflexive probe cannot see. A publicly routable address is
  discoverable that way already, so offering it here as well is a duplicate that
  costs part of a bounded check budget; a private address is invisible to every
  server and is the only way to reach a shared segment. Interfaces that are down
  are skipped, and loopback and link-local fall outside the ranges rather than
  needing a rule of their own.
- **Shared address space is offered only when asked for.** It is reachable when
  both ends are behind the same carrier translation or on the same overlay
  network, and a wasted check for every peer that is not, so it is opted into.
- **The list is capped, and a cap that binds is reported.** A machine with
  several bridges can present a long list and each entry costs the peer part of
  a budget bounded in both attempts and time.
- **Address family is determined structurally, never by scanning the text form for a colon.**
  A v4-mapped address contains colons and is IPv4. Classifying it as IPv6 removes every v4
  candidate and kills connectivity on v4-only paths. *Named regression test,
  [impl-plan §Phase 2](impl-plan.md).*
- Candidates are emitted to the application as they are discovered, not batched at the end.
  The peer can begin probing the first candidate while later ones are still being gathered.
- A candidate arriving for an unknown attempt is discarded silently. It is not an error; it is
  a race with teardown.
- **A candidate a peer sends is not guaranteed to be an address**, and one that is not is
  declined out loud rather than dropped in silence ([04 §3](04-signaling.md)).

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
- **A response leaves from the address the request arrived at** (2026-08-29). The peer's
  filter admitted exactly that address pair, and on a multihomed host the kernel's default
  source selection answers from the primary sibling instead -- unsolicited traffic to a
  filtering translator, which drops it, so the one candidate a second address exists for
  never completes a check. The shell reports each datagram's arrival address and the engine
  carries it on the queued answer ([02 s5](02-io-shell.md)).
- **Sixteen answers are held pending, in every state** (2026-09-12). A peer running the full
  procedure checks every pair it holds in one burst, and four slots dropped answers from the
  burst; and it keeps checking the path it uses after the path is chosen, reading an
  unanswered check as the path gone, so the engine arms its timer for an owed answer after
  establishment as well as before. A check is also bounded at 256 bytes, which is what such a
  peer's request comes to with its username, priority, role, integrity and fingerprint
  attributes; nothing larger is a check.
- **A browser may offer a host candidate as a multicast name rather than an address**, which
  this engine cannot probe. The address it hides is learned when that browser's own check
  arrives from it -- the peer-reflexive admission of §3 -- and the path completes from there.

## §5 The punch

```
once, when a reflexive candidate exists:
    send low-TTL probe to it    -> opens our mapping on the crossing path
for each remote candidate, in arrival order:
    send binding request        -> full TTL, authenticated, repeating
    on binding response:
        the first candidate to answer is the path to be
on binding request from the peer:
    answer it                   -> before and after a path, always
once both have happened:
    adopt the path, stop probing others
    send nothing before the answer that completed it
```

*(Corrected 2026-08-29: this section previously showed a low-TTL probe per candidate. The
probe is one per attempt, and its target is the peer's server-reflexive candidate alone --
that is the path that crosses translation, which is the only mapping worth opening ahead of
a full-length check. A directly routable candidate never draws it, and the probe waits for
a reflexive candidate to exist rather than spending itself on whichever address arrived
first.)*

- Checks repeat on a bounded schedule until a response arrives or the attempt times out.
- Both sides probe simultaneously. Simultaneous open is the normal case, not an exception.
- **Full-length checks toward translated-path candidates wait for the peer's readiness
  marker** (2026-08-29); direct candidates and the mapping probe do not. A check that
  reaches a translated path before the peer has sent anything outward is unsolicited
  traffic to its translator, which can commit a state entry whose reply tuple is exactly
  the one the peer's own punch then needs -- poisoning the very mapping under negotiation.
  A direct candidate has no translator on its path to poison, and the probe never reaches
  the peer at all. A peer that never sends the marker still establishes through us: its
  own checks arrive, are answered, and teach a direct candidate that is checked at once.
- **The local address the winning answer arrived at is adopted with the path** (2026-08-29),
  and every datagram for the life of the session claims it. Left to itself the kernel
  re-selects the source per send, and on a multihomed host a routing change moves it
  mid-session -- the peer's filter then sees a stranger where its session was. Checks and
  reflexive probes before the path exists leave unpinned, because nothing is proven yet.
- The first candidate to answer wins. There is no priority ordering and no attempt to find a
  better path afterward; the cost of switching mid-stream exceeds the benefit.
- **A path is both directions** (2026-09-23): a candidate has answered our check, and the peer
  has checked us and been answered. The peer adopts its own path only when its own check is
  answered, which can be a whole check interval after ours, and until then it reads every
  datagram as a check. A path taken on our answer alone sent the session's first records --
  the initialization, the declarations, the acknowledgement cadence -- to a reader that drops
  them, and every connect of this client, direct or relayed, showed a burst of malformed
  connectivity messages on an established host's log in the second before its session
  began, where an established client's connect shows none. The answer that completes the
  peer's punch leaves before any record, even while pacing holds it back. Nothing is lost by
  waiting: a peer whose checks never reach us cannot begin its session either, and an
  attempt answered but never checked ends like any other that found no path.
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

When no direct path exists, media can be forwarded through a relay, and **the client is the
one that allocates it** (*rewritten 2026-09-23*, [00 D15](00-overview.md): this section used
to plan the host as the relay client, and that plan is dropped). The host takes no part: it is
offered one more candidate, checks it like any other, and sends plain datagrams to it.

**This is our addition, not a protocol requirement**, and the design consequence matters: the
peer needs no relay support at all. We allocate, we advertise the relayed address as our
candidate, and the peer sends there as it would to any other candidate. From the peer's side
nothing is different.

That matters more than it might appear, because **most peers have no relay of their own**. The
protocol's native relay offering is a paid-tier feature, absent for free and consumer-tier
users, and it is an endpoint both sides connect out to rather than a standard allocation; it is
neither used nor imitated here. So relay availability cannot be assumed from the peer, cannot
be negotiated, and must be transparent to it. A design that expected the far side to
participate in relay setup would work for a minority of sessions.

### §7.1 Where the relay runs

**The supported deployment puts the relay on the host's own machine**, and it is the reason
the client is the one that allocates:

- **One forwarded port.** The host's router forwards the relay's listening port and nothing
  else. The relay's allocation range stays on the host's side of the router.
- **The relayed address is the machine's own local address.** The relay is configured with no
  external address, so the address it hands out is the one its sockets are bound to, and the
  host reaches it by local delivery to its own address -- never through the router and never
  over loopback. The host's traffic arrives at the relay from the host's own address and port,
  unchanged.
- **The client's side crosses anything.** The client talks to one server address for the life
  of the session, an ordinary outbound flow that the strictest translation passes, so a client
  that cannot be punched at all is still reached.
- **Configuring the relay's external address breaks it.** The relayed address becomes the
  router's public one, both legs hairpin through the router into an allocation range nobody
  forwarded, and allocation and permission both succeed while no media ever arrives.
- **Only the client can use this arrangement.** A relay allocated by the host on its own
  machine would hand the remote peer a private address. Allocated by the client, the relayed
  address only has to be reachable by the host, which is on the same machine.

A relay on a public server works the same way from the client's side; nothing in the client
depends on where the relay runs, only the deployment does. Operating a relay server is not
this project's: nothing ships one, and no default server address is compiled in.

### §7.2 A relay attempt

**A relay attempt is relay-only.** When the application configures a relay, the attempt
advertises the relayed address and the readiness marker and nothing else: no host candidate,
no reflexive candidate, and no reflexive server is asked. Every check leaves through the
relay. A host on the same network cannot quietly turn a relay attempt into a direct one, and
the path an attempt reports is the path it uses.

The order is fixed, and its third step is the one that matters:

1. Allocate: the server's challenge, then the authenticated request.
2. Permit the machine the relayed address belongs to -- the host's own, when the relay runs
   there.
3. **Only then** advertise the relayed address, and the readiness marker after it
   ([04 §3](04-signaling.md)).

A relayed address advertised before its permission exists has the host's first checks dropped
at the relay, silently. The host keeps checking, and the path waits for a check that gets
through (§5): the session loses its first second. *(Before a path needed both directions, the
media sent meanwhile reached a reader still expecting checks, and presented at the host as
unparseable connectivity messages and at the client as resends.)*

- **Permissions are per address and independent.** After the relay's machine, each host
  candidate's address is permitted as it arrives, so a host elsewhere on the relay's network is
  reached too. A server that refuses one address refuses only that one, and a request that goes
  unanswered is sent again. One request may name several addresses, but then one refusal sinks
  all of them, so each goes alone. An IPv6 address is not asked for on an IPv4 allocation.
- **Answers leave the way the check came.** A check that arrived through the relay is answered
  through the relay, before the path exists and for as long as the session lasts, while the
  host keeps checking the path it uses (§4). A relayed check answered from the socket directly
  goes to an address the host cannot be reached at, and a host whose checks go unanswered
  withholds media.
- **Nothing is relayed toward a loopback address.** A candidate or a check source on loopback
  is not permitted, not checked, and never sent to through the relay. A deployed relay destroys
  the allocation that sends toward loopback, so one such candidate, hostile or mistaken, would
  end the session.
- **Media rides a channel.** Once the path exists it is bound to a channel, and media carries 4
  bytes of framing instead of 36; checks travel as indications. A relay delivers both forms and
  both are accepted, padded or not.
- **The path follows the host.** After establishment the client sends to the address the
  host's authenticated traffic arrives from, whichever of its candidates answered first.
- **Receive buffers account for the framing.** The relay wraps each datagram in its own header,
  so a buffer sized for the media datagram alone discards every full-size packet while small
  control packets pass. It presents as a working connection that never shows video, and it is
  independent of the network. Sizing is specified in [02 §5](02-io-shell.md), from the protocol
  ceiling plus a fixed relay margin, which holds the largest indication: 51 bytes of framing,
  toward an IPv6 peer.

### §7.3 Keeping the relay

- **A permission lasts 300 seconds, and traffic does not extend it.** Each is re-issued well
  before it lapses, every 240 seconds, and a channel binding is renewed on the same cadence,
  which renews its address's permission too. A permission left to lapse makes the relay drop
  both directions without a word: the session freezes at five minutes and times out a minute
  later. **Each is renewed as though the other did not exist.** A bound channel hides a
  permission renewed late, and a relay that binds no channel -- or a binding that lapses --
  then exposes it; renewed at 300 seconds rather than before, the media stalled five seconds
  later in the simulator, and not at all while a channel was bound.
- **The allocation is refreshed at half its lifetime.** An answer with no lifetime, or a zero
  one, is read as the lifetime asked for; otherwise the next refresh falls due in the past and
  the requests storm.
- **The class is two bits, and they are not adjacent.** A success response sets one of them
  and an error response sets both, so an error carries the success bit as well. Reading one bit
  takes every error for a success, and a refused renewal goes unnoticed until the permission
  lapses.
- **A stale nonce is a challenge.** A relay rotates its nonce every few minutes and answers the
  next request with a stale-nonce error that carries the new one; the nonce is adopted and the
  request sent again. A session that cannot do this dies at the first rotation.
- **No exchange with the relay blocks the reader.** A request is state and a deadline in the
  same stream the media arrives on; a reader that waits for one answer discards the media that
  arrives meanwhile.
- **Failure is typed and final** (§9). No answer to the allocation, a refused allocation, or a
  hard error on a renewal mid-session ends the attempt with its own outcome; nothing is retried
  forever.
- **A clean leave releases the allocation**, with a refresh of zero lifetime, rather than
  holding a relay port until it expires.

### §7.4 Transport

**Reaching the relay over a stream transport is deferred.** It cannot sit behind the sans-IO
boundary, where there is no async runtime and no transport security, so it is a shell concern
if it is ever wanted. The datagram transport is the whole of the first implementation, and the
relayed address is IPv4.

## §8 Policy and the ladder

**Every stage beyond the direct punch is opt-in, per connection, and driven by the
application.** The SDK does not decide to escalate on its own.

The reason is structural rather than philosophical. Escalating to a relay requires a fresh
candidate exchange, which requires signaling, which the application owns
([00-overview.md](00-overview.md) D3). An SDK that decided to escalate internally would need
to reach back through a seam it does not own.

So the ladder is:

1. Direct punch with whatever candidates were gathered. Report the outcome.
2. The application, seeing a typed failure, may start a new attempt with a relay configured,
   which makes it a relay attempt (§7.2), or with mapping enabled.

An application that knows its host is reachable only through a relay may start with the relay
attempt; nothing requires a failed direct attempt first. The active path is reported, relayed
or direct, so the application can display it and decide.

## §9 Failure outcomes

Failures are **typed**, never a generic timeout, because the correct response differs
completely between them.

| Outcome | Meaning | Application response |
|---|---|---|
| peer gone | the other side abandoned the attempt | give up, inform the user |
| no permission | rejected before connectivity began | do not retry |
| probe timeout | probes sent, nothing answered | retry with mapping or relay |
| relay unreachable | the relay did not answer in time to allocate and permit its own machine: five seconds | retry direct, or a different relay |
| relay refused | the relay refused the credentials or the allocation, or is full | fix the configuration; the same relay refuses again |
| relay lost | a renewal was refused or went unanswered mid-session | reconnect; the relay has already let the allocation go |

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

**Confirmed, and load bearing for what we offer:** a peer gathers v4 host candidates only
from private address space -- `10/8`, `172.16/12`, `192.168/16`, and shared address space
behind a setting; a public v4 address is never offered as a host candidate.

*(Corrected 2026-08-29, against a three-client host capture.)* Two claims this paragraph
used to make did not survive that capture. A peer **does** offer a globally routable IPv6
address, and it arrives **marked lan**, not server reflexive: on that family there is no
translation to negotiate, so the lan marking means "check this without ceremony" rather
than "this is your segment", and it applies however the address was discovered. This
implementation now marks its own v6 the same way on the way out, whichever probe found it,
so the peer checks it at once and keeps its one path-opening probe for a translated path.

Also from the same capture: **a candidate may carry neither marking** -- a peer's public
address at its local port, a translated-path guess no server verified, offered in case its
translator preserves ports (observed beside the server-verified mapping at a different
port, and observed alone). The candidate model carries all three classes end to end for
that reason. And a readiness marker's address field is arbitrary in practice: one peer
sends a fixed placeholder, another echoes the recipient's own reflexive address, so the
marker must never reach the candidate table whatever it carries.

**Confirmed:** the shared socket and its demultiplexing rule; binding requests and responses
carrying message integrity; the fixed controlling role; TTL-scoped probes with restoration;
gateway mapping present and opportunistic. From a multi-peer capture: peers offer a host
candidate per interface; a globally routable IPv6 host candidate carries the host flag; a
v4-mapped address arrives in its textual form on both host and reflexive candidates; a
readiness marker carries an arbitrary address, the sender's own reflexive one in at least one
case; and a peer may offer a host candidate as a `.local` name rather than an address.

**Untested, and known to be:** every IPv6 path in the simulator and the namespace fixtures.
The topology matrix in [08-testing.md](08-testing.md) is v4 only, so the punch state machine
has never been exercised on a v6 topology even though the socket carries both families and the
unit tests run over v6 loopback. A v6 host candidate is now offered, which makes this the
widest untested surface in this document rather than a theoretical one.

**Carried, pending re-verification before Phase 2 closes:** probe scheduling and backoff
constants; the exact attempt timeout; candidate emission ordering; the mapping lease duration;
whether the peer imposes any ordering requirement between candidate arrival and the first
probe.

**Ours by design, with no peer-side counterpart:** the relay path in §7 in its entirety.

**Confirmed 2026-09-23, with the deployment of §7.1 and an established host:** the host took
the relayed address as an ordinary candidate, reached it by local delivery to its own address,
and streamed at 0.8 ms more round trip than a direct session on the same pair. Measured against
the same deployed relay the same day: it relays to its own machine's address with full-size
datagrams intact, delivers channel data both ways, keeps its allocation range off the router,
grants a permission for any address it is asked for, and destroys an allocation that sends
toward loopback, which no closed port, hostless address or router hairpin did.

**Built 2026-09-23 (C10), and confirmed three ways.** Against a relay written from the
specification in the simulator, with a deployed relay's lapses, rotations and loopback rule:
a symmetric client reaches the host through it and not without it, full-size datagrams cross
both ways in both framings, and sixteen minutes cross every permission lifetime three times
and two rotations with nothing lost. Against a real relay server in network namespaces,
beside the host behind a router that forwards its one port, the client behind symmetric
translation: the host's path is the relayed address, the machine's own, and the same
topology without the relay times out. And live, through the deployed relay of the paragraph
above to an established host on its machine: the relayed address was the machine's own, the
path the host's own address through the relay, 27 ms of round trip at the median over a
minute, nothing lost or late. The comparison with a direct session of this client on the same
pair is the phase gate's.

**Confirmed against two browser families, 2026-09-12 and 2026-09-13:** the fixed controlled
role against a full agent that is always controlling; sixteen pending answers under a check
burst; consent checks answered for the life of the session; a `.local` host candidate
completed from the browser's own check; 43 distinct browser checks captured on the attempt
socket and run through the check parser's fuzz corpus, which kept the seven that reached
new branches.
