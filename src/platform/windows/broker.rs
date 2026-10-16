//! The one-time elevated broker: an administrator copy of the application that
//! attaches and detaches the cables in Windows on request, so the user
//! approves elevation once per session rather than on every change.

use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use windows_sys::Win32::Security::Cryptography::{BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom};
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;

use super::attach::{ServerTarget, find_usbip_executable, remove_cable_device, run_attach_helper};
use super::gui::{Tone, cable_ids, clear_status, gui_logger, main_window, server_address, set_status};
use super::win::wide;
use crate::config::{Cable, MAX_CABLE_ID, MAX_CABLES, is_valid_serial};
use crate::logf;
use crate::logging::Logger;

const NO_BROKER: &str = "Not in Windows Sound \u{00b7} the administrator helper did not start";
const LOST_BROKER: &str = "Windows Sound not updated \u{00b7} lost the administrator helper";

struct Connection {
    id: u64,
    stream: TcpStream,
    /// Whether a full synchronization has succeeded on this connection; until
    /// then, attachments may be left from an earlier run.
    synced: bool,
}

struct Broker {
    conn: Option<Connection>,
    /// The IDs of the cables Windows should have.
    desired: Vec<u32>,
    /// Removed cables whose Windows devices are still to be deleted.
    removed: Vec<Cable>,
    starting: bool,
}

static BROKER: Mutex<Broker> =
    Mutex::new(Broker { conn: None, desired: Vec::new(), removed: Vec::new(), starting: false });
/// Serializes synchronization requests on the broker connection.
static COMMAND: Mutex<()> = Mutex::new(());
static NEXT_CONNECTION: AtomicU64 = AtomicU64::new(1);

fn broker() -> std::sync::MutexGuard<'static, Broker> {
    BROKER.lock().unwrap_or_else(|p| p.into_inner())
}

fn mark_broker_start_failed() {
    broker().starting = false;
}

fn new_broker_token() -> Result<String, String> {
    let mut raw = [0u8; 32];
    // SAFETY: fills the local buffer with random bytes from the system RNG.
    let status = unsafe {
        BCryptGenRandom(std::ptr::null_mut(), raw.as_mut_ptr(), raw.len() as u32, BCRYPT_USE_SYSTEM_PREFERRED_RNG)
    };
    if status != 0 {
        return Err(format!("BCryptGenRandom failed with status 0x{:08X}", status as u32));
    }
    Ok(raw.iter().map(|b| format!("{b:02x}")).collect())
}

pub fn start_cable_broker() {
    let ids = cable_ids();
    if ids.is_empty() && broker().removed.is_empty() {
        return; // nothing to attach or delete, so no reason to ask for elevation
    }
    {
        let mut b = broker();
        if b.starting || b.conn.is_some() {
            return;
        }
        b.starting = true;
        b.desired = ids;
    }
    let logger = gui_logger();

    if find_usbip_executable().is_none() {
        mark_broker_start_failed();
        set_status(Tone::Error, "Not in Windows Sound \u{00b7} install the driver from the Options menu");
        return;
    }

    let listener = match TcpListener::bind("127.0.0.1:0") {
        Ok(listener) => listener,
        Err(err) => {
            mark_broker_start_failed();
            logf!(logger, "Could not create administrator broker channel: {err}");
            return;
        }
    };
    let token = match new_broker_token() {
        Ok(token) => token,
        Err(err) => {
            mark_broker_start_failed();
            logf!(logger, "Could not create administrator broker token: {err}");
            return;
        }
    };
    let exe = match std::env::current_exe() {
        Ok(exe) => exe.display().to_string(),
        Err(err) => {
            mark_broker_start_failed();
            logf!(logger, "Could not locate executable for administrator broker: {err}");
            return;
        }
    };
    let address = listener.local_addr().map(|a| a.to_string()).unwrap_or_default();
    // The elevated copy reaches the server at the address this one listens on,
    // including a --listen override.
    let listen = server_address();
    let parameters = format!("--attach-broker --broker-address {address} --broker-token {token} --listen \"{listen}\"");
    let directory = match exe.rfind(['\\', '/']) {
        Some(index) => exe[..index].to_string(),
        None => ".".into(),
    };
    // ShellExecuteW returns only once the elevation prompt is answered.
    set_status(Tone::Pending, "Waiting for one-time administrator approval\u{2026}");
    // SAFETY: all strings are NUL-terminated UTF-16 that outlive the call.
    let (result, err) = unsafe {
        let result = ShellExecuteW(
            main_window(),
            wide("runas").as_ptr(),
            wide(&exe).as_ptr(),
            wide(&parameters).as_ptr(),
            wide(&directory).as_ptr(),
            SW_HIDE, // the broker is a background process
        ) as isize;
        (result, std::io::Error::last_os_error())
    };
    if result <= 32 {
        mark_broker_start_failed();
        if err.raw_os_error().unwrap_or(0) != 0 {
            logf!(logger, "Administrator broker was not started ({result}): {err}");
        } else {
            logf!(logger, "Administrator broker was not started (ShellExecute code {result})");
        }
        set_status(Tone::Error, "Not in Windows Sound \u{00b7} administrator approval was cancelled");
        return;
    }
    std::thread::spawn(move || accept_cable_broker(listener, token));
}

/// Waits up to 60 seconds for the elevated copy to connect and authenticate.
fn accept_cable_broker(listener: TcpListener, token: String) {
    let logger = gui_logger();
    let deadline = Instant::now() + Duration::from_secs(60);
    let _ = listener.set_nonblocking(true);
    let conn = loop {
        match listener.accept() {
            Ok((conn, _)) => break Ok(conn),
            Err(err) if err.kind() == ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    break Err("accept timed out after 60 seconds".to_string());
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(err) => break Err(err.to_string()),
        }
    };
    drop(listener);
    let conn = match conn {
        Ok(conn) => conn,
        Err(err) => {
            mark_broker_start_failed();
            logf!(logger, "Administrator broker did not connect: {err}");
            set_status(Tone::Error, NO_BROKER);
            return;
        }
    };
    let _ = conn.set_nonblocking(false);
    let _ = conn.set_read_timeout(Some(Duration::from_secs(5)));
    let received = read_line(&conn);
    if received.as_deref().map(str::trim) != Ok(token.as_str()) {
        let _ = conn.shutdown(std::net::Shutdown::Both);
        mark_broker_start_failed();
        logf!(logger, "Administrator broker authentication failed");
        set_status(Tone::Error, NO_BROKER);
        return;
    }
    let _ = conn.set_read_timeout(None);
    {
        let mut b = broker();
        b.conn = Some(Connection { id: NEXT_CONNECTION.fetch_add(1, Ordering::SeqCst), stream: conn, synced: false });
        b.starting = false;
    }
    logf!(logger, "One-time administrator broker connected");
    sync_cable_devices();
}

/// Reads one line, byte by byte so that nothing after it is consumed.
fn read_line(mut conn: &TcpStream) -> Result<String, String> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match conn.read(&mut byte) {
            Ok(0) => return Err("EOF".into()),
            Ok(_) if byte[0] == b'\n' => return Ok(String::from_utf8_lossy(&line).into_owned()),
            Ok(_) => line.push(byte[0]),
            Err(err) if err.kind() == ErrorKind::Interrupted => {}
            Err(err) => return Err(err.to_string()),
        }
    }
}

/// Synchronizes Windows Sound again from scratch, for recovering from a
/// failure: every cable is detached and attached again, and the elevated
/// broker is started again if it is gone.
pub fn resync_cables() {
    let nothing_to_do = {
        let mut b = broker();
        if let Some(conn) = b.conn.as_mut() {
            conn.synced = false;
        }
        b.conn.is_none() && b.removed.is_empty() && cable_ids().is_empty()
    };
    if nothing_to_do {
        clear_status(); // no cables, so Windows has nothing to be out of line with
        return;
    }
    request_cable_sync();
}

pub fn request_cable_sync() {
    let (connected, starting) = {
        let mut b = broker();
        b.desired = cable_ids();
        (b.conn.is_some(), b.starting)
    };
    if connected {
        std::thread::spawn(sync_cable_devices);
        return;
    }
    if !starting {
        start_cable_broker();
    }
}

/// Has the broker delete the Windows devices of each removed cable. A cable
/// whose request cannot be sent stays queued for the next connection.
fn forget_removed_cables(mut conn: &TcpStream, connection: u64, logger: &Logger) {
    loop {
        let Some(cable) = broker().removed.first().cloned() else {
            return;
        };
        let _ = conn.set_read_timeout(Some(Duration::from_secs(45)));
        let _ = conn.set_write_timeout(Some(Duration::from_secs(45)));
        let reply = conn
            .write_all(forget_request(cable.id, &cable.serial).as_bytes())
            .map_err(|err| err.to_string())
            .and_then(|()| read_line(conn));
        let _ = conn.set_read_timeout(None);
        let _ = conn.set_write_timeout(None);
        match reply {
            Ok(reply) => {
                broker().removed.retain(|c| c != &cable);
                match reply.trim() {
                    "OK" => logf!(logger, "Removed the Windows devices of cable {} ({})", cable.id, cable.serial),
                    reply => logf!(logger, "Could not remove the Windows devices of cable {}: {reply}", cable.id),
                }
            }
            Err(err) => {
                logf!(logger, "Administrator broker device removal failed: {err}");
                let _ = conn.shutdown(std::net::Shutdown::Both);
                clear_broker_connection(connection);
                return;
            }
        }
    }
}

/// Queues the Windows devices of a cable that was just removed for deletion;
/// the next synchronization deletes them once the cable is detached.
pub fn forget_cable_devices(cable: &Cable) {
    let mut b = broker();
    if !b.removed.contains(cable) {
        b.removed.push(cable.clone());
    }
}

/// Sends the broker the cables Windows should have. The list is read once
/// this synchronization has the connection to itself, so whichever runs last
/// applies the latest list, however the threads that asked for them are
/// scheduled.
fn sync_cable_devices() {
    let _command = COMMAND.lock().unwrap_or_else(|p| p.into_inner());
    let logger = gui_logger();
    let connection = {
        let b = broker();
        b.conn.as_ref().and_then(|c| Some((c.id, c.stream.try_clone().ok()?, !c.synced, b.desired.clone())))
    };
    let Some((id, conn, full, ids)) = connection else {
        return;
    };
    set_status(Tone::Pending, "Synchronizing with Windows Sound\u{2026}");
    let _ = conn.set_read_timeout(Some(Duration::from_secs(45)));
    let _ = conn.set_write_timeout(Some(Duration::from_secs(45)));
    if let Err(err) = (&conn).write_all(sync_request(&ids, full).as_bytes()) {
        logf!(logger, "Could not send device synchronization request: {err}");
        let _ = conn.shutdown(std::net::Shutdown::Both);
        clear_broker_connection(id);
        set_status(Tone::Error, LOST_BROKER);
        return;
    }
    let reply = read_line(&conn);
    let _ = conn.set_read_timeout(None);
    let _ = conn.set_write_timeout(None);
    let reply = match reply {
        Ok(reply) => reply,
        Err(err) => {
            logf!(logger, "Administrator broker reply failed: {err}");
            let _ = conn.shutdown(std::net::Shutdown::Both);
            clear_broker_connection(id);
            set_status(Tone::Error, LOST_BROKER);
            return;
        }
    };
    let reply = reply.trim();
    // Removed cables are detached now either way, so their devices can go.
    forget_removed_cables(&conn, id, &logger);
    if reply != "OK" {
        logf!(logger, "Windows device synchronization failed: {reply}");
        set_status(Tone::Error, "Windows Sound synchronization failed \u{00b7} see the diagnostic log");
        return;
    }
    if let Some(c) = broker().conn.as_mut().filter(|c| c.id == id) {
        c.synced = true;
    }
    logf!(logger, "Windows Sound now synchronized with {} cable(s)", ids.len());
    if ids.is_empty() {
        clear_status();
    } else {
        set_status(Tone::Ok, "Devices available");
    }
}

fn clear_broker_connection(id: u64) {
    let mut b = broker();
    if b.conn.as_ref().is_some_and(|c| c.id == id) {
        b.conn = None;
    }
}

pub fn stop_cable_broker() {
    let mut b = broker();
    if let Some(conn) = b.conn.take() {
        let _ = (&conn.stream).write_all(b"QUIT\n");
        let _ = conn.stream.shutdown(std::net::Shutdown::Both);
    }
}

/// Runs in the elevated copy: connects back to the application and serves
/// synchronization requests until told to quit.
pub fn run_attach_broker(address: &str, token: &str, listen: &str, logger: &Logger) -> Result<(), String> {
    if !address.starts_with("127.0.0.1:") || token.len() != 64 {
        return Err("invalid broker parameters".into());
    }
    let target = ServerTarget::from_listen(listen)?;
    let addr: SocketAddr = address.parse().map_err(|err| format!("connect to main application: {err}"))?;
    let conn = TcpStream::connect_timeout(&addr, Duration::from_secs(10))
        .map_err(|err| format!("connect to main application: {err}"))?;
    (&conn).write_all(format!("{token}\n").as_bytes()).map_err(|err| err.to_string())?;
    let result = serve_broker_requests(conn, &target, logger);
    // Whether the application quit or was stopped, its server is gone. Take its
    // cables out of Windows, so usbip-win2 does not keep trying to reconnect
    // them until the next start.
    logf!(logger, "Main application closed; detaching its cables from Windows");
    if let Err(err) = run_attach_helper(&[], false, &target, logger) {
        logf!(logger, "Could not detach the cables: {err}");
    }
    result
}

/// Serves requests until QUIT, or until the application closes the connection
/// or is stopped.
fn serve_broker_requests(conn: TcpStream, target: &ServerTarget, logger: &Logger) -> Result<(), String> {
    let mut writer = conn.try_clone().map_err(|err| err.to_string())?;
    for line in BufReader::new(conn).lines() {
        let line = line.map_err(|err| err.to_string())?;
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields == ["QUIT"] {
            return Ok(());
        }
        let reply = match fields.as_slice() {
            [verb @ ("SYNC" | "RESYNC"), ids @ ..] => match parse_sync_ids(ids) {
                Some(ids) => match run_attach_helper(&ids, *verb == "RESYNC", target, logger) {
                    Ok(()) => "OK",
                    Err(err) => {
                        logf!(logger, "Broker synchronization failed: {err}");
                        "ERR synchronization failed"
                    }
                },
                None => "ERR invalid cable list",
            },
            ["FORGET", id, serial] => match parse_cable_device(id, serial) {
                Some((id, serial)) => match remove_cable_device(id, serial, logger) {
                    Ok(()) => "OK",
                    Err(err) => {
                        logf!(logger, "Broker device removal failed: {err}");
                        "ERR removal failed"
                    }
                },
                None => "ERR invalid cable",
            },
            _ => "ERR invalid request",
        };
        let _ = writer.write_all(format!("{reply}\n").as_bytes());
    }
    Ok(())
}

/// The request that makes the broker attach exactly these cables: RESYNC
/// re-attaches every one, SYNC changes only what differs.
fn sync_request(ids: &[u32], full: bool) -> String {
    let ids: Vec<String> = ids.iter().map(u32::to_string).collect();
    let verb = if full { "RESYNC" } else { "SYNC" };
    format!("{verb} {}\n", ids.join(" "))
}

/// The request that makes the broker delete a removed cable's devices.
fn forget_request(id: u32, serial: &str) -> String {
    format!("FORGET {id} {serial}\n")
}

/// The cable ID and serial number of a FORGET request. They are checked
/// strictly, since the elevated broker deletes the device they name.
fn parse_cable_device<'a>(id: &str, serial: &'a str) -> Option<(u32, &'a str)> {
    let id: u32 = id.parse().ok().filter(|id| (1..=MAX_CABLE_ID).contains(id))?;
    is_valid_serial(serial).then_some((id, serial))
}

/// The cable IDs of a SYNC request: up to 32 distinct IDs from 1 to 255. None
/// at all detaches every cable.
fn parse_sync_ids(fields: &[&str]) -> Option<Vec<u32>> {
    if fields.len() > MAX_CABLES {
        return None;
    }
    let mut ids = Vec::new();
    for field in fields {
        let id: u32 = field.parse().ok().filter(|id| (1..=MAX_CABLE_ID).contains(id))?;
        if ids.contains(&id) {
            return None;
        }
        ids.push(id);
    }
    Some(ids)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn broker_token_is_64_hex_digits() {
        let a = new_broker_token().unwrap();
        let b = new_broker_token().unwrap();
        assert_eq!(a.len(), 64);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn sync_requests_carry_cable_ids() {
        assert_eq!(sync_request(&[3, 1, 12], false), "SYNC 3 1 12\n");
        assert_eq!(sync_request(&[3, 1, 12], true), "RESYNC 3 1 12\n");
        assert_eq!(parse_sync_ids(&["3", "1", "12"]), Some(vec![3, 1, 12]));
        assert_eq!(sync_request(&[], false), "SYNC \n");
        assert_eq!(parse_sync_ids(&[]), Some(vec![]), "no cables detaches them all");
        for bad in [&["0"][..], &["256"], &["2", "2"], &["-1"], &["x"]] {
            assert_eq!(parse_sync_ids(bad), None, "{bad:?}");
        }
        let too_many: Vec<String> = (1..=33).map(|id| id.to_string()).collect();
        let too_many: Vec<&str> = too_many.iter().map(String::as_str).collect();
        assert_eq!(parse_sync_ids(&too_many), None);
    }

    #[test]
    fn forget_requests_name_one_cable_device() {
        assert_eq!(forget_request(3, "8F3A2C1D"), "FORGET 3 8F3A2C1D\n");
        assert_eq!(parse_cable_device("3", "8F3A2C1D"), Some((3, "8F3A2C1D")));
        assert_eq!(parse_cable_device("0", "8F3A2C1D"), None);
        assert_eq!(parse_cable_device("256", "8F3A2C1D"), None);
        // Nothing that could reach outside the cable's own instance ID.
        for serial in ["", "..\\x", "A\\B", "A&B", "A*", "A B", &"A".repeat(33)] {
            assert_eq!(parse_cable_device("3", serial), None, "{serial:?}");
        }
    }

    const LISTEN: &str = "127.0.0.1:3240";

    #[test]
    fn broker_rejects_bad_parameters_and_requests() {
        let logger = Logger::discard();
        assert!(run_attach_broker("127.0.0.1:5", &"a".repeat(64), "bad", &logger).is_err(), "invalid listen address");
        assert_eq!(
            run_attach_broker("10.0.0.1:5", &"a".repeat(64), LISTEN, &logger),
            Err("invalid broker parameters".into())
        );
        assert_eq!(run_attach_broker("127.0.0.1:5", "short", LISTEN, &logger), Err("invalid broker parameters".into()));

        // The broker answers malformed requests and stops on QUIT. Its requests
        // are served directly: once connected, run_attach_broker would detach
        // this machine's real cables when the connection ends.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let conn = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (served, _) = listener.accept().unwrap();
        let target = ServerTarget::from_listen(LISTEN).unwrap();
        let broker = std::thread::spawn(move || serve_broker_requests(served, &target, &Logger::discard()));
        (&conn).write_all(b"HELLO\nSYNC 2 2\nSYNC 256\nSYNC x\n").unwrap();
        assert_eq!(read_line(&conn).unwrap(), "ERR invalid request");
        for _ in 0..3 {
            assert_eq!(read_line(&conn).unwrap(), "ERR invalid cable list");
        }
        (&conn).write_all(b"FORGET 3 A\\B\nFORGET 0 8F3A2C1D\nFORGET 3\n").unwrap();
        assert_eq!(read_line(&conn).unwrap(), "ERR invalid cable");
        assert_eq!(read_line(&conn).unwrap(), "ERR invalid cable");
        assert_eq!(read_line(&conn).unwrap(), "ERR invalid request");
        (&conn).write_all(b"QUIT\n").unwrap();
        assert_eq!(broker.join().unwrap(), Ok(()));
    }
}
