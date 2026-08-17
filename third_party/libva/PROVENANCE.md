# libva headers

Upstream copy. Do not edit anything under `include/`; to change the pin, replace the whole
directory from upstream and regenerate the bindings.

| | |
|---|---|
| Upstream | https://github.com/intel/libva |
| Tag | `2.20.0` |
| Commit | `907b2b5405ca1091b4360bf35060e143bd704b62` (2023-09-14) |
| Interface version | 1.20.0 |
| Runtime soname | `libva.so.2`, `libva-drm.so.2` |

Every header is byte-identical to that tag, verified by checksum, **except one**.
`va_version.h` does not exist upstream: it is generated from `va_version.h.in` by the build
system, and the copy here was produced by the same substitution the build performs, with the
interface version the tag declares. `compat_win32.h` is omitted because nothing includes it on
this platform.

The layout is the **installed** one rather than the source tree's: upstream keeps `va_drm.h`
under `va/drm/` and installs it beside the rest, and every header that includes it spells it
`va/va_drm.h`. Matching what is installed is what lets these headers stand in for a system
copy without editing an include line.

## Licensing

MIT, Intel Corporation, with the notice carried in each header and the full text in `COPYING`.
`cargo deny` does not see headers, so this is the compliance record.

## Why this version

Unlike the codec interface next door, this one has no version stamp inside its structures and
no matching driver floor, so the pin is a much softer choice. Two things still argue for not
taking the newest:

- **The structures grow by appending**, and buffer sizes are passed explicitly at every call,
  so a driver older than the header reads the prefix it knows. Compiling against a header
  older than the installed runtime is therefore the safe direction, and compiling against a
  newer one is the risky one.
- **This tag is what the current long-term-support distributions carry**, so it is a version
  every target actually has rather than one they will have.

Entry points are resolved individually at runtime, so a call added after this tag is simply
absent rather than a link failure, and the backend falls back. That is why the pin can be
conservative at no cost.

## What is generated from this

`crates/encode` generates Rust bindings for the core interface, its DRM display, and the
H.264 and HEVC encode structures. As next door, the bindings are committed rather than built,
so no build machine needs a C toolchain, and no function is declared: the libraries are opened
at runtime.
