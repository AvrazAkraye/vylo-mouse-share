//! System tray: status line, show window, clipboard-sync toggle,
//! open received-files folder, quit.

use std::sync::Mutex;

use tauri::{
    AppHandle, Manager, Wry,
    menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem},
    tray::TrayIconBuilder,
};
use tauri_plugin_opener::OpenerExt;

pub struct TrayState {
    status: MenuItem<Wry>,
    clipboard: CheckMenuItem<Wry>,
    info: Mutex<TrayInfo>,
}

#[derive(Default)]
struct TrayInfo {
    ipc_up: bool,
    sync_connected: bool,
    peer_name: Option<String>,
    file_dir: Option<String>,
}

pub fn setup(app: &AppHandle) -> tauri::Result<()> {
    let status = MenuItem::with_id(app, "status", "Starting service…", false, None::<&str>)?;
    let show = MenuItem::with_id(app, "show", "Show Vylo", true, None::<&str>)?;
    let clipboard =
        CheckMenuItem::with_id(app, "clipboard", "Clipboard Sync", true, true, None::<&str>)?;
    let open_files =
        MenuItem::with_id(app, "open_files", "Open Received Files", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let separator = PredefinedMenuItem::separator(app)?;

    let menu = Menu::with_items(
        app,
        &[&status, &show, &clipboard, &open_files, &separator, &quit],
    )?;

    app.manage(TrayState {
        status: status.clone(),
        clipboard: clipboard.clone(),
        info: Mutex::new(TrayInfo::default()),
    });

    // Monochrome template icon (white "V" on transparent) so macOS adapts it
    // to the menu-bar theme.
    let icon = tauri::image::Image::from_bytes(include_bytes!("../icons/tray.png"))?;

    let builder = TrayIconBuilder::with_id("vylo-tray")
        .icon(icon)
        .tooltip("Vylo")
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(on_menu_event);

    #[cfg(target_os = "macos")]
    let builder = builder.icon_as_template(true);

    builder.build(app)?;
    Ok(())
}

fn on_menu_event(app: &AppHandle, event: MenuEvent) {
    match event.id().as_ref() {
        "show" => show_window(app),
        "clipboard" => {
            let state = app.state::<TrayState>();
            // muda toggles the check state before the event is delivered.
            let checked = state.clipboard.is_checked().unwrap_or(true);
            if let Some(ipc) = app.try_state::<crate::ipc::IpcHandle>() {
                let _ = ipc.send(format!("{{\"SetClipboardSync\":{checked}}}"));
            }
        }
        "open_files" => {
            let dir = {
                let state = app.state::<TrayState>();
                let info = state.info.lock().unwrap();
                info.file_dir.clone()
            }
            .unwrap_or_else(default_file_dir);
            let _ = std::fs::create_dir_all(&dir);
            let _ = app.opener().open_path(dir, None::<&str>);
        }
        "quit" => app.exit(0),
        _ => {}
    }
}

/// Show + focus the main window (and restore the dock icon on macOS).
pub fn show_window(app: &AppHandle) {
    #[cfg(target_os = "macos")]
    let _ = app.set_activation_policy(tauri::ActivationPolicy::Regular);
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

fn default_file_dir() -> String {
    #[cfg(windows)]
    let home = std::env::var("USERPROFILE").unwrap_or_default();
    #[cfg(not(windows))]
    let home = std::env::var("HOME").unwrap_or_default();
    std::path::Path::new(&home)
        .join("Downloads")
        .join("VyloShare")
        .to_string_lossy()
        .into_owned()
}

/// Called by the IPC bridge on connect / disconnect.
pub fn set_ipc_connected(app: &AppHandle, connected: bool) {
    if let Some(state) = app.try_state::<TrayState>() {
        state.info.lock().unwrap().ipc_up = connected;
        refresh_status(app);
    }
}

/// Peek at daemon event lines to keep the tray in sync
/// (clipboard toggle state, received-files dir, connection status line).
pub fn observe_line(app: &AppHandle, line: &str) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
        return;
    };
    let Some(obj) = value.as_object() else { return };
    let Some(state) = app.try_state::<TrayState>() else {
        return;
    };

    if let Some(vylo) = obj.get("VyloState") {
        let clipboard_sync = vylo
            .get("clipboard_sync")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        if let Some(dir) = vylo.get("file_dir").and_then(|v| v.as_str()) {
            state.info.lock().unwrap().file_dir = Some(dir.to_string());
        }
        let clipboard = state.clipboard.clone();
        let _ = app.run_on_main_thread(move || {
            let _ = clipboard.set_checked(clipboard_sync);
        });
    } else if let Some(sync) = obj.get("SyncStatus") {
        {
            let mut info = state.info.lock().unwrap();
            info.sync_connected = sync
                .get("connected")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            info.peer_name = sync
                .get("peer_name")
                .and_then(|v| v.as_str())
                .map(String::from);
        }
        refresh_status(app);
    }
}

fn refresh_status(app: &AppHandle) {
    let Some(state) = app.try_state::<TrayState>() else {
        return;
    };
    let text = {
        let info = state.info.lock().unwrap();
        if !info.ipc_up {
            "Starting service…".to_string()
        } else if info.sync_connected {
            format!(
                "Connected to {}",
                info.peer_name.as_deref().unwrap_or("peer")
            )
        } else {
            "Not connected to a peer".to_string()
        }
    };
    let status = state.status.clone();
    let _ = app.run_on_main_thread(move || {
        let _ = status.set_text(text);
    });
}
