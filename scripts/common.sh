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
OVMF_VARS="${NOVA_OVMF_VARS:-$BUILD/OVMF_VARS.4m.fd}"   # private writable copy; the system file is never modified

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

# The boot disk: on the q35 SATA (AHCI) controller, or as an NVMe drive with
# NOVA_BOOT_DEVICE=nvme (as on PCs that boot from an NVMe SSD).
boot_drive_args() {
    if [[ "${NOVA_BOOT_DEVICE:-sata}" == nvme ]]; then
        echo "-drive if=none,id=bootdisk,format=raw,file=$IMAGE -device nvme,serial=nova-boot,drive=bootdisk"
    else
        echo "-drive format=raw,file=$IMAGE"
    fi
}

# Common QEMU arguments: q35 machine, OVMF pflash, our disk image, user-mode network.
# The NIC gets no option ROM (romfile=): Nova never boots from the network, and the
# firmware would otherwise sometimes try PXE first (boot entries persist in OVMF_VARS),
# which can stall a boot for longer than a test case's timeout.
qemu_base_args() {
    [[ -f "$IMAGE" ]] || fail "missing $IMAGE - run scripts/build.sh first"
    [[ -f "$OVMF_VARS" ]] || cp "$OVMF_VARS_TEMPLATE" "$OVMF_VARS"
    echo "-machine q35,vmport=${NOVA_VMPORT:-on}${NOVA_MACHINE_OPTS:+,$NOVA_MACHINE_OPTS} -m ${NOVA_RAM:-$( [[ -d $BUILD/sysroot/opt/chromium ]] && echo 4G || echo 1G )} $(qemu_accel_args) -netdev user,id=net0 -device ${NOVA_NIC:-e1000},netdev=net0,romfile= -no-reboot" \
         "-drive if=pflash,format=raw,readonly=on,file=$OVMF_CODE" \
         "-drive if=pflash,format=raw,file=$OVMF_VARS" \
         "$(boot_drive_args)"
}
