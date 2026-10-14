#!/usr/bin/env bash
# Download the ad and tracker filter lists Shields uses (EasyList, EasyPrivacy) into
# build/filters/. scripts/build.sh then ships them in the image under
# usr/share/nova/shields/. They are not kept in this repository: both lists are
# dual-licensed GPLv3+ / CC BY-SA 3.0+ by The EasyList authors (https://easylist.to).
source "$(dirname "$0")/common.sh"
dest="$BUILD/filters"
mkdir -p "$dest"
for name in easylist easyprivacy; do
    log "Downloading $name"
    curl -fSL --progress-bar -o "$dest/$name.txt.part" "https://easylist.to/easylist/$name.txt" || fail "download failed: $name"
    mv "$dest/$name.txt.part" "$dest/$name.txt"
done
log "Filter lists ready in $dest ($(cat "$dest"/*.txt | wc -l) lines; run scripts/build.sh to include them)"
