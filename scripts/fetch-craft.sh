#!/usr/bin/env bash
# Download the Craft App Suite (https://github.com/storytold, MIT OR Apache-2.0): the
# Linux x86_64 release tarball of each app, checked against the release's SHA-256
# (SHA256SUMS.txt, else the digest GitHub lists for the asset), unpacked in the cache.
# scripts/build.sh runs this for NOVA_CRAFT=... and installs the apps under /opt/<app>.
#   scripts/fetch-craft.sh wordcraft gridcraft     these apps (latest release)
#   scripts/fetch-craft.sh                         the default set (below)
#   NOVA_CRAFT_CACHE=/somewhere scripts/fetch-craft.sh
# The downloads (20-120 MB each) stay out of the repository and build/: they go to
# $NOVA_CRAFT_CACHE (default /tmp/nova-craft-cache). Each app's unpacked release ends up
# in $NOVA_CRAFT_CACHE/<app>/ (bin/, share/), with its version in <app>/VERSION. Without a
# network connection, an app already in the cache is used as it is.
source "$(dirname "$0")/common.sh"
CACHE="${NOVA_CRAFT_CACHE:-/tmp/nova-craft-cache}"
CRAFT_DEFAULT=(wordcraft gridcraft deckcraft pdfcraft photocraft)
CRAFT_ALL=(wordcraft gridcraft deckcraft pdfcraft photocraft lightcraft filmcraft soundcraft vectorcraft designcraft figcraft cadcraft effectcraft craft-launcher)
apps=("$@")
[[ ${#apps[@]} -gt 0 ]] || apps=("${CRAFT_DEFAULT[@]}")
mkdir -p "$CACHE"

for app in "${apps[@]}"; do
    [[ " ${CRAFT_ALL[*]} " == *" $app "* ]] || fail "unknown Craft app '$app' (known: ${CRAFT_ALL[*]})"
    json="$CACHE/$app.release.json"
    if ! curl -fsSL -o "$json.new" "https://api.github.com/repos/storytold/$app/releases/latest"; then
        rm -f "$json.new"
        if [[ -f "$CACHE/$app/VERSION" ]]; then log "$app: offline, using the cached $(cat "$CACHE/$app/VERSION")"; continue; fi
        fail "$app: cannot reach the GitHub API and nothing is cached"
    fi
    mv "$json.new" "$json"
    # The Linux x86_64 tarball (craft-launcher's is named artcraft-launcher-...), its URL,
    # the release's checksum list and GitHub's digest of the tarball.
    read -r tag name url sums digest < <(python3 -I - "$json" <<'PY'
import json, re, sys
d = json.load(open(sys.argv[1]))
tar = [a for a in d["assets"] if re.fullmatch(r"[a-z-]+-[0-9][^-]*-linux-x86_64\.tar\.gz", a["name"]) and "-cli-" not in a["name"] and "-server-" not in a["name"]]
sums = [a["browser_download_url"] for a in d["assets"] if a["name"] == "SHA256SUMS.txt"]
if not tar:
    sys.exit("no linux-x86_64 tarball in " + d.get("tag_name", "?"))
a = tar[0]
print(d["tag_name"], a["name"], a["browser_download_url"], sums[0] if sums else "-", (a.get("digest") or "-").removeprefix("sha256:"))
PY
) || fail "$app: no Linux x86_64 tarball in its latest release"
    if [[ "$(cat "$CACHE/$app/VERSION" 2>/dev/null)" == "$tag" && -d "$CACHE/$app/bin" ]]; then
        log "$app $tag: cached"
        continue
    fi
    want=""
    if [[ "$sums" != - ]]; then
        want="$(curl -fsSL "$sums" | awk -v n="$name" '$2 == n || $2 == "*" n {print $1}')"
    fi
    [[ -n "$want" ]] || want="$digest"
    [[ "$want" =~ ^[0-9a-f]{64}$ ]] || fail "$app: the release publishes no SHA-256 for $name"
    if [[ ! -f "$CACHE/$name" ]] || ! echo "$want  $CACHE/$name" | sha256sum -c --status; then
        log "Downloading $name"
        curl -fSL --progress-bar -o "$CACHE/$name.part" "$url" || fail "download failed: $url"
        mv "$CACHE/$name.part" "$CACHE/$name"
    fi
    echo "$want  $CACHE/$name" | sha256sum -c --status || fail "$app: SHA-256 mismatch for $name"
    rm -rf "$CACHE/$app.new"
    mkdir -p "$CACHE/$app.new"
    tar -xzf "$CACHE/$name" -C "$CACHE/$app.new" --no-same-owner || fail "$app: unpacking $name failed"
    # The tarball holds one top-level directory (<app>-<version>-linux-x86_64/).
    top="$(find "$CACHE/$app.new" -mindepth 1 -maxdepth 1 -type d | head -1)"
    [[ -d "$top/bin" ]] || fail "$app: $name has no bin/ directory"
    rm -rf "$CACHE/$app"
    mv "$top" "$CACHE/$app"
    rm -rf "$CACHE/$app.new"
    echo "$tag" >"$CACHE/$app/VERSION"
    log "$app $tag ready in $CACHE/$app (sha256 $want)"
done
