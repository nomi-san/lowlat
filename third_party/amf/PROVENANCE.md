# AMF headers

Upstream copy of a subset. Do not edit any file under `include/`; to change the pin, replace
the files from upstream at the new tag and regenerate whatever is generated from them.

| | |
|---|---|
| Upstream | https://github.com/GPUOpen-LibrariesAndSDKs/AMF |
| Tag | `v1.5.3` |
| Commit | `57ee0cfb0e1a08d9041f77435653794c1b012be0` (2026-10-16) |
| Interface version | 1.5.3 (`AMF_FULL_VERSION`) |
| Runtime | `amfrt64.dll`, installed with AMD's display driver |

Every header is byte-identical to upstream's `amf/public/include` at that tag: each file's blob
here is upstream's blob, checked when they were added. The layout is the installed one,
`AMF/core` and `AMF/components`, which is how distributions and other consumers include them;
the headers include one another by relative paths, so either layout works unedited.
`LICENSE.txt` is upstream's text, with the line endings this repository stores text in.

**Only what the decoder and the encoders need is carried**: the include closure of the factory
(`core/Factory.h`), the version (`core/Version.h`), the decoder component
(`components/VideoDecoderUVD.h`), its capability query (`components/ComponentCaps.h`), and the
H.264 and HEVC encoder components (`components/VideoEncoderVCE.h`,
`components/VideoEncoderHEVC.h`, with the `components/PreAnalysis.h` they include), 25 headers,
which compile as C and as C++. The AV1 encoder, capture, the pre-processing components, the
software codec wrappers, the Vulkan and D3D12 interop headers, the samples and the documentation
are not carried. The three encoder headers were added on 2026-10-19, and all 25 replaced from
the tag above on 2026-11-17, each blob checked against upstream's there.

## Licensing

MIT, Advanced Micro Devices, with the notice carried in each header and the full text, with
upstream's notice on codec standards, in `LICENSE.txt`. `cargo deny` does not see headers, so
this is the compliance record.

## Why this version

The runtime is the display driver's, and the documented initialization passes the runtime's
own version, read with `AMFQueryVersion`, to `AMFInit`; the library passes the lesser of that
and these headers'. So the header does not raise a driver floor by itself, as a version stamped
into every structure would; a feature the runtime lacks is refused where it is asked for.
Everything the decoder uses is here -- its low-latency mode arrived at 1.4.26 -- and everything
the encoders use, and a runtime of 1.4.37 answers all of it.

**The pin is upstream's latest release**, raised from 1.5.0 on 2026-11-17, when drivers in use
carried runtimes 1.5.2 and 1.5.3. Between the two tags the carried headers gained interfaces
derived from the surface, the buffer and the context (mapping to the processor, shared surface
resources), a colour-matrix property on the decoder, one surface format, and the Windows order
of two property-information entries in the component's C table; one encoder property, its memory
type, changed its name. No method the tables call moved and no name the library writes changed:
each asserted slot was read from both sets of headers.

## What is generated from this

Nothing. The tables the decoder and the encoders are called through are declared by hand in
`crates/drivers/src/amf.rs`, only the methods used, each at the slot these headers give it, which
a test asserts; the property names and values used are written there as constants.
