# The client demo

A window that shows a host's desktop: the library decodes, this presents.
Pure C on the application toolkit (`third_party/matoya`), one file for the
session and the window and one for the signaling service, which the library
does not speak ([docs/06-api.md](../../docs/06-api.md) section 4).

    make
    LOWLAT_PEER=<the host's peer id> LOWLAT_SESSION=<a session token> make run

`make` builds the toolkit from its tree, the library in release, and the demo.
The token is what `lowlat-login` produces; the peer id is the host's, as its
service prints it. `LOWLAT_DEVICE` names a render node for the decoder (the
first that decodes by default), `LOWLAT_DECODER` picks `auto`, `open` or
`none`, and `LOWLAT_SERVER` names the signaling service.

Three more are for measuring rather than watching. `LOWLAT_FPS` asks the
host for that frame rate through the application protocol once the first
picture is in, the way a settings panel does. `LOWLAT_PRESENT_HZ` caps how
often a new picture is taken: the cached one is still drawn on every refresh,
so on a display faster than the cap the stream is above the presentation
rate with both clocks still the display's. `LOWLAT_SECONDS` leaves cleanly
after that long, as closing the window does.

Once a second a line goes to stdout with the presentation cadence as numbers:
presents and polls in the second (equal without a cap), new pictures, repeats
(a poll with no new picture) and skips (pictures published and never shown, a
newer one having arrived), the codec, the decoder's decode and read-back times
and the host's own encode time, the reader's lag, the round trip, the video
rate, and the process's resident set. The same figures go in the window's
title bar. Nothing is judged; the figures are what the deferred decisions of
[docs/10-client.md](../../docs/10-client.md) are decided on.

Picture only. Input, sound and the cursor come with their phases.
