# Shared settings for Nova scripts. Sourced, not executed.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BUILD="$ROOT/build"
LOGS="$BUILD/logs"
KERNEL_DIR="$ROOT/kernel"
PROFILE="${PROFILE:-release}"
# Cargo names the dev profile's output directory "debug".
KERNEL_ELF="$KERNEL_DIR/target/x86_64-unknown-none/$([[ $PROFILE == dev ]] && echo debug || echo "$PROFILE")/nova-kernel"
IMAGE="${IMAGE:-$BUILD/nova.img}"
USERLAND_SRC="$ROOT/userland"
USERLAND_OUT="$BUILD/userland"   # statically linked programs, shipped as Limine modules

# Host-provided inputs (override via environment if your distro uses other paths).
LIMINE_EFI="${LIMINE_EFI:-/usr/share/limine/BOOTX64.EFI}"
OVMF_CODE="${OVMF_CODE:-/usr/share/edk2/x64/OVMF_CODE.4m.fd}"
OVMF_VARS_TEMPLATE="${OVMF_VARS_TEMPLATE:-/usr/share/edk2/x64/OVMF_VARS.4m.fd}"
OVMF_VARS="$BUILD/OVMF_VARS.4m.fd"   # private writable copy; the system file is never modified

log()  { printf '\033[1;34m[nova]\033[0m %s\n' "$*"; }
fail() { printf '\033[1;31m[nova] FAIL:\033[0m %s\n' "$*" >&2; exit 1; }

# Accelerate with KVM when the current user can use it; fall back to TCG emulation.
qemu_accel_args() {
    if [[ -r /dev/kvm && -w /dev/kvm && "${NOVA_NO_KVM:-0}" != 1 ]]; then
        echo "-accel kvm -cpu host"
    else
        echo "-accel tcg -cpu max"
    fi
}

# Common QEMU arguments: q35 machine, OVMF pflash, our disk image, no network.
qemu_base_args() {
    [[ -f "$IMAGE" ]] || fail "missing $IMAGE - run scripts/build.sh first"
    [[ -f "$OVMF_VARS" ]] || cp "$OVMF_VARS_TEMPLATE" "$OVMF_VARS"
    echo "-machine q35,vmport=on -m ${NOVA_RAM:-$( [[ -d $BUILD/sysroot/opt/chromium ]] && echo 4G || echo 1G )} $(qemu_accel_args) -nic user,model=${NOVA_NIC:-e1000} -no-reboot" \
         "-drive if=pflash,format=raw,readonly=on,file=$OVMF_CODE" \
         "-drive if=pflash,format=raw,file=$OVMF_VARS" \
         "-drive format=raw,file=$IMAGE"
}
