//! The host's USB bus clock, as Windows USB audio reads it.

use std::time::{Duration, Instant};

use crate::event::Event;

/// The number of 100 ns bus-clock ticks in one full-speed USB frame (1 ms).
pub const TICKS_PER_FRAME: u64 = 10_000;

/// A sample of the host's USB bus clock together with the monotonic clock.
///
/// Windows USB audio schedules isochronous streams against the bus frame
/// number (QueryBusTime). usbip-win2 implements it as KeQueryInterruptTime() /
/// 10000, and KeQueryInterruptTime only advances on each system clock
/// interrupt: every 0.5-15.6 ms depending on the current timer resolution. The
/// device must use that same coarse value for frame numbers, or it reports
/// transfers on frames usbaudio believes have not happened yet, and usbaudio
/// resets the pipe.
///
/// `coarse` is that interrupt-time value; `precise` is the exact interrupt
/// time, used only to convert frame boundaries to wall-clock deadlines.
#[derive(Clone, Copy, Debug)]
pub struct BusTime {
    pub wall: Instant,
    pub coarse: u64,
    pub precise: u64,
}

pub fn current_bus_time() -> BusTime {
    let (coarse, precise) = host_bus_ticks();
    BusTime { wall: Instant::now(), coarse, precise }
}

impl BusTime {
    /// The current full-speed frame number, as usbaudio sees it.
    pub fn frame(&self) -> u64 {
        self.coarse / TICKS_PER_FRAME
    }

    /// Converts the start of a bus frame to an instant. The coarse clock may
    /// still show an earlier frame then; see `wait_bus_frame`.
    pub fn frame_start(&self, frame: u64) -> Instant {
        let delta = (frame * TICKS_PER_FRAME) as i64 - self.precise as i64;
        let offset = Duration::from_nanos(delta.unsigned_abs() * 100);
        if delta >= 0 { self.wall + offset } else { self.wall.checked_sub(offset).unwrap_or(self.wall) }
    }
}

/// Returns once the coarse bus clock reaches `frame`, so that a completion is
/// never reported before usbaudio can observe its last frame as elapsed.
/// It returns `Err` if `cancelled` reports true while waiting.
pub fn wait_bus_frame(event: &Event, frame: u64, cancelled: &dyn Fn() -> bool) -> Result<(), Cancelled> {
    loop {
        if host_bus_ticks().0 / TICKS_PER_FRAME >= frame {
            return Ok(());
        }
        if cancelled() {
            return Err(Cancelled);
        }
        event.wait(Some(Instant::now() + Duration::from_micros(500)));
    }
}

/// The wait was cancelled before it completed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cancelled;

#[cfg(windows)]
mod host {
    use std::sync::OnceLock;

    use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

    type QueryInterruptTimeFn = unsafe extern "system" fn(*mut u64);

    struct Functions {
        coarse: QueryInterruptTimeFn,
        precise: QueryInterruptTimeFn,
    }

    /// QueryInterruptTime returns the same tick-granular value as the kernel's
    /// KeQueryInterruptTime, which usbip-win2 reports as the USB frame number.
    /// QueryInterruptTimePrecise returns the exact interrupt time.
    fn functions() -> Option<&'static Functions> {
        static FUNCTIONS: OnceLock<Option<Functions>> = OnceLock::new();
        FUNCTIONS
            .get_or_init(|| {
                for dll in ["kernel32.dll", "api-ms-win-core-realtime-l1-1-1.dll"] {
                    let name: Vec<u16> = dll.encode_utf16().chain([0]).collect();
                    // SAFETY: loading a system DLL by name and looking up exports;
                    // the signatures match the documented functions.
                    unsafe {
                        let module = LoadLibraryW(name.as_ptr());
                        if module.is_null() {
                            continue;
                        }
                        let coarse = GetProcAddress(module, c"QueryInterruptTime".as_ptr().cast());
                        let precise = GetProcAddress(module, c"QueryInterruptTimePrecise".as_ptr().cast());
                        if let (Some(coarse), Some(precise)) = (coarse, precise) {
                            return Some(Functions {
                                coarse: std::mem::transmute::<unsafe extern "system" fn() -> isize, QueryInterruptTimeFn>(coarse),
                                precise: std::mem::transmute::<unsafe extern "system" fn() -> isize, QueryInterruptTimeFn>(precise),
                            });
                        }
                    }
                }
                None
            })
            .as_ref()
    }

    #[cfg(all(test, windows))]
    pub fn available() -> bool {
        functions().is_some()
    }

    pub fn host_bus_ticks() -> (u64, u64) {
        let Some(f) = functions() else {
            return super::fallback_ticks();
        };
        let (mut coarse, mut precise) = (0u64, 0u64);
        // SAFETY: both functions write one u64 through the pointer.
        unsafe {
            // Read precise first so coarse can never appear to be ahead of it.
            (f.precise)(&mut precise);
            (f.coarse)(&mut coarse);
        }
        (coarse.min(precise), precise)
    }
}

#[cfg(not(windows))]
mod host {
    #[cfg(all(test, windows))]
    pub fn available() -> bool {
        false
    }

    /// Uses the monotonic clock where no host bus clock is available.
    pub fn host_bus_ticks() -> (u64, u64) {
        super::fallback_ticks()
    }
}

fn fallback_ticks() -> (u64, u64) {
    use std::sync::OnceLock;
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    let ticks = (EPOCH.get_or_init(Instant::now).elapsed().as_nanos() / 100) as u64;
    (ticks, ticks)
}

/// Whether the host's interrupt-time clock is available.
#[cfg(all(test, windows))]
pub fn host_clock_available() -> bool {
    host::available()
}

/// The coarse and precise host bus clock, in 100 ns ticks.
pub fn host_bus_ticks() -> (u64, u64) {
    host::host_bus_ticks()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn host_bus_clock_uses_interrupt_time() {
        assert!(host_clock_available(), "QueryInterruptTime is unavailable");
        let start = current_bus_time();
        std::thread::sleep(Duration::from_millis(50));
        let end = current_bus_time();
        let ticks = Duration::from_nanos((end.precise - start.precise) * 100);
        let wall = end.wall - start.wall;
        let diff = ticks.as_secs_f64() - wall.as_secs_f64();
        assert!(diff.abs() < 0.002, "bus clock advanced {ticks:?} while the clock advanced {wall:?}");
        // The coarse clock trails the precise one by at most one timer tick.
        let lag = Duration::from_nanos((end.precise - end.coarse) * 100);
        assert!(lag <= Duration::from_millis(16), "coarse interrupt time lags by {lag:?}");
    }

    #[test]
    fn wait_bus_frame_reaches_coarse_frame() {
        let target = current_bus_time().frame() + 5;
        wait_bus_frame(&Event::new(), target, &|| false).unwrap();
        assert!(current_bus_time().frame() >= target);
    }

    #[test]
    fn frame_start_converts_frames_to_instants() {
        let now = BusTime { wall: Instant::now(), coarse: 1000 * TICKS_PER_FRAME, precise: 1000 * TICKS_PER_FRAME };
        assert_eq!(now.frame_start(1011) - now.wall, Duration::from_millis(11));
        assert_eq!(now.wall - now.frame_start(990), Duration::from_millis(10));
    }
}
