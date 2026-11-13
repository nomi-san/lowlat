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

### Through a relay

The page takes its ICE servers from `ice-servers.json` beside `index.html`
when the file is there -- the `{ "iceServers": [...] }` a TURN service's
credential call returns, or a bare list of the same entries -- and otherwise
uses one public reflexive server. The file holds credentials, so git ignores
it; short-lived ones are written into it again when they expire, and a reload
reads them. The idle line says how many URLs it found and how many are relays.
With a relay among them the **relay** box is offered: ticked, the session goes
through a relay and nothing else; unticked, the browser is given the file's
reflexive servers alone, because offered both it once moved a running direct
session onto a relay's longer path. The status line names the path in use:
`via relay/udp`, `relay/tcp` or `relay/tls`, or `host`, `srflx` or `prflx`.

A relay far from both ends carries less than the direct path, and a host that
sends more than the path takes queues its control messages behind the
picture: through a relay at about 95 ms the default 30 Mbit/s fell silent on
the control channel within 7 to 20 s while the picture still arrived, and
5 Mbit/s held. Ask for a rate the relayed path carries.

## What is where

| File | |
|---|---|
| `src/signaling.ts` | the service, in the client role: offer, answer, candidates, close |
| `src/transport.ts` | the peer connection, three pre-agreed channels, the synthesized answer |
| `src/control.ts` | the 13-byte control header and the messages this page sends |
| `src/client.ts` | one session: the connect flow, liveness, the control channel |
| `src/video.ts` | keyframe detection off the bitstream, the decoder, the quad |
| `src/reorder.ts` | an H.264 sequence set given the reorder depth it leaves unsaid |
| `src/main.ts` | the form, the ICE servers and the status line |

## Things worth knowing before writing another page

**No description crosses signaling; each side writes its own.** Only the
credential triple travels. The answer this page hands its browser names the
host as the active side of the handshake, the stream port, and the largest
message the host accepts. A candidate must not be offered to the browser
before that answer is set -- one family refuses it -- so candidates that
arrive first are held. A reflexive candidate of the host's is written with
the unspecified related address (`raddr 0.0.0.0 rport 0`): Firefox drops a
reflexive line without one, silently, and with it the host's address beyond
its translation.

**A failure before the session opens is not the end of the attempt.** Through
a relay the host's private address fails at once, because the relay refuses
it, and the host's routable candidates can arrive after its own readiness
marker; Chrome reports the connection failed and then connects when they come.
The five-second deadline ends an attempt that never opens.

**The host learns that our candidates are complete from a marker, not from
silence.** The readiness candidate is sent once gathering finishes, with a cap
in case the reflexive server never answers. Without it the host sends its
full-length checks only to directly routable addresses.

**A browser gets the bitstream and nothing else on the video channel.** No
dimensions, no keyframe flag, no codec: the page reads the unit types off the
bitstream to mark keyframes, and takes the size from the decoded picture. When
the decoder needs a fresh reference chain, opcode 13 with the same flags and
the reinitialization argument set asks the host for a keyframe.

**A sequence set that does not state its reorder depth costs four pictures.**
A browser's decoder holds pictures back to the level's worst case when the
H.264 sequence set says nothing of reordering: 120 to 130 ms at 30 frames a
second from a stock host whose stream never reorders. Where the set's picture
order counts follow frame_num (type 2, under which output order is decode
order) and it carries no bitstream restriction, the page adds one saying so
before decoding, and the same stream decodes in 0.3 ms. `lowlatd` states the
depth itself, so its sets pass untouched, as does any set of another type,
which may reorder.

**Firefox does not connect to a stock host on Windows**, directly or through a
relay. That host, the client of the handshake, sends its second flight in
three datagrams and each only after a datagram from the browser. Chrome
retransmits its own flight on a short timer, which draws the rest out at a
cost of two round trips; Firefox, once the first piece is in, waits for the
rest, and neither side sends again. Firefox streams from `lowlatd`.
