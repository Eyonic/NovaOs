// title: System Monitor
// icon: %
// color: #2E9E6A
// size: 640x480
//
// CPU load history, memory, storage, network and the hardware/driver list.

fn init() {
    #{ hist: [], n: 0, info: sys_info(), scroll: 0 }
}

fn tick() {
    this.n += 1;
    if this.n % 10 == 1 {
        this.info = sys_info();
        this.hist.push(this.info.cpu);
        if this.hist.len() > 50 { this.hist.remove(0); }
    }
}

fn scroll(n) {
    this.scroll = max(0, this.scroll + n);
}

fn fmt_bytes(b) {
    if b >= 1073741824 { return `${b / 1073741824}.${(b % 1073741824) * 10 / 1073741824} GiB`; }
    if b >= 1048576 { return `${b / 1048576} MiB`; }
    if b >= 1024 { return `${b / 1024} KiB`; }
    `${b} B`
}

fn card(g, x, y, w, h, title) {
    g.round_rect(x, y, w, h, 10, 0x262838);
    g.text(x + 14, y + 10, title, 0x9A9CB0);
}

fn clip(s, n) {
    if s.len() <= n { s } else { s.sub_string(0, n - 3) + "..." }
}

fn draw(g) {
    let i = this.info;
    g.clear(0x1A1B26);

    // CPU with a load graph.
    card(g, 12, 12, 300, 176, "CPU");
    g.text_big(26, 34, `${i.cpu}%`, 0xFFFFFF);
    g.text(26, 74, clip(i.cpu_name, 34), 0x9A9CB0);
    let gx = 26; let gy = 176; let gh = 72; let gw = 272;
    g.frame(gx - 1, gy - gh - 1, gw + 2, gh + 2, 0x34364A);
    let n = this.hist.len();
    let bw = gw / 50;
    for k in 0..n {
        let v = this.hist[k];
        let bh = max(1, v * gh / 100);
        g.rect(gx + (50 - n + k) * bw, gy - bh, bw - 1, bh, if v > 80 { 0xEF5B6B } else { 0x4FC08D });
    }

    // Memory.
    card(g, 324, 12, 304, 84, "Memory");
    let pct = i.mem_used * 100 / max(1, i.mem_total);
    g.text(338, 38, `${fmt_bytes(i.mem_used)} of ${fmt_bytes(i.mem_total)} (${pct}%)`, 0xFFFFFF);
    g.progress(338, 66, 276, 12, pct, 0x8B7CF6);

    // Storage.
    card(g, 324, 104, 304, 84, "Storage");
    if i.disk_total != () {
        let dp = i.disk_used * 100 / max(1, i.disk_total);
        g.text(338, 130, `${fmt_bytes(i.disk_used)} of ${fmt_bytes(i.disk_total)} used`, 0xFFFFFF);
        g.progress(338, 158, 276, 12, dp, 0xE5A84B);
    } else {
        g.text(338, 130, "no filesystem", 0xFFFFFF);
    }

    // Network and system.
    card(g, 12, 196, 616, 72, "Network & system");
    g.text(26, 222, `IP ${i.ip}   gateway ${i.gateway}   rx ${fmt_bytes(i.net_rx)}   tx ${fmt_bytes(i.net_tx)}`, 0xFFFFFF);
    let up = i.uptime;
    g.text(26, 242, `uptime ${up / 3600}h ${(up / 60) % 60}m ${up % 60}s   tasks ${i.tasks}`, 0xCDD3DE);

    // Devices and drivers.
    card(g, 12, 276, 616, g.height - 288, "Hardware (PCI) and drivers");
    let y = 300;
    let rows = 0;
    for d in i.devices {
        if rows < this.scroll { rows += 1; continue; }
        if y > g.height - 30 { break; }
        let c = if d.driver == "missing" { 0xEF5B6B } else if d.driver == "built in" { 0x9A9CB0 } else if d.driver == "port drafted" { 0xE5C07B } else { 0x4FC08D };
        g.text(26, y, d.id, 0x9A9CB0);
        g.text(110, y, clip(d.name, 44), 0xFFFFFF);
        g.text(480, y, clip(d.driver, 17), c);
        y += 18;
        rows += 1;
    }
}
