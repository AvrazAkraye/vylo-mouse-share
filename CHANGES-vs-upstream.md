# What changed vs. lan-mouse (and why)

Vylo Mouse Share is a fork of [feschber/lan-mouse](https://github.com/feschber/lan-mouse)
(GPL-3.0). This documents everything that differs from upstream and the reasoning.

## Why lan-mouse (not freemouse)

The brief named `freemouse` as the default base. I audited both first. freemouse's
macOS/Windows path is architecturally broken — it forwards absolute controller
coordinates while the input grab pins the local cursor (so the remote cursor can't
track), its `RemoteLeave` handler turns the controlled machine into an input dead-end,
and pairing can never authenticate because each machine dials out with its *own* random
PIN. Its security is inadequate for keystroke transport (offline-brute-forceable 6-digit
PIN key, no replay protection, file-receive path traversal). It's ~4.7k LOC, unmaintained
since June, with failing CI — generated-and-untested code.

lan-mouse is mature (~15k LOC, 5k+ stars, active), with **working** macOS and Windows
capture *and* emulation (CGEventTap / low-level hooks / SendInput), DTLS-encrypted
transport, and a headless daemon + IPC-socket design that made swapping in a Tauri
frontend clean. It lacks clipboard, files, and PIN pairing — so those are what I built.

## Added

**Clipboard + file transfer + pairing over a new TCP/TLS side-channel** (`src/sync/`).
Input stays on upstream's DTLS/UDP path (port 4242, untouched); all bulk data rides a
separate TLS 1.3 channel (port 4243) so a large file never delays the mouse. It reuses
the machine's existing self-signed certificate identity, mutually authenticated and
pinned to the same `authorized_fingerprints` allowlist.
- `sync/clipboard.rs` — single-owner clipboard thread, text + images (PNG), ring-buffer
  echo suppression that survives OS re-encoding, decompression-bomb guard.
- `sync/files.rs` + `sync/mod.rs` — chunked transfer, sha256 verify, `.vylopart` →
  rename on success, filename sanitization (traversal + Windows reserved names), size &
  per-chunk bounds, into `~/Downloads/VyloShare`.
- `sync/pairing.rs` — 6-digit PIN → SPAKE2 (no offline brute-force) bound to the TLS
  session via exported keying material (defeats a relay MITM), then fingerprint exchange
  into the allowlist and automatic client setup.
- `sync/discovery.rs` — mDNS `_vylo-share._tcp` LAN discovery for the pairing screen.

**Tauri desktop app** (`vylo-app/`) — React + TypeScript + Tailwind, menu-bar/tray-first,
four screens (Pairing / Layout / Status / Settings). Embeds the daemon in-process (single
binary; macOS TCC permissions attach to the app) and talks to it over upstream's IPC
socket. Replaces the GTK frontend.

**IPC additions** (`lan-mouse-ipc`) — new `FrontendRequest`/`FrontendEvent` variants for
pairing, file transfer, clipboard toggle, discovery and Vylo settings.

**Config** — new keys: `clipboard_sync`, `file_dir`, `sync_port`, `device_name`; per
client: `speed` and `[clients.modifiers]`.

**Per-device input tuning** (`src/tuning.rs`, applied in `src/capture.rs` at send time)
— pointer speed multiplier (with sub-pixel carry so slow speeds don't drop motion) and
a modifier remap (what local Ctrl / Alt / Meta act as on the peer, applied to key codes
and to the XKB bitmask; synthesized key-ups on release are remapped identically).
Configured per client from Settings → *Input on <device>*.

**CI** (`.github/workflows/build.yml`) — tests on macOS + Windows, builds macOS `.dmg`
and Windows NSIS installer, publishes a GitHub release on tag together with signed
updater artifacts and the `latest.json` manifest.

**In-app updates** (`vylo-app/src/components/UpdateCard.tsx`, `tauri-plugin-updater`)
— Settings → About shows the installed version and a *Check for updates* button that
downloads, installs and relaunches. Manifest and artifacts come from GitHub Releases;
every artifact is minisign-signed in CI and verified against the public key embedded
in `tauri.conf.json` before install. This is the only non-LAN connection the app
makes, and only on user action (or when Settings is opened).

## Security fix to upstream

Upstream's **outbound** DTLS connection set `insecure_skip_verify` with no server
pinning (`src/connect.rs`) — a LAN attacker could impersonate the receiving host and
capture keystrokes. The fork pins the peer's certificate fingerprint against the
allowlist on outbound connections too.

## Removed

The GTK frontend (`lan-mouse-gtk`), Nix/flake and Flatpak packaging, the systemd unit,
desktop files, and screenshots — all replaced by the Tauri app and CI. Internal crate
names (`lan-mouse-ipc`, `lan-mouse-proto`, …) were intentionally kept to keep future
upstream merges tractable; only the product, binary, and user-facing names became Vylo.

## Renamed

Package `lan-mouse` → `vylo-mouse-share`, binary → `vylo`, config dir → `vylo/`, cert →
`vylo.pem`, IPC socket → `vylo-socket.sock`, app/window/identifier → Vylo Mouse Share /
`com.vylo.mouseshare`, tray tooltip → "Vylo".

## Verification

All new security-critical code has tests (`src/sync/mod.rs` `#[cfg(test)]`): full
TLS+SPAKE2 pairing over localhost, wrong-PIN rejection, unauthorized-peer rejection, the
strict-connector-rejects-unpinned-peer-during-pairing-window regression test, protocol
framing, oversized-frame rejection, and filename sanitization (traversal + reserved
names). The whole new sync subsystem also went through a multi-agent adversarial review;
the confirmed findings are fixed (see the "Harden sync channel" commit).

**Not runtime-tested end to end:** actual two-machine cursor edge-switching, clipboard,
and file transfer require a real Mac + Windows pair on a LAN, which this build
environment doesn't have. The daemon starts cleanly, binds both channels, and the code
paths are unit-tested, but you should treat the first real two-machine run as the
integration test.
