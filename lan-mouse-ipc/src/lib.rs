use std::{
    collections::{HashMap, HashSet},
    env::VarError,
    fmt::Display,
    io,
    net::{IpAddr, SocketAddr},
    str::FromStr,
};
use thiserror::Error;

#[cfg(unix)]
use std::{
    env,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

mod connect;
mod connect_async;
mod listen;

pub use connect::{FrontendEventReader, FrontendRequestWriter, connect};
pub use connect_async::{AsyncFrontendEventReader, AsyncFrontendRequestWriter, connect_async};
pub use listen::AsyncFrontendListener;

#[derive(Debug, Error)]
pub enum ConnectionError {
    #[error(transparent)]
    SocketPath(#[from] SocketPathError),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("connection timed out")]
    Timeout,
}

#[derive(Debug, Error)]
pub enum IpcListenerCreationError {
    #[error("could not determine socket-path: `{0}`")]
    SocketPath(#[from] SocketPathError),
    #[error("service already running!")]
    AlreadyRunning,
    #[error("failed to bind lan-mouse socket: `{0}`")]
    Bind(io::Error),
}

#[derive(Debug, Error)]
pub enum IpcError {
    #[error("io error occured: `{0}`")]
    Io(#[from] io::Error),
    #[error("invalid json: `{0}`")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Connection(#[from] ConnectionError),
    #[error(transparent)]
    Listen(#[from] IpcListenerCreationError),
}

pub const DEFAULT_PORT: u16 = 4242;
/// TCP port of the clipboard / file-transfer / pairing side channel
pub const DEFAULT_SYNC_PORT: u16 = 4243;

#[derive(Debug, Default, Eq, Hash, PartialEq, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Position {
    #[default]
    Left,
    Right,
    Top,
    Bottom,
}

impl Position {
    pub fn opposite(&self) -> Self {
        match self {
            Position::Left => Position::Right,
            Position::Right => Position::Left,
            Position::Top => Position::Bottom,
            Position::Bottom => Position::Top,
        }
    }
}

#[derive(Debug, Error)]
#[error("not a valid position: {pos}")]
pub struct PositionParseError {
    pos: String,
}

impl FromStr for Position {
    type Err = PositionParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "left" => Ok(Self::Left),
            "right" => Ok(Self::Right),
            "top" => Ok(Self::Top),
            "bottom" => Ok(Self::Bottom),
            _ => Err(PositionParseError { pos: s.into() }),
        }
    }
}

impl Display for Position {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Position::Left => "left",
                Position::Right => "right",
                Position::Top => "top",
                Position::Bottom => "bottom",
            }
        )
    }
}

impl TryFrom<&str> for Position {
    type Error = ();

    fn try_from(s: &str) -> Result<Self, Self::Error> {
        match s {
            "left" => Ok(Position::Left),
            "right" => Ok(Position::Right),
            "top" => Ok(Position::Top),
            "bottom" => Ok(Position::Bottom),
            _ => Err(()),
        }
    }
}

/// A modifier key role. Used to describe what a local modifier key acts
/// as on a given client (see [`ModifierMap`]).
#[derive(Debug, Eq, Hash, PartialEq, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Modifier {
    /// Control on every platform
    Ctrl,
    /// Alt (Windows/Linux) / Option (macOS)
    Alt,
    /// Windows key (Windows) / Super (Linux) / Command (macOS)
    Meta,
}

impl Display for Modifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Modifier::Ctrl => "ctrl",
            Modifier::Alt => "alt",
            Modifier::Meta => "meta",
        })
    }
}

impl TryFrom<&str> for Modifier {
    type Error = ();

    fn try_from(s: &str) -> Result<Self, Self::Error> {
        match s {
            "ctrl" => Ok(Modifier::Ctrl),
            "alt" => Ok(Modifier::Alt),
            "meta" => Ok(Modifier::Meta),
            _ => Err(()),
        }
    }
}

/// Which modifier each local modifier key acts as when input is sent to
/// a client. The default is the identity (keys act as themselves). This
/// lets e.g. a Mac keyboard's Command key act as Ctrl on a Windows
/// machine, or undo a macOS-level Ctrl/Command swap for the peer.
#[derive(Debug, Eq, PartialEq, Clone, Copy, Serialize, Deserialize)]
pub struct ModifierMap {
    /// what the local Ctrl key(s) act as
    #[serde(default = "ModifierMap::default_ctrl")]
    pub ctrl: Modifier,
    /// what the local Alt / Option key(s) act as
    #[serde(default = "ModifierMap::default_alt")]
    pub alt: Modifier,
    /// what the local Windows / Super / Command key(s) act as
    #[serde(default = "ModifierMap::default_meta")]
    pub meta: Modifier,
}

impl ModifierMap {
    const fn default_ctrl() -> Modifier {
        Modifier::Ctrl
    }
    const fn default_alt() -> Modifier {
        Modifier::Alt
    }
    const fn default_meta() -> Modifier {
        Modifier::Meta
    }

    /// true if every key acts as itself
    pub fn is_identity(&self) -> bool {
        *self == Self::default()
    }
}

impl Default for ModifierMap {
    fn default() -> Self {
        Self {
            ctrl: Modifier::Ctrl,
            alt: Modifier::Alt,
            meta: Modifier::Meta,
        }
    }
}

/// pointer speed multiplier applied to motion sent to a client (1.0 = unchanged)
pub const DEFAULT_SPEED: f64 = 1.0;
/// bounds for [`ClientConfig::speed`]
pub const MIN_SPEED: f64 = 0.25;
pub const MAX_SPEED: f64 = 4.0;

/// clamp a requested speed into the supported range; NaN → default
pub fn clamp_speed(speed: f64) -> f64 {
    if speed.is_nan() {
        DEFAULT_SPEED
    } else {
        speed.clamp(MIN_SPEED, MAX_SPEED)
    }
}

#[derive(Debug, PartialEq, Clone, Serialize, Deserialize)]
pub struct ClientConfig {
    /// hostname of this client
    pub hostname: Option<String>,
    /// fix ips, determined by the user
    pub fix_ips: Vec<IpAddr>,
    /// both active_addr and addrs can be None / empty so port needs to be stored seperately
    pub port: u16,
    /// position of a client on screen
    pub pos: Position,
    /// enter hook
    pub cmd: Option<String>,
    /// pointer speed multiplier for motion sent to this client
    #[serde(default = "default_speed")]
    pub speed: f64,
    /// what the local modifier keys act as on this client
    #[serde(default)]
    pub modifiers: ModifierMap,
}

fn default_speed() -> f64 {
    DEFAULT_SPEED
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            port: DEFAULT_PORT,
            hostname: Default::default(),
            fix_ips: Default::default(),
            pos: Default::default(),
            cmd: None,
            speed: DEFAULT_SPEED,
            modifiers: ModifierMap::default(),
        }
    }
}

pub type ClientHandle = u64;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ClientState {
    /// events should be sent to and received from the client
    pub active: bool,
    /// `active` address of the client, used to send data to.
    /// This should generally be the socket address where data
    /// was last received from.
    pub active_addr: Option<SocketAddr>,
    /// tracks whether or not the client is available for emulation
    pub alive: bool,
    /// ips from dns
    pub dns_ips: Vec<IpAddr>,
    /// all ip addresses associated with a particular client
    /// e.g. Laptops usually have at least an ethernet and a wifi port
    /// which have different ip addresses
    pub ips: HashSet<IpAddr>,
    /// client has pressed keys
    pub has_pressed_keys: bool,
    /// dns resolving in progress
    pub resolving: bool,
    /// Peer's build short commit hash from the [`Hello`] proto
    /// event. `None` means we haven't received a Hello yet — either
    /// the connection is fresh, or the peer is on an older build
    /// that predates the Hello event. The frontend uses this to
    /// soft-warn on version mismatch.
    pub peer_commit: Option<[u8; 8]>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FrontendEvent {
    /// a client was created
    Created(ClientHandle, ClientConfig, ClientState),
    /// no such client
    NoSuchClient(ClientHandle),
    /// state changed
    State(ClientHandle, ClientConfig, ClientState),
    /// the client was deleted
    Deleted(ClientHandle),
    /// new port, reason of failure (if failed)
    PortChanged(u16, Option<String>),
    /// list of all clients, used for initial state synchronization
    Enumerate(Vec<(ClientHandle, ClientConfig, ClientState)>),
    /// an error occured
    Error(String),
    /// capture status
    CaptureStatus(Status),
    /// emulation status
    EmulationStatus(Status),
    /// authorized public key fingerprints have been updated
    AuthorizedUpdated(HashMap<String, String>),
    /// public key fingerprint of this device
    PublicKeyFingerprint(String),
    /// new device connected
    DeviceConnected {
        addr: SocketAddr,
        fingerprint: String,
    },
    /// incoming device entered the screen
    DeviceEntered {
        fingerprint: String,
        addr: SocketAddr,
        pos: Position,
    },
    /// incoming disconnected
    IncomingDisconnected(SocketAddr),
    /// failed connection attempt (approval for fingerprint required)
    ConnectionAttempt { fingerprint: String },
    /// a pairing window was opened on this device
    PairingStarted { pin: String, port: u16 },
    /// pairing finished successfully (either side)
    PairingComplete { fingerprint: String, name: String },
    /// pairing failed or was cancelled
    PairingFailed(String),
    /// devices discovered on the LAN via mdns (full list, replaces previous)
    PeersDiscovered(Vec<DiscoveredPeer>),
    /// state of the clipboard/file side channel
    SyncStatus {
        connected: bool,
        peer_name: Option<String>,
    },
    /// vylo settings state (part of initial sync)
    VyloState {
        clipboard_sync: bool,
        keyboard_layout_sync: bool,
        file_dir: String,
        device_name: String,
        sync_port: u16,
    },
    /// a clipboard item was relayed
    ClipboardSynced {
        direction: Direction,
        kind: ClipboardKind,
    },
    /// file transfer progress
    FileTransfer(FileTransferStatus),
    /// files dragged across from the peer were dropped here (final paths)
    FilesDropped { paths: Vec<String> },
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct DiscoveredPeer {
    pub name: String,
    pub addrs: Vec<IpAddr>,
    pub port: u16,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Sent,
    Received,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClipboardKind {
    Text,
    Image,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TransferState {
    Active,
    Done,
    Failed,
}

/// what a transfer row represents
#[derive(Debug, Clone, Copy, Eq, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TransferKind {
    #[default]
    File,
    /// a whole directory tree, reported as one transfer
    Folder,
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct FileTransferStatus {
    pub id: u64,
    pub name: String,
    pub direction: Direction,
    pub transferred: u64,
    pub total: u64,
    pub state: TransferState,
    /// destination path when done (received), error message when failed
    pub detail: Option<String>,
    #[serde(default)]
    pub kind: TransferKind,
    /// number of files inside a folder transfer (0 for a single file)
    #[serde(default)]
    pub files: u32,
}

#[derive(Debug, PartialEq, Clone, Serialize, Deserialize)]
pub enum FrontendRequest {
    /// activate/deactivate client
    Activate(ClientHandle, bool),
    /// add a new client
    Create,
    /// change the listen port (recreate udp listener)
    ChangePort(u16),
    /// remove a client
    Delete(ClientHandle),
    /// request an enumeration of all clients
    Enumerate(),
    /// resolve dns
    ResolveDns(ClientHandle),
    /// update hostname
    UpdateHostname(ClientHandle, Option<String>),
    /// update port
    UpdatePort(ClientHandle, u16),
    /// update position
    UpdatePosition(ClientHandle, Position),
    /// update fix-ips
    UpdateFixIps(ClientHandle, Vec<IpAddr>),
    /// request reenabling input capture
    EnableCapture,
    /// request reenabling input emulation
    EnableEmulation,
    /// synchronize all state
    Sync,
    /// authorize fingerprint (description, fingerprint)
    AuthorizeKey(String, String),
    /// remove fingerprint (fingerprint)
    RemoveAuthorizedKey(String),
    /// change the hook command
    UpdateEnterHook(u64, Option<String>),
    /// save config file
    SaveConfiguration,
    /// open a pairing window: daemon generates a PIN and accepts one
    /// pairing attempt for a limited time
    StartPairing,
    /// pair with a peer that is showing a PIN
    PairWithPeer { addr: String, pin: String },
    /// close the pairing window / abort a pairing attempt
    CancelPairing,
    /// send files to the paired peer
    SendFiles(Vec<String>),
    /// enable / disable clipboard sync
    SetClipboardSync(bool),
    /// enable / disable keyboard input-language sync
    SetKeyboardLayoutSync(bool),
    /// set the directory incoming files are written to
    SetFileDir(String),
    /// set this device's name
    SetDeviceName(String),
    /// pointer speed multiplier for motion sent to a client
    UpdateSpeed(ClientHandle, f64),
    /// what the local modifier keys act as on a client
    UpdateModifierMap(ClientHandle, ModifierMap),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub enum Status {
    #[default]
    Disabled,
    Enabled,
}

impl From<Status> for bool {
    fn from(status: Status) -> Self {
        match status {
            Status::Enabled => true,
            Status::Disabled => false,
        }
    }
}

#[cfg(unix)]
const LAN_MOUSE_SOCKET_NAME: &str = "vylo-socket.sock";

#[derive(Debug, Error)]
pub enum SocketPathError {
    #[error("could not determine $XDG_RUNTIME_DIR: `{0}`")]
    XdgRuntimeDirNotFound(VarError),
    #[error("could not determine $HOME: `{0}`")]
    HomeDirNotFound(VarError),
}

#[cfg(all(unix, not(target_os = "macos")))]
pub fn default_socket_path() -> Result<PathBuf, SocketPathError> {
    let xdg_runtime_dir =
        env::var("XDG_RUNTIME_DIR").map_err(SocketPathError::XdgRuntimeDirNotFound)?;
    Ok(Path::new(xdg_runtime_dir.as_str()).join(LAN_MOUSE_SOCKET_NAME))
}

#[cfg(all(unix, target_os = "macos"))]
pub fn default_socket_path() -> Result<PathBuf, SocketPathError> {
    let home = env::var("HOME").map_err(SocketPathError::HomeDirNotFound)?;
    Ok(Path::new(home.as_str())
        .join("Library")
        .join("Caches")
        .join(LAN_MOUSE_SOCKET_NAME))
}
