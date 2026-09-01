mod commands;
mod daemon;
mod ipc;
mod tray;

use tauri::{Manager, WindowEvent};

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
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
            commands::get_platform,
        ])
        .on_window_event(|window, event| {
            // Closing the window hides it; the app lives in the tray.
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
                // Without a visible window, drop out of the dock on macOS.
                #[cfg(target_os = "macos")]
                let _ = window
                    .app_handle()
                    .set_activation_policy(tauri::ActivationPolicy::Accessory);
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running vylo mouse share");
}
