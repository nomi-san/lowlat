//! A handle's device: what the host asked for, the virtual HID framework's
//! device made from it, the answers it gives and what the system wrote to it
//! (docs/11-drivers.md sections 2.2 and 2.3).
//!
//! **Each handle's state lives from the handle's opening to its close**,
//! behind the pointer its file object's context holds. The framework closes a
//! file object only once every request on it has completed, so no request
//! reads the state after the close frees it, and the virtual HID framework
//! calls nothing after its delete returns. No lock is held across a call into
//! the virtual HID framework, so none of its callbacks can wait on one the
//! call holds.

use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use lowlat_common::{log_info, log_warn};
use lowlat_hid_proto::control::{Create, FEATURE_MAX, Features, Written, WrittenKind};
use lowlat_hid_proto::{Claim, Kind, Live, container_id};

use crate::ffi::{
    GUID, HID_XFER_PACKET, NTSTATUS, PHID_XFER_PACKET, PVOID, STATUS_BUFFER_TOO_SMALL,
    STATUS_CANCELLED, STATUS_DEVICE_BUSY, STATUS_INSUFFICIENT_RESOURCES,
    STATUS_INVALID_DEVICE_STATE, STATUS_INVALID_PARAMETER, STATUS_SUCCESS, ULONG, ULONG_PTR,
    VHF_CONFIG, VHFHANDLE, VHFOPERATIONHANDLE, VhfAsyncOperationComplete, VhfCreate, VhfDelete,
    VhfReadReportSubmit, VhfStart, WDF_IO_TARGET_OPEN_PARAMS, WDFDEVICE, WDFFILEOBJECT,
    WDFIOTARGET, WDFQUEUE, WDFREQUEST, WdfIoTargetOpenLocalTargetByFile, nt_success,
    object_attributes, size_of, status_word, wdf_io_queue_retrieve_request_by_file_object,
    wdf_io_target_create, wdf_io_target_open, wdf_io_target_wdm_get_target_file_handle,
    wdf_object_delete, wdf_request_complete, wdf_request_complete_with_information,
    wdf_request_forward_to_io_queue, wdf_request_retrieve_output_buffer,
};

/// The kinds and slots with a device, across every handle: one device to a
/// kind and slot, at most [`lowlat_hid_proto::MAX_DEVICES`] in all.
static LIVE: Mutex<Live> = Mutex::new(Live::new());

fn live() -> MutexGuard<'static, Live> {
    LIVE.lock().unwrap_or_else(PoisonError::into_inner)
}

/// How many of the system's writes wait for the host; past it the oldest goes,
/// counted and said. **Not every write is a whole state**: a DualSense's output
/// report names, by its flags, the parts it changes, so one lost can lose a
/// lightbar or a trigger effect for good -- the guest's loop drains them each
/// pass, and this is room for a pass that stretches.
const WRITTEN_KEPT: usize = 32;

/// The input reports a device remembers for the system's reads of them, one
/// per identifier from 1.
const INPUT_IDS: usize = 2;

/// The largest input report any kind sends.
const INPUT_MAX: usize = 64;

/// How long the neutral reports have to arrive before the device goes: the
/// system's keyboard path drops a report still on its way at the delete, and
/// raw input then never sees the keys come up. **Every kind waits**: a device
/// deleted before the system has finished setting it up -- a few milliseconds
/// to tens -- holds the next device's removal for about five seconds, and the
/// wait covers that.
const NEUTRAL_SETTLE: Duration = Duration::from_millis(50);

/// One handle's state.
pub(crate) struct File {
    file: WDFFILEOBJECT,
    /// The device beneath, opened through this handle: the virtual HID
    /// framework makes the handle's device through it.
    target: WDFIOTARGET,
    /// The queue a read of what was written waits in, the device's own.
    pending: WDFQUEUE,
    state: Mutex<State>,
}

// SAFETY: the framework's handles may be used from any thread; everything
// that changes is behind the state's lock.
unsafe impl Send for File {}
// SAFETY: as above.
unsafe impl Sync for File {}

struct State {
    /// The kind asked for, from the moment the create starts: the system may
    /// ask a device's answers before its create returns.
    kind: Option<Kind>,
    creating: bool,
    device: Option<Device>,
    features: Features,
    last: [Option<Report>; INPUT_IDS],
    written: Ring,
    /// Writes of the system's that went before the host read them.
    dropped: u32,
}

/// The made device, and the configuration it was made with, kept as long as
/// it.
struct Device {
    kind: Kind,
    slot: u8,
    vhf: VHFHANDLE,
    _made_with: Box<VHF_CONFIG>,
}

#[derive(Clone, Copy)]
struct Report {
    len: usize,
    bytes: [u8; INPUT_MAX],
}

/// The system's writes the host has not read, oldest first.
struct Ring {
    items: [Option<Written>; WRITTEN_KEPT],
    head: usize,
    len: usize,
}

impl Ring {
    const fn new() -> Self {
        Self {
            items: [None; WRITTEN_KEPT],
            head: 0,
            len: 0,
        }
    }

    /// Keep `written`; true when the oldest went to make room.
    fn push(&mut self, written: Written) -> bool {
        let full = self.len == WRITTEN_KEPT;
        if full {
            self.head = (self.head + 1) % WRITTEN_KEPT;
            self.len -= 1;
        }
        if let Some(slot) = self.items.get_mut((self.head + self.len) % WRITTEN_KEPT) {
            *slot = Some(written);
            self.len += 1;
        }
        full
    }

    fn pop(&mut self) -> Option<Written> {
        if self.len == 0 {
            return None;
        }
        let item = self.items.get_mut(self.head).and_then(Option::take);
        self.head = (self.head + 1) % WRITTEN_KEPT;
        self.len -= 1;
        item
    }
}

impl State {
    const fn new() -> Self {
        Self {
            kind: None,
            creating: false,
            device: None,
            features: Features::new(),
            last: [None; INPUT_IDS],
            written: Ring::new(),
            dropped: 0,
        }
    }

    /// Remember an input report the system took, for its reads of it.
    fn remember(&mut self, report: &[u8]) {
        let Some(slot) = report
            .first()
            .and_then(|id| usize::from(*id).checked_sub(1))
            .and_then(|at| self.last.get_mut(at))
        else {
            return;
        };
        let mut bytes = [0u8; INPUT_MAX];
        if let Some(head) = bytes.get_mut(..report.len()) {
            head.copy_from_slice(report);
            *slot = Some(Report {
                len: report.len(),
                bytes,
            });
        }
    }
}

/// Submit one input report to the device.
fn submit(vhf: VHFHANDLE, report: &[u8]) -> NTSTATUS {
    let Some(&id) = report.first() else {
        return STATUS_INVALID_PARAMETER;
    };
    let mut packet = HID_XFER_PACKET {
        reportBuffer: report.as_ptr().cast_mut(),
        reportBufferLen: ULONG::try_from(report.len()).unwrap_or(0),
        reportId: id,
    };
    // SAFETY: a device made and not yet deleted, and a packet over a live
    // buffer, which the framework copies before returning and never writes.
    unsafe { VhfReadReportSubmit(vhf, &raw mut packet) }
}

impl File {
    /// A handle's state as it opens: the device beneath opened through the
    /// handle, which the virtual HID framework makes the handle's device on.
    pub(crate) fn open(
        device: WDFDEVICE,
        file: WDFFILEOBJECT,
        pending: WDFQUEUE,
    ) -> Result<Box<File>, NTSTATUS> {
        let mut attributes = object_attributes();
        attributes.ParentObject = file.cast();
        let mut target: WDFIOTARGET = core::ptr::null_mut();
        // SAFETY: the device in hand and live outputs.
        let status = unsafe { wdf_io_target_create(device, &raw mut attributes, &raw mut target) };
        if !nt_success(status) {
            log_warn!(
                "hid: target create refused, status={:#x}",
                status_word(status)
            );
            return Err(status);
        }
        let mut params = WDF_IO_TARGET_OPEN_PARAMS {
            Size: size_of::<WDF_IO_TARGET_OPEN_PARAMS>(),
            Type: WdfIoTargetOpenLocalTargetByFile,
            ..Default::default()
        };
        // SAFETY: the target just made and live parameters.
        let status = unsafe { wdf_io_target_open(target, &raw mut params) };
        if !nt_success(status) {
            log_warn!(
                "hid: target open refused, status={:#x}",
                status_word(status)
            );
            // SAFETY: the target made above, deleted once.
            unsafe { wdf_object_delete(target.cast()) };
            return Err(status);
        }
        Ok(Box::new(File {
            file,
            target,
            pending,
            state: Mutex::new(State::new()),
        }))
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Make the handle's device: one to a handle and one to a kind and slot,
    /// its answers the kind's own with the request's on top.
    pub(crate) fn create(&self, request: &Create) -> NTSTATUS {
        let kind = request.kind;
        let mut features = Features::of(kind);
        for report in request.features.iter() {
            if features.set(report).is_err() {
                return STATUS_INVALID_PARAMETER;
            }
        }
        {
            let mut state = self.lock();
            if state.creating || state.device.is_some() {
                return STATUS_INVALID_DEVICE_STATE;
            }
            state.creating = true;
            state.kind = Some(kind);
            state.features = features;
        }
        // Claimed before the device is made, across every handle: a kind and
        // slot is one device at a time.
        let claimed = live().claim(kind, request.slot);
        if let Err(why) = claimed {
            let mut state = self.lock();
            state.creating = false;
            state.kind = None;
            log_warn!(
                "hid: device refused, kind={kind:?} slot={} why={why:?}",
                request.slot
            );
            return match why {
                Claim::Taken => STATUS_DEVICE_BUSY,
                Claim::Full => STATUS_INSUFFICIENT_RESOURCES,
                Claim::Slot => STATUS_INVALID_PARAMETER,
            };
        }

        let identity = kind.identity();
        let container = container_id(kind, request.slot);
        let descriptor = kind.descriptor();
        // No instance: the framework names each device itself, so no two can
        // share a name, which the system's device manager stops the machine
        // on rather than refusing.
        let mut made = Box::new(VHF_CONFIG {
            Size: size_of::<VHF_CONFIG>(),
            VhfClientContext: core::ptr::from_ref(self).cast_mut().cast(),
            // SAFETY: the target this handle opened.
            FileHandle: unsafe { wdf_io_target_wdm_get_target_file_handle(self.target) },
            VendorID: identity.vendor,
            ProductID: identity.product,
            VersionNumber: identity.version,
            ContainerID: GUID {
                Data1: container.data1,
                Data2: container.data2,
                Data3: container.data3,
                Data4: container.data4,
            },
            ReportDescriptorLength: u16::try_from(descriptor.len()).unwrap_or(0),
            ReportDescriptor: descriptor.as_ptr().cast_mut(),
            EvtVhfAsyncOperationGetFeature: Some(get_feature),
            EvtVhfAsyncOperationSetFeature: Some(set_feature),
            EvtVhfAsyncOperationWriteReport: Some(write_report),
            EvtVhfAsyncOperationGetInputReport: Some(get_input_report),
            ..Default::default()
        });

        let mut vhf: VHFHANDLE = core::ptr::null_mut();
        // SAFETY: a whole configuration whose pointers are static descriptors,
        // and a live output.
        let mut status = unsafe { VhfCreate(&raw mut *made, &raw mut vhf) };
        if nt_success(status) {
            // SAFETY: the device just made.
            status = unsafe { VhfStart(vhf) };
            if !nt_success(status) {
                // SAFETY: the device made above, never started; deleted once.
                unsafe { VhfDelete(vhf, 1) };
            }
        }
        let mut state = self.lock();
        state.creating = false;
        if !nt_success(status) {
            live().release(kind, request.slot);
            state.kind = None;
            log_warn!(
                "hid: device not made, kind={kind:?} slot={} status={:#x}",
                request.slot,
                status_word(status)
            );
            return status;
        }
        state.device = Some(Device {
            kind,
            slot: request.slot,
            vhf,
            _made_with: made,
        });
        log_info!(
            "hid: device made, kind={kind:?} slot={} vendor={:#06x} product={:#06x}",
            request.slot,
            identity.vendor,
            identity.product
        );
        STATUS_SUCCESS
    }

    /// One input report: taken only as its kind declares it.
    pub(crate) fn write(&self, report: &[u8]) -> NTSTATUS {
        let Some((kind, vhf)) = self
            .lock()
            .device
            .as_ref()
            .map(|device| (device.kind, device.vhf))
        else {
            return STATUS_INVALID_DEVICE_STATE;
        };
        if !kind.accepts(report) {
            return STATUS_INVALID_PARAMETER;
        }
        let status = submit(vhf, report);
        if nt_success(status) {
            self.lock().remember(report);
        }
        status
    }

    /// A read of what the system wrote: answered at once with the oldest
    /// kept, or kept waiting for the next write -- none while it waits.
    pub(crate) fn read_written(&self, request: WDFREQUEST) -> Option<(NTSTATUS, usize)> {
        let mut state = self.lock();
        if state.kind.is_none() {
            return Some((STATUS_INVALID_DEVICE_STATE, 0));
        }
        if let Some(written) = state.written.pop() {
            return Some(reply(request, &written));
        }
        // Forwarded under the lock: a write arriving now finds the read
        // waiting rather than a ring nobody reads.
        // SAFETY: the request in hand and the device's own queue.
        let status = unsafe { wdf_request_forward_to_io_queue(request, self.pending) };
        if nt_success(status) {
            None
        } else {
            Some((status, 0))
        }
    }

    /// What the system wrote, to the host: to a read waiting for it, else
    /// kept, the oldest going past [`WRITTEN_KEPT`].
    fn deliver(&self, written: Written) {
        let mut state = self.lock();
        let mut request: WDFREQUEST = core::ptr::null_mut();
        // SAFETY: the device's own queue and this handle's file object.
        let found = unsafe {
            wdf_io_queue_retrieve_request_by_file_object(self.pending, self.file, &raw mut request)
        };
        if nt_success(found) && !request.is_null() {
            let (status, len) = reply(request, &written);
            // SAFETY: the request just taken from the queue, completed once.
            unsafe {
                wdf_request_complete_with_information(
                    request,
                    status,
                    ULONG_PTR::try_from(len).unwrap_or(0),
                );
            }
            return;
        }
        if state.written.push(written) {
            state.dropped = state.dropped.saturating_add(1);
            if state.dropped == 1 {
                log_warn!(
                    "hid: a write the host had not read went, kind={:?} kept={WRITTEN_KEPT}",
                    state.kind
                );
            }
        }
    }

    /// The handle's cleanup: every read still waiting is cancelled, so the
    /// handle can close.
    pub(crate) fn cleanup(&self) {
        loop {
            let mut request: WDFREQUEST = core::ptr::null_mut();
            // SAFETY: the device's own queue and this handle's file object.
            let found = unsafe {
                wdf_io_queue_retrieve_request_by_file_object(
                    self.pending,
                    self.file,
                    &raw mut request,
                )
            };
            if !nt_success(found) || request.is_null() {
                break;
            }
            // SAFETY: the request just taken from the queue, completed once.
            unsafe { wdf_request_complete(request, STATUS_CANCELLED) };
        }
    }
}

/// The handle's close: **the kind's neutral reports, time for them to arrive,
/// then the device goes**, so a host that ends, a killed one included, leaves
/// nothing held; then the state goes with it.
pub(crate) fn close(file: Box<File>) {
    let (device, dropped) = {
        let mut state = file.lock();
        (state.device.take(), state.dropped)
    };
    if let Some(device) = device {
        for report in device.kind.neutral() {
            let status = submit(device.vhf, report);
            if !nt_success(status) {
                log_warn!(
                    "hid: neutral report refused, kind={:?} status={:#x}",
                    device.kind,
                    status_word(status)
                );
            }
        }
        std::thread::sleep(NEUTRAL_SETTLE);
        // SAFETY: the device made on this handle, deleted once; the delete
        // waits, so no callback reaches the state after it.
        unsafe { VhfDelete(device.vhf, 1) };
        live().release(device.kind, device.slot);
        log_info!(
            "hid: device removed, kind={:?} slot={} writes_dropped={dropped}",
            device.kind,
            device.slot
        );
    }
    // SAFETY: the target this handle opened, deleted once, after the device
    // that used its handle.
    unsafe { wdf_object_delete(file.target.cast()) };
}

/// Write `written` into a read's output.
fn reply(request: WDFREQUEST, written: &Written) -> (NTSTATUS, usize) {
    let mut buffer: PVOID = core::ptr::null_mut();
    let mut length = 0usize;
    // SAFETY: the request in hand; live outputs.
    let status = unsafe {
        wdf_request_retrieve_output_buffer(request, Written::LEN, &raw mut buffer, &raw mut length)
    };
    if !nt_success(status) || buffer.is_null() {
        return (STATUS_BUFFER_TOO_SMALL, 0);
    }
    // SAFETY: the framework's buffer for this request, `length` bytes; no
    // other view of it is alive.
    let out = unsafe { core::slice::from_raw_parts_mut(buffer.cast::<u8>(), length) };
    match written.encode(out) {
        Ok(len) => (STATUS_SUCCESS, len),
        Err(_) => (STATUS_BUFFER_TOO_SMALL, 0),
    }
}

/// The handle's state a framework callback's context word names.
///
/// # Safety
///
/// The context is the one `create` gave the framework: a live handle's state.
unsafe fn from_context<'a>(context: PVOID) -> Option<&'a File> {
    // SAFETY: the caller's contract.
    unsafe { context.cast::<File>().as_ref() }
}

/// The report a packet carries, identifier byte first, as the system writes
/// one.
///
/// # Safety
///
/// The packet is the framework's, live for the callback.
unsafe fn carried<'a>(packet: &HID_XFER_PACKET) -> &'a [u8] {
    let len = usize::try_from(packet.reportBufferLen).unwrap_or(0);
    if packet.reportBuffer.is_null() {
        return &[];
    }
    // SAFETY: the framework's buffer, `reportBufferLen` bytes.
    unsafe { core::slice::from_raw_parts(packet.reportBuffer, len) }
}

/// Answer a read with `report`, identifier byte first.
fn fill(packet: &mut HID_XFER_PACKET, report: &[u8]) -> NTSTATUS {
    let room = usize::try_from(packet.reportBufferLen).unwrap_or(0);
    if packet.reportBuffer.is_null() || room < report.len() {
        return STATUS_BUFFER_TOO_SMALL;
    }
    // SAFETY: the framework's buffer, `reportBufferLen` bytes, written here
    // alone.
    let out = unsafe { core::slice::from_raw_parts_mut(packet.reportBuffer, room) };
    match out.get_mut(..report.len()) {
        Some(head) => {
            head.copy_from_slice(report);
            STATUS_SUCCESS
        }
        None => STATUS_BUFFER_TOO_SMALL,
    }
}

/// **A feature read is answered from the device's table**, and an identifier
/// nobody defined is refused.
unsafe extern "C" fn get_feature(
    context: PVOID,
    operation: VHFOPERATIONHANDLE,
    _operation_context: PVOID,
    packet: PHID_XFER_PACKET,
) {
    // SAFETY: the framework's arguments for this callback.
    let status = match (unsafe { from_context(context) }, unsafe { packet.as_mut() }) {
        (Some(file), Some(packet)) => {
            let state = file.lock();
            match state.features.get(packet.reportId) {
                Some(report) => fill(packet, report),
                None => STATUS_INVALID_PARAMETER,
            }
        }
        _ => STATUS_INVALID_PARAMETER,
    };
    // SAFETY: the operation this callback was handed, completed once.
    unsafe { VhfAsyncOperationComplete(operation, status) };
}

/// **A feature write is answered at once and handed to no one**
/// (docs/11-drivers.md section 2.3): a pad's features carry its pairing, test
/// commands and firmware update, none a game needs, so nothing of it travels.
/// The mouse keeps its multipliers as their answer, which the system reads
/// back.
unsafe extern "C" fn set_feature(
    context: PVOID,
    operation: VHFOPERATIONHANDLE,
    _operation_context: PVOID,
    packet: PHID_XFER_PACKET,
) {
    // SAFETY: the framework's arguments for this callback.
    let status = match (unsafe { from_context(context) }, unsafe { packet.as_ref() }) {
        (Some(file), Some(packet)) => {
            // SAFETY: the packet is live for the callback.
            let report = unsafe { carried(packet) };
            let mut state = file.lock();
            let keeps = state.kind.is_some_and(Kind::keeps_feature_writes);
            if !keeps || state.features.set(report).is_ok() {
                STATUS_SUCCESS
            } else {
                STATUS_INVALID_PARAMETER
            }
        }
        _ => STATUS_INVALID_PARAMETER,
    };
    // SAFETY: the operation this callback was handed, completed once.
    unsafe { VhfAsyncOperationComplete(operation, status) };
}

/// **An output report is handed to the host and completed at once**, so a
/// program's write never waits on the host's loop.
unsafe extern "C" fn write_report(
    context: PVOID,
    operation: VHFOPERATIONHANDLE,
    _operation_context: PVOID,
    packet: PHID_XFER_PACKET,
) {
    // SAFETY: the framework's arguments for this callback.
    let status = match (unsafe { from_context(context) }, unsafe { packet.as_ref() }) {
        (Some(file), Some(packet)) => {
            // SAFETY: the packet is live for the callback.
            let report = unsafe { carried(packet) };
            match Written::new(WrittenKind::Output, report) {
                Some(written) => {
                    file.deliver(written);
                    STATUS_SUCCESS
                }
                None => {
                    log_warn!(
                        "hid: an output report past {FEATURE_MAX} bytes, len={}",
                        report.len()
                    );
                    STATUS_INVALID_PARAMETER
                }
            }
        }
        _ => STATUS_INVALID_PARAMETER,
    };
    // SAFETY: the operation this callback was handed, completed once.
    unsafe { VhfAsyncOperationComplete(operation, status) };
}

/// A read of an input report: the last one submitted with its identifier,
/// else the kind's report at rest.
unsafe extern "C" fn get_input_report(
    context: PVOID,
    operation: VHFOPERATIONHANDLE,
    _operation_context: PVOID,
    packet: PHID_XFER_PACKET,
) {
    // SAFETY: the framework's arguments for this callback.
    let status = match (unsafe { from_context(context) }, unsafe { packet.as_mut() }) {
        (Some(file), Some(packet)) => {
            let state = file.lock();
            let id = packet.reportId;
            let last = usize::from(id)
                .checked_sub(1)
                .and_then(|at| state.last.get(at).copied().flatten());
            match last {
                Some(report) => fill(packet, report.bytes.get(..report.len).unwrap_or(&[])),
                None => match state
                    .kind
                    .and_then(|kind| kind.neutral().iter().find(|r| r.first() == Some(&id)))
                {
                    Some(report) => fill(packet, report),
                    None => STATUS_INVALID_PARAMETER,
                },
            }
        }
        _ => STATUS_INVALID_PARAMETER,
    };
    // SAFETY: the operation this callback was handed, completed once.
    unsafe { VhfAsyncOperationComplete(operation, status) };
}
