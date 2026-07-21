#!/usr/bin/env bash
# Build a fully self-contained AppImage of Gapless.
#
# The binary dynamically links GTK4 + libadwaita + GStreamer (~150 shared
# objects). GNOME boxes have the GTK side but a fresh KDE/Kubuntu machine
# does not ship libadwaita, and no distro guarantees the exact GStreamer
# plugin set — so a bare binary dies on "libadwaita-1.so.0: cannot open
# shared object file" or plays nothing. This bundles the whole GTK and
# GStreamer stacks (all system gst plugins included) into one file that runs
# on any distro with no packages installed on the target.
#
# Output: dist/Gapless-x86_64.AppImage
set -euo pipefail

APP_ID=com.procomputation.Gapless
REPO="$(cd "$(dirname "$0")/.." && pwd)"
TARGET_DIR="${CARGO_TARGET_DIR:-$HOME/.cache/cargo-target/gapless}"
BIN="$TARGET_DIR/release/gapless"
# The repo lives on an exfat volume, which supports neither the symlinks nor
# the permission bits an AppDir needs. Assemble on a native fs under ~/.cache
# and copy only the finished single-file AppImage back into the repo.
TOOLS="$HOME/.cache/gapless/appimage-tools"
WORK="$HOME/.cache/gapless/appimage-build"
APPDIR="$WORK/AppDir"
DIST="$REPO/dist"

# AppImage tools are themselves AppImages — extract-and-run avoids needing FUSE.
export APPIMAGE_EXTRACT_AND_RUN=1
export DEPLOY_GTK_VERSION=4

fetch() {  # fetch URL DEST (skip if present)
  local url="$1" dest="$2"
  [ -f "$dest" ] && return 0
  echo "  ↓ $(basename "$dest")"
  curl -f#L --retry 3 -o "$dest" "$url"
  chmod +x "$dest"
}

echo "==> Building release binary"
# cd first: cargo resolves .cargo/config.toml (and its target-dir) from the
# working directory, not from --manifest-path.
( cd "$REPO" && cargo build --release )
[ -x "$BIN" ] || { echo "error: release binary not found at $BIN" >&2; exit 1; }

echo "==> Fetching bundling tools into $TOOLS"
mkdir -p "$TOOLS"
fetch "https://github.com/linuxdeploy/linuxdeploy/releases/download/continuous/linuxdeploy-x86_64.AppImage" \
      "$TOOLS/linuxdeploy-x86_64.AppImage"
fetch "https://github.com/linuxdeploy/linuxdeploy-plugin-gtk/raw/master/linuxdeploy-plugin-gtk.sh" \
      "$TOOLS/linuxdeploy-plugin-gtk.sh"
fetch "https://github.com/linuxdeploy/linuxdeploy-plugin-gstreamer/raw/master/linuxdeploy-plugin-gstreamer.sh" \
      "$TOOLS/linuxdeploy-plugin-gstreamer.sh"
fetch "https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-x86_64.AppImage" \
      "$TOOLS/appimagetool-x86_64.AppImage"

echo "==> Assembling AppDir"
rm -rf "$WORK"
mkdir -p "$APPDIR/usr/bin" \
         "$APPDIR/usr/share/applications" \
         "$APPDIR/usr/share/icons/hicolor/scalable/apps"
install -m755 "$BIN" "$APPDIR/usr/bin/gapless"
install -m644 "$REPO/data/$APP_ID.desktop" "$APPDIR/usr/share/applications/$APP_ID.desktop"
install -m644 "$REPO/data/$APP_ID.svg" \
        "$APPDIR/usr/share/icons/hicolor/scalable/apps/$APP_ID.svg"

echo "==> Running linuxdeploy + gtk + gstreamer plugins"
PATH="$TOOLS:$PATH" "$TOOLS/linuxdeploy-x86_64.AppImage" \
  --appdir "$APPDIR" \
  --plugin gtk \
  --plugin gstreamer \
  --desktop-file "$APPDIR/usr/share/applications/$APP_ID.desktop" \
  --icon-file "$APPDIR/usr/share/icons/hicolor/scalable/apps/$APP_ID.svg" \
  --executable "$APPDIR/usr/bin/gapless"

# The gstreamer hook reads $APPDIR but only the gtk hook — sourced AFTER it —
# defaults the variable. Launched as an extracted tree (no AppImage runtime to
# set APPDIR), the GStreamer plugin paths would come out empty and playback
# would find no decoders. Default APPDIR at the top of AppRun itself.
sed -i 's|^this_dir=.*|&\nexport APPDIR="${APPDIR:-$this_dir}"|' "$APPDIR/AppRun"

echo "==> Packing AppImage"
( cd "$WORK" && PATH="$TOOLS:$PATH" ARCH=x86_64 \
    "$TOOLS/appimagetool-x86_64.AppImage" AppDir "Gapless-x86_64.AppImage" )

mkdir -p "$DIST"
OUT="$DIST/Gapless-x86_64.AppImage"
# cp (not mv) — the finished .AppImage is a plain file, fine on the exfat repo.
cp -f "$WORK/Gapless-x86_64.AppImage" "$OUT"
chmod +x "$OUT"
echo
echo "Built: $OUT ($(du -h "$OUT" | cut -f1))"
