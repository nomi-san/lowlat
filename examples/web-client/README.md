# A guest in a browser

The smallest page that streams from `lowlatd`: signaling, the browser pipe
([01 §14](../../docs/01-protocol.md)), the session declaration, and the picture
decoded by the browser's own decoder and drawn with WebGL2. No sound, no
input, no pointer: it exists to prove the pipe on two browser families and to
put numbers on the status line. Everything runs on the page's main thread.

## Running it

```sh
npm install
npm run check      # tsc --noEmit
npm run serve      # bundles, watches, serves http://127.0.0.1:8000/
```

Open the page in Chrome or Firefox, paste a **client** session token and the
host's peer id, and connect. The page must be served from a loopback address:
the decoder API and the peer connection are only available to a secure
context, and `127.0.0.1` is one. On another machine, forward the port over ssh
(`ssh -L 8000:127.0.0.1:8000 host`) rather than serving over the LAN.

The rate and frame rate fields are sent as the application's video
configuration once the first picture has arrived; the host applies them to the
running stream and logs `changed the stream`. HEVC is offered where this
browser can decode it and the host codes it for the room.

## What is where

| File | |
|---|---|
| `src/signaling.ts` | the service, in the client role: offer, answer, candidates, close |
| `src/transport.ts` | the peer connection, three pre-agreed channels, the synthesized answer |
| `src/control.ts` | the 13-byte control header and the messages this page sends |
| `src/client.ts` | one session: the connect flow, liveness, the control channel |
| `src/video.ts` | keyframe detection off the bitstream, the decoder, the quad |
| `src/main.ts` | the form and the status line |

## Three things worth knowing before writing another page

**No description crosses signaling; each side writes its own.** Only the
credential triple travels. The answer this page hands its browser names the
host as the active side of the handshake, the stream port, and the largest
message the host accepts. A candidate must not be offered to the browser
before that answer is set -- one family refuses it -- so candidates that
arrive first are held.

**The host learns that our candidates are complete from a marker, not from
silence.** The readiness candidate is sent once gathering finishes, with a cap
in case the reflexive server never answers. Without it the host sends its
full-length checks only to directly routable addresses.

**A browser gets the bitstream and nothing else on the video channel.** No
dimensions, no keyframe flag, no codec: the page reads the unit types off the
bitstream to mark keyframes, and takes the size from the decoded picture. When
the decoder needs a fresh reference chain, opcode 13 with the same flags and
the reinitialization argument set asks the host for a keyframe.
