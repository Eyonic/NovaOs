#!/usr/bin/env bash
# Build the kernel and package it into a GPT disk image with a FAT32 EFI System Partition.
#   scripts/build.sh            normal build
#   FEATURES=panic-test scripts/build.sh   build with a cargo feature enabled
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
[[ -f "$LIMINE_EFI" ]] || fail "Limine EFI binary not found at $LIMINE_EFI (set LIMINE_EFI=...)"

# --- Disk image: 64 MiB file, GPT, one ESP partition starting at 1 MiB. -------------
# Safety: only ever operate on a regular file inside build/, never on a block device.
log "Creating disk image $IMAGE"
case "$IMAGE" in "$BUILD"/*) ;; *) fail "refusing to write image outside $BUILD" ;; esac
rm -f "$IMAGE"
truncate -s 64M "$IMAGE"
[[ -f "$IMAGE" && ! -b "$IMAGE" ]] || fail "image is not a regular file"
sfdisk --quiet "$IMAGE" <<'PART' >>"$BUILD_LOG" 2>&1 || fail "sfdisk failed (log: $BUILD_LOG)"
label: gpt
start=2048, type=C12A7328-F81F-11D2-BA4B-00A0C93EC93B, name="NOVA ESP"
PART

ESP="$IMAGE@@1M"   # mtools syntax: filesystem at byte offset 1 MiB
{
    mformat -i "$ESP" -F -v NOVA ::
    mmd -i "$ESP" ::/EFI ::/EFI/BOOT ::/boot ::/boot/limine
    mcopy -i "$ESP" "$LIMINE_EFI"             ::/EFI/BOOT/BOOTX64.EFI
    mcopy -i "$ESP" "$ROOT/boot/limine.conf"  ::/boot/limine/limine.conf
    mcopy -i "$ESP" "$KERNEL_ELF"             ::/boot/nova-kernel
} >>"$BUILD_LOG" 2>&1 || fail "populating ESP failed (log: $BUILD_LOG)"

{
    echo "--- image contents"; mdir -i "$ESP" -/ ::
    echo "--- inputs"
    sha256sum "$KERNEL_ELF" "$LIMINE_EFI"
    command -v pacman >/dev/null && pacman -Q limine edk2-ovmf qemu-system-x86 2>/dev/null || true
} >>"$BUILD_LOG" 2>&1

log "Image ready: $IMAGE (build log: $BUILD_LOG)"
