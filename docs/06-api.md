# 06 - Public API

**Status:** locked 2026-08-15. Implemented by `lowlat-host`, generated as one C header.

**The C ABI is the only public surface.** There is no public Rust API, no C++ wrapper, and no
language-specific SDK. Every binding anyone will ever want consumes C: C#, Java, Swift,
Python, and Rust itself all speak it, and it is the one calling convention that survives across
compilers, runtimes, and toolchain versions.

## §1 Shape

Six rules, each of which removes a class of integration failure.

1. **Opaque handles.** The application holds a pointer it cannot dereference. Internal layout
   changes freely.
2. **Plain data structs with a leading `size` field.** The caller sets it. We read it and
   behave according to what the caller knows about, so a struct can grow without breaking
   binaries compiled against an older header. It is also what makes a translation unit that
   disagrees with the library about a structure's size fail to start rather than misread every
   field after the first.
3. **Stable-numbered enums, and never a parameter.** Values are assigned once and never
   reused, never renumbered, and never reordered; new variants append. A code travels **outward**
   as its enumeration and **inward** as a plain integer, because a value the application wrote
   is whatever the application wrote, and reading one nothing defined is undefined behaviour.
   Every such field is checked at the boundary rather than converted.
4. **A boolean field is a `bool`.** One byte on every target this builds for, asserted at
   compile time by the header's own test, and C normalizes anything assigned to one.
5. **Poll based, not callback based.** The application asks for events on its own thread at its
   own cadence. No callback fires from inside our threads, so there is no reentrancy contract
   and no lock the application can deadlock against.
6. **Prefixed symbols.** Every exported name begins `lowlat_`. This is checked mechanically
   against the symbol table ([impl-plan §Phase 8](impl-plan.md)).

The **call shape follows the established host SDK** ([00-overview.md](00-overview.md) D6), so
porting an existing integration is close to mechanical. **Struct layouts are ours.** Binary
drop-in compatibility is explicitly not offered, and the symbol prefix guarantees that a
mismatch is a link error rather than silent memory corruption at the first field.

## §2 Lifecycle

```c
lowlat_status lowlat_create(const lowlat_create_info *info, lowlat **out);
void          lowlat_destroy(lowlat *ll);
lowlat_status lowlat_set_log_callback(lowlat_log_fn fn, void *opaque);
lowlat_status lowlat_set_log_level(uint32_t level);
const char   *lowlat_status_string(int32_t status);
uint32_t      lowlat_abi_version(void);
```

One handle owns one host session. `lowlat_destroy` stops hosting, disconnects every guest,
joins every thread, and returns only when all of it has happened.

`lowlat_abi_version` lets a loader verify the library matches the header it was built against
before calling anything else. It is the one function whose signature can never change.

**`lowlat_debug_panic` is exported on purpose and is not for applications.** It panics, so that
containment can be tested against the object that ships rather than against a copy of the same
code linked into a test, which answers for the test's build settings instead. One symbol is a
small price for the only check that can fail if [§9](#9-panics-and-unwinding) regresses.

The log callback is the single exception to rule 5. It is cold, it fires on whichever thread
logged, and it must not call back into the API.

**It is replaceable, and passing `NULL` returns the library to writing lines itself.** The sink
underneath is process-wide and takes one installation; what an application registers sits behind
that, so registering again changes where lines go rather than being refused.

**The message is a NUL-terminated copy**, which is the one allocation on that path: a Rust
string carries its length rather than a terminator, and handing out a pointer to one would be
handing out something C cannot read to the end of.

**`lowlat_set_log_level` decides what is formatted at all**, not only what is delivered. A line
above the level costs a comparison; below it, the message is built. That is why the level is a
call rather than a filter the callback applies.

**With no callback registered, lines carry the elapsed time** since the first of them and go to
standard error. Every diagnosis made from these logs has come down to an interval -- how long a
wait actually waited, whether a periodic line stopped -- and a log with no clock answers none of
them. It is monotonic rather than a wall clock: that is the quantity being read, and it needs no
timezone to mean something.

## §3 Host

```c
lowlat_status lowlat_host_start(lowlat *ll, const lowlat_host_config *cfg);
lowlat_status lowlat_host_stop(lowlat *ll);
lowlat_status lowlat_host_get_status(lowlat *ll, lowlat_host_status *out);
lowlat_status lowlat_host_poll_microphone(lowlat *ll, uint32_t timeout_ms, int16_t *samples,
                                          uint32_t *count, uint32_t *guest, uint32_t *dropped);

lowlat_status lowlat_host_set_video_config(lowlat *ll, const lowlat_host_video_config *cfg);
lowlat_status lowlat_host_get_video_config(lowlat *ll, lowlat_host_video_config *out);

lowlat_status lowlat_host_set_audio_config(lowlat *ll, const lowlat_host_audio_config *cfg);
lowlat_status lowlat_host_get_audio_config(lowlat *ll, lowlat_host_audio_config *out);

uint32_t      lowlat_host_get_guests(lowlat *ll, lowlat_guest *out, uint32_t *count);
lowlat_status lowlat_host_kick_guest(lowlat *ll, uint32_t guest_id, int32_t reason);
lowlat_status lowlat_host_set_permissions(lowlat *ll, uint32_t guest_id,
                                          const lowlat_permissions *perms);

lowlat_status lowlat_host_send_user_data(lowlat *ll, uint32_t guest_id, uint32_t id,
                                         const void *data, uint32_t len);
lowlat_status lowlat_host_send_roster(lowlat *ll, const void *data, uint32_t len,
                                      uint32_t *reached);
lowlat_status lowlat_host_get_metrics(lowlat *ll, uint32_t guest_id, lowlat_metrics *out);
```

**The roster is not a variant of an application message.** It travels on its own opcode, it is
addressed to everybody rather than to a guest, and each peer finds *itself* in the list by
number and takes that entry as what it is allowed to do. A peer has no way to ask for one, so a
guest that is never sent one does not know what it is. Its body's shape belongs to the clients
an application serves, exactly as a message's does.

**A guest carries the attempt it was registered under**, which is the link between the seam's
two halves: everything before a guest is seated is addressed by attempt and everything after by
number, and without it an application holding one peer per attempt cannot tell which peer an
event about guest three concerns.

**Metrics live behind their own call rather than inside `lowlat_guest`, and that follows from
rule 2.** A guest is delivered as an array element, an array element cannot usefully carry a
`size` -- the caller walks it by stride -- so `lowlat_guest` is fixed for the major version.
Metrics are the numbers most likely to grow, so they live where growing them is free.

**One stream, not an array of them.** This host produces one and switches which display feeds
it, so there is nothing to index.

**They report what this host can answer for and nothing else.** A peer's own decode time and how
many frames it has queued waiting to decode are the peer's to know; reporting either would be
reporting a number this host made up. What is here is what the congestion controller already
reads -- outstanding fragments, how many are past due, the measured rate, encode time, the
smoothed round trip -- plus when each kind of input last arrived, which is the one question an
application kicking idle guests can ask nobody else. **Zero means never, which is not zero
milliseconds ago.**

**There is no separate call to enable or disable a guest's input.** It was declared here and
removed 2026-08-21 before anything was built against it: it is `lowlat_host_set_permissions`
with every flag cleared, and two calls that write one field can disagree about what a guest is
allowed to do. Permissions are the field; there is one way to set them.

**`lowlat_host_poll_microphone` is a poll of its own, not an event.** A hundred packets a
second sharing the event queue would evict the events it is there to deliver, so sound from a
guest has its own queue and an application that wants both polls both. **It hands over samples
rather than a codec**: sixteen-bit, mono, 48 kHz, whichever way the guest encoded them. The
buffer must hold `LOWLAT_MICROPHONE_SAMPLES_MAX`, because a packet cannot be larger and there is
therefore no partial delivery to call back for. `dropped` reports what a queue nobody drained
had to discard -- oldest first, because late sound is worth less than the sound behind it -- and
travels with the next delivery, which is the only place it can.

**It refuses rather than waits when the microphone is not accepted.**
`LOWLAT_ERR_NOT_STARTED` comes back immediately: a host that is not taking microphones will
never have one, and spending the caller's timeout to say so would read as sound that is merely
late.

**`lowlat_host_get_status` reports what is happening, not what was asked for**, which is why
it carries the picture's size and the guest count and not the settings that produced them: the
display decides its own size and the room decides its own occupancy. It answers on a handle that
is not hosting too, with `running` clear -- an application asking what state something is in
should not have to know the answer to ask.

**Sound is there for the same reason, and it is the half the settings cannot express.**
`audio_active` is whether a device is being read right now, which is clear in an empty room
however sound is configured, and clear when the device could not be opened or has gone away --
the case an application could otherwise not learn at all, because the settings go on saying
enabled. `audio_device` is what the capture landed on: an empty `device` in the configuration
asks for the default output's monitor and the sound server may move a stream while it runs, so
this is the only place the request and the answer can be compared. **The configuration keeps the
request** rather than being rewritten to the resolved name, so an application that reads the
settings, changes one field and writes them back does not pin a host that was following the
default.

**`lowlat_host_stop` takes no reason, and that is a gap rather than a design.**
Stopping ends every guest loop and joins every thread, and the far side learns from its own
liveness deadline rather than from a message -- so a peer pays the wait instead of being told.
Telling it means sending the disconnect status the protocol already carries
([01 §11.2](01-protocol.md)) on the way down, which the seam has no path for yet. A reason
parameter appends when it does.

**`lowlat_host_kick_guest`'s reason is not a `lowlat_status`.** It is what the peer is told on
the way out, which belongs to the protocol's own numbering rather than to this API's
([01 §11.2](01-protocol.md)); the two spaces share a width and nothing else. Zero is not a
value to pass: a peer carries on through it.

### The two halves of a configuration

**A setting is either settled when hosting starts or changeable while it runs, and which one it
is follows from what changing it costs.** They are separate structures rather than one struct
with a comment, so an application cannot ask for something the answer to which is "not while
this is running".

`lowlat_host_video_config` is the live half, and `lowlat_host_start` takes it nested inside the
whole:

| Field | Live | Why |
|---|---|---|
| `fps` | yes | A **ceiling** over the display's own rate, not a target. Changes the pacing from the next frame. |
| `bitrate_mbps` | yes | Re-bases the rate budget and reaches the encoder through the reconfigure the rate loop already performs every pass. No keyframe, no interruption ([00 §D8](00-overview.md)). |
| `min_bitrate_mbps` | yes | The floor congestion control may not descend below, and it **moves down with the ceiling**: a ceiling lowered under a floor that stayed leaves every controller pinned at a rate the operator just asked not to exceed. |
| `full_fps` | yes | Emit at `fps` even when the picture has not changed. **Clearing it is a permission, not an instruction** -- there is no damage signal here, so nothing yet skips a repeated picture, and continuing to send costs bitrate rather than being wrong. |
| `output` | yes | The exception in cost rather than in kind: a picture from another output cannot be absorbed into a stream built for one, so it rebuilds around the new source for **one coded refresh** and every guest keeps its seat and its channel. |
Everything in `lowlat_host_config` outside that structure is settled at `lowlat_host_start`:

| Field | Why not live |
|---|---|
| `codec` | One encode serves every seat and a session has one video configuration ([00 §D11](00-overview.md)). |
| `encoder` | A consequence of where the display is rather than a preference, and changing it rebuilds the pipeline. Absent means **follow the display**, which is the right default; choosing one is an override. |
| `quality` | One of [`lowlat_quality`](#quality). It is what the encoder is built with, and one encode serves every seat, so moving it under a running session would change the picture every guest is watching on one guest's behalf. |
| `cg_level` | Every guest's controller is built with it. **Zero is the most aggressive, not "off"**: its threshold declares congestion on any stale fragment once the send window passes its floor, and it exists only for compatibility with an older scheme. |
| `base_port`, `servers` | Bound and consulted per attempt; moving them under running guests moves nothing that is already connected. |
| `max_guests` | Advertised capacity, read when a guest asks for a seat. |
| `exclusive_pointer`, `exclusive_hold_ms` | The pointer arbiter is built once with them. The hold is **clamped rather than refused**: it is a comfort setting, and the nearest usable value beats a host that will not start.

<a name="quality"></a>
**`lowlat_quality` names where a host sits between delay and picture**, and it is the only
encoder tuning this boundary exposes:

| Value | Quantiser floor | Search effort |
|---|---|---|
| `LOWLAT_QUALITY_LOWEST_LATENCY` = 0 | 5 | the most a device offers |
| `LOWLAT_QUALITY_BALANCED` = 1 | none | the most a device offers |
| `LOWLAT_QUALITY_HIGHEST` = 2 | none | the least, and two passes where the encoder has them |

**Zero is the low-latency end, and that is deliberate**: a zeroed structure has to mean the
sensible default, and for a product whose first goal is delay the sensible default is the floor.

**The floor reads backwards until you see why.** A lower floor lets the encoder spend more bits
refining a picture; more bits is a larger frame, a larger frame is more packets, and more packets
is longer on the wire and longer in every queue in between. Below about five those bits refine
nothing the eye resolves, so they are spent purely on delay. Raising the floor above five trades
visible sharpness for smaller frames; removing it does the reverse.

**Search effort is not fidelity and the two are easy to confuse.** Effort says how far the
encoder looks -- motion range, sub-pixel refinement, how many modes it tries -- not how coarsely
it quantises. At a fixed rate more effort spends fewer bits on the same picture; it also takes
longer, and on one device measured here the span between the extremes is 1.5 ms against 3.3 at
1080p. That is why the highest setting carries a warning rather than being the default.

**What a host reports back is what it asked for.** No interface here says whether a driver acted
on a quantiser floor or an effort level, and the drivers differ: one encoder advertises thirty-two
effort levels and its timings track none of them, another takes the floor on one codec and ignores
it on the other, and a third has no second pass at all. So the three values are points on a trade
honoured as far as each device allows, a host logs the request and the levers it derived once per
stream, and an application that needs to know what a device really did has to measure coded bytes
rather than ask.

**Sound has no settled half at all**, so `lowlat_host_audio_config` is both what a host starts
with and what `lowlat_host_set_audio_config` takes:

| Field | Why it can change | |
|---|---|---|
| `enabled` | Switching it off gives the sound device back and restores the speakers, exactly as the last guest leaving does. Switching it on takes the device again -- **including on a host that started with it off**, which is what makes this field live rather than a settled one wearing a setter. |
| `bitrate_kbps` | Read on the frame that uses it, so a change costs no rebuild and no discontinuity a listener would hear. **A rate of zero or one past the ceiling is refused** rather than clamped in silence, because the codec would clamp its own and the application would be told yes and given something else. |
| `allow_uncompressed` | **A permission, not a request, and off by default.** A guest asks for the uncompressed form in its own initialization; this is whether a host will serve it. It costs an order of magnitude more of the uplink than the compressed form, and that comes out of what is left for the picture ([05 §9](05-host.md)). A guest denied it is sent the compressed form, priced as the compressed form, and told it is the compressed form. |
| `accept_microphone` | Whether a guest's microphone is taken, **off by default**. It is two things at once and they cannot be separated: this host decodes what arrives, and it tells the peer it will -- **a peer sends nothing until it is told**, so nothing arrives while this is clear however the guest configured itself. It costs a packet every ten milliseconds on the channel that carries control messages ([05 §9.6](05-host.md)), which is why it is a decision rather than something switched on by polling for it. Live, like the rest: switching it off tells every connected peer to stop. |
| `mute_local` | Silences the speakers at the desk while a guest is connected, off by default. On a device that applies its own mute the tap is ahead of it, so a guest still hears everything. **On a device whose mute the sound server applies, nothing is silenced and the log says why** -- the mix the mute reaches is the one being captured, so obeying would silence every guest ([05 §9.4](05-host.md)); the setting is still accepted, because the device can change under a running host. **It restores rather than unmutes**: a device somebody had already muted stays muted, and one they unmuted mid-session is not muted again. |
| `device` | Empty means the default output's monitor, **followed as the default changes**. A named one is checked against the enumeration at the call and refused with `LOWLAT_ERR_INVALID_ARGUMENT` if it is not there, because a name that does not resolve is substituted by the sound server rather than refused -- and the loop that opens it runs long after the call returned, so the call is the only place that can say no. Refused changes nothing: the host keeps the device it has. **The start does not check**, because a host whose sound server is not up yet must still be able to stream pictures. |

**The sound device is held only while somebody is listening.** It is opened when the first guest
arrives and given back when the last leaves, so a host that is advertised but empty holds no
capture and no speakers.

Starting a host that is already running is refused rather than quietly reconfiguring, because a
second configuration that looks accepted and is not is a host running settings nobody can see.

**What is being captured is read back, never remembered.** `lowlat_host_get_video_config`
reports the output the loop is actually on, which a guest may have switched and a display may
have moved by itself; an application that kept its own copy would mark the wrong screen.

## §4 Signaling seam

The four calls from [04 §9](04-signaling.md). This is the entire contact surface between any
signaling implementation and the SDK.

```c
lowlat_status lowlat_host_new_attempt(lowlat *ll, const lowlat_attempt_info *info);
void          lowlat_host_add_candidate(lowlat *ll, const char *attempt_id,
                                        const lowlat_candidate *cand);
lowlat_status lowlat_host_begin_p2p(lowlat *ll, const char *attempt_id,
                                    uint16_t port, lowlat_credentials *out);
void          lowlat_host_end_connection(lowlat *ll, const char *attempt_id);
```

**Registering is not approving.** `lowlat_host_new_attempt` takes a seat's worth of
bookkeeping and nothing else; no socket is opened and no thread is started until
`lowlat_host_begin_p2p`. An application that decides to decline simply never calls that and
says so over its own signaling.

**Every refusal is its own status**, in the -100 band, because the correct response differs per
outcome: a full host declines the offer, a race with teardown is retried or dropped, and a
crypto failure is neither. `LOWLAT_ERR_AT_CAPACITY` in particular means **decline**, not stay
quiet -- nothing in the protocol reports a host that never replied, so a peer given silence sits
connecting until its own deadline expires.

`lowlat_host_begin_p2p` writes host credentials into `out` for the application to send as its
answer. It does not send anything, because the SDK has no transport.

**The port is an in and an out pair, and they are different questions.** `port` is where the
bind *starts*; `out->port` is where it *landed*. The bind walks when a port is taken and takes
any port once the walk is exhausted, so the two differ whenever the range is busy -- and
advertising the one that was asked for produces a peer that answers checks and never
establishes. Everything advertised is built from the address the socket reports, which is why
landing somewhere unexpected costs a mapping rather than a session.

**Zero asks for the configured base**, which is what an application with no port of its own to
manage passes. An application that does have one has it for a reason -- a mapping it made on
the gateway, a rule it opened on the firewall, a pool it allocates from -- and none of those
survive the SDK choosing for it. `out->port` is also the only way to learn the number
*synchronously*: candidates carry it too, in the addresses they name, but those arrive as
events afterwards.

**Credentials stay an output.** The application never supplies them: the key material is
generated here, from the one audited source of entropy, and both directions of the session key
from it. An application-supplied key would make an integrator's random number generator the
session's.

`lowlat_host_add_candidate` and `lowlat_host_end_connection` accept unknown attempt
identifiers silently. Those are races with teardown, not errors, and returning a status the
caller would have to ignore is worse than returning nothing. A withdrawal that arrives before
the offer it withdraws is **remembered**, so admitting that offer afterwards is refused with
`LOWLAT_ERR_WITHDRAWN` rather than spending a socket and a thread on a guest already gone.

**A candidate marked `sync` is a readiness marker rather than an address**, and whatever
address rides along is ignored -- so it alone is accepted without one. A peer may withhold every
real candidate until it has seen one.

**`lowlat_host_end_connection` takes no reason**, for the same reason `lowlat_host_stop` does
not: ending stops the guest's loop, and the far side learns from its own liveness deadline
rather than from a message. The disconnect status the protocol carries exists; nothing calls it
on the way down yet.

## §5 Events

```c
lowlat_status lowlat_host_poll_events(lowlat *ll, uint32_t timeout_ms, lowlat_event *out,
                                      void *body, uint32_t *body_len);
```

Returns `LOWLAT_OK` with an event, or `LOWLAT_TIMEOUT` if none arrived. A `timeout_ms` of zero
polls without blocking.

**The one event that carries a body is handed it through the caller's own buffer**, which is
why the poll call takes one. `body_len` is the buffer's capacity going in and the bytes written
coming out; `NULL` means the application does not want bodies, and one that arrives is dropped
with the loss counted like any other. **The event itself never carries a pointer**, only the
body's length, so the union stays blittable ([§12](#12-bindings)).

**A buffer too small does not lose the message.** The needed length is written to `body_len`,
`LOWLAT_ERR_TOO_SMALL` is returned, and the event stays at the head of the queue for a second
call with a larger buffer. That is what lets an application run a small scratch buffer instead
of sizing it at the ceiling it will almost never reach, and it is the reason a poll is a peek
that commits on delivery rather than an unconditional take.

**No allocation crosses this boundary and there is no lookup key.** An application message is
the only variable-length thing an application ever receives - frames, audio and cursor images
are all excluded by [§13](#13-what-is-deliberately-absent) - so a side table of pending buffers
would exist for one event type, and it would bring the two failure modes that come with it: a
handle that is stale because the buffer was already taken, and a free performed by a runtime
that did not allocate it.

`lowlat_event` is a tagged union: a stable-numbered type followed by a union of plain
sub-structs. Adding an event type is additive; an application that does not recognize a type
ignores it, which is why the type field is first.

| Event | Meaning |
|---|---|
| candidate | a local candidate to forward over signaling |
| ready | tell the peer this host is ready to be checked, once |
| established | a path was found and media is flowing |
| ended | the attempt is over, with a typed outcome |
| user data | an application message from a guest |
| capture changed | a different output, or the same one at a different size |
| input owner changed | the guest holding the pointer changed |
| fatal | the host could not serve anyone and every guest was told |

**A guest's state changes are the four attempt events**, not one event with a
state field: candidate and ready while it negotiates, established when a path is found, ended
with a typed outcome. Splitting them is what lets an application respond to each without
switching on a state inside a state.

**Each of the last three is raised where its change happens**, which is the only place that can
tell a change from a repetition. The capture one comes from the loop that rebuilt, because
nothing above it knows whether the output moved or the display resized. The input owner comes
from inside the arbiter's own lock, because a guest thread can only report that the pointer is
now its own -- which is also what it would report on every message while it merely keeps
holding it.

**A guest that is chronically behind is not yet an event.** The skip-and-resync cycle exists
([05 §6](05-host.md)) and what is missing is the threshold that makes a cycle "chronic"; adding
the event before deciding that would mean either firing on every skip or picking a number
nothing measured.

**Poll from one thread.** The queue is single-consumer. Every other call is safe from any
thread.

Events are dropped **oldest-droppable-first** if the application stops polling, and `fatal` is
never dropped -- so a fatal event sitting at the front of a full queue is not the first thing
thrown away, which is what a plain oldest-first rule would do to the one event whose loss no
count can convey. Holding it does not make the queue unbounded: everything droppable still
goes. A dropped-count field on the next delivered event makes the loss visible rather
than silent.

**The queue is bounded in bytes as well as in entries**, because one of the two is not a bound:
a body may reach a megabyte, so a queue limited only by how many events it holds is limited to
that many megabytes. Either ceiling evicts oldest-first and both count into the same field.

## §6 Enumeration

```c
lowlat_status lowlat_get_outputs(lowlat_output *out, uint32_t *count);
lowlat_status lowlat_can_host(void);
```

**`lowlat_can_host` exists because the two ways of failing look identical afterwards.** A host
that cannot capture fails deep in the stream loop, where an application can tell "there is no
display" from "this process may not read one" only by reading a log. This answers which, before
anything starts, and it is a read: no encoder is built and no thread starts.

**It reads the framebuffer's buffer handles, not merely whether a plane is lit**, and that is
the whole difficulty. Enumerating a connector and finding its framebuffer both succeed without
the capability; getting the handles back out does not. Measured on a real display: the same
binary answers `LOWLAT_ERR_DISPLAY_UNREACHABLE` as an unprivileged user in the `video` group and
`LOWLAT_OK` as root. A weaker probe reports a machine ready to host that cannot.

**An identity is sized for a device path, not for a connector name.** The same bound carries a
display's identity and a sound device's, and the longest of them is a path: a USB output's name
from the sound server passes a hundred characters once its serial and profile are in it, and a
display identity on one platform is an operating-system device path bounded at 260. A name that
does not fit is truncated silently and then resolves to nothing, so the bound is set by the worst
case rather than by what a machine happens to report today.

`lowlat_get_audio_outputs` lists what sound could be captured from. **The identity it returns is
the monitor of an output, not the output**, because that is the device a host reads: it carries
what the speakers are playing. The name beside it is what a person calls the speakers, which is
what an application shows them. A machine with no sound server answers with none rather than
failing, which is the same thing an application does with the answer.

`lowlat_get_encoders` arrives when there is a choice worth reporting: the encoder follows the
display, so today the answer is a consequence rather than a menu. Adding a function is additive;
a call that answers nothing is worse than no call.

Two-call pattern: pass `NULL` to learn the count, then a buffer. **Nothing returned by this API
is heap allocated on the caller's behalf**, so there is no free function and no ownership
question. The caller owns every buffer it passes.

These are available before `lowlat_host_start`, so an application can present a configuration
interface before committing.

## §7 Status codes

A single `lowlat_status` enum spans success, warnings, and errors. Zero is success; negative
values are errors; positive values are non-fatal conditions such as `LOWLAT_TIMEOUT`.

**An enumeration for the names, and never a parameter.** Grouping the codes under a type is
what tells a reader that `LOWLAT_TIMEOUT` is a status and `LOWLAT_ATTEMPT_MAX` is a size, which
a header full of bare defines cannot. Accepting one back by value would be a different
question: reading a value nothing defined is undefined behaviour, and an application is free to
hand back any integer it holds. So statuses travel outward as the enumeration and every call
that takes one in declares a plain integer. `lowlat_status_string` is the case that makes it
obvious -- describing a code this version does not define is the reason somebody calls it.

Ranges are partitioned by subsystem so a numeric code identifies its origin without a lookup.
Codes are assigned once and never reused, including for removed conditions.

`lowlat_status_string` returns a static string. It never allocates and the pointer is valid
forever.

Signaling outcomes are typed rather than collapsed into a generic failure
([04 §8](04-signaling.md)), because the application's correct response differs per outcome.

## §8 Threading and reentrancy

- **Every call is safe from any thread**, except that `lowlat_host_poll_events` has one
  consumer.
- **No call blocks on a network operation.** The longest a call can take is a lock acquisition
  and a memory copy. `lowlat_host_poll_events` blocks only for its explicit timeout.
- **One lock covers the seam, and approving an attempt holds it** while a socket is bound and
  that guest's threads are started. That is milliseconds rather than the microseconds the line
  above promises, and it is stated here rather than fixed because the fix is worse: a second
  lock is a lock ordering, and a lock ordering is what produces the first deadlock the day
  somebody adds a call that needs both. Admission happens once per guest and never on a path
  that carries a frame.
- **The event queue is not behind that lock.** A poll that waits out its timeout must not stop
  every other call for the length of it.
- **The SDK never calls into the application** except through the log callback. There is no
  reentrancy contract to violate.
- **The SDK owns all its threads.** The application never provides one, and never runs our
  work on its own ([00-overview.md](00-overview.md) D3).
- `lowlat_destroy` may be called from any thread but not from inside the log callback.

## §9 Panics and unwinding

**Every `extern "C"` entry point catches unwinding.** A panic crossing the boundary is
undefined behavior, and this library loads into processes we do not control.

- A caught panic is logged with its location and returns `LOWLAT_ERR_INTERNAL`.
- The handle is marked poisoned. Subsequent calls return the same error rather than proceeding
  on state whose invariants may be broken. Only `lowlat_destroy` still works.
- **The shared library keeps unwinding enabled.** Building it to abort on panic silently
  disables all of the above, which is why the release profile is split
  ([AGENTS.md](../AGENTS.md) §17).
- A deliberately panicking call is a named test at [Phase 8](impl-plan.md).

## §10 Memory and strings

- **The caller owns every buffer.** Nothing crosses the boundary that the application must
  free.
- **Input strings are NUL-terminated `const char *`**, copied immediately. The API never
  retains a caller pointer past the call.
- **Output strings are written into caller-provided fixed arrays** inside structs, never
  returned as pointers. Sizes are named constants in the header.
- **All strings are UTF-8.**
- Binary payloads are pointer plus length, copied on the way in and copied into caller storage
  on the way out. Inbound, that storage is the buffer handed to the poll call ([§5](#5-events)).
- **There is no free function, and that is a property to preserve.** Every allocator that
  crosses a library boundary eventually meets an application built against a different runtime,
  and the failure is a corrupted heap rather than an error code.

## §11 Versioning

The ABI is **additive within a major version**:

- New functions may be added.
- New struct fields may be appended, guarded by the `size` field.
- New enum variants may be appended.
- New event types may be added.

Never, without a major version change: reordering or removing a field, renumbering a value,
changing a signature, or changing the meaning of an existing field.

`lowlat_abi_version` returns major and minor packed. A loader refusing a mismatched major is
correct; refusing a newer minor is not.

## §12 Bindings

The header generates from the Rust definitions, so it cannot drift from the implementation.
Header generation runs in continuous integration and a stale header fails the build.

**Every struct is blittable**: fixed-size fields, no pointers into managed memory, no nested
variable-length data. In C# that means plain sequential structs with no marshalling
directives, which is what makes source-generated interop work without a runtime marshaller.

**The header compiles standalone under both C and C++ with warnings as errors**, verified at
[Phase 8](impl-plan.md), because a header that only compiles in the author's translation unit
is a header nobody can use.

A C# integration is the reference case, since it exercises the signaling seam, event polling,
and struct blittability at once. It is the Phase 8 gate.

## §13 What is deliberately absent

- **No client API.** lowlat is a host. The far side is an existing client.
- **No signaling.** [04 §1](04-signaling.md).
- **No callbacks on data paths.** Frames and the sound a host sends never cross this boundary;
  the SDK captures and encodes internally. An application that wants the frames wants a
  different product. **A guest's microphone is the one exception and it is still not a
  callback**: it is polled, on a call of its own, and what crosses is samples.
- **No microphone device.** A shared library has no business creating a capture device in
  somebody's session -- it owns neither the session nor the naming nor the lifetime -- so what a
  host does with a guest's microphone is the application's decision. The SDK decodes and hands
  over sixteen-bit samples.
- **No configuration file parsing.** The application decides where configuration comes from
  and passes structs.
- **No threading knobs.** Thread counts scale to available parallelism and are not exposed.
  An application that wants fewer threads wants fewer guests.
- **No public Rust API.** Deferred indefinitely. A stable Rust surface is a second ABI to
  maintain, and the C one already works from Rust.

## §14 Verification status

**Ours by design:** all of it. The API shape follows an established convention, but no part of
this document is constrained by wire compatibility. It changes only for our reasons.

**Settled 2026-08-21, and the shape it took:** no resolution and no rotation, an `output`
identity, `fps` as a ceiling, and a split between what is settled at start and what changes
while the host runs (§3). Codec, encoder and congestion level are named by enumerations but
**carried as plain integers**, because the application writes those fields and reading one back
as a variant would be reading whatever it wrote; every one is checked at the boundary rather
than converted. Reflexive servers are a fixed array with a count rather than a pointer and a
length, so the structure stays one blittable block with nothing in it to free.

**Rotation is followed, not configured.** A display decides its own orientation exactly as it
decides its own size, so asking for one is the same request as asking for a mode. Nothing here
reads it from the display yet, so a stream is declared flat until something does; that is a gap
rather than a decision, and it is the one thing removing the field cost.

**Was open until Phase 8:** the concrete `lowlat_host_config` field set, which depends on the
capture backend decision in [07-platforms.md](07-platforms.md), and the status code range
partitioning, which wants the full error surface visible before it is fixed.

**One field of that set is already decided, because it is the only one whose meaning could not
be appended later.** There is no requested resolution. The display decides the picture's size,
the encoder follows it, and the application is told what it got -- through the status call and
the capture-changed event -- rather than asking for it. A host that creates its own display
chooses that display's size when it creates it, which is a different question from setting the
mode of a display somebody else owns, and that one is nobody's here
([impl-plan.md](impl-plan.md), *Output selection*). A frame rate is a **cap** over whatever the
display runs at, not a target.
