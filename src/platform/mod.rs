//! Platform integration: the Windows GUI, device attachment and process
//! tuning, with stand-ins elsewhere.

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{raise_process_priority, run_attach_broker, run_gui, show_fatal_dialog};

#[cfg(not(windows))]
mod other {
    use std::sync::Arc;

    use crate::AppServer;
    use crate::logging::Logger;

    pub fn raise_process_priority(_: &Logger) {}

    /// There is no window outside Windows: serve until the process is stopped.
    pub fn run_gui(_: &Arc<AppServer>) {
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
}
#[cfg(not(windows))]
pub use other::{raise_process_priority, run_attach_broker, run_gui, show_fatal_dialog};
