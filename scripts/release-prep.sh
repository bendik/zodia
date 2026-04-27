#!/usr/bin/env bash
# Pre-release hook for cargo-release.
# Ensures a <release> entry for VERSION exists in metainfo.xml and stages the file.
set -euo pipefail

VERSION="$1"
DATE="$(date +%Y-%m-%d)"
METAINFO="app/data/io.github.bendik.Zodia.metainfo.xml"

# Insert a new release entry only if one for this version isn't already present.
if ! grep -q "version=\"${VERSION}\"" "$METAINFO"; then
  sed -i "s|<releases>|<releases>\n    <release version=\"${VERSION}\" date=\"${DATE}\">\n      <description><p></p></description>\n    </release>|" "$METAINFO"
  echo "Inserted blank release entry for ${VERSION} in ${METAINFO} — fill in the description before the commit is pushed."
fi

git add "$METAINFO"
