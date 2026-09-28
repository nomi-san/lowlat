# AMF headers

Upstream copy of a subset. Do not edit any file under `include/`; to change the pin, replace
the files from upstream at the new tag and regenerate whatever is generated from them.

| | |
|---|---|
| Upstream | https://github.com/GPUOpen-LibrariesAndSDKs/AMF |
| Tag | `v1.5.0` |
| Commit | `afed28d37aca1938da2eedc50599bb3535a987ec` (2025-10-29) |
| Interface version | 1.5.0 (`AMF_FULL_VERSION`) |
| Runtime | `amfrt64.dll`, installed with AMD's display driver |

Every header is byte-identical to upstream's `amf/public/include` at that tag: each file's blob
here is upstream's blob, checked when they were added. The layout is the installed one,
`AMF/core` and `AMF/components`, which is how distributions and other consumers include them;
the headers include one another by relative paths, so either layout works unedited.
`LICENSE.txt` is upstream's text, with the line endings this repository stores text in.

**Only what the decoder needs is carried**: the include closure of the factory
(`core/Factory.h`), the version (`core/Version.h`), the decoder component
(`components/VideoDecoderUVD.h`) and its capability query (`components/ComponentCaps.h`), 22
headers, which compile as C and as C++. The encoders, capture, the pre-processing and analysis
components, the software codec wrappers, the Vulkan and D3D12 interop headers, the samples and
the documentation are not carried; the host's encoder takes its headers from the same tag when
it is planned.

## Licensing

MIT, Advanced Micro Devices, with the notice carried in each header and the full text, with
upstream's notice on codec standards, in `LICENSE.txt`. `cargo deny` does not see headers, so
this is the compliance record.

## Why this version

The runtime is the display driver's, and the documented initialization passes the runtime's
own version, read with `AMFQueryVersion`, to `AMFInit`. So the header does not raise a driver
floor by itself, as a version stamped into every structure would; a feature the runtime lacks
is refused where it is asked for. Everything the decoder uses is here -- its low-latency mode
arrived at 1.4.26 -- and the next release, 1.5.2, changes nothing in these headers the decoder
calls: it adds a surface interface for mapping to the processor, and encoder options. **Raise
the pin only for a feature we actually call.**

## What is generated from this

Nothing yet. W1.6, the vendor's decoder on AMD, generates its bindings from these, committed
rather than produced at build time, as the other vendored interfaces are.
