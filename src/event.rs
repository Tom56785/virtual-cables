//! An auto-reset event with precise, interruptible timed waits.
//!
//! Isochronous completions are timed to the 1 ms USB frame. Condition-variable
//! timeouts on Windows only have millisecond granularity, so the Windows
//! implementation waits on a high-resolution waitable timer together with an
//! event that cancellation and new work signal.

pub use imp::Event;

#[cfg(windows)]
mod imp {
    use std::ptr::null;
    use std::time::Instant;

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Threading::{
        CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CancelWaitableTimer, CreateEventW, CreateWaitableTimerExW, INFINITE,
        SetEvent, SetWaitableTimer, TIMER_ALL_ACCESS, WaitForMultipleObjects, WaitForSingleObject,
    };

    /// An auto-reset event. Only one thread may wait on it at a time; any
    /// thread may signal it.
    pub struct Event {
        event: HANDLE,
        timer: HANDLE,
    }

    // SAFETY: Windows event and timer handles may be used from any thread.
    unsafe impl Send for Event {}
    unsafe impl Sync for Event {}

    impl Event {
        pub fn new() -> Event {
            // SAFETY: plain object creation with default security and no name.
            unsafe {
                let event = CreateEventW(null(), 0, 0, null());
                assert!(!event.is_null(), "CreateEventW failed");
                let mut timer =
                    CreateWaitableTimerExW(null(), null(), CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, TIMER_ALL_ACCESS);
                if timer.is_null() {
                    // High-resolution timers need Windows 10 1803 or later.
                    timer = CreateWaitableTimerExW(null(), null(), 0, TIMER_ALL_ACCESS);
                }
                assert!(!timer.is_null(), "CreateWaitableTimerExW failed");
                Event { event, timer }
            }
        }

        pub fn signal(&self) {
            // SAFETY: the handle is valid for the lifetime of self.
            unsafe { SetEvent(self.event) };
        }

        /// Waits until the event is signalled or `deadline` passes, and
        /// reports whether it was signalled. `None` waits without a deadline.
        pub fn wait(&self, deadline: Option<Instant>) -> bool {
            // SAFETY: the handles are valid for the lifetime of self, and only
            // the single waiting thread arms the timer.
            unsafe {
                let Some(deadline) = deadline else {
                    return WaitForSingleObject(self.event, INFINITE) == WAIT_OBJECT_0;
                };
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return WaitForSingleObject(self.event, 0) == WAIT_OBJECT_0;
                }
                // A negative due time is relative, in 100 ns units.
                let due = -(remaining.as_nanos().div_ceil(100).min(i64::MAX as u128) as i64);
                if SetWaitableTimer(self.timer, &due, 0, None, null(), 0) == 0 {
                    // Fall back to the coarse wait rather than not waiting at all.
                    let ms = remaining.as_millis().max(1).min(u32::MAX as u128 - 1) as u32;
                    return WaitForSingleObject(self.event, ms) == WAIT_OBJECT_0;
                }
                let handles = [self.event, self.timer];
                let result = WaitForMultipleObjects(2, handles.as_ptr(), 0, INFINITE);
                if result == WAIT_OBJECT_0 {
                    CancelWaitableTimer(self.timer);
                    return true;
                }
                false
            }
        }
    }

    impl Drop for Event {
        fn drop(&mut self) {
            // SAFETY: the handles were created by Event::new and are closed once.
            unsafe {
                CloseHandle(self.timer);
                CloseHandle(self.event);
            }
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use std::sync::{Condvar, Mutex};
    use std::time::Instant;

    /// An auto-reset event. Only one thread may wait on it at a time; any
    /// thread may signal it.
    pub struct Event {
        signalled: Mutex<bool>,
        changed: Condvar,
    }

    impl Event {
        pub fn new() -> Event {
            Event { signalled: Mutex::new(false), changed: Condvar::new() }
        }

        pub fn signal(&self) {
            *self.signalled.lock().unwrap_or_else(|p| p.into_inner()) = true;
            self.changed.notify_one();
        }

        /// Waits until the event is signalled or `deadline` passes, and
        /// reports whether it was signalled. `None` waits without a deadline.
        pub fn wait(&self, deadline: Option<Instant>) -> bool {
            let mut signalled = self.signalled.lock().unwrap_or_else(|p| p.into_inner());
            loop {
                if *signalled {
                    *signalled = false;
                    return true;
                }
                match deadline {
                    None => signalled = self.changed.wait(signalled).unwrap_or_else(|p| p.into_inner()),
                    Some(deadline) => {
                        let remaining = deadline.saturating_duration_since(Instant::now());
                        if remaining.is_zero() {
                            return false;
                        }
                        signalled =
                            self.changed.wait_timeout(signalled, remaining).unwrap_or_else(|p| p.into_inner()).0;
                    }
                }
            }
        }
    }
}

impl Default for Event {
    fn default() -> Event {
        Event::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    #[test]
    fn times_out_at_deadline() {
        let event = Event::new();
        let start = Instant::now();
        assert!(!event.wait(Some(start + Duration::from_millis(20))));
        let elapsed = start.elapsed();
        assert!(elapsed >= Duration::from_millis(20), "returned early after {elapsed:?}");
        assert!(elapsed < Duration::from_millis(200), "returned late after {elapsed:?}");
    }

    #[test]
    fn signal_wakes_waiter_and_auto_resets() {
        let event = Arc::new(Event::new());
        let signaller = Arc::clone(&event);
        let thread = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(10));
            signaller.signal();
        });
        assert!(event.wait(Some(Instant::now() + Duration::from_secs(5))));
        thread.join().unwrap();
        // The signal was consumed by the first wait.
        assert!(!event.wait(Some(Instant::now() + Duration::from_millis(5))));
    }

    #[test]
    fn signal_before_wait_is_kept() {
        let event = Event::new();
        event.signal();
        assert!(event.wait(None));
    }

    #[cfg(windows)]
    #[test]
    fn short_waits_are_fine_grained() {
        // A 500 us wait must take about one millisecond,
        // not 2 ms or a 15.6 ms default clock tick. The median is checked, as
        // other tests running in parallel can delay single wakeups.
        let event = Event::new();
        let mut waits: Vec<Duration> = (0..21)
            .map(|_| {
                let start = Instant::now();
                event.wait(Some(start + Duration::from_micros(500)));
                start.elapsed()
            })
            .collect();
        waits.sort();
        let median = waits[waits.len() / 2];
        assert!(median >= Duration::from_micros(500), "returned early: {median:?}");
        assert!(median < Duration::from_micros(1500), "median 500 us wait took {median:?}");
    }
}
