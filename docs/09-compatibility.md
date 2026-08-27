# 09 - Hardware compatibility

What a machine needs to host, which parts meet it, and how each answer was arrived at.

**This document is about hosting.** A guest needs nothing installed and no particular hardware;
see [§6](#6-guests).

---

## §1 Three stages, and a host needs all three

Hosting is a chain, and the weakest link decides. A machine that can encode but cannot be fed
is not a machine that can host.

| Stage | What it needs |
|---|---|
| **Capture** | a display device offering universal planes **and atomic modesetting**, and a scanout buffer this process may export |
| **Conversion** | a compute interface that can import that buffer with its tiling described: Vulkan 1.1 plus the external-memory and format-modifier extensions, **or** desktop OpenGL 4.3 |
| **Encode** | one of the three backends in [05 §4](05-host.md) |

**The floor is usually set by conversion rather than by encode**, which is the least intuitive
thing here and the reason this document exists. Hardware that has encoded video for a decade
can still be out of reach, because the picture has to get to the encoder before the encoder
matters.

**Codec scope is [00 §D7](00-overview.md): H.264 and HEVC, 8-bit 4:2:0.** A part that encodes
only VP9 or only AV1 does not host, whatever else it can do.

---

## §2 How to read the tables

Every row carries how it was established, because these are not equally solid:

- **measured** -- run on that part, on this machine, end to end.
- **from the driver** -- read out of the driver's own source, which is what actually decides at
  runtime. Reliable for what a driver will and will not offer; it cannot tell you the part
  works.
- **derived** -- follows from a documented interface floor. Weakest, and marked so it can be
  challenged.

Nothing here is marked measured unless a stream came out of that part and decoded.

---

## §3 AMD

Driver gates read from the shipped open stack, version 25.0.7.

| Generation | Capture | Conversion | Encode | Hosts? |
|---|---|---|---|---|
| TeraScale | no: legacy driver, no atomic | none | none | **no** |
| GCN 1-2 (Southern/Sea Islands) | no by default: legacy driver claims them | GL only | VAAPI, H.264 | **not as shipped** (see below) |
| GCN 3-4 (Tonga, Fiji, Polaris) | yes | **GL only** | VAAPI, H.264 and HEVC on Polaris | **yes** |
| Vega, Raven (GFX9) | yes | Vulkan or GL | VAAPI | **yes** |
| RDNA 1 | yes | Vulkan or GL | VAAPI, and Vulkan Video | **yes** |
| RDNA 2 | yes | Vulkan or GL | VAAPI, and Vulkan Video | **yes, measured** |
| RDNA 3 | yes | Vulkan or GL | VAAPI, and Vulkan Video | **yes** |
| RDNA 4 | yes | Vulkan or GL | VAAPI; **no Vulkan Video** | **yes** |

**Two things here are worth stating plainly.**

**The OpenGL conversion is load bearing, and not for the reason it was built.** It was added as
a fallback for parts without a compute interface. What it actually covers is **GCN 1 through
Polaris**, because the open Vulkan driver offers the format-modifier extension only from GFX9
onward -- so on those parts the Vulkan conversion is not merely slower, it is absent. That is a
large installed base and it is served entirely by the fallback tier. *From the driver.*

**GCN 1 and 2 are refused before any of this**, and by the kernel rather than by us. Those
parts are claimed by the legacy display driver on a stock distribution, and that driver offers
no atomic modesetting, so capture cannot open the device at all. The modern driver can claim
them, but only if asked at boot; its support for both generations defaults to off wherever the
legacy driver is present. **This is a boot parameter and not something a host can fix**, and it
is the reason those rows say "not as shipped" rather than "no". *From the driver.*

**Vulkan Video is not the newest-hardware option it sounds like.** It covers RDNA 1 through
RDNA 3 and **stops before RDNA 4**, which the open driver has not implemented -- so the newest
cards fall back to VAAPI like everything else. It is also gated on the encoder firmware
version, not only the part. *From the driver.*

---

## §4 Intel

| Generation | Capture | Conversion | Encode | Hosts? |
|---|---|---|---|---|
| Sandy Bridge, Ivy Bridge, Bay Trail | yes | **none** | VAAPI, H.264 | **no** |
| Haswell, Broadwell | yes | Vulkan or GL | VAAPI, H.264 | **yes** |
| Skylake through Tiger Lake | yes | Vulkan or GL | VAAPI, H.264 and HEVC | **yes** |
| Arc, and Xe discrete | yes | Vulkan or GL | VAAPI through the **low-power** entry point | **yes, measured** |
| Meteor Lake and newer | yes | Vulkan or GL | VAAPI | **yes** |

**The floor is Haswell, and the encoder is not what sets it.** VAAPI encodes H.264 as far back
as **Sandy Bridge, 2011** -- further back than the vendor's own dispatch library reaches on
this platform, and about as far back as it reaches on Windows. Bay Trail, the Celeron N and J
parts, encodes through the Ivy Bridge path. Those parts are still out, because **neither
conversion tier exists below Haswell**: there is no Vulkan driver for them at all, and their
OpenGL tops out at 4.2, one version below the compute shaders the conversion is written as.
*Encode support from the driver; the conversion floor derived from both interfaces' own
version floors.*

So on old Intel the answer is not "the encoder is too old". It is that we cannot get the
picture to an encoder that would happily take it. **Closing that would mean a third conversion
tier written against an older interface**, and it is not planned.

**Discrete Arc needs the low-power entry point.** VAAPI names two ways to reach an encoder, and
the shader-driven one was removed from Intel's discrete parts. Asking for it by name reported
no encoder at all on a card that encodes both codecs. Fixed 2026-08-26; both codecs verified on
an A380. **Meteor Lake and newer merge the two names back**, so the middle of the range is the
only part of it that was ever affected. *Measured.*

**It is a live path now, not just a correct one.** An A380 driving a display has captured,
converted, encoded and streamed both codecs to a client. Getting there cost three faults, and
every one of them was a parameter set that disagreed with the device it configured: an entry
point named by hand, a transform tree declared deeper than the hardware codes, and a
quantiser-delta granularity of zero against hardware that quantises to the smallest coding
block. **None was device-specific and none is guarded by a device check.** They were invisible
on the other vendor's driver, which rewrites the set to match what it coded, and fatal here,
which writes exactly the bytes it is handed. A second vendor is what turned three latent errors
into visible ones. *Measured.*

**The vendor dispatch library adds nothing here.** On this platform it is a client of VAAPI --
it links it and calls it -- so it cannot reach hardware VAAPI cannot, and the runtime that
ships covers **only the newest generations**, where VAAPI covers all of them. This is the
reverse of the Windows arrangement, where that library ships inside the graphics driver and is
the only way in. *Measured: the runtime's own link table and imported symbols.*

---

## §5 NVIDIA

| Generation | Capture | Conversion | Encode | Hosts? |
|---|---|---|---|---|
| Pre-Kepler | no | none | none | **no** |
| Kepler through Pascal | driver-dependent | Vulkan | NVENC, H.264; HEVC from Maxwell 2 | **driver-dependent** |
| Turing and newer | yes | Vulkan | NVENC, both codecs | **yes, measured** |

**Two floors apply and the higher one wins.** The encoder interface is pinned to a version
whose minimum driver is R455, which is old enough not to bind. What binds instead is the
**format-modifier extension**, which capture and conversion both need and which the vendor
driver gained much later. **That minimum is not established** -- it is the one number in this
document nobody has checked -- so the Kepler-to-Pascal row says driver-dependent rather than
giving a version. *Encoder floor from the vendor's own documentation; the binding floor
unverified.*

**Display mode setting must be enabled in the driver** or there is no display device to capture
from at all. This is a module parameter and off by default on some distributions.

**The OpenGL conversion cannot serve this vendor.** It names the chroma plane by a format
spelling the open stack uses and this driver does not, and the two spellings are byte-swapped
rather than synonyms. It does not matter in practice: parts old enough to want the fallback are
served by the compute tier anyway.

---

## §6 Guests

**Nothing to install, and no hardware requirement worth stating.** A guest decodes H.264 or
HEVC, which every platform with a client has done in hardware for a decade and can do in
software otherwise. A guest that cannot decode what it is sent is the one party that can tell,
and it reports that itself ([05 §6.2](05-host.md)); the host is not able to detect it and does
not try.

The one thing a guest's hardware does decide is **which codec the session uses**, which is
settled once from what every seated guest declares ([05 §6.1](05-host.md)) and never adapted
afterwards.

---

## §7 Windows

Planned, after the Linux capture gate ([07 §10](07-platforms.md)). The stages map across but
the constraints do not:

- Capture is through the platform duplication interface, which has no equivalent of the atomic
  modesetting requirement and works on any display driver including the basic one a virtual
  machine gets.
- **The operating system ships a software H.264 encoder**, which this platform does not, so a
  machine with no usable hardware encoder still has a path there and has none here.
- The vendor dispatch library ships inside the graphics driver and reaches back roughly a
  decade, which is what makes it the right answer there and the wrong one here (§4).

---

## §8 What a software encoder would and would not fix

**It would not widen this matrix much**, which is why it is not the next thing built.

A machine with a display and a driver good enough to capture and convert almost always has a
hardware encoder, because the same generations gained both. The parts this document excludes
are excluded at **capture or conversion**, and a software encoder sits downstream of both. The
exceptions are narrow: a virtual machine with no passthrough, and Intel parts below Haswell
that would still need a conversion tier that does not exist.

**There is also no software encoder to reach for on this platform.** The system provides none,
the widely available one is under a copyleft licence this project cannot link or load, and the
permissively licensed alternatives are either a dependency the distribution may not carry or,
in pure-Rust form, several times too slow at 1080p60 and short of the live bitrate control and
on-demand keyframes this design requires. **Software encode on this platform is a real piece of
work with a small payoff**, and it is recorded here so the question does not get re-asked as
though it were free.

There is no software path for the conversion either, in the sense that matters: the compute
tier does run on a software Vulkan device -- measured at 2.5 ms a frame at 1080p, which is
comfortably inside budget -- but **software Vulkan offers no video encode at all**, so it
closes half the gap and not the half that was open.

---

## §9 Open questions

| Question | Status |
|---|---|
| The vendor driver version that first offers the format-modifier extension | **unverified**, and it sets the real NVIDIA floor (§5) |
| Whether GCN 1-2 host correctly when the modern driver is asked for at boot | untested; no such part here |
| Whether the low-power entry point is better than the shader one where both exist | never measured; the order prefers the shader one so nothing already served changes |
| Whether the low-power path costs latency against the shader one | **answered 2026-08-27**: it does not. Intel discrete encodes 1080p desktop content in about 3.7 ms, roughly 1 to 1.5 ms behind the vendor backend, and the reference encoder on the same device reads 3.2 ms |
| A conversion tier below OpenGL 4.3 | not planned (§4) |
