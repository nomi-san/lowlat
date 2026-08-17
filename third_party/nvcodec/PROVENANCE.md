# nvcodec headers

Unmodified upstream copy. Do not edit any file under `include/`; to change the pin, replace
the whole directory from upstream and regenerate the bindings.

| | |
|---|---|
| Upstream | https://github.com/FFmpeg/nv-codec-headers |
| Tag | `n11.0.10.3` |
| Commit | `625b3199e94db49e3bb0dc797fc4cffbf7115d81` (2023-09-28) |
| Video Codec SDK | 11.0.10 |
| Minimum driver | Linux 455.28, Windows 456.71 |

The five headers are byte-identical to that tag, verified by checksum at the time they were
added. `README` is upstream's.

## Licensing

Each header carries its own permission notice, which is what makes redistribution here
possible; there is no separate licence file upstream. `nvEncodeAPI.h`, `dynlink_cuviddec.h`
and `dynlink_nvcuvid.h` are NVIDIA's, 2010-2020. `dynlink_cuda.h` and `dynlink_loader.h` are
2016 and belong to the upstream project. All five grant use, copy, modification and
redistribution on the condition that the notice travels with the file, which it does.

`cargo deny` does not see these, because they are headers rather than crates. The notices
above are the compliance record.

## Why this version, and not the newest

**Every encoder struct carries a version stamp built from the header it was compiled
against**, and the compatibility is one-way: a newer driver accepts an older stamp, an older
driver rejects a newer one with an invalid-version status on every call, naming nothing. So
the header a binary is built against silently sets that binary's **minimum driver version**.

Pinning to the newest available header would have raised our floor to a driver newer than
several current distributions ship, refusing hardware that is otherwise perfectly capable.
Every feature the encoder backend uses -- the low-latency tuning info and the P1 to P7 preset
family, encoder-owned CUDA streams, non-reference P frames, live reconfiguration, CUDA array
input, HEVC including its 10-bit format, and the non-blocking bitstream lock -- is present at
11.0, and each one was checked in this header before the pin was chosen. Nothing above 11.0 is
reachable from our codec scope; the first thing that would require moving is AV1.

**Raise the pin only for a feature we actually call**, and record the new driver floor here
when you do.

## What is generated from this

`crates/encode` generates Rust bindings from `nvEncodeAPI.h` and `dynlink_cuda.h`, allowlisted
to the encode surface, committed rather than produced at build time so that no build machine
needs a C toolchain. The decode headers are vendored because they are part of the upstream
unit and a decode path is expected later; nothing generates from them today.

Regeneration is a manual step, documented alongside the generated file.
