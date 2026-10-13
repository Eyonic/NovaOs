#!/usr/bin/env bash
# Boot Nova in QEMU interactively.
#   scripts/run.sh             VM window + serial console in this terminal
#   scripts/run.sh --headless  serial console only (quit with Ctrl-A then X)
source "$(dirname "$0")/common.sh"
display=(-display gtk)
[[ "${1:-}" == --headless ]] && display=(-display none)
log "Booting $IMAGE (serial on this terminal; Ctrl-A X to quit QEMU)"
# shellcheck disable=SC2046
exec qemu-system-x86_64 $(qemu_base_args) "${display[@]}" -serial mon:stdio
