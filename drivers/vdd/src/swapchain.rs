//! A monitor's swapchain: the device it is given and the worker that takes its
//! pictures (docs/11-drivers.md sections 1.6 to 1.8).
//!
//! The worker sets the device first and does the rest after, takes every
//! picture, shares and publishes it where its monitor's frames were asked for
//! (`frames`), finishes it at once, and deletes the swapchain on every way
//! out. **Its stop is immediate**: the system cannot depart a monitor while a
//! picture is held, so the wait for the next picture also waits on the stop.
//! A swapchain's deletion may call the unassignment back on another thread
//! and wait for it, so a worker is taken from its slot under the lock and
//! stopped outside it, where that call finds nothing left to stop; for the
//! same reason an assignment ends the swapchain still there before it starts
//! the new one.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread::JoinHandle;

use lowlat_common::{log_debug, log_info, log_warn};
use lowlat_drivers::d3d11::{Com, D3d11, Luid, query};
use lowlat_drivers::ffi::d3d11::{ID3D11Device, ID3D11DeviceContext, IUnknown};
use lowlat_drivers::ffi::d3d11_guids::IID_IDXGIDevice;
use lowlat_vdd_proto::frames::{self as channel, ShapeKind};
use lowlat_vdd_proto::settings::Pointer;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::System::Threading::{
    CreateEventW, GetCurrentThread, SetEvent, SetThreadPriority, THREAD_PRIORITY_HIGHEST,
    WaitForMultipleObjects, WaitForSingleObject,
};

use crate::ffi::{
    DXGI_ERROR_ACCESS_LOST, E_PENDING, IDARG_IN_QUERY_HWCURSOR, IDARG_IN_SETSWAPCHAIN,
    IDARG_IN_SETUP_HWCURSOR, IDARG_IN_SWAPCHAINSETDEVICE, IDARG_OUT_QUERY_HWCURSOR,
    IDARG_OUT_RELEASEANDACQUIREBUFFER, IDDCX_CURSOR_CAPS, IDDCX_CURSOR_SHAPE_TYPE_ALPHA,
    IDDCX_CURSOR_SHAPE_TYPE_MASKED_COLOR, IDDCX_MONITOR, IDDCX_MONITOR__, IDDCX_SWAPCHAIN__,
    IDDCX_XOR_CURSOR_SUPPORT_FULL, NTSTATUS, STATUS_GRAPHICS_INDIRECT_DISPLAY_ABANDON_SWAPCHAIN,
    STATUS_SUCCESS, idd_monitor_query_hardware_cursor, idd_monitor_setup_hardware_cursor,
    idd_swapchain_finished_processing_frame, idd_swapchain_release_and_acquire_buffer,
    idd_swapchain_set_device, nt_success, size_of, wdf_object_delete,
};
use crate::frames::{PointerShare, Sharing};
use crate::monitor::{self, Handle, Stats, status_word};
/// How long the worker waits for a picture before asking again, as the
/// class's own sample does.
const WAIT_MS: u32 = 17;

/// An event the worker waits on, closed when dropped.
#[derive(Debug)]
struct Event(HANDLE);

// SAFETY: an event is a kernel object any thread may signal or wait on.
unsafe impl Send for Event {}
// SAFETY: as above.
unsafe impl Sync for Event {}

impl Event {
    fn new(manual: bool) -> Option<Self> {
        // SAFETY: no name and no security attributes.
        let handle =
            unsafe { CreateEventW(core::ptr::null(), i32::from(manual), 0, core::ptr::null()) };
        (!handle.is_null()).then_some(Self(handle))
    }

    fn set(&self) {
        // SAFETY: a live event.
        unsafe { SetEvent(self.0) };
    }

    fn is_set(&self) -> bool {
        // SAFETY: a live event, not waited on.
        unsafe { WaitForSingleObject(self.0, 0) == WAIT_OBJECT_0 }
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: an event this owns.
        unsafe { CloseHandle(self.0) };
    }
}

/// A value moved to the worker's thread and used there alone.
struct Moved<T>(T);

// SAFETY: the device is free-threaded, and its immediate context and the
// class's handles move to the worker and are used by it alone from then on.
unsafe impl<T> Send for Moved<T> {}

/// A running swapchain worker.
#[derive(Debug)]
pub(crate) struct Worker {
    thread: Option<JoinHandle<()>>,
    stop: Arc<Event>,
}

impl Worker {
    fn start(
        slot: usize,
        monitor: Handle<IDDCX_MONITOR__>,
        args: &IDARG_IN_SETSWAPCHAIN,
        stats: Arc<Stats>,
    ) -> Result<Self, NTSTATUS> {
        let render = Luid {
            high: args.RenderAdapterLuid.HighPart,
            low: args.RenderAdapterLuid.LowPart,
        };
        stats.render_luid.store(render.value(), Ordering::Relaxed);
        let device = D3d11::load()
            .and_then(|d3d11| d3d11.plain_device(render))
            .map_err(|error| {
                log_warn!("vdd: no device on the render adapter, slot={slot} luid={render} error={error:?}");
                STATUS_GRAPHICS_INDIRECT_DISPLAY_ABANDON_SWAPCHAIN
            })?;
        let stop =
            Arc::new(Event::new(true).ok_or(STATUS_GRAPHICS_INDIRECT_DISPLAY_ABANDON_SWAPCHAIN)?);
        let run = Moved(Run {
            slot,
            monitor,
            swapchain: Handle(args.hSwapChain),
            next: Handle(args.hNextSurfaceAvailable.cast()),
            stop: Arc::clone(&stop),
            device,
            stats,
        });
        let thread = std::thread::Builder::new()
            .name(format!("vdd-swapchain-{slot}"))
            .spawn(move || {
                let run = run;
                // Above normal work: its wait's boost wins the acquire, but
                // the fence's signal and flush after it ran late beside busy
                // threads of the same priority. Below the compositor's.
                // SAFETY: the calling thread's own pseudo-handle.
                if unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST) } == 0 {
                    log_warn!("vdd: the frame thread's priority was not raised, slot={slot}");
                }
                run.0.run();
            })
            .map_err(|_| STATUS_GRAPHICS_INDIRECT_DISPLAY_ABANDON_SWAPCHAIN)?;
        log_info!("vdd: swapchain assigned, slot={slot} render={render}");
        Ok(Self {
            thread: Some(thread),
            stop,
        })
    }

    /// Stop the worker and wait until it has deleted its swapchain -- unless
    /// this is the worker's own thread, called back from that deletion.
    pub(crate) fn stop(mut self) {
        self.stop.set();
        let Some(thread) = self.thread.take() else {
            return;
        };
        if thread.thread().id() == std::thread::current().id() {
            return;
        }
        let _ = thread.join();
    }
}

struct Run {
    slot: usize,
    monitor: Handle<IDDCX_MONITOR__>,
    swapchain: Handle<IDDCX_SWAPCHAIN__>,
    next: Handle<core::ffi::c_void>,
    stop: Arc<Event>,
    device: (Com<ID3D11Device>, Com<ID3D11DeviceContext>),
    stats: Arc<Stats>,
}

impl Run {
    fn run(self) {
        let slot = self.slot;
        // The device's graphics-infrastructure interface, held by its base:
        // only its address is passed on.
        let set = match query::<IUnknown>(self.device.0.as_ptr().cast(), &IID_IDXGIDevice) {
            Ok(dxgi) => {
                let args = IDARG_IN_SWAPCHAINSETDEVICE {
                    pDevice: dxgi.as_ptr().cast(),
                };
                // SAFETY: the swapchain this worker owns; the device is live
                // for the call, and the class takes its own reference.
                unsafe { idd_swapchain_set_device(self.swapchain.0, &raw const args) }
            }
            Err(_) => STATUS_GRAPHICS_INDIRECT_DISPLAY_ABANDON_SWAPCHAIN,
        };
        if set == DXGI_ERROR_ACCESS_LOST {
            // The usual answer on a monitor's first assignment; the system
            // assigns a fresh swapchain once this one is deleted.
            log_info!("vdd: swapchain lost before its device was set, slot={slot}");
        } else if set < 0 {
            log_warn!(
                "vdd: swapchain refused its device, slot={slot} hr={:#x}",
                status_word(set)
            );
        } else {
            self.stats.assigned.store(true, Ordering::Relaxed);
            // The hardware cursor, unless the file asks for the pointer in
            // the pictures -- where the system draws it when none is set up.
            let mut cursor = match monitor::pointer() {
                Pointer::Hardware => Cursor::setup(self.monitor, slot),
                Pointer::Drawn => None,
            };
            self.frames(cursor.as_mut());
            self.stats.assigned.store(false, Ordering::Relaxed);
        }
        // SAFETY: the swapchain this worker owns, deleted once, on its way out.
        unsafe { wdf_object_delete(self.swapchain.0.cast()) };
        log_info!(
            "vdd: swapchain ended, slot={slot} frames={}",
            self.stats.frames.load(Ordering::Relaxed)
        );
    }

    fn frames(&self, mut cursor: Option<&mut Cursor>) {
        let slot = self.slot;
        // Ends before the swapchain's deletion below: its handles close first.
        let mut sharing = Sharing::new(
            slot,
            self.stats.render_luid.load(Ordering::Relaxed),
            monitor::committed_refresh(slot),
            &self.device,
        );
        // The last picture's surface and what it was, held until the next
        // acquire -- as long as it stays the driver's -- so a page asked for
        // while the desktop is still is given the picture on the display.
        let mut held: Option<(Com<IUnknown>, u32, bool)> = None;
        let mut said = PointerShare::new(slot);
        loop {
            let mut acquired = IDARG_OUT_RELEASEANDACQUIREBUFFER::default();
            // SAFETY: the swapchain this worker owns.
            let hr = unsafe {
                idd_swapchain_release_and_acquire_buffer(self.swapchain.0, &raw mut acquired)
            };
            if hr == E_PENDING {
                // The pointer's event when there is one: a wait may not name
                // one handle twice.
                let pointer = cursor.as_ref().map(|cursor| cursor.event.0);
                let handles: [HANDLE; 3] = [
                    self.next.0,
                    self.stop.0,
                    pointer.unwrap_or(core::ptr::null_mut()),
                ];
                let count = if pointer.is_some() { 3 } else { 2 };
                // SAFETY: `count` live handles for the wait.
                let woke = unsafe { WaitForMultipleObjects(count, handles.as_ptr(), 0, WAIT_MS) };
                if woke == WAIT_OBJECT_0 + 2
                    && let Some(cursor) = cursor.as_deref_mut()
                {
                    cursor.query();
                } else if woke != WAIT_OBJECT_0 && woke != WAIT_TIMEOUT {
                    if woke != WAIT_OBJECT_0 + 1 {
                        log_warn!("vdd: wait failed, slot={slot} result={woke:#x}");
                    }
                    return;
                }
                // The pointer as last said, kept on the page -- no hardware
                // cursor said as such, the pointer then in the pictures.
                match cursor.as_deref() {
                    Some(cursor) => said.keep(&cursor.said, cursor.pixels()),
                    None => said.keep(&channel::Pointer::default(), &[]),
                }
                sharing.catch_up(
                    held.as_ref()
                        .map(|(surface, frame, protected)| (surface.as_ptr(), *frame, *protected)),
                );
                continue;
            }
            if hr == DXGI_ERROR_ACCESS_LOST {
                // The system abandoned the swapchain, as it does a few times
                // while a mode settles; it assigns a fresh one.
                log_info!("vdd: swapchain lost, slot={slot}");
                return;
            }
            if hr < 0 {
                log_warn!("vdd: acquire failed, slot={slot} hr={:#x}", status_word(hr));
                return;
            }
            self.stats.frames.fetch_add(1, Ordering::Relaxed);
            let frame = acquired.MetaData.PresentationFrameNumber;
            let protected = acquired.MetaData.HwProtectedSurface != 0;
            // The surface's reference is the driver's to release, and the
            // surface stays the driver's until the next acquire: kept until
            // then, the last one let go here.
            // SAFETY: the reference the acquire handed over.
            let surface = unsafe { Com::<IUnknown>::from_raw(acquired.MetaData.pSurface.cast()) };
            if let Some(surface) = &surface {
                sharing.picture(surface.as_ptr(), frame, protected);
            }
            held = surface.map(|surface| (surface, frame, protected));
            // SAFETY: the swapchain this worker owns.
            let hr = unsafe { idd_swapchain_finished_processing_frame(self.swapchain.0) };
            if hr < 0 {
                log_warn!("vdd: finish failed, slot={slot} hr={:#x}", status_word(hr));
                return;
            }
            if self.stop.is_set() {
                return;
            }
        }
    }
}

/// The hardware cursor: the system draws no pointer into the pictures and
/// reports its shape and position instead.
struct Cursor {
    monitor: Handle<IDDCX_MONITOR__>,
    slot: usize,
    event: Event,
    shape: Vec<u8>,
    last_shape: u32,
    /// What the system last said, and how many bytes of `shape` its shape's
    /// pixels take.
    said: channel::Pointer,
    bytes: usize,
}

impl Cursor {
    /// After the swapchain's device is set, once per swapchain; asked at once
    /// for what the system shows, rather than left to the first change.
    fn setup(monitor: Handle<IDDCX_MONITOR__>, slot: usize) -> Option<Self> {
        let event = Event::new(false)?;
        let args = IDARG_IN_SETUP_HWCURSOR {
            CursorInfo: IDDCX_CURSOR_CAPS {
                Size: size_of::<IDDCX_CURSOR_CAPS>(),
                ColorXorCursorSupport: IDDCX_XOR_CURSOR_SUPPORT_FULL,
                AlphaCursorSupport: 1,
                MaxX: channel::POINTER_LIMIT,
                MaxY: channel::POINTER_LIMIT,
            },
            hNewCursorDataAvailable: event.0.cast(),
        };
        // SAFETY: a live monitor and event.
        let status = unsafe { idd_monitor_setup_hardware_cursor(monitor.0, &raw const args) };
        if !nt_success(status) {
            log_warn!(
                "vdd: hardware cursor refused, slot={slot} status={:#x}",
                status_word(status)
            );
            return None;
        }
        let mut cursor = Self {
            monitor,
            slot,
            event,
            shape: vec![0; channel::SHAPE_BYTES],
            last_shape: 0,
            said: channel::Pointer {
                cursor: true,
                ..channel::Pointer::default()
            },
            bytes: 0,
        };
        cursor.query();
        Some(cursor)
    }

    /// The pixels of the shape last said.
    fn pixels(&self) -> &[u8] {
        self.shape.get(..self.bytes).unwrap_or_default()
    }

    /// Ask the system what it shows now. **A shape is taken only when it
    /// comes with one**: with the cursor hidden the system says nothing of
    /// the shape, and the last number asked with stays the last one taken, so
    /// a shape set meanwhile comes when the cursor shows again.
    fn query(&mut self) {
        let args = IDARG_IN_QUERY_HWCURSOR {
            LastShapeId: self.last_shape,
            ShapeBufferSizeInBytes: u32::try_from(self.shape.len()).unwrap_or(0),
            pShapeBuffer: self.shape.as_mut_ptr(),
        };
        let mut out = IDARG_OUT_QUERY_HWCURSOR::default();
        // SAFETY: a live monitor; the shape buffer is the size it says.
        let status = unsafe {
            idd_monitor_query_hardware_cursor(self.monitor.0, &raw const args, &raw mut out)
        };
        if !nt_success(status) {
            log_debug!(
                "vdd: pointer query failed, slot={} status={:#x}",
                self.slot,
                status_word(status)
            );
            return;
        }
        self.said.visible = out.IsCursorVisible != 0;
        let info = out.CursorShapeInfo;
        let kind = match info.CursorType {
            IDDCX_CURSOR_SHAPE_TYPE_MASKED_COLOR => Some(ShapeKind::MaskedColour),
            IDDCX_CURSOR_SHAPE_TYPE_ALPHA => Some(ShapeKind::Alpha),
            _ => None,
        };
        let bytes =
            usize::try_from(u64::from(info.Pitch) * u64::from(info.Height)).unwrap_or(usize::MAX);
        if out.IsCursorShapeUpdated != 0
            && info.ShapeId != 0
            && kind.is_some()
            && bytes <= self.shape.len()
        {
            self.last_shape = info.ShapeId;
            self.bytes = bytes;
            self.said = channel::Pointer {
                cursor: true,
                visible: self.said.visible,
                shape: info.ShapeId,
                kind,
                width: info.Width,
                height: info.Height,
                pitch: info.Pitch,
                hot_x: info.XHot,
                hot_y: info.YHot,
            };
            log_debug!(
                "vdd: pointer shape, slot={} id={} type={} size={}x{} hot={},{} visible={}",
                self.slot,
                info.ShapeId,
                info.CursorType,
                info.Width,
                info.Height,
                info.XHot,
                info.YHot,
                out.IsCursorVisible
            );
        }
    }
}

/// The class assigns a swapchain to a monitor.
pub(crate) unsafe extern "C" fn assign(
    monitor: IDDCX_MONITOR,
    args: *const IDARG_IN_SETSWAPCHAIN,
) -> NTSTATUS {
    let monitor = Handle(monitor);
    // SAFETY: the class's arguments, live for the call.
    let Some(args) = (unsafe { args.as_ref() }) else {
        return STATUS_GRAPHICS_INDIRECT_DISPLAY_ABANDON_SWAPCHAIN;
    };
    // A swapchain still there ends first: its deletion may call the
    // unassignment back, which must not find the new one.
    if let Some((_, stale)) = monitor::detach_worker(monitor) {
        stale.stop();
    }
    let Some((slot, stats)) = monitor::stats_for(monitor) else {
        log_warn!("vdd: a swapchain for a monitor not known, abandoned");
        return STATUS_GRAPHICS_INDIRECT_DISPLAY_ABANDON_SWAPCHAIN;
    };
    match Worker::start(slot, monitor, args, stats) {
        Ok(worker) => {
            if let Some(stale) = monitor::attach_worker(monitor, worker) {
                stale.stop();
            }
            STATUS_SUCCESS
        }
        Err(status) => status,
    }
}

/// The class takes a monitor's swapchain away.
pub(crate) unsafe extern "C" fn unassign(monitor: IDDCX_MONITOR) -> NTSTATUS {
    match monitor::detach_worker(Handle(monitor)) {
        Some((slot, worker)) => {
            log_info!("vdd: swapchain unassigned, slot={slot}");
            worker.stop();
        }
        None => log_info!("vdd: unassignment with no swapchain running"),
    }
    STATUS_SUCCESS
}
