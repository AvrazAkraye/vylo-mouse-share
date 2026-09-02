/**
 * Typed layer over the Tauri <-> daemon IPC bridge.
 *
 * The Rust backend forwards every newline-delimited JSON line from the daemon
 * socket verbatim as the tauri event `"daemon"`, and emits `{"connected":bool}`
 * as `"daemon-ipc"` whenever the socket connects / disconnects.
 *
 * Wire format: serde externally-tagged JSON (lan-mouse-ipc + Vylo additions).
 *   - unit variants:    "Sync"
 *   - tuple variants:   {"State":[3,{...},{...}]}
 *   - struct variants:  {"DeviceConnected":{"addr":"1.2.3.4:4242","fingerprint":"aa:bb"}}
 */

import { invoke } from "@tauri-apps/api/core";

/* ------------------------------------------------------------------ */
/* Shared shapes (mirror lan-mouse-ipc)                                */
/* ------------------------------------------------------------------ */

export type Position = "left" | "right" | "top" | "bottom";
export type Status = "Enabled" | "Disabled";

export interface ClientConfig {
  hostname: string | null;
  fix_ips: string[];
  port: number;
  pos: Position;
  cmd: string | null;
}

export interface ClientState {
  active: boolean;
  active_addr: string | null;
  alive: boolean;
  dns_ips: string[];
  ips: string[];
  has_pressed_keys: boolean;
  resolving: boolean;
  peer_commit: number[] | null;
}

export type ClientHandle = number;

export interface DiscoveredPeer {
  name: string;
  addrs: string[];
  port: number;
}

export interface SyncStatus {
  connected: boolean;
  peer_name: string | null;
}

export interface VyloState {
  clipboard_sync: boolean;
  keyboard_layout_sync: boolean;
  file_dir: string;
  device_name: string;
  sync_port: number;
}

export type TransferDirection = "sent" | "received";
export type TransferState = "active" | "done" | "failed";

export interface FileTransfer {
  id: number;
  name: string;
  direction: TransferDirection;
  transferred: number;
  total: number;
  state: TransferState;
  detail: string | null;
}

export interface ClipboardSynced {
  direction: TransferDirection;
  kind: "text" | "image";
}

/* ------------------------------------------------------------------ */
/* Daemon -> UI events, as a discriminated union                       */
/* ------------------------------------------------------------------ */

export type DaemonEvent =
  /* upstream lan-mouse-ipc */
  | { type: "Created"; handle: ClientHandle; config: ClientConfig; state: ClientState }
  | { type: "NoSuchClient"; handle: ClientHandle }
  | { type: "State"; handle: ClientHandle; config: ClientConfig; state: ClientState }
  | { type: "Deleted"; handle: ClientHandle }
  | { type: "PortChanged"; port: number; error: string | null }
  | { type: "Enumerate"; clients: Array<{ handle: ClientHandle; config: ClientConfig; state: ClientState }> }
  | { type: "Error"; message: string }
  | { type: "CaptureStatus"; status: Status }
  | { type: "EmulationStatus"; status: Status }
  | { type: "AuthorizedUpdated"; keys: Record<string, string> }
  | { type: "PublicKeyFingerprint"; fingerprint: string }
  | { type: "DeviceConnected"; addr: string; fingerprint: string }
  | { type: "DeviceEntered"; fingerprint: string; addr: string; pos: Position }
  | { type: "IncomingDisconnected"; addr: string }
  | { type: "ConnectionAttempt"; fingerprint: string }
  /* vylo additions */
  | { type: "PairingStarted"; pin: string; port: number }
  | { type: "PairingComplete"; fingerprint: string; name: string }
  | { type: "PairingFailed"; reason: string }
  | { type: "PeersDiscovered"; peers: DiscoveredPeer[] }
  | { type: "SyncStatus"; status: SyncStatus }
  | { type: "VyloState"; state: VyloState }
  | { type: "ClipboardSynced"; info: ClipboardSynced }
  | { type: "FileTransfer"; transfer: FileTransfer }
  /** files dragged across from the peer landed here (final paths) */
  | { type: "FilesDropped"; paths: string[] };

/**
 * Parse one raw JSON line from the daemon into a typed event.
 * Returns null for lines we do not understand (forward compatible).
 */
export function parseDaemonEvent(raw: string): DaemonEvent | null {
  let value: unknown;
  try {
    value = JSON.parse(raw);
  } catch {
    return null;
  }

  // Externally-tagged unit variants arrive as plain strings; none of the
  // events we care about are unit variants, so ignore them.
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    return null;
  }

  const obj = value as Record<string, unknown>;
  const tag = Object.keys(obj)[0];
  if (tag === undefined) return null;
  const p = obj[tag] as any;

  try {
    switch (tag) {
      case "Created":
        return { type: "Created", handle: p[0], config: p[1], state: p[2] };
      case "NoSuchClient":
        return { type: "NoSuchClient", handle: p };
      case "State":
        return { type: "State", handle: p[0], config: p[1], state: p[2] };
      case "Deleted":
        return { type: "Deleted", handle: p };
      case "PortChanged":
        return { type: "PortChanged", port: p[0], error: p[1] ?? null };
      case "Enumerate":
        return {
          type: "Enumerate",
          clients: (p as Array<[number, ClientConfig, ClientState]>).map(
            ([handle, config, state]) => ({ handle, config, state }),
          ),
        };
      case "Error":
        return { type: "Error", message: p };
      case "CaptureStatus":
        return { type: "CaptureStatus", status: p };
      case "EmulationStatus":
        return { type: "EmulationStatus", status: p };
      case "AuthorizedUpdated":
        return { type: "AuthorizedUpdated", keys: p };
      case "PublicKeyFingerprint":
        return { type: "PublicKeyFingerprint", fingerprint: p };
      case "DeviceConnected":
        return { type: "DeviceConnected", addr: p.addr, fingerprint: p.fingerprint };
      case "DeviceEntered":
        return { type: "DeviceEntered", fingerprint: p.fingerprint, addr: p.addr, pos: p.pos };
      case "IncomingDisconnected":
        return { type: "IncomingDisconnected", addr: p };
      case "ConnectionAttempt":
        return { type: "ConnectionAttempt", fingerprint: p.fingerprint };
      case "PairingStarted":
        return { type: "PairingStarted", pin: p.pin, port: p.port };
      case "PairingComplete":
        return { type: "PairingComplete", fingerprint: p.fingerprint, name: p.name };
      case "PairingFailed":
        return { type: "PairingFailed", reason: typeof p === "string" ? p : String(p) };
      case "PeersDiscovered":
        return { type: "PeersDiscovered", peers: p };
      case "SyncStatus":
        return { type: "SyncStatus", status: p };
      case "VyloState":
        return { type: "VyloState", state: p };
      case "ClipboardSynced":
        return { type: "ClipboardSynced", info: p };
      case "FileTransfer":
        return { type: "FileTransfer", transfer: p };
      case "FilesDropped":
        return { type: "FilesDropped", paths: p.paths ?? [] };
      default:
        return null;
    }
  } catch {
    return null;
  }
}

/* ------------------------------------------------------------------ */
/* UI -> daemon requests                                               */
/* ------------------------------------------------------------------ */

async function send(payload: unknown): Promise<void> {
  await invoke("request", { json: JSON.stringify(payload) });
}

export const requests = {
  sync: () => send("Sync"),
  enumerate: () => send({ Enumerate: [] }),
  activate: (handle: ClientHandle, active: boolean) => send({ Activate: [handle, active] }),
  create: () => send("Create"),
  delete: (handle: ClientHandle) => send({ Delete: handle }),
  updatePosition: (handle: ClientHandle, pos: Position) => send({ UpdatePosition: [handle, pos] }),
  authorizeKey: (description: string, fingerprint: string) =>
    send({ AuthorizeKey: [description, fingerprint] }),
  removeAuthorizedKey: (fingerprint: string) => send({ RemoveAuthorizedKey: fingerprint }),
  saveConfiguration: () => send("SaveConfiguration"),
  enableCapture: () => send("EnableCapture"),
  enableEmulation: () => send("EnableEmulation"),
  /** Ask the daemon to retry both input paths (after granting OS permissions). */
  retryInput: async () => {
    await send("EnableCapture");
    await send("EnableEmulation");
  },
  /* vylo additions */
  startPairing: () => send("StartPairing"),
  cancelPairing: () => send("CancelPairing"),
  pairWithPeer: (addr: string, pin: string) => send({ PairWithPeer: { addr, pin } }),
  sendFiles: (paths: string[]) => send({ SendFiles: paths }),
  setClipboardSync: (enabled: boolean) => send({ SetClipboardSync: enabled }),
  setKeyboardLayoutSync: (enabled: boolean) => send({ SetKeyboardLayoutSync: enabled }),
  setFileDir: (dir: string) => send({ SetFileDir: dir }),
  setDeviceName: (name: string) => send({ SetDeviceName: name }),
};

/* ------------------------------------------------------------------ */
/* Other backend commands                                              */
/* ------------------------------------------------------------------ */

export const backend = {
  pickFiles: () => invoke<string[] | null>("pick_files"),
  pickDir: () => invoke<string | null>("pick_dir"),
  openFileDir: (path: string) => invoke<void>("open_file_dir", { path }),
  setAutostart: (enabled: boolean) => invoke<void>("set_autostart", { enabled }),
  getAutostart: () => invoke<boolean>("get_autostart"),
  getPlatform: () => invoke<string>("get_platform"),
  ipcConnected: () => invoke<boolean>("ipc_connected"),
};
