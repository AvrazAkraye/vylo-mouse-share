//! The Vylo sync side-channel: clipboard sync, file transfer, PIN
//! pairing and LAN discovery.
//!
//! Runs as an actor next to capture/emulation in the service's
//! single-threaded runtime: the service pushes [`SyncRequest`]s in and
//! polls [`SyncEvent`]s out of its `select!` loop. All bulk data flows
//! over a dedicated TCP+TLS connection (see [`tls`]), never over the
//! latency-sensitive UDP input channel.

mod clipboard;
mod discovery;
mod files;
mod pairing;
mod proto;
mod tls;

use crate::crypto;
use lan_mouse_ipc::{ClipboardKind, Direction, DiscoveredPeer, FileTransferStatus, TransferState};
use proto::{PROTO_VERSION, SyncMessage, read_msg, write_msg};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    net::IpAddr,
    path::PathBuf,
    rc::Rc,
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{
    io::{ReadHalf, WriteHalf},
    net::{TcpListener, TcpStream},
    sync::{mpsc, oneshot},
    task::{JoinHandle, spawn_local},
};
use tokio_rustls::{TlsStream, rustls::pki_types::ServerName};
use tokio_util::sync::CancellationToken;
use webrtc_dtls::crypto::Certificate;

const PAIRING_WINDOW: Duration = Duration::from_secs(120);
const DIAL_INTERVAL: Duration = Duration::from_secs(3);
const READ_TIMEOUT: Duration = Duration::from_secs(45);
const PING_INTERVAL: Duration = Duration::from_secs(15);
/// a single frame write must complete within this bound, else the peer
/// is treated as dead (prevents a non-reading peer wedging the writer)
const WRITE_TIMEOUT: Duration = Duration::from_secs(20);
/// a full PIN pairing exchange must complete within this bound
const PAIRING_EXCHANGE_TIMEOUT: Duration = Duration::from_secs(20);
const OUT_QUEUE: usize = 32;

static NEXT_TRANSFER_ID: AtomicU64 = AtomicU64::new(1);
fn next_transfer_id() -> u64 {
    NEXT_TRANSFER_ID.fetch_add(1, Ordering::Relaxed)
}

#[derive(Debug)]
pub(crate) enum SyncRequest {
    StartPairing,
    PairWithPeer {
        addr: String,
        pin: String,
    },
    CancelPairing,
    SendFiles(Vec<PathBuf>),
    SetClipboardSync(bool),
    SetFileDir(PathBuf),
    SetDeviceName(String),
    /// an address the peer was seen at (from the input channel);
    /// used as a dial candidate for the sync connection
    AddrHint(IpAddr),
}

#[derive(Debug)]
pub(crate) enum SyncEvent {
    Status {
        connected: bool,
        peer_name: Option<String>,
    },
    PairingStarted {
        pin: String,
        port: u16,
    },
    PairingComplete {
        fingerprint: String,
        name: String,
        addr: IpAddr,
        /// true when this machine dialed (the user typed the PIN here)
        initiated: bool,
    },
    PairingFailed(String),
    PeersDiscovered(Vec<DiscoveredPeer>),
    Clipboard {
        direction: Direction,
        kind: ClipboardKind,
    },
    Transfer(FileTransferStatus),
}

#[derive(Clone)]
pub(crate) struct EventSender(local_channel::mpsc::Sender<SyncEvent>);

impl EventSender {
    pub(crate) fn send(&self, event: SyncEvent) {
        let _ = self.0.send(event);
    }
}

pub(crate) struct SyncOptions {
    pub(crate) sync_port: u16,
    pub(crate) clipboard_sync: bool,
    pub(crate) file_dir: PathBuf,
    pub(crate) device_name: String,
}

pub(crate) struct VyloSync {
    request_tx: local_channel::mpsc::Sender<SyncRequest>,
    event_rx: local_channel::mpsc::Receiver<SyncEvent>,
    task: JoinHandle<()>,
}

impl VyloSync {
    pub(crate) fn new(
        cert: Certificate,
        authorized: Arc<RwLock<HashMap<String, String>>>,
        opts: SyncOptions,
    ) -> Self {
        let (request_tx, request_rx) = local_channel::mpsc::channel();
        let (event_tx, event_rx) = local_channel::mpsc::channel();
        let task = spawn_local(run_actor(
            cert,
            authorized,
            opts,
            request_rx,
            EventSender(event_tx),
        ));
        Self {
            request_tx,
            event_rx,
            task,
        }
    }

    pub(crate) fn request(&self, request: SyncRequest) {
        let _ = self.request_tx.send(request);
    }

    pub(crate) async fn event(&mut self) -> SyncEvent {
        match self.event_rx.recv().await {
            Some(e) => e,
            None => std::future::pending().await,
        }
    }

    pub(crate) async fn terminate(&mut self) {
        self.request_tx.close();
        let _ = (&mut self.task).await;
    }
}

/* ------------------------- actor internals ------------------------- */

enum Internal {
    Established {
        id: u64,
        out_tx: mpsc::Sender<SyncMessage>,
        ctl_tx: mpsc::UnboundedSender<ConnCtl>,
        peer_fp: String,
        peer_name: String,
        outbound: bool,
        cancel: CancellationToken,
    },
    ConnClosed {
        id: u64,
    },
    PairingOk {
        fingerprint: String,
        name: String,
        addr: IpAddr,
        initiated: bool,
    },
    PairingErr {
        msg: String,
        initiated: bool,
    },
    DialFinished,
}

enum ConnCtl {
    SendFiles(Vec<PathBuf>),
}

struct ActiveConn {
    id: u64,
    out_tx: mpsc::Sender<SyncMessage>,
    ctl_tx: mpsc::UnboundedSender<ConnCtl>,
    peer_fp: String,
    peer_name: String,
    outbound: bool,
    cancel: CancellationToken,
}

struct Actor {
    authorized: Arc<RwLock<HashMap<String, String>>>,
    tls: tls::TlsConfigs,
    local_fp: String,
    sync_port: u16,
    device_name: String,
    file_dir: Rc<RefCell<PathBuf>>,
    clipboard_enabled: Arc<AtomicBool>,
    clipboard: Rc<clipboard::ClipboardMonitor>,
    pairing_open: Arc<AtomicBool>,
    /// pin of the open pairing window, if any
    pairing: Option<(String, Instant)>,
    /// true while an outbound pairing attempt is running
    pairing_dial: bool,
    events: EventSender,
    internal_tx: local_channel::mpsc::Sender<Internal>,
    active: Option<ActiveConn>,
    /// a local clipboard change that couldn't be enqueued yet (queue
    /// busy, e.g. during a file transfer); retried on the next tick so
    /// copies made mid-transfer aren't silently dropped
    pending_clip: Option<(SyncMessage, ClipboardKind)>,
    conn_seq: u64,
    candidates: HashSet<IpAddr>,
    dialing: bool,
    discovery: Option<discovery::Discovery>,
    peers_tx: mpsc::UnboundedSender<Vec<DiscoveredPeer>>,
}

async fn run_actor(
    cert: Certificate,
    authorized: Arc<RwLock<HashMap<String, String>>>,
    opts: SyncOptions,
    mut request_rx: local_channel::mpsc::Receiver<SyncRequest>,
    events: EventSender,
) {
    let local_fp = crypto::certificate_fingerprint(&cert);
    let pairing_open = Arc::new(AtomicBool::new(false));
    let tls = match tls::build_tls(&cert, authorized.clone(), pairing_open.clone()) {
        Ok(t) => t,
        Err(e) => {
            log::error!("sync channel disabled, tls setup failed: {e}");
            // drain requests so the service never blocks
            while request_rx.recv().await.is_some() {}
            return;
        }
    };

    let clipboard_enabled = Arc::new(AtomicBool::new(opts.clipboard_sync));
    let (change_tx, mut change_rx) = mpsc::unbounded_channel();
    let clipboard = Rc::new(clipboard::ClipboardMonitor::new(
        change_tx,
        clipboard_enabled.clone(),
    ));

    let (peers_tx, mut peers_rx) = mpsc::unbounded_channel();
    let (internal_tx, mut internal_rx) = local_channel::mpsc::channel();

    let mut actor = Actor {
        authorized,
        tls,
        local_fp: local_fp.clone(),
        sync_port: opts.sync_port,
        device_name: opts.device_name,
        file_dir: Rc::new(RefCell::new(opts.file_dir)),
        clipboard_enabled,
        clipboard,
        pairing_open,
        pairing: None,
        pairing_dial: false,
        events,
        internal_tx,
        active: None,
        pending_clip: None,
        conn_seq: 0,
        candidates: HashSet::new(),
        dialing: false,
        discovery: None,
        peers_tx,
    };
    actor.start_discovery();

    // Bind the listener without blocking the actor: if the port is
    // taken we still process requests (pairing as the dialing side,
    // config changes) and keep retrying the bind on each tick, rather
    // than hanging silently until the port frees up.
    let mut listener: Option<TcpListener> =
        match TcpListener::bind(("0.0.0.0", actor.sync_port)).await {
            Ok(l) => {
                log::info!("sync channel listening on tcp port {}", actor.sync_port);
                Some(l)
            }
            Err(e) => {
                log::error!(
                    "cannot listen on sync port {}: {e} — will keep retrying",
                    actor.sync_port
                );
                None
            }
        };

    let mut tick = tokio::time::interval(DIAL_INTERVAL);
    loop {
        tokio::select! {
            request = request_rx.recv() => match request {
                Some(r) => actor.handle_request(r),
                None => break,
            },
            accepted = async { listener.as_ref().unwrap().accept().await }, if listener.is_some() => {
                match accepted {
                    Ok((tcp, addr)) => actor.spawn_accept(tcp, addr.ip()),
                    Err(e) => log::warn!("sync accept failed: {e}"),
                }
            }
            Some(change) = change_rx.recv() => actor.handle_clip_change(change),
            Some(peers) = peers_rx.recv() => actor.events.send(SyncEvent::PeersDiscovered(peers)),
            Some(internal) = internal_rx.recv() => actor.handle_internal(internal),
            _ = tick.tick() => {
                if listener.is_none() {
                    if let Ok(l) = TcpListener::bind(("0.0.0.0", actor.sync_port)).await {
                        log::info!("sync channel now listening on tcp port {}", actor.sync_port);
                        listener = Some(l);
                    }
                }
                actor.tick();
            }
        }
    }

    if let Some(active) = actor.active.take() {
        active.cancel.cancel();
    }
    if let Some(d) = actor.discovery.take() {
        d.shutdown();
    }
}

impl Actor {
    fn start_discovery(&mut self) {
        match discovery::Discovery::new(
            &self.device_name,
            self.sync_port,
            &self.local_fp,
            self.peers_tx.clone(),
        ) {
            Ok(d) => self.discovery = Some(d),
            Err(e) => log::warn!("mdns discovery unavailable: {e}"),
        }
    }

    fn handle_request(&mut self, request: SyncRequest) {
        match request {
            SyncRequest::StartPairing => {
                let pin = pairing::generate_pin();
                self.pairing = Some((pin.clone(), Instant::now()));
                self.pairing_open.store(true, Ordering::SeqCst);
                self.events.send(SyncEvent::PairingStarted {
                    pin,
                    port: self.sync_port,
                });
            }
            SyncRequest::PairWithPeer { addr, pin } => self.spawn_pair_dial(addr, pin),
            SyncRequest::CancelPairing => {
                self.close_pairing_window();
                self.events
                    .send(SyncEvent::PairingFailed("cancelled".to_string()));
            }
            SyncRequest::SendFiles(paths) => match &self.active {
                Some(active) => {
                    let _ = active.ctl_tx.send(ConnCtl::SendFiles(paths));
                }
                None => {
                    for path in paths {
                        let name = path
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_else(|| path.display().to_string());
                        self.events.send(SyncEvent::Transfer(files::status(
                            next_transfer_id(),
                            &name,
                            Direction::Sent,
                            0,
                            0,
                            TransferState::Failed,
                            Some("no device connected".to_string()),
                        )));
                    }
                }
            },
            SyncRequest::SetClipboardSync(enabled) => {
                self.clipboard_enabled.store(enabled, Ordering::SeqCst);
            }
            SyncRequest::SetFileDir(dir) => *self.file_dir.borrow_mut() = dir,
            SyncRequest::SetDeviceName(name) => {
                if name != self.device_name {
                    self.device_name = name;
                    if let Some(d) = self.discovery.take() {
                        d.shutdown();
                    }
                    self.start_discovery();
                }
            }
            SyncRequest::AddrHint(ip) => {
                self.candidates.insert(ip);
            }
        }
    }

    fn close_pairing_window(&mut self) {
        self.pairing = None;
        if !self.pairing_dial {
            self.pairing_open.store(false, Ordering::SeqCst);
        }
    }

    fn tick(&mut self) {
        // expire the pairing window
        if let Some((_, opened)) = &self.pairing {
            if opened.elapsed() > PAIRING_WINDOW {
                self.close_pairing_window();
                self.events
                    .send(SyncEvent::PairingFailed("pairing window timed out".into()));
            }
        }
        self.flush_pending_clip();
        self.maybe_dial();
    }

    fn maybe_dial(&mut self) {
        if self.active.is_some() || self.dialing || self.candidates.is_empty() {
            return;
        }
        if self.authorized.read().expect("lock").is_empty() {
            return;
        }
        // Don't run ordinary dials while a pairing window is open: the
        // strict connector wouldn't admit an unpinned peer anyway, but
        // suppressing dials keeps the pairing exchange the only thing
        // touching an as-yet-untrusted peer.
        if self.pairing_open.load(Ordering::SeqCst) {
            return;
        }
        self.dialing = true;
        let candidates: Vec<IpAddr> = self.candidates.iter().copied().collect();
        let port = self.sync_port;
        let connector = self.tls.connector.clone();
        let internal = self.internal_tx.clone();
        let ctx = self.conn_ctx();
        spawn_local(async move {
            for ip in candidates {
                match dial(ip, port, &connector).await {
                    Ok(stream) => {
                        let _ = internal.send(Internal::DialFinished);
                        run_conn(stream, true, ctx).await;
                        return;
                    }
                    Err(e) => log::debug!("sync dial {ip}:{port} failed: {e}"),
                }
            }
            let _ = internal.send(Internal::DialFinished);
        });
    }

    /// everything a connection task needs from the actor
    fn conn_ctx(&mut self) -> ConnCtx {
        self.conn_seq += 1;
        ConnCtx {
            id: self.conn_seq,
            device_name: self.device_name.clone(),
            file_dir: self.file_dir.clone(),
            clipboard: self.clipboard.clone(),
            clipboard_enabled: self.clipboard_enabled.clone(),
            events: self.events.clone(),
            internal: self.internal_tx.clone(),
        }
    }

    fn spawn_accept(&mut self, tcp: TcpStream, addr: IpAddr) {
        let acceptor = self.tls.acceptor.clone();
        let authorized = self.authorized.clone();
        let pairing_pin = self.pairing.as_ref().map(|(pin, _)| pin.clone());
        let ctx = self.conn_ctx();
        spawn_local(async move {
            let _ = tcp.set_nodelay(true);
            let stream = match acceptor.accept(tcp).await {
                Ok(s) => TlsStream::Server(s),
                Err(e) => {
                    log::debug!("sync tls accept from {addr} failed: {e}");
                    return;
                }
            };
            let Some(peer_fp) = tls::peer_fingerprint(&stream) else {
                return;
            };
            let is_authorized = authorized.read().expect("lock").contains_key(&peer_fp);
            if is_authorized {
                run_conn(stream, false, ctx).await;
            } else if let Some(pin) = pairing_pin {
                // an unauthorized connection during an open pairing
                // window: this is the pairing attempt
                let internal = ctx.internal.clone();
                let device_name = ctx.device_name.clone();
                let exporter = match tls::exporter(&stream) {
                    Ok(e) => e,
                    Err(_) => return,
                };
                let (mut r, mut w) = tokio::io::split(stream);
                // bound the exchange so a stalling peer can't hold a
                // pairing task open indefinitely
                let result = tokio::time::timeout(
                    PAIRING_EXCHANGE_TIMEOUT,
                    pairing::run_responder(&mut r, &mut w, &exporter, &pin, &device_name),
                )
                .await
                .unwrap_or(Err(pairing::PairingError::UnexpectedMessage));
                match result {
                    Ok(peer_name) => {
                        let _ = internal.send(Internal::PairingOk {
                            fingerprint: peer_fp,
                            name: peer_name,
                            addr,
                            initiated: false,
                        });
                    }
                    Err(e) => {
                        let _ = internal.send(Internal::PairingErr {
                            msg: e.to_string(),
                            initiated: false,
                        });
                    }
                }
            }
        });
    }

    fn spawn_pair_dial(&mut self, addr: String, pin: String) {
        self.pairing_dial = true;
        self.pairing_open.store(true, Ordering::SeqCst);
        // the pairing connector admits the not-yet-pinned peer; the PIN
        // exchange in pair_dial is what establishes trust
        let connector = self.tls.pairing_connector.clone();
        let internal = self.internal_tx.clone();
        let device_name = self.device_name.clone();
        spawn_local(async move {
            let result = pair_dial(addr, pin, connector, device_name).await;
            let _ = internal.send(match result {
                Ok((fingerprint, name, ip)) => Internal::PairingOk {
                    fingerprint,
                    name,
                    addr: ip,
                    initiated: true,
                },
                Err(msg) => Internal::PairingErr {
                    msg,
                    initiated: true,
                },
            });
        });
    }

    fn handle_clip_change(&mut self, change: clipboard::Change) {
        let (msg, kind) = match change {
            clipboard::Change::Text(text) => (SyncMessage::ClipText { text }, ClipboardKind::Text),
            clipboard::Change::Image { width, height, png } => (
                SyncMessage::ClipImage { width, height, png },
                ClipboardKind::Image,
            ),
        };
        self.send_clip(msg, kind);
    }

    fn send_clip(&mut self, msg: SyncMessage, kind: ClipboardKind) {
        let Some(active) = &self.active else {
            // no peer yet: hold the latest change so it syncs on connect
            self.pending_clip = Some((msg, kind));
            return;
        };
        match active.out_tx.try_send(msg) {
            Ok(()) => {
                self.pending_clip = None;
                self.events.send(SyncEvent::Clipboard {
                    direction: Direction::Sent,
                    kind,
                });
            }
            Err(tokio::sync::mpsc::error::TrySendError::Full(msg)) => {
                // queue busy (e.g. mid file transfer): retry on next tick
                // rather than dropping the copy
                self.pending_clip = Some((msg, kind));
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                self.pending_clip = None;
            }
        }
    }

    fn flush_pending_clip(&mut self) {
        if self.active.is_some() {
            if let Some((msg, kind)) = self.pending_clip.take() {
                self.send_clip(msg, kind);
            }
        }
    }

    fn handle_internal(&mut self, internal: Internal) {
        match internal {
            Internal::Established {
                id,
                out_tx,
                ctl_tx,
                peer_fp,
                peer_name,
                outbound,
                cancel,
            } => {
                let new = ActiveConn {
                    id,
                    out_tx,
                    ctl_tx,
                    peer_fp,
                    peer_name,
                    outbound,
                    cancel,
                };
                // Both machines may connect to each other at once. Keep
                // the connection initiated by the machine with the
                // smaller fingerprint — deterministic on both ends, so
                // both settle on the same TCP stream.
                let keep_new = match &self.active {
                    None => true,
                    // A fresh connection from the SAME peer means that
                    // peer tore down the old link and re-dialed; replace
                    // it rather than letting the orientation rule keep a
                    // link the peer has already abandoned.
                    Some(old) if old.peer_fp == new.peer_fp && old.outbound != new.outbound => true,
                    Some(old) => {
                        let preferred = |c: &ActiveConn| c.outbound == (self.local_fp < c.peer_fp);
                        preferred(&new) || !preferred(old)
                    }
                };
                if keep_new {
                    if let Some(old) = self.active.take() {
                        old.cancel.cancel();
                    }
                    self.events.send(SyncEvent::Status {
                        connected: true,
                        peer_name: Some(new.peer_name.clone()),
                    });
                    self.active = Some(new);
                    self.flush_pending_clip();
                } else {
                    new.cancel.cancel();
                }
            }
            Internal::ConnClosed { id } => {
                if self.active.as_ref().is_some_and(|a| a.id == id) {
                    self.active = None;
                    self.events.send(SyncEvent::Status {
                        connected: false,
                        peer_name: None,
                    });
                }
            }
            Internal::PairingOk {
                fingerprint,
                name,
                addr,
                initiated,
            } => {
                self.pairing_dial = false;
                self.close_pairing_window();
                self.pairing_open.store(false, Ordering::SeqCst);
                self.candidates.insert(addr);
                self.events.send(SyncEvent::PairingComplete {
                    fingerprint,
                    name,
                    addr,
                    initiated,
                });
            }
            Internal::PairingErr { msg, initiated } => {
                if initiated {
                    // our own outbound pairing attempt failed
                    self.pairing_dial = false;
                    self.pairing_open.store(false, Ordering::SeqCst);
                    self.events.send(SyncEvent::PairingFailed(msg));
                } else {
                    // An inbound attempt failed the PIN. Do NOT close the
                    // window: that would let any unauthenticated LAN host
                    // cancel the user's pairing by probing a wrong PIN.
                    // The window still expires on its own timeout, and a
                    // 6-digit PIN can't be brute-forced within it. Don't
                    // surface it to the UI either — it's just a probe.
                    log::warn!("rejected an incoming pairing attempt: {msg}");
                }
            }
            Internal::DialFinished => self.dialing = false,
        }
    }
}

/* --------------------------- connections --------------------------- */

struct ConnCtx {
    id: u64,
    device_name: String,
    file_dir: Rc<RefCell<PathBuf>>,
    clipboard: Rc<clipboard::ClipboardMonitor>,
    clipboard_enabled: Arc<AtomicBool>,
    events: EventSender,
    internal: local_channel::mpsc::Sender<Internal>,
}

async fn dial(
    ip: IpAddr,
    port: u16,
    connector: &tokio_rustls::TlsConnector,
) -> Result<TlsStream<TcpStream>, String> {
    let tcp = tokio::time::timeout(Duration::from_secs(5), TcpStream::connect((ip, port)))
        .await
        .map_err(|_| "timeout".to_string())?
        .map_err(|e| e.to_string())?;
    let _ = tcp.set_nodelay(true);
    let stream = connector
        .connect(ServerName::IpAddress(ip.into()), tcp)
        .await
        .map_err(|e| e.to_string())?;
    Ok(TlsStream::Client(stream))
}

async fn pair_dial(
    addr: String,
    pin: String,
    connector: tokio_rustls::TlsConnector,
    device_name: String,
) -> Result<(String, String, IpAddr), String> {
    let mut addrs = tokio::net::lookup_host(addr.as_str())
        .await
        .map_err(|e| format!("cannot resolve {addr}: {e}"))?;
    let sock_addr = addrs
        .next()
        .ok_or_else(|| format!("cannot resolve {addr}"))?;
    let stream = dial(sock_addr.ip(), sock_addr.port(), &connector)
        .await
        .map_err(|e| format!("cannot reach {addr}: {e}"))?;
    let peer_fp =
        tls::peer_fingerprint(&stream).ok_or_else(|| "peer sent no certificate".to_string())?;
    let exporter = tls::exporter(&stream).map_err(|e| e.to_string())?;
    let (mut r, mut w) = tokio::io::split(stream);
    let peer_name = tokio::time::timeout(
        PAIRING_EXCHANGE_TIMEOUT,
        pairing::run_initiator(&mut r, &mut w, &exporter, &pin, &device_name),
    )
    .await
    .map_err(|_| "pairing timed out".to_string())?
    .map_err(|e| e.to_string())?;
    Ok((peer_fp, peer_name, sock_addr.ip()))
}

/// Write one frame, bounded by [`WRITE_TIMEOUT`] and raced against
/// cancellation. Returns false (caller tears down the connection) if the
/// write fails, times out — a peer that stops reading must never block
/// the writer forever — or the connection is cancelled mid-write.
async fn write_framed(
    w: &mut WriteHalf<TlsStream<TcpStream>>,
    msg: &SyncMessage,
    cancel: &CancellationToken,
) -> bool {
    tokio::select! {
        _ = cancel.cancelled() => false,
        r = tokio::time::timeout(WRITE_TIMEOUT, write_msg(w, msg)) => matches!(r, Ok(Ok(()))),
    }
}

async fn run_conn(stream: TlsStream<TcpStream>, outbound: bool, ctx: ConnCtx) {
    let Some(peer_fp) = tls::peer_fingerprint(&stream) else {
        return;
    };
    let (mut r, mut w) = tokio::io::split(stream);

    /* exchange hello */
    let hello = SyncMessage::Hello {
        version: PROTO_VERSION,
        name: ctx.device_name.clone(),
    };
    if write_msg(&mut w, &hello).await.is_err() {
        return;
    }
    let peer_name = match tokio::time::timeout(Duration::from_secs(10), read_msg(&mut r)).await {
        Ok(Ok(SyncMessage::Hello { version, name })) => {
            if version != PROTO_VERSION {
                log::warn!("peer runs sync protocol v{version}, expected v{PROTO_VERSION}");
            }
            name
        }
        _ => return,
    };

    let (out_tx, mut out_rx) = mpsc::channel::<SyncMessage>(OUT_QUEUE);
    let (ctl_tx, mut ctl_rx) = mpsc::unbounded_channel::<ConnCtl>();
    let cancel = CancellationToken::new();
    let _ = ctx.internal.send(Internal::Established {
        id: ctx.id,
        out_tx: out_tx.clone(),
        ctl_tx,
        peer_fp,
        peer_name,
        outbound,
        cancel: cancel.clone(),
    });

    /* writer (also produces keepalive pings) */
    let writer_cancel = cancel.clone();
    let writer: JoinHandle<()> = spawn_local(async move {
        let mut ping = tokio::time::interval(PING_INTERVAL);
        ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            let msg = tokio::select! {
                _ = writer_cancel.cancelled() => break,
                msg = out_rx.recv() => match msg {
                    Some(msg) => msg,
                    None => break,
                },
                _ = ping.tick() => SyncMessage::Ping,
            };
            if !write_framed(&mut w, &msg, &writer_cancel).await {
                break;
            }
        }
        writer_cancel.cancel();
    });

    /* reader */
    reader_loop(&mut r, &out_tx, &mut ctl_rx, &cancel, &ctx).await;

    cancel.cancel();
    let _ = writer.await;
    let _ = ctx.internal.send(Internal::ConnClosed { id: ctx.id });
}

async fn reader_loop(
    r: &mut ReadHalf<TlsStream<TcpStream>>,
    out_tx: &mpsc::Sender<SyncMessage>,
    ctl_rx: &mut mpsc::UnboundedReceiver<ConnCtl>,
    cancel: &CancellationToken,
    ctx: &ConnCtx,
) {
    /* incoming transfers, keyed by the peer's ids */
    let mut recv: HashMap<u64, files::RecvTransfer> = HashMap::new();
    /* outgoing transfers waiting for the peer to accept */
    let mut awaiting_accept: HashMap<u64, oneshot::Sender<Result<(), String>>> = HashMap::new();

    loop {
        let msg = tokio::select! {
            _ = cancel.cancelled() => break,
            Some(ctl) = ctl_rx.recv() => {
                match ctl {
                    ConnCtl::SendFiles(paths) => {
                        for path in paths {
                            let id = next_transfer_id();
                            let (accept_tx, accept_rx) = oneshot::channel();
                            awaiting_accept.insert(id, accept_tx);
                            spawn_local(files::send_file(
                                id,
                                path,
                                out_tx.clone(),
                                accept_rx,
                                ctx.events.clone(),
                            ));
                        }
                    }
                }
                continue;
            }
            msg = tokio::time::timeout(READ_TIMEOUT, read_msg(r)) => match msg {
                Err(_) => {
                    log::warn!("sync connection timed out");
                    break;
                }
                Ok(Err(e)) => {
                    log::info!("sync connection closed: {e}");
                    break;
                }
                Ok(Ok(msg)) => msg,
            },
        };

        match msg {
            SyncMessage::Ping => {
                // non-blocking: a peer that floods pings without reading
                // must not be able to wedge the reader on a full queue
                let _ = out_tx.try_send(SyncMessage::Pong);
            }
            SyncMessage::Pong | SyncMessage::Hello { .. } => (),

            SyncMessage::ClipText { text } => {
                if ctx.clipboard_enabled.load(Ordering::SeqCst) {
                    ctx.clipboard.apply(clipboard::Apply::Text(text));
                    ctx.events.send(SyncEvent::Clipboard {
                        direction: Direction::Received,
                        kind: ClipboardKind::Text,
                    });
                }
            }
            SyncMessage::ClipImage { png, .. } => {
                if ctx.clipboard_enabled.load(Ordering::SeqCst) {
                    ctx.clipboard.apply(clipboard::Apply::Image { png });
                    ctx.events.send(SyncEvent::Clipboard {
                        direction: Direction::Received,
                        kind: ClipboardKind::Image,
                    });
                }
            }

            SyncMessage::FileOffer { id, name, size } => {
                // reject a reused id rather than orphaning the in-flight
                // transfer's .vylopart file
                if recv.contains_key(&id) {
                    let _ = out_tx
                        .send(SyncMessage::FileReject {
                            id,
                            reason: "transfer id already in use".to_string(),
                        })
                        .await;
                    continue;
                }
                let dir = ctx.file_dir.borrow().clone();
                match files::begin_recv(&dir, &name, size).await {
                    Ok(transfer) => {
                        ctx.events.send(SyncEvent::Transfer(files::status(
                            id,
                            &transfer.name,
                            Direction::Received,
                            0,
                            size,
                            TransferState::Active,
                            None,
                        )));
                        recv.insert(id, transfer);
                        let _ = out_tx.send(SyncMessage::FileAccept { id }).await;
                    }
                    Err(reason) => {
                        ctx.events.send(SyncEvent::Transfer(files::status(
                            id,
                            &name,
                            Direction::Received,
                            0,
                            size,
                            TransferState::Failed,
                            Some(reason.clone()),
                        )));
                        let _ = out_tx.send(SyncMessage::FileReject { id, reason }).await;
                    }
                }
            }
            SyncMessage::FileChunk { id, offset, data } => {
                use sha2::Digest;
                use tokio::io::AsyncWriteExt;
                let Some(transfer) = recv.get_mut(&id) else {
                    continue;
                };
                // Bound the write by the offered size and per-chunk size:
                // without this a peer could stream unbounded chunks (or a
                // huge single frame) and fill the receiver's disk.
                let overflows = data.len() > proto::CHUNK_SIZE
                    || transfer.received + data.len() as u64 > transfer.size;
                if overflows {
                    let reason = "transfer exceeds offered size".to_string();
                    ctx.events.send(SyncEvent::Transfer(files::status(
                        id,
                        &transfer.name,
                        Direction::Received,
                        transfer.received,
                        transfer.size,
                        TransferState::Failed,
                        Some(reason.clone()),
                    )));
                    let _ = out_tx.send(SyncMessage::FileCancel { id, reason }).await;
                    let t = recv.remove(&id).expect("transfer");
                    let _ = tokio::fs::remove_file(&t.part_path).await;
                    continue;
                }
                if offset != transfer.received {
                    let reason = "chunks out of order".to_string();
                    ctx.events.send(SyncEvent::Transfer(files::status(
                        id,
                        &transfer.name,
                        Direction::Received,
                        transfer.received,
                        transfer.size,
                        TransferState::Failed,
                        Some(reason.clone()),
                    )));
                    let _ = out_tx.send(SyncMessage::FileCancel { id, reason }).await;
                    let t = recv.remove(&id).expect("transfer");
                    let _ = tokio::fs::remove_file(&t.part_path).await;
                    continue;
                }
                transfer.hasher.update(&data);
                if let Err(e) = transfer.file.write_all(&data).await {
                    let reason = format!("write failed: {e}");
                    ctx.events.send(SyncEvent::Transfer(files::status(
                        id,
                        &transfer.name,
                        Direction::Received,
                        transfer.received,
                        transfer.size,
                        TransferState::Failed,
                        Some(reason.clone()),
                    )));
                    let _ = out_tx.send(SyncMessage::FileCancel { id, reason }).await;
                    let t = recv.remove(&id).expect("transfer");
                    let _ = tokio::fs::remove_file(&t.part_path).await;
                    continue;
                }
                transfer.received += data.len() as u64;
                if transfer.progress_due() {
                    ctx.events.send(SyncEvent::Transfer(files::status(
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
            SyncMessage::FileDone { id, sha256 } => {
                let Some(transfer) = recv.remove(&id) else {
                    continue;
                };
                let name = transfer.name.clone();
                let size = transfer.size;
                match files::finish_recv(transfer, sha256).await {
                    Ok(path) => ctx.events.send(SyncEvent::Transfer(files::status(
                        id,
                        &name,
                        Direction::Received,
                        size,
                        size,
                        TransferState::Done,
                        Some(path.display().to_string()),
                    ))),
                    Err(reason) => ctx.events.send(SyncEvent::Transfer(files::status(
                        id,
                        &name,
                        Direction::Received,
                        size,
                        size,
                        TransferState::Failed,
                        Some(reason),
                    ))),
                }
            }
            SyncMessage::FileCancel { id, reason } => {
                if let Some(t) = recv.remove(&id) {
                    ctx.events.send(SyncEvent::Transfer(files::status(
                        id,
                        &t.name,
                        Direction::Received,
                        t.received,
                        t.size,
                        TransferState::Failed,
                        Some(reason),
                    )));
                    let _ = tokio::fs::remove_file(&t.part_path).await;
                }
            }
            SyncMessage::FileAccept { id } => {
                if let Some(tx) = awaiting_accept.remove(&id) {
                    let _ = tx.send(Ok(()));
                }
            }
            SyncMessage::FileReject { id, reason } => {
                if let Some(tx) = awaiting_accept.remove(&id) {
                    let _ = tx.send(Err(reason));
                }
            }

            /* pairing messages are only valid on pairing connections */
            SyncMessage::PairStart { .. }
            | SyncMessage::PairResponse { .. }
            | SyncMessage::PairConfirmA { .. }
            | SyncMessage::PairConfirmB { .. } => {
                log::warn!("unexpected pairing message on established connection");
            }
        }
    }

    /* clean up unfinished incoming transfers */
    for (id, t) in recv {
        ctx.events.send(SyncEvent::Transfer(files::status(
            id,
            &t.name,
            Direction::Received,
            t.received,
            t.size,
            TransferState::Failed,
            Some("connection closed".to_string()),
        )));
        let _ = tokio::fs::remove_file(&t.part_path).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    fn test_identity() -> (
        Certificate,
        Arc<RwLock<HashMap<String, String>>>,
        Arc<AtomicBool>,
    ) {
        let cert = Certificate::generate_self_signed(["ignored".to_owned()]).expect("cert");
        let authorized = Arc::new(RwLock::new(HashMap::new()));
        let pairing_open = Arc::new(AtomicBool::new(false));
        (cert, authorized, pairing_open)
    }

    async fn tls_pair_over_localhost(
        pin_a: &str,
        pin_b: &str,
    ) -> (
        Result<(String, String), String>,
        Result<(String, String), String>,
    ) {
        let (cert_a, auth_a, pairing_a) = test_identity();
        let (cert_b, auth_b, pairing_b) = test_identity();
        pairing_a.store(true, Ordering::SeqCst);
        pairing_b.store(true, Ordering::SeqCst);
        let tls_a = tls::build_tls(&cert_a, auth_a, pairing_a).expect("tls");
        let tls_b = tls::build_tls(&cert_b, auth_b, pairing_b).expect("tls");

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");

        let pin_a = pin_a.to_string();
        let pin_b = pin_b.to_string();
        let responder = async move {
            let (tcp, _) = listener.accept().await.map_err(|e| e.to_string())?;
            let stream = TlsStream::Server(
                tls_b
                    .acceptor
                    .accept(tcp)
                    .await
                    .map_err(|e| e.to_string())?,
            );
            let fp = tls::peer_fingerprint(&stream).ok_or("no peer cert")?;
            let exporter = tls::exporter(&stream).map_err(|e| e.to_string())?;
            let (mut r, mut w) = tokio::io::split(stream);
            let name = pairing::run_responder(&mut r, &mut w, &exporter, &pin_b, "bob")
                .await
                .map_err(|e| e.to_string())?;
            Ok((name, fp))
        };
        let initiator = async move {
            let tcp = TcpStream::connect(addr).await.map_err(|e| e.to_string())?;
            let stream = TlsStream::Client(
                tls_a
                    .pairing_connector
                    .connect(ServerName::IpAddress(addr.ip().into()), tcp)
                    .await
                    .map_err(|e| e.to_string())?,
            );
            let fp = tls::peer_fingerprint(&stream).ok_or("no peer cert")?;
            let exporter = tls::exporter(&stream).map_err(|e| e.to_string())?;
            let (mut r, mut w) = tokio::io::split(stream);
            let name = pairing::run_initiator(&mut r, &mut w, &exporter, &pin_a, "alice")
                .await
                .map_err(|e| e.to_string())?;
            Ok((name, fp))
        };
        tokio::join!(responder, initiator)
    }

    #[tokio::test]
    async fn pairing_succeeds_with_matching_pin() {
        let (responder, initiator) = tls_pair_over_localhost("482913", "482913").await;
        let (name_seen_by_b, _fp_of_a) = responder.expect("responder should succeed");
        let (name_seen_by_a, _fp_of_b) = initiator.expect("initiator should succeed");
        assert_eq!(name_seen_by_b, "alice");
        assert_eq!(name_seen_by_a, "bob");
    }

    #[tokio::test]
    async fn pairing_fails_with_wrong_pin() {
        let (responder, initiator) = tls_pair_over_localhost("482913", "482914").await;
        assert!(responder.is_err(), "responder must reject a wrong pin");
        assert!(initiator.is_err(), "initiator must not complete");
    }

    #[tokio::test]
    async fn unauthorized_peer_rejected_without_pairing_window() {
        let (cert_a, auth_a, pairing_a) = test_identity();
        let (cert_b, auth_b, pairing_b) = test_identity();
        // no pairing window open on either side, empty allowlists
        let tls_a = tls::build_tls(&cert_a, auth_a, pairing_a).expect("tls");
        let tls_b = tls::build_tls(&cert_b, auth_b, pairing_b).expect("tls");

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let accept = async move {
            let (tcp, _) = listener.accept().await.expect("accept");
            tls_b.acceptor.accept(tcp).await
        };
        let connect = async move {
            let tcp = TcpStream::connect(addr).await.expect("connect");
            tls_a
                .connector
                .connect(ServerName::IpAddress(addr.ip().into()), tcp)
                .await
        };
        let (accepted, connected) = tokio::join!(accept, connect);
        assert!(
            accepted.is_err() || connected.is_err(),
            "handshake must fail when neither side authorized the other"
        );
    }

    #[tokio::test]
    async fn strict_connector_rejects_unpinned_peer_during_pairing_window() {
        // The security-critical property: while a pairing window is
        // open, the ORDINARY outbound connector must still refuse an
        // unpinned peer. Only the dedicated pairing connector may admit
        // one (and only to run the PIN exchange).
        let (cert_a, auth_a, pairing_a) = test_identity();
        let (cert_b, auth_b, pairing_b) = test_identity();
        pairing_a.store(true, Ordering::SeqCst); // window OPEN on the dialer
        pairing_b.store(true, Ordering::SeqCst);
        let tls_a = tls::build_tls(&cert_a, auth_a, pairing_a).expect("tls");
        let tls_b = tls::build_tls(&cert_b, auth_b, pairing_b).expect("tls");

        async fn try_connect(
            connector: &tokio_rustls::TlsConnector,
            acceptor: tokio_rustls::TlsAcceptor,
        ) -> bool {
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
            let addr = listener.local_addr().expect("addr");
            let accept = async move {
                let (tcp, _) = listener.accept().await.expect("accept");
                acceptor.accept(tcp).await
            };
            let connect = async move {
                let tcp = TcpStream::connect(addr).await.expect("connect");
                connector
                    .connect(ServerName::IpAddress(addr.ip().into()), tcp)
                    .await
            };
            let (accepted, connected) = tokio::join!(accept, connect);
            accepted.is_ok() && connected.is_ok()
        }

        // strict connector must be refused despite the open window
        assert!(
            !try_connect(&tls_a.connector, tls_b.acceptor.clone()).await,
            "strict outbound connector must reject an unpinned peer even during a pairing window"
        );
        // the pairing connector is allowed to establish the TLS session
        assert!(
            try_connect(&tls_a.pairing_connector, tls_b.acceptor.clone()).await,
            "pairing connector should establish the session so the PIN exchange can run"
        );
    }

    #[test]
    fn sanitize_rejects_windows_reserved_names() {
        assert_eq!(files::sanitize_name("NUL"), Some("_NUL".into()));
        assert_eq!(files::sanitize_name("nul.txt"), Some("_nul.txt".into()));
        assert_eq!(files::sanitize_name("COM1"), Some("_COM1".into()));
        assert_eq!(files::sanitize_name("LPT9.pdf"), Some("_LPT9.pdf".into()));
        assert_eq!(files::sanitize_name("CON"), Some("_CON".into()));
        // not reserved: COM0, a longer name, a normal file
        assert_eq!(files::sanitize_name("COM0"), Some("COM0".into()));
        assert_eq!(
            files::sanitize_name("communicate.txt"),
            Some("communicate.txt".into())
        );
        assert_eq!(
            files::sanitize_name("report.pdf"),
            Some("report.pdf".into())
        );
    }

    #[tokio::test]
    async fn proto_roundtrip() {
        let (mut a, mut b) = tokio::io::duplex(1024 * 1024);
        let msg = SyncMessage::FileChunk {
            id: 7,
            offset: 1234,
            data: vec![42u8; 4096],
        };
        write_msg(&mut a, &msg).await.expect("write");
        match read_msg(&mut b).await.expect("read") {
            SyncMessage::FileChunk { id, offset, data } => {
                assert_eq!(id, 7);
                assert_eq!(offset, 1234);
                assert_eq!(data, vec![42u8; 4096]);
            }
            other => panic!("wrong message: {other:?}"),
        }
    }

    #[tokio::test]
    async fn oversized_frame_rejected() {
        let (mut a, mut b) = tokio::io::duplex(64);
        use tokio::io::AsyncWriteExt;
        a.write_u32(proto::MAX_FRAME_SIZE + 1).await.expect("write");
        assert!(matches!(
            read_msg(&mut b).await,
            Err(proto::ProtoError::FrameTooLarge(_))
        ));
    }

    #[test]
    fn sanitize_rejects_traversal() {
        assert_eq!(files::sanitize_name(".."), None);
        assert_eq!(
            files::sanitize_name("../../etc/passwd"),
            Some("passwd".into())
        );
        assert_eq!(files::sanitize_name("/etc/passwd"), Some("passwd".into()));
        // both separators are stripped on every OS, so a windows-style
        // path reduces to its basename regardless of receiver platform
        assert_eq!(
            files::sanitize_name("C:\\Windows\\evil.exe"),
            Some("evil.exe".into())
        );
        assert_eq!(
            files::sanitize_name("..\\..\\secret"),
            Some("secret".into())
        );
        assert_eq!(
            files::sanitize_name("report.pdf"),
            Some("report.pdf".into())
        );
        assert_eq!(files::sanitize_name("...."), None);
        assert_eq!(files::sanitize_name(""), None);
        assert_eq!(files::sanitize_name("a\x00b.txt"), Some("a_b.txt".into()));
    }

    #[test]
    fn unique_path_suffixes() {
        let dir = std::env::temp_dir().join(format!("vylo-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let first = files::unique_path(&dir, "a.txt");
        assert_eq!(first, dir.join("a.txt"));
        std::fs::write(&first, b"x").expect("write");
        let second = files::unique_path(&dir, "a.txt");
        assert_eq!(second, dir.join("a (1).txt"));
        std::fs::write(&second, b"x").expect("write");
        assert_eq!(files::unique_path(&dir, "a.txt"), dir.join("a (2).txt"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
