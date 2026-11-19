//! The frameworks' declarations, and the calls this driver makes through them.
//!
//! `umdf.rs` and `status.rs` are generated from the driver kit's headers by
//! `scripts/gen-vdd-bindings.sh` and committed. The frameworks' calls are inline
//! functions in those headers, each an entry of a function table the stub
//! libraries fill when the framework binds the driver; they are written here,
//! one per call made, behind the same availability check the headers make. An
//! entry the running framework lacks answers with a status, never a panic.

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

/// The indirect display class's entry at `index`, as [`wdf_entry`].
///
/// # Safety
///
/// As [`wdf_entry`].
unsafe fn idd_entry(index: IDDFUNCENUM) -> PFN_IDD_CX {
    let index = usize::try_from(index).ok()?;
    // SAFETY: the stub's globals, as in `wdf_entry`; the table is an array the
    // stub defines, its address taken without a reference.
    let (table, newer, count) = unsafe {
        (
            (&raw const IddFunctions).cast::<PFN_IDD_CX>(),
            IddClientVersionHigherThanFramework,
            IddFunctionCount,
        )
    };
    if newer != 0 && index >= usize::try_from(count).ok()? {
        return None;
    }
    if index >= usize::try_from(IddFunctionTableNumEntries).ok()? {
        return None;
    }
    // SAFETY: within the table the class filled.
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

macro_rules! idd_calls {
    ($(
        $(#[$meta:meta])*
        fn $name:ident = $pfn:ident [$index:ident] ($($arg:ident: $ty:ty),*) $(-> $ret:ty)? => $missing:expr;
    )*) => {$(
        $(#[$meta])*
        ///
        /// # Safety
        ///
        /// The class's own contract for the call.
        pub(crate) unsafe fn $name($($arg: $ty),*) $(-> $ret)? {
            // SAFETY: as in `wdf_calls`.
            let call = unsafe { core::mem::transmute::<PFN_IDD_CX, $pfn>(idd_entry($index)) };
            match call {
                // SAFETY: the caller's contract; the globals are the stub's.
                Some(call) => unsafe { call(IddDriverGlobals, $($arg),*) },
                None => $missing,
            }
        }
    )*};
}

/// What a call answers when the running framework has no such entry.
const MISSING: NTSTATUS = STATUS_NOT_SUPPORTED;
/// The same, for a call that answers in the graphics stack's form.
const MISSING_HR: HRESULT = E_NOTIMPL_HR;
/// `E_NOTIMPL`, which no header here defines as a value.
const E_NOTIMPL_HR: HRESULT = 0x8000_4001_u32 as HRESULT;

wdf_calls! {
    fn wdf_driver_create = PFN_WDFDRIVERCREATE [WdfDriverCreateTableIndex]
        (driver: PDRIVER_OBJECT, registry: PCUNICODE_STRING, attributes: PWDF_OBJECT_ATTRIBUTES,
         config: PWDF_DRIVER_CONFIG, out: *mut WDFDRIVER) -> NTSTATUS => MISSING;
    fn wdf_device_init_set_pnp_power_event_callbacks =
        PFN_WDFDEVICEINITSETPNPPOWEREVENTCALLBACKS [WdfDeviceInitSetPnpPowerEventCallbacksTableIndex]
        (init: PWDFDEVICE_INIT, callbacks: PWDF_PNPPOWER_EVENT_CALLBACKS) => ();
    fn wdf_device_init_set_file_object_config =
        PFN_WDFDEVICEINITSETFILEOBJECTCONFIG [WdfDeviceInitSetFileObjectConfigTableIndex]
        (init: PWDFDEVICE_INIT, config: PWDF_FILEOBJECT_CONFIG, attributes: PWDF_OBJECT_ATTRIBUTES) => ();
    fn wdf_device_create = PFN_WDFDEVICECREATE [WdfDeviceCreateTableIndex]
        (init: *mut PWDFDEVICE_INIT, attributes: PWDF_OBJECT_ATTRIBUTES, out: *mut WDFDEVICE) -> NTSTATUS => MISSING;
    fn wdf_device_create_device_interface =
        PFN_WDFDEVICECREATEDEVICEINTERFACE [WdfDeviceCreateDeviceInterfaceTableIndex]
        (device: WDFDEVICE, class: *const GUID, reference: PCUNICODE_STRING) -> NTSTATUS => MISSING;
    fn wdf_object_delete = PFN_WDFOBJECTDELETE [WdfObjectDeleteTableIndex] (object: WDFOBJECT) => ();
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
}

idd_calls! {
    fn idd_device_init_config = PFN_IDDCXDEVICEINITCONFIG [IddCxDeviceInitConfigTableIndex]
        (init: PWDFDEVICE_INIT, config: *const IDD_CX_CLIENT_CONFIG) -> NTSTATUS => MISSING;
    fn idd_device_initialize = PFN_IDDCXDEVICEINITIALIZE [IddCxDeviceInitializeTableIndex]
        (device: WDFDEVICE) -> NTSTATUS => MISSING;
    fn idd_adapter_init_async = PFN_IDDCXADAPTERINITASYNC [IddCxAdapterInitAsyncTableIndex]
        (args: *const IDARG_IN_ADAPTER_INIT, out: *mut IDARG_OUT_ADAPTER_INIT) -> NTSTATUS => MISSING;
    fn idd_monitor_create = PFN_IDDCXMONITORCREATE [IddCxMonitorCreateTableIndex]
        (adapter: IDDCX_ADAPTER, args: *const IDARG_IN_MONITORCREATE, out: *mut IDARG_OUT_MONITORCREATE) -> NTSTATUS => MISSING;
    fn idd_monitor_arrival = PFN_IDDCXMONITORARRIVAL [IddCxMonitorArrivalTableIndex]
        (monitor: IDDCX_MONITOR, out: *mut IDARG_OUT_MONITORARRIVAL) -> NTSTATUS => MISSING;
    fn idd_monitor_departure = PFN_IDDCXMONITORDEPARTURE [IddCxMonitorDepartureTableIndex]
        (monitor: IDDCX_MONITOR) -> NTSTATUS => MISSING;
    fn idd_monitor_setup_hardware_cursor =
        PFN_IDDCXMONITORSETUPHARDWARECURSOR [IddCxMonitorSetupHardwareCursorTableIndex]
        (monitor: IDDCX_MONITOR, args: *const IDARG_IN_SETUP_HWCURSOR) -> NTSTATUS => MISSING;
    fn idd_monitor_query_hardware_cursor =
        PFN_IDDCXMONITORQUERYHARDWARECURSOR [IddCxMonitorQueryHardwareCursorTableIndex]
        (monitor: IDDCX_MONITOR, args: *const IDARG_IN_QUERY_HWCURSOR, out: *mut IDARG_OUT_QUERY_HWCURSOR) -> NTSTATUS => MISSING;
    fn idd_swapchain_set_device = PFN_IDDCXSWAPCHAINSETDEVICE [IddCxSwapChainSetDeviceTableIndex]
        (swapchain: IDDCX_SWAPCHAIN, args: *const IDARG_IN_SWAPCHAINSETDEVICE) -> HRESULT => MISSING_HR;
    fn idd_swapchain_release_and_acquire_buffer =
        PFN_IDDCXSWAPCHAINRELEASEANDACQUIREBUFFER [IddCxSwapChainReleaseAndAcquireBufferTableIndex]
        (swapchain: IDDCX_SWAPCHAIN, out: *mut IDARG_OUT_RELEASEANDACQUIREBUFFER) -> HRESULT => MISSING_HR;
    fn idd_swapchain_finished_processing_frame =
        PFN_IDDCXSWAPCHAINFINISHEDPROCESSINGFRAME [IddCxSwapChainFinishedProcessingFrameTableIndex]
        (swapchain: IDDCX_SWAPCHAIN) -> HRESULT => MISSING_HR;
    fn idd_get_version = PFN_IDDCXGETVERSION [IddCxGetVersionTableIndex]
        (out: *mut IDARG_OUT_GETVERSION) -> NTSTATUS => MISSING;
    fn idd_adapter_set_render_adapter =
        PFN_IDDCXADAPTERSETRENDERADAPTER [IddCxAdapterSetRenderAdapterTableIndex]
        (adapter: IDDCX_ADAPTER, args: *const IDARG_IN_ADAPTERSETRENDERADAPTER) => ();
}
