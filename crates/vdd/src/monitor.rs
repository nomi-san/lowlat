//! The adapter and its monitors: the slots, plugging, unplugging and
//! replugging, the file's settings, and the modes the system asks for
//! (docs/11-drivers.md sections 1.3 to 1.5 and 1.7).
//!
//! **No framework call is made with the lock held.** Creating a monitor makes
//! the system ask for its modes, its departure can unassign its swapchain, and
//! a swapchain's teardown can call back again on another thread; each of those
//! takes the lock, so every call that can lead to one is made between two
//! holds of it.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

use lowlat_common::{log_info, log_warn};
use lowlat_drivers::d3d11::D3d11;
use lowlat_vdd_proto::control::{Entry, Lifetime, List, Plug, Plugged};
use lowlat_vdd_proto::edid::Edid;
use lowlat_vdd_proto::modes::{Mode, ModeList};
use lowlat_vdd_proto::settings::{Gpu, Pointer, Settings};
use lowlat_vdd_proto::{MAX_MONITORS, container_id};

use crate::ffi::{
    DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DISPLAYPORT_EXTERNAL, GUID, IDARG_IN_ADAPTER_INIT,
    IDARG_IN_ADAPTERSETRENDERADAPTER, IDARG_IN_MONITORCREATE, IDARG_OUT_ADAPTER_INIT,
    IDARG_OUT_MONITORARRIVAL, IDARG_OUT_MONITORCREATE, IDDCX_ADAPTER__, IDDCX_ADAPTER_CAPS,
    IDDCX_ADAPTER_FLAGS_USE_SMALLEST_MODE, IDDCX_ENDPOINT_DIAGNOSTIC_INFO, IDDCX_ENDPOINT_VERSION,
    IDDCX_FEATURE_IMPLEMENTATION_NONE, IDDCX_MONITOR__, IDDCX_MONITOR_DESCRIPTION,
    IDDCX_MONITOR_DESCRIPTION_TYPE_EDID, IDDCX_MONITOR_INFO, IDDCX_TRANSMISSION_TYPE_WIRED_OTHER,
    LUID, NTSTATUS, STATUS_ACCESS_DENIED, STATUS_DEVICE_BUSY, STATUS_INSUFFICIENT_RESOURCES,
    STATUS_INVALID_DEVICE_STATE, STATUS_INVALID_PARAMETER, STATUS_SUCCESS, WDFDEVICE__,
    WDFFILEOBJECT__, idd_adapter_init_async, idd_adapter_set_render_adapter, idd_monitor_arrival,
    idd_monitor_create, idd_monitor_departure, nt_success, object_attributes, size_of,
    wdf_object_delete,
};
use crate::file;
use crate::swapchain::Worker;

/// A framework handle: the name of an object the framework owns.
#[derive(Debug)]
pub(crate) struct Handle<T>(pub(crate) *mut T);

impl<T> PartialEq for Handle<T> {
    fn eq(&self, other: &Self) -> bool {
        core::ptr::eq(self.0, other.0)
    }
}

impl<T> Eq for Handle<T> {}

impl<T> Clone for Handle<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for Handle<T> {}

// SAFETY: a handle is a name for an object the framework owns and
// serialises; passing the name between threads is how its calls are made.
unsafe impl<T> Send for Handle<T> {}

impl<T> Handle<T> {
    pub(crate) const NONE: Self = Self(core::ptr::null_mut());

    pub(crate) fn is_none(self) -> bool {
        self.0.is_null()
    }
}

/// What a monitor's swapchain worker and the list share.
#[derive(Debug, Default)]
pub(crate) struct Stats {
    /// Pictures the system handed over, across every swapchain.
    pub(crate) frames: AtomicU64,
    /// The GPU the last swapchain renders on.
    pub(crate) render_luid: AtomicU64,
    /// A swapchain is assigned and its device set.
    pub(crate) assigned: AtomicBool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Free,
    Arriving,
    Live,
    /// Departed for a replug and arriving again: the slot stays taken.
    Replugging,
    Departing,
}

#[derive(Debug)]
struct Slot {
    phase: Phase,
    monitor: Handle<IDDCX_MONITOR__>,
    /// The handle that plugged it, whose end unplugs it; none for a kept one.
    owner: Handle<WDFFILEOBJECT__>,
    kept: bool,
    restore: bool,
    /// Its handle ended while it arrived: the plug departs it once it has.
    orphaned: bool,
    modes: Option<ModeList>,
    committed: Option<Mode>,
    os_target_id: u32,
    os_adapter_luid: u64,
    stats: Arc<Stats>,
    worker: Option<Worker>,
}

impl Slot {
    fn free() -> Self {
        Self {
            phase: Phase::Free,
            monitor: Handle::NONE,
            owner: Handle::NONE,
            kept: false,
            restore: false,
            orphaned: false,
            modes: None,
            committed: None,
            os_target_id: 0,
            os_adapter_luid: 0,
            stats: Arc::default(),
            worker: None,
        }
    }

    /// Whether `asker` may unplug or replug it: its own handle, or anyone
    /// for a kept one.
    fn answers_to(&self, asker: Handle<WDFFILEOBJECT__>) -> bool {
        self.kept || self.owner == asker
    }
}

#[derive(Debug)]
struct State {
    adapter: Handle<IDDCX_ADAPTER__>,
    ready: bool,
    /// The file as last read.
    settings: Settings,
    /// The GPU last named, so naming it again -- which re-creates every
    /// swapchain -- is skipped.
    named: Option<u64>,
    slots: [Slot; MAX_MONITORS],
}

impl State {
    fn new() -> Self {
        Self {
            adapter: Handle::NONE,
            ready: false,
            settings: Settings::default(),
            named: None,
            slots: core::array::from_fn(|_| Slot::free()),
        }
    }
}

static STATE: OnceLock<Mutex<State>> = OnceLock::new();

/// The state. A thread that panicked holding it ended the process, so a
/// poisoned lock is still a consistent one.
fn lock() -> MutexGuard<'static, State> {
    STATE
        .get_or_init(|| Mutex::new(State::new()))
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

/// Each slot's EDID, where the system reads it: the system may read a
/// description after the call that handed it over, so it lives as long as
/// the driver. **A slot's is written only under the lock and only while the
/// slot has no monitor**, and read only under the lock, so no description the
/// system holds ever changes under it.
struct Edids([UnsafeCell<Edid>; MAX_MONITORS]);

// SAFETY: every access is under the state's lock, by the rule above.
unsafe impl Sync for Edids {}
// SAFETY: as above.
unsafe impl Send for Edids {}

fn edids() -> &'static Edids {
    static EDIDS: OnceLock<Edids> = OnceLock::new();
    EDIDS.get_or_init(|| Edids(core::array::from_fn(|_| UnsafeCell::new(Edid::generated()))))
}

/// Set a slot's EDID. The caller holds the lock, and the slot has no monitor.
fn set_edid(_held: &State, slot: usize, edid: Edid) {
    if let Some(cell) = edids().0.get(slot) {
        // SAFETY: the lock is held and nothing reads this slot's EDID while
        // it has no monitor.
        unsafe { *cell.get() = edid };
    }
}

/// A slot's EDID. The caller holds the lock.
fn edid_of(_held: &State, slot: usize) -> Option<Edid> {
    // SAFETY: the lock is held, and writes take it too.
    edids().0.get(slot).map(|cell| unsafe { *cell.get() })
}

pub(crate) fn guid(id: lowlat_vdd_proto::Guid) -> GUID {
    GUID {
        Data1: id.data1,
        Data2: id.data2,
        Data3: id.data3,
        Data4: id.data4,
    }
}

pub(crate) fn luid_value(luid: LUID) -> u64 {
    u64::from(luid.LowPart) | (u64::from(u32::from_ne_bytes(luid.HighPart.to_ne_bytes())) << 32)
}

fn luid_of(value: u64) -> LUID {
    LUID {
        LowPart: u32::try_from(value & 0xffff_ffff).unwrap_or(0),
        HighPart: i32::from_ne_bytes(u32::try_from(value >> 32).unwrap_or(0).to_ne_bytes()),
    }
}

/// A new device in this host process: nothing of a previous one carries over.
pub(crate) fn reset() {
    *lock() = State::new();
}

const fn wide<const N: usize>(text: &str) -> [u16; N] {
    let bytes = text.as_bytes();
    let mut out = [0u16; N];
    let mut i = 0;
    while i < bytes.len() && i + 1 < N {
        out[i] = bytes[i] as u16;
        i += 1;
    }
    out
}

static NAME: [u16; 16] = wide("lowlat");
static MODEL: [u16; 32] = wide("lowlat virtual display");

/// Initialise the adapter, once per device: the device's first power-up.
pub(crate) fn start_adapter(device: Handle<WDFDEVICE__>) -> NTSTATUS {
    if !lock().adapter.is_none() {
        return STATUS_SUCCESS;
    }
    let mut version = IDDCX_ENDPOINT_VERSION {
        Size: size_of::<IDDCX_ENDPOINT_VERSION>(),
        MajorVer: 0,
        MinorVer: 1,
        ..Default::default()
    };
    let mut caps = IDDCX_ADAPTER_CAPS {
        Size: size_of::<IDDCX_ADAPTER_CAPS>(),
        // Without it the system sometimes refuses a monitor, and a desktop
        // smaller than the mode is scaled rather than given a mode of its own.
        Flags: IDDCX_ADAPTER_FLAGS_USE_SMALLEST_MODE,
        MaxMonitorsSupported: u32::try_from(MAX_MONITORS).unwrap_or(1),
        EndPointDiagnostics: IDDCX_ENDPOINT_DIAGNOSTIC_INFO {
            Size: size_of::<IDDCX_ENDPOINT_DIAGNOSTIC_INFO>(),
            TransmissionType: IDDCX_TRANSMISSION_TYPE_WIRED_OTHER,
            pEndPointFriendlyName: NAME.as_ptr(),
            pEndPointModelName: MODEL.as_ptr(),
            pEndPointManufacturerName: NAME.as_ptr(),
            pHardwareVersion: &raw mut version,
            pFirmwareVersion: &raw mut version,
            GammaSupport: IDDCX_FEATURE_IMPLEMENTATION_NONE,
        },
        // One picture again after the desktop stills; the host's own clock
        // makes any more it wants.
        StaticDesktopReencodeFrameCount: 1,
        ..Default::default()
    };
    let mut attributes = object_attributes();
    let args = IDARG_IN_ADAPTER_INIT {
        WdfDevice: device.0,
        pCaps: &raw mut caps,
        ObjectAttributes: &raw mut attributes,
    };
    let mut out = IDARG_OUT_ADAPTER_INIT::default();
    // SAFETY: live arguments for the call; the class copies the capabilities.
    let status = unsafe { idd_adapter_init_async(&raw const args, &raw mut out) };
    if !nt_success(status) {
        log_warn!(
            "vdd: adapter init refused, status={:#x}",
            status_word(status)
        );
        return status;
    }
    lock().adapter = Handle(out.AdapterObject);
    log_info!("vdd: adapter init started");
    STATUS_SUCCESS
}

/// The class's word on the adapter's initialisation. Once ready, the file
/// is read, the GPU named and the kept monitors it lists plugged, in that
/// order: the GPU before any monitor, as the class asks.
pub(crate) fn adapter_ready(status: NTSTATUS) {
    if !nt_success(status) {
        log_warn!(
            "vdd: adapter init failed, status={:#x}",
            status_word(status)
        );
        return;
    }
    let settings = read_settings();
    let adapter = {
        let mut state = lock();
        state.ready = true;
        state.settings = settings;
        state.adapter
    };
    log_info!("vdd: adapter ready");
    name_gpu(adapter, settings.gpu);
    restore(&settings);
}

/// The file, read and logged.
fn read_settings() -> Settings {
    let (settings, skipped, found) = file::read();
    if !found {
        log_info!("vdd: no settings file, the defaults");
        return settings;
    }
    log_info!(
        "vdd: settings read, gpu={} pointer={} modes={} restore={} edid={} skipped={skipped}",
        settings.gpu.map_or_else(
            || "auto".to_string(),
            |g| format!("{:04x}:{:04x}", g.vendor, g.device)
        ),
        if settings.pointer == Pointer::Drawn {
            "drawn"
        } else {
            "hardware"
        },
        settings.modes().len(),
        settings.restore.iter().filter(|m| m.is_some()).count(),
        if settings.edid.is_some() {
            "dumped"
        } else {
            "own"
        },
    );
    settings
}

/// Name the GPU the system renders on: the file's when it names one present,
/// else the first hardware GPU in the system's high-performance order.
fn name_gpu(adapter: Handle<IDDCX_ADAPTER__>, gpu: Option<Gpu>) {
    let adapters = match D3d11::load().and_then(|d3d11| d3d11.adapters()) {
        Ok(adapters) => adapters,
        Err(error) => {
            log_warn!("vdd: no GPU named, the adapters did not enumerate: {error:?}");
            return;
        }
    };
    let hardware = || adapters.iter().filter(|a| a.decodes_here());
    let named = gpu.and_then(|gpu| {
        hardware().find(|a| a.vendor == u32::from(gpu.vendor) && a.device == u32::from(gpu.device))
    });
    if let (Some(gpu), None) = (gpu, named) {
        log_warn!(
            "vdd: the file's GPU {:04x}:{:04x} is not here, the high-performance one instead",
            gpu.vendor,
            gpu.device
        );
    }
    let Some(chosen) = named.or_else(|| hardware().next()) else {
        log_warn!("vdd: no hardware GPU, the system chooses");
        return;
    };
    if lock().named.replace(chosen.luid.value()) == Some(chosen.luid.value()) {
        return;
    }
    let args = IDARG_IN_ADAPTERSETRENDERADAPTER {
        PreferredRenderAdapter: luid_of(chosen.luid.value()),
    };
    // SAFETY: the adapter the class made for this driver; live arguments.
    unsafe { idd_adapter_set_render_adapter(adapter.0, &raw const args) };
    log_info!(
        "vdd: render GPU asked, {:04x}:{:04x} '{}' {}",
        chosen.vendor,
        chosen.device,
        chosen.description,
        chosen.luid
    );
}

/// Plug the kept monitors the file lists, each at its slot.
fn restore(settings: &Settings) {
    for (slot, mode) in settings.restore.iter().enumerate() {
        let (Some(mode), Ok(slot)) = (mode, u8::try_from(slot)) else {
            continue;
        };
        let plug = Plug {
            mode: *mode,
            lifetime: Lifetime::Kept,
            restore: true,
            slot: Some(slot),
        };
        if let Err(status) = plug_monitor(Handle::NONE, plug) {
            log_warn!(
                "vdd: a kept monitor not restored, slot={slot} status={:#x}",
                status_word(status)
            );
        }
    }
}

/// Read the file again, at a controller's word: the GPU at once if it
/// changed, the rest for what is plugged after.
pub(crate) fn reload() -> NTSTATUS {
    let settings = read_settings();
    let (adapter, ready, gpu_changed) = {
        let mut state = lock();
        let changed = state.settings.gpu != settings.gpu;
        state.settings = settings;
        (state.adapter, state.ready, changed)
    };
    if ready && gpu_changed {
        name_gpu(adapter, settings.gpu);
    }
    STATUS_SUCCESS
}

/// Where the pointer is drawn, for a swapchain being set up.
pub(crate) fn pointer() -> Pointer {
    lock().settings.pointer
}

pub(crate) fn status_word(status: i32) -> u32 {
    u32::from_ne_bytes(status.to_ne_bytes())
}

fn release_slot(index: usize) {
    if let Some(slot) = lock().slots.get_mut(index) {
        *slot = Slot::free();
    }
}

/// Plug a monitor for `owner` (none for a kept one).
pub(crate) fn plug(owner: Handle<WDFFILEOBJECT__>, plug: Plug) -> Result<Plugged, NTSTATUS> {
    let owner = if plug.lifetime == Lifetime::Kept {
        Handle::NONE
    } else {
        owner
    };
    plug_monitor(owner, plug)
}

fn plug_monitor(owner: Handle<WDFFILEOBJECT__>, plug: Plug) -> Result<Plugged, NTSTATUS> {
    let (adapter, index) = {
        let mut state = lock();
        if !state.ready || state.adapter.is_none() {
            return Err(STATUS_INVALID_DEVICE_STATE);
        }
        let index = match plug.slot {
            Some(slot) => {
                let index = usize::from(slot);
                match state.slots.get(index).map(|s| s.phase) {
                    Some(Phase::Free) => index,
                    Some(_) => return Err(STATUS_DEVICE_BUSY),
                    None => return Err(STATUS_INVALID_PARAMETER),
                }
            }
            None => state
                .slots
                .iter()
                .position(|slot| slot.phase == Phase::Free)
                .ok_or(STATUS_INSUFFICIENT_RESOURCES)?,
        };
        let modes = ModeList::for_plug(plug.mode, state.settings.modes());
        let edid = state.settings.edid.unwrap_or_default();
        let slot = u8::try_from(index).map_err(|_| STATUS_INSUFFICIENT_RESOURCES)?;
        set_edid(&state, index, edid.for_slot(slot));
        let entry = state
            .slots
            .get_mut(index)
            .ok_or(STATUS_INSUFFICIENT_RESOURCES)?;
        *entry = Slot {
            phase: Phase::Arriving,
            owner,
            kept: plug.lifetime == Lifetime::Kept,
            restore: plug.restore,
            modes: Some(modes),
            ..Slot::free()
        };
        (state.adapter, index)
    };
    let (monitor, target, luid) = match arrive(adapter, index) {
        Ok(arrived) => arrived,
        Err(status) => {
            release_slot(index);
            return Err(status);
        }
    };
    if settle(index, target, luid) {
        // The handle that asked ended while the monitor arrived.
        depart(index, monitor);
        return Err(STATUS_INVALID_DEVICE_STATE);
    }
    log_info!(
        "vdd: plugged, slot={index} mode={} {} target={target} adapter={luid:#018x}",
        plug.mode,
        if plug.lifetime == Lifetime::Kept {
            if plug.restore {
                "kept, restored"
            } else {
                "kept"
            }
        } else {
            "bound"
        },
    );
    Ok(Plugged {
        slot: u32::try_from(index).unwrap_or(u32::MAX),
        os_target_id: target,
        os_adapter_luid: luid,
    })
}

/// A slot's monitor arrived: live, unless its handle ended meanwhile, which
/// this answers with true and the slot departing.
fn settle(index: usize, target: u32, luid: u64) -> bool {
    let mut state = lock();
    let Some(entry) = state.slots.get_mut(index) else {
        return false;
    };
    entry.os_target_id = target;
    entry.os_adapter_luid = luid;
    if entry.orphaned {
        entry.phase = Phase::Departing;
        return true;
    }
    entry.phase = Phase::Live;
    false
}

/// Create a slot's monitor and make it arrive, with the slot's EDID and its
/// modes already in place. On a failure the slot is the caller's to free.
fn arrive(
    adapter: Handle<IDDCX_ADAPTER__>,
    index: usize,
) -> Result<(Handle<IDDCX_MONITOR__>, u32, u64), NTSTATUS> {
    let slot = u8::try_from(index).map_err(|_| STATUS_INSUFFICIENT_RESOURCES)?;
    let (data, size) = {
        let state = lock();
        let edid = edid_of(&state, index).ok_or(STATUS_INVALID_PARAMETER)?;
        let cell = edids().0.get(index).ok_or(STATUS_INVALID_PARAMETER)?;
        (cell.get().cast::<u8>(), edid.as_slice().len())
    };
    let mut info = IDDCX_MONITOR_INFO {
        Size: size_of::<IDDCX_MONITOR_INFO>(),
        // As the EDID says.
        MonitorType: DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DISPLAYPORT_EXTERNAL,
        ConnectorIndex: u32::from(slot),
        MonitorDescription: IDDCX_MONITOR_DESCRIPTION {
            Size: size_of::<IDDCX_MONITOR_DESCRIPTION>(),
            Type: IDDCX_MONITOR_DESCRIPTION_TYPE_EDID,
            DataSize: u32::try_from(size).unwrap_or(0),
            // The EDID's bytes lead its cell; they stay put while the slot
            // has a monitor (`Edids`).
            pData: data.cast(),
        },
        MonitorContainerId: guid(container_id(slot)),
    };
    let mut attributes = object_attributes();
    let args = IDARG_IN_MONITORCREATE {
        ObjectAttributes: &raw mut attributes,
        pMonitorInfo: &raw mut info,
    };
    let mut created = IDARG_OUT_MONITORCREATE::default();
    // SAFETY: live arguments; the description outlives the monitor.
    let status = unsafe { idd_monitor_create(adapter.0, &raw const args, &raw mut created) };
    if !nt_success(status) {
        log_warn!(
            "vdd: monitor create refused, slot={index} status={:#x}",
            status_word(status)
        );
        return Err(status);
    }
    let monitor = Handle(created.MonitorObject);
    // Recorded before the arrival, which can assign a swapchain that has to
    // find its slot.
    if let Some(entry) = lock().slots.get_mut(index) {
        entry.monitor = monitor;
    }
    let mut arrived = IDARG_OUT_MONITORARRIVAL::default();
    // SAFETY: a monitor the class just created.
    let status = unsafe { idd_monitor_arrival(monitor.0, &raw mut arrived) };
    if !nt_success(status) {
        // A monitor that never arrived is the driver's object to delete.
        // SAFETY: the monitor the class created for this driver.
        unsafe { wdf_object_delete(monitor.0.cast()) };
        if let Some(entry) = lock().slots.get_mut(index) {
            entry.monitor = Handle::NONE;
        }
        log_warn!(
            "vdd: monitor arrival refused, slot={index} status={:#x}",
            status_word(status)
        );
        return Err(status);
    }
    Ok((
        monitor,
        arrived.OsTargetId,
        luid_value(arrived.OsAdapterLuid),
    ))
}

/// Unplug a monitor: one `asker` plugged, or a kept one.
pub(crate) fn unplug(asker: Handle<WDFFILEOBJECT__>, index: usize) -> Result<(), NTSTATUS> {
    let monitor = {
        let mut state = lock();
        let slot = state.slots.get_mut(index).ok_or(STATUS_INVALID_PARAMETER)?;
        match slot.phase {
            Phase::Live => {}
            Phase::Arriving | Phase::Replugging => return Err(STATUS_DEVICE_BUSY),
            Phase::Free | Phase::Departing => return Err(STATUS_INVALID_PARAMETER),
        }
        if !slot.answers_to(asker) {
            return Err(STATUS_ACCESS_DENIED);
        }
        slot.phase = Phase::Departing;
        slot.monitor
    };
    depart(index, monitor);
    Ok(())
}

/// Plug a monitor again with a mode its list lacks, first and preferred: the
/// same slot, the same identity, the slot held throughout.
pub(crate) fn replug(
    asker: Handle<WDFFILEOBJECT__>,
    index: usize,
    mode: Mode,
) -> Result<Plugged, NTSTATUS> {
    let (adapter, monitor) = {
        let mut state = lock();
        let slot = state.slots.get_mut(index).ok_or(STATUS_INVALID_PARAMETER)?;
        match slot.phase {
            Phase::Live => {}
            Phase::Arriving | Phase::Replugging => return Err(STATUS_DEVICE_BUSY),
            Phase::Free | Phase::Departing => return Err(STATUS_INVALID_PARAMETER),
        }
        if !slot.answers_to(asker) {
            return Err(STATUS_ACCESS_DENIED);
        }
        slot.phase = Phase::Replugging;
        let monitor = slot.monitor;
        (state.adapter, monitor)
    };
    leave(index, monitor);
    {
        let mut state = lock();
        let list = ModeList::for_plug(mode, state.settings.modes());
        if let Some(slot) = state.slots.get_mut(index) {
            slot.monitor = Handle::NONE;
            slot.modes = Some(list);
            slot.committed = None;
        }
    }
    let (monitor, target, luid) = match arrive(adapter, index) {
        Ok(arrived) => arrived,
        Err(status) => {
            release_slot(index);
            log_warn!("vdd: replug failed, the monitor is gone, slot={index}");
            return Err(status);
        }
    };
    if settle(index, target, luid) {
        depart(index, monitor);
        return Err(STATUS_INVALID_DEVICE_STATE);
    }
    log_info!("vdd: replugged, slot={index} mode={mode} target={target}");
    Ok(Plugged {
        slot: u32::try_from(index).unwrap_or(u32::MAX),
        os_target_id: target,
        os_adapter_luid: luid,
    })
}

/// Every monitor `owner` plugged: its handle has ended. One still arriving
/// or replugging is left to its plug, which departs it once it has arrived.
pub(crate) fn release_owner(owner: Handle<WDFFILEOBJECT__>) {
    if owner.is_none() {
        return;
    }
    let leaving: Vec<(usize, Handle<IDDCX_MONITOR__>)> = {
        let mut state = lock();
        let mut leaving = Vec::new();
        for (index, slot) in state.slots.iter_mut().enumerate() {
            if slot.kept || slot.owner != owner {
                continue;
            }
            match slot.phase {
                Phase::Live => {
                    slot.phase = Phase::Departing;
                    leaving.push((index, slot.monitor));
                }
                Phase::Arriving | Phase::Replugging => slot.orphaned = true,
                Phase::Free | Phase::Departing => {}
            }
        }
        leaving
    };
    for (index, monitor) in leaving {
        depart(index, monitor);
    }
}

/// A monitor leaves the system and its slot is freed.
fn depart(index: usize, monitor: Handle<IDDCX_MONITOR__>) {
    leave(index, monitor);
    release_slot(index);
    log_info!("vdd: unplugged, slot={index}");
}

/// A monitor leaves the system; its slot stays as it is.
fn leave(index: usize, monitor: Handle<IDDCX_MONITOR__>) {
    // SAFETY: a monitor that arrived and has not departed.
    let status = unsafe { idd_monitor_departure(monitor.0) };
    if !nt_success(status) {
        log_warn!(
            "vdd: departure refused, slot={index} status={:#x}",
            status_word(status)
        );
    }
    // A swapchain the departure did not unassign is stopped here, outside
    // the lock, as an unassignment stops one.
    let worker = lock()
        .slots
        .get_mut(index)
        .and_then(|slot| slot.worker.take());
    if let Some(worker) = worker {
        worker.stop();
    }
}

/// Every slot, as `asker` sees it.
pub(crate) fn list(asker: Handle<WDFFILEOBJECT__>) -> List {
    let state = lock();
    let mut list = List::default();
    for ((index, slot), entry) in state.slots.iter().enumerate().zip(list.entries.iter_mut()) {
        entry.slot = u32::try_from(index).unwrap_or(u32::MAX);
        if slot.phase == Phase::Free {
            continue;
        }
        let mode = slot
            .committed
            .or(slot.modes.map(|modes| modes.preferred()))
            .unwrap_or_default();
        let flag = |set: bool, bit: u32| if set { bit } else { 0 };
        entry.flags = Entry::IN_USE
            | flag(!slot.kept && slot.owner == asker, Entry::OWNED)
            | flag(
                slot.stats.assigned.load(Ordering::Relaxed),
                Entry::SWAPCHAIN,
            )
            | flag(slot.kept, Entry::KEPT)
            | flag(slot.restore, Entry::RESTORE);
        entry.width = mode.width;
        entry.height = mode.height;
        entry.refresh_hz = mode.refresh_hz;
        entry.os_target_id = slot.os_target_id;
        entry.os_adapter_luid = slot.os_adapter_luid;
        entry.render_luid = slot.stats.render_luid.load(Ordering::Relaxed);
        entry.frames = slot.stats.frames.load(Ordering::Relaxed);
    }
    list
}

/// The modes of the monitor whose description this is: the slot whose EDID
/// it is, byte for byte.
pub(crate) fn modes_for_description(description: &[u8]) -> Option<ModeList> {
    let state = lock();
    state
        .slots
        .iter()
        .enumerate()
        .filter(|(_, slot)| slot.phase != Phase::Free)
        .find(|(index, _)| {
            edid_of(&state, *index).is_some_and(|edid| edid.as_slice() == description)
        })
        .and_then(|(_, slot)| slot.modes)
}

fn find(state: &State, monitor: Handle<IDDCX_MONITOR__>) -> Option<usize> {
    if monitor.is_none() {
        return None;
    }
    state.slots.iter().position(|slot| slot.monitor == monitor)
}

/// The modes of this monitor.
pub(crate) fn modes_for_monitor(monitor: Handle<IDDCX_MONITOR__>) -> Option<ModeList> {
    let state = lock();
    let index = find(&state, monitor)?;
    state.slots.get(index)?.modes
}

/// The mode the system committed this monitor to.
pub(crate) fn committed(monitor: Handle<IDDCX_MONITOR__>, mode: Mode) {
    let mut state = lock();
    if let Some(index) = find(&state, monitor) {
        if let Some(slot) = state.slots.get_mut(index) {
            slot.committed = Some(mode);
        }
        log_info!("vdd: mode committed, slot={index} mode={mode}");
    }
}

/// The refresh the system committed this slot's monitor to, zero before any.
pub(crate) fn committed_refresh(index: usize) -> u32 {
    lock()
        .slots
        .get(index)
        .and_then(|slot| slot.committed)
        .map_or(0, |mode| mode.refresh_hz)
}

/// The slot and the counters a swapchain for this monitor reports into.
pub(crate) fn stats_for(monitor: Handle<IDDCX_MONITOR__>) -> Option<(usize, Arc<Stats>)> {
    let state = lock();
    let index = find(&state, monitor)?;
    Some((index, Arc::clone(&state.slots.get(index)?.stats)))
}

/// Keep the worker of this monitor's new swapchain, handing back one that was
/// still there -- or this one, when the monitor is gone -- to be stopped
/// outside the lock.
pub(crate) fn attach_worker(monitor: Handle<IDDCX_MONITOR__>, worker: Worker) -> Option<Worker> {
    let mut state = lock();
    let Some(index) = find(&state, monitor) else {
        return Some(worker);
    };
    match state.slots.get_mut(index) {
        Some(slot) => slot.worker.replace(worker),
        None => Some(worker),
    }
}

/// Take this monitor's worker and its slot, the worker to be stopped outside
/// the lock.
pub(crate) fn detach_worker(monitor: Handle<IDDCX_MONITOR__>) -> Option<(usize, Worker)> {
    let mut state = lock();
    let index = find(&state, monitor)?;
    let worker = state.slots.get_mut(index)?.worker.take()?;
    Some((index, worker))
}
