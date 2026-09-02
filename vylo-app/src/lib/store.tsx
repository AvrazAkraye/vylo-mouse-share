import {
  createContext,
  useContext,
  useEffect,
  useReducer,
  useState,
  type Dispatch,
  type ReactNode,
} from "react";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import {
  backend,
  parseDaemonEvent,
  requests,
  type ClientConfig,
  type ClientState,
  type ClipboardSynced,
  type DaemonEvent,
  type DiscoveredPeer,
  type FileTransfer,
  type SyncStatus,
  type VyloState,
} from "./ipc";

/* ------------------------------------------------------------------ */
/* State                                                               */
/* ------------------------------------------------------------------ */

export interface Client {
  handle: number;
  config: ClientConfig;
  state: ClientState;
}

export type PairingPhase =
  | "idle"
  | "showing-pin" // we display a PIN, waiting for peer to enter it
  | "waiting-peer" // we sent PairWithPeer, waiting for result
  | "complete"
  | "failed";

export interface PairingUiState {
  phase: PairingPhase;
  pin: string | null;
  port: number | null;
  error: string | null;
  peerName: string | null;
}

export interface DaemonStore {
  ipcConnected: boolean;
  clients: Record<number, Client>;
  captureEnabled: boolean;
  emulationEnabled: boolean;
  fingerprint: string | null;
  authorized: Record<string, string>;
  vylo: VyloState | null;
  sync: SyncStatus;
  peers: DiscoveredPeer[];
  pairing: PairingUiState;
  transfers: FileTransfer[];
  lastClipboard: (ClipboardSynced & { at: number }) | null;
  /** most recent cross-machine drag-and-drop that landed here */
  lastDrop: { paths: string[]; at: number } | null;
  /** addrs of currently connected incoming devices (DTLS input link) */
  incoming: string[];
  port: number;
  lastError: string | null;
  dragHover: boolean;
}

const initialState: DaemonStore = {
  ipcConnected: false,
  clients: {},
  captureEnabled: false,
  emulationEnabled: false,
  fingerprint: null,
  authorized: {},
  vylo: null,
  sync: { connected: false, peer_name: null },
  peers: [],
  pairing: { phase: "idle", pin: null, port: null, error: null, peerName: null },
  transfers: [],
  lastClipboard: null,
  lastDrop: null,
  incoming: [],
  port: 4242,
  lastError: null,
  dragHover: false,
};

/* ------------------------------------------------------------------ */
/* Actions + reducer                                                   */
/* ------------------------------------------------------------------ */

export type Action =
  | { type: "daemon-event"; event: DaemonEvent }
  | { type: "ipc"; connected: boolean }
  | { type: "drag"; hover: boolean }
  | { type: "pairing-waiting" } // we just sent PairWithPeer
  | { type: "pairing-dismiss" } // cancel / acknowledge result
  | { type: "clear-error" };

const MAX_TRANSFERS = 20;

function upsertTransfer(list: FileTransfer[], t: FileTransfer): FileTransfer[] {
  const idx = list.findIndex((x) => x.id === t.id);
  if (idx >= 0) {
    const next = list.slice();
    next[idx] = t;
    return next;
  }
  return [t, ...list].slice(0, MAX_TRANSFERS);
}

function applyEvent(s: DaemonStore, e: DaemonEvent): DaemonStore {
  switch (e.type) {
    case "Created":
    case "State":
      return {
        ...s,
        clients: {
          ...s.clients,
          [e.handle]: { handle: e.handle, config: e.config, state: e.state },
        },
      };
    case "Deleted": {
      const clients = { ...s.clients };
      delete clients[e.handle];
      return { ...s, clients };
    }
    case "Enumerate": {
      const clients: Record<number, Client> = {};
      for (const c of e.clients) clients[c.handle] = c;
      return { ...s, clients };
    }
    case "NoSuchClient":
      return s;
    case "PortChanged":
      return { ...s, port: e.port, lastError: e.error ?? s.lastError };
    case "Error":
      return { ...s, lastError: e.message };
    case "CaptureStatus":
      return { ...s, captureEnabled: e.status === "Enabled" };
    case "EmulationStatus":
      return { ...s, emulationEnabled: e.status === "Enabled" };
    case "AuthorizedUpdated":
      return { ...s, authorized: e.keys };
    case "PublicKeyFingerprint":
      return { ...s, fingerprint: e.fingerprint };
    case "DeviceConnected":
      return s.incoming.includes(e.addr)
        ? s
        : { ...s, incoming: [...s.incoming, e.addr] };
    case "IncomingDisconnected":
      return { ...s, incoming: s.incoming.filter((a) => a !== e.addr) };
    case "DeviceEntered":
    case "ConnectionAttempt":
      return s;
    case "PairingStarted":
      return {
        ...s,
        pairing: {
          phase: "showing-pin",
          pin: e.pin,
          port: e.port,
          error: null,
          peerName: null,
        },
      };
    case "PairingComplete":
      return {
        ...s,
        pairing: {
          phase: "complete",
          pin: null,
          port: null,
          error: null,
          peerName: e.name,
        },
      };
    case "PairingFailed":
      return {
        ...s,
        pairing: {
          ...s.pairing,
          phase: "failed",
          pin: null,
          error: e.reason || "Pairing failed",
        },
      };
    case "PeersDiscovered":
      return { ...s, peers: e.peers };
    case "SyncStatus":
      return { ...s, sync: e.status };
    case "VyloState":
      return { ...s, vylo: e.state };
    case "ClipboardSynced":
      return { ...s, lastClipboard: { ...e.info, at: Date.now() } };
    case "FileTransfer":
      return { ...s, transfers: upsertTransfer(s.transfers, e.transfer) };
    case "FilesDropped":
      return { ...s, lastDrop: { paths: e.paths, at: Date.now() } };
    default:
      return s;
  }
}

function reducer(s: DaemonStore, a: Action): DaemonStore {
  switch (a.type) {
    case "daemon-event":
      return applyEvent(s, a.event);
    case "ipc":
      return a.connected
        ? { ...s, ipcConnected: true }
        : {
            ...s,
            ipcConnected: false,
            sync: { connected: false, peer_name: s.sync.peer_name },
            incoming: [],
            captureEnabled: false,
            emulationEnabled: false,
          };
    case "drag":
      return s.dragHover === a.hover ? s : { ...s, dragHover: a.hover };
    case "pairing-waiting":
      return {
        ...s,
        pairing: { phase: "waiting-peer", pin: null, port: null, error: null, peerName: null },
      };
    case "pairing-dismiss":
      return {
        ...s,
        pairing: { phase: "idle", pin: null, port: null, error: null, peerName: null },
      };
    case "clear-error":
      return { ...s, lastError: null };
    default:
      return s;
  }
}

/* ------------------------------------------------------------------ */
/* Context / provider                                                  */
/* ------------------------------------------------------------------ */

const StoreContext = createContext<DaemonStore>(initialState);
const DispatchContext = createContext<Dispatch<Action>>(() => {});

export function DaemonProvider({ children }: { children: ReactNode }) {
  const [state, dispatch] = useReducer(reducer, initialState);

  useEffect(() => {
    let disposed = false;
    const unlisteners: Array<() => void> = [];

    (async () => {
      const unDaemon = await listen<string>("daemon", (ev) => {
        const parsed = parseDaemonEvent(ev.payload);
        if (parsed) dispatch({ type: "daemon-event", event: parsed });
      });
      const unIpc = await listen<{ connected: boolean }>("daemon-ipc", (ev) => {
        dispatch({ type: "ipc", connected: ev.payload.connected });
      });
      const unDrag = await getCurrentWebview().onDragDropEvent((ev) => {
        const p = ev.payload;
        if (p.type === "enter" || p.type === "over") {
          dispatch({ type: "drag", hover: true });
        } else if (p.type === "drop") {
          dispatch({ type: "drag", hover: false });
          if (p.paths.length > 0) requests.sendFiles(p.paths).catch(() => {});
        } else {
          dispatch({ type: "drag", hover: false });
        }
      });
      if (disposed) {
        unDaemon();
        unIpc();
        unDrag();
        return;
      }
      unlisteners.push(unDaemon, unIpc, unDrag);

      // Initial state: the bridge may have connected (and synced) before this
      // webview was ready, so ask again.
      try {
        const connected = await backend.ipcConnected();
        if (!disposed) dispatch({ type: "ipc", connected });
        if (connected) await requests.sync();
      } catch {
        /* backend not ready; events will catch us up */
      }
    })();

    return () => {
      disposed = true;
      for (const un of unlisteners) un();
    };
  }, []);

  return (
    <StoreContext.Provider value={state}>
      <DispatchContext.Provider value={dispatch}>{children}</DispatchContext.Provider>
    </StoreContext.Provider>
  );
}

export function useDaemon(): DaemonStore {
  return useContext(StoreContext);
}

export function useDaemonDispatch(): Dispatch<Action> {
  return useContext(DispatchContext);
}

/* Platform is static for the process; fetch once and cache module-wide. */
let platformCache: string | null = null;

/** OS name from the backend ("macos" | "windows" | "linux"), null until known. */
export function usePlatform(): string | null {
  const [platform, setPlatform] = useState(platformCache);
  useEffect(() => {
    if (platformCache !== null) return;
    let disposed = false;
    backend
      .getPlatform()
      .then((p) => {
        platformCache = p;
        if (!disposed) setPlatform(p);
      })
      .catch(() => {});
    return () => {
      disposed = true;
    };
  }, []);
  return platform;
}

/** The peer machine: first client by handle (two-machine design). */
export function usePeerClient(): Client | null {
  const { clients } = useDaemon();
  const handles = Object.keys(clients)
    .map(Number)
    .sort((a, b) => a - b);
  return handles.length > 0 ? clients[handles[0]] : null;
}
