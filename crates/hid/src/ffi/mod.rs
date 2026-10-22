//! The frameworks' declarations, and the calls this driver makes through them.
//!
//! `umdf.rs` and `status.rs` are generated from the driver kit's and the SDK's
//! headers by `scripts/gen-hid-bindings.sh` and committed. The user-mode
//! framework's calls are inline functions in its headers, each an entry of a
//! function table its stub library fills when the framework binds the driver;
//! they are written here, one per call made, behind the same availability
//! check the headers make. An entry the running framework lacks answers with a
//! status, never a panic. The virtual HID framework's calls are its library's
//! own functions, declared as generated.

#[allow(
    dead_code,
    non_camel_case_types,
    non_snake_case,
    non_upper_case_globals,
    unreachable_pub,
    missing_debug_implementations,
    clippy::all,
    clippy::pedantic
)]
mod umdf;

#[allow(dead_code, unreachable_pub)]
mod status;

pub(crate) use status::*;
pub(crate) use umdf::*;

/// The size a framework structure says it is, in the word its `Size` field
/// takes.
pub(crate) fn size_of<T>() -> u32 {
    u32::try_from(core::mem::size_of::<T>()).unwrap_or(u32::MAX)
}

/// A status the frameworks count as success.
pub(crate) const fn nt_success(status: NTSTATUS) -> bool {
    status >= 0
}

/// A status as the word it is, for a log line.
pub(crate) fn status_word(status: NTSTATUS) -> u32 {
    u32::from_ne_bytes(status.to_ne_bytes())
}

/// The empty attributes a framework object takes when the driver keeps no
/// context on it: `WDF_OBJECT_ATTRIBUTES_INIT`.
pub(crate) fn object_attributes() -> WDF_OBJECT_ATTRIBUTES {
    WDF_OBJECT_ATTRIBUTES {
        Size: size_of::<WDF_OBJECT_ATTRIBUTES>(),
        ExecutionLevel: WdfExecutionLevelInheritFromParent,
        SynchronizationScope: WdfSynchronizationScopeInheritFromParent,
        ..Default::default()
    }
}

/// The user-mode framework's entry at `index`, or none where the framework
/// that loaded this driver is older than its table.
///
/// # Safety
///
/// After the framework bound the driver, which is before `DriverEntry`.
unsafe fn wdf_entry(index: WDFFUNCENUM) -> WDFFUNC {
    let index = usize::try_from(index).ok()?;
    // SAFETY: the stub's globals, written once when the framework bound the
    // driver and only read after.
    let (table, newer, count) = unsafe {
        (
            WdfFunctions_02025,
            WdfClientVersionHigherThanFramework,
            WdfFunctionCount,
        )
    };
    if table.is_null() || (newer != 0 && index >= usize::try_from(count).ok()?) {
        return None;
    }
    // SAFETY: within the table the framework filled.
    unsafe { *table.add(index) }
}

macro_rules! wdf_calls {
    ($(
        $(#[$meta:meta])*
        fn $name:ident = $pfn:ident [$index:ident] ($($arg:ident: $ty:ty),*) $(-> $ret:ty)? => $missing:expr;
    )*) => {$(
        $(#[$meta])*
        ///
        /// # Safety
        ///
        /// The framework's own contract for the call.
        pub(crate) unsafe fn $name($($arg: $ty),*) $(-> $ret)? {
            // SAFETY: the entry at this index is this call, as the headers
            // declare it; both are optional function pointers of one size.
            let call = unsafe { core::mem::transmute::<WDFFUNC, $pfn>(wdf_entry($index)) };
            match call {
                // SAFETY: the caller's contract; the globals are the stub's.
                Some(call) => unsafe { call(WdfDriverGlobals, $($arg),*) },
                None => $missing,
            }
        }
    )*};
}

/// What a call answers when the running framework has no such entry.
const MISSING: NTSTATUS = STATUS_NOT_SUPPORTED;

wdf_calls! {
    fn wdf_driver_create = PFN_WDFDRIVERCREATE [WdfDriverCreateTableIndex]
        (driver: PDRIVER_OBJECT, registry: PCUNICODE_STRING, attributes: PWDF_OBJECT_ATTRIBUTES,
         config: PWDF_DRIVER_CONFIG, out: *mut WDFDRIVER) -> NTSTATUS => MISSING;
    fn wdf_fdo_init_set_filter = PFN_WDFFDOINITSETFILTER [WdfFdoInitSetFilterTableIndex]
        (init: PWDFDEVICE_INIT) => ();
    fn wdf_device_init_set_file_object_config =
        PFN_WDFDEVICEINITSETFILEOBJECTCONFIG [WdfDeviceInitSetFileObjectConfigTableIndex]
        (init: PWDFDEVICE_INIT, config: PWDF_FILEOBJECT_CONFIG, attributes: PWDF_OBJECT_ATTRIBUTES) => ();
    fn wdf_device_create = PFN_WDFDEVICECREATE [WdfDeviceCreateTableIndex]
        (init: *mut PWDFDEVICE_INIT, attributes: PWDF_OBJECT_ATTRIBUTES, out: *mut WDFDEVICE) -> NTSTATUS => MISSING;
    fn wdf_device_create_device_interface =
        PFN_WDFDEVICECREATEDEVICEINTERFACE [WdfDeviceCreateDeviceInterfaceTableIndex]
        (device: WDFDEVICE, class: *const GUID, reference: PCUNICODE_STRING) -> NTSTATUS => MISSING;
    fn wdf_io_queue_create = PFN_WDFIOQUEUECREATE [WdfIoQueueCreateTableIndex]
        (device: WDFDEVICE, config: PWDF_IO_QUEUE_CONFIG, attributes: PWDF_OBJECT_ATTRIBUTES,
         out: *mut WDFQUEUE) -> NTSTATUS => MISSING;
    fn wdf_io_queue_retrieve_request_by_file_object =
        PFN_WDFIOQUEUERETRIEVEREQUESTBYFILEOBJECT [WdfIoQueueRetrieveRequestByFileObjectTableIndex]
        (queue: WDFQUEUE, file: WDFFILEOBJECT, out: *mut WDFREQUEST) -> NTSTATUS => MISSING;
    fn wdf_request_forward_to_io_queue =
        PFN_WDFREQUESTFORWARDTOIOQUEUE [WdfRequestForwardToIoQueueTableIndex]
        (request: WDFREQUEST, queue: WDFQUEUE) -> NTSTATUS => MISSING;
    fn wdf_request_complete = PFN_WDFREQUESTCOMPLETE [WdfRequestCompleteTableIndex]
        (request: WDFREQUEST, status: NTSTATUS) => ();
    fn wdf_request_complete_with_information =
        PFN_WDFREQUESTCOMPLETEWITHINFORMATION [WdfRequestCompleteWithInformationTableIndex]
        (request: WDFREQUEST, status: NTSTATUS, information: ULONG_PTR) => ();
    fn wdf_request_retrieve_input_buffer =
        PFN_WDFREQUESTRETRIEVEINPUTBUFFER [WdfRequestRetrieveInputBufferTableIndex]
        (request: WDFREQUEST, minimum: usize, buffer: *mut PVOID, length: *mut usize) -> NTSTATUS => MISSING;
    fn wdf_request_retrieve_output_buffer =
        PFN_WDFREQUESTRETRIEVEOUTPUTBUFFER [WdfRequestRetrieveOutputBufferTableIndex]
        (request: WDFREQUEST, minimum: usize, buffer: *mut PVOID, length: *mut usize) -> NTSTATUS => MISSING;
    fn wdf_request_get_file_object = PFN_WDFREQUESTGETFILEOBJECT [WdfRequestGetFileObjectTableIndex]
        (request: WDFREQUEST) -> WDFFILEOBJECT => core::ptr::null_mut();
    fn wdf_io_target_create = PFN_WDFIOTARGETCREATE [WdfIoTargetCreateTableIndex]
        (device: WDFDEVICE, attributes: PWDF_OBJECT_ATTRIBUTES, out: *mut WDFIOTARGET) -> NTSTATUS => MISSING;
    fn wdf_io_target_open = PFN_WDFIOTARGETOPEN [WdfIoTargetOpenTableIndex]
        (target: WDFIOTARGET, params: PWDF_IO_TARGET_OPEN_PARAMS) -> NTSTATUS => MISSING;
    fn wdf_io_target_wdm_get_target_file_handle =
        PFN_WDFIOTARGETWDMGETTARGETFILEHANDLE [WdfIoTargetWdmGetTargetFileHandleTableIndex]
        (target: WDFIOTARGET) -> HANDLE => core::ptr::null_mut();
    fn wdf_object_delete = PFN_WDFOBJECTDELETE [WdfObjectDeleteTableIndex] (object: WDFOBJECT) => ();
    fn wdf_object_get_typed_context_worker =
        PFN_WDFOBJECTGETTYPEDCONTEXTWORKER [WdfObjectGetTypedContextWorkerTableIndex]
        (object: WDFOBJECT, info: PCWDF_OBJECT_CONTEXT_TYPE_INFO) -> PVOID => core::ptr::null_mut();
}
