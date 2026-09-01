//! IPC bridge to the Vylo daemon.
//!
//! Connects to the daemon's frontend socket (unix stream on macOS/Linux,
//! TCP 127.0.0.1:5252 on Windows), exchanging newline-delimited JSON.
//!
//! - Every received line is re-emitted verbatim to the webview as the tauri
//!   event `"daemon"` (payload: the raw JSON string).
//! - `{"connected": bool}` is emitted as event `"daemon-ipc"` whenever the
//!   socket connects / disconnects.
//! - On (re)connect a `"Sync"` request is written so the daemon broadcasts
//!   its full state.
//! - Reconnects forever with a 1 s backoff.
//!
//! Outgoing requests (from the `request` command or the tray) are queued on
//! an unbounded channel and flushed while a connection is up.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use tauri::{AppHandle, Emitter, Manager};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader},
    sync::mpsc,
    time::{Duration, sleep},
};

/// Handle managed as tauri state; used by commands and the tray.
pub struct IpcHandle {
    tx: mpsc::UnboundedSender<String>,
    connected: Arc<AtomicBool>,
}

impl IpcHandle {
    /// Queue one request line for the daemon.
    pub fn send(&self, line: String) -> Result<(), String> {
        self.tx
            .send(line)
            .map_err(|_| "ipc bridge shut down".to_string())
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }
}

/// Create the handle + the receiving end for the bridge task.
pub fn init() -> (IpcHandle, mpsc::UnboundedReceiver<String>) {
    let (tx, rx) = mpsc::unbounded_channel();
    (
        IpcHandle {
            tx,
            connected: Arc::new(AtomicBool::new(false)),
        },
        rx,
    )
}

/// Spawn the reconnect-forever bridge loop. `IpcHandle` must already be in
/// managed state.
pub fn spawn(app: AppHandle, mut rx: mpsc::UnboundedReceiver<String>) {
    let connected = app.state::<IpcHandle>().connected.clone();
    tauri::async_runtime::spawn(async move {
        loop {
            match connect().await {
                Ok(stream) => {
                    log::info!("connected to daemon ipc socket");
                    connected.store(true, Ordering::SeqCst);
                    notify_connected(&app, true);
                    drive(&app, stream, &mut rx).await;
                    log::info!("daemon ipc socket disconnected");
                    connected.store(false, Ordering::SeqCst);
                    notify_connected(&app, false);
                }
                Err(e) => {
                    log::debug!("daemon ipc connect failed: {e}");
                }
            }
            sleep(Duration::from_secs(1)).await;
        }
    });
}

fn notify_connected(app: &AppHandle, connected: bool) {
    let _ = app.emit("daemon-ipc", serde_json::json!({ "connected": connected }));
    crate::tray::set_ipc_connected(app, connected);
}

/// Pump one live connection until it drops.
async fn drive<S>(app: &AppHandle, stream: S, rx: &mut mpsc::UnboundedReceiver<String>)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (reader, mut writer) = tokio::io::split(stream);

    // Ask for a full state broadcast right away.
    if writer.write_all(b"\"Sync\"\n").await.is_err() {
        return;
    }

    let mut lines = BufReader::new(reader).lines();
    loop {
        tokio::select! {
            line = lines.next_line() => match line {
                Ok(Some(line)) => {
                    crate::tray::observe_line(app, &line);
                    let _ = app.emit("daemon", line);
                }
                // EOF or read error: connection is gone.
                _ => return,
            },
            msg = rx.recv() => match msg {
                Some(mut msg) => {
                    if !msg.ends_with('\n') {
                        msg.push('\n');
                    }
                    if writer.write_all(msg.as_bytes()).await.is_err() {
                        return;
                    }
                }
                // all senders dropped: app is shutting down
                None => return,
            },
        }
    }
}

#[cfg(target_os = "macos")]
async fn connect() -> std::io::Result<tokio::net::UnixStream> {
    let home = std::env::var("HOME")
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::NotFound, "$HOME not set"))?;
    let path = std::path::Path::new(&home)
        .join("Library")
        .join("Caches")
        .join("vylo-socket.sock");
    tokio::net::UnixStream::connect(path).await
}

#[cfg(all(unix, not(target_os = "macos")))]
async fn connect() -> std::io::Result<tokio::net::UnixStream> {
    let dir = std::env::var("XDG_RUNTIME_DIR").map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "$XDG_RUNTIME_DIR not set")
    })?;
    tokio::net::UnixStream::connect(std::path::Path::new(&dir).join("vylo-socket.sock")).await
}

#[cfg(windows)]
async fn connect() -> std::io::Result<tokio::net::TcpStream> {
    tokio::net::TcpStream::connect(("127.0.0.1", 5252)).await
}
