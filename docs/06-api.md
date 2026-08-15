# 06 - Public API

**Status:** locked 2026-08-15. Implemented by `lowlat-host`, generated as one C header.

**The C ABI is the only public surface.** There is no public Rust API, no C++ wrapper, and no
language-specific SDK. Every binding anyone will ever want consumes C: C#, Java, Swift,
Python, and Rust itself all speak it, and it is the one calling convention that survives across
compilers, runtimes, and toolchain versions.

## §1 Shape

Five rules, each of which removes a class of integration failure.

1. **Opaque handles.** The application holds a pointer it cannot dereference. Internal layout
   changes freely.
2. **Plain data structs with a leading `size` field.** The caller sets it. We read it and
   behave according to what the caller knows about, so a struct can grow without breaking
   binaries compiled against an older header.
3. **Stable-numbered enums.** Values are assigned once and never reused, never renumbered, and
   never reordered. New variants append.
4. **Poll based, not callback based.** The application asks for events on its own thread at its
   own cadence. No callback fires from inside our threads, so there is no reentrancy contract
   and no lock the application can deadlock against.
5. **Prefixed symbols.** Every exported name begins `lowlat_`. This is checked mechanically
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
const char   *lowlat_status_string(lowlat_status status);
uint32_t      lowlat_abi_version(void);
```

One handle owns one host session. `lowlat_destroy` stops hosting, disconnects every guest,
joins every thread, and returns only when all of it has happened.

`lowlat_abi_version` lets a loader verify the library matches the header it was built against
before calling anything else. It is the one function whose signature can never change.

The log callback is the single exception to rule 4. It is cold, it fires on whichever thread
logged, and it must not call back into the API.

## §3 Host

```c
lowlat_status lowlat_host_start(lowlat *ll, const lowlat_host_config *cfg);
void          lowlat_host_stop(lowlat *ll, const char *reason);
lowlat_status lowlat_host_get_status(lowlat *ll, lowlat_host_status *out);
lowlat_status lowlat_host_set_config(lowlat *ll, const lowlat_host_config *cfg);

uint32_t      lowlat_host_get_guests(lowlat *ll, lowlat_guest *out, uint32_t *count);
lowlat_status lowlat_host_kick_guest(lowlat *ll, uint32_t guest_id, lowlat_status reason);
lowlat_status lowlat_host_set_permissions(lowlat *ll, uint32_t guest_id,
                                          const lowlat_permissions *perms);
lowlat_status lowlat_host_set_input_enabled(lowlat *ll, uint32_t guest_id, bool enabled);

lowlat_status lowlat_host_send_user_data(lowlat *ll, uint32_t guest_id, uint32_t id,
                                         const void *data, uint32_t len);
```

`lowlat_host_set_config` applies live. Bitrate, frame rate, congestion mode, and guest limit
change without restarting the session. Changing the capture source or codec restarts the
video pipeline but not the sessions.

## §4 Signaling seam

The four calls from [04 §9](04-signaling.md). This is the entire contact surface between any
signaling implementation and the SDK.

```c
bool          lowlat_host_new_attempt(lowlat *ll, const lowlat_attempt_info *info);
void          lowlat_host_add_candidate(lowlat *ll, const char *attempt_id,
                                        const lowlat_candidate *cand);
lowlat_status lowlat_host_begin_p2p(lowlat *ll, const char *attempt_id, uint16_t port,
                                    lowlat_credentials *out);
void          lowlat_host_end_connection(lowlat *ll, const char *attempt_id,
                                         lowlat_status reason);
```

`lowlat_host_begin_p2p` writes host credentials into `out` for the application to send as its
answer. It does not send anything, because the SDK has no transport.

`lowlat_host_add_candidate` and `lowlat_host_end_connection` accept unknown attempt
identifiers silently. Those are races with teardown, not errors, and returning a status the
caller would have to ignore is worse than returning nothing.

## §5 Events

```c
lowlat_status lowlat_host_poll_events(lowlat *ll, uint32_t timeout_ms, lowlat_event *out);
```

Returns `LOWLAT_OK` with an event, or `LOWLAT_TIMEOUT` if none arrived. A `timeout_ms` of zero
polls without blocking.

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
  on the way out.

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

**Open until Phase 8:** the concrete `lowlat_host_config` field set, which depends on the
capture backend decision in [07-platforms.md](07-platforms.md), and the status code range
partitioning, which wants the full error surface visible before it is fixed.
