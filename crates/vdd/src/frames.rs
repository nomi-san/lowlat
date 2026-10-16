//! The frame channel's driver half (docs/11-drivers.md section 1.6).
//!
//! **One page per monitor**, made at the first request for that monitor's
//! frames with the event set at each publish, and kept for the driver's life:
//! a page survives a replug, so a reader keeps it across a change of mode.
//! **Per swapchain**, each buffer is shared once, read-only, by an NT handle,
//! and a fence of the swapchain's device is signalled after each acquire;
//! then the latest picture is published.
//!
//! **Nothing here waits on a reader.** A monitor nobody asked for is shared
//! with nobody; one somebody asked for is published whether anybody reads it
//! or not. The system writes a buffer again only once the driver has
//! acquired as many newer pictures as the swapchain has buffers, less one,
//! which is the reader's to check.
//!
//! **The pointer goes in the same section** (section 1.7): what the system
//! last said of the hardware cursor, published by the same worker when it
//! changes, a new shape's pixels before the state that names them.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use lowlat_common::{log_info, log_warn};
use lowlat_drivers::d3d11::{Com, query};
use lowlat_drivers::ffi::d3d11::{
    D3D11_FENCE_FLAG_SHARED, D3D11_TEXTURE2D_DESC, DXGI_SHARED_RESOURCE_READ, ID3D11Device,
    ID3D11Device5, ID3D11DeviceContext, ID3D11DeviceContext4, ID3D11Fence, ID3D11Texture2D,
    IDXGIResource1, IUnknown,
};
use lowlat_drivers::ffi::d3d11_guids::{
    IID_ID3D11Device5, IID_ID3D11DeviceContext4, IID_ID3D11Fence, IID_ID3D11Texture2D,
    IID_IDXGIResource1,
};
use lowlat_drivers::vcall;
use lowlat_vdd_proto::MAX_MONITORS;
use lowlat_vdd_proto::control::FrameChannel;
use lowlat_vdd_proto::frames::{
    self, MAX_BUFFERS, Picture, Pointer, SECTION_WORDS, SIZE, Snapshot,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, FILETIME, GENERIC_ALL, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_READ, FILE_MAP_WRITE, MapViewOfFile, PAGE_READWRITE,
};
use windows_sys::Win32::System::Performance::QueryPerformanceCounter;
use windows_sys::Win32::System::Threading::{
    CreateEventW, GetCurrentProcess, GetCurrentProcessId, GetProcessTimes, SetEvent,
};

use crate::ffi::{NTSTATUS, STATUS_INSUFFICIENT_RESOURCES, STATUS_INVALID_PARAMETER};

/// Each monitor's page, mapped, and its event: zero until the first request.
/// Read by the monitor's swapchain worker at each picture, so not behind the
/// lock the requests take.
static PAGES: [AtomicUsize; MAX_MONITORS] = [const { AtomicUsize::new(0) }; MAX_MONITORS];
static EVENTS: [AtomicUsize; MAX_MONITORS] = [const { AtomicUsize::new(0) }; MAX_MONITORS];

/// Each monitor's section, for the answer to a request: made once, under
/// this lock, so two requests at once make one.
static SECTIONS: Mutex<[usize; MAX_MONITORS]> = Mutex::new([0; MAX_MONITORS]);

/// The last generation given out: one more for each swapchain shared.
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// The system's performance counter, which the readers' clock is too.
fn now() -> u64 {
    let mut count = 0i64;
    // SAFETY: a live output.
    unsafe { QueryPerformanceCounter(&raw mut count) };
    u64::try_from(count).unwrap_or(0)
}

/// When this process started, which tells it from a later one that came to
/// carry its identifier.
fn started() -> u64 {
    let zero = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
    // SAFETY: this process's own pseudo-handle; live outputs.
    unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &raw mut created,
            &raw mut exited,
            &raw mut kernel,
            &raw mut user,
        )
    };
    (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime)
}

/// A monitor's page, once made: mapped for the driver's life.
fn page_of(slot: usize) -> Option<&'static [AtomicU64]> {
    // Acquire: pairs with the release that published the address, after the
    // page was prepared.
    let address = PAGES.get(slot)?.load(Ordering::Acquire);
    if address == 0 {
        return None;
    }
    // SAFETY: a view of the whole section, page-aligned, mapped and never
    // unmapped; only atomics touch it.
    Some(unsafe { core::slice::from_raw_parts(address as *const AtomicU64, SECTION_WORDS) })
}

/// Answer a request for `slot`'s frames: its page, made at the first.
/// **Any caller the device admits is answered** (docs/11-drivers.md section
/// 1.2): the answer is handle values in this process.
pub(crate) fn request(slot: usize) -> Result<FrameChannel, NTSTATUS> {
    let mut sections = SECTIONS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let section = sections.get_mut(slot).ok_or(STATUS_INVALID_PARAMETER)?;
    if *section == 0 {
        let size = u32::try_from(SIZE).map_err(|_| STATUS_INSUFFICIENT_RESOURCES)?;
        // SAFETY: a section backed by the page file, no name, no security
        // attributes: what reaches it is a handle taken from this process.
        let made = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                core::ptr::null(),
                PAGE_READWRITE,
                0,
                size,
                core::ptr::null(),
            )
        };
        if made.is_null() {
            log_warn!("vdd: no page for a monitor's frames, slot={slot}");
            return Err(STATUS_INSUFFICIENT_RESOURCES);
        }
        // SAFETY: the section just made, mapped whole.
        let view = unsafe { MapViewOfFile(made, FILE_MAP_READ | FILE_MAP_WRITE, 0, 0, SIZE) };
        // SAFETY: no name, no security attributes; reset by each wait it ends.
        let event = unsafe { CreateEventW(core::ptr::null(), 0, 0, core::ptr::null()) };
        if view.Value.is_null() || event.is_null() {
            // SAFETY: handles this made; the view, if any, stays mapped.
            unsafe {
                CloseHandle(made);
                if !event.is_null() {
                    CloseHandle(event);
                }
            }
            log_warn!("vdd: the page for a monitor's frames was not mapped, slot={slot}");
            return Err(STATUS_INSUFFICIENT_RESOURCES);
        }
        // SAFETY: as in `page_of`, before any other thread has the address.
        let page =
            unsafe { core::slice::from_raw_parts(view.Value as *const AtomicU64, SECTION_WORDS) };
        frames::prepare(page);
        frames::publish(
            page,
            &Snapshot {
                event: event as u64,
                ..Snapshot::default()
            },
        );
        if let (Some(address), Some(events)) = (PAGES.get(slot), EVENTS.get(slot)) {
            events.store(event as usize, Ordering::Relaxed);
            // Release: the page is prepared before its address is seen.
            address.store(view.Value as usize, Ordering::Release);
        }
        *section = made as usize;
        log_info!("vdd: frames asked for, the page made, slot={slot}");
    }
    Ok(FrameChannel {
        version: frames::VERSION,
        // SAFETY: no precondition.
        process: unsafe { GetCurrentProcessId() },
        size: u32::try_from(SIZE).unwrap_or(0),
        started: started(),
        section: *section as u64,
    })
}

/// One swapchain's sharing, on its worker's thread: from the first picture
/// after its monitor's page exists until the swapchain ends.
pub(crate) struct Sharing {
    slot: usize,
    device5: Option<Com<ID3D11Device5>>,
    context4: Option<Com<ID3D11DeviceContext4>>,
    context: *mut ID3D11DeviceContext,
    page: Option<&'static [AtomicU64]>,
    event: HANDLE,
    fence: Option<(Com<ID3D11Fence>, HANDLE)>,
    /// Each buffer's texture, by the address the system hands it over at,
    /// in the order first seen; its handle is in the snapshot.
    textures: [usize; MAX_BUFFERS],
    snapshot: Snapshot,
    last_frame: Option<u32>,
    /// Said once a swapchain, not every picture.
    refused: bool,
}

impl Sharing {
    pub(crate) fn new(
        slot: usize,
        render: u64,
        refresh_hz: u32,
        device: &(Com<ID3D11Device>, Com<ID3D11DeviceContext>),
    ) -> Self {
        Self {
            slot,
            device5: query::<ID3D11Device5>(device.0.as_ptr().cast(), &IID_ID3D11Device5).ok(),
            context4: query::<ID3D11DeviceContext4>(
                device.1.as_ptr().cast(),
                &IID_ID3D11DeviceContext4,
            )
            .ok(),
            context: device.1.as_ptr(),
            page: None,
            event: core::ptr::null_mut(),
            fence: None,
            textures: [0; MAX_BUFFERS],
            snapshot: Snapshot {
                render,
                refresh: (refresh_hz, 1),
                ..Snapshot::default()
            },
            last_frame: None,
            refused: false,
        }
    }

    fn refuse(&mut self, what: &str, status: i32) {
        if !self.refused {
            self.refused = true;
            log_warn!(
                "vdd: frames not shared, {what}, slot={} hr={:#x}",
                self.slot,
                crate::monitor::status_word(status)
            );
        }
    }

    /// A picture acquired: shared and published, when the monitor's page
    /// exists. `surface` is the system's buffer, live for the call.
    pub(crate) fn picture(&mut self, surface: *mut IUnknown, frame: u32, protected: bool) {
        let at = now();
        if self.page.is_none() {
            self.page = page_of(self.slot);
            let Some(event) = EVENTS.get(self.slot) else {
                return;
            };
            self.event = event.load(Ordering::Relaxed) as HANDLE;
        }
        let Some(page) = self.page else {
            return;
        };
        if self.fence.is_none() && !self.begin() {
            return;
        }
        let Some(index) = self.buffer(surface) else {
            return;
        };
        let snapshot = &mut self.snapshot;
        let repeat = self.last_frame == Some(frame);
        if let Some(last) = self.last_frame
            && frame > last.wrapping_add(1)
        {
            snapshot.gaps += u64::from(frame - last - 1);
        }
        self.last_frame = Some(frame);
        snapshot.pictures += 1;
        let sequence = snapshot.picture.sequence + 1;
        let (Some((fence, _)), Some(context4)) = (&self.fence, &self.context4) else {
            return;
        };
        // SAFETY: the worker's own context, on its thread, and a fence of its
        // device. The signal lies behind the system's write of this buffer.
        let hr = unsafe { vcall!(context4.as_ptr(), Signal, fence.as_ptr(), sequence) };
        // SAFETY: as above. Handed to the device at once: a signal left in
        // the runtime's buffer would hold every reader's wait.
        unsafe { vcall!(self.context, Flush) };
        if hr.is_none_or(|hr| hr < 0) {
            self.refuse("the fence was not signalled", hr.unwrap_or(-1));
            return;
        }
        snapshot.picture = Picture {
            sequence,
            index,
            frame,
            acquired: at,
            repeat,
            protected,
        };
        frames::publish(page, snapshot);
        if !self.event.is_null() {
            // SAFETY: the monitor's event, kept for the driver's life.
            unsafe { SetEvent(self.event) };
        }
    }

    /// The picture the worker still holds, published when the monitor's page
    /// has appeared since the last: **a still desktop hands over nothing
    /// new**, and a reader that asks while it is still must still get the
    /// picture on the display. Nothing when the page was there already.
    pub(crate) fn catch_up(&mut self, held: Option<(*mut IUnknown, u32, bool)>) {
        if self.page.is_some() {
            return;
        }
        let Some((surface, frame, protected)) = held else {
            return;
        };
        if page_of(self.slot).is_some() {
            self.picture(surface, frame, protected);
        }
    }

    /// This swapchain's first picture shared: its fence, made shared, and a
    /// generation of its own.
    fn begin(&mut self) -> bool {
        let Some(device5) = &self.device5 else {
            self.refuse("the device has no shared fences", -1);
            return false;
        };
        let mut out: *mut core::ffi::c_void = core::ptr::null_mut();
        // SAFETY: a live device; the identifier and output are live.
        let hr = unsafe {
            vcall!(
                device5.as_ptr(),
                CreateFence,
                0,
                D3D11_FENCE_FLAG_SHARED,
                &IID_ID3D11Fence,
                &raw mut out
            )
        }
        .unwrap_or(-1);
        // SAFETY: the reference the call handed over, if any.
        let Some(fence) = (unsafe { Com::from_raw(out.cast::<ID3D11Fence>()) }) else {
            self.refuse("no fence", hr);
            return false;
        };
        let mut handle: lowlat_drivers::ffi::d3d11::HANDLE = core::ptr::null_mut();
        // SAFETY: a live fence; no attributes, no name; a live output.
        let hr = unsafe {
            vcall!(
                fence.as_ptr(),
                CreateSharedHandle,
                core::ptr::null(),
                GENERIC_ALL,
                core::ptr::null(),
                &raw mut handle
            )
        }
        .unwrap_or(-1);
        if hr < 0 || handle.is_null() {
            self.refuse("the fence was not shared", hr);
            return false;
        }
        self.snapshot.generation = GENERATION.fetch_add(1, Ordering::Relaxed) + 1;
        self.snapshot.event = self.event as u64;
        self.snapshot.fence = handle as u64;
        self.fence = Some((fence, handle.cast()));
        log_info!(
            "vdd: frames shared, slot={} generation={}",
            self.slot,
            self.snapshot.generation
        );
        true
    }

    /// Which buffer `surface` is, shared the first time it is seen.
    fn buffer(&mut self, surface: *mut IUnknown) -> Option<u32> {
        let texture = query::<ID3D11Texture2D>(surface, &IID_ID3D11Texture2D).ok()?;
        let address = texture.as_ptr() as usize;
        let seen = usize::try_from(self.snapshot.buffers).ok()?;
        if let Some(index) = self
            .textures
            .get(..seen)?
            .iter()
            .position(|&known| known == address)
        {
            return u32::try_from(index).ok();
        }
        if seen >= MAX_BUFFERS {
            self.refuse("more buffers than a page names", -1);
            return None;
        }
        let resource = query::<IDXGIResource1>(surface, &IID_IDXGIResource1).ok()?;
        let mut handle: lowlat_drivers::ffi::d3d11::HANDLE = core::ptr::null_mut();
        // SAFETY: a live resource; no attributes, no name; a live output.
        let hr = unsafe {
            vcall!(
                resource.as_ptr(),
                CreateSharedHandle,
                core::ptr::null(),
                DXGI_SHARED_RESOURCE_READ,
                core::ptr::null(),
                &raw mut handle
            )
        }
        .unwrap_or(-1);
        if hr < 0 || handle.is_null() {
            self.refuse("a buffer was not shared", hr);
            return None;
        }
        // SAFETY: plain data the call fills whole.
        let mut desc: D3D11_TEXTURE2D_DESC = unsafe { core::mem::zeroed() };
        // SAFETY: a live texture; a live output.
        unsafe { vcall!(texture.as_ptr(), GetDesc, &raw mut desc) };
        *self.textures.get_mut(seen)? = address;
        *self.snapshot.buffer.get_mut(seen)? = handle as u64;
        self.snapshot.buffers += 1;
        self.snapshot.width = desc.Width;
        self.snapshot.height = desc.Height;
        log_info!(
            "vdd: buffer shared, slot={} index={seen} {}x{} format={} misc={:#x}",
            self.slot,
            desc.Width,
            desc.Height,
            desc.Format,
            desc.MiscFlags
        );
        u32::try_from(seen).ok()
    }
}

impl Drop for Sharing {
    fn drop(&mut self) {
        let Some((_, fence)) = self.fence.take() else {
            return;
        };
        // **The generation ends before its handles close**: a reader that read
        // a handle's value and pulls it after the close finds the generation
        // gone when it reads again, rather than trusting whatever came to
        // carry that value.
        if let Some(page) = self.page {
            frames::publish(
                page,
                &Snapshot {
                    event: self.event as u64,
                    pictures: self.snapshot.pictures,
                    gaps: self.snapshot.gaps,
                    ..Snapshot::default()
                },
            );
        }
        // SAFETY: handles this made, each closed once.
        unsafe {
            CloseHandle(fence);
            for &handle in self.snapshot.buffer.iter().filter(|&&h| h != 0) {
                CloseHandle(handle as HANDLE);
            }
        }
        log_info!(
            "vdd: frames unshared, slot={} generation={} buffers={} pictures={} gaps={}",
            self.slot,
            self.snapshot.generation,
            self.snapshot.buffers,
            self.snapshot.pictures,
            self.snapshot.gaps
        );
    }
}

/// The pointer's half of a monitor's section, on its swapchain worker's
/// thread: what the system last said of the pointer, published when it
/// changes, and whole when the page has appeared since.
pub(crate) struct PointerShare {
    slot: usize,
    page: Option<&'static [AtomicU64]>,
    /// What the page says now; nothing before this has said anything.
    said: Option<Pointer>,
}

impl PointerShare {
    pub(crate) fn new(slot: usize) -> Self {
        Self {
            slot,
            page: None,
            said: None,
        }
    }

    /// Keep the page at `pointer`, `shape` the pixels of the shape it names.
    /// **A shape's pixels go before the state that names them**, so a reader
    /// that sees a new number finds them; a state that names the same shape
    /// leaves the pixels as they are.
    pub(crate) fn keep(&mut self, pointer: &Pointer, shape: &[u8]) {
        if self.page.is_none() {
            self.page = page_of(self.slot);
        }
        let Some(page) = self.page else {
            return;
        };
        if self.said.as_ref() == Some(pointer) {
            return;
        }
        if pointer.shape != 0 && self.said.is_none_or(|said| said.shape != pointer.shape) {
            frames::publish_shape(page, pointer.shape, shape);
        }
        frames::publish_pointer(page, pointer);
        self.said = Some(*pointer);
    }
}
