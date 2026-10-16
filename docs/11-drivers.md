# 11 - Drivers

**Status:** planned 2026-10-15, at W3's interview; the display's driver and its package built
the same day (W3.0), and its controller (W3.1); the rest planned. Built by [impl-plan-windows.md](impl-plan-windows.md) W3,
where the decisions and their reasons are recorded; this document is the design they add up to.

On Windows the host carries drivers of its own where the system offers no interface for what a
host needs: a display, for a machine with no monitor and for pictures the host reads without the
system's own duplication pass (§1), and later an input device for the guests' mice, keyboards and
pads (§2). Both are user-mode drivers: a library the system's user-mode driver host loads, the
system's own kernel components beneath it, nothing of this project's in the kernel. Their C
interface is the library's third half ([06](06-api.md)); the host's side of the display is
[05 §2](05-host.md); installing and signing them is [07 §10](07-platforms.md).

## §1 The display

### §1.1 The device

- One root-enumerated device of the display class: an indirect display adapter with up to
  sixteen monitors. The system composes each monitor's desktop on a GPU and hands the driver each
  composed picture; the driver shows them nowhere.
- Written in Rust in this workspace. The declarations of the user-mode driver framework and of
  the indirect display class are generated once from the driver kit's headers, against the
  oldest versions the floor needs -- the class at 1.4, the framework at 2.25 -- and committed; a
  later call is made only behind its availability check. A callback never unwinds into the
  system's code: a panic ends the driver's host process, which the system starts again.
- A host process of its own, never shared with another driver, so the process the host takes
  handles from holds this driver alone; raised in priority, since its wake lies on every frame's
  path (measured with and without). *Amended 2026-10-16:* the setup's high-priority setting
  chooses a shared pool meant for input drivers, and a driver with a process of its own has none:
  the process runs at normal priority, measured. **The frame thread stays at normal priority**,
  measured under one busy loop per logical processor at 120 a second: woken by the system's
  frame it takes the processor from them at once, nineteen acquires in twenty within 0.09 ms of
  the median and the worst of 1,200 between 0.9 and 3.1 ms later. Raised through the system's
  multimedia scheduling it does worse: one acquire in twenty 1.4 to 1.6 ms later than the median
  and the worst about 2.2 ms, about a fifth of them late by up to 2 ms -- consistent with that
  scheduling keeping a fifth of each period for other work, the threads it raised dropped below
  normal meanwhile. Idle, the two are alike.
- The floor is Windows 10 1903, the first release whose display class lets a driver name the GPU
  to render on.
- Its log goes to the debugger stream: the driver's host process has no console and no file it
  may write.

### §1.2 Two interfaces

| Interface | Who opens it | What it carries |
|---|---|---|
| control | any interactive user, administrators, the system account | a plug, bound or kept, at a slot asked for or any; an unplug; a replug with a mode; the list; a reload of the file; the version |
| frames | anyone the device admits may ask; only the system account can use the answer | the section, the buffers and the fence of §1.6; the pointer of §1.7, in the same section |

**The frame interface is a screen-capture channel, and the handles are what limit it.** Its
answer is a set of handle values in the driver's own process, which only a process allowed to take
handles from that process can use -- the system account, or an administrator who turns on the
debugging privilege and could find them there unasked. So the driver answers anyone the device
admits, one device carries one access list for both interfaces, and nothing the driver does waits
on a reader. (*Amended 2026-10-16*: first written as refused at open to any caller but the system
account; that check would have guarded only what the handles already guard.) Plugging a
display asks no more than a person at the desk may do, so the control interface is open to any
interactive user, and ownership (§1.3) keeps one user's monitors from another's requests. Both
are answered on the display class's own request path.

### §1.3 Monitors, and how long they live

A monitor lives one of two ways, chosen when it is plugged:

- **Bound**: owned by the handle that plugged it. Only that handle re-modes or unplugs it, and
  the handle's end -- a close, or its process's exit or crash -- unplugs it at once. The host's
  own display is bound to the host, so a host that ends, however it ends, leaves no display
  behind.
- **Kept**: owned by no handle. Any controller may re-mode or unplug it; it lives until removed
  or until the driver stops, and it may be marked to come back at start (§1.4).

**Nothing keeps a monitor alive by traffic.** A driver whose monitors belong to nobody can read
only a stream of pings as somebody still wanting them, and silence then removes every monitor;
any stall of the pinging thread -- a debugger, a resume from sleep, a starved thread -- takes a
display away mid-session. Ownership by handle removes a monitor exactly when its owner goes.

**Each monitor is a slot**, 0 to 15, which a plug may ask for. Its identity -- its container and
the serial in its EDID -- derives from the slot and never changes, so the system keeps each
display's resolution, position and scale across a replug and a reboot. The EDID is generated once
per slot, or is a real monitor's dumped EDID given in the file (§1.4), its serial patched per
slot. **Each monitor is reported as a DisplayPort monitor**, as its EDID says. The host's own
display takes the highest free slot, so it keeps one identity and never takes an application's
first free one.

### §1.4 What persists

One file, `%ProgramData%\lowlat\display\displays.conf`, in a folder of its own, holds what
outlives the driver, a setting a line: the GPU the monitors render on (`gpu`), whether the
pointer is drawn into the pictures (`pointer`), the custom modes (`mode`, §1.5), the kept
monitors marked to come back, each at its slot and mode (`monitor`), and an optional EDID
(`edid`). **The driver reads it when the adapter starts, before any request, and again when a
controller asks; it never writes it** -- a user-mode driver has no right to -- and the
controller writes it, under a lock any user's controller takes, replacing it whole. **Any user
may write it, so the driver reads it as hostile**: every line bounded, a line it cannot read
skipped and counted, a word it does not know skipped quietly so an older driver reads a newer
file, nothing in it able to stop the driver starting. The folder, made by the installer, lets
users change it, so an unelevated program keeps a display, and lets the driver's own account
read it; the host's configuration beside it stays readable by the system account and
administrators alone ([07 §10](07-platforms.md)). The GPU is named by vendor and device, since
an adapter's identifier changes across boots (§1.7).

### §1.5 Modes

- **A monitor's modes are fixed when it arrives**: a built-in list of common sizes at common
  rates, the custom modes, and the mode the plug asks for, kept within about ninety entries -- a
  monitor with around a hundred has been seen not to arrive. The preferred one is the plug's.
- **A listed mode is set through the system's display configuration.** A kept monitor's is saved
  in the system's database of arrangements, as any monitor's is, so it comes back after a reboot
  where it was; a bound monitor's never is. **An arrangement in which only virtual displays would
  be active is never saved**: one that only a virtual display can satisfy holds a later boot
  waiting for the driver.
- **A mode not listed is a replug of that monitor alone**, with the mode added first, then set
  once the replacement is ready. Requests during a replug are coalesced into one more, and a
  replug waits until the old monitor's swapchain is torn down. Such a mode is not kept; a custom
  mode is. Updating a live monitor's list in place leaves the system's cached modes stale, so it
  is not done.
- The adapter asks the system for the smallest mode that holds the desktop, without which a
  monitor is sometimes refused.
- **A mode asks for no display bandwidth.** The adapter declares no budget, and the system keeps
  the active modes' sum within it, so a mode asking for any is never made active.
- **A plugged monitor is made active by the controller when the system does not.** The system
  extends a set of monitors it has not seen and restores a set it saved, at once; a set it
  remembers with nothing saved it leaves dark. The controller then adds the monitor's path to
  the active ones, saved as a mode is. **A saved arrangement overrules the mode a plug asks
  for**, so the controller reads the mode back and sets the one asked for. *Amended 2026-10-16*:
  the overruling can come after the mode was put -- a monitor plugged again is restored to its
  set's arrangement as its arrival completes, more than three seconds after the mode while a
  program presented on it, measured -- and a bound monitor's unsaved mode does not survive it, so
  the controller puts the mode again whenever the system has the monitor at another, until its
  bound.
- **The host's display holds its mode** for as long as the host runs: another monitor's arrival
  and a program leaving exclusive fullscreen each put a saved mode back on a monitor, so the host
  puts its own back whenever the system's differs.

### §1.6 Pictures

The driver converts nothing: formats, depth, full chroma and scaling stay the host's, so a change
to them never re-signs the driver. The display is standard dynamic range, and every buffer is
eight-bit BGRA. **The host reads the system's own buffer** ([05 §2](05-host.md)):

- **Per swapchain**, the driver shares each distinct buffer once, read-only, with a fence of the
  device the swapchain was given, and publishes their handles in a section of its own. The host
  takes the section, the handles and an event the driver sets at each publish from the driver's
  process, and keeps a view per buffer until the swapchain is replaced. **A swapchain has three
  buffers, used in turn** (measured); the count is read per swapchain, not assumed.
- **Per picture**, the driver acquires, notes the time, signals the fence, publishes the picture
  -- its buffer, sequence and frame number -- and finishes it at once. **Nothing waits on the
  host.** The system writes a buffer again only for the third composition after it, so a picture
  stays intact for about three refreshes after its acquire -- 24 ms at 120 a second, measured --
  where the host converts within half a millisecond.
- **The host takes the latest picture**, waits for the fence on its device and converts into
  the encoder's input; after queuing the conversion it checks the driver's sequence, and when the
  driver has acquired as many newer pictures as the swapchain has buffers less one, the buffer may
  have been written again, so the latest is converted instead, and counted.
- **A still desktop's last picture stays readable**: an acquire that finds nothing new leaves the
  driver the buffer it has, so the latest picture is never written again while it is the latest.
- **A picture's time is the driver's acquire.** The time the system attaches to a frame is when
  it should be shown, which lies ahead of the hand-over.
- A frame repeating the last frame number is no new picture, and the system's marker for a still
  desktop is a present of the same picture; after it the system sends nothing, and the host's own
  clock keeps the stream's pace. *Amended 2026-10-16, at W3.3's build:* **the same picture is the
  one the reader took last**, by its own frame number: the marker repeats the frame number in
  another buffer, and to a reader that never took the picture it repeats, it is that picture.
- Each acquired buffer carries a reference the driver releases at its next acquire. *Amended
  2026-10-16, at W3.3's build:* **the driver keeps it until then**, as long as the buffer stays
  its own, so a page asked for while the desktop is still is given the picture on the display:
  the system hands over nothing new, and the duplication's first picture is the desktop too.
- *Added 2026-10-16, at W3.3's build:* **a monitor's page lives as long as the driver**, made at
  the first request for its frames: a replug, a mode and every new swapchain keep it, each a new
  generation in it. Any number of readers may take it and nothing in the driver knows how many;
  the event a publish sets wakes one waiter, so a second reader at once can learn of a picture a
  refresh late, at its wait's own bound. Measured, two readers waiting on it together at 120 a
  second each converted every one of 240 pictures, neither waiting more than 9.9 ms for one.

*Rewritten 2026-10-16, at W3.3's review.* The driver first held each buffer, acquiring no other,
until the host released it or a bound inside a frame period passed. Measured, the system's own
cycle keeps a picture intact six times longer than that bound, and the hold added only the
driver's dependence on the host: a release fence, a claim, a picture held across a monitor's
departure, and a later next picture whenever the host was slow. The buffer opens in another
process as it is, with no keyed mutex; were a later system's not to, the driver would convert
into targets of its own, by measurement.

### §1.7 The GPU, and the pointer

- **One GPU for every monitor**, since the display class takes one for the whole adapter and
  re-creates every swapchain when it changes: the file's, by vendor and device, else the first
  hardware GPU in the system's high-performance order, named by the driver at its start before
  any monitor arrives. The system may choose another, and each swapchain says which it got; the
  host builds on that one and never names a GPU itself.
- **The system lists the monitor's output under the GPU that renders it**, as that GPU's own
  output, not under the display adapter.
- **The pointer is a hardware cursor**, alpha and full XOR up to 256 a side -- the largest the
  wire carries -- so the pictures carry no pointer, and the host reads the shape, the hotspot and
  the visibility from the driver, the position and the show state as for any display
  ([05 §8](05-host.md)). The file's setting draws it into the pictures instead, for software that
  wants it there.
  *Amended 2026-10-16, at W3.4's review:* **the driver publishes the pointer in the monitor's
  frame section** (§1.6), after the pictures' page: whether a hardware cursor is set up and
  whether it is visible, and the shape -- its kind, size, row pitch, hotspot and the system's
  number for it -- with its pixels copied in only when that number moves. Each part has a
  sequence lock of its own, and the monitor's worker writes them when the system signals new
  cursor data; the channel's version is 2. The system hands a driver two kinds of shape, alpha
  and masked colour, having converted every monochrome pointer to the latter, so the host draws
  a masked colour shape made only of black, white, clear and inverting pixels as the monochrome
  one it is ([05 §8.2](05-host.md)). **A pointer larger than 256 stays the system's to draw**:
  drawn into the pictures and reported invisible, so a guest is told the pointer is drawn and
  takes it over until it shrinks, as for any pointer the system draws. With no hardware cursor
  -- the file's setting, or a setup the system refused -- the section says so, and the pointer
  is in the pictures.
  *Built 2026-10-16:* each kind of pointer reads the same to the byte from the driver as from the
  system's duplication of the display, and **the secure desktop keeps the hardware cursor** --
  the pointer brought up there by Ctrl+Alt+Del was the driver's, visible, measured. The
  duplication says a pointer over 256 hidden as well. The worker reads the cursor between
  pictures: a read costs 4 to 5 us at the median, so a picture's acquire waits behind one at most
  that long.

### §1.8 Lifecycle

- A swapchain comes with a device on the GPU it renders on; the driver sets the device first and
  does the rest after. A device that cannot be made abandons the swapchain; a swapchain that
  refuses its device, or is lost, ends its worker; either way the system assigns another. The
  swapchain is deleted on every exit of its worker.
- **The system takes back the first one to three swapchains after a mode is set** -- unassigned
  before the device is set, or lost at the first acquire -- and assigns another within about a
  quarter of a second; each ends and is deleted at once.
- An unassignment takes the worker from its monitor under the lock the callbacks share and waits,
  outside it, for the worker to delete its swapchain; an assignment first ends a worker still
  there, so a deletion that calls the unassignment back would find nothing. A replacement monitor is
  never created inside the old one's unassignment.
- A display switched off takes the monitor's path out of the topology; the host keeps the display
  awake while it captures.
  *Corrected 2026-10-16, at W3.5's review:* **a display asleep keeps the monitor's output** on
  the desktop and the monitor active, and loses its pictures: the system took the driver's
  swapchain away about five seconds before it said the display was off, and assigned another as
  it woke, measured. A request to keep the display on holds it on but does not wake it; input
  does ([05 §2](05-host.md)).
- An update of the driver: the host closes its handle when the driver asks to be removed, and
  opens it again when the driver is back. *Amended 2026-10-16, at W3.3's build:* **this is not
  optional.** A restart of the device while a handle on it stayed open was refused, and from then
  on the system answered every restart, and so every update, with a reboot still to come, nothing
  open any more. *Corrected the same day, after a restart of the machine:* not every update -- an
  update still restarted the device onto the new driver while it waited for a reboot; a restart
  asked of the device alone is what is refused until then. **An update restarts the other virtual
  displays' devices too**, two other vendors' here, every time, and one held open by its own
  software refuses: the update is then recorded as waiting for a reboot, though this driver's
  device was restarted onto the new driver at once. And a driver whose process ends is started
  again by the system about five times in a few minutes, not a sixth: the device stays failed
  until it is restarted. Its pictures' reader is stranded by neither, measured: a conversion
  queued behind the fence of a picture published just before the driver's process ended, five
  times, or its device was restarted, twenty times, finished every time; restarted, the output
  was gone in 160 to 206 ms.
  *Amended 2026-10-16, at W3.5's review:* **the controller does this for every user** -- the
  host's display, `lowlatd display hold`, an application's third half: it registers for its
  handle's notifications and closes the handle when the driver is asked to be removed, the
  monitors bound to it going with it, as they would with the driver; it closes it too when a call
  finds the driver gone, its process ended, which leaves the handle reaching nothing. Every call
  after answers that the driver is absent, and a new open is a new handle, so what was bound to
  the old one is never taken for the new one's. The host then opens one and plugs its display
  again, also while its stream waits for a display ([05 §2](05-host.md)).
  *Built 2026-10-16:* the device restarted with a handle open and a monitor bound to it went
  through in 87 ms -- before, refused, and the device left waiting for a reboot -- the handle
  answering absent at once and a new one plugging again 178 ms after. A driver whose process
  ended gives no notification first: the old handle's next request is answered that the driver's
  process ended, within 2 ms, and closes it then. The host's own display was plugged again 139 ms
  after a restart and 283 ms after the process's end; updated twice under a stream, the driver
  was installed with no reboot owed and the guest kept, about 0.9 s without pictures, most of it
  the stream's wait for its once-a-second look at its display.
- A GPU reset reassigns every swapchain; no monitor departs during one.
  *Measured 2026-10-16, at W3.5's review:* **the GPU's driver restarted, the system assigned the
  next swapchain on the same GPU by itself**, under the GPU's new identity, 3.5 s after the last
  was lost twice and 17.5 s once -- the first restart since boot, the GPU itself back that late
  -- with no new preference from the driver, so the driver names none. For about 1.5 s
  meanwhile the monitor's output was listed under the integrated GPU; the monitor stayed plugged
  and active, and its name stayed while the panels' changed.
  *Checked 2026-10-16, at W3.5's build:* restarted under a stream, the host's display stayed
  plugged and the stream read it straight from the driver again on the GPU about 3 s after it saw
  its device lost. Rendered on the integrated GPU on purpose and back, the stream waited for it
  and went on, where before it ended the guests at once.

### §1.9 The controller, and people

The controller is the driver's control interface from user mode, behind the library's third half
([06 §3c](06-api.md)), the host's display and `lowlatd display`. **Every call waits for the
system, within about three seconds**: a plug until the monitor is active at its mode, a mode
until it is current, an unplug until the monitor has gone; past the bound a status of its own
says the monitor is plugged and the system has not finished. Calls on one handle take turns.

`lowlatd display` drives the same controller, so a person with no program of their own manages
displays from a shell: `list`; `add WxH[@Hz] [--restore] [--slot N]`; `hold WxH[@Hz] [--slot
N] [--for SECONDS]`, a bound monitor for as long as the command runs; `remove SLOT`; `mode SLOT
WxH[@Hz]`; and the file's settings, `modes [add|remove WxH@Hz]`, `gpu [auto|VVVV:DDDD]`,
`pointer [hardware|drawn]` and `edid [FILE|none]`. A display a person adds is kept, since a
command's process ends with it. A tray is later.

## §2 Input

**Deferred.** A HID device of the host's own for the guests' mice, keyboards and pads, whose input
the system takes as a device's rather than a program's. Until it is planned, input on Windows is
[05 §7](05-host.md)'s: the system's batch input call, and pads on a virtual bus as Xbox 360
pads.

## §3 Verification status

Planned 2026-10-15. **The driver and its package are built and checked live** (W3.0,
2026-10-15): installed silently with no test mode; a bound monitor plugged by an unelevated
program, made active and composing on the GPU the system named, at 1920x1080 at 60 and 2560x1440
at 120; unplugged by a request and by its handle's end, a killed process's included. **The controller is
built and checked live** (W3.1, 2026-10-15): kept and bound monitors from an unelevated shell
and through the C interface, a mode set listed and unlisted, a bound monitor refused to another
program and gone with its holder, a kept one back at its saved mode and place after the driver
restarted and again after a reboot, the file's settings changed. **The host's own display is
built and checked live** (W3.2, 2026-10-16): plugged by the host, bound to it, held at its mode,
sized at an owner's join and streamed through the system's duplication of it. The
pictures' hand-over (§1.6) and reading the pointer (§1.7) are not built. *The hand-over probed
2026-10-16, at W3.3's review*: the system's buffer shared, opened and read by the system account's
own device in another process, three buffers in turn, a picture intact about three refreshes.
**The hand-over is built and checked offline** (W3.3, 2026-10-16): the page, its publication
model-checked, the host's display reading it as a second source, the freshness check and a
still desktop's picture each shown working and broken, the driver's process ended and its device
restarted mid-stream stranding nothing, two readers at once both served, the frame thread left at
normal priority by measurement; the host's display streams it from W3.4. **The pointer from
the driver is built and checked** (W3.4, 2026-10-16): published beside the pictures and
model-checked, each kind drawn as the duplication draws it, read on whatever desktop takes the
input, the secure one included; the host's own display read straight from the driver as the
system account, and duplicated as any other user. **The lifecycle is built and checked**
(W3.5, 2026-10-16): a controller's handle follows its driver away and an update goes through
under a running host with no reboot owed; the host's own display comes back after its host,
its driver or its GPU went, after the display slept and after the machine slept, beside a panel
and as the machine's only display.
W3's gate ([impl-plan-windows.md](impl-plan-windows.md)) is the first check of the whole, and its
comparison decides whether the host reads pictures as §1.6 describes or through the system's
duplication of this display.
