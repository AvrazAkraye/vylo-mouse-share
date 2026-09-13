//! App-level preferences.
//!
//! Everything about sharing itself lives in the daemon's config; this file
//! only holds the handful of settings that belong to the window/tray shell.
//! Stored as JSON next to the app config:
//!
//!   macOS:   ~/Library/Application Support/com.vylo.mouseshare/app.json
//!   Windows: %APPDATA%\com.vylo.mouseshare\app.json

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    /// Start without showing the window: Vylo runs in the tray / menu bar.
    /// A login start is always in the background, whatever this says.
    pub start_hidden: bool,
    /// One-shot, set before an update relaunches the app: show the window
    /// on the next start even if this launch came from the login item.
    /// Cleared as soon as it is read.
    pub show_on_next_start: bool,
}

fn path(app: &AppHandle) -> Option<PathBuf> {
    Some(app.path().app_config_dir().ok()?.join("app.json"))
}

/// Read the preferences; defaults when the file is missing or unreadable.
pub fn load(app: &AppHandle) -> Prefs {
    let Some(path) = path(app) else {
        return Prefs::default();
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
            log::warn!("ignoring unreadable {}: {e}", path.display());
            Prefs::default()
        }),
        Err(_) => Prefs::default(),
    }
}

pub fn store(app: &AppHandle, prefs: Prefs) -> Result<(), String> {
    let path = path(app).ok_or("no config directory")?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let text = serde_json::to_string_pretty(&prefs).map_err(|e| e.to_string())?;
    std::fs::write(&path, text).map_err(|e| e.to_string())
}
