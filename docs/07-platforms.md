# 07 - Platforms

**Status:** decision framed 2026-08-15, **resolved at Gate B by measurement**. This is the one
document with a live decision in it rather than a settled one, and §3 says exactly what
resolves it.

## §1 Matrix

| | Status |
|---|---|
| Linux host | primary target |
| Windows host | planned, after Gate B |
| Clients | any platform with an existing client; nothing to install |

Platform-specific code is confined to `lowlat-capture`, `lowlat-inject`, and the platform
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

So the resolution is a **session-side probe**: when a user session is present, a small helper
inside it reports pointer state to the daemon over the same channel the tray already uses
([§5](#5-process-topology)). When no session is present, the daemon reports the pointer as
shown and relative mode never engages, which is correct rather than degraded.

That keeps scanout viable without weakening the cursor rules in [05 §8](05-host.md).

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

**If steps 3 and 4 succeed:** scanout is the v1 backend, with §2.1's session-side probe, and
compositor-mediated becomes the v1.x path for locked-down desktops.

**If they fail on the physical device but succeed on the virtual one:** the virtual display is
v1, the product is a headless host rather than a screen mirror, and the physical-screen case
waits for compositor-mediated.

**If both fail:** compositor-mediated becomes v1, the daemon gains a mandatory session helper,
and unattended operation is deferred with the reason recorded.

Until that probe runs, `lowlat_host_config`'s capture fields stay unspecified
([06 §14](06-api.md)), and the frame variant crossing capture to encode stays abstract
([05 §2](05-host.md)).

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

Compositors discover the device through the normal input stack, so no compositor cooperation is
required and nothing needs to be configured per desktop environment.

## §5 Process topology

The topology follows from §3, and this is the coupling worth seeing before the capture
decision is made rather than after.

**Scanout or virtual display:**

```
lowlatd            system service, owns capture, encode, inject, media
   |
   +-- unix socket, peer-credential authenticated
   |
lowlat-tray        user session, configuration and guest list
lowlat-cursor      user session, pointer state probe (§2.1), optional
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

## §6 Privileges

The daemon runs as a dedicated system user, never as root.

| Resource | Access |
|---|---|
| display device | supplementary group for the device class, plus a rule for the render node |
| input device node | supplementary group plus a rule granting the daemon write access |
| framebuffer export | may require a specific capability rather than group membership, which §3's probe determines |

The unit grants the minimum that works, denies device access by default and allows the two
device classes explicitly, and runs with a private temporary directory, no new privileges, and
a read-only system tree.

If §3 concludes that scanout needs a broad capability, **that is a product decision, not a
packaging detail**, and it is documented prominently rather than buried in a unit file. A
remote desktop daemon holding a broad capability is a meaningful attack surface, and users are
entitled to choose the lower-privilege backend with the reduced feature set instead.

## §7 Audio

Same session-versus-system question as video, decided with it ([05 §9](05-host.md)).

- A system-wide sound server instance, or a per-session one the daemon reaches through a
  helper, mirroring §5.
- A kernel loopback device is the fully session-independent option, at the cost of a module and
  manual routing.

If capture lands on a session helper, audio rides the same helper and the question disappears.

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

None of this reaches the protocol core, the IO shell's logic, or the public API.

## §11 Open items

| Item | Closes at | Resolved by |
|---|---|---|
| framebuffer export on the proprietary driver | before Phase 9 | the §3 probe |
| capture backend, and therefore the frame variant | Phase 9 | the §3 probe |
| process topology | Phase 9 | follows the backend, §5 |
| pointer-hidden signal source | Phase 9 | follows the backend, §2.1 |
| audio capture surface | Phase 10 | follows the topology |
| privilege requirement | Phase 9 | the §3 probe |

Six open items, one probe. That is the argument for running it early rather than treating it as
part of Phase 9.
