//! The Windows service: the USB/IP server and the cables without the window,
//! from Windows start-up and with no administrator prompt. It runs as
//! LocalSystem from a copy of the program in Program Files, keeps its settings
//! and log in a folder shared by every user, and is managed by the window
//! through its pipe.

use std::ffi::c_void;
use std::fs;
use std::io;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicPtr, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_CALL_NOT_IMPLEMENTED, ERROR_CANCELLED, ERROR_FAILED_SERVICE_CONTROLLER_CONNECT,
    ERROR_SERVICE_ALREADY_RUNNING, ERROR_SERVICE_DOES_NOT_EXIST, ERROR_SERVICE_NOT_ACTIVE, HWND, NO_ERROR,
};
use windows_sys::Win32::Security::{DACL_SECURITY_INFORMATION, SECURITY_ATTRIBUTES};
use windows_sys::Win32::Storage::FileSystem::{CreateDirectoryW, DELETE};
use windows_sys::Win32::System::Services::*;
use windows_sys::Win32::System::Threading::{GetExitCodeProcess, INFINITE, WaitForSingleObject};
use windows_sys::Win32::UI::Shell::{SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW};
use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;

use super::control;
use super::remote::PipeServer;
use super::win::{SecurityDescriptor, is_elevated, last_error, wide};
use crate::AppServer;
use crate::config::{CONFIG_FILE, service_data_dir};
use crate::logf;
use crate::logging::Logger;

const SERVICE_NAME: &str = "VirtualCables";
const DISPLAY_NAME: &str = "Virtual Cables";
const DESCRIPTION: &str = "Virtual audio cables exported as USB Audio Class devices over USB/IP.";
/// The service's permissions: Windows' defaults, plus starting and stopping
/// for users signed in at the computer, so the window can start the service it
/// manages without an administrator prompt.
const SERVICE_SECURITY: &str = "D:(A;;CCLCSWRPWPDTLOCRRC;;;SY)(A;;CCDCLCSWRPWPDTLOCRSDRCWDWO;;;BA)\
                                (A;;CCLCSWRPWPLOCRRC;;;IU)(A;;CCLCSWLOCRRC;;;SU)";
/// The service's settings folder: SYSTEM and administrators may change it, and
/// users may only read it, since the service acts on what it holds.
const FOLDER_SECURITY: &str = "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FRFX;;;BU)";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// The service as the service control manager sees it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ServiceState {
    NotInstalled,
    Stopped,
    /// Starting or stopping.
    Pending,
    Running,
}

struct ScHandle(SC_HANDLE);

impl Drop for ScHandle {
    fn drop(&mut self) {
        // SAFETY: the handle came from OpenSCManagerW, OpenServiceW or
        // CreateServiceW and is closed once.
        unsafe { CloseServiceHandle(self.0) };
    }
}

fn open_manager(access: u32) -> Result<ScHandle, String> {
    // SAFETY: opens the local service control manager.
    let manager = unsafe { OpenSCManagerW(null(), null(), access) };
    if manager.is_null() {
        return Err(format!("open the service control manager: {}", last_error()));
    }
    Ok(ScHandle(manager))
}

fn open_service(manager: &ScHandle, access: u32) -> io::Result<ScHandle> {
    let name = wide(SERVICE_NAME);
    // SAFETY: a NUL-terminated name and a valid manager handle.
    let service = unsafe { OpenServiceW(manager.0, name.as_ptr(), access) };
    if service.is_null() {
        return Err(io::Error::last_os_error());
    }
    Ok(ScHandle(service))
}

fn not_installed(err: &io::Error) -> bool {
    err.raw_os_error() == Some(ERROR_SERVICE_DOES_NOT_EXIST as i32)
}

fn query(service: &ScHandle) -> Result<SERVICE_STATUS, String> {
    // SAFETY: SERVICE_STATUS is plain data that QueryServiceStatus fills in.
    unsafe {
        let mut status: SERVICE_STATUS = std::mem::zeroed();
        if QueryServiceStatus(service.0, &mut status) == 0 {
            return Err(format!("query the service: {}", last_error()));
        }
        Ok(status)
    }
}

/// The service's state; reading it needs no administrator rights.
pub fn service_state() -> ServiceState {
    let Ok(manager) = open_manager(SC_MANAGER_CONNECT) else {
        return ServiceState::NotInstalled;
    };
    let Ok(service) = open_service(&manager, SERVICE_QUERY_STATUS) else {
        return ServiceState::NotInstalled;
    };
    match query(&service).map(|status| status.dwCurrentState) {
        Ok(SERVICE_RUNNING) => ServiceState::Running,
        Ok(SERVICE_STOPPED) | Err(_) => ServiceState::Stopped,
        Ok(_) => ServiceState::Pending,
    }
}

/// Waits for the service to reach `state`. Waiting for it to run fails as soon
/// as it stops instead.
fn wait_for(service: &ScHandle, state: u32, timeout: Duration) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    loop {
        let status = query(service)?;
        if status.dwCurrentState == state {
            return Ok(());
        }
        if state == SERVICE_RUNNING && status.dwCurrentState == SERVICE_STOPPED {
            return Err(format!(
                "the service stopped (exit code {}); its log is in {}",
                status.dwWin32ExitCode,
                service_data_dir().display()
            ));
        }
        if Instant::now() >= deadline {
            return Err("the service did not respond in time".into());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Starts the installed service and waits until it runs. The service's
/// permissions let the window do this without an administrator prompt.
pub fn start_service() -> Result<(), String> {
    let manager = open_manager(SC_MANAGER_CONNECT)?;
    let service = open_service(&manager, SERVICE_START | SERVICE_QUERY_STATUS)
        .map_err(|err| format!("open the service: {err}"))?;
    // SAFETY: starts the service with no arguments.
    if unsafe { StartServiceW(service.0, 0, null()) } == 0 {
        let err = io::Error::last_os_error();
        if err.raw_os_error() != Some(ERROR_SERVICE_ALREADY_RUNNING as i32) {
            return Err(format!("start the service: {err}"));
        }
    }
    wait_for(&service, SERVICE_RUNNING, Duration::from_secs(30))
}

/// Runs this program with administrator rights and `parameters`, and waits for
/// it to finish: through the administrator prompt, or directly when this
/// process already has them. `owner` owns the prompt.
pub fn run_elevated(parameters: &str, owner: HWND) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|err| format!("locate the program: {err}"))?;
    let code = if is_elevated() {
        Command::new(&exe)
            .raw_arg(parameters)
            .creation_flags(CREATE_NO_WINDOW)
            .status()
            .map_err(|err| format!("start {}: {err}", exe.display()))?
            .code()
            .unwrap_or(1) as u32
    } else {
        let (verb, file, parameters) = (wide("runas"), wide(&exe.display().to_string()), wide(parameters));
        // SAFETY: SHELLEXECUTEINFOW is plain data; its strings outlive the
        // call, and the process handle it returns is closed here.
        unsafe {
            let mut info: SHELLEXECUTEINFOW = std::mem::zeroed();
            info.cbSize = size_of::<SHELLEXECUTEINFOW>() as u32;
            info.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC;
            info.hwnd = owner;
            info.lpVerb = verb.as_ptr();
            info.lpFile = file.as_ptr();
            info.lpParameters = parameters.as_ptr();
            info.nShow = SW_HIDE;
            if ShellExecuteExW(&mut info) == 0 {
                let err = io::Error::last_os_error();
                if err.raw_os_error() == Some(ERROR_CANCELLED as i32) {
                    return Err("administrator approval was cancelled".into());
                }
                return Err(format!("start the administrator copy: {err}"));
            }
            if info.hProcess.is_null() {
                return Err("the administrator copy did not start".into());
            }
            WaitForSingleObject(info.hProcess, INFINITE);
            let mut code = 1;
            GetExitCodeProcess(info.hProcess, &mut code);
            CloseHandle(info.hProcess);
            code
        }
    };
    if code != 0 {
        return Err(format!(
            "it failed with exit code {code}; the diagnostic log in the settings folder has the details"
        ));
    }
    Ok(())
}

/// Where the service's program is installed: a folder only administrators can
/// change, since the service runs it as LocalSystem.
fn program_path() -> PathBuf {
    let base = std::env::var_os("ProgramFiles").filter(|v| !v.is_empty()).unwrap_or_else(|| r"C:\Program Files".into());
    PathBuf::from(base).join("Virtual Cables").join("VirtualCables.exe")
}

fn same_file(a: &Path, b: &Path) -> bool {
    matches!((fs::canonicalize(a), fs::canonicalize(b)), (Ok(a), Ok(b)) if a == b)
}

/// Runs elevated: copies this program to Program Files and `config` to the
/// service's settings folder, then installs the service, or updates it if it
/// exists. The window starts it.
pub fn install_service(config: &Path, logger: &Logger) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|err| format!("locate the program: {err}"))?;
    let program = program_path();
    if !same_file(&exe, &program) {
        let folder = program.parent().unwrap_or(Path::new("."));
        fs::create_dir_all(folder).map_err(|err| format!("create {}: {err}", folder.display()))?;
        fs::copy(&exe, &program).map_err(|err| format!("copy the program to {}: {err}", program.display()))?;
        logf!(logger, "Copied the program to {}", program.display());
    }

    let folder = service_data_dir();
    create_service_folder(&folder, FOLDER_SECURITY)?;
    // Read and written rather than copied, so the copy takes the folder's
    // permissions.
    match fs::read(config) {
        Ok(content) => {
            let target = folder.join(CONFIG_FILE);
            fs::write(&target, content).map_err(|err| format!("write {}: {err}", target.display()))?;
            logf!(logger, "Copied {} to {}", config.display(), target.display());
        }
        Err(err) => {
            logf!(logger, "The service starts with the default settings: could not read {}: {err}", config.display())
        }
    }

    let binary = wide(&format!("\"{}\" --service", program.display()));
    let display = wide(DISPLAY_NAME);
    let manager = open_manager(SC_MANAGER_CONNECT | SC_MANAGER_CREATE_SERVICE)?;
    let service = match open_service(&manager, SERVICE_ALL_ACCESS) {
        Ok(service) => {
            // SAFETY: NUL-terminated strings; null leaves a setting unchanged.
            let ok = unsafe {
                ChangeServiceConfigW(
                    service.0,
                    SERVICE_WIN32_OWN_PROCESS,
                    SERVICE_AUTO_START,
                    SERVICE_ERROR_NORMAL,
                    binary.as_ptr(),
                    null(),
                    null_mut(),
                    null(),
                    null(),
                    null(),
                    display.as_ptr(),
                )
            };
            if ok == 0 {
                return Err(format!("update the service: {}", last_error()));
            }
            service
        }
        Err(err) if not_installed(&err) => {
            let name = wide(SERVICE_NAME);
            // SAFETY: NUL-terminated strings; a null account is LocalSystem.
            let service = unsafe {
                CreateServiceW(
                    manager.0,
                    name.as_ptr(),
                    display.as_ptr(),
                    SERVICE_ALL_ACCESS,
                    SERVICE_WIN32_OWN_PROCESS,
                    SERVICE_AUTO_START,
                    SERVICE_ERROR_NORMAL,
                    binary.as_ptr(),
                    null(),
                    null_mut(),
                    null(),
                    null(),
                    null(),
                )
            };
            if service.is_null() {
                return Err(format!("create the service: {}", last_error()));
            }
            ScHandle(service)
        }
        Err(err) => return Err(format!("open the service: {err}")),
    };

    let mut description = wide(DESCRIPTION);
    // Restart a failed service after 5 seconds, twice a day at most.
    let mut restart = [SC_ACTION { Type: SC_ACTION_RESTART, Delay: 5000 }; 2];
    let security = SecurityDescriptor::new(SERVICE_SECURITY)?;
    // SAFETY: the structures and the strings they point to outlive each call.
    unsafe {
        let info = SERVICE_DESCRIPTIONW { lpDescription: description.as_mut_ptr() };
        ChangeServiceConfig2W(service.0, SERVICE_CONFIG_DESCRIPTION, (&info as *const SERVICE_DESCRIPTIONW).cast());
        let failure = SERVICE_FAILURE_ACTIONSW {
            dwResetPeriod: 24 * 60 * 60,
            lpRebootMsg: null_mut(),
            lpCommand: null_mut(),
            cActions: restart.len() as u32,
            lpsaActions: restart.as_mut_ptr(),
        };
        ChangeServiceConfig2W(
            service.0,
            SERVICE_CONFIG_FAILURE_ACTIONS,
            (&failure as *const SERVICE_FAILURE_ACTIONSW).cast(),
        );
        if SetServiceObjectSecurity(service.0, DACL_SECURITY_INFORMATION, security.as_ptr()) == 0 {
            return Err(format!("set the service's permissions: {}", last_error()));
        }
    }
    logf!(logger, "Installed the Windows service, running {}", program.display());
    Ok(())
}

/// Makes the service's settings folder afresh, created with permissions that
/// let only SYSTEM and administrators change it. ProgramData lets any user
/// create folders and put files and links in them, which the service, running
/// as LocalSystem, would act on; nothing made before the folder may remain.
fn create_service_folder(folder: &Path, sddl: &str) -> Result<(), String> {
    // A link or junction is removed, not followed.
    match fs::remove_dir_all(folder) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(format!("clear {}: {err}", folder.display())),
    }
    let security = SecurityDescriptor::new(sddl)?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: security.as_ptr(),
        bInheritHandle: 0,
    };
    let path = wide(&folder.display().to_string());
    // SAFETY: a NUL-terminated path and attributes that outlive the call. It
    // fails if the folder was made again meanwhile.
    if unsafe { CreateDirectoryW(path.as_ptr(), &attributes) } == 0 {
        return Err(format!("create {}: {}", folder.display(), last_error()));
    }
    Ok(())
}

/// Runs elevated: stops the service, which takes its cables out of Windows,
/// and removes it and its program. Its settings folder and log stay.
pub fn uninstall_service(logger: &Logger) -> Result<(), String> {
    let manager = open_manager(SC_MANAGER_CONNECT)?;
    let service = match open_service(&manager, SERVICE_STOP | SERVICE_QUERY_STATUS | DELETE) {
        Ok(service) => service,
        Err(err) if not_installed(&err) => {
            logf!(logger, "The Windows service is not installed");
            return Ok(());
        }
        Err(err) => return Err(format!("open the service: {err}")),
    };
    // SAFETY: SERVICE_STATUS is plain data that ControlService fills in.
    unsafe {
        let mut status: SERVICE_STATUS = std::mem::zeroed();
        if ControlService(service.0, SERVICE_CONTROL_STOP, &mut status) == 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() != Some(ERROR_SERVICE_NOT_ACTIVE as i32) {
                return Err(format!("stop the service: {err}"));
            }
        }
    }
    wait_for(&service, SERVICE_STOPPED, Duration::from_secs(60))?;
    // SAFETY: marks the service for deletion; it goes once the handle closes.
    if unsafe { DeleteService(service.0) } == 0 {
        return Err(format!("remove the service: {}", last_error()));
    }
    drop(service);

    let program = program_path();
    let exe = std::env::current_exe().unwrap_or_default();
    if !same_file(&exe, &program) {
        if let Err(err) = fs::remove_file(&program) {
            logf!(logger, "Could not remove {}: {err}", program.display());
        }
        if let Some(folder) = program.parent() {
            let _ = fs::remove_dir(folder);
        }
    }
    logf!(logger, "Removed the Windows service");
    Ok(())
}

static SERVICE_MANAGER: OnceLock<Arc<AppServer>> = OnceLock::new();
static STATUS_HANDLE: AtomicPtr<c_void> = AtomicPtr::new(null_mut());
static CHECKPOINT: AtomicU32 = AtomicU32::new(0);
/// Set when Windows asks the service to stop.
static STOP: (Mutex<bool>, Condvar) = (Mutex::new(false), Condvar::new());

/// Runs as the service until Windows stops it.
pub fn run_service(manager: &Arc<AppServer>) -> Result<(), String> {
    let _ = SERVICE_MANAGER.set(Arc::clone(manager));
    let mut name = wide(SERVICE_NAME);
    let table = [
        SERVICE_TABLE_ENTRYW { lpServiceName: name.as_mut_ptr(), lpServiceProc: Some(service_main) },
        SERVICE_TABLE_ENTRYW { lpServiceName: null_mut(), lpServiceProc: None },
    ];
    // SAFETY: the table outlives the call, which returns once the service stops.
    if unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) } == 0 {
        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(ERROR_FAILED_SERVICE_CONTROLLER_CONNECT as i32) {
            return Err("--service is for Windows to start the service with; install it from the Options menu".into());
        }
        return Err(err.to_string());
    }
    Ok(())
}

fn report(state: u32, wait_hint: Duration) {
    let accepted = if state == SERVICE_RUNNING { SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN } else { 0 };
    let checkpoint = match state {
        SERVICE_RUNNING | SERVICE_STOPPED => 0,
        _ => CHECKPOINT.fetch_add(1, Ordering::SeqCst) + 1,
    };
    let status = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        dwControlsAccepted: accepted,
        dwWin32ExitCode: NO_ERROR,
        dwServiceSpecificExitCode: 0,
        dwCheckPoint: checkpoint,
        dwWaitHint: wait_hint.as_millis() as u32,
    };
    // SAFETY: the handle came from RegisterServiceCtrlHandlerExW.
    unsafe { SetServiceStatus(STATUS_HANDLE.load(Ordering::SeqCst), &status) };
}

unsafe extern "system" fn service_main(_: u32, _: *mut windows_sys::core::PWSTR) {
    let Some(manager) = SERVICE_MANAGER.get() else {
        return;
    };
    let logger = &manager.logger;
    let name = wide(SERVICE_NAME);
    // SAFETY: registers this service's control handler by its name.
    let handle = unsafe { RegisterServiceCtrlHandlerExW(name.as_ptr(), Some(control_handler), null()) };
    if handle.is_null() {
        logf!(logger, "Could not register the service control handler: {}", last_error());
        return;
    }
    STATUS_HANDLE.store(handle, Ordering::SeqCst);
    report(SERVICE_START_PENDING, Duration::from_secs(10));

    logf!(logger, "Running as the Windows service");
    control::init(manager);
    control::start(manager.cables());
    let pipe = PipeServer::start(logger)
        .inspect_err(|err| logf!(logger, "Could not open the service pipe, so no window can manage the service: {err}"))
        .ok();
    report(SERVICE_RUNNING, Duration::ZERO);

    let (stop, stopped) = &STOP;
    let mut stopping = stop.lock().unwrap_or_else(|p| p.into_inner());
    while !*stopping {
        stopping = stopped.wait(stopping).unwrap_or_else(|p| p.into_inner());
    }
    drop(stopping);

    logf!(logger, "Stopping the Windows service");
    report(SERVICE_STOP_PENDING, Duration::from_secs(60));
    if let Some(pipe) = pipe {
        pipe.stop();
    }
    control::stop();
    report(SERVICE_STOPPED, Duration::ZERO);
}

unsafe extern "system" fn control_handler(control: u32, _: u32, _: *mut c_void, _: *mut c_void) -> u32 {
    match control {
        SERVICE_CONTROL_STOP | SERVICE_CONTROL_SHUTDOWN => {
            let (stop, stopped) = &STOP;
            *stop.lock().unwrap_or_else(|p| p.into_inner()) = true;
            stopped.notify_all();
            NO_ERROR
        }
        SERVICE_CONTROL_INTERROGATE => NO_ERROR,
        _ => ERROR_CALL_NOT_IMPLEMENTED,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permissions_parse() {
        for sddl in [SERVICE_SECURITY, FOLDER_SECURITY] {
            assert!(SecurityDescriptor::new(sddl).is_ok(), "{sddl}");
        }
    }

    #[test]
    fn the_service_folder_is_made_afresh_without_following_links() {
        let base = std::env::temp_dir().join(format!("virtualcables-test-folder-{}", std::process::id()));
        let (folder, elsewhere) = (base.join("Virtual Cables"), base.join("elsewhere"));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&elsewhere).unwrap();
        fs::write(elsewhere.join("keep.txt"), "x").unwrap();
        // A junction planted where the folder goes, as any user could.
        let mut mklink = Command::new("cmd");
        mklink.arg("/C").arg("mklink").arg("/J").arg(&folder).arg(&elsewhere).creation_flags(CREATE_NO_WINDOW);
        assert!(mklink.status().unwrap().success());

        create_service_folder(&folder, "D:P(A;OICI;FA;;;AU)").unwrap();
        assert!(elsewhere.join("keep.txt").exists(), "the junction's target was cleared");
        let made = fs::symlink_metadata(&folder).unwrap();
        assert!(made.is_dir() && !made.file_type().is_symlink());
        assert_eq!(fs::read_dir(&folder).unwrap().count(), 0);
        // Installing again replaces the folder, with whatever is in it.
        fs::write(folder.join("old.log"), "x").unwrap();
        create_service_folder(&folder, "D:P(A;OICI;FA;;;AU)").unwrap();
        assert!(!folder.join("old.log").exists());
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn the_service_program_lives_in_program_files() {
        let path = program_path();
        assert!(path.ends_with(r"Virtual Cables\VirtualCables.exe"), "{}", path.display());
        assert!(!same_file(&std::env::current_exe().unwrap(), &path));
    }
}
