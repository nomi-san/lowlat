# 06 - Public API

**Status:** locked 2026-08-15. Implemented by `lowlat-sdk`, generated as one C header.

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
uint32_t      lowlat_abi_version(void);
uint32_t      lowlat_features(void);
const char   *lowlat_status_string(int32_t status);
lowlat_status lowlat_set_log_callback(lowlat_log_fn fn, void *opaque);
lowlat_status lowlat_set_log_level(uint32_t level);

lowlat_status lowlat_host_create(const lowlat_host_create_info *info, lowlat_host **out);
void          lowlat_host_destroy(lowlat_host *hl);
```

**One library, one header, two halves, and a handle type per half.** A host session is a
`lowlat_host`, made by `lowlat_host_create` and used by every `lowlat_host_*` call; a client
session is a `lowlat_client` in the same way ([§3b](#3b-client)). The two are distinct opaque
types rather than one handle in two roles, so a host call on a client handle fails to compile,
which is the same rule the symbol prefix enforces at link time (§1, rule 6). What takes no
handle -- the version, the features, the status text, the log -- belongs to neither half and
is in every build.

**Either half can be left out of a build.** A platform that can only be a client gets a
library with no host in it and none of the host's display stack compiled; the header hides
the same half when the application defines `LOWLAT_NO_HOST` (or `LOWLAT_NO_CLIENT`) before
including it, so a call into the missing half is a compile error rather than a link error.
A plain include declares everything. `lowlat_features` reports the halves of the library
actually loaded, as `LOWLAT_FEATURE_HOST | LOWLAT_FEATURE_CLIENT`, so a loader that resolves
names one at a time learns once what it lacks rather than at whichever name it reached first.
It reports one more thing decided at build time (minor 12): `LOWLAT_FEATURE_GPL_LIBAVCODEC`,
set by a library built with the `gpl-libavcodec` feature, whose software decoder loads a GPL
build of the machine's codec library as well as an LGPL one ([§3b](#3b-client)). The header
is one for every build, so the bit is the only way a loader learns what this one would load.

One handle owns one host session. `lowlat_host_destroy` stops hosting, disconnects every
guest, joins every thread, and returns only when all of it has happened.

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
lowlat_status lowlat_host_start(lowlat_host *hl, const lowlat_host_config *cfg);
lowlat_status lowlat_host_stop(lowlat_host *hl);
lowlat_status lowlat_host_get_status(lowlat_host *hl, lowlat_host_status *out);
lowlat_status lowlat_host_poll_microphone(lowlat_host *hl, uint32_t timeout_ms, int16_t *samples,
                                          uint32_t *count, uint32_t *guest, uint32_t *dropped);
lowlat_status lowlat_host_poll_pad_report(lowlat_host *hl, uint32_t timeout_ms, uint32_t *guest,
                                          uint32_t *pad, uint32_t *type, uint32_t *kind,
                                          uint8_t *report, uint32_t *len,
                                          uint32_t *dropped);               /* minor 10 */
lowlat_status lowlat_host_send_pad_report(lowlat_host *hl, uint32_t guest, uint32_t pad,
                                          uint32_t kind, const uint8_t *report,
                                          uint32_t len);                    /* minor 10 */

lowlat_status lowlat_host_set_video_config(lowlat_host *hl, const lowlat_host_video_config *cfg);
lowlat_status lowlat_host_get_video_config(lowlat_host *hl, lowlat_host_video_config *out);

lowlat_status lowlat_host_set_audio_config(lowlat_host *hl, const lowlat_host_audio_config *cfg);
lowlat_status lowlat_host_get_audio_config(lowlat_host *hl, lowlat_host_audio_config *out);

uint32_t      lowlat_host_get_guests(lowlat_host *hl, lowlat_guest *out, uint32_t *count);
lowlat_status lowlat_host_kick_guest(lowlat_host *hl, uint32_t guest_id, int32_t reason);
lowlat_status lowlat_host_set_permissions(lowlat_host *hl, uint32_t guest_id,
                                          const lowlat_permissions *perms);

lowlat_status lowlat_host_send_user_data(lowlat_host *hl, uint32_t guest_id, uint32_t id,
                                         const void *data, uint32_t len);
lowlat_status lowlat_host_send_roster(lowlat_host *hl, const void *data, uint32_t len,
                                      uint32_t *reached);
lowlat_status lowlat_host_get_metrics(lowlat_host *hl, uint32_t guest_id, lowlat_metrics *out);
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

**Named channels, not an array of streams.** A number here would be a stream index, and this
host produces one stream and switches which display feeds it, so there is nothing to index.
What genuinely differs between these figures is the channel -- control, sound, video -- and
each gets a `lowlat_channel_metrics` of its own: fragments sent, retransmissions by cause, the
rate, what the payload cost to produce and what the peer says it costs to decode.

**Shared figures appear once.** The smoothed round trip is the path's and there is one path
under every channel, so it sits beside the channels rather than being repeated in each. So does
the congestion count: video is the only channel a rate controller steers, and a count reported
against sound or control would be a number with nothing behind it.

**They report what this host can answer for, and one thing it cannot.** How many frames a peer
has queued waiting to decode is the peer's to know and is not here. Decode time is the
exception: a peer volunteers it, and this host stores what it was told rather than deriving
anything, so the field is the peer's own figure and reads zero until one arrives. Everything
else is what the congestion controller already reads -- outstanding fragments, how many are past
due, the measured rate, encode time -- plus when each kind of input last arrived, which is the
one question an application kicking idle guests can ask nobody else. **Zero means never, which
is not zero milliseconds ago.**

**Counters pin rather than wrap.** They are cumulative for the life of a guest and reported in
thirty-two bits; a count that wrapped would read as a session that had just started, which is
wrong by an unknowable amount where the ceiling is wrong by a knowable one.

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

**`lowlat_host_poll_pad_report` is the same shape for a pad's reports** (minor 10;
[05 §7.2](05-host.md)), and exists for an application that owns the virtual device: with
`pad_sink` set to the application, a DualShock 4's or a DualSense's reports come out here --
the guest, the pad, the product, the kind, then the report in the pad's USB form -- feature
reports ahead of the first input report, **and the pad's end after its last report**
(`LOWLAT_PAD_REPORT_UNPLUG`, no report, on the guest's unplug and on its leaving), which is
what the application destroys its device on; `lowlat_host_send_pad_report` carries back what
that device was written. Two hundred and fifty reports a second is the microphone's rate,
so it is the microphone's queue: bounded, the oldest input report dropped and counted when
the application falls behind, never a feature report or a pad's end. The wait is the same
wake the microphone's is, so the cost over the library's own device is one cross-thread wake
(measured, push to return, 13 us at the median and 26 us at the ninety-ninth percentile); an
application that polls it with a zero timeout from a timed loop adds the loop's period,
which is why the header says to park a thread on it.

**It refuses rather than waits when the microphone is not accepted.**
`LOWLAT_ERR_NOT_STARTED` comes back immediately: a host that is not taking microphones will
never have one, and spending the caller's timeout to say so would read as sound that is merely
late.

**`lowlat_host_get_status` reports what is happening, not what was asked for**, which is why
it carries the picture's size and the guest count and not the settings that produced them: the
display decides its own size and the room decides its own occupancy. It answers on a handle that
is not hosting too, with `running` clear -- an application asking what state something is in
should not have to know the answer to ask.

**The live codec, chroma and depth are there for the same reason**, and they are the half the
settings genuinely cannot express: a seated guest may ask for a different codec or a different
depth mid-session and the host rebuilds the encoder to match ([05 §6.1](05-host.md)), so what
the configuration holds is what was asked for at the start and what this holds is what is
coming out now. `codec` and `chroma` are enumerations, `ten_bit` is a flag -- **an enumeration
where the axis can grow and a flag where it cannot**: a codec may gain another entry and chroma
already has a third layout in wide use, while no encoder on this platform offers a depth above
ten and one of them cannot express one. All three read zero, zero and clear on a handle that is
not streaming, the same way the picture's size does, because until then there is nothing
truthful to report.

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
| `fps` | yes | A **ceiling** over the display's own rate, not a target, and **clamped to it**: a rate above what the display presents is one the loop will not reach while still being the number the encoder's per-frame budget is divided by. Zero asks for the display's own rate. Changes the pacing from the next frame. |
| `bitrate_mbps` | yes | Re-bases the rate budget and reaches the encoder through the reconfigure the rate loop already performs every pass. No keyframe, no interruption ([00 §D8](00-overview.md)). |
| `min_bitrate_mbps` | yes | The floor congestion control may not descend below, and it **moves down with the ceiling**: a ceiling lowered under a floor that stayed leaves every controller pinned at a rate the operator just asked not to exceed. |
| `full_fps` | yes | Emit at `fps` even when the picture has not changed. **Clearing it is a permission, not an instruction** -- there is no damage signal here, so nothing yet skips a repeated picture, and continuing to send costs bitrate rather than being wrong. |
| `output` | yes | The exception in cost rather than in kind: a picture from another output cannot be absorbed into a stream built for one, so it rebuilds around the new source for **one coded refresh** and every guest keeps its seat and its channel. |
**A configuration nobody filled in is not a zeroed one.** Every enumerated field here is
validated rather than clamped, so a structure the caller zeroed is a *valid* request for the
first variant of each -- including the most aggressive congestion level -- and the boundary
cannot tell that apart from an application that meant it. `lowlat_host_config_default` returns
what a host would choose for itself, `size` fields included, and is the thing to start from and
overwrite. **A null configuration to `lowlat_host_start` means exactly those defaults**, which
is the one reading of a null pointer there that cannot be a mistake.

Everything in `lowlat_host_config` outside that structure is settled at `lowlat_host_start`:

| Field | Why not live |
|---|---|
| `codec` | One encode serves every seat and a session has one video configuration ([00 §D11](00-overview.md)). |
| `encoder` | A consequence of where the display is rather than a preference, and changing it rebuilds the pipeline. Absent means **follow the display**, which is the right default; choosing one is an override. |
| `quality` | One of [`lowlat_quality`](#quality). It is what the encoder is built with, and one encode serves every seat, so moving it under a running session would change the picture every guest is watching on one guest's behalf. |
| `cg_level` | Every guest's controller is built with it. **Zero is the most aggressive, not "off"**: its thresholds are all zero, so every outstanding fragment classifies stale and congestion is declared on every pass once the send window passes its floor. The default is *sensitive*, and *adaptive* runs the same tuning while reserving a place for host-local signals (§10). |
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

## §3b Client

**Planned 2026-09-15, built from 2026-09-17 by [impl-plan-client.md](impl-plan-client.md).**
Everything below is in the header (minor 4 the session, minor 5 the pictures, minor 6 the
input, minor 7 the sound, minor 8 the preferences and the handle, minor 9 the cursor and the
metrics, minor 10 the pad reports, minor 14 the relay); the header is the truth.

```c
lowlat_status lowlat_client_create(const lowlat_client_create_info *info, lowlat_client **out);
void          lowlat_client_destroy(lowlat_client *cl);

lowlat_status lowlat_client_new_attempt(lowlat_client *cl, const lowlat_client_config *cfg,
                                        const char *attempt_id, uint32_t transport,
                                        lowlat_credentials *ours);
void          lowlat_client_add_candidate(lowlat_client *cl, const char *attempt_id,
                                          const lowlat_candidate *cand);
lowlat_status lowlat_client_begin_p2p(lowlat_client *cl, const char *attempt_id,
                                      const lowlat_credentials *theirs);
void          lowlat_client_end_connection(lowlat_client *cl);

lowlat_status lowlat_client_send_user_data(lowlat_client *cl, uint32_t id,
                                           const void *data, uint32_t len);
lowlat_status lowlat_client_get_status(lowlat_client *cl, lowlat_client_status *out);
lowlat_status lowlat_client_poll_events(lowlat_client *cl, uint32_t timeout_ms,
                                        lowlat_event *out, void *body, uint32_t *body_len);

lowlat_status lowlat_client_acquire_frame(lowlat_client *cl, uint8_t stream, uint32_t timeout_ms,
                                          lowlat_frame *out);
lowlat_status lowlat_client_release_frame(lowlat_client *cl, const lowlat_frame *frame,
                                          const lowlat_fence *done);

lowlat_status lowlat_client_set_viewport(lowlat_client *cl, int32_t x, int32_t y,
                                         int32_t w, int32_t h);
lowlat_status lowlat_client_send_key(lowlat_client *cl, uint32_t code, uint32_t mods,
                                     bool pressed);
lowlat_status lowlat_client_send_mouse_button(lowlat_client *cl, uint32_t button,
                                              bool pressed, int32_t x, int32_t y);
lowlat_status lowlat_client_send_mouse_wheel(lowlat_client *cl, int32_t x, int32_t y);
lowlat_status lowlat_client_send_mouse_motion(lowlat_client *cl, int32_t x, int32_t y,
                                              bool relative);
lowlat_status lowlat_client_send_pad_button(lowlat_client *cl, uint32_t pad, uint32_t button,
                                            bool pressed);
lowlat_status lowlat_client_send_pad_axis(lowlat_client *cl, uint32_t pad, uint32_t axis,
                                          int16_t value);
lowlat_status lowlat_client_send_pad_state(lowlat_client *cl, uint32_t pad,
                                           const lowlat_pad_state *state);
lowlat_status lowlat_client_send_pad_unplug(lowlat_client *cl, uint32_t pad);
lowlat_status lowlat_client_send_pad_report(lowlat_client *cl, uint32_t pad, uint32_t type,
                                            uint32_t kind, const uint8_t *report,
                                            uint32_t len);                  /* minor 10 */
lowlat_status lowlat_client_send_release_all(lowlat_client *cl);

lowlat_status lowlat_client_acquire_audio(lowlat_client *cl, uint32_t timeout_ms,
                                          int16_t *samples, uint32_t *count);

lowlat_status lowlat_client_set_video_config(lowlat_client *cl, const lowlat_client_video_config *cfg);
lowlat_status lowlat_client_set_decoder(lowlat_client *cl, uint32_t decoder,
                                        const char *device);                /* minor 11 */
lowlat_status lowlat_client_get_metrics(lowlat_client *cl, lowlat_client_metrics *out);
```

**Minor 11 (2026-09-21): the software decoder, and a decoder chosen mid-session.**
`lowlat_client_set_decoder(cl, decoder, device)` takes the kind and render node of creation
and of the listing's rows (`LOWLAT_DECODER_AUTO` walks the automatic order again). The
decoder is probed there and then, on the caller's thread, exactly as creation probes; a
kind that does not open answers with its stage and **nothing changes** -- the running
decoder keeps decoding. Before an attempt the choice is replaced and that is all. During a
session it is one act: the declaration re-masked by the new decoder's capability and
restated to the host where it changed, the running decoder torn down, the new one opened,
and one keyframe request with the reinitialisation argument once the new decoder exists --
so a keyframe never arrives for a decoder that is still opening, and a runtime that fails to
open costs the host nothing; the picture resumes at the next keyframe, a picture the
application holds stays valid, the queue never closes. Costs the host one keyframe and an
established host an encoder rebuild, so it is for a person changing a setting. **The frame
kind stays the creation's**: a session of `LOWLAT_FRAME_HANDLE` refuses the call with
`LOWLAT_ERR_DECODER_UNSUPPORTED`, because its device slots are bound to the device; changing
that is a recreate. `LOWLAT_DECODER_NONE` is refused the same way. Measured against this
host at 2560x1440: the open stack to the vendor's, to software, and round again every
hundred seconds, each answered by exactly one keyframe and the picture back within the
second.

`LOWLAT_DECODER_SOFTWARE` names the
machine's own codec library -- loaded at runtime, never linked, and **only when the library
answers that it is an LGPL build**: before any other entry point is called it is asked its
licence, and a build that answers otherwise is closed unused and refused with
`LOWLAT_ERR_NO_DECODER_LICENCE`, a status of its own, so the person who has a codec library
installed and is refused is told why rather than told to install one. **A library built with
the `gpl-libavcodec` feature loads a GPL build as well** (*added 2026-09-21*, minor 12): the
default build never does, and a build that opts in is its maker's combination, to which the
GPL's terms apply; it says so through `lowlat_features` (`LOWLAT_FEATURE_GPL_LIBAVCODEC`),
and the listing names whatever was loaded with its licence. A build answering `nonfree` is
refused by every build. The pair is looked for
in the environment (`LOWLAT_FFMPEG_DIR` a directory, `LOWLAT_FFMPEG_VERSION` a major of 4
through 9; a pair named there that fails is the answer, never a walk), in the directory
`lowlat_client_create_info.device` names when it names one, beside the running executable,
then the linker's own way; the highest major that opens wins. It hands out the same four
formats the hardware decoders do, planes only, so an application that draws NV12 and P010
draws this decoder's pictures unchanged; `LOWLAT_FRAME_HANDLE` on it is refused at creation.
`LOWLAT_DECODER_AUTO` reaches it last, after the open stack's nodes and the vendor's devices,
so a machine with any hardware decoder never does; status `backend` says which was chosen.
`lowlat_enum_decoders` lists it last, `name` carrying the library's version and its licence
and `device` the directory it came from (empty for the linker's own search), every
capability true where the library opens the second codec; a row's `decoder` and `device` open
exactly that pair. Two corrections to the automatic order under the same minor: with a
render node named it now tries the vendor's interface on the card behind that node after the
open stack refuses (it stopped at the open stack), and the handle kind with a node named
resolves the vendor's device from that node rather than taking any. Nothing moves.

**Minor 9 (2026-09-19, evening): the cursor, rumble, the guest list, the client's
metrics.** Three events join §5's set. `LOWLAT_EVENT_CURSOR` carries the pointer as the
host described it -- hidden, relative, suppressed, the position it reappears at in window
units, the hotspot in the picture's own pixels, the picture's size and its checksum -- and,
when a picture came or was named from the cache, **a pointer to the decoded RGBA in a buffer
the handle owns, valid until the next `poll_events` on that handle**: the one event that
carries a pointer, because a pointer picture at its ceiling is a megabyte and arrives a few
times an hour, which is the wrong shape for a caller's scratch buffer and the `TOO_SMALL`
retry ([§5](#5-events)); `image_update` says whether one is there. The picture is at its
native size and the library scales nothing: the application resamples picture and hotspot
by the ratio of its rectangle to the picture, as an established client does
([10 §7](10-client.md)). A picture the reader cannot take (anything but 8-bit RGB or RGBA,
non-interlaced, up to 512 square) is dropped and counted in status, the rest of the update
delivered; **the picture already delivered, named or sent again, travels as its checksum
alone**, so a host that repeats the name on every update does not have it decoded on every
update -- the application keeps the picture it was given. `LOWLAT_EVENT_RUMBLE` names the
pad the application reported and the two motors as eight-bit values. `LOWLAT_EVENT_GUEST_LIST` carries the recipient's own number and the
list's body through the caller's buffer, exactly as user data does; the library reads
nothing in it, and `lowlat_client_status.number` carries the same number for a late reader.
`lowlat_client_get_metrics` fills `lowlat_client_metrics { size, connected_ms, rtt_ms,
control, video, audio }`, each channel a `lowlat_client_channel_metrics { fragments, late,
duplicates, out_of_window, nacks_sent, bytes, messages, loss_30s }` as [10 §9](10-client.md)
defines them -- the receiver's own figures under the receiver's names; the host's figures for
this guest reach the application through the guest list, on the host's `lowlat_metrics`,
so a panel shows both ends of one path from the structure each end can fill. Status gains
`number` and the pointer's three counts (pictures delivered, names not held, pictures
refused).

**Minor 10 (2026-09-20, C7 and Phase 14): the pad reports.** The client half is C7.1, the
host half 14.2.
`lowlat_client_send_pad_report(cl, pad, type, kind, report, len)` hands over a DualShock 4's
or a DualSense's own report (`type` one of `lowlat_pad_type`: `LOWLAT_PAD_TYPE_DS4`,
`LOWLAT_PAD_TYPE_DS5`; `kind` one of `lowlat_pad_report`: `LOWLAT_PAD_REPORT_INPUT` the input
report as the device delivered it, USB or Bluetooth form, or `LOWLAT_PAD_REPORT_FEATURE` a
feature report read from the pad -- calibration or firmware, identifier in byte 0, sent
before the first input report; at most `LOWLAT_PAD_REPORT_MAX` bytes). The library derives
and sends the standard state beside the report, so `send_pad_state` is never called for such
a pad and is refused for its identifier until `send_pad_unplug`, as a report is refused for a
pad already sent as states -- `LOWLAT_ERR_INVALID_ARGUMENT` either way, at the call
([10 §8](10-client.md)). `LOWLAT_EVENT_PAD_REPORT` carries back what the host's device was
written (`lowlat_pad_report_event { pad, kind, len, report }`, `kind`
`LOWLAT_PAD_REPORT_OUTPUT` or `LOWLAT_PAD_REPORT_FEATURE`), an output report already in the
pad's own framing and a feature write in the USB form, **a pointer valid until the next
`poll_events`** as the cursor's picture is; the application writes it to the pad.
`LOWLAT_EVENT_RUMBLE` is unchanged and still arrives for any pad a host rumbles that way.
Status gains `pad_reports_sent`, `pad_reports_received` and `pad_reports_dropped` (received
for a pad this client never sent as reports).

On the host half ([§3](#3-host)): `lowlat_host_config.pad_sink` (`LOWLAT_PAD_SINK_DEVICE`,
the default, or `LOWLAT_PAD_SINK_APP`), `lowlat_host_poll_pad_report(hl, timeout_ms, &guest,
&pad, &type, &kind, report, &len, &dropped)` and `lowlat_host_send_pad_report(hl, guest, pad,
kind, report, len)`, the pair an application uses when it owns the virtual device
([05 §7.2](05-host.md)); a poll of its own, like the microphone's, parked on the same wake --
call it from a thread that waits on it, not from a loop that polls it. `lowlat_pad_report`
gains `LOWLAT_PAD_REPORT_UNPLUG`, the pad's end, which that poll delivers after the pad's
last report; the pad enumerations are in the half both sides share. No status field for the
report pads: the guest's own line names the message when it first arrives.

**Minor 8 (2026-09-19): the preferences, the second backend, the handle, the decoders
listed.** `lowlat_client_config` gains a `video` block, `lowlat_client_video_config {
resolution_x, resolution_y, hevc, ten_bit, chroma_444 }` -- the size request moves into it
from the top level, and the three booleans are preferences, "this if the host has it", masked
by what the decoder opened at creation decodes before anything is declared
([10 §7](10-client.md)); one block for the one stream. `lowlat_client_set_video_config` takes
the same block mid-session: the new declaration goes out with a reinitialisation request and
the decoder is torn down, so the next keyframe builds one for whatever the host now sends.
Status gains what was asked, what was declared and what the stream is (`asked_flags`,
`declared_flags`, `stream_format` beside the codec). Two planar formats join the two:
`LOWLAT_FORMAT_YUV444` and `LOWLAT_FORMAT_YUV444_16`, three planes at full size, ten bits in
the high bits of sixteen as `P010` holds them. `LOWLAT_DECODER_VENDOR` is accepted at
creation.

`lowlat_frame` gains its handle: `handle_kind` (`LOWLAT_HANDLE_OPAQUE_FD` now, a buffer
descriptor with modifier reserved), `fd`, `handle_size`, `modifier`, an `allocation` number,
and each `lowlat_plane` an `offset`; a frame of kind handle leaves the plane pointers null.
The descriptor is the library's for the lease and is closed when the allocation behind it is
freed, which happens after the last hold on it is released, so an application that keeps one
past a release duplicates it -- and an import that takes ownership of what it is given (GL's
does) is given a duplicate. **The application's import is keyed by `allocation`, not by the
descriptor's number**: numbers are reused once closed, so the number alone cannot tell a new
allocation from an old one; the ordinal can, and it also says which imports may be dropped.
`frame_kind = LOWLAT_FRAME_HANDLE` is accepted for the vendor backend (and settles
`LOWLAT_DECODER_AUTO` on it, since nothing else exports) and refused with
`LOWLAT_ERR_DECODER_UNSUPPORTED` for the open stack in this minor.

`lowlat_enum_decoders(index, out)` lists what this machine can open ([§6](#6-enumeration)):
one `lowlat_decoder_info` per backend and device that decodes anything, carrying what
creation takes to open exactly that one (`decoder`, `device`), what it decodes (the five
capability rows), the size limits per codec, whether it hands out a handle, and the device's
own name for a label.

**Creation names the decoder** (minor 5). `lowlat_client_create_info` carries the backend by
kind (`LOWLAT_DECODER_AUTO`, the open interface, the vendor's from minor 8, software from
minor 11, or `LOWLAT_DECODER_NONE` for a client with nowhere to draw -- a test peer, a
probe), the frame kind asked for, a ceiling on the picture the slots are sized for (4096
square when zero), and a render node (the first that decodes when empty; for software, the
directory of the library pair). The decoder is opened here, not at the attempt, so a machine
without one is refused at creation with the stage named: `LOWLAT_ERR_NO_DECODER_RUNTIME`,
`_DEVICE`, `_PROFILE` or `_LICENCE`.

**The seam is the host's, mirrored.** A client makes the offer: `new_attempt` produces the
credentials and certificate digest the application puts in it (its `port` is zero -- the
socket does not exist until the answer), candidates come out as events for the application to
relay, and `begin_p2p` takes what the answer carried. One attempt at a time. The
configuration is the attempt's, given with it: the size asked of the host (zero for none,
and it is a request to change the host's display, not a description of this one),
uncompressed sound, reflexive servers, and `legacy_cipher`, which leaves the media key out of
the offer so both ends key the older 128-bit cipher from the host's certificate digest; with
it clear the session is keyed from the host's media key in the answer, and an answer without
one takes the legacy path regardless. Nothing in the library speaks to a signaling service
(D3); the example client does, itself. `end_connection` says goodbye on the control channel
and gives the message a moment to arrive; it raises no event, because the application caused
it.

**A relay makes the attempt a relay attempt** (minor 14; [03 §7](03-connectivity.md),
[10 §11](10-client.md)). `lowlat_client_config.relay` names it as `host:port`, resolved to its
first IPv4 address, with `relay_username` and `relay_password` beside it; a relay without both,
or whose name does not resolve, is refused as an invalid argument while the caller can still
fix it. A relay attempt asks no reflexive server and raises no host candidate: its one
candidate event is the relayed address, marked as a reflexive server's report, followed by the
readiness event, and both come only once the relay has allocated and permitted its own
machine. `lowlat_client_status` carries the relayed address and port once there is one, and
`relayed`, whether the path goes through the relay. A relay that does not answer in time ends
the attempt with `LOWLAT_OUTCOME_RELAY_UNREACHABLE`, one that refuses the credential or the
allocation with `LOWLAT_OUTCOME_RELAY_REFUSED`, and one that lets the allocation go mid-session
with `LOWLAT_OUTCOME_RELAY_LOST`; nothing is retried. The credential is never logged, the
library clears every copy it takes, and a clean leave releases the allocation once the
departure has gone through it.

**The seam's types are shared.** `lowlat_candidate`, `lowlat_credentials`,
`lowlat_transport`, `lowlat_event` and its bodies are declared for either half, so an
application built against a library carrying one half sees the same types as one built
against the other. `lowlat_client_status` says where the session stands -- idle, connecting,
established, over -- with the host's disconnect status, the round trip, how far behind the
reader is and for how long, and what has been taken off each channel.

**Pictures are acquired and released, never called back with.** `acquire_frame` is the poll:
it waits up to its timeout for a picture newer than the last one lent, discards older ready
ones, and lends the newest. It waits outside the handle's lock, as the event poll does, so
status stays answerable meanwhile. At most two are held per stream -- the one being presented
and the one just acquired, so a swap has no gap -- and a third acquire is refused with
`LOWLAT_ERR_TOO_MANY_HELD` rather than silently dropping one. `release_frame` may carry a
fence the application's device signals when it has finished reading, which is what lets a
decoder write into shared memory without waiting on the application's CPU; a null fence
means reusable now, and **in this minor it is the only fence**: a picture of the planes kind
was copied, and one of the handle kind was copied on the device before the acquire returned
(§4 of [10](10-client.md)), so `lowlat_fence` has one kind, none, and a fence of any other
kind is refused. The rule 5 of §1 holds: no callback fires from inside the library.

**`lowlat_frame` carries either planes or a handle**, and says which. Planes are pointers,
pitches and a format (`LOWLAT_FORMAT_NV12`, `LOWLAT_FORMAT_P010`) into memory valid for the
lease; a handle is a device-level reference -- a buffer descriptor and layout modifier, or a
shared texture and fence -- the application imports into its own device. The application
names the kind it wants in `lowlat_client_create_info` and is told the kind it got; a decoder
that cannot export lends planes, and in this minor every decoder does. Every picture also
carries size, rotation, generation and a sequence number -- a gap between two consecutive
presents is a skip -- so a renderer needs nothing from the stream itself.

**Sound is decoded, not played** (minor 7). `acquire_audio` hands out one packet a call,
signed sixteen-bit stereo at 48 kHz, in the order the host sent them, as many frames as the
packet held -- 960 for a host at 20 ms, at most 8000; `count` is the room in frames going in
and the frames written coming out, and a buffer too short is told the need with
`LOWLAT_ERR_TOO_SMALL` and the packet kept for the next call. The decode runs on the caller's
thread, inside the call, and the wait for a packet is outside the handle's lock, as the
picture's is; two threads calling at once take turns. Nothing paces it: the device is the
application's and its buffer is the playback window ([10 §6](10-client.md)). Packets the
application has not taken wait in a pool of 32, past which the newest is dropped and counted
in status, which is a caller that stopped calling. Status carries the packets decoded,
dropped and refused, those waiting, the last packet's age between the wire and the call, and
the codec the decoder was built for (`LOWLAT_AUDIO_OPUS`, `LOWLAT_AUDIO_PCM`).

**Input is one call per kind** (minor 6): key, mouse button, wheel, motion, pad button, pad
axis, pad state, pad unplug and release-all, each with its arguments in the signature rather
than in a tagged structure, so a call site is checked where it is written and a binding
needs no union; the library transforms, guards and encodes ([10 §8](10-client.md)). **The application says where it drew
the picture** with `set_viewport`, a rectangle in the same units as the positions it reports,
and that is all the library knows about the window: no fit is computed, so stretching,
shrinking, a percent scale and a rotated picture are the application's ways of producing one
rectangle, and a display scale factor never enters; the picture's own size is the stream's.
Keys are usage codes with the modifier mask in the wire's bits (`LOWLAT_MOD_*`), the lock
bits included because a host keeps its own locks in step from them; mouse buttons, pad
buttons by index, pad axes and pad state bits have their `LOWLAT_*` names, and the two pad
forms number the buttons differently on purpose, as the wire does. A press outside the
rectangle is not sent and a release always is; a key of code zero is not sent; an unchanged
pad state is not repeated. **The call never blocks**: reports cross a fixed ring to the
session thread, and a ring that fills -- a thread that is not running -- drops the newest and
counts it in `lowlat_client_status.input_dropped`.

**Status and metrics mirror the host's** (§3): the same named channels, seen from the
receiving side, plus the decoder's state (none yet, built, failed), the backend in use, the
codec the decoder was built for, the queue depth, the last picture's decode and read-back
times, the host's own encode time as it last reported it, the count decoded and the bytes
taken off the video channel (a rate is a difference over time, the application's clock), so
one panel serves both ends. A decoder that fails past recovery ends the session with
`LOWLAT_OUTCOME_DECODER_FAILED`.

**Events** add to §5's set: cursor (the decoded picture from the handle's own buffer,
hotspot, suppressed; minor 9), relative mode (`LOWLAT_EVENT_RELATIVE`, on the transition
alone, with the position the pointer reappears at on the way out in the window's units, so
the application confines and hides its pointer on entry and warps it once on exit), blocked
and unblocked, rumble, the guest list, stream ended with a reason, host mode.

## §4 Signaling seam

The four calls from [04 §9](04-signaling.md). This is the entire contact surface between any
signaling implementation and the SDK.

```c
lowlat_status lowlat_host_new_attempt(lowlat_host *hl, const lowlat_attempt_info *info);
void          lowlat_host_add_candidate(lowlat_host *hl, const char *attempt_id,
                                        const lowlat_candidate *cand);
lowlat_status lowlat_host_begin_p2p(lowlat_host *hl, const char *attempt_id,
                                    uint16_t port, lowlat_credentials *out);
void          lowlat_host_end_connection(lowlat_host *hl, const char *attempt_id);
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

**The offer's media key selects the cipher, by presence alone.** An offer registered with an
empty `aes256` comes from a peer generation that has no such field: the session keys from the
answer's fingerprint under the legacy 128-bit mode, and `out->aes256` comes back empty -- the
application relays no media key, because the peer has no field to read one from. An offer that
carried one takes the 256-bit mode, keyed from the answer's media key as usual. The peer's own
material is never the key either way; presence is the whole of what it says.

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

**Which pipe an attempt speaks is the offer's to say**, and the application copies it across in
`lowlat_attempt_info.transport` as a `lowlat_transport` value -- `LOWLAT_TRANSPORT_BUD`, the
native transport and the zero a cleared structure carries, or `LOWLAT_TRANSPORT_WEB`, a
browser's data channel on the same socket ([01 §14](01-protocol.md)). On the browser pipe the
application also copies the peer's certificate digest into `fingerprint`, with or without its
hash name; registering without one is refused with `LOWLAT_ERR_FINGERPRINT`, because there
would be nothing the handshake could be checked against. On the native pipe the field is not
read. The credentials that come back differ in the same two places: a browser's answer carries
this process's certificate digest with its hash name in `fingerprint` and an empty `aes256`,
which is the truth about a pipe that keys itself.

**Both fields were appended in minor 2 and are read only when `size` says they exist.** An
application built against minor 1 sets the size that structure had and registers a native
attempt with no digest, whatever lies past its allocation; the boundary reads the structure
field by field and never through a reference to the whole of it. A `transport` value nothing
defines is refused as an invalid argument rather than defaulted.

`lowlat_host_add_candidate` and `lowlat_host_end_connection` accept unknown attempt
identifiers silently. Those are races with teardown, not errors, and returning a status the
caller would have to ignore is worse than returning nothing. A withdrawal that arrives before
the offer it withdraws is **remembered**, so admitting that offer afterwards is refused with
`LOWLAT_ERR_WITHDRAWN` rather than spending a socket and a thread on a guest already gone.

**A candidate marked `sync` is a readiness marker rather than an address**, and whatever
address rides along is ignored -- so it alone is accepted without one. A peer may withhold every
real candidate until it has seen one.

**A candidate carries the exchange's two markings, `lan` and `reflexive`, copied verbatim
from the signaling** (2026-08-29). `reflexive` steers the path-opening probe: it goes only
toward such a candidate, because that is the path that crosses translation. `lan` names an
address that is directly routable -- a host address, or any IPv6 one -- and wins when both
are set. Neither set is a real class of its own, a translated-path guess no server verified,
so all-zero is safe and means exactly that. The outbound candidate event carries the same
two flags, decided by the boundary -- IPv6 goes out marked `lan` whichever probe discovered
it -- so an application relays both directions without interpreting either.

**`lowlat_host_end_connection` takes no reason**, for the same reason `lowlat_host_stop` does
not: ending stops the guest's loop, and the far side learns from its own liveness deadline
rather than from a message. The disconnect status the protocol carries exists; nothing calls it
on the way down yet.

## §5 Events

```c
lowlat_status lowlat_host_poll_events(lowlat_host *hl, uint32_t timeout_ms, lowlat_event *out,
                                      void *body, uint32_t *body_len);
```

Returns `LOWLAT_OK` with an event, or `LOWLAT_TIMEOUT` if none arrived. A `timeout_ms` of zero
polls without blocking.

**An event that carries a body is handed it through the caller's own buffer**, which is
why the poll call takes one. `body_len` is the buffer's capacity going in and the bytes written
coming out; `NULL` means the application does not want bodies, and one that arrives is dropped
with the loss counted like any other. **The event itself never carries a pointer**, only the
body's length, so the union stays blittable ([§12](#12-bindings)) -- with one exception on
the client half, the cursor's decoded picture ([§3b](#3b-client), minor 9), which points
into a buffer the handle owns and is valid until the next poll on that handle: a megabyte at
its ceiling and rare, the wrong shape for a scratch buffer sized per poll.

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
| blocked | the host blocked this client's input, or unblocked it (client) |
| stream ended | the host ended one stream and not the session (client) |
| host mode | the host said which mode it is in (client) |
| relative | the host took the pointer or gave it back, with where it reappears (client) |
| cursor | the host's pointer changed: its picture, hotspot or flags (client, minor 9) |
| rumble | the host asked a pad to vibrate (client, minor 9) |
| guest list | the room, as the host describes it, with this client's own number (client, minor 9) |
| pad report | the host's virtual pad was written: an output report or a feature write, in the pad's own framing (client, minor 10) |

**A guest's state changes are the four attempt events**, not one event with a
state field: candidate and ready while it negotiates, established when a path is found, ended
with a typed outcome. Splitting them is what lets an application respond to each without
switching on a state inside a state. One outcome belongs to the browser pipe alone:
`LOWLAT_OUTCOME_HANDSHAKE_FAILED`, a path that existed and a pipe on it that did not -- the
security handshake never completed, the peer was not the one the offer named, or its
association ended with an error. The native pipe has no such outcome, because its records
either authenticate or they do not, and a peer that stops is `PEER_GONE`.

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

**The decoders are a menu, so they are listed** (minor 8). `lowlat_enum_decoders(index, out)`
fills the `lowlat_decoder_info` of slot `index` and answers true, or false past the table's
end, so a caller iterates from zero until false; the shape is the one an established client
SDK's decoder list has. **The table is fixed and a call probes one slot** (*corrected
2026-09-22*, minor 12): on Linux, slots 0 to 7 are the open interface on render nodes
`renderD128` to `renderD135`, 8 to 15 the vendor's on its devices by ordinal, 16 the
software decoder from the codec library's own search (an LGPL build; a GPL one too with
`LOWLAT_FEATURE_GPL_LIBAVCODEC`, §3b). The same slot means the same thing on every machine
and every call, and nothing is remembered between calls: a call opens its one slot exactly
the way creation opens it and closes it again, so a loop costs every slot once -- measured
on the development machine, 5 ms for the open interface on a node, microseconds for a node
or an ordinal that is not there, under a millisecond for the codec library, and 190 ms for
a vendor device, whose five capabilities are each proved by building a real decoder; 200 ms
for the whole table where the first shape cost 800 for three rows, because every call had
re-probed the whole machine. A slot with nothing usable behind it still answers true, with
`available` clear, every capability false and the reason in `driver` -- a node that is not
there, a device past the last, a codec library the build does not load -- and a loop skips
it. An available row is a decoder creation will open, named by the two values creation
takes; its `name` is a label for a menu, the interface and the card's maker (`VA-API
[Intel]`, `NVDEC [NVIDIA]`, `libavcodec [LGPL]`), and `driver` (minor 13) the driver's own
words -- its banner and version, the device's product name, the library's version and
licence, or why the slot is unavailable. For a startup or a settings screen, not a per-frame
call.

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

**Minor 2** (2026-09-12) appended `transport` and `fingerprint` to `lowlat_attempt_info`, the
`lowlat_transport` enumeration, the status `LOWLAT_ERR_FINGERPRINT` and the outcome
`LOWLAT_OUTCOME_HANDSHAKE_FAILED` ([§4](#4-signaling-seam)).

**Minor 3** (2026-09-15) added `lowlat_features` and its two bits, and renamed the handle:
`lowlat` is `lowlat_host`, `lowlat_create_info` is `lowlat_host_create_info`, `lowlat_create`
and `lowlat_destroy` are `lowlat_host_create` and `lowlat_host_destroy` ([§2](#2-lifecycle)).
A rename before the first major version, under the rule below, taken before the first
pre-release so that the header it ships is the one that lasts; nothing moved and nothing
changed meaning.

**Minor 4** (2026-09-17) added the client half ([§3b](#3b-client)): `lowlat_client` and its
create, destroy, seam, status, user data and poll calls, `lowlat_client_create_info`,
`lowlat_client_config` and `lowlat_client_status`; the event types `BLOCKED`, `STREAM_ENDED`
and `HOST_MODE` with their bodies and the outcome `LOWLAT_OUTCOME_DISCONNECTED`. The seam's
types moved out of the host's guard into one both halves share; nothing moved in memory and
nothing changed meaning.

**Minor 5** (2026-09-17) added the pictures ([§3b](#3b-client)): `lowlat_client_acquire_frame`
and `lowlat_client_release_frame`, `lowlat_frame`, `lowlat_plane` and `lowlat_fence`, the
`lowlat_decoder`, `lowlat_frame_kind` and `lowlat_fence_kind` enumerations and the picture
formats; `lowlat_client_create_info` gained the decoder, the frame kind, the ceiling and the
device, and `lowlat_client_status` the decoder's state and backend, the codec, the queue
depth, the decode, read-back and reported encode times, the count decoded and the video
bytes; the statuses `LOWLAT_ERR_TOO_MANY_HELD` and the `LOWLAT_ERR_NO_DECODER_*` range with
`LOWLAT_ERR_DECODER_UNSUPPORTED`, and the outcome `LOWLAT_OUTCOME_DECODER_FAILED`.
`lowlat_rotation` and `lowlat_codec` moved from the host's guard to the shared block, their
values unchanged.

**Minor 6** (2026-09-18) added the input ([§3b](#3b-client)): `lowlat_client_set_viewport`,
the nine `lowlat_client_send_*` calls with `lowlat_pad_state`, the `LOWLAT_MOD_*`,
`LOWLAT_MOUSE_*` and `LOWLAT_PAD_*` vocabulary, `LOWLAT_EVENT_RELATIVE` with its body, and
`input_dropped` in `lowlat_client_status`.

**Minor 7** (2026-09-18) added the sound ([§3b](#3b-client)): `lowlat_client_acquire_audio`,
`LOWLAT_AUDIO_OPUS` and `LOWLAT_AUDIO_PCM`, and in `lowlat_client_status` the packets decoded,
dropped, refused and queued, the last packet's age and the codec.

**Minor 8** (2026-09-19) is the preferences, the second backend, the handle and the decoder
list ([§3b](#3b-client), [§6](#6-enumeration)): `lowlat_client_video_config` as
`lowlat_client_config.video`, taking the size request with it; `lowlat_client_set_video_config`;
the two full-chroma formats; the handle fields of `lowlat_frame` and `lowlat_plane.offset`;
`lowlat_handle_kind`; the vendor decoder accepted; the status fields for what was asked,
declared and decoded; `lowlat_enum_decoders` with `lowlat_decoder_info`; the client's decode
and sound figures as reported to the host. The size request moves, which is a layout change
to `lowlat_client_config` under the rule below.

**Minor 9** (2026-09-19) is the cursor, rumble, the guest list and the client's metrics
([§3b](#3b-client)): the three events with their bodies, `lowlat_client_get_metrics` with
`lowlat_client_metrics` and `lowlat_client_channel_metrics`, and the status fields for the
client's own number and the pointer's counts.

**Minor 10** (2026-09-20) is the pad reports ([§3b](#3b-client), [§3](#3-host)): the client
half built with C7.1 -- `lowlat_pad_type`, `lowlat_pad_report`, `LOWLAT_PAD_REPORT_MAX`,
`lowlat_client_send_pad_report`, `LOWLAT_EVENT_PAD_REPORT` with `lowlat_pad_report_event`, the
three `pad_reports_*` status fields; the host half built with Phase 14 under the same minor --
`pad_sink` in a reserved byte of `lowlat_host_config`, `lowlat_pad_sink`,
`lowlat_host_poll_pad_report` and `lowlat_host_send_pad_report`, `LOWLAT_PAD_REPORT_UNPLUG`.
Nothing moves.

**Minor 11** (2026-09-21) is the software decoder and the decoder chosen mid-session
([§3b](#3b-client), [§6](#6-enumeration)): `LOWLAT_DECODER_SOFTWARE`,
`LOWLAT_ERR_NO_DECODER_LICENCE`, the software row last in `lowlat_enum_decoders`,
`lowlat_client_set_decoder`, and the automatic order's two corrections. Nothing moves.

**Minor 12** (2026-09-21) is one feature bit, `LOWLAT_FEATURE_GPL_LIBAVCODEC` ([§2](#2-lifecycle),
[§3b](#3b-client)): set by a library built with the `gpl-libavcodec` feature, whose software
decoder loads a GPL codec library as well as an LGPL one. And (2026-09-22) one bit in a
reserved byte of `lowlat_decoder_info`, `available`, with the enumeration corrected to a
fixed table of slots probed one at a time ([§6](#6-enumeration)): a slot with nothing behind
it now answers true with the bit clear where the first shape listed only what opened, so a
loop written against minor 11 that reads capabilities still works and one that counts rows
now counts slots. Nothing moves.

**Minor 13** (2026-09-22) appends `driver` to `lowlat_decoder_info` ([§6](#6-enumeration)) and
makes `name` a label: the interface and the card's maker -- `VA-API [Intel]`, `VA-API [AMD]`,
`NVDEC [NVIDIA]`, `libavcodec [LGPL]` -- where it had carried the driver's own words, which
now sit in `driver` together with a slot's reason for being unavailable. The row is filled as
far as the caller's `size` reaches, so a caller built against minor 12 gets the fields it
knows. Nothing moves.

**Minor 14** (2026-09-23) is the relay ([§3b](#3b-client)): `relay`, `relay_username` and
`relay_password` appended to `lowlat_client_config` with `LOWLAT_RELAY_CREDENTIAL_MAX`;
`relay_address`, `relay_port` and `relayed` appended to `lowlat_client_status`; the outcomes
`LOWLAT_OUTCOME_RELAY_UNREACHABLE`, `_REFUSED` and `_LOST`. Nothing moves. **Both structures
are read and filled as far as the caller's `size` reaches**, with the size they had at minor
13 as the least accepted. Until this minor each was refused unless `size` covered the whole
of it, which would have refused every caller built against an earlier header the first time
either grew; the rule above had not been kept for them, and is now.

**This surface is ours and carries no inherited compatibility.** It was designed here rather
than adopted, so before the first major version a name that turns out to be wrong is corrected
rather than kept: `LOWLAT_CG_LEVEL_LEGACY` became `LOWLAT_CG_LEVEL_AGGRESSIVE` because the
value never selected an older scheme and the name said it did. The value did not move and the
behaviour did not change; only the name stopped misdescribing it.

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

- **No signaling**, on either half. [04 §1](04-signaling.md).
- **No callbacks on data paths.** The frames and the sound a host sends never cross this
  boundary; the SDK captures and encodes internally, and an application that wants a host's
  frames wants a different product. What the client half hands out -- pictures, sound,
  events -- crosses on the application's own call, by acquire and poll
  ([§3b](#3b-client)), never by a call from inside the library. **A guest's microphone is
  the host's one exception and it is still not a callback**: it is polled, on a call of its
  own, and what crosses is samples.
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
decides its own size, so asking for one is the same request as asking for a mode. **It is read
from the session since 2026-09-10**, on the same connection that places the output in the
desktop, and declared in the video header from there; a host with no session to ask declares
the picture flat. A guest's request for a size or a turn is the application's to relay to the
session, and it stays out of this configuration for the same reason a size does
([impl-plan.md](impl-plan.md), *Output selection*).

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
display runs at, not a target, and **zero asks for the display's own rate**: the display is
read when the pipeline is built, so the number the application is told is one the stream can
actually reach.
