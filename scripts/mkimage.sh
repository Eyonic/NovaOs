#!/usr/bin/env bash
# Package the already-built kernel into a GPT disk image with a FAT32 EFI System Partition.
#   scripts/mkimage.sh <image-path> [kernel command line]
# The image must be inside build/. Appends details to build/logs/build.log.
source "$(dirname "$0")/common.sh"
image="${1:?usage: mkimage.sh <image> [cmdline]}"
cmdline="${2:-}"
mkdir -p "$LOGS"
BUILD_LOG="$LOGS/build.log"

[[ -f "$KERNEL_ELF" ]] || fail "kernel ELF not found at $KERNEL_ELF - run scripts/build.sh"
[[ -f "$LIMINE_EFI" ]] || fail "Limine EFI binary not found at $LIMINE_EFI (set LIMINE_EFI=...)"
# Safety: only ever operate on a regular file inside build/, never on a block device.
image="$(realpath -m "$image")"
case "$image" in "$BUILD"/*) ;; *) fail "refusing to write image outside $BUILD: $image" ;; esac
[[ ! -e "$image" || -f "$image" ]] || fail "$image exists and is not a regular file"
[[ "$cmdline" != *$'\n'* ]] || fail "command line must be a single line"

log "Creating disk image $image${cmdline:+ (cmdline: $cmdline)}"
mkdir -p "$(dirname "$image")"
rm -f "$image"
truncate -s 64M "$image"   # sparse: uses only a few MiB on disk
sfdisk --quiet "$image" <<'PART' >>"$BUILD_LOG" 2>&1 || fail "sfdisk failed (log: $BUILD_LOG)"
label: gpt
start=2048, type=C12A7328-F81F-11D2-BA4B-00A0C93EC93B, name="NOVA ESP"
PART

# limine.conf from boot/, plus an optional cmdline: line under the kernel's path: line.
conf="$(mktemp "$BUILD/limine.conf.XXXXXX")"
trap 'rm -f "$conf"' EXIT
if [[ -n "$cmdline" ]]; then
    awk -v c="$cmdline" '{print} /^[[:space:]]+path:/ {match($0,/^[[:space:]]+/); printf "%scmdline: %s\n", substr($0,1,RLENGTH), c}' \
        "$ROOT/boot/limine.conf" >"$conf"
else
    cp "$ROOT/boot/limine.conf" "$conf"
fi

ESP="$image@@1M"   # mtools syntax: filesystem at byte offset 1 MiB
{
    mformat -i "$ESP" -F -v NOVA ::
    mmd -i "$ESP" ::/EFI ::/EFI/BOOT ::/boot ::/boot/limine
    mcopy -i "$ESP" "$LIMINE_EFI"  ::/EFI/BOOT/BOOTX64.EFI
    mcopy -i "$ESP" "$conf"        ::/boot/limine/limine.conf
    mcopy -i "$ESP" "$KERNEL_ELF"  ::/boot/nova-kernel
} >>"$BUILD_LOG" 2>&1 || fail "populating ESP failed (log: $BUILD_LOG)"

{
    echo "--- image $image"; mdir -i "$ESP" -/ ::
    echo "--- limine.conf"; cat "$conf"
    echo "--- inputs"
    sha256sum "$KERNEL_ELF" "$LIMINE_EFI"
    command -v pacman >/dev/null && pacman -Q limine edk2-ovmf qemu-system-x86 2>/dev/null || true
} >>"$BUILD_LOG" 2>&1
