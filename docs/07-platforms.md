# 07 - Platforms

**Status:** **resolved by measurement 2026-08-15** (§3.1). The capture backend, frame
variant, process topology, and privilege requirement are settled; §3.2 lists what is not.

## §1 Matrix

| | Status |
|---|---|
| Linux host | primary target |
| Windows host | planned, after Gate B |
| Clients | any platform with an existing client; nothing to install |

Platform-specific code is confined to `lowlat-capture`, `lowlat-audio`, `lowlat-inject`, and the platform
receive strategy in `lowlat-net` ([02 §6](02-io-shell.md)). Everything else is portable by
construction, because the protocol core is `no_std` and touches no operating system.

## §2 The Linux capture problem

Windows has one answer for desktop capture that handles the lock screen, elevation prompts,
and unattended boot. Linux has four partial answers, and the differences between them are
product differences rather than implementation details.

| | scanout | compositor-mediated | X11 | virtual display |
|---|---|---|---|---|
| unattended, no session | yes | **no** | partial | yes |
| login screen | yes | **no** | only if the greeter is X11 | not applicable |
| any compositor | yes | yes | **no** | not applicable |
| mirrors a physical screen | yes | yes | yes | **no** |
| zero-copy buffer export | yes | yes | only with a vendor path | yes |
| pointer-hidden signal | **no** | yes | yes | **exact** |
| privilege | high | low | low | module load |

Read the columns as products, not as implementations:

- **scanout** reads the composited framebuffer directly from the display device. It is
  compositor-agnostic because it sits below the compositor, which is what gives it the login
  screen and unattended operation. It costs elevated privilege and it cannot see the pointer's
  requested visibility ([05 §8.2](05-host.md)).
- **compositor-mediated** is the sanctioned path: the compositor hands over a stream with
  cursor metadata attached. It is correct, low-privilege, and it works on every modern
  desktop. It is also bound to a live user session and its first use requires an interactive
  grant, which is exactly the thing a machine you connect *back* to cannot provide.
- **X11** is easy and complete, and it is a dead end. Every major distribution now defaults to
  a Wayland session.
- **virtual display** creates a display that is not attached to any monitor. Either as a
  kernel-provided virtual device that the user's existing compositor extends onto, or as a
  session we run ourselves. It has no greeter problem because there is no greeter, and no
  privilege problem beyond loading a module.

### §2.1 The pointer signal is not a tiebreaker

At first reading, scanout losing the pointer-hidden signal looks fatal, since relative pointer
mode is not a niche feature for this product.

It is not fatal, because **the signal is only needed when a session exists**. An application
hides the pointer to take over input. That requires an application, which requires a logged-in
session. On a greeter or an idle unattended machine there is nothing to put into relative mode.

So the resolution is a **session-side probe**: when a user session is present, a helper inside
it reports pointer state to the daemon over the channel in
[§5.1](#51-the-session-helper-and-the-channel-it-speaks-on). When no session is present, the daemon reports the pointer as
shown and relative mode never engages, which is correct rather than degraded.

That keeps scanout viable without weakening the cursor rules in [05 §8](05-host.md).

**Measured 2026-08-19, and the helper is now required rather than preferred.** The tempting
shortcut is to drive relative mode from what scanout *can* see: the hardware pointer plane
either carries a pointer or it does not, and it stops carrying one exactly when an application
hides it. On a real desktop that holds for mouselook and for a video player, both of which are
wanted. It also holds for a case that is not wanted at all: **a pointer that has merely grown
too large for the plane is drawn into the main image instead**, so the plane empties while the
pointer is still on screen and still being used. Shaking the mouse to find the cursor does it.

So the two signals are genuinely different states and neither substitutes for the other:

| signal | source | what it is for |
|---|---|---|
| a pointer is being composited | the hardware plane, visible to scanout | whether to draw one for a guest ([05 §8](05-host.md)) |
| an application hid the pointer | session state, above this backend | whether to put a guest into relative mode |

Driving relative mode from the first would lock a guest's pointer because they shook the mouse,
which is worse than not offering the feature. The helper is the only source of the second.

### §2.2 The virtual display path is the same code

A kernel-provided virtual display presents as an ordinary display device, so **the scanout
capture backend reads it with no changes**. One backend serves two products: mirror a physical
screen, or run a headless display at a resolution the client asks for.

The headless case is also the strongest one for cursor handling, because a session we own
gives exact pointer state rather than an inferred one.

It is also the safest to bring up first, for a reason that has nothing to do with design: a
software virtual display driver has no vendor-specific buffer export behavior, so it sidesteps
§3 entirely.

## §3 The open question, and what closes it

**Scanout capture depends on exporting the display device's framebuffer as a shareable buffer,
and that path is historically weakest on the proprietary NVIDIA driver**, which is the primary
development target.

This is the highest-risk assumption in the Linux plan. It is not resolvable by reading, and it
is not worth designing around in either direction until measured.

**The probe**, to run on bare metal before Phase 9 begins:

1. Confirm the display device exposes a modesetting interface at all, which on this driver
   requires the modesetting option to be enabled at module load.
2. Enumerate the active display pipeline and fetch the current framebuffer.
3. Export it as a shareable buffer handle.
4. Import that handle into the compute context the encoder uses, and encode one frame.
5. Repeat with a software virtual display driver as the control.

### §3.1 Result, measured 2026-08-15

**Scanout is the v1 capture backend.** Probed on Debian 13, kernel 6.12, a Ryzen 8845HS with
integrated RDNA 3 graphics on the open driver, running a KDE Plasma Wayland session at
2560x1440.

Every step passed once one mistake was removed:

| Step | Result |
|---|---|
| modesetting interface | present; the open driver needs no opt-in |
| enumerate pipeline, fetch framebuffer | works, with the elevated capability |
| export as a shareable buffer | **works, read-only only** |
| encode on the same device | confirmed, 1080p60 |

**Export must be read-only.** Requesting write access makes the driver refuse a scanout buffer
outright, on every plane, including a plain linear cursor buffer. A capture path has no reason
to write to a framebuffer, so this costs nothing, but it presents as a total and
undiagnosable failure if you ask for it. The standard command-line capture tool does ask for
it, which is why that tool cannot capture this machine at all.

Three properties of the real framebuffer the backend must handle, none of them assumed by the
design as written:

- **It is 10-bit, and that is not predictable.** The compositor scans out `ABGR2101010`, not
  an 8-bit format. Colour conversion must accept 10-bit input even while the encoder emits
  8-bit ([05 §3](05-host.md)), and it argues for bringing 10-bit encode forward from its
  deferred position.

  **Do not infer the scanout format from anything above it.** Asking the X server reports
  depth 24, because that is the visual X clients draw into; under a Wayland session the X
  server is itself a client several layers above the buffer that reaches the display. The
  format is the compositor's choice and it changes when high dynamic range is toggled, a
  display is swapped, or the compositor restarts. **Read format and modifier from the kernel
  on every framebuffer change** and handle what comes back. A backend that probes once at
  startup and caches will produce correct output until the day it silently produces
  garbage.
- **It is multi-plane and compressed.** The primary framebuffer reports three buffers with
  differing pitches under a vendor compression modifier. Import must be modifier-aware and
  carry every plane; treating it as one linear buffer produces garbage.
- **The cursor is a separate plane**, linear and 8-bit, exactly as [05 §8.1](05-host.md)
  assumes.

**The render node cannot be used for this.** It refuses plane enumeration outright. Capture
needs the card node plus the elevated capability, which settles the privilege question in
§6.

### §3.2 Result on the second vendor, measured 2026-08-17

**Export works there too.** Probed with the same program on the development workstation, whose
display is driven by a discrete GPU on that vendor's **open kernel modules** (610.57.04),
2560x1440 under the same compositor:

| Step | Result |
|---|---|
| modesetting interface | present; the compositor is running on it |
| enumerate pipeline, fetch framebuffer | works, with the elevated capability |
| export as a shareable buffer | **works** |

The buffer differs from §3.1's in a way the import path has to handle, and it is the opposite
of what the first result led us to expect:

- **One buffer, not three.** The primary framebuffer reports a single buffer with a pitch that
  is exactly the width times four. The multi-buffer, differing-pitch case in §3.1 is one
  vendor's compression scheme, not a general property of scanout.
- **A vendor tiling modifier, and allocation padded past the visible height.** The exported
  size divides out to more rows than the display has, so the buffer is tiled rather than
  linear. It cannot be read as a plain image at its pitch.
- **10-bit again, on completely different hardware.** The same packed 32-bit 10-bit-per-channel
  format as §3.1. Two vendors, two drivers, one conclusion: **treat 10-bit scanout as the
  normal case rather than the exception**, which is what D7 already assumes.

So both entries in §11's export row are now closed for the drivers that matter here. What
remains unmeasured is that vendor's **classic** kernel module, which this machine does not run
and which is not on the path to Gate B.

The virtual-display control could not run: the software virtual driver is not built for this
kernel. §2.2's claim is therefore untested, and now less load-bearing.

### §3.3 The scanout format changes while you watch, measured 2026-08-19

§3.1 said to read format and modifier from the kernel on every framebuffer change, and named
the triggers as swapping a display, toggling high dynamic range, or restarting the compositor.
All three are rare, which makes the rule easy to read as a precaution.

**It is not a precaution. The format changes several times a minute in ordinary use.** A
fifteen-minute session on the development workstation recorded **twelve** format changes, and
the trigger was a user entering and leaving fullscreen -- including in a browser:

```
2560x1440 ABGR2101010  modifier 0x0300000000606014  pitch 10240   composited desktop
2560x1440 ABGR8888     modifier 0x0300000000606014  pitch 10240   a fullscreen surface
```

The compositor scans out ten-bit for the desktop it composes, and hands a fullscreen
application's own eight-bit buffer straight to the plane instead. Modifier and pitch are
unchanged, because both formats are thirty-two bits per pixel; **only the interpretation of the
bytes moves**, which is the failure mode with no symptom. A backend that probes the format once
at startup produces a correct picture until the moment somebody presses F11, and then produces
a wrong one with nothing logged.

Three consequences the pipeline has to carry:

- **The import is rebuilt on a format change, not the session.** This is the `Lost` contract
  from [05 §2](05-host.md), and it now has a frequency attached.
- **The conversion accepts both packed depths.** Ten-bit input is the normal case (§3.1, §3.2)
  and eight-bit is what arrives whenever an application is fullscreen, which is most of the
  time anyone is playing something.
- **A rebuild must not cost a keyframe.** At this rate, a reinitialization that forces one
  would put a visible hitch on every fullscreen toggle.

## §4 Input

Input injection is the one platform question with no tradeoff. **The kernel input layer is the
answer on every Linux display stack**, and it is the capability that display-server-level
injection structurally cannot match: it works identically on X11, on Wayland, at the greeter,
and inside a session we own.

- One virtual device per guest, created at connect and destroyed at disconnect.
- Keyboard, pointer buttons and wheel, relative motion, and absolute motion as a separate
  device, since absolute and relative pointers cannot share one device cleanly.
- Absolute coordinates map to the output's geometry at injection, rotation-aware.
- Device creation needs write access to the input device node. The daemon gets it through a
  group and a rule, not through running as root ([§6](#6-privileges)).

### §4.1 A created device is not a usable one

**The kernel publishes the device in under half a millisecond and the display stack takes
between one and three tenths of a second to start delivering from it.** Events written into
that gap are accepted by the kernel and go nowhere. Measured on a real desktop session by
creating a device and injecting one key per step at increasing delays, then reading back what
the display server actually delivered:

| what was created | first delay whose event arrived |
|---|---|
| one keyboard, keys only | 100 to 150 ms |
| one keyboard, keys and indicator lights | 200 ms, every run |
| **the three devices a guest really gets** | **200 to 260 ms** |

**The last row is the one to design against**, and it is nearly double the first. The delay
scales with how many devices the system has to process at once, not with any one device, so a
figure measured by creating a single keyboard understates the case that actually ships.

Three consequences, and the first two are cheap.

- **Do not declare indicator lights.** A device claiming them costs roughly twice the setup
  time, because the display server configures keyboard state for it. An injection device has no
  use for them: nothing reads its lights.
- **Create the device when the guest is admitted, not when its first input arrives.** The wait
  then overlaps connectivity and session initialization, which take longer than it does, and
  costs nothing observable. A device created on demand puts the whole delay in front of a
  keystroke somebody is waiting for.
- **Hold events until the deadline rather than dropping them.** The first keystroke of a session
  arriving a fraction of a second late is not noticeable; the first keystroke vanishing is, and
  it presents as a network fault.

**There is no readiness signal to wait on instead**, and two candidates were tested rather than
dismissed. A host cannot ask whether a consumer has opened its device, and the consumer writes
nothing back: a keyboard that declares indicator lights is the one case where the display server
might reply on the same descriptor, and across six runs it never did. The second candidate is
better and still fails: the device's event node is created owned by the system alone and is
granted to the group part way through the window, which is observable. It is a **precondition,
not a signal** -- the grant lands at 50 to 82 ms and does not predict delivery, with the same
51 ms grant preceding delivery at 200 ms in one run and 230 ms in another.

A fixed wait is the only portable answer, which is the argument for starting it as early as
possible rather than for making it shorter. It is set **above** the worst measured case rather
than at it, because the two errors are not symmetric.

**The delay is not the display server's, so it is not the display server's to differ on.**
Timing two endpoints in the same run -- a display-server client, and the input library a
compositor consumes -- gives 140 to 230 ms and 100 to 260 ms respectively. They are the same
within the noise of two processes being scheduled, so **the display server adds nothing
measurable** and the whole cost sits in device discovery, which every display stack shares.

That is a better answer than measuring the same thing twice would have been: it says the figure
travels, rather than that two figures happened to agree. A compositor session would still show
whether its own device setup costs anything on top, but it does not set the number.

Compositors discover the device through the normal input stack, so no compositor cooperation is
required and nothing needs to be configured per desktop environment.

### §4.2 Gamepads, and the two device layers

Linux offers two ways to publish a virtual controller, and **which one is right is decided by
the controller being emulated, not by preference.**

| Layer | What it publishes | Suits |
|---|---|---|
| kernel input layer | buttons and axes directly, with an identity of our choosing | an Xbox 360 pad |
| kernel HID layer | a report descriptor the kernel's own driver then binds | a DualShock 4 or DualSense |

**An Xbox 360 pad must go through the input layer**, because it is not a HID device at all: it
speaks a vendor protocol on a vendor-class interface, and its kernel driver binds on the USB bus
rather than the HID one. There is nothing for the HID layer to present.

**A DualShock 4 or DualSense must go through the HID layer**, because the whole point of
emulating one is that applications recognise it as one. Presented as a report descriptor, the
kernel's own driver claims it and supplies the correct button map, the touchpad as its own
device, motion, battery, and lights and rumble as ordinary output reports. Published through the
input layer instead you get the button layout and none of the identity, which is the part that
was wanted.

**v1 emulates the Xbox 360 pad only.** A peer sends one button layout regardless of what it is
holding, so a second emulation buys identity and nothing else, and the identity is not free: the
kernel driver interrogates a device of that family during setup and will not attach to one that
cannot answer. That is worth doing and is not worth doing first.

**A virtual pad has to borrow a real controller's identity.** Everything that reads a gamepad
decides what its buttons mean by looking the bus, vendor, product and version up in a table:
browsers, the common controller libraries, and every per-title mapping people share. A device
with an identity nobody has heard of is delivered as a bag of numbered buttons and axes, and no
amount of correct input makes it usable. So the pad presents the Xbox 360 controller's, and
**the mapping that selects is verified against what the device actually emits** rather than
assumed: buttons zero to ten in kernel-code order, axes zero to five, direction pad on the first
hat. Borrowing an identity whose mapping did not match would be worse than borrowing none,
because everything would look recognised and half the buttons would be wrong.

Identification does not have to be given up for it. The **physical-location string** carries
which guest and which pad, which is what that field is for, so a device listing shows the model
and the location shows the session.

**The HID layer's device node carries the same packaging obligation as the input layer's**
([§6](#6-privileges)): no distribution grants access to either by default, so a second backend
means a second rule.

## §5 Process topology

The topology follows from §3, and this is the coupling worth seeing before the capture
decision is made rather than after.

**Scanout or virtual display:**

```
lowlatd            system service, owns capture, encode, inject, media
   |
   +-- unix socket, peer-credential authenticated
   |
lowlatd helper     user session, the same binary in its session role, optional
lowlat-tray        user session, configuration and guest list, optional
```

**Compositor-mediated:**

```
lowlatd            system service, owns encode, inject, media
   |
   +-- unix socket + buffer handle passing
   |
lowlat-session     user session, owns capture, REQUIRED
```

The difference matters: in the first topology the session-side components are optional and the
stream survives a logout. In the second, capture lives in the session, so a logout ends the
stream and the daemon becomes a coordinator rather than the thing doing the work.

**The tray is never load-bearing in either.** It attaches, shows state, kicks guests, changes
configuration, and detaches. Closing it does nothing to the stream. That was the design intent
from the start and it survives the capture decision either way.

Buffer handles pass over the socket as file descriptors, so even the split topology stays
zero-copy.

### §5.1 The session helper, and the channel it speaks on

**A system service cannot reach a desktop session, and the two ways it might are both
wrong.** Measured on a live session:

| route | result |
|---|---|
| the session's message bus | refused: the bus authenticates by peer identity and drops a service |
| the compositor's own socket | reachable, but the protocol differs per compositor and one of the major ones offers none at all |
| starting a process inside the session | works, and requires the service to discover a session, drop privilege and guess a desktop |

So the service does not reach into the session at all. **The session side connects outward**,
to a socket the service already listens on. That removes the problem rather than solving it:
nothing discovers a session, nothing drops privilege, nothing has to know which desktop is
running. It also arrives with an identity, because a local socket carries the peer's
credentials.

**One binary, two roles.** The helper is `lowlatd` invoked in its session role, not a second
program. The reason is version skew: the two sides speak a private protocol that changes
whenever either does, and one build cannot disagree with itself. The role is chosen by the
first argument and by nothing else -- **never by a flag that may appear anywhere in a command
line**, because a service and a session agent run at different privilege and a file that can
be talked into the wrong one is a security defect rather than a bug.

**The tray stays a separate program.** Not for protocol reasons: it will link a user-interface
toolkit, and that has no business inside a system service's binary.

**It is one channel with two roles, not two channels.** The tray needs the same two directions
the helper does -- state pushed as it changes, and questions that get one answer -- so it
speaks the same framing and announces its role in the first frame. What differs between the
two is authorisation rather than transport, and that difference is the whole of it: a helper
makes statements about its own session and is trusted with exactly that, while a tray acts on
the host, and kicking a guest is not something any local user may do.

#### What it is for

Four things need session state, and none of them can be answered below it:

| customer | shape | what is lost without a helper |
|---|---|---|
| relative pointer mode (§2.1) | a signal, pushed on change | the feature; a guest is never put into relative mode |
| the idle inhibitor | a lease, held while asked | the screen may blank during a session |
| the display layout | a question, asked when needed | the backend's own reading is used instead |
| display mode and rotation | a request, rarely | a guest's request is refused with a reason |
| the clipboard | an ownership, held | a guest's text is dropped and the desktop's never leaves |

#### The clipboard is an ownership, not a value

Copied text looks like the one thing on that list a service could do for itself, and it is the
one that most needs something living in the session.

**Putting text on a clipboard is announcing that you own the selection.** The bytes are not
handed over then; they are asked for later, when somebody pastes, in a request back to whoever
claimed it. That is true of the privileged selection protocol on one display stack and of X
selections on the other. **A program that sets the clipboard and exits takes the clipboard with
it**, so there is no one-shot form of this: the thing that wrote it has to still be there when
the paste happens. Watching for changes is the same shape from the other end, a subscription
that lasts exactly as long as the thing holding it.

It can appear to work without any of that, because a desktop's own clipboard manager may keep a
copy of what it saw. That is a property of somebody's configuration and not of the design, and
building on it means a feature that works on one desk and not the next.

The mechanism differs per desktop as well -- a privileged selection protocol here, an X
selection there -- which is the same reason the rest of this section exists.

**The policy, though, is not the helper's and not the boundary's.** Copied text travels as an
opaque application message ([01 §11.1](01-protocol.md)), so the library never sees it and the
service decides who may have it.

| `guest_clipboard` | the desktop's clipboard reaches a guest | a guest's clipboard reaches the desktop |
|---|---|---|
| `off` | no | no |
| `send` | yes | no |
| `recv` | no | yes |
| `both` | yes | yes |

**The names are the host's point of view**, and which way round they read is load bearing:
`send` is this machine sending its own clipboard out. **Anything else is `off`** -- absent,
empty, misspelled, or a value from a newer version -- so that a typo cannot open a clipboard
and a configuration this build does not understand fails closed.

**The two directions are not equally dangerous, which is the whole reason there are four
values and not two.** `send` ships whatever the person at the machine copied, and that includes
what a password manager put there. `recv` puts a guest's text on the desktop's clipboard, where
a person still has to choose to paste it. One switch would mean letting a guest paste a link
into your machine also hands them everything you copy.

**A peer that owns the machine is not a guest** and is not subject to this: ownership arrives
relayed from signaling and is never read from the peer ([04 §3](04-signaling.md)). The setting
is about guests, which is also what the stock arrangement does.

Nothing new bounds the size: copied text is an application message and is already refused above
the ceiling those carry.

#### Who may connect, and where

**Any local user may connect.** The check is that the peer is local, and there is no second
one. On the machine this is for, the person at the keyboard is the person the session belongs
to, and inventing an authorisation scheme for a case that does not arise buys nothing and adds
a thing to get wrong. The consequence is worth naming rather than discovering: on a machine
with several people logged in at once, any of them can read the guest list, kick a guest and
change the stream's settings.

**This is not the guest permission model and must not grow into one.** What a *guest* may
drive, whether it owns the machine, and whether it needed approval at all are decided by
signaling and arrive relayed -- never from the peer itself, which is the whole reason they are
relayed ([04 §3](04-signaling.md)). A local client on this channel is a different question with
a different answer, and the two must not be made to look alike.

**The socket is at a known path**, and the reason is a consequence rather than a preference.
The service starts both session-side programs itself and could hand each a private path, but a
tray started by hand -- which is how a person gets one back after closing it -- is not started
by the service and has nothing to be handed. A path it cannot find is a tray that cannot
connect, and asking the service to start another one needs the channel it is missing. So the
path is known, a tray started by hand simply connects and is the tray, and no protocol for
asking to be restarted has to exist at all. A private path would buy secrecy that the rule
above already gives away.

**A session-side program exiting means nothing to the stream.** The service starts them and
does not depend on them: a tray that is closed is not a tray that is missed, and a session
that ends takes both with it while the stream carries on. Nothing is restarted into a session
that is no longer there.

#### The rules that matter more than the encoding

1. **The stream never depends on the helper.** No frame waits on it, no encode consults it,
   and a helper that dies mid-session disturbs nothing. This is the same rule the tray has
   always had, and it is what keeps a logout survivable.
2. **Absent is not degraded.** With no helper the answers are the honest ones: the pointer is
   reported shown, so relative mode never engages; no idle lease is held; a mode request is
   refused and the guest is told why. Nothing guesses.
3. **The service never blocks on it.** Every request carries a deadline, and a helper that
   stops answering is dropped rather than waited for. An unbounded wait on a process in
   somebody's session is an unbounded wait on somebody's session.
4. **A helper speaks only for itself.** The peer's credentials, not its claims, say which user
   is on the other end, and its statements apply to that user's session and no other. It is
   never asked to do anything it could not do on its own behalf, so it carries no privilege
   worth taking.
5. **One helper to a session, newest wins.** A reconnect replaces its predecessor rather than
   joining it, because two answers to "is the pointer hidden" is not a state anything can act
   on.
6. **Both directions, and they are not symmetric.** The session pushes signals as they change;
   the service asks questions and gets one answer. A signal is never a reply and a reply is
   never assumed.

#### The channel

Length-prefixed frames on a Unix stream socket: a fixed-width little-endian length, then a
body. The first frame each way is a version, a role and what the sender can do; a version that
is not understood ends the connection rather than being worked around, because both sides ship
in one file and the only way to see a mismatch is a stale process -- continuing with one is how
a stale process becomes a wrong answer.

**The body is JSON, and the reason is not convenience.** The rate is a handful of messages a
second at its worst, so nothing here is paying for it, and what it buys is that a wrong answer
can be read out of a log rather than reconstructed from a hex dump, which is the same reason
the opaque messages this host passes between a guest and an application are logged as the
exact bytes that were sent ([01 §11.1](01-protocol.md)). It also needs nothing added: the
serializer is already in this program. The framing is ours, so a denser body is a change of
one function if a reason for one ever appears. **The fixed-width discipline the wire protocol is written to does not apply here**:
that rule exists because the wire has to match a format this project does not own, and this
channel is private and versioned by its own build.

**No remote-procedure-call library.** What one would supply is serialization and request and
reply plumbing; what it would not supply is any of the work -- the peer's credentials, the
deadline on every request, one helper to a session, and the file descriptors the
compositor-mediated topology would pass. It would also arrive with an executor, which this
project keeps to signaling alone ([00-overview.md](00-overview.md) D3); a socket carrying a
few messages a second does not need a runtime to read it.

The channel is local and carries no media, so it has none of the congestion apparatus the wire
protocol has. Buffer handles are a separate matter and belong to the compositor-mediated
topology above, not here.

## §6 Privileges

The daemon runs as a dedicated system user, never as root.

| Resource | Access |
|---|---|
| display device | supplementary group for the device class, plus a rule for the render node |
| input device node | supplementary group plus a rule granting the daemon write access |
| HID device node | the same, and only once a controller needing that layer is emulated ([§4.2](#42-gamepads-and-the-two-device-layers)) |
| framebuffer export | may require a specific capability rather than group membership, which §3's probe determines |

The unit grants the minimum that works, denies device access by default and allows the two
device classes explicitly, and runs with a private temporary directory, no new privileges, and
a read-only system tree.

If §3 concludes that scanout needs a broad capability, **that is a product decision, not a
packaging detail**, and it is documented prominently rather than buried in a unit file. A
remote desktop daemon holding a broad capability is a meaningful attack surface, and users are
entitled to choose the lower-privilege backend with the reduced feature set instead.

## §7 Audio

**Closed 2026-08-22, and the session question turned out not to arise.** The sound server is
per-session -- its socket sits in that session's own runtime directory, which is private to the
user -- but a service already privileged enough to reach the directory **is admitted to the
socket and is never challenged for a credential**. Measured both ways, with and without the
session user's authentication cookie: identical. So audio is reached exactly as the desktop
layout is reached in §2.1 -- find the session's socket, open it -- and **no helper is required
for it**.

- **The PulseAudio client interface, loaded at runtime**, is what this host speaks. PipeWire
  serves it as well as PulseAudio does, so one path reaches both, and it is a handful of symbols
  out of two shared libraries rather than a linked dependency: a machine without them has no
  audio rather than a service that will not start. The native PipeWire interface is the
  alternative, and it buys a smaller capture period than the graph's own -- which is worth
  nothing while a packet is 20 ms of it.
- **The device is the default output's monitor**, which the server will name on request, so
  finding it costs no enumeration. A named device is checked against the enumeration first,
  because **a name that does not resolve is substituted rather than refused** ([05 §9.3](05-host.md)).
- **A kernel loopback device is not needed** and is not used. It was the fully
  session-independent option and its cost was a module and manual routing on the user's machine.

**What the platform will not do**, both measured rather than assumed:

1. **A capture does not follow the default output.** The device is resolved once, when the stream
   is connected; changing the default afterwards leaves the capture on the old one. Following it
   is this host's work, not the server's ([05 §9.3](05-host.md)).
2. **There is no per-application exclusion.** Capturing everything except one named program is an
   operating-system call on another platform and has no equivalent here, which is why this host
   does not offer it ([05 §9.5](05-host.md)).

### §7.1 The guest microphone as a device

**A guest's microphone arrives decoded ([05 §9.6](05-host.md)); making it a capture device that
other programs can select is a different problem, and it is the application's**
([06 §13](06-api.md)). The library owns neither the session, the naming, nor the lifetime. What
follows is what the two platforms make possible, because the shape differs enough to change who
owns the timing.

**Linux: a pipe-backed source, measured 2026-08-26.** One module load creates a device that
programs list as a microphone rather than as a monitor, and feeding it is a write of raw
sixteen-bit samples to a FIFO -- mono at 48 kHz, which is what the wire already carries, so
nothing converts. It needs **no dependency this host does not already have**: the module is
loaded over the same client interface §7 already speaks, and the write side is a file. Measured
end to end on PipeWire's PulseAudio server: samples written to the pipe read back from the source
at full amplitude with no gaps.

**The obvious alternative does not work, and it fails quietly.** A null sink declared as a
virtual source *is* offered as a real source, but it is not a sink, so a stream cannot be pointed
at it by name: the session manager moves that stream to the default output instead, and the
device stays silent while something else gets louder. Feeding one means linking ports through the
native interface, which is a second sound library and a routing decision the session manager can
override.

**Linux leaves the timing with us.** A capture device is read on the sound server's clock and
packets arrive on the network's, so writing only when a packet lands underruns and crackles. That
costs a short prime before the first write and silence written rather than nothing on a gap --
the same shape as the receive side of any jitter buffer, and the part worth measuring rather than
assuming.

**Windows inverts that.** A virtual capture device there is a kernel-mode driver, which makes it
a signing and distribution problem rather than a coding one: it must be code signed to load at
all. The drivers that exist are third-party and installed separately -- AudioRelay's microphone
is one -- and **they carry the jitter buffer and the sample clocking inside the driver**. So the
host's side of it is small and unglamorous: open the device handle and hand it fixed-size buffers
of PCM through an IOCTL. The timing that Linux leaves to us is already handled behind that
handle.

**So this host ships no driver on either platform.** On Linux it can create the device itself,
from what it already links. On Windows it can drive a device somebody else installed, and if
nobody installed one there is no microphone -- which is a deployment answer rather than a
missing feature.

## §8 GPU and encoder

- Hardware encode requires an NVIDIA GPU and a current driver. The encoder library is loaded at
  runtime, so its absence is a missing backend rather than a failed start.
- The compute interoperation path that imports capture buffers is what makes the pipeline
  zero-copy ([05 §2](05-host.md)). It is the same path §3 probes.
- Software encode requires no GPU and is the continuous integration path
  ([impl-plan §Phase 11](impl-plan.md)).
- Open-stack hardware encode for other vendors comes after there is hardware to test on.
- **Device identifiers are discovered at runtime and never persisted.** They change across
  reboots and driver reloads, so a configuration file naming one is a configuration file that
  breaks.

## §9 Development environment

Phases 0 to 8 need no display hardware, which is why the phase plan is ordered as it is
([impl-plan](impl-plan.md)).

A virtualized Linux environment on a Windows host is sufficient for all of them: the kernel
input device node is present, so injection is testable against real devices rather than mocks;
hardware encode is available; and outbound networking is real, so connectivity works against
real peers.

What such an environment cannot provide is any display device at all, which is the direct
argument for capture being a trait with a synthetic implementation from the first commit rather
than a late abstraction.

Namespace-based network fixtures ([08-testing.md](08-testing.md)) also run there, which matters
because a developer network provides exactly one address-translation topology and the
connectivity engine must handle six.

## §10 Windows

After Gate B. The differences are contained:

- Capture through the platform duplication interface, which handles the session, lock screen,
  and elevation cases that motivate §2 on Linux.
- Injection through the platform batch input call, which requires interactive session context.
  A service must therefore run capture and injection in the interactive session, mirroring the
  split topology in §5.
- Timer resolution must be raised explicitly ([02 §2](02-io-shell.md)).
- Completion-port receive rather than batched polling ([02 §6](02-io-shell.md)).
- Shared texture handles use the legacy form, not the modern one, for cross-process
  compatibility.
- A guest's microphone becomes a capture device only through a signed kernel-mode driver, which
  this host does not ship and does not need to write ([§7.1](#71-the-guest-microphone-as-a-device)).

None of this reaches the protocol core, the IO shell's logic, or the public API.

## §11 Open items

| Item | Status |
|---|---|
| framebuffer export, open stack | **closed**: works, read-only (§3.1) |
| framebuffer export, second vendor | **closed**: works, single tiled buffer (§3.2) |
| capture backend | **closed**: scanout |
| frame variant | **closed**: multi-plane, modifier-bearing, 10-bit capable |
| process topology | **closed**: system service, session helpers optional (§5) |
| guest microphone as a device | **open**: the Linux route is measured and unbuilt (§7.1); Windows needs a driver this host will not ship |
| privilege requirement | **closed**: card node plus the elevated capability |
| pointer-hidden signal source | **closed as a question, open as work**: the session-side probe is required, not preferred, and the plane signal was measured to be a different state (§2.1) |
| scanout format stability | **closed**: it changes several times a minute in ordinary use (§3.3) |
| framebuffer export, classic module of the second vendor | open, not run here, off the path |
| virtual display | open: the software virtual driver is absent from this kernel |
| audio capture surface | **closed**: the session's sound server, reached over its own socket, no helper (§7) |

Six of the ten were closed by one probe, run before Phase 0 rather than at Phase 9. A second
run of the same probe on different hardware closed the seventh (§3.2), and the first run of the
real backend closed two more (§2.1, §3.3) while adding one of them to the list. The rest are all
narrower than the questions they replaced.
