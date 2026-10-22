//! Platform integration: the Windows GUI and service, device attachment and
//! process tuning, with stand-ins elsewhere.

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{
    enter_audio_thread, install_service, raise_process_priority, run_attach_broker, run_gui, run_service,
    show_fatal_dialog, uninstall_service,
};

#[cfg(not(windows))]
mod other {
    use std::path::Path;
    use std::sync::Arc;

    use crate::AppServer;
    use crate::logging::Logger;

    pub fn raise_process_priority(_: &Logger) {}

    pub struct AudioThread;

    pub fn enter_audio_thread(_: &Logger) -> AudioThread {
        AudioThread
    }

    /// There is no window outside Windows: serve until the process is stopped.
    pub fn run_gui(manager: &Arc<AppServer>) {
        let _ = manager.restart(&manager.cables());
        loop {
            std::thread::park();
        }
    }

    pub fn show_fatal_dialog(message: &str) {
        eprintln!("{message}");
    }

    pub fn run_attach_broker(_: &str, _: &str, _: &str, _: &Logger) -> Result<(), String> {
        Err("USB/IP attachment is only supported on Windows".into())
    }

    pub fn run_service(_: &Arc<AppServer>) -> Result<(), String> {
        Err("the service is only supported on Windows".into())
    }

    pub fn install_service(_: &Path, _: &Logger) -> Result<(), String> {
        Err("the service is only supported on Windows".into())
    }

    pub fn uninstall_service(_: &Logger) -> Result<(), String> {
        Err("the service is only supported on Windows".into())
    }
}
#[cfg(not(windows))]
pub use other::{
    enter_audio_thread, install_service, raise_process_priority, run_attach_broker, run_gui, run_service,
    show_fatal_dialog, uninstall_service,
};
