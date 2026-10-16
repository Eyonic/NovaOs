#!/usr/bin/env bash
# Boot Nova in QEMU, like a real machine with an installation medium and a disk.
#   scripts/run.sh              installation image + build/disk.img (2 GiB, created empty if missing);
#                               once Nova is installed on disk.img, boots that disk alone
#   scripts/run.sh --live       always boot the installation image (disk.img still attached)
#   scripts/run.sh --fresh      erase build/disk.img first (start over)
#   scripts/run.sh --headless   no window; the serial console is in this terminal
#   scripts/run.sh --no-host-login   don't pass your host login to Nova
# Assistant sign-in, passed to Nova at every boot through QEMU's fw_cfg (Nova keeps it in
# memory only; nothing is written to the Nova disk, the repository or the QEMU command line).
# First found wins:
#   DeepSeek:  $DEEPSEEK_API_KEY, or the file ~/.config/nova/deepseek-api-key
#   Anthropic: a fresh short-lived token from the Anthropic CLI (`ant auth login`),
#              $ANTHROPIC_AUTH_TOKEN, $ANTHROPIC_API_KEY
# NOVA_AI=anthropic checks the Anthropic sources first.
# Image generation for the assistant: a ComfyUI server from $NOVA_COMFYUI_URL or the file
# ~/.config/nova/comfyui-url (kept out of the repository), passed in the same way.
# The serial console is always in this terminal (quit QEMU with Ctrl-A then X).
# Only files under build/ are used as disks; nothing touches your real disks.
source "$(dirname "$0")/common.sh"
DISK="$BUILD/disk.img"
display=(-display gtk,grab-on-hover=off) live=0 host_login=1
for arg in "$@"; do
    case "$arg" in
        --headless) display=(-display none) ;;
        --live) live=1 ;;
        --fresh) rm -f "$DISK" ;;
        --no-host-login) host_login=0 ;;
        *) fail "unknown option $arg" ;;
    esac
done
[[ -f "$DISK" ]] || { truncate -s 2G "$DISK"; log "Created an empty 2 GiB disk: $DISK"; }

# Writes the host's assistant credential to $1 (mode 600) in the format Nova's claude.rs reads:
# "deepseek-key", "oauth" or "api-key", the secret, where it came from. Returns 1 if there is none.
DEEPSEEK_KEY_FILE="${XDG_CONFIG_HOME:-$HOME/.config}/nova/deepseek-api-key"
deepseek_credential() {
    local key="${DEEPSEEK_API_KEY:-}" src="DEEPSEEK_API_KEY on the host"
    if [[ -z "$key" && -r "$DEEPSEEK_KEY_FILE" ]]; then
        key=$(head -n1 "$DEEPSEEK_KEY_FILE" | tr -d '[:space:]') src="~/.config/nova/deepseek-api-key"
    fi
    [[ -n "$key" ]] || return 1
    printf 'deepseek-key\n%s\n%s\n' "$key" "$src" >"$1"
}
host_credential() {
    if [[ "${NOVA_AI:-deepseek}" != anthropic ]] && deepseek_credential "$1"; then return 0; fi
    anthropic_credential "$1" && return 0
    deepseek_credential "$1"
}
anthropic_credential() {
    local token
    if command -v ant >/dev/null && token=$(ant auth print-credentials --access-token 2>/dev/null) && [[ -n "$token" ]]; then
        printf 'oauth\n%s\nant auth login on the host\n' "$token" >"$1"
    elif [[ -n "${ANTHROPIC_AUTH_TOKEN:-}" ]]; then
        printf 'oauth\n%s\nANTHROPIC_AUTH_TOKEN on the host\n' "$ANTHROPIC_AUTH_TOKEN" >"$1"
    elif [[ -n "${ANTHROPIC_API_KEY:-}" ]]; then
        printf 'api-key\n%s\nANTHROPIC_API_KEY on the host\n' "$ANTHROPIC_API_KEY" >"$1"
    else
        return 1
    fi
}
COMFYUI_URL="${NOVA_COMFYUI_URL:-}"
[[ -z "$COMFYUI_URL" && -r "${XDG_CONFIG_HOME:-$HOME/.config}/nova/comfyui-url" ]] && COMFYUI_URL=$(head -n1 "${XDG_CONFIG_HOME:-$HOME/.config}/nova/comfyui-url" | tr -d '[:space:]')
cred_file="" comfy_file=""
trap 'rm -f ${cred_file:+"$cred_file"} ${comfy_file:+"$comfy_file"}' EXIT
fw_cfg=()
refresh_login() {
    fw_cfg=()
    (( host_login )) || return 0
    [[ -n "$cred_file" ]] || cred_file=$(umask 077; mktemp "$BUILD/.claude-credential.XXXXXX")
    if host_credential "$cred_file"; then
        log "Assistant: passing your host login to Nova ($(sed -n 3p "$cred_file"))"
        fw_cfg=(-fw_cfg "name=opt/nova/claude-credential,file=$cred_file")
    else
        log "Assistant: no host login found (set DEEPSEEK_API_KEY or run \`ant auth login\` to skip typing a key)"
    fi
    if [[ -n "$COMFYUI_URL" ]]; then
        [[ -n "$comfy_file" ]] || comfy_file=$(umask 077; mktemp "$BUILD/.comfyui-url.XXXXXX")
        printf '%s\n' "$COMFYUI_URL" >"$comfy_file"
        fw_cfg+=(-fw_cfg "name=opt/nova/comfyui-url,file=$comfy_file")
        log "Assistant: image generation via your ComfyUI server"
    fi
}

installed() { sfdisk -d "$DISK" 2>/dev/null | grep -qi '4E6F7661-4653-4E44-8001-4E4F56414653'; }

while true; do
    if (( ! live )) && installed; then
        log "Booting the installed system on $DISK"
        drives=()   # qemu_base_args attaches $image, which is the disk itself
        image="$DISK"
    else
        log "Booting the installation image $IMAGE (target disk: $DISK)"
        drives=(-drive "format=raw,file=$DISK")
        image="$IMAGE"
    fi
    refresh_login   # a fresh token for every boot (they are short-lived)
    disk_before=$(sfdisk -d "$DISK" 2>/dev/null | md5sum)
    # shellcheck disable=SC2046
    IMAGE="$image" qemu-system-x86_64 $(IMAGE="$image" qemu_base_args) "${drives[@]}" "${fw_cfg[@]}" "${display[@]}" -serial mon:stdio
    # QEMU runs with -no-reboot: a reboot request ends it. After installing, the
    # system reboots, and this loop starts the installed disk (also after --live: the
    # user just chose to install).
    if installed && [[ "$image" == "$IMAGE" ]] && { (( ! live )) || [[ "$(sfdisk -d "$DISK" 2>/dev/null | md5sum)" != "$disk_before" ]]; }; then
        log "Installation finished; restarting from the installed disk"
        live=0
        continue
    fi
    break
done
