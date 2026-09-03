# Vylo Mouse Share — Setup

Two machines, one wired LAN, one mouse + keyboard, shared clipboard, file drops.

## 1. Install

### macOS
1. Open the `.dmg` and drag **Vylo Mouse Share** to Applications.
2. The app is unsigned, so the first launch needs: **right-click the app → Open → Open**
   (or approve it under *System Settings → Privacy & Security*).
3. On first run macOS will ask for two permissions — both are required for input sharing:
   - **Accessibility** (to move the cursor and type on this machine)
   - **Input Monitoring** (to capture the mouse/keyboard when you switch screens)
   Grant them under *System Settings → Privacy & Security*, then use the app's
   re-enable buttons (or restart the app).

### Windows
1. Run the `.exe` installer (NSIS). SmartScreen may warn because it's unsigned —
   *More info → Run anyway*.
2. No special permissions are needed on Windows.
3. If Windows Firewall asks, allow Vylo on **private networks** (it needs UDP 4242 and
   TCP 4243 on the LAN).

## 2. Pair the two machines (once, under a minute)

1. Start Vylo on both machines.
2. On machine A open the **Pairing** screen and press **Show PIN** — a 6-digit PIN
   appears and A waits for a peer (2-minute window).
3. On machine B the **Pairing** screen lists nearby devices (via mDNS). Click **Pair**
   next to machine A and type the PIN.
   - If discovery doesn't show the peer (some networks block multicast), use
     **Pair by address** with A's IP and sync port, e.g. `192.168.1.20:4243`.
4. Done. Both machines now trust exactly each other (certificate fingerprints are
   pinned on both sides); everything is encrypted end-to-end. The wrong PIN burns the
   window — press Show PIN again to retry.

## 3. Arrange the screens

Open **Layout** and drag the peer's screen to whichever side it physically sits on
(left/right/top/bottom). Default after pairing: the machine where you typed the PIN
treats the other one as being on its **left**.

Move the cursor off that screen edge and it crosses over; move it back the opposite
edge to return. Stuck? Press **Ctrl+Shift+Alt/Option+Meta/Cmd** (the release bind) to
yank the cursor back.

## 3b. Keys and mouse speed on the other machine

**Settings → Input on <device>** tunes how *this* keyboard and mouse behave over there:

- **Mouse speed** — a multiplier (0.25× to 4×) for cursor movement while it's on the
  other machine. Each machine sets its own; the setting travels with the mouse, so on
  the Mac you set the speed for the Windows screen and on the PC the speed for the Mac.
- **Modifier keys** — what each of your Control / Option / Command (or Ctrl / Alt /
  Windows) keys acts as on the other side. Typical uses:
  - A Mac keyboard on a Windows PC: set **Command → Ctrl** so ⌘C / ⌘V copy and paste.
  - You swapped Control and Command in *macOS System Settings → Keyboard → Modifier
    Keys* (so Ctrl+C copies on the Mac): macOS applies that swap before Vylo sees the
    key, which is why your Ctrl showed up as the Windows key. Press **Swap ⌃ and ⌘**
    to undo it for the peer.
  - Two keys may act as the same thing (e.g. both Control and Command → Ctrl).

Changes apply immediately, only affect keys sent *from* this machine, and are stored
per device in `config.toml` (`speed` and `[clients.modifiers]`).

## 4. Clipboard

On by default — copy text or an image on one machine, paste on the other. Toggle it
from the tray/menu-bar icon or the Status screen.

## 5. Send files and folders

Three ways, all of which take files *and* folders:

- **Drag off the edge of the screen** — start dragging in Finder/Explorer, move the
  cursor across to the other machine and release: the items land on its Desktop.
- **Drop onto the Vylo window** (drop overlay appears).
- **Choose files… / Choose folder…** on the Status screen.

Files land on the other machine in `~/Downloads/VyloShare` (configurable in Settings),
integrity-checked with sha256. A folder arrives with its structure intact — subfolders,
empty folders included — and appears only once every file inside has verified; if
anything fails, nothing half-copied is left behind. Symbolic links inside a folder are
skipped. Both machines need Vylo 1.0.5 or newer for folders.

## 6. Everyday use

Vylo lives in the menu bar (macOS) / system tray (Windows): connection status,
clipboard toggle, received-files folder, quit. Closing the window hides it — the app
keeps running. Enable **Start on login** in Settings on both machines and forget it.

## 7. Updating

The installed version shows at the bottom of the sidebar and in **Settings → About**.
Press **Check for updates** (Settings also checks quietly when opened); if a newer
release exists the button becomes **Update to vX.Y.Z** — click it to download, install
and relaunch. Updates are fetched from this project's GitHub Releases only and are
installed solely when their signature verifies against the key built into the app.
This is the one connection Vylo makes outside your LAN, and only when you open
Settings or press the button.

Update both machines: new features that touch the wire protocol need matching
versions on both sides.

On macOS, an update installed this way carries the release build's signature, so
macOS may ask you to re-grant **Accessibility** / **Input Monitoring** afterwards.
If it does, re-tick both in *System Settings → Privacy & Security* and press Retry.

## Known limitations

- Both machines must use the same sync port (default 4243); change it in
  `config.toml` (`sync_port`) on both if it collides with something.
- Clipboard sync covers text and images (not files-on-clipboard or rich text).
- Very large clipboard images sync with a short delay (they are PNG-encoded).
- A receiving Windows machine with no physical mouse attached may hide its cursor
  until a mouse is plugged in once (Windows quirk inherited from upstream).
- Unsigned builds: Gatekeeper/SmartScreen prompts on first launch (see above).
- The config file lives at `~/.config/vylo/config.toml` (macOS/Linux) or
  `%LOCALAPPDATA%\vylo\config.toml` (Windows).
