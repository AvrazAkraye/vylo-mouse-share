//! Keep the service working while the app has no window on screen.
//!
//! Vylo is a background service that happens to ship with a window: input
//! capture, the DTLS input channel and the clipboard/file channel must keep
//! running when the window is minimized, hidden in the tray, or never shown
//! at all (login start).
//!
//! Windows actively works against that. Since Windows 10 1709 the scheduler
//! puts processes whose windows are all minimized/hidden into EcoQoS
//! ("Efficiency mode"): their threads are demoted to the efficiency cores and
//! timers are coarsened. For most apps that is merely slower; for Vylo it is
//! fatal, because the low-level input hooks have to return within
//! `LowLevelHooksTimeout` (300 ms by default) or Windows silently unhooks
//! them — after which the machine stops capturing until the app is restarted.
//! So opt the whole process out of throttling.
//!
//! macOS has no equivalent switch that applies here (App Nap does not
//! suspend a process holding a CGEventTap, and the tap itself already
//! recovers from `kCGEventTapDisabledByTimeout`), so this is a no-op there.

/// Opt this process out of OS background throttling. Safe to call more than
/// once; failures are logged and otherwise ignored.
pub fn opt_out_of_throttling() {
    #[cfg(windows)]
    {
        use windows::Win32::System::Threading::{
            GetCurrentProcess, PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            PROCESS_POWER_THROTTLING_EXECUTION_SPEED, PROCESS_POWER_THROTTLING_STATE,
            ProcessPowerThrottling, SetProcessInformation,
        };

        let state = PROCESS_POWER_THROTTLING_STATE {
            Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
            // managed by us (ControlMask) + off (StateMask) == always run at
            // full speed, whatever the window state is
            StateMask: 0,
        };

        let res = unsafe {
            SetProcessInformation(
                GetCurrentProcess(),
                ProcessPowerThrottling,
                &state as *const PROCESS_POWER_THROTTLING_STATE as *const std::ffi::c_void,
                std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
            )
        };
        match res {
            Ok(()) => log::debug!("process opted out of background throttling"),
            // older builds may not know the policy - not fatal
            Err(e) => log::warn!("could not disable background throttling: {e}"),
        }
    }
}
