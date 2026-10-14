#!/usr/bin/env bash
# Download and build BusyBox (sh, coreutils, tar, xz, sed, grep, awk and ~300 more Unix
# tools in one static binary) for Nova's Linux userland. scripts/build.sh then ships it as
# bin/busybox with one link per tool (bin/sh, bin/ls, ...). BusyBox is GPL-2.0 and is not
# kept in this repository; the source is downloaded from busybox.net and checked against
# the pinned sha256.
source "$(dirname "$0")/common.sh"

VERSION=1.38.0
SHA256=34f9ea6ff8636f2c9241153b9114eefa9e65674a45318ae1ef95bb5f31c53bb2
url="https://busybox.net/downloads/busybox-$VERSION.tar.bz2"
dl="$BUILD/busybox-dl"
src="$dl/busybox-$VERSION"
dest="$BUILD/busybox"
mkdir -p "$dl" "$dest"

if [[ -x "$dest/busybox" && "$(cat "$dest/VERSION" 2>/dev/null)" == "$VERSION" ]]; then
    log "BusyBox $VERSION already built in $dest"
    exit 0
fi
if ! echo "$SHA256  $dl/busybox-$VERSION.tar.bz2" | sha256sum -c --status 2>/dev/null; then
    log "Downloading BusyBox $VERSION"
    curl -fSL --progress-bar -o "$dl/busybox-$VERSION.tar.bz2" "$url" || fail "download failed: $url"
    echo "$SHA256  $dl/busybox-$VERSION.tar.bz2" | sha256sum -c --status || fail "checksum mismatch for busybox-$VERSION.tar.bz2"
fi
rm -rf "$src"
tar -xjf "$dl/busybox-$VERSION.tar.bz2" -C "$dl" || fail "unpacking BusyBox failed"

log "Building BusyBox $VERSION (static)"
LOG_BB="$LOGS/busybox.log"
mkdir -p "$LOGS"
(
    cd "$src"
    make defconfig
    # Static, so it needs no shared libraries; "tc" does not build against current
    # kernel headers.
    sed -i 's/^# CONFIG_STATIC is not set/CONFIG_STATIC=y/; s/^CONFIG_TC=y/# CONFIG_TC is not set/' .config
    make oldconfig </dev/null
    make -j"$(nproc)"
) >"$LOG_BB" 2>&1 || fail "building BusyBox failed (log: $LOG_BB)"
cp "$src/busybox" "$dest/busybox"
strip "$dest/busybox"
"$src/busybox" --list > "$dest/applets"
echo "$VERSION" > "$dest/VERSION"
log "BusyBox $VERSION ready: $(wc -l < "$dest/applets") tools (run scripts/build.sh to include it)"
