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

    // admin mode: the login item is the elevated task (see elevation.rs)
    #[cfg(windows)]
    if crate::prefs::load(&app).run_as_admin {
        if crate::elevation::is_elevated() {
            let _ = autolaunch.disable();
            return if enabled {
                crate::elevation::create_task()
            } else {
                crate::elevation::delete_task()
            };
        }
        if !enabled {
            crate::elevation::delete_task()?;
        }
    }

    if enabled {
        autolaunch.enable().map_err(|e| e.to_string())
    } else {
        autolaunch.disable().map_err(|e| e.to_string())
    }
}

#[tauri::command]
pub fn get_autostart(app: AppHandle) -> Result<bool, String> {
    #[cfg(windows)]
    if crate::prefs::load(&app).run_as_admin && crate::elevation::task_exists() {
        return Ok(true);
    }
    app.autolaunch().is_enabled().map_err(|e| e.to_string())
}

/// State of "Control admin windows" for the settings screen.
#[derive(serde::Serialize)]
pub struct AdminMode {
    /// only Windows has this setting
    supported: bool,
    /// the preference
    enabled: bool,
    /// whether this copy actually runs as administrator right now
    elevated: bool,
}

#[tauri::command]
pub fn get_admin_mode(app: AppHandle) -> AdminMode {
    #[cfg(windows)]
    return AdminMode {
        supported: true,
        enabled: crate::prefs::load(&app).run_as_admin,
        elevated: crate::elevation::is_elevated(),
    };
    #[cfg(not(windows))]
    {
        let _ = app;
        AdminMode {
            supported: false,
            enabled: false,
            elevated: false,
        }
    }
}

/// Turn "Control admin windows" on or off.
///
/// Turning it on from a normal copy asks for permission through UAC, then
/// this copy exits and the elevated one takes over (with its window open).
/// Turning it off takes effect at the next start; the running copy stays
/// elevated until then.
#[tauri::command]
pub fn set_admin_mode(app: AppHandle, enabled: bool) -> Result<(), String> {
    #[cfg(not(windows))]
    {
        let _ = (app, enabled);
        Err("only available on Windows".into())
    }

    #[cfg(windows)]
    {
        use crate::elevation;
        let before = crate::prefs::load(&app);
        let mut prefs = before;

        if !enabled {
            // keep starting at login, just without admin
            let had_task = elevation::task_exists();
            elevation::delete_task()
                .map_err(|e| format!("could not remove the admin login task: {e}"))?;
            prefs.run_as_admin = false;
            crate::prefs::store(&app, prefs)?;
            if had_task {
                if let Err(e) = app.autolaunch().enable() {
                    log::warn!("could not restore the login item: {e}");
                }
            }
            return Ok(());
        }

        prefs.run_as_admin = true;
        if elevation::is_elevated() {
            crate::prefs::store(&app, prefs)?;
            crate::refresh_autostart(&app);
            return Ok(());
        }

        // the elevated copy must open its window even if this copy was
        // started in the background: the user is looking at the settings
        prefs.show_on_next_start = true;
        crate::prefs::store(&app, prefs)?;
        match elevation::relaunch_elevated() {
            Ok(true) => {
                // let this call answer the window, then make way
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(300));
                    app.exit(0);
                });
                Ok(())
            }
            Ok(false) => {
                crate::prefs::store(&app, before)?;
                Err("Windows did not give permission".into())
            }
            Err(e) => {
                crate::prefs::store(&app, before)?;
                Err(e)
            }
        }
    }
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
