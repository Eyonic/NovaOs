//! The Nova shell: a line editor with history and a handful of built-in commands.
//!
//! Input comes from the PS/2 keyboard and the serial line alike (see input.rs);
//! output goes to the screen and the serial line. Editing happens at the end of
//! the line only: Backspace, Ctrl-U (erase line), Ctrl-C (cancel), Ctrl-L
//! (clear screen), Tab (complete a command name), Up/Down (history).

use alloc::string::String;
use alloc::vec::Vec;

use crate::lineedit::{self, History, ReadResult};
use crate::{console, drivers, limine, memory, net, pci, pit, print, println, selftest, task};

const PROMPT: &str = "\x1b[1;32mnova\x1b[0m> ";
const LINE_MAX: usize = 1000;
const HISTORY_LEN: usize = 50;

struct Command {
    name: &'static str,
    args: &'static str,
    help: &'static str,
    run: fn(args: &str, history: &History),
}

const COMMANDS: &[Command] = &[
    Command { name: "help", args: "", help: "list commands and editing keys", run: cmd_help },
    Command { name: "echo", args: "[text]", help: "print text", run: cmd_echo },
    Command { name: "clear", args: "", help: "clear the screen", run: cmd_clear },
    Command { name: "info", args: "", help: "kernel, bootloader and display information", run: cmd_info },
    Command { name: "mem", args: "", help: "physical memory map summary", run: cmd_mem },
    Command { name: "uptime", args: "", help: "time since the timer started", run: cmd_uptime },
    Command { name: "sleep", args: "<seconds>", help: "wait (uses the timer interrupt)", run: cmd_sleep },
    Command { name: "history", args: "", help: "list previous commands", run: cmd_history },
    Command { name: "ps", args: "", help: "list kernel tasks", run: cmd_ps },
    Command { name: "lspci", args: "", help: "PCI devices and their drivers", run: cmd_lspci },
    Command { name: "ifconfig", args: "", help: "network interface status", run: cmd_ifconfig },
    Command { name: "host", args: "<name>", help: "look up a host name (DNS)", run: cmd_host },
    Command { name: "fetch", args: "<url>", help: "HTTP GET a URL and print the response", run: cmd_fetch },
    Command { name: "disks", args: "", help: "disks, partitions and the mounted filesystem", run: cmd_disks },
    Command { name: "ls", args: "[prefix]", help: "list files", run: cmd_ls },
    Command { name: "cat", args: "<file>", help: "print a file", run: cmd_cat },
    Command { name: "write", args: "<file> <text>", help: "write text to a file", run: cmd_write },
    Command { name: "rm", args: "<file>", help: "delete a file", run: cmd_rm },
    Command { name: "install", args: "[disk]", help: "install Nova onto a disk", run: cmd_install },
    Command { name: "run", args: "<file> [args]", help: "run a Linux x86-64 program (C/C++, static or dynamic)", run: cmd_run },
    Command { name: "strace", args: "<file> [args]", help: "run a program, logging its system calls to serial", run: cmd_strace },
    Command { name: "browser", args: "[url]", help: "open Nova Browser (Chromium engine)", run: cmd_browser },
    Command { name: "download", args: "<url> <file>", help: "save a URL to a file (e.g. a Shields filter list)", run: cmd_download },
    Command { name: "crash", args: "<case>", help: "trigger a CPU exception or panic", run: cmd_crash },
    Command { name: "halt", args: "", help: "stop the CPU", run: cmd_halt },
    Command { name: "reboot", args: "", help: "reset the machine", run: cmd_reboot },
    Command { name: "shutdown", args: "", help: "power off (QEMU/ACPI)", run: cmd_shutdown },
];

/// Tab: complete the command name if exactly one command starts with what was typed.
fn complete(typed: &str) -> Option<String> {
    if typed.contains(' ') {
        return None;
    }
    let mut matches = COMMANDS.iter().filter(|c| c.name.starts_with(typed));
    match (matches.next(), matches.next()) {
        (Some(only), None) => Some(alloc::format!("{} ", &only.name[typed.len()..])),
        _ => None,
    }
}

/// Run one command line. Returns false if the line was empty.
pub fn execute(text: &str, history: &History) -> bool {
    let text = text.trim();
    if text.is_empty() {
        return false;
    }
    let (name, args) = text.split_once(' ').map_or((text, ""), |(n, a)| (n, a.trim()));
    match COMMANDS.iter().find(|c| c.name == name) {
        Some(cmd) => (cmd.run)(args, history),
        None if crate::fs::exists(&alloc::format!("bin/{name}")) => run_program(&alloc::format!("bin/{name}"), text),
        None => println!("nova: unknown command: {name} (try 'help')"),
    }
    true
}

/// Run a native program; `line` is the full command line (program name first).
fn run_program(path: &str, line: &str) {
    let args: Vec<String> = line.split_whitespace().map(String::from).collect();
    match crate::process::run(path, &args) {
        Ok(0) => {}
        Ok(code) => println!("\x1b[2m[exit status {code}]\x1b[0m"),
        Err(e) => println!("nova: {e}"),
    }
}

fn cmd_download(args: &str, _: &History) {
    let Some((url, path)) = args.split_once(' ') else {
        println!("usage: download <url> <file>   e.g. download https://easylist.to/easylist/easylist.txt system/shields/easylist.txt");
        return;
    };
    match crate::http::get(url.trim()) {
        Ok(r) if r.status == 200 => match crate::fs::write(path.trim(), &r.body) {
            Ok(()) => println!("saved {} bytes to {}", r.body.len(), path.trim()),
            Err(e) => println!("download: {e}"),
        },
        Ok(r) => println!("download: HTTP {}", r.status),
        Err(e) => println!("download: {e}"),
    }
}

fn cmd_browser(args: &str, _: &History) {
    if !crate::wm::running() {
        println!("browser: the desktop is not running");
        return;
    }
    crate::browser::launch(if args.is_empty() { None } else { Some(String::from(args)) });
}

fn cmd_strace(args: &str, _: &History) {
    let Some(path) = args.split_whitespace().next() else {
        println!("usage: strace <program> [args]   (system calls are logged to the serial console)");
        return;
    };
    let path = path.trim_start_matches('/');
    let path = if crate::fs::exists(path) { String::from(path) } else { alloc::format!("bin/{path}") };
    let argv: Vec<String> = args.split_whitespace().map(String::from).collect();
    match crate::process::run_traced(&path, &argv, true) {
        Ok(code) => println!("\x1b[2m[exit status {code}]\x1b[0m"),
        Err(e) => println!("nova: {e}"),
    }
}

fn cmd_run(args: &str, _: &History) {
    let Some(path) = args.split_whitespace().next() else {
        println!("usage: run <program> [args]   (a statically linked x86-64 Linux ELF on NovaFS)");
        return;
    };
    let path = path.trim_start_matches('/');
    run_program(path, args);
}

/// Run the shell until end of input (Ctrl-D or the terminal window closing).
pub fn run() {
    println!();
    println!("Welcome to \x1b[1mProject Nova\x1b[0m. Type \x1b[1mhelp\x1b[0m for a list of commands.");
    let mut history = History::new(HISTORY_LEN);
    let complete = complete;
    let opts = lineedit::Options { prompt: PROMPT, max_len: LINE_MAX, masked: false, complete: Some(&complete) };
    loop {
        match lineedit::read_line(&opts, Some(&history)) {
            ReadResult::Line(line) => {
                history.add(&line);
                execute(&line, &history);
            }
            ReadResult::Cancelled => {}
            ReadResult::Eof => return,
        }
    }
}

fn cmd_help(_: &str, _: &History) {
    println!("Available commands:");
    for c in COMMANDS {
        println!("  \x1b[1m{:<9}\x1b[0m{:<11}{}", c.name, c.args, c.help);
    }
    println!("Keys: Backspace, Tab (complete), Up/Down (history), Ctrl-C (cancel line),");
    println!("      Ctrl-U (erase line), Ctrl-L (clear screen)");
}

fn cmd_echo(args: &str, _: &History) {
    println!("{args}");
}

fn cmd_clear(_: &str, _: &History) {
    print!("\x1b[2J\x1b[H");
}

fn cmd_info(_: &str, _: &History) {
    println!("kernel:      {} {} (x86_64)", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
    match limine::BOOTLOADER_INFO.get() {
        Some((name, version)) => println!("bootloader:  {name} {version}, protocol base revision {}", limine::REQUESTED_REVISION),
        None => println!("bootloader:  unknown"),
    }
    match console::screen_info() {
        Some((w, h, cols, rows)) => println!("display:     {w}x{h} framebuffer, {cols}x{rows} text console"),
        None => println!("display:     none (serial only)"),
    }
    println!("cmdline:     \"{}\"", limine::EXECUTABLE_CMDLINE.get().unwrap_or(""));
    println!("timer:       PIT at {} Hz, {} ticks", pit::HZ, pit::ticks());
}

fn cmd_mem(_: &str, _: &History) {
    let mut totals = [(0u64, 0usize); limine::MEMMAP_KINDS.len()];
    let mut other = (0u64, 0usize);
    for e in limine::MEMMAP.entries() {
        let slot = totals.get_mut(e.kind as usize).unwrap_or(&mut other);
        slot.0 += e.length;
        slot.1 += 1;
    }
    println!("{:<24}{:>13}{:>9}", "type", "size", "regions");
    for (name, &(bytes, n)) in limine::MEMMAP_KINDS.iter().zip(&totals).chain([(&"other", &other)]) {
        if n > 0 {
            println!("{name:<24}{:>9} KiB{n:>9}", bytes / 1024);
        }
    }
    let usable = totals[limine::MEMMAP_USABLE as usize].0;
    println!("usable RAM: {} MiB", usable / (1024 * 1024));
    let (used, free) = memory::heap_usage();
    println!("kernel heap: {} MiB, {} KiB used, {} KiB free", memory::heap_size() >> 20, used / 1024, free / 1024);
}

fn cmd_history(_: &str, history: &History) {
    for (i, l) in history.lines().iter().enumerate() {
        println!("{:>3}  {l}", i + 1);
    }
}

fn cmd_ps(_: &str, _: &History) {
    println!("{:>4}  {:<10} {}", "id", "state", "name");
    for (id, name, state) in task::list() {
        println!("{id:>4}  {state:<10} {name}");
    }
}

fn cmd_lspci(_: &str, _: &History) {
    for r in drivers::records() {
        let status = match &r.status {
            drivers::Status::Bound { driver, .. } => String::from(*driver),
            drivers::Status::Failed { driver, error } => alloc::format!("{driver} (failed: {error})"),
            drivers::Status::NotNeeded => "-".into(),
            drivers::Status::Missing => "\x1b[33mno driver\x1b[0m".into(),
            drivers::Status::Ported { .. } => "\x1b[33mport drafted (next build)\x1b[0m".into(),
        };
        let d = &r.device;
        println!("{}  {:04x}:{:04x}  {:<26} {:<16} {status}", d.address, d.vendor, d.device, d.class_name(), pci::vendor_name(d.vendor));
    }
}

fn cmd_ifconfig(_: &str, _: &History) {
    let Some(i) = net::info() else {
        println!("no network interface");
        return;
    };
    let m = i.mac;
    println!("eth0: mac {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}, link {}", m[0], m[1], m[2], m[3], m[4], m[5], if i.link { "up" } else { "down" });
    match i.ip {
        Some(ip) => println!("      inet {ip}"),
        None => println!("      inet (waiting for DHCP)"),
    }
    if let Some(gw) = i.gateway {
        println!("      gateway {gw}");
    }
    let dns: Vec<String> = i.dns.iter().map(|d| alloc::format!("{d}")).collect();
    println!("      dns {}", if dns.is_empty() { "-".into() } else { dns.join(" ") });
}

fn cmd_host(args: &str, _: &History) {
    if args.is_empty() {
        println!("usage: host <name>");
        return;
    }
    match net::resolve(args) {
        Ok(ip) => println!("{args} has address {ip}"),
        Err(e) => println!("host: {e}"),
    }
}

fn cmd_fetch(args: &str, _: &History) {
    match crate::http::get(args) {
        Ok(resp) => {
            println!("HTTP {} ({} bytes)", resp.status, resp.body.len());
            let text = String::from_utf8_lossy(&resp.body);
            for line in text.lines().take(20) {
                println!("{}", line.chars().take(150).collect::<String>());
            }
        }
        Err(e) => println!("fetch: {e}"),
    }
}

fn cmd_disks(_: &str, _: &History) {
    for d in crate::storage::list() {
        println!("disk {}: {} ({} MiB){}", d.index, d.name, d.sectors / 2048, if d.is_boot { " [boot]" } else { "" });
        for p in &d.partitions {
            let kind = if p.type_guid == crate::storage::ESP_TYPE {
                "EFI system"
            } else if p.type_guid == crate::storage::NOVA_DATA_TYPE {
                "NovaFS"
            } else {
                "other"
            };
            println!("  part {}: {:<10} {:>6} MiB  {}", p.index, kind, p.sectors() / 2048, p.name);
        }
    }
    let mode = match crate::install::mode() {
        crate::install::Mode::Live => "live session",
        crate::install::Mode::Installed => "installed system",
    };
    match (crate::fs::mount_info(), crate::fs::usage()) {
        (Some((label, disk)), Some((used, total))) => println!("mounted: NovaFS \"{label}\" on {disk}, {} KiB of {} KiB used ({mode})", used / 1024, total / 1024),
        _ => println!("no filesystem mounted ({mode})"),
    }
}

fn cmd_ls(args: &str, _: &History) {
    for (name, size, _) in crate::fs::list(args) {
        println!("{size:>9}  {name}");
    }
}

fn cmd_cat(args: &str, _: &History) {
    match crate::fs::read_string(args) {
        Ok(text) => {
            print!("{text}");
            if !text.ends_with('\n') {
                println!();
            }
        }
        Err(e) => println!("cat: {e}"),
    }
}

fn cmd_write(args: &str, _: &History) {
    let Some((name, text)) = args.split_once(' ') else {
        println!("usage: write <file> <text>");
        return;
    };
    let mut data = String::from(text);
    data.push('\n');
    if let Err(e) = crate::fs::write(name, data.as_bytes()) {
        println!("write: {e}");
    }
}

fn cmd_rm(args: &str, _: &History) {
    if let Err(e) = crate::fs::delete(args) {
        println!("rm: {e}");
    }
}

fn cmd_install(args: &str, _: &History) {
    let targets = crate::install::targets();
    if targets.is_empty() {
        println!("install: no suitable disk (needs a non-boot disk of at least 256 MiB)");
        return;
    }
    let Ok(index) = args.parse::<usize>() else {
        println!("usage: install <disk>   (ALL DATA ON THAT DISK IS ERASED)");
        for t in &targets {
            println!("  disk {}: {} ({} MiB)", t.index, t.name, t.sectors / 2048);
        }
        return;
    };
    let mut last = u32::MAX;
    let result = crate::install::install(index, |pct, msg| {
        if pct / 10 != last / 10 || pct == 100 {
            println!("[{pct:>3}%] {msg}");
            last = pct;
        }
    });
    match result {
        Ok(()) => println!("Nova is installed on disk {index}. Reboot from that disk to use it."),
        Err(e) => println!("install failed: {e}"),
    }
}

fn cmd_uptime(_: &str, _: &History) {
    let t = pit::ticks();
    let s = t / pit::HZ;
    println!("up {}:{:02}:{:02}.{:02} ({t} ticks at {} Hz)", s / 3600, s / 60 % 60, s % 60, t % pit::HZ, pit::HZ);
}

fn cmd_sleep(args: &str, _: &History) {
    let Ok(secs) = args.parse::<u64>() else {
        println!("usage: sleep <seconds>");
        return;
    };
    task::sleep_ticks(secs * pit::HZ);
}

fn cmd_crash(args: &str, _: &History) {
    if selftest::CASES.contains(&args) {
        selftest::run_case(args);
    }
    print!("usage: crash <case>   cases:");
    for case in selftest::CASES {
        print!(" {case}");
    }
    println!();
}

fn cmd_halt(_: &str, _: &History) {
    println!("[nova] system halted by user");
    crate::halt_forever()
}

fn cmd_reboot(_: &str, _: &History) {
    crate::power::reboot();
}

fn cmd_shutdown(_: &str, _: &History) {
    crate::power::shutdown();
}
