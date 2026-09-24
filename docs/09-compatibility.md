# 09 - Hardware compatibility

What a machine needs to host, which parts meet it, and how each answer was arrived at.

**This document is about hosting.** A guest needs nothing installed and no particular hardware;
see [§7](#7-guests).

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

**Codec scope is [00 §D7](00-overview.md): H.264 and HEVC, 4:2:0, eight bits by default and ten
where a guest asks and the part can.** A part that encodes only VP9 or only AV1 does not host,
whatever else it can do. **Depth does not decide whether a part hosts**, only what it can be
asked for; §5a is that column.

---

## §1a Colour, measured

Every row here was run on the part named, not read from a support matrix. Two committed probes
reproduce it: one asks each interface which colour profiles it will encode, the other measures
what a chroma layout costs against another on the same content.

| | HEVC 10-bit 4:2:0 | HEVC 4:4:4 | H.264 above 8-bit 4:2:0 |
|---|---|---|---|
| AMD, RDNA2 | **yes**, both interfaces | **none, at any depth** | no profile exists |
| Intel, Arc | **yes** | encode-capable, low-power entry point only, packed AYUV/XYUV/Y410 surfaces | no profile exists |
| NVIDIA, Turing and newer | **yes** | encode-capable, three planar planes as input | **the driver refuses it** |

Three things follow, and each cost time to establish:

- **Ten-bit 4:2:0 is universal among the parts that host at all**, which is what makes it v1
  and settled per session rather than a capability a host has to advertise carefully.
- **4:4:4 is absent from one vendor entirely** -- not a driver gap, no profile in either
  direction, confirmed through two independent interfaces and on more than one generation. That
  is why it is out of v1 (D7) rather than merely unimplemented.
- **H.264 above 8-bit 4:2:0 does not exist to be used.** No profile on the open stack, an
  outright refusal from the vendor encoder, and the third interface's own headers cannot name
  one. **Where a part will encode H.264 4:4:4 it will not decode it**, on the same chip, which
  makes it an encode-only format and therefore not a format.

**What 4:4:4 wants as input is settled by probe, not by guess.** The open stack's 4:4:4
surfaces are packed at both depths -- AYUV or XYUV at eight bits, Y410 at ten -- while the
vendor interface reads three planar planes, so the conversion needs one body per layout, which
is what [the plan, Phase 11.6](impl-plan.md) sizes. Both import over DRM prime, so the
zero-copy path survives. **The third interface codes no full chroma at all, and that is now
what it reports rather than what a pairing discovers.** It has no device where it would
serve: it refuses every full-chroma profile on one vendor, offers no encode queue on another,
and on the third the profile exists but a shader may not write the picture the encoder reads
-- which is the whole reason that interface is offered. So its capabilities carry the answer,
a stream that has settled on full chroma passes over it before a device is asked, and the
session continues on a backend that can code it instead of ending on one that cannot.

**A guest's decoder is the other half and is not in these tables**; see [§7](#7-guests).

---

## §2 How to read the tables

Every row carries how it was established, because these are not equally solid:

- **measured** -- run on that part, on this machine, end to end.
- **from the driver** -- read out of the driver's own source, which is what actually decides at
  runtime. Reliable for what a driver will and will not offer; it cannot tell you the part
  works.
- **derived** -- follows from a documented interface floor. Weakest, and marked so it can be
  challenged.
- **not run** -- nothing has been tried. Said out loud rather than left blank, because a row with
  no grade reads as one that passed.

Nothing here is marked measured unless a stream came out of that part and decoded. The same rule
applies to the desktop side in [§6](#6-desktop-environments): one desktop is measured and the
rest are not run.

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

**Vulkan Video does not cover this vendor at all.** The open driver hides its encode
extensions behind an environment flag, and with them enabled the device offers no rate control
beyond turning it off -- which is the one congestion actuator this design has -- and no encode
completes. Measured on a discrete part, both codecs. So the open stack is not a fallback here,
it is the only path.

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
| Arc, and Xe discrete | yes | Vulkan or GL | VAAPI through the **low-power** entry point; **no Vulkan Video** | **yes, measured** |
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

## §6 Desktop environments

**The three stages above are below the display server and do not care which desktop is
running.** Capture reads the display device, conversion and encode are the graphics stack; a
session that draws anything at all is a session this can capture. What the desktop decides is a
separate list, and it is short:

| what | what it needs from the session | without it |
|---|---|---|
| absolute input landing in the right place | the desktop's layout: which outputs exist, and where each sits | the axis spans the captured picture alone, which is right on one output and wrong on two |
| following a display that appears or moves | the same, watched rather than asked | the mapping is right about the desktop as it was when the display opened |
| a turned display streaming upright | the same events, which carry the output's transform | the picture streams on its side, declared flat |
| a guest's request for a size or a turn | the compositor's output-management protocol, asked by the session | refused with a reason |
| the screen not blanking mid-session | an inhibitor the session honours | the desktop blanks while somebody is watching it |
| the clipboard, either direction | something that can own a selection and outlive the copy | copied text does not cross |
| the attention chord | whatever the desktop binds the combination to | the keys are typed and nothing answers them |

**Only the first two decide whether hosting is correct.** The rest are features that are absent
rather than broken, which is the rule the session channel is built to
([07 §5.1](07-platforms.md)).

### The mechanisms, and where each one exists

| capability | mechanism | grade |
|---|---|---|
| layout, and changes to it | `zxdg_output_manager_v1` on the session's own socket | **measured** on KDE Plasma Wayland, including an output appearing and going away |
| the output's transform | `wl_output` on the same socket, which every compositor offers | **measured** on KDE Plasma Wayland, a head turned and turned back |
| a display mode or turn on request | `kde_output_management_v2` on the session's own socket | **measured** on KDE Plasma Wayland, a mode in 75 ms and a turn in 9 ms, and from a stock client end to end |
| idle inhibitor | `org.freedesktop.ScreenSaver` on the session bus | **measured** on KDE Plasma Wayland |
| clipboard | `org.kde.klipper` on the session bus | **measured** on KDE Plasma Wayland, and **it is that desktop's own interface** |
| attention chord | none: the keys are typed on the guest's own keyboard | **measured** on KDE Plasma Wayland, which answers with its leave dialog |

**The clipboard is the one with no portable mechanism**, and it is named here rather than in a
footnote. The interface used is a single desktop's. The portable-looking route is a privileged
selection protocol -- `wlr-data-control-unstable-v1` and its successor -- which some compositors
offer and at least one major one does not; the alternative on those is a client holding a
selection, which needs focus that a background program does not have. A session that offers
nothing announces that it offers nothing and the host answers honestly.

**An X11 session answers none of the first two.** The layout is read over a Wayland socket and a
session without one is a session that does not answer, so absolute input spans the picture
alone -- correct on one output, wrong on two. The clipboard and the inhibitor are on the session
bus and are not Wayland's, so a desktop offering them on X11 offers them here too.

### Other desktops, by what each one offers

Nothing below is measured; it is what each stack offers against the rows above, and it says
what would have to be built for each.

| | GNOME on Wayland | Xfce, and any X11 session |
|---|---|---|
| capture | scanout, as on any Wayland compositor | scanout where the X server presents through the display device: the open drivers do, one vendor's does not (§3 to §5 and [07 §2](07-platforms.md)) |
| layout and orientation | `zxdg_output_manager_v1` and `wl_output`, offered | **no Wayland socket**: a RandR query, not built; absent, the axis spans the picture |
| idle inhibitor | the shell serves `org.freedesktop.ScreenSaver` | the desktop's screensaver or power manager serves it |
| clipboard | **none**: no privileged selection protocol to speak of | an X selection client, not built |
| mode and turn | `org.gnome.Mutter.DisplayConfig` on the session bus, a second mechanism behind the same capability | RandR, not built |
| login screen | GDM is a Wayland compositor session: captured like any session, with a socket for the layout | X: the vendor-driver case |

So GNOME needs one mechanism (the mode request over its bus) to reach parity minus the
clipboard, and its login screen is the good case. X11 needs three session-side mechanisms
and, on one vendor's driver, a capture route that does not exist below the session.

**Two arrangements to avoid on X11**, both measured 2026-09-10. An X server on one vendor's
own driver presents through no plane the display device reports, so there is nothing to
capture and the pointer plane is empty too. And a panel driven by one card and rendered by
another -- one X server, the second card as an output sink -- **hung the scanning-out card
and the desk with it** after four minutes of capture: the conversion on that card stopped
completing, then the card's own display commit waited on a fence that never came, and the
X server blocked behind it until a reboot. Whether the capture caused it or exposed it is not
known; the configuration is not supported either way.

### What is not run

Everything above is measured on **one** desktop. Nothing else has been run at all, and none of
it is marked otherwise. What to run, per desktop, is four things:

| check | how |
|---|---|
| can this machine host | `cargo run -p lowlat-host --example can-host` |
| does the layout read, and does it follow a change | `cargo run -p lowlat-capture --example layout-watch`, then plug a display in or start a virtual one |
| does a turn arrive | the same, then turn a display in the desktop's own settings; the transform on that output changes |
| does the session take a mode request | `cargo run -p lowlat-capture --example mode-probe -- <connector> <WxH> [transform]`, as the person logged in |
| does the screen stay awake | `cargo test -p lowlatd -- --ignored the_screen` |
| does the clipboard cross | `cargo test -p lowlatd -- --ignored the_desktop_clipboard` |

The first is the only one that gates hosting. The other three each answer for one feature, and a
"no" from any of them is a session that says so rather than a host that misbehaves.

---

## §7 Guests

**Nothing to install, and no hardware requirement worth stating.** A guest decodes H.264 or
HEVC, which every platform with a client has done in hardware for a decade and can do in
software otherwise. A guest that cannot decode what it is sent is the one party that can tell,
and it reports that itself ([05 §6.2](05-host.md)); the host is not able to detect it and does
not try.

The one thing a guest's hardware does decide is **which codec and which depth the session
uses**, settled from what every seated guest declares ([05 §6.1](05-host.md)) and never adapted
to a seat that arrives later.

**Ten-bit decode is as widely available as HEVC decode itself** -- every part in §3 to §5
decodes it, and so does software -- so a guest asking for it is not asking for something
exotic. **4:4:4 decode is not**: one vendor decodes none of it, and a client meeting a 4:4:4
stream there falls back to software or fails. The client library of [10](10-client.md) asks
for it only where its decoder was verified to decode it (§7a), so what it declares is what it
takes.

**A guest that cannot decode what it is sent is the one party that can tell**, and it says so
by disconnecting with a decode status rather than by degrading quietly. A host reads that
status and reports it; it cannot detect the condition itself and does not guess.

### §7a The client library, on Linux

The above is any guest. **A guest running the client library of [10](10-client.md) does have
a requirement**: a hardware decoder reached through the open interface or the vendor's, or
(*2026-09-21*) an LGPL build of the codec library on the machine, because the library
decodes in hardware, in software through that one library, or refuses
([10 §5.1](10-client.md)). What it needs of the part is the decode entry point for the
stream's profile -- H.264 High, HEVC Main, HEVC Main 10, and for full chroma the
range-extensions profile -- which on the open stack every part that encodes in §3 to §5
also has, and many older ones besides. **Full chroma decode is declared only where the
library has verified the surface it reads back**: the vendor interface, the software
decoder, and (*2026-09-22*) the open stack where the driver decodes into a layout the
library reads -- planar, or the packed eight-bit and ten-bit layouts a discrete Intel part
hands out -- which a part listing the profile with another layout does not get asked for. **The software row exists only past a licence check**: the
distribution this is developed on ships a GPL build, which is refused, so on a stock
distribution the row is absent until an LGPL pair is placed beside the application or
named in the environment -- or the library is built with the `gpl-libavcodec` feature,
which takes the distribution's pair and is the builder's own combination under the GPL's
terms ([10 §5.1](10-client.md)); with it, every committed clip decodes bit-exact through
the distribution's 7.1 pair too.

| Part | Interface | H.264 High | HEVC Main | HEVC Main 10 | HEVC 4:4:4, 8 and 10 | Handle | Evidence |
|---|---|---|---|---|---|---|---|
| AMD RDNA 2 (discrete) | VA-API, open stack 25.0.7 | yes | yes | yes | no | no | **measured**: twenty-one clips from four encoders, every picture the reference decoder's; a 2560x1440 stream at 120 pictures a second decodes in about 2 ms and reads back in about 2 ms |
| Other AMD from Fiji (GCN 3) | VA-API | yes | yes | Polaris and later | no | no | from the driver's published profile tables; the same code path, not run here |
| Intel from Broadwell | VA-API | yes | Skylake and later | Kaby Lake and later | not asked for | no | from the driver's published profile tables; not run here |
| Intel Arc A380 (DG2, discrete) | VA-API, the vendor's media driver 25.2.3 | yes | yes | yes | yes, both depths | no | **measured** (*2026-09-22*): every committed clip bit-exact, the second codec's ten-bit and full-chroma ones included -- the four full-chroma clips through the range-extension structures and the driver's packed layouts unpacked in the read-back, the first open-stack part to verify that path. The card sat on a PCIe link one lane wide, and the read-back is the link: at 2560x1440 a picture costs 8 ms to read back as NV12 and 16 ms as P010, the decode itself 0.9 to 1.5 ms, and from 60 pictures a second up the driver serialises the next decode behind the read-back, so the reader manages 58 a second of H.264 and 31 of HEVC ten-bit whatever the host sends; asked for 30 it keeps pace with the reader at most one message behind, asked for more it falls behind by the difference until the receive ring is full, which is the design's stated limit for a decoder slower than its stream ([10 §4.1](10-client.md)) |
| NVIDIA RTX 5060 | the vendor's decode interface, driver 615 | yes | yes | yes | yes | yes, an opaque descriptor | **measured** (*2026-09-19*): every clip bit for bit, full chroma at both depths included; a 2560x1440 stream at 120 pictures a second decodes in 0.6 ms and reads back in 0.5-0.9 ms, or is copied on the device in 0.09 ms when handed out as a handle; (*2026-09-24*) full chroma from an established host handed out as a handle for ten minutes at each depth, the device copy 0.10 ms at eight bits and 0.15 at ten |
| Other NVIDIA | the vendor's decode interface | from the interface's capability query | from the query | from the query | probed by building a decoder | yes | the creation-time probe builds a real decoder per combination, so a part that lacks one says so at creation; not run here |
| Any processor | the machine's own codec library, an LGPL build of it | where its decoder opens | yes | yes | yes | no | **measured** (*2026-09-21*): every committed clip bit-exact through an LGPL 7.1 pair (one built for the test) and the second codec's through the 8.x and 4.3 pairs another application had installed, whose H.264 decoder refuses to open; for ten minutes from this host at 2560x1440 H.264 with motion, 120 pictures a second decoded in 1.8 ms at the median (2.2 at the 95th percentile) and converted in 0.16 ms, the whole client at 31 percent of one core of the development machine's processor; HEVC ten-bit the same way in 3.0 ms (3.5) and 0.42 ms, 55 percent of one core |

---

## §8 Windows

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

## §9 What a software encoder would and would not fix

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

## §10 Open questions

| Question | Status |
|---|---|
| The vendor driver version that first offers the format-modifier extension | **unverified**, and it sets the real NVIDIA floor (§5) |
| Whether GCN 1-2 host correctly when the modern driver is asked for at boot | untested; no such part here |
| Whether the low-power entry point is better than the shader one where both exist | never measured; the order prefers the shader one so nothing already served changes |
| Whether the low-power path costs latency against the shader one | **answered 2026-08-27**: it does not. Intel discrete encodes 1080p desktop content in about 3.7 ms, roughly 1 to 1.5 ms behind the vendor backend, and the reference encoder on the same device reads 3.2 ms |
| A conversion tier below OpenGL 4.3 | not planned (§4) |
| Whether any desktop but one answers the layout, the inhibitor and the clipboard | **not run** (§6). The first decides whether absolute input lands correctly on more than one output; the others are features that are absent rather than broken |
| A clipboard mechanism that is not one desktop's own | **open** (§6). The portable-looking route is offered by some compositors and not by at least one major one |
