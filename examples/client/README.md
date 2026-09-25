# The client demo

A window that shows a host's desktop: the library decodes, this presents.
Pure C on the application toolkit (`third_party/matoya`), one file for the
session and the window and one for the signaling service, which the library
does not speak ([docs/06-api.md](../../docs/06-api.md) section 4).

    make
    LOWLAT_PEER=<the host's peer id> LOWLAT_SESSION=<a session token> make run

`make` builds the toolkit from its tree, the library in release, and the demo;
the toolkit's build needs the shader compiler and the ALSA, PNG and JPEG
headers. `make LOWLAT_LIB=<dir>` links a library already built there instead,
with a run path of `../lib` relative to the binary, which is how the build
workflow packages the demo beside the client-only library. The demo's first
line names the library it loaded, its version and its halves.
The token is what `lowlat-login` produces; the peer id is the host's, as its
service prints it. `LOWLAT_DEVICE` names a render node for the decoder (the
first that decodes by default), `LOWLAT_DECODER` picks `auto`, `open`,
`vendor` or `none`, and `LOWLAT_SERVER` names the signaling service. The
decoders this machine can open are printed at start, one numbered row each
with what it decodes, and `LOWLAT_DECODER_INDEX` picks a row by number.
`LOWLAT_STUN` names reflexive servers, `host:port` separated by commas, up to
four: without one a direct attempt offers only this machine's own addresses,
which a host behind its own translator cannot answer. `LOWLAT_RELAY=host:port`
makes the attempt a relay attempt through that relay
([docs/03-connectivity.md](../../docs/03-connectivity.md) section 7), with
`LOWLAT_RELAY_USER` and `LOWLAT_RELAY_PASS` its credential, which is handed to
the library and never printed: the relayed address is all the demo offers,
every check goes through the relay, and the line at establishment says the
path is relayed and where. A relay that does not answer, refuses the
credential, or lets the allocation go ends the session with its own outcome.
`LOWLAT_HEVC`, `LOWLAT_10BIT` and `LOWLAT_444` are the preferences the
attempt starts with, masked by what the decoder takes; Ctrl+Shift+C cycles
them live. `LOWLAT_HANDLE` asks for pictures as device handles, which the
toolkit's renderer imports and draws with nothing copied through this
process; only a decoder whose row says "handles" opens that way.

`LOWLAT_PAD_RAW` sends a DualShock 4 or a DualSense as its own report
([docs/10-client.md](../../docs/10-client.md) section 8): the demo reads the
pad's raw node itself, feature reports first, and puts what the host's device
is written back on it; the toolkit's events for those pads are dropped
meanwhile. Off, they go as the sixteen-button pad every host takes. An
established host reads the report only in a mode its owner set, and this
library's hosts read it as it is, so which peer is on the other end is the
application's to know: this one is told. `LOWLAT_PAD_RAW=only` drops every
controller the toolkit reports as well. On the host's own machine the
virtual pads the host makes are controllers to the toolkit and would go back
as states, making more of them; the demo tells them by the location the
host writes on them and never sends them (a real pad of the same identity
on that machine is kept out with them, since the toolkit names a controller
by identity alone). `LOWLAT_PAD_TRACE` prints every report either way.

Three more are for measuring rather than watching. `LOWLAT_FPS` asks the
host for that frame rate through the application protocol once the first
picture is in, the way a settings panel does. `LOWLAT_PRESENT_HZ` caps how
often a new picture is taken: the cached one is still drawn on every refresh,
so on a display faster than the cap the stream is above the presentation
rate with both clocks still the display's. `LOWLAT_SECONDS` leaves cleanly
after that long, as closing the window does. `LOWLAT_PEER` may name several
hosts, separated by commas: they are visited in turn on the one handle, each
for `LOWLAT_SECONDS`, the session with one left and an attempt made to the
next -- a reconnect, the same host named twice included.

Once a second a line goes to stdout with the presentation cadence as numbers:
presents and polls in the second (equal without a cap), new pictures, repeats
(a poll with no new picture) and skips (pictures published and never shown, a
newer one having arrived), the codec, the decoder's decode and read-back times
and the host's own encode time, the reader's lag, the round trip, the video
rate, and the process's resident set. The same figures go in the window's
title bar. Nothing is judged; the figures are what the deferred decisions of
[docs/10-client.md](../../docs/10-client.md) are decided on.

Keyboard, mouse and pads go to the host as the toolkit reports them; the
rectangle the picture is drawn into is told to the library, which maps
positions into the picture, so a resized window or a picture shown at its
own size aims the same. When the host captures the pointer the demo confines
and hides its own, and warps it back where the host says on the way out.
The chords are the demo's and are never sent, `Ctrl+Shift` and a letter:
`I` grabs the keyboard (and lets it go), so the desktop's own keys -- the
Windows key, its task switch -- go to the host while the window has the
focus; `W` toggles fullscreen; `D` asks the host to stream its next output,
through the same application protocol a settings panel uses; `F` switches
the picture between stretched to the window and shown at its own size; `R`
lets go of a captured pointer (and takes it again); `C` cycles the video
preferences. A bare Windows key is not sent while the keyboard is not
grabbed, because the desktop here takes it and the host would be left
holding the modifier; it reaches the host on chords, and whole once grabbed.

The key table `keys.h` is generated by `scripts/gen-client-keys.py` from the
toolkit's own Linux key map and the kernel's usage table. **An Xbox pad's X
and Y are read back by letter**: the kernel's Xbox driver names the two upper
face buttons by letter where the PlayStation driver names them by position,
and the two namings are each other crossed; the toolkit reads every pad by
position, so the demo looks up the pad's driver in sysfs and crosses the two
bits back for a pad the Xbox driver holds. The toolkit's controller events
arrive only while the window is in front (its rule on every platform), which
is why a pad it reads goes quiet when the window is not, while a pad read
raw keeps reporting -- the same split an established client has between its
mapped pads and its raw ones.

Sound and the cursor come with their phases.
