//! Attaches the cables to Windows with usbip-win2's usbip.exe.

use std::env;
use std::io::Read;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::logf;
use crate::logging::Logger;
use crate::uac2::{PRODUCT_ID_BASE, bus_id};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub fn find_usbip_executable() -> Option<PathBuf> {
    if let Some(path) = env::var_os("PATH") {
        for dir in env::split_paths(&path) {
            let candidate = dir.join("usbip.exe");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    let program_files = env::var_os("ProgramFiles").map(PathBuf::from).unwrap_or_default();
    let mut candidates = vec![
        program_files.join("USBip").join("usbip.exe"),
        program_files.join("usbip-win2").join("usbip.exe"),
        program_files.join("usbip").join("usbip.exe"),
    ];
    if let Some(x86) = env::var_os("ProgramFiles(x86)").filter(|v| !v.is_empty()) {
        let x86 = PathBuf::from(x86);
        candidates.push(x86.join("USBip").join("usbip.exe"));
        candidates.push(x86.join("usbip-win2").join("usbip.exe"));
    }
    candidates.into_iter().find(|candidate| candidate.is_file())
}

/// Hides the console window of a child process.
pub fn hide_command_window(command: &mut Command) {
    command.creation_flags(CREATE_NO_WINDOW);
}

/// Runs a command and returns its combined standard output and error, as text,
/// and whether it succeeded (`Err` holds the exit description).
pub fn combined_output(command: &mut Command) -> (String, Result<(), String>) {
    let (mut reader, writer) = match std::io::pipe() {
        Ok(pipe) => pipe,
        Err(err) => return (String::new(), Err(err.to_string())),
    };
    let stderr = match writer.try_clone() {
        Ok(stderr) => stderr,
        Err(err) => return (String::new(), Err(err.to_string())),
    };
    command.stdin(Stdio::null()).stdout(writer).stderr(stderr);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => return (String::new(), Err(err.to_string())),
    };
    // The command's copies of the pipe's write end must close before the read
    // can reach end of file.
    command.stdout(Stdio::null()).stderr(Stdio::null());
    let mut output = Vec::new();
    let _ = reader.read_to_end(&mut output);
    let status = child.wait();
    let text = String::from_utf8_lossy(&output).trim().to_string();
    let result = match status {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(match status.code() {
            Some(code) => format!("exit status {code}"),
            None => status.to_string(),
        }),
        Err(err) => Err(err.to_string()),
    };
    (text, result)
}

/// Joins multi-line output into one log line.
pub fn single_line(text: &str) -> String {
    text.replace("\r\n", " | ").replace('\n', " | ")
}

fn run_usbip(executable: &Path, logger: &Logger, arguments: &[&str]) -> Result<String, String> {
    logf!(logger, "usbip.exe {}", arguments.join(" "));
    let mut command = Command::new(executable);
    command.args(arguments);
    hide_command_window(&mut command);
    let (text, result) = combined_output(&mut command);
    if !text.is_empty() {
        logf!(logger, "usbip: {}", single_line(&text));
    }
    match result {
        Ok(()) => Ok(text),
        Err(err) => Err(format!("usbip {}: {err}", arguments.join(" "))),
    }
}

/// Reports the port number of a `usbip port` line such as "Port 01: <...>".
fn port_number(line: &str) -> Option<&str> {
    let rest = line.get(..4).filter(|p| p.eq_ignore_ascii_case("port")).map(|_| &line[4..])?;
    let digits = rest.trim_start();
    if digits.len() == rest.len() {
        return None; // at least one space after "Port"
    }
    let end = digits.find(|c: char| !c.is_ascii_digit()).unwrap_or(digits.len());
    if end == 0 || !digits[end..].starts_with(':') {
        return None;
    }
    Some(&digits[..end])
}

/// Where usbip.exe reaches this application's USB/IP server: the host and
/// port of the LISTEN address, with a wildcard host replaced by loopback.
#[derive(Debug, PartialEq)]
pub struct ServerTarget {
    host: String,
    port: u16,
}

impl ServerTarget {
    /// Parses a LISTEN address such as 127.0.0.1:3240
    /// The host is limited to the characters of host names and
    /// IP addresses, since the elevated broker passes it to usbip.exe.
    pub fn from_listen(listen: &str) -> Result<ServerTarget, String> {
        let invalid = || format!("invalid USB/IP listen address {listen:?}");
        let (host, port) = listen.trim().rsplit_once(':').ok_or_else(invalid)?;
        let port: u16 = port.parse().map_err(|_| invalid())?;
        let host = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(host);
        if !host.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':')) {
            return Err(invalid());
        }
        let host = match host {
            "" | "0.0.0.0" => "127.0.0.1",
            "::" => "::1",
            host => host,
        };
        Ok(ServerTarget { host: host.to_string(), port })
    }

    /// The start of the URLs `usbip port` shows for this server's devices.
    fn url_prefix(&self) -> String {
        if self.host.contains(':') {
            format!("usbip://[{}]:{}/", self.host, self.port)
        } else {
            format!("usbip://{}:{}/", self.host, self.port)
        }
    }

    /// Arguments for a usbip.exe subcommand that talks to this server.
    fn command<'a>(&'a self, port: &'a str, subcommand: &[&'a str]) -> Vec<&'a str> {
        let mut arguments = vec!["--tcp-port", port, subcommand[0], "-r", self.host.as_str()];
        arguments.extend_from_slice(&subcommand[1..]);
        arguments
    }
}

/// Returns the ports `usbip port` lists for this server's devices, with the
/// bus ID attached to each.
fn own_ports(output: &str, target: &ServerTarget) -> Vec<(String, String)> {
    let prefix = target.url_prefix();
    let mut current_port: Option<String> = None;
    let mut ports = Vec::new();
    for line in output.split('\n') {
        let trimmed = line.trim();
        if let Some(port) = port_number(trimmed) {
            current_port = Some(port.to_string());
            continue;
        }
        if let Some(start) = trimmed.find(&prefix)
            && let Some(port) = current_port.take()
        {
            let bus_id = trimmed[start + prefix.len()..].split_whitespace().next().unwrap_or("");
            ports.push((port, bus_id.to_string()));
        }
    }
    ports
}

/// What a synchronization changes.
#[derive(Debug, Default, PartialEq)]
struct SyncPlan {
    /// The ports to detach.
    detach: Vec<String>,
    /// The bus IDs to stop usbip-win2 reattaching by itself, because their
    /// cables are gone.
    stop: Vec<String>,
    /// The bus IDs to attach.
    attach: Vec<String>,
}

/// Plans a synchronization. A full one replaces every attachment; otherwise
/// cables that are already attached stay as they are, and only removed ones
/// (or a second port for the same cable) are detached and only new ones
/// attached.
fn plan_sync(attached: &[(String, String)], ids: &[u32], full: bool) -> SyncPlan {
    let wanted: Vec<String> = ids.iter().map(|id| bus_id(*id)).collect();
    let mut plan = SyncPlan::default();
    let mut kept: Vec<&str> = Vec::new();
    for (port, bus) in attached {
        if !full && wanted.contains(bus) && !kept.contains(&bus.as_str()) {
            kept.push(bus);
            continue;
        }
        plan.detach.push(port.clone());
        if !wanted.contains(bus) && !plan.stop.contains(bus) {
            plan.stop.push(bus.clone());
        }
    }
    plan.attach = wanted.into_iter().filter(|bus| !kept.contains(&bus.as_str())).collect();
    plan
}

/// Attaches exactly the cables with these IDs (bus IDs 1-<id>) to Windows and
/// detaches this server's other devices. With `full`, every cable is detached
/// and attached again, which clears attachments left from an earlier run;
/// otherwise attached cables are left alone, so they keep playing.
pub fn run_attach_helper(ids: &[u32], full: bool, target: &ServerTarget, logger: &Logger) -> Result<(), String> {
    let count = ids.len();
    let Some(executable) = find_usbip_executable() else {
        return Err("usbip.exe was not found; install the driver from the main window".into());
    };
    let kind = if full { "full" } else { "incremental" };
    logf!(logger, "Elevated Windows device synchronization ({kind}) started for {count} cable(s)");
    logf!(logger, "usbip.exe: {}", executable.display());
    let port = target.port.to_string();

    let attached = match run_usbip(&executable, logger, &["port"]) {
        Ok(output) => own_ports(&output, target),
        Err(err) => {
            logf!(logger, "Could not inspect existing USB/IP ports: {err}");
            Vec::new()
        }
    };
    let plan = plan_sync(&attached, ids, full);
    for port in &plan.detach {
        if let Err(err) = run_usbip(&executable, logger, &["detach", "-p", port]) {
            logf!(logger, "Could not detach Virtual Cables port {port}: {err}");
        }
    }
    for bus_id in &plan.stop {
        if let Err(err) = run_usbip(&executable, logger, &target.command(&port, &["attach", "-b", bus_id, "--stop"])) {
            logf!(logger, "Could not stop reattaching {bus_id}: {err}");
        }
    }
    for bus_id in &plan.attach {
        // Low-latency receive completes each URB from usbip-win2's network
        // event callback instead of a per-device receive thread, so replies
        // reach Windows without a thread switch. Audio sends small URBs at a
        // high rate, which is what that mode is for.
        let attach = ["attach", "-b", bus_id, "--receive-mode", "low-latency"];
        if let Err(err) = run_usbip(&executable, logger, &target.command(&port, &attach)) {
            return Err(format!("attach {bus_id} failed: {err}"));
        }
    }
    logf!(logger, "Windows device synchronization completed for {count} cable(s)");
    Ok(())
}

/// The Windows instance ID of a cable's USB device, which its USB vendor ID,
/// product ID and serial number make up.
pub fn cable_instance_id(id: u32, serial: &str) -> String {
    format!("USB\\VID_FFFF&PID_{:04X}\\{serial}", PRODUCT_ID_BASE + id)
}

/// Removes a removed cable's devices from Windows: its USB device and, with
/// it, the audio device and Windows Sound endpoints beneath it. The cable must
/// already be detached.
pub fn remove_cable_device(id: u32, serial: &str, logger: &Logger) -> Result<(), String> {
    let instance = cable_instance_id(id, serial);
    let pnputil = PathBuf::from(env::var("SystemRoot").unwrap_or_default()).join("System32").join("pnputil.exe");
    logf!(logger, "pnputil.exe /remove-device {instance} /subtree");
    let mut command = Command::new(&pnputil);
    command.args(["/remove-device", &instance, "/subtree"]);
    hide_command_window(&mut command);
    let (text, result) = combined_output(&mut command);
    if !text.is_empty() {
        logf!(logger, "pnputil: {}", single_line(&text));
    }
    result.map_err(|err| format!("pnputil /remove-device {instance}: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_own_ports_in_usbip_port_output() {
        let output = "Imported USB devices\r\n====================\r\nPort 01: <Port in Use> at Full Speed(12Mbps)\r\n       unknown vendor : unknown product (ffff:ca01)\r\n       ?-? -> usbip://127.0.0.1:3240/1-1\r\nport 02: <Port in Use> at Full Speed(12Mbps)\r\n       ?-? -> usbip://192.168.1.5:3240/1-1\r\nPORT   3: <Port in Use>\r\n       ?-? -> usbip://127.0.0.1:3240/1-2\r\nPort 04: <Port in Use>\r\n       ?-? -> usbip://127.0.0.1:4000/1-3\r\n";
        let port = |port: &str, bus: &str| (port.to_string(), bus.to_string());
        let default = ServerTarget::from_listen("127.0.0.1:3240").unwrap();
        assert_eq!(own_ports(output, &default), [port("01", "1-1"), port("3", "1-2")]);
        let other_port = ServerTarget::from_listen("0.0.0.0:4000").unwrap();
        assert_eq!(own_ports(output, &other_port), [port("04", "1-3")]);
    }

    #[test]
    fn server_targets_follow_the_listen_address() {
        let target = |listen: &str| ServerTarget::from_listen(listen).map(|t| (t.host, t.port));
        assert_eq!(target("127.0.0.1:3240"), Ok(("127.0.0.1".into(), 3240)));
        assert_eq!(target("0.0.0.0:4000"), Ok(("127.0.0.1".into(), 4000)), "wildcard means loopback");
        assert_eq!(target(":5000"), Ok(("127.0.0.1".into(), 5000)));
        assert_eq!(target("[::]:3240"), Ok(("::1".into(), 3240)));
        assert_eq!(target("[::1]:3240"), Ok(("::1".into(), 3240)));
        assert_eq!(target("my-pc.local:3240"), Ok(("my-pc.local".into(), 3240)));
        for bad in ["127.0.0.1", "127.0.0.1:x", "127.0.0.1:70000", "a b:3240", "a\"b:3240", "-x&y:3240"] {
            assert!(ServerTarget::from_listen(bad).is_err(), "{bad:?}");
        }
        assert_eq!(ServerTarget::from_listen("[::1]:3240").unwrap().url_prefix(), "usbip://[::1]:3240/");
        let target = ServerTarget::from_listen("0.0.0.0:4000").unwrap();
        assert_eq!(
            target.command("4000", &["attach", "-b", "1-2"]),
            ["--tcp-port", "4000", "attach", "-r", "127.0.0.1", "-b", "1-2"]
        );
    }

    #[test]
    fn sync_plans_change_only_what_differs() {
        let port = |port: &str, bus: &str| (port.to_string(), bus.to_string());
        let strings = |items: &[&str]| items.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let plan = |detach: &[&str], stop: &[&str], attach: &[&str]| SyncPlan {
            detach: strings(detach),
            stop: strings(stop),
            attach: strings(attach),
        };
        let attached = [port("1", "1-1"), port("2", "1-2"), port("3", "1-1")];

        // Cable 2 removed, cable 4 added; 1 stays, and its second port goes.
        assert_eq!(plan_sync(&attached, &[1, 4], false), plan(&["2", "3"], &["1-2"], &["1-4"]));
        // Nothing changed: nothing to do.
        assert_eq!(plan_sync(&attached[..2], &[1, 2], false), plan(&[], &[], &[]));
        // A full synchronization replaces everything.
        assert_eq!(plan_sync(&attached, &[1, 4], true), plan(&["1", "2", "3"], &["1-2"], &["1-1", "1-4"]));
        // No cables: everything is detached and stays detached.
        assert_eq!(plan_sync(&attached, &[], false), plan(&["1", "2", "3"], &["1-1", "1-2"], &[]));
    }

    #[test]
    fn cable_instance_ids() {
        assert_eq!(cable_instance_id(1, "8F3A2C1D"), "USB\\VID_FFFF&PID_CB01\\8F3A2C1D");
        assert_eq!(cable_instance_id(255, "VCABLE-255"), "USB\\VID_FFFF&PID_CBFF\\VCABLE-255");
    }

    #[test]
    fn port_line_syntax() {
        assert_eq!(port_number("Port 01: x"), Some("01"));
        assert_eq!(port_number("port\t7:"), Some("7"));
        assert_eq!(port_number("Port01: x"), None);
        assert_eq!(port_number("Port 01 x"), None);
        assert_eq!(port_number("Portal 1:"), None);
    }

    #[test]
    fn combined_output_captures_both_streams() {
        let mut command = Command::new("cmd");
        command.args(["/C", "echo out & echo err 1>&2 & exit 3"]);
        hide_command_window(&mut command);
        let (text, result) = combined_output(&mut command);
        assert!(text.contains("out") && text.contains("err"), "{text:?}");
        assert_eq!(result, Err("exit status 3".into()));
    }
}
