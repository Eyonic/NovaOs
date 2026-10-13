#!/usr/bin/env bash
# Remove generated artifacts: build/ (image, logs, OVMF vars copy) and kernel/target/.
source "$(dirname "$0")/common.sh"
log "Removing $BUILD and $KERNEL_DIR/target"
rm -rf -- "$BUILD"
(cd "$KERNEL_DIR" && cargo clean)
