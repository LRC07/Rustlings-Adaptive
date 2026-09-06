//! Terminal rendering helpers (M4.2): the pure-function core the REPL
//! and pages repaint with — display-width aware wrapping, page
//! headers, chat-tail recaps, progress bars, and ANSI styling.
//!
//! Rules:
//! - ANSI output is emitted only on a real terminal without NO_COLOR;
//!   piped output (tests, logs, demo capture) stays plain and readable.
//! - Viewport clearing uses `ESC[2J` + `ESC[H` only: the scrollback
//!   buffer is preserved, so "clean screen" never means "lost history".
//!   `ESC[3J` (clear scrollback too) is reserved for the explicit
//!   `/clear all`.

use std::io::IsTerminal;
use std::sync::OnceLock;

use unicode_width::UnicodeWidthStr;

// ---------------------------------------------------------------------------
// ANSI gating
// ---------------------------------------------------------------------------

/// Runtime check (computed once): tty and no NO_COLOR.
pub(crate) fn ansi_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        ansi_enabled_for(std::io::stdout().is_terminal(), std::env::var_os("NO_COLOR").is_some())
    })
}

/// Pure decision (unit-testable): ANSI on = real terminal, opt-out unset.
fn ansi_enabled_for(is_tty: bool, no_color_set: bool) -> bool {
    is_tty && !no_color_set
}

fn paint(code: &str, text: &str) -> String {
    if ansi_enabled() {
        format!("\x1B[{code}m{text}\x1B[0m")
    } else {
        text.to_string()
    }
}

// ---------------------------------------------------------------------------
// Color semantics (M9a) — one vocabulary across all pages; pick from
// here for new call sites, no ad-hoc styling:
// - green  = success (✓, passed, generated)
// - red    = error / failure (✗, error codes, failed runs)
// - yellow = warning (⚠, constraint hits, due reviews, reminders)
// - cyan   = structure (page & card headers, section titles)
// - dim    = secondary (echoes, hints, footers, trivia)
// - bold   = the one key number / verdict of a block

pub(crate) fn dim(text: &str) -> String {
    paint("2", text)
}

pub(crate) fn bold(text: &str) -> String {
    paint("1", text)
}

pub(crate) fn cyan(text: &str) -> String {
    paint("36", text)
}

pub(crate) fn green(text: &str) -> String {
    paint("32", text)
}

pub(crate) fn red(text: &str) -> String {
    paint("31", text)
}

pub(crate) fn yellow(text: &str) -> String {
    paint("33", text)
}

/// Page/card header (M9a): `── title ─────…`, CJK-aware, padded to a
/// fixed rhythm (capped at the terminal width). One helper so every
/// page and card shares the same visual language.
pub(crate) fn header(title: &str) -> String {
    let total = term_width().min(72);
    let used = 3 + UnicodeWidthStr::width(title) + 1; // "── " + title + ' '
    let fill = total.saturating_sub(used).min(28);
    cyan(&format!("── {title} {}", "─".repeat(fill)))
}

/// Clear the viewport but keep scrollback (`2J` + home, never `3J`).
/// No-op off-tty.
pub(crate) fn clear_viewport() {
    if ansi_enabled() {
        print!("\x1B[2J\x1B[H");
        use std::io::Write;
        let _ = std::io::stdout().flush();
    }
}

/// `2J` + `3J` + home: the deep wipe behind `/clear all`.
pub(crate) fn clear_all() {
    if ansi_enabled() {
        print!("\x1B[2J\x1B[3J\x1B[H");
        use std::io::Write;
        let _ = std::io::stdout().flush();
    }
}

/// Terminal width via `TIOCGWINSZ`, clamped to a sane range; 100 when
/// unavailable (pipes, exotic platforms).
pub(crate) fn term_width() -> usize {
    term_width_ioctl().clamp(40, 200)
}

#[cfg(unix)]
fn term_width_ioctl() -> usize {
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(1, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0 {
            return ws.ws_col as usize;
        }
    }
    100
}

#[cfg(not(unix))]
fn term_width_ioctl() -> usize {
    100
}

// ---------------------------------------------------------------------------
// Width-aware wrapping
// ---------------------------------------------------------------------------

/// Wrap one line (no fence semantics). Greedy fill by display width;
/// break at the last space inside the window when the overflow char
/// is not itself a space, else hard-break. Unbreakable ASCII runs
/// longer than the width are hard-broken too.
pub(crate) fn wrap_line(line: &str, width: usize) -> Vec<String> {
    if width == 0 || line.width() <= width {
        return vec![line.to_string()];
    }
    let mut pieces = Vec::new();
    let mut current = String::new();
    let mut current_w = 0usize;
    for ch in line.chars() {
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if current_w + w > width && !current.is_empty() {
            // Prefer a space break: cut before the trailing "word".
            if let Some(pos) = current.rfind(' ').filter(|p| *p > 0 && !ch.is_whitespace()) {
                let head = current[..=pos].trim_end().to_string();
                let tail = current[pos + 1..].to_string();
                pieces.push(head);
                current = tail;
                current_w = current.width();
                current.push(ch);
                current_w += w;
                continue;
            }
            pieces.push(current.trim_end().to_string());
            current.clear();
            current_w = 0;
        }
        current.push(ch);
        current_w += w;
    }
    if !current.is_empty() {
        pieces.push(current.trim_end().to_string());
    }
    pieces.into_iter().filter(|p| !p.is_empty()).collect()
}

// ---------------------------------------------------------------------------
// Page furniture
// ---------------------------------------------------------------------------

/// One dim header line for the chat viewport.
pub(crate) fn chat_header(session_id: &str, model: &str, spent: f64, budget: Option<f64>) -> String {
    let budget_part = match budget {
        Some(b) => format!(" / 预算 ${b:.2}"),
        None => String::new(),
    };
    dim(&format!(
        "── {session_id} ｜ {model} ｜ 累计 ${spent:.4}{budget_part} ｜ /help 帮助 ──"
    ))
}

/// Compact recap of the last few conversation entries (dim): user
/// inputs and final coach replies only, tool traffic skipped, each
/// truncated. `(label, text)` pairs already truncated to `max_chars`.
pub(crate) fn chat_tail(
    messages: &[crate::llm::ChatMessage],
    max_entries: usize,
    max_chars: usize,
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for m in messages.iter().rev() {
        if out.len() >= max_entries {
            break;
        }
        match m.role.as_str() {
            "user" => {
                let t = m.content.as_deref().unwrap_or("").replace('\n', " ");
                out.push(("你".to_string(), ellipsize(&t, max_chars)));
            }
            "assistant" if !m.tool_calls.is_empty() => {
                for c in m.tool_calls.iter().rev() {
                    if out.len() >= max_entries {
                        break;
                    }
                    out.push(("工具".to_string(), format!("{}(…)", c.name)));
                }
            }
            "assistant" => {
                let t = m.content.as_deref().unwrap_or("").replace('\n', " ");
                if !t.is_empty() {
                    out.push(("教练".to_string(), ellipsize(&t, max_chars)));
                }
            }
            _ => {}
        }
    }
    out.reverse();
    out
}

/// `[████████░░░░] 5/12 (42%)` — `bar_width` display cells.
pub(crate) fn progress_bar(done: usize, total: usize, bar_width: usize) -> String {
    let total = total.max(1);
    let filled = ((done as f64 / total as f64) * bar_width as f64).round() as usize;
    let filled = filled.min(bar_width);
    let bar = format!("{}{}", "█".repeat(filled), "░".repeat(bar_width - filled));
    let pct = done * 100 / total;
    format!("[{bar}] {done}/{total} ({pct}%)")
}

/// Left-align `s` in `width` display cells (CJK-aware), so table
/// columns with mixed Chinese/ASCII line up.
pub(crate) fn pad_display(s: &str, width: usize) -> String {
    let w = s.width();
    if w >= width {
        return s.to_string();
    }
    format!("{s}{}", " ".repeat(width - w))
}

/// Truncate `s` to at most `max_cells` display cells, appending `…`
/// when anything was cut (CJK-aware). Used by the spinner so a long
/// status can never wrap and destroy the in-place redraw.
pub(crate) fn truncate_display(s: &str, max_cells: usize) -> String {
    if max_cells == 0 {
        return String::new();
    }
    if s.width() <= max_cells {
        return s.to_string();
    }
    let mut cells = 0usize;
    let mut out = String::new();
    for c in s.chars() {
        let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if cells + w > max_cells.saturating_sub(1) {
            break;
        }
        cells += w;
        out.push(c);
    }
    out.push('…');
    out
}

/// Nearest known command for a mistyped one (Damerau-ish Levenshtein
/// without transposition is enough here), only when close enough.
pub(crate) fn suggest_command(raw: &str, known: &[&str]) -> Option<String> {
    let raw = raw.trim_start_matches('/');
    let mut best: Option<(usize, &str)> = None;
    for k in known {
        let d = edit_distance(raw, k);
        if best.is_none() || d < best.unwrap().0 {
            best = Some((d, k));
        }
    }
    let (d, k) = best?;
    (d <= 2 && d < raw.len().max(k.len())).then(|| format!("/{k}"))
}

fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

fn ellipsize(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_string();
    }
    let head: String = text.chars().take(max_chars).collect();
    format!("{head}…（共 {count} 字）")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::ChatMessage;

    // --- wrap -----------------------------------------------------------

    #[test]
    fn truncate_display_is_cjk_aware() {
        assert_eq!(truncate_display("hello", 10), "hello");
        assert_eq!(truncate_display("hello", 4), "hel…");
        // CJK chars are 2 cells: 3 chars = 6 cells.
        assert_eq!(truncate_display("一二三四", 8), "一二三四");
        assert_eq!(truncate_display("一二三四", 7), "一二三…");
        assert_eq!(truncate_display("一二三四", 2), "…");
        assert_eq!(truncate_display("abc", 0), "");
        // The result always fits within the budget.
        for budget in 1..12 {
            assert!(truncate_display("生成练习：自由生成第1轮被拒", budget).width() <= budget);
        }
    }

    #[test]
    fn wraps_cjk_by_display_width() {
        // 10 CJK chars = 20 columns; width 8 → pieces of ≤8 columns.
        let line = "一二三四五六七八九十";
        let pieces = wrap_line(line, 8);
        assert!(pieces.len() >= 2);
        for p in &pieces {
            assert!(p.width() <= 8, "piece too wide: {p}");
        }
        assert_eq!(pieces.concat(), line);
    }

    #[test]
    fn mixed_cjk_ascii_wraps_cleanly() {
        let line = "错误 E0382 表示 moved value 的借用问题";
        for p in wrap_line(line, 12) {
            assert!(p.width() <= 12, "piece too wide: {p}");
        }
    }

    #[test]
    fn long_ascii_run_hard_breaks() {
        let pieces = wrap_line(&"a".repeat(30), 10);
        assert_eq!(pieces.len(), 3);
        assert!(pieces.iter().all(|p| p.width() <= 10));
    }

    #[test]
    fn short_line_passes_through() {
        assert_eq!(wrap_line("你好", 10), vec!["你好".to_string()]);
    }

    #[test]
    fn zero_width_is_safe() {
        assert_eq!(wrap_line("abc", 0), vec!["abc".to_string()]);
    }

    // --- tail / header / bar ---------------------------------------------

    fn sample_history() -> Vec<ChatMessage> {
        vec![
            ChatMessage::system("sys"),
            ChatMessage::user("第一个问题"),
            ChatMessage::assistant("第一个回答"),
            ChatMessage::assistant_with_calls(vec![crate::llm::ToolCall {
                id: "c1".into(),
                name: "check_code".into(),
                arguments: "{}".into(),
            }]),
            ChatMessage::tool_result("c1", "r"),
            ChatMessage::user(format!("第二个问题 {}", "长".repeat(100))),
            ChatMessage::assistant("第二个回答"),
        ]
    }

    #[test]
    fn chat_tail_keeps_latest_entries_skips_tool_results() {
        let tail = chat_tail(&sample_history(), 3, 20);
        assert_eq!(tail.len(), 3);
        // Chronological order after the reverse; raw tool results are
        // skipped but the assistant tool-call line is kept.
        assert_eq!(tail[0].0, "工具");
        assert_eq!(tail[1].0, "你");
        assert!(tail[1].1.contains("…（共")); // long line truncated
        assert_eq!(tail[2].0, "教练");
        assert_eq!(tail[2].1, "第二个回答");
    }

    #[test]
    fn chat_tail_can_include_tool_calls() {
        let tail = chat_tail(&sample_history(), 5, 20);
        assert!(tail.iter().any(|(who, _)| who == "工具"));
    }

    #[test]
    fn progress_bar_segments_and_percent() {
        assert_eq!(progress_bar(0, 12, 6), "[░░░░░░] 0/12 (0%)");
        assert_eq!(progress_bar(6, 12, 6), "[███░░░] 6/12 (50%)");
        assert_eq!(progress_bar(12, 12, 6), "[██████] 12/12 (100%)");
        assert_eq!(progress_bar(3, 0, 4), "[████] 3/1 (300%)"); // degenerate total, no panic
    }

    #[test]
    fn pad_display_is_cjk_aware() {
        assert_eq!(pad_display("ab", 5), "ab   ");
        assert_eq!(pad_display("中文", 6), "中文  ");
        assert_eq!(pad_display("toolong", 4), "toolong");
        assert_eq!(pad_display("", 3), "   ");
    }

    #[test]
    fn header_carries_budget_when_set() {
        let h = chat_header("s1", "m1", 0.0100, Some(5.0));
        assert!(h.contains("$0.0100"));
        assert!(h.contains("预算 $5.00"));
        let h2 = chat_header("s1", "m1", 1.0, None);
        assert!(!h2.contains("预算"));
    }

    // --- suggestions -------------------------------------------------------

    #[test]
    fn suggests_near_misses() {
        let known = ["new", "practice", "generate", "usage", "config", "sessions", "help", "exit"];
        assert_eq!(suggest_command("/sesions", &known).as_deref(), Some("/sessions"));
        assert_eq!(suggest_command("/hel", &known).as_deref(), Some("/help"));
        assert_eq!(suggest_command("/totally-off", &known), None);
        assert_eq!(suggest_command("/exit", &["exit", "new"]).as_deref(), Some("/exit")); // exact match
    }

    // --- ansi gate ----------------------------------------------------------

    #[test]
    fn ansi_gate_matrix() {
        assert!(ansi_enabled_for(true, false));
        assert!(!ansi_enabled_for(true, true)); // NO_COLOR
        assert!(!ansi_enabled_for(false, false)); // piped
    }
}
