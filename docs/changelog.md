# Changelog

Newest first. One entry per phase; approach changes and gate revisions go in
[impl-plan.md](impl-plan.md) instead.

## 2026-09-26 - W0.4: the client's decoders chosen and opened by the platform

### Changed
- **The decoder the client settles on is chosen by a platform module**
  ([07 §10](07-platforms.md)): the automatic order and the devices it walks -- render nodes,
  the card behind one, the machine's own codec library -- the decoders the decode thread opens
  on them, and the decoder table an application lists. The seam, the decode thread and the
  queue around them are written once. What a client opened says which backend it is through
  an accessor, which the library's status reads rather than naming the platform's variants.
- The device-backed picture slots are left as they are: on Windows they become shared
  textures and a fence, the handle kind the boundary appends, which is the client's phase.

### Measured
- Nothing on Linux behaves differently: every test the client and the library had passes
  unchanged.

## 2026-09-26 - W0.5: the host's frame loop and guest loop make no system call of their own

### Changed
- **The frame loop waits for a present through the display** rather than polling the
  display's descriptor itself; what a wait means -- a present, something else, nothing in the
  time -- is the display's to say, and the loop's pacing is unchanged.
- **What a stream is built on is a platform module** ([07 §10](07-platforms.md)): the encoder
  that shares the capture's device, the one the display's device is served by, the choice
  between them and the full-chroma census. The rebuild, the degrade and the frame loop are
  written once and call it.
- The display's shared types -- the outputs it lists, a pre-flight's answer, what a capture
  produced -- sit apart from the Linux display, which the rest of the host names only through
  them and the display itself; where an output sits in the desktop moves to the capture
  crate's root.
- The guest loop names the devices a guest's input lands on through the injection crate's
  root, where the platform decides what they are.

### Measured
- Nothing on Linux behaves differently: every test the host and the library had passes
  unchanged, and the loop's vblank wait keeps its three outcomes exactly.

## 2026-09-26 - W0.6: the whole workspace builds for Windows, and CI holds it there

### Added
- **A Windows job in continuous integration** ([08 §12](08-testing.md)): lints as errors
  and the tests, over the whole workspace. Per-platform code is a module of the same name on
  each platform rather than a trait, so this build is what holds the two sides to one shape.

### Changed
- The crates with no Windows side yet -- capture, encode, inject, the host, the client and
  the library itself -- build empty on Windows rather than failing, and their Linux-only
  dependencies (the display interfaces, the graphics loaders, `libc`) are Linux's alone.
  The service and the shell's fixture endpoint are a small program in front of their Linux
  body, which elsewhere says it is not built for the platform and exits. Nothing on Linux
  changes: the header the library generates is byte for byte what it was.

### Measured
- The whole workspace builds, lints clean and passes its tests for Windows, run here under a
  compatibility layer (libraries, programs and tests; the examples, which probe Linux
  hardware, are left out).

## 2026-09-26 - W0.3: the device interfaces and the decoders build for Windows

### Changed
- **The vendor interfaces' bindings are generated per platform**, Linux and Windows side by
  side and chosen when the crate is built. Windows gives `long` four bytes where Linux gives
  it eight, which moves every structure holding one, so one set's layout assertions cannot
  compile on the other. The Linux set is byte for byte what it was. The generator reads the
  MinGW system headers for the Windows set and defines the 32-bit calling-convention keyword
  away, which on the 64-bit platform changes nothing and without which every entry-point
  type of the compute runtime is dropped without a word.
- The compute runtime's descriptor interop -- an allocation exported for the handle path, a
  capture buffer imported for the encoder -- is a module of its own, which exists on Linux;
  a Windows half takes the same place with that platform's handles.
- The open stack's interface and its decoder exist on Linux alone. The vendor runtimes and
  the codec libraries are opened by their Windows names there, and the vendor decoder's
  creation fields take the platform's own `unsigned long` rather than eight bytes.

### Measured
- Both crates build and pass the lint for Windows. Their tests, and the core's and the
  crypto crate's, pass there, run here under a compatibility layer: both readers read every
  committed fixture and hand out every picture in order, as on Linux.

## 2026-09-26 - W0.2: the IO shell's system calls apart from the loop

### Changed
- **One module per platform under the shell** ([02 §6](02-io-shell.md)): the socket and its
  options, the wait and batched receive, offload and source-pinned sends, the wake and the
  interface walk, chosen when the crate is built and holding every `unsafe` block the crate
  has. The loop, the send batching, the attempt thread, the port walk, the address choice and
  the browser transport are written once above it. The crate no longer compiles to nothing
  off Linux: the browser transport and the address filters build for Windows today.
- The receive storage belongs to the platform with the socket and the wake, because on a
  completion port the wait is the receive and pre-posted storage lives exactly as long as its
  socket. The public surface is unchanged except for three items nothing used: the readiness
  wait on a bare socket, the socket's raw descriptor and the public receive batch.

### Measured
- Every test the shell had still runs, now in the module that owns what it checks; the
  receive tests go through the loop's own receive path, which makes them the contract the
  next platform's module meets. Nothing on Linux behaves differently.

## 2026-09-26 - W0.1: the shared primitives on Windows

### Changed
- **The address wait uses the system's own primitive on Windows** ([02 §3](02-io-shell.md)).
  It was a table of mutex-and-condvar buckets, which the picture queue, sound and the unit
  pool all wait on. Other platforms keep the table.
- **Each library handle raises the timer resolution for its life** ([02 §2](02-io-shell.md)),
  host and client alike, released after the handle's threads are joined. Nothing requested
  it before, and every wait's timeout would have landed on the system tick.
- `arrived_us` reads the performance counter on Windows rather than zero; the ABI text says
  which clock ([06 §3b](06-api.md)). No layout changes.
- Libraries load from the application's and the system's directories, and a library named
  by full path resolves its own imports beside it; never from the current directory or the
  search path.

### Measured
- The common tests pass on Windows, run here under a compatibility layer, loader and clock
  included. With the wake removed, the wake test's waiter runs to its 10 s timeout.

### Added
- [impl-plan-windows.md](impl-plan-windows.md): the platform seams (W0), then the client and
  the host.

## 2026-09-25 - Records sealed by a vetted library, lent to the core

### Changed
- **A live session's records are sealed and opened by `ring`** ([01 §4.1](01-protocol.md),
  [00 D4](00-overview.md)). Host and client both key a session's cipher on the thread that
  runs it (`lowlat_crypto::Record`) and lend it to the core's envelope (`Envelope::lent`).
  The core keeps its portable cipher as the reference, and for its own tests, simulator and
  fuzzing: the library always brings a random source, which the core must be unable to
  reach. Through the envelope, AES-256, on the development machine: a 1200-byte datagram
  seals in 147 ns and opens in 133, against 692 and 689; a 64-byte one in 59 and 58, against
  84 and 68; the 99th percentile within 3 ns of the median throughout. A four-guest session
  at 40 Mbps spends a quarter of one percent of a core on it rather than 1.2, and the
  client's receive about a fifth as much as before.
- `ring` 0.17 was already in the graph as the signaling TLS provider; it now links into both
  halves of the library too. Nothing new is duplicated and the licences are unchanged.

### Measured
- The lent cipher writes the records the portable one writes, byte for byte, at every length
  from empty through 300 bytes, at each side of the group boundaries past it and at a full
  datagram, under both ciphers, and each opens the other's; a record touched in its counter,
  its tag or either end of its body is refused. Each check was shown failing with its part
  broken: a seal under the wrong nonce, an open that accepts anything. Live, the client
  streamed from both established hosts, sound and video, with every record through it.
- **Live on this host, rebuilt with it, both ciphers.** Under AES-256: our client, then the
  established Android and macOS clients beside it, three guests at once for two and a half
  minutes. Under the legacy 128-bit cipher: the established Android client of an older
  generation, which offers no media key, for three and a half minutes, and our client asking
  for it. Every session carried video and sound; none had a datagram refused, a resend
  asked or a sound packet dropped, and the established clients each left cleanly.

### Added
- The demo's `LOWLAT_LEGACY_CIPHER` offers no media key, so the session takes the legacy
  128-bit cipher, keyed from the host's certificate digest, as an older client's would. The
  library has taken the request since its first client release; the demo never asked.

## 2026-09-25 - Demo: the toolkit's websocket reader hands out whole messages

### Fixed
- **A signaling message sent in fragments reaches the demo whole.** The vendored toolkit's
  reader took one frame for the message: a text frame without its final bit came out as
  though whole, cut short, and the continuation frames after it were dropped. That fits the
  answer the demo lost twice to a parse error, at byte 259 and 275 of a 742-byte message; it
  was never caught happening live. The reader gathers the fragments now, answering pings,
  noting pongs and taking a close that arrive between them; the change is recorded in the
  toolkit's provenance file. When a message still does not parse, the demo prints its
  length, which tells a message cut short from a malformed one.

### Added
- `make -C examples/client check`: the reader against a loopback server that sends a
  message whole, an answer's length in three fragments with a ping and a pong between them,
  one whose last fragment comes 200 ms late, a binary one, an empty one and a close. Before
  the fix it handed out the answer's first 259 bytes; CI runs it after building the demo.
  Live after the fix, 27 attempts across both established hosts all came up.

## 2026-09-25 - C5: the handle path's copy is waited on asleep

### Changed
- **The vendor backend's copy into a device slot is waited on asleep** ([10 §4](10-client.md)).
  It was waited for on its stream, and a wait in the vendor's context spins by default: the
  decode thread held a core for the whole copy, and on a machine whose cores were all busy
  it was preempted mid-spin and saw the end a scheduling slice late. An event made to block
  is recorded behind the copies and waited on instead. Back to back at 2160p, paced at 120
  pictures a second: 110 us of CPU a picture down to 32, for 20-30 us more wait; with the
  decode thread and two busy threads on two cores, the 99th percentile was 5.3-6.2 ms in
  three runs of four before and 1.6-1.8 ms in every run after.

### Measured
- **Where the vendor backend's waits spin, and why the context keeps its flags.** The
  interface's own wait for a decode sleeps (30-40 us of CPU across a 0.44-1.65 ms map); only
  our copy waits spin. Telling the whole context to sleep halves the planes route's CPU but
  slows its read-back, which the driver stages through its own buffer in chunks and waits on
  each: 332 -> 446 us a picture at 1080p and 1003 -> 1424 at 2160p. Reading back into
  page-locked memory with a sleeping wait takes the staging out: 26 us of CPU a picture at
  2160p where the route spends 1023 today, at the same 1.0 ms, and a 99th percentile of
  2.5 ms on two busy cores against 5.9. It is written down in the client plan as later work,
  since it changes how the picture queue's slots are backed.

## 2026-09-25 - C5: a stream silent about reordering stays in time past a frame number wrap

### Fixed
- **A stream that says nothing about its reordering stays in time past its frame number's
  wrap** ([10 §5.1](10-client.md)). Under order counts that follow the frame number, the
  count restarted low at the first wrap: the offset that carries it on was derived after
  the frame number it is compared against had been replaced. A stream that declares its
  reordering never showed it; one that declares nothing took every later picture for a late
  one, and its output stopped for sixteen pictures and then ran sixteen behind for the rest
  of the session, 133 ms at 120 pictures a second. The vendor encoder at its defaults writes
  such a stream; this host's three encoders and the established hosts tried here declare
  their depth. A clip of it, 300 pictures past the wrap at 256, joins the fixtures: it held
  unit 257 back before, and decodes to the reference on both device interfaces and the
  software backend after.

### Changed
- **The demo keeps its window's size rather than asking the display on every pass.** The
  picture's rectangle asked for it every millisecond, a round trip to the display each; it
  is read once the window exists and again on the toolkit's size and move events. Live on a
  headless display against an established host, the event loop woke 742 times a second
  where it had 1435, and a resize still refits the picture.

## 2026-09-25 - C5: a new attempt starts from nothing the last session left

### Fixed
- **Attempts follow one another on one handle** ([06 §3b](06-api.md)). The next attempt read
  as over until its session came up, was handed the last session's untaken events under its
  own name, and carried that session's figures and up to four of its access units. It
  starts on fresh figures now -- connecting, every count from zero -- with the event queue
  and the unit pool emptied.
- **The demo no longer spins while no session is up.** Its sound and presenting threads
  asked the library to wait, were answered at once and asked again: 1.2 s of a core at
  every connect. They wait themselves now; the sound thread spent 24 ms over a
  ninety-second run with three connects.

### Added
- The demo visits several peers in turn on one handle: `LOWLAT_PEER` takes a
  comma-separated list, each held for `LOWLAT_SECONDS` and left for the next.

### Changed
- **The demo's signaling socket lives as long as its attempt's negotiation.** It stayed
  open for the whole session, was read on every loop pass, and a clean session ended by
  withdrawing its offer. It closes once the path is established, the offer withdrawn only
  for an attempt that never came up, and each attempt opens its own; live, three attempts
  across two hosts held theirs for one to three seconds each.

### Measured
- The demo on one handle through an established host, the same host again, then a second
  one: the first and third came up, and between them the status read connecting with every
  count at zero and the thread count fell back to its idle ten. The second attempt's answer
  arrived unparseable and was lost; thirteen more attempts with the signaling frames
  logged all came up. See the client plan's C5 item.

## 2026-09-25 - C5: a second session waits for its pictures; a departure ends with its loop

### Fixed
- **A second session on one handle waits for its pictures** ([10 §4](10-client.md)). A
  departure closes the picture queue so no waiter is stranded, and nothing opened it again:
  after a reconnect every `lowlat_client_acquire_frame` came back at once, 4 us where the first
  session's waited its 100 ms, and a renderer paced by the wait would spin. The next attempt
  reopens the queue from its offer on, and lets go of a picture the last session left
  untaken so the new session's first acquire is never handed it.
- **A departure ends when the session's loop does.** The leave waited while the loop looked
  alive, and alive meant only that no stop had been asked for, so every departure ran to the
  cap on its grace: `lowlat_client_end_connection` took 501 ms on an established session,
  where the loop returns after 263.

## 2026-09-25 - C5: a departure holds nothing else up

### Fixed
- **A call made while the session leaves is answered at once** ([06 §3b](06-api.md),
  [10 §10](10-client.md)). `lowlat_client_end_connection` held the handle's lock through the
  departure's grace and both joins, half a second on an established session, and every other
  call takes that lock, so input, status and acquire made on another thread meanwhile waited
  the whole of it. The attempt is taken out under the lock and left outside it: a status read
  50 ms into a departure answered in 451 ms before and 7 us after. Until the departure is
  over a new attempt is refused with `LOWLAT_ERR_ALREADY_STARTED`, because the leaving threads
  share the unit pool, the picture queue and the sound pool with the next session.

## 2026-09-24 - Packaging: an install finishes with the login alone

### Fixed
- **The login tool takes the answer a successful login is given.** A created session is
  answered 201, and the tool took only 200, so it reported the login refused, left the
  session it had created on the account and never configured the service. Found by the
  first install on a distribution other than this one's; either answer is taken now.
- **The service needs the session and nothing else** ([impl-plan.md](impl-plan.md) Phase
  12). The configuration file ships the signaling server empty and the login fills in the
  session alone, so a new install stayed "not configured" after a successful login. An empty
  server is the public service now, a name without a scheme is a secure socket as before,
  and the service's message names the session as the one thing missing.

### Added
- **`packaging/install.sh`**, for a distribution without the package: the package's files
  at the package's paths and modes, the configuration kept when it exists, the service
  enabled and restarted and the session side's units enabled for every user;
  `--uninstall` removes all but the configuration, and `DESTDIR` stages the files for a
  distribution's own recipe without touching the machine. Staged, the tree matches the
  package's; run here as root, it replaced the packaged service, which came back up with the
  server left empty, advertised itself, and served a guest.

## 2026-09-24 - C5: the full range is the application's to ask for; minor 17

### Fixed
- **The declaration no longer asks for the full range on every application's behalf**
  ([01 §11](01-protocol.md), [06 §3b](06-api.md), [10 §7](10-client.md), minor 17). Bit 3 of
  a declaration, set on every attempt as a base flag that meant nothing, is full range: an
  established host on the vendor's encoder codes the full range when every seat sets it and
  the video range otherwise, measured on the same picture minutes apart, 8 percent of luma
  below 16 with it and 0.2 without. Each picture has said its range since minor 15, but a
  renderer that never reads it drew those pictures darker. `lowlat_client_video_config.
  full_range`, the reserved byte until now, is the application's word that its renderer
  takes it: declared as asked and never masked by the decoder, which decodes either range
  alike; zeroed, the video range.

### Changed
- `FLAG_BASE` is `FLAG_FULL_RANGE`. The host behaves as before: it puts the bit into every
  declaration it records, as its mark that a seat has declared, and codes the video range
  whatever is declared. What it owes is in [impl-plan.md](impl-plan.md) Phase 11.
- The demo asks for the full range, since its renderer takes it; `LOWLAT_FULL_RANGE=0` asks
  for the video range.

### Corrected
- 01 §11's flag table, 10 §4 and 06 §3b: bit 3 is full range, and the range is asked for by
  the client and decided by the host rather than chosen by the host unasked.

## 2026-09-24 - C5: every picture says when it arrived; minor 16

### Added
- **`lowlat_frame.arrived_us`** ([06 §3b](06-api.md), [10 §4](10-client.md), minor 16): when
  the message a picture was decoded from was taken off the network, in microseconds of
  `CLOCK_MONOTONIC`, or zero where that is not known. The session thread stamps each message
  with the pass that completed it and the stamp travels with the unit and the picture, so an
  application can time a picture from the network to its own present. Filled as far as the
  caller's `size` reaches, like the rest of the frame.
- The demo's second line gains a new picture's time from arrival to its acquire and to its
  present returning; `LOWLAT_LATENCY_TRACE` prints both per picture, with the motion sent so
  far.

### Measured
- Beside an established client on the same host, windows dragged through both and presents
  not waiting for the refresh: from arrival to the present returning, 1.32 ms at the median
  and 2.31 at the 99th percentile by planes, 0.67 and 1.37 by handle; the library's share
  outside the decode and the copy is about 40 us, and pictures with this client's own pointer
  motion in flight read the same as the rest. The few pixels a dragged window still trailed
  by in one capture in twenty are the planes route's read-back and upload; by handle this
  client's picture was seen ahead about as often as behind.

## 2026-09-24 - C5 closed: relative motion as the device reported it, a picture drawn as it arrives

### Fixed
- **A relative delta goes to the host as the device reported it** ([10 §8](10-client.md),
  [06 §3b](06-api.md)). It was scaled by the picture's size against the rectangle it was
  drawn into, so a 1920x1080 host stretched into a 2560x1440 window moved its pointer at
  three quarters of the hand, and a window dragged on a host that captures the pointer for
  the drag lagged behind it. A mouse's counts are not window pixels; motion made up from a
  device that reports positions is the application's to scale.
- **The demo draws a picture the moment it arrives** ([10 §4](10-client.md)). Its loop showed
  the picture on screen again every refresh and looked for a new one only after that present
  returned, which waits for the refresh, so a picture arriving mid-refresh reached the screen
  a refresh late: beside an established client on the same host a dragged window trailed by
  15 to 30 pixels. It now waits in acquire and presents what arrives, the picture on screen
  again only after a wait that brought nothing.

### Added
- The demo's `LOWLAT_VSYNC=0`, presenting without waiting for the refresh, and
  `LOWLAT_GFX=vk`, planes drawn through Vulkan; the second's line carries the wait from a
  picture in hand to its present and how many refreshes each picture stayed up.

### Gate
- Gate C in full passed ([impl-plan-client.md](impl-plan-client.md) C5): the desk items
  confirmed by the person at the desk against an established host, after the two fixes
  above; ten unattended minutes from a cold connect on each backend, the preferences walked
  every hundred seconds through every format that host sends -- full chroma at both depths on
  the vendor's decoder -- with no fault, nothing lost or late, and a clean leave. That host
  was silent, so those runs carried no sound. C5 is closed.
- *Later the same day*, with that host playing a looping video and its sound: ten minutes at
  each depth of full chroma handed out as a device handle, the one part of the decode half's
  gate not yet run. Each the full-chroma format throughout from one decoder build, decode
  about 0.9 ms and the device copy 0.10 ms at eight bits and 0.15 at ten, the reader at most
  one message behind, no skip, a clean leave; fifty sound packets a second decoded, none
  dropped and no resync.

## 2026-09-24 - C5: the picture's range, from the stream

### Fixed
- **A full-range picture is said to be one** ([10 §4](10-client.md), [06 §3b](06-api.md),
  minor 15). An established host sent the full range at every codec and depth, declared in its
  parameter set and true of its samples (luma 0 to 255, a fifth of them outside 16 to 235),
  and every picture was handed out as if in the video range: drawn that way it was darker, its
  blacks crushed and its contrast raised. The readers of both codecs keep the parameter set's
  range flag, where they walked past it; the software backend asks the codec library, which
  says it as a layout's full-range twin on older majors and as the decoder's range on newer
  ones, the number for it resolved by name like the pixel formats; and `lowlat_frame` gains
  `full_range`. The samples are handed out as coded, never converted.
- **`lowlat_client_acquire_frame` fills the frame as far as the caller's `size` reaches**, with
  minor 14's size the least accepted ([06 §11](06-api.md)); it refused a frame unless `size`
  covered the whole of it, which the frame's first growth would have turned into a refusal of
  every caller built against an earlier header.

### Added
- The demo hands the range to the toolkit's conversion and shows `full` in its title; the
  library logs the range when it is learned and when it changes.
- Three full-range clips, eight-bit H.264 and HEVC and ten-bit HEVC, from two independent
  encoders; the reference keeps their samples as coded.
- The readers' test checks every clip's range, and every backend's fixture run checks each
  picture's; each check was shown failing with its mechanism taken out.

### Gate
- Every committed clip, the three new ones included, decodes bit-exact with the right range on
  the open stack's two parts, the vendor's, and software through three majors of the codec
  library. Live against the established host that sends the full range: the pictures carry it
  on the open stack and the vendor's decoder, a saved picture spans luma 0 to 255, and the
  person at the desk saw the picture right through H.264, HEVC and HEVC 4:4:4 at eight and
  ten bits, every one of which that host sends when asked.

## 2026-09-23 - C10 closed: the relay, on the client

### Added
- **A relay attempt** ([03 §7](03-connectivity.md), [10 §11](10-client.md), minor 14): a relay
  and its credential in the client's configuration make the attempt relay-only. It allocates,
  permits the relay's own machine, and only then offers the relayed address and the readiness
  marker; every check, answer and record leaves through the relay, media on a channel once one
  is bound to where the host's traffic comes from. Permissions per address renewed at 240 s,
  the allocation at half its lifetime, a stale nonce adopted, nothing relayed toward loopback,
  and three typed ends: unreachable, refused, lost. A clean leave releases the allocation.
  The host takes no part.
- The relay's codec in the core, fuzzed, and the standard's long-term sample reproduced byte
  for byte; `md-5` for the key the protocol fixes, `zeroize` to clear it.
- A relay server in the simulator written from the standard, with a deployed relay's
  behaviours, and a real one in the namespace fixtures beside the host behind one forwarded
  port. The simulator's network gains a forwarded port and delivery on a shared network.
- The demo takes `LOWLAT_RELAY`, `LOWLAT_RELAY_USER` and `LOWLAT_RELAY_PASS`.

### Changed
- **A path is both directions** ([03 §5](03-connectivity.md)): a candidate that answered our
  check, and a check of the peer's that we answered, whose answer leaves before any record
  even while pacing holds it. On our answer alone the session's first records reached a peer
  still reading checks, which it dropped as malformed: every connect of this client, direct
  or relayed, showed a burst of them on an established host's log, and gate 3 failed on it.
  The namespace fixture that withholds a candidate to force the answer's source can no
  longer make a path by construction, and is judged on the probing side's check being
  answered.
- `lowlat_client_config` and `lowlat_client_status` are read and filled as far as the caller's
  `size` reaches; both were refused below their full size, which would have refused every
  caller built against an earlier header ([06 §11](06-api.md)).

### Gate
- Gates 1 and 2 ([impl-plan-client.md](impl-plan-client.md) C10): 1083 tests, the relayed
  round allocation-free both ways; nine simulated runs including sixteen minutes across three
  permission lifetimes and two nonce rotations, made through a relay that binds channels and
  one that does not, since a binding hides a permission renewed late; a real relay in
  namespaces, its host's path the relayed address and the same topology without it timing out.
- Live, a minute through the deployed relay to an established host on its machine: 27 ms of
  round trip at the median, nothing lost or late.
- Gate 3, twelve minutes through the deployed relay to the established host with motion:
  26 ms at the median against 27 for a direct session on the same pair, 688,343 fragments
  with nothing lost or late and no resend asked for -- and malformed connectivity messages on
  the host's log at connect, which failed the first run. With the path made both directions,
  a relayed session to the same host, restarted with its log cleared, showed none.
- Gate 4, this project's host with a credentialed relay beside it on its machine, twelve and
  a half minutes: the host's log naming the relayed path, nothing lost, and the relay's own
  log showing every renewal granted, its rotated nonce refused twice and the requests granted
  when sent again, and the allocation released at the leave.

## 2026-09-22 - C9 closed: a live full-chroma stream through the open stack

### Gate
- Gate 2 passed ([impl-plan-client.md](impl-plan-client.md) C9), on the discrete Intel part
  against this project's own host in its full-chroma mode, since no host at hand sends full
  chroma at its defaults: ten minutes of HEVC 4:4:4 at eight bits and 2560x1440 with motion
  -- 11,966 pictures at the rate asked for, the full-chroma format throughout, decode 1.83 ms
  at the median, never more than one message behind, no skip, no loss -- and five minutes of
  the ten-bit full-chroma layout the same way. The rate is asked for rather than taken
  because that part's link is one lane wide and the read-back is 22 ms a picture: asked for
  more, the reader falls behind by the difference, which is what [10 §4.1](10-client.md)
  says a decoder slower than its stream does.

## 2026-09-22 - Phase 11.5: the vendor backend's ten-bit read against its source, the live triple read through the boundary

### Gate
- Gate 1's vendor half ([impl-plan.md](impl-plan.md) 11.5): the ten-bit test lengthened past
  its in-flight depth, and 120 pictures of HEVC Main 10 from the vendor encoder decoded by an
  outside decoder -- 120 of 120 -- read 88.2 dB in luma at worst and bit for bit on most,
  chroma no worse than 76 dB, flat across the run. And the Vulkan backend the same way,
  through the path the host drives: nothing reads the encoder's picture back, so every
  source picture is converted a second time into a readable target and that is what is
  kept beside the stream; sixty pictures of a coloured bar over a coloured field decoded as
  Main 10 -- 60 of 60 -- read 70.5 dB on the refresh picture and bit for bit on every
  predicted one. Gate 1 is met on all three backends.
- Gate 5 met: the C# host, reading `lowlat_host_get_status` once a second, printed the
  codec, chroma and depth moving with a guest's requests -- ten-bit on, off, on and off
  again across seven encoder generations -- while the guest's decoded format followed
  between NV12 and P010. Full chroma stayed at 4:2:0 under this machine's census, as the
  degradation rule says.

## 2026-09-22 - C6: the client-only build checked on every push, two tarballs, the demo packaged

### Added
- **Each half of the library linted alone on every push**, and the client demo linked
  against the client-only library there: every other step builds all features or the
  defaults, under which a reference from one half into the other's crates compiles, and
  the demo is the one thing that links the client symbols the way an application does
  ([08 §12](08-testing.md)). The demo's Makefile takes `LOWLAT_LIB`, a directory holding
  the library to link, with a run path of `../lib` relative to the binary.
- **The build workflow produces the library in both forms**, each in a tarball named for
  what it carries, the client one with the demo inside as `bin/client` and the toolkit's
  notice beside the licence. Both builds land on the same file, so each is copied out
  before the next, and a `FEATURES` line in each tarball is written by a program that opens
  the tarball's own copy, checks its version against the header that ships beside it and
  refuses any feature bits but the expected ones. The demo names the library it loaded,
  with its version and halves, as its first line and before it asks for a peer, so a
  packaged demo can be asked what it carries and every run's log says what it ran against.
- The documentation read against the header and the code once more, and the pages that
  still described a host with a client planned brought up to date: the README's status,
  platform rows and build section, [06 §2](06-api.md) and [§13](06-api.md), the document
  maps, [10](10-client.md)'s status and thread rule.

### Fixed
- Five comments in the header's client half that a later minor had made false: an
  unavailable decoder slot's reason is in `driver`, not `name` (minor 13), the backend in
  the status is the one resolved at creation *or chosen since* (minor 11), and the fence
  on release is null because every picture was copied before it was handed out -- on the
  host for planes, on the device for a handle (minor 8) -- not because every picture is
  planes. [06 §3b](06-api.md) and [§6](06-api.md) carried two of the same.
- The client's code had never reached the CI workflow, which ran on the main branch alone.
  Its first run on a machine with no decoder found a unit test creating a client with the
  defaults, which ask for the first decoder that opens; the hermetic session's ring
  storage, leaked on purpose so its sessions can be static, counted by the leak checker
  (the blocks are kept reachable from a registry now, and the checker was shown to report
  them without it); and the three system headers the demo's toolkit compiles its sound and
  image loaders against, which the runners lack.

### Gate
- Both items passed ([impl-plan-client.md](impl-plan-client.md) C6): the build workflow's
  run produced the two libraries and the demo with the expected feature bits read from
  each tarball's own copy, and that run's client tarball, downloaded rather than rebuilt,
  streamed from an established host on macOS over the LAN for ten minutes -- 16,140
  pictures, 3.4 ms to decode and 3.1 to read back at the median, never more than one
  message behind, no loss. C6 closed, and with it the client's phases C0 to C9.

## 2026-09-22 - C9: full chroma on the open stack, the listing's labels (minor 13)

### Added
- **Full chroma through the open stack where the driver decodes into a layout the library
  reads**: the range-extension structures staged, the profile settled by the stream's own
  parameter set, the packed eight-bit and ten-bit layouts a discrete Intel part hands out
  unpacked in the read-back into the same two full-chroma formats the other backends give.
  Reported only where such a layout is offered; verified bit for bit on the four committed
  full-chroma clips on an Arc A380 ([10 §5.1](10-client.md), [09 §7a](09-compatibility.md)).
- **Every decoder row a label**, `VA-API [Intel]`, `VA-API [AMD]`, `NVDEC [NVIDIA]`,
  `libavcodec [LGPL]`, with the driver's own words in a new **`driver`** field appended to
  `lowlat_decoder_info`; the row is filled as far as the caller's size reaches
  ([06 §6](06-api.md), [§11](06-api.md)).
- The software decoder's worker count is a named rule with a test: never more workers than
  the machine has threads, and no thread of the library's raised above the application's
  ([10 §5.1](10-client.md)).

## 2026-09-22 - an Intel Arc A380 on the open stack, measured

### Measured
- Every committed clip bit-exact through the vendor's media driver on an Arc A380, ten-bit
  HEVC included; on a one-lane PCIe link the read-back is the cost -- 8 ms a picture for
  NV12 and 16 for P010 at 2560x1440 against a decode of about a millisecond -- so the reader
  keeps pace with 30 pictures a second and manages 58 of H.264 and 31 of HEVC ten-bit when
  sent more, falling behind by the difference until the receive ring is full, as
  [10 §4.1](10-client.md) says a decoder slower than its stream does; the loss the client
  then reports is the full ring's, not the wire's ([09 §7a](09-compatibility.md)).

## 2026-09-22 - the decoder table fixed, one slot a call; the open stack's messages into the log

### Fixed
- **`lowlat_enum_decoders` re-probed the whole machine on every call**: 200 ms and seven
  lines of the open stack's own chatter on standard error per index, 0.8 s for a loop over
  three rows, where the documentation said a few milliseconds. The index now names a fixed
  slot -- the open stack on each of eight render nodes, the vendor's device by ordinal, the
  codec library -- and a call probes that slot alone: 200 ms for the whole table here, the
  vendor's real decoders the bulk of it, nothing remembered between calls. A slot with
  nothing usable behind it answers with the new **`available`** bit clear (a reserved byte,
  minor 12) and the reason in its name, so a loop runs to the table's end
  ([06 §6](06-api.md), [§11](06-api.md)).
- The open stack's own messages go to this library's log per display rather than to the
  application's standard error, on both halves.

## 2026-09-21 - Phase C8 closed

### Gate
- All four items passed ([impl-plan-client.md](impl-plan-client.md) C8): every committed
  clip bit-exact through the software decoder; ten minutes each of H.264 and HEVC ten-bit
  at 2560x1440 from this host on software (1.8 and 3.0 ms a picture at the median, 31 and
  55 percent of one core, never more than one message behind) and the established host's
  defaults over the internet; the hundred-second walk over five moves against this host
  and against the established host, each move exactly one keyframe on the host's log --
  the established host's own log showing one encoder build per move and nothing else; the
  process map after a run holding the LGPL pair and nothing copyleft.

## 2026-09-21 - C8, a review and one build option: a GPL codec library by opt-in (minor 12)

### Added
- **The `gpl-libavcodec` build feature**, off by default and in no release build: the
  software decoder takes a GPL build of the machine's codec library as well as an LGPL one;
  `nonfree` stays refused. A build that opts in is its maker's combination under the GPL's
  terms and says so through **`lowlat_features`** as `LOWLAT_FEATURE_GPL_LIBAVCODEC`, the
  header being one for every build. CI tests the licence rule as shipped in a step of its
  own. With the feature on, every committed clip decodes bit-exact through the
  distribution's GPL 7.1 pair ([06 §2](06-api.md), [10 §5.1](10-client.md)).

### Fixed
- The loader asks the pair its licence before its versions, as every sentence about it
  says; an empty `LOWLAT_FFMPEG_VERSION` is unset rather than a refusal.

## 2026-09-21 - C8, second half: a decoder chosen mid-session

### Added
- **`lowlat_client_set_decoder(cl, decoder, device)`**: another decoder before a session or
  during one, probed on the caller's thread as creation probes; a kind that does not open
  answers with its stage and nothing changes. During a session it is one act -- the
  declaration re-masked and restated where it changed, the running decoder torn down, the
  new runtime opened on the decode thread, and one keyframe request made by the replacement
  once it exists -- the queue never closed, a held picture valid across it. A session of
  the handle kind refuses it; so does `LOWLAT_DECODER_NONE`
  ([06 §3b](06-api.md), [10 §5.1](10-client.md)).
- The demo moves to the next listed decoder on `Ctrl+Shift+X`, or every
  `LOWLAT_DECODER_EVERY` seconds. Against this host at 2560x1440, the open stack, the
  vendor's and software walked every ten seconds and then every hundred for five moves:
  each move exactly one reinitialisation on the host's log and the picture back within the
  second ([impl-plan-client.md](impl-plan-client.md) C8 gate 3).

## 2026-09-21 - C8, first half: the software decoder (minor 11)

### Added
- **`LOWLAT_DECODER_SOFTWARE`**: the machine's own codec library, loaded at runtime and
  only when it answers that it is an LGPL build; the pair looked for in the environment,
  the directory named at creation, beside the executable, then the linker's way, the
  highest major of 4 through 9 that opens winning; a build that answers otherwise is closed
  unused and refused with **`LOWLAT_ERR_NO_DECODER_LICENCE`**. The leading fields relied on
  are checked against the library that loaded, and every pixel format is resolved by name,
  because a 4.x pair numbers them differently ([10 §5.1](10-client.md)).
- The same four formats out of it as out of the hardware backends, converted in the
  hand-out copy: 74 to 554 us at 2560x1440 for the four. A codec counts as decoded only if
  its decoder opens. Last in the automatic order; listed last, named by version and licence
  and the directory it came from ([06 §3b](06-api.md), [§6](06-api.md)).
- Every committed clip bit-exact through an LGPL 7.1 pair, the second codec's through an
  8.x one; ten minutes from this host at 2560x1440 with motion, 120 pictures a second:
  H.264 decoded in 1.8 ms at the median at 31 percent of one core, HEVC ten-bit in 3.0 ms
  at 55 percent ([impl-plan-client.md](impl-plan-client.md) C8 gate 1, 2).

### Fixed
- The automatic decoder order with a render node named stopped after the open stack
  instead of trying the vendor's interface on the card behind that node, and the handle
  kind with a node named took any vendor device; both now do what the header's own
  sentence says ([10 §5.1](10-client.md)).

### Recorded
- A stream that reorders more than it declares loses a picture at each depth the codec
  library discovers, where the library's own readers hold it; no host compared here sends
  such a stream. The low-delay flag changed nothing on any clip and is not set.

## 2026-09-21 - C8 planned: software decode over the machine's own libavcodec, a decoder chosen mid-session

### Decided
- The deferred software-decode question ([impl-plan-client.md](impl-plan-client.md) C8,
  [10 §5.1](10-client.md)): the client decodes in software through a libavcodec the machine
  already carries, loaded at runtime and only when it answers that it is an LGPL build;
  nothing is shipped or built, and the copyleft rule of gate 4 gains that one exception as a
  mechanism -- the pair is asked before any other entry point is called and a GPL build is
  closed and refused with a status of its own. The pair is looked for in the environment,
  beside the executable, then the linker's way, the highest major of 4 through 9 winning;
  no header is pinned, the leading fields are checked against the library that loaded, and
  every number that has moved between majors is resolved by name. The same four formats
  leave it as leave the hardware backends, converted in the copy. It is last in the
  automatic order. And the application may move a session to another decoder with one
  call, one act and one keyframe; the frame kind stays the creation's. No automatic
  fallback of any kind is added.

## 2026-09-21 - Phase 14 closed

### Gate
- All four legs passed ([impl-plan.md](impl-plan.md) Phase 14): both pads through the
  launcher and a browser on this machine, with rumble and the trigger effects; both pads from
  the established client against this host; the hermetic session and the wake measured; a
  sixteen-button pad beside a report pad from one guest. Against an established host, our
  client's DualSense works whole in that host's DualSense mode and its DualShock 4 loses only
  the motion, which that host never reads ([impl-plan-client.md](impl-plan-client.md) C7).

## 2026-09-21 - demo: an Xbox pad's X and Y read back by letter; the host's own pads kept out

### Fixed
- **On the host's own machine the demo relayed the host's virtual Xbox pads back as new
  pads**, one more per pass up to the host's cap, since they are controllers to the toolkit
  like any other; a browser on that machine then evicted the real pad from its four slots.
  The demo now tells the host's pads by the location the host writes on them and never
  sends them, so a loopback run makes exactly the pads the desk has.
- **An Xbox pad's X and Y were crossed on every host.** The kernel's Xbox driver names the
  upper face buttons by letter and the PlayStation driver by position, and the letter codes
  are the position codes crossed; the toolkit reads every pad by position. The demo now
  looks up the pad's driver in sysfs and crosses the two bits back for a pad the Xbox
  driver holds. Found at Phase 14's gate with a wired third-party Xbox pad, the same on
  this host and on an established one, which placed it in the client.

## 2026-09-21 - demo: the chords on Ctrl+Shift, a keyboard grab, a fullscreen toggle

### Changed
- **The demo's chords are `Ctrl+Shift` and a letter**
  ([examples/client/README.md](../examples/client/README.md)): `I` grabs the keyboard
  through the toolkit, so the
  desktop's own keys -- the Windows key, its task switch -- go to the host while the window
  has the focus, and the bare Windows key is then sent rather than dropped; `W` toggles
  fullscreen; `D` cycles the host's output (was `Ctrl+Alt+O`); `F`, `R` and `C` as before,
  under the new modifiers.

## 2026-09-21 - 14.5: the absolute pointer is no joystick

### Changed
- **The absolute pointer declares the three primary buttons, not five, and the scan-code
  type** ([07 §4](07-platforms.md)): the kernel's joystick handler takes any device with an
  absolute axis unless it is shaped exactly as an absolute mouse, and a pointer it takes
  holds a joystick number ahead of a guest's own pads -- which a browser's vibration cannot
  reach past the fourth ([07 §4.2](07-platforms.md)). The side buttons go to the relative
  pointer whichever pointer moved last, and their releases follow them. Verified on the
  input layer: no joystick node for the pointer, the input library still calls it a pointer.

## 2026-09-21 - 14.4: the rumble road, proven byte by byte

### Added
- **Two hardware tests and a tool** for what a consumer writes to a virtual pad: a
  force-feedback effect played on the virtual pad's event node comes back as the driver's
  own report with the motors set, both products (`a_rumble_on_the_virtual_pad_comes_back_as_
  its_output_report`); a five-minute window that presents both products, keeps them alive
  with idle reports, and prints every write that reaches them (`whatever_is_written_to_the_
  virtual_pads_is_printed`); and `scripts/rumble-via-sdl.py`, which rumbles a pad through
  SDL3's HIDAPI driver, the road a game takes. The demo's pad trace prints the head of every
  report it writes back to the pad.

### Fixed
- **A set-report write keeps its own kind.** The descriptor's set-report event carries the
  report type, and every one was taken as a feature write; a writer without the output
  path would have had its output report written to the pad as a feature. A write dropped
  for its size is now logged rather than silent.

### Gate
- Leg 1 passed on this machine with the installed service, both pads, all but the trigger
  effects ([impl-plan.md](impl-plan.md) Phase 14); what the consumers themselves need is in
  [07 §4.2](07-platforms.md).

## 2026-09-20 - 14.2 and 14.3: the application as the pad sink, and the package

### Added
- **`pad_sink` in `lowlat_host_config`, `lowlat_host_poll_pad_report` and
  `lowlat_host_send_pad_report`** ([06 §3](06-api.md), [05 §7.2](05-host.md), minor 10, the
  host half): with the sink set to the application, a report pad gets no device of the
  library's own and its reports come out of a poll of their own -- the guest, the pad, the
  product, the kind, the report in the USB form -- feature reports ahead of the first input
  report, and **the pad's end after its last report** (`LOWLAT_PAD_REPORT_UNPLUG`, on the
  guest's unplug and on its leaving), which is what the application destroys its device on;
  what that device is written goes back as the output report or the feature write, and the
  guest frames it for its pad's transport. The queue is the microphone's shape: sixty-four
  reports, the oldest input report dropped and counted when nobody drains, never a feature
  report or a pad's end. **Measured through the boundary at a wired DualSense's rate**, from
  the guest thread's push to the parked poll's return: p50 13 us, p95 19 us, p99 26 us, worst
  40 us (release; 17 / 24 / 32 us in a debug build). The pad enumerations moved to the half
  both sides share.
- **The per-report path allocates nothing**, asserted (`crates/host/tests/no_alloc.rs`): the
  message read, the family rule, the report handed to the sink, queued and taken, and the
  drop at the queue's cap.
- **The package**: the service's device policy names the HID node beside the input one
  (measured: without it the node is refused under the policy and no report pad can be made),
  and a seat-access rule for the raw node of the virtual DualShock 4 or DualSense, keyed on
  the identity it shares with the real pad, for a host without the game launcher's own.

### Changed
- The seam's application-message shape carries the third argument, which a pad's report
  message names the pad in; the others leave it at zero.

## 2026-09-20 - 14.1: a pad sent as its reports is that pad, and what it is written goes back

### Added
- **The injector keeps a HID slot per report pad** ([05 §7.2](05-host.md)): a feature
  report takes the slot and is kept, the first input report creates the device from it, the
  product is fixed by that first report and a report naming another is not the pad's; the
  pad is unplugged with its slot, on the message and on the guest's end. The first message
  that can create a pad fixes its family per identifier: a state or a button makes the
  sixteen-button pad, a report naming a product makes the HID one, the sixteen-button
  messages beside a report pad are dropped, a release-all leaves a report pad alone, and an
  established peer's ten-byte touch block creates nothing.
- **What the device is written travels back whole** ([01 §11.2](01-protocol.md)): the guest
  loop polls each HID pad's descriptor every pass -- answering the driver's questions, and
  taking what an application wrote -- and sends an output report or a feature write as
  opcode 33; never the rumble message for such a pad.

### Fixed
- **The DualSense's output report is sixty-three bytes**, as the kernel's driver writes it,
  not the forty-eight a toolkit writes; the client dropped the driver's, and now takes
  either length.

## 2026-09-20 - 14.0: a controller on the HID layer

### Added
- **`lowlat_inject::uhid`** ([07 §4.2](07-platforms.md)): a DualShock 4 or a DualSense
  presented through the kernel's HID device interface with the product's own descriptor,
  identity, name and version, its location naming the guest and the pad, and a
  locally-administered address made from the guest's label and the slot, which the driver
  insists be unique on the host; the driver's calibration and firmware questions answered
  from what the peer sent or from the fixtures, the pairing answer built around the
  address; the descriptor serviced from the guest loop with no thread per pad, the device
  written only once the kernel runs it. Verified against the kernel's driver on this
  machine: both products registered with three input nodes and a raw node each, the
  driver's questions answered within its wait, the probe's own output report back, a
  second device with the same address refused as the driver promises. **Loopback on this
  machine** with the demo's raw pads and a by-hand host: both pads presented, the
  DualSense registered with the real pad's firmware version, the driver's lightbar
  reports back at the physical pads (the wireless one's framed for its transport), fifty
  thousand motion events in twelve seconds from the virtual DualSense.

### Fixed
- The DualShock 4's pairing report over USB is `0x12`, sixteen bytes (an older driver's
  `0x81` was in the fixtures and the capture script).

## 2026-09-20 - C7.2: the demo reads the Sony pads raw

### Added
- **`examples/client/pads.c`**: with `LOWLAT_PAD_RAW` set, a DualShock 4 or a DualSense is
  read from its own raw node -- found by identity once the session is up, the two feature
  reports sent first, every queued report each pass of the millisecond loop, the host's
  writes put back on the node, a motor-only report for the rumble message -- and the
  toolkit's events for the vendor's pads are dropped meanwhile; `only` drops every
  controller the toolkit reports, for a run on the host's own machine, where the pads the
  host makes from these reports are controllers to the toolkit. The second's line counts
  the reports sent and the writes applied. Smoke-run against this host: both pads found
  (one over USB, one over Bluetooth), the feature reports taken, nine hundred reports a
  second sent with none dropped.

### Fixed
- **Pad identifiers below 256** ([01 §11.1](01-protocol.md)): an established host keys the
  state, button, axis and unplug messages on the identifier's low eight bits and the report
  message on the whole of it, so the demo's raw pads, named from 0x5000, reached its
  DualShock mode as sixteen-button pads only, without touch, and their rumble came back
  under an identifier the demo did not know. The demo names them from 200; the header says
  to keep the identifier below 256.

## 2026-09-20 - C7.1: the client sends a pad's own reports

### Added
- **`lowlat_client_send_pad_report`** and **`LOWLAT_EVENT_PAD_REPORT`** ([06 §3b](06-api.md),
  [10 §8](10-client.md), minor 10): a DualShock 4's or a DualSense's input or feature report,
  as the pad delivered it, normalised to the USB form on the application's thread and sent
  raw with the standard state it implies beside it -- a DualShock 4's as its body, the touch
  block an established host reads and the state; what the host's device is written comes
  back as an event in the pad's own framing, a wireless pad's identifier, sequence and
  checksum put back. `lowlat_pad_type`, `lowlat_pad_report`, `LOWLAT_PAD_REPORT_MAX`; status
  counts the reports sent, received and dropped.
- A pad is one family until unplugged: a state for a report pad, or a report for a state
  pad, is refused at the call with `LOWLAT_ERR_INVALID_ARGUMENT`, as is a report this path
  does not carry.

### Changed
- The whole-pad message's state type is the core's; the input ring's message body grew to a
  report's size.

## 2026-09-20 - C7.0: the pad report framing in the core

### Added
- **`lowlat_core::pad`** ([01 §11.1](01-protocol.md), §11.2): the two products and their
  identifiers; the input, output and feature report shapes in the USB form; the standard
  state a report implies (the toolkit's own numbers: sticks exact over the signed range,
  the vertical ones away-from-the-player positive, the hat as direction bits); the
  wireless framings normalised on the way in and put back on the way out, under the
  checksum the pads use; the touch block an established host's DualShock mode reads; the
  inbound and outbound message framing, with the DualShock 4's body travelling without its
  identifier byte. Opcodes 31 and 33 named in the control vocabulary.
- **Fixtures from the pads on the desk** (`crates/core/tests/data/pad/`, captured by
  `scripts/capture-pad-fixtures.py`, addresses zeroed): the descriptors, the calibration
  and firmware reports, the pairing report's shape, an input report at rest and one held
  (a finger on the touchpad, Cross down) for each, and **both pads over Bluetooth**: the
  wireless inputs verify and normalise; the DualSense's wireless calibration and firmware
  answers are, checksums stripped, byte for byte its USB ones, which ties the seeds and the
  offsets to the device; the DualShock 4's wireless calibration answer groups its gyro
  ranges where the USB one interleaves them, and is reordered for the driver as it is
  re-identified. The reads are tested against them and the framing round-trips over ten
  thousand random reports.

### Changed
- The injector's whole-pad button bits are the core's, re-exported, so the raw report is
  read into the same set the two established messages use.

## 2026-09-20 - Phase 14 and C7 planned

### Decided
- **The DualShock 4 and DualSense pair rides the pair of opcodes that exist for it**, 31 in
  and 33 out, raw reports in the USB form, the product named in an argument no established
  host reads ([01 §11](01-protocol.md)). Not a new message and not the device passthrough.
- **The peer chooses the family, per pad, by the first message that can create it**; the
  host presents the product on the HID layer through `uhid`, answers the driver from the
  peer's feature reports with defaults for the rest, makes the pad's address itself, and
  sends back what the device is written whole ([05 §7.2](05-host.md), [07 §4.2](07-platforms.md),
  [00 D12](00-overview.md) amended).
- **An application may own the device instead** (`pad_sink`, a poll of its own on the
  microphone's wake; [06 §3](06-api.md)); the daemon keeps the library's device.
- **The client library derives the standard state beside the report**, normalises Bluetooth,
  sends feature reports first, and hands back the host's output reports as an event; the
  demo reads the raw nodes itself, and reports go to this library's hosts by default
  ([10 §8](10-client.md), [06 §3b](06-api.md), minor 10 planned).
- Order: C5's desk items, then C7 (the client half, gated against an established host's
  DualShock mode), then Phase 14, then C6.

## 2026-09-19 - Phase C5's second half built

### Added
- **The pointer decoded** ([10 §7](10-client.md), [06 §3b](06-api.md), minor 9):
  `LOWLAT_EVENT_CURSOR` with the picture as RGBA from a buffer the handle owns, valid until
  the next poll, the hotspot in its pixels, the suppressed flag and the position in window
  units; the cache the initialization declares, kept by checksum and emptied on the host's
  forget bit; a name not held delivers the rest; a picture already delivered travels as its
  checksum alone. The reader takes 8-bit RGB and RGBA up to 512 square and is fuzzed.
- **Rumble** (`LOWLAT_EVENT_RUMBLE`) and **the guest list** (`LOWLAT_EVENT_GUEST_LIST`, the
  body through the caller's buffer with the recipient's number; `number` in status).
- **The client's metrics** (`lowlat_client_get_metrics`, [10 §9](10-client.md)): per
  channel the fragments arrived, arrived late, duplicates, out-of-window drops, negatives
  sent, bytes, messages and a recent-loss figure over about thirty seconds; the round trip
  and the connected time.
- The demo draws the pointer at the picture's ratio, rumbles the pad named, reads the guest
  list with its toolkit and puts the host's figures for this guest beside its own.

### Changed
- The receive ring counts what it accepted and what arrived behind a later fragment; the
  session counts the negatives it sent per channel ([01 §9](01-protocol.md) unchanged on the
  wire).

### Fixed
- **The demo's sticks were upside down on an established host**: the toolkit already hands
  a stick over with away-from-the-player positive, the wire's convention, and the demo
  negated it again on a trace read backwards at C3. The values pass through now.

## 2026-09-19 - Phase C5's second half planned

### Decided
- The pointer's picture is decoded in the library and delivered from a buffer the handle
  owns, valid until the next poll; the cache the initialization declares is kept by
  checksum; scaling the pointer is the application's, by the ratio of its rectangle to the
  picture ([10 §7](10-client.md), [06 §3b](06-api.md), minor 9 planned).
- The guest list is handed to the application with the recipient's own number, unread by
  the library; the number goes into status ([10 §7](10-client.md)).
- The client's metrics take a shape of their own -- arrivals, late arrivals, duplicates,
  out-of-window drops, negatives sent, bytes, messages, and a recent-loss figure over about
  thirty seconds, per channel -- and the host's structure stays the host's, reaching the
  client's application through the guest list ([10 §9](10-client.md)).
- Rumble is an event with the pad and two eight-bit motors ([10 §7](10-client.md)).

### Corrected
- [01 §11.2](01-protocol.md): a cached pointer name that carries no size means the size and
  hotspot sent with the picture.

## 2026-09-19 - Two faults the gate found, fixed

### Fixed
- **The host lost its encoder after ten to twenty pipeline builds** and ended every guest
  with the encoder's status until restarted: each build loaded and unloaded the vendor
  runtimes and the graphics loader, and the C library's static thread-local area, given back
  only in stack order, ran out ([07 §8](07-platforms.md)). Runtime libraries now stay loaded
  for the process's life and the process keeps one graphics instance. The full-chroma census
  refusal is logged once per census rather than once per frame.
- **The demo's picture froze after a switch to ten bits**: the toolkit's renderer keyed a
  texture's recreation on the upload format, which the eight and sixteen bit layouts share,
  so the sixteen-bit uploads went into an eight-bit texture and changed nothing
  (`third_party/matoya/PROVENANCE.md`). The demo now prints the toolkit's own lines.

## 2026-09-19 - Phase C5, the decode half built

### Added
- **The client reports its decode times** ([10 §7](10-client.md), [01 §11.4a](01-protocol.md)):
  both kinds on a two-second clock, the smoothed figure of decode and hand-over per picture
  and of the sound decode; the round-trip figure in status is live against a host that sends
  nothing else to acknowledge.
- **Full chroma in the HEVC reader** ([10 §5.1](10-client.md)): the range-extensions profile
  at 4:2:0 and 4:4:4, both extension syntaxes; two planar formats, `LOWLAT_FORMAT_YUV444` and
  `LOWLAT_FORMAT_YUV444_16` ([06 §3b](06-api.md)).
- **The preferences** ([06 §3b](06-api.md), minor 8): `lowlat_client_config.video` with the
  size request and the three colour preferences, masked by what the decoder takes;
  `lowlat_client_set_video_config` mid-session; status says what was asked, declared and
  decoded.
- **The vendor decoder** ([10 §5.1](10-client.md), [09 §7a](09-compatibility.md)): driven from
  the library's own readers, probed at creation by building a decoder per combination, every
  clip bit for bit including full chroma; `LOWLAT_DECODER_VENDOR` accepted.
- **The handle path** ([10 §4](10-client.md), [06 §3b](06-api.md)): `LOWLAT_FRAME_HANDLE` on
  the vendor decoder hands out an opaque descriptor with per-plane offsets and an allocation
  number; the read-back becomes a 0.09 ms device copy at 2560x1440.
- **The decoders listed** ([06 §6](06-api.md)): `lowlat_enum_decoders` with one row per
  backend and device, what it decodes, its limits and whether it exports a handle.
- The application toolkit the demo draws with is a vendored tree with a provenance note; its
  GL renderer imports the descriptor and fills its textures on the device.

### Gated
- Ten-minute runs against this host on both backends and by both routes with the
  preferences walked mid-session, and against an established host over the internet: the
  reader never more than one message behind, every switch answered within the second
  ([10 §4.1](10-client.md), [impl-plan-client.md](impl-plan-client.md) C5). Real full
  chroma stays unstreamed: this host's service refuses it by census and twice answered a
  ten-bit reconfigure with an encoder fault that outlived the session, a host-side item.

## 2026-09-19 - Phase C5 planned, the decode half first

### Decided
- The video declaration is a preference masked by capability, grouped as
  `lowlat_client_config.video`, with no fallback to another decoder ([10 §5.1](10-client.md),
  §7; [06 §3b](06-api.md), minor 8 planned).
- The second backend is driven from the library's own readers and probed at creation by
  building a real decoder per combination ([10 §5.1](10-client.md)).
- The handle path is in this phase on Linux: exportable device slots sized at the stream's
  size, one device copy, an opaque descriptor as the first handle kind ([10 §4](10-client.md)).
- The three deferred decisions of 2026-09-16 are decided as none in v1; the reader's lag in
  status is the application's warning ([10 §4.1](10-client.md)).
- Opcode 21 goes out on a two-second clock with both kinds ([10 §7](10-client.md),
  [01 §11.4a](01-protocol.md)).

### Corrected
- [01 §11.5](01-protocol.md): both colour bits are preferences a host degrades on, as
  [05 §6.1](05-host.md) has said since 2026-09-01; the paragraph still said 4:4:4 was refused.

## 2026-09-18 - Phase C4: sound

### Added
- **`lowlat_client_acquire_audio`** ([06 §3b](06-api.md), [10 §6](10-client.md), minor 7):
  one packet a call, signed sixteen-bit stereo at 48 kHz, in the order the host sent them,
  decoded on the caller's thread into the caller's buffer -- the uncompressed form passed
  through, the compressed one decoded through the same contained decoder the host reads a
  microphone with, generalised over channels. The receive loop stamps each packet and parks
  it in a pool of 32; a full pool drops the newest and counts it. There is no playback window
  in the library: the application's device has the clock and its buffer is the window, so the
  library orders and decodes and the device paces. Status gains the packets decoded, dropped,
  refused and queued, the last packet's age between the wire and the call, and the codec.
- **The demo plays sound** through the toolkit's device from a listening thread, at the
  window a desktop client runs (75 ms to 150); a resync is read from the device's own queue
  and logged as it happens; the second's line carries the sound figures; `LOWLAT_AUDIO_TRACE`
  prints every packet and `LOWLAT_RAW_AUDIO` asks the host for uncompressed.

### Changed
- The sound decoder moved from the microphone's module to `lowlat-audio`'s `decode`, taking
  its channel count and capacity; the fuzz target feeds a stereo decoder beside the mono one,
  with the harness's own panic hook silenced -- it aborted before unwinding, so a contained
  panic read as a crash and the target had never run.

### Verified
- The hermetic session carries a real tone in both codecs under the three network scripts:
  every packet handed over, uncompressed sound equal sample for sample, compressed sound at
  each channel's level ([impl-plan-client.md](impl-plan-client.md) C4). Against an
  established host on the second machine, eighty-five minutes with sound playing there: a
  quarter of a million packets, none dropped or refused, the packet's age at hand-over 0 to
  1 ms, the device's queue drifting at 40 ppm and flushing every 35 minutes -- at most one
  resync in any thirty. From this host, thirty minutes with nothing dropped or refused and one
  resync, on a device that was a null sink so the host would not re-capture the demo -- which
  measures the sink's timer, not drift.

## 2026-09-18 - Phase C3: input

### Added
- **`lowlat_client_set_viewport` and the `lowlat_client_send_*` calls**, one per kind ([06 §3b](06-api.md),
  [10 §8](10-client.md), minor 6). The application says where it drew the picture, in the
  units its positions use, and reports what happened in its window; the library maps
  positions into the picture's own pixels on the session thread, where the stream's size is
  known -- the far edge bumped, the clamp, the extents swapped for a quarter turn, relative
  deltas scaled by the picture against the rectangle -- and applies the rules every client
  applies: a press outside the picture dropped and its release sent, a key of code zero
  dropped, an unchanged pad state not repeated, release-all on the application's word. No
  fit is computed and no display scale factor enters. Reports cross a fixed ring from the
  handle to the loop, dropped and counted when it is full, never blocking the caller.
- **The relative-mode event** (`LOWLAT_EVENT_RELATIVE`): the pointer message read for its
  two mode bits, the event raised on the transition alone with the position to warp to on
  the way out, put back through the same rectangle.
- **The demo drives a host**: keyboard, mouse and both attached pads from the toolkit, the
  key table generated from the toolkit's own map crossed with the kernel's usage table,
  presenting on a thread of its own so the event loop runs at the toolkit's cadence, chords
  for the fit, for letting go of a captured pointer, and for cycling the streamed output
  through the application protocol.

### Verified
- Against an established host with two monitors over the wide area: typing, aiming on both
  outputs switched from the demo, aiming stretched and at the picture's own size, a drag out
  of the window, both pads in a game, mouselook in and out; against this host, a minute of
  scripted input with the two ends' counts agreeing by kind and the census naming every
  opcode an established client sends and no other ([impl-plan-client.md](impl-plan-client.md)
  C3). The hermetic session runs this host's own input expansion over what the client sends.

## 2026-09-17 - Phase C2: a picture from a real host

### Added
- **`lowlat-decode`: the library reads the bitstream itself, the device decodes it**
  ([10 §5.1](10-client.md), [impl-plan-client.md](impl-plan-client.md) C2). The device
  interfaces on this platform decode a picture from its parameters and slices, so the
  parameter sets, slice headers, picture order, reference lists and the decoded picture
  buffer are the library's: H.264 in full syntax (fields and MBAFF, B slices, reference-list
  modification, weighted prediction, the marking process with long-term references, gaps
  in the frame count) and HEVC (short- and long-term reference sets, dependent slices,
  tiles and wavefront entry points, leading pictures dropped after a stream-starting random
  access point), eight and ten bit, scaling lists from the sets or the defaults. The VA-API
  backend beneath: one configuration and context, a fixed pool of surfaces the picture
  buffer indexes, every parameter staged in storage allocated once, the picture read back
  through the surface's own mapping. Nothing is allocated per unit; a unit that needs more
  than the fixed arrays hold is refused, never truncated. A stream that declares nothing
  about its reordering is held back only as far as it proves it must.
- **Eighteen committed clips with an independent decoder's checksums** ([08 §10](08-testing.md)):
  three from this host's synthetic source at 720p and fifteen from two other encoders at
  128 and 256 square -- B pyramids, MBAFF, CAVLC, slices, scaling lists, ten bit -- every one
  decoding bit-exact on the device. Both readers are fuzzed; the two crash inputs the first
  minutes found are regression tests.
- **`lowlat-drivers`**: the device interfaces reached at runtime -- the generated bindings,
  the loaders, the display and the device context -- in one crate shared by the encoders
  and the decoder, so a client-only build carries neither pipeline.
- **The frame queue and the decode thread** ([10 §4](10-client.md), [10 §10](10-client.md)):
  a latest-wins ring of four slots, model checked, the producer stealing the oldest ready
  slot and never a held one; the slots sized at the configuration's ceiling, backed on the
  decode thread at the first picture, demand-zero, each picture laid out at its own pitch;
  the decode thread taking units from the receive thread's pool, running the C1 policy
  over the real backend and carrying the one keyframe request to the session thread. The
  decoder is opened at creation, so a machine without one is refused there with the stage
  named; a client with nowhere to draw may ask for none.
- **The pictures at the boundary, minor 5** ([06 §3b](06-api.md)): `lowlat_client_acquire_frame`
  outside the handle's lock, `lowlat_client_release_frame` with a none-only fence, the
  frame, plane and fence types, the decoder and frame-kind choices at creation, the ceiling
  and the device, the decoder's state and its times in status, the decoder statuses and
  the decoder-failed outcome.
- **`examples/client`**: the C demo on the application toolkit -- one file for signaling,
  one for the session and the window; a line of figures a second and the same in the title
  bar; three knobs for measuring (the rate asked of the host, a presentation cap, a timed
  leave).
- The hermetic session decodes: 477 pictures through this host's own framing to the real
  decoder, frame for frame the reference decoder's, with the keyframes announced and with
  them not; a decoder at half the stream's rate records its lag.

### Measured
- On this machine's open-stack decoder, 1080p H.264 at 120 pictures a second: decode 2.0 ms
  a picture at the median and 2.3 at the ninety-fifth percentile, read-back 2.0 and 2.2,
  the queue at one, the reader at most one message behind, 208 MB resident (63 at
  creation; the slots are 12 MB of it, the rest the two drivers' code and buffers). Ten
  minutes against this host with one decoder build and no keyframe request; ten minutes
  against an established host on another machine with a clean departure on both logs and
  no copyleft library in the process map. The cadence at the three rate ratios and the
  half-rate lag are in [10 §4.1](10-client.md) and [10 §5](10-client.md).
- The read-back copy: the driver's own mapping of the surface is most of the 2 ms, the
  copy out of it a quarter of a millisecond; a streaming-load copy makes the copy four
  times faster and the live figure six percent better, and was not taken.
- An established host requires the offer's `mode` ([04 §4](04-signaling.md)).

## 2026-09-17 - Phase C1: the client core, and a hermetic session

### Added
- **`lowlat-client`, the connecting side of the protocol below the media**
  ([10](10-client.md), [impl-plan-client.md](impl-plan-client.md) C1). The seam mirrored: the
  offer's credentials out, the answer's in, one socket, the session keyed from the host's
  block under either cipher, with a setting that asks for the legacy one. The session logic
  is a sans-IO driver the shell thread runs one pass at a time: the start-up sequence (the
  fourteen-key initialization, the diagnostics message, a declaration for each secondary
  stream and none for the first), the control vocabulary of [10 §7](10-client.md) as events,
  access units off the video channel into a pool for the decoder's thread, sound packets
  counted, a clean departure as a zero disconnect, and the reader's lag as a number.
- **The keyframe-aligned catch-up over arrived messages**, on the receive thread: when the
  reader is more than one message behind, keyframe metadata ahead whose picture has arrived
  is skipped to and the pictures before it are discarded and counted. Nothing is skipped over
  a gap and nothing is skipped when no keyframe is ahead; both have named tests. The core's
  receive ring can now count, peek at and skip the complete messages ahead of the reader.
- **The decode policy as a state machine over a decoder interface** ([10 §5](10-client.md)),
  with only a test fake behind it until the first backend: a decoder is built from the
  stream, every parameter-set-led unit rebuilds it unless the announced bit is set, a stale
  generation and a rebuild bit tear it down, a fault destroys it and asks the host for a
  keyframe exactly once, a decoder waiting for a keyframe asks for nothing, a format change
  rebuilds and re-feeds the unit once, and a backend that cannot be built ends the stream.
  Each rule has a named test and each was broken once to see it fail.
- **The client half of the C ABI**, minor 4 ([06 §3b](06-api.md)): `lowlat_client` with
  create, destroy, the four-call seam, status, user data out and the event poll; the seam's
  types shared by both halves; the header compiles alone with either half hidden.
- **A hermetic session under the simulator**: this host's own framing and negotiation
  against the client's driver, at zero loss, one percent loss and five milliseconds of
  reorder, thirty simulated seconds each and three hundred for the lossy two, every access
  unit whole and in order with keyframes where the host said, the census agreeing opcode for
  opcode in both directions. And the real threads against this host's own admission over
  loopback, under both ciphers.
- The connecting side writes the initialization; a body with a real client's values comes
  out the length that client sent.

### Changed
- The frame pool and the event queue moved from the host crate to the common one, where
  both halves reach them; the pool's keyframe flag became a tag word, the queue took the
  seam's event type, and the pool's model check now runs in CI.

### Corrected
- [10 §3](10-client.md): the access-unit buffer is sized from the ring, not at 16 MiB. A
  message has to sit entirely in the receive ring before it can be taken, and the ring
  refuses a fragment further than its depth past the reader, so nothing larger than the
  ring's depth times a fragment's body -- about 4.8 MB -- can ever complete, on this ring or
  on an established client's. The 16 MiB that client allocates is room nothing fills.
- [10 §10](10-client.md): the receive thread owns the ring and hands access units to the
  decode thread; the decode thread does not drain the ring. The catch-up therefore runs on
  the receive side, and it runs whether or not the decoder keeps up, which bounds a stalled
  reader's backlog at one keyframe interval.

## 2026-09-16 - The video protocol, read from both ends and sent to a guest that reads it

### Changed
- **A guest that declared the video protocol is sent a keyframe-metadata message before every
  keyframe, and the keyframe carries the announced bit** ([05 §6.1a](05-host.md),
  [01 §11.3](01-protocol.md)). Per guest, from the initialization's `_VideoProtocolVersion`,
  and never over the browser pipe, which carries no video header; a guest that did not declare
  it is sent the older framing exactly. The rebuilt bit is a latch per guest cleared by the
  first keyframe that reaches the peer, so a refused keyframe does not spend it. Without the
  pair every keyframe is a decoder rebuild on the current client generation, since parameter
  sets are repeated on every one.
- The video header's byte 8 carries the codec, and a guest seated on a running stream is
  described by what the stream codes rather than by the admission defaults: a guest joining
  after another moved the stream to ten bits was told eight, in the one field a decoder is
  built from before any bitstream is parsed.

### Corrected
- [01 §11.3](01-protocol.md), [§11.5](01-protocol.md): `_VideoProtocolVersion` carries the
  literal `1` and a host tests it for nonzero, nothing finer; the established host applies it
  room-wide and rebuilds its encoder when the room's answer flips, while a host writing the
  header per guest may decide per guest. What the declaration switches on, all of it or none:
  a 21-byte metadata message before every keyframe (its layout is now written down), whose
  rebuild bit replaces the parameter-set rule on the client; bit 5 on every keyframe and no
  other picture; and the host's configured keyframe interval, ignored otherwise. Bit 5 alone
  is safe but is nobody's protocol. `resolutions` is the size request per stream, its first
  entry taking precedence over `resolutionX` and `resolutionY`, not a list of what the client
  can display.
- [01 §11.3](01-protocol.md): byte 8 of the video header is the codec, `1` H.264 and `2` HEVC,
  which older hosts wrote as a constant `1`; bit 4 is the lock state of the host's session,
  not full screen.
- [10 §5](10-client.md), [impl-plan-client.md](impl-plan-client.md) C2: the decoder is also
  torn down by a metadata message's rebuild bit, and bit 5 exempts a picture from the
  generation rule.
- [10 §5](10-client.md), [§7](10-client.md), [impl-plan-client.md](impl-plan-client.md) C1:
  a client's keyframe request is one act with its decoder teardown and there is at most one
  per fault, because a decoder that has no picture to fault on asks for nothing; the earlier
  "ask on the first fault, rebuild when faults persist" would have asked per bad unit, which
  on the established host is an encoder rebuild each. At start a client declares stream 0
  through the initialization and sends opcode 13 for the two secondary streams only, waiting
  on no acknowledgement; the first keyframe is the host's to send.
- [10 §4.1](10-client.md), [§5](10-client.md), [§9](10-client.md), the client plan: what
  latest-wins costs (uneven motion under a rate mismatch, never accumulated delay) and what
  it cannot do (thin an undecoded backlog), with four decisions deferred to the numbers C2
  records -- a decode-lag keyframe request, a presentation-rate hint with a sustainability
  event relayed by the application as `encoderFPS`, a pacer in the application, and temporal
  layering on the host as the only per-guest frame-rate lever.

## 2026-09-15 - The client, designed

The client half is planned: [10-client.md](10-client.md) is the design,
[impl-plan-client.md](impl-plan-client.md) the phases C0 to C6, [06 §3b](06-api.md) the
surface, [00 D14](00-overview.md) the decision. Decided at the interview: acquire and release
for pictures with a fence, at most two held; VA-API then NVDEC, no Vulkan Video decode,
software decode deferred with its licence question; the library decodes sound and encodes
input, the application owns the device and the window; signaling in the example, not the
library; one stream over the native transport on Linux. The gate is an established host,
passed again at every phase so that what ships is always a client that connects.

### Corrected
- [01 §11.3](01-protocol.md): video header bits 5 and 6 -- parameter sets that need no
  decoder rebuild, and the keyframe-metadata message a lagging client looks ahead for and
  skips to. Neither is emitted by this host yet; the first is the cheapest improvement it owes
  a newer client, since it and the established host both repeat parameter sets on every
  keyframe and every such keyframe is a decoder rebuild without it.
- [01 §11.5](01-protocol.md): the fourteen-key initialization the current client generation
  sends, in order, with what each key means; a client of ours sends it.

## 2026-09-15 - The C ABI in its own crate, in two halves

The first step of the client SDK, taken before the first pre-release so that the header it
ships is the one that lasts ([06 §2](06-api.md), [§11](06-api.md)).

### Changed
- **`lowlat-sdk` builds the shared object and the header; `lowlat-host` is the host
  orchestration beneath it.** The generator now reads a crate that holds nothing but the
  boundary, which is what lets a feature on a module become a guard in the header without a
  file being named.
- **Two features, `host` and `client`, each a half of the library.** A build without the
  host compiles none of the display stack, so a platform that can only be a client can build
  the library at all. The header declares both halves unless the application defines
  `LOWLAT_NO_HOST` or `LOWLAT_NO_CLIENT`, and then a call into the missing half fails to
  compile rather than to link. The client half is empty for now; the seam exists.
- **The handle is `lowlat_host`**, made by `lowlat_host_create` and freed by
  `lowlat_host_destroy`, so that a client handle can be a second type and a host call on it
  does not compile. The one-handle-two-roles shape of the established SDK is what this
  refuses. Minor 3.

### Added
- `lowlat_features()`: which halves the loaded library carries, as two bits, so a loader
  learns it once rather than at whichever name it failed to resolve first.
- The gate's harness asks it first. A build of one half leaves an object of the same name in
  the same place, and the gate ran against one once.

## 2026-09-12 - The browser transport, begun

Phase 13 opens ([impl-plan.md](impl-plan.md)): a browser as a guest over SCTP on DTLS 1.2, on
the same attempt socket, chosen by one field in the offer. The decisions are in
[00 D3 and D13](00-overview.md); this entry grows as the sub-phases land.

### Added
- **The process certificate** (`lowlat-crypto`): a P-256 key pair in a self-signed
  certificate with no extensions, minted once per process from the crate's own entropy, and
  its SHA-256 digest in the form the credential exchange carries -- the hash name, a space,
  and uppercase pairs joined by colons. A peer trusts it by that digest and nothing else, so
  the certificate is a container for a key and carries nothing a peer might refuse.
- **The username fragment is drawn from six bytes rather than four**, so its encoding is
  eight characters with no padding; `=` is not a character the credential grammar admits,
  and a browser that checks would refuse it.
- **The media seam.** The guest loop reads a fixed set of calls from its session -- feed a
  record, poll, the next deadline, drain, queue a message, take one, liveness, pressure and
  the round-trip figures -- and that set is now a trait in the core, implemented by the
  native session by delegation. The endpoint and the shell take the media half as a type
  parameter with the native session as the default, so everything that exists keeps its
  shape and the browser session is a second instantiation of the same loop rather than a
  second loop. A media half may also report a **fault** -- a handshake that did not complete,
  an association the peer ended with an error -- which the native session never does.
- **Owed answers, in every state.** The connectivity engine keeps sixteen answers pending
  rather than four, because a peer running a full agent checks every pair it holds in one
  burst, and it arms its timer for an owed answer after the path is chosen as well as
  before, because such a peer keeps checking the path it uses and reads an unanswered check
  as the path gone.
- **The browser session** (`lowlat-net`, `web`): a DTLS 1.2 record layer and an SCTP
  association behind the media seam, both sans-IO crates fed bytes and told the time by the
  shell, both timed from the session's own clock -- the record layer's instants are derived
  from the millisecond clock against one epoch, the association's timeline from the moment it
  was opened -- so a browser session runs under the same fake clock as a native one. The host
  is the DTLS client and fires the first flight the moment the path exists; the association
  begins once the peer's certificate matches the digest the credential exchange carried, and
  a certificate that does not, or a handshake that never completes, is a **fault** rather
  than a silence. The mapping is the browser client's and is applied in one place: the
  control header kept, the video and audio headers dropped, one association message per
  protocol message, the binary identifier only, every stream reliable and ordered. What the
  congestion controller reads -- the window, the stale count, the byte and packet counters
  -- is synthesised from the messages queued and in flight and the association's own
  figures, so the controller and the gate steer this pipe unchanged. Records the layer
  produces while input is fed are staged for the next output pass; a message a reader has
  not taken is held, a thousand deep per channel, and the oldest goes beyond that.
- The dependency policy gained one ignored advisory, with its reason: the time crate the
  record layer formats a validity with, whose fix wants a newer compiler than the workspace
  minimum. The minimum is to be raised on its own rather than inside this phase.
- **A server link is made at once, a client link on the path.** The peer's first flight can
  land before this side's own punch has settled, and a link that did not exist yet dropped
  it and cost the peer a whole retransmission interval: over loopback the pipe was secure
  976 ms after the path and is now secure 18 ms after it. A client link still waits for
  the path, because it fires its first flight the moment it exists and its retries would
  run out while the punch was still finding one.
- **What the pipe carries on a lossy path is bounded the way any fair stream is.** The
  association answers every loss and every reordering by halving its window and then
  growing it one packet per round trip, so a 600 KiB picture and eight deltas that cross
  a clean 20 ms link in about half a second take seven seconds at one percent loss, and
  five milliseconds of per-datagram jitter alone -- neighbours reordered constantly, every
  reordering a reported gap -- took six. About 1.2 packets per round trip over the square
  root of the loss rate: two megabits at this path's 60 ms and one percent. The native
  transport sends at the rate the host chooses and repairs the gaps; this pipe cannot, and
  the rate controller follows it down. A lower retransmission floor was tried and moved
  the figure by five percent, so the defaults stand.
- **The pipe is chosen by the offer and reaches the C ABI** (`lowlat-kessel`, `lowlat-host`,
  `lowlatd`). The offer's `mode` and the peer's certificate digest are read; a browser attempt
  without a digest, or with one that is not a SHA-256 digest, is refused at registration with
  its own status, because there would be nothing its handshake could be checked against; on a
  browser attempt the answer carries the process certificate's digest with its hash name and
  no media key. `lowlat_attempt_info` gains `transport` and `fingerprint`, **appended and
  size-gated**: the boundary reads the structure field by field within the caller's `size`
  and never through a reference to the whole of it, so an application built against minor 1
  registers a native attempt as it always did, whatever lies past its allocation. Minor 2,
  with `lowlat_transport`, `LOWLAT_ERR_FINGERPRINT` and `LOWLAT_OUTCOME_HANDSHAKE_FAILED`.
  The guest loop is written once against the media seam and built for whichever pipe the
  offer asked for; the helpers it calls take the seam and not the native session.
- The daemon carries the two state machines' own log lines onto its stream at the level they
  chose, so a handshake that fails is one log and not two.
- **A failed pipe is its own ending.** The guest loop asks its media half for a fault on every
  pass and ends the attempt with `HandshakeFailed` before liveness can call it a peer gone: a
  path existed and the pipe on it did not.
- **The encode-latency report is on the clock**: every two seconds from the moment the path
  exists, on every transport, zero until a picture has been timed, instead of every thirtieth
  frame. A count is a cadence only while frames flow; a still desktop sends one frame a second,
  and a browser page reads five seconds of silence on the control channel as a dead link.
- **A declaration without the base bit counts.** Every native declaration carries it and a
  browser's carries only the codec bit, so a browser declaring the base codec declared zero,
  which the consensus reads as no declaration at all; the bit is put in on both the
  initialisation and the encoder configuration.
- **Two fuzz targets for the pipe.** The record layer is fed arbitrary datagrams in every
  state it passes through, with a real peer advanced between them so the deeper states are
  reachable at all; the association is fed arbitrary bytes carried as real application data
  by the far side's record layer, so its parser sees them exactly as it would from a hostile
  browser. Both run seeded. Thirty and forty seconds clean at about fifty executions a
  second, which is what a handshake's signatures cost under the sanitizer; the minimized
  corpora are committed.
- **Live gate 1, a stock browser client on Chrome, on the first connect** (2026-09-12,
  17:05): the offer took the pipe, the path came up over the LAN, the handshake completed,
  the association came up through an INIT collision (the browser begins it too), the
  browser declared itself, and the stream ran at 120 fps on a still desktop with 5 to 7 ms
  of round trip, no retransmissions, and input landing. Two things it found:
  - **The sequence set said nothing about its reorder depth**, and the browser's hardware
    decoder on one platform held pictures back to the level's worst case: **a hundred
    milliseconds** of decode on a stream with no reordering in it. Both H.264 writers now
    carry the bitstream restriction with `max_num_reorder_frames` zero, traced on the vendor
    stream before and after; **ten milliseconds** on the second connect, against two on the
    same machine's native client. Every macOS decoder of this stream, native or browser, had
    been paying this.
  - The two state machines' packet dumps reached the journal at hundreds of lines a second;
    below a warning they are debug now, and formatted only when asked for.
  Then the soak: sixteen minutes at 50 Mbps with sound and input, 94738 frames and 47638
  sound packets carried, no retransmission on the LAN, round trip 4 to 18 ms, and the
  browser's departure read as a clean leave rather than a silence. The association's
  library warns once per connect that a cookie acknowledgement arrived in a state it did
  not expect, which is the collision -- both ends begin the association -- resolving; benign.
- **Measured, in release, ten seconds of a stream shaped like a real one** -- sixty 30 KiB
  pictures and fifty sound packets a second, 14.7 Mbps -- through a pair of sessions under
  a fake clock: **23 allocations per datagram on the host side and 20 on the guest's**,
  about 350 and 300 per message; **14 us per datagram at p50 on either side, 15 at p95,
  24 at p99**. At that rate the pipe alone is about two percent of a core per browser
  guest, against a native path that allocates nothing. Accepted, and now a number.
- **`examples/web-client`, and live gate 2 on two browser families** (2026-09-13). The
  smallest page that streams from the service: the signaling client in the peer role, a peer
  connection with the three pre-agreed channels and a synthesized answer in the current
  data-channel form, the declaration on channel 0, and the picture decoded by the browser's
  own decoder and drawn as a WebGL2 quad, all on the main thread. Keyframes are read off the
  bitstream's unit types rather than replayed from a cache; a decoder that needs a fresh
  chain asks with opcode 13. The rate and frame rate fields go out as the application's
  video configuration once the first picture has arrived, because a request that arrives
  before the stream exists is dropped as a change to nothing. Both Chrome and Firefox
  connected on the first try -- 340 ms from offer to association, Firefox over the host's
  IPv6 candidate -- and were driven through their developer-tools protocols rather than
  clicked; then both sat eleven minutes side by side at 60 fps, 41293 and 40445 frames decoded with no decoder error and no retransmission, and both departures -- a button on one, the tab closed on the other -- read on the host as a clean leave. The 43 distinct connectivity checks the two browsers sent
  during their connects went through the check parser's corpus, which keeps the seven that
  reach branches nothing else did. Two things the page's own rate
  field made visible and which stay open: the keyframe an encoder rebuild produces is 20 to
  26 KB whatever the rate, so the keyframe-over-262144-bytes item is still unverified against
  a browser; and on loopback with no loss the stale count holds the rate at picture size
  times frame rate ([05 §5](05-host.md)).

### Fixed
- **The Vulkan Video H.264 set now carries its usability information** (2026-09-13). It was
  the one writer with none: no bitstream restriction, so a decoder that cannot know the
  reorder depth holds pictures back to the level's worst case, and no colour description, so
  a decoder is entitled to guess the matrix. It now states a reorder depth of zero, a
  buffering of one, and BT.709 throughout, like the other two; traced on the device's own
  stream before and after, and decoded by an independent decoder as BT.709 with every frame
  read. The seat dump that produced the trace can now reach the third encoder, which it could
  not: it named the open backend, and the third is chosen only over an unnamed one on a real
  output.

### Testing
- One certificate per process, the digest round-tripping with and without its hash name and
  in either case, a refusal for any other hash or length, and the DER parsing back as a
  certificate whose self-signature verifies under the key its PKCS#8 form loads to -- which
  is the check that catches a key encoding the handshake library cannot read before a
  session does.
- Sixteen checks in one burst all answered, which failed with the old capacity; an owed
  answer arming the timer after establishment, and the timer back to infinity once it is out.
- A pair of browser sessions under a fake clock: handshake and association in twenty ticks,
  the control header kept and the media headers dropped in both directions, a foreign
  payload identifier and an out-of-range stream skipped and counted, a wrong digest a
  handshake fault with no association made, a 600 KiB message crossing whole, a message past
  the ceiling refused as oversized, the timer the sooner of the two engines', and a clean
  close read as dead with no fault. The process certificate loads into the record layer and
  produces its first flight, which is the check that catches a key encoding the layer cannot
  read.
- **Against a second implementation**, ignored and run by hand: the handshake completes
  against OpenSSL's DTLS server -- cookie exchange, the server's certificate against the
  digest, the client's certificate on request -- and the association's first packet is read
  on the far side as application data. The server's standard input has to be held open;
  on end of file there it shuts the connection down, which cost a round of suspecting the
  record layer.
- Under the simulator, from a seed: both roles punch, handshake and associate in 200 ms of
  simulated time; a 600 KiB picture and eight deltas cross whole and in order over loss,
  duplication and reordering with the path's own drop count asserted above zero; two
  thousand messages arrive in order on the sound channel under the same path; a wrong
  digest is a handshake fault with no association made; a clean close reads as dead with no
  fault; on a policed path the window passes the controller's floor with a stale share in
  it and a round trip measured; and two ends beginning the association at once still
  associate. Two shells over real sockets: punch, handshake, association and a 200 KiB
  picture, then a clean close read on the far side.
- A browser's offer reads as the browser pipe with its digest, a native one as native, and
  one naming a pipe nothing defines as native. Across the boundary: an attempt described at
  the previous minor's size registers as native with the tail poisoned, one byte short is
  refused, an undefined transport value is refused, a browser attempt without a digest or
  with a malformed one is refused with its own status and description, and a browser's answer
  carries this process's digest with its hash name and an empty media key. The C harness
  does the same three things under C and C++ with warnings as errors, and the C# example
  builds.
- The latency report due at once, then on the interval whether ninety frames passed or none;
  a bare declaration reading as the base codec and a bare codec bit as the base codec plus
  it; and, end to end through the seam, a real socket and a real peer whose certificate is
  not the digest the offer named, the attempt ending with the handshake outcome inside a
  tenth of a second.

## 2026-09-14 - Continuous integration is green again

The workflow had failed on every push since 2026-08-16, at the format step, so nothing
after it had run in a month. Clearing that reached seven more layers, one per push, each
in a test's harness rather than in the product:

### Fixed
- **Format**: three files rustfmt had been asking about.
- **A lint from the newer stable** on a probe example, since the runner tracks stable and
  the machine here was one release behind.
- **The conversion tests on a runner**: the sanitizer job never installed the software
  driver; the GL fallback's tests had no EGL on either job; a device opened for a colour
  check demanded the four import-only interfaces it never calls, and Ubuntu's software
  driver lacks one; and the blocking diagnostic shared the loop's hundred-millisecond
  collect bound, which a software driver compiling its shader on first dispatch exceeds.
  Each fix is in the test helper or the diagnostic; the product's device requirements and
  the loop's bound are unchanged.
- **The sanitizer's own noise named rather than switched off**: the software driver's
  exit-time allocations and a desktop's implicit layer, in a suppression file, so leak
  detection stays on for everything the job exists for.
- **The ABI gate's nested build** finds its profile directory by name (nightly cargo lays
  the build out differently), names the target it was built for so a sanitizer stays off
  the proc macros, and builds the object unsanitized, since a C harness with no sanitizer
  runtime cannot load one.
- **A refresh-count test that starts a real pipeline** is off by default like its
  neighbours, with the run instruction.
- **The net soak** reads the thread count once it has settled and does not read the
  resident-memory slope under a sanitizer, whose quarantine of freed memory is a slope by
  design; descriptors and threads still gate in both jobs.

### Changed
- **The sanitizer job leaves the simulator's rate trajectories to the plain job.** They are
  pure-Rust simulation of the controller and took forty-nine of the job's fifty-two minutes
  sanitized; the simulator's other targets, which drive the shell's real paths, stay.

## 2026-09-14 - Logging in is a person's job

### Added
- **A Debian package**, `cargo deb -p lowlatd`: the service and its unit, the login tool as
  `lowlat-login`, the two user units enabled for every user the way the distribution enables
  its own, the environment file as a conffile at 0640 so the session token survives an
  upgrade, and the login-screen example under the package's documentation. Installed on the
  development machine over the hand-installed files: the conffile prompt kept the existing
  environment, the unit in force moved from `/etc` to the package's, and the service restarted
  onto the packaged binary with the helper and the tray reconnecting.
- **A host that is installed and not yet logged in exits cleanly**, saying what is missing and
  where it goes. It used to fail on the missing session, which under `Restart=on-failure` is a
  restart every two seconds from the first boot until somebody logs in.
- **`scripts/kessel-login.py`**, standard library only: logs in to the signaling service with
  an email, a password and an optional second factor, says who it logged in as and which peer
  the host will be, and with `--install` writes the session into the unit's environment file
  and restarts the service; `--logout` revokes the session that file carries. **Not in the
  daemon, deliberately.** Logging in can need a second factor or a confirmation from the
  account's mail, so it is interactive by nature; and the service keeps a token alive by
  holding its signaling connection, which the daemon already does, so a token obtained once
  lasts until the machine has been off for a week. A refusal is reported in the service's own
  words: a wrong password, an address that has to be confirmed by mail first.
- **The helper and the tray start with the session.** Two user units, `lowlat-session` and
  `lowlat-tray`, part of the graphical session's target and wanted by it, so a login starts
  both and a logout ends both, on every desktop that runs its session under the user manager.
  Enabled for every user. The tray's Quit stays quit until the next login or a `systemctl
  --user start`; the helper is restarted only if it fails, since being replaced by a newer
  helper is its one deliberate exit.

## 2026-09-11 - The tray, drawn by the desktop

### Added
- **`lowlatd tray`, the same binary in a third role.** It links no toolkit: the desktop's own
  panel draws the icon and the menu from a status notifier item this describes over the
  session bus, so the only reason the tray was to be a separate program is gone, and the
  reason the helper is not one -- two sides of a private protocol shipped in one file cannot
  disagree with each other -- applies to it as it stands. The role is selected by the first
  argument and nothing else, like the session's.
- **What it shows and what it asks.** A line saying what the host is doing -- idle, waiting
  for a display, or the output, size, rate, ceiling and codec being streamed and how many are
  watching -- a kick per seated guest, the rate ceiling as a choice of four, and quit. A click
  is a frame to the service; the answer is the state the service pushes back, so a guest gone
  from the list or a rate marked is the acknowledgement.
- **The service tells every tray what it is doing**, on connect and on change and not on a
  repeat: the state is worked out each pass while a tray is attached and sent only when it
  differs from what was last sent, and nothing is worked out for nobody. A tray connecting
  between changes is told the last state at once.
- **A host action names who asked for it.** The connection's credentials are on the line that
  records a kick and on the line that records a change to the stream, which is what keeps
  local authorisation a deferral rather than a gap. A change asked for by a tray goes through
  the same reader a guest's request does, so the two cannot drift.
- **The item survives the panel.** The register is repeated whenever the name that draws
  items changes hands, and a service restart reads as a passive item that comes back active
  when the service does.
- **A guest arriving or leaving is a desktop notification**, put up by the tray through the
  session's own notification service the way any application does. **Connected, not seated,
  on both edges**: a guest has a number from the answer onward, before its media path exists,
  so the toast is for the path coming up and for a guest that had come up going away, and an
  attempt that never got that far is not news. What a tray finds already there when it first
  looks is shown, not announced.
- **A guest is named.** The offer carries the peer's account name and the daemon dropped it;
  it is carried now, so a toast and a kick entry read `Guest#3 someone@example.org`, the seat's
  number first because that is what the log and the roster know the guest by, and the name
  where the service gave one.

### Testing
- The menu is walked back out of the bytes it was written as, and the rate in force is the
  one marked; every separator's number is nothing to click. **The last assertion failed first:
  the separators were numbered into the rate range, so a clicked separator was a request for
  9000 Mbps.** The ranges no longer meet.
- A tray is told the state on connect, on change and not on a repeat; what a tray says is
  queued as an action with its credentials and what a helper says is not, driven through the
  real dispatch. **Both were confirmed to fail** with the branch bent.
- Live, against the running service: the bus's own tool decodes every property and the whole
  layout, a rate click and a kick click both land on the service's log with the tray's pid and
  uid, a separator click lands nowhere, six trays attached and detached with the service and
  the helper untouched, and a service restart under a tray is one reconnect 250 ms later.
- Who is announced is pinned without a desktop: a path coming up is an arrival, a guest that
  had come up and is gone is a departure, and a seated attempt is neither on either edge. An
  ignored test puts one notification on a real desktop and asserts the server answered with
  an id, which is the one check that catches a body marshalled wrong.

## 2026-09-10 - The display is the default, the turn is followed, the mode is asked for

### Fixed
- **A host streamed the generator unless told `--capture`**, and the packaged unit did not say
  it, so an installed service would have streamed a test pattern. The display is now the
  default and **`--synth`** asks for the generator; `--capture` is gone. A host that cannot reach
  the display says so at startup -- before it has advertised a stream it could never produce --
  and names both ways out: the capture privilege, or `--synth`. Nothing lit is waited for, not
  refused, because the session may not have started yet.
- **A display the session had turned streamed on its side.** A turned display is drawn turned
  into a framebuffer that keeps its landscape shape, and nothing below the session says by how
  much; the header declared whatever `--rotate` was told, which was a startup flag about a
  display it never looked at. The session's transform now arrives with the layout, the header
  declares it, and `--rotate` is gone.

### Added
- **A guest's request for a size or a turn is asked of the session**, which owns the display,
  over the compositor's own output-management protocol; the stream takes no part and follows
  whatever the display becomes. One request outstanding at a time, on a deadline at both ends,
  and a helper that does not answer is dropped rather than waited for. A session with no
  mechanism, or no session at all, refuses with a reason. This is the channel's first request,
  and the deadline every request was to carry landed with it.

- **The session's layout thread stopped after its first quiet second.** The watch answered the
  same thing for a read that timed out and for a connection that closed, and the loop took the
  first for the second; its change detection had been proven through a probe that loops on
  time, so the helper carried its first layout and never another. Seen live as a turn the
  session applied and the stream never heard of.
- **A pointer read off a turned display arrived lying down.** The pointer plane is drawn
  turned with the rest of the framebuffer, and a peer sets its own pointer from the picture,
  so the picture is turned back before it is encoded, and the drawn part's place is turned
  into the desktop's orientation, which is the space the hotspot is learned in.

### Changed
- **The mode of a display the session owns is asked for rather than never set.** The earlier
  decision rested on the display device refusing every client but its owner, which is still
  true and is no longer the point: the owner takes requests, and was measured setting a real
  mode in a fifth of a second.

## 2026-09-09 - The desktop's shape is watched, not read once

### Fixed
- **A display added while a stream runs left absolute input mapped against the desktop as it
  was.** Where the captured picture sits was read once, when the display opened, and never again:
  adding a display does not change the captured output's own size, so nothing rebuilt and nothing
  re-read. The absolute axis is spread over the whole desktop, so every position came back scaled
  by the ratio between the old extent and the new one and the far part of the screen could not be
  reached at all. Rearranging or removing an output is the same stale read.

### Added
- **The session reports its layout, and every change to it.** This was planned as a question the
  service asks and it is a signal: the answer has to be right whenever a guest moves its pointer,
  which is continuously, so a host that asks once is silently wrong from the moment somebody
  plugs a display in.
- **The connection is the subscription.** A session re-describes an output when it moves and
  announces one that appears, but only to a client that is still there -- a query that opens,
  reads and closes learns the layout once and can never learn that it changed. The helper holds
  its own session's connection open; a service with no helper keeps the one-shot reading, which
  was always right for the one-output case.
- **Events are not changes.** A session re-sends every field of an output it re-describes, most
  of them unchanged, so what is reported is a layout compared against the last one rather than
  the arrival of anything.
- **An output that goes away takes its rectangle with it.** Left behind it keeps contributing to
  the bounding box the axis is spread over, so a desktop that shrank would go on being mapped at
  its old width.
- **The service repeats what it worked out on every pass**, because the stream publishes its own
  one-shot reading whenever a pipeline is rebuilt, and without this a rebuild for any reason at
  all would quietly put the stale answer back.

### Testing
- The change detection is pinned without a compositor: an output appearing and one going away are
  both changes, and a description repeating what it already said is not. **Both were confirmed to
  fail** -- with every event treated as a change, and with a departed output left in place.
- The layout crosses the channel unchanged, including an output described only in part, and the
  bounding box it reduces to is asserted rather than the fields alone.
- A live test against this session, off by default: the watch's layout agrees with the one-shot
  query for every output, and a desktop nobody touched reports no change -- which is the failure
  that would look most like working.

## 2026-09-09 - The frame rate comes from the display

### Added
- **A rate a guest asks for over the configuration message is clamped to the display as well.**
  It was applied as asked, under a comment saying the display bounds it anyway. The display
  bounds the rate and not the number, and the number is what the encoder's per-frame budget is
  divided by: a guest asking for four times the display's rate got the display's rate with a
  quarter of the budget for each frame. Zero stays no change, as it is for the rate ceiling
  beside it.
- **The output listing is enumerated once per configuration message**, where describing what is
  running and checking what a guest asked for each read the devices separately. Two reads of one
  machine is how the two answers come to disagree about what is lit.
- **`--fps` absent now means the captured output's own refresh rate.** It is the one answer this
  program cannot give before it has looked at a display, so it is carried as zero and settled
  where the display is opened.
- **A rate that was asked for is clamped to the display**: `min(asked, refresh)`. Bounding it in
  the loop was never enough on its own. The frame clock caps the rate and the display's present
  sets the phase, so a stream asking for more than the display can present already ran at the
  display's rate -- but the number it asked for is also what the encoder's per-frame budget is
  divided by, so asking for twice the frames halved what each frame could spend and produced a
  worse picture at the same rate.
- **The refresh figure is computed where the mode does not carry one.** A mode filled in by a
  driver states it; one built by userspace leaves it zero, and then the timings say it: the pixel
  clock over the whole frame including the parts that are not picture.
- **Resolved on every build rather than once**, so an output switched while a stream runs is
  followed, and published where the loop paces from rather than only where the encoder is
  configured.
- **The output listing carries it too**, so a client that asks for the configuration before a
  stream exists is told a rate rather than the zero that means "follow". Same rule as the
  picture's size one field along: before a display has been opened, the display's own answer is
  what the stream is about to produce.

### Testing
- The rule is pinned three ways -- a ceiling above the display is clamped, a ceiling below it is
  honoured, and nothing asked takes the display's rate -- and **two of the three were confirmed
  to fail**, with the clamp removed and with the follow removed.
- **The first test written for the guest's half tested nothing**, and removing the clamp under it
  did not make it fail: the message handler returns early when nothing is streaming, so the
  branch was never reached. The decision moved out into a function of its own, which the test now
  drives directly; both halves of it were then confirmed to fail.
- Read against three real displays on three different cards, and **checked against what the
  desktop itself reports**: 1920x1080@60, 1920x1200@60 and 2560x1440@120, matching in every case.
  A figure that agreed with itself and with nothing else would have looked identical.

## 2026-09-09 - Copied text crosses in both directions

### Added
- **The clipboard, behind `guest_clipboard`.** `send` is a guest's text arriving on this
  desktop, where a person still has to choose to paste it; `both` adds the direction that ships
  whatever the person at this machine copied. An owner is `both` whatever the setting says, and
  **anything unrecognised is off**, so a typo cannot open a clipboard. The setting is said out
  loud at startup, because which way it is set should not have to be inferred from behaviour.
- **The session owns the selection, because nothing else can.** Putting text on a clipboard is
  announcing that you own it and the bytes are asked for later, when somebody pastes, so
  whatever serves it has to still be running then. That is the desktop's own clipboard
  component here; another desktop needs another mechanism behind the same capability, and one
  that has none announces that it has none.
- **A thread of its own for it**, because owning a clipboard is a wait: the desktop says when
  its selection changed and nothing says when it will. One connection does both directions,
  which is also what keeps the echo out -- what this host sets, it remembers, so the change it
  causes is not read back and handed to the guest that caused it.

### Fixed
- **The log printed the body of every application message.** Having the exact bytes beside the
  question is what makes a wrong answer findable, and that reasoning inverts for the identifiers
  carrying what somebody typed or copied: the same line turns the log into a transcript of a
  desktop. Length and identifier for those, exact bytes for the rest, in both directions.
- **A stray terminator no longer reaches a desktop's clipboard.** The wire counts one and the
  layer that reads it takes one off; a peer that sent two would otherwise leave a byte on
  somebody's clipboard that is invisible until it is pasted into something that minds.

### Testing
- The three-valued setting: every unrecognised spelling is off, the milder direction is
  available on its own, and an owner is not a guest.
- A live test against a real desktop, off by default, asserting **both** halves: what this host
  sets is not reported back as a change, and a change made on another connection is. Asserting
  only the first would pass equally well on a connection that hears nothing at all.
- Driven end to end against a running service: the helper announces `idle` and `clipboard`, and
  a copy made on the desktop arrives at the service.
- **The tests that share the helper register are serialised.** What the service pushes goes to
  every helper, which is right for a service and wrong for tests running side by side in one
  process, where one test's message lands in another's socket.

## 2026-09-09 - The screen stays awake while somebody is watching

### Added
- **The idle inhibitor, the helper's first real customer.** A screen blanking during a session
  is the desktop doing exactly what it was told, and nothing below the session can argue with it,
  so the session is asked to stand its screen saver down while a guest is connected and to let go
  when the last one leaves.
- **Pushed as a state, not asked as a question.** It is state the service owns and the session
  acts on; asking would mean waiting on a process in somebody's session for an answer nothing
  needs. Sent on the change rather than on a timer, because a state repeated can arrive out of
  order with the one that replaced it.
- **Just enough of the session bus, written out here.** The whole of what is needed is two calls
  on one interface, and a client library for that bus arrives with an executor this program keeps
  to signaling. **The connection is the lease**: what comes back lives exactly as long as the
  connection that asked for it, so a program that asks and exits has not asked at all -- which is
  why the ordinary command line tools for that bus cannot hold one, and why this belongs in the
  agent whose lifetime is the session's.
- **A helper that finds no screen saver says so**, and the service gives the honest answer for
  that session rather than waiting on one. The lease is also let go when the socket goes: held
  while it is asked for, and a connection that has gone is nobody asking.

### Fixed
- **A header field carrying a signature was written as a string.** A signature counts its length
  in one byte where a string counts it in four, so the call went out three bytes long and the bus
  closed the connection without saying why. Found on the first call that carried a body: the one
  before it has none and worked, which is what narrowed it.

### Testing
- The lease reaches the helper holding the place and reads back as the state it was sent as, and
  three other things on the channel do not read as one.
- A live test against a real session bus, off by default. **What it can assert is bounded and the
  bound is the interface's**: the screen saver answers whether it is active, never who asked it
  not to be, and the desktop's own list of what holds the machine awake is a different interface
  that does not carry these. So it asserts the real service answered a well formed call with a
  cookie of its own -- which is exactly the assertion that caught the signature field above.
- **The register of helpers is one static for the program**, so the tests that read it were
  changed to ask whether a process holds a place rather than to count what is in it. Counting
  passed alone and failed beside a second test that registers.

## 2026-09-09 - A helper announces what its session can do

### Added
- **The first frame carries what the sender can do**, and the service records it per connection.
  The mechanisms behind pointer visibility, the idle inhibitor, the clipboard and the display
  layout differ per desktop and one of them offers no protocol at all, so a helper says what it
  found and the service answers the honest way for the rest. This is what makes "absent is not
  degraded" a per-customer answer rather than an all-or-nothing one. **A name this build does not
  know is passed over rather than refused**, unlike a version, which says the framing itself may
  differ.
- **One helper to a session, newest wins**, keyed by the user -- the nearest thing to a session
  the credentials carry. Two sessions belonging to one person is the case that gets wrong, and
  only one of them is in front of the screen.
- **A session agent reconnects rather than exits.** It outlives the service by design: a system
  service restarts and a session does not, so losing the socket is a wait rather than an ending.

### Fixed
- **A replaced helper displaced its own replacement, and the two traded the place forever.**
  Found in a live run rather than in a test, because both halves are individually correct: newest
  wins, and a helper that loses the socket comes back. From the session side the two closes look
  identical, so the service now says which it is before closing a connection it ends on purpose,
  and a helper told it was replaced stays gone.

### Testing
- Capability names round trip, an unknown one is dropped, and a second helper for one session
  replaces the first -- takes its place, tells it why, and closes it. **Both halves of that were
  confirmed to fail**, with the displacement removed and with the close removed. The close check
  is on a deadline and asserts the error kind, so a replacement that closed nothing fails rather
  than hangs.
- The two-helper case was then driven end to end against a running service, which is where the
  ping-pong was found.

## 2026-09-09 - The session side has a socket to connect to

### Added
- **The role is the first argument and nothing else.** The two roles run at different privilege,
  so a file that can be talked into the wrong one is a security defect rather than a bug. A flag
  is matched wherever it appears in a command line and an argument in first position is not, and
  the role is decided before any flag is read, because a role that depends on a line having been
  scanned is a role a line can be written to change. The boundary landed ahead of the agent it
  protects, so nothing is ever added to the wrong side of it.
- **The service listens and the session connects outward**, on a known path. That removes the
  problem rather than solving it: nothing discovers a session, drops privilege or guesses which
  desktop is running, and a connection arrives with an identity because a local socket carries
  the peer's credentials.
- **Length-prefixed frames with a JSON first frame** carrying a version and a role. A version
  this build does not speak ends the connection rather than being worked around: both sides ship
  in one file, so the only way to see a mismatch is a stale process, and continuing with one is
  how a stale process becomes a wrong answer.
- **A thread rather than the runtime**, which stays signaling's. A socket carrying a handful of
  messages a second does not need an executor to read it.
- **The credentials are recorded and nothing gates on them.** A host action has to be able to
  say who asked for it; the criterion that would read them is deferred, and the socket's mode
  says so rather than hiding the deferral in a file permission.
- **Only the first frame is on a clock.** A helper that has announced itself is long lived and
  silent by design -- it speaks when its session changes -- so a deadline past the greeting would
  drop exactly the quiet ones.

### Testing
- A frame survives a round trip, a declared length over the cap is refused before it is believed,
  and four shapes of unusable greeting are each refused.
- **A real bound socket, accepted, rather than a socket pair**: a pair is one process at both
  ends however the credentials are read, so only a bound path exercises the bind, the mode it is
  left with, and the accept. **Both new checks were confirmed to fail** with the mode narrowed
  and with the version comparison removed.

## 2026-09-08 - A guest can ask for the attention chord

### Added
- **`Ctrl+Alt+Del`, typed on the asking guest's own keyboard.** The combination is taken by the
  operating system a client runs on before any application sees it, so a remote user physically
  cannot send it as keystrokes and asks the host for it instead. The request is an application
  message with an empty body and it is answered where the rest of this host's application
  protocol lives, not at the boundary: input comes from guests, and a general "inject these keys"
  call would be a different decision.
- **The chord is typed rather than passed through**, both modifiers down, the third key down,
  then all three up in reverse. Whatever the guest was already holding of the three ends up
  released, which is what the chord leaves behind on a real keyboard too.
- **A text console is refused.** In front of a graphical session the combination reaches the
  compositor and produces its leave dialog, which is what a client means by asking. In front of
  a text console the terminal translates it and the machine restarts. The foreground terminal's
  keyboard mode separates them in one request; no terminal at all is not the dangerous case and
  is allowed.

### Testing
- Two named tests: the chord releases every key it pressed, including one the guest was already
  holding, and a guest without the keyboard permission cannot ask for it. **The first was
  confirmed to fail** with the release half removed.
- **The reported stuck key did not reproduce and the expansion was re-read instead.** Two tests
  pin the shape it was reported in -- a letter typed under a held modifier leaves nothing held,
  and a modifier absent from a peer's modifier mask releases nothing, since that mask is the
  peer's platform reporting itself at one keystroke rather than a state to act on.

## 2026-09-08 - Every key a guest sends can be read off the log

### Added
- **A line per keyboard message, naming what the expansion did with it.** A key that will not
  release on the far side has two causes with the same symptom and different fixes: a release the
  peer never sent, and a release this host dropped because it had no matching press recorded. The
  message stream on its own tells those apart only if it can be read, and the per-opcode census
  says the first of each kind and nothing after. The line carries the peer's code, its modifier
  mask, the direction, the kernel key it mapped to, and which of `sent`, `not-held` and `no-key`
  happened.
- **`--verbose` on the daemon**, which is what turns it on. Nothing else in the program says
  anything at this level, so in practice the switch means "log every key as it is expanded". The
  line is not on the trace level, because that one is compiled out of the build a live run uses.
- **The lock tap says so too.** The modifier mask is the peer's platform reporting itself at that
  keystroke rather than a state this host can rely on, and a mask whose lock bit comes and goes
  makes this host tap the lock key in the middle of somebody's typing. That is invisible in a
  stream of injected events and obvious in one line.

## 2026-09-08 - A settled picture is re-coded, so still text sharpens

### Fixed
- **Duplicate suppression had no exit, and still text kept whatever quality the motion that drew
  it could afford.** The frame carrying a scroll or a window switch is coded against that
  instant's budget; once the picture stops changing nothing re-codes it, so the softness stays
  for as long as the desk is left alone. The heartbeat does not help: it submits the same picture
  once a second, which codes as almost all skip and refines nothing.
- **A settled picture is now submitted for a bounded run.** A picture that changed re-arms the
  run; one that did not spends a frame of it. Re-coding a still scene is what refines it -- the
  encoder runs a variable rate against an average far above what a still scene costs, so each
  unchanged frame is cheap, the budget goes unspent, and the quantiser walks down. This is the
  bounded form of what asking for every picture already does, and a desk nobody is touching stops
  costing frames once the run is out.

### Testing
- The rule gains two assertions -- a settling picture is sent, and an unchanged one is suppressed
  again once the run is spent -- and **the first was confirmed to fail** with the exit removed.

## 2026-09-08 - The video buffer is three frames, not one

### Fixed
- **A frame could not exceed `bitrate / fps`, so a scene change was quantised until it fit.**
  The buffer was exactly one frame's budget, which forbids any frame from spending more than
  its share however little the frames around it cost. A scroll or a window switch is precisely
  the frame that needs more, and a still picture then keeps whatever quality the motion that
  drew it could afford, because nothing re-codes it afterwards. Measured on a live 2K stream
  where the transport was idle throughout: the send window peaked at 18 of a hundred, nothing
  went stale, and no congestion event was declared, so none of it was the rate controller.
- **The buffer is now `min(768 kbit, one frame x 3.1)`.** The argument the single frame was
  chosen on -- that a larger buffer smooths bitrate across frames, which is queueing, and those
  bits arrive late rather than not at all -- is sound, and the number was still wrong. Bounding
  the buffer is what keeps the smoothing from becoming an unbounded delay; refusing to smooth at
  all is a different and stricter thing than it was reasoned to be.

### Testing
- The arithmetic is pinned at three rates and two frame rates, including the point where the
  ceiling starts to bind, and **was confirmed to fail** with the multiple returned to one frame.

## 2026-09-07 - An adaptive congestion setting, and a configuration that is not zero

### Added
- **A fourth congestion setting, `adaptive`, with nothing behind it yet.** The three levels are
  the whole of the detector and each is a tuning of it; adaptive runs level 1's tuning and
  reserves a place for host-local signals that see what the window floor hides. It is a seam,
  not a feature: it exists so a signal which earns its measurement becomes a setting rather than
  a rebuild, and it draws the line that matters -- **an addition beyond the three levels sits
  behind it, a correction found in them is fixed in them**, because a correction that has to be
  asked for is a defect left on by default. Recorded as deferred in the plan so that an empty
  setting is a decision rather than an oversight.
- **`lowlat_host_config_default`, and a null configuration means it.** Every enumerated field is
  validated rather than clamped, so a structure the caller zeroed is a *valid* request for the
  first variant of each -- including the most aggressive congestion level -- and the boundary
  cannot tell that apart from an application that meant it. There was no way to ask for the
  defaults, and the obvious way to build a configuration asked for something else.

### Changed
- **`LOWLAT_CG_LEVEL_LEGACY` is now `LOWLAT_CG_LEVEL_AGGRESSIVE`.** The value never selected an
  older scheme; its thresholds are all zero, so every outstanding fragment classifies stale and
  congestion is declared on every pass once the window passes its floor. The value did not move
  and the behaviour did not change; the name stopped misdescribing it. This surface is ours and
  carries no inherited compatibility, which is what makes correcting a name cheaper than keeping
  a wrong one ([06 §11](06-api.md)).
- Every settable configuration field now states its default beside it.

### Testing
- Three checks added and **each broken on purpose first**: defaults that pick the aggressive
  level, a null configuration refused instead of taken as the defaults, and the pointer hold
  drifting from the one figure the arbitration was tuned to.

## 2026-09-07 - The retransmission timeout is linear, and the spec called it exponential

### Fixed
- **The retransmission timeout was described as exponential in the retry count. It is linear.**
  The formula in §9 was always stated correctly and always implemented correctly, so no
  behaviour changes; only the description of it was wrong. Each retry adds one `2 * srtt`, so
  the series runs 2, 4, 6, 8 times the round trip rather than doubling. §9 now says so, names
  the series, and points at the outstanding fragment cap as what actually bounds
  retransmission -- on a fast path the 50 ms floor swallows the multiply until
  `(n + 1) * srtt` passes 25 ms, so the timeout barely backs off at all.
- **The word was load-bearing, which is why it is worth an entry.** "Exponential" implies a
  backoff that self-limits. This one does not, and any future change that filters round-trip
  samples would be relying on exactly that property to carry the timer while samples are dry.

### Testing
- **The regression test asserted only that the timeout grows, which an exponential
  implementation also satisfies.** It now pins the constant step between successive retries,
  and was confirmed to fail against a doubling `rto_ms` before being taken.

## 2026-09-06 - A guest's line says what it was allowed and what it was coded in

### Changed
- **The rate allowance and the codec are on each guest's progress line.** A guest's picture is
  decided by the two together, and a room that gains a less capable guest changes both at
  once: the rate divides by the room, and the codec drops to whatever every seated guest can
  decode. Neither was visible from the rate a guest actually used, so a session where one
  guest's picture got worse because a *different* guest arrived read as a mystery. The
  allowance is published where it is decided, beside the congestion count.

### Not changed, deliberately
- **The rate reported to a guest is still the undivided one.** That key is also how a guest
  asks for a change, and the request is applied as given. Reporting the divided figure would
  have a client's panel echo it back as the new configured rate, so every guest that joined
  would ratchet the room down toward the floor. The wire figure means "the rate you may ask
  for"; the log line is where what a guest gets is stated.

## 2026-09-04 - Staleness asks whether the path got slower, not whether it is slow

### Changed
- **A fragment's staleness is judged against the round trip that held when it was queued.**
  The second staleness clause compared the smoothed round trip against a fixed
  hundred-millisecond budget, so it asked whether the path is slow rather than whether it got
  slower: true of a bad path from its first frame, never true of a good path going bad, and
  the second is the case the clause exists for. On a link whose queue was building it said
  nothing until the round trip passed an absolute figure the level sets at 130 or 200 ms. Each
  fragment now carries the round trip as it stood when it was queued, stamped once and never
  restamped, so a fragment the outstanding cap held back is compared against the path from
  before the queue built.
- **It restores a counterweight the fixed budget had removed.** Round-trip samples come from a
  fragment's first send and are not filtered, so retransmissions inflate the smoothed figure
  during congestion, which loosens the first staleness clause exactly when it should tighten.
  An inflating round trip tightens this one by the same motion, because it is measured against
  its own past.

### Measured
- **Clean paths are untouched, which is the result that mattered.** The trajectory harness's
  three lossless profiles -- still, 2 ms jitter, and 2 percent reorder -- are bit-identical
  before and after, for all three controller shapes. A staleness rule that fired on a healthy
  path would have shown up here.
- **Under genuine stress it stops overshooting.** At an 8 Mibit/s cap the rate settles at 6.56
  where it settled at 8.31, with **delivered throughput unchanged** at 5.13: the old figure was
  a ceiling the path could not carry. At 5 percent loss the rate settles at 4.63 against 5.67,
  with delivered 4.38 against 4.47 -- two percent less carried for a rate a fifth lower. One
  more decrease at the cap, two fewer under loss.

## 2026-09-04 - A fragment delivered out of order is counted as delivered

### Fixed
- **The delivered-bytes figure dropped a fragment every time the peer named one ahead of its
  cumulative count.** A receiver may name a fragment it stored out of order while still
  missing something below it; the sender clears that fragment there, which took it out of the
  walk that sums what the cumulative advance covered, so its payload never reached the figure
  at all. One fragment per such acknowledgement -- and they happen precisely under the loss
  and reorder that the delivered figure is read for, so the offered-against-delivered picture
  read worse than the path actually was. On a clean in-order path nothing names a fragment
  ahead of the cumulative, delivered tracked offered exactly, and the defect was invisible.
  The bytes are now counted where the fragment leaves the outstanding set; the two regions are
  disjoint, so nothing is counted twice.

## 2026-09-03 - A fragment is counted once, and the congestion count is proven to arrive

### Changed
- **The published fragment count takes first transmissions only.** It counted every write,
  retransmissions included, which made it grow with the very counters a reader divides it by:
  a loss rate computed from it read low, and read lower the worse the loss got. The byte
  counter still takes both, because that one answers what the path was made to carry and is
  what the rate controller steers on. The two now mean different things deliberately, and
  both say so.

### Verified
- **The congestion count is published, and the check has been shown to fail.** The
  controllers live on the encode loop's thread and the guest that reports telemetry does not,
  so the seat is the only path between them -- and every live run so far read zero, because
  nothing congested. A count that has never moved is indistinguishable from a publish that
  never happens. The test drives a congested window through the real tick and reads the count
  back through the handle a guest holds; removing the publish makes it fail, and it holds the
  total across a clean pass, because what a guest reports is what congestion has cost it for
  the whole session rather than on the last frame.

## 2026-09-03 - A roster figure is always a number, and per-frame timing is deferred

### Fixed
- **A figure that is not a number would have cost the whole roster, not one field.** The
  readers this body is written for require every metric key to be a JSON number and abandon
  the entire guest list -- every guest, not the one bad block -- when one is not, taking with
  it everything the roster gates. A non-finite float serialises as a null, which is a token
  and not a number, so a single NaN anywhere would have deleted the room from a reader's view.
  Nothing upstream produces one; a non-finite figure is now written as zero at the boundary so
  that this can never become load bearing, and the test feeds one through five blocks and
  asserts all forty values survive as numbers.

### Deferred
- **Per-frame timing telemetry ([01 §11.2](01-protocol.md)) is not implemented, deliberately.**
  It is the only source for six of the series a reader's performance graph draws -- video
  total, capture and frame time, and the three audio equivalents -- and those read zero
  against this host. It is diagnostic only: nothing renders, decodes or steers on it, the
  ordinary stats a reader shows come from the roster and the encode-latency message, and both
  are now correct. Two things must be settled before it is written, and neither is settled:
  the flag that gates emission is received and ignored, so a reader asking for the telemetry
  currently gets no answer either way; and **the order of the four video values is not
  established** -- the order recorded in §11.2 and the order a reader's own structure holds
  them in disagree on the last two, so implementing from either today would ship three correct
  series and one silently transposed. The audio form carries three values and has the same
  question.

## 2026-09-02 - The roster carries each channel's own numbers

### Fixed
- **The guest list reported zeros where a reader expected telemetry.** Every metric block in
  the roster body was filled by a helper that returned a row of zeros, five times per guest,
  while the numbers those blocks describe were already live one call away. A stock reader
  paints the roster over the figures its own messages gave it, so the effect was not a
  missing value but an alternating one: the encode figure flipped to "not reported" and back
  once a second, because two sources disagreed and only one of them was telling the truth.
- **Congestion events were counted and never published.** The counter existed in the rate
  controller and the field existed at the boundary, and nothing joined them, so an
  application reading what congestion had cost a guest read zero however hard the path was
  working. The controller lives on the loop's thread and the guest that reports it does not,
  so the count is published to the seat where the guest can reach it.
- **A peer's decode time was parsed and dropped.** It is the one figure in a guest's
  telemetry no host can measure, a stock client volunteers it unprompted, and it was being
  read off the wire and discarded. It is now stored against the channel it names. **A report
  that names no kind is taken as video**, which recovers the figure from an older peer that
  predates the field; no peer sends a kind of zero meaning anything else.

### Changed
- **`lowlat_metrics` gained a channel dimension, and it is named rather than numbered.** A
  number would be a stream index, and this host produces one stream and switches which
  display feeds it. What genuinely differs is the channel, so `control`, `audio` and `video`
  each carry a `lowlat_channel_metrics`: fragments sent, retransmissions by cause, the rate,
  what the payload cost to produce, and what the peer says it costs to decode. The figures
  that are the same across channels stay where they were -- one round trip, because there is
  one path under all of them, and one congestion count, because video is the only channel a
  rate controller steers.
- **The roster is repeated every two seconds, in time rather than in frames.** Membership
  changes on an event and the message was sent then; the telemetry inside it changes
  continuously and nothing announced that. A frame count is the obvious spacing and it is
  wrong here: a still desktop is coded at a frame a second, so a count meaning two seconds
  under load stretches to two minutes of stale numbers exactly when a reader is watching.
  Nothing is sent while the room is empty.
- **The send ring counts fragments beside the bytes it already counted**, on the same terms:
  a retransmission moves both, because what the counter answers is what the path was made to
  carry. Reported counters pin at their width rather than wrapping -- a wrap reads as a
  session that has just started, which is wrong by an unknowable amount.
- **Sound is timed around its codec**, on the same terms as the picture's figure, so the
  audio channel reports what its payload cost rather than a zero.

### Verified
- `cargo clippy --all-targets -- -D warnings` and the full suite green: eight new tests
  covering the decode report landing on the channel it names, the kind-zero fallback, an
  unknown kind landing nowhere, a counter pinning rather than wrapping, the fragment count
  including retransmissions, and the roster body carrying each channel's numbers with the
  stream array still three long and its unused entries entirely zero.

## 2026-09-01 - The public header is spelled and annotated like a C SDK

### Changed
- **Every type is `typedef enum X { ... } X;` and every use is the bare name.** The
  enumerations were generated with their width named, which only C23 and C++ have syntax
  for, so each carried a `__STDC_VERSION__` fork -- a macro left undefined by MSVC in its
  own default C mode -- and the same name meant an enumeration under one standard and an
  integer under another. The width that bought is now asserted at compile time in the
  header's own translation unit, on the three enumerations that are used as types; the other
  seven only name values of fields carried as plain integers, so their width reaches
  nothing. The generator ties the tag to the keyword and offers neither alone, so keeping
  the tags -- which is what lets an application forward-declare a handle -- would have meant
  `enum lowlat_status` at every signature and every field. The header is post-processed
  instead, in the same step that regenerates it and compares.
- **`LOWLAT_NOEXCEPT` on all twenty-nine declarations.** Nothing here unwinds: a panic is
  caught at the boundary and comes back as a status, so a C++ caller emitting landing pads
  around every call is paying for an exception that cannot arrive. It expands to `noexcept`
  in C++ and to nothing in C, and it tests `_MSVC_LANG` first, because MSVC reports
  `__cplusplus` as 199711L unless it is asked not to and the annotation would have been
  silently dropped on the compiler that most wants it.
- **The documentation is the C toolchain's dialect.** `///` rather than `//`, `@attention`
  for the caller's obligations, and a name in backticks for the cross-references. The
  definitions keep `# Safety` and rustdoc links, which is what clippy and rustdoc read; the
  translation happens on the way out. The references to these documents also pointed three
  levels up from where the header lands and now resolve from `include/`.
- **Every parameter and every return is documented**, sixty-eight of the first and
  twenty-six of the second, in the form an editor renders as a table beside the call. The
  names are checked against the signatures mechanically rather than by eye, in order, and
  the generation refuses to write a header where the two disagree. The returns were read out
  of the implementations: the deliberate panic answers the internal error and poisons the
  handle, and the code first written for it here did not exist.

### Measured
- **`noexcept` is free at runtime and not free to go without.** The hot loop is identical
  instruction for instruction and the p50 is 0.901 ns against 0.902 -- the landing pad it
  removes sits past the return, which is what zero-cost unwinding means. Compiling a
  translation unit that calls all twenty-nine with something to unwind takes **26.1 ms
  against 35.5 ms**, and the object is **8.8 KB against 18.0 KB**: no `.gcc_except_table`,
  no cold unwind code, and a third of the `.eh_frame`.
- **The width annotation never reached code.** The shared object's `.text` is identical in
  size and hash with the enumerations generated either way. It only ever described the C
  side, which is why asserting it there costs nothing to give up.

### Learned
- **Precision loses to what renders.** The editor tooling most applications read this
  header with knows a fixed set of block commands and drops every other one silently, taking
  the text under it along with it. `@pre` is the accurate word for a caller's obligation and
  it vanished; `@return` is not in the set either, only `@returns`. A cross-reference is the
  same trade the other way: `@ref` links in a generated site and reduces to undistinguished
  prose in a tooltip, so a name keeps the backticks it already had and is code in both. None
  of this is visible from the header, from the generator, or from a compiler -- only from
  the thing that displays it.
- **A documentation generator documents a file's members only when the file itself is
  documented.** Without a `@file` block the header indexed twenty-two structures and nothing
  else: every function, enumeration, constant and typedef was skipped however carefully it
  was commented, and every cross-reference into them failed to resolve. Adding one block
  took the index from twenty-three entities to two hundred and forty-one. This is not
  visible without running the generator, and it is a hole that outlives whoever wrote the
  comments.

## 2026-09-01 - A declared capability is a preference, and preferences degrade

### Changed
- **What a guest declares is a preference, and one the host cannot meet ends nobody.** A client
  offers the codec and both colour axes as "prefer this if the host has it" and follows what the
  stream turns out to be, so none of them decides whether a guest can be served. Three behaviours
  followed from reading them as requirements and all three are gone.
- **The guest that asked is no longer ended when its request will not build.** The reasoning was
  that a peer rebuilds its decoder the moment it asks and would be holding one for a stream that
  never arrives; that is true of a peer which cannot decode what it is sent, and a refused
  preference is not that. With the kick goes the whole mechanism that remembered whose request
  was being tried. Ending a session over a preference is the one thing a preference must never
  cost.
- **A screen is kept and a preference is dropped, not the other way.** A guest that asked to look
  at another output and landed on hardware that cannot code the running colour was silently put
  back on the screen it asked to leave -- from its side, the request appeared to do nothing. The
  axes now come off one at a time on whatever output is current: ten-bit colour, then full
  chroma, then the second codec. Depth first because it costs the most bytes for the least
  visible difference, the codec last because dropping it doubles the rate for the same picture.
  Only a device that refuses the baseline reaches past that, where the screen goes back as the
  last resort.
- **What was dropped is remembered against the device.** The guests go on declaring what they
  prefer, so without it the loop wants the dropped axis back on the very next pass, rebuilds,
  fails the same way and drops it again -- a stream spending itself rebuilding. It clears when
  the captured output moves, because what one card refused says nothing about the next.

### Fixed
- **A declared preference is no longer reported as a decode failure.** The line warning that a
  guest declaring any of the three bits would decode nothing fired on every ordinary connection;
  in one live session it warned about a guest that then streamed four hundred pictures without a
  fault.

## 2026-08-31 - Full chroma, measured against its source rather than looked at

### Fixed
- **The reconstruction pool was allocated in a layout the runtime chose.** A runtime format
  is a family and not a layout: at full chroma it covers a packed member and a planar one,
  and which one a runtime picks for a pool it allocates is its own decision. The source
  handed to the encoder is the packed one the conversion writes, so the device reconstructed
  into one layout and predicted from it as another. Nothing refused it -- the surfaces were
  created, the encode succeeded, the stream decoded without an error -- and it cost every
  predicted picture while the intra picture, which references nothing, came out right.
  Naming the layout on the pool fixes it: **86 dB on the refresh then 96, 96, 95, 93 and
  flat**, against 86 then 14 and settling near 12.
- **The packed surface is the coded size, not the visible one.** A picture whose height is
  not a multiple of the coding alignment is coded taller than it is shown, and the encoder
  reads every coded row out of the surface it was handed. It cost the colour and only the
  colour: one packed word carries all three components, so the luma of the rows past the end
  is cropped and never seen, while the reference the next picture predicts from is wrong at
  the top and the error is added to itself once a picture -- the first row's colour doubling,
  102 to 204 to saturated, spreading a few rows further with every predicted picture. Four
  hundred live pictures of a still desktop read 255 at the top row from the first predicted
  picture onward; after, 136 to 139 on every row of every picture. The stream is also
  **510 KB where it was 7.0 MB**, because the bits were going on coding the colour as it
  exploded. The coding alignment moves out of the parameter-set writer as `coded_size`,
  because the surface has to be allocated at it and only that writer knew it.
- **The range extensions profile names its constraint flags.** The forty-three bits after
  the frame-only flag are reserved under Main and Main 10, where zero is the only legal
  value, and are the constraint flags on the range extensions profile -- where the profile a
  decoder resolves is the one they name rather than the number in the profile field. Left
  clear, a full-chroma stream claimed a profile that does not exist in the table. A lenient
  decoder reads the field instead and carries on, which is why an outside decoder reported
  the right profile and the fault was invisible to it. Checked by parsing this encoder's own
  output against a second encoder's on the same device, which now agree field for field at
  both depths.
- **The shared-picture ring is lent at the encoder's depth.** The third interface builds its
  own pictures and lends them to the conversion, through a descriptor that was built with an
  eight-bit shorthand whatever depth the session had settled on. Nothing downstream reads the
  depth from anywhere else, so a granted ten-bit session converted with the eight-bit range
  constants and quantised against 255 into the low eight bits of a sixteen-bit sample while
  the encoder was built for ten. The shorthand is gone with it: naming the depth is now the
  only way to build the descriptor.
- **A two-plane frame is refused by a full-chroma session.** The vendor backend's upload is
  shaped for half-resolution chroma whatever the session codes, so subsampled bytes landed in
  the top of a pool allocated and registered for three full planes. Refused rather than
  widened, because unlike the depth there is nothing to widen -- the frame type carries two
  planes and full chroma needs three.
- **The chroma enumeration reaches the generated header.** The status structure has carried
  a chroma field since full chroma was negotiated, documented as one of `lowlat_chroma`, and
  the header defined no such enumeration: nothing references it by type, deliberately, so the
  generator has to be told to export it by name and was not. The drift gate could not see it,
  because it regenerates and compares and both sides were missing the same thing.

### Changed
- **The third interface reports that it codes no full chroma.** It never did -- every profile
  it builds names half-resolution chroma -- but the capability was implicit, and a stream that
  had settled on full chroma found out three steps later where the pipeline refuses to pair.
  A refusal that late is a display that fails to open, which ends every guest on the stream;
  now the stream passes over the backend before a device is asked and continues on one that
  can code it.

### Learned
- **The check that passed this path was one 128x128 picture of uniform white.** Flat colour
  makes every row length describe the plane correctly, one workgroup covers the extent, and a
  lone intra picture predicts nothing, so no size, layout or reference fault can show. The
  import test now uses a real size over a run with chroma detail at the pixel, keeps every
  fed surface beside the stream for a decoder comparison, and carries the knobs that
  separate the causes: the subsampled layout through the identical path as the control, the
  picture held still, and every picture refreshed. Held still is what named the second fault
  -- a predicted picture identical to its reference carries neither motion nor residual, so
  a decoder can only show its own reconstruction, and it still collapsed.
- **Four faults of one shape in this phase, three of them here.** A size or a layout known in
  one place and not another, refused nowhere, decoding without an error. The three found here
  were the first where the wrong place was a surface this code allocated rather than a set it
  wrote, and the last of them was invisible in luma while destroying the colour.

## 2026-08-30 - Full chroma is negotiated, gated and live

### Measured
- **The whole phase gate is green.** The chroma axis rides beside the depth
axis end to
  end -- configuration, reinitialization, the status -- and the offer is gated
on a census
  over every encoder the host could select: a machine with one part that
cannot code full
  chroma refuses it with the part named, because a later output move onto that
part would
  end the session rather than degrade. The rig is that machine, and the
refusal is verified
  by forcing it; the preferred-third-encoder refusal is a committed test.
- **The live overlapped loop reports the cost the serialized probe could
not**: 2560x1440 at
  full frame rate on the vendor encoder, twenty seconds a run, the host stage
sum reads
  4.489 ms against 4.116 at eight bits and 4.648 ms against 4.198 at ten --
  full chroma costs about ten percent of the host path at either depth, with
the
  conversion's share under 0.1 ms.
- **The live 4:4:4 stream decodes through two decoder families** at both
depths: the
  software family and the open stack's hardware one each read `Rext /
yuv444p` and `Rext /
  yuv444p10le` from the vendor encoder's output without an error.

## 2026-08-30 - The open backend codes Main444 and Main444_10

### Measured
- **Both depths encode and decode on the open backend, with the pixels checked,
  not just the
  headers.** An outside decoder reads `Rext / yuv444p` and `Rext / yuv444p10le`
from both the
  self-encode path and the packed-import path, and the decoded values land on th
e reference:
  the eight-bit import reads 235/128/128 from a white source, the ten-bit one 94
0/512/512.
  The synthetic ten-bit path decodes at exactly four times its eight-bit twin, s
o the upload
  scale and the surface depth agree.
- **The packed ten-bit word is the one the importer documents**: X, red differen
ce, luma, blue
  difference at 2:10:10:10. The first composition used a different order and a
  4:2:2-shaped four-character code that the driver accepted anyway, and the stre
am decoded
  without an error to a plausible wrong picture; the decoded-pixel comparison ca
ught it, and
  the reference test now pins the corrected composition at both depths.
- **The two recorded low-power traps hold on the full-chroma path.** The set wri
ter declares
  the range-extensions profile only when full chroma is asked, the transform tre
e the device
  actually codes stays the declared one, and the re-run of the sets test passes
at both
  depths with the chroma knob on.

## 2026-08-30 - The conversion now has one body per 4:4:4 layout

### Measured
- **The packed full-chroma body landed beside the planar one**, still one shader file and one
  set of colour rules: AYUV at eight bits, Y410 at ten, composed word by word through a
  dedicated integer binding, with the byte orders pinned by a committed reference test at both
  depths. The fallback tier compiles the same source untouched, so nothing drifted.

## 2026-08-30 - The vendor interface codes 4:4:4 on the live path

### Measured
- **The conversion gained its first 4:4:4 body.** The shader is still one file: the
  full-chroma conversion is a second entry in it, compiled to its own blob with a define that
  picks the wrapper, so the colour rules and the summary stay in one place. It lands on the
  reference at both depths in the committed test.
- **`pipeline-probe` walks the whole path at 4:4:4.** A chroma knob makes it capture, convert
  into three full-resolution planes, export, import into the vendor's runtime and register,
  and an outside decoder reads `Rext / yuv444p` and `Rext / yuv444p10le` from the 2560x1440
  output. The 4:2:0 run beside it still decodes, so the renumbered descriptor layout changed
  nothing on the ordinary path.

## 2026-08-30 - Phase 11.6 is written, and 4:4:4 is measured at both depths

### Measured
- **The vendor interface at ten bits.** `colour-cost-probe` gained a depth knob: on identical
  content, 4:4:4 reads **1.86x the bytes** against 4:2:0 at ten bits (1.39x at eight), with
  the dumps verified by an outside decoder as `Rext / yuv444p10le` against
  `Main 10 / yuv420p10le`. The serialized encode-time ratio at ten bits is not quotable --
  two semantically identical probe shapes read 1.24x and 2.1x, each stable -- and the live
  overlapped loop is the measurement that will settle it.
- **What the open stack wants as a 4:4:4 surface**, asked of the driver with a new ignored
  probe test rather than read off a matrix: packed **AYUV or XYUV at eight bits, Y410 at
  ten**, through the low-power entry point only, importable over DRM prime. The 4:2:0 answers
  come from the same call as a cross-check.
- **The third interface has no device to serve 4:4:4**: it refuses on one vendor, offers no
  encode queue on another, and on the third the vendor interface is already the better
  encoder.
- **The encoder-engine count is read, not remembered.** The vendor backend's capabilities now
  report how many engines a part carries, and the probe prints it; the card here answers one,
  which closes the split-encode question for this hardware.

### Planned
- **Phase 11.6** lands in the impl plan, unchecked, with the conversion shapes settled: one
  body per layout -- three planar planes for the vendor interface, one packed plane for the
  open stack -- each keeping the depth uniform and sharing the colour rules. The offer stays
  gated on every encoder the host could select (D11), and 4:4:4 remains out of v1 on
  coverage, not cost.

## 2026-08-30 - Ten-bit is v1 on HEVC, 4:4:4 is out, and both were measured first

### Decided
- **Ten-bit colour enters v1 on HEVC** and gets its own phase (11.5). The display already hands
  the conversion ten bits for the ordinary desktop and the conversion discards them at the
  write, so this recovers a loss rather than adding a feature. **Negotiated, not configured**:
  a guest declares the depth, the consensus across seated guests decides, and a session runs
  eight-bit until one asks. No host setting and no new configuration field.
- **4:4:4 is out**, with the cost recorded so the question is not re-opened from intuition:
  **0.22 ms a picture (1.09x) and 1.39x the bytes**, measured at 1080p over 2000 pictures on
  the vendor backend with identical content both ways. What settles it is coverage rather than
  cost -- one of the three encoders produces no 4:4:4 at any depth, and since the encoder
  follows the display, a host that offered it and then had its captured output moved to that
  device would have to end the session rather than degrade.

### Measured
- **The colour matrix, per part, through all three interfaces.** Ten-bit 4:2:0 is universal
  among parts that can host at all, including on the interface where the conversion writes the
  encoder's own picture -- that arrangement survives the depth change, which was the open
  question. 4:4:4 is absent from one vendor in both directions.
- **H.264 above 8-bit 4:2:0 does not exist to be used**, established four independent ways: no
  profile on the open stack, an outright refusal from the vendor encoder, no way to name one in
  the third interface's headers at all, and a part that will encode H.264 4:4:4 refusing to
  decode it on the same chip.
- Two probes are committed and reproduce all of it: `colour-profile-probe` asks each interface
  what colour it will encode and whether a shader may still write the picture; `colour-cost-probe`
  measures one chroma layout against another and **dumps both streams so an outside decoder can
  say what they really were** -- two runs silently coding the same chroma would otherwise
  produce a believable comparison of nothing.

### Corrected
- **A quoted figure of "4:4:4 triples encode time" is withdrawn.** It came from a platform where
  4:4:4 also loses encode overlap, because the encoder there reads the capture's own staging
  surface. Overlap here comes from a per-slot ring and is unaffected by the input format, and
  the measured cost is 1.09x.
- **"4:4:4 is a branch in the existing conversion shader" is withdrawn.** That reasoning came
  from the decode direction, where a sampler hides subsampling and the layout is a texture
  dimension. Writing is not symmetric: the two chroma layouts differ in kind rather than in
  size, so it needs its own compiled variant exactly as depth does.
- **The video header's depth bit stops being "never set".** The rule that mattered was always
  that the bit must describe the pixels, not that it must be clear; a receiver builds its
  decoder from it before parsing anything, so a bit disagreeing with the stream fails every
  picture whichever way it disagrees.

### Found
- **The host parses a peer's disconnect status and throws it away.** A guest leaving because it
  could not decode is currently indistinguishable from one that closed its window, which is
  exactly the failure ten-bit can cause and the only place it is reportable. Fixed as an item
  in phase 11.5 rather than separately, because that is what makes it load-bearing.

### Changed
- `lowlat_host_status` gains the **live** codec, chroma and depth, read from the running encoder
  rather than the configuration, because a guest's request moves them mid-session. Enumerations
  where the axis can grow, a flag where it cannot.
- The set of capabilities a pipeline cannot emit stops being a constant and becomes a function
  of the encoder actually built, so a refusal can name the backend that refused.
- The vendor backend carries a chroma setting used by the probe alone, default unchanged, plus
  a 4:4:4 capability query. It ships default-off so the measurement stays reproducible against
  future drivers.


## The controller's trajectory is measured, beside its candidates

**A rate-controlled loop was being changed on argument.** The simulator now
drives a session over a scripted link with the sender offering frames at the
rate the controller lands on, so the window is the path's answer to the rate
and nothing else; the incumbent and the two candidate predicates -- an
explicit loss rate, and a peak tracker fed delivered bytes instead of
offered -- run over identical seeded traffic, with the rate reported in
tenths. The link also carries an optional byte budget, a policer rather than
a queue, because a loss-only profile cannot show the case the candidates are
aimed at and a queued model under a rate-following sender never converges.
The congested half of the controller's tick split into a `cut` so a
candidate predicate runs through the same arithmetic. First picture: under
pure loss the incumbent's cuts come from the timeout's resends going stale
rather than from any peer report; under an 8 Mibit/s cap the incumbent
settles at 8.3, the loss-rate candidate at 7.2 with fewer cuts, and the
goodput-fed peak at 6.9 with the fewest. Nothing actuates anywhere else; the
candidates earn adoption from this picture or not at all.

## The transport counts what the path did, beside what it was asked

**The numbers a host steers by said nothing about delivery.** The byte count
a stream's throughput is read from includes retransmissions by design, so
what climbed under loss was the offered load; a negative acknowledgement
fired the fast retransmission and reached no counter; the round-trip
estimate had no recent minimum to read a building queue against; and an
acknowledgement silence -- the earliest signal a return path has stopped --
was not timestamped at all. The send rings now count delivered payload bytes
beside offered ones and resends by cause (the peer reported the gap, or the
timeout found it), the session keeps a windowed minimum round trip and the
arrival time of the last acknowledgement, and the guest log line carries all
four beside the figures it already had. Telemetry only: nothing actuates on
any of them, and the wire and the controller are untouched.

## The acknowledgement cadence has two floors, not one

**Every accepted store was answered immediately**, so a video channel at full
rate produced one acknowledgement per fragment: hundreds of datagrams a
second, each one a receive, a decrypt and a send on the far side for nothing
the cumulative counts would not have carried within the floor anyway. The
cadence is two floors on one timestamp: a data-driven acknowledgement is
suppressed unless 10 ms have passed since the last one, leaving early only
when it carries a negative acknowledgement or the arriving fragment ends its
message, and the 30 ms timer is the keepalive, firing only when nothing else
has sent. Control and input messages are single fragments, so the second
bypass keeps the handshake and small messages at full speed. Watched red
first: a non-tail fragment inside the floor is now answered by nothing, a
gap or a message tail inside the floor still answers at once, and the
keepalive cadence is untouched.

## Translated-path checks wait for the readiness marker

**The readiness marker was recorded and read by nothing**, while the far
side of this exchange gates its own translated-path checks on it and checks
direct candidates immediately. A full-length check that reaches a
translator before the peer has sent anything outward is unsolicited traffic
that can commit a state entry whose reply tuple is exactly the one the
peer's punch then needs -- the poisoning the namespace fixtures' own guard
comment describes. The engine now holds checks toward reflexive and
unverified translated-path candidates until the marker arrives, forwarded
from the seam as readiness rather than as the arbitrary address that rides
on it; direct candidates and the mapping probe never wait, a gated
candidate does not arm the wakeup timer, and a peer that never sends the
marker still establishes through the checks it sends us. A failed punch
also says which typed failure it was in the log now, because a probe
timeout is the one outcome that justifies escalating and a missing
candidate list is a signaling gap, not a network one.

## A lying emission length is refused, not copied

**The send batch bounded a committed length against the whole staging
buffer while its restart copy spans that length from the current offset**,
so an emission that claimed more than the stage had handed out read past
the buffer and panicked on the send path. Bounded against the actual room
now; watched panicking first with a sixty-three kibibyte stage and a
two-kilobyte lie.

## The candidate model carries what the exchange carries

**The boundary collapsed the exchange's two candidate markings into one**,
and a capture of a live multi-client session showed all three of their
combinations in real traffic: lan (host addresses, and every IPv6 address
-- no translation to negotiate on that family, however the address was
found), server-reflexive, and neither -- a peer's public address at its
local port, a translated-path guess no server verified. The candidate
struct and the outbound candidate event now carry both flags; the boundary
decides the marking (IPv6 goes out lan whichever probe discovered it, so a
peer checks it at once and keeps its one path-opening probe for a
translated path) and applications relay both directions verbatim. The
engine models the third class alongside the other two: checked like
anything else, never the probe's target. Two stale claims in the
connectivity document fell to the same capture and are corrected in place,
and the readiness placeholder this implementation sends was observed
verbatim from a stock peer.

## An answer of any latency inside the window matches

**A candidate and a server held one transaction identifier, overwritten by
each re-check**, so an answer slower than the 500 ms cadence always matched
an already-replaced identifier and was discarded as redundant: a path whose
round trip exceeds the cadence failed all fifteen checks deterministically,
the answer forever one identifier behind. Every identifier is now kept for
the life of the attempt, per target, in storage fixed by what the window's
budget allows, and freed with the attempt. Watched red first from both
sides: a candidate's slow answer establishing, and a server's slow report
teaching.

## The probe waits for the candidate it exists for

**The one path-opening probe went to whichever candidate arrived first**,
and the document showed a probe per candidate; both were wrong. The probe
is once per attempt and its target is the peer's server-reflexive candidate
alone -- the path that crosses translation, the only mapping worth opening
ahead of a full-length check -- so a directly routable candidate never draws
it, and the latch waits for a reflexive candidate to exist rather than
spending itself on the first arrival. Candidates now carry the distinction
from the application's signaling through the boundary (`reflexive` on the
candidate struct, in what was a reserved byte; zero is safe and merely
forgoes the early probe), 03 s5's pseudocode is corrected in place, and all
seven namespace topologies still establish or refuse exactly as specified.

## Only a global unicast v6 source is offered

**The probed IPv6 host candidate excluded only loopback and the unspecified
address**, while 03 s3 has always required a globally routable one. On a
network built on unique-local addressing the routing table answers the probe
with a ULA -- routable here, invisible everywhere else -- and a link-local
answer would need a scope the candidate cannot carry; either way the peer
spends part of a bounded check budget on an address that cannot answer. The
filter now requires global unicast, as its own predicate with the decision
table written out in a test. The validated v6 discovery is the reflexive
path, which needs no filter by construction: a server that answered over v6
proves the route, and every configured server name resolves to one address
per family already. The daemon's default server list now names two operators
on separate infrastructure -- two answers is what tells a symmetric
translator from an endpoint-independent one, one outage no longer costs
every reflexive candidate, and two names by two families fills exactly the
four server slots the engine holds.

## A transient refusal no longer costs offload for the session

**One failed offload send disabled segmentation for the rest of the run,
whatever the failure was.** A full send buffer in the middle of a keyframe
burst -- the exact load segmentation exists for -- counted the same as a
kernel that cannot segment at all, and the session paid a syscall per
datagram forever after, on one warning line. The latch now fires only on the
capability errors, written out as a closed set with transient as the
default; anything about the moment (a full buffer, an interrupt, a policy or
a route refusing the destination) falls back for that batch alone, and the
per-datagram sends it falls back to are counted by the existing refusal
accounting. The join bound also closes at what one send may carry (one
maximal UDP payload) rather than at the staging buffer, which is
twenty-nine bytes larger: an exact-fit batch of kibibyte segments used to
reach the kernel only to be refused whole -- and that refusal then killed
offload too. Both halves watched red first: a policy-refused burst kept
offload where it used to lose it, and the sixty-fourth kibibyte datagram now
starts a new batch instead of poisoning the one it filled.

## The answer leaves from the address it was asked at

**Packet information was enabled at the socket and never consumed**, so every
reply left from whatever source the routing table picked. On a host with
several addresses -- the exact case enumerating host candidates exists for --
a check probing the second address was answered from the first: unsolicited
traffic to the peer's filtering translator, which drops it, so that
candidate could never complete a check. Receive now reports the address each
datagram arrived at; a binding answer leaves from exactly that address; and
the address the winning answer arrived at is latched with the path and
claimed on every datagram for the session's life, so a routing change cannot
move the source out from under the peer's filter mid-stream. The claim rides
the offloaded send path too, and only names the source -- the interface
choice stays with the routing table. A seventh namespace fixture (multihome)
proves it end to end: the probing side sits behind a port restricted
translator, so the kernel itself drops an answer from the wrong address and
the punch only completes when the pin is real -- watched failing with the
pin disabled before it was trusted.

## The pass runs on the post-wait clock

**The shell read its clock once per pass, before the wait, and stamped
everything after the wait with it.** A pass woken by its own deadline saw the
deadline as not yet due, emitted nothing, and paid a second wake one clamped
minimum later -- every idle-path deadline cost two wakes and fired a pass late
-- and an acknowledgement arriving mid-wait was stamped before it arrived, so
round-trip samples read short by up to a full wait and fed the retransmission
clock noise. The wait is now armed from one reading and the pass runs on a
second, taken as the wait returns; the shell owns the clock outright and hands
the pass's reading back, so its caller times the rest of the pass with the
same value. The poll timeout also rounds up rather than truncating, because a
fractional wait rounded down wakes just before the deadline it was armed for
-- the same two-wake pattern by another route. The idle wake-accounting test
now runs on real time over an established path, where it fails against either
half of the defect; it previously stepped a synthetic clock by exactly the
cadence, which is the one drive pattern that cannot see them.

## The vestigial controller

**The session no longer carries a second rate controller.** It ticked at poll
cadence with a hardcoded zero throughput, summed pressure across every
channel, and its output was read by nobody -- the live control is the
per-guest controller fed per frame with the video channel's real samples. A
duplicate that is wrong on every input is a trap for whoever finds it first.

## The send window holds to the shallow ring until the peer is known

**The send window may not exceed the peer's ring depth, and that depth is not
a constant.** The oldest generation carries 1500 slots per channel where
current ones carry 4000, and the window was bounded by the host's own storage
instead -- so against a shallow peer it could run onto slots the peer had not
delivered from, a wrap that looks like the peer losing fragments it already
took. The window now starts at the smallest ring in circulation and opens to
the deep ring once the peer reports the full channel count in a group
acknowledgement, which is its generation's own statement and arrives during
the control handshake, before any media is under load. A peer reporting fewer
channels is the older generation and the floor stands.

## The legacy cipher path

**An offer without a media key takes the 128-bit mode, keyed from the host's
fingerprint.** The field does not exist for that peer generation, so its
absence is the selection -- exactly what the attempt info always documented
and the implementation never honored: every session was keyed 256-bit
regardless, so a peer of that generation could never establish. The material
decoder now takes the cipher's key length (the nonce prefix follows the key,
so its offset moves with it), the guest session is built with the attempt's
own cipher, and a legacy answer carries no media key at all.

## Three small guards

- **The counter stops at the sequence space.** The envelope's counter field is
  two bytes of epoch and six of sequence number, so the usable space is 48
  bits; the sealer now refuses the first value past it rather than running
  into bits peers refuse. Nine centuries away at ten thousand packets a
  second, and stated in code because a counter that wrapped would reuse a
  nonce.
- **A stall is said once.** The 60-second soft liveness state was computed and
  observed by nothing; the guest loop now logs it once per transition, which
  is the line a live run reads backwards from a hard failure.
- **A zero kick reason is refused at the seam** as well as at the public
  boundary. A peer carries on through a zero status, so a kick carrying one
  ends nobody while reporting that it worked.

## One unit on the control path

**The rate control path runs in mebibits per second; decimal megabits exist
only at the boundary.** The throughput sample, the sound cost and the
controller's tuning constants all divide by 2^20, but the configured rate
entered undivided and the actuator multiplied by 10^6 -- the budget ran 4.66
percent high and the encoder was driven the same amount low. The
configuration, the live change and the reported per-guest bitrate speak
decimal megabits and convert once at the boundary. The delivery gate's
ceiling steps stay on the boundary scale, so the documented thresholds keep
their meaning.

## The acknowledgement path, corrected in three places

- **A keepalive frees windows and nothing else.** Its trigger field is zeros by
  construction, not a name; read as an acknowledgement it cleared the fragment
  at sequence zero of channel zero while that fragment was in flight, and
  fabricated a round-trip sample from its age. A lost first control fragment
  was never retransmitted and the channel wedged until the delivery deadline.
  The cumulative counts still apply.
- **The group acknowledgement names the accepted store.** The trigger was
  stamped from the last data arrival on any channel, before the ring was even
  consulted, and the negative bit was a gap on any ring. An acknowledgement
  could therefore name a fragment whose store was refused -- which the peer
  clears and never retransmits, a permanent gap -- and a gap on one channel
  could fast-retransmit another's window. The trigger is now captured at the
  accepted store; the negative bit is the storing channel's own gap, with a
  reorder of two or less tolerated; a refused store is counted per kind
  (`rx_dup`, `rx_oow`, `rx_big` on the guest line) and acknowledged by
  nothing. A pending negative is not displaced by a later clean arrival
  before the acknowledgement leaves.
- **A control message up to the protocol ceiling is taken.** The take buffer
  stopped at 64 KiB against the mebibyte the protocol permits, and a take that
  does not fit ends the attempt, so a peer sending a legal 100 KiB body killed
  its own session. The buffer is now the ceiling plus a header, sized once at
  guest spawn; only a message past the protocol ceiling remains terminal.

Each carries a regression test that was watched to fail first.

## Three peer sizes that were never one size

**`docs/01-protocol.md` corrected in three places.** The document treated a
peer's ring depth, its slot payload capacity and its receive buffer as protocol
constants. They are none of them constants; each peer generation picks its own,
and the three in circulation disagree on every one.

- **Ring depth is 1500 to 4000, not 4000.** The oldest generation runs four
  channels of 1500 slots where the current two run nineteen of 4000. The safe
  send window against an unidentified peer is therefore 1500. Nothing shipping
  is affected -- the outstanding fragment cap of 100 holds a conforming sender
  an order of magnitude below either -- but the bound was stated as a MUST and
  the MUST was wrong.
- **The newest generation receives 1229 bytes and no more**, exactly one
  default-sized datagram, against 2000 and 3000 for the other two. It does not
  test whether a read was truncated, so an oversized datagram is cut short,
  fails authentication and is counted as corrupt. That is indistinguishable
  from ordinary loss, which is why nothing ever surfaced it. **1229 is the only
  datagram size every peer accepts.**
- **The path probe is unchanged and still correct**: a probe is judged by
  whether it is acknowledged, so against a current peer the first step fails
  and the session stays at the floor, which is the intended outcome. What
  changes is the expectation -- probing buys nothing against a current client,
  and its failure is not a defect.
- **The group acknowledgement has no fixed size.** The entry count is the
  sender's channel count, so 23 bytes and 83 bytes are both valid and a decoder
  that requires either drops every acknowledgement the other generation sends.
  The constants table said 83.

Also recorded in §3: the envelope's counter sits in the record's epoch and
sequence-number positions, giving a **48-bit** space rather than 64, and a
sender must stop rather than wrap. Nine centuries away at ten thousand packets
a second, and stated because a wrapped counter reuses a nonce.

## One quality setting on the boundary

**`lowlat_quality` in the host configuration**, three values, settled when
hosting starts. An encoder has a dozen knobs and almost none are an
application's business; what an application wants to say is whether its guests
would rather wait less or look at more.

- **Zero is the low-latency end**, so a zeroed structure gets the sensible
  default rather than the middle of a range.
- **An unrecognised value is refused, not rounded.** Serving the nearest
  setting is how an application comes to believe it got what it asked for.
- **Two levers underneath, both already present**: the quantiser floor, which
  bounds how many bits a picture may spend and so its time on the wire, and the
  effort level, which is how far the encoder searches. Backends express what
  they can -- two of the three fix their effort when the session is created and
  take the floor alone.
- **Settled rather than live.** It is what the encoder is built with, and one
  encode serves every seat, so moving it under a running session would change
  the picture every guest is watching on one guest's behalf.
- **What a host reports back is what it asked for.** Nothing in any of these
  interfaces says whether a driver acted on either lever, and they differ: one
  takes the floor on H.264 and ignores it on H.265, on the same part. A host
  logs the setting and the derived levers once per stream; measuring coded
  bytes is the only way to learn what a device really did.

The daemon takes `--quality latency|balanced|quality`, named rather than
numbered, because a number there is a code somebody has to look up.

## The encoder was never told how hard to search

**Every picture carried a rate and a frame rate and nothing about effort**, so
each device ran at whatever it considers balanced -- a setting chosen for
transcoding a file rather than for a desktop somebody is waiting on. The
interface has a level for it and both devices measured here advertise a range
that was never read.

**Effort is not fidelity**, and the two are easy to conflate. A level says how
far the encoder searches -- motion range, sub-pixel refinement, how many modes
it tries -- not how coarsely it quantises. The bitrate is already what holds
fidelity up, so search the rate will not let the encoder spend is latency with
nothing to show for it.

- **Measured at 1080p, 500 pictures, against what each device does when
  nothing is sent.** A discrete part offering seven levels: 1.98 ms unsent,
  3.33 at the most thorough, 1.94 in the middle, 1.53 at the top. An
  integrated part offering thirty-two: 2.97 unsent, then 3.23, 3.51 and 2.98.
- **One implements the range and one does not.** The second's own default is
  already as quick as anything it offers, and the middle of its range is
  slower than doing nothing.
- **So the top of a device's own range is what is asked for**, which serves
  both without any knowledge of which device is which: half a millisecond on
  the one that implements it, neither gain nor loss on the one that does not.
- A device advertising no range is sent nothing, because a level it never
  offered is a configuration it did not agree to.

**What this is not.** It is not a quantiser floor, which is a different
control with the opposite shape -- a floor bounds how many bits a picture may
spend and so bounds its time on the wire, where this bounds how long the
encoder spends deciding. That one is not implemented and the rate control
buffer still carries a floor of zero.

## The wakeup poke was too small to wake anything fully

**The integrated device powers its compute block down between frames**, and a
trivial submission is sent ahead of the next present so the conversion that
follows runs warm. Both halves of how that was sized were wrong, and each hid
the other: a poke that recovers half the wakeup reads as a working poke, and a
lead that lands past the wake reads as a lead that does not matter.

- **The poke covered a 256th of the picture.** Swept against 0.43 ms warm and
  1.95 ms with no poke at all: a sixteenth of each axis leaves the conversion
  at 0.93 ms, a quarter of each axis reaches 0.44 but keeps a 1.09 ms tail,
  and only a whole-picture poke holds the median and the tail together, at
  0.43 and 0.55.
- **The lead only matters once the poke is large enough that it can.** At full
  size the conversion after it costs 0.43 ms at half a millisecond of lead,
  0.44 at one, 0.99 at two and 2.04 at three -- the last being the same as
  never poking.
- **Neither constant does anything alone.** Moving the lead by itself was
  tried first, and measured as no change both live and on the bench, which is
  what sent the search to the other half.
- **Live, integrated head at 1080p**: the conversion stage reads 0.54 p50
  where it read 1.00, and the figure a guest is told goes from about 4.0 ms to
  about 3.6. At 1440p the bench reads 1.18 to 0.70 against 0.64 warm.

The poke now costs a second conversion of device time per present. That is
spent in the window the loop is already blocked waiting for the vblank, so it
adds nothing to the latency it removes.

## The encoder ran on whichever device was numbered first

**The open backend named its device by a constant** while the choice of
backend already followed the display. On a machine with one video-capable
card the two agree and nothing shows. With two, which card is numbered first
is the order the kernel probed them in, so installing or moving a card
repoints the encoder at a device that did not draw the picture: it still
encodes, it crosses the bus every frame, and nothing in the log says which
device is doing the work.

**The node is taken from the display's own device now.** The constant remains
only for a host with no display to follow, and the device the encode runs on
is logged once per stream.

- **Measured on one head at 1080p, no code changing across the three**:
  4.47 ms guest-visible with one video-capable card, 5.5 to 6.0 ms once a
  second card took the first number and the encode landed on it, 4.0 ms with
  that card removed again.
- **The check asserts the pairing, not the number.** A test naming a
  particular node passes on any single-card machine and verifies nothing;
  this one requires the render node and the display node to resolve to the
  same device, and that no two devices are handed the same node.
- **The wait between a submit and the collect costs nothing**, which closes a
  latency item this changelog's previous entry left open. The device begins
  the picture when it is submitted, so a delay before the collect shortens
  the wait by exactly itself and leaves the whole span unchanged -- measured
  on three devices and both codecs. The gap that does cost is one taken
  before the submit, on a device that powers its encode block down between
  pictures.

## The open encoder's collect waited on the wrong object

**Encode latency ran about twice what the hardware does**, and it was the
collect, not the encoder. The collect synchronised on the coded buffer with a
zero timeout and read the timeout as "not done yet". That buffer's completion
signal lags the actual encode by milliseconds on every open-stack driver
measured, so a finished picture was reported long after it finished.

**The surface is what says the encode is done**, and it is what the reference
encoder waits on. The collect now blocks on the surface synchronise, which is
the encode time itself.

- **Measured on both cards, 1080p HEVC, registered path**: the Intel
  low-power path reads 7.5 ms against the buffer probe and 2.1 ms against
  the surface; the other card 10.7 ms against 3.0 ms. The reference encoder
  on the same device reads 3.2 ms.
- **A picture in flight costs the wait either way.** Against a busy encoder
  there is nothing the loop could usefully spend the gap on, so the probe
  bought responsiveness it could not spend.
- The coded-buffer symbol is gone from the loaded runtime; the surface
  synchronise exists in every version this backend runs on.

## HEVC predicted pictures drifted into a ghost, on the low-power path only

**A live HEVC stream decodes into a ghost of earlier content** on the
low-power entry point: the first picture is right and every one after is
worse, until the moving parts of the picture smear and tear, with no
decode error anywhere. The same stream on the other card decodes
exactly, which is what made it look like a driver fault rather than one
of our own.

**The picture set declared the quantiser delta at the top of the block
tree only.** `diff_cu_qp_delta_depth` was zero, which tells a decoder to
read a quantiser delta at the 64x64 block and nowhere finer. The device
quantises down to the smallest coding block regardless, so every decoder
rebuilt each picture at a coarser quantiser than the encoder had
predicted from. The difference is small for one picture and compounds
across a run of predicted pictures.

- **The device's own granularity is what the set now carries.** The
  depth is the difference between the coding tree block and the smallest
  coding block, written in both the packed set and the picture parameter
  buffer, so the decoder's quantiser map and the encoder's match.
- **The other card never showed it because it rewrites the set** to
  match what it codes; the low-power driver writes exactly the bytes it
  is handed, so a disagreement that was always wrong is only visible
  there. The same shape as the two faults above it.
- **Found by a pixel comparison over a run, not by a decode-error
  count.** The stream decoded cleanly at every stage of being wrong: the
  drift probe reads 42 dB at the sixtieth picture against 99 once fixed.
  Named byte-level test on the picture set, watched to fail.

## Two faults in one picture, each hiding the other

**H.264 encoded, returned plausible bytes, and decoded to flat grey** on
the low-power path -- the entropy decoder lost synchronisation on the
very first macroblock of the first picture and never recovered. HEVC on
the same card was clean throughout, which is what made it look like a
codec-specific driver problem rather than two of our own mistakes.

**A refresh period of zero is not "no period".** One driver reads it that
way; another does not. The interface has no spelling for "never", so the
period is now longer than any session will last, which both read alike.

**And the picture parameter buffer named a tool the parameter set
denied.** The device was told the eight-by-eight transform was available
while the set written for it stopped above the field that declares it --
and a set that omits that field declares, by inference, that the tool is
off. So the hardware coded with it and the decoder was told to expect
nothing of it. The set carries the field now, so the two agree and the
tool is kept rather than surrendered.

- **Neither is a device quirk and neither is guarded by one.** A
  parameter set that disagrees with the device it configures was always
  wrong. It never reached the wire because the driver in front of it
  rewrites the set to match what it actually coded; the other driver
  writes exactly the bytes it is handed, and that is the whole of the
  difference. **A second vendor is what turned a latent error into a
  visible one**, which is the argument for having one.
- **Each fix alone changed the error count and fixed nothing**, so six
  single-variable experiments came back negative against a configuration
  the other fault was still poisoning. The hypotheses were not wrong; the
  ground under them was. Where two faults overlap, a negative result
  means less than it looks like -- and the way out was to stop testing
  one variable against a broken baseline and compare the whole
  configuration against an encoder known to work on the same device.
- The headers were proven innocent first, three ways, and that held: the
  bytes handed to the driver appear in the stream unchanged, the slice
  header decodes field by field to exactly what it should be, and
  suppressing it so the driver writes its own changes nothing.
- Verified on both cards and both codecs, with the decoded picture
  checked to be a picture rather than concealment -- identical luma
  statistics from each. Then each fix was reverted on its own and watched
  to fail.

## An encoder that reported no encoder, on a card that has two

**The open backend asked for one entry point by name.** A device offers
the slice entry point, the low-power one, or both; newer discrete parts
of one vendor carry only the second, having dropped the shader-driven
encode path entirely. Against those the probe walked every profile the
driver listed, found none that answered on the entry point it knew, and
refused with "no encoder" -- naming a missing profile for what was a
missing entry point, on a card that encodes both codecs.

The probe takes the first of the two a profile offers, and the
capabilities carry which one answered, because everything downstream has
to name what they were read under. **The order is unchanged where a
device offers both**, so nothing already working changes: the shipped
measurements were all taken on the slice entry point and they still
describe what runs. Which one is better where both exist is a
measurement nobody has made.

- **A second card found two test assumptions, which is the point of
  having one.** The render node was a constant, so only whichever the
  loader numbered first could be measured at all, and the second card is
  exactly where a backend's assumptions get found out. And a surface
  identifier of zero was read as unallocated, which is one driver's
  numbering rather than the interface's rule -- an absent surface has its
  own sentinel, and the other driver numbers its pool from zero.
- **The H.264 fault this opened is closed below**, and the headers were
  innocent as the evidence said: the fault was in what the device was
  configured with.

## A frame larger than a guest's ceiling stopped deciding forever

**Connected, input working, nothing on screen**, and the only way out was
to switch displays. The host captured, converted and encoded throughout,
and every refresh counter read zero -- which is what named it, because a
guest waiting for a picture with no history behind it should have been
asking for one continuously.

The delivery mark was the largest frame of the session and **only ever
grew**. A guest waiting for a keyframe may only ask for one when it has
room for that mark, so a single frame above its ceiling meant it could
neither be admitted nor ask to be, for as long as the stream lived. A
coded refresh on a 2560x1440 output is enough at ten megabits, where the
ceiling is fifteen hundred fragments -- which is why it appeared on
switching to a larger screen, why it survived every reconnection, and why
rebuilding the stream under it cleared it.

**The mark now spans a window rather than a session**: two of two
seconds, so a size is remembered for between two and four and then stops
deciding. Measured in time and not in frames, because a still desktop
sends one picture a second and a window counted in frames would hold a
spike for ten minutes on exactly the stream a guest is most likely to be
joining. Named regression test, watched to fail.

- **What the counters could not say is what took longest to find.** There
  is a count for a keyframe refused for want of room and none for a guest
  that could not even ask, so the log said nothing at all rather than
  saying it repeatedly. The absence was the evidence in the end -- a
  joining guest and `sent=0 no_room=0 asked=0` together are impossible
  unless the request itself is being withheld.

## 11.9: the third encoder carries the other codec

**The encoder that shares the capture's device codes HEVC now**, where it
coded H.264 only. A stream asking for the other codec followed the display
instead -- and that is the codec a stock client negotiates, so the
preference knob did nothing for the client most likely to set it. The
profile, the capabilities, the three parameter sets, the picture and slice
descriptions, and the reference set a predicted picture carries inline are
all the codec's own. Verified on both devices under the validation layer,
300 pictures each, decoded in full outside the project; and through the
conversion writing the encoder's own ring, with the decoded pixels compared
against the source picture. 1080p costs 2.99 ms a picture on the integrated
device, which is what the other codec costs there.

**A live client stream ran on it the same day.** A stock client negotiated
the codec mid-session and the ring was rebuilt for it: about ninety frames
over six seconds at 1080p on the integrated head, nothing lost, nothing
stale, nothing refused, the guest reporting 4.1 to 4.4 ms an encode. The
client then moved the capture to the other card, where a copy would have to
stand between the conversion and the encode; the path refused it in one
logged line and let the display's own encoder take the stream, which is the
refusal behaviour exercised live rather than asserted. **A longer session
pinned to that head measured it**: 1080p, a client with motion, half the
frames reaching the encoder rather than being suppressed -- acquire 0.02 ms,
conversion 0.99, **encode 3.25 p50 against 4.36 at the ninety-ninth**, held
steady across a hundred and twenty seconds of reports. The conversion figure
is that head's compositor-fence cost and not the encoder's; the same client
on the other card converts in 0.16.

- **The log now names the encoder that took the stream.** The line naming a
  backend is printed before the third encoder is tried, so it names the one
  the display would have chosen; a refusal logged its reason and a success
  logged nothing, which left the shape of the display's registration as the
  only way to read which encoder served a live stream.

- **A set declares whole units of what the device says it accesses, not of
  the codec's smallest coding block.** The block is all the codec asks for,
  and a set declaring it decoded without a single error, reported the size
  it was asked for, and carried a picture whose every row was right except
  the last partial row of blocks. The device reads and writes whole units
  whatever the set says: one here accesses this codec 64x16 at a time where
  it accesses the other 16x16, which is why the other path's rounding to its
  own block size was enough and this one's was not. The conformance window
  carries the difference, as it already did there. Named regression test,
  watched to fail.
- **What found it was a pixel comparison, not a decode.** The stream decoded
  cleanly at every stage of being wrong. What said otherwise was the mean
  error against the source picture: twenty-five with a worst case of two
  hundred and thirty-nine while the bottom rows were wrong, against eleven
  once they were right -- which is what the other codec reads on the same
  path.
- **The codec's interface is enabled where a device advertises it and never
  required**, so a device that carries one codec keeps the encoder for it
  rather than losing the path entirely.
- **Both codecs' structures stand side by side in one recording** and the
  codec chooses which is chained. One recording per codec is two control
  flows to keep identical, and they would not stay identical.
- The device's own limits are read rather than assumed: the coding tree and
  transform sizes a set may declare, and the level it may name, come from
  what the device reports and are capped by it.

## 11.8: the third encoder streams to a live client

**A stock client and the predecessor's client both stream the third encoder
now** -- 60 fps, millisecond decode, zero loss -- after two defects only a
live decoder could surface. A refresh access unit opens with the parameter
sets, as on every other backend: the encode produces slices only, and a
decoder joining at a refresh without them reports a picture of no size at
all. And the collect is gated on the fence: the feedback query keeps the
previous submission's result until the next recording's reset executes on
the device, so an unguarded read collected instantly with stale bytes --
the dishonest collect one vendor's path is documented for, rebuilt here by
trusting the query. The impossible figure in the log, an encode time of
zero, is what named it.

- **The probe that should have caught both was measuring the wrong
  backend**: it named one unconditionally, which blocks the preference knob,
  so every "verified" run before the client was the open backend's. It
  follows the display now, dumps what the seat receives for an outside
  decoder to judge, and takes the codec from the environment.
- **A publish storm was seen three times and is not explained**: the loop
  fed a guest at thousands of frames a second with the window full and
  stale, one frame per client message. It has not recurred since, under
  identical and heavier load; the signature and the counters to catch it
  are recorded locally.
- **An idle keepalive pays the encode engine's wakeup on the integrated
  device** -- about 14 ms against 3.3 warm, codec-independent, the same
  power gating the conversion's poke already pays for one engine over. Not
  worth a fix: a still desktop has no viewer-visible latency and the first
  moving frame pays it once.

## 11.7: the third encoder reaches the stream, preferred by a knob

**`LOWLAT_VULKAN_ENCODE=1`, or the daemon's `--vulkan-encode`, prefers the
encoder that shares the capture's device.** Nothing reaches the boundary: the
boundary names meanings and this names a mechanism, the same rule that keeps
the conversion interface off it; the software encoder, when it lands, gets a
boundary field precisely because software-versus-hardware is a meaning. A
device that cannot serve the path -- an H.265 stream, no encode interface, a
copy standing between conversion and encode -- says why once and the stream
follows the display exactly as before; a device that can, and then fails
building, keeps its refusal. Verified through the real stream loop against
the live display: capture, conversion into the encoder's own ring, encode,
frames at a seat, with the idle path suppressing duplicates as on every
other backend.

- **The display pipeline gains the ring**: conversion targets that belong to
  the encoder, a registration that is the slot number, and an open that
  re-checks the node and size the ring was built against, because a display
  can move between the two calls.
- **The encoder carries the loop's trait**: predicted pictures through the
  written-slot path, a second in-flight picture refused as back pressure,
  the bytes path honestly unsupported.
- **The shared device is counted, not borrowed** (11.6): the display
  pipeline and the encoder each hold a clone of one underlying device, the
  last clone dropped releases it, and drop order between them stops
  mattering.

## 11.5: the conversion writes the encoder's ring


**The whole shared-device arrangement runs end to end**: an uploaded picture
is converted by the compute shader directly into the third encoder's own
slots, predicted pictures are encoded from them, and the decoded stream's
pixels match the source. Zero copies, zero validation messages.

- **A conversion target is named by its handles, not owned.** An owned
  allocation borrows itself; an encoder that lends its picture's planes
  builds the same reference from those handles. One two-plane image
  transitions once, not once per plane view.
- **The final layout is the reader's, paid by the writer.** A lent picture
  is handed over in the layout its encoder reads, in the conversion's own
  recording, so the encoder never touches a picture it does not own -- the
  written-picture entry point exists for exactly that, and the discard the
  ordinary entry performs on its first pass is what it must not do.
- **The pictures are shared between the writing and encoding families**, so
  no per-frame ownership transfer stands between the conversion and the
  encode.
- **The validation layer found a violation in the shipped conversion on the
  way**: the converter resets its one command buffer per submission, and its
  pool never carried the reset flag. Both drivers tolerate the violation
  silently, which is why it survived a live session; the pool carries the
  flag now.

## 11.4: a ring of source pictures on the third encoder

**The third encoder takes a ring of source slots**, fixed at build, where it
owned exactly one picture. One source serialises the pipeline -- nothing may
write the picture an encode in flight is reading -- and a ring restores the
discipline the other backends run under: the next picture is written while
the previous one encodes. The planes a conversion writes and the picture a
copy fills are now per slot, and a submit names the slot it encodes.
Verified on both devices with alternating slots, 300 pictures each, decoded
in full outside the project.

## 11.3: predicted pictures on the third encoder

**The third encoder codes predicted pictures now**, where every picture had
been a coded refresh and the keyframe request was discarded. A refresh is
coded on request and whenever there is nothing to predict from; everything
else predicts from the previous picture, and the collect reports the kind
honestly. Verified on both devices under the validation layer: 300 pictures,
exactly the two requested refreshes, the rest predicted, and the stream
decodes to the full count outside the project.

- **The reconstruction slots are layers of one image, never separate
  images.** The separate form is a capability, one implementation here does
  not report it, and programming it anyway sent the encode engine into
  unmapped memory and took the desktop down with it. The layered form works
  everywhere, which is also what the reference implementations fall back to.
- **Every scope names what stands in its slots.** The slot being written is
  opened inactive with its description chained all the same; the reference
  being read carries its own numbers in both the scope and the encode
  command.
- **The parameter sets come from the driver.** The bitstream carries slices
  only, so a stream opens with sets fetched from the session itself -- the
  driver is the authority on what it encodes against, which a hand-written
  copy cannot promise; an earlier backend's history says exactly how that
  goes wrong.
- The order count stays type 2, derived from the frame number -- the choice
  for a stream with no bidirectional pictures -- and the frame number wraps
  at the size the sequence set names while the derivation stays continuous.

## 9: capture (the conversion tier follows the device)

**Absent a name, the conversion interface follows the device**: the compute
interface where it exists, the GL fallback where it does not. The fallback
costs about a millisecond more per frame and reaches only the open encoder,
so it is a floor for old devices rather than a substitute: only the two
errors that mean "this device has no such interface" fall through, and any
other refusal keeps its reason instead of being masked by a slower tier.
Naming an interface (`--convert`, `LOWLAT_CONVERT`) pins it for measurement
exactly as before; the boundary still does not expose the knob, and a stream
started there follows the device. The tier chosen shows in the stream's own
log line either way. **The fallback arm is unexercised on this machine** --
both devices here carry the compute interface -- so its first honest test is
a device without one.

## 9: capture (the wakeup is paid by a poke)

**The open stack's conversion ran cold, and now it runs warm.** Measured with
a stock client before and after: the integrated device's convert stage was
2.35 ms, and captured-to-bitstream sat at 5.6 ms against 3.5 ms on the vendor
head. The 2 ms were not the encoder and not the display controller: the same
conversion costs 0.38 ms in a tight loop and 1.3 ms after a few milliseconds
of idle, with nothing else running. The integrated device powers its compute
block down between frames, and the first work after the gap pays the wakeup.
The stream now sends a partial-conversion poke a moment before the display's
next present, and the real conversion lands warm: **convert p50 0.41 ms,
captured-to-bitstream p50 3.7 ms** on the same head, vendor head unchanged.

- **The wakeup scales with the poke's work, up to a point.** One workgroup
  wakes half of it and a buffer fill wakes less than that; a sixteenth of the
  picture in each axis wakes it fully, which is a fraction of a percent of a
  frame. The figure is a named constant, measured rather than chosen, so a
  caller cannot drift from it.
- **The poke rides the vblank schedule.** The loop already knows when the
  next present is expected -- one observed period after the last event -- and
  sends the poke two milliseconds ahead of it, while it is waiting on the
  event anyway. A poke that lands late is a wasted few microseconds and
  corrects itself on the next event; a display without vblank events never
  gets one, and pays the wakeup once per tick as it always did.
- **The poke is the real pipeline, on purpose.** It writes a few blocks into
  a target the next conversion overwrites whole, and its digest is discarded;
  nothing in the frame path or the duplicate check sees it.
- **The wake curve is a probe now.** `cadence-probe` converts a pinned or
  live framebuffer at a chosen cadence with no encoder, which is what
  separated this cause from the encoder: the figure grew with the gap alone.

## 9: capture (the conversion leaves the acquire)

**The colour conversion no longer blocks the capture.** The acquire now
submits the conversion and returns; the loop collects the previous picture
while the device converts, then waits for the conversion on the far side. The
wait that used to sit inside the acquire stage is a stage of its own, and it
is normally already over when it is asked.

Measured through the stream loop with a real display, before and after, as
p50/p99 in milliseconds:

| | acquire before | acquire after | convert after |
|---|---|---|---|
| 2560x1440, 10-bit, vendor driver | 0.225 / 0.345 | 0.029 / 0.078 | 0.161 / 0.198 |
| 1920x1080, open stack | 1.79 / 2.47 | 0.034 / 0.091 | 2.35 / 2.78 |

The vendor head's acquire tail (max 0.74 ms) is gone from the frame path. The
open stack figure is the same total, now attributed: on the integrated device
the conversion genuinely waits for the encoder and the display controller it
shares memory with, and no placement of the submit changes that. The split is
what makes the two machines legible, and it removes one frame-path wait from
both.

- **The collect is bounded.** The conversion's fence orders behind the
  compositor's own in-flight render into the captured buffer, so it is not
  only this process's work: unbounded, it hands the loop's liveness -- and its
  teardown's -- to a display stack that may have stopped signalling. Past a
  hundred milliseconds the flight is left standing and the stream holds its
  last picture; a conversion that never finishes leaks its converter at
  teardown rather than hanging it.
- **Submit and collect, like the encoder.** The converter holds one fence and
  one command buffer; a submission in flight is refused rather than raced,
  and the collect waits only for work that was submitted before the loop's
  own collect, which is the whole point of the placement.
- **The per-frame driver churn came out with it.** The descriptor set is
  allocated once and rewritten per submission, the command buffer is reset
  and re-recorded, and the fence was already shared. What remains per frame
  is the view over the captured buffer, which names that buffer and so cannot
  be cached; the vendor head submits the conversion in 29 us.
- **The digest arrives with the collect.** The duplicate check moves one half
  a tick later, which is invisible: the decision it feeds is made on the same
  side of the submit as before. Suppression measured unchanged on a live
  stream: 590 of 600 frames on a still desktop.
- **The stage report grows a convert column**, and the stream log prints it
  beside the acquire it split from.

## 9: capture (the vblank paces the tick)

**The capture tick is phase-locked to the display's own present**, where the
display delivers vblank events: the frame clock caps the rate and the event
sets the phase, so the picture read is the one just presented rather than one
taken at an arbitrary moment inside a refresh, which is up to a frame interval
of avoidable age.

**It is not universal, and it is probed rather than assumed.** Measured on
both drivers on this machine: the open stack delivers the events (index zero,
ten milliseconds after the arm), and the proprietary NVIDIA driver refuses
the request outright with EOPNOTSUPP. A stream asks once at start - a refused
arm answers immediately, a working display answers within a refresh - and
falls back to the timer, which is exactly the pacing it had before. A display
whose vblanks stop while the picture is static (panel self-refresh) is
covered by a bound past the deadline, so it cannot hold the stream either.

- **The controller is named by index, not by handle.** The kernel numbers
  vblanks by the controller's position in the mode configuration, which is
  unrelated to the object identifier: the lit controller here is index zero
  while its handle is 79. The wrapper this project uses elsewhere cannot say
  either, so the arm is built from the raw type word. **The index rides in
  bits 1 to 5, above the relative flag**, and a named test holds the encoding:
  an index written into the low bits unshifted is not refused, it names the
  controller at half the value, which agrees with the right answer only at
  index zero -- the index this machine happens to scan out on.
- **The event is armed after the tick and consumed by the next.** One event
  per arm, so the arm is never doubled; a tick that fired on the bound leaves
  the arm standing, and the stream recovers the phase when the panel wakes.
- **The card descriptor is opened non-blocking**, and the loop polls it in
  its own wait rather than sleeping through the deadline; an empty queue is
  nothing rather than a wait.

## 9: capture (the second interface reaches the encoder)

**A session can now be streamed on either conversion interface, chosen by
name.** Both produce the same encoded stream from the same desktop, byte for
byte: 120 pictures, 95588 bytes, identical files.

- **Who allocates had to be inverted.** The first interface allocates the frame
  and lends each plane a view of it, which is what puts the colour plane exactly
  one luma plane in; the second can allocate nothing it is able to hand out. So
  the display device allocates one untiled region, the conversion imports it
  back as two planes at the offsets an encoder assumes, and the encoder imports
  that same descriptor. Three parties have to agree on where the colour plane
  starts, so the arithmetic is written once and all three read it. The device's
  own row length is not the width -- 2048 for a 1920-wide frame here -- so
  computing it from the width instead would have put the colour plane a tenth of
  a frame early.

- **The seam is an enum over the two halves, not a trait.** They are not
  interchangeable: only the first reaches both encoders, and hiding that behind
  one interface would hide the thing that decides which encoders a machine can
  use. Asking for the pairing that cannot work is refused at startup and says
  so, rather than being quietly served by the other interface.

- **Registration takes a descriptor and a layout** rather than a device and a
  frame, which is what lets both paths share it. A conversion target is generic
  now, because the second instantiation exists.

Selection is `--convert`, or the environment when the flag is absent. **Absent
means the first interface**, never whichever a machine happens to support.

The startup refusal also stopped attaching the device hint to every cause. It
had been asserting that a display and its encoder must share a device even when
the real reason was something else, which is a wrong cause stated with
confidence.

## 9: capture (a second conversion interface)

**The import and conversion now exist over the display stack's other
interface, chosen by name and never as a rescue.** It comes up on both drivers
on this machine, converts a picture byte for byte the same as the first one,
and computes the same summary.

- **It exists to be measured, not to rescue anything.** The requirements the
  first interface puts on a driver are newer than the ones here. A machine that
  refuses the first one says which part it is missing, and choosing the second
  automatically would replace that answer with a working pipeline of unknown
  provenance. Selection is by environment variable or by a name a caller
  parses, and a name that is neither is refused rather than quietly taken as
  the default.

- **One shader serves both.** The colour rules, the subsampling and the summary
  are what must not drift between them, so they are written once; what differs
  is nine lines of binding declarations behind a conditional. The compiled form
  is built with that conditional set and the text is handed to a driver with it
  unset, so neither can be changed without the other. Two locals had to be
  renamed: one driver's shading language reserves both names and the other
  compiler accepts them silently.

- **The check is that the two agree**, not that each looks right. The same
  picture goes through both and their summaries are compared, which is a
  comparison over every pixel either wrote. It was made to fail before it was
  trusted. Each interface is also checked against the transform computed from
  the definitions, on saturated colours rather than greys, since every luma
  matrix agrees on grey.

**The real framebuffer imports on both cards here**, which is the question the
backend exists to answer and the one an uploaded picture cannot reach. Between
them the two cards cover the cases that matter: one scans out ten bit under a
vendor tiling modifier at a pitch four times its width, the other eight bit and
untiled. The same shader converts both, and each result is a sharp desktop
rather than the diagonal smears a wrongly described tiling produces -- which is
the check, because an import described wrongly succeeds at every call and
returns a buffer of the right size.

**One difference between the interfaces is a trap rather than a detail.** The
first takes ownership of the descriptor it is handed; this one duplicates what
it needs and leaves it the caller's. Assuming either way round leaks a
descriptor a frame or closes one the driver still holds.

**Not yet seen: a capture that arrives as several buffers with differing
pitches.** One vendor's compression scheme does that, and neither card here is
in that state, so the multi-buffer half of the import is written and unexercised.

**Nothing is wired to an encoder yet, and that is where the two stop being
alike.** The first interface allocates the result itself and lends each plane a
view of it, so one descriptor leaves for either encoder. The second has the
driver allocate and does not say where, so handing those bytes on needs the
allocation to come from outside -- and the two encoders want different things
from it. That is the next piece.

## 9: capture (why a device is refused)

**A machine the capture path cannot run on now says which part it is missing.**
Three of the requirements were enforced by the driver rather than by us, so a
device that failed them reported a bare result code or, worse, the wrong cause
entirely.

- **A device that cannot say which display node it drives read as a display
  with nothing on it.** The node is matched against a property, and a driver
  that does not carry that property returns the same zeroes as a device driving
  a different node. Every device failing that way came out as "no device reports
  driving that display node", which sends the next person to look at the
  display. The two are now separated by counting the devices that answered at
  all, and only the second keeps that message.

- **Two device capabilities were requested without being asked for.** Naming an
  unsupported one at device creation is refused with a single result code
  covering the whole chain, so neither the extended storage formats the
  conversion writes through nor the two-plane image layout its target is built
  as could say it was the one missing. Both are queried first and refused by
  name.

Each of the three was made to fail before it was trusted: the first against a
driver on this machine that genuinely lacks the property, the other two by
inverting the test and watching the right name come back.

## 9: capture (duplicate suppression)

**A still desktop is sent once a second instead of sixty times.** The frame
rate on the wire was the display's own whatever was on it; it is now what
changed. Measured across one session as activity varied: 25.8 frames a second
on a quiet desktop, 58.6 on a busy one, **1.00 on a still one**, with the
stream thread down from 6.67 percent of a core to 1.78 and the wire from 0.16
Mbit/s to 0.03.

- **What was drawn decides, not which buffer it was drawn into.** The obvious
  key is the scanned-out buffer's identity, and it is wrong here: this
  compositor redraws in place rather than flipping on most frames, so keying on
  it would have discarded real frames on half to seven in ten of them. Measured
  three times against a display in use, every frame whose identity held steady
  had changed anyway.

- **The summary comes out of the conversion**, which already reads every pixel,
  so detecting a duplicate costs one small read rather than a second pass over
  the frame. Sixty-four invocations fold in shared memory and one atomic per
  group reaches device memory: about eight thousand a frame instead of half a
  million. Position is mixed in, because a sum and an exclusive-or are both
  blind to order and a picture that merely moved would otherwise match.

- **Every reason a duplicate is still owed is in one function**, because the
  failure it guards against is a screen that stops updating, and that is
  invisible to any test not looking for it: a refresh owed to a guest that
  joined or fell behind, a seat arriving or leaving, and a heartbeat. The
  heartbeat is not a cadence -- it is a bound on how long a mistake in that
  list can freeze a screen, and it is why the list being hand-written is
  survivable.

Confirmed on a live session: 590 of 600 frames suppressed, the ten that went
are exactly the heartbeat, no refreshes spent buying it, and the picture
updating the instant anything moved.

**What still runs every frame** is the display read and the conversion. Nothing
in the display interface announces a change, and the summary that detects one
is what the conversion produces, so the ioctl rate is unchanged at about four
hundred a second.

## 9: capture (idle cost)

**A host with a guest seated on an untouched desktop cost 6.67 percent of a
core and now costs 2.32.** Three changes, each measured before and after on
the same machine with the same guest.

- **One conversion fence, reset and reused.** It was created and destroyed per
  frame, which on one driver is two synchronous round trips to the card's own
  processor with a busy-poll attached to each. That was **0.45 ms of every
  conversion, 85 percent of the whole thing**, and three quarters of the
  acquire stage: p50 0.858 ms to 0.222. The other driver is unchanged, which is
  the point -- the two now cost the same shape.

  **The obvious test does not catch a missing reset.** A readback submitted
  after the conversion is ordered behind it on the same queue, so it hides the
  fault; the exposure is an encoder reading the image from elsewhere. The
  invariant is asserted at the submit instead, and the reuse path has a test of
  its own, which it did not have while every test converted exactly once.

- **The spin at the end of a sleep is 100 us, not 200.** At sixty landings a
  second the larger margin cost 0.92 percent of a core against 0.33, for
  landings that are the same to within a fraction of a microsecond at p50 and
  p95. What it looked like it was buying -- a controlled tail -- belongs to the
  scheduler: preemption puts p99 and the maximum in the same place at either
  margin, and at no margin at all. It cannot go much below 100, because the
  sleep overshoots by 45 to 55 us and a margin under that never runs.

- **The encoder is polled only while it holds something.** A pass with nothing
  in flight has nothing to collect, and asking anyway is a driver round trip
  for an answer that cannot have changed: **748 wakeups a second became 121**.
  A stopped encoder is still caught, because one that stops answering is one
  whose submissions never come back.

Pacing is untouched where it matters: the interval holds at 16.666 ms and an
uncapped stream still free-runs.

**What remains is the duplicate itself.** The frame rate is still exactly the
display's on a desktop nobody is touching, and the display re-read stays at
frame cadence because nothing yet knows the picture did not change.

## 9: capture (reopened for measurement)

**The stream now says how often it asks for a refresh, and why.** A refresh
costs a picture with no history behind it, so a stream sending them often
spends its rate on recovery rather than on content -- and nothing in the frame
rate or the encode time says so. The report carries a line per window:

```
stream: refresh over 600 frames sent=0 no_slot=0 too_large=0 no_room=0 \
        asked=0 reinit=0 starved=0
```

- **The causes are separated because they call for different answers.** A
  starved frame pool is back pressure, a refused room test is a peer that
  cannot keep up, and a reinitialization is neither. `starved` counts pool
  exhaustion whether or not a refresh was granted, because the throttle
  otherwise hides the pressure behind it.

- **`sent` is not the sum of the rest, deliberately.** Several causes landing
  in one frame interval produce one refresh, and the throttle refuses most of
  what is asked for, so `sent` is what reached the encoder and the others are
  what wanted to. **`reinit` is the one cause the throttle never sees**, which
  makes it the only one that can run away, and the only reason it has a column
  of its own.

The first thing it measured: seventeen consecutive windows across two encoder
backends and two displays, with a guest seated throughout, reported no refresh
at all. A frame rate pinned at exactly the display's own, on a desktop nobody
was touching, is not recovery traffic -- it is the loop having no way to know
the picture did not change.

## 10: audio (closed 2026-08-23)

**The gate is green, all six.** The long-run item passed well past its bar:
thirty minutes first, then **two hours of continuous streaming** with no
drift and nothing audible. That the source is the clock is what makes it
true -- there is no second clock on this side to drift against.

**Two defaults changed with it, both because a host that streams a
desktop streams its sound.**

- **Sound is captured, with nothing to turn it off.** The daemon's
  opt-out is gone: there was no reason to run a host without sound, and
  a machine that has no sound server already says so once and streams
  anyway -- which is the same answer switching it off would have given.
  The boundary keeps its switch, because an application embedding this
  library may have reasons a daemon does not.

- **The speakers at the desk are silenced by default**, with
  `--no-host-mute` to keep them. Somebody hosting their own machine is
  in the room with it, and hearing the session played back at them is
  the surprising default rather than the quiet one. The tap is ahead of
  the mute wherever the device allows it, so a guest hears everything
  either way -- and on a device where it would not, the host declines
  and says so rather than silencing every guest.

**The guest microphone**, off by default, written and not yet heard from
a real peer.

- **Accepting it and announcing it are one switch.** A peer sends no
  microphone audio until it is told the host will take it: told nothing
  it keeps its own microphone muted, however it is configured itself. So
  a host that merely listened would receive silence and look broken from
  both ends, and the setting that decodes is the same setting that says
  so on the wire -- set it and every connected peer is told, clear it and
  they are told to stop.

- **It arrives on the control channel, not the audio one.** Sound to a
  guest and sound from one share nothing but a rate: this is a virtual
  device, one opcode carrying several kinds, told apart by both header
  arguments together and refused when the body's own kind disagrees with
  them. **Its encoding byte spells uncompressed the opposite way** the
  audio channel's codec tag does, which is close enough to swap without
  noticing until a listener hears static.

  It costs a packet every ten milliseconds -- around 193 kB/s in two
  fragments, roughly 200 fragments a second, reliable and ordered and
  head-of-line with the guest list and the cursor. **That cost is why
  taking one is a decision** rather than something switched on by
  polling for it.

- **The application receives samples, never a codec**, on a poll of its
  own: a hundred packets a second sharing the event queue would evict
  the events. It refuses at once when the microphone is not accepted,
  rather than spending the caller's timeout on sound that by
  construction cannot come. What a host does with the samples is its
  business -- creating a capture device in somebody's session is not a
  shared library's decision.

- **Heard from a real client**, 11,266 packets with nothing dropped,
  nothing refused and nothing panicked. Getting there took one fix that
  is worth writing down: **the selectors are the header's second and
  third arguments, not its first two.** The first is the declared body
  length, as it is for an application message, and reading the selectors
  a position early found the length where a selector should be. The host
  matched nothing and passed over every packet as another device's --
  **in silence**, because passing over another device is what this
  opcode asks of a host that does not implement it. A client sending a
  hundred packets a second and a client sending nothing looked exactly
  the same from here.

  What separated them was a **census on the receive path**: the first
  sighting of each opcode a peer sends, with its arguments, once per
  kind. One line printed the true header and the mistake was obvious.
  It stays, because any path that ignores unknown input in silence needs
  one.

- **The decoder is the first thing here to read bytes a peer chose, and
  it is treated that way.** What it may produce is bounded from a
  constant rather than from the packet's own length, and a panic is
  caught, counted, and the state it unwound through thrown away rather
  than decoded against again.

  **Not precautionary: seventeen of forty thousand random payloads
  panicked** rather than erroring. And the failure is **a property of
  the decoder's accumulated state rather than of any one packet** -- the
  same bytes decode cleanly on a fresh decoder -- so the regression test
  replays a sequence, the fuzz target feeds sequences, and the count of
  contained panics is its own number in the guest line beside the
  ordinary refusals.

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

## 8: public C ABI (in progress)

**Changed**

- **`lowlat_host_begin_p2p` takes the port to start the bind at.** It walks from
  there exactly as the configured base does, and the port it reached still comes
  back in the credentials, so the two are an in and an out pair rather than one
  value asked and assumed. Zero asks for the configured base, which is what an
  application with no port of its own to manage passes; one that has a mapping
  on the gateway, a rule on the firewall or a pool to allocate from has it for a
  reason, and none of those survive this library choosing for it.

  **Credentials stay an output.** The reference generates them inside the SDK on
  every path -- the client's attempt, the host's approval, and the certificate
  the browser path needs -- and hands them out. An application-supplied key
  would make an integrator's random number generator the session's, and both
  directions key from that one value.

- **An exhausted port walk takes any port rather than refusing the guest.** A
  host whose range is occupied can still serve: nothing advertises the port that
  was asked for, because every candidate is built from the address the socket
  reports. Refusing turned a busy range into a guest who could not connect at
  all, which is the harsher failure and is not what a peer does.

**Fixed**

- **The readiness marker is raised after the candidates, not before.** A
  captured exchange puts every real candidate on the wire first and the marker
  last, a second after the answer: it reads as "that is all of mine, now yours"
  rather than "I am ready, begin". It had been moved ahead of them on reasoning
  about unblocking the peer sooner, which the capture does not support.

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
