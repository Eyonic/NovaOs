// title: Files
// icon: ≡
// color: #3D7DD8
// size: 620x440
//
// Browse the Nova filesystem: open apps and text files, create notes, delete files.

fn init() {
    let s = #{ files: [], sel: -1, scroll: 0, msg: "", confirm: false };
    s.files = fs_list("");
    s
}

fn refresh() {
    this.files = fs_list("");
    if this.sel >= this.files.len() { this.sel = -1; }
}

fn tick() {
    if this.n == () { this.n = 0; }
    this.n += 1;
    if this.n % 20 == 0 { this.refresh(); }
}

fn scroll(n) {
    this.scroll = max(0, min(this.scroll + n * 3, max(0, this.files.len() - 18)));
}

fn selected() {
    if this.sel < 0 || this.sel >= this.files.len() { return ""; }
    this.files[this.sel].name
}

fn open_path(p) {
    if p.ends_with(".rhai") {
        launch(p);
    } else {
        fs_write("system/editor-open", p);
        launch("system/apps/editor.rhai");
    }
}

fn click(x, y) {
    let g_h = 440;
    if y >= 52 && y < 52 + 18 * 19 {
        let i = (y - 52) / 19 + this.scroll;
        if i < this.files.len() {
            if this.sel == i && this.last_click != () && ticks() - this.last_click < 400 {
                this.open_path(this.files[i].name);
            }
            this.sel = i;
            this.last_click = ticks();
            this.confirm = false;
        }
        return;
    }
    if y >= 400 {
        let p = this.selected();
        if x >= 12 && x < 102 {
            if p != "" { this.open_path(p); }
        } else if x >= 110 && x < 200 {
            if p != "" { fs_write("system/editor-open", p); launch("system/apps/editor.rhai"); }
        } else if x >= 208 && x < 318 {
            let name = `notes/note-${ticks()}.txt`;
            fs_write(name, "");
            fs_write("system/editor-open", name);
            launch("system/apps/editor.rhai");
            this.refresh();
        } else if x >= 326 && x < 416 {
            if p != "" {
                if this.confirm {
                    fs_delete(p);
                    this.msg = `deleted ${p}`;
                    this.confirm = false;
                    this.refresh();
                } else {
                    this.confirm = true;
                }
            }
        }
    }
}

fn fmt_size(b) {
    if b >= 1048576 { return `${b / 1048576} MiB`; }
    if b >= 1024 { return `${b / 1024} KiB`; }
    `${b} B`
}

fn draw(g) {
    g.clear(0x1A1B26);
    g.label_bold(12, 10, "Nova filesystem", 0xFFFFFF, 17);
    g.label(150, 12, `${this.files.len()} files`, 0x9A9CB0);
    g.rect(0, 40, g.width, 1, 0x34364A);
    g.label(16, 44 - 2, "", 0);
    let y = 52;
    let shown = 0;
    for i in this.scroll..this.files.len() {
        if shown >= 18 { break; }
        let f = this.files[i];
        if i == this.sel { g.round_rect(6, y - 2, g.width - 12, 19, 4, 0x3A3D58); }
        let icon = if f.name.ends_with(".rhai") { "♦" } else if f.name.starts_with("system/") { "■" } else { "≡" };
        g.label(14, y, icon, if f.name.ends_with(".rhai") { 0x8B7CF6 } else { 0x9A9CB0 });
        g.label(34, y, f.name, 0xE8E8F2);
        g.label(g.width - 100, y, fmt_size(f.size), 0x9A9CB0);
        y += 19;
        shown += 1;
    }
    g.rect(0, 392, g.width, 1, 0x34364A);
    g.button(12, 400, 90, 28, "Open");
    g.button(110, 400, 90, 28, "Edit", 0x3E4157);
    g.button(208, 400, 110, 28, "New note", 0x3E4157);
    g.button(326, 400, 90, 28, if this.confirm { "Sure?" } else { "Delete" }, if this.confirm { 0xC2364A } else { 0x3E4157 });
    g.label(430, 406, this.msg, 0x9A9CB0);
}
