# The client demo

A window that shows a host's desktop: the library decodes, this presents.
C++20 on the application toolkit (`third_party/matoya`), with SDL for the pads
and the sound, a file per part, each holding its own state: the session, the
signaling service, which the library does not speak
([docs/06-api.md](../../docs/06-api.md) section 4), the presenting thread, the
window's picture and pointer, keyboard and mouse, pads, sound, and the line
each second. The knobs are listed in `config.h`.

    make
    LOWLAT_PEER=<the host's peer id> LOWLAT_SESSION=<a session token> make run

`make` builds the toolkit from its tree, the library in release, and the demo;
the toolkit's build needs the shader compiler and the ALSA, PNG and JPEG
headers, and the demo's a C++20 compiler and SDL 3, which `pkg-config` finds:
the system's (3.2 or later), or where the system has none the static one
`scripts/build-sdl3.sh` builds from SDL's release, which is how the build
workflows link it, so the packaged demo needs no SDL on the machine it lands
on (`PKG_CONFIG_PATH=<prefix>/lib/pkgconfig make`). `make LOWLAT_LIB=<dir>`
links a library already built there instead, with a run path of `../lib`
relative to the binary, which is how the build workflow packages the demo
beside the client-only library. The demo's first line names the library it loaded, its
version and its halves. `make check` runs the toolkit's websocket reader,
which the signaling reads through, against a loopback server that sends
messages in fragments; it needs no library and no network.

On Windows the demo builds with CMake and Ninja from presets, as an
application that links the library into itself builds: with clang-cl, which
finds Visual Studio and the Windows SDK itself, so no developer environment is
needed,

    cmake --preset win-x64-clang-release
    cmake --build --preset win-x64-clang-release
    set LOWLAT_PEER=<the host's peer id>
    set LOWLAT_SESSION=<a session token>
    build\win-x64-clang-release\client.exe

or with Visual Studio's own compiler from its x64 developer environment
(`win-x64-release`); each has a debug twin. The library is linked in from its
static build, which cargo is asked for on every build and remakes only when a
crate changed (`-DLOWLAT_LIB=<file>` names one built elsewhere), so
`client.exe` is the one file; a release build optimises the demo's own code
across its files at the link. SDL comes from vcpkg, static, at the version
`vcpkg.json` names: the presets take vcpkg's toolchain from `VCPKG_ROOT`, and
a build directory's first configure installs SDL into it. The toolkit is
built once, when it has not been, through its own makefile, which needs the
developer environment, with `fxc`
(the Windows SDK's) and `glslangValidator` (Khronos' shader compiler) on the
path. The C and C++ runtimes are linked statically throughout. The library
there carries the client half; for its software decoder `LOWLAT_FFMPEG_DIR`
names the directory of an LGPL FFmpeg build's DLLs, or the pair sits beside
`client.exe`. Pictures are drawn as planes through Direct3D 11, or 12 or
Vulkan with `LOWLAT_GFX=d3d12` or `vk`. The first run asks the firewall to let
the demo receive, since the session arrives over UDP.

The token is what `lowlat-login` produces; the peer id is the host's, as its
service prints it. `LOWLAT_DEVICE` names a render node for the decoder (the
first that decodes by default), `LOWLAT_DECODER` picks `auto`, `open`,
`vendor` or `none`, and `LOWLAT_SERVER` names the signaling service. The
decoders this machine can open are printed at start, one numbered row each
with what it decodes, and `LOWLAT_DECODER_INDEX` picks a row by number.
`LOWLAT_STUN` names reflexive servers, `host:port` separated by commas, up to
four: without one a direct attempt offers only this machine's own addresses,
which a host behind its own translator cannot answer. `LOWLAT_IPV4_ONLY`
offers and checks IPv4 addresses only: no IPv6 candidate of this machine's,
none of the host's checked, so a host that IPv6 would reach directly is
reached across the translation on IPv4 instead. Every candidate is printed as
it goes and comes -- this machine's as the library raises it, the host's as
signaling delivers it, and each side's readiness marker -- stamped with the
local time to the millisecond, as the library's own lines are, so a connect
can be read against the host's log. `LOWLAT_NAT=1` asks those
servers at start, before the attempt, where they see the port, and prints the
translation in front of it: the number game consoles give it, how it maps,
where the first server saw the port, and the gateway's mapping beside it --
two servers at two addresses are the least that tells. `LOWLAT_PORT` is the port
every attempt binds first; by default it is the same one every run, 24000 plus
a hash of `LOWLAT_PORT_SEED` or of the machine's name, so two copies on one
machine can be given two. The demo asks the gateway to keep that port open, by
PCP, NAT-PMP or UPnP, holding the port from start to exit, and prints what it
mapped, or why it could not, whenever that changes; `LOWLAT_PORT_MAPPING=0`
leaves the gateway alone and binds the port at each attempt instead. On
Windows the system's firewall asks once, at start (at the first attempt with
the mapping off), whether the demo may receive: what reaches the port from a
peer it never sent to is dropped until it is allowed, and allowing it adds
rules for TCP and UDP alike. `LOWLAT_RELAY=host:port`
makes the attempt a relay attempt through that relay
([docs/03-connectivity.md](../../docs/03-connectivity.md) section 7), with
`LOWLAT_RELAY_USER` and `LOWLAT_RELAY_PASS` its credential, which is handed to
the library and never printed: the relayed address is all the demo offers,
every check goes through the relay, and the line at establishment says the
path is relayed and where. A relay that does not answer, refuses the
credential, or lets the allocation go ends the session with its own outcome.
`LOWLAT_RELAY_CARRIER=tcp` reaches the relay over one TCP stream instead, for a
network that carries no datagrams (section 7.4); `udp` is the default. It is
the last rung, not a quality option: through a relay that stalls its streams
the session's own recovery turns each stall into resends.
`LOWLAT_LEGACY_CIPHER` offers no media key, so the host answers for the
legacy 128-bit cipher, keyed from its certificate digest, as an older client
would. `LOWLAT_MIC_TONE=440` sends a tone at that pitch as the microphone,
since the demo opens no recording device: a few milliseconds at a time from a
thread of its own, as a device would hand them over, compressed unless
`LOWLAT_RAW_AUDIO` declared uncompressed sound. The line each second carries
what went (`mic`, `mic_dropped`), the host's answer (`mic_open`) and the calls
it refused (`mic_refused`), and the host's change of answer is printed when it
comes. `LOWLAT_HEVC`, `LOWLAT_10BIT` and `LOWLAT_444` are the preferences the
attempt starts with, masked by what the decoder takes; Ctrl+Shift+C cycles
them live. `LOWLAT_RESOLUTION=WxH` asks the host's display for that size, which
a host sizing a display of its own takes from an owner arriving into an empty
room; unset, the demo asks nothing of the person's monitor. `LOWLAT_HANDLE`
asks for pictures as device handles, which the
toolkit's renderer imports and draws with nothing copied through this
process; only a decoder whose row says "handles" opens that way.

`LOWLAT_PAD_RAW` sends a DualShock 4 or a DualSense as its own report
([docs/10-client.md](../../docs/10-client.md) section 8), on every platform:
the demo reads the pad through SDL's HID layer, its feature reports first in
each session, and puts what the host's device is written back on it. Such a
pad is never opened as a gamepad, since SDL writes its own lights on a pad it
opens, so nothing but the host writes it. Off, they go as the sixteen-button
pad every host takes. An established host reads the report only in a mode its
owner set, and this library's hosts read it as it is, so which peer is on the
other end is the application's to know: this one is told.
`LOWLAT_PAD_RAW=only` leaves every other pad unsent as well. On the host's own
machine the virtual pads the host makes are pads to SDL and would go back as
states, making more of them; the demo tells them by the location the host
writes on them and never sends them. `LOWLAT_PAD_TRACE` prints every report
and state either way.

Three more are for measuring rather than watching. `LOWLAT_FPS` asks the
host for that frame rate through the application protocol once the first
picture is in, the way a settings panel does. `LOWLAT_PRESENT_HZ` caps how
often a new picture is taken: the cached one is still drawn on every refresh,
so on a display faster than the cap the stream is above the presentation
rate with both clocks still the display's. `LOWLAT_SECONDS` leaves cleanly
after that long, as closing the window does. `LOWLAT_PEER` may name several
hosts, separated by commas: they are visited in turn on the one handle, each
for `LOWLAT_SECONDS`, the session with one left and an attempt made to the
next -- a reconnect, the same host named twice included. `LOWLAT_MODES` asks
the host for sizes, turns and bitrates in order once the session is up, one every
`LOWLAT_MODE_EVERY` seconds (five by default), the first a period after --
after the output `LOWLAT_OUTPUT` names was asked for, where it names one, and
never when the host does not list it: a size such as `1280x720`, `turn`,
`upright` or a bitrate such as `20mbps`, comma separated, each sent alone as a
settings panel sends it. A host takes them from an owner alone.

Once a second a line goes to stdout with the presentation cadence as numbers:
presents and polls in the second (equal without a cap), new pictures, repeats
(a poll with no new picture) and skips (pictures published and never shown, a
newer one having arrived), the codec, the decoder's decode and read-back times
and the host's own encode time, the reader's lag, the round trip, the video
rate, and the process's resident set. The same figures go in the window's
title bar. Nothing is judged; the figures are what the deferred decisions of
[docs/10-client.md](../../docs/10-client.md) are decided on.

Keyboard and mouse go to the host as the toolkit reports them, pads as SDL
does; the rectangle the picture is drawn into is told to the library, which maps
positions into the picture, so a resized window or a picture shown at its
own size aims the same. When the host captures the pointer the demo confines
and hides its own, and warps it back where the host says on the way out.
The chords are the demo's and are never sent, `Ctrl+Shift` and a letter:
`I` grabs the keyboard (and lets it go), so the desktop's own keys -- the
Windows key, its task switch -- go to the host while the window has the
focus; `W` toggles fullscreen; `D` asks the host to stream its next output,
through the same application protocol a settings panel uses; `F` switches
the picture between stretched to the window and shown at its own size; `R`
lets go of a captured pointer (and takes it again); `C` cycles the video
preferences; `X` moves the session to the next decoder listed at start; `Q`
leaves the session cleanly and quits, as closing the window does. A bare
Windows key is not sent while the keyboard is not
grabbed, because the desktop here takes it and the host would be left
holding the modifier; it reaches the host on chords, and whole once grabbed.

The key table `keys.h` is generated by `scripts/gen-client-keys.py` from the
toolkit's own Linux key map and the kernel's usage table. The pads are SDL's,
whose mappings name every pad's buttons by position, and the toolkit is told
to leave them alone. A pad's state goes to the host only while the window is in
front, as the keys and the mouse do, and what it holds goes when the window
comes back, while a pad read raw keeps reporting -- the same split an
established client has between its mapped pads and its raw ones. Sound plays
through SDL on the default output, which waits until 75 ms are queued and is
emptied past 150; the line's `snd_q_ms` is what the stream holds ahead of the
device.

Sound and the cursor come with their phases.
