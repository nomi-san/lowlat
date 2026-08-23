# Changelog

Newest first. One entry per phase; approach changes and gate revisions go in
[impl-plan.md](impl-plan.md) instead.

## 10: audio (in progress)

**Live against a stock client, 2026-08-23**: both codecs heard on a real
peer that chose each in its own settings, the speakers at the desk
silenced while it was connected and restored when it left, and the
sound device taken and given back with the room.

**Five of its six gate items are closed**, the last four of them from a
run with two guests seated at once: a source change survived cleanly, a
guest of either encoding joined a room that already held the other
without disturbing it, and silence was shown to cost about a hundredth
of what sound does. **What the gate still owes is the long half of the
drift run** -- thirty minutes rather than the fifteen that has been
done, because the second half is where a peer's buffer is expected to
reach an edge and re-prime, which is a peer's behaviour rather than a
fault.

**Fixed**

- **Silencing the speakers could silence every guest.** Whether the tap
  is ahead of a device's mute is a property of the device, which this
  had assumed away. A device with its own mixer applies mute and volume
  in the device, and the mix that reaches the monitor is untouched --
  which is what the local mute rests on. A device without one has both
  applied by the sound server to the mix the monitor is fed from, so
  muting it silences everybody listening and the volume control at the
  desk scales what they hear.

  Measured both ways on one machine: a capture of a virtual output goes
  to digital silence for exactly as long as the mute lasts, 301 frames
  of 600 with a tone playing throughout, and a capture of the hardware
  output is unchanged across the same mute. **A virtual output is always
  of the second kind**, and so is any device the system mixes for.

  The mute is now refused on such a device and says so, rather than
  keeping the promise to the person at the desk by breaking the one made
  to every guest. The setting is still accepted, because the device can
  change under a running host, so the check belongs where the device is
  rather than at the call. **The volume half is not refusable and is not
  ours**: on such a device the person's own control sits upstream of the
  capture, and nothing here can separate the two.

- **A capture that stopped stayed stopped.** A capture ends on its own
  thread when the sound server goes away, and it tells nobody: the host
  held a thread that had already returned, with the room still saying
  somebody was listening, and nothing read the device again for the rest
  of the session. It is the same shape as the entry below it, one level
  down -- a decision taken on a change rather than on every pass -- and
  it is why the pass that knows the room's size now asks whether the
  device is still delivering rather than assuming it from having opened
  it once.

  **Only a device that was once held is taken again**, and not more
  often than every two seconds. A capture that never opened is a machine
  with no sound server: asking it again costs a connection attempt that
  blocks the loop trying to encode, and the answer does not change.

  **A failed reopen keeps the stream that is working.** A device switch
  that will not open, or a new default output that is not ready yet, no
  longer ends the capture over it -- and a device that has genuinely
  gone stops delivering on its own, which is the path above.

  Its regression test cuts a proxy in front of the sound server, which
  produces a server going away without disturbing the one the machine is
  using. Off by default, and watched to fail without the fix.

- **A sound device that does not resolve was accepted and then took the
  sound away.** The boundary answered yes, the loop found no such device
  and the capture ended there. The name is now checked against the
  enumeration at the call, which is the only place that can refuse: the
  loop that opens it runs long after the call returned. **The setter
  only**, never the start -- a host whose sound server is not up yet
  must still be able to stream pictures.

- **Sound could not be switched on by a host that started with it off.**
  Whether a host had a sound source at all was decided once, from the
  value `enabled` held at the start, and the switch was the presence of
  that source -- so `enabled` was the one field of a structure
  documented as having no settled half that was not live. Having a
  source and being switched on are two things now: one is decided when
  the stream is built and cannot change, the other is a setting and can.

- **The boundary reported what was asked for and called it what was
  happening.** The settings are the request -- `device` is empty for a
  host following the default output, and `enabled` goes on saying yes
  after a capture has died -- so the status carries the other half:
  whether a device is being read right now, and which one it landed on.

  **The settings are not rewritten to the resolved name**, so an
  application that reads them, changes one field and writes them back
  does not pin a host that was following the default. And the state is
  read with a try rather than a wait: the loop holds it while it opens a
  device, which can take seconds against a server that is not answering,
  and a caller asking what is happening must not be parked behind that.

- **A sound device held after everybody had gone.** It was taken by the
  loop that waits for a guest and given back by that same loop -- which
  is never reached again, because the encode loop sleeps through an
  empty room rather than returning. One host held a capture, and
  somebody's muted speakers, across three sessions.

  The decision now lives where the room's size is known and is taken on
  every pass, because a room empties without anything else happening:
  no rebuild, no arrival, no error. The device moved off that loop's
  stack into the shared state, so the loop that notices need not be the
  one that built it.

- **Silence is not skipped the instant sound stops.** A peer plays only
  once it has queued its minimum -- measured at 75 to 150 ms on a
  desktop and 150 to 300 on a phone -- and reaching zero makes it wait
  that out again, so stopping at the first silent frame clipped the
  next word. The uncompressed path now holds for two seconds: past any
  pause inside speech, and still short enough that a quiet desktop
  stops spending 1.54 Mbit/s on nothing.

**Added**

- **What sound costs a guest, in the line a live run is read from.**
  Sound appeared nowhere in it: the picture's numbers say nothing about
  it, and a packet refused for a full window is invisible on the wire by
  design. The packets sent, the ones dropped and the rate the channel is
  carrying now travel with the rest.

  **The rate is the one that answers the question.** The compressed path
  keeps sending through silence, so a count that goes on climbing cannot
  tell a quiet desktop from a loud one, and only the rate falling to a
  hundredth of itself says silence is costing nothing.

- **The framing, in the protocol core.** Fifteen bytes ahead of the
  payload on the audio channel: a channel mask, the sample count per
  channel, the rate, the codec, and the channel count. Three of those
  rebuild a receiver's decoder when they change and nothing else does,
  which is what makes changing sound device mid-session free and
  changing the layout expensive.

  **Two of the fields are not what a writer's description of them
  says**, and a reader is the authority on a header. The leading word
  is the channel mask rather than a reserved zero: it selects the
  stream layout a decoder is built with, and only its low-frequency bit
  is consulted, so stereo decodes identically whether it is written or
  not -- and it is written, because it describes the payload. The byte
  beside the codec is the channel count, not half of a two-byte tag;
  the pair only looks like one because stereo makes both of them two.

- **Its own crate**, `lowlat-audio`. The two crates that look like its
  home carry a display stack and two vendor runtimes between them, and
  sound needs none of it: a machine with no graphics device still has
  audio. What the three share is the shape of the problem.

- **Capture from the desktop's own output**, over the sound server's
  socket, with the client library loaded at runtime rather than linked.
  A service outside the session is admitted to that socket without a
  credential, which is what makes this a stream rather than a helper.

  **The source is the clock.** Frames are reassembled from whatever the
  server delivers rather than pulled on a timer: fragments arrive on
  its graph's own period of 21.33 ms and not the 20 ms asked for, and
  the rate is exact even so -- sixty seconds of reading came out 47 ms
  short of the wall clock, the same figure a five second run gives, so
  it is the connect and not a drift.

  **A device name that does not resolve is substituted rather than
  refused**, so what the stream landed on is read back and compared.
  And a capture does not follow the default output on its own, so the
  loop is told when the server's state changes and reconnects. The
  device a host is actually on is published, never the one it asked
  for, because somebody else's volume control can move the stream.

- **The codec**, as a pure-Rust port rather than bindings: no runtime
  library to find, nothing new that is `unsafe`, and a bitstream the
  reference implementation was measured to decode correctly to within
  half a percent of amplitude. Encoding a frame costs 0.078 ms at the
  median and allocates nothing, asserted under the counting allocator.

- **One capture serving both codecs, and a slot per encoding somebody
  wants.** A guest that asked for the uncompressed form is sent the
  frame exactly as it was read; a guest that did not is sent the
  packet. The choice is the guest's own, from its initialization, so a
  room may hold both at once and neither costs anything per guest.

  Sound has its own channel, window and wake, and is sent whether or
  not a picture can be. Nothing is retransmitted: a window with no room
  refuses the whole message, which drops a packet rather than leaving a
  gap a peer would wait on, and a guest that stops draining loses its
  own packets without holding a slot the room needs.

- **The sound device is held while somebody is listening and not
  otherwise**, opened for the first guest and given back with the last.
  It outlives an encoder rebuild, which a guest does not notice and
  which would otherwise cost a gap in the sound for a change to the
  picture.

- **The speakers at the desk can be silenced while a guest is
  connected**, off by default. The tap is ahead of the device's own
  mute, so what a guest hears is unaffected. It **restores rather than
  unmutes**: the state is read first and undone only when this host is
  what changed it and it is still that way, so somebody who muted their
  own speakers keeps them muted and somebody who unmuted mid-session is
  not re-muted. It moves with the device.

  **Its live test failed the first time**, which is why it is written
  down here: the restore waited on the same flag that had just stopped
  the loop, so it gave up at once and left the speakers muted after the
  process exited. That is the worst failure this feature has -- silent,
  and on somebody else's machine. The restore now takes no cancellation
  and a short deadline of its own.

- **What sound costs comes off the picture's ceiling.** The rate
  controllers measure the video channel and know nothing of the audio
  one, so a host that ignored it would send the configured rate plus
  whatever sound costs -- five percent of a thirty megabit session for
  a guest on the uncompressed form. It is taken off the top, before the
  division, because every guest carries its own.

- **Sound is configured from the boundary, and every field is live.**
  Unlike video there is no settled half: one structure is both what a
  host starts with and what the setter takes. Switching sound off gives
  the device back and restores the speakers; the rate is read on the
  frame that uses it; the permission for the uncompressed form is read
  by everything that produces, prices or labels a packet, through one
  accessor, because a guest sent one encoding and told it is another
  hears noise.

  Enumeration answers before hosting starts and without disturbing a
  host that is running. **The identity is the monitor of an output
  rather than the output**, because that is the device a host reads.

- **Silence costs what it costs, which is not what the plan assumed.**
  Measured: the compressed path collapses digital silence to 1.2 kbit/s
  against the 128 it carries with sound. So it is sent compressed --
  a peer whose buffer drains pays for it audibly when sound returns --
  and skipped uncompressed, where it would spend the whole 1.54 Mbit/s
  saying nothing.

## 8: public C ABI (in progress)

**Fixed**

- **An output identity was bounded by the shortest thing it carries.**
  Sixty-four bytes is the shape of a display connector name; the same
  array holds the sound server's own name for a device, which on the
  development machine is fifty of those sixty-four before any USB serial
  or profile suffix, and a display identity on Windows is an operating
  system device path bounded at 260. The failure is silent -- a name
  that does not fit is truncated, and a truncated name resolves to
  nothing, so enumeration would hand back an identity that could never
  be selected. The bound is 260, set by the worst case rather than by
  the observed one.

**Added**

- **The boundary's skeleton, and the four mechanical gates.** Version, status
  codes and their descriptions, the containment every entry point runs inside,
  and one entry that panics on purpose so the containment can be tested. The
  header is generated from the definitions and committed, and a test that
  regenerates it fails when the two disagree.

  **The generated header is built from the ABI module alone, not from the
  crate.** Generating from the crate publishes every public constant in it: the
  first header carried the guest cap, the pointer hold and four other numbers
  that have nothing to do with the boundary, and an application including it
  would find its own names redefined. Naming the one file makes publishing a
  decision, and it forces the other half of the rule -- a type that crosses the
  boundary is defined at the boundary, because nothing else is visible from
  there.

  **Status codes are an integer with named constants rather than an
  enumeration.** A status travels back in as well as out, ending a guest
  carries one as its reason, and an application is free to hand back a number
  nobody defined. Reading an undefined discriminant into an enumeration is
  undefined behaviour, so the type that crosses is one where every bit pattern
  is valid.

- **A bounded event queue, and the seam does not hold it.** Every other call
  into the seam is a lock and a copy; a poll waits for as long as its caller
  asked. If the two shared a lock, a poll with a hundred millisecond timeout
  would stop a hundred milliseconds of everything else, so the queue is handed
  to its consumer once and the seam only pushes into it. Handing it over is
  what makes the single-consumer rule true rather than something to remember:
  two consumers would each see part of the stream and each be told a different
  fraction of what was lost.

  **Bounded in bytes as well as in events, because a count of events is not a
  bound.** A message body may carry a megabyte, so a queue limited only by how
  many it holds is limited to that many megabytes. The oldest go first, the
  count of what went travels with the next event delivered -- the only place it
  can be reported, since the drop happened because nobody was listening -- and
  **what was just handed over is never what gets dropped**, so a body larger
  than the whole budget empties the queue and is still delivered rather than
  vanishing with nothing to say it did.

  Waiting is the wait and wake pair rather than a condition variable, which is
  the house rule and the reason it is: the two halves are one primitive. The
  arrival count is sampled before the queue is found empty, so a push landing
  in between changes the value the sleeper is parked against and the wait
  returns at once instead of sleeping through it.

- **The handle, the event union, and the poll that fills the caller's buffer.**
  An opaque handle the application holds, created and destroyed, poisoned by a
  contained panic and refusing everything afterwards except being destroyed.
  Events cross as a tagged union of plain structs with fixed arrays and no
  pointers, so there is nothing to free and nothing to marshal.

  **A body that does not fit consumes nothing.** The length it needed is
  reported, the event stays at the head, and the next call with room delivers
  it -- which is what lets an application run a small buffer instead of sizing
  every poller at the ceiling. A caller that passes no buffer at all is saying
  it does not want bodies; the event still reports how long the one it gave up
  was.

- **Hosting starts and stops from the boundary**, and the configuration field
  set is settled with it. No resolution: the display decides the picture's
  size, the encoder follows, and `fps` is a ceiling over whatever the display
  runs at rather than a target. An output identity, empty meaning whichever the
  host would pick on its own. Reflexive servers as a fixed array with a count
  rather than a pointer and a length, so the structure stays one blittable
  block with nothing in it to free.

  **Codec, encoder and rotation are named by enumerations and carried as plain
  integers**, and every one of them is checked at the boundary rather than
  converted. The application fills that structure, so each field is whatever it
  wrote; reading one back as a variant would be reading a value nothing
  defined. That is the same rule the status codes follow, arriving from the
  other direction -- and it is why the enumerations have to be asked for
  explicitly in the header, since nothing in a signature references them.

  Starting twice is refused rather than quietly reconfiguring: a second
  configuration that looks accepted and is not is a host running settings
  nobody can see. Stopping and starting again on one handle works, and the
  queue outlives the seam, because what was raised on the way down is still
  worth polling.

- **A configuration split into what is settled and what changes.** Frame rate,
  bitrate, its floor, the full-rate permission and the output can all be
  changed while a host runs; the codec, the encoder, the congestion level, the
  ports and the guest limit are settled when it starts. They are separate
  structures rather than one with a comment, so an application cannot ask for
  something whose answer is "not while this is running". The live half is
  applied without rebuilding anything: a bitrate re-bases the budget and
  reaches the encoder through the reconfigure the rate loop already performs,
  and a frame rate changes the pacing from the next frame. The output is the
  exception in cost rather than in kind, rebuilding around the new source for
  one coded refresh.

  **The floor moves down with the ceiling.** A ceiling lowered under a floor
  that stayed leaves every controller pinned at a rate the operator has just
  asked not to exceed, which reads as a bitrate setting that does nothing.

  **Rotation left the configuration with the resolution.** A display decides
  its own orientation exactly as it decides its own size, so asking for one is
  the same request as asking for a mode. Nothing reads it from the display yet,
  so a stream is declared flat until something does.

- **The signaling seam at the boundary**, which is the last thing standing
  between an application and a connected guest: register an offer, trickle
  candidates, approve, end. Everything it carries arrived over a transport this
  library does not have and does not want, so all of it crosses as fixed arrays
  with nothing to free.

  **Registering is not approving**, and the two costs are why: registering is
  bookkeeping, approving opens a socket and starts this guest's threads. An
  application that declines simply never approves. **Every refusal is its own
  status**, in the band the partition set aside for admission, because the
  right response differs per outcome -- a full host declines the offer, a race
  with teardown is dropped, and neither is a crypto failure. A full host in
  particular must *decline*: nothing in the protocol reports a host that never
  replied, so a peer given silence sits connecting until its own deadline.

  **Approval reports the port that was bound, and takes none.** The bind walks
  when a port is taken, so the port is an answer rather than a request;
  advertising the configured one gives a peer an address that answers checks
  and never establishes. The credential arrays are sized by the media key,
  which travels as 254 characters -- anything shorter truncates a key into one
  that decrypts nothing and reports no reason.

- **The roster and application messages at the boundary**, in the two-call
  shape: ask how many guests there are, then pass an array of that many.
  Nothing is allocated on the caller's behalf. A buffer smaller than the roster
  is filled as far as it goes and told what it needed, because the roster moves
  and a caller that sized its array a moment ago must not lose the call for it.
  **The guest structure is the one that cannot carry its own size**: the caller
  walks an array of them by stride, so a size written per element says nothing
  about how far apart they are, and the count is the versioning instead.

- **The three events only their own producer can raise.** What is being
  captured comes from the loop that rebuilt, because nothing above it knows
  whether the output moved or the display resized. The pointer's owner comes
  from inside the arbiter's lock, because a guest thread can only report that
  the pointer is now its own -- which is also what it would report on every
  message while it merely keeps holding it. And the fatal one comes from the
  loop that could not build an encoder for anybody.

  **The fatal event is never dropped, and the rule is not oldest-first with an
  exception.** A queue under pressure discards the oldest *droppable* event, so
  a fatal one sitting at the front is not the first thing thrown away -- which
  is exactly what a plain oldest-first rule does to the one event whose loss no
  count can convey. Holding it does not make the queue unbounded; everything
  droppable still goes.

  **A guest that is chronically behind is still not an event.** The
  skip-and-resync cycle exists; what is missing is the threshold that makes a
  cycle chronic, and adding the event first would mean firing on every skip or
  choosing a number nothing measured.

- **Ending a guest and changing what it may drive**, both through one
  per-guest channel: only that guest's own thread may touch its session or its
  input devices, so an ask is delivered to it rather than applied behind its
  back. A kick leaves its reason where the stream leaves one, so the ending is
  the single path that already knows how to end a guest -- the message, the
  moment for it to arrive, and the seat going back. **Zero is refused as a
  reason**: a peer carries on through a status of zero, so a guest kicked with
  one is told nothing and stays exactly where it was.

- **Output enumeration, and a pre-flight that says why a host cannot start.**
  The two ways of failing are indistinguishable afterwards -- a host that cannot
  capture fails deep in the stream loop, and only a log separates "nothing is
  lit" from "this process may not read what is".

- **Host status, and log lines an application can receive.** The callback is
  replaceable although the sink beneath it takes one installation, so an
  application changes where its logs go rather than being refused because
  something is already there. The message crosses as a NUL-terminated copy,
  which is the one allocation on that path: a Rust string carries a length
  rather than a terminator, and handing out a pointer to one hands out
  something C cannot read to the end of. The level decides what is **formatted**
  and not only what is delivered.

- **A host in C#**, which is what the boundary was built for. It imports the
  shared object and no package: the signaling is written against what its own
  runtime ships with, because a seam proven by borrowing this library's own
  signaling is not proven at all. Every call an integration makes runs from
  there -- the pre-flight, enumeration, start, the four-call seam, the roster,
  messages, permissions, a kick, the event pump, the log callback -- and the
  application's own admission policy sits where it belongs, which is outside.

  **No marshalling directives in any structure.** A managed boolean is four
  bytes to a marshaller and pinning it to one takes the directive that stops a
  structure being blittable, so the mirrors carry a byte with a property over
  it. That is what lets the interop be generated at compile time rather than
  walked field by field at run time, and it is the property the boundary was
  shaped around.

- **Telling every guest who is in the room**, which the boundary could not do
  at all. It travels on its own opcode and is addressed to everybody; a peer
  cannot ask for one and finds itself in the list by number, so a guest never
  sent one does not know what it is. An application had `send_user_data` and no
  way to send this.

- **What a guest is, and what it is doing.** A guest now carries the attempt it
  was registered under -- the link between the seam's two halves, since
  everything before a guest is seated is addressed by attempt and everything
  after by number. Metrics live behind their own call rather than inside the
  guest, because a guest is an array element and an array element cannot carry
  a size: it is fixed for the major version and metrics are the numbers most
  likely to grow.

  **They report what this host can answer for.** The congestion controller's
  own inputs, the measured rate, encode time and the smoothed round trip, plus
  when each kind of input last arrived -- the one question an application
  kicking idle guests can ask nobody else. A peer's decode time and its queued
  frames are the peer's to know, and reporting either would be reporting a
  number this host made up.

- **The event queue outlives the host on it.** It exists from the moment a
  handle does rather than arriving with a host, so a poll before hosting waits
  on something real instead of a placeholder that slept for the timeout, and
  what a host raised on the way down is still there to be taken after it has
  stopped.

- **The daemon's own lines go through the log**, so one run is one account:
  the same level, the same timestamp, the same stream. It had been printing
  operational lines on standard output while the library wrote to standard
  error, which is two stories about one session in two formats. The `--outputs`
  listing stays on standard output, because that is a question answered rather
  than a run reported.

- **Emitting at the frame rate when nothing changed is off by default.** It is
  a permission rather than a behaviour -- nothing here skips a repeated picture,
  and a host that keeps sending costs bitrate rather than being wrong -- so
  defaulting it on would promise to spend that bitrate whatever becomes
  possible later.

**Found by running it**

- **A field the boundary accepted and then dropped.** The permission above was
  read from the configuration at start, checked, and never carried into the
  stream, because the stream's own configuration had no such field: the live
  cell was built from a default and what the caller asked for went nowhere. It
  reads back wrong immediately, which is how it was noticed -- an application
  setting it and then asking would be told the opposite. **A field silently
  ignored is worse than one refused**, because the application believes it
  asked.

- **An attempt that is not ended holds its seat.** A guest's loop stopping is
  reported as an event, but the attempt stays registered until the application
  ends it -- and while it does, it holds that guest's number, its seat and its
  port. An application that removes the peer from its own bookkeeping and stops
  there leaves a peer that has gone still on the roster and still counting
  against capacity, so a host fills up over a few disconnects and refuses
  offers with nothing connected. Found against a real client, which showed one
  guest still present after it had left.

- **A rule the test could not have exercised.** A stamp landing at the very
  first millisecond must not be written as zero, because zero is how "never"
  is spelled -- an application would read a guest that typed as the session
  opened as one that has never typed. The test for it went through a seated
  guest that sends nothing, so the stamping was never reached and deleting the
  rule changed no result. It is now tested against the stamping itself.

- **A log with no clock answers none of the questions logs are read for.**
  Every diagnosis this project has made from a log came down to an interval --
  how long a wait actually waited, how far apart two frames left, whether a
  periodic line stopped -- and the default sink printed a level and a message
  and nothing else. It now stamps the elapsed time since the first line:
  monotonic rather than a wall clock, because that is the quantity being read
  and it needs no timezone to mean something.

- **A pre-flight that only checks whether a plane is lit passes when capture
  will fail.** Enumerating a connector and finding its framebuffer both succeed
  without the capability; getting the buffer handles back out of it does not,
  and a framebuffer with none is what every later stage fails on. The first
  version asked the weaker question and answered that this machine could host
  while running as an ordinary user. Measured both ways on a real display: the
  same binary now reports the display unreachable as a user in the `video`
  group and ready as root.

- **Every guest ran the most aggressive congestion control.** The controller
  built for each guest was pinned at level zero, which the level table names as
  compatibility-only and explicitly not the default -- its threshold declares
  congestion on any stale fragment once the send window passes its floor, so
  the bitrate was cut more eagerly than any measurement intended. Found while
  deciding whether the level was worth making configurable; it turned out to be
  worth fixing.

- **A check that watched the wrong side of the change.** The live settings were
  read back through the same cell the setter had just written, so pinning the
  counter the loop reads -- making it blind to every change -- passed the entire
  suite. The decision is now a function of its own with a test that fails both
  ways: once when a change would not be seen, once when it would be applied
  again on every pass.

- **A poll with nothing to poll must still cost the time it was given.** With
  no queue yet -- an application starts its polling thread before it starts
  hosting -- the call answered immediately, which turns that thread into a spin
  on a core. **The C harness found it by timing the call**, and the unit test
  written for the same behaviour could not have: it asked for a timeout of
  zero, which is the one value that makes returning at once correct.

- **A gate that tests the wrong artifact reports on the wrong artifact.** The
  containment check loads the built shared object on purpose, because the
  library form linked into a test answers for the test's build settings rather
  than for the shipped one. It then passed with the containment deleted: a test
  binary depends on the library form and nothing asks for the shared one, so
  the file being opened was eight hours old and belonged to an earlier command.
  The gate now builds the object it is about to open. **Every check here was
  then made to fail on the fault it exists for** -- a panic crossing the
  boundary, a name exported without the prefix, a stale header, a header that
  does not compile, and a header that compiles as C but not as C++ -- because
  until it has failed once, a check has only been shown to pass.

## 9: capture (in progress)

**Added**

- **Absolute input placed within the captured output.** An absolute device is
  spread by the layer above over the whole desktop, so a coordinate normalised
  against the picture alone lands proportionally short of where it belongs on
  any desktop bigger than that picture: a 2560-wide picture on a 4480-wide
  desktop reached its own right edge 57 percent of the way across and the rest
  of it could not be reached at all. The mapping now clamps into the picture,
  converts into the captured output's rectangle, and places that rectangle in
  the desktop; with one output the rectangle is the desktop and the two
  conversions cancel, so nothing about the single-display case moves.

  **The rectangle and the desktop come from the session, because nothing below
  it knows them.** A controller reports its position inside its own
  framebuffer, which reads as the corner whatever the desktop looks like, and a
  compositor's own virtual output has no controller, no connector and no plane
  at all. The layout is asked for once when the display opens and matched to
  the captured output by the name both sides know it by. **A session that does
  not answer is not a degraded case**: one output is exactly what the axis
  already spans.

  **The clamp is part of the fix rather than tidiness.** A coordinate past the
  picture puts the pointer on the neighbouring output, where the pointer plane
  this host reads goes empty -- which it cannot tell from an application hiding
  the pointer, so the peer is told to switch to relative motion, its cursor
  disappears, and it has to be walked back by hand before the mode clears. The
  reported edge flicker was that cycle repeating, not a stream fault.

  It also **restores the pointer hotspot on a multi-display desktop**, which
  was silently lost: the hotspot is the difference between where a guest
  commanded the pointer and where the display then drew it, and under a
  compressed mapping that difference is negative for all but the first few
  pixels, so every sample was refused and every shape fell back to no offset.

**Found by running it**

- **Leaving a walk early takes what the walk was also doing.** The pass that
  finds the lit display plane is the same one that collects every pointer
  plane, because which pointer plane to use cannot be decided until the lit
  controller is known. Stopping at the display plane did the first job and
  abandoned the second, so the display opened with no pointer at all and a
  guest saw only its own client's fallback shape. It was introduced with named
  selection and hidden by it: the exit only ran when an output was named, and
  nothing named one until capturing the main screen made every run a named one.

**Added**

- **The encoder follows the display.** A conversion target is allocated on the
  device the display is on, and an encoder belonging to another cannot take it,
  so the encoder is a consequence of where the display is rather than a
  preference. It is resolved on every rebuild, which is also what makes a
  display that moves to another card *followed* rather than merely noticed --
  previously the guests were ended with a reason and nothing recovered.

- **Capturing nothing in particular means the main screen.** The search took
  the first device the kernel enumerated, which is an ordering, and on a
  machine with two cards picked the secondary one. It now prefers the output at
  the desktop's own corner: nothing in a layout says "primary", but every
  arrangement puts one output at the origin and hangs the rest off it.

- **What a peer is told about the capture is what is running**, not what was
  asked for. A guest can switch outputs and a display can move by itself, and
  only the loop that rebuilt knows which happened. Changes are pushed rather
  than waited on, because a reader asks after it acts: a change it did not
  cause never reaches it, and a change it did cause may not have landed by the
  time it asks.

- **A guest naming an output nothing is lighting is refused** where the request
  arrives. Refusing it later means failing to open a display, which ends every
  guest on the stream including the one that asked.

- **A guest is told who else is connected** ([01 §11.2b](01-protocol.md)).
  The same body reaches every guest and the second argument does not: each is
  sent its own number alongside, because that is how a peer finds itself and
  learns what it may do. Sent whenever the room changes, because a peer has no
  way to ask.

  **This was recorded as gating nothing and that was measured against one
  question.** Frames render without it, which is all the first gate asked. What
  actually depends on it is everything a peer decides from knowing what it is --
  a client that never receives one hides its own settings entirely, and finding
  that out cost a day spent on the messages that *reply* to a question rather
  than the one nobody asks.

- **The daemon speaks an application protocol** an established client already
  has: the queries it sends on connecting, the configuration and output listing
  it expects back, and a configuration it sends when somebody changes one. None
  of it is in the SDK, which carries the body and never looks inside it.

  Three things running it against a real client settled that reading could not.
  A stream is described by **what it is producing**, per query -- described from
  configuration it reported a size nobody was streaming and named no output at
  all. **The word for no output means opposite things in the two directions**:
  from a host, a stream that has none; from a client, the *Auto* entry asking
  for whichever the host would pick. And **a requested output is checked against
  what is really lit before it is acted on**, because an unknown name is refused
  by failing to open a display, which ends every guest on the stream including
  the one that asked.

  Outputs are named by connector and size rather than by what the display calls
  itself, which is a deliberate divergence: a machine with two identical
  monitors otherwise offers two entries under one label.

- **Application messages, both directions.** The framing had existed since the
  protocol core was written and nothing used it: one arriving was counted and
  dropped, and there was no way to send one. A message now reaches the
  application as an event carrying its sub-identifier and the guest it came
  from, and can be sent to one guest or to all.

  **Nothing here reads the body.** The sub-identifier and the text are an
  application's own protocol; two applications using the same opcode are
  speaking different languages over one channel, and a host that acted on
  either would be choosing between them.

  **The terminator is written on the way out and not required on the way in.**
  A peer reading the body as a C string runs past one that ends without it, so
  it is always written and always counted -- and written once, because the
  declared length counts both and a second one becomes part of the message. On
  the way in it is stripped if present and never insisted on: this is a
  pass-through, and refusing a message because a peer framed its own payload
  differently discards something there was no entitlement to judge.

  **A body past the ceiling is refused locally.** One byte over is dropped at
  the far end with nothing said, so a sender that does not check loses the
  message and cannot find out why.

- **Which output to capture, chosen by name.** A device can be driving more
  than one screen, and the walk that found the picture took whichever plane the
  kernel listed last -- a coin flip between two monitors that changes with the
  hardware. An output is now named by its connector scoped to its device, such
  as `card0:DP-2`, because a connector name is unique within a device and not
  across them and an index moves whenever a cable does. The listing reports
  every lit output with its rectangle in the desktop.

  **A name that is not lit is refused, never fallen back on.** Capturing a
  different screen from the one asked for looks like the selection working, and
  the person who asked is the one least able to see that it did not.

  **Only what the display device is scanning out can be offered**, so an output
  a compositor invented does not appear: it has no controller to read. The
  desktop extent printed beside each output is what shows that there is more
  screen than this.

- **Capturing a different output mid-session**, through the rebuild a display
  changing size already uses. The guests keep their seats and their channel and
  are told the reference chain restarted; it costs one coded refresh. Two
  outputs of the same size are not a special case, because the content is
  entirely different and the refresh is owed either way.

**Found by running it**

- **A cached pointer keeps the offset it arrived with.** A peer that keeps
  pictures is sent a name, a name carries no hotspot, and the far side applies
  one only when a picture arrives. The hotspot is derived from a guest's own
  command, so every shape necessarily travels once before its hotspot is known
  and carries none at all -- naming it from then on froze that, and an I-beam
  drew half its own height low while an arrow looked right, because an arrow's
  offset really is near nothing. What a peer holds is now the picture and the
  offset it came with, and only both together are a name. Every earlier run
  used a peer that does not cache and is therefore sent the picture every time,
  which corrected itself on the next frame.


- **A guest is shown the pointer**, read once on the thread that owns the
  display and reported per guest, because what a guest is owed depends on what
  it already holds. A peer that declared no pointer cache is sent the picture
  every time; a full cache is emptied rather than evicted from, because the far
  side cannot report what it dropped.
- **A guest is shown when the pointer is not its to move.** With the pointer
  arbitrated, a guest that does not hold it had its input dropped and nothing
  happened, which is indistinguishable from a session that has stopped
  responding. It is sent a refused shape instead and gets the real pointer back
  when its turn comes. The shape is loaded from the desktop's own icon theme
  rather than drawn: those files carry a picture and its hotspot together, and
  nothing here can derive a hotspot for a shape the display never draws.
- **The hotspot, derived from the host's own injection.** Nothing reports one,
  and the far side draws the picture against its own pointer, so the offset it
  applies is the one the host sends and zero draws every pointer down and to
  the right of where it is. A guest commands a position, the display draws the
  shape with its point on it, and the difference is the hotspot: sampled once
  per command on the read after it, refused unless it lands inside its own
  shape, and cached per shape.
- The scanout capture backend: enumerate the display pipeline, describe the
  primary and cursor planes with their format, modifier and per-buffer pitches,
  and export those buffers for import elsewhere. It reads no pixels; a
  framebuffer leaves here as file descriptors.
- A diagnostic that prints every transition the display pipeline makes, so
  format changes, pointer disappearances and pointer redraws are observable
  while a desktop is driven by hand.
- The device the display is on, opened by matching the node's own numbers as
  the driver reports them. Exact, where a name or an index is a coin flip on a
  machine with two cards.
- Import of a captured framebuffer with no copy, the tiling modifier and
  per-plane pitches handed over rather than inferred.
- Colour conversion on the device: one compute shader for every input depth,
  writing a two-plane result through a view per plane. The two-plane format
  reports no write support on any device here while each of its planes reports
  it everywhere, so the views are the only way in.
- The converted frame handed out as a descriptor an encoder can take: untiled,
  two planes in one allocation, and laid out so the colour plane begins exactly
  one luma plane in. That is not a choice. An encoder registering a frame by
  pointer is given one address and one row length and assumes it, with no field
  in which to say otherwise, while a driver asked to lay out a two-plane image
  put the colour plane 49152 bytes further on. Two images bound at offsets of
  our choosing settle it, and the result needs less machinery than the
  two-plane image it replaced. The allocation is exportable as either handle
  kind, so the encoder is chosen at the handover.

- The encoder taking a converted frame directly, with no upload. Registration
  needed only an address and a row length, so the existing path is unchanged
  and a frame already on the device skips the copy entirely.

- The real desktop as the stream's frame source, in place of the generator.
  The display node is discovered rather than configured, the plane is re-read
  every frame, imports are kept per buffer of the display's rotation, and there
  is one conversion target per picture in flight.

- The pointer read off its plane and cropped to what is actually drawn, the
  image form the wire carries it in, and the pointer message itself. The host
  wiring that would send them is not written yet.

**Found by running it**

- **Plane position needs the atomic capability, not just universal planes.**
  Without it a plane carries no position property at all, and a reader that
  defaults a missing property reports a pointer parked in the corner rather
  than a value that does not exist. Both capabilities are requested at open so
  a driver that cannot answer says so once; a missing property is an error.
- **Plane coordinates are signed in an unsigned field**, and go negative in
  ordinary use: the first corrected run read a pointer two pixels past the left
  edge, which reads as four billion pixels the other way if taken unsigned.
- **The scanout pixel format changes several times a minute.** Ten-bit for the
  composited desktop, eight-bit whenever a fullscreen surface takes the display
  over, and back again. Modifier, stride and plane count are identical across
  the change, so nothing about the buffer announces it
  ([07 §3.3](07-platforms.md)).
- **A pointer leaving the hardware plane is not the relative-mode signal.** It
  leaves both when an application hides it and when it merely grows past what
  the plane can carry, at which point it is still on screen and in use. Only
  the first means relative, so that signal has to come from inside the session
  ([07 §2.1](07-platforms.md)).
- **A pointer shape cannot be detected from metadata.** The buffer identity
  turns over as the pointer moves and carries no information about what the
  pointer looks like, so the shape has to be read and compared. The buffer is
  linear and maps directly, and it is a fixed size whatever the pointer is, so
  the extent comes from the alpha channel.
- **A peer that dies with a full send window is never reaped.** The window
  climbs to its cap, every fragment goes stale, and the host retransmits at
  three times the configured rate indefinitely. The process stays up, which is
  worse than exiting: one that dies gets restarted and this one consumes the
  uplink. The same session ends three other guests correctly, so the reaping
  path is not broken in general.
- **A source that imports once is indistinguishable from a working one until
  you watch it move.** The display cycles through a pool of buffers as it
  draws, so one import reads one buffer of that rotation for ever. The stream
  decodes perfectly, every stage reports success, and the picture never
  changes. The check is therefore that consecutive pictures differ, not that
  the file decodes.
- **The whole path produces a picture something else can read.** Thirty frames
  captured from a real desktop, imported, converted and encoded with no copy at
  any stage, decoded outside the project as yuv420p, limited range, BT.709.
  That is what settles the frame layout: a colour plane in the wrong place
  shows as garbage chroma, and there is none.
- **A desktop is a poor test vector for colour.** Comparing a conversion round
  trip against the source separates a correct matrix from a wrong one by under
  three times, because a grey pixel gives every matrix the same luma and no
  chroma at all, and a dark desktop is almost entirely grey. The figure that
  moves is the one taken over saturated pixels alone. The check that settles it
  is eight saturated colours against the transform computed on the processor,
  which agrees exactly and needs a driver rather than a graphics card, so it
  runs by default and in continuous integration.

**Fixed**

- **A button is released on the device that took it.** Which pointer device an
  event goes to follows whichever kind of motion arrived last, and a peer
  changes kind mid-gesture, so a release could reach a device that never saw
  the press while the kernel went on holding the button down on the one that
  did. The press still follows the pointer that produced its position; only
  the release is pinned.
- **The acquire stage measured a clock against itself** and reported zero on
  every display run, hiding capture and colour conversion inside a figure
  nobody could break down. Measured on the integrated device at 2560x1440, the
  two halves of 7.5 ms are 2.1 ms of capture and conversion and 5.3 ms of
  encode.
- **A display that has left the device is noticed.** A controller whose
  connector is unplugged keeps scanning out, holding the last picture it was
  given, so every read succeeds and the only thing wrong is that the picture
  never changes again. The connector is what says so.
- **A framebuffer identifier is not a buffer.** GPU imports were cached against
  it and the kernel reuses them: measured over a monitor switched off and on,
  two identifiers came back naming different memory, so the encoder was fed a
  picture from before the display went dark, alternating with live frames until
  the cache turned over. The export has an identity and it is what the cache is
  keyed on now.
- **A display that changes size rebuilds the stream**, and a peer's declaration
  rebuilds it too rather than waiting for an explicit request. Both are the same
  shape: something the encoder is built around changed, and only a message from
  a peer was being treated as a reason.
- **Deriving the hidden signal from the pointer plane**, which took four goes
  and is now written down in [05-host.md §8.4](05-host.md). It must be
  debounced, it must not speak before a pointer has ever been seen, and only a
  read that examined the pixels may say a pointer is still there. And the plane
  chosen has to be the one on the controller that is lit: a card has one per
  controller, and the others never have a pointer on them.
- **A pointer redrawn into the buffer it already occupied was never noticed.**
  The pixels were read only when the plane's framebuffer identifier moved, and
  a compositor that redraws a pointer in place defeats that: a browser's link
  pointer became an arrow with the identifier unchanged, and a guest kept the
  hand while the screen showed the arrow. Thirteen of nineteen shape changes in
  twenty seconds of ordinary hovering arrived in the buffer that carried the
  previous one, so the identifier is not even usable as a hint. The picture is
  now read on a cadence instead, and the position every time, because the two
  cost three orders of magnitude apart: 0.006 ms to describe the plane against
  about 3 ms to look at the pixels.

  **The pixels are copied out in bulk before anything scans them**, which is
  worth more than reading fewer of them: the mapping is uncached, and touching
  every fourth byte of it to find the drawn part measured 43 ms against 6.6 to
  copy the same bytes out and 0.025 to scan the copy. Only the first 64 rows
  are copied, with a full copy behind it for a pointer that is not in them.
- **A guest described the stream with the configured size rather than the one
  it produces.** A display decides its own size and the stream follows it, but
  the guest kept the configured numbers, and the size a peer is told is the
  coordinate space its absolute input comes back in. Every position therefore
  arrived scaled by the ratio between the two, quietly and proportionally: a
  2560-wide display described as 1920 reached the right edge of the screen
  three quarters of the way across the picture. The size is now read from the
  stream and re-read while the session runs, because a guest is seated before
  the stream has opened a display, and a display can change size afterwards.
- **A window of stale fragments was re-sent whole on every pass.** The
  outstanding cap was applied to fragments awaiting a first send and to nothing
  else, so retransmission had no ceiling at all: a peer that stopped
  acknowledging was sent **74 Mbps against a configured 10**, decaying to 21 and
  staying there. The cap now stops the scan rather than one branch of it, and a
  fragment in a retransmitting state counts against it whether or not it is due
  this pass -- without the second half the window behind the first hundred is
  admitted every pass and the ceiling does not hold. The same cut against the
  same peer now measures **5.30, 2.64, 1.77, 1.77, 1.77 Mbps**.
- **A datagram the path refused took the session down with it.** A send error
  was returned out of the shell's turn and the guest loop stopped on it without
  reporting an outcome, so a local filter rule, a route that had not come back
  or a link that had gone ended a session silently: nothing reaped the attempt,
  and its port, its seat and its share of the advertised capacity were never
  released. A refused datagram is now dropped like the loss it is, logged on the
  edges of an outage rather than per datagram, and a loop that does stop reports
  why. **Found by a live run**, not by a test: the two faults compose, and the
  first one hid the second.
- **A session nothing could be delivered on was never ended.** Liveness watched
  the inbound direction only, so a peer that keeps acknowledging on the cadence
  while it has stopped receiving satisfied it indefinitely, and the whole send
  window was retransmitted at 88 to 92 Mbps against a configured 30 for as long
  as the session was allowed to last. A channel that has held outstanding
  fragments with none of them acknowledged for fifteen seconds now ends the
  session with an outcome of its own, so a live run can tell a peer that went
  away from a peer that stopped reading.

  **Judged on the acknowledged count, not on the window.** A congested path
  fills a window and looks identical from the send side, and it acknowledges
  throughout; the two are told apart only by whether the count moves. And
  judged per channel, because a peer that has stopped draining one ring keeps
  acknowledging the others, so a figure summed across them is refreshed by the
  cheap traffic while the expensive traffic goes nowhere.

## 6: HEVC (closed 2026-08-18)

**Added**

- The codec and the encoder backend are chosen at startup and drive the same
  loop, so a stream can be HEVC on the vendor backend without a second
  pipeline. A stock client decodes it at 60 fps.
- HEVC on the open backend: a slice header for that codec, its sequence,
  picture and slice buffers, and its three parameter sets carried together in
  one packed header. Both codecs now run on both backends through the same
  loop. A stock client decoded 2072 frames at 1920x1080 and 60 fps, decode
  1.1 ms, encode 3.3 ms, zero loss and zero retransmissions.

**Fixed**

- The second codec's parameter sets declared a coded size rounded to the
  standard's minimum block. The device codes at a coarser alignment and
  corrects the size in the set it is handed, so a picture came out eight rows
  taller than asked for with no conformance window to crop it.
- The same sets left the per-block quantiser delta disabled, which on this
  codec is the only handle rate control has. The configured bitrate did
  nothing without it.
- The same sets declared wavefront parallelism, which requires entry point
  offsets in every slice header. Those are byte counts into slice data that
  the side writing the header never sees.

- **The capability a guest declares is read from both places it arrives in**
  ([01 §11.5](01-protocol.md)), and a guest's reinitialization request now
  changes what the stream codes rather than only forcing a keyframe. A live
  client moved a session between the two codecs in both directions with zero
  loss and one frame-rate sample below sixty across the change.
- Every opcode a peer sends is logged once, with its arguments, so what a
  peer actually speaks is on record rather than inferred.

- **A session the host cannot serve ends with a reason** ([05 §6.2](05-host.md)):
  no room, no encoder for what was asked, no capability report from the
  device, or an encoder that stopped answering. A guest used to sit
  connected receiving nothing until its own liveness deadline noticed,
  minutes later, and then blame the network.
- `--max-guests` sets the advertised capacity and the number of seats,
  which were previously one hardcoded number.

**Fixed**

- An encoder configuration message names the stream it is about, and the
  index was being ignored. A peer sends one for each stream it holds, and a
  client was observed declaring for its secondary streams before the one it
  was receiving, so the host recorded a capability about a stream nobody was
  sending it and would have acted on it.
- A build that fails no longer ends the encode loop. It goes back to
  waiting, so the next guest gets its own attempt at the device -- and the
  waiting retires the seats it is waiting on, without which the loop saw a
  guest that had already left, called it occupied, and rebuilt the encoder
  that had just failed nearly ten thousand times a minute at 76 percent of
  a core.
- A maximum picture size of zero was read as a stated ceiling. Peers exist
  that declare no maximum at all, and a ceiling of nothing is not one.

**Notes**

- **Verified against stock clients, and against two guests at once.** A client
  moved a live session between the codecs in both directions; with two
  seated, the move waited until both agreed and then changed for both. A
  third guest arriving at capacity was declined in signalling.
- **Two guests on a wide-area path behind one uplink fill their send
  windows** and retransmit at several times the configured rate, recovering
  each time; two guests on a local path peak at a window of fourteen. The
  delivery gate behaves correctly throughout and no picture broke. Left open
  against multi-guest delivery and the retransmission scan, neither of which
  is this phase.
- **A stream that encodes without error can decode to nothing.** All three
  fixes above are of that shape, and none of them fails a call. Two were
  found by encoding the same input with a second encoder on the same device
  and comparing the two streams field by field, which is the method to reach
  for first.
- **The coding tools a parameter set declares have to be the ones the device
  actually uses**, not the ones the writer would prefer. A tool declared off
  and used anyway produces a bitstream a decoder reads with the wrong syntax,
  and it reports a decode failure rather than a mismatch.
- **A capability request is answered against every seated guest, not against
  the guest that asked.** One encode serves them all, so the stream codes
  what they have in common; granting one seat's capability would hand the
  others a stream their decoders were not built for.
- **The base flag is set on every declaration and means nothing.** Counting
  it as a capability the pipeline does not emit reported a refusal on every
  ordinary request, which only a live run showed.
- **A host reports what it could not do, not what it guessed a peer could
  not.** D11 said a seat that cannot decode the session's codec is
  disconnected by the host; a peer is the only party that can tell its
  decoder failed, and it raises an error of its own when it does. The
  decision is amended and the phase gate rewritten to the half a host can
  actually know.
- **Two messages a peer sends in the first second of an ordinary session
  were undocumented**, and both turned up by logging every opcode once
  rather than by reading: a decode-latency report whose arguments are
  transposed against the host's own, and the flag that turns per-frame
  timing on. See [01 §11.1](01-protocol.md).

- **The trait paid for itself here.** One generic loop already drove two
  backends; adding a second codec was selection, not a pipeline.
- **A guest that declared H.264 decoded the HEVC stream, and that proves
  less than it appears.** The client used for the run sniffs the first
  parameter set and reconfigures its decoder. A peer without that sniff
  builds the decoder it declared and fails every picture, which is the same
  failure this project has already spent a day on from the other side. The
  refusal path is still required.

## 5: encoder and Gate A (closed 2026-08-18)

**Added**

- Vendored codec headers under `third_party/nvcodec/`, and the encoder FFI
  generated from them into `lowlat-encode`. No crate dependency is added.
- `lowlat-common::dynlib`: opening a shared library at runtime and resolving
  symbols from it, one implementation per platform.
- Loading the encoder runtime, with the interface version checked once at load.
- Loading the compute runtime, enumerating devices by bus address, and
  retaining a primary context.
- Opening an encode session against that context, and querying what the
  hardware will actually do.
- Configuring the encoder for low latency, with the colour description the
  wire requires, and changing its bitrate live.
- Registering an input surface, submitting a picture, and collecting the
  encoded access unit.
- Vendored display headers under `third_party/libva/`, their bindings, and the
  second backend's runtime loading, display binding and profile query.
- The second backend's capability query, encode configuration, surface pool and
  context.
- A bit-level writer for parameter sets: exponential-Golomb, fixed-width
  fields, trailing bits, and start-code escaping.
- The sequence and picture parameter sets themselves, carrying the colour
  description one backend has no other way to state.
- The second backend's encode path: a picture is submitted, the parameter sets
  and the slice header travel with it as packed headers, and a finished picture
  is collected as a probe rather than a wait.
- Live bitrate change on the second backend, carried with each picture, so it
  reinitialises nothing and forces no refresh.
- `lowlat-capture`: the synthetic frame source, planar 4:2:0 with a moving bar
  and a static colour block, and the frame type both encode backends take.
- Upload paths on both backends, each writing a frame into its own input
  surfaces at the surface's own stride.
- `scripts/check-encoded-frames.py`, which decodes a dump and checks each
  picture against the frame index that produced it.
- The encoder trait, and one shared collect result, with both hardware
  backends implementing it and one generic loop driving both.
- Predicted pictures on the second backend: reference bookkeeping, the
  slice header for both picture kinds, and parameter sets that travel
  with a refresh rather than with every picture.
- The encoded-frame pool: fixed slots, one copy per frame however many
  guests take it, and a hold per guest that releases itself.
- `lowlat-common::sync` made public, so a second crate building a
  cross-thread handoff shares the shim the model check swaps out rather
  than keeping a second copy in step.
- The per-guest delivery gate: the window ceiling, the skip-until-keyframe
  latch, the running-maximum retest, and a throttled global keyframe.
- The video packetiser: a stream's fixed facts, the ten-byte header ahead
  of each access unit, and the message the send ring takes.
- The initialization parser in the core: the eight-key body, its sentinels
  and its flag bits, allocation free.
- Session negotiation in the host: the five-second deadline, the encoder
  configuration message, and the encode-latency and generation cadences.
- The bitrate budget: a ceiling divided by the guests on a stream, the
  minimum across their controllers, and a deadband before reconfiguring.
- Two-channel ring geometry per guest, sized from the largest frame the stream
  can produce, and the control channel a guest declares itself on.

- The encode loop: one capture and one encode serving every guest, the seats
  guests take on it, and the daemon wiring that starts it.
- The two cadences a stream owes its peer, sent from the guest that carries
  them: the encode latency every thirtieth frame, and the encoder generation
  once, on the frame after the encoder is ready.
- `lowlat-host::timing`: per-stage percentiles, recorded as a store into a
  fixed ring and sorted only where a report is asked for.

- A guest's refresh request is honoured: a peer that cannot decode asks for a
  picture with no history behind it, and the request now reaches the encoder
  through the gate's throttle.
- Live diagnostics on a streaming guest: what it declared, what it is being
  sent, and what it is still sending back.

- A peer that says it is leaving is taken at its word: the control channel
  carries the notice, and the seat, the port and the share of the bitrate
  budget come back at once rather than two minutes later.

- The video header's third flag bit is the colour depth, not a keyframe
  marker, and we no longer set it.

- The refresh picture's cost, measured at each quantiser floor, as a test
  rather than as a remembered figure.

- A band of unpredictable detail in the synthetic source, off by default, so a
  frame can be made large enough to need more than one fragment.
- The frame rate is declared to the encoder, which is what a bitrate is spent
  at and was never being said.

**Notes on the frames that never fragmented**

- **Every message we had ever sent fit in one fragment**, so the fragmenting
  path, a peer's reassembly and the window arithmetic had never met a message
  that had to be split. Resolution does not fix it: a bar on a flat field is
  trivially compressible at any size. The source now takes a band of detail
  derived from the frame index, which an encoder cannot predict from the frame
  before it.
- **Off by default, and that is deliberate.** Every latency figure and every
  refresh size on record was measured against the flat picture; content that
  changed underneath them would invalidate them silently.
- **It found a defect on its first run.** The stream ran at exactly twice its
  configured rate at every setting. The encoder was never told the frame rate,
  so it budgeted bits for its default of thirty frames a second and received
  sixty. A congestion controller actuating through that is wrong by the same
  factor and would push a path into loss while believing it was inside budget.
  The flat picture could never have shown it: at a tenth of a megabit nothing
  was near the target.

**Notes on a burst that never existed**

- **The vendor backend's 2.4 MB refresh was a length, not a picture.** A
  collect racing the driver reported a size the encoder had not written; the
  same picture held 651 bytes. The race was found and fixed the following day
  and the plan paragraph was never revised, so a fixed defect stood as a gate
  condition. Measured now: 651 bytes against a raw frame of 3110400, under a
  thousandth.
- **The floor is the only bound that moves anything.** A quantiser ceiling and
  an initial quantiser were both swept and neither changed the refresh or the
  quantiser it was coded at, so neither is configured.
- A number nobody re-measures is a memory. This one is a test now.

**Notes on the flag that was not a keyframe**

- **A peer built a ten-bit decoder for our eight-bit stream and failed every
  picture**, on one decoder family out of four. We set that bit on every
  keyframe because this project's own protocol document called it a keyframe
  flag and argued that setting it was more informative and free.
- **The evidence against that reading was already in the same paragraph.**
  Across 4883 recorded video messages the flags byte was identical on every
  one, including the two whose first unit was a parameter set. A host that
  never sets the bit on its own keyframes is not describing keyframes with it.
  The document noticed the pattern, concluded that peers are unreliable about
  a keyframe flag, and then licensed us to set it anyway.
- **Only one decoder family reads it**, so three rendered our stream happily
  and the fourth reported a decode error rather than a mismatch. That is what
  made it look like a defect on the peer's side.
- Keyframes are now classified from the bitstream and from nowhere else. The
  classifier used to check the bit first, which would have called every
  ten-bit predicted frame a keyframe.

**Notes on the first stock client to render our frames**

- **The session was keyed from the media key alone**, discarding the four-byte
  nonce prefix that follows it, so every record we sealed was undecryptable by
  the peer and every record it sent failed our tag check. It presents as a path
  that establishes and then carries nothing, which is indistinguishable from a
  loop that was never wired up. The constructor used documents itself as being
  for fixtures, and the seam was its only non-fixture caller.
- **No test could have caught it**, and that is worth stating rather than
  regretting: every test builds both endpoints the same way, so the two agreed
  with each other and proved nothing about the prefix.
- **A peer builds one decoder from what it declared** and never switches on
  what arrives, so a guest asking for a codec this stream does not produce
  fails every frame and reports a decode error rather than a mismatch. Said
  plainly in the log now.
- **A peer that cannot decode asks for a refresh, and we were dropping the
  request.** It was parsed and thrown away, so the only recovery a peer has was
  dead against us.
- **Every frame we have sent fits in a single fragment**: median 129 bytes,
  largest 868, over four hundred access units. The multi-fragment path is
  covered by tests and by the corpus comparison and has never met a real peer.
- **Nothing in signalling reports a peer closing a session it was using**, but
  the peer itself does, on the control channel, and we were ignoring it. The
  seat, the port and the share of the bitrate budget were held until the media
  path's two-minute liveness deadline noticed, which is what made repeated test
  connections exhaust capacity.

**Notes on the timing, and the loop shape it forced**

- **The loop had to be restructured before it could be measured.** It
  submitted a frame and then waited for it, which is serialised by
  construction: nothing prepares the next frame while the hardware works, and
  the pipeline caps at one frame per encode however fast the encoder is. It
  now has two deadlines -- a poll for finished pictures and a frame clock for
  new ones -- so an encode overlaps the acquire and submit behind it and a
  picture leaves within a poll of being ready.
- **The measurement is what showed the difference is real**, and it is the
  arithmetic rather than the frame rate that carries it: stages sum to 10.670
  ms across a 2.665 ms interval unpaced, and holding one picture in flight
  instead of four collapses that to 3.064 ms of stages inside a 3.066 ms
  interval.
- **A stamp per picture, not one for the loop.** With more than one picture in
  flight the one that comes back is not the one that went in last, so a single
  stamp would report the wrong frame's latency for every frame after the
  first.
- **Percentiles, never averages**, and nearest rank rather than interpolated,
  so the figure reported is one an actual frame took. Its own test uses two
  slow samples in a hundred rather than one, because one lands exactly on the
  p99 boundary and says nothing either way -- the sort of check that looks
  like a measurement and is an arithmetic accident.

**Notes on the encode loop**

- **The loop is written against the encoder trait**, so the second backend is
  a construction change rather than a second loop, and the tests drive the
  same code through a fake encoder with no device and no hardware latency.
- **A seat has four states and each transition has one owner**, which is what
  keeps the handoff lock free. The loop promotes a claimed seat at the top of
  a frame, before the gate runs, so the guests a frame goes to are fixed for
  that frame and a guest arriving mid-frame waits for the next one rather than
  being handed a predicted frame it cannot decode.
- **The loop empties a leaving guest's ring, and only the loop can.** The
  guest stops touching it before marking the seat, so a push already in flight
  lands after that; every index dropped instead is a pool slot that never
  comes back, and one leak per session exhausts a host.
- **A frame a guest did not get is a broken reference chain whatever the
  reason.** Three ways to lose one, and each latches the guest and reaches the
  refresh that recovers it: the room test refusing, a publish ring that is
  full, and no pool slot free. The last needed a new entry point, because the
  pass that would otherwise ask for the refresh is the one that could not take
  a slot. Removing any of the three fails its own test and no other.
- **`publish` reports which rings took the frame, not how many.** A count
  leaves the caller knowing a frame was lost and unable to latch the guest
  that lost it, which is the silent form of the failure the gate exists to
  prevent.
- **The pool is deliberately smaller than the guests could hold between
  them.** Sizing it so exhaustion is impossible costs a slot per guest per
  queued frame at the width of the largest frame a window can carry, which is
  tens of megabytes for guests that need none of it. Exhaustion is back
  pressure with a defined answer instead.
- **The send ring counts bytes now**, because the rate controller's peak is
  tracked from measured throughput and a controller fed zero collapses to its
  floor on the first congestion rather than to a fraction of what the path was
  carrying. Mebibits per second over an interval of at least half a second,
  which is also the period the controller increases on.
- **The gate's ceiling is the divided rate, not the configured one.** A second
  guest halves both what a guest may send and the window it is measured
  against.
- **The encode latency belongs to the stream, not to a guest.** One encode
  serves them all, so they all waited the same time for it; each guest folds
  the same figure into its own smoothed value because the cadence that reports
  it is per guest.
- **The announced generation and the one in every video header are read from
  one place**, so they cannot disagree. A peer told one number and shown
  another would be tracking a reference chain that does not exist.
- The host-mode message goes out before the first frame. It is thirteen bytes
  with no body, a peer stores it and gates nothing on it, so sending it costs
  less than being wrong about that.

**Notes on the ring geometry and the control channel**

- **The control channel was not attached at all**, so a peer's declaration was
  counted as unhandled and dropped while the group acknowledgement reported
  zero for that channel for the life of the session. A peer therefore
  retransmitted its declaration until it gave up, and nothing above could see
  the message that decides whether a guest is streamable.
- **The slot width is the fragment width, so it is also the datagram width.**
  A ring sized wider than the datagram floor does not gain headroom; it emits
  datagrams no probe has justified, and a peer that cannot take one discards
  the whole datagram rather than truncating it. The previous width put every
  full fragment 207 bytes past the floor, which nothing noticed because no
  message long enough to fill a fragment had ever been sent.
- **The video ring is the peer's ring depth, which is the gate's top ceiling.**
  Those two numbers have to be the same or a frame the gate admits is refused
  by the ring it is admitted into, and the test says so rather than the
  constants agreeing by coincidence.
- **The largest frame that fits is four bytes short of the arithmetic**, since
  the length prefix rides in the first fragment.
- **A take that does not fit does not consume the message.** The channel only
  advances on a completed take, so the same message would be read again every
  pass at full speed. It ends the attempt with its own outcome instead.
- Nothing is attached for video receive. Video is host to guest only, and an
  unattached channel acknowledges zero, which is the truth about a channel the
  peer never sends on.

**Notes on the bitrate budget**

- **Two aggregations compose and they do different jobs.** Each guest's ceiling
  is the configured rate divided by the guests on its stream, which bounds what
  the host can send in total; the rate applied is the minimum of what their
  controllers return, which bounds it to what the slowest path carries. Each
  has its own test, and breaking either fails only its own.
- **The slowest guest does pull everyone down, and that is intended.** The rate
  is what the transport can actually carry, and sending a guest more than that
  produces loss rather than quality. What the slow guest must not do is break
  the others' streams, and it cannot: delivery is decided per guest by the
  gate.
- **A guest arriving and the operator changing the rate are the same event.**
  Both move a ceiling, and a controller has to be told rather than discovering
  it, because the rate it is holding may already be above the new one.
- **The tick is the frame.** The controller's periods are counted in ticks, so
  at sixty a second its thirty clean ticks are half a second. Ticking it from a
  timer would silently change what those numbers mean.
- **The deadband is what stops a reconfigure per frame.** Removing it fails its
  test, which is the point of having one for a behaviour whose absence is
  otherwise invisible.

**Notes on session initialization**

- **The recorded initialization is parsed by the code that will meet a real
  one.** The replay finds the message a stock client actually sent, runs the
  parser on it, and checks that argument 0 really is the body length. A fixture
  we wrote would only prove the parser agrees with the writer.
- **Two of the fields are sentinels, not measurements.** A maximum size of
  60000 means no limit and a resolution of zero means no preference; a host
  taking either literally tries to encode a picture nobody asked for.
- **Only the version is mandatory.** Everything else defaults, and unknown keys
  are ignored rather than refused: peers send different objects, and requiring
  a shape refuses them over fields nothing reads.
- **A body that will not parse leaves the guest on the clock** rather than
  admitting it or abandoning it early. It is the same position as a guest that
  has not spoken, and the deadline already covers that.
- **Time is a parameter, as it is in the core.** Both this and the delivery
  gate previously took a clock reading, which made two of their tests unable to
  reach the case that mattered: nothing could advance five seconds, or half a
  second, without sleeping. The deadline and the keyframe throttle are now
  driven by a millisecond figure, and both tests check the far side of the
  interval rather than only the near one.

**Notes on the packetiser**

- **Parsing proves we can read a peer; re-emitting proves a peer could read
  us.** The corpus replay now takes every recorded video header, re-encodes it
  with our own writer, and requires the bytes back byte for byte -- 4883 of
  them. It then reframes the message and requires the fragment count to match
  what the recording actually used. A field written at the wrong offset, in the
  wrong endianness, or with the rotation off by one fails there rather than in
  a client that renders nothing and says why.
- **Shown capable of failing.** Writing the rotation zero-based, which is the
  documented trap, fails the comparison against the recording.
- **Almost nothing about a video header is per frame.** Dimensions, rotation
  and the generation counter are fixed for a stream and only the keyframe flag
  moves, so the packetiser is a value that outlives a frame rather than a
  function taking six arguments that could each be got wrong per call.
- **The generation counter moves only on reconfiguration**, never per frame,
  and a bitrate change is not a reconfiguration: it neither reinitialises the
  encoder nor changes what a decoder must do. A test frames fifty pictures and
  requires the counter not to move.

**Notes on the delivery gate**

- **The cascade is the invariant and the latch is how it is kept.** A guest
  that misses one frame must miss every frame until a keyframe. Delivering a
  single dependent frame across the gap breaks the reference chain silently:
  the decoder keeps going and produces progressively wrong output rather than
  failing, which is the gray-frame failure. The regression test starves a
  guest, drains its window completely, and asserts every predicted frame is
  still withheld -- it fails the moment the latch is removed.
- **The wrong thing is unsayable.** There is no operation that withholds one
  frame without marking the guest pending, and a caller is told only which
  guests take the frame, never which were withheld from. An interface that
  exposed the other half would eventually have it called.
- **A skipping guest is retested against the largest frame the session has
  produced**, not the frame in hand. Testing against the frame in hand lets a
  guest out of the cascade on a small predicted frame, whereupon the keyframe
  it actually needs does not fit, the throttled grant is spent, and every guest
  pays the spike for a recovery that did not happen. Its own test fails, and
  only it fails, when the comparison is swapped.
- **A joining guest starts pending**, which is what produces its join keyframe.
  Nothing separate arranges one: a guest that has received nothing is in the
  same position as a guest that has fallen out of the chain, so it is the same
  state rather than a second one to keep in step.

**Notes on the collect block**

- **Reading one field wrongly is invisible; reading four is not.** The block
  is audited on every hardware run rather than trusted: the picture kind
  against the one refresh that was asked for, the frame index against the
  collect order, the quantiser against the range the codec has, the structure
  against a whole frame, and the length against the last byte that was
  actually coded. A block read at offsets the driver did not write does not
  land on five right answers at once, and each of them is something the pool
  or the packetiser is about to depend on.
- **The length assertion is the regression test for the race above**, and the
  slice count is its early warning. The count is the more sensitive of the
  two, so it is checked on every picture rather than only where it failed.
- **What was measured and proved nothing is gone.** The macroblock counts and
  the timestamp echoes stay zero under this configuration, and a field that
  cannot move cannot witness anything.

**Notes on the frame pool**

- **The refcount is the only thing that says a slot is reusable**, and it
  is the first cross-thread handoff in the workspace outside the shared
  primitives, so it carries phase 0's obligation: model checked under
  `loom`, and **shown capable of failing**. Weakening the release on a
  guest's decrement and the acquire on the producer's search makes `loom`
  report a causality violation on concurrent access to the frame storage;
  restoring them makes it pass. A model check that cannot fail proves
  nothing about the orderings it is supposed to be exercising.
- **The count is raised before any index is pushed.** Raising it after
  would let the first guest finish and take the slot to zero while later
  guests were still being handed the same index, and the producer would
  then be free to overwrite a frame nobody had sent yet. A ring that
  refuses the index gives its hold straight back, or the pool bleeds one
  slot per congested guest until it stops entirely.
- **One place releases the writer's own hold.** Publishing and abandoning
  both end in the same drop, so neither path can release twice -- which
  the first version did, and which handed slots back while a guest still
  held them.

**Notes on predicted pictures**

- **A refresh happens on request, or when there is nothing to predict
  from.** The second half is not a special case for the first picture; it
  is the same rule, because a reference we do not hold is one we cannot
  point at. That is what makes the refresh request meaningful on this
  backend at last, and with it the gate asking for zero keyframes across
  a bitrate change becomes statable: a backend that refreshed every
  picture would have satisfied any keyframe assertion trivially.
- **The reference is named twice and both are load bearing.** The slice
  points at it, and the picture parameters list it separately. A picture
  missing from that list is one the driver is entitled to release, and it
  will, while the slice still points at it.
- **The counter widths are exercised at a real value rather than zero.**
  They size fixed-width fields in every slice header, so a width the
  writer and the sequence set disagree on shifts every field after it.
  Zero would also wrap the frame number every sixteen pictures, which
  hides the question rather than answering it.

**Fixed**

- **The collect asked the driver not to wait, and the driver answered
  anyway.** `NV_ENC_LOCK_BITSTREAM::doNotWait` neither is ignored nor reports
  a busy lock: set, it returns success on a block the driver has not finished
  writing, with the coded bytes in place and the length not. A refresh picture
  came back claiming megabytes it had never coded, and the slice count came
  back as noise. Both were taken for driver defects for a day; they are one
  race, and it was ours. **A flag that is ignored is harmless and invites a
  retry on a newer driver; a flag that answers wrongly must never be set.**
  Clearing it costs nothing measurable -- the lock takes the same 0.7 to
  1.8 ms either way, and the caller is still never parked, because what gates
  the lock is a completion marker rather than the flag.
- **The quantiser floor was configured but never applied.** It was added to
  the configuration block, documented, and given a default, and nothing ever
  wrote it to the rate controller. The change that added it was verified by
  checking the encoder still encoded, which it did either way: a check that
  cannot distinguish the two states proves nothing about which one holds. It
  is applied now, to refresh and predicted pictures alike -- a floor on the
  predicted ones only would leave the largest picture in the stream, and the
  one that matters most for delay, unbounded.

**Notes**

- **A picture is read from one surface and reconstructed into another, and the
  two are never the same.** The interface takes a surface at the start of a
  picture and a second one in the picture parameters, and it is tempting to
  pass the same identifier to both, because for a stream of independently
  coded pictures there is no reconstruction to keep. That is wrong: as the
  driver takes a surface into its reference store for the first time it
  releases the buffer backing it and allocates a replacement, and the pointer
  it encodes from was captured at the start of the picture. It then encodes
  from freed memory. The first picture tends to survive, because a replacement
  allocated immediately after a release usually lands on the same block, so
  the failure presents as an intermittent fault a few pictures in with nothing
  in the call chain naming a surface. Two pools, paired by index, and the
  question does not arise. The context test asserts the pools are disjoint.
- **The bitrate does not travel in the sequence parameters.** That structure
  has a field for it, and one driver never reads that field; it takes the rate
  only from a separate rate-control parameter, and silently runs on its own
  default otherwise. The field is still filled because another driver does read
  it, but the rate-control parameter is what makes it true. This is what the
  congestion actuator will drive, so a rate that is quietly ignored would have
  been discovered much later and at much greater cost.
- **The source's content is a function of the frame index, and that is what
  makes an encoder checkable.** A bar whose left edge is `index * step` can be
  found in a decoded picture by a checker that shares nothing with the
  producer but the frame number, with an independent decoder in between. That
  catches what a structural check cannot: a wrong upload stride shears the
  picture, a wrong plane offset moves or destroys the bar, and an off-by-one
  in ordering shows as a bar one step out -- all of which otherwise produce a
  stream that parses, decodes, and reports the right resolution. The two
  chroma components of the static block deliberately differ, because equal
  ones survive being written in the wrong order.
- **Noise would have been the wrong content.** It is incompressible, so every
  frame arrives at the rate ceiling and nothing about rate control behaves as
  it will in production; it cannot be checked without reproducing the exact
  generator, which lossy coding defeats anyway; and it offers motion search
  nothing to track, so every picture is effectively intra and the predicted
  path is never exercised. Worth having later as a worst-case size stress, not
  as the default.
- **One input surface per in-flight picture, on both backends.** One surface
  shared across a queue is overwritten while the hardware is still reading it,
  so the encoder emits the newest content under an older picture's timestamp:
  output that decodes cleanly and is wrong, with no error anywhere.
- **Parameter buffers are the caller's to release.** They are read while the
  picture is being assembled and are not consumed by it, so the eight a picture
  carries accumulate for the life of the context until they are destroyed
  after the picture closes. They are released on the failing paths too: a call
  that got far enough to return a status has already read them.

- **The vendored headers are pinned to an old interface version on purpose.**
  Every encoder structure carries a version stamp taken from the header it was
  compiled against, and the compatibility runs one way: a newer driver accepts
  an older stamp, an older driver rejects a newer one on every call and reports
  only that the version is invalid. So the header chooses the binary's minimum
  driver. Pinning to the newest available would have floored us above what
  current distributions ship. Every feature the backend needs was checked
  present in the older header before the pin was taken.
- **The bindings are generated, and committed rather than built.** Generated,
  because the structures are version-stamped and bitfield-heavy and a
  hand-transcription error is not a compile error but a runtime status code
  with nothing pointing at its cause. Committed, because a build script would
  put a C toolchain on every build machine and in continuous integration, which
  installs nothing today. The generated output carries its own gate: forty-two
  compile-time assertions of size, alignment and every field offset, so
  building is the check.
- **Codec and preset identifiers cannot be linked against.** They are
  file-static constants in the header, so no exported symbol exists for any of
  them, and a generator renders each as an extern static -- a reference that
  can never resolve. They are emitted as constants instead, produced from the
  header text so that nothing is hand-copied.
- **A group-level lint allow loses to an explicitly configured lint**, on
  either side of the command line. Allowing the whole lint group over generated
  code left two hundred errors standing; the entries are named individually.
- **No function is declared in the generated output.** The libraries are opened
  at runtime, so an extern block would turn a missing driver into a failed
  start instead of a missing backend.
- **The loader lives in the common crate, not beside its first caller.** It is
  the piece that differs per platform while everything above it does not, and
  that crate is the only one in the workspace containing `unsafe`, which keeps
  that obligation in one auditable place.
- **Symbols resolve eagerly and privately.** Eagerly, so a library whose own
  dependencies are missing fails at open where a caller can fall back, rather
  than at the first call through a function pointer. Privately, because this
  ships inside a shared library loaded into other processes and publishing a
  vendor runtime's symbols can capture lookups never meant for us.
- **Both loader tests were shown capable of failing.** Making the open return
  nothing fails four of the five; making every symbol resolve fails exactly the
  one written to catch it, and no other. A pair that cannot both pass under a
  broken implementation is the point.
- **The compute device is selected by bus address and a miss is an error.** A
  machine with two GPUs has the frame source on exactly one of them, and
  encoding on the other moves every frame across the bus, which is a readback
  under another name and which section 4 of the host document requires to be
  chosen rather than discovered. There is no fallback to another device,
  because that failure would surface as a latency figure rather than as an
  error. The address is read at construction and never stored: enumeration
  order is not stable across driver reloads and neither is which card drives
  the display.
- **The hardware settled the phase's open question: there is no asynchronous
  completion on this platform.** The capability reports absent for both codecs,
  so a completion object cannot be waited on and the collect has exactly two
  honest options: the non-blocking form of the bitstream lock, or a compute
  event recorded on the encoder's own stream. A blocking collect padded with
  queue depth is the third option and is the one to avoid, because it converts
  "the encoder fell behind" into "the pipeline thread is stopped", which is the
  one moment it must not be. The gate item that requires a non-blocking poll is
  therefore load bearing rather than defensive.
- **Live bitrate change is supported, so the congestion actuator can exist.**
  Asked rather than assumed, because the only actuator the design has would
  otherwise be unimplementable and the gate that counts keyframes across a rate
  change could never pass.
- **The encoder accepts packed colour formats directly.** That is the internal
  conversion the pipeline exists to avoid, and its availability is exactly why
  the rule against it has to be explicit: it is the easy path and it is
  measurably worse. The planar format the conversion targets is accepted too,
  which is what makes the rule followable.
- **The configuration starts from the preset and overrides only what is
  required.** Building one from zero means silently accepting a default for
  every field nobody thought about, and the fields nobody thinks about in an
  encoder are the ones that add a frame of latency.
- **No B-frames, though the hardware offers up to seven.** Every one of them is
  reorder delay: latency paid on every frame to save bits on some of them,
  which is the wrong trade for this product. Output order is pinned to capture
  order for the same reason.
- **One frame of rate-control buffer.** A larger one lets the encoder smooth
  bitrate across frames, which is precisely the queueing this pipeline exists
  to avoid; those bits arrive late rather than not at all.
- **Keyframes are never scheduled.** The interval is set to infinite, so one is
  produced when the delivery gate asks and at no other time. A periodic
  keyframe on top of a throttled on-demand one is bandwidth spent to a
  timetable rather than to a need.
- **Parameter sets repeat on every keyframe.** A guest joining mid-stream is
  then decodable from the next keyframe alone, with no separate out-of-band
  step that can be got wrong.
- **The reconfigure clears both the reset and the keyframe flags.** Either one
  turns a rate change into a visible discontinuity, and congestion moves the
  rate many times a minute.
- **The initialisation block does not keep the pointer it was given.** The
  interface copies the configuration during the call, and the block it named is
  a local about to be moved into the returned value, so retaining it would
  leave a dangling pointer in a structure that is reused on every reconfigure.
- **The second backend pins its headers the opposite way round, and for a
  reason.** The codec interface stamps a version into every structure and an
  older driver rejects a newer stamp, so that pin has to be low. This one has
  no stamp, passes buffer sizes explicitly at every call, and grows its
  structures by appending, so a driver older than the header reads the prefix
  it knows. Compiling against a header older than the installed runtime is the
  safe direction either way; only the reason differs, and writing down which
  reason applies is what stops the next person applying the wrong rule.
- **Cropping is measured in chroma samples, not pixels.** A coded picture is a
  whole number of macroblocks, so 1080 rows are coded as 1088 and eight rows
  are cropped -- but the field takes four, because each unit is two rows at
  4:2:0. Writing pixels crops twice what was intended, on every decoder, and it
  presents as a capture bug rather than as a parameter-set one.
- **The parameter-set tests check structure, not meaning, and that distinction
  is worth keeping visible.** They agree with the writer because both came from
  one reading of the standard, so they would agree just as well if that reading
  were wrong. Nothing yet proves a decoder reads the colour description as
  intended, and a parameter set alone cannot be used to find out: a decoder
  reports nothing about a stream containing no picture. The check arrives with
  the first frame this backend encodes, and the dumper that will perform it is
  in place rather than left to be remembered.
- **One backend has nowhere to put the colour description.** Its sequence
  parameters carry a single aspect-ratio flag and no colour fields at all,
  where the other takes primaries, matrix, transfer and range as ordinary
  structure fields. Since the description is required and not optional, that
  backend has to write its own parameter set and hand it over as a packed
  header. This is why the packed-header attribute was worth asking about, and
  why the answer only arrived by looking at the structures rather than at the
  capability.
- **Start-code escaping is the kind of fault that arrives with content.** A
  payload may not contain a start code, so a run of two zeros followed by a low
  byte needs a marker inserted. Omit it and the stream decodes correctly until
  the day the encoded data happens to contain the pattern, which attributes
  itself to anything but the writer. Every byte that can end a start code is
  covered by a test, and the zero run restarts after each insertion.
- **The parameter-set writer is testable without hardware**, because the coding
  is fixed by the standard rather than by a vendor. Its expectations are the
  standard's own code table written as bit strings, not values captured from a
  device, so they can be checked by eye. Both halves were shown capable of
  failing: reversing the bit order fails four of the eight, and removing the
  escaping fails three, and no test overlaps both.
- **The quantiser floor is a latency control and reads backwards.** A lower
  floor lets the encoder spend more bits on a frame, and more bits is a larger
  frame, more packets, and longer in every queue between here and the far side.
  Below about five those bits buy nothing the eye resolves, so they are spent
  purely on delay. The setting with the *higher* floor is therefore the
  lowest-latency one, which is the opposite of how a quality knob reads, and it
  is the default here because latency is this product's first goal.
- **An unsupported attribute reports a sentinel, not zero.** Reading the
  interface's not-supported marker as a bit set makes every bit read as set,
  which turns "this device does nothing" into "this device does everything".
  Folded to zero at the boundary, once, rather than at each use.
- **What a driver accepts is not what it requires.** The packed-header
  attribute says which parameter sets the driver will take from us, and reading
  it as which ones we must supply is the same capability-for-behaviour mistake
  that a renderer's format list invited earlier in this phase. Whether the
  driver emits them unasked is settled by encoding a frame and looking, not by
  an attribute. The accessor is named for what it answers.
- **A profile is not an encoder.** A device may decode a codec and not encode
  it, and both are entry points against the same profile, so the query asks for
  the encode entry point specifically. The test asserts the refusal as well as
  the answer: a profile the driver cannot encode must come back empty, or the
  positive result proves nothing.
- **Generated code satisfies the safety lint rather than being exempted from
  it.** Trailing-array helpers perform unsafe operations inside unsafe
  functions, which the workspace denies. The generator now wraps them, which is
  a flag; adding the lint to the module's allow list would have been a flag
  too, and would have quietly widened what the crate permits.
- **There is no way to learn a picture is finished without waiting for it.**
  Two mechanisms were tried and measured. The interface's own no-wait flag is
  documented for exactly this case and is ignored by the driver. A completion
  marker recorded on the encoder's own stream does gate -- most polls in a
  burst come back not-ready -- but it passes before the bitstream can be
  retrieved, because the encode runs on a hardware engine rather than on that
  stream. What the marker does buy is the half that matters: a caller with
  nothing ready gets an answer in about 300 ns instead of being parked for a
  frame. The phase gate was narrowed to that, and the retrieval cost recorded
  as a number.
- **A stream handle is not a pointer to a stream.** The encoder's stream setter
  takes the address of a handle, and passing the handle makes the driver
  dereference it as memory. It faults inside the driver with nothing pointing
  back at the call site, which is the worst diagnostic shape available.
- **Field declaration order is load bearing when fields own driver objects.**
  Fields drop in declaration order, and the session owns the compute context;
  a stream destroyed after its context has been released is a use-after-free.
  The encoder therefore declares its stream and markers first and its session
  last, and the comment says why, because the next person to add a field will
  not guess it.
- **The no-wait collect is documented and not implemented.** The interface
  states that its no-wait flag returns a busy status when a picture is not
  ready, explicitly including the synchronous mode that is the only mode this
  platform offers. The driver ignores it: four collects for four pictures, not
  one busy status, and the slowest collect equal to one picture's encode time.
  A genuinely non-blocking lock spun in that loop would report busy hundreds of
  times. **The phase gate for a non-blocking collect is therefore not met by
  this path**, and is met instead by recording a completion event on the
  encoder's own stream and querying it, which is the next piece of work. The
  measurement is kept as a test rather than deleted, because it is the evidence
  that the cheap path was tried and does not work, and because a driver update
  could change the answer.
- **Nothing was allowed to pass by lowering the bar.** The obvious repair when
  the assertion failed was to relax it. That would have made the gate item pass
  against exactly the behaviour it exists to reject, so the assertion was
  replaced by a recorded number and the gate left open.
- **The test bug that hid the finding is worth naming.** The first version
  polled once to time it, then drained a full queue's worth, having already
  consumed one picture; it waited forever for a frame that could not arrive.
  The symptom was a hang, and the temptation was to treat the hang as the
  finding. The actual finding was one layer down and only visible after
  instrumenting the status the driver returned.
- **The selection test proves itself the same way the loader's does.** On a
  machine with one compute device, matching the display's address cannot
  distinguish a correct implementation from one that ignores the address
  entirely. What distinguishes them is the second assertion, that an address
  belonging to no device is refused: an implementation returning the first
  device regardless would satisfy the first check and fail that one.

## 4: signaling (in progress)

**Added**

- `lowlat-crypto`: credential generation, key material decoding, and the only
  source of randomness in the workspace.
- The admission seam in `lowlat-host`: register an attempt, add a candidate,
  approve returning host credentials, end a connection, and an event queue the
  application drains.
- `lowlat-kessel`: the connect URL, the message set, and a transport with one
  reader and one writer over a queue, so producers never touch the socket.
- The host advertisement, and a runnable endpoint that publishes a host into the
  discovery listing and holds the connection open.
- Reconnection with bounded exponential backoff and jitter, re-registering and
  re-advertising on every connection rather than only the first.

**Notes**

- **Entropy gets its own crate, below the core so the core cannot reach it.**
  The core owns no generator by construction and everything above it needs one:
  a session key, a check password, the seed a transaction identifier derives
  from. Until now every one of those was a constant supplied by a fixture, which
  is correct for a test and is not a source. One audited crate is the
  alternative to scattering it into whichever crate happened to need it first.
- **A generator that is not generating passes every length check**, so the test
  that matters asserts two draws differ rather than that one is the right size.
- **Credentials never render their contents**, whatever the format string, and
  a test asserts it. A credential reaches a log by accident, and the accident is
  worth making impossible rather than unlikely.
- **The advertisement's field order is pinned by a test.** A strict parser on
  the far side is entitled to care, and matching the order costs nothing here
  while being invisible to find later.
- **`app_v` is a string even though it holds a build number**, and a schema that
  types it as a number is wrong about the wire. Named test.
- **A candidate's `ip` is a string and its `port` is a number**, with exactly
  three booleans after them. Transposing the pair produces a candidate a peer
  accepts and silently ignores, which is the worst failure shape available, so
  the layout carries a named test rather than a comment.
- **The query hangs off exactly one root path.** Without the path the request
  line is malformed and the edge answers 400 rather than upgrading; with two the
  service has no such route. Named test, because the failure is a status code
  with no body and nothing that names the cause.
- **The credential is longer than the key.** The media key field carries far
  more material than the cipher consumes, and only its leading portion is key
  and nonce prefix. A validator written to the key's length rejects every real
  offer.
- **Both directions key from the host's material.** A client that supplies its
  own is signalling support, not proposing a key; the session is encrypted with
  what the host returns when it approves.
- **Exactly one TLS provider is chosen here**, rather than left to feature
  unification, which resolves to none and panics inside the TLS stack at the
  first connection. That reads as a crash rather than as a configuration gap.
- **Jitter is the part of a reconnect schedule that matters.** Without it, a
  service restart brings every host that was connected back on the same
  schedule, arriving together at exactly the moment the service is least able
  to take them. The draw is bounded below as well as above, so an unlucky run
  cannot hammer a service trying to come back. A fixed schedule passes every
  bound a growth test can state, so the test that matters asserts two schedules
  disagree.
- **The registering frame is resent on every connection, not just the first.**
  The service takes it as what associates the connection with the host, so a
  reconnect without it is a connection nobody has associated with anything.
- **A reconnect abandons what was negotiating and keeps what established.** An
  attempt still trading candidates is gone: the peer gave up when the
  connection carrying them dropped. A guest that already established never
  depended on that connection, and tearing it down because signaling blinked
  would drop a working session for an unrelated reason.
- **Silence is not a refusal.** A declined answer is a wire event the peer acts
  on at once; no answer at all leaves it connecting indefinitely, because
  nothing in the protocol reports a host that never replied. An offer refused
  on capacity was being dropped without a word, which is the worst failure shape
  available: neither side reports anything. Every offer is answered now,
  including the ones turned down.
- **"Still waiting for the host" is not a protocol outcome.** There is no
  message for it in either direction, so anything that needs to surface it owns
  the timer itself.
- **Two inbound actions were being dropped silently**: the service's close,
  whose reason is the only thing separating a bad session from an unknown host,
  and an opaque passthrough channel no schema lists. Both are reported now.
- **A keepalive without a deadline detects nothing; it only makes silence look
  like traffic.** A connection whose peer has gone stays established locally for
  as long as the kernel keeps retrying, so writes queue and nothing reports a
  fault. Found at ten hours: the socket up, bytes stuck in its send queue, a
  ping leaving every thirty seconds, no reply to any of them, and not one drop
  logged. Anything inbound now counts as a sign of life and two missed replies
  end the connection, which is the only thing that will ever notice.
- **Silence is not a stable state for a connection, and answering pings is not
  enough.** A host with nothing to say has to put something on the wire itself,
  on a schedule, because the path to the service closes an idle websocket after
  about a hundred seconds. Inbound pings are answered too, which is correct and
  was not the cause.
- **A working reconnect hid this for an hour.** The first diagnosis was that
  queued pongs were never flushed, and the connection dropping every two minutes
  was read as fixed because the host stayed in the discovery listing. It stayed
  there because it was reconnecting roughly thirty times an hour, fast enough
  that nothing above noticed. **A recovery mechanism masks the fault it recovers
  from**, so the measurement that settles it is drops per hour, not whether the
  host is visible.
- **The first regression test for it passed against the defect.** It asserted
  that an inbound ping was answered, which was true before and after the fix and
  therefore proved nothing. The test that discriminates asserts an idle
  connection transmits unprompted, and it fails in ten seconds against a client
  with no keepalive.
- **An established guest learns the peer left from the media path, not from
  signaling.** A peer that closes a session it was using does not withdraw its
  offer, so nothing arrives to say so. Without a liveness check the loop runs
  forever holding its socket, and the next guest walks to the next port; three
  connects in a row took three ports and freed none.
- **A withdrawal can overtake the offer it withdraws.** Observed: a cancel for
  an attempt arrived before that attempt's offer. Treating the cancel as a
  no-op for an unknown attempt then admits the offer behind it, spending a
  socket and a thread on a guest that has already gone. Withdrawals are
  remembered briefly so the offer behind one is refused.
- **The queue carries what the application did not cause.** Ending a connection
  is the application causing it, so it emits nothing; reporting it back
  produced a second terminal event for an attempt that had already reported
  one, and the reaping call would have looped.
- **A candidate marked `sync` is a readiness signal, not an address**, and the
  flag is a parameter of the call rather than the caller's business, because
  both ways of getting it wrong are silent. Adding one to the table spends
  checks on whatever the placeholder names, and a peer that sends a literal
  `1.2.3.4:1234` will be checked at that address. Never sending one is worse:
  a peer is entitled to withhold every real candidate until it sees one, so
  negotiation succeeds and then there is nothing to check. Approval queues the
  request without being prompted. Two named tests, both shown to fail.
- **The seam is polled, not called back.** A callback runs on our thread, so an
  application that blocks in one stalls a media loop and every integration has
  to reason about which thread it is on. A queue moves that decision to the
  caller and keeps our threads ours.
- **One socket per guest, so concurrent guests walk the port.** The socket
  punched for an attempt becomes that guest's media socket for the whole
  session rather than being handed back, so a second guest cannot have the
  configured port. Without the walk a host cannot admit a second guest at all,
  which turns the walk from a convenience into the thing multi-guest rests on.
  Named test over three guests taking P, P+1 and P+2.
- **The bound port goes back in the answer, not the configured one.**
  Advertising the port that was asked for when the bind walked produces a peer
  that answers checks and never establishes.
- **Credentials are generated at approval.** Earlier binds them to no socket;
  per registration leaks state for attempts that are never approved. Two
  concurrent guests are asserted not to share a media key.
- **A candidate that arrives before approval is kept.** Candidates trickle and
  the peer starts sending before the answer reaches it, so the early ones are
  buffered and handed over on approval. On a wide-area path one of them may be
  the only one that works.
- **The advertisement is emitted on state change, not on a schedule**, and it is
  driven by a stale mark rather than by a timer: something marks it dirty, the
  loop publishes and clears the mark. A capture appearing to show a ten second
  cadence was the layer above driving it, not the layer being measured -- the
  cadence was attributed before its cause was, and correcting that took two
  passes over the same document. Whether a host that advertises once stays
  discoverable over hours is open, and is a question for the listing rather than
  for argument.

## 3: io shell (2026-08-16)

**Added**

- `endpoint`: one object owning the connectivity engine and the session,
  classifying each datagram and reporting the sooner of the two deadlines.
- The acknowledgement cadence is labelled correctly: one the cadence produced
  carries the keepalive flag and a zeroed trigger.
- `lowlat-net`: the media socket with the full option set and the granted
  buffer readable at open, batched receive pulling a burst per syscall, and
  batched send with segmentation offload and a per-datagram fallback, the
  application send wake, the event loop that drives an endpoint over them, and
  the merged per-guest thread with its teardown.
- The bind walks forward when the configured port is occupied, so an occupied
  port delays a host's start rather than preventing it.
- `conn` retains the addresses reflexive servers report, so a caller that
  processes datagrams in batches can still ask for its own candidates.
- A second fixture endpoint that drives the namespace topologies through the
  real shell instead of a loop standing in for one. The topology matrix passes
  with it, as does a run between two machines on different networks.
- The sustained loopback soak, carrying gates 1, 2 and 4 in one harness: nothing
  lost, nothing allocated in steady state, and wake accounting as a number.
- The drain flushes a full staging batch instead of asking the core again with
  room it has already established is too small.
- The connect and teardown churn soak, counting descriptors, threads and
  resident memory across ten thousand cycles.

**Changed**

- **The wait reports which descriptor it heard from, and the pass leaves the
  other one alone.** Poll fills in the events per descriptor and the loop was
  discarding them, so every pass spent an eventfd read and a receive call to be
  told what poll had already said. An idle pass is now one syscall where it was
  three, and a pass carrying a stream is 1.94 where it was 3.

  Measured with a counting interposer on the two harnesses that already exist.
  The idle loop test is exact, because it runs ten passes on an injected clock
  with no traffic at all: eventfd reads 10 to 0, receive calls 10 to 0, polls
  unchanged at 11. The sustained loopback soak runs about 1700 passes against
  real traffic, and there the receive call falls to the 94 percent of passes
  that had something queued while the eventfd read disappears outright, since
  that harness drives the loop directly and never notifies.

  **The test is anything the wait reported, not readability alone.** An error or
  hangup bit is a condition to go and collect and is cleared by the call that
  collects it, so a pass that saw one and skipped the call would wake again
  immediately on the same unconsumed bit, for ever. That is a spin in place of a
  saved syscall, and gating on readability alone is how it would arrive.

  **The application ring is pulled on every pass regardless**, which is the one
  thing the gating must not reach. A producer can fill a ring and have its
  notify land after the wait returned, and a pull gated on the wake would hold
  that work until the next deadline.

**Notes**

- **A bind failure is not a startup failure.** The walk takes 50 ports from the
  configured one, each attempt on a fresh descriptor, because the option set is
  applied before the bind and cannot be retried on the socket that carried it.
  It stops at the top of the range instead of wrapping: wrapping lands on the
  privileged ports, where the bind fails for an unrelated reason and reports it
  as though the range were occupied.
- **The fixture endpoint is swapped, not grown.** The original loop stays as the
  reflexive server, which no shell provides, and stays deliberately the simplest
  thing that works. The endpoint under test is a separate binary owning a shell,
  so the two can be run against the same topologies and compared.
- **Signaling reaches the loop through the wake, not through a poll.** The
  fixture's rendezvous is read on its own thread and injected where the
  application's work is pulled. Polling it from the loop instead ties how fast a
  candidate is noticed to how long the loop happens to be waiting, and the loop
  waits on the endpoint's deadline -- tens of milliseconds when nothing is due.
  That delay is invisible against a peer that waits and decisive against one
  that does not, which is what made the difference in three topologies.
- **A test that passes because it is fast is not passing.** The loop that stood
  in for the shell polled every 5 ms and always punched outward first. The
  event-driven loop does not, and three topologies failed until the wake carried
  the candidate. Neither result was about the topology.
- **The churn gate's exact counts were shown to fail.** Leaking a descriptor
  per cycle takes the count from 4 to 254 over two hundred cycles, and leaking
  the guest takes threads from 52 to 252. Resident memory is the one with a
  tolerance, because an allocator holds arenas back and a plateau is not a
  slope; the two that can be counted exactly are asserted exactly.
- **A full staging batch is not a malformed emission, and treating it as one
  wedges the loop.** Staging hands back the room that is left; once that is
  shorter than the next datagram the core cannot encode into it and fails.
  Asking again unchanged returns the same failure forever, so the loop spun at
  full CPU with the stream stopped the moment a pass produced more than the
  batch holds. Two messages crossed and then nothing. The drain flushes, which
  makes the whole buffer available, and asks once more; a failure with all of it
  free is ours and the emission is dropped. **Every test that sends a datagram
  or two ran straight over this**, and so did the two-machine punch: it takes a
  sustained stream to reach at all.
- **A regression test for it has to punch first.** The first attempt queued a
  burst on one shell with a candidate and no path. Media waits until a path
  exists, so nothing was emitted, the batch never filled, and the test passed
  just as happily against the loop that spins. It proves nothing without a peer.
- **The receive buffer is the deployment's to grant, and now there is a number.**
  Ten minutes at 10009 datagrams/s on a stock kernel: 6005139 messages sent,
  6005139 received, no gaps, and **1648 datagrams dropped by the kernel** for
  want of receive buffer. Recovery carried every one of them, which is the point
  of the recovery, but the datagrams were still lost on arrival. With the
  ceiling raised the same run drops none. The request has always been logged
  against the grant; what this adds is that the grant is a deployment setting
  and the service will have to raise it rather than assume it.
- **The translator that filters must not be poisoned by what it filters.** Two
  fixtures let an unsolicited inbound check commit a connection entry whose
  reply direction was exactly the one the outbound punch then needed; with the
  external port pinned there was no second choice, so the punch was dropped for
  as long as the peer kept retrying. Real equipment discards unsolicited inbound
  without keeping anything, and the fixtures now do too, dropping before
  translation so the entry is never confirmed. Until that landed, the matrix
  turned on which side transmitted first, which is not what any of the six cases
  is about. It is shown by making the endpoint slow again: the arrangement that
  failed three of six now passes all six.
- **One green run proved nothing here.** A first hypothesis about the difference
  was supported by a single passing run and refuted by repeating it three times
  each way. On a fixture with a race in it, a single pass is not evidence, and
  the repeat is what turned an answer that fit the facts into one that was true.
- **A candidate reported once is a candidate lost.** The reflexive address was
  returned from the call that processed the datagram carrying it and kept
  nowhere. A loop that handles one datagram at a time can read that return
  value; the shell pulls a burst per syscall and has nowhere to put it, so the
  candidate a wide-area path depends on was learned and discarded on every
  path that matters. It is retained and asked for instead.
- **Landing on a port nobody asked for is opt-in.** Exhausting the walk returns
  the bind error. A caller that would rather have any port than none asks for
  that by name and must read the bound port back, because a host that silently
  takes an arbitrary port advertises the one it wanted and receives nothing on
  it -- a peer that answers checks and never establishes.
- **Classification and timer merging are protocol decisions, so they live in the
  core rather than in the shell.** There they run on injected time and replay
  from a seed; in the shell they would be the untested glue written once per
  platform. The shell's job against an endpoint is four calls.
- A shell arming from the session alone misses every connectivity deadline; one
  arming from connectivity alone polls forever once the attempt is over, because
  a finished attempt asks for no wakeups. Both are one-line mistakes and neither
  is visible in a short test.
- **Media has nowhere to go before a path exists**, so it waits rather than being
  emitted to a default destination or dropped. Named test.
- **The event loop's upper clamp was 5 ms, which is shorter than every deadline
  the session actually arms.** It bound on every wake and reinstated exactly the
  fixed over-poll the rule beside it forbids, while the gate two lines down
  exists to prove the loop is event driven. Raised to 50 ms, where it never
  binds in normal operation and still catches a core returning nonsense.
- **The socket is opened by the shell, not by the connectivity engine.** Two
  documents said otherwise; the engine is sans-IO and owns nothing. The rule
  that mattered survives: options are set once at open and nothing lowers one
  afterwards.
- **`poll` rather than an event port, deliberately.** The loop waits on two
  descriptors, the socket and the send wake. At that count a readiness scan is
  free and an event port saves no syscall per wait, since both are one call;
  it would only add registration state and a third descriptor. The trade
  reverses if a thread ever multiplexes many guests, which the threading model
  does not do, so revisit it only if that changes.
- **Batched receive is not an optimisation.** A single outstanding receive plus
  a poll loses a keyframe burst outright, so the batch pulls up to 64 datagrams
  per syscall straight into slots the kernel writes.
- **The address length is in and out.** A reused descriptor whose length is not
  reset before each pass presents the previous datagram's value and truncates
  the source address. Reset every slot, every pass; a named test sends from two
  sockets in turn and checks the second is not reported as the first.
- **The message descriptors hold raw pointers into the batch's own
  allocations**, so one field exists purely to keep an allocation alive and is
  never read through its handle. Removing it because nothing reads it would
  leave the kernel writing through dangling pointers.
- **This crate adds no model-checking obligation, and that is a finding rather
  than a skip.** Its batches are single threaded by construction, the
  application seam is a call rather than a ring, and the one shared word is a
  teardown flag carrying no payload: a model of it passes under relaxed
  ordering too, so it could not fail. What closes the teardown race is the wake
  descriptor, which a model checker cannot represent, because it cannot execute
  a syscall. The primitives that do carry the obligation stay model checked.
- **ThreadSanitizer is the checked build here**, and it runs clean over every
  test in the crate. It covers what the model checker cannot: kernel-mediated
  concurrency across a real spawn boundary.
- A churn test over spawn and teardown cycles covers what a single pass cannot,
  and seeds the connect-and-teardown soak.
- **Teardown wakes before it joins.** Setting the state and joining strands the
  thread in its wait until the deadline expires, which turns a clean disconnect
  into a visible hang. The state is set, the loop's descriptor is notified, and
  only then is the thread joined; teardown also runs from `Drop`, so a caller
  who forgets is not the difference between a clean exit and a stranded loop.
- **The teardown test's threshold sits below the loop's wait cap, deliberately.**
  At or above it the test passes without the wake at all, because the thread
  times out into the same check and joins looking prompt. Only a threshold below
  the cap can distinguish being woken from timing out.
- **No thread raises its own priority**, and the crate says why where someone
  would otherwise add it: a library outranking its host process's interface
  thread is a priority inversion that has shipped as a hard hang. The process
  class is the lever that works and it belongs to the application.
- **The wake is taken before the application rings are pulled, never after.**
  Anything enqueued from that point on leaves the descriptor armed, so the next
  wait returns at once. The reverse order consumes the token belonging to an
  item that has not been read yet and leaves it sitting until the next timeout.
  It is the same shape as a notify that never reaches its waiter: the wake
  exists and the sequence around it loses it. Named test.
- **Producers own their own descriptor** rather than sharing a reference count,
  so the send path touches no atomic refcount and ownership stays single.
- **Wake accounting is a counter, not a description.** The loop records why each
  pass woke, so "event driven rather than polling" is a number: an idle loop
  wakes about once per deadline it armed, where a ticking one wakes an order of
  magnitude more. Asserted at the shell.
- **The kernel's segmentation rules shape the send API rather than hiding
  inside it.** Every segment but the last must be the same size and all go to
  one destination, so a burst closes on a size change, a destination change, or
  a datagram needing its own hop limit. Making that visible is what keeps the
  caller from silently producing an unsendable batch.
- **A probe never rides with anything else.** It carries a hop limit that cannot
  reach the peer, so it leaves alone and the socket is restored in the same
  call. Asserted at this layer as well as in the core, because this is the layer
  that holds the option.
- **Offload is a fast path, never a requirement**, and it is dropped for good
  the first time a kernel refuses it rather than paying a failed syscall per
  burst. The burst test asserts offload was still enabled afterwards; without
  that it would pass identically on the fallback and keep passing if offload
  silently stopped working.
- **The keepalive is the acknowledgement cadence, not a separate schedule.**
  Every acknowledgement resets the cadence, whatever prompted it, so the timer
  fires only when nothing else has sent one and an acknowledgement leaves at
  least every 30 ms for the life of a session. It carries the keepalive flag and
  a zeroed trigger when only the cadence prompted it, and the ordinary flag with
  the real trigger when data did. A working note had this recorded as "keepalive
  emission is not implemented, an idle session eventually trips liveness". That
  was wrong: the cadence already emitted, so an idle pair never died. Only the
  label and the stale trigger were wrong. An idle pair is now driven past the
  hard liveness deadline in a test rather than argued about.
- **The crypto per-thread leak cannot occur here.** The primitives in use keep no
  per-thread state, so the churn soak is kept for leaks that can still happen
  and the original cause is recorded as absent by construction rather than as
  covered. A gate that passes without testing anything is worse than no gate.

## 1: protocol core (2026-08-16)

**Fixed**

- **A group acknowledgement is not a fixed length, and requiring one dropped
  every acknowledgement a whole peer generation sends.** The count of
  cumulative entries is the number of channels the *sender* carries: this
  implementation writes nineteen, and a generation in current use writes four,
  making its acknowledgement 23 bytes rather than 83. The parser refused
  anything shorter and the shell discards a parse failure, so those
  acknowledgements vanished without a log line.

  **It presents as a peer that has stopped receiving**, which is the reading
  that costs the session: the send window only grows, every fragment goes
  stale, the scan retransmits the lot -- measured at nine times the payload --
  and the delivery deadline ends the guest as undeliverable while that peer is
  decoding perfectly well and saying so in its own latency reports. The same
  host served a different peer generation on the same build throughout, which
  is what made it read as a network fault rather than a wire one.

  The count now comes from the packet length, and a channel the sender did not
  report is treated as unreported rather than as an acknowledgement of nothing:
  reading an absent entry as a zero does nothing most of the time and, near a
  sequence wrap, looks like an acknowledgement that never happened.

- **A datagram the endpoint refuses is counted rather than only dropped.** The
  drop itself is right, but it left no trace, so a peer speaking the wire
  differently and a path carrying nothing produced identical logs -- and the
  per-channel counters cannot see it, because a rejected datagram reaches no
  channel. That is where a real mismatch hid.

- **The progress line carries datagrams and the smoothed round trip.**
  `rx_frag` counts what reached a channel, which an acknowledgement never does,
  so the line could not tell a peer that had stopped reading from one whose
  acknowledgements were arriving and being discarded. Both look like a window
  that only grows. The raw datagram counts separate them, and a round trip that
  never leaves zero says no acknowledgement was ever applied.

## 2: connectivity (2026-08-16)

**Fixed**

- **Both address families reach the wire.** Four faults, found by reading the
  candidate exchange end to end against a multi-peer capture.

  **A peer's candidate was edited as text before it was parsed.** The v4-mapped
  prefix was stripped from the front of the string, which handles the dotted
  spelling a peer usually sends and turns the equally valid hex spelling into a
  fragment that parses as nothing -- so that candidate was dropped without a
  word and the peer was never probed there. It is parsed first now, and the
  collapse to IPv4 is left to the connectivity engine, which already does it to
  every address it is handed; a second copy here would be a second place for
  that rule to drift.

  **No IPv6 host candidate was offered at all.** The routing-table probe asked
  the v4 family only, so a machine with global v6 advertised its v4 address and
  nothing else, and a v6-only peer had nothing from us to probe. Both families
  are asked now, and a family the machine does not have contributes nothing
  rather than failing. Verified live: the service that offered one address now
  offers two.

  **A reflexive server name contributed one family, whichever the resolver put
  first.** A dual-stack name answers with both and the order follows the host's
  own addressing, so a machine with global v6 learned its v6 reflexive address
  and no v4 one. One of each family is taken, and because two per name can
  exceed what the engine holds, what is dropped is now logged rather than
  silently discarded.

  **A readiness marker was parsed before its flag was read.** The receiver
  ignores the address on one, so peers put different things there -- the capture
  carries both the well-known placeholder and a sender's own reflexive address
  -- and an unparseable one took the barrier with it. A peer that withholds its
  real candidates until the barrier arrives then waits for something that had
  already come, with nothing logged at either end. The flag is read first.

- **The v6 path refuses to fragment.** `IP_MTU_DISCOVER` was set and
  `IPV6_MTU_DISCOVER` was not, and neither setting carries to the other:
  measured, a dual-stack socket sat at the v6 default, which fragments locally
  rather than refusing. The path probe reads an arrival as the size having
  worked, and since IPv6's minimum is 1280 while the ladder climbs to 1400, the
  rungs above the minimum were reportable on a path that could only carry them
  in pieces.

- **Host candidates are gathered by the SDK, not by the application.** Which
  local addresses are worth offering is a connectivity decision with a rule
  behind it, and an application that had to re-derive that rule would reach a
  different answer per integration -- the daemon had the whole filter in its
  own `main`, where nothing else could reach it. It lives beside the socket
  now, and the seam raises a host candidate as an ordinary candidate event, so
  an application relays what it is given and decides nothing. **The readiness
  marker is raised before them**, since a peer may withhold its own candidates
  until it has seen one and anything queued ahead of it delays both directions.

  Reading a peer's candidate exchange moved the other way, into the signaling
  crate that already owns the message: the barrier and the address parse are
  what that message means, not what a host does with it. The daemon's `main` is
  wiring again and carries no tests, because it carries nothing to test.

- **IPv4 host candidates are enumerated, not probed.** This machine sits on one
  subnet through both a wired and a wireless interface, and the routing-table
  probe named only the wired one -- a peer that could reach the other was
  offered nothing it could use. Every interface that is up is walked now, and
  only private address space is kept: a publicly routable address is already
  discoverable reflexively, so offering it as a host candidate too is a
  duplicate that costs part of a bounded check budget.

  **The v6 side stays probed, which is the opposite treatment for the same
  reason.** There is no translation on that family, so the address a peer sees
  is the source we would send from, and one interface here carries three global
  addresses at once -- a stable one, a temporary one and a route-local one -- of
  which only the kernel's chosen source is worth advertising. Enumerating offers
  all three and makes the peer spend checks finding out which answers.

  Shared address space is offered behind `--shared-address-space`, reachable
  only when both ends are behind the same carrier translation or on the same
  overlay network. The list is capped and a cap that binds is logged.

- **Reflexive servers are named, and both families of a name are asked.** A
  literal can only ever be one family, and a v4 literal is why this host had no
  v6 reflexive candidate to offer: a dual-stack name answers with an A and an
  AAAA record and both are now taken, one per family, with what will not fit
  reported rather than dropped in silence. A name that does not resolve is
  reported and skipped by the service, because an attempt with no reflexive
  server still punches on what it gathered locally; the configuration call
  refuses instead, while the caller can still fix it. The rule lives in one
  place and both front doors call it.

  **A host-side switch for refusing IPv6 was built and then removed.** Which
  families a session uses is the connecting side's to decide, and a stock peer
  with IPv6 turned off already stops offering v6 addresses on its own -- so the
  switch duplicated a decision that was already being made, in the wrong place.

- **A candidate that is not an address is declined out loud.** Peers anonymise
  host candidates behind a `.local` name that only multicast resolution
  answers. None is resolved, which is correct, but a candidate silently dropped
  and one deliberately declined are indistinguishable afterwards.



The punch, sans-IO like the rest of the core. Candidates in, checks out, a path
or a typed failure.

**Added**

- `stun`: the check codec. Binding requests carrying the attribute set and order
  a peer expects, binding responses carrying the address a request was observed
  from, integrity and fingerprint over both.
- `conn`: the punch state machine. Candidate table, check schedule, the
  once-per-attempt mapping probe, first-answer-wins path selection, and a typed
  failure when the window closes.
- An output carries a destination and a send-time TTL, so a probe cannot be
  emitted without the shell being told to restore the socket afterwards.
- `hmac` and `sha1`, default features off, asserted allocation free rather than
  assumed to be.
- `demux`: the two-byte classification that lets checks and media share one
  socket.
- Reflexive discovery: bare binding requests to a server, and the address it
  reports emitted as a candidate.
- `check` fuzz target over the classifier, the parser, every accessor, and
  verification.
- `lowlat-sim`: address translation modelled as two independent behaviours,
  chained translators, hairpin, a seeded path with loss, duplication,
  reordering, jitter, and hop-limited delivery.
- The topology matrix, each case stating the outcome it expects.
- The recovery gate: ten thousand messages across a degraded path, delivered
  and in order.
- Network namespace fixtures: six topologies against a real kernel, driven by a
  fixture endpoint that runs the engine over a real socket.
- Peer-reflexive candidates: the source address of a verified check becomes a
  candidate and is checked like any other.

**Notes**

- **The check length field is written twice.** Integrity covers a message whose
  length claims to end after the integrity attribute, while the length left on
  the wire claims to end after the fingerprint. Hashing the bytes as received
  fails every message. The digest is fed a substituted value instead of the
  message being copied and edited.
- **Integrity and fingerprint must be adjacent and last**, and a message outside
  52 to 256 bytes is refused before parsing. A peer rejects both cases with no
  diagnostic, so the codec rejects at exactly the same boundaries.
- **The mapping probe is emitted once per attempt, not once per candidate.** It
  exists to open the local mapping, not to reach anyone, so repeating it per
  candidate buys nothing and spends budget.
- **There are two passwords and swapping them still authenticates.** A check we
  send is signed with the peer's password; a check we receive was signed with
  ours. Both directions carry a test that fails if they are exchanged.
- **The window is 7500 ms at a 500 ms per-candidate cadence**, so an attempt has
  about fifteen checks per candidate and no slow retry tier behind it. A
  candidate that cannot answer must therefore never be admitted, which is why a
  gateway mapping outside globally routable space is discarded rather than
  offered.
- Transaction identifiers are derived from a per-session seed rather than
  generated. The identifier is echoed rather than validated and integrity is
  what authenticates, so deriving it keeps the core free of a random number
  generator and makes a failing run replayable from its seed alone.

- **Two trust domains share the check codec.** A peer check is authenticated and
  verification is the whole of its admission. A reflexive server's answer
  carries no credentials at all, so it is admitted only on a transaction
  identifier still outstanding toward that exact address. Parsing therefore
  accepts an unauthenticated message and can never be mistaken for having
  authenticated one, which is why `is_authenticated` exists and why
  verification refuses such a message under every password.
- **Classification is asymmetric on purpose.** Anything not shaped like a check
  goes to the record layer, where authentication rejects it, so the check
  parser is never handed input that was not already check-shaped.
- **Mapping and filtering are separate knobs**, and the matrix is the pairings
  of them. Mapping decides whether the address a peer was told about is the one
  our packets leave from, which is what a punch depends on; filtering decides
  what is let back in, which is what simultaneous open defeats. A model with one
  knob cannot express the difference and the difference is the whole matrix.
- **Carrier-grade translation is decided by mapping behaviour, not by the number
  of layers.** Two layers that keep mappings endpoint independent are punchable
  and the matrix requires them to establish; a symmetric carrier translator is
  not. The plan previously assumed all carrier-grade cases fail, which would
  have made a real regression on that path look like expected behaviour.
- **Half the matrix is expected to fail**, so every case states its expected
  outcome and the timeout cases require the specific failure rather than any
  failure. Two pairs differ by a single behaviour flag and produce opposite
  outcomes, which is what shows the harness can report both.

- **Recovery figures**, ten thousand messages on one channel: a clean path
  delivers them in 440 ms of simulated time; five percent loss with five percent
  reordering takes 4530 ms and discards 1323 datagrams; twenty percent loss with
  ten percent reordering and five percent duplication takes 24705 ms and
  discards 8644, and still converges with nothing lost or reordered at the
  application. The order-of-magnitude cost at five percent is the in-order
  channel stalling behind each gap until the retransmission arrives, which is
  the expected shape and is worth remembering when reading a freeze.
- A clean run is compared against a lossy one in the same test, because a
  recovery suite where the conditions silently failed to apply would otherwise
  pass while measuring nothing.

- **A default masquerade rule is not a cone translator.** It reallocates the
  source port per destination, which is address-and-port-dependent mapping, so
  the stock configuration is symmetric. A fixture built on it looks like a
  port-restricted cone, fails to punch, and confirms the exact opposite of what
  it was written to check. Every cone topology pins the external port; only the
  symmetric case is left at the default.
- **The path between fixture endpoints must be longer than a mapping probe can
  travel.** On a short path the probe crosses the whole fabric and reaches the
  far translator before that side has sent anything, which creates an entry in
  the inbound direction; the far side's own outbound then matches that entry as
  a reply, so no inward path is ever established and both sides time out. This
  is the mechanism the reduced TTL exists to avoid, and it is only visible with
  real distance. Carrier-grade needs one hop more than a plain gateway, because
  the probe must die before reaching either of that side's two translators.
- **An endpoint that exits the moment it establishes strands the other side.**
  Answering checks is unconditional and outlives path selection, so the fixture
  keeps running for a settling period after a path is found. Without it one side
  established and the other timed out, on every topology.

- **Peer-reflexive candidates are not optional, and a wide-area run is what
  found that.** Under symmetric translation the address a peer advertised was
  created toward a reflexive server, so its packets to us leave from a different
  mapping and the advertised address is unreachable. Only the address its check
  actually arrived from is. Without this the far side answers our checks while
  never finding a path of its own, and a host that never finds a path never
  sends media. The failure is one-sided and looks like the peer connecting
  successfully.
- Neither the simulator nor the namespace fixtures caught it, because in every
  case there both sides advertised addresses that were genuinely reachable. The
  matrix now carries the symmetric-to-full-cone pairing that exposes it; it
  failed before the change with exactly the wide-area symptom.
- Admission is the check having authenticated, and nothing weaker. An
  unauthenticated source address would let anyone able to reach the socket
  decide where we send.

**Gate closed 2026-08-16.**

```
matrix:   10 topologies simulated, 6 against a real kernel, 0 unexpected
wide area: both sides established between two networks, 32 ms to first path
recovery: 10000 messages at 5 percent loss and reorder, in order, 4530 ms
fuzz:     check parser 35.3M executions, no crash
tests:    206 passed; clippy, fmt, ascii clean
```

The wide-area run is the item that earned its place: both synthetic tiers were
green while a one-sided failure sat in the engine, because in every synthetic
case both sides advertised addresses that were genuinely reachable.

## 1: protocol core (2026-08-16)

Sans-IO, `no_std`, allocation free. Bytes in, bytes out, time as a parameter.

**Added**

- `envelope`: the record layer, both ciphers, nonce derived from the credential.
- `packet`: data packets and group acknowledgements with the full flag validation matrix.
- `message`: the length-prefix framing and fragmentation arithmetic.
- `channel`: the receive ring, length-driven reassembly, and the stall escape.
- `send`: the send ring, retransmission timeout, fast retransmission, staleness scan.
- `congestion`: the host-local rate controller.
- `pmtu`: path probing.
- `control`, `video`: message headers and keyframe classification.
- `session`: the facade the shell drives.
- Fuzz targets for every surface that parses network bytes.

**Notes**

- **The nonce is not a zero prefix plus a counter.** The credential decodes to the key
  followed by a four-byte nonce prefix, which is why a recorded key is 72 hex characters
  rather than 64. Found by reading a working implementation before running the corpus, not by
  the corpus failing.
- **The cipher is a parameter, never inferred from material length.** The legacy path keys
  from a 32-byte fingerprint with a 16-byte key, so a length guess picks the wrong cipher and
  fails every packet on the one path with no corpus to catch it.
- **Reassembly is length-driven and ignores the last-fragment flag.** Keying on the flag works
  against a well-behaved sender and fails exactly when a tail is truncated or reordered.
- **The retransmission timeout is not the congestion level table.** It is per fragment and
  exponential in the retry count; the table classifies staleness, and the scan produces the
  count the controller consumes.
- **The stall escape jumps to the furthest resumable slot, never the nearest.** Jumping to the
  nearest crawls the window one gap at a time. Which slots are resumable is the caller's
  decision, because only the layer that understands the payload can tell a message start from
  the middle of one.
- **The first round-trip sample seeds the estimate outright.** Averaging against zero would
  leave it an order of magnitude low for the first dozen samples, and the retransmission
  timeout is built on it.
- **The core contains no `unsafe`.** Every path uses checked slicing. That was not a goal; it
  is what fell out of writing the parsers against hostile input, and it moves the `miri`
  obligation to `lowlat-common` where the risk actually is.

## 0: workspace and common primitives (2026-08-15)

**Added**

- Cargo workspace, edition 2024, eleven crates with the dependency direction enforced by the
  manifest. Directories are unprefixed; package names carry `lowlat-`. The shared library
  target is named `lowlat`, so it links as `liblowlat`.
- `lowlat-common`:
  - `clock`: monotonic time, **fractional-millisecond** intervals, absolute-deadline sleep
    built from `CLOCK_MONOTONIC` with a 200 us spin finish.
  - `wait`: address-based wait and wake over the raw futex on Linux, with a bucketed portable
    fallback. Wait and notify live in one module because they are one primitive.
  - `spsc`: bounded single-producer single-consumer ring, fixed capacity, no allocation after
    construction, never blocks, never grows.
  - `seq`: RFC 1982 serial comparisons.
  - `bytes`: bounds-checked fixed-width wire accessors, all returning options.
  - `log`: leveled logging with an application sink; trace compiled out in release.
  - `alloc_counter`: thread-local counting allocator behind a test-only feature.
- Deterministic swap of atomics and cells for model checking, so one body of ring code serves
  both the real build and `loom`.
- CI: ASCII check, format, clippy with warnings denied, tests, release build, model checking,
  dependency and license audit, sanitizers.
- `deny.toml`. The GPL denial is load bearing: codec libraries are loaded at runtime and never
  linked, and this is what makes a violation a build failure rather than a discipline.
- Pre-commit hook running the ASCII check on staged files.

**Notes**

- **The model check was shown capable of failing.** Weakening the producer's release store to
  relaxed makes `loom` report a causality violation. A passing check that has never failed is
  not yet evidence.
- **The ASCII checker was found silently passing.** It reported success while examining zero
  files, because a directory argument fell through its file filter. Fixed to expand
  directories, and the incident is cited in [08-testing.md](08-testing.md) 8 as the concrete
  case for the harness rule.
- Hardware encode confirmed working in the development VM by `scripts/probe-capture.sh`
  stage 3: 1080p60, 60 frames. That is what Phase 5 depends on, and it is now measured rather
  than inferred.
- Gate 3's wording was corrected from "strictly monotonic" to non-decreasing-and-advancing.
  The platform guarantees the former, not the latter, and asserting strict increase would test
  the timer's resolution rather than our contract.
