# VPL headers

Upstream copy of a subset. Do not edit any file under `include/`; to change the pin, replace
the files from upstream at the new tag and regenerate whatever is generated from them.

| | |
|---|---|
| Upstream | https://github.com/intel/libvpl |
| Tag | `v2.16.0` |
| Commit | `778a66d6c6537f08eabb91955dbbf1bce3812894` (2025-12-08) |
| Interface version | 2.16 (`MFX_VERSION`) |
| Runtimes | the dispatcher `libvpl.dll` over the current runtime, which Intel's display driver installs; the older runtime (`libmfxhw64.dll`) for the parts the current one does not reach |

Every header and `LICENSE` is byte-identical to upstream's `api/vpl` and root at that tag:
each file's blob here is upstream's blob, checked when they were added. The layout is the
installed one, `vpl/`, which is how the headers name one another.

**Only what the decoder and the encoder need is carried**: the include closure of the session,
decode and encode calls (`mfxvideo.h`), the dispatcher that loads and chooses an implementation
(`mfxdispatcher.h`), the implementations' capabilities (`mfximplcaps.h`) and the memory
interface a decoder's surfaces are taken through (`mfxmemory.h`), nine headers, which compile
as C. The umbrella header, the C++ wrapper, and the encode statistics, external rate control,
camera, JPEG, multi-view, VP8, protected-content and adapter-query headers are not carried; one
is added from the same tag when a call needs it.

The older runtime is reached through the same declarations: the decode calls and their
structures kept their names when the interface was renamed, so one set serves both, and a
runtime is asked only for what the version it reports has.

## Licensing

MIT, Intel Corporation, with the notice carried in each header and the full text in
`LICENSE`. `cargo deny` does not see headers, so this is the compliance record.

## Why this version

The interface grows by appending: its structures carry reserved space and versioned extension
buffers, and a session reports the version its runtime implements, so a runtime older than
the header is asked only for what its version has. So the header does not raise a driver floor
by itself. The next release, 2.17, changes nothing in these headers the decoder or the encoder
calls: it adds a bitstream memory interface and pre-processing options for encoding, neither
called here. **Raise the pin only for a feature we actually call.**

## What is generated from this

The types and constants the decoder and the encoder use, `crates/drivers/src/ffi/windows/vpl.rs`,
by `scripts/gen-vpl-bindings.sh`, committed rather than produced at build time, as the other
vendored interfaces are.
