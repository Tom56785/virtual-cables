//! The application log: timestamped lines written by a background thread.

use std::fmt;
use std::fs::File;
use std::io::{self, Write};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// A cheaply cloneable logger. Each call writes one line prefixed with the
/// local date and time to microseconds, as `2026/10/12 21:58:23.089746 text`.
#[derive(Clone)]
pub struct Logger {
    sink: Arc<dyn Sink>,
}

trait Sink: Send + Sync {
    fn write_line(&self, line: Vec<u8>);
}

/// Logs a formatted line: `logf!(logger, "format {}", args)`.
#[macro_export]
macro_rules! logf {
    ($logger:expr, $($arg:tt)*) => {
        $logger.print(format_args!($($arg)*))
    };
}

impl Logger {
    pub fn print(&self, args: fmt::Arguments) {
        let mut line = format!("{} {args}", timestamp());
        if !line.ends_with('\n') {
            line.push('\n');
        }
        self.sink.write_line(line.into_bytes());
    }

    /// A logger that discards everything.
    pub fn discard() -> Logger {
        Logger { sink: Arc::new(Discard) }
    }

    /// A logger that writes synchronously to standard error.
    pub fn stderr() -> Logger {
        Logger { sink: Arc::new(Stderr) }
    }

    /// A logger that collects lines in memory, and the buffer it fills.
    pub fn memory() -> (Logger, Arc<Mutex<Vec<u8>>>) {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        (Logger { sink: Arc::new(Memory(Arc::clone(&buffer))) }, buffer)
    }

    /// A logger that hands lines to `writer` on a background thread.
    pub fn asynchronous(writer: Arc<AsyncWriter>) -> Logger {
        Logger { sink: writer }
    }
}

/// The local date and time to microseconds: `2026/10/12 21:58:23.089746`.
#[cfg(windows)]
fn timestamp() -> String {
    use windows_sys::Win32::Foundation::{FILETIME, SYSTEMTIME};
    use windows_sys::Win32::System::SystemInformation::GetSystemTimePreciseAsFileTime;
    use windows_sys::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};

    // SAFETY: the calls write into the local structures passed to them.
    unsafe {
        let mut now: FILETIME = std::mem::zeroed();
        GetSystemTimePreciseAsFileTime(&mut now);
        let ticks = (now.dwHighDateTime as u64) << 32 | now.dwLowDateTime as u64;
        let micros = ticks / 10 % 1_000_000;
        let mut utc: SYSTEMTIME = std::mem::zeroed();
        let mut local: SYSTEMTIME = std::mem::zeroed();
        if FileTimeToSystemTime(&now, &mut utc) == 0
            || SystemTimeToTzSpecificLocalTime(std::ptr::null(), &utc, &mut local) == 0
        {
            local = utc;
        }
        format!(
            "{:04}/{:02}/{:02} {:02}:{:02}:{:02}.{micros:06}",
            local.wYear, local.wMonth, local.wDay, local.wHour, local.wMinute, local.wSecond
        )
    }
}

#[cfg(not(windows))]
fn timestamp() -> String {
    chrono::Local::now().format("%Y/%m/%d %H:%M:%S%.6f").to_string()
}

struct Discard;

impl Sink for Discard {
    fn write_line(&self, _: Vec<u8>) {}
}

struct Stderr;

impl Sink for Stderr {
    fn write_line(&self, line: Vec<u8>) {
        let _ = io::stderr().write_all(&line);
    }
}

struct Memory(Arc<Mutex<Vec<u8>>>);

impl Sink for Memory {
    fn write_line(&self, line: Vec<u8>) {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).extend_from_slice(&line);
    }
}

enum Message {
    Line(Vec<u8>),
    Close,
}

/// Hands log lines to a background thread. The USB/IP request loops log from
/// the audio path; a synchronous file write that stalls (for example while
/// antivirus scans the growing log) would stop every cable from receiving URBs
/// and break the isochronous streams. Lines are dropped rather than blocking
/// when the queue is full.
pub struct AsyncWriter {
    lines: SyncSender<Message>,
    pending: Arc<AtomicI64>,
}

impl AsyncWriter {
    /// Writes each line to `file` and then to standard output, as the log
    /// always has. Standard output is usually absent in the windowed app.
    pub fn new(file: File) -> Arc<AsyncWriter> {
        let (lines, receiver) = mpsc::sync_channel(1024);
        let pending = Arc::new(AtomicI64::new(0));
        let written = Arc::clone(&pending);
        std::thread::Builder::new()
            .name("log writer".into())
            .spawn(move || {
                let mut file = Some(file);
                for message in receiver {
                    match message {
                        Message::Line(line) => {
                            if let Some(file) = file.as_mut() {
                                // The file comes first: if it fails, stdout is not tried.
                                if file.write_all(&line).is_ok() {
                                    let _ = io::stdout().write_all(&line);
                                }
                            }
                            written.fetch_sub(1, Ordering::SeqCst);
                        }
                        Message::Close => file = None,
                    }
                }
            })
            .expect("start log writer thread");
        Arc::new(AsyncWriter { lines, pending })
    }

    /// Waits up to `timeout` for queued lines to be written.
    pub fn flush(&self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while self.pending.load(Ordering::SeqCst) > 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Flushes queued lines and closes the file. Later lines are discarded.
    pub fn close(&self) {
        self.flush(Duration::from_secs(1));
        let _ = self.lines.send(Message::Close);
    }
}

impl Sink for AsyncWriter {
    fn write_line(&self, line: Vec<u8>) {
        self.pending.fetch_add(1, Ordering::SeqCst);
        match self.lines.try_send(Message::Line(line)) {
            Ok(()) => {}
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
                self.pending.fetch_sub(1, Ordering::SeqCst);
            }
        }
    }
}

/// The asynchronous writer behind the application log, if any.
static APP_LOG_WRITER: OnceLock<Arc<AsyncWriter>> = OnceLock::new();

pub fn set_app_log_writer(writer: Arc<AsyncWriter>) {
    let _ = APP_LOG_WRITER.set(writer);
}

/// Writes out queued log lines before the process exits directly.
pub fn flush_log() {
    if let Some(writer) = APP_LOG_WRITER.get() {
        writer.flush(Duration::from_secs(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_have_log_timestamp_format() {
        let (logger, buffer) = Logger::memory();
        logf!(logger, "hello {}", 42);
        let text = String::from_utf8(buffer.lock().unwrap().clone()).unwrap();
        // 2026/10/12 21:58:23.089746 hello 42
        let (stamp, message) = text.split_at(26);
        assert_eq!(message, " hello 42\n");
        let bytes = stamp.as_bytes();
        for (i, b) in bytes.iter().enumerate() {
            match i {
                4 | 7 => assert_eq!(*b, b'/'),
                10 => assert_eq!(*b, b' '),
                13 | 16 => assert_eq!(*b, b':'),
                19 => assert_eq!(*b, b'.'),
                _ => assert!(b.is_ascii_digit(), "unexpected {:?} in {stamp:?}", *b as char),
            }
        }
    }
}
