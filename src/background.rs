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
//! macOS does the same through App Nap: once the window is hidden or
//! minimized, the process's timers are coalesced and its threads
//! deprioritized, so the input channel and the injected pointer lag or
//! stall while the peer is driving this Mac. The event tap recovers from
//! `kCGEventTapDisabledByTimeout`, but nothing recovers the latency. An
//! `NSProcessInfo` activity held for the life of the process keeps App Nap
//! off (idle system sleep is still allowed).

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

    #[cfg(target_os = "macos")]
    {
        use objc2_foundation::{NSActivityOptions, NSProcessInfo, NSString};
        use std::sync::Once;

        static APP_NAP_OFF: Once = Once::new();
        APP_NAP_OFF.call_once(|| {
            let activity = NSProcessInfo::processInfo().beginActivityWithOptions_reason(
                NSActivityOptions::UserInitiatedAllowingIdleSystemSleep
                    | NSActivityOptions::LatencyCritical,
                &NSString::from_str("Sharing mouse, keyboard and clipboard"),
            );
            // the activity lasts as long as this token lives: for the whole
            // process, so it is never ended
            std::mem::forget(activity);
            log::debug!("App Nap disabled");
        });
    }
}
