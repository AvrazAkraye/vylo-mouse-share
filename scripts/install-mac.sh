#!/usr/bin/env bash
# Build, sign and install Vylo Mouse Share on this Mac — cleanly.
#
# Why this exists: an unsigned (ad-hoc) build gets a NEW code signature on
# every rebuild, which makes macOS forget the Accessibility / Input
# Monitoring grants each time, and every leftover build bundle registers
# as a duplicate "Vylo Mouse Share" in Launch Services. This script
# signs with a stable identity (your Apple Development cert if present),
# installs exactly one copy, and purges the build artifacts so grants
# persist and only one app is ever registered.
#
# Usage:  scripts/install-mac.sh            # build + sign + install + relaunch
#         SIGN_ID="Apple Development: ..." scripts/install-mac.sh   # explicit identity
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
APP_NAME="Vylo Mouse Share.app"
SRC="$ROOT/target/release/bundle/macos/$APP_NAME"
DEST="/Applications/$APP_NAME"
LSR=/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister

# pick a stable signing identity: explicit > Apple Development > ad-hoc
if [[ -z "${SIGN_ID:-}" ]]; then
  SIGN_ID="$(security find-identity -v -p codesigning 2>/dev/null \
    | grep -oE '"Apple Development: [^"]+"' | head -1 | tr -d '"')"
fi
if [[ -z "${SIGN_ID:-}" ]]; then
  echo "warning: no Apple Development identity found; using ad-hoc signing" >&2
  echo "         (permissions will reset on each install — see README)" >&2
  SIGN_ID="-"
fi

echo "==> building release app"
( cd "$ROOT/vylo-app" && npx tauri build --bundles app )

echo "==> signing with: $SIGN_ID"
codesign --force --deep --sign "$SIGN_ID" "$SRC"
codesign --verify --deep --strict "$SRC"

echo "==> installing"
osascript -e 'quit app "Vylo Mouse Share"' 2>/dev/null || true
pkill -9 -f "$APP_NAME/Contents/MacOS" 2>/dev/null || true
sleep 2
rm -f "$HOME/Library/Caches/vylo-socket.sock"
rm -rf "$DEST"
cp -R "$SRC" /Applications/
xattr -dr com.apple.quarantine "$DEST" 2>/dev/null || true

echo "==> purging build copy so only /Applications is registered"
"$LSR" -u "$SRC" >/dev/null 2>&1 || true
rm -rf "$SRC"
"$LSR" -f "$DEST" >/dev/null 2>&1 || true

echo "==> registered copies (should be exactly one):"
"$LSR" -dump 2>/dev/null | grep -iE "^\s*path:.*$APP_NAME" | sort -u

open "$DEST"
echo "==> installed and launched: $DEST ($(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$DEST/Contents/Info.plist"))"
