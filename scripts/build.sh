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

"$ROOT/scripts/mkimage.sh" "$IMAGE" "${NOVA_CMDLINE:-}" || fail "image creation failed (log: $BUILD_LOG)"
log "Image ready: $IMAGE (build log: $BUILD_LOG)"
