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
| `src/gfx/gl/gl.c` | `mty_gl_valid_hardware_frame` answers whether the context imports descriptors; `mty_gl_render` with `hardware` imports the descriptor once per allocation number (a duplicate of it, since the import takes ownership), keeps it as a buffer, and fills the plane textures from that buffer at each plane's offset and pitch -- a device-side transfer, nothing read by the CPU |

Nothing else is touched: no other renderer, no platform code, no shader. A diff against the
upstream commit lists exactly the files above.

## Licensing

MIT, Snowcone Ltd., the notice in `LICENSE` and at the head of each source file. `cargo deny`
does not see C sources, so this is the compliance record. The change carries the same licence.
