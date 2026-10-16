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
# 64 MiB, or more when the image carries large files (the Chromium engine).
# Links in the sysroot (BusyBox tools) are not counted: mkpak.py stores them as aliases.
payload_mb=$(( ( $( (du -sbL "$KERNEL_ELF" "$BUILD/userland"; du -sb "$BUILD/sysroot") 2>/dev/null | awk '{s+=$1} END {print s+0}') >> 20 ) ))
image_mb=$(( payload_mb + 64 > 64 ? payload_mb + 64 : 64 ))
truncate -s "${image_mb}M" "$image"   # sparse: uses only the space actually written
sfdisk --quiet "$image" <<'PART' >>"$BUILD_LOG" 2>&1 || fail "sfdisk failed (log: $BUILD_LOG)"
label: gpt
start=2048, type=C12A7328-F81F-11D2-BA4B-00A0C93EC93B, name="NOVA ESP"
PART

# limine.conf: boot/limine.conf's entry, plus an optional cmdline: line under the
# kernel's path: line and the module lines below. NOVA_BOOT_MENU=1 (USB sticks for real
# hardware) shows a menu instead, each entry with the same modules; the default (first)
# keeps the display away from amdgpu until that has been seen to work on the machine.
conf="$(mktemp "$BUILD/limine.conf.XXXXXX")"
mods="$(mktemp "$BUILD/limine.mods.XXXXXX")"
trap 'rm -f "$conf" "$mods"' EXIT
entry() {   # entry <title> <cmdline>
    printf '\n/%s\n    protocol: limine\n    path: boot():/boot/nova-kernel\n' "$1"
    [[ -z "$2" ]] || printf '    cmdline: %s\n' "$2"
    cat "$mods"
}
# Bundled user programs become Limine modules; the kernel installs them into bin/.
programs=()
for p in "$USERLAND_OUT"/*; do
    [[ -f "$p" && -x "$p" ]] && programs+=("$p")
done
for p in "${programs[@]}"; do
    printf '    module_path: boot():/nova/bin/%s\n    module_string: %s\n' "$(basename "$p")" "$(basename "$p")" >>"$mods"
done
# Shared libraries, the dynamic loader and the Chromium engine: one pak module
# (many small modules would exhaust Limine's memory map).
pak=""
if [[ -d "$BUILD/sysroot" ]] && [[ -n "$(ls -A "$BUILD/sysroot")" ]]; then
    pak="$BUILD/nova-system.pak"
    python3 "$ROOT/scripts/mkpak.py" "$BUILD/sysroot" "$pak" >>"$BUILD_LOG" 2>&1 || fail "packing the system files failed"
    printf '    module_path: boot():/nova/system.pak\n    module_string: @pak\n' >>"$mods"
fi
if [[ "${NOVA_BOOT_MENU:-0}" == 1 ]]; then
    {
        printf '# Limine configuration (Limine 8+ syntax), boot menu for real hardware.\ntimeout: 5\n'
        entry "Nova (safe graphics: GPU driver without display takeover)" "${cmdline:+$cmdline }amdgpu.dc=0"
        entry "Nova" "$cmdline"
        entry "Nova (no GPU driver)" "${cmdline:+$cmdline }nova.disable_driver=amdgpu"
        entry "Nova (safe graphics, one CPU)" "${cmdline:+$cmdline }amdgpu.dc=0 nova.cpus=1"
    } >"$conf"
else
    { grep -v '^/\|^[[:space:]]' "$ROOT/boot/limine.conf"; entry "Project Nova" "$cmdline"; } >"$conf"
fi

# The ESP gets a copy without debug info (the full ELF stays in target/ for gdb).
kernel="$(mktemp "$BUILD/nova-kernel.XXXXXX")"
trap 'rm -f "$conf" "$mods" "$kernel"' EXIT
objcopy --strip-debug "$KERNEL_ELF" "$kernel" 2>>"$BUILD_LOG" || cp "$KERNEL_ELF" "$kernel"

ESP="$image@@1M"   # mtools syntax: filesystem at byte offset 1 MiB
{
    mformat -i "$ESP" -F -v NOVA ::
    mmd -i "$ESP" ::/EFI ::/EFI/BOOT ::/boot ::/boot/limine
    mcopy -i "$ESP" "$LIMINE_EFI"  ::/EFI/BOOT/BOOTX64.EFI
    mcopy -i "$ESP" "$conf"        ::/boot/limine/limine.conf
    mcopy -i "$ESP" "$kernel"      ::/boot/nova-kernel
    mmd -i "$ESP" ::/nova ::/nova/bin
    for p in "${programs[@]}"; do mcopy -i "$ESP" "$p" "::/nova/bin/$(basename "$p")"; done
    if [[ -n "$pak" ]]; then mcopy -i "$ESP" "$pak" ::/nova/system.pak; fi
} >>"$BUILD_LOG" 2>&1 || fail "populating ESP failed (log: $BUILD_LOG)"

{
    echo "--- image $image"; mdir -i "$ESP" -/ ::
    echo "--- limine.conf"; cat "$conf"
    echo "--- inputs"
    sha256sum "$KERNEL_ELF" "$LIMINE_EFI"
    command -v pacman >/dev/null && pacman -Q limine edk2-ovmf qemu-system-x86 2>/dev/null || true
} >>"$BUILD_LOG" 2>&1
