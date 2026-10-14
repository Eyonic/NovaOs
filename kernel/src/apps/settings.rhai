// title: Settings
// icon: ☼
// color: #6B7280
// size: 600x420
//
// Claude account, network, installation and system information.

fn init() {
    #{ info: sys_info(), targets: install_targets(), live: is_live(), msg: "", n: 0, claude: claude_status() }
}

fn tick() {
    this.n += 1;
    if this.n % 20 == 0 {
        this.info = sys_info();
        this.claude = claude_status();
    }
}

fn click(x, y) {
    if y >= 70 && y < 98 && x >= 440 && x < 580 { launch_claude(); }
    if this.live {
        let ty = 214;
        for t in this.targets {
            if y >= ty && y < ty + 28 && x >= 440 && x < 580 {
                this.msg = `Installing to disk ${t.disk}...`;
                let r = install_to(t.disk);
                this.msg = if r == "ok" { "Installed. Restart and boot from the new disk." } else { `Install failed: ${r}` };
            }
            ty += 32;
        }
    }
}

fn section(g, y, title) {
    g.text(20, y, title, 0x8B7CF6);
    g.rect(20, y + 20, g.width - 40, 1, 0x34364A);
}

fn draw(g) {
    let i = this.info;
    g.clear(0x1A1B26);
    section(g, 16, "Claude");
    g.text(20, 46, this.claude, 0xE8E8F2);
    g.button(440, 70, 140, 28, "Open " + assistant_name(), 0xD97757);

    section(g, 110, "Network");
    g.text(20, 140, `address ${i.ip}   gateway ${i.gateway}`, 0xE8E8F2);

    section(g, 170, "Installation");
    if this.live {
        if this.targets.len() == 0 {
            g.text(20, 200, "Running live. No disk to install onto was found.", 0xE8E8F2);
        } else {
            g.text(20, 196, "Running live from the installation image.", 0xE8E8F2);
            let ty = 214;
            for t in this.targets {
                g.text(20, ty + 6, `disk ${t.disk}: ${t.name} (${t.mib} MiB)`, 0xCDD3DE);
                g.button(440, ty, 140, 28, "Install here", 0x2E9E6A);
                ty += 32;
            }
        }
    } else {
        g.text(20, 200, "Nova is installed on this disk.", 0xE8E8F2);
    }
    g.text(20, 300, this.msg, 0xFFD787);

    section(g, 330, "About");
    g.text(20, 360, `Nova 0.4   ${i.cpu_name}`, 0xE8E8F2);
    g.text(20, 380, `${i.mem_total / 1048576} MiB memory   ${i.devices.len()} PCI devices`, 0x9A9CB0);
}
