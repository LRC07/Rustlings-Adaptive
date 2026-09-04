//! Line input (M4.3 hardening): one entry point for every prompt.
//!
//! On a unix tty this is a minimal raw-mode line editor:
//! - backspace erases by *display width* (a CJK char dies in one press),
//! - ESC/arrow sequences are swallowed (no garbage insertion),
//! - bytes that cannot be decoded become U+FFFD with a one-time hint —
//!   a non-UTF-8 terminal never crashes or silently exits the REPL
//!   (this was the reported "typed a Chinese question and the program
//!   exited" bug: `read_line` returned Err(InvalidData) and the old
//!   code treated it as EOF),
//! - Ctrl-C cancels the current line, Ctrl-D on an empty line ends
//!   input (EOF).
//!
//! Off-tty (pipes/tests) or off-unix it falls back to plain
//! `read_line`, preserving machine-driven behavior.

use std::io::{IsTerminal, Write};

/// Result of one input read.
pub(crate) enum Line {
    Text(String),
    /// Ctrl-C: cancel the current input, stay in the loop.
    Interrupted,
    /// Ctrl-D on an empty line / stream end.
    Eof,
}

pub(crate) fn read_line(prompt: &str) -> Line {
    print!("{prompt}");
    let _ = std::io::stdout().flush();
    let tty = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    #[cfg(unix)]
    if tty {
        return read_line_raw();
    }
    read_line_fallback()
}

fn read_line_fallback() -> Line {
    let mut s = String::new();
    match std::io::stdin().read_line(&mut s) {
        Ok(0) => Line::Eof,
        // Decode errors (non-UTF-8 terminal): keep the REPL alive with
        // an empty line + a hint instead of silently "exiting".
        Err(e) => {
            eprintln!("  提示：读取输入失败（{e}），该行已忽略；终端编码可能不是 UTF-8。");
            Line::Text(String::new())
        }
        Ok(_) => Line::Text(s.trim().to_string()),
    }
}

// ---------------------------------------------------------------------------
// Raw-mode editor (unix tty)
// ---------------------------------------------------------------------------

#[cfg(unix)]
fn read_line_raw() -> Line {
    let term = match RawGuard::new() {
        Ok(t) => t,
        Err(_) => return read_line_fallback(),
    };
    let mut ed = Editor::default();
    loop {
        let mut byte = [0u8; 1];
        let n = unsafe { libc::read(0, byte.as_mut_ptr().cast(), 1) };
        if n <= 0 {
            // With ISIG on, Ctrl-C manifests as EINTR here: cancel the
            // current line (the interrupt flag is set by the handler,
            // the loop top reports it). Other signals like SIGWINCH
            // also EINTR — losing the partial line there is acceptable.
            if n < 0 && errno_is_eintr() {
                println!();
                return Line::Interrupted;
            }
            return Line::Eof;
        }
        match ed.feed(byte[0]) {
            Feed::Keep => {}
            Feed::Submit => {
                println!();
                return Line::Text(ed.line.trim().to_string());
            }
            Feed::Interrupted => {
                println!();
                return Line::Interrupted;
            }
            Feed::Eof => {
                println!();
                return Line::Eof;
            }
            Feed::Escaped => {
                // Swallow the rest of a CSI/SS3 arrow sequence: read
                // until a final byte (0x40..=0x7E), never echoing.
                loop {
                    let mut b2 = [0u8; 1];
                    let n = unsafe { libc::read(0, b2.as_mut_ptr().cast(), 1) };
                    if n <= 0 {
                        return Line::Eof;
                    }
                    term.touch();
                    if (0x40..=0x7E).contains(&b2[0]) {
                        break;
                    }
                }
            }
        }
    }
}

#[cfg(unix)]
fn errno_is_eintr() -> bool {
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR)
}

/// Byte-level state machine: bytes in, one action out. Pure enough to
/// unit-test (everything except the escape swallow, which blocks).
#[cfg(unix)]
#[derive(Default)]
struct Editor {
    line: String,
    asm: CharAssembler,
    warned_encoding: bool,
}

/// Byte→char accumulator: pure state machine (unit-testable), no I/O.
#[cfg(unix)]
#[derive(Default)]
struct CharAssembler {
    buf: Vec<u8>,
    need: usize,
}

#[cfg(unix)]
impl CharAssembler {
    /// Push one byte: `None` = need more; `Some(Ok)` = one decoded
    /// char; `Some(Err)` = undecodable sequence (dropped).
    fn push(&mut self, b: u8) -> Option<Result<char, ()>> {
        if self.need == 0 {
            let len = utf8_len(b);
            match len {
                1 => return Some(Ok(b as char)),
                0 => return Some(Err(())),
                _ => {
                    self.need = len;
                    self.buf = vec![b];
                    return None;
                }
            }
        }
        self.buf.push(b);
        if self.buf.len() < self.need {
            return None;
        }
        self.need = 0;
        let bytes = std::mem::take(&mut self.buf);
        match std::str::from_utf8(&bytes) {
            Ok(s) => Some(Ok(s.chars().next().unwrap_or('\u{FFFD}'))),
            Err(_) => Some(Err(())),
        }
    }
}

#[cfg(unix)]
enum Feed {
    Keep,
    Submit,
    Interrupted,
    Eof,
    Escaped,
}

#[cfg(unix)]
impl Editor {
    fn feed(&mut self, b: u8) -> Feed {
        match b {
            b'\r' | b'\n' => Feed::Submit,
            0x03 => Feed::Interrupted, // Ctrl-C
            0x04 => {
                if self.line.is_empty() {
                    Feed::Eof // Ctrl-D on empty line
                } else {
                    Feed::Keep // ignore mid-line (forward-delete semantics vary)
                }
            }
            0x1b => Feed::Escaped,
            0x7f | 0x08 => {
                self.erase_last();
                Feed::Keep
            }
            0x00..=0x1f => Feed::Keep, // other control bytes: ignore
            _ => match self.asm.push(b) {
                None => Feed::Keep, // mid-character: need more bytes
                Some(Ok(c)) => {
                    self.push_char(c);
                    Feed::Keep
                }
                Some(Err(())) => {
                    // Undecodable bytes become U+FFFD plus a one-time
                    // hint; the line survives — a non-UTF-8 terminal
                    // must not kill the REPL.
                    self.push_char('\u{FFFD}');
                    self.warn_encoding();
                    Feed::Keep
                }
            },
        }
    }

    fn push_char(&mut self, c: char) {
        self.line.push(c);
        print!("{c}");
        let _ = std::io::stdout().flush();
    }

    /// Remove the last char (the whole UTF-8 sequence) and wipe its
    /// full display width on screen — the fix for "need two backspaces
    /// per Chinese char".
    fn erase_last(&mut self) {
        let Some(c) = self.line.chars().next_back() else { return };
        self.line.pop();
        let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(1).max(1);
        let bs = "\u{8}".repeat(w);
        print!("{bs}{}{bs}", " ".repeat(w));
        let _ = std::io::stdout().flush();
    }

    fn warn_encoding(&mut self) {
        if !self.warned_encoding {
            self.warned_encoding = true;
            println!();
            println!("  提示：检测到无法解码的输入字节——你的终端编码可能不是 UTF-8。");
            println!("  该字符已按占位符保留；建议将终端编码切到 UTF-8（export LANG=C.UTF-8）。");
            print!("  当前输入回显> {}", self.line);
            let _ = std::io::stdout().flush();
        }
    }
}

/// UTF-8 lead-byte length; 0 = stray continuation / invalid lead.
fn utf8_len(b: u8) -> usize {
    match b {
        0x00..=0x7f => 1,
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => 0,
    }
}

/// RAII: enter raw mode on construction, restore on any exit path
/// (return, panic unwind) so the terminal is never left broken.
#[cfg(unix)]
struct RawGuard {
    saved: libc::termios,
}

#[cfg(unix)]
impl RawGuard {
    fn new() -> Result<Self, ()> {
        unsafe {
            let mut saved: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut saved) != 0 {
                return Err(());
            }
            let mut raw = saved;
            // ICANON off: bytes arrive immediately, we handle erase.
            // ECHO off: we echo ourselves (width-aware).
            // ISIG stays ON: Ctrl-C keeps raising SIGINT; the read is
            // interrupted (EINTR) and we report Line::Interrupted.
            raw.c_lflag &= !(libc::ICANON | libc::ECHO);
            raw.c_cc[libc::VMIN] = 1;
            raw.c_cc[libc::VTIME] = 0;
            if libc::tcsetattr(0, libc::TCSANOW, &raw) != 0 {
                return Err(());
            }
            Ok(Self { saved })
        }
    }

    /// No-op hook for the escape swallow loop (keeps the guard alive).
    fn touch(&self) {}
}

#[cfg(unix)]
impl Drop for RawGuard {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(0, libc::TCSANOW, &self.saved);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_lengths() {
        assert_eq!(utf8_len(b'a'), 1);
        assert_eq!(utf8_len(0xe4), 3); // 你
        assert_eq!(utf8_len(0xf0), 4); // emoji lead
        assert_eq!(utf8_len(0x80), 0); // stray continuation
        assert_eq!(utf8_len(0xff), 0); // invalid
    }

    #[test]
    fn control_flow_actions() {
        let mut ed = Editor::default();
        for b in "hi".bytes() {
            assert!(matches!(ed.feed(b), Feed::Keep));
        }
        assert_eq!(ed.line, "hi");
        assert!(matches!(ed.feed(b'\r'), Feed::Submit));
        assert!(matches!(ed.feed(0x03), Feed::Interrupted));
    }

    #[test]
    fn ctrl_d_on_empty_line_is_eof() {
        let mut ed = Editor::default();
        assert!(matches!(ed.feed(0x04), Feed::Eof));
    }

    #[test]
    fn ctrl_d_midline_is_ignored() {
        let mut ed = Editor::default();
        ed.feed(b'x');
        assert!(matches!(ed.feed(0x04), Feed::Keep));
    }

    #[test]
    fn invalid_bytes_become_replacement_not_exit() {
        let mut ed = Editor::default();
        // GBK "变量" = B1E4 C1BF — stray continuation bytes to UTF-8.
        for b in [0xb1, 0xe4, 0xc1, 0xbf] {
            ed.feed(b);
        }
        assert!(ed.line.contains('\u{FFFD}'), "line: {}", ed.line);
        assert!(ed.warned_encoding);
    }

    #[test]
    fn cjk_char_erases_in_one_press() {
        let mut ed = Editor::default();
        ed.feed(0xe4);
        ed.feed(0xbd);
        ed.feed(0xa0); // 你
        assert_eq!(ed.line, "你");
        ed.erase_last();
        assert_eq!(ed.line, "");
    }

    #[test]
    fn erase_ascii_and_cjk_mix() {
        let mut ed = Editor::default();
        for b in "a你b".bytes() {
            ed.feed(b);
        }
        assert_eq!(ed.line, "a你b");
        ed.erase_last(); // b
        assert_eq!(ed.line, "a你");
        ed.erase_last(); // 你
        assert_eq!(ed.line, "a");
    }
}
