# The application toolkit

A vendored copy, **modified**. It was a submodule until 2026-09-19; it is a tree now so that
its GL renderer can take a picture that lives in device memory, which upstream's cannot.

| | |
|---|---|
| Upstream | https://github.com/parsec-cloud/libmatoya |
| Branch | `stable` |
| Commit | `87ad84d48baabff6b668368e49ed7d114e5308dc` (2026-08-05) |

Everything is upstream's at that commit, byte for byte, except what is listed below and two
omissions: `deps/bin/` (a Windows shader-compiler binary the Linux build does not use; the
Linux makefile calls the one on the path) and `.github/` (upstream's own automation).

## What is changed, and why

The renderer draws a picture the library hands out as a device handle
([docs/10-client.md](../../docs/10-client.md) section 4, [docs/06-api.md](../../docs/06-api.md)
section 3b): an opaque descriptor of the device's own compute runtime, with a plane layout of
offsets and pitches. Upstream's GL renderer refuses every hardware frame
(`mty_gl_valid_hardware_frame` returns false); the change makes it import one.

| file | change |
|---|---|
| `src/matoya.h` | `MTY_HardwareFrame`: the descriptor, its allocation's number and size, and the planes' offsets and pitches, which is what `MTY_WindowDrawQuad` takes as the image when `MTY_RenderDesc.hardware` is set on GL |
| `src/gfx/gl/glproc.h`, `glproc.c` | the external-memory entry points and their constants, resolved when the context has them and left null otherwise |
| `src/gfx/gl/gl.c` | `mty_gl_valid_hardware_frame` answers whether the context imports descriptors; `mty_gl_render` with `hardware` imports the descriptor once per allocation number (a duplicate of it, since the import takes ownership), keeps it as a buffer, and fills the plane textures from that buffer at each plane's offset and pitch -- a device-side transfer, nothing read by the CPU. The staging textures are remade on a change of internal format, not only of upload format: upstream keys the check on the upload format, which the eight and sixteen bit layouts share, so a picture that changed depth mid-stream kept its old texture and the old picture stayed on the screen |

The websocket reader hands out whole messages only (RFC 6455 section 5.4). Upstream's reads one
frame per call and returns it as the message: a text frame without its final bit set came out
as though whole, cut short, and the continuation frames after it were dropped as unknown. A
signaling message lost that way arrived at the demo as text that did not parse.

| file | change |
|---|---|
| `src/unix/linux/ws.c` | `MTY_WebSocketRead` gathers a fragmented message into the caller's buffer frame by frame, each under the reader's own one-second frame deadline, until the frame with the final bit, and answers pings, notes pongs and takes a close that arrive between the fragments; `ws_read` reports the final bit and reads a control frame's payload (at most 125 bytes) into a buffer of its own, so one arriving mid-message leaves the fragments gathered so far alone. A continuation of nothing is dropped, as upstream drops it; a new message begun before the last one's final fragment is an error. `examples/client/ws-check.c` (`make -C examples/client check`) is the check: a loopback server sending a message whole, one in three fragments with a ping and a pong between them, one whose last fragment comes 200 ms late, a binary one, an empty one and a close |

Nothing else is touched: no other renderer, no other platform code, no shader. A diff against
the upstream commit lists exactly the files above.

## Licensing

MIT, Snowcone Ltd., the notice in `LICENSE` and at the head of each source file. `cargo deny`
does not see C sources, so this is the compliance record. The change carries the same licence.
