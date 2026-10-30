# The application toolkit

A vendored copy, **modified**. It was a submodule until 2026-09-19; it is a tree now so that
its GL renderer can take a picture that lives in device memory, which upstream's cannot.

| | |
|---|---|
| Upstream | https://github.com/parsec-cloud/libmatoya |
| Branch | `stable` |
| Commit | `87ad84d48baabff6b668368e49ed7d114e5308dc` (2026-08-05) |

Everything is upstream's at that commit, byte for byte, except what is listed below and two
omissions: `deps/bin/` (a Windows shader-compiler binary; both makefiles call the one on the
path) and `.github/` (upstream's own automation).

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

The Direct3D 11 context is made on the GPU the application names. The library hands pictures
out as textures shared in the legacy form, which open only on a device of the GPU they were
made on, and upstream makes its device on the system's default adapter with no way to name
another.

| file | change |
|---|---|
| `src/matoya.h` | `MTY_SetGFXAdapter(uint64_t luid)`: the adapter the next Direct3D 11 context is made on, named by its locally unique identifier as one value, low part first; zero for the default |
| `src/windows/gfx/d3d11-ctx.c` | the context's device is made on that adapter, found by its identity through the factory, with the unknown driver type an explicit adapter takes; an identity no adapter has any longer, or zero, makes it on the default adapter as before, the miss logged |

A context whose device is lost is made again, as upstream means it to be; upstream's remake
could never succeed. A flip swap chain outlives its last reference until the immediate
context is cleared and flushed (the `ID3D11DeviceContext::Flush` documentation, "Deferred
Destruction Issues with Flip Presentation Swap Chains"), and the renderer's objects keep the
old device alive, so the new swap chain was refused on the window (`E_ACCESSDENIED`) and the
window drew nothing again. A loss found while taking the back buffer, rather than at the
present, was not acted on at all.

| file | change |
|---|---|
| `src/windows/gfx/d3d11-ctx.c` | the context's free clears and flushes the immediate context after releasing the swap chain; a removed, hung or reset device found by `GetBuffer` or `CreateRenderTargetView` remakes the context as one found at the present does, once the buffer is released; a remake that fails is tried again at a later surface, at most every 250 ms, and a failed remake inside the resize path no longer leaves a null swap chain to be read |

The Windows build takes its Vulkan shader compiler from the path, as the Linux build does,
rather than from `deps/bin/`, which is not carried: no executable is kept in this tree.

| file | change |
|---|---|
| `makefile` | the two shader rules call `glslangValidator` rather than `deps\bin\glslangValidator`; the compiler is Khronos' glslang, upstream's own copy of which is 11.10.0 |

An application that reads its controllers itself can keep the toolkit off them. Upstream reads
every controller whatever the application asks: on Windows it registers for their raw input,
writes an output report of its own on a PlayStation pad's first report (the motors stopped, a
lightbar colour of its choosing) and polls XInput; on Linux it opens their input nodes.

| file | change |
|---|---|
| `src/matoya.h` | `MTY_APP_FLAG_NO_CONTROLLERS`: no controller is opened, read or written, and no controller event is raised |
| `src/windows/appw.c` | with the flag, the main window registers for no controller's raw input, so the HID layer learns of no device and writes to none, and XInput is not polled |
| `src/unix/linux/x11/app.c` | with the flag, the input-node layer is not made |

A picture drawn by handle on Direct3D 11 is opened once per allocation, as the GL renderer
imports its descriptor once. Upstream takes the planes' shared handles as the image and opens
each again, with a new view, at every draw; a renderer on one GPU drawing to a monitor on
another then grew by some 14 KB a draw -- 0.84 MB a second at sixty draws -- its processor time
climbing with it.

| file | change |
|---|---|
| `src/matoya.h` | `MTY_HardwareFrame.texture`: each plane's texture by its shared handle, which with the frame's `id` is what the Direct3D 11 renderer takes as the image when `MTY_RenderDesc.hardware` is set |
| `src/windows/gfx/d3d11.c` | up to eight imports kept, each a frame's planes opened on the renderer's device with their views, found by `id`, the oldest replaced by a new one; a draw takes references on the import's views and opens nothing; `mty_d3d11_valid_hardware_frame` reads the frame, and answers yes for none, every device opening the shared textures of its own GPU |

A picture uploaded from memory is read where each plane lies, at each plane's own row length.
Upstream takes one image with the planes in sequence and derives every plane's row length from
one width, so a picture whose planes lie apart, or whose chroma rows are aligned on their own --
as the system's decoder on macOS hands them out -- was drawn wrong, or had to be copied into
that shape first.

| file | change |
|---|---|
| `src/matoya.h` | `MTY_RenderDesc.planes` and `MTY_RenderDesc.pitches`: each plane's own address and row length in bytes, taken where set |
| `src/gfx/fmt.h` | `fmt_reload_textures` uploads a plane from its own address at its own row length where the description gives them, and from the image in sequence as before where not; every renderer's upload goes through it |

The Metal renderer draws a picture the library hands out as a device handle on macOS: the
picture's IOSurface, each of whose planes Metal imports by its index. Upstream's Metal renderer
refuses every hardware frame (`mty_metal_valid_hardware_frame` returns false).

| file | change |
|---|---|
| `src/matoya.h` | `MTY_HardwareFrame.surface`: the picture's `IOSurfaceRef`, which with the frame's `id` is what the Metal renderer takes as the image when `MTY_RenderDesc.hardware` is set |
| `src/unix/apple/gfx/metal.m` | up to eight imports kept, each a frame's planes made from its surface by plane index at the size the format and the chroma give them, found by `id` at the same size and format, the oldest replaced by a new one; an import's textures are drawn from for the draw alone and never become the staging textures an upload writes into; `mty_metal_valid_hardware_frame` answers yes for a frame with a surface, and for none |

The macOS window keeps the application's cursor and hears every key let go. Upstream's gives
an RGBA cursor a size in points equal to its pixel count, so a 2x screen draws it at twice its
size, and its hotspot in those same points lands at twice its offset; it only `set`s its cursor,
which AppKit replaces with its own arrow on almost any move and keeps; and AppKit's dispatch
never hands a window the release of a key let go while Command is held, so that key stayed down.

| file | change |
|---|---|
| `src/unix/apple/macosx/app.m` | an RGBA cursor given no size (`MTY_AppSetCursorSize`) is sized at its pixels over the backing scale of the first window's screen, its hotspot divided alike; the content view lays the app's cursor over its bounds as a cursor rect, invalidated at every change of cursor, so AppKit sets that one wherever it would set its own; the event pump hands a key release that carries the Command flag to its window as well, before AppKit's dispatch drops it |

Nothing else is touched: no other renderer, no other platform code, no shader. A diff against
the upstream commit lists exactly the files above.

## Licensing

MIT, Snowcone Ltd., the notice in `LICENSE` and at the head of each source file. `cargo deny`
does not see C sources, so this is the compliance record. The change carries the same licence.
