#!/usr/bin/env bash
# Verify that every tool and file the build needs is present. Changes nothing.
source "$(dirname "$0")/common.sh"

missing=0
check() { # check <description> <command...>
    local what="$1"; shift
    if out="$("$@" 2>&1)"; then
        printf '  \033[32mok\033[0m   %-28s %s\n' "$what" "$(head -1 <<<"$out")"
    else
        printf '  \033[31mMISS\033[0m %-28s %s\n' "$what" "$(head -1 <<<"$out")"; missing=1
    fi
}

log "Host: $(uname -srm)"
check "rustup"                    rustup --version
check "cargo (pinned toolchain)"  bash -c "cd '$KERNEL_DIR' && cargo --version"
check "target x86_64-unknown-none" bash -c "cd '$KERNEL_DIR' && rustup target list --installed | grep -x x86_64-unknown-none"
check "qemu-system-x86_64"        qemu-system-x86_64 --version
check "OVMF code"                 ls "$OVMF_CODE"
check "OVMF vars template"        ls "$OVMF_VARS_TEMPLATE"
check "Limine BOOTX64.EFI"        ls "$LIMINE_EFI"
check "mtools (mformat/mcopy)"    bash -c "command -v mformat && command -v mcopy && command -v mmd"
check "sfdisk"                    bash -c "command -v sfdisk"
check "KVM (optional)"            bash -c "[[ -r /dev/kvm && -w /dev/kvm ]] && echo /dev/kvm usable || { echo 'not usable, TCG fallback'; true; }"

if (( missing )); then
    log "Missing prerequisites. On Arch/CachyOS:"
    echo "    sudo pacman -S --needed rustup qemu-system-x86 edk2-ovmf mtools util-linux limine"
    echo "    rustup default stable   # kernel/rust-toolchain.toml then pins the exact version"
    exit 1
fi
log "All prerequisites present."
