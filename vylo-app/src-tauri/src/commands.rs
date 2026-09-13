//! Tauri commands exposed to the webview.

use tauri::{AppHandle, State};
use tauri_plugin_autostart::ManagerExt as _;
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_opener::OpenerExt;

use crate::ipc::IpcHandle;

/// Write one raw JSON request line to the daemon socket.
#[tauri::command]
pub fn request(state: State<'_, IpcHandle>, json: String) -> Result<(), String> {
    state.send(json)
}

/// Whether the bridge currently holds a live daemon connection.
#[tauri::command]
pub fn ipc_connected(state: State<'_, IpcHandle>) -> bool {
    state.is_connected()
}

/// Native multi-file picker; `None` when cancelled.
#[tauri::command]
pub async fn pick_files(app: AppHandle) -> Result<Option<Vec<String>>, String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog()
        .file()
        .set_title("Send files")
        .pick_files(move |paths| {
            let _ = tx.send(paths);
        });
    let paths = rx.await.map_err(|e| e.to_string())?;
    Ok(paths.map(|paths| {
        paths
            .into_iter()
            .filter_map(|p| p.into_path().ok())
            .map(|p| p.to_string_lossy().into_owned())
            .collect()
    }))
}

/// Native folder picker; `None` when cancelled.
#[tauri::command]
pub async fn pick_dir(app: AppHandle, title: Option<String>) -> Result<Option<String>, String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog()
        .file()
        .set_title(title.as_deref().unwrap_or("Choose folder for received files"))
        .pick_folder(move |path| {
            let _ = tx.send(path);
        });
    let path = rx.await.map_err(|e| e.to_string())?;
    Ok(path
        .and_then(|p| p.into_path().ok())
        .map(|p| p.to_string_lossy().into_owned()))
}

/// Reveal a file in its folder (or open the folder itself if `path` is a dir).
#[tauri::command]
pub fn open_file_dir(app: AppHandle, path: String) -> Result<(), String> {
    let p = std::path::Path::new(&path);
    if p.is_dir() {
        app.opener()
            .open_path(path.clone(), None::<&str>)
            .map_err(|e| e.to_string())
    } else {
        app.opener()
            .reveal_item_in_dir(p)
            .map_err(|e| e.to_string())
    }
}

#[tauri::command]
pub fn set_autostart(app: AppHandle, enabled: bool) -> Result<(), String> {
    let autolaunch = app.autolaunch();
    if enabled {
        autolaunch.enable().map_err(|e| e.to_string())
    } else {
        autolaunch.disable().map_err(|e| e.to_string())
    }
}

#[tauri::command]
pub fn get_autostart(app: AppHandle) -> Result<bool, String> {
    app.autolaunch().is_enabled().map_err(|e| e.to_string())
}

/// Whether a manual launch should go straight to the tray.
#[tauri::command]
pub fn get_start_hidden(app: AppHandle) -> bool {
    crate::prefs::load(&app).start_hidden
}

#[tauri::command]
pub fn set_start_hidden(app: AppHandle, enabled: bool) -> Result<(), String> {
    let mut prefs = crate::prefs::load(&app);
    prefs.start_hidden = enabled;
    crate::prefs::store(&app, prefs)
}

/// Restart into a freshly installed update.
///
/// Tauri relaunches with the arguments this process was started with, so an
/// app that the login item started with `--hidden` would come back invisible
/// right after the user pressed *Update* in its window. Remember that the
/// window is open before handing over.
#[tauri::command]
pub fn relaunch_for_update(app: AppHandle) {
    let mut prefs = crate::prefs::load(&app);
    prefs.show_on_next_start = true;
    if let Err(e) = crate::prefs::store(&app, prefs) {
        log::warn!("could not remember the window state across the update: {e}");
    }
    app.restart()
}

#[tauri::command]
pub fn get_platform() -> String {
    std::env::consts::OS.to_string()
}
