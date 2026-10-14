//! Line editor shared by the shell and the Claude client.
//!
//! Editing happens at the end of the line only: Backspace, Ctrl-U (erase line),
//! Ctrl-C (cancel), Ctrl-L (clear screen), Ctrl-D on an empty line (end of input),
//! Tab (completion callback), Up/Down (history).

use alloc::string::String;
use alloc::vec::Vec;

use crate::input::{self, Key};
use crate::{print, println};

pub struct History {
    lines: Vec<String>,
    max: usize,
}

impl History {
    pub const fn new(max: usize) -> Self {
        Self { lines: Vec::new(), max }
    }

    /// Remember a line (not empty lines, not an immediate repeat).
    pub fn add(&mut self, line: &str) {
        if line.trim().is_empty() || self.lines.last().is_some_and(|l| l == line) {
            return;
        }
        if self.lines.len() == self.max {
            self.lines.remove(0);
        }
        self.lines.push(line.into());
    }

    pub fn lines(&self) -> &[String] {
        &self.lines
    }
}

pub enum ReadResult {
    Line(String),
    /// Ctrl-C.
    Cancelled,
    /// Ctrl-D on an empty line, or the terminal was closed.
    Eof,
}

pub struct Options<'a> {
    pub prompt: &'a str,
    pub max_len: usize,
    /// Echo '*' instead of the typed characters (passwords, API keys).
    pub masked: bool,
    /// Tab: given the line so far, the text to append (if any).
    pub complete: Option<&'a dyn Fn(&str) -> Option<String>>,
}

fn erase(n: usize) {
    for _ in 0..n {
        print!("\x08 \x08");
    }
}

/// Print the prompt and read one line.
pub fn read_line(opts: &Options, history: Option<&History>) -> ReadResult {
    print!("{}", opts.prompt);
    let mut line = String::new();
    let shown = |l: &str| if opts.masked { "*".repeat(l.len()) } else { l.into() };
    let empty = Vec::new();
    let past = history.map_or(&empty, |h| &h.lines);
    // Index into history while browsing; == past.len() means "the line being typed".
    let mut browse = past.len();
    let mut draft = String::new();
    loop {
        match input::read_key() {
            Key::Char(b'\n') => {
                println!();
                return ReadResult::Line(line);
            }
            Key::Char(0x08) => {
                if line.pop().is_some() {
                    erase(1);
                }
            }
            Key::Char(0x03) => {
                println!("^C");
                return ReadResult::Cancelled;
            }
            Key::Char(0x04) if line.is_empty() => {
                println!();
                return ReadResult::Eof;
            }
            Key::Char(0x15) => {
                erase(line.len());
                line.clear();
            }
            Key::Char(0x0C) => print!("\x1b[2J\x1b[H{}{}", opts.prompt, shown(&line)),
            Key::Char(b'\t') => {
                if let Some(extra) = opts.complete.and_then(|f| f(&line)) {
                    let extra: String = extra.chars().take(opts.max_len.saturating_sub(line.len())).collect();
                    print!("{}", shown(&extra));
                    line.push_str(&extra);
                }
            }
            Key::Char(c) if (c == b' ' || c.is_ascii_graphic()) && line.len() < opts.max_len => {
                line.push(c as char);
                print!("{}", if opts.masked { '*' } else { c as char });
            }
            Key::Up if browse > 0 && !opts.masked => {
                if browse == past.len() {
                    draft = line.clone();
                }
                browse -= 1;
                erase(line.len());
                line = past[browse].clone();
                print!("{line}");
            }
            Key::Down if browse < past.len() && !opts.masked => {
                browse += 1;
                erase(line.len());
                line = if browse == past.len() { draft.clone() } else { past[browse].clone() };
                print!("{line}");
            }
            _ => {}
        }
    }
}
