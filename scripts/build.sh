#!/usr/bin/env bash
# Build the kernel and package it into a GPT disk image with a FAT32 EFI System Partition.
#   scripts/build.sh            normal build
#   FEATURES=x scripts/build.sh          build with cargo feature(s) enabled
#   NOVA_CMDLINE="nova.test=ud" scripts/build.sh   bake a kernel command line into the image
source "$(dirname "$0")/common.sh"
mkdir -p "$LOGS"
BUILD_LOG="$LOGS/build.log"
: > "$BUILD_LOG"

log "Building kernel ($PROFILE${FEATURES:+, features: $FEATURES})"
cargo_args=(build --manifest-path "$KERNEL_DIR/Cargo.toml")
[[ "$PROFILE" == release ]] && cargo_args+=(--release)
[[ -n "${FEATURES:-}" ]] && cargo_args+=(--features "$FEATURES")
if ! (cd "$KERNEL_DIR" && cargo "${cargo_args[@]}") 2>&1 | tee -a "$BUILD_LOG"; then
    fail "cargo build failed (log: $BUILD_LOG)"
fi
[[ -f "$KERNEL_ELF" ]] || fail "kernel ELF not found at $KERNEL_ELF"

# User programs: plain C/C++ compiled for Linux x86-64 and linked statically; Nova
# runs them through its Linux system call layer (kernel/src/linux.rs).
log "Building user programs (static C/C++)"
mkdir -p "$USERLAND_OUT"
for src in "$USERLAND_SRC"/*.c "$USERLAND_SRC"/*.cpp; do
    [[ -f "$src" ]] || continue
    name="$(basename "${src%.*}")"
    case "$src" in
        *.c)   cc="${CC:-gcc}";  extra=(-lm) ;;
        *.cpp) cc="${CXX:-g++}"; extra=() ;;
    esac
    if ! "$cc" -static -O2 -Wall -o "$USERLAND_OUT/$name" "$src" "${extra[@]}" >>"$BUILD_LOG" 2>&1; then
        fail "compiling $src failed (log: $BUILD_LOG)"
    fi
    strip "$USERLAND_OUT/$name"
done

# Dynamically linked programs (userland/dynamic/) run with the host's dynamic loader
# and shared libraries, which are collected into a small sysroot shipped alongside.
SYSROOT="$BUILD/sysroot"
rm -rf "$SYSROOT"
mkdir -p "$SYSROOT"
# Home screen artwork (assets/home/, generated images; see assets/home/README.md).
mkdir -p "$SYSROOT/usr/share/nova/home"
cp "$ROOT"/assets/home/*.png "$SYSROOT/usr/share/nova/home/"
mkdir -p "$SYSROOT/usr/share/nova/icons"
cp "$ROOT"/assets/icons/*.png "$SYSROOT/usr/share/nova/icons/"
mkdir -p "$SYSROOT/usr/share/nova/glyphs"
cp "$ROOT"/assets/glyphs/*.png "$SYSROOT/usr/share/nova/glyphs/"
# The desktop's UI font (Noto Sans, SIL Open Font License), rendered by kernel/src/font.rs.
mkdir -p "$SYSROOT/usr/share/fonts/noto"
for f in NotoSans-Regular.ttf NotoSans-Bold.ttf; do
    src="$(fc-match -f '%{file}' "Noto Sans:style=${f#NotoSans-}" 2>/dev/null)"
    [[ -f "/usr/share/fonts/noto/$f" ]] && src="/usr/share/fonts/noto/$f"
    if [[ "$(basename "${src:-}")" == "$f" ]]; then cp -L "$src" "$SYSROOT/usr/share/fonts/noto/$f"; else log "warning: UI font $f not found (install Noto Sans); the desktop falls back to the bitmap font"; fi
done
# Shields filter lists (scripts/fetch-filters.sh), if downloaded.
if compgen -G "$BUILD/filters/*.txt" >/dev/null; then
    mkdir -p "$SYSROOT/usr/share/nova/shields"
    cp "$BUILD"/filters/*.txt "$SYSROOT/usr/share/nova/shields/"
fi
for src in "$USERLAND_SRC"/dynamic/*.c "$USERLAND_SRC"/dynamic/*.cpp; do
    [[ -f "$src" ]] || continue
    name="$(basename "${src%.*}")"
    case "$src" in
        *.c)   cc="${CC:-gcc}";  extra=(-lm) ;;
        *.cpp) cc="${CXX:-g++}"; extra=() ;;
    esac
    "$cc" -O2 -Wall -o "$USERLAND_OUT/$name" "$src" "${extra[@]}" >>"$BUILD_LOG" 2>&1 || fail "compiling $src failed (log: $BUILD_LOG)"
    strip "$USERLAND_OUT/$name"
    interp="$(readelf -l "$USERLAND_OUT/$name" | sed -n 's/.*Requesting program interpreter: \(.*\)]/\1/p')"
    for lib in "$interp" $(ldd "$USERLAND_OUT/$name" | awk '/=> \//{print $3}'); do
        [[ -n "$lib" ]] || continue
        mkdir -p "$SYSROOT$(dirname "$lib")"
        cp -L "$lib" "$SYSROOT$lib"
    done
done
# The Chromium engine (scripts/fetch-chromium.sh), if downloaded: its libraries too.
CHROMIUM="$BUILD/chromium/chrome-headless-shell-linux64"
if [[ -x "$CHROMIUM/chrome-headless-shell" ]]; then
    log "Including Chromium $(cat "$BUILD/chromium/VERSION" 2>/dev/null) and its shared libraries"
    # NSS loads its crypto module with dlopen, so ldd does not list it.
    extra=()
    for l in /usr/lib/libsoftokn3.so /usr/lib/libfreeblpriv3.so /usr/lib/libfreebl3.so; do [[ -f "$l" ]] && extra+=("$l"); done
    for lib in $(ldd "$CHROMIUM/chrome-headless-shell" "${extra[@]}" | awk '/=> \//{print $3}') "${extra[@]}"; do
        mkdir -p "$SYSROOT$(dirname "$lib")"
        cp -L "$lib" "$SYSROOT$lib"
    done
    mkdir -p "$SYSROOT/opt"
    cp -r "$CHROMIUM" "$SYSROOT/opt/chromium"
    # Fonts for web pages (Liberation and Noto, SIL Open Font License) and fontconfig's
    # configuration: Latin, Greek, Cyrillic, Arabic, Hebrew, Indic, Thai, CJK, symbols, emoji.
    for d in /usr/share/fonts/liberation /etc/fonts; do
        [[ -d "$d" ]] && { mkdir -p "$SYSROOT$(dirname "$d")"; cp -rL "$d" "$SYSROOT$d"; }
    done
    for f in noto/NotoSans-Regular.ttf noto/NotoSans-Bold.ttf noto/NotoSansArabic-Regular.ttf noto/NotoSansArmenian-Regular.ttf \
             noto/NotoSansBengali-Regular.ttf noto/NotoSansDevanagari-Regular.ttf noto/NotoSansEthiopic-Regular.ttf \
             noto/NotoSansGeorgian-Regular.ttf noto/NotoSansHebrew-Regular.ttf noto/NotoSansTamil-Regular.ttf \
             noto/NotoSansThai-Regular.ttf noto/NotoSansSymbols-Regular.ttf noto/NotoSansSymbols2-Regular.ttf \
             noto/NotoColorEmoji.ttf noto-cjk/NotoSansCJK-Regular.ttc; do
        [[ -f "/usr/share/fonts/$f" ]] && { mkdir -p "$SYSROOT/usr/share/fonts/$(dirname "$f")"; cp -L "/usr/share/fonts/$f" "$SYSROOT/usr/share/fonts/$f"; }
    done
fi
# The dynamic loader looks in /usr/lib: make sure every library is there.
if [[ -d "$SYSROOT/usr/lib64" ]]; then mkdir -p "$SYSROOT/usr/lib"; cp -n "$SYSROOT"/usr/lib64/* "$SYSROOT/usr/lib/" 2>/dev/null || true; fi

"$ROOT/scripts/mkimage.sh" "$IMAGE" "${NOVA_CMDLINE:-}" || fail "image creation failed (log: $BUILD_LOG)"
log "Image ready: $IMAGE (build log: $BUILD_LOG)"
