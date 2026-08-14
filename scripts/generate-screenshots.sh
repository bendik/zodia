#!/usr/bin/env bash
# Regenerate docs/screenshots/*.png from two live app instances.
#
# The heavy lifting is the cucumber scenario in app/tests/screenshots.rs
# (feature: app/tests/features/screenshots.feature): two instances driven
# through ZODIA_SCREENSHOT_SCRIPT discover each other over the real P2P
# stack, connect, share a circle, and snapshot their own windows — an
# 800x600 logical window captured at a real 2x scale factor (1600x1200
# PNGs, crisp like a Retina screenshot without an oversized capture
# window). This wrapper just points it at a work dir and downsizes the
# results into docs/screenshots/ (800x620, the size the metainfo's
# screenshot URLs have always served).
#
# Needs: a display (any Wayland/X11 session; headless works under `cage`),
# real UDP networking (mDNS discovery), dbus-run-session, imagemagick.
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

OUT="$(mktemp -d)"
trap 'rm -rf "$OUT"' EXIT

echo "Generating screenshots into ${OUT}..."
ZODIA_SCREENSHOTS_OUT="$OUT" cargo test -p zodia-app --test screenshots

for shot in chart network synastry circles; do
  magick "${OUT}/${shot}.png" -resize 800x620! "docs/screenshots/${shot}.png"
  echo "docs/screenshots/${shot}.png updated"
done

echo "Done — review with: git diff --stat docs/screenshots/"
