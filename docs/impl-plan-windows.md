# Implementation plan: Windows

**Status:** planned 2026-09-26. Phase W0, the platform seams, is being built; the client's
phase (W1) and the host's (W2) are planned at their own interviews.

Conventions as [impl-plan.md](impl-plan.md): a gate is a command that passes or a peer that
streams, one phase per commit, changelog entry before the checkbox.

## Why the seams come first

Everything so far was built for Linux with no platform scaffolding. The IO shell compiled
only there; the shared primitives' Windows side was a stand-in (a wait on a mutex-and-condvar
table, no timer resolution, a clock that read zero); the Linux-only dependencies were
unconditional; the generated driver bindings carried Linux type sizes; and the host's frame
loop polled a display descriptor itself. A Windows backend written on top of that would land
as conditional compilation inside the longest functions in the tree.

So W0 moves code rather than adding behaviour. Each step keeps Linux exactly as it was and
gives the Windows side a named place. Per-platform code is a module of the same name on each
platform, never a trait with one implementation per build, which leaves a compile on each
platform as the only contract keeping the two in step: CI builds both.

## Phase W0 - The platform seams

- [x] **W0.1 common**: the address wait on the system's own primitive; the timer resolution
  raised by each library handle for its life ([02 §2](02-io-shell.md)); the arrival clock on
  the performance counter; libraries loaded from the application's and the system's
  directories, never the current one or the search path. *Built 2026-09-26; the common tests
  pass on Windows, run here under a compatibility layer, and the wake test fails with the
  wake removed.*
- [x] **W0.2 net**: the system calls apart from the loop, so the loop, the attempt thread,
  the browser transport and the address filters compile everywhere and the completion-port
  receive has one module to fill ([02 §6](02-io-shell.md)). *Built 2026-09-26: the platform
  module owns the socket, the wake and the receive storage together; the receive tests now
  run through the loop's own receive path, so they are the contract the next platform's
  module meets. The browser transport and the address filters build for Windows; the loop
  waits for that platform's module.*
- [ ] **W0.3 drivers and decode**: bindings generated per target; the open-stack interface
  on Linux only; the vendor runtimes by their Windows names; descriptors confined to Linux.
- [ ] **W0.4 client**: decoder selection and the device-backed picture slots as a module per
  platform.
- [ ] **W0.5 host**: the frame loop waits for a present through the display rather than on a
  descriptor; the encoder builders move out of the frame loop's module, one module per
  platform; types every platform shares move out of the Linux modules; the guest loop takes
  its input devices from a module per platform.
- [ ] **W0.6 build**: Linux-only dependencies scoped to Linux, and a Windows job in CI.

**Gate:** the Linux tests and every lint as before, nothing on Linux behaving differently,
and the Windows target building every crate that has a Windows side.

## Phase W1 - The client (to be planned)

What [impl-plan-client.md](impl-plan-client.md) left for later: the completion-port receive,
the vendor decoder and a system one, shared textures with fences as the handle kind, and the
toolkit's D3D11 renderer in the demo.

## Phase W2 - The host (to be planned)

Desktop duplication, a D3D11 conversion, the vendor encoder on the capture's device, input
through the system's batch input call and a virtual pad device, loopback sound, and a
service. Open before it is planned: the process topology ([07 §10](07-platforms.md)), and
whether an application may supply the frames, for a virtual display that already holds them.

## Change log

- 2026-09-26: planned; W0.1 and W0.2 built.
