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
    collections::HashSet,
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
/// platform. Splits on BOTH `/` and `\` regardless of the host OS (not
/// `Path::file_name`, which treats `\` as a separator only on Windows),
/// so a name crafted on either platform is reduced the same way.
pub(crate) fn sanitize_name(offered: &str) -> Option<String> {
    let base = offered
        .rsplit(|c| c == '/' || c == '\\')
        .next()
        .unwrap_or(offered);
    let name: String = base
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
    /// the cross-machine drag this transfer belongs to, if any
    pub(crate) drag: Option<u64>,
}

/// A cross-machine drag being received: its files are staged in a
/// private directory and only moved to the drop location once the peer
/// confirms the button was released (`DragDrop`). A `DragCancel`, a
/// connection loss, or a superseding drag discards the staging area.
pub(crate) struct IncomingDrag {
    pub(crate) drag: u64,
    /// `<file_dir>/.vylo-drag-<drag>/`
    pub(crate) dir: PathBuf,
    /// number of files the source announced
    pub(crate) expected: u32,
    /// offers seen so far (accepted or rejected)
    pub(crate) offered: u32,
    /// transfer ids offered but not yet finished (done or failed)
    pub(crate) outstanding: HashSet<u64>,
    /// staged files that completed successfully
    pub(crate) done: Vec<PathBuf>,
    /// the source reported the button release
    pub(crate) drop_requested: bool,
}

impl IncomingDrag {
    /// all announced files have been offered and every offer has settled
    fn settled(&self) -> bool {
        self.offered >= self.expected && self.outstanding.is_empty()
    }
}

/// Where dropped files land: the Desktop is the natural "I dragged it to
/// the other computer" destination; fall back to the regular receive dir.
pub(crate) fn drop_dir(fallback: &Path) -> PathBuf {
    dirs::desktop_dir().unwrap_or_else(|| fallback.to_path_buf())
}

/// If the drop was requested and every file has settled, move the staged
/// files into place, remove the staging dir and report the final paths.
pub(crate) async fn try_finalize_drag(
    incoming: &mut Option<IncomingDrag>,
    fallback_dir: &Path,
    events: &super::EventSender,
) {
    let ready = incoming
        .as_ref()
        .is_some_and(|d| d.drop_requested && d.settled());
    if !ready {
        return;
    }
    let Some(d) = incoming.take() else { return };
    let dest = drop_dir(fallback_dir);
    if let Err(e) = tokio::fs::create_dir_all(&dest).await {
        log::warn!("cannot create drop dir {}: {e}", dest.display());
    }
    let mut placed = Vec::new();
    for staged in d.done {
        let Some(name) = staged.file_name().map(|n| n.to_string_lossy().to_string()) else {
            continue;
        };
        let target = unique_path(&dest, &name);
        // rename first; the staging dir normally shares a volume with
        // file_dir but the Desktop may not, so fall back to copy+remove
        let moved = match tokio::fs::rename(&staged, &target).await {
            Ok(()) => true,
            Err(_) => match tokio::fs::copy(&staged, &target).await {
                Ok(_) => {
                    let _ = tokio::fs::remove_file(&staged).await;
                    true
                }
                Err(e) => {
                    log::warn!("cannot place dropped file {}: {e}", target.display());
                    false
                }
            },
        };
        if moved {
            placed.push(target);
        }
    }
    let _ = tokio::fs::remove_dir_all(&d.dir).await;
    if !placed.is_empty() {
        log::info!("dropped {} file(s) into {}", placed.len(), dest.display());
        events.send(super::SyncEvent::DragDropped { paths: placed });
    }
}

/// throw away a staged drag (cancelled, superseded, or connection lost)
pub(crate) async fn discard_drag(d: IncomingDrag) {
    let _ = tokio::fs::remove_dir_all(&d.dir).await;
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
    // when part of a cross-machine drag, the receiver stages it instead of
    // delivering it
    drag: Option<u64>,
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

    let offer = match drag {
        Some(drag) => SyncMessage::DragOffer {
            drag,
            id,
            name: name.clone(),
            size,
        },
        None => SyncMessage::FileOffer {
            id,
            name: name.clone(),
            size,
        },
    };
    if out_tx.send(offer).await.is_err() {
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
    drag: Option<u64>,
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
        drag,
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
