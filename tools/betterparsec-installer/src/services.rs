//! Native Service Control Manager helpers. Native (not `sc.exe`) so the broker's
//! `SERVICE_WIN32_OWN_PROCESS` command line — a quoted path under
//! `C:\Program Files\...` plus `--service` — is passed verbatim to
//! `CreateServiceW`, avoiding the notorious `sc.exe binPath=` quoting hazard.

use windows::Win32::System::Services::{
    CloseServiceHandle, ControlService, CreateServiceW, DeleteService, ENUM_SERVICE_TYPE,
    OpenSCManagerW, OpenServiceW, SC_HANDLE, SC_MANAGER_ALL_ACCESS, SERVICE_ALL_ACCESS,
    SERVICE_AUTO_START, SERVICE_CONTROL_STOP, SERVICE_DEMAND_START, SERVICE_ERROR_NORMAL,
    SERVICE_KERNEL_DRIVER, SERVICE_START_TYPE, SERVICE_STATUS, SERVICE_WIN32_OWN_PROCESS,
};
use windows::core::PCWSTR;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

struct ScmHandle(SC_HANDLE);
impl Drop for ScmHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseServiceHandle(self.0);
        }
    }
}

fn open_scm() -> Result<ScmHandle, String> {
    unsafe {
        OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_ALL_ACCESS)
            .map(ScmHandle)
            .map_err(|e| format!("OpenSCManager failed: {e}"))
    }
}

/// Create a kernel-driver service (`type= kernel start= demand`). The driver's
/// `.sys` path is the binary path.
pub fn create_kernel_driver(name: &str, display: &str, sys_path: &str) -> Result<(), String> {
    create_service(name, display, sys_path, SERVICE_KERNEL_DRIVER, SERVICE_DEMAND_START)
}

/// Create a LocalSystem own-process service (`type= own obj= LocalSystem`),
/// auto-start. `command` is the full command line (quoted exe + args).
pub fn create_localsystem_service(name: &str, display: &str, command: &str) -> Result<(), String> {
    create_service(name, display, command, SERVICE_WIN32_OWN_PROCESS, SERVICE_AUTO_START)
}

fn create_service(
    name: &str,
    display: &str,
    bin_path: &str,
    service_type: ENUM_SERVICE_TYPE,
    start_type: SERVICE_START_TYPE,
) -> Result<(), String> {
    let scm = open_scm()?;
    let wname = wide(name);
    let wdisplay = wide(display);
    let wbin = wide(bin_path);
    unsafe {
        let handle = CreateServiceW(
            scm.0,
            PCWSTR(wname.as_ptr()),
            PCWSTR(wdisplay.as_ptr()),
            SERVICE_ALL_ACCESS,
            service_type,
            start_type,
            SERVICE_ERROR_NORMAL,
            PCWSTR(wbin.as_ptr()),
            PCWSTR::null(),
            None,
            PCWSTR::null(),
            PCWSTR::null(), // null start name → LocalSystem
            PCWSTR::null(),
        )
        .map_err(|e| format!("CreateService({name}) failed: {e}"))?;
        let _ = CloseServiceHandle(handle);
    }
    Ok(())
}

/// Whether a service of this name exists.
pub fn service_exists(name: &str) -> bool {
    let scm = match open_scm() {
        Ok(s) => s,
        Err(_) => return false,
    };
    let wname = wide(name);
    unsafe {
        match OpenServiceW(scm.0, PCWSTR(wname.as_ptr()), SERVICE_ALL_ACCESS) {
            Ok(h) => {
                let _ = CloseServiceHandle(h);
                true
            }
            Err(_) => false,
        }
    }
}

/// Stop (if running) and delete a service. A missing service is not an error.
pub fn delete_service(name: &str) -> Result<(), String> {
    let scm = open_scm()?;
    let wname = wide(name);
    unsafe {
        let handle = match OpenServiceW(scm.0, PCWSTR(wname.as_ptr()), SERVICE_ALL_ACCESS) {
            Ok(h) => h,
            Err(_) => return Ok(()), // absent → nothing to delete
        };
        // Best-effort stop; a demand-start driver that is not running errors here.
        let mut status = SERVICE_STATUS::default();
        let _ = ControlService(handle, SERVICE_CONTROL_STOP, &mut status);
        let result = DeleteService(handle).map_err(|e| format!("DeleteService({name}) failed: {e}"));
        let _ = CloseServiceHandle(handle);
        result
    }
}
