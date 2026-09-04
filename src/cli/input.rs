//! Line input (M4.3 hardening, M4.9 paste aggregation): one entry
//! point for every prompt.
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
//! Multi-line paste (M4.9): a pasted 20-line snippet must become ONE
//! message, not one turn per line. Two mechanisms:
//! - **bracketed paste** (modern terminals): the paste arrives wrapped
//!   in `ESC[200~ … ESC[201~`; everything inside is literal content,
//!   newlines included;
//! - **zero-gap burst heuristic** (fallback for terminals without
//!   bracketed paste): a Enter followed by more input within
//!   `PASTE_GAP_MS` is a paste stream, not a human — the editor
//!   switches to paste mode until a short silence (`BURST_SILENCE_MS`).
//!
//! The buffer is therefore multi-line; on submit it is handed over as
//! a single string. Off-tty (pipes/tests) or off-unix it falls back to
//! plain `read_line`, preserving machine-driven behavior.

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

/// After an Enter, input arriving within this window marks a paste
/// burst (inter-line gap of a paste stream is <1ms; no human types
/// that fast).
#[cfg(unix)]
const PASTE_GAP_MS: i32 = 10;

/// A paste burst ends after this much silence (terminals may deliver
/// a long paste in several chunks).
#[cfg(unix)]
const BURST_SILENCE_MS: i32 = 40;

#[cfg(unix)]
fn read_line_raw() -> Line {
    let term = match RawGuard::new() {
        Ok(t) => t,
        Err(_) => return read_line_fallback(),
    };
    // Keep the guard alive until every return path below (Drop
    // restores the terminal).
    let _guard = term;

    let mut ed = Editor::default();
    let mut buf = [0u8; 256];
    // Heuristic paste in progress (terminal without bracketed paste).
    let mut burst = false;
    loop {
        let n = read_chunk(&mut buf);
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
        let mut i: usize = 0;
        while i < n as usize {
            let b = buf[i];
            i += 1;
            // Paste-burst detection (fallback path): an Enter followed
            // by more input within the gap window is a paste. Bytes
            // still buffered in this chunk count as "more".
            if (b == b'\r' || b == b'\n')
                && !ed.paste
                && ed.esc.is_none()
                && (i < n as usize || poll_readable(PASTE_GAP_MS))
            {
                ed.paste = true;
                burst = true;
            }
            match ed.feed(b) {
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
            }
        }
        // Heuristic burst: no bracketed terminator will arrive; end
        // the paste after a short silence. (If the terminal does send
        // brackets, ESC[201~ already turned paste mode off and this
        // branch never fires.)
        if burst && ed.paste && !poll_readable(BURST_SILENCE_MS) {
            ed.paste = false;
            burst = false;
        }
    }
}

/// Blocking read of up to `buf.len()` bytes; negative = -errno.
#[cfg(unix)]
fn read_chunk(buf: &mut [u8]) -> isize {
    unsafe { libc::read(0, buf.as_mut_ptr().cast(), buf.len()) }
}

/// Is more input already queued on fd 0? Waits up to `timeout_ms`.
#[cfg(unix)]
fn poll_readable(timeout_ms: i32) -> bool {
    let mut fds = libc::pollfd { fd: 0, events: libc::POLLIN, revents: 0 };
    let r = unsafe { libc::poll(&mut fds, 1, timeout_ms) };
    r > 0 && (fds.revents & libc::POLLIN) != 0
}

#[cfg(unix)]
fn errno_is_eintr() -> bool {
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR)
}

/// Byte-level state machine: bytes in, one action out. Pure enough to
/// unit-test (escape/paste markers included — no I/O happens here).
#[cfg(unix)]
#[derive(Default)]
struct Editor {
    line: String,
    asm: CharAssembler,
    warned_encoding: bool,
    /// Paste mode: newlines are content, not submit (bracketed paste
    /// or burst heuristic).
    paste: bool,
    /// Partial escape sequence (after ESC) awaiting its final byte.
    esc: Option<Vec<u8>>,
    /// Last paste byte was '\r' (to swallow the '\n' of a CRLF pair).
    paste_cr: bool,
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
}

#[cfg(unix)]
impl Editor {
    fn feed(&mut self, b: u8) -> Feed {
        // Escape sequence accumulation (CSI/SS3) — active in BOTH
        // modes; only the bracketed-paste markers change state
        // (end_escape), everything else is swallowed.
        if let Some(seq) = self.esc.as_mut() {
            seq.push(b);
            if Self::escape_complete(seq) {
                let done = self.esc.take().unwrap_or_default();
                self.end_escape(&done);
            }
            return Feed::Keep;
        }
        if self.paste {
            return self.feed_paste(b);
        }
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
            0x1b => {
                self.esc = Some(Vec::new());
                Feed::Keep
            }
            0x7f | 0x08 => {
                self.erase_last();
                Feed::Keep
            }
            0x00..=0x1f => Feed::Keep, // other control bytes: ignore
            _ => self.push_byte(b),
        }
    }

    /// Paste mode: newlines and tabs become content; only the
    /// bracketed-paste terminators (seen as escape sequences) or a
    /// submit outside paste mode end the line.
    fn feed_paste(&mut self, b: u8) -> Feed {
        if b != b'\r' && b != b'\n' {
            self.paste_cr = false;
        }
        match b {
            0x1b => {
                self.esc = Some(Vec::new());
            }
            b'\r' => {
                self.paste_cr = true;
                self.push_char('\n');
            }
            b'\n' => {
                if !self.paste_cr {
                    self.push_char('\n');
                }
                self.paste_cr = false;
            }
            b'\t' => self.push_char('\t'),
            0x7f | 0x08 => self.erase_last(),
            0x00..=0x1f => {} // other control bytes inside a paste: drop
            _ => {
                self.push_byte(b);
            }
        }
        Feed::Keep
    }

    /// Normal-mode and paste-mode payload bytes share the UTF-8
    /// assembler (mid-character bytes and undecodable input).
    fn push_byte(&mut self, b: u8) -> Feed {
        match self.asm.push(b) {
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
        }
    }

    fn push_char(&mut self, c: char) {
        self.line.push(c);
        print!("{c}");
        let _ = std::io::stdout().flush();
    }

    /// Remove the last char (the whole UTF-8 sequence) and wipe its
    /// full display width on screen — the fix for "need two backspaces
    /// per Chinese char". Erasing a newline joins the lines on screen
    /// too (cursor up + reposition), so backspacing through pasted
    /// multi-line input stays coherent.
    fn erase_last(&mut self) {
        let Some(c) = self.line.chars().next_back() else { return };
        self.line.pop();
        if c == '\n' {
            self.erase_newline();
            return;
        }
        let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(1).max(1);
        let bs = "\u{8}".repeat(w);
        print!("{bs}{}{bs}", " ".repeat(w));
        let _ = std::io::stdout().flush();
    }

    /// The cursor always sits at the end of the buffer, so the removed
    /// '\n' was followed by an empty last line: move up one row and to
    /// the end of the (now last) line. Wrapped long lines are handled
    /// by column math (col = width % term_width).
    fn erase_newline(&mut self) {
        let last = self.line.lines().last().unwrap_or("");
        let w = unicode_width::UnicodeWidthStr::width(last);
        let tw = crate::cli::render::term_width().max(1);
        let col = w % tw;
        print!("\x1b[1A\r\x1b[{col}C\x1b[0K");
        let _ = std::io::stdout().flush();
    }

    /// Is the accumulated escape sequence (bytes after ESC) complete?
    /// Per VT100: CSI = `[` + parameter bytes (0x30-3F) + intermediate
    /// bytes (0x20-2F) + one final byte (0x40-7E); SS3 = `O` + one
    /// final byte; any other byte right after ESC is Meta-key.
    fn escape_complete(seq: &[u8]) -> bool {
        match seq {
            [] | [b'['] | [b'O'] => false,
            [b'[', rest @ ..] => {
                let Some(last) = rest.last() else { return false };
                (0x40..=0x7E).contains(last)
                    && rest[..rest.len() - 1].iter().all(|c| (0x20..=0x3F).contains(c))
            }
            [b'O', f] => (0x40..=0x7E).contains(f),
            [_] => true,
            _ => true, // overly long: give up and swallow
        }
    }

    /// End of an escape sequence (bytes after ESC, final byte last).
    /// Only the bracketed-paste markers change state; everything else
    /// (arrows, SS3, stray CSI) is swallowed.
    fn end_escape(&mut self, seq: &[u8]) {
        match seq {
            b"[200~" => self.paste = true,
            b"[201~" => self.paste = false,
            _ => {}
        }
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
            // Input flags: ICRNL/INLCR/IGNCR off so a pasted CRLF
            // arrives verbatim (the editor normalizes it — with ICRNL
            // on, every '\r' became '\n' and pasted code grew double
            // newlines). IXON off so Ctrl-S/Q never freeze the editor.
            raw.c_iflag &= !(libc::IXON | libc::ICRNL | libc::INLCR | libc::IGNCR | libc::ISTRIP);
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

    fn feed_all(ed: &mut Editor, s: &str) -> Vec<Feed> {
        s.bytes().map(|b| ed.feed(b)).collect()
    }

    /// ESC [ 2 0 0 ~ … ESC [ 2 0 1 ~
    const BP_START: &[u8] = &[0x1b, b'[', b'2', b'0', b'0', b'~'];
    const BP_END: &[u8] = &[0x1b, b'[', b'2', b'0', b'1', b'~'];

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

    #[test]
    fn bracketed_paste_keeps_newlines_as_content() {
        let mut ed = Editor::default();
        for &b in BP_START {
            assert!(matches!(ed.feed(b), Feed::Keep));
        }
        assert!(ed.paste);
        let feeds = feed_all(&mut ed, "fn a() {\r\n    let x = 1;\r\n}\r\n");
        assert!(feeds.iter().all(|f| matches!(f, Feed::Keep)), "paste must not submit");
        assert_eq!(ed.line, "fn a() {\n    let x = 1;\n}\n");
        for &b in BP_END {
            assert!(matches!(ed.feed(b), Feed::Keep));
        }
        assert!(!ed.paste);
        assert!(matches!(ed.feed(b'\r'), Feed::Submit));
        // The REPL trims on submit; multi-line content survives.
        assert_eq!(ed.line.trim(), "fn a() {\n    let x = 1;\n}");
    }

    #[test]
    fn paste_crlf_is_one_newline() {
        let mut ed = Editor::default();
        ed.feed(0x1b);
        for &b in b"[200~" {
            ed.feed(b);
        }
        for b in "a\r\n\r\nb".bytes() {
            ed.feed(b);
        }
        assert_eq!(ed.line, "a\n\nb");
    }

    #[test]
    fn paste_keeps_tabs_and_lone_lf() {
        let mut ed = Editor::default();
        ed.feed(0x1b);
        for &b in b"[200~" {
            ed.feed(b);
        }
        for b in "\tx\ny".bytes() {
            ed.feed(b);
        }
        assert_eq!(ed.line, "\tx\ny");
    }

    #[test]
    fn arrow_keys_still_swallowed_and_do_not_toggle_paste() {
        let mut ed = Editor::default();
        // CSI arrows, with parameters (bracketed-paste markers) and
        // SS3 (ESC O P) all swallow whole — including bytes like '['
        // that a naive "final byte" check would misjudge.
        for seq in [
            [0x1b, b'[', b'A'].as_slice(),
            &[0x1b, b'[', b'C'][..],
            &[0x1b, b'O', b'P'][..],
        ] {
            for &b in seq {
                assert!(matches!(ed.feed(b), Feed::Keep));
            }
        }
        assert_eq!(ed.line, "");
        assert!(!ed.paste, "arrow-like sequences must not enter paste mode");
        // A lone ESC (Meta) swallows exactly one following byte, so
        // Meta-Enter is not a submit — documented readline-ish
        // behavior; a real Enter after Esc just needs a second press.
        ed.feed(0x1b);
        assert!(matches!(ed.feed(b'\r'), Feed::Keep));
        assert!(matches!(ed.feed(b'\r'), Feed::Submit));
    }

    #[test]
    fn multi_line_erase_joins_lines() {
        let mut ed = Editor::default();
        ed.feed(0x1b);
        for &b in b"[200~" {
            ed.feed(b);
        }
        for b in "ab\ncd".bytes() {
            ed.feed(b);
        }
        ed.erase_last(); // d
        ed.erase_last(); // c
        assert_eq!(ed.line, "ab\n");
        ed.erase_last(); // the newline itself
        assert_eq!(ed.line, "ab");
    }

    #[test]
    fn burst_paste_via_programmatic_mode_switch() {
        // The heuristic itself polls the fd (I/O, not unit-testable);
        // the state it drives is: paste on → newlines are content →
        // paste off → Enter submits.
        let mut ed = Editor::default();
        ed.paste = true;
        for b in "let x = 1;\r\nlet y = 2;\r\n".bytes() {
            assert!(matches!(ed.feed(b), Feed::Keep));
        }
        assert_eq!(ed.line, "let x = 1;\nlet y = 2;\n");
        ed.paste = false;
        assert!(matches!(ed.feed(b'\r'), Feed::Submit));
        assert_eq!(ed.line.trim(), "let x = 1;\nlet y = 2;");
    }
}
