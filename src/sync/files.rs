//! File sending / receiving over the sync channel.
//!
//! Transfers are chunked over the TLS stream (which is separate from
//! the UDP input channel, so a large transfer never delays input
//! events). The receiver writes to `<file_dir>/<name>.vylopart` and
//! only renames to the final name once the sha256 sent with `FileDone`
//! matches what was received.

use super::proto::{CHUNK_SIZE, SyncMessage};
use lan_mouse_ipc::{Direction, FileTransferStatus, TransferState};
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use tokio::{
    fs::File,
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{mpsc::Sender, oneshot},
};

const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);

/// how a transfer is reported to the frontends
pub(crate) fn status(
    id: u64,
    name: &str,
    direction: Direction,
    transferred: u64,
    total: u64,
    state: TransferState,
    detail: Option<String>,
) -> FileTransferStatus {
    FileTransferStatus {
        id,
        name: name.to_string(),
        direction,
        transferred,
        total,
        state,
        detail,
    }
}

/// Reduce an offered file name to a safe basename: no directories, no
/// parent traversal, no characters that are invalid on the receiving
/// platform.
pub(crate) fn sanitize_name(offered: &str) -> Option<String> {
    let name = Path::new(offered)
        .file_name()?
        .to_string_lossy()
        .to_string();
    let name: String = name
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if (c as u32) < 0x20 => '_',
            c => c,
        })
        .collect();
    let name = name.trim_matches([' ', '.']).to_string();
    if name.is_empty() {
        return None;
    }
    // avoid Windows reserved device names (CON, NUL, COM1..9, LPT1..9);
    // the rule ignores the extension, so "NUL.txt" is reserved too
    let stem = name.split('.').next().unwrap_or(&name).to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.len() == 4
            && stem.as_bytes()[3].is_ascii_digit()
            && stem.as_bytes()[3] != b'0');
    if reserved {
        Some(format!("_{name}"))
    } else {
        Some(name)
    }
}

/// `name.ext` -> `name (1).ext`, `name (2).ext`, ... until unused
pub(crate) fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let candidate = dir.join(name);
    if !candidate.exists() {
        return candidate;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
        _ => (name.to_string(), String::new()),
    };
    for i in 1u32.. {
        let candidate = dir.join(format!("{stem} ({i}){ext}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    unreachable!()
}

/// an incoming transfer in progress
pub(crate) struct RecvTransfer {
    pub(crate) name: String,
    pub(crate) size: u64,
    pub(crate) received: u64,
    pub(crate) file: File,
    pub(crate) part_path: PathBuf,
    pub(crate) final_path: PathBuf,
    pub(crate) hasher: Sha256,
    pub(crate) last_progress: Instant,
}

impl RecvTransfer {
    pub(crate) fn progress_due(&mut self) -> bool {
        if self.last_progress.elapsed() >= PROGRESS_INTERVAL {
            self.last_progress = Instant::now();
            true
        } else {
            false
        }
    }
}

/// Stream one file to the peer. Waits for the receiver to accept,
/// then sends chunks (the bounded outbound queue provides
/// backpressure), then `FileDone` with the hash.
pub(crate) async fn send_file(
    id: u64,
    path: PathBuf,
    out_tx: Sender<SyncMessage>,
    accept_rx: oneshot::Receiver<Result<(), String>>,
    events: super::EventSender,
) {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".to_string());

    let fail = |events: &super::EventSender, msg: String| {
        events.send(super::SyncEvent::Transfer(status(
            id,
            &name,
            Direction::Sent,
            0,
            0,
            TransferState::Failed,
            Some(msg),
        )));
    };

    let mut file = match File::open(&path).await {
        Ok(f) => f,
        Err(e) => return fail(&events, format!("cannot open {}: {e}", path.display())),
    };
    let meta = match file.metadata().await {
        Ok(m) => m,
        Err(e) => return fail(&events, format!("cannot stat {}: {e}", path.display())),
    };
    if meta.is_dir() {
        return fail(&events, "folders cannot be sent (zip it first)".to_string());
    }
    let size = meta.len();

    if out_tx
        .send(SyncMessage::FileOffer {
            id,
            name: name.clone(),
            size,
        })
        .await
        .is_err()
    {
        return fail(&events, "connection closed".to_string());
    }
    events.send(super::SyncEvent::Transfer(status(
        id,
        &name,
        Direction::Sent,
        0,
        size,
        TransferState::Active,
        None,
    )));

    match accept_rx.await {
        Ok(Ok(())) => (),
        Ok(Err(reason)) => return fail(&events, format!("peer rejected: {reason}")),
        Err(_) => return fail(&events, "connection closed".to_string()),
    }

    let mut hasher = Sha256::new();
    let mut offset = 0u64;
    let mut buf = vec![0u8; CHUNK_SIZE];
    let mut last_progress = Instant::now();
    loop {
        let n = match file.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                let _ = out_tx
                    .send(SyncMessage::FileCancel {
                        id,
                        reason: format!("read error: {e}"),
                    })
                    .await;
                return fail(&events, format!("read error: {e}"));
            }
        };
        hasher.update(&buf[..n]);
        if out_tx
            .send(SyncMessage::FileChunk {
                id,
                offset,
                data: buf[..n].to_vec(),
            })
            .await
            .is_err()
        {
            return fail(&events, "connection closed".to_string());
        }
        offset += n as u64;
        if last_progress.elapsed() >= PROGRESS_INTERVAL {
            last_progress = Instant::now();
            events.send(super::SyncEvent::Transfer(status(
                id,
                &name,
                Direction::Sent,
                offset,
                size,
                TransferState::Active,
                None,
            )));
        }
    }

    let sha256: [u8; 32] = hasher.finalize().into();
    if out_tx
        .send(SyncMessage::FileDone { id, sha256 })
        .await
        .is_err()
    {
        return fail(&events, "connection closed".to_string());
    }
    events.send(super::SyncEvent::Transfer(status(
        id,
        &name,
        Direction::Sent,
        offset,
        size,
        TransferState::Done,
        None,
    )));
}

/// start an incoming transfer: create the target directory and the
/// `.vylopart` file
pub(crate) async fn begin_recv(
    dir: &Path,
    offered_name: &str,
    size: u64,
) -> Result<RecvTransfer, String> {
    let name = sanitize_name(offered_name).ok_or_else(|| "invalid file name".to_string())?;
    tokio::fs::create_dir_all(dir)
        .await
        .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let final_path = unique_path(dir, &name);
    let part_path = final_path.with_extension(format!(
        "{}vylopart",
        final_path
            .extension()
            .map(|e| format!("{}.", e.to_string_lossy()))
            .unwrap_or_default()
    ));
    let file = File::create(&part_path)
        .await
        .map_err(|e| format!("cannot create {}: {e}", part_path.display()))?;
    Ok(RecvTransfer {
        name,
        size,
        received: 0,
        file,
        part_path,
        final_path,
        hasher: Sha256::new(),
        last_progress: Instant::now(),
    })
}

/// verify hash and move the finished file into place
pub(crate) async fn finish_recv(mut t: RecvTransfer, sha256: [u8; 32]) -> Result<PathBuf, String> {
    let _ = t.file.flush().await;
    let _ = t.file.sync_all().await;
    drop(t.file);
    let actual: [u8; 32] = t.hasher.finalize().into();
    if actual != sha256 {
        let _ = tokio::fs::remove_file(&t.part_path).await;
        return Err("integrity check failed (sha256 mismatch)".to_string());
    }
    if t.received != t.size {
        let _ = tokio::fs::remove_file(&t.part_path).await;
        return Err(format!(
            "incomplete transfer ({} of {} bytes)",
            t.received, t.size
        ));
    }
    tokio::fs::rename(&t.part_path, &t.final_path)
        .await
        .map_err(|e| format!("cannot rename to {}: {e}", t.final_path.display()))?;
    Ok(t.final_path.clone())
}
