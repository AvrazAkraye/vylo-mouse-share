# Vylo Mouse Share — Design

Fork of [lan-mouse](https://github.com/feschber/lan-mouse) (GPL-3.0-or-later). Two-machine
(Mac ↔ Windows) software KVM: shared mouse/keyboard with edge switching (inherited, DTLS/UDP),
plus new: clipboard sync (text + images), file transfer, PIN pairing, mDNS discovery, and a
Tauri UI replacing the GTK frontend.

## Architecture

```
┌─────────────────────────── Vylo Mouse Share.app (Tauri) ───────────────────────────┐
│  React UI (Pair / Layout / Status / Settings + tray)                               │
│      ⇅ tauri events + commands (thin JSON bridge)                                  │
│  Tauri Rust backend ── connects to IPC socket like any frontend                    │
│  Daemon thread: Service::run() (embedded; falls back to connect-only if a daemon   │
│  is already running)                                                               │
└────────────────────────────────────────────────────────────────────────────────────┘
   Service (single-threaded tokio LocalSet, actor pattern, one big select!)
   ├─ Capture / Emulation / DTLS-UDP input path (port 4242)   [unchanged upstream]
   ├─ VyloSync actor  [NEW]  — TCP+TLS side-channel (port 4243)
   │    ├─ clipboard monitor (single thread, echo-suppressed, text+PNG images)
   │    ├─ file transfers (chunked, sha256-verified, .vylopart → rename)
   │    ├─ PIN pairing (SPAKE2 + TLS exporter channel binding)
   │    └─ mDNS discovery (_vylo-share._tcp, LAN multicast only)
   └─ IPC listener (newline-JSON; unix socket on macOS, 127.0.0.1:5252 on Windows)
```

Input stays on DTLS/UDP; bulk data (clipboard images, files) rides the separate TCP/TLS
channel so large transfers never block input.

## Identity & security

- One self-signed ECDSA P-256 cert per machine (upstream), PEM at config dir, sha256
  colon-hex fingerprint = device identity for BOTH channels.
- `authorized_fingerprints` allowlist (upstream) is the single trust store.
- NEW: outbound DTLS connections now also verify the peer's cert fingerprint against the
  allowlist (upstream had `insecure_skip_verify` with no pinning → keystroke MITM risk).
- Sync channel: TLS 1.3, mutual auth, both sides pin via the same allowlist.
- Pairing: while a 120s pairing window is open, one unauthorized TLS conn is allowed.
  6-digit PIN → SPAKE2 (symmetric, Ed25519 group) → key K → both sides exchange
  HMAC-SHA256(K, label ‖ TLS exporter). The exporter binds the PIN proof to this exact TLS
  session, so a MITM bridging two sessions fails the MAC, and PAKE means a sniffer gets no
  offline PIN oracle. On success each side reads the peer's cert from the TLS session,
  stores its fingerprint in the allowlist, and auto-creates the peer as a client
  (PIN-entering side places peer Left; PIN-showing side places peer Right).

## Sync wire protocol (TCP/TLS :4243)

Frames: u32-BE length + bincode. Max frame 1 MiB. Messages:

```
Hello { version: u16, name: String }
ClipText { text: String }
ClipImage { width: u32, height: u32, png: Vec<u8> }
FileOffer { id: u64, name: String, size: u64 }
FileAccept { id } | FileReject { id, reason }
FileChunk { id, offset: u64, data: Vec<u8> }        // 256 KiB
FileDone { id, sha256: [u8;32] }                    // receiver verifies, then renames
FileCancel { id, reason }
PairStart { spake_msg, name } / PairResponse { spake_msg, name }
PairConfirmA { mac: [u8;32] } / PairConfirmB { mac: [u8;32] }
Ping / Pong
```

Connection policy: both machines listen; the actor dials any known candidate address
(client IPs, incoming-conn addrs, discovered peers) with backoff. At most one active sync
connection; simultaneous-connect tie broken by fingerprint ordering.

Received files land in `file_dir` (default `~/Downloads/VyloShare`), filenames sanitized to
their basename, collisions get ` (1)` suffixes, written as `.vylopart` and renamed only
after the sha256 matches.

Clipboard echo prevention: the monitor records sha256 of the last content it *applied* from
the peer and the last it *sent*; matching content is never re-broadcast. Exactly one
monitor thread, owned by the actor.

## Config (config.toml, dir `vylo` — e.g. ~/.config/vylo/, %LOCALAPPDATA%\vylo\)

New top-level keys (all optional):
- `clipboard_sync: bool` (default true)
- `file_dir: path` (default ~/Downloads/VyloShare)
- `sync_port: u16` (default 4243; must match on both machines)
- `device_name: string` (default OS hostname)
Cert file: `vylo.pem`. IPC socket: `vylo-socket.sock` (macOS: ~/Library/Caches/).

## IPC additions (lan-mouse-ipc crate; newline-delimited externally-tagged serde JSON)

FrontendRequest (UI → daemon):
```
"StartPairing"                                   // open pairing window, daemon picks PIN
{"PairWithPeer": {"addr": "192.168.1.7:4243", "pin": "123456"}}
"CancelPairing"
{"SendFiles": ["/path/a.zip", "/path/b.png"]}
{"SetClipboardSync": true}
{"SetFileDir": "/Users/x/Downloads/VyloShare"}
{"SetDeviceName": "avraz-mac"}
```
(plus all upstream variants: Activate, Create, Delete, UpdatePosition, Sync, AuthorizeKey, …)

FrontendEvent (daemon → all frontends; `Sync` triggers full state broadcast incl. VyloState):
```
{"PairingStarted": {"pin": "123456", "port": 4243}}
{"PairingComplete": {"fingerprint": "..", "name": "avraz-pc"}}
{"PairingFailed": "wrong pin"}
{"PeersDiscovered": [{"name": "avraz-pc", "addrs": ["192.168.1.7"], "port": 4243}]}   // full list
{"SyncStatus": {"connected": true, "peer_name": "avraz-pc"}}
{"VyloState": {"clipboard_sync": true, "file_dir": "...", "device_name": "...", "sync_port": 4243}}
{"ClipboardSynced": {"direction": "sent"|"received", "kind": "text"|"image"}}
{"FileTransfer": {"id": 1, "name": "a.zip", "direction": "sent"|"received",
                  "transferred": 12345, "total": 99999,
                  "state": "active"|"done"|"failed", "detail": "path or error"}}
```

## Tauri app (`vylo-app/`)

- Backend: spawns daemon thread (embedded service); if `AlreadyRunning`, just connects.
  Bridge: command `request(json: String)` writes one IPC line; every IPC event line is
  re-emitted to the webview as tauri event `"daemon"` with the raw JSON string payload.
  Extra commands: `pick_files() -> Vec<String>`, `open_file_dir()`, `pick_dir() -> String`,
  `set_autostart(bool)` / `get_autostart() -> bool`, `local_fingerprint() -> String`.
- Drag-drop: webview file-drop events → `SendFiles`.
- Tray (primary entry point): icon shows connection state; menu = status line, toggle
  clipboard sync, open received-files folder, Show window, Quit. Window close = hide.
- Screens (React + Tailwind, shadcn-style, minimal settings-app aesthetic, dark/light):
  1. **Pairing** — this device's name + PIN button ("Show PIN" → big PIN + "waiting…");
     discovered devices list → click → PIN entry → paired. Manual "IP:port" fallback field.
  2. **Layout** — two screen rectangles, drag peer to left/right/top/bottom of this machine
     → `UpdatePosition`.
  3. **Status** (default screen) — peer name, input-link state (DeviceConnected /
     CaptureStatus / EmulationStatus), sync-channel state, clipboard toggle, recent
     transfers with progress, "Send files…" button, open folder shortcut.
  4. **Settings** — device name, file drop folder, sync port, start on login,
     this device's fingerprint (copyable), authorized devices list with remove.
- productName "Vylo Mouse Share", identifier com.vylo.mouseshare, window title
  "Vylo Mouse Share", tray tooltip "Vylo".

## Packaging

- macOS: `tauri build` → .app + .dmg (unsigned; Gatekeeper right-click-open documented).
- Windows: GitHub Actions (windows-latest) NSIS installer; local `cargo-xwin` cross-build
  as secondary path. CI workflow builds both OS artifacts on tag push.

## Removed vs upstream

- GTK frontend (lan-mouse-gtk), flatpak/nix packaging, desktop files — replaced by Tauri.
- Everything else (input backends, proto, DTLS path) inherited with minimal diffs to ease
  future upstream merges. Internal crate names (`lan-mouse-*`) intentionally kept.
