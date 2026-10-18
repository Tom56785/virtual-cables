#![cfg_attr(all(windows, not(test)), windows_subsystem = "windows")]

//! Virtual Cables: virtual audio cables exported as USB Audio Class devices
//! over USB/IP, so Windows' in-box USB audio driver exposes them as standard
//! playback and recording devices.

mod audio;
mod cli;
mod config;
mod event;
mod logging;
mod platform;
mod uac1;
mod usbip;

use std::env;
use std::fs::{self, OpenOptions};
use std::path::PathBuf;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use clap::Parser;

use crate::cli::Cli;
use crate::config::{
    Cable, Config, DEFAULT_LISTEN, MAX_CABLES, config_bool, config_cables, config_int, config_string,
    ensure_config_file, load_config_content, numbered_cables, parse_config,
};
use crate::logging::{AsyncWriter, Logger, flush_log, set_app_log_writer};
use crate::uac1::{DEFAULT_LATENCY_MS, Device, MAX_LATENCY_MS, MIN_LATENCY_MS, bus_id};
use crate::usbip::Server;
use crate::usbip::server::PlaybackCompletion;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Runs and restarts the USB/IP server for the GUI.
pub struct AppServer {
    inner: Mutex<AppInner>,
    address: String,
    /// The delay between each cable's playback and recording sides.
    latency_ms: i64,
    playback_completion: PlaybackCompletion,
    /// For troubleshooting: how long to hold all transfers once a minute.
    stall_test: Option<Duration>,
    /// Enables the USB/IP stream traces in the log.
    diagnostics: bool,
    pub logger: Logger,
    log_path: String,
}

struct AppInner {
    server: Option<Arc<Server>>,
    done: Option<mpsc::Receiver<Result<(), String>>>,
    cables: Vec<Cable>,
    last_error: String,
}

impl AppServer {
    fn lock(&self) -> MutexGuard<'_, AppInner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn restart(&self, cables: &[Cable]) -> Result<(), String> {
        let mut inner = self.lock();
        self.stop_locked(&mut inner);

        let devices = match make_devices(cables, self.latency_ms) {
            Ok(devices) => devices,
            Err(err) => {
                self.set_error_locked(&mut inner, &err);
                return Err(err);
            }
        };
        let mut server = Server::new(self.address.clone(), devices, self.logger.clone());
        server.diagnostics = self.diagnostics;
        server.playback_completion = self.playback_completion;
        server.stall_test = self.stall_test;
        let server = Arc::new(server);
        let (ready_tx, ready) = mpsc::channel();
        let (done_tx, done) = mpsc::channel();
        let running = Arc::clone(&server);
        std::thread::Builder::new()
            .name("usbip server".into())
            .spawn(move || {
                let result = running.serve(move |r| {
                    let _ = ready_tx.send(r);
                });
                let _ = done_tx.send(result);
            })
            .map_err(|e| e.to_string())?;

        let failure = match ready.recv_timeout(Duration::from_secs(3)) {
            Ok(Ok(())) => {
                inner.server = Some(server);
                inner.done = Some(done);
                inner.cables = cables.to_vec();
                inner.last_error.clear();
                logf!(self.logger, "Virtual Cables {VERSION} started with {} cable(s)", cables.len());
                return Ok(());
            }
            Ok(Err(err)) => err,
            Err(RecvTimeoutError::Disconnected) => "USB/IP server stopped before becoming ready".to_string(),
            Err(RecvTimeoutError::Timeout) => format!("timed out while opening USB/IP port {}", self.address),
        };
        server.shutdown();
        self.set_error_locked(&mut inner, &failure);
        Err(failure)
    }

    /// Brings the running server in line with `cables`: removed cables stop
    /// being exported, which closes their sessions, and new ones are added,
    /// while the others keep streaming. Starts the server if it is not running,
    /// and restarts it if the change cannot be applied in place.
    #[cfg_attr(not(windows), allow(dead_code))] // used by the Windows GUI
    pub fn apply(&self, cables: &[Cable]) -> Result<(), String> {
        let mut inner = self.lock();
        let Some(server) = inner.server.clone() else {
            drop(inner);
            return self.restart(cables);
        };
        let added: Vec<Cable> = cables.iter().filter(|c| !inner.cables.contains(c)).cloned().collect();
        let removed: Vec<Cable> = inner.cables.iter().filter(|c| !cables.contains(c)).cloned().collect();
        let devices = make_devices(&added, self.latency_ms)?;
        for cable in &removed {
            server.remove_device(&bus_id(cable.id));
        }
        for device in devices {
            if let Err(err) = server.add_device(device) {
                logf!(self.logger, "Could not add a cable in place ({err}); restarting the USB/IP server");
                drop(inner);
                return self.restart(cables);
            }
        }
        inner.cables = cables.to_vec();
        if !added.is_empty() || !removed.is_empty() {
            logf!(
                self.logger,
                "Cables updated: {} added, {} removed, {} in all",
                added.len(),
                removed.len(),
                cables.len()
            );
        }
        Ok(())
    }

    pub fn stop(&self) {
        let mut inner = self.lock();
        self.stop_locked(&mut inner);
    }

    pub fn last_error(&self) -> String {
        self.lock().last_error.clone()
    }

    pub fn log_path(&self) -> String {
        self.log_path.clone()
    }

    /// The address the USB/IP server listens on.
    #[cfg_attr(not(windows), allow(dead_code))] // used by the Windows GUI
    pub fn address(&self) -> &str {
        &self.address
    }

    pub fn cables(&self) -> Vec<Cable> {
        self.lock().cables.clone()
    }

    fn set_error_locked(&self, inner: &mut AppInner, err: &str) {
        inner.last_error = err.to_string();
        logf!(self.logger, "USB/IP server error: {err}");
    }

    fn stop_locked(&self, inner: &mut AppInner) {
        let Some(server) = inner.server.take() else {
            return;
        };
        server.shutdown();
        if let Some(done) = inner.done.take() {
            match done.recv_timeout(Duration::from_secs(2)) {
                Ok(Err(err)) => logf!(self.logger, "USB/IP server stopped with error: {err}"),
                Ok(Ok(())) | Err(RecvTimeoutError::Disconnected) => {}
                Err(RecvTimeoutError::Timeout) => {
                    logf!(self.logger, "USB/IP server shutdown timed out; continuing restart")
                }
            }
        }
    }
}

fn main() {
    let parsed = Cli::try_parse_from(env::args_os());
    // Only a normal start clears the log. Helper runs (the administrator broker
    // in particular runs alongside the application), --help and command-line
    // errors append to it instead.
    let fresh = matches!(&parsed, Ok(cli) if !cli.is_helper_mode());
    let (logger, log_closer, log_path) = new_application_logger(fresh);
    let code = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(&logger, &log_path, parsed))) {
        Ok(code) => code,
        Err(panic) => {
            let detail = panic
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".into());
            let message = format!("Unexpected startup failure: {detail}");
            logf!(logger, "PANIC: {message}");
            platform::show_fatal_dialog(&format!("{message}\n\nDiagnostic log:\n{log_path}"));
            0
        }
    };
    if let Some(closer) = log_closer {
        closer.close();
    }
    if code != 0 {
        std::process::exit(code);
    }
}

/// Runs the application and returns the process exit code.
fn run(logger: &Logger, log_path: &str, parsed: Result<Cli, clap::Error>) -> i32 {
    logf!(logger, "============================================================");
    logf!(logger, "Virtual Cables {VERSION} ({}/{})", os(), arch());
    logf!(logger, "Process started. Executable: {}", executable_path());
    logf!(logger, "Log file: {log_path}");
    logf!(logger, "USB/IP protocol: 0x{:04X}", usbip::PROTOCOL_VERSION);
    platform::raise_process_priority(logger);

    let cli = match parsed {
        Ok(cli) => cli,
        Err(err) => {
            // --help prints to standard output and exits 0; errors exit 2.
            let _ = err.print();
            return err.exit_code();
        }
    };
    if cli.version {
        println!("Virtual Cables {VERSION} ({}/{})", os(), arch());
        return 0;
    }

    let config = parse_config(&load_config_content()).unwrap_or_else(|err| {
        // The application still starts, with the defaults; saving the cables
        // refuses to overwrite the file until it is fixed.
        logf!(logger, "Using the default settings: {err}");
        Config::default()
    });
    let cables = startup_cables(cli.cables, &config, logger);
    let (latency_ms, playback_completion, stall_test) = audio_settings(&cli, &config, logger);
    let diagnostics = cli.diagnostics.unwrap_or_else(|| config_bool(&config, "diagnostics", false));
    let listen = cli.listen.clone().unwrap_or_else(|| config_string(&config, "listen", DEFAULT_LISTEN));
    logf!(logger, "Diagnostic logging: {diagnostics}");
    if cli.attach_broker {
        let address = cli.broker_address.unwrap_or_default();
        let token = cli.broker_token.unwrap_or_default();
        if let Err(err) = platform::run_attach_broker(&address, &token, &listen, logger) {
            logf!(logger, "Administrator broker failed: {err}");
            flush_log();
            return 1;
        }
        return 0;
    }

    let self_test = cli.self_test;
    let dump = cli.dump_descriptors;
    // The diagnostics check one stand-in cable when none are configured.
    let checked = if cables.is_empty() && (self_test || dump) { numbered_cables(1) } else { cables.clone() };
    let devices = match make_devices(&checked, latency_ms) {
        Ok(devices) => devices,
        Err(err) => {
            return fail(logger, log_path, "Could not create the virtual cable devices", &err, self_test || dump);
        }
    };
    if self_test {
        if let Err(err) = run_self_test(&devices) {
            return fail(logger, log_path, "Self-test failed", &err, true);
        }
        println!("Virtual Cables {VERSION} self-test passed.");
        logf!(logger, "Self-test passed");
        return 0;
    }
    if dump {
        for dev in &devices {
            print!("\n[{} | bus ID {}]\n", dev.product(), dev.bus_id);
            print!("{}", hex_dump(&dev.descriptors.device));
            print!("{}", hex_dump(&dev.descriptors.config));
        }
        return 0;
    }

    ensure_config_file(logger);

    let manager = Arc::new(AppServer {
        inner: Mutex::new(AppInner { server: None, done: None, cables: cables.clone(), last_error: String::new() }),
        address: listen,
        latency_ms,
        playback_completion,
        stall_test,
        diagnostics,
        logger: logger.clone(),
        log_path: log_path.to_string(),
    });
    if let Err(err) = manager.restart(&cables) {
        // Do not close the GUI. The window remains available so the user can
        // install the driver, inspect the status and open the diagnostic log.
        logf!(logger, "Initial USB/IP server start failed; keeping GUI open: {err}");
    }
    platform::run_gui(&manager);
    logf!(logger, "Virtual Cables window closed normally");
    manager.stop();
    0
}

/// Logs and reports a fatal startup error, and returns the exit code.
fn fail(logger: &Logger, log_path: &str, title: &str, err: &str, console_only: bool) -> i32 {
    let message = format!("{title}: {err}");
    logf!(logger, "FATAL: {message}");
    eprintln!("[ERROR] {message}");
    if !console_only {
        platform::show_fatal_dialog(&format!("{message}\n\nDiagnostic log:\n{log_path}"));
    }
    flush_log();
    1
}

/// The longest stall the stall test may simulate; it repeats every minute.
const MAX_STALL_TEST_MS: i64 = 10_000;

/// The audio settings, from the command line, config.toml or the defaults,
/// with invalid values replaced by the defaults and logged.
fn audio_settings(cli: &Cli, config: &Config, logger: &Logger) -> (i64, PlaybackCompletion, Option<Duration>) {
    let mut latency_ms = cli.latency_ms.unwrap_or_else(|| config_int(config, "latency_ms", DEFAULT_LATENCY_MS));
    if !(MIN_LATENCY_MS..=MAX_LATENCY_MS).contains(&latency_ms) {
        logf!(
            logger,
            "Invalid latency {latency_ms} ms (must be {MIN_LATENCY_MS}-{MAX_LATENCY_MS} ms); falling back to {DEFAULT_LATENCY_MS} ms"
        );
        latency_ms = DEFAULT_LATENCY_MS;
    }

    let playback_completion = cli.playback_completion.unwrap_or_else(|| {
        let text = config_string(config, "playback_completion", PlaybackCompletion::default().name());
        PlaybackCompletion::parse(&text).unwrap_or_else(|| {
            let fallback = PlaybackCompletion::default().name();
            logf!(logger, "Invalid playback_completion {text:?} (must be \"start\" or \"end\"); using \"{fallback}\"");
            PlaybackCompletion::default()
        })
    });

    let mut stall_test_ms = cli.stall_test_ms.unwrap_or_else(|| config_int(config, "stall_test_ms", 0));
    if !(0..=MAX_STALL_TEST_MS).contains(&stall_test_ms) {
        logf!(logger, "Invalid stall test {stall_test_ms} ms (must be 0-{MAX_STALL_TEST_MS} ms); turning it off");
        stall_test_ms = 0;
    }
    let stall_test = (stall_test_ms > 0).then(|| Duration::from_millis(stall_test_ms as u64));

    logf!(
        logger,
        "Audio: {latency_ms} ms latency; playback transfers complete at their {}",
        playback_completion.name()
    );
    if stall_test.is_some() {
        logf!(logger, "Stall test: holding audio transfers for {stall_test_ms} ms every minute");
    }
    (latency_ms, playback_completion, stall_test)
}

/// The cables to start with: cables 1 to N for --cables N, otherwise the
/// [[cable]] tables of config.toml, which may be none.
fn startup_cables(cli_count: Option<i64>, config: &Config, logger: &Logger) -> Vec<Cable> {
    match cli_count {
        Some(count) if (1..=MAX_CABLES as i64).contains(&count) => numbered_cables(count),
        Some(count) => {
            logf!(logger, "Invalid cable count {count}; using the cables in config.toml");
            config_cables(config)
        }
        None => config_cables(config),
    }
}

pub fn make_devices(cables: &[Cable], latency_ms: i64) -> Result<Vec<Arc<Device>>, String> {
    cables
        .iter()
        .map(|cable| {
            Device::named(cable.id, &cable.serial, &cable.name, latency_ms)
                .map(Arc::new)
                .map_err(|err| format!("create cable {} ({}): {err}", cable.id, cable.name))
        })
        .collect()
}

fn run_self_test(devices: &[Arc<Device>]) -> Result<(), String> {
    use crate::audio::PACKET_BYTES;
    use crate::uac1::{REQUEST_SET_CONFIGURATION, REQUEST_SET_INTERFACE, SetupPacket};
    for dev in devices {
        dev.descriptors.validate().map_err(|err| format!("{}: {err}", dev.product()))?;
        let (_, status) =
            dev.handle_control(SetupPacket { request: REQUEST_SET_CONFIGURATION, value: 1, ..Default::default() }, &[]);
        if status != 0 {
            return Err(format!("set configuration status {status}"));
        }
        for iface in [1, 2] {
            let setup =
                SetupPacket { request_type: 0x01, request: REQUEST_SET_INTERFACE, index: iface, value: 1, length: 0 };
            let (_, status) = dev.handle_control(setup, &[]);
            if status != 0 {
                return Err(format!("activate interface status {status}"));
            }
        }
        // A playback packet must come out of the recording side unchanged,
        // exactly the latency later.
        let packet: Vec<u8> = (0..PACKET_BYTES).map(|i| (i * 7 + 1) as u8).collect();
        let frame = 1_000_000;
        let latency = dev.audio.delay();
        dev.write_playback(frame, [packet.as_slice()]);
        let mut early = vec![0u8; PACKET_BYTES];
        dev.read_capture(frame + latency - 1, &mut early, [PACKET_BYTES]);
        let mut out = vec![0u8; PACKET_BYTES];
        dev.read_capture(frame + latency, &mut out, [PACKET_BYTES]);
        if early.iter().any(|&b| b != 0) {
            return Err("PCM loopback came out early".into());
        }
        if out != packet {
            return Err("PCM loopback mismatch".into());
        }
    }
    Ok(())
}

/// Formats bytes as encoding/hex Dump does (the `hexdump -C` layout).
fn hex_dump(data: &[u8]) -> String {
    let mut text = String::new();
    for (line, chunk) in data.chunks(16).enumerate() {
        text.push_str(&format!("{:08x}  ", line * 16));
        for position in 0..16 {
            match chunk.get(position) {
                Some(b) => text.push_str(&format!("{b:02x} ")),
                None => text.push_str("   "),
            }
            if position == 7 || position == 15 {
                text.push(' ');
            }
        }
        text.push('|');
        text.extend(chunk.iter().map(|&b| if (32..=126).contains(&b) { b as char } else { '.' }));
        text.push_str("|\n");
    }
    text
}

fn os() -> &'static str {
    env::consts::OS
}

fn arch() -> &'static str {
    match env::consts::ARCH {
        "x86_64" => "amd64",
        "x86" => "386",
        "aarch64" => "arm64",
        other => other,
    }
}

fn executable_path() -> String {
    env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| "unknown".into())
}

/// Flushes and closes the log file at exit.
pub struct LogCloser(Arc<AsyncWriter>);

impl LogCloser {
    fn close(self) {
        self.0.close();
    }
}

/// Opens the persistent log. With `fresh` set, the log is cleared so that it
/// holds only the current session and cannot grow forever.
fn new_application_logger(fresh: bool) -> (Logger, Option<LogCloser>, String) {
    let exe_logs = env::current_exe().ok().and_then(|exe| exe.parent().map(|dir| dir.join("Logs")));
    let candidates: Vec<PathBuf> =
        [Some(config::application_data_dir()), exe_logs, Some(env::temp_dir().join("Virtual Cables"))]
            .into_iter()
            .flatten()
            .collect();

    for dir in candidates {
        if dir.as_os_str().is_empty() || dir == *"." || fs::create_dir_all(&dir).is_err() {
            continue;
        }
        let path = dir.join("VirtualCables_USBIP.log");
        if fresh && OpenOptions::new().write(true).create(true).truncate(true).open(&path).is_err() {
            continue;
        }
        // Append mode: the administrator broker writes to the same file.
        let Ok(file) = OpenOptions::new().create(true).append(true).open(&path) else {
            continue;
        };
        let writer = AsyncWriter::new(file);
        set_app_log_writer(Arc::clone(&writer));
        return (Logger::asynchronous(Arc::clone(&writer)), Some(LogCloser(writer)), path.display().to_string());
    }

    let path = env::temp_dir().join("VirtualCables_USBIP.log");
    (Logger::stderr(), None, path.display().to_string())
}

#[cfg(test)]
pub(crate) mod alloc_counter {
    //! Counts heap allocations made by the current thread.

    // The initializer below is already const; clippy misreports it.
    #![allow(clippy::missing_const_for_thread_local)]

    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;

    thread_local! {
        static COUNT: Cell<u64> = const { Cell::new(0) };
    }

    struct Counting;

    fn count() {
        let _ = COUNT.try_with(|c| c.set(c.get() + 1));
    }

    // SAFETY: every call is forwarded unchanged to the system allocator.
    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            count();
            unsafe { System.alloc(layout) }
        }
        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            count();
            unsafe { System.alloc_zeroed(layout) }
        }
        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            count();
            unsafe { System.realloc(ptr, layout, new_size) }
        }
        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            unsafe { System.dealloc(ptr, layout) }
        }
    }

    #[global_allocator]
    static ALLOCATOR: Counting = Counting;

    pub fn allocations() -> u64 {
        COUNT.with(|c| c.get())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn application_log_starts_fresh() {
        let _guard = config::tests::ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = config::tests::temp_app_data("log");
        let path = dir.join("Virtual Cables").join("VirtualCables_USBIP.log");
        fs::create_dir_all(path.parent().unwrap()).unwrap();

        let open = |fresh: bool, line: &str| {
            fs::write(&path, "previous session\n").unwrap();
            let (logger, closer, logged_path) = new_application_logger(fresh);
            assert_eq!(logged_path, path.display().to_string());
            logf!(logger, "{line}");
            closer.unwrap().close();
            fs::read_to_string(&path).unwrap()
        };
        let fresh = open(true, "new session");
        assert!(!fresh.contains("previous session") && fresh.contains("new session"), "fresh log={fresh:?}");
        let appended = open(false, "helper");
        assert!(appended.contains("previous session") && appended.contains("helper"), "appended log={appended:?}");
    }

    #[test]
    fn hex_dump_matches() {
        let data = |n: usize| -> Vec<u8> { (0..n).map(|i| (i * 37 + 20) as u8).collect() };
        let cases = [
            (0, ""),
            (1, "00000000  14                                                |.|\n"),
            (7, "00000000  14 39 5e 83 a8 cd f2                              |.9^....|\n"),
            (8, "00000000  14 39 5e 83 a8 cd f2 17                           |.9^.....|\n"),
            (15, "00000000  14 39 5e 83 a8 cd f2 17  3c 61 86 ab d0 f5 1a     |.9^.....<a.....|\n"),
            (16, "00000000  14 39 5e 83 a8 cd f2 17  3c 61 86 ab d0 f5 1a 3f  |.9^.....<a.....?|\n"),
            (
                18,
                "00000000  14 39 5e 83 a8 cd f2 17  3c 61 86 ab d0 f5 1a 3f  |.9^.....<a.....?|\n00000010  64 89                                             |d.|\n",
            ),
            (
                33,
                "00000000  14 39 5e 83 a8 cd f2 17  3c 61 86 ab d0 f5 1a 3f  |.9^.....<a.....?|\n00000010  64 89 ae d3 f8 1d 42 67  8c b1 d6 fb 20 45 6a 8f  |d.....Bg.... Ej.|\n00000020  b4                                                |.|\n",
            ),
        ];
        for (n, want) in cases {
            assert_eq!(hex_dump(&data(n)), want, "{n} bytes");
        }
    }

    #[test]
    fn self_test_passes() {
        let devices = make_devices(&numbered_cables(2), DEFAULT_LATENCY_MS).unwrap();
        run_self_test(&devices).unwrap();
    }

    #[test]
    fn startup_cables_sources() {
        let logger = Logger::discard();
        let named = parse_config(
            "[[cable]]\nid = 3\nserial = \"0000AAAA\"\nname = \"Game\"\n\n\
             [[cable]]\nid = 1\nserial = \"0000BBBB\"\nname = \"Chat\"\n",
        )
        .unwrap();
        let ids = |cables: Vec<Cable>| cables.iter().map(|c| c.id).collect::<Vec<_>>();
        assert_eq!(ids(startup_cables(None, &named, &logger)), [3, 1]);
        assert_eq!(ids(startup_cables(Some(2), &named, &logger)), [1, 2], "--cables overrides the configuration");
        let no_cables = parse_config("listen = \"x\"\n").unwrap();
        assert_eq!(ids(startup_cables(None, &no_cables, &logger)), [0u32; 0], "no default cable");
        assert_eq!(ids(startup_cables(Some(40), &named, &logger)), [3, 1], "invalid --cables is ignored");
    }

    #[test]
    fn apply_changes_cables_without_restarting() {
        let manager = AppServer {
            inner: Mutex::new(AppInner { server: None, done: None, cables: Vec::new(), last_error: String::new() }),
            address: "127.0.0.1:0".into(),
            latency_ms: DEFAULT_LATENCY_MS,
            playback_completion: PlaybackCompletion::End,
            stall_test: None,
            diagnostics: false,
            logger: Logger::discard(),
            log_path: String::new(),
        };
        let cable = |id: u32| Cable { id, serial: format!("S{id}"), name: format!("C{id}") };
        let running = || manager.lock().server.clone().expect("server running");
        let exported = |server: &Server| {
            let list = server.device_list();
            u32::from_be_bytes([list[8], list[9], list[10], list[11]])
        };

        manager.apply(&[cable(1)]).unwrap(); // starts the server
        let server = running();
        manager.apply(&[cable(1), cable(2)]).unwrap();
        manager.apply(&[cable(2)]).unwrap();
        assert!(Arc::ptr_eq(&server, &running()), "the server was restarted");
        assert_eq!(exported(&server), 1);
        assert_eq!(manager.cables(), [cable(2)]);
        manager.stop();
    }

    #[test]
    fn devices_take_cable_ids_and_names() {
        let cables = [Cable { id: 7, serial: "8F3A2C1D".into(), name: "Game".into() }];
        let devices = make_devices(&cables, DEFAULT_LATENCY_MS).unwrap();
        assert_eq!((devices[0].bus_id.as_str(), devices[0].product()), ("1-7", "Game"));
        assert_eq!(devices[0].descriptors.serial, "8F3A2C1D");
        // The product ID follows the cable ID.
        assert_eq!(&devices[0].descriptors.device[10..12], &0xCA07u16.to_le_bytes());
    }

    #[test]
    fn platform_names() {
        assert_eq!(arch(), if cfg!(target_arch = "x86_64") { "amd64" } else { arch() });
    }
}
