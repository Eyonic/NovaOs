#!/usr/bin/env bash
# Automated boot test: build the image, boot it headless in QEMU, capture COM1 to a
# log file, and check for the expected lines. QEMU never exits on its own (the kernel
# idles forever), so this script terminates it once the verdict is known.
#
#   scripts/test.sh                       build + boot + verify the default banner
#   scripts/test.sh --expect "TEXT"       verify a different banner (exact line match)
#   scripts/test.sh --no-build            reuse the existing image
#   scripts/test.sh --panic               build with panic-test; expect the panic handler output
#   TIMEOUT=60 scripts/test.sh            change the boot timeout (seconds, default 30)
#
# Exit status: 0 = all checks passed, 1 = build or boot verification failed.
source "$(dirname "$0")/common.sh"

EXPECTED_BANNER="PROJECT NOVA KERNEL 0.1 - IDE TEST PASSED"
EXPECTED_STATUS="[nova] status: kernel main path reached, init OK"
PANIC_MARKER="!!! KERNEL PANIC !!!"
TIMEOUT="${TIMEOUT:-30}"
do_build=1 mode=normal

while (( $# )); do
    case "$1" in
        --expect)   EXPECTED_BANNER="${2:?--expect needs a value}"; shift ;;
        --no-build) do_build=0 ;;
        --panic)    mode=panic ;;
        *) fail "unknown argument: $1" ;;
    esac
    shift
done

if [[ $mode == panic ]]; then
    export IMAGE="$BUILD/nova-panic.img" FEATURES=panic-test
    expected=("$PANIC_MARKER" "panicked at")
else
    expected=("$EXPECTED_BANNER" "$EXPECTED_STATUS")
fi

if (( do_build )); then
    "$ROOT/scripts/build.sh" || fail "build failed - not attempting to boot"
fi

run_dir="$LOGS/test-$(date +%Y%m%d-%H%M%S)-$mode"
mkdir -p "$run_dir"
ln -sfn "$(basename "$run_dir")" "$LOGS/test-latest"
serial_log="$run_dir/serial.log"
qemu_log="$run_dir/qemu.log"

# shellcheck disable=SC2046
qemu-system-x86_64 $(qemu_base_args) -display none -monitor none \
    -serial "file:$serial_log" >"$qemu_log" 2>&1 &
qemu_pid=$!

stop_qemu() {
    kill -0 "$qemu_pid" 2>/dev/null || return 0
    kill -TERM "$qemu_pid" 2>/dev/null || true
    for _ in $(seq 50); do kill -0 "$qemu_pid" 2>/dev/null || break; sleep 0.1; done
    kill -KILL "$qemu_pid" 2>/dev/null || true
    wait "$qemu_pid" 2>/dev/null || true
}
trap stop_qemu EXIT INT TERM

log "QEMU started (pid $qemu_pid), waiting up to ${TIMEOUT}s for serial output"
log "Expecting lines: $(printf '"%s" ' "${expected[@]}")"

# Serial output has CRLF line endings; compare whole lines after dropping CR.
has_line()    { tr -d '\r' <"$serial_log" | grep -qFx -- "$1"; }
has_substr()  { tr -d '\r' <"$serial_log" | grep -qF  -- "$1"; }

verdict=timeout
deadline=$(( SECONDS + TIMEOUT ))
while (( SECONDS < deadline )); do
    if ! kill -0 "$qemu_pid" 2>/dev/null; then verdict=qemu-exited; break; fi
    if [[ -f "$serial_log" ]]; then
        if [[ $mode == normal ]] && has_substr "$PANIC_MARKER"; then verdict=panic; break; fi
        ok=1
        if [[ $mode == normal ]]; then
            has_line "${expected[0]}" || ok=0
            has_line "${expected[1]}" || ok=0
        else
            has_line "${expected[0]}" || ok=0
            has_substr "${expected[1]}" || ok=0
        fi
        (( ok )) && { verdict=pass; break; }
    fi
    sleep 0.2
done

stop_qemu
kill -0 "$qemu_pid" 2>/dev/null && fail "QEMU (pid $qemu_pid) is still running after termination attempt"
log "QEMU terminated. Logs: $run_dir"

echo "----- captured serial output (Nova lines) -----"
tr -d '\r' <"$serial_log" 2>/dev/null | sed 's/\x1b\[[0-9;=]*[A-Za-z]//g' | grep -v '^$' || true
echo "-----------------------------------------------"

case $verdict in
    pass)        log "PASS: VM booted and all expected lines were found ($mode mode)"; exit 0 ;;
    timeout)     has_line "${expected[0]}" || echo "  missing line: \"${expected[0]}\"" >&2
                 [[ $mode == panic ]] && { has_substr "${expected[1]}" || echo "  missing text: \"${expected[1]}\"" >&2; } \
                                      || { has_line   "${expected[1]}" || echo "  missing line: \"${expected[1]}\"" >&2; }
                 fail "expected output not seen within ${TIMEOUT}s" ;;
    panic)       fail "kernel panicked during boot" ;;
    qemu-exited) cat "$qemu_log" >&2; fail "QEMU exited before the expected output appeared" ;;
esac
