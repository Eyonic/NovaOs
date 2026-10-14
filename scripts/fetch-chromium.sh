#!/usr/bin/env bash
# Download the Chromium engine (Chrome for Testing "chrome-headless-shell", linux64)
# into build/chromium/. scripts/build.sh then ships it in the image under /opt/chromium,
# with the shared libraries it needs, for Nova's browser.
#   scripts/fetch-chromium.sh            latest Stable
#   CHROMIUM_VERSION=155.0.8059.79 scripts/fetch-chromium.sh
source "$(dirname "$0")/common.sh"
dest="$BUILD/chromium"
mkdir -p "$dest"
index="https://googlechromelabs.github.io/chrome-for-testing/last-known-good-versions-with-downloads.json"
if [[ -z "${CHROMIUM_VERSION:-}" ]]; then
    CHROMIUM_VERSION="$(curl -fsSL "$index" | python3 -c 'import json,sys; print(json.load(sys.stdin)["channels"]["Stable"]["version"])')"
fi
url="https://storage.googleapis.com/chrome-for-testing-public/$CHROMIUM_VERSION/linux64/chrome-headless-shell-linux64.zip"
log "Downloading Chromium $CHROMIUM_VERSION (chrome-headless-shell, about 100 MB)"
curl -fSL --progress-bar -o "$dest/chs.zip" "$url" || fail "download failed: $url"
rm -rf "$dest/chrome-headless-shell-linux64"
(cd "$dest" && unzip -q chs.zip && rm chs.zip) || fail "unzip failed"
echo "$CHROMIUM_VERSION" >"$dest/VERSION"
log "Chromium $CHROMIUM_VERSION ready in $dest (run scripts/build.sh to include it)"
