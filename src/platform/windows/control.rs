//! The cables and their state in Windows Sound, kept by the process that runs
//! the USB/IP server: the window when it runs on demand, or the Windows
//! service. The window reads them directly in the first case, and through the
//! service's pipe in the second.

use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use super::broker::{forget_cable_devices, request_cable_sync, resync_cables, start_cable_broker, stop_cable_broker};
use crate::AppServer;
use crate::config::{Cable, DEFAULT_LISTEN, new_cable, save_cables};
use crate::logging::Logger;

/// How a status line reads at a glance, shown as the colour of its dot.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Tone {
    Ok,
    Pending,
    Error,
}

/// What the window shows of the server and its cables.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    /// The cables, in list order.
    pub cables: Vec<Cable>,
    /// Why the USB/IP server is not running; empty while it runs.
    pub server_error: String,
    /// Windows Sound synchronization, when there is something to say about it.
    pub status: Option<(Tone, String)>,
}

static MANAGER: OnceLock<Arc<AppServer>> = OnceLock::new();
/// The cables, in list order.
static CABLES: Mutex<Vec<Cable>> = Mutex::new(Vec::new());
static STATUS: Mutex<Option<(Tone, String)>> = Mutex::new(None);
/// Serializes changes to the cables, which the service takes from any window.
static CHANGES: Mutex<()> = Mutex::new(());
/// Called whenever the snapshot may have changed, from any thread.
static ON_CHANGE: OnceLock<fn()> = OnceLock::new();

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|p| p.into_inner())
}

fn manager() -> Option<&'static Arc<AppServer>> {
    MANAGER.get()
}

fn changed() {
    if let Some(on_change) = ON_CHANGE.get() {
        on_change();
    }
}

/// Sets the server this process runs the cables on.
pub fn init(manager: &Arc<AppServer>) {
    let _ = MANAGER.set(Arc::clone(manager));
}

/// Sets what to call whenever the snapshot may have changed.
pub fn on_change(callback: fn()) {
    let _ = ON_CHANGE.set(callback);
}

/// Starts the USB/IP server with `cables` and brings Windows in line. A server
/// that cannot start leaves its error in the snapshot.
pub fn start(cables: Vec<Cable>) {
    let Some(manager) = manager() else {
        return;
    };
    *lock(&CABLES) = cables.clone();
    // A failure is logged and kept as the server's error.
    let _ = manager.restart(&cables);
    changed();
    start_cable_broker();
}

/// Takes the cables out of Windows and stops the USB/IP server.
pub fn stop() {
    stop_cable_broker();
    if let Some(manager) = manager() {
        manager.stop();
    }
    changed();
}

pub fn snapshot() -> Snapshot {
    Snapshot {
        cables: lock(&CABLES).clone(),
        server_error: manager().map(|m| m.last_error()).unwrap_or_default(),
        status: lock(&STATUS).clone(),
    }
}

/// The IDs of the cables Windows should have.
pub fn cable_ids() -> Vec<u32> {
    lock(&CABLES).iter().map(|c| c.id).collect()
}

/// The application log.
pub fn logger() -> Logger {
    manager().map_or_else(Logger::discard, |m| m.logger.clone())
}

/// Where the application log is.
pub fn log_path() -> String {
    manager().map(|m| m.log_path()).unwrap_or_default()
}

/// The address the USB/IP server listens on.
pub fn server_address() -> String {
    manager().map(|m| m.address().to_string()).unwrap_or_else(|| DEFAULT_LISTEN.to_string())
}

/// Sets the Windows Sound status; safe from any thread.
pub fn set_status(tone: Tone, text: &str) {
    *lock(&STATUS) = Some((tone, text.to_string()));
    changed();
}

/// Removes the Windows Sound status; safe from any thread.
pub fn clear_status() {
    *lock(&STATUS) = None;
    changed();
}

/// Adds a cable with the name the user typed. The error says why it could
/// not be added, or that it was added but could not be saved.
pub fn add_cable(name: &str) -> Result<(), String> {
    let _change = lock(&CHANGES);
    let mut updated = lock(&CABLES).clone();
    updated.push(new_cable(name, &updated)?);
    apply_cables(updated, None)
}

/// Removes the cable with this serial number, and its devices from Windows.
pub fn remove_cable(serial: &str) -> Result<(), String> {
    let _change = lock(&CHANGES);
    let mut updated = lock(&CABLES).clone();
    let index = updated.iter().position(|c| c.serial == serial).ok_or("That cable no longer exists.")?;
    let removed = updated.remove(index);
    apply_cables(updated, Some(removed))
}

/// Applies `updated` to the running server, saves it and brings Windows in
/// line, deleting the Windows devices of the `removed` cable. Cables that did
/// not change keep streaming.
fn apply_cables(updated: Vec<Cable>, removed: Option<Cable>) -> Result<(), String> {
    let manager = manager().ok_or("The USB/IP server is not available.")?;
    manager.apply(&updated).map_err(|err| format!("Could not update the USB/IP server:\n{err}"))?;
    *lock(&CABLES) = updated.clone();
    let saved = save_cables(&updated);
    changed();
    if let Some(removed) = removed {
        forget_cable_devices(&removed);
    }
    request_cable_sync();
    saved.map_err(|err| format!("The cables were changed, but config could not be saved:\n{err}"))
}

/// Brings the server and Windows Sound back in line with the cables, for
/// recovering from a failure: starts the USB/IP server if it is not running,
/// then detaches and attaches every cable again.
pub fn sync_now() -> Result<(), String> {
    let _change = lock(&CHANGES);
    let started = match manager() {
        Some(manager) => {
            manager.apply(&lock(&CABLES).clone()).map_err(|err| format!("Could not start the USB/IP server:\n{err}"))
        }
        None => Ok(()),
    };
    changed();
    resync_cables();
    started
}
