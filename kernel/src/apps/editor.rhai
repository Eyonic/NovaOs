// title: Text Editor
// icon: ¶
// color: #B0569C
// size: 640x460
//
// A small text editor. Arrows move, Ctrl-S saves. Files opens documents here.

fn init() {
    let path = fs_read("system/editor-open");
    if path != "" { fs_delete("system/editor-open"); } else { path = "notes/untitled.txt"; }
    let text = fs_read(path);
    let lines = text.split("\n");
    if lines.len() == 0 { lines = [""]; }
    set_title(`${path} - Text Editor`);
    #{ path: path, lines: lines, row: 0, col: 0, top: 0, dirty: false, msg: "Ctrl-S to save", w: 640 }
}

fn save() {
    let text = "";
    for i in 0..this.lines.len() {
        if i > 0 { text += "\n"; }
        text += this.lines[i];
    }
    if fs_write(this.path, text) {
        this.dirty = false;
        this.msg = `saved ${this.path}`;
    } else {
        this.msg = "could not save!";
    }
}

fn key(k) {
    let line = this.lines[this.row];
    if k == "Ctrl-S" { this.save(); return; }
    if k == "Up" { if this.row > 0 { this.row -= 1; } }
    else if k == "Down" { if this.row + 1 < this.lines.len() { this.row += 1; } }
    else if k == "Left" {
        if this.col > 0 { this.col -= 1; } else if this.row > 0 { this.row -= 1; this.col = this.lines[this.row].len(); }
    } else if k == "Right" {
        if this.col < line.len() { this.col += 1; } else if this.row + 1 < this.lines.len() { this.row += 1; this.col = 0; }
    } else if k == "Enter" {
        let c = min(this.col, line.len());
        let rest = line.sub_string(c, line.len() - c);
        this.lines[this.row] = line.sub_string(0, c);
        this.lines.insert(this.row + 1, rest);
        this.row += 1; this.col = 0; this.dirty = true;
    } else if k == "Backspace" {
        let c = min(this.col, line.len());
        if c > 0 {
            this.lines[this.row] = line.sub_string(0, c - 1) + line.sub_string(c, line.len() - c);
            this.col = c - 1; this.dirty = true;
        } else if this.row > 0 {
            let prev = this.lines[this.row - 1];
            this.lines[this.row - 1] = prev + line;
            this.lines.remove(this.row);
            this.row -= 1; this.col = prev.len(); this.dirty = true;
        }
    } else if k == "Tab" {
        this.insert("    ");
    } else if k.len() == 1 {
        this.insert(k);
    }
    this.col = min(this.col, this.lines[this.row].len());
}

fn insert(s) {
    let line = this.lines[this.row];
    let c = min(this.col, line.len());
    this.lines[this.row] = line.sub_string(0, c) + s + line.sub_string(c, line.len() - c);
    this.col = c + s.len();
    this.dirty = true;
}

fn click(x, y) {
    if y < 30 && x > this.w - 90 { this.save(); return; }
    if y >= 36 {
        let r = this.top + (y - 36) / 18;
        if r < this.lines.len() {
            this.row = r;
            this.col = min(max(0, (x - 52) / 8), this.lines[r].len());
        }
    }
}

fn scroll(n) {
    this.top = max(0, min(this.top + n * 3, max(0, this.lines.len() - 5)));
}

fn draw(g) {
    this.w = g.width;
    g.clear(0x1A1B26);
    g.rect(0, 0, g.width, 30, 0x232536);
    g.label_bold(10, 6, this.path + if this.dirty { " *" } else { "" }, 0xE8E8F2);
    g.button(g.width - 90, 3, 80, 24, "Save");
    let visible = (g.height - 60) / 18;
    if this.row < this.top { this.top = this.row; }
    if this.row >= this.top + visible { this.top = this.row - visible + 1; }
    let y = 36;
    for r in this.top..min(this.lines.len(), this.top + visible) {
        g.text(6, y, `${r + 1}`, 0x5C6370);
        g.text(52, y, this.lines[r], 0xE8E8F2);
        if r == this.row {
            g.rect(52 + this.col * 8, y, 2, 16, 0x8B7CF6);
        }
        y += 18;
    }
    g.rect(0, g.height - 22, g.width, 22, 0x232536);
    g.label(10, g.height - 20, `line ${this.row + 1}, col ${this.col + 1}   ${this.msg}`, 0x9A9CB0);
}
