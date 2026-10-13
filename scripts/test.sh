#!/usr/bin/env bash
# Automated QEMU tests. Each case boots the same kernel binary headless, with a
# case-specific kernel command line, captures COM1, and checks the output.
# The kernel never exits (it idles or halts), so this script stops QEMU itself.
#
#   scripts/test.sh                   boot regression test (default case "boot")
#   scripts/test.sh --case gp         run one case (see --list)
#   scripts/test.sh --all             run every case, print a summary
#   scripts/test.sh --list            list cases
#   scripts/test.sh --expect "TEXT"   boot case: expect a different banner (exact line)
#   scripts/test.sh --panic           same as --case panic
#   scripts/test.sh --no-build        reuse the existing kernel build
#   TIMEOUT=60 scripts/test.sh        boot timeout per case in seconds (default 30)
#
# Exit status: 0 = all selected cases passed, 1 = a build or VM check failed.
source "$(dirname "$0")/common.sh"

EXPECTED_BANNER="PROJECT NOVA KERNEL 0.2"
STATUS="[nova] status: kernel main path reached, init OK"
IDLE="[nova] entering idle loop"
EXC="!!! CPU EXCEPTION !!!"
PANIC="!!! KERNEL PANIC !!!"
HALTED="[nova] system halted: unrecoverable exception"
NO_ERR="error code: none (not pushed for this vector)"
TIMEOUT="${TIMEOUT:-30}"
SETTLE=2   # seconds the VM must stay up and silent after the verdict (no reset, no extra output)

ALL_CASES=(boot panic de ud gp pf bp df nested)

# Per-case spec: EXPECT (exact lines), CONTAINS (substrings), FORBID (substrings),
# CHECK_RIP (1 = report RIP must equal the address the kernel announced).
define_case() {
    EXPECT=() CONTAINS=() FORBID=("$PANIC" "$EXC" "NESTED") CHECK_RIP=0 CMDLINE="nova.test=$1"
    local fault=("$EXC" "$STATUS" "$HALTED")
    local fault_forbid=("$PANIC" "$IDLE" "NESTED")
    case "$1" in
        boot)   CMDLINE=""; EXPECT=("$EXPECTED_BANNER" "$STATUS" "$IDLE") ;;
        panic)  EXPECT=("$STATUS" "$PANIC" "[nova] system halted: kernel panic")
                CONTAINS=("panicked at" "intentional panic requested by nova.test=panic")
                FORBID=("$EXC" "NESTED" "$IDLE") ;;
        de)     EXPECT=("${fault[@]}" "exception: #DE Divide Error (vector 0, fault)" "$NO_ERR")
                FORBID=("${fault_forbid[@]}"); CHECK_RIP=1 ;;
        ud)     EXPECT=("${fault[@]}" "exception: #UD Invalid Opcode (vector 6, fault)" "$NO_ERR")
                FORBID=("${fault_forbid[@]}"); CHECK_RIP=1 ;;
        gp)     EXPECT=("${fault[@]}" "exception: #GP General Protection (vector 13, fault)"
                        "error code: 0x0000000000000028 (selector index 5, GDT)")
                FORBID=("${fault_forbid[@]}"); CHECK_RIP=1 ;;
        pf)     EXPECT=("${fault[@]}" "exception: #PF Page Fault (vector 14, fault)"
                        "error code: 0x0000000000000002 (not-present, write, supervisor)"
                        "fault address (CR2): 0x00000deadbeef000")
                FORBID=("${fault_forbid[@]}"); CHECK_RIP=1 ;;
        bp)     EXPECT=("${fault[@]}" "exception: #BP Breakpoint (vector 3, trap)" "$NO_ERR")
                FORBID=("${fault_forbid[@]}"); CHECK_RIP=1 ;;
        df)     EXPECT=("${fault[@]}" "exception: #DF Double Fault (vector 8, abort)"
                        "error code: 0x0000000000000000 (always zero for #DF)"
                        "note: saved RIP is architecturally undefined for #DF")
                CONTAINS=("RSP: 0x00000deadbeef000")
                FORBID=("${fault_forbid[@]}" "exception: #PF") ;;
        nested) EXPECT=("$EXC" "$STATUS" "$HALTED" "exception: #BP Breakpoint (vector 3, trap)"
                        "!!! NESTED EXCEPTION: #UD Invalid Opcode (vector 6) while reporting an exception")
                FORBID=("$PANIC" "$IDLE" "$NO_ERR" "RIP:") ;;   # first report must stop at the nested fault
        *) return 1 ;;
    esac
}

cases=() do_build=1 expect_override=""
while (( $# )); do
    case "$1" in
        --case)     cases+=("${2:?--case needs a name}"); shift ;;
        --all)      cases=("${ALL_CASES[@]}") ;;
        --list)     printf '%s\n' "${ALL_CASES[@]}"; exit 0 ;;
        --expect)   expect_override="${2:?--expect needs a value}"; shift ;;
        --no-build) do_build=0 ;;
        --panic)    cases+=(panic) ;;
        *) fail "unknown argument: $1" ;;
    esac
    shift
done
(( ${#cases[@]} )) || cases=(boot)
[[ -n "$expect_override" ]] && EXPECTED_BANNER="$expect_override"
for c in "${cases[@]}"; do define_case "$c" || fail "unknown case '$c' (try --list)"; done

if (( do_build )); then
    "$ROOT/scripts/build.sh" || fail "build failed - not attempting to boot"
fi

run_dir="$LOGS/test-$(date +%Y%m%d-%H%M%S)"
mkdir -p "$run_dir"
ln -sfn "$(basename "$run_dir")" "$LOGS/test-latest"

# Runs one case in the current shell; returns 0 on pass. Leaves no QEMU behind.
run_case() {
    local name="$1" image="$BUILD/test/$1.img"
    local serial_log="$run_dir/$1.serial.log" qemu_log="$run_dir/$1.qemu.log"
    define_case "$name"
    echo
    log "=== case: $name ${CMDLINE:+(cmdline: $CMDLINE)}"

    IMAGE="$image" "$ROOT/scripts/mkimage.sh" "$image" "$CMDLINE" >/dev/null || { echo "  image build failed"; return 1; }
    : >"$serial_log"
    # shellcheck disable=SC2046
    IMAGE="$image" qemu-system-x86_64 $(IMAGE="$image" qemu_base_args) -display none -monitor none \
        -serial "file:$serial_log" >"$qemu_log" 2>&1 &
    local pid=$!
    QEMU_PIDS+=("$pid")

    clean()   { tr -d '\r' <"$serial_log" | sed 's/\x1b\[[0-9;=?]*[A-Za-z]//g'; }
    has_line(){ clean | grep -qFx -- "$1"; }
    has_sub() { clean | grep -qF  -- "$1"; }
    all_ok() {
        local l
        for l in "${EXPECT[@]}";   do has_line "$l" || return 1; done
        for l in "${CONTAINS[@]}"; do has_sub  "$l" || return 1; done
        return 0
    }
    forbidden() { local l; for l in "${FORBID[@]}"; do has_sub "$l" && { echo "$l"; return 0; }; done; return 1; }

    local verdict=timeout why="" deadline=$(( SECONDS + TIMEOUT ))
    while (( SECONDS < deadline )); do
        if ! kill -0 "$pid" 2>/dev/null; then verdict=fail why="QEMU exited (reset/triple fault?)"; break; fi
        if all_ok; then verdict=pass; break; fi
        sleep 0.2
    done

    if [[ $verdict == pass ]]; then
        # Controlled halt: the VM must still be running and print nothing more.
        local size; size=$(stat -c %s "$serial_log"); sleep "$SETTLE"
        if ! kill -0 "$pid" 2>/dev/null; then verdict=fail why="QEMU exited after the report (reset/triple fault?)"
        elif [[ $(stat -c %s "$serial_log") != "$size" ]]; then verdict=fail why="output continued after the expected end"
        elif bad=$(forbidden); then verdict=fail why="forbidden output present: \"$bad\""
        fi
    fi
    if [[ $verdict == pass && $CHECK_RIP == 1 ]]; then
        local rip
        rip=$(clean | sed -n 's/^\[nova\] selftest: triggering .*, expected RIP \(0x[0-9a-f]\{16\}\)$/\1/p')
        if [[ -z "$rip" ]]; then verdict=fail why="kernel did not announce the expected RIP"
        elif ! clean | grep -qE "^RIP: $rip  CS: 0x0008  RFLAGS: 0x[0-9a-f]{16}$"; then
            verdict=fail why="report RIP/CS does not match expected RIP $rip, CS 0x0008"
        else
            echo "  RIP check: report RIP == announced $rip, CS == 0x0008"
        fi
    fi

    kill -TERM "$pid" 2>/dev/null || true
    for _ in $(seq 50); do kill -0 "$pid" 2>/dev/null || break; sleep 0.1; done
    kill -KILL "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
    kill -0 "$pid" 2>/dev/null && { verdict=fail why="QEMU pid $pid survived termination"; }

    echo "  ----- serial output -----"
    clean | grep -v -e '^$' -e '^BdsDxe' | sed 's/^/  | /'
    echo "  -------------------------"
    if [[ $verdict == pass ]]; then
        printf '  \033[32mPASS\033[0m %s\n' "$name"; return 0
    fi
    [[ $verdict == timeout ]] && {
        why="expected output not seen within ${TIMEOUT}s"
        for l in "${EXPECT[@]}"; do has_line "$l" || echo "  missing line: \"$l\""; done
        for l in "${CONTAINS[@]}"; do has_sub "$l" || echo "  missing text: \"$l\""; done
    }
    printf '  \033[31mFAIL\033[0m %s: %s (logs: %s)\n' "$name" "$why" "$run_dir"
    return 1
}

QEMU_PIDS=()
trap 'for p in "${QEMU_PIDS[@]}"; do kill -KILL "$p" 2>/dev/null || true; done' EXIT INT TERM

passed=() failed=()
for c in "${cases[@]}"; do
    if run_case "$c"; then passed+=("$c"); else failed+=("$c"); fi
done

echo
log "Results (logs: $run_dir)"
for c in "${passed[@]}"; do printf '  \033[32mPASS\033[0m %s\n' "$c"; done
for c in "${failed[@]}"; do printf '  \033[31mFAIL\033[0m %s\n' "$c"; done
(( ${#failed[@]} == 0 )) || fail "${#failed[@]} of ${#cases[@]} case(s) failed"
log "PASS: ${#passed[@]} of ${#cases[@]} case(s) passed"
