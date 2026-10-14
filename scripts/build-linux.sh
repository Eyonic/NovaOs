#!/usr/bin/env bash
# Build the Linux side of Nova's Linux driver layer: configure the Linux tree from
# scripts/fetch-linux.sh with kernel/linuxkpi/nova.config, then let Kbuild compile the
# objects in kernel/linuxkpi/linux-objects.txt and lx_emul (kernel/linuxkpi/lx_emul,
# copied into the tree as nova/). The list of objects goes to build/linux/nova-objects.list,
# which kernel/build.rs links into the kernel. scripts/build.sh runs this when FEATURES
# contains linux-drivers.
source "$(dirname "$0")/common.sh"

LINUX="$BUILD/linux"
KPI="$KERNEL_DIR/linuxkpi"
[[ -f "$LINUX/Makefile" ]] || fail "no Linux source in $LINUX - run scripts/fetch-linux.sh"
mkdir -p "$LOGS"
LOG="$LOGS/build-linux.log"
: > "$LOG"
kmake() { make -C "$LINUX" ARCH=x86_64 -j"$(nproc)" "$@" >>"$LOG" 2>&1; }

# Reconfigure only when the fragment changed (a full reconfigure rebuilds everything).
stamp="$LINUX/.nova-config-stamp"
if [[ ! -f "$stamp" || "$KPI/nova.config" -nt "$stamp" || ! -f "$LINUX/.config" ]]; then
    log "Configuring Linux $(cat "$LINUX/VERSION" 2>/dev/null) for Nova (tinyconfig + kernel/linuxkpi/nova.config)"
    kmake tinyconfig || fail "make tinyconfig failed (log: $LOG)"
    (cd "$LINUX" && scripts/kconfig/merge_config.sh -m .config "$KPI/nova.config") >>"$LOG" 2>&1 || fail "merging nova.config failed (log: $LOG)"
    kmake olddefconfig || fail "make olddefconfig failed (log: $LOG)"
    # Every setting of the fragment must have survived Kconfig's dependency resolution
    # (an option that is off may also be absent, when its dependencies are off).
    while IFS= read -r line; do
        if [[ "$line" =~ ^(CONFIG_[A-Z0-9_]+)=(.*)$ ]]; then
            grep -qxF "$line" "$LINUX/.config" || fail "Kconfig did not accept '$line' from nova.config (check its dependencies)"
        elif [[ "$line" =~ ^#\ (CONFIG_[A-Z0-9_]+)\ is\ not\ set$ ]]; then
            ! grep -q "^${BASH_REMATCH[1]}=" "$LINUX/.config" || fail "Kconfig turned on ${BASH_REMATCH[1]}, which nova.config disables"
        fi
    done < "$KPI/nova.config"
    touch "$stamp"
fi
kmake prepare || fail "make prepare failed (log: $LOG)"

# lx_emul is built as the tree's nova/ directory; a copy (timestamps kept, so make only
# rebuilds what changed) keeps Kbuild's output out of the source tree.
[[ -L "$LINUX/nova" ]] && rm "$LINUX/nova"
mkdir -p "$LINUX/nova"
cp -p "$KPI"/lx_emul/{*.c,*.h,*.S,Makefile} "$LINUX/nova/"
objects=()
while IFS= read -r line; do
    line="${line%%#*}"; line="${line//[[:space:]]/}"
    [[ -n "$line" ]] && objects+=("$line")
done < "$KPI/linux-objects.txt"
for o in $(sed -n 's/^obj-y += //p' "$KPI/lx_emul/Makefile"); do objects+=("nova/$o"); done

log "Building ${#objects[@]} Linux objects with Kbuild"
kmake "${objects[@]}" || fail "Linux objects failed to build (log: $LOG)"
printf '%s\n' "${objects[@]/#/$LINUX/}" > "$LINUX/nova-objects.list"
log "Linux objects ready ($LINUX/nova-objects.list)"
