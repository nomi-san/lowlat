//! The framework's callbacks: the driver and its device, the file object each
//! handle is, and the control interface's requests (docs/11-drivers.md
//! section 2.2).

use std::sync::atomic::{AtomicPtr, Ordering};

use lowlat_common::{log_info, log_warn};
use lowlat_hid_proto::control::{
    Create, IOCTL_CREATE, IOCTL_VERSION, IOCTL_WRITTEN, Refused, VERSION, Version,
};
use lowlat_hid_proto::{CONTROL_INTERFACE, MAX_DEVICES};

use crate::device::{self, File};
use crate::ffi::{
    GUID, NTSTATUS, PCUNICODE_STRING, PDRIVER_OBJECT, PVOID, PWDFDEVICE_INIT,
    STATUS_BUFFER_TOO_SMALL, STATUS_INSUFFICIENT_RESOURCES, STATUS_INVALID_DEVICE_REQUEST,
    STATUS_INVALID_PARAMETER, STATUS_SUCCESS, ULONG, ULONG_PTR, WDF_DRIVER_CONFIG,
    WDF_FILEOBJECT_CONFIG, WDF_IO_QUEUE_CONFIG, WDF_OBJECT_CONTEXT_TYPE_INFO, WDFDEVICE, WDFDRIVER,
    WDFFILEOBJECT, WDFQUEUE, WDFQUEUE__, WDFREQUEST, WdfFalse, WdfFileObjectWdfCannotUseFsContexts,
    WdfIoQueueDispatchManual, WdfIoQueueDispatchParallel, nt_success, object_attributes, size_of,
    status_word, wdf_device_create, wdf_device_create_device_interface,
    wdf_device_init_set_file_object_config, wdf_driver_create, wdf_fdo_init_set_filter,
    wdf_io_queue_create, wdf_object_get_typed_context_worker,
    wdf_request_complete_with_information, wdf_request_get_file_object,
    wdf_request_retrieve_input_buffer, wdf_request_retrieve_output_buffer,
};

/// The queue a read of what was written waits in, the device's own. One
/// device carries the driver, so one queue.
static PENDING: AtomicPtr<WDFQUEUE__> = AtomicPtr::new(core::ptr::null_mut());

/// The context each file object carries: the pointer to its handle's state.
struct ContextType(WDF_OBJECT_CONTEXT_TYPE_INFO);

// SAFETY: a description the framework reads and nothing writes.
unsafe impl Sync for ContextType {}

#[allow(clippy::cast_possible_truncation)] // a structure of five words
static FILE_CONTEXT: ContextType = ContextType(WDF_OBJECT_CONTEXT_TYPE_INFO {
    Size: core::mem::size_of::<WDF_OBJECT_CONTEXT_TYPE_INFO>() as ULONG,
    ContextName: c"lowlat-hid handle".as_ptr(),
    ContextSize: core::mem::size_of::<*mut File>(),
    UniqueType: &raw const FILE_CONTEXT.0,
    EvtDriverGetUniqueContextType: None,
});

/// Create the framework's driver object, from `DriverEntry`.
///
/// # Safety
///
/// The framework's own arguments to `DriverEntry`.
pub(crate) unsafe fn entry(driver: PDRIVER_OBJECT, registry: PCUNICODE_STRING) -> NTSTATUS {
    let mut config = WDF_DRIVER_CONFIG {
        Size: size_of::<WDF_DRIVER_CONFIG>(),
        EvtDriverDeviceAdd: Some(device_add),
        ..Default::default()
    };
    let mut attributes = object_attributes();
    // SAFETY: the caller's arguments; live configuration for the call.
    let status = unsafe {
        wdf_driver_create(
            driver,
            registry,
            &raw mut attributes,
            &raw mut config,
            core::ptr::null_mut(),
        )
    };
    if !nt_success(status) {
        log_warn!(
            "hid: driver create refused, status={:#x}",
            status_word(status)
        );
    }
    status
}

fn guid(id: lowlat_hid_proto::Guid) -> GUID {
    GUID {
        Data1: id.data1,
        Data2: id.data2,
        Data3: id.data3,
        Data4: id.data4,
    }
}

unsafe extern "C" fn device_add(_driver: WDFDRIVER, init: PWDFDEVICE_INIT) -> NTSTATUS {
    let mut init = init;
    // A filter above the virtual HID framework, which is the device's function
    // driver and makes the HID devices asked of it.
    // SAFETY: the framework's device init, before the device is created.
    unsafe { wdf_fdo_init_set_filter(init) };
    // A file object per handle, carrying the pointer to its state. A handle's
    // create, cleanup and close stop here rather than going on down: the
    // handle is this driver's.
    let mut files = WDF_FILEOBJECT_CONFIG {
        Size: size_of::<WDF_FILEOBJECT_CONFIG>(),
        EvtDeviceFileCreate: Some(file_create),
        EvtFileClose: Some(file_close),
        EvtFileCleanup: Some(file_cleanup),
        AutoForwardCleanupClose: WdfFalse,
        FileObjectClass: WdfFileObjectWdfCannotUseFsContexts,
    };
    let mut file_attributes = object_attributes();
    file_attributes.ContextTypeInfo = &raw const FILE_CONTEXT.0;
    // SAFETY: as above; live configuration for the call.
    unsafe {
        wdf_device_init_set_file_object_config(init, &raw mut files, &raw mut file_attributes);
    }
    let mut attributes = object_attributes();
    let mut device: WDFDEVICE = core::ptr::null_mut();
    // SAFETY: as above; the framework takes the init.
    let status = unsafe { wdf_device_create(&raw mut init, &raw mut attributes, &raw mut device) };
    if !nt_success(status) {
        log_warn!(
            "hid: device create refused, status={:#x}",
            status_word(status)
        );
        return status;
    }

    // Control requests and writes in parallel, each handle being a device of
    // its own; never held for power, the device being software.
    let mut config = WDF_IO_QUEUE_CONFIG {
        Size: size_of::<WDF_IO_QUEUE_CONFIG>(),
        DispatchType: WdfIoQueueDispatchParallel,
        PowerManaged: WdfFalse,
        DefaultQueue: 1,
        EvtIoDeviceControl: Some(io_control),
        EvtIoWrite: Some(io_write),
        ..Default::default()
    };
    config.Settings.Parallel.NumberOfPresentedRequests = ULONG::MAX;
    let mut queue: WDFQUEUE = core::ptr::null_mut();
    // SAFETY: the device just created; live configuration and output.
    let status = unsafe {
        wdf_io_queue_create(
            device,
            &raw mut config,
            core::ptr::null_mut(),
            &raw mut queue,
        )
    };
    if !nt_success(status) {
        log_warn!(
            "hid: queue create refused, status={:#x}",
            status_word(status)
        );
        return status;
    }
    // The reads of what was written wait here, each found again by its
    // handle's file object.
    let mut manual = WDF_IO_QUEUE_CONFIG {
        Size: size_of::<WDF_IO_QUEUE_CONFIG>(),
        DispatchType: WdfIoQueueDispatchManual,
        PowerManaged: WdfFalse,
        ..Default::default()
    };
    let mut pending: WDFQUEUE = core::ptr::null_mut();
    // SAFETY: as above.
    let status = unsafe {
        wdf_io_queue_create(
            device,
            &raw mut manual,
            core::ptr::null_mut(),
            &raw mut pending,
        )
    };
    if !nt_success(status) {
        log_warn!(
            "hid: pending queue create refused, status={:#x}",
            status_word(status)
        );
        return status;
    }
    PENDING.store(pending, Ordering::Release);

    // The control interface; the device's own access, from its setup
    // information, admits the system account and administrators alone.
    let interface = guid(CONTROL_INTERFACE);
    // SAFETY: the device just created; no reference string.
    let status = unsafe {
        wdf_device_create_device_interface(device, &raw const interface, core::ptr::null())
    };
    if !nt_success(status) {
        log_warn!(
            "hid: control interface refused, status={:#x}",
            status_word(status)
        );
        return status;
    }
    log_info!("hid: device added, max_devices={MAX_DEVICES}");
    STATUS_SUCCESS
}

/// Where a file object keeps the pointer to its handle's state.
fn context(file: WDFFILEOBJECT) -> Option<*mut *mut File> {
    if file.is_null() {
        return None;
    }
    // SAFETY: a file object of this device, whose attributes name the type.
    let slot =
        unsafe { wdf_object_get_typed_context_worker(file.cast(), &raw const FILE_CONTEXT.0) };
    (!slot.is_null()).then_some(slot.cast())
}

/// The state of the handle `file` is, between its create and its close.
fn state<'a>(file: WDFFILEOBJECT) -> Option<&'a File> {
    let slot = context(file)?;
    // SAFETY: the context the framework keeps for the file object, holding
    // null or a pointer from `file_create` that `file_close` alone frees.
    unsafe { (*slot).as_ref() }
}

unsafe extern "C" fn file_create(device: WDFDEVICE, request: WDFREQUEST, file: WDFFILEOBJECT) {
    let status = match (
        context(file),
        File::open(device, file, PENDING.load(Ordering::Acquire)),
    ) {
        (Some(slot), Ok(state)) => {
            // SAFETY: the file object's own context, written before any
            // request on the handle can arrive.
            unsafe { *slot = Box::into_raw(state) };
            STATUS_SUCCESS
        }
        (None, Ok(state)) => {
            device::close(state);
            STATUS_INSUFFICIENT_RESOURCES
        }
        (_, Err(status)) => status,
    };
    // SAFETY: the create request in hand, completed once.
    unsafe { wdf_request_complete_with_information(request, status, 0) };
}

unsafe extern "C" fn file_cleanup(file: WDFFILEOBJECT) {
    if let Some(state) = state(file) {
        state.cleanup();
    }
}

unsafe extern "C" fn file_close(file: WDFFILEOBJECT) {
    let Some(slot) = context(file) else {
        return;
    };
    // SAFETY: the file object's own context; the close is its last use.
    let pointer = unsafe { core::ptr::replace(slot, core::ptr::null_mut()) };
    if !pointer.is_null() {
        // SAFETY: the pointer `file_create` made from a box, taken back once.
        device::close(unsafe { Box::from_raw(pointer) });
    }
}

/// What a refused request answers.
fn refused(why: Refused) -> NTSTATUS {
    match why {
        Refused::Short | Refused::Room => STATUS_BUFFER_TOO_SMALL,
        Refused::Length | Refused::Range => STATUS_INVALID_PARAMETER,
    }
}

/// The request's input, whole.
fn input<'a>(request: WDFREQUEST) -> Result<&'a [u8], NTSTATUS> {
    let mut buffer: PVOID = core::ptr::null_mut();
    let mut length = 0usize;
    // SAFETY: the request in hand; live outputs.
    let status =
        unsafe { wdf_request_retrieve_input_buffer(request, 1, &raw mut buffer, &raw mut length) };
    if !nt_success(status) || buffer.is_null() {
        return Err(STATUS_BUFFER_TOO_SMALL);
    }
    // SAFETY: the framework's buffer for this request, `length` bytes, read
    // before any reply is written into it.
    Ok(unsafe { core::slice::from_raw_parts(buffer.cast::<u8>(), length) })
}

/// Write the reply into the request's output.
fn reply(
    request: WDFREQUEST,
    encode: impl FnOnce(&mut [u8]) -> Result<usize, Refused>,
) -> (NTSTATUS, usize) {
    let mut buffer: PVOID = core::ptr::null_mut();
    let mut length = 0usize;
    // SAFETY: the request in hand; live outputs.
    let status =
        unsafe { wdf_request_retrieve_output_buffer(request, 1, &raw mut buffer, &raw mut length) };
    if !nt_success(status) || buffer.is_null() {
        return (STATUS_BUFFER_TOO_SMALL, 0);
    }
    // SAFETY: the framework's buffer for this request, `length` bytes; no
    // other view of it is alive.
    let out = unsafe { core::slice::from_raw_parts_mut(buffer.cast::<u8>(), length) };
    match encode(out) {
        Ok(written) => (STATUS_SUCCESS, written),
        Err(why) => (refused(why), 0),
    }
}

fn complete(request: WDFREQUEST, status: NTSTATUS, written: usize) {
    // SAFETY: the request in hand, completed once.
    unsafe {
        wdf_request_complete_with_information(
            request,
            status,
            ULONG_PTR::try_from(written).unwrap_or(0),
        );
    }
}

unsafe extern "C" fn io_control(
    _queue: WDFQUEUE,
    request: WDFREQUEST,
    _output_length: usize,
    _input_length: usize,
    code: ULONG,
) {
    // SAFETY: the request in hand.
    let Some(handle) = state(unsafe { wdf_request_get_file_object(request) }) else {
        complete(request, STATUS_INVALID_DEVICE_REQUEST, 0);
        return;
    };
    let (status, written) = match code {
        IOCTL_VERSION => {
            let version = Version {
                version: VERSION,
                max_devices: u32::try_from(MAX_DEVICES).unwrap_or(0),
            };
            reply(request, |out| version.encode(out))
        }
        IOCTL_CREATE => {
            match input(request).and_then(|bytes| Create::decode(bytes).map_err(refused)) {
                Ok(create) => (handle.create(&create), 0),
                Err(status) => (status, 0),
            }
        }
        IOCTL_WRITTEN => match handle.read_written(request) {
            Some(done) => done,
            // Waiting for the system's next write.
            None => return,
        },
        _ => (STATUS_INVALID_DEVICE_REQUEST, 0),
    };
    complete(request, status, written);
}

/// An input report, written to the handle's device.
unsafe extern "C" fn io_write(_queue: WDFQUEUE, request: WDFREQUEST, length: usize) {
    // SAFETY: the request in hand.
    let Some(handle) = state(unsafe { wdf_request_get_file_object(request) }) else {
        complete(request, STATUS_INVALID_DEVICE_REQUEST, 0);
        return;
    };
    let status = match input(request) {
        Ok(report) => handle.write(report),
        Err(status) => status,
    };
    complete(request, status, if nt_success(status) { length } else { 0 });
}
