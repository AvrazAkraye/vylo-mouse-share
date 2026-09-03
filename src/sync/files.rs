//! File and folder sending / receiving over the sync channel.
//!
//! Transfers are chunked over the TLS stream (which is separate from
//! the UDP input channel, so a large transfer never delays input
//! events). The receiver writes to `<file_dir>/<name>.vylopart` and
//! only renames to the final name once the sha256 sent with `FileDone`
//! matches what was received. A folder is walked on the sender and
//! streamed file by file into a staging directory on the receiver, which
//! is renamed into place only once every file inside has verified — so
//! a folder, like a file, is either delivered whole or not at all.

use super::proto::{CHUNK_SIZE, SyncMessage};
use lan_mouse_ipc::{Direction, FileTransferStatus, TransferKind, TransferState};
use sha2::{Digest, Sha256};
use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    path::{Component, Path, PathBuf},
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    fs::File,
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{Semaphore, mpsc::Sender, oneshot},
    task::spawn_local,
};
use tokio_util::sync::CancellationToken;

pub(crate) const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);
/// files of one folder in flight at once: enough to hide the per-file
/// offer/accept round trip, few enough to bound open handles on both ends
const TREE_WINDOW: usize = 3;
/// sanity bound on entries (files + directories) in one folder transfer
pub(crate) const MAX_TREE_ENTRIES: u64 = 1_000_000;
/// deepest relative path accepted inside a folder
const MAX_TREE_DEPTH: usize = 64;
/// name of the staging directory for a standalone folder transfer
pub(crate) fn tree_staging_name(tree: u64) -> String {
    format!(".vylo-tree-{tree}")
}
/// name of the staging directory for a cross-machine drag
pub(crate) fn drag_staging_name(drag: u64) -> String {
    format!(".vylo-drag-{drag}")
}

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
        kind: TransferKind::File,
        files: 0,
    }
}

/// a folder transfer as one row: `files` inside, bytes as progress
#[allow(clippy::too_many_arguments)]
pub(crate) fn tree_status(
    id: u64,
    name: &str,
    files: u32,
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
        kind: TransferKind::Folder,
        files,
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

/// A path relative to a folder root, as sent in `TreeDir` / `TreeFile`,
/// reduced to something safe to join under the staging root: every
/// component is sanitized like a file name (which rejects `.`, `..` and
/// empty names outright), no absolute paths, bounded depth. `None` means
/// the entry must be refused.
pub(crate) fn sanitize_rel_path(rel: &str) -> Option<PathBuf> {
    if rel.is_empty() || rel.len() > 4096 {
        return None;
    }
    let mut out = PathBuf::new();
    let mut depth = 0;
    for component in rel.split(['/', '\\']) {
        // an empty component means a leading/trailing/double separator:
        // not something our sender produces, so treat it as hostile
        if component.is_empty() {
            return None;
        }
        depth += 1;
        if depth > MAX_TREE_DEPTH {
            return None;
        }
        out.push(sanitize_name(component)?);
    }
    // belt and braces: the joined result must be purely normal components
    if !out.components().all(|c| matches!(c, Component::Normal(_))) {
        return None;
    }
    Some(out)
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

/// like [`unique_path`] but for a directory name, where a dot is not an
/// extension separator: `my.photos` -> `my.photos (1)`
pub(crate) fn unique_dir_path(dir: &Path, name: &str) -> PathBuf {
    let candidate = dir.join(name);
    if !candidate.exists() {
        return candidate;
    }
    for i in 1u32.. {
        let candidate = dir.join(format!("{name} ({i})"));
        if !candidate.exists() {
            return candidate;
        }
    }
    unreachable!()
}

/// Move a file or directory into place: rename when possible (staging
/// normally shares a volume with the destination), else copy + delete,
/// recursively for directories (the Desktop may be on another volume).
pub(crate) async fn move_path(src: &Path, dst: &Path) -> std::io::Result<()> {
    if tokio::fs::rename(src, dst).await.is_ok() {
        return Ok(());
    }
    let (src, dst) = (src.to_path_buf(), dst.to_path_buf());
    tokio::task::spawn_blocking(move || {
        if std::fs::symlink_metadata(&src)?.is_dir() {
            // never leave a half-copied tree at the destination
            if let Err(e) = copy_dir_all(&src, &dst) {
                let _ = std::fs::remove_dir_all(&dst);
                return Err(e);
            }
            std::fs::remove_dir_all(&src)
        } else {
            if let Err(e) = std::fs::copy(&src, &dst) {
                let _ = std::fs::remove_file(&dst);
                return Err(e);
            }
            std::fs::remove_file(&src)
        }
    })
    .await
    .map_err(|e| std::io::Error::other(e.to_string()))?
}

fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let target = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_all(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

/// Remove staging directories left behind by a previous run (crash or
/// kill mid-transfer). Called once at startup, when nothing is in flight.
pub(crate) async fn purge_stale_staging(file_dir: &Path) {
    let Ok(mut entries) = tokio::fs::read_dir(file_dir).await else {
        return;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with(".vylo-drag-") || name.starts_with(".vylo-tree-") {
            log::info!("removing stale staging dir {}", entry.path().display());
            let _ = tokio::fs::remove_dir_all(entry.path()).await;
        }
    }
}

/* ------------------------------ sending ------------------------------ */

/// Outgoing transfers waiting on the peer, keyed by our transfer id:
/// the one-shot accept/reject reply and a cancellation the reader flips
/// when the peer sends `FileCancel` for a file we are streaming — so we
/// stop reading and sending instead of pushing the rest into the void.
#[derive(Clone, Default)]
pub(crate) struct Outgoing(Rc<RefCell<HashMap<u64, OutgoingSlot>>>);

struct OutgoingSlot {
    accept: Option<oneshot::Sender<Result<(), String>>>,
    abort: Rc<Abort>,
}

#[derive(Default)]
pub(crate) struct Abort {
    token: CancellationToken,
    reason: RefCell<Option<String>>,
}

impl Abort {
    fn reason(&self) -> String {
        self.reason
            .borrow()
            .clone()
            .unwrap_or_else(|| "cancelled by peer".to_string())
    }
}

impl Outgoing {
    fn register(&self, id: u64) -> (oneshot::Receiver<Result<(), String>>, Rc<Abort>) {
        let (tx, rx) = oneshot::channel();
        let abort = Rc::new(Abort::default());
        self.0.borrow_mut().insert(
            id,
            OutgoingSlot {
                accept: Some(tx),
                abort: abort.clone(),
            },
        );
        (rx, abort)
    }

    fn finished(&self, id: u64) {
        self.0.borrow_mut().remove(&id);
    }

    pub(crate) fn accepted(&self, id: u64) {
        if let Some(tx) = self.0.borrow_mut().get_mut(&id).and_then(|s| s.accept.take()) {
            let _ = tx.send(Ok(()));
        }
    }

    pub(crate) fn rejected(&self, id: u64, reason: String) {
        if let Some(tx) = self.0.borrow_mut().get_mut(&id).and_then(|s| s.accept.take()) {
            let _ = tx.send(Err(reason));
        }
    }

    /// the peer gave up on a transfer we are sending
    pub(crate) fn aborted(&self, id: u64, reason: String) -> bool {
        match self.0.borrow().get(&id) {
            Some(slot) => {
                *slot.abort.reason.borrow_mut() = Some(reason);
                slot.abort.token.cancel();
                true
            }
            None => false,
        }
    }
}

/// how a file is announced to the peer
pub(crate) enum Offer {
    /// a standalone file, delivered straight into the receive folder
    Plain,
    /// staged as part of a cross-machine drag
    Drag(u64),
    /// part of a folder, at `rel` inside it
    Tree { tree: u64, rel: String },
}

/// where a file's progress goes: its own row, or its folder's aggregate
pub(crate) enum Report {
    Single(super::EventSender),
    Tree(Rc<TreeProgress>),
}

impl Report {
    fn active(&self, id: u64, name: &str, size: u64) {
        if let Report::Single(events) = self {
            events.send(super::SyncEvent::Transfer(status(
                id,
                name,
                Direction::Sent,
                0,
                size,
                TransferState::Active,
                None,
            )));
        }
    }

    fn progress(&self, id: u64, name: &str, offset: u64, size: u64) {
        match self {
            Report::Single(events) => events.send(super::SyncEvent::Transfer(status(
                id,
                name,
                Direction::Sent,
                offset,
                size,
                TransferState::Active,
                None,
            ))),
            Report::Tree(tree) => tree.file_progress(id, offset),
        }
    }

    fn done(&self, id: u64, name: &str, size: u64) {
        match self {
            Report::Single(events) => events.send(super::SyncEvent::Transfer(status(
                id,
                name,
                Direction::Sent,
                size,
                size,
                TransferState::Done,
                None,
            ))),
            Report::Tree(tree) => tree.file_done(id, size),
        }
    }

    fn failed(&self, id: u64, name: &str, reason: &str) {
        match self {
            Report::Single(events) => events.send(super::SyncEvent::Transfer(status(
                id,
                name,
                Direction::Sent,
                0,
                0,
                TransferState::Failed,
                Some(reason.to_string()),
            ))),
            // the folder reports the failure once, with the file named
            Report::Tree(tree) => tree.file_failed(id, name, reason),
        }
    }
}

/// Aggregate progress of a folder being sent: bytes of finished files
/// plus the offsets of the ones in flight, reported as one row.
pub(crate) struct TreeProgress {
    id: u64,
    name: String,
    files: u32,
    total: u64,
    done: Cell<u64>,
    inflight: RefCell<HashMap<u64, u64>>,
    error: RefCell<Option<String>>,
    last: Cell<Instant>,
    events: super::EventSender,
}

impl TreeProgress {
    fn transferred(&self) -> u64 {
        self.done.get() + self.inflight.borrow().values().sum::<u64>()
    }

    fn emit(&self, state: TransferState, detail: Option<String>) {
        self.events
            .send(super::SyncEvent::Transfer(tree_status(
                self.id,
                &self.name,
                self.files,
                Direction::Sent,
                self.transferred(),
                self.total,
                state,
                detail,
            )));
    }

    fn file_progress(&self, id: u64, offset: u64) {
        self.inflight.borrow_mut().insert(id, offset);
        if self.last.get().elapsed() >= PROGRESS_INTERVAL {
            self.last.set(Instant::now());
            self.emit(TransferState::Active, None);
        }
    }

    fn file_done(&self, id: u64, size: u64) {
        self.inflight.borrow_mut().remove(&id);
        self.done.set(self.done.get() + size);
        // small files never reach the in-file progress path, so the row
        // would otherwise sit at 0% until the very end
        if self.last.get().elapsed() >= PROGRESS_INTERVAL {
            self.last.set(Instant::now());
            self.emit(TransferState::Active, None);
        }
    }

    fn file_failed(&self, id: u64, name: &str, reason: &str) {
        self.inflight.borrow_mut().remove(&id);
        let mut error = self.error.borrow_mut();
        if error.is_none() {
            *error = Some(format!("{name}: {reason}"));
        }
    }

    fn error(&self) -> Option<String> {
        self.error.borrow().clone()
    }
}

/// Stream one file to the peer. Waits for the receiver to accept,
/// then sends chunks (the bounded outbound queue provides
/// backpressure), then `FileDone` with the hash. Returns the bytes sent.
pub(crate) async fn send_file(
    id: u64,
    path: PathBuf,
    out_tx: Sender<SyncMessage>,
    outgoing: Outgoing,
    offer: Offer,
    report: Report,
) -> Result<u64, String> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".to_string());
    let (accept_rx, abort) = outgoing.register(id);
    let result = stream_file(id, &path, &name, out_tx, accept_rx, &abort, offer, &report).await;
    outgoing.finished(id);
    match &result {
        Ok(size) => report.done(id, &name, *size),
        Err(reason) => report.failed(id, &name, reason),
    }
    result
}

#[allow(clippy::too_many_arguments)]
async fn stream_file(
    id: u64,
    path: &Path,
    name: &str,
    out_tx: Sender<SyncMessage>,
    accept_rx: oneshot::Receiver<Result<(), String>>,
    abort: &Abort,
    offer: Offer,
    report: &Report,
) -> Result<u64, String> {
    let opened = match File::open(path).await {
        Ok(f) => match f.metadata().await {
            Ok(meta) if meta.is_dir() => {
                // folders take the tree path (see `send_tree`); reaching
                // this means the peer is too old for it
                Err("folders need Vylo 1.0.5 or newer on the other machine".to_string())
            }
            Ok(meta) => Ok((f, meta.len())),
            Err(e) => Err(format!("cannot stat {}: {e}", path.display())),
        },
        Err(e) => Err(format!("cannot open {}: {e}", path.display())),
    };
    let (mut file, size) = match opened {
        Ok(ok) => ok,
        Err(reason) => {
            // A drag announced how many items to expect; an item that never
            // gets offered would leave the drop waiting forever. Offer it
            // anyway and cancel straight away so the receiver can settle.
            if let Offer::Drag(drag) = offer {
                let _ = out_tx
                    .send(SyncMessage::DragOffer {
                        drag,
                        id,
                        name: name.to_string(),
                        size: 0,
                    })
                    .await;
                let _ = out_tx
                    .send(SyncMessage::FileCancel {
                        id,
                        reason: reason.clone(),
                    })
                    .await;
            }
            return Err(reason);
        }
    };

    let offer = match offer {
        Offer::Drag(drag) => SyncMessage::DragOffer {
            drag,
            id,
            name: name.to_string(),
            size,
        },
        Offer::Plain => SyncMessage::FileOffer {
            id,
            name: name.to_string(),
            size,
        },
        Offer::Tree { tree, rel } => SyncMessage::TreeFile {
            tree,
            id,
            rel,
            size,
        },
    };
    if out_tx.send(offer).await.is_err() {
        return Err("connection closed".to_string());
    }
    report.active(id, name, size);

    let accepted = tokio::select! {
        r = accept_rx => r,
        _ = abort.token.cancelled() => return Err(abort.reason()),
    };
    match accepted {
        Ok(Ok(())) => (),
        Ok(Err(reason)) => return Err(format!("peer rejected: {reason}")),
        Err(_) => return Err("connection closed".to_string()),
    }

    let mut hasher = Sha256::new();
    let mut offset = 0u64;
    let mut buf = vec![0u8; CHUNK_SIZE];
    let mut last_progress = Instant::now();
    loop {
        if abort.token.is_cancelled() {
            return Err(abort.reason());
        }
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
                return Err(format!("read error: {e}"));
            }
        };
        hasher.update(&buf[..n]);
        let chunk = SyncMessage::FileChunk {
            id,
            offset,
            data: buf[..n].to_vec(),
        };
        let sent = tokio::select! {
            r = out_tx.send(chunk) => r.is_ok(),
            _ = abort.token.cancelled() => return Err(abort.reason()),
        };
        if !sent {
            return Err("connection closed".to_string());
        }
        offset += n as u64;
        if last_progress.elapsed() >= PROGRESS_INTERVAL {
            last_progress = Instant::now();
            report.progress(id, name, offset, size);
        }
    }
    // a file that grew or shrank since the offer can't verify on the other
    // end; say so here rather than letting the peer report a mismatch
    if offset != size {
        let reason = format!("file changed while sending ({offset} of {size} bytes)");
        let _ = out_tx
            .send(SyncMessage::FileCancel {
                id,
                reason: reason.clone(),
            })
            .await;
        return Err(reason);
    }

    let sha256: [u8; 32] = hasher.finalize().into();
    if out_tx
        .send(SyncMessage::FileDone { id, sha256 })
        .await
        .is_err()
    {
        return Err("connection closed".to_string());
    }
    Ok(size)
}

/// everything under a folder, relative paths with forward slashes
pub(crate) struct TreePlan {
    /// every directory (empty ones included), parents before children
    pub(crate) dirs: Vec<String>,
    /// (relative path, absolute path, size)
    pub(crate) files: Vec<(String, PathBuf, u64)>,
    pub(crate) bytes: u64,
}

/// Walk a folder (blocking; run on the blocking pool). Symlinks are
/// skipped rather than followed — they could point outside the folder or
/// loop — and any unreadable directory fails the walk, since delivering
/// a silently incomplete folder would defeat the whole-or-nothing promise.
pub(crate) fn walk_tree(root: &Path) -> std::io::Result<TreePlan> {
    let mut plan = TreePlan {
        dirs: Vec::new(),
        files: Vec::new(),
        bytes: 0,
    };
    // (absolute dir, relative prefix)
    let mut pending: Vec<(PathBuf, String)> = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, prefix)) = pending.pop() {
        let mut entries: Vec<_> = std::fs::read_dir(&dir)?.collect::<Result<_, _>>()?;
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let meta = std::fs::symlink_metadata(entry.path())?;
            if meta.file_type().is_symlink() {
                log::debug!("skipping symlink {}", entry.path().display());
                continue;
            }
            let rel = if prefix.is_empty() {
                entry.file_name().to_string_lossy().to_string()
            } else {
                format!("{prefix}/{}", entry.file_name().to_string_lossy())
            };
            if (plan.dirs.len() + plan.files.len()) as u64 >= MAX_TREE_ENTRIES {
                return Err(std::io::Error::other(format!(
                    "folder has more than {MAX_TREE_ENTRIES} entries"
                )));
            }
            if meta.is_dir() {
                plan.dirs.push(rel.clone());
                pending.push((entry.path(), rel));
            } else {
                plan.bytes += meta.len();
                plan.files.push((rel, entry.path(), meta.len()));
            }
        }
    }
    Ok(plan)
}

/// Send a whole folder: walk it, announce it, recreate its directories,
/// stream its files through a small window, then close it with `TreeEnd`.
/// Any failed file aborts the rest and is reported once, as the folder's.
pub(crate) async fn send_tree(
    tree: u64,
    root: PathBuf,
    drag: Option<u64>,
    out_tx: Sender<SyncMessage>,
    outgoing: Outgoing,
    events: super::EventSender,
) {
    let name = root
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "folder".to_string());
    let fail = |reason: String| {
        events.send(super::SyncEvent::Transfer(tree_status(
            tree,
            &name,
            0,
            Direction::Sent,
            0,
            0,
            TransferState::Failed,
            Some(reason),
        )));
    };

    // the walk can take a while on a big tree; keep it off the reactor
    let walk_root = root.clone();
    let walked = match tokio::task::spawn_blocking(move || walk_tree(&walk_root)).await {
        Ok(Ok(plan)) => Ok(plan),
        Ok(Err(e)) => Err(format!("cannot read folder: {e}")),
        Err(e) => Err(format!("cannot read folder: {e}")),
    };
    let plan = match walked {
        Ok(plan) => plan,
        Err(reason) => {
            // announce and close it at once: a drag counted this item, and
            // the peer's row shows the reason instead of nothing at all
            let _ = out_tx
                .send(SyncMessage::TreeOffer {
                    tree,
                    name: name.clone(),
                    files: 0,
                    bytes: 0,
                    drag,
                })
                .await;
            let _ = out_tx
                .send(SyncMessage::TreeEnd {
                    tree,
                    error: Some(reason.clone()),
                })
                .await;
            return fail(reason);
        }
    };
    let files = plan.files.len() as u32;
    let progress = Rc::new(TreeProgress {
        id: tree,
        name: name.clone(),
        files,
        total: plan.bytes,
        done: Cell::new(0),
        inflight: RefCell::new(HashMap::new()),
        error: RefCell::new(None),
        last: Cell::new(Instant::now()),
        events: events.clone(),
    });

    let announce = SyncMessage::TreeOffer {
        tree,
        name: name.clone(),
        files,
        bytes: plan.bytes,
        drag,
    };
    if out_tx.send(announce).await.is_err() {
        return fail("connection closed".to_string());
    }
    progress.emit(TransferState::Active, None);
    for rel in plan.dirs {
        if out_tx.send(SyncMessage::TreeDir { tree, rel }).await.is_err() {
            return fail("connection closed".to_string());
        }
    }

    let window = Arc::new(Semaphore::new(TREE_WINDOW));
    let mut tasks = Vec::new();
    for (rel, path, _size) in plan.files {
        // a failure anywhere stops the folder; in-flight files still drain
        if progress.error().is_some() {
            break;
        }
        let Ok(permit) = window.clone().acquire_owned().await else {
            break;
        };
        // a file may have failed while we waited for the permit
        if progress.error().is_some() {
            break;
        }
        let id = super::next_transfer_id();
        let task = spawn_local({
            let out_tx = out_tx.clone();
            let outgoing = outgoing.clone();
            let progress = progress.clone();
            async move {
                let _permit = permit;
                let _ = send_file(
                    id,
                    path,
                    out_tx,
                    outgoing,
                    Offer::Tree { tree, rel },
                    Report::Tree(progress),
                )
                .await;
            }
        });
        tasks.push(task);
    }
    for task in tasks {
        let _ = task.await;
    }

    let error = progress.error();
    let _ = out_tx
        .send(SyncMessage::TreeEnd {
            tree,
            error: error.clone(),
        })
        .await;
    match error {
        None => progress.emit(TransferState::Done, None),
        Some(reason) => progress.emit(TransferState::Failed, Some(reason)),
    }
}

/* ----------------------------- receiving ----------------------------- */

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
    /// the folder transfer this file belongs to, if any
    pub(crate) tree: Option<u64>,
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

/// something that finished into a drag's staging area and is waiting for
/// the drop to be moved to its final location
pub(crate) struct StagedItem {
    pub(crate) id: u64,
    pub(crate) name: String,
    pub(crate) kind: TransferKind,
    pub(crate) files: u32,
    pub(crate) size: u64,
    pub(crate) path: PathBuf,
}

/// A cross-machine drag being received: its files are staged in a
/// private directory and only moved to the drop location once the peer
/// confirms the button was released (`DragDrop`). A `DragCancel`, a
/// connection loss, or a superseding drag discards the staging area.
pub(crate) struct IncomingDrag {
    pub(crate) drag: u64,
    /// `<file_dir>/.vylo-drag-<drag>/`
    pub(crate) dir: PathBuf,
    /// number of items (files and folders) the source announced
    pub(crate) expected: u32,
    /// offers seen so far (accepted or rejected)
    pub(crate) offered: u32,
    /// transfer ids (files and folders) offered but not yet settled
    pub(crate) outstanding: HashSet<u64>,
    /// staged items that completed successfully
    pub(crate) done: Vec<StagedItem>,
    /// the source reported the button release
    pub(crate) drop_requested: bool,
}

impl IncomingDrag {
    /// all announced items have been offered and every offer has settled
    pub(crate) fn settled(&self) -> bool {
        self.offered >= self.expected && self.outstanding.is_empty()
    }
}

/// A folder being received: everything lands under `root` inside a
/// staging directory and is renamed into place (or handed to the drag it
/// belongs to) once the sender closes it and every file has verified.
pub(crate) struct IncomingTree {
    pub(crate) tree: u64,
    pub(crate) name: String,
    /// staging location of the folder itself
    pub(crate) root: PathBuf,
    /// the staging dir owned by this tree (`None` when inside a drag,
    /// whose staging dir owns it)
    pub(crate) staging: Option<PathBuf>,
    pub(crate) drag: Option<u64>,
    pub(crate) files: u32,
    pub(crate) total: u64,
    pub(crate) done_bytes: u64,
    /// file ids offered but not yet settled
    pub(crate) outstanding: HashSet<u64>,
    /// entries created so far (bounds a runaway sender)
    pub(crate) entries: u64,
    /// `TreeEnd` seen
    pub(crate) ended: bool,
    /// first failure; the tree is discarded once settled
    pub(crate) failed: Option<String>,
    pub(crate) last_progress: Instant,
}

impl IncomingTree {
    pub(crate) fn settled(&self) -> bool {
        self.ended && self.outstanding.is_empty()
    }

    pub(crate) fn fail(&mut self, reason: String) {
        if self.failed.is_none() {
            self.failed = Some(reason);
        }
    }

    pub(crate) fn progress_due(&mut self) -> bool {
        if self.last_progress.elapsed() >= PROGRESS_INTERVAL {
            self.last_progress = Instant::now();
            true
        } else {
            false
        }
    }

    pub(crate) fn status(
        &self,
        transferred: u64,
        state: TransferState,
        detail: Option<String>,
    ) -> FileTransferStatus {
        tree_status(
            self.tree,
            &self.name,
            self.files,
            Direction::Received,
            transferred,
            self.total,
            state,
            detail,
        )
    }
}

/// Where dropped files land: the Desktop is the natural "I dragged it to
/// the other computer" destination; fall back to the regular receive dir.
pub(crate) fn drop_dir(fallback: &Path) -> PathBuf {
    dirs::desktop_dir().unwrap_or_else(|| fallback.to_path_buf())
}

/// start an incoming transfer: create the target directory and the
/// `.vylopart` file
pub(crate) async fn begin_recv(
    dir: &Path,
    offered_name: &str,
    size: u64,
    drag: Option<u64>,
    tree: Option<u64>,
) -> Result<RecvTransfer, String> {
    let name = sanitize_name(offered_name).ok_or_else(|| "invalid file name".to_string())?;
    tokio::fs::create_dir_all(dir)
        .await
        .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let final_path = unique_path(dir, &name);
    // the part file is picked with the same uniqueness rule, so it can't
    // truncate a real file that happens to be called `<name>.vylopart`
    let part_name = format!(
        "{}.vylopart",
        final_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| name.clone())
    );
    let part_path = unique_path(dir, &part_name);
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
        tree,
    })
}

/// verify hash and move the finished file into place
pub(crate) async fn finish_recv(mut t: RecvTransfer, sha256: [u8; 32]) -> Result<PathBuf, String> {
    let _ = t.file.flush().await;
    // A standalone file is durable before it appears under its final
    // name. Files inside a folder skip the per-file fsync — the folder
    // only appears once complete, and thousands of small files would
    // otherwise crawl.
    if t.tree.is_none() {
        let _ = t.file.sync_all().await;
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rel_paths_are_confined_to_the_root() {
        assert_eq!(
            sanitize_rel_path("sub/dir/file.txt"),
            Some(PathBuf::from("sub").join("dir").join("file.txt"))
        );
        // backslashes are separators no matter which OS sent them
        assert_eq!(
            sanitize_rel_path("sub\\file.txt"),
            Some(PathBuf::from("sub").join("file.txt"))
        );
        for hostile in [
            "../evil",
            "sub/../../evil",
            "/etc/passwd",
            "..",
            ".",
            "sub/./x",
            "",
            "a//b",
            "a/",
            "/a",
        ] {
            assert!(
                sanitize_rel_path(hostile).is_none(),
                "{hostile:?} must be refused"
            );
        }
        // a drive prefix is not an escape once the colon is neutralised
        assert_eq!(
            sanitize_rel_path("C:\\Windows\\evil.exe"),
            Some(PathBuf::from("C_").join("Windows").join("evil.exe"))
        );
        // reserved device names are neutralised per component
        assert_eq!(
            sanitize_rel_path("logs/nul.txt"),
            Some(PathBuf::from("logs").join("_nul.txt"))
        );
        // depth bound
        let deep = vec!["d"; MAX_TREE_DEPTH + 1].join("/");
        assert!(sanitize_rel_path(&deep).is_none());
        let ok = vec!["d"; MAX_TREE_DEPTH].join("/");
        assert!(sanitize_rel_path(&ok).is_some());
    }

    #[test]
    fn unique_dir_path_does_not_treat_dots_as_extensions() {
        let tmp = std::env::temp_dir().join(format!("vylo-udp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("my.photos")).unwrap();
        assert_eq!(
            unique_dir_path(&tmp, "my.photos"),
            tmp.join("my.photos (1)")
        );
        assert_eq!(unique_dir_path(&tmp, "fresh"), tmp.join("fresh"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn part_file_never_truncates_a_real_file_of_that_name() {
        let tmp = std::env::temp_dir().join(format!("vylo-part-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        // a real file that looks like our temp name is already there
        std::fs::write(tmp.join("x.txt.vylopart"), b"precious").unwrap();
        let t = begin_recv(&tmp, "x.txt", 3, None, None).await.unwrap();
        assert_ne!(t.part_path, tmp.join("x.txt.vylopart"));
        assert_eq!(std::fs::read(tmp.join("x.txt.vylopart")).unwrap(), b"precious");
        drop(t);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn walk_skips_symlinks_and_lists_empty_dirs() {
        let tmp = std::env::temp_dir().join(format!("vylo-walk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("root/sub/empty")).unwrap();
        std::fs::write(tmp.join("root/a.txt"), b"hello").unwrap();
        std::fs::write(tmp.join("root/sub/b.bin"), vec![1u8; 300]).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/", tmp.join("root/escape")).unwrap();

        let plan = walk_tree(&tmp.join("root")).unwrap();
        let mut dirs = plan.dirs.clone();
        dirs.sort();
        assert_eq!(dirs, vec!["sub".to_string(), "sub/empty".to_string()]);
        let mut rels: Vec<String> = plan.files.iter().map(|(r, ..)| r.clone()).collect();
        rels.sort();
        assert_eq!(rels, vec!["a.txt".to_string(), "sub/b.bin".to_string()]);
        assert_eq!(plan.bytes, 305);
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
