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

Once a second a line goes to stdout with the presentation cadence as numbers:
presents and new pictures in the second, repeats (a present with no new
picture) and skips (pictures published and never shown, a newer one having
arrived), the decoder's decode and read-back times, the reader's lag, and the
process's resident set. Nothing is judged; the figures are what the deferred
decisions of [docs/10-client.md](../../docs/10-client.md) are decided on.

Picture only. Input, sound and the cursor come with their phases.
