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
void          lowlat_set_log_callback(lowlat_log_fn fn, void *opaque);
const char   *lowlat_status_string(int32_t status);
uint32_t      lowlat_abi_version(void);
```

One handle owns one host session. `lowlat_destroy` stops hosting, disconnects every guest,
joins every thread, and returns only when all of it has happened.

`lowlat_abi_version` lets a loader verify the library matches the header it was built against
before calling anything else. It is the one function whose signature can never change.

The log callback is the single exception to rule 5. It is cold, it fires on whichever thread
logged, and it must not call back into the API.

## §3 Host

```c
lowlat_status lowlat_host_start(lowlat *ll, const lowlat_host_config *cfg);
lowlat_status lowlat_host_stop(lowlat *ll);
lowlat_status lowlat_host_get_status(lowlat *ll, lowlat_host_status *out);

lowlat_status lowlat_host_set_video_config(lowlat *ll, const lowlat_host_video_config *cfg);
lowlat_status lowlat_host_get_video_config(lowlat *ll, lowlat_host_video_config *out);

uint32_t      lowlat_host_get_guests(lowlat *ll, lowlat_guest *out, uint32_t *count);
lowlat_status lowlat_host_kick_guest(lowlat *ll, uint32_t guest_id, int32_t reason);
lowlat_status lowlat_host_set_permissions(lowlat *ll, uint32_t guest_id,
                                          const lowlat_permissions *perms);

lowlat_status lowlat_host_send_user_data(lowlat *ll, uint32_t guest_id, uint32_t id,
                                         const void *data, uint32_t len);
```

**There is no separate call to enable or disable a guest's input.** It was declared here and
removed 2026-08-21 before anything was built against it: it is `lowlat_host_set_permissions`
with every flag cleared, and two calls that write one field can disagree about what a guest is
allowed to do. Permissions are the field; there is one way to set them.

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
| `cg_level` | Every guest's controller is built with it. **Zero is the most aggressive, not "off"**: its threshold declares congestion on any stale fragment once the send window passes its floor, and it exists only for compatibility with an older scheme. |
| `base_port`, `servers` | Bound and consulted per attempt; moving them under running guests moves nothing that is already connected. |
| `max_guests` | Advertised capacity, read when a guest asks for a seat. |
| `exclusive_pointer`, `exclusive_hold_ms` | The pointer arbiter is built once with them. The hold is **clamped rather than refused**: it is a comfort setting, and the nearest usable value beats a host that will not start.

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
                                    lowlat_credentials *out);
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
answer. It does not send anything, because the SDK has no transport. **The port it reports is
the one that was bound**, which is not necessarily the configured one: the bind walks when a
port is taken, and advertising the configured port produces a peer that answers checks and
never establishes. It takes no port argument for that reason -- the port is an answer, not a
request.

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
| guest state changed | connecting, connected, disconnected, with reason |
| candidate | a local candidate to forward over signaling |
| user data | an application message from a guest |
| guest degraded | a guest is chronically behind ([05 §6](05-host.md)) |
| input owner changed | the most recent injecting guest changed |
| capture changed | resolution, refresh, or output topology changed |
| fatal | the session cannot continue |

**Poll from one thread.** The queue is single-consumer. Every other call is safe from any
thread.

Events are dropped oldest-first if the application stops polling, except `fatal`, which is
never dropped. A dropped-count field on the next delivered event makes the loss visible rather
than silent.

**The queue is bounded in bytes as well as in entries**, because one of the two is not a bound:
a body may reach a megabyte, so a queue limited only by how many events it holds is limited to
that many megabytes. Either ceiling evicts oldest-first and both count into the same field.

## §6 Enumeration

```c
lowlat_status lowlat_get_outputs(lowlat_output *out, uint32_t *count);
lowlat_status lowlat_get_audio_outputs(lowlat_audio_output *out, uint32_t *count);
lowlat_status lowlat_get_encoders(lowlat_encoder_info *out, uint32_t *count);
```

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
- **No callbacks on data paths.** Frames and audio never cross this boundary; the SDK captures
  and encodes internally. An application that wants the frames wants a different product.
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
