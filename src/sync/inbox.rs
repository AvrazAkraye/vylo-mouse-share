//! Receiving side of file, folder and drag transfers on one connection.
//!
//! Everything the peer offers is tracked here: plain files, files staged
//! for a cross-machine drag, and folders (trees) — standalone or inside
//! a drag. The state can never outlive its connection; `close` discards
//! whatever was still in flight.

use super::{
    EventSender, SyncEvent,
    files::{self, IncomingDrag, IncomingTree, RecvTransfer, StagedItem},
    proto::{self, SyncMessage},
};
use lan_mouse_ipc::{Direction, TransferKind, TransferState};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    rc::Rc,
    time::Instant,
};
use tokio::sync::mpsc;

pub(crate) struct Inbox {
    file_dir: Rc<RefCell<PathBuf>>,
    events: EventSender,
    /// where dropped items land instead of the Desktop (tests)
    drop_dir: Option<PathBuf>,
    /// incoming file transfers, keyed by the peer's ids
    recv: HashMap<u64, RecvTransfer>,
    /// the drag being staged, if any (one at a time: a pointer can only
    /// drag one thing)
    drag: Option<IncomingDrag>,
    /// folders being received, keyed by the peer's tree ids
    trees: HashMap<u64, IncomingTree>,
}

/// whether `msg` is something the inbox handles
pub(crate) fn is_inbound_transfer(msg: &SyncMessage) -> bool {
    matches!(
        msg,
        SyncMessage::FileOffer { .. }
            | SyncMessage::DragBegin { .. }
            | SyncMessage::DragOffer { .. }
            | SyncMessage::DragDrop { .. }
            | SyncMessage::DragCancel { .. }
            | SyncMessage::FileChunk { .. }
            | SyncMessage::FileDone { .. }
            | SyncMessage::FileCancel { .. }
            | SyncMessage::TreeOffer { .. }
            | SyncMessage::TreeDir { .. }
            | SyncMessage::TreeFile { .. }
            | SyncMessage::TreeEnd { .. }
    )
}

impl Inbox {
    pub(crate) fn new(file_dir: Rc<RefCell<PathBuf>>, events: EventSender) -> Self {
        Self {
            file_dir,
            events,
            drop_dir: None,
            recv: HashMap::new(),
            drag: None,
            trees: HashMap::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_drop_dir(mut self, dir: PathBuf) -> Self {
        self.drop_dir = Some(dir);
        self
    }

    /// an incoming transfer with this id exists (disambiguates
    /// `FileCancel`, which either side may send)
    pub(crate) fn has_transfer(&self, id: u64) -> bool {
        self.recv.contains_key(&id)
    }

    fn file_dir(&self) -> PathBuf {
        self.file_dir.borrow().clone()
    }

    pub(crate) async fn handle(&mut self, msg: SyncMessage, out_tx: &mpsc::Sender<SyncMessage>) {
        match msg {
            SyncMessage::FileOffer { id, name, size } => {
                let dir = self.file_dir();
                self.accept_offer(&dir, id, &name, size, None, None, out_tx)
                    .await;
            }

            /* ---- cross-machine drag-and-drop (destination side) ---- */
            SyncMessage::DragBegin { drag, count } => {
                // a new drag supersedes any staged one that never resolved
                if let Some(old) = self.drag.take() {
                    log::warn!("drag {} superseded before it resolved", old.drag);
                    self.discard_drag(old).await;
                }
                let dir = self.file_dir().join(files::drag_staging_name(drag));
                if let Err(e) = tokio::fs::create_dir_all(&dir).await {
                    log::warn!("cannot create drag staging dir {}: {e}", dir.display());
                    return;
                }
                log::info!("incoming drag {drag}: {count} item(s)");
                self.drag = Some(IncomingDrag {
                    drag,
                    dir,
                    expected: count,
                    offered: 0,
                    outstanding: HashSet::new(),
                    done: Vec::new(),
                    drop_requested: false,
                });
            }
            SyncMessage::DragOffer {
                drag,
                id,
                name,
                size,
            } => {
                let Some(inc) = self.drag.as_mut().filter(|d| d.drag == drag) else {
                    let _ = out_tx
                        .send(SyncMessage::FileReject {
                            id,
                            reason: "no drag in progress".to_string(),
                        })
                        .await;
                    return;
                };
                inc.offered += 1;
                let dir = inc.dir.clone();
                if self
                    .accept_offer(&dir, id, &name, size, Some(drag), None, out_tx)
                    .await
                {
                    if let Some(inc) = self.drag.as_mut() {
                        inc.outstanding.insert(id);
                    }
                } else {
                    // a rejected offer has settled; the drop may now be complete
                    self.try_finalize_drag().await;
                }
            }
            SyncMessage::DragDrop { drag } => {
                if let Some(inc) = self.drag.as_mut().filter(|d| d.drag == drag) {
                    inc.drop_requested = true;
                    self.try_finalize_drag().await;
                }
            }
            SyncMessage::DragCancel { drag } => {
                if self.drag.as_ref().is_some_and(|d| d.drag == drag) {
                    let d = self.drag.take().expect("checked");
                    log::info!("drag {drag} cancelled by peer; discarding staged files");
                    self.discard_drag(d).await;
                }
            }

            /* ---- folders ---- */
            SyncMessage::TreeOffer {
                tree,
                name,
                files,
                bytes,
                drag,
            } => self.tree_offer(tree, name, files, bytes, drag).await,
            SyncMessage::TreeDir { tree, rel } => {
                let Some(t) = self.trees.get_mut(&tree) else { return };
                if t.failed.is_some() {
                    return;
                }
                t.entries += 1;
                if t.entries > files::MAX_TREE_ENTRIES {
                    t.fail("folder has too many entries".to_string());
                    return;
                }
                let Some(rel) = files::sanitize_rel_path(&rel) else {
                    t.fail(format!("invalid path inside folder: {rel}"));
                    return;
                };
                let path = t.root.join(rel);
                if let Err(e) = tokio::fs::create_dir_all(&path).await {
                    t.fail(format!("cannot create {}: {e}", path.display()));
                }
            }
            SyncMessage::TreeFile {
                tree,
                id,
                rel,
                size,
            } => self.tree_file(tree, id, rel, size, out_tx).await,
            SyncMessage::TreeEnd { tree, error } => {
                if let Some(t) = self.trees.get_mut(&tree) {
                    t.ended = true;
                    if let Some(reason) = error {
                        t.fail(format!("sender: {reason}"));
                    }
                    self.try_finalize_tree(tree).await;
                }
            }

            /* ---- per-file stream (shared by all three kinds) ---- */
            SyncMessage::FileChunk { id, offset, data } => self.chunk(id, offset, data, out_tx).await,
            SyncMessage::FileDone { id, sha256 } => self.done(id, sha256).await,
            SyncMessage::FileCancel { id, reason } => {
                if let Some(t) = self.recv.remove(&id) {
                    if t.tree.is_none() {
                        self.events.send(SyncEvent::Transfer(files::status(
                            id,
                            &t.name,
                            Direction::Received,
                            t.received,
                            t.size,
                            TransferState::Failed,
                            Some(reason.clone()),
                        )));
                    }
                    let _ = tokio::fs::remove_file(&t.part_path).await;
                    self.settle_failure(&t, id, reason).await;
                }
            }
            other => log::debug!("inbox ignoring {other:?}"),
        }
    }

    /// Accept (or reject) an incoming file offer into `dir`, replying to
    /// the peer and — for standalone files and drag files — reporting it.
    /// Returns whether the transfer was accepted and registered.
    #[allow(clippy::too_many_arguments)]
    async fn accept_offer(
        &mut self,
        dir: &Path,
        id: u64,
        name: &str,
        size: u64,
        drag: Option<u64>,
        tree: Option<u64>,
        out_tx: &mpsc::Sender<SyncMessage>,
    ) -> bool {
        // reject a reused id rather than orphaning the in-flight transfer's
        // .vylopart file
        if self.recv.contains_key(&id) {
            let _ = out_tx
                .send(SyncMessage::FileReject {
                    id,
                    reason: "transfer id already in use".to_string(),
                })
                .await;
            return false;
        }
        match files::begin_recv(dir, name, size, drag, tree).await {
            Ok(transfer) => {
                if tree.is_none() {
                    self.events.send(SyncEvent::Transfer(files::status(
                        id,
                        &transfer.name,
                        Direction::Received,
                        0,
                        size,
                        TransferState::Active,
                        None,
                    )));
                }
                self.recv.insert(id, transfer);
                let _ = out_tx.send(SyncMessage::FileAccept { id }).await;
                true
            }
            Err(reason) => {
                if tree.is_none() {
                    self.events.send(SyncEvent::Transfer(files::status(
                        id,
                        name,
                        Direction::Received,
                        0,
                        size,
                        TransferState::Failed,
                        Some(reason.clone()),
                    )));
                }
                let _ = out_tx.send(SyncMessage::FileReject { id, reason }).await;
                false
            }
        }
    }

    async fn tree_offer(&mut self, tree: u64, name: String, files: u32, bytes: u64, drag: Option<u64>) {
        if self.trees.contains_key(&tree) {
            log::warn!("folder transfer {tree} offered twice; ignoring");
            return;
        }
        // Where the tree stages: inside the drag's staging dir when it is
        // part of a drag (the drag owns cleanup), else in its own dir.
        let (parent, staging) = match drag {
            Some(d) => match self.drag.as_mut().filter(|inc| inc.drag == d) {
                Some(inc) => {
                    inc.offered += 1;
                    inc.outstanding.insert(tree);
                    (inc.dir.clone(), None)
                }
                None => {
                    log::warn!("folder {tree} belongs to unknown drag {d}; refusing");
                    self.trees.insert(
                        tree,
                        refused_tree(tree, &name, files, bytes, drag, "no drag in progress"),
                    );
                    return;
                }
            },
            None => {
                let dir = self.file_dir().join(files::tree_staging_name(tree));
                (dir.clone(), Some(dir))
            }
        };
        let safe_name = match files::sanitize_name(&name) {
            Some(n) => n,
            None => {
                self.trees.insert(
                    tree,
                    refused_tree(tree, &name, files, bytes, drag, "invalid folder name"),
                );
                self.events.send(SyncEvent::Transfer(files::tree_status(
                    tree,
                    &name,
                    files,
                    Direction::Received,
                    0,
                    bytes,
                    TransferState::Failed,
                    Some("invalid folder name".to_string()),
                )));
                return;
            }
        };
        if let Err(e) = tokio::fs::create_dir_all(&parent).await {
            let reason = format!("cannot create {}: {e}", parent.display());
            self.trees
                .insert(tree, refused_tree(tree, &safe_name, files, bytes, drag, &reason));
            self.events.send(SyncEvent::Transfer(files::tree_status(
                tree,
                &safe_name,
                files,
                Direction::Received,
                0,
                bytes,
                TransferState::Failed,
                Some(reason),
            )));
            return;
        }
        let root = files::unique_dir_path(&parent, &safe_name);
        let mut t = IncomingTree {
            tree,
            name: safe_name,
            root: root.clone(),
            staging,
            drag,
            files,
            total: bytes,
            done_bytes: 0,
            outstanding: HashSet::new(),
            entries: 0,
            ended: false,
            failed: None,
            last_progress: Instant::now(),
        };
        if let Err(e) = tokio::fs::create_dir_all(&root).await {
            t.fail(format!("cannot create {}: {e}", root.display()));
        }
        log::info!("incoming folder {tree}: {} ({files} files)", t.name);
        self.events
            .send(SyncEvent::Transfer(t.status(0, TransferState::Active, None)));
        self.trees.insert(tree, t);
    }

    async fn tree_file(
        &mut self,
        tree: u64,
        id: u64,
        rel: String,
        size: u64,
        out_tx: &mpsc::Sender<SyncMessage>,
    ) {
        let Some(t) = self.trees.get_mut(&tree) else {
            let _ = out_tx
                .send(SyncMessage::FileReject {
                    id,
                    reason: "no such folder transfer".to_string(),
                })
                .await;
            return;
        };
        if let Some(reason) = t.failed.clone() {
            let _ = out_tx
                .send(SyncMessage::FileReject {
                    id,
                    reason: format!("folder transfer failed: {reason}"),
                })
                .await;
            return;
        }
        t.entries += 1;
        if t.entries > files::MAX_TREE_ENTRIES {
            t.fail("folder has too many entries".to_string());
            let _ = out_tx
                .send(SyncMessage::FileReject {
                    id,
                    reason: "folder has too many entries".to_string(),
                })
                .await;
            return;
        }
        let Some(rel_path) = files::sanitize_rel_path(&rel) else {
            let reason = format!("invalid path inside folder: {rel}");
            t.fail(reason.clone());
            let _ = out_tx.send(SyncMessage::FileReject { id, reason }).await;
            return;
        };
        let dir = match rel_path.parent() {
            Some(p) => t.root.join(p),
            None => t.root.clone(),
        };
        let name = rel_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let drag = t.drag;
        if self
            .accept_offer(&dir, id, &name, size, drag, Some(tree), out_tx)
            .await
        {
            if let Some(t) = self.trees.get_mut(&tree) {
                t.outstanding.insert(id);
            }
        } else if let Some(t) = self.trees.get_mut(&tree) {
            t.fail(format!("could not receive {rel}"));
        }
    }

    async fn chunk(
        &mut self,
        id: u64,
        offset: u64,
        data: Vec<u8>,
        out_tx: &mpsc::Sender<SyncMessage>,
    ) {
        use sha2::Digest;
        use tokio::io::AsyncWriteExt;
        let Some(transfer) = self.recv.get_mut(&id) else {
            return;
        };
        // Bound the write by the offered size and per-chunk size:
        // without this a peer could stream unbounded chunks (or a
        // huge single frame) and fill the receiver's disk.
        let error = if data.len() > proto::CHUNK_SIZE
            || transfer.received + data.len() as u64 > transfer.size
        {
            Some("transfer exceeds offered size".to_string())
        } else if offset != transfer.received {
            Some("chunks out of order".to_string())
        } else {
            transfer.hasher.update(&data);
            match transfer.file.write_all(&data).await {
                Ok(()) => {
                    transfer.received += data.len() as u64;
                    None
                }
                Err(e) => Some(format!("write failed: {e}")),
            }
        };
        if let Some(reason) = error {
            let t = self.recv.remove(&id).expect("transfer");
            if t.tree.is_none() {
                self.events.send(SyncEvent::Transfer(files::status(
                    id,
                    &t.name,
                    Direction::Received,
                    t.received,
                    t.size,
                    TransferState::Failed,
                    Some(reason.clone()),
                )));
            }
            let _ = out_tx
                .send(SyncMessage::FileCancel {
                    id,
                    reason: reason.clone(),
                })
                .await;
            let _ = tokio::fs::remove_file(&t.part_path).await;
            self.settle_failure(&t, id, reason).await;
            return;
        }
        // progress: the file's own row, or its folder's aggregate
        let transfer = self.recv.get_mut(&id).expect("transfer");
        match transfer.tree {
            None => {
                if transfer.progress_due() {
                    self.events.send(SyncEvent::Transfer(files::status(
                        id,
                        &transfer.name,
                        Direction::Received,
                        transfer.received,
                        transfer.size,
                        TransferState::Active,
                        None,
                    )));
                }
            }
            Some(tree) => {
                if let Some(t) = self.trees.get_mut(&tree) {
                    if t.progress_due() {
                        let inflight: u64 = t
                            .outstanding
                            .iter()
                            .filter_map(|fid| self.recv.get(fid))
                            .map(|r| r.received)
                            .sum();
                        let transferred = t.done_bytes + inflight;
                        self.events.send(SyncEvent::Transfer(t.status(
                            transferred,
                            TransferState::Active,
                            None,
                        )));
                    }
                }
            }
        }
    }

    async fn done(&mut self, id: u64, sha256: [u8; 32]) {
        let Some(transfer) = self.recv.remove(&id) else {
            return;
        };
        let name = transfer.name.clone();
        let size = transfer.size;
        let drag = transfer.drag;
        let tree = transfer.tree;
        let result = files::finish_recv(transfer, sha256).await;
        if tree.is_none() {
            match &result {
                Ok(path) => self.events.send(SyncEvent::Transfer(files::status(
                    id,
                    &name,
                    Direction::Received,
                    size,
                    size,
                    TransferState::Done,
                    Some(path.display().to_string()),
                ))),
                Err(reason) => self.events.send(SyncEvent::Transfer(files::status(
                    id,
                    &name,
                    Direction::Received,
                    size,
                    size,
                    TransferState::Failed,
                    Some(reason.clone()),
                ))),
            }
        }
        match tree {
            // a file inside a folder settled
            Some(tree) => {
                if let Some(t) = self.trees.get_mut(&tree) {
                    t.outstanding.remove(&id);
                    match result {
                        Ok(_) => t.done_bytes += size,
                        Err(reason) => t.fail(format!("{name}: {reason}")),
                    }
                    self.try_finalize_tree(tree).await;
                }
            }
            // a staged drag file settled (done or failed)
            None => {
                if let (Some(drag), Some(inc)) = (drag, self.drag.as_mut()) {
                    if inc.drag == drag {
                        inc.outstanding.remove(&id);
                        if let Ok(path) = result {
                            inc.done.push(StagedItem {
                                id,
                                name,
                                kind: TransferKind::File,
                                files: 0,
                                size,
                                path,
                            });
                        }
                        self.try_finalize_drag().await;
                    }
                }
            }
        }
    }

    /// a file failed mid-transfer: take it off whatever it belonged to
    async fn settle_failure(&mut self, t: &RecvTransfer, id: u64, reason: String) {
        if let Some(tree) = t.tree {
            if let Some(tr) = self.trees.get_mut(&tree) {
                tr.outstanding.remove(&id);
                tr.fail(format!("{}: {reason}", t.name));
                self.try_finalize_tree(tree).await;
            }
        } else if let (Some(drag), Some(inc)) = (t.drag, self.drag.as_mut()) {
            if inc.drag == drag {
                inc.outstanding.remove(&id);
                self.try_finalize_drag().await;
            }
        }
    }

    /// Once the sender closed the folder and every file settled: rename
    /// the finished tree into place (or hand it to its drag), or discard
    /// it if anything failed.
    async fn try_finalize_tree(&mut self, tree: u64) {
        if !self.trees.get(&tree).is_some_and(|t| t.settled()) {
            return;
        }
        let t = self.trees.remove(&tree).expect("checked");
        if let Some(reason) = t.failed.clone() {
            log::warn!("folder {} failed: {reason}", t.name);
            // (a refused tree never got a root)
            if !t.root.as_os_str().is_empty() {
                let _ = tokio::fs::remove_dir_all(&t.root).await;
            }
            if let Some(staging) = &t.staging {
                let _ = tokio::fs::remove_dir_all(staging).await;
            }
            self.events.send(SyncEvent::Transfer(t.status(
                t.done_bytes,
                TransferState::Failed,
                Some(reason),
            )));
            if let (Some(drag), Some(inc)) = (t.drag, self.drag.as_mut()) {
                if inc.drag == drag {
                    inc.outstanding.remove(&tree);
                    self.try_finalize_drag().await;
                }
            }
            return;
        }
        match t.drag {
            // inside a drag: stays staged until the drop
            Some(drag) => {
                self.events.send(SyncEvent::Transfer(t.status(
                    t.total,
                    TransferState::Done,
                    Some(t.root.display().to_string()),
                )));
                if let Some(inc) = self.drag.as_mut().filter(|d| d.drag == drag) {
                    inc.outstanding.remove(&tree);
                    inc.done.push(StagedItem {
                        id: tree,
                        name: t.name.clone(),
                        kind: TransferKind::Folder,
                        files: t.files,
                        size: t.total,
                        path: t.root.clone(),
                    });
                    self.try_finalize_drag().await;
                } else {
                    // the drag is gone; nothing will move it, so drop it
                    let _ = tokio::fs::remove_dir_all(&t.root).await;
                }
            }
            None => {
                let dest_dir = self.file_dir();
                if let Err(e) = tokio::fs::create_dir_all(&dest_dir).await {
                    log::warn!("cannot create {}: {e}", dest_dir.display());
                }
                let dest = files::unique_dir_path(&dest_dir, &t.name);
                let outcome = files::move_path(&t.root, &dest).await;
                if let Some(staging) = &t.staging {
                    let _ = tokio::fs::remove_dir_all(staging).await;
                }
                match outcome {
                    Ok(()) => {
                        log::info!("received folder {} ({} files)", dest.display(), t.files);
                        self.events.send(SyncEvent::Transfer(t.status(
                            t.total,
                            TransferState::Done,
                            Some(dest.display().to_string()),
                        )));
                    }
                    Err(e) => {
                        let _ = tokio::fs::remove_dir_all(&t.root).await;
                        self.events.send(SyncEvent::Transfer(t.status(
                            t.done_bytes,
                            TransferState::Failed,
                            Some(format!("cannot place folder at {}: {e}", dest.display())),
                        )));
                    }
                }
            }
        }
    }

    /// If the drop was requested and every item has settled, move the
    /// staged items into place, remove the staging dir and report them.
    async fn try_finalize_drag(&mut self) {
        let ready = self
            .drag
            .as_ref()
            .is_some_and(|d| d.drop_requested && d.settled());
        if !ready {
            return;
        }
        let d = self.drag.take().expect("checked");
        let dest = self
            .drop_dir
            .clone()
            .unwrap_or_else(|| files::drop_dir(&self.file_dir()));
        if let Err(e) = tokio::fs::create_dir_all(&dest).await {
            log::warn!("cannot create drop dir {}: {e}", dest.display());
        }
        let mut placed = Vec::new();
        for item in d.done {
            let target = match item.kind {
                TransferKind::Folder => files::unique_dir_path(&dest, &item.name),
                TransferKind::File => files::unique_path(&dest, &item.name),
            };
            match files::move_path(&item.path, &target).await {
                Ok(()) => {
                    // the row's "Show" should point at where it ended up
                    self.events
                        .send(SyncEvent::Transfer(lan_mouse_ipc::FileTransferStatus {
                            id: item.id,
                            name: item.name.clone(),
                            direction: Direction::Received,
                            transferred: item.size,
                            total: item.size,
                            state: TransferState::Done,
                            detail: Some(target.display().to_string()),
                            kind: item.kind,
                            files: item.files,
                        }));
                    placed.push(target);
                }
                Err(e) => log::warn!("cannot place dropped item {}: {e}", target.display()),
            }
        }
        let _ = tokio::fs::remove_dir_all(&d.dir).await;
        if !placed.is_empty() {
            log::info!("dropped {} item(s) into {}", placed.len(), dest.display());
            self.events.send(SyncEvent::DragDropped { paths: placed });
        }
    }

    /// throw away a staged drag and everything in flight for it
    async fn discard_drag(&mut self, d: IncomingDrag) {
        // items that had finished into staging are gone with it
        for item in &d.done {
            self.events
                .send(SyncEvent::Transfer(lan_mouse_ipc::FileTransferStatus {
                    id: item.id,
                    name: item.name.clone(),
                    direction: Direction::Received,
                    transferred: item.size,
                    total: item.size,
                    state: TransferState::Failed,
                    detail: Some("drag cancelled".to_string()),
                    kind: item.kind,
                    files: item.files,
                }));
        }
        // in-flight files offered directly to the drag
        for id in &d.outstanding {
            if let Some(t) = self.recv.remove(id) {
                let _ = tokio::fs::remove_file(&t.part_path).await;
            }
        }
        // folders inside the drag, and their in-flight files
        let tree_ids: Vec<u64> = self
            .trees
            .iter()
            .filter(|(_, t)| t.drag == Some(d.drag))
            .map(|(id, _)| *id)
            .collect();
        for tree in tree_ids {
            if let Some(t) = self.trees.remove(&tree) {
                for id in &t.outstanding {
                    if let Some(r) = self.recv.remove(id) {
                        let _ = tokio::fs::remove_file(&r.part_path).await;
                    }
                }
                self.events.send(SyncEvent::Transfer(t.status(
                    t.done_bytes,
                    TransferState::Failed,
                    Some("drag cancelled".to_string()),
                )));
            }
        }
        let _ = tokio::fs::remove_dir_all(&d.dir).await;
    }

    /// the connection is gone: nothing in flight can complete
    pub(crate) async fn close(mut self) {
        for (id, t) in self.recv.drain() {
            if t.tree.is_none() {
                self.events.send(SyncEvent::Transfer(files::status(
                    id,
                    &t.name,
                    Direction::Received,
                    t.received,
                    t.size,
                    TransferState::Failed,
                    Some("connection closed".to_string()),
                )));
            }
            let _ = tokio::fs::remove_file(&t.part_path).await;
        }
        for (_, t) in self.trees.drain() {
            log::info!("folder {} abandoned: connection closed", t.name);
            let _ = tokio::fs::remove_dir_all(&t.root).await;
            if let Some(staging) = &t.staging {
                let _ = tokio::fs::remove_dir_all(staging).await;
            }
            self.events.send(SyncEvent::Transfer(t.status(
                t.done_bytes,
                TransferState::Failed,
                Some("connection closed".to_string()),
            )));
        }
        // a drag can't survive its connection: discard whatever was staged
        if let Some(d) = self.drag.take() {
            log::info!("drag {} abandoned: connection closed", d.drag);
            let _ = tokio::fs::remove_dir_all(&d.dir).await;
        }
    }
}

/// a tree we will never accept files for; it still has to exist so the
/// sender's `TreeFile`s are rejected with a reason and `TreeEnd` is
/// consumed cleanly
fn refused_tree(
    tree: u64,
    name: &str,
    files: u32,
    bytes: u64,
    drag: Option<u64>,
    reason: &str,
) -> IncomingTree {
    IncomingTree {
        tree,
        name: name.to_string(),
        root: PathBuf::new(),
        staging: None,
        drag,
        files,
        total: bytes,
        done_bytes: 0,
        outstanding: HashSet::new(),
        entries: 0,
        ended: false,
        failed: Some(reason.to_string()),
        last_progress: Instant::now(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::files::{Offer, Outgoing, Report, send_file, send_tree};
    use std::collections::BTreeMap;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("vylo-inbox-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn events() -> (EventSender, local_channel::mpsc::Receiver<SyncEvent>) {
        let (tx, rx) = local_channel::mpsc::channel();
        (EventSender(tx), rx)
    }

    /// every file under `root` as rel path -> contents
    fn snapshot(root: &Path) -> BTreeMap<String, Vec<u8>> {
        let mut out = BTreeMap::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).unwrap() {
                let e = e.unwrap();
                let p = e.path();
                let rel = p
                    .strip_prefix(root)
                    .unwrap()
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy().to_string())
                    .collect::<Vec<_>>()
                    .join("/");
                if p.is_dir() {
                    out.insert(format!("{rel}/"), Vec::new());
                    stack.push(p);
                } else {
                    out.insert(rel, std::fs::read(&p).unwrap());
                }
            }
        }
        out
    }

    fn build_source(base: &Path) -> PathBuf {
        let root = base.join("Photos.2024");
        std::fs::create_dir_all(root.join("trip/empty")).unwrap();
        std::fs::create_dir_all(root.join("deep/x/y")).unwrap();
        std::fs::write(root.join("a.txt"), b"hello folder").unwrap();
        // several chunks' worth so the chunk loop is exercised
        let big: Vec<u8> = (0..(proto::CHUNK_SIZE * 2 + 12345))
            .map(|i| (i % 251) as u8)
            .collect();
        std::fs::write(root.join("trip/big.bin"), &big).unwrap();
        std::fs::write(root.join("deep/x/y/z.txt"), b"deep").unwrap();
        std::fs::write(root.join("trip/nul.txt"), b"reserved on windows").unwrap();
        root
    }

    /// Pump a sender task against an inbox until the sender finishes and
    /// nothing is left in either direction.
    async fn pump(
        mut inbox: Inbox,
        mut out_rx: mpsc::Receiver<SyncMessage>,
        reply_tx: mpsc::Sender<SyncMessage>,
        mut reply_rx: mpsc::Receiver<SyncMessage>,
        outgoing: Outgoing,
        mut sender: tokio::task::JoinHandle<()>,
        // extra messages the test injects after the sender is done
        tail: Vec<SyncMessage>,
    ) -> Inbox {
        let route = |outgoing: &Outgoing, m: SyncMessage| match m {
            SyncMessage::FileAccept { id } => outgoing.accepted(id),
            SyncMessage::FileReject { id, reason } => outgoing.rejected(id, reason),
            SyncMessage::FileCancel { id, reason } => {
                outgoing.aborted(id, reason);
            }
            other => panic!("unexpected reply {other:?}"),
        };
        let mut done = false;
        while !done {
            tokio::select! {
                Some(m) = out_rx.recv() => inbox.handle(m, &reply_tx).await,
                Some(r) = reply_rx.recv() => route(&outgoing, r),
                _ = &mut sender => done = true,
            }
        }
        loop {
            let mut idle = true;
            while let Ok(m) = out_rx.try_recv() {
                idle = false;
                inbox.handle(m, &reply_tx).await;
            }
            while let Ok(r) = reply_rx.try_recv() {
                idle = false;
                route(&outgoing, r);
            }
            if idle {
                break;
            }
        }
        for m in tail {
            inbox.handle(m, &reply_tx).await;
        }
        while let Ok(r) = reply_rx.try_recv() {
            route(&outgoing, r);
        }
        inbox
    }

    /// everything queued on the event channel right now
    async fn drain_all(rx: &mut local_channel::mpsc::Receiver<SyncEvent>) -> Vec<SyncEvent> {
        use futures::StreamExt;
        let mut out = Vec::new();
        loop {
            let next = std::future::poll_fn(|cx| std::task::Poll::Ready(rx.poll_next_unpin(cx))).await;
            match next {
                std::task::Poll::Ready(Some(ev)) => out.push(ev),
                _ => break,
            }
        }
        out
    }

    async fn drain(rx: &mut local_channel::mpsc::Receiver<SyncEvent>) -> Vec<lan_mouse_ipc::FileTransferStatus> {
        drain_all(rx)
            .await
            .into_iter()
            .filter_map(|ev| match ev {
                SyncEvent::Transfer(s) => Some(s),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn folder_arrives_whole_with_structure_intact() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let base = scratch("roundtrip");
                let src = build_source(&base);
                let dst = base.join("recv");
                let (events, mut ev_rx) = events();
                let (out_tx, out_rx) = mpsc::channel(8);
                let (reply_tx, reply_rx) = mpsc::channel(8);
                let outgoing = Outgoing::default();
                let inbox = Inbox::new(Rc::new(RefCell::new(dst.clone())), events.clone());

                let sender = tokio::task::spawn_local(send_tree(
                    7,
                    src.clone(),
                    None,
                    out_tx.clone(),
                    outgoing.clone(),
                    events.clone(),
                ));
                let inbox = pump(inbox, out_rx, reply_tx, reply_rx, outgoing, sender, vec![]).await;
                drop(inbox);

                // delivered under the receive dir, byte-for-byte, empty dirs too
                let got = dst.join("Photos.2024");
                assert!(got.is_dir(), "folder must land in the receive dir");
                let mut expected = snapshot(&src);
                // the Windows-reserved name is neutralised on arrival
                let v = expected.remove("trip/nul.txt").unwrap();
                expected.insert("trip/_nul.txt".to_string(), v);
                assert_eq!(snapshot(&got), expected);
                // no staging left behind, no stray .vylopart files
                let leftovers: Vec<_> = std::fs::read_dir(&dst)
                    .unwrap()
                    .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
                    .filter(|n| n.starts_with(".vylo"))
                    .collect();
                assert!(leftovers.is_empty(), "staging left behind: {leftovers:?}");

                // one folder row per side, never per-file rows
                let statuses = drain(&mut ev_rx).await;
                assert!(!statuses.is_empty());
                assert!(
                    statuses.iter().all(|s| s.kind == TransferKind::Folder && s.id == 7),
                    "only the folder row may be reported: {statuses:?}"
                );
                let recv_done = statuses
                    .iter()
                    .find(|s| s.direction == Direction::Received && s.state == TransferState::Done)
                    .expect("receiver reports Done");
                assert_eq!(recv_done.files, 4);
                assert_eq!(recv_done.detail.as_deref(), Some(got.to_str().unwrap()));
                assert_eq!(recv_done.transferred, recv_done.total);
                assert!(statuses
                    .iter()
                    .any(|s| s.direction == Direction::Sent && s.state == TransferState::Done));
                let _ = std::fs::remove_dir_all(&base);
            })
            .await;
    }

    #[tokio::test]
    async fn folder_inside_a_drag_lands_on_drop() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let base = scratch("drag");
                let src = build_source(&base);
                let dst = base.join("recv");
                let desktop = base.join("desktop");
                let (events, mut ev_rx) = events();
                let (out_tx, out_rx) = mpsc::channel(8);
                let (reply_tx, reply_rx) = mpsc::channel(8);
                let outgoing = Outgoing::default();
                let mut inbox = Inbox::new(Rc::new(RefCell::new(dst.clone())), events.clone())
                    .with_drop_dir(desktop.clone());
                let drag = 99;
                inbox
                    .handle(SyncMessage::DragBegin { drag, count: 2 }, &reply_tx)
                    .await;

                // a folder and a loose file dragged together
                let loose = base.join("note.txt");
                std::fs::write(&loose, b"loose").unwrap();
                let sender = tokio::task::spawn_local({
                    let (out_tx, outgoing, events) = (out_tx.clone(), outgoing.clone(), events.clone());
                    async move {
                        let tree = send_tree(7, src, Some(drag), out_tx.clone(), outgoing.clone(), events.clone());
                        let file = send_file(
                            8,
                            loose,
                            out_tx,
                            outgoing,
                            Offer::Drag(drag),
                            Report::Single(events),
                        );
                        let _ = tokio::join!(tree, file);
                    }
                });
                // the user releases the button after everything arrived
                let inbox = pump(
                    inbox,
                    out_rx,
                    reply_tx,
                    reply_rx,
                    outgoing,
                    sender,
                    vec![SyncMessage::DragDrop { drag }],
                )
                .await;
                drop(inbox);

                assert!(desktop.join("Photos.2024").is_dir());
                assert!(desktop.join("Photos.2024/trip/empty").is_dir());
                assert_eq!(std::fs::read(desktop.join("note.txt")).unwrap(), b"loose");
                assert!(!dst.exists() || std::fs::read_dir(&dst).unwrap().next().is_none(), "staging removed");

                let mut dropped = None;
                let mut final_rows = Vec::new();
                for ev in drain_all(&mut ev_rx).await {
                    match ev {
                        SyncEvent::DragDropped { paths } => dropped = Some(paths),
                        SyncEvent::Transfer(s)
                            if s.direction == Direction::Received && s.state == TransferState::Done =>
                        {
                            final_rows.push(s)
                        }
                        _ => {}
                    }
                }
                let mut dropped = dropped.expect("DragDropped emitted");
                dropped.sort();
                assert_eq!(
                    dropped,
                    vec![desktop.join("Photos.2024"), desktop.join("note.txt")]
                );
                // rows end up pointing at the final locations
                assert!(final_rows.iter().any(|s| s.kind == TransferKind::Folder
                    && s.detail.as_deref() == Some(desktop.join("Photos.2024").to_str().unwrap())));
                assert!(final_rows.iter().any(|s| s.kind == TransferKind::File
                    && s.detail.as_deref() == Some(desktop.join("note.txt").to_str().unwrap())));
                let _ = std::fs::remove_dir_all(&base);
            })
            .await;
    }

    #[tokio::test]
    async fn unreadable_drag_items_do_not_strand_the_drop() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let base = scratch("strand");
                let dst = base.join("recv");
                let desktop = base.join("desktop");
                let (events, mut ev_rx) = events();
                let (out_tx, out_rx) = mpsc::channel(8);
                let (reply_tx, reply_rx) = mpsc::channel(8);
                let outgoing = Outgoing::default();
                let mut inbox = Inbox::new(Rc::new(RefCell::new(dst.clone())), events.clone())
                    .with_drop_dir(desktop.clone());
                let drag = 5;
                // three items: a missing file, a missing folder, and a good file
                inbox
                    .handle(SyncMessage::DragBegin { drag, count: 3 }, &reply_tx)
                    .await;
                let good = base.join("good.txt");
                std::fs::write(&good, b"still arrives").unwrap();
                let missing_file = base.join("nope.txt");
                let missing_dir = base.join("nope-dir");

                let sender = tokio::task::spawn_local({
                    let (out_tx, outgoing, events) = (out_tx.clone(), outgoing.clone(), events.clone());
                    async move {
                        let a = send_file(1, missing_file, out_tx.clone(), outgoing.clone(), Offer::Drag(drag), Report::Single(events.clone()));
                        let b = send_tree(2, missing_dir, Some(drag), out_tx.clone(), outgoing.clone(), events.clone());
                        let c = send_file(3, good, out_tx, outgoing, Offer::Drag(drag), Report::Single(events));
                        let _ = tokio::join!(a, b, c);
                    }
                });
                let inbox = pump(
                    inbox,
                    out_rx,
                    reply_tx,
                    reply_rx,
                    outgoing,
                    sender,
                    vec![SyncMessage::DragDrop { drag }],
                )
                .await;
                drop(inbox);

                // the good file still lands even though two items failed
                assert_eq!(std::fs::read(desktop.join("good.txt")).unwrap(), b"still arrives");
                let statuses = drain(&mut ev_rx).await;
                // the receiver learnt why the folder failed
                assert!(statuses.iter().any(|s| s.kind == TransferKind::Folder
                    && s.direction == Direction::Received
                    && s.state == TransferState::Failed
                    && s.detail.as_deref().is_some_and(|d| d.contains("cannot read folder"))));
                let _ = std::fs::remove_dir_all(&base);
            })
            .await;
    }

    #[tokio::test]
    async fn hostile_paths_fail_the_folder_and_touch_nothing_outside() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let base = scratch("hostile");
                let dst = base.join("recv");
                let (events, mut ev_rx) = events();
                let (reply_tx, mut reply_rx) = mpsc::channel(8);
                let mut inbox = Inbox::new(Rc::new(RefCell::new(dst.clone())), events);
                inbox
                    .handle(
                        SyncMessage::TreeOffer {
                            tree: 1,
                            name: "evil".into(),
                            files: 2,
                            bytes: 10,
                            drag: None,
                        },
                        &reply_tx,
                    )
                    .await;
                inbox
                    .handle(SyncMessage::TreeDir { tree: 1, rel: "../../escaped".into() }, &reply_tx)
                    .await;
                inbox
                    .handle(
                        SyncMessage::TreeFile { tree: 1, id: 11, rel: "../pwned.txt".into(), size: 5 },
                        &reply_tx,
                    )
                    .await;
                inbox
                    .handle(
                        SyncMessage::TreeFile { tree: 1, id: 12, rel: "fine.txt".into(), size: 5 },
                        &reply_tx,
                    )
                    .await;
                inbox
                    .handle(SyncMessage::TreeEnd { tree: 1, error: None }, &reply_tx)
                    .await;
                drop(inbox);

                // both files refused (the second because the folder already failed)
                let mut rejects = 0;
                while let Ok(m) = reply_rx.try_recv() {
                    assert!(matches!(m, SyncMessage::FileReject { .. }), "{m:?}");
                    rejects += 1;
                }
                assert_eq!(rejects, 2);
                assert!(!base.join("escaped").exists());
                assert!(!base.join("pwned.txt").exists());
                assert!(!dst.join("evil").exists(), "failed folder must not be delivered");
                assert!(
                    std::fs::read_dir(&dst).map(|mut d| d.next().is_none()).unwrap_or(true),
                    "staging removed"
                );
                let statuses = drain(&mut ev_rx).await;
                assert!(statuses.iter().any(|s| s.state == TransferState::Failed
                    && s.kind == TransferKind::Folder));
                let _ = std::fs::remove_dir_all(&base);
            })
            .await;
    }

    #[tokio::test]
    async fn sender_stops_streaming_when_peer_cancels() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let base = scratch("abort");
                let big = base.join("big.bin");
                std::fs::write(&big, vec![7u8; proto::CHUNK_SIZE * 6]).unwrap();
                let (events, _ev_rx) = events();
                let (out_tx, mut out_rx) = mpsc::channel::<SyncMessage>(2);
                let outgoing = Outgoing::default();

                let task = tokio::task::spawn_local(send_file(
                    5,
                    big,
                    out_tx,
                    outgoing.clone(),
                    Offer::Plain,
                    Report::Single(events),
                ));
                // offer -> accept, then take one chunk and cancel
                assert!(matches!(out_rx.recv().await, Some(SyncMessage::FileOffer { id: 5, .. })));
                outgoing.accepted(5);
                assert!(matches!(out_rx.recv().await, Some(SyncMessage::FileChunk { id: 5, .. })));
                assert!(outgoing.aborted(5, "disk full".into()));
                let result = task.await.unwrap();
                assert_eq!(result, Err("disk full".to_string()));
                // whatever was already queued is bounded by the channel; no FileDone follows
                let mut rest = 0;
                while let Ok(m) = out_rx.try_recv() {
                    assert!(!matches!(m, SyncMessage::FileDone { .. }));
                    rest += 1;
                }
                assert!(rest <= 2, "sender must stop promptly, got {rest} more frames");
                let _ = std::fs::remove_dir_all(&base);
            })
            .await;
    }
}
