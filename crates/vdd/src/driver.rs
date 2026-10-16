//! The framework's and the class's callbacks: the driver and its device, the
//! adapter's start, the modes, and the control interface's requests
//! (docs/11-drivers.md section 1).

use lowlat_common::{log_info, log_warn};
use lowlat_vdd_proto::CONTROL_INTERFACE;
use lowlat_vdd_proto::control::{
    Frames, IOCTL_FRAMES, IOCTL_LIST, IOCTL_PLUG, IOCTL_RELOAD, IOCTL_REPLUG, IOCTL_UNPLUG,
    IOCTL_VERSION, Plug, Refused, Replug, Unplug, VERSION, Version,
};
use lowlat_vdd_proto::modes::{Mode, ModeList};

use crate::ffi::{
    DISPLAYCONFIG_2DREGION, DISPLAYCONFIG_RATIONAL, DISPLAYCONFIG_SCANLINE_ORDERING_PROGRESSIVE,
    DISPLAYCONFIG_TARGET_MODE, DISPLAYCONFIG_VIDEO_SIGNAL_INFO,
    DISPLAYCONFIG_VIDEO_SIGNAL_INFO__bindgen_ty_1, IDARG_IN_ADAPTER_INIT_FINISHED,
    IDARG_IN_COMMITMODES, IDARG_IN_GETDEFAULTDESCRIPTIONMODES, IDARG_IN_PARSEMONITORDESCRIPTION,
    IDARG_IN_QUERYTARGETMODES, IDARG_OUT_GETDEFAULTDESCRIPTIONMODES, IDARG_OUT_GETVERSION,
    IDARG_OUT_PARSEMONITORDESCRIPTION, IDARG_OUT_QUERYTARGETMODES, IDD_CX_CLIENT_CONFIG,
    IDDCX_ADAPTER, IDDCX_MONITOR, IDDCX_MONITOR_MODE, IDDCX_MONITOR_MODE_ORIGIN,
    IDDCX_MONITOR_MODE_ORIGIN_DRIVER, IDDCX_MONITOR_MODE_ORIGIN_MONITORDESCRIPTOR,
    IDDCX_PATH_FLAGS_ACTIVE, IDDCX_TARGET_MODE, NTSTATUS, PCUNICODE_STRING, PDRIVER_OBJECT, PVOID,
    PWDFDEVICE_INIT, STATUS_BUFFER_TOO_SMALL, STATUS_INVALID_DEVICE_REQUEST,
    STATUS_INVALID_PARAMETER, STATUS_SUCCESS, ULONG, ULONG_PTR, WDF_DRIVER_CONFIG,
    WDF_FILEOBJECT_CONFIG, WDF_PNPPOWER_EVENT_CALLBACKS, WDF_POWER_DEVICE_STATE, WDFDEVICE,
    WDFDRIVER, WDFFILEOBJECT, WDFREQUEST, WdfFileObjectWdfCannotUseFsContexts, WdfUseDefault,
    idd_device_init_config, idd_device_initialize, idd_get_version, nt_success, object_attributes,
    size_of, wdf_device_create, wdf_device_create_device_interface,
    wdf_device_init_set_file_object_config, wdf_device_init_set_pnp_power_event_callbacks,
    wdf_driver_create, wdf_request_complete_with_information, wdf_request_get_file_object,
    wdf_request_retrieve_input_buffer, wdf_request_retrieve_output_buffer,
};
use crate::monitor::{self, Handle, status_word};
use crate::swapchain;

/// The video standard a mode's signal names: none of the broadcast ones.
const VIDEO_STANDARD_OTHER: u32 = 255;

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
            "vdd: driver create refused, status={:#x}",
            status_word(status)
        );
    }
    status
}

unsafe extern "C" fn device_add(_driver: WDFDRIVER, init: PWDFDEVICE_INIT) -> NTSTATUS {
    let mut init = init;
    let mut power = WDF_PNPPOWER_EVENT_CALLBACKS {
        Size: size_of::<WDF_PNPPOWER_EVENT_CALLBACKS>(),
        EvtDeviceD0Entry: Some(d0_entry),
        ..Default::default()
    };
    // A file object per handle, so a handle's end -- a close, or its
    // process's -- reaches the driver and unplugs what it plugged.
    let mut files = WDF_FILEOBJECT_CONFIG {
        Size: size_of::<WDF_FILEOBJECT_CONFIG>(),
        EvtFileCleanup: Some(file_cleanup),
        AutoForwardCleanupClose: WdfUseDefault,
        FileObjectClass: WdfFileObjectWdfCannotUseFsContexts,
        ..Default::default()
    };
    let config = IDD_CX_CLIENT_CONFIG {
        Size: size_of::<IDD_CX_CLIENT_CONFIG>(),
        EvtIddCxDeviceIoControl: Some(io_control),
        EvtIddCxParseMonitorDescription: Some(parse_monitor_description),
        EvtIddCxAdapterInitFinished: Some(adapter_init_finished),
        EvtIddCxAdapterCommitModes: Some(commit_modes),
        EvtIddCxMonitorGetDefaultDescriptionModes: Some(default_description_modes),
        EvtIddCxMonitorQueryTargetModes: Some(query_target_modes),
        EvtIddCxMonitorAssignSwapChain: Some(swapchain::assign),
        EvtIddCxMonitorUnassignSwapChain: Some(swapchain::unassign),
        ..Default::default()
    };
    // SAFETY: the framework's device init, before the device is created;
    // live configuration for each call.
    let status = unsafe {
        wdf_device_init_set_pnp_power_event_callbacks(init, &raw mut power);
        wdf_device_init_set_file_object_config(init, &raw mut files, core::ptr::null_mut());
        idd_device_init_config(init, &raw const config)
    };
    if !nt_success(status) {
        log_warn!(
            "vdd: class config refused, status={:#x}",
            status_word(status)
        );
        return status;
    }
    let mut attributes = object_attributes();
    let mut device: WDFDEVICE = core::ptr::null_mut();
    // SAFETY: as above; the framework takes the init.
    let status = unsafe { wdf_device_create(&raw mut init, &raw mut attributes, &raw mut device) };
    if !nt_success(status) {
        log_warn!(
            "vdd: device create refused, status={:#x}",
            status_word(status)
        );
        return status;
    }
    let interface = monitor::guid(CONTROL_INTERFACE);
    // SAFETY: the device just created; no reference string.
    let status = unsafe {
        wdf_device_create_device_interface(device, &raw const interface, core::ptr::null())
    };
    if !nt_success(status) {
        log_warn!(
            "vdd: control interface refused, status={:#x}",
            status_word(status)
        );
        return status;
    }
    // SAFETY: as above.
    let status = unsafe { idd_device_initialize(device) };
    if !nt_success(status) {
        log_warn!(
            "vdd: class initialize refused, status={:#x}",
            status_word(status)
        );
        return status;
    }
    monitor::reset();
    let mut version = IDARG_OUT_GETVERSION::default();
    // SAFETY: a live output.
    let known = unsafe { idd_get_version(&raw mut version) };
    log_info!(
        "vdd: device added, class={:#x}",
        if nt_success(known) {
            version.IddCxVersion
        } else {
            0
        }
    );
    STATUS_SUCCESS
}

unsafe extern "C" fn d0_entry(device: WDFDEVICE, _previous: WDF_POWER_DEVICE_STATE) -> NTSTATUS {
    monitor::start_adapter(Handle(device))
}

unsafe extern "C" fn adapter_init_finished(
    _adapter: IDDCX_ADAPTER,
    args: *const IDARG_IN_ADAPTER_INIT_FINISHED,
) -> NTSTATUS {
    // SAFETY: the class's arguments, live for the call.
    let status =
        unsafe { args.as_ref() }.map_or(STATUS_INVALID_PARAMETER, |args| args.AdapterInitStatus);
    monitor::adapter_ready(status);
    STATUS_SUCCESS
}

unsafe extern "C" fn file_cleanup(file: WDFFILEOBJECT) {
    monitor::release_owner(Handle(file));
}

/// A mode's signal. A monitor's modes take no divider and a target's take
/// one, so the desktop updates at the mode's own rate.
fn signal(mode: Mode, divider: u32) -> DISPLAYCONFIG_VIDEO_SIGNAL_INFO {
    let size = DISPLAYCONFIG_2DREGION {
        cx: mode.width,
        cy: mode.height,
    };
    DISPLAYCONFIG_VIDEO_SIGNAL_INFO {
        pixelRate: u64::from(mode.refresh_hz) * u64::from(mode.width) * u64::from(mode.height),
        hSyncFreq: DISPLAYCONFIG_RATIONAL {
            Numerator: mode.refresh_hz.saturating_mul(mode.height),
            Denominator: 1,
        },
        vSyncFreq: DISPLAYCONFIG_RATIONAL {
            Numerator: mode.refresh_hz,
            Denominator: 1,
        },
        activeSize: size,
        totalSize: size,
        __bindgen_anon_1: DISPLAYCONFIG_VIDEO_SIGNAL_INFO__bindgen_ty_1 {
            videoStandard: VIDEO_STANDARD_OTHER | (divider << 16),
        },
        scanLineOrdering: DISPLAYCONFIG_SCANLINE_ORDERING_PROGRESSIVE,
    }
}

fn monitor_mode(mode: Mode, origin: IDDCX_MONITOR_MODE_ORIGIN) -> IDDCX_MONITOR_MODE {
    IDDCX_MONITOR_MODE {
        Size: size_of::<IDDCX_MONITOR_MODE>(),
        Origin: origin,
        MonitorVideoSignalInfo: signal(mode, 0),
    }
}

fn target_mode(mode: Mode) -> IDDCX_TARGET_MODE {
    IDDCX_TARGET_MODE {
        Size: size_of::<IDDCX_TARGET_MODE>(),
        TargetVideoSignalInfo: DISPLAYCONFIG_TARGET_MODE {
            targetVideoSignalInfo: signal(mode, 1),
        },
        // The adapter declares no display pipeline budget, and the system
        // keeps the active modes' sum within it: a mode that asked for any
        // bandwidth would never be made active.
        RequiredBandwidth: 0,
    }
}

/// Answer a mode query: the count alone when no buffer came, the modes when
/// it is large enough, too small otherwise.
///
/// # Safety
///
/// `buffer` holds `capacity` modes, or is null with a capacity of zero.
unsafe fn fill_monitor_modes(
    modes: &ModeList,
    origin: IDDCX_MONITOR_MODE_ORIGIN,
    capacity: u32,
    buffer: *mut IDDCX_MONITOR_MODE,
    count: &mut u32,
    preferred: &mut u32,
) -> NTSTATUS {
    let modes = modes.as_slice();
    *count = u32::try_from(modes.len()).unwrap_or(0);
    if usize::try_from(capacity).unwrap_or(0) < modes.len() || buffer.is_null() {
        return if capacity > 0 {
            STATUS_BUFFER_TOO_SMALL
        } else {
            STATUS_SUCCESS
        };
    }
    for (i, mode) in modes.iter().enumerate() {
        // SAFETY: within the caller's buffer, checked above.
        unsafe { buffer.add(i).write(monitor_mode(*mode, origin)) };
    }
    *preferred = 0;
    STATUS_SUCCESS
}

unsafe extern "C" fn parse_monitor_description(
    args: *const IDARG_IN_PARSEMONITORDESCRIPTION,
    out: *mut IDARG_OUT_PARSEMONITORDESCRIPTION,
) -> NTSTATUS {
    // SAFETY: the class's arguments, live for the call.
    let (Some(args), Some(out)) = (unsafe { args.as_ref() }, unsafe { out.as_mut() }) else {
        return STATUS_INVALID_PARAMETER;
    };
    let description = args.MonitorDescription;
    let length = usize::try_from(description.DataSize).unwrap_or(0);
    let bytes = if description.pData.is_null() {
        &[][..]
    } else {
        // SAFETY: the description the class handed over, `DataSize` bytes.
        unsafe { core::slice::from_raw_parts(description.pData.cast::<u8>(), length) }
    };
    let Some(modes) = monitor::modes_for_description(bytes) else {
        log_warn!("vdd: a description of no slot, length={length}");
        return STATUS_INVALID_PARAMETER;
    };
    // SAFETY: the class's buffer and its count.
    unsafe {
        fill_monitor_modes(
            &modes,
            IDDCX_MONITOR_MODE_ORIGIN_MONITORDESCRIPTOR,
            args.MonitorModeBufferInputCount,
            args.pMonitorModes,
            &mut out.MonitorModeBufferOutputCount,
            &mut out.PreferredMonitorModeIdx,
        )
    }
}

unsafe extern "C" fn default_description_modes(
    monitor: IDDCX_MONITOR,
    args: *const IDARG_IN_GETDEFAULTDESCRIPTIONMODES,
    out: *mut IDARG_OUT_GETDEFAULTDESCRIPTIONMODES,
) -> NTSTATUS {
    // SAFETY: the class's arguments, live for the call.
    let (Some(args), Some(out)) = (unsafe { args.as_ref() }, unsafe { out.as_mut() }) else {
        return STATUS_INVALID_PARAMETER;
    };
    let Some(modes) = monitor::modes_for_monitor(Handle(monitor)) else {
        return STATUS_INVALID_PARAMETER;
    };
    // SAFETY: the class's buffer and its count.
    unsafe {
        fill_monitor_modes(
            &modes,
            IDDCX_MONITOR_MODE_ORIGIN_DRIVER,
            args.DefaultMonitorModeBufferInputCount,
            args.pDefaultMonitorModes,
            &mut out.DefaultMonitorModeBufferOutputCount,
            &mut out.PreferredMonitorModeIdx,
        )
    }
}

unsafe extern "C" fn query_target_modes(
    monitor: IDDCX_MONITOR,
    args: *const IDARG_IN_QUERYTARGETMODES,
    out: *mut IDARG_OUT_QUERYTARGETMODES,
) -> NTSTATUS {
    // SAFETY: the class's arguments, live for the call.
    let (Some(args), Some(out)) = (unsafe { args.as_ref() }, unsafe { out.as_mut() }) else {
        return STATUS_INVALID_PARAMETER;
    };
    let Some(modes) = monitor::modes_for_monitor(Handle(monitor)) else {
        return STATUS_INVALID_PARAMETER;
    };
    let modes = modes.as_slice();
    out.TargetModeBufferOutputCount = u32::try_from(modes.len()).unwrap_or(0);
    let capacity = usize::try_from(args.TargetModeBufferInputCount).unwrap_or(0);
    if capacity >= modes.len() && !args.pTargetModes.is_null() {
        for (i, mode) in modes.iter().enumerate() {
            // SAFETY: within the class's buffer, checked above.
            unsafe { args.pTargetModes.add(i).write(target_mode(*mode)) };
        }
    }
    STATUS_SUCCESS
}

unsafe extern "C" fn commit_modes(
    _adapter: IDDCX_ADAPTER,
    args: *const IDARG_IN_COMMITMODES,
) -> NTSTATUS {
    // SAFETY: the class's arguments, live for the call.
    let Some(args) = (unsafe { args.as_ref() }) else {
        return STATUS_INVALID_PARAMETER;
    };
    let count = usize::try_from(args.PathCount).unwrap_or(0);
    if count == 0 || args.pPaths.is_null() {
        return STATUS_SUCCESS;
    }
    // SAFETY: the class's paths, `PathCount` of them.
    let paths = unsafe { core::slice::from_raw_parts(args.pPaths, count) };
    for path in paths
        .iter()
        .filter(|path| path.Flags & IDDCX_PATH_FLAGS_ACTIVE != 0)
    {
        let signal = path.TargetVideoSignalInfo;
        let rate = signal.vSyncFreq;
        let refresh_hz = rate
            .Numerator
            .saturating_add(rate.Denominator / 2)
            .checked_div(rate.Denominator)
            .unwrap_or(0);
        monitor::committed(
            Handle(path.MonitorObject),
            Mode::new(signal.activeSize.cx, signal.activeSize.cy, refresh_hz),
        );
    }
    STATUS_SUCCESS
}

/// What a refused request answers.
fn refused(why: Refused) -> NTSTATUS {
    match why {
        Refused::Short | Refused::Room => STATUS_BUFFER_TOO_SMALL,
        Refused::Length | Refused::Range => STATUS_INVALID_PARAMETER,
    }
}

/// The request's input, decoded before anything is written: a buffered
/// request's input and output are one buffer.
fn read<T>(request: WDFREQUEST, decode: fn(&[u8]) -> Result<T, Refused>) -> Result<T, NTSTATUS> {
    let mut buffer: PVOID = core::ptr::null_mut();
    let mut length = 0usize;
    // SAFETY: the request in hand; live outputs.
    let status =
        unsafe { wdf_request_retrieve_input_buffer(request, 1, &raw mut buffer, &raw mut length) };
    if !nt_success(status) || buffer.is_null() {
        return Err(STATUS_BUFFER_TOO_SMALL);
    }
    // SAFETY: the framework's buffer for this request, `length` bytes, read
    // and dropped before any reply is written into it.
    let bytes = unsafe { core::slice::from_raw_parts(buffer.cast::<u8>(), length) };
    decode(bytes).map_err(refused)
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

fn control(
    request: WDFREQUEST,
    owner: Handle<crate::ffi::WDFFILEOBJECT__>,
    code: ULONG,
) -> (NTSTATUS, usize) {
    match code {
        IOCTL_VERSION => {
            let version = Version {
                version: VERSION,
                max_monitors: u32::try_from(lowlat_vdd_proto::MAX_MONITORS).unwrap_or(0),
            };
            reply(request, |out| version.encode(out))
        }
        IOCTL_PLUG => {
            if owner.is_none() {
                return (STATUS_INVALID_DEVICE_REQUEST, 0);
            }
            let plug = match read(request, Plug::decode) {
                Ok(plug) => plug,
                Err(status) => return (status, 0),
            };
            match monitor::plug(owner, plug) {
                Ok(plugged) => reply(request, |out| plugged.encode(out)),
                Err(status) => (status, 0),
            }
        }
        IOCTL_UNPLUG => {
            let unplug = match read(request, Unplug::decode) {
                Ok(unplug) => unplug,
                Err(status) => return (status, 0),
            };
            match monitor::unplug(owner, usize::from(unplug.slot)) {
                Ok(()) => (STATUS_SUCCESS, 0),
                Err(status) => (status, 0),
            }
        }
        IOCTL_REPLUG => {
            let replug = match read(request, Replug::decode) {
                Ok(replug) => replug,
                Err(status) => return (status, 0),
            };
            match monitor::replug(owner, usize::from(replug.slot), replug.mode) {
                Ok(plugged) => reply(request, |out| plugged.encode(out)),
                Err(status) => (status, 0),
            }
        }
        IOCTL_RELOAD => (monitor::reload(), 0),
        // Asked by any caller the device admits: the answer is handle values
        // in this process, which only a process allowed to take handles from
        // it can use (docs/11-drivers.md section 1.2).
        IOCTL_FRAMES => {
            let frames = match read(request, Frames::decode) {
                Ok(frames) => frames,
                Err(status) => return (status, 0),
            };
            match crate::frames::request(usize::from(frames.slot)) {
                Ok(channel) => reply(request, |out| channel.encode(out)),
                Err(status) => (status, 0),
            }
        }
        IOCTL_LIST => {
            let list = monitor::list(owner);
            reply(request, |out| list.encode(out))
        }
        _ => (STATUS_INVALID_DEVICE_REQUEST, 0),
    }
}

unsafe extern "C" fn io_control(
    _device: WDFDEVICE,
    request: WDFREQUEST,
    _output_length: usize,
    _input_length: usize,
    code: ULONG,
) {
    // SAFETY: the request in hand.
    let owner = Handle(unsafe { wdf_request_get_file_object(request) });
    let (status, written) = control(request, owner, code);
    // SAFETY: the request in hand, completed once.
    unsafe {
        wdf_request_complete_with_information(
            request,
            status,
            ULONG_PTR::try_from(written).unwrap_or(0),
        );
    }
}
