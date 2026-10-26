# 11 - Drivers

**Status:** planned 2026-10-15, at W3's interview; the display's driver and its package built
the same day (W3.0), and its controller (W3.1); W3.2 to W3.5 built and checked 2026-10-16, and
the controller as a program of its own (W3.6) and the review's fixes (W3.7) built 2026-10-17;
every monitor read straight from the driver, and the desk's settings of the host's display left
to it (W3.8), built and checked 2026-10-20, and the turn of the host's own display (W3.9)
built and checked live the same day; a guest's size and turn of any display put by `lowlatd`
(W3.10) built and checked live the same day; a program's own mode on the host's display (W3.11)
built and checked live 2026-10-21, and a display read from its driver paced on the period it is
handed pictures at (W3.12) built and checked live the same day; W3's gate to come. The input
device (§2) planned 2026-10-22, at W4's interview; the driver built the same day (W4.0), and the
host's keyboard on it with the switch 2026-10-26 (W4.1). Built by
[impl-plan-windows.md](impl-plan-windows.md) W3 and W4,
where the decisions and their reasons are recorded; this document is the design they add up to.

On Windows the host carries drivers of its own where the system offers no interface for what a
host needs: a display, for a machine with no monitor and for pictures the host reads without the
system's own duplication pass (§1), and an input device for the guests' keyboards, mice and Sony
pads, whose input the system takes as a device's (§2). Both are user-mode drivers: a library the
system's user-mode driver host loads, the system's own kernel components beneath it, nothing of
this project's in the kernel. The display's C interface is the library's third half
([06](06-api.md)), and the input device has none, the host being its one user; the host's side of
the display is [05 §2](05-host.md), of the input device [05 §7](05-host.md); installing and
signing them is [07 §10](07-platforms.md). *Amended 2026-10-22:* **each is installed, updated and
removed by a script of its own, which the machine's administrator runs**, never by the host or its
service, and whatever the host does on a driver needs it installed first.

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
  normal meanwhile. Idle, the two are alike. *Amended 2026-10-17:* **the frame thread runs at
  the highest priority of its class after all**, below the compositor's. The measurement above
  timed the acquire, where the wait's boost wins; after it, the fence's signal and flush ran late
  beside busy threads of its own priority -- with the host at its high class, under one busy loop
  per logical processor, about one picture in a hundred reached the host 1.5 ms later, a tail the
  system's duplication of the same display did not have. Raised, none did, and the direct frame
  read at or below the duplication at every percentile ([impl-plan-windows.md](impl-plan-windows.md),
  the gate's comparison).
- The floor is Windows 10 1903, the first release whose display class lets a driver name the GPU
  to render on.
- Its log goes to the debugger stream: the driver's host process has no console and no file it
  may write.
- *Added 2026-10-17, at W3.6:* **its files are named `lowlat-idd`** -- the library
  `lowlat_idd.dll`, its setup information and catalogue `lowlat-idd.inf` and `lowlat-idd.cat` --
  while the device keeps its hardware ID `Root\lowlat-vdd` and its service, and the controller's
  program is `lowlat-vdd` (§1.9): a program's debug symbols take its name with underscores, so a
  library of the same name would share one file with it. A person sees the adapter as "Lowlat
  Virtual Display", from the provider "Lowlat".

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

*Measured 2026-10-17, at W3.7:* the driver's process admits its own identity alone, in full;
administrators may only query it, and nothing below the system's integrity level may read or
write it. The account it runs as is not in its access list, so another service of that account
is refused it too. The system account opens it through the debugging privilege, as above.

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

*Amended 2026-10-17, at W3.6:* the generated EDID names each monitor **"LowlatDisplay"**, the
thirteen characters its name field holds, where it said "lowlat" before. A different EDID is a
different monitor to the system, so the driver that brought the name made every slot a new
monitor once, with nothing of the old one's arrangement.

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
file, nothing in it able to stop the driver starting. The folder, made by the driver's install
script, lets users change it, so an unelevated program keeps a display, and lets the driver's
own account read it; the host's configuration beside it stays readable by the system account and
administrators alone ([07 §10](07-platforms.md)). The GPU is named by vendor and device, since
an adapter's identifier changes across boots (§1.7).
*Amended 2026-10-25, at W3.13 ([impl-plan-windows.md](impl-plan-windows.md)):* **or by its maker,
or as the power-saving one.** A maker's number alone (`gpu 10de`) takes the first GPU of that
maker in the system's high-performance order, so a card replaced by another of its maker's keeps
the choice; `gpu igpu` takes the first GPU in the system's minimum-power order, the integrated one
where a machine has one. A choice with nothing here falls back to the high-performance GPU with a
warning, and a driver from before either reads the line as one it cannot read and keeps that
default. The commands take a maker by name -- `nvidia`, `amd`, `intel` -- and keep its number. The
library's settings say an exact GPU alone: a maker's choice or the power-saving one reads as zero
there, and written back as read it stands.

### §1.5 Modes

- **A monitor's modes are fixed when it arrives**: a built-in list of common sizes at common
  rates, the custom modes, and the mode the plug asks for, kept within about ninety entries -- a
  monitor with around a hundred has been seen not to arrive. The preferred one is the plug's.
- **A listed mode is set through the system's display configuration.** A kept monitor's is saved
  in the system's database of arrangements, as any monitor's is, so it comes back after a reboot
  where it was; a bound monitor's never is. **An arrangement in which only virtual displays would
  be active is never saved**: one that only a virtual display can satisfy holds a later boot
  waiting for the driver. *Amended 2026-10-17, at W3.7:* a display counts as virtual when the
  kernel calls its adapter a virtual display's, whatever connection its driver reports --
  another vendor's virtual display reports DisplayPort or HDMI as readily as this one.
- **A mode not listed is a replug of that monitor alone**, with the mode added first, then set
  once the replacement is ready. A request for a monitor being plugged again is refused as busy,
  and asked again (*corrected 2026-10-17*: first written as coalesced into one more, which was
  never built), and a replug waits until the old monitor's swapchain is torn down. Such a mode
  is not kept; a custom mode is. Updating a live monitor's list in place leaves the system's
  cached modes stale, so it is not done.
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
  *Amended 2026-10-20, at W3.8:* **a monitor reports whether it is turned**, its mode the
  monitor's own either way: the system's configuration keeps a turned display's source at the
  sides the monitor lists and puts the turn on its path, where the desktop's pixels have the sides
  swapped -- measured, a quarter turn of 1920x1080 read back 1920x1080 with the turn on the path
  and 1080x1920 as the desktop's pixels. **A plug may ask for a size alone**: the refresh the
  system restores with the set's arrangement -- the last its display settings saved -- stands,
  and the plug's own is put where it restores none or another size.
  *Amended 2026-10-20, at W3.9:* **a monitor is turned through the system's configuration** --
  its path's rotation changed, every mode supplied as it is -- unsaved for a bound one, saved
  for a kept one as a mode is; measured, accepted both ways, leaving what a turn at the desk
  leaves. **And a mode is put on a turned monitor at the desktop's sides**: the desktop's own
  interface lists a turned display's modes with their sides swapped and refuses one put at the
  monitor's own (a 1920x1080 mode on a quarter turn refused, 1080x1920 taken with the turn
  kept), so the controller swaps them there; before, a mode put on a turned monitor replugged it
  and lost the turn.
  *Amended 2026-10-20, at W3.10:* **any display of the desktop takes a size and a turn by the
  system's name for it** -- a panel, another vendor's virtual display -- the same two puts,
  unsaved: a size at the refresh the display runs where it lists the size, else the highest the
  size lists, and refused where it lists none. `lowlatd` puts a guest's this way, and a kept
  monitor's through the controller, which adds a size it does not list
  ([impl-plan-windows.md](impl-plan-windows.md) W3.10).
  *Amended 2026-10-21, at W3.11:* **the controller reads the mode the system's store holds for a
  monitor** among the monitors connected -- what the system puts back on it as another monitor
  arrives or a program leaves exclusive fullscreen.
- **The host's display holds its mode** for as long as the host runs: another monitor's arrival
  and a program leaving exclusive fullscreen each put a saved mode back on a monitor, so the host
  puts its own back whenever the system's differs.
  *Amended 2026-10-20, at W3.8:* **its size, and its refresh only where one is asked** -- the
  configuration's, or a frame-rate ceiling, a guest's included ([06 §3](06-api.md)). With none,
  the refresh is the display's own: it is plugged for a size alone, and whatever its display
  settings set after stands, the stream pacing from it. **Turned at the desk, it is left as it
  is**: nothing is put on it while it is turned, and it streams through the duplication
  ([05 §2](05-host.md)). Before, a refresh set in its display settings was put back at once.
  *Amended 2026-10-20, at W3.9:* **a turn is never held, and a mode is put keeping it** -- the
  controller now takes a turned monitor's modes at the desktop's sides. A turn a guest asks is
  put once ([06 §3](06-api.md)), as one at the desk is; whichever turn came last stands.
  *Amended 2026-10-20, at W3.10:* put by `lowlatd` by the display's name, as the desk turns it,
  and only an owner's; the library itself turns no display.
  *Amended 2026-10-21, at W3.11:* **held against the system's restore alone.** A program in
  exclusive fullscreen at a size of its own changes the display's mode, measured, and putting the
  host's back over it fought the program -- one that takes its size back flapped the display twice
  a second. So the mode is put as it is asked, and after that only where the display is found at
  the mode the system's store holds for it among the monitors connected, which is what another
  monitor's arrival and a program leaving fullscreen put back. Any other mode stands, and the
  stream follows it: a program's in exclusive fullscreen, one set from outside and not saved. A
  program asking for exactly the store's mode cannot be told from a restore, and is put over.
  *Amended 2026-10-24, at W3's gate:* **a mode the system refused is asked again while a stream
  runs**, every two seconds and one short attempt each time, until it takes or another is asked.
  The mode was otherwise asked only as a stream was built: a new session's logon screen refused
  an owner's size for seconds after a logoff, measured, and the owner signed in at the display's
  configured size, though the same mode was put at once from the signed-in desktop. An attempt
  waits no more than a tenth of a second, on the stream's own thread between two pictures, so a
  system that goes on refusing costs a stream one short pause every two seconds.

### §1.6 Pictures

The driver converts nothing: formats, depth, full chroma and scaling stay the host's, so a change
to them never re-signs the driver. The display is standard dynamic range, and every buffer is
eight-bit BGRA. **The host reads the system's own buffer** ([05 §2](05-host.md)) -- *amended
2026-10-20, at W3.8:* of any of the driver's monitors it captures, not its own display's alone,
**while the monitor stands upright**, since the driver is told nothing of a turn:

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
  *Amended 2026-10-21, at W3.12:* **the driver is handed the first composition after each refresh
  of its monitor's own**, at most the mode's rate -- 60 a second at 60 under a 120 Hz clock,
  which composes 120 -- so the host paces a monitor read from it on the slower of the two
  ([05 §2](05-host.md)).
- **The host takes the latest picture**, waits for the fence on its device and converts into
  the encoder's input; after queuing the conversion it checks the driver's sequence, and when the
  driver has acquired as many newer pictures as the swapchain has buffers less one, the buffer may
  have been written again, so the latest is converted instead, and counted. *Amended 2026-10-19:*
  on an AMD GPU the encoder reads the buffer itself and converts it in its own hardware
  ([05 §3](05-host.md)), so the check follows the picture's collect, and a buffer written again by
  then is counted and the next picture made a keyframe. *Amended 2026-10-19, at W2.7's build:*
  at eight bits only -- a ten-bit stream converts as before. The buffer goes to the encoder once
  the fence has the picture's value, waited for on the processor: the device's wait orders its own
  queue, not the encoder's engine. The check runs once the engine is done -- the buffer written
  again, and the newest picture acquired before the encoder's last picture came out -- so a read
  in time is never counted: none under a load at 120 refreshes, 19 of 20 with each picture handed
  over 30 ms late.
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
*Amended 2026-10-17, at W3.7:* that status names the monitor's slot, and a plug or a mode
answering it hands back the monitor as it stands. **Nothing of a refused request stays behind**:
a plug the system's display configuration refuses is unplugged again, its line in the file with
it, as one whose line could not be written already was. And an unplug removes the line first,
so a monitor the driver then fails to unplug stays plugged but no longer comes back, where the
other order brought a removed one back at the driver's next start.

`lowlatd display` drives the same controller, so a person with no program of their own manages
displays from a shell: `list`; `add WxH[@Hz] [--restore] [--slot N]`; `hold WxH[@Hz] [--slot
N] [--for SECONDS]`, a bound monitor for as long as the command runs; `remove SLOT`; `mode SLOT
WxH[@Hz]`; and the file's settings, `modes [add|remove WxH@Hz]`, `gpu
[auto|igpu|nvidia|amd|intel|VVVV|VVVV:DDDD]`, `pointer [hardware|drawn]` and `edid [FILE|none]`.
A display a person adds is kept, since a command's process ends with it. A tray is later.

*Added 2026-10-17, at W3.6:* **the program `lowlat-vdd` takes the same commands**, through the
same code, for a machine that runs no host; and a library built with the third half alone is the
C interface for an application that wants displays and nothing else ([06 §3c](06-api.md)). Both
ship with the host. A package of their own, with a command that installs the driver, waits for
a release signature ([07 §10](07-platforms.md)).

## §2 Input

*Planned 2026-10-22, at W4's interview. Built the same day (W4.0), installed and checked on the
development machine 2026-10-23; the host's keyboard on it built 2026-10-26 (W4.1).* HID devices of
the host's own for
a guest's keyboard, mouse and Sony pads, whose input the system takes as a device's rather than a
program's: a low-level hook sees no injected flag on it, raw input names the device it came from,
and it reaches the input desktop by itself, the secure one included. Until it is built, and
wherever it is not switched on, input on Windows is [05 §7](05-host.md)'s: the system's batch
input call, and pads on a virtual bus as Xbox 360 pads. The Xbox 360 pad stays on the bus even
then, since a HID pad never reaches the system's Xbox pad interface.

### §2.1 The device

- One root-enumerated device of the system class, `Root\lowlat-hid`, "Lowlat Virtual Input"
  from "Lowlat": a user-mode filter above the system's virtual HID framework, which makes each
  HID device the driver asks for. The system's own drivers claim those as they claim real ones --
  the keyboard driver, the mouse driver, the game controller class -- and the framework's kernel
  half is the system's.
- Written in Rust in this workspace, as the display's driver is (§1.1): the user-mode framework's
  and the HID framework's declarations generated once from the pinned kit's and the SDK's headers
  and committed, a set of its own so the display driver stays as it is; a callback never unwinds
  into the system's code. Its files are `lowlat_hid.dll`, `lowlat-hid.inf` and `lowlat-hid.cat`;
  what it shares with the host is `lowlat-hid-proto`, which has no platform code.
- Whether it runs in a host process of its own or in the pool the system keeps for HID drivers
  at a raised priority is measured (W4.0): the write it takes is on the path of every input.
  *Decided 2026-10-23, at W4.0's measurements:* **a host process of its own, which the driver
  raises to the high class as it loads**, a class that takes no privilege. At the normal class,
  under one busy loop per logical processor and 1000 reports a second, a write now and then
  waited 10 to 15 ms and 11 to 18 mouse reports in 4,000 were lost; at the high class the worst
  was 0.6 ms and none were lost. Idle, the two are alike, and the host's work is about 40 us of
  processor a report either way. The pool would share a process with other vendors' input
  drivers.
- Its log goes to the debugger stream, as the display's does.
- Installed, updated and removed by a script of its own, which the machine's administrator runs,
  as the display's driver is (*amended 2026-10-22*); nothing of the host installs it, and the
  host only opens it, refusing `virtual_input` and giving the Xbox pad where it is absent
  ([07 §10](07-platforms.md)).

### §2.2 The control interface, and who may open it

- **The system account and administrators alone.** A device's input reaches the secure desktop
  and its elevation prompts, and lands on the console session's input desktop whoever made it, so
  a user who could open the driver could type into a consent prompt or into another person's
  session. The display's control interface admits interactive users (§1.9); this one does not.
  An administrator matches only elevated.
- **No C interface.** The host is the driver's one user, and an application embedding the host
  half reaches it through the host; an application that holds its own pads takes their reports
  through the pad sink ([05 §7.2](05-host.md)) and keeps its own device.
- **A handle makes one device, of a kind it names**: a keyboard, a mouse, a DualShock 4 or a
  DualSense. The descriptors and identities are compiled into the driver and the host alike and
  never sent, so nothing describes a device to the driver; a descriptor's change re-signs it. A
  pad's handle gives its feature table as the pad is made.
- **A report is a write of the report's bytes** on the device's handle; what the system writes to
  a pad comes back through a read the host keeps outstanding. The handle is opened for overlapped
  requests: one that is not serializes them, and the outstanding read would hold every report
  behind it.
- **Three requests** (*built 2026-10-22*): the version, which the host asks first and refuses
  another's; the create, naming the kind and its slot -- a guest's for the keyboard and the mouse,
  a guest's pad's for the pads -- with up to four feature reports of up to 64 bytes beside the
  kind's own; and the read of what the system wrote, kept outstanding. Each is a fixed sequence of
  little-endian words with its own length first, parsed field by field and never cast.
  **Every write is checked before the system sees it**: an input report the kind declares,
  whole, never the keyboard page's power key and never a button on the mouse's absolute
  collection. At most 96 devices: every seated guest's keyboard and mouse and four pads each.
- **The handle's end removes its device**, a killed host's included, **after the driver submits
  the device's neutral report** -- no key down, no button, the sticks centred -- so a host that
  dies leaves nothing held. Nothing keeps a device alive by traffic, as nothing keeps a monitor
  alive (§1.3). *Amended 2026-10-22, at W4's review:* **each kind's neutral report is its own**,
  defined beside its descriptor and checked against it: a pad's d-pad is a hat whose released
  value lies outside its range, so a report of zeroes would hold it up and pull both sticks into a
  corner, and the mouse's moves nothing. The host closes its handles when the system asks for the
  device, as the display's controller does (§1.8), so an update or a removal of the driver waits
  on nothing of the host's. (*Built 2026-10-26, W4.1:* a request in flight on a handle finishes
  first, and a handle whose driver's process ended is closed at the next request; every request
  after answers that the driver is absent, and the guest's keys go through the batch input call.) *Measured 2026-10-23:* a game's handle still open on a pad does not
  hold the device back -- the host's close took 51 ms, the game's pending read answered with the
  pad at rest -- but the driver's removal then finishes at a reboot: the device and its packages
  go and an install works at once, on a new root device, while the old driver host stays until
  the reboot. *Amended 2026-10-23, at W4.0's measurements:* **the device goes 50 ms
  after its neutral reports.** The system's keyboard path drops a report still on its way when
  the device is deleted: raw input saw a key go down and never come up, though the system's own
  key state and its hooks saw the release -- a game reading raw input kept the key held, and a
  release the host wrote just before closing was lost the same way. With 20 ms before the delete
  every release arrived; the mouse's and the pads' last reports arrived either way. *Amended
  2026-10-26, at W4.3's review:* **the wait is the keyboard's alone**, since nothing else needs it
  and a pad's end held the guest's loop for it at every unplug. *Corrected 2026-10-26, at W4.3's
  build:* **every kind waits again.** A mouse deleted at once, before the system had finished
  setting it up, held the next device's removal for about five seconds -- the keyboard let go of
  beside it took that long to leave -- and the wait covers that. A pad's end goes off the guest's
  thread instead, so no unplug holds the loop for it.

### §2.3 The devices

- **The keyboard**: a bitmap of the keyboard page with its eight modifiers, so no number of keys
  held at once is too many, and a consumer collection for mute, volume and the media keys. No
  system control collection, and the keyboard page's power key never set, so the power, sleep and
  wake keys stay out of a guest's reach; no lock lights. The system's own keyboard driver
  translates it, so Pause, Break, PrintScreen and SysRq take the forms a keyboard plugged into the
  machine gives them, each keyboard keeping its own modifiers. **It never types the attention
  chord** ([05 §7.3](05-host.md)).
- **The mouse**: a relative collection -- sixteen-bit deltas, five buttons, the wheel and the
  horizontal wheel through a resolution multiplier, so the peer's own units pass whole -- and an
  absolute one. **Every button and wheel rides the relative collection, and the absolute one
  carries positions alone** (*amended 2026-10-22*; it was the report of the pointer that moved
  last), its buttons declared and never pressed. Each collection is a pointer of its own to the
  system, with its own buttons: a release sent on the other would leave the button held there,
  and a report of the absolute collection carries a position, so a button sent on it after
  relative motion would move the pointer back to where the absolute one last was. With every
  button on one collection neither can happen, and a drag held while the client turns its pointer
  relative goes on. It rests on the system keeping a move on one collection before the click that
  follows on the other, measured at W4.0; where it does not, a button rides the absolute report at
  the pointer's place read back. Where the absolute range lands with more than one display is
  measured at W4.0. *Measured 2026-10-23:* **the move stays ahead of the click**, 300 of 300 pairs
  written back to back, idle and under one busy loop per logical processor; and **the absolute
  range spans the primary display alone**, a value v landing on its pixel floor(v x W / 32768) in
  its own pixels at 100 and 125 percent alike, so an absolute position on any other output goes
  through the batch input call, the relative motion, buttons and wheels staying on the HID mouse
  (the maintainer's decision). *Built 2026-10-26 (W4.2):* the host takes a position the
  expansion spread over the whole desktop back to the pixel the batch input call would put the
  pointer on, by the system's own layout, and gives the primary's the collection's value for
  that pixel's centre; motion past a report's range is split over as many reports as it needs,
  the held buttons in each.
- **The DualShock 4 and the DualSense**: the real products' descriptors and identities, as the
  Linux host presents them ([07 §4.2](07-platforms.md)). The driver answers a feature read from
  the pad's table -- the peer's calibration and firmware, else a real pad's, and a pairing report
  around an address the host makes up, unique per guest and pad -- and refuses an identifier
  nobody defined. What the system writes to the pad, an output report or a feature write, is
  completed at once and kept for the host, a few deep and the oldest dropped, each being a whole
  state; a read of the input report answers the last one submitted. No audio: the pads present
  their HID interface alone. *Amended 2026-10-26, at W4.3's review (the maintainer's decision):*
  **a feature write is answered and kept by no one**: none of it reaches the host, so none of it
  travels -- a pad's features carry its pairing, test commands and, on a DualSense, its firmware
  update, none a game needs, and the peer applying one would change the guest's own controller.
  The mouse's multipliers stay kept, which the system reads back. **An output report is not a
  whole state**: a DualSense's names, by its flags, the parts it changes, so a dropped one can lose
  a lightbar or a trigger effect for good; the driver keeps 32 for the host, and one dropped is
  counted and said. **The pads' version is 0x0100, as the real ones read on Windows**; the Linux
  host presents its own platform's. *Measured 2026-10-26 (W4.3):* **an output report reaches the
  driver as the interrupt write alone**: sent as a set-report, the framework refuses it as not
  supported, so a program writing a pad's output that way gets an error and the guest nothing.
  The controller libraries, the game launcher and a browser write it the other way.
- **Identities**: each device's instance is stable per kind and slot, so the system sets a slot's
  device up once rather than at every connection; the keyboard and the mouse carry an identity of
  the project's own, never a real vendor's. *Built 2026-10-22:* in development the open-source
  projects' shared vendor and two of its test products, `1209:0001` and `1209:0002`, an
  identity assigned to the project before a release; the instance names the kind and the slot,
  and each kind and slot has a container of its own. The mouse answers its multipliers' feature
  report itself and keeps what the system sets there. *Amended 2026-10-23:* **the driver names no
  instance; the framework names each device itself.** Two devices of one name stop the machine
  rather than being refused, and a run of W4.0's checks did: the name reached the framework a
  character short, so two slots of one ten shared it. A kind and slot is still one device at a
  time, in a container of its own. The framework gives the first device of a kind the same name at
  every connection, so the system sets it up once; a second of the kind at the same time gets a
  new name, and a setup, each time. *Measured 2026-10-26:* two DualSenses at once, five times,
  left ten device nodes behind, and the second was ready in 12 to 47 ms; kept as it is.
  *Seen 2026-10-26:* **no device carries a name of its own** --
  the framework takes no product string from its driver and asks the driver for none, so the
  system answers every device's with the framework's, "HID VHF Driver", as a browser's HID
  listing shows the keyboard and the mouse. Read back from every collection: the product "HID VHF
  Driver", the maker "Microsoft" and the serial "1.0", with the device's own identity in its
  attributes and in its hardware identifiers (`HID\VID_1209&PID_0001&Col01`); **its device path
  names the framework, never the identity** (`hid#hid_device_system_vhf&col01#...`). What
  consumers key on is the identity and the descriptor; a name of each device's own would need a
  HID driver of the host's own in place of the framework. For the pads, whose consumers might
  read either, it is a check of their own (W4.3). *Checked 2026-10-26, at W4.3's review:* the
  controller libraries' HID layer, the common game launcher, the system's game controller panel
  and a browser's gamepad interface read the pads by their identity -- named as the products,
  the standard mapping, everything written back -- and the framework's string shows only in a
  browser's HID chooser and in the text its gamepad interface names a pad by. The panel's name is
  what the system keeps for the identity once a real pad was installed, so a machine that never
  had one may show the framework's.

### §2.4 Lifetimes

- **A guest's keyboard and mouse exist while the application's switch is on**
  (`virtual_input` in the host's input configuration, [06 §3](06-api.md)): made for each seated
  guest on its next pass after the switch, or at its admission while the switch is on, and
  removed when the switch goes off or the guest leaves. A switch either way first releases
  everything held on the old path.
- **A pad sent as its reports is made at its first report while the owner has `virtual_pads`
  on**, a change reaching the pads plugged after it, and removed at its unplug or its guest's
  leaving, as on Linux. *Amended 2026-10-26, at W4.3's review:* a pad keeps its reports when the
  setting goes off, which decides a new pad's family alone; and **a pad whose device never comes,
  or goes, plays on as the Xbox pad from its own reports**, so no pad does nothing.
- **A device made is not yet a device that delivers**: input for it is held until it does, as on
  Linux ([07 §4.1](07-platforms.md)), and the time, with whatever announces it, is measured at
  W4.0. *Built 2026-10-26 for the keyboard (W4.1):* held until raw input lists the device, which
  W4.0 measured to predict delivery, and through the batch input call where that has not happened
  within five seconds. A guest's keyboard takes the first slot the driver gives -- a slot another's
  keyboard holds is refused as busy -- and is let go of off the guest's thread at a switch, its
  end waiting while its neutral reports arrive. *Built the same day for the mouse (W4.2):* the
  same, beside the keyboard, its slots its own kind's. *Built 2026-10-26 for the pads (W4.3):*
  the same, a pad's latest report held, and a pad not listed within five seconds played on as the
  Xbox pad. *Amended 2026-10-26, at W4.3's checks on the machine:* **a device counts as listed
  only with every collection listed under a raw input handle of its own.** The device before it
  in its slot had its name and container, and where the slot was used again soon raw input went
  on listing that one after it had gone -- about half a second for a keyboard, a second for a
  mouse -- so the next device, taken as listed at once, lost its first reports for as long; each
  new device came under handles raw input had not given before. A mouse's absolute collection was
  listed a few milliseconds after its relative one, and a position written between was lost.

## §3 Verification status

**The input device is planned** (W4, 2026-10-22): nothing of it is built, and W4.0's checks and
measurements are its first. *Built 2026-10-22* (W4.0): the driver, its setup information and its
package, signed with the test certificate, and the host's side of its interface. Run so far,
needing no installed driver: each kind's descriptor read against the code that writes its reports
and against its neutral report, the requests read and refused, each such check broken once. Owed:
the install by its script, the checks on the machine and W4.0's measurements. *Built 2026-10-26*
(W4.1): a guest's keyboard on the driver behind the application's switch, the attention chord's
rule, the host's handles following the driver away. Run, needing no driver: the chord's rule in
both orders, the reports and the hold, every key's usage translating back to its key, the
refusals' statuses, each broken once. Owed: the checks on the machine and live. *Built
2026-10-26* (W4.2): a guest's mouse on the driver beside its keyboard. Run, needing no driver:
every button and wheel on the relative collection, a long move split, a position on the
primary on its pixel and one off it refused, the switch releasing keys and buttons, each broken
once. On the machine: positions on the primary on their pixels through it, and every display's
on its pixel with the switch on. Live: the pointer in a game whose anti-cheat refuses the batch
input call's motion, relative aim, both wheels, a second display's clicks. Owed: W4's gate.
*Reviewed 2026-10-26* (W4.3): the pads checked on the installed driver before the host's side is
built -- the maintainer's own pads relayed into the driver's, read as their products by the
consumers named in §2.3, everything written back whole. *Built 2026-10-26* (W4.3): a guest's
DualShock 4 and DualSense on the driver behind `virtual_pads`, each made at its first report and
answering from what its peer sent, what a game writes to it handed back, a feature write kept on
the host. Run, needing no driver: the switch both ways and a set taking both or neither, a pad
keeping its reports as the switch goes off, a pad whose device goes played on as the Xbox pad
from its reports, the table the Linux host's answers, a feature write kept on the host and
refused from an application's own device, each broken once. On the machine: a pad's feature
write answered and handed to no one, its output report back; a guest's DualSense made at its
first report, read as a game reads it, a game's output report back, gone at its unplug; the
keyboard's and the mouse's checks back to back, the second's devices made while raw input still
listed the first's. Live: an established macOS client's DualSense on a cable as its product,
everything written back. Owed: W4's gate.

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
pictures' hand-over (§1.6) and reading the pointer (§1.7) were not built yet. *The hand-over probed
2026-10-16, at W3.3's review*: the system's buffer shared, opened and read by the system account's
own device in another process, three buffers in turn, a picture intact about three refreshes.
**The hand-over is built and checked offline** (W3.3, 2026-10-16): the page, its publication
model-checked, the host's display reading it as a second source, the freshness check and a
still desktop's picture each shown working and broken, the driver's process ended and its device
restarted mid-stream stranding nothing, two readers at once both served, the frame thread left at
normal priority by measurement (*amended 2026-10-17*: raised since, §1.1); the host's display
streams it from W3.4. **The pointer from
the driver is built and checked** (W3.4, 2026-10-16): published beside the pictures and
model-checked, each kind drawn as the duplication draws it, read on whatever desktop takes the
input, the secure one included; the host's own display read straight from the driver as the
system account, and duplicated as any other user. **The lifecycle is built and checked**
(W3.5, 2026-10-16): a controller's handle follows its driver away and an update goes through
under a running host with no reboot owed; the host's own display comes back after its host,
its driver or its GPU went, after the display slept and after the machine slept, beside a panel
and as the machine's only display. **The controller as a program of its own is built** (W3.6,
2026-10-17): every command through `lowlat-vdd` and `lowlatd display` alike on the installed
driver, the same lines from both; a library of the third half alone built and linked as an
application links it, its check failing with another half forced in and with this one left out;
the driver packaged and signed under its new names, its date in UTC, and installed over the
driver it replaces in 1.2 s with no reboot owed, a kept monitor plugged again by the new one and
read by the system as "LowlatDisplay" on "Lowlat Virtual Display". The old library stays in the
system's folder for user-mode drivers, loaded by nothing. **The review's fixes are built**
(W3.7, 2026-10-17): the driver's process found to admit no other account (§1.2); a virtual
display reporting DisplayPort read as virtual by its adapter, live beside a panel read as real
(§1.5); a monitor's description handed over from its own bytes and read back by the system;
the pictures skipped counted across the frame number's wrap; an unplug held by a busy file
leaving the monitor and its line both in place, and failing with the old order (§1.9). The
unplugging of a refused plug is built but could not be forced: the system here shows a plugged
monitor before a plug returns. **Every monitor read from the driver is built and checked**
(W3.8, 2026-10-20): a monitor of the driver that is not the host's own read straight from it as
the system account and duplicated as a user; turned, duplicated with its turn declared, and a
turn under a read from the driver rebuilt (§1.6); the host's own display keeping a refresh and a
turn set at the desk, and a refresh saved there coming back with it (§1.5) -- each check failing
with its fix undone. **The turn of the host's own display is built and checked** (W3.9,
2026-10-20): a turned monitor's driver picture the duplication's to the sample, its pointer's
shape turned for the panel, so a turned monitor stays on the duplication; the host's display
turned a quarter and put at another mode while turned, still turned; and live, an established
client's rotate turning it and back, the stream following each way (§1.5). **A guest's size and
turn on any display are built and checked live** (W3.10, 2026-10-20): a panel resized and turned
by its name and a size it does not list refused; the host's own display turned from outside and
put at another size still turned; and live, an owner's sizes and turns from the library's demo
put on a panel, the stream following each (§1.5). **A program's own mode on the host's display is
built and checked live** (W3.11, 2026-10-21): a program in exclusive fullscreen at a size of its
own left at it while it holds the display, and the host's size put back over the system's restore
as it leaves, the check failing on the hold as it was; and live, a guest of the host's display
following such a program's 1280x720 and back to the size its owner asked (§1.5). **A monitor read
from the driver is paced on the period it is handed pictures at** (W3.12, 2026-10-21): at 60
under the 120 Hz clock, each picture handed over 0.19 ms after the driver acquired it at the
median, live, where it had been taken a composition late, 9.3 ms (§1.6).
W3's gate ([impl-plan-windows.md](impl-plan-windows.md)) is the first check of the whole, and its
comparison decides whether the host reads pictures as §1.6 describes or through the system's
duplication of this display. *Amended 2026-10-21:* that default was taken at W3.4 and widened at
W3.8, so the comparison confirms it (amended in the gate at W3.10).
