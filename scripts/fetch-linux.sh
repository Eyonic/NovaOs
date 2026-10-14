#!/usr/bin/env bash
# Download the Linux kernel source that Nova's Linux driver layer (kernel/linuxkpi/) compiles
# drivers from, and unpack it into build/linux/. The source is GPL-2.0 and is not kept in this
# repository; it is only compiled into the kernel with `FEATURES=linux-drivers scripts/build.sh`,
# which makes that kernel binary GPL-2.0 (see docs/linux-drivers.md).
#   scripts/fetch-linux.sh                 the pinned version
#   LINUX_VERSION=7.2.10 scripts/fetch-linux.sh   another release (the layer may need updating)
source "$(dirname "$0")/common.sh"

LINUX_VERSION="${LINUX_VERSION:-7.2.9}"
major="${LINUX_VERSION%%.*}"
base="https://cdn.kernel.org/pub/linux/kernel/v$major.x"
dl="$BUILD/linux-dl"
dest="$BUILD/linux"
tarball="linux-$LINUX_VERSION.tar.xz"
mkdir -p "$dl"

if [[ -f "$dest/VERSION" && "$(cat "$dest/VERSION")" == "$LINUX_VERSION" ]]; then
    log "Linux $LINUX_VERSION already in $dest"
    exit 0
fi

log "Downloading Linux $LINUX_VERSION source (about 150 MB)"
curl -fsSL -o "$dl/sha256sums.asc" "$base/sha256sums.asc" || fail "download failed: $base/sha256sums.asc"
expected="$(grep " $tarball\$" "$dl/sha256sums.asc")" || fail "$tarball is not listed in $base/sha256sums.asc"
if ! (cd "$dl" && sha256sum -c --status - <<<"$expected") 2>/dev/null; then
    curl -fSL --progress-bar -o "$dl/$tarball" "$base/$tarball" || fail "download failed: $base/$tarball"
    (cd "$dl" && sha256sum -c --status - <<<"$expected") || fail "checksum mismatch for $tarball"
fi
log "Checksum matches kernel.org's sha256sums.asc"

rm -rf "$dest"
mkdir -p "$dest"
tar -xJf "$dl/$tarball" -C "$dest" --strip-components=1 || fail "unpacking $tarball failed"
echo "$LINUX_VERSION" > "$dest/VERSION"
log "Linux $LINUX_VERSION ready in $dest (build with FEATURES=linux-drivers scripts/build.sh)"
