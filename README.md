# Vylo Mouse Share

Share one mouse and keyboard between two machines on the same LAN — move the cursor off
the edge of one screen and it appears on the other, Barrier/Synergy style — plus:

- **Clipboard sync** — text and images, both directions, with echo prevention
- **File transfer** — drag files onto the Vylo window (or use *Send files…*); they arrive
  sha256-verified in `~/Downloads/VyloShare` on the other machine
- **PIN pairing** — a 6-digit PIN pairs exactly your two machines (SPAKE2, no offline
  brute-force), after which both channels are mutually authenticated end-to-end
- **LAN-only** — traffic is DTLS/TLS-encrypted peer-to-peer; no servers, no telemetry,
  nothing leaves your network. The single exception is the on-demand update check
  (Settings → About), which fetches signed releases from this GitHub repo

Supported: **macOS ↔ Windows** (the primary target), and the Linux backends inherited
from upstream.

See [SETUP.md](SETUP.md) for installation, pairing and usage.

## Architecture

Input events travel over the low-latency DTLS/UDP channel (port 4242). Clipboard and
files travel over a dedicated TCP/TLS channel (port 4243) so a large transfer never
delays your mouse. Both channels authenticate with the same per-machine certificate,
pinned during pairing. The desktop app is Tauri (`vylo-app/`); it embeds the daemon and
lives in the menu bar / tray.

## Building

```sh
# headless daemon
cargo build --release --no-default-features

# desktop app
cd vylo-app && npm install && npx tauri build
```

## Credits & license

Vylo Mouse Share is a fork of [lan-mouse](https://github.com/feschber/lan-mouse) by
Ferdinand Bachmann and contributors — the input capture/emulation, DTLS transport and
service architecture come from that excellent project. Clipboard sync, file transfer,
PIN pairing, mDNS discovery, the hardened outbound certificate pinning and the Tauri app
are new in this fork.

Licensed under the [GNU GPL v3.0 or later](LICENSE), same as upstream.
