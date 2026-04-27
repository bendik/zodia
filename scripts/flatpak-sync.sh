#!/usr/bin/env bash
# Post-release hook for cargo-release.
# Updates the flathub submodule (manifest, metainfo, cargo-sources) and pushes.
set -euo pipefail

VERSION="$1"
TAG="v${VERSION}"
COMMIT="$(git rev-parse HEAD)"
FLATHUB="flatpak/flathub"

echo "Syncing flathub submodule to ${TAG} (${COMMIT})..."

# 1. Update manifest tag + commit
sed -i "s|tag: v.*|tag: ${TAG}|"          "${FLATHUB}/io.github.bendik.Zodia.yml"
sed -i "s|commit: .*|commit: ${COMMIT}|"  "${FLATHUB}/io.github.bendik.Zodia.yml"

# 2. Sync metainfo
cp app/data/io.github.bendik.Zodia.metainfo.xml "${FLATHUB}/"

# 3. Regenerate cargo-sources.json (requires: pip install aiohttp toml)
python3 flatpak/flatpak-cargo-generator.py Cargo.lock \
  -o "${FLATHUB}/cargo-sources.json"

# 4. Commit + push submodule to a branch on Flathub remote (master is protected).
# Open a PR at: https://github.com/flathub/io.github.bendik.Zodia/pull/new/update/${TAG}
BRANCH="update/${TAG}"
git -C "${FLATHUB}" add -A
git -C "${FLATHUB}" commit -m "bump to ${TAG}"
git -C "${FLATHUB}" push origin "HEAD:refs/heads/${BRANCH}"

# 5. Record the updated submodule pointer in zodia
git add "${FLATHUB}"
git commit -m "chore: update flathub submodule to ${TAG}"
git push origin main
