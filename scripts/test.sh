#!/usr/bin/env bash
# Automated QEMU tests. Each case boots the same kernel binary headless, with a
# case-specific kernel command line, captures COM1, and checks the output.
# Shell cases also type commands, over COM1 or through the PS/2 keyboard (QEMU
# monitor "sendkey"), each one after the shell has printed its prompt.
# The kernel never exits (it runs the shell or halts), so this script stops QEMU itself.
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

EXPECTED_BANNER="PROJECT NOVA KERNEL 0.4"
STATUS="[nova] status: kernel main path reached, init OK"
SHELL_START="[nova] starting shell"
PROMPT="nova> "
EXC="!!! CPU EXCEPTION !!!"
PANIC="!!! KERNEL PANIC !!!"
HALTED="[nova] system halted: unrecoverable exception"
NO_ERR="error code: none (not pushed for this vector)"
TIMEOUT="${TIMEOUT:-30}"
SETTLE=2   # seconds the VM must stay up and silent after the verdict (no reset, no extra output)

ALL_CASES=(boot panic de ud gp pf bp df nested shell keyboard crash native net chromium browser setup installed hostlogin deepseek oom apps)
MOCK_PORT=$(( 20000 + $$ % 20000 ))
TEST_KEY="sk-ant-test-0123456789abcdef"   # the mock API's only valid key (scripts/mock_anthropic.py)
CLAUDE_BOX="│ ✻ Claude Code for Nova                               │"

# Per-case spec: EXPECT (exact lines), CONTAINS (substrings), FORBID (substrings),
# CHECK_RIP (1 = report RIP must equal the address the kernel announced),
# INPUT (lines to type, each prefixed "ser:" for COM1 or "kbd:" for the PS/2
# keyboard; Enter is added unless the entry ends with Ctrl-C), FINAL_PROMPT (1 = the shell prompts again after the
# last input; 0 if the last input halts the machine).
# Step mode: if INPUT contains "wait:TEXT" entries, inputs are not paced by shell prompts. Each wait
# step blocks until TEXT has appeared once more than at the previous wait for the same TEXT; other
# steps are sent immediately ("raw:..." sends without Enter, "sleep:N" waits N seconds,
# "dump:FILE" saves a screenshot of the display to the log directory, "key:NAME" presses
# a key through the QEMU monitor).
# QEMU_EXTRA: extra QEMU arguments. EXPECT_EXIT=1: the case ends with the VM rebooting (QEMU exits).
# MOCK=1: run with the mock Anthropic API and fail on any protocol violation it logs.
define_case() {
    EXPECT=() CONTAINS=() FORBID=("$PANIC" "$EXC" "NESTED") CHECK_RIP=0 CMDLINE="nova.test=$1"
    INPUT=() FINAL_PROMPT=1 QEMU_EXTRA=() EXPECT_EXIT=0 MOCK=0 BOOT_IMAGE="" FRESH_TARGET=0 SKIP="" CASE_TIMEOUT=$TIMEOUT
    local fault=("$EXC" "$STATUS" "$HALTED")
    local fault_forbid=("$PANIC" "$SHELL_START" "NESTED")
    local shell=("$EXPECTED_BANNER" "$STATUS" "$SHELL_START")
    case "$1" in
        boot)   CMDLINE="nova.mode=shell"; EXPECT=("${shell[@]}" "[nova] interrupts: enabled (timer, keyboard, mouse, COM1)" "$PROMPT") ;;
        shell)  CMDLINE="nova.mode=shell"
                INPUT=("ser:help" "ser:echo hello from serial" "ser:frobnicate" "ser:sleep 1" "ser:echo first"
                       "ser:echo second" $'ser:\e[A\e[A' $'ser:junk\x7f\x7f\x7f\x7fhistory')
                EXPECT=("${shell[@]}" "Available commands:" "hello from serial"
                        "nova: unknown command: frobnicate (try 'help')" "  7  echo first" "  8  history")
                CONTAINS=("sleep    <seconds>  wait (uses the timer interrupt)") ;;
        keyboard) CMDLINE="nova.mode=shell"
                INPUT=("kbd:echo Hello, Keyboard!" $'kbd:echoo\b tab: ok' $'kbd:ec\t~Typed_via-PS/2.' $'kbd:abc\x03')
                EXPECT=("${shell[@]}" "Hello, Keyboard!" "tab: ok" "~Typed_via-PS/2." "${PROMPT}abc^C") ;;
        crash)  CMDLINE="nova.mode=shell"; INPUT=("ser:crash pf"); FINAL_PROMPT=0
                EXPECT=("${shell[@]}" "$EXC" "$HALTED" "exception: #PF Page Fault (vector 14, fault)"
                        "fault address (CR2): 0x00000deadbeef000")
                FORBID=("$PANIC" "NESTED"); CHECK_RIP=1 ;;
        setup)  # First boot from the live image: drivers, Claude login, driver porting, installation.
                CMDLINE="nova.claude_api=http://10.0.2.2:$MOCK_PORT"; MOCK=1; EXPECT_EXIT=1
                FRESH_TARGET=1
                QEMU_EXTRA=(-drive "format=raw,file=${run_dir:-}/target.img" -device ich9-intel-hda)
                INPUT=("wait:  API key: " "ser:$TEST_KEY" "wait:Install Nova on disk 1? [Y/n]" "raw:y")
                EXPECT=("$EXPECTED_BANNER" "Setting up this computer" "$CLAUDE_BOX"
                        "Porting drivers with Claude" "⏺ PciConfig(00:03.0)" "⏺ Write(drivers/intel_hda.rs)"
                        "  ✓ Nova is installed. Your settings and files were copied." "[nova] rebooting")
                CONTAINS=("e1000  Intel 82540EM" "sata1 (QEMU HARDDISK) 2048 MiB"
                          "Intel HD audio controller  no driver yet" "✓ Signed in." "network: online, address 10.0.2.15/24")
                FORBID=("$PANIC" "$EXC" "API error") ;;
        installed) # Boot the disk the setup case installed: desktop, Claude Code builds an app.
                CMDLINE=""; MOCK=1; BOOT_IMAGE="${run_dir:-}/target.img"
                QEMU_EXTRA=(-device ich9-intel-hda)
                INPUT=("wait:$CLAUDE_BOX" "wait:> " "ser:make me a clock app" "wait:Your clock app is ready"
                       "sleep:2" "key:meta_l" "sleep:1" "key:up" "key:up" "key:up" "key:ret" "sleep:3" "dump:terminal.ppm")
                EXPECT=("$EXPECTED_BANNER" "$CLAUDE_BOX" "> make me a clock app" "⏺ Write(apps/clock.rhai)"
                        "⏺ CheckApp(apps/clock.rhai)" "  ⎿  OK, screenshot attached" "⏺ Launch(apps/clock.rhai)")
                CONTAINS=("storage: installed system, NovaFS on sata0" "Rust port drafted by Claude (drivers/intel_hda.rs)"
                          "Your clock app is ready")
                FORBID=("$PANIC" "$EXC" "API error" "  API key: ") ;;
        hostlogin) # First boot with the host's `ant auth login` token passed in via fw_cfg (as
                # scripts/run.sh does): no key prompt, requests use Bearer + the OAuth beta.
                CMDLINE="nova.claude_api=http://10.0.2.2:$MOCK_PORT nova.install=no"; MOCK=1
                [[ -n "${run_dir:-}" ]] && printf 'oauth\nnova-oauth-test-token\nant auth login on the host\n' >"$run_dir/host-credential"
                QEMU_EXTRA=(-fw_cfg "name=opt/nova/claude-credential,file=${run_dir:-}/host-credential" -device ich9-intel-hda)
                # Signed in twice: first-boot setup, then Claude Code on the desktop.
                INPUT=("wait:✓ Signed in with your Anthropic CLI login" "wait:Starting the desktop"
                       "wait:✓ Signed in with your Anthropic CLI login" "wait:> ")
                EXPECT=("$EXPECTED_BANNER" "$CLAUDE_BOX" "Porting drivers with Claude" "⏺ PciConfig(00:03.0)" "⏺ Write(drivers/intel_hda.rs)")
                CONTAINS=("✓ Signed in with your Anthropic CLI login (ant auth login on the host)." "Starting the desktop")
                FORBID=("$PANIC" "$EXC" "API error" "  API key: " "not usable") ;;
        deepseek) # DeepSeek as the provider (key passed in like scripts/run.sh does): Nova Assistant
                # signs in via /models, ports a driver, then builds an app on the desktop; the mock
                # checks DeepSeek's rules (model, effort levels, screenshots beside tool results).
                CMDLINE="nova.deepseek_api=http://10.0.2.2:$MOCK_PORT/anthropic nova.comfyui=http://10.0.2.2:$MOCK_PORT nova.install=no"; MOCK=1
                [[ -n "${run_dir:-}" ]] && printf 'deepseek-key\nsk-deepseek-test-0123456789\nDEEPSEEK_API_KEY on the host\n' >"$run_dir/deepseek-credential"
                QEMU_EXTRA=(-fw_cfg "name=opt/nova/claude-credential,file=${run_dir:-}/deepseek-credential" -device ich9-intel-hda)
                INPUT=("wait:✓ Signed in with a DeepSeek API key" "wait:Starting the desktop"
                       "wait:✓ Signed in with a DeepSeek API key" "wait:> " "ser:make me a clock app" "wait:Your clock app is ready"
                       "wait:> " "ser:/clear" "wait:conversation cleared" "ser:make a poster image" "wait:Poster image ready.")
                EXPECT=("$EXPECTED_BANNER" "│ ✻ Nova Assistant · DeepSeek                          │" "Porting drivers with DeepSeek"
                        "⏺ PciConfig(00:03.0)" "⏺ Write(apps/clock.rhai)" "⏺ CheckApp(apps/clock.rhai)" "  ⎿  OK, screenshot attached")
                CONTAINS=("Your clock app is ready" "⏺ GenerateImage(apps/assets/poster-bg.png)" "256x144 image saved")
                FORBID=("$PANIC" "$EXC" "API error" "  API key: " "not usable" "Claude" "The image failed") ;;
        native) # C/C++ programs (static glibc/libstdc++) through the Linux syscall layer.
                CMDLINE="nova.mode=shell"
                INPUT=("wait:$PROMPT" "ser:mem" "wait:kernel heap:" "ser:hello one two" "wait:  HOME=/" "ser:ctest" "wait:ctest: 24 passed"
                       "ser:cxxtest" "wait:static destructor ran" "ser:threads" "wait:threads: " "ser:hello-dyn" "wait:objects in the process"
                       "ser:cxxtest-dyn" "wait:static destructor ran" "ser:guess" "wait:Your guess: " "ser:50"
                       "wait:Your guess: " "ser:x" "wait:[exit status 1]" "ser:mem" "wait:kernel heap:")
                EXPECT=("${shell[@]}" "Hello from C on Nova!" "  argv[2] = two" "ctest: 24 passed, 0 failed"
                        "cxxtest: 17 passed, 0 failed" "cxxtest: static destructor ran" "threads: 7 passed, 0 failed"
                        "Hello from a dynamically linked C program on Nova!" "  loaded /usr/lib/libc.so.6" "I'm thinking of a number between 1 and 100."
                        "[exit status 1]")
                CONTAINS=("C/C++ compatibility: Linux x86-64 ABI")
                FORBID=("$PANIC" "$EXC" "FAIL " "unimplemented Linux syscall" "illegal instruction" "segmentation fault") ;;
        oom)    # A program that uses up all memory is stopped; the kernel and shell keep working.
                CMDLINE="nova.mode=shell"; CASE_TIMEOUT=$(( TIMEOUT > 90 ? TIMEOUT : 90 ))
                INPUT=("wait:$PROMPT" "ser:memhog" "wait:the program was stopped" "ser:hello again" "wait:  argv[1] = again" "ser:mem")
                EXPECT=("${shell[@]}" "Hello from C on Nova!" "[exit status 137]")
                CONTAINS=("memhog: 1024 MiB" "memhog: out of memory (Nova has" "kernel heap:")
                FORBID=("$PANIC" "$EXC" "NESTED") ;;
        net)    # Linux sockets: glibc DNS (UDP) and TCP, against the local mock server.
                CMDLINE="nova.mode=shell"; MOCK=1
                INPUT=("wait:$PROMPT" "ser:nettest 10.0.2.2:$MOCK_PORT" "wait:bytes")
                EXPECT=("${shell[@]}" "nettest: 10.0.2.2 resolves to 10.0.2.2" "nettest: connected")
                CONTAINS=("nettest: HTTP/1.1 404 Not Found" "nettest: MSG_PEEK kept the data")
                FORBID=("$PANIC" "$EXC" "segmentation fault" "illegal instruction") ;;
        apps)   # Native apps on the desktop: the Text Editor with a sample file (quick open,
                # find) and Files (grid, preview, list), with screenshots for review.
                CMDLINE="nova.claude_api=http://10.0.2.2:$MOCK_PORT nova.install=no"; MOCK=1; CASE_TIMEOUT=120
                INPUT=("wait:  API key: " "ser:$TEST_KEY" "wait:> " "ser:show the editor" "wait:The editor is open."
                       "sleep:2" "dump:editor.ppm" "key:ctrl-p" "key:h" "key:e" "sleep:1" "dump:editor-open.ppm"
                       "key:esc" "key:ctrl-f" "key:w" "key:o" "key:r" "key:d" "key:ret" "sleep:1" "dump:editor-find.ppm"
                       "key:esc" "ser:/clear" "wait:conversation cleared" "ser:show the files" "wait:Files is open." "sleep:2" "dump:files.ppm"
                       "key:right" "key:right" "sleep:1" "dump:files-preview.ppm" "key:ctrl-l" "sleep:1" "dump:files-list.ppm")
                EXPECT=("$EXPECTED_BANNER" "⏺ Launch(notes/hello.rs)")
                CONTAINS=("The editor is open.")
                FORBID=("$PANIC" "$EXC" "API error") ;;
        webpage) # Diagnostic (needs internet, not in --all): Nova Browser opens WEB_URL (default
                # DuckDuckGo), logs every host that served it, and saves a screenshot.
                CMDLINE="nova.claude_api=http://10.0.2.2:$MOCK_PORT nova.install=no nova.browser_log"; MOCK=1; CASE_TIMEOUT=180
                [[ -d "$BUILD/sysroot/opt/chromium" ]] || SKIP="Chromium not downloaded"
                INPUT=("wait:  API key: " "ser:$TEST_KEY" "wait:> " "ser:open url ${WEB_URL:-https://duckduckgo.com}" "wait:The page is open." "sleep:${WEB_WAIT:-30}" "dump:webpage.ppm")
                EXPECT=("$EXPECTED_BANNER")
                CONTAINS=("The page is open.") ;;
        webimg) # Diagnostic (needs internet, not in --all): Chromium loads HTTPS images directly.
                CMDLINE="nova.mode=shell"; CASE_TIMEOUT=150
                [[ -d "$BUILD/sysroot/opt/chromium" ]] || SKIP="Chromium not downloaded"
                local ch="run opt/chromium/chrome-headless-shell --no-sandbox --single-process --no-zygote --disable-gpu --virtual-time-budget=9000 --dump-dom"
                INPUT=("wait:$PROMPT" "ser:$ch data:text/html,<img%20src=https://duckduckgo.com/favicon.ico%20onerror=document.title='E'%20onload=document.title='K'>" "wait:</html>"
                       "ser:$ch data:text/html,<img%20src=https://www.wikipedia.org/static/favicon/wikipedia.ico%20onerror=document.title='E'%20onload=document.title='K'>" "wait:</html>")
                EXPECT=("${shell[@]}")
                CONTAINS=("<title>K</title>") ;;
        chromium) # The Chromium engine renders a page (no network needed) to a PNG.
                CMDLINE="nova.mode=shell"
                [[ -d "$BUILD/sysroot/opt/chromium" ]] || SKIP="Chromium not downloaded (scripts/fetch-chromium.sh)"
                INPUT=("wait:$PROMPT" "ser:run opt/chromium/chrome-headless-shell --no-sandbox --single-process --no-zygote --disable-gpu --screenshot=/tmp/t.png --window-size=640,480 data:text/html,<h1>Nova</h1>" "wait:bytes written to file /tmp/t.png" "ser:ls tmp")
                EXPECT=("${shell[@]}")
                CONTAINS=("bytes written to file /tmp/t.png" "tmp/t.png")
                FORBID=("$PANIC" "$EXC" "FATAL") ;;
        browser) # Desktop: Claude opens a page in Nova Browser; Chromium loads it, Shields blocks its tracker.
                # Then screenshots: the browser, Home (Windows key), an app preview and the app.
                CMDLINE="nova.claude_api=http://10.0.2.2:$MOCK_PORT nova.install=no"; MOCK=1; CASE_TIMEOUT=$(( TIMEOUT > 90 ? TIMEOUT : 90 ))
                [[ -d "$BUILD/sysroot/opt/chromium" ]] || SKIP="Chromium not downloaded (scripts/fetch-chromium.sh)"
                INPUT=("wait:  API key: " "ser:$TEST_KEY" "wait:> " "ser:open the test page" "wait:shields: blocked www.google-analytics.com"
                       "wait:The test page is open in Nova Browser." "sleep:8" "dump:browser.ppm"
                       "key:meta_l" "sleep:3" "dump:home.ppm"
                       "key:down" "key:right" "key:right" "key:right" "key:right" "sleep:4" "dump:home-preview.ppm"
                       "key:ret" "sleep:4" "dump:app.ppm")
                EXPECT=("$EXPECTED_BANNER" "⏺ OpenUrl(http://10.0.2.2:$MOCK_PORT/test-page)" "  ⎿  Opened in Nova Browser")
                CONTAINS=("[nova] shields: blocked www.google-analytics.com" "The test page is open in Nova Browser.")
                FORBID=("$PANIC" "$EXC" "API error" "engine stopped") ;;
        panic)  EXPECT=("$STATUS" "$PANIC" "[nova] system halted: kernel panic")
                CONTAINS=("panicked at" "intentional panic requested by nova.test=panic")
                FORBID=("$EXC" "NESTED" "$SHELL_START") ;;
        de)     EXPECT=("${fault[@]}" "exception: #DE Divide Error (vector 0, fault)" "$NO_ERR")
                FORBID=("${fault_forbid[@]}"); CHECK_RIP=1 ;;
        ud)     EXPECT=("${fault[@]}" "exception: #UD Invalid Opcode (vector 6, fault)" "$NO_ERR")
                FORBID=("${fault_forbid[@]}"); CHECK_RIP=1 ;;
        gp)     EXPECT=("${fault[@]}" "exception: #GP General Protection (vector 13, fault)"
                        "error code: 0x0000000000000040 (selector index 8, GDT)")
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
                FORBID=("$PANIC" "$SHELL_START" "$NO_ERR" "RIP:") ;;   # first report must stop at the nested fault
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

# QEMU "sendkey" names for the characters the keyboard case types.
# \b = Backspace, \t = Tab, \x03 = Ctrl-C.
key_name() {
    case "$1" in
        [a-z0-9]) echo "$1" ;;
        [A-Z])    echo "shift-${1,,}" ;;
        ' ') echo spc ;;      $'\b') echo backspace ;; $'\t') echo tab ;; $'\x03') echo ctrl-c ;;
        ',') echo comma ;;    '.') echo dot ;;     '/') echo slash ;;       '-') echo minus ;;
        '_') echo shift-minus ;; ':') echo shift-semicolon ;; '!') echo shift-1 ;; '~') echo shift-grave_accent ;;
        *) return 1 ;;
    esac
}

# Type one INPUT entry ("ser:..." or "kbd:...") into the fds opened by run_case,
# followed by Enter unless it ends with Ctrl-C (which already ends the line).
send_input() {
    local entry="$1" text="${1#*:}" enter=1 i k
    [[ "$text" == *$'\x03' ]] && enter=0
    case "$entry" in
        ser:*) printf '%s' "$text" >&"$SERIAL_FD"
               if (( enter )); then printf '\r' >&"$SERIAL_FD"; fi ;;
        kbd:*) for (( i = 0; i < ${#text}; i++ )); do
                   k=$(key_name "${text:i:1}") || { echo "  no key for '${text:i:1}'"; return 1; }
                   echo "sendkey $k" >&"$MONITOR_FD"; sleep 0.05
               done
               if (( enter )); then echo "sendkey ret" >&"$MONITOR_FD"; fi ;;
        *) echo "  bad INPUT entry: $entry"; return 1 ;;
    esac
}

# Runs one case in the current shell; returns 0 on pass. Leaves no QEMU behind.
run_case() {
    local name="$1" image="$BUILD/test/$1.img"
    local serial_log="$run_dir/$1.serial.log" qemu_log="$run_dir/$1.qemu.log"
    local fifo="$run_dir/$1.fifo"
    define_case "$name"
    echo
    log "=== case: $name ${CMDLINE:+(cmdline: $CMDLINE)}"
    if [[ -n "$SKIP" ]]; then
        printf '  \033[33mSKIP\033[0m %s: %s\n' "$name" "$SKIP"
        return 0
    fi

    if [[ -n "$BOOT_IMAGE" ]]; then
        [[ -f "$BOOT_IMAGE" ]] || { echo "  $BOOT_IMAGE missing: run the setup case first (--case setup --case $name)"; return 1; }
        image="$BOOT_IMAGE"
    else
        IMAGE="$image" "$ROOT/scripts/mkimage.sh" "$image" "$CMDLINE" >/dev/null || { echo "  image build failed"; return 1; }
    fi
    if (( FRESH_TARGET )); then
        rm -f "${run_dir:-}/target.img"
        truncate -s 2G "${run_dir:-}/target.img"
    fi
    local mock_mark=0
    (( MOCK )) && mock_mark=$(wc -l <"$MOCK_LOG")
    # COM1 and the monitor are QEMU "pipe" chardevs: <path>.in carries input to the guest,
    # <path>.out its output. cat drains the outputs (COM1 into the log); it exits with QEMU.
    mkdir -p "$fifo"
    mkfifo "$fifo/serial.in" "$fifo/serial.out" "$fifo/monitor.in" "$fifo/monitor.out"
    cat "$fifo/serial.out" >"$serial_log" &
    local cat_pids=($!)
    cat "$fifo/monitor.out" >"$run_dir/$1.monitor.log" &
    cat_pids+=($!)
    # Opening the input FIFOs read-write never blocks, even if QEMU has died.
    exec {SERIAL_FD}<>"$fifo/serial.in" {MONITOR_FD}<>"$fifo/monitor.in"
    # shellcheck disable=SC2046
    IMAGE="$image" qemu-system-x86_64 $(IMAGE="$image" qemu_base_args) -display none \
        -chardev "pipe,id=com1,path=$fifo/serial" -serial chardev:com1 \
        -chardev "pipe,id=mon,path=$fifo/monitor" -mon chardev=mon,mode=readline "${QEMU_EXTRA[@]}" >"$qemu_log" 2>&1 &
    local pid=$!
    QEMU_PIDS+=("$pid")

    clean()   { tr -d '\r' <"$serial_log" | sed 's/\x1b\[[0-9;=?]*[A-Za-z]//g'; }
    has_line(){ clean | grep -qFx -- "$1"; }
    has_sub() { clean | grep -qF  -- "$1"; }
    prompts() { clean | grep -oF -- "$PROMPT" | wc -l; }
    all_ok() {
        local l
        for l in "${EXPECT[@]}";   do has_line "$l" || return 1; done
        for l in "${CONTAINS[@]}"; do has_sub  "$l" || return 1; done
        # Interactive cases are complete only once the shell has handled every input.
        (( step_mode || ${#INPUT[@]} == 0 || $(prompts) >= ${#INPUT[@]} + FINAL_PROMPT )) || return 1
        return 0
    }
    forbidden() { local l; for l in "${FORBID[@]}"; do has_sub "$l" && { echo "$l"; return 0; }; done; return 1; }

    local verdict=timeout why="" deadline=$(( SECONDS + CASE_TIMEOUT )) sent=0 step_mode=0
    local -A seen=()
    [[ " ${INPUT[*]} " == *" wait:"* || "${INPUT[0]:-}" == wait:* ]] && step_mode=1
    count_of() { clean | grep -oF -- "$1" | wc -l; }
    while (( SECONDS < deadline )); do
        if ! kill -0 "$pid" 2>/dev/null; then
            if (( EXPECT_EXIT )) && (( sent == ${#INPUT[@]} )) && all_ok; then verdict=pass; else verdict=fail why="QEMU exited (reset/triple fault?)"; fi
            break
        fi
        if (( step_mode )) && (( sent < ${#INPUT[@]} )); then
            local step="${INPUT[sent]}"
            if [[ $step == wait:* ]]; then
                local text="${step#wait:}" want=$(( ${seen[${step#wait:}]:-0} + 1 ))
                if (( $(count_of "$text") >= want )); then seen[$text]=$want; sent=$(( sent + 1 )); continue; fi
            elif [[ $step == raw:* ]]; then
                printf '%s' "${step#raw:}" >&"$SERIAL_FD"; sent=$(( sent + 1 )); continue
            elif [[ $step == sleep:* ]]; then
                sleep "${step#sleep:}"; sent=$(( sent + 1 )); continue
            elif [[ $step == key:* ]]; then
                # A key press through the QEMU monitor (e.g. key:meta_l, the Windows key).
                printf 'sendkey %s\n' "${step#key:}" >&"$MONITOR_FD"; sleep 1; sent=$(( sent + 1 )); continue
            elif [[ $step == dump:* ]]; then
                # Screenshot of the VM's display (PPM) into the log directory.
                printf 'screendump %s\n' "$run_dir/${step#dump:}" >&"$MONITOR_FD"; sleep 1; sent=$(( sent + 1 )); continue
            else
                send_input "$step" || { verdict=fail why="could not send input"; break; }
                sent=$(( sent + 1 )); continue
            fi
        elif (( ! step_mode )) && (( sent < ${#INPUT[@]} )) && (( $(prompts) > sent )); then
            # Type the next input once the shell is waiting for it (prompt number sent+1).
            send_input "${INPUT[sent]}" || { verdict=fail why="could not send input"; break; }
            sent=$(( sent + 1 ))
            continue
        fi
        if (( ! EXPECT_EXIT )) && (( sent == ${#INPUT[@]} )) && all_ok; then verdict=pass; break; fi
        sleep 0.2
    done

    if [[ $verdict == pass && $EXPECT_EXIT == 1 ]]; then
        if bad=$(forbidden); then verdict=fail why="forbidden output present: \"$bad\""; fi
    elif [[ $verdict == pass ]]; then
        # Controlled halt: the VM must still be running and print nothing more.
        local size; size=$(stat -c %s "$serial_log"); sleep "$SETTLE"
        if ! kill -0 "$pid" 2>/dev/null; then verdict=fail why="QEMU exited after the report (reset/triple fault?)"
        elif [[ $(stat -c %s "$serial_log") != "$size" ]]; then verdict=fail why="output continued after the expected end"
        elif bad=$(forbidden); then verdict=fail why="forbidden output present: \"$bad\""
        fi
    fi
    if [[ $verdict == pass && $MOCK == 1 ]] && tail -n +$(( mock_mark + 1 )) "$MOCK_LOG" | grep -q VIOLATION; then
        verdict=fail why="the mock API saw protocol violations: $(tail -n +$(( mock_mark + 1 )) "$MOCK_LOG" | grep VIOLATION | head -3 | tr '\n' ' ')"
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
    # With QEMU gone the output FIFOs have no writer left, so both cats see EOF.
    exec {SERIAL_FD}>&- {MONITOR_FD}>&-
    wait "${cat_pids[@]}" 2>/dev/null || true
    rm -rf -- "$fifo"

    echo "  ----- serial output -----"
    clean | grep -v -e '^$' -e '^BdsDxe' | sed 's/^/  | /'
    echo "  -------------------------"
    if [[ $verdict == pass ]]; then
        printf '  \033[32mPASS\033[0m %s\n' "$name"; return 0
    fi
    [[ $verdict == timeout ]] && {
        why="expected output not seen within ${CASE_TIMEOUT}s"
        for l in "${EXPECT[@]}"; do has_line "$l" || echo "  missing line: \"$l\""; done
        for l in "${CONTAINS[@]}"; do has_sub "$l" || echo "  missing text: \"$l\""; done
    }
    printf '  \033[31mFAIL\033[0m %s: %s (logs: %s)\n' "$name" "$why" "$run_dir"
    return 1
}

QEMU_PIDS=()
MOCK_PID=""
MOCK_LOG="$run_dir/mock-api.log"
: >"$MOCK_LOG"
for c in "${cases[@]}"; do
    define_case "$c"
    if (( MOCK )) && [[ -z "$MOCK_PID" ]]; then
        python3 "$ROOT/scripts/mock_anthropic.py" "$MOCK_PORT" "$MOCK_LOG" >/dev/null 2>&1 &
        MOCK_PID=$!
        sleep 0.5
    fi
done
trap 'for p in "${QEMU_PIDS[@]}" $MOCK_PID; do kill -KILL "$p" 2>/dev/null || true; done' EXIT INT TERM

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
