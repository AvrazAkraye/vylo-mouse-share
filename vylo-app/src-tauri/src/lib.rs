mod commands;
mod daemon;
mod ipc;
mod prefs;
mod tray;

use tauri::{AppHandle, Manager, WindowEvent};

/// Argument the login item launches Vylo with, and what the "start in
/// background" preference passes on a manual launch.
pub(crate) const HIDDEN_FLAG: &str = "--hidden";

/// Whether this launch should go straight to the tray without a window:
/// either the login item started us, or the user asked for it.
///
/// Consumes the one-shot flag an update sets before relaunching, so the
/// window comes back exactly once after an update.
fn start_in_background(app: &AppHandle) -> bool {
    let prefs = prefs::load(app);
    if prefs.show_on_next_start {
        let cleared = prefs::Prefs {
            show_on_next_start: false,
            ..prefs
        };
        if let Err(e) = prefs::store(app, cleared) {
            log::warn!("could not clear the post-update window flag: {e}");
        }
        return false;
    }
    std::env::args().any(|arg| arg == HIDDEN_FLAG || arg == "--background")
        || prefs.start_hidden
}

/// Rewrite the login item so it carries the current arguments — installs
/// that enabled "start on login" before `--hidden` existed registered a
/// plain launch, which would pop the window open at every sign-in.
fn refresh_autostart(app: &AppHandle) {
    use tauri_plugin_autostart::ManagerExt as _;
    let autolaunch = app.autolaunch();
    if matches!(autolaunch.is_enabled(), Ok(true)) {
        if let Err(e) = autolaunch.enable() {
            log::warn!("could not refresh the login item: {e}");
        }
    }
}

pub fn run() {
    let app = tauri::Builder::default()
        // Must stay the first plugin. Vylo is a background service with a
        // tray icon: launching it again (start menu, dock, installer) has
        // to reveal the running instance, not start a second tray icon
        // beside a daemon that cannot bind its socket.
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            tray::show_window(app);
        }))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            // a login start never shows the window
            Some(vec![HIDDEN_FLAG]),
        ))
        // in-app updates: signed manifest + artifacts from GitHub Releases
        .plugin(tauri_plugin_updater::Builder::new().build())
        // needed to relaunch after an update is installed
        .plugin(tauri_plugin_process::init())
        .setup(|app| {
            // ============================================================
            // DAEMON EMBED POINT
            //
            // The embedded Vylo service will be spawned here, before the
            // IPC bridge starts connecting: `daemon::spawn_daemon()` runs
            // `Service::run()` on its own thread and falls back to
            // connect-only when a daemon is already running.
            // ============================================================
            daemon::spawn_daemon();

            // Outgoing-request channel must be in managed state before the
            // tray (which sends requests) and commands are wired up.
            let (ipc_handle, ipc_rx) = ipc::init();
            app.manage(ipc_handle);

            tray::setup(app.handle())?;

            // Connect-forever bridge to the daemon socket.
            ipc::spawn(app.handle().clone(), ipc_rx);

            // The window is configured invisible so a background start
            // never flashes it on screen; show it unless this launch is
            // meant to stay in the tray.
            if start_in_background(app.handle()) {
                log::info!("starting in the background");
                // no window, no dock icon on macOS
                #[cfg(target_os = "macos")]
                let _ = app
                    .handle()
                    .set_activation_policy(tauri::ActivationPolicy::Accessory);
            } else {
                tray::show_window(app.handle());
            }

            refresh_autostart(app.handle());

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::request,
            commands::ipc_connected,
            commands::pick_files,
            commands::pick_dir,
            commands::open_file_dir,
            commands::set_autostart,
            commands::get_autostart,
            commands::set_start_hidden,
            commands::get_start_hidden,
            commands::relaunch_for_update,
            commands::get_platform,
        ])
        .on_window_event(|window, event| match event {
            // Closing the window hides it; the app lives in the tray.
            WindowEvent::CloseRequested { api, .. } => {
                api.prevent_close();
                let _ = window.hide();
                // Without a visible window, drop out of the dock on macOS.
                #[cfg(target_os = "macos")]
                let _ = window
                    .app_handle()
                    .set_activation_policy(tauri::ActivationPolicy::Accessory);
            }
            // Windows has no "minimized" window event, but a minimize does
            // resize the window — send it to the tray instead of the
            // taskbar so there is one place Vylo lives while it runs.
            // (The service keeps running either way; see
            // `background::opt_out_of_throttling`.)
            #[cfg(windows)]
            WindowEvent::Resized(_) => {
                if window.is_minimized().unwrap_or(false) {
                    let _ = window.hide();
                }
            }
            _ => {}
        })
        .build(tauri::generate_context!())
        .expect("error while running vylo mouse share");

    app.run(|_app, _event| {
        // Opening an already-running app on macOS reactivates this process
        // rather than starting a second one, so a click in Finder, the
        // dock or Spotlight arrives here — not through the single-instance
        // plugin. Bring the window back, that is what the user asked for.
        #[cfg(target_os = "macos")]
        if let tauri::RunEvent::Reopen { .. } = _event {
            tray::show_window(_app);
        }
    });
}
