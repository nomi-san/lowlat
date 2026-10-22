//! `lowlatd install` and `lowlatd uninstall`: the service's registration and
//! the programs' place and access. **No driver is the service's to install**:
//! each of the host's drivers is installed and removed by its own script, run
//! by the machine's administrator (docs/07-platforms.md section 10).
//!
//! **A program the system account runs is writable by administrators alone**,
//! so the programs live under the program-files directory and never in the
//! build tree. The configuration carries the signaling secret, so its folder
//! is given an explicit owner and access list rather than the inherited one
//! (docs/impl-plan-windows.md W2.5).

use std::path::{Path, PathBuf};
use std::time::Duration;

use windows_sys::Win32::Foundation::{
    ERROR_SERVICE_ALREADY_RUNNING, ERROR_SERVICE_DOES_NOT_EXIST, ERROR_SERVICE_EXISTS,
    ERROR_SERVICE_NOT_ACTIVE, ERROR_SUCCESS, GetLastError, LocalFree,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1, SE_FILE_OBJECT,
    SetNamedSecurityInfoW,
};
use windows_sys::Win32::Security::{
    ACL, CreateWellKnownSid, DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl,
    OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSID, SECURITY_MAX_SID_SIZE,
    WinBuiltinAdministratorsSid,
};
use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_DELAY_UNTIL_REBOOT, MoveFileExW};
use windows_sys::Win32::System::Services::{
    ChangeServiceConfig2W, ChangeServiceConfigW, ControlService, CreateServiceW, DeleteService,
    OpenSCManagerW, OpenServiceW, QueryServiceStatus, SC_ACTION, SC_ACTION_RESTART,
    SC_MANAGER_CONNECT, SC_MANAGER_CREATE_SERVICE, SERVICE_ALL_ACCESS, SERVICE_AUTO_START,
    SERVICE_CONFIG_DESCRIPTION, SERVICE_CONFIG_FAILURE_ACTIONS,
    SERVICE_CONFIG_FAILURE_ACTIONS_FLAG, SERVICE_CONTROL_STOP, SERVICE_DESCRIPTIONW,
    SERVICE_ERROR_NORMAL, SERVICE_FAILURE_ACTIONS_FLAG, SERVICE_FAILURE_ACTIONSW,
    SERVICE_QUERY_STATUS, SERVICE_RUNNING, SERVICE_START, SERVICE_STATUS, SERVICE_STOP,
    SERVICE_STOPPED, SERVICE_WIN32_OWN_PROCESS, StartServiceW,
};

use crate::windows_service::{DISPLAY_NAME, SERVICE_NAME};

/// The configuration written when the file is absent and no template was
/// staged beside the programs.
const TEMPLATE: &str = "\
# The Windows service's configuration. The file carries the signaling secret,
# so this folder is readable by the system account and administrators alone.
KESSEL_WS_SERVER=
KESSEL_SESSION=

# The host's arguments, one string the C runtime's rules split.
LOWLATD_ARGS=
";

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

#[link(name = "shell32")]
unsafe extern "system" {
    fn IsUserAnAdmin() -> i32;
}

fn elevated() -> bool {
    // SAFETY: no arguments, no state.
    (unsafe { IsUserAnAdmin() }) != 0
}

fn directory(variable: &str) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let base = std::env::var_os(variable)
        .ok_or_else(|| format!("{variable} is not in the environment"))?;
    Ok(PathBuf::from(base))
}

fn program_files() -> Result<PathBuf, Box<dyn std::error::Error>> {
    Ok(directory("ProgramFiles")?.join("lowlat"))
}

fn program_data() -> Result<PathBuf, Box<dyn std::error::Error>> {
    Ok(directory("ProgramData")?.join("lowlat"))
}

fn same_path(left: &Path, right: &Path) -> bool {
    match (std::fs::canonicalize(left), std::fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => left
            .to_string_lossy()
            .eq_ignore_ascii_case(&right.to_string_lossy()),
    }
}

/// Copy a file, retrying briefly while a just-stopped service releases its
/// image. The service is reported stopped a moment before its process ends.
fn copy_file(from: &Path, to: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let mut last = None;
    for _ in 0..10 {
        match std::fs::copy(from, to) {
            Ok(_) => return Ok(()),
            Err(error) if matches!(error.raw_os_error(), Some(5 | 32)) => {
                last = Some(error);
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(error) => return Err(error.into()),
        }
    }
    match last {
        Some(error) => Err(format!("{}: {error}", to.display()).into()),
        None => Err(format!("{}: the copy did not happen", to.display()).into()),
    }
}

/// Give a folder or file the access list the configuration needs: the system
/// account and administrators, full, nothing inherited.
///
/// **The owner is set too**, because an owner keeps the right to change the
/// list: a file a user planted before the install would otherwise carry that
/// user's ownership into the protected folder.
fn protect(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let path_w = wide(&path.to_string_lossy());
    let sddl = wide("D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)");
    let mut descriptor: PSID = std::ptr::null_mut();
    // SAFETY: a terminated SDDL string and an output pointer that lives for
    // the call.
    let ok = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &raw mut descriptor,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(format!(
            "the access list for {} was not built, error={}",
            path.display(),
            unsafe { GetLastError() }
        )
        .into());
    }
    let mut present = 0;
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut defaulted = 0;
    // SAFETY: the descriptor was just made and its DACL is read out.
    let read = unsafe {
        GetSecurityDescriptorDacl(
            descriptor,
            &raw mut present,
            &raw mut dacl,
            &raw mut defaulted,
        )
    };
    if read == 0 || present == 0 || dacl.is_null() {
        // SAFETY: the descriptor is owned here.
        unsafe { LocalFree(descriptor) };
        return Err(format!("the access list for {} has no DACL", path.display()).into());
    }

    let mut administrators = [0u8; SECURITY_MAX_SID_SIZE as usize];
    let mut size = SECURITY_MAX_SID_SIZE;
    // SAFETY: a buffer of the documented maximum size and its length.
    let sid_ok = unsafe {
        CreateWellKnownSid(
            WinBuiltinAdministratorsSid,
            std::ptr::null_mut(),
            administrators.as_mut_ptr().cast(),
            &raw mut size,
        )
    } != 0;

    let mut info = DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION;
    let mut owner: PSID = std::ptr::null_mut();
    if sid_ok {
        info |= OWNER_SECURITY_INFORMATION;
        owner = administrators.as_mut_ptr().cast();
    }
    // SAFETY: a terminated path, a SID in a live buffer, and a DACL inside a
    // live descriptor.
    let mut rc = unsafe {
        SetNamedSecurityInfoW(
            path_w.as_ptr(),
            SE_FILE_OBJECT,
            info,
            owner,
            std::ptr::null_mut(),
            dacl,
            std::ptr::null_mut(),
        )
    };
    if rc != ERROR_SUCCESS && sid_ok {
        // The owner needs a privilege an elevation does not always carry;
        // the access list alone still keeps the secret, and a line says the
        // owner was left as it was.
        rc = unsafe {
            SetNamedSecurityInfoW(
                path_w.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                dacl,
                std::ptr::null_mut(),
            )
        };
        if rc == ERROR_SUCCESS {
            lowlat_common::log_warn!(
                "lowlatd: the owner of {} was left as it was; the access list is set",
                path.display()
            );
        }
    }
    // SAFETY: the descriptor is owned here and not used again.
    unsafe { LocalFree(descriptor) };
    if rc != ERROR_SUCCESS {
        return Err(format!(
            "the access list for {} was not set, error={rc}",
            path.display()
        )
        .into());
    }
    Ok(())
}

/// Register the service, or point an existing registration at the installed
/// program and restore its start type.
fn register_service(program: &Path) -> Result<(), Box<dyn std::error::Error>> {
    // SAFETY: no machine and no database name, a local handle only.
    let manager = unsafe {
        OpenSCManagerW(
            std::ptr::null(),
            std::ptr::null(),
            SC_MANAGER_CREATE_SERVICE | SC_MANAGER_CONNECT,
        )
    };
    if manager.is_null() {
        return Err(
            format!("the service manager was not opened, error={}", unsafe {
                GetLastError()
            })
            .into(),
        );
    }
    let name = wide(SERVICE_NAME);
    let display = wide(DISPLAY_NAME);
    let binary = wide(&format!("\"{}\" service", program.display()));
    // SAFETY: terminated strings; the local system account is the empty
    // start name, as the service runs as it.
    let mut service = unsafe {
        CreateServiceW(
            manager,
            name.as_ptr(),
            display.as_ptr(),
            SERVICE_ALL_ACCESS,
            SERVICE_WIN32_OWN_PROCESS,
            SERVICE_AUTO_START,
            SERVICE_ERROR_NORMAL,
            binary.as_ptr(),
            std::ptr::null(),
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    if service.is_null() {
        let error = unsafe { GetLastError() };
        if error != ERROR_SERVICE_EXISTS {
            // SAFETY: the manager is open here.
            unsafe { windows_sys::Win32::System::Services::CloseServiceHandle(manager) };
            return Err(format!("the service was not made, error={error}").into());
        }
        // SAFETY: a terminated name and the same access the creation asked for.
        service = unsafe { OpenServiceW(manager, name.as_ptr(), SERVICE_ALL_ACCESS) };
        if service.is_null() {
            let error = unsafe { GetLastError() };
            // SAFETY: the manager is open here.
            unsafe { windows_sys::Win32::System::Services::CloseServiceHandle(manager) };
            return Err(format!("the service was not opened, error={error}").into());
        }
        // SAFETY: an open service and terminated strings.
        unsafe {
            ChangeServiceConfigW(
                service,
                SERVICE_WIN32_OWN_PROCESS,
                SERVICE_AUTO_START,
                SERVICE_ERROR_NORMAL,
                binary.as_ptr(),
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                display.as_ptr(),
            )
        };
    }

    let description = wide(
        "The lowlat remote desktop host: one process in the console session, started and \
         watched by this service.",
    );
    let description = SERVICE_DESCRIPTIONW {
        lpDescription: description.as_ptr() as *mut u16,
    };
    // SAFETY: an open service and a structure that lives for the call.
    unsafe {
        ChangeServiceConfig2W(
            service,
            SERVICE_CONFIG_DESCRIPTION,
            (&raw const description).cast(),
        )
    };

    // Two seconds after a failed exit, as the loop itself restarts one; the
    // screen is served at boot, so the start type is automatic.
    let mut action = SC_ACTION {
        Type: SC_ACTION_RESTART,
        Delay: 2000,
    };
    let actions = SERVICE_FAILURE_ACTIONSW {
        dwResetPeriod: 86_400,
        lpRebootMsg: std::ptr::null_mut(),
        lpCommand: std::ptr::null_mut(),
        cActions: 1,
        lpsaActions: &raw mut action,
    };
    // SAFETY: an open service and a structure that lives for the call.
    unsafe {
        ChangeServiceConfig2W(
            service,
            SERVICE_CONFIG_FAILURE_ACTIONS,
            (&raw const actions).cast(),
        )
    };
    let flag = SERVICE_FAILURE_ACTIONS_FLAG {
        fFailureActionsOnNonCrashFailures: 1,
    };
    // SAFETY: as above.
    unsafe {
        ChangeServiceConfig2W(
            service,
            SERVICE_CONFIG_FAILURE_ACTIONS_FLAG,
            (&raw const flag).cast(),
        )
    };
    // SAFETY: both handles are open here and closed once.
    unsafe {
        windows_sys::Win32::System::Services::CloseServiceHandle(service);
        windows_sys::Win32::System::Services::CloseServiceHandle(manager);
    }
    Ok(())
}

/// Start the service and wait for it to report running, bounded at ten
/// seconds.
fn start_service() -> Result<(), Box<dyn std::error::Error>> {
    // SAFETY: no machine and no database name.
    let manager = unsafe { OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_CONNECT) };
    if manager.is_null() {
        return Err(
            format!("the service manager was not opened, error={}", unsafe {
                GetLastError()
            })
            .into(),
        );
    }
    let name = wide(SERVICE_NAME);
    // SAFETY: a terminated name; no arguments.
    let service = unsafe {
        OpenServiceW(
            manager,
            name.as_ptr(),
            SERVICE_START | SERVICE_QUERY_STATUS | SERVICE_STOP,
        )
    };
    if service.is_null() {
        let error = unsafe { GetLastError() };
        // SAFETY: the manager is open here.
        unsafe { windows_sys::Win32::System::Services::CloseServiceHandle(manager) };
        return Err(format!("the service was not opened, error={error}").into());
    }
    // SAFETY: an open service and no arguments.
    let started = unsafe { StartServiceW(service, 0, std::ptr::null()) };
    if started == 0 {
        let error = unsafe { GetLastError() };
        if error != ERROR_SERVICE_ALREADY_RUNNING {
            // SAFETY: the handles are open here.
            unsafe {
                windows_sys::Win32::System::Services::CloseServiceHandle(service);
                windows_sys::Win32::System::Services::CloseServiceHandle(manager);
            }
            return Err(format!("the service was not started, error={error}").into());
        }
    }
    for _ in 0..20 {
        let mut status = SERVICE_STATUS::default();
        // SAFETY: an open service and a live status structure.
        let queried = unsafe { QueryServiceStatus(service, &raw mut status) } != 0;
        if queried && status.dwCurrentState == SERVICE_RUNNING {
            // SAFETY: the handles are open here.
            unsafe {
                windows_sys::Win32::System::Services::CloseServiceHandle(service);
                windows_sys::Win32::System::Services::CloseServiceHandle(manager);
            }
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    // SAFETY: the handles are open here.
    unsafe {
        windows_sys::Win32::System::Services::CloseServiceHandle(service);
        windows_sys::Win32::System::Services::CloseServiceHandle(manager);
    }
    Err("the service did not reach running within ten seconds".into())
}

/// Is the virtual gamepad bus installed? A missing one refuses pads alone, so
/// it is said and not fatal.
fn vigem_installed() -> bool {
    // SAFETY: no machine and no database name.
    let manager = unsafe { OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_CONNECT) };
    if manager.is_null() {
        return false;
    }
    let name = wide("ViGEmBus");
    // SAFETY: a terminated name and query access only.
    let service = unsafe { OpenServiceW(manager, name.as_ptr(), SERVICE_QUERY_STATUS) };
    let installed = !service.is_null();
    // SAFETY: whichever handles are open here.
    unsafe {
        if !service.is_null() {
            windows_sys::Win32::System::Services::CloseServiceHandle(service);
        }
        windows_sys::Win32::System::Services::CloseServiceHandle(manager);
    }
    installed
}

/// Stop an installed service before its program is replaced, bounded at
/// thirty seconds. Absent is fine, and a running host goes with the job.
fn stop_existing_service() -> Result<(), Box<dyn std::error::Error>> {
    // SAFETY: no machine and no database name.
    let manager = unsafe { OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_CONNECT) };
    if manager.is_null() {
        return Err(
            format!("the service manager was not opened, error={}", unsafe {
                GetLastError()
            })
            .into(),
        );
    }
    let name = wide(SERVICE_NAME);
    // SAFETY: a terminated name and the access the stop needs.
    let service =
        unsafe { OpenServiceW(manager, name.as_ptr(), SERVICE_STOP | SERVICE_QUERY_STATUS) };
    if service.is_null() {
        let error = unsafe { GetLastError() };
        // SAFETY: the manager is open here.
        unsafe { windows_sys::Win32::System::Services::CloseServiceHandle(manager) };
        if error == ERROR_SERVICE_DOES_NOT_EXIST {
            return Ok(());
        }
        return Err(format!("the service was not opened, error={error}").into());
    }
    let mut status = SERVICE_STATUS::default();
    // SAFETY: an open service and a live status structure.
    let requested = unsafe { ControlService(service, SERVICE_CONTROL_STOP, &raw mut status) };
    if requested == 0 {
        let error = unsafe { GetLastError() };
        if error != ERROR_SERVICE_NOT_ACTIVE {
            lowlat_common::log_warn!("lowlatd: the service stop was not accepted, error={error}");
        }
    }
    let mut stopped = false;
    for _ in 0..60 {
        let mut status = SERVICE_STATUS::default();
        // SAFETY: as above.
        if unsafe { QueryServiceStatus(service, &raw mut status) } != 0
            && status.dwCurrentState == SERVICE_STOPPED
        {
            stopped = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    // SAFETY: both handles are open here.
    unsafe {
        windows_sys::Win32::System::Services::CloseServiceHandle(service);
        windows_sys::Win32::System::Services::CloseServiceHandle(manager);
    }
    if !stopped {
        return Err("the service did not stop within thirty seconds".into());
    }
    Ok(())
}

pub(crate) fn install() -> Result<(), Box<dyn std::error::Error>> {
    if !elevated() {
        return Err("install needs an elevated shell (Run as administrator)".into());
    }
    // A running service holds its own image open, so it is stopped first;
    // the host it started goes with the job.
    stop_existing_service()?;
    let exe = std::env::current_exe()?;
    let source = exe
        .parent()
        .ok_or("this program's folder could not be read")?;
    let target = program_files()?;
    std::fs::create_dir_all(&target)?;
    let installed = target.join("lowlatd.exe");
    if !same_path(&exe, &installed) {
        copy_file(&exe, &installed)?;
        lowlat_common::log_info!("lowlatd: copied {}", installed.display());
    }
    for name in ["lowlat-vdd.exe", "lowlat-login.py", "lowlatd.env.template"] {
        let from = source.join(name);
        let to = target.join(name);
        if from.exists() && !same_path(&from, &to) {
            copy_file(&from, &to)?;
            lowlat_common::log_info!("lowlatd: copied {}", to.display());
        }
    }
    // The configuration's folder is made before its file, and both get the
    // explicit list; the file is written only when absent, so an install over
    // one keeps what the login wrote.
    let data = program_data()?;
    std::fs::create_dir_all(&data)?;
    protect(&data)?;
    let environment = data.join("lowlatd.env");
    if !environment.exists() {
        let template = std::fs::read_to_string(target.join("lowlatd.env.template"))
            .unwrap_or_else(|_| TEMPLATE.to_string());
        std::fs::write(&environment, template)?;
        lowlat_common::log_info!("lowlatd: wrote {}", environment.display());
    }
    protect(&environment)?;

    register_service(&installed)?;
    if !vigem_installed() {
        lowlat_common::log_warn!(
            "lowlatd: ViGEmBus is not installed; gamepads will be refused until it is"
        );
    }
    start_service()?;
    lowlat_common::log_info!("lowlatd: installed, program={}", installed.display());
    Ok(())
}

/// Stop and delete the service, bounded at thirty seconds for the stop.
fn remove_service() -> Result<(), Box<dyn std::error::Error>> {
    // SAFETY: no machine and no database name.
    let manager = unsafe { OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_CONNECT) };
    if manager.is_null() {
        return Err(
            format!("the service manager was not opened, error={}", unsafe {
                GetLastError()
            })
            .into(),
        );
    }
    let name = wide(SERVICE_NAME);
    // SAFETY: a terminated name and the access the stop and delete need.
    let service = unsafe { OpenServiceW(manager, name.as_ptr(), SERVICE_ALL_ACCESS) };
    if service.is_null() {
        let error = unsafe { GetLastError() };
        // SAFETY: the manager is open here.
        unsafe { windows_sys::Win32::System::Services::CloseServiceHandle(manager) };
        if error == ERROR_SERVICE_DOES_NOT_EXIST {
            return Ok(());
        }
        return Err(format!("the service was not opened, error={error}").into());
    }
    let mut status = SERVICE_STATUS::default();
    // SAFETY: an open service and a live status structure.
    unsafe { ControlService(service, SERVICE_CONTROL_STOP, &raw mut status) };
    for _ in 0..60 {
        let mut status = SERVICE_STATUS::default();
        // SAFETY: an open service and a live status structure.
        if unsafe { QueryServiceStatus(service, &raw mut status) } != 0
            && status.dwCurrentState == SERVICE_STOPPED
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    let mut status = SERVICE_STATUS::default();
    let stopped = {
        // SAFETY: as above.
        let queried = unsafe { QueryServiceStatus(service, &raw mut status) } != 0;
        queried && status.dwCurrentState == SERVICE_STOPPED
    };
    if !stopped {
        lowlat_common::log_warn!("lowlatd: the service was still running at the delete");
    }
    // SAFETY: an open service, deleted once.
    unsafe { DeleteService(service) };
    // SAFETY: both handles are open here.
    unsafe {
        windows_sys::Win32::System::Services::CloseServiceHandle(service);
        windows_sys::Win32::System::Services::CloseServiceHandle(manager);
    }
    Ok(())
}

pub(crate) fn uninstall() -> Result<(), Box<dyn std::error::Error>> {
    if !elevated() {
        return Err("uninstall needs an elevated shell (Run as administrator)".into());
    }
    remove_service()?;
    let target = program_files()?;
    if target.exists() {
        match std::fs::remove_dir_all(&target) {
            Ok(()) => lowlat_common::log_info!("lowlatd: removed {}", target.display()),
            Err(error) => {
                // A copy still running keeps its file open; the system deletes
                // it at the next boot.
                let path = wide(&target.to_string_lossy());
                // SAFETY: a terminated path, no new name, the documented flag.
                let queued = unsafe {
                    MoveFileExW(path.as_ptr(), std::ptr::null(), MOVEFILE_DELAY_UNTIL_REBOOT)
                } != 0;
                lowlat_common::log_warn!(
                    "lowlatd: {} was not removed, error={error} queued={}",
                    target.display(),
                    u8::from(queued)
                );
            }
        }
    }
    // The configuration is kept: it carries the login, as Linux keeps its
    // environment file across a removal.
    lowlat_common::log_info!("lowlatd: uninstalled, configuration kept");
    Ok(())
}
