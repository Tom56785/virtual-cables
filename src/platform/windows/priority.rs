//! Process scheduling for real-time audio streaming.

use std::ffi::c_void;

use windows_sys::Win32::Media::timeBeginPeriod;
use windows_sys::Win32::System::Threading::{
    ABOVE_NORMAL_PRIORITY_CLASS, GetCurrentProcess, PROCESS_POWER_THROTTLING_CURRENT_VERSION,
    PROCESS_POWER_THROTTLING_EXECUTION_SPEED, PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
    PROCESS_POWER_THROTTLING_STATE, ProcessPowerThrottling, SetPriorityClass,
};

use super::win::{last_error, system_proc};
use crate::logf;
use crate::logging::Logger;

type SetProcessInformationFn = unsafe extern "system" fn(*mut c_void, i32, *const c_void, u32) -> i32;
type NtQueryTimerResolutionFn = unsafe extern "system" fn(*mut u32, *mut u32, *mut u32) -> i32;

/// Prepares the process for real-time audio streaming.
///
/// - Above-normal priority keeps ordinary desktop load from delaying the URB
///   loops. Isochronous streams have no retry: a late completion is lost audio.
/// - Windows USB audio reads the bus frame number from the kernel interrupt
///   time, which only advances on each clock interrupt. A 1 ms timer resolution
///   makes it advance every frame, as a real host controller does.
/// - Windows 11 ignores timer-resolution requests from minimized or hidden
///   processes and may run them on efficiency cores (EcoQoS) unless they opt
///   out, which would undo both of the above when the window is minimized.
pub fn raise_process_priority(logger: &Logger) {
    // SAFETY: these calls take the current-process pseudo handle and plain
    // values or a pointer to a correctly sized local structure.
    unsafe {
        let process = GetCurrentProcess();
        if SetPriorityClass(process, ABOVE_NORMAL_PRIORITY_CLASS) == 0 {
            logf!(logger, "Could not raise process priority: {}", last_error());
        }

        // SetProcessInformation is looked up at run time, as it was missing
        // before Windows 8.
        if let Some(set_info) = system_proc::<SetProcessInformationFn>("kernel32.dll", c"SetProcessInformation") {
            let mut state = PROCESS_POWER_THROTTLING_STATE {
                Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
                ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED
                    | PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
                StateMask: 0, // controlled and disabled: never throttle
            };
            let set_throttling = |state: &PROCESS_POWER_THROTTLING_STATE| {
                let ok = set_info(
                    process,
                    ProcessPowerThrottling,
                    (state as *const PROCESS_POWER_THROTTLING_STATE).cast(),
                    size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
                );
                (ok, last_error())
            };
            let (mut ok, mut err) = set_throttling(&state);
            if ok == 0 {
                // The timer-resolution control is Windows 11 only; Windows 10 does
                // not throttle timer requests, so only opt out of EcoQoS there.
                state.ControlMask = PROCESS_POWER_THROTTLING_EXECUTION_SPEED;
                (ok, err) = set_throttling(&state);
            }
            if ok == 0 {
                logf!(logger, "Could not opt out of power throttling: {err}");
            }
        }

        let result = timeBeginPeriod(1);
        if result != 0 {
            logf!(logger, "Could not request a 1 ms timer resolution (error {result})");
        }
    }
    log_timer_resolution(logger);
}

/// Records the system clock-interrupt period, which is the granularity of the
/// USB frame number Windows audio sees.
fn log_timer_resolution(logger: &Logger) {
    let Some(query) = system_proc::<NtQueryTimerResolutionFn>("ntdll.dll", c"NtQueryTimerResolution") else {
        return;
    };
    let (mut coarsest, mut finest, mut current) = (0u32, 0u32, 0u32);
    // SAFETY: NtQueryTimerResolution writes one u32 through each pointer.
    if unsafe { query(&mut coarsest, &mut finest, &mut current) } != 0 {
        return;
    }
    logf!(
        logger,
        "System timer resolution: {:.3} ms (supported {:.3}-{:.3} ms)",
        current as f64 / 10000.0,
        finest as f64 / 10000.0,
        coarsest as f64 / 10000.0
    );
}
