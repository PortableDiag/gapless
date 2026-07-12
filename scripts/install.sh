#!/usr/bin/env bash
# Installs the binary, icon and desktop entry into ~/.local so the app appears in
# the launcher with a proper icon instead of the generic fallback.
set -euo pipefail
cd "$(dirname "$0")/.."

APP_ID=com.procomputation.Gapless
BIN_DIR="$HOME/.local/bin"
ICON_DIR="$HOME/.local/share/icons/hicolor/scalable/apps"
DESKTOP_DIR="$HOME/.local/share/applications"

cargo build --release
BIN=$(cargo metadata --format-version 1 --no-deps \
      | python3 -c 'import json,sys;print(json.load(sys.stdin)["target_directory"])')/release/gapless

mkdir -p "$BIN_DIR" "$ICON_DIR" "$DESKTOP_DIR"
install -m755 "$BIN" "$BIN_DIR/gapless"
install -m644 "data/$APP_ID.svg" "$ICON_DIR/$APP_ID.svg"
install -m644 "data/$APP_ID.desktop" "$DESKTOP_DIR/$APP_ID.desktop"

# Without these the icon cache and MIME associations do not pick the new files up.
gtk-update-icon-cache -qtf "$HOME/.local/share/icons/hicolor" 2>/dev/null || true
update-desktop-database -q "$DESKTOP_DIR" 2>/dev/null || true

echo "Installed:"
echo "  $BIN_DIR/gapless"
echo "  $ICON_DIR/$APP_ID.svg"
echo "  $DESKTOP_DIR/$APP_ID.desktop"
echo
echo "If ~/.local/bin is not on your PATH, add it. Launch from your menu as 'Gapless'."
