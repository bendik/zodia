#!/usr/bin/env bash
# bundle-macos.sh — build a macOS .app bundle for Zodia
#
# Requirements (all via Homebrew on macOS):
#   brew install imagemagick          # SVG → PNG → .icns
#   brew install dylibbundler         # optional: bundle GTK libs for distribution
#
# Usage:
#   ./tools/bundle-macos.sh           # builds target/macos/Zodia.app
#   OPEN=1 ./tools/bundle-macos.sh    # also opens the app after bundling

set -euo pipefail

# ── config ────────────────────────────────────────────────────────────────────
APP_NAME="Zodia"
BUNDLE_ID="io.github.bendik.Zodia"
BINARY="zodia"
SVG="app/data/io.github.bendik.Zodia.svg"
OUT="target/macos/${APP_NAME}.app"

# ── sanity checks ─────────────────────────────────────────────────────────────
cd "$(git rev-parse --show-toplevel)"

if ! command -v magick &>/dev/null && ! command -v convert &>/dev/null; then
    echo "error: ImageMagick not found  →  brew install imagemagick" >&2; exit 1
fi
MAGICK=$(command -v magick 2>/dev/null || command -v convert)

if ! command -v iconutil &>/dev/null; then
    echo "error: iconutil not found — this script must run on macOS" >&2; exit 1
fi

# ── version ───────────────────────────────────────────────────────────────────
VERSION=$(grep '^version' Cargo.toml | head -1 | sed 's/.*"\(.*\)".*/\1/')
echo "Bundling ${APP_NAME} v${VERSION}"

# ── build ─────────────────────────────────────────────────────────────────────
echo ""
echo "▸ cargo build --release"
cargo build --release -p zodia-app

# ── .icns ─────────────────────────────────────────────────────────────────────
echo ""
echo "▸ generating .icns from SVG"

ICONSET_DIR=$(mktemp -d)
ICONSET="${ICONSET_DIR}/Zodia.iconset"
mkdir -p "$ICONSET"

# render SVG at $1 pixels square, write to $ICONSET/$2
render() {
    "$MAGICK" -background none -density 300 "$SVG" -resize "$1x$1" \
        +profile '*' "${ICONSET}/$2"
}

render 16   icon_16x16.png
render 32   icon_16x16@2x.png
render 32   icon_32x32.png
render 64   icon_32x32@2x.png
render 128  icon_128x128.png
render 256  icon_128x128@2x.png
render 256  icon_256x256.png
render 512  icon_256x256@2x.png
render 512  icon_512x512.png
render 1024 icon_512x512@2x.png

ICNS_PATH="${ICONSET_DIR}/${BINARY}.icns"
iconutil -c icns -o "$ICNS_PATH" "$ICONSET"
echo "  generated: ${ICNS_PATH}"

# ── .app structure ────────────────────────────────────────────────────────────
echo ""
echo "▸ assembling ${OUT}"

rm -rf "$OUT"
mkdir -p "$OUT/Contents/MacOS"
mkdir -p "$OUT/Contents/Resources"

cp "target/release/${BINARY}"  "$OUT/Contents/MacOS/${BINARY}"
cp "$ICNS_PATH"                 "$OUT/Contents/Resources/${BINARY}.icns"
chmod +x "$OUT/Contents/MacOS/${BINARY}"

cat > "$OUT/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>               <string>${APP_NAME}</string>
    <key>CFBundleDisplayName</key>        <string>${APP_NAME}</string>
    <key>CFBundleIdentifier</key>         <string>${BUNDLE_ID}</string>
    <key>CFBundleVersion</key>            <string>${VERSION}</string>
    <key>CFBundleShortVersionString</key> <string>${VERSION}</string>
    <key>CFBundlePackageType</key>        <string>APPL</string>
    <key>CFBundleExecutable</key>         <string>${BINARY}</string>
    <key>CFBundleIconFile</key>           <string>${BINARY}</string>
    <key>NSHighResolutionCapable</key>    <true/>
    <key>NSHumanReadableCopyright</key>   <string>Copyright © 2026 Bendik Aagaard Lynghaug. AGPL-3.0-or-later.</string>
</dict>
</plist>
PLIST

# ── dylibbundler (optional) ───────────────────────────────────────────────────
if command -v dylibbundler &>/dev/null; then
    echo ""
    echo "▸ dylibbundler — bundling GTK/GLib libraries"
    mkdir -p "$OUT/Contents/libs"
    dylibbundler -od -b \
        -x "$OUT/Contents/MacOS/${BINARY}" \
        -d "$OUT/Contents/libs" \
        -p @executable_path/../libs
else
    echo ""
    echo "  note: dylibbundler not found — skipping library bundling."
    echo "        The .app requires GTK4 + libadwaita installed (e.g. via Homebrew)."
    echo "        For a self-contained distributable: brew install dylibbundler"
fi

# ── done ─────────────────────────────────────────────────────────────────────
echo ""
echo "✓ ${OUT}"

if [[ "${OPEN:-0}" == "1" ]]; then
    open "$OUT"
fi
