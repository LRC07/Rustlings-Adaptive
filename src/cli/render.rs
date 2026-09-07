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

/// ANSI-aware variant of `wrap_line` for panel bodies: SGR escape
/// sequences are zero-width (never counted, never split mid-sequence)
/// and a piece left with an open style gets an explicit reset so the
/// colour cannot bleed into the following rows. Used by `panel`; the
/// chat path keeps `wrap_line` (fences render plain).
pub(crate) fn wrap_line_ansi(line: &str, width: usize) -> Vec<String> {
    if width == 0 || visible_width(line) <= width {
        return vec![line.to_string()];
    }
    let mut pieces = Vec::new();
    let mut current = String::new();
    let mut current_w = 0usize;
    let mut in_esc = false;
    for ch in line.chars() {
        if in_esc {
            current.push(ch);
            if ch == 'm' {
                in_esc = false;
            }
            continue;
        }
        if ch == '\x1b' {
            in_esc = true;
            current.push(ch);
            continue;
        }
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if current_w + w > width && !current.is_empty() {
            if let Some(pos) = current.rfind(' ').filter(|p| *p > 0 && !ch.is_whitespace()) {
                let head = current[..=pos].trim_end().to_string();
                let tail = current[pos + 1..].to_string();
                push_closed(&mut pieces, head);
                current = tail;
                current_w = visible_width(&current);
            } else {
                let piece = current.trim_end().to_string();
                current.clear();
                current_w = 0;
                push_closed(&mut pieces, piece);
            }
        }
        current.push(ch);
        current_w += w;
    }
    let piece = current.trim_end().to_string();
    push_closed(&mut pieces, piece);
    pieces
}

/// Push a wrapped piece, closing an SGR style left open at the break
/// so it cannot bleed into the following rows.
fn push_closed(pieces: &mut Vec<String>, piece: String) {
    if piece.is_empty() {
        return;
    }
    if ends_with_open_sgr(&piece) {
        pieces.push(format!("{piece}\x1b[0m"));
    } else {
        pieces.push(piece);
    }
}

/// Whether `s` ends inside an unterminated (non-reset) SGR style.
/// Only our own CSI form `ESC [ params m` is produced here.
fn ends_with_open_sgr(s: &str) -> bool {
    let mut in_esc = false;
    let mut param_start = 0usize;
    let mut last_open = false;
    for (i, c) in s.char_indices() {
        if in_esc {
            if c == 'm' {
                in_esc = false;
                last_open = &s[param_start..i] != "0";
            }
            continue;
        }
        if c == '\x1b' {
            in_esc = true;
            param_start = i + 2; // skip "ESC ["
        }
    }
    last_open
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
        "── {session_id} ｜ {model} ｜ 累计 ${spent:.4}{budget_part} ｜ Ctrl+J 换行 ｜ /help 帮助 ｜ q 退出 ──"
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

/// Display width ignoring ANSI SGR sequences (`ESC [ … m`), so columns
/// padded from colored cells stay aligned. Plain text is unaffected.
pub(crate) fn visible_width(s: &str) -> usize {
    let mut w = 0usize;
    let mut in_esc = false;
    for c in s.chars() {
        if in_esc {
            if c == 'm' {
                in_esc = false;
            }
        } else if c == '\x1b' {
            in_esc = true;
        } else {
            w += unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        }
    }
    w
}

/// Left-align `s` in `width` display cells (CJK-aware, ANSI-aware), so
/// table columns with mixed Chinese/ASCII and colored cells line up.
pub(crate) fn pad_display(s: &str, width: usize) -> String {
    let w = visible_width(s);
    if w >= width {
        return s.to_string();
    }
    format!("{s}{}", " ".repeat(width - w))
}

/// Truncate `s` to at most `max_cells` display cells, appending `…`
/// when anything was cut (CJK- and ANSI-aware: escape sequences don't
/// count as width, a cut mid-sequence is dropped, and a still-active
/// SGR style gets an explicit reset). Used by the spinner so a long
/// status can never wrap and destroy the in-place redraw.
pub(crate) fn truncate_display(s: &str, max_cells: usize) -> String {
    if max_cells == 0 {
        return String::new();
    }
    if visible_width(s) <= max_cells {
        return s.to_string();
    }
    let mut cells = 0usize;
    let mut out = String::new();
    let mut in_esc = false;
    let mut esc_start: Option<usize> = None;
    let mut sgr_active = false;
    for c in s.chars() {
        if in_esc {
            out.push(c);
            if c == 'm' {
                in_esc = false;
                sgr_active = true;
                esc_start = None;
            }
            continue;
        }
        if c == '\x1b' {
            in_esc = true;
            esc_start = Some(out.len());
            out.push(c);
            continue;
        }
        let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if cells + w > max_cells.saturating_sub(1) {
            break;
        }
        cells += w;
        out.push(c);
    }
    // A cut inside an escape sequence would leave broken bytes behind.
    if let Some(start) = esc_start {
        out.truncate(start);
    }
    if sgr_active {
        out.push_str("\x1b[0m");
    }
    out.push('…');
    out
}

// ---------------------------------------------------------------------------
// Panels (M9b, docs/UI_设计规划_v1.md): boxed blocks for fixed-content
// pages (title cards, the debrief "theatre"). Deliberately NOT used for
// the scrolling chat — a framed chat area needs self-managed scrolling
// and fights the terminal's native scrollback (M4.2 decision).
//
// Shape: double-line frame + an optional key/value summary zone (the
// design doc's "sidebar", realized as a header grid — side columns
// fight terminal reflow) + titled sections.
// ---------------------------------------------------------------------------

/// One titled section inside a panel.
pub(crate) struct PanelSection {
    pub title: String,
    pub lines: Vec<String>,
}

/// Total width of a panel: clamp the terminal to a readable band.
pub(crate) fn panel_width() -> usize {
    term_width().clamp(46, 96)
}

/// Render a double-line panel as printable rows. Pure (no I/O): all
/// rows are exactly `panel_width()` display cells wide (ANSI-aware);
/// long body lines wrap (ANSI-aware) instead of being truncated — the
/// debrief theatre's LLM analysis must stay readable (9.6 实测), and
/// titles are centered in their bars.
pub(crate) fn panel(title: &str, summary: &[(String, String)], sections: &[PanelSection]) -> Vec<String> {
    let width = panel_width();
    let content_w = width - 4; // "║ " + body + " ║"
    let mut rows = Vec::new();

    // Overlong titles are truncated (with the … marker) so the bar math
    // below always has room for its `═` filler.
    let clip_title = |t: &str| {
        let max_tw = width.saturating_sub(8);
        if visible_width(t) > max_tw {
            truncate_display(t, max_tw)
        } else {
            t.to_string()
        }
    };
    let centered = |t: &str| {
        let total = width.saturating_sub(visible_width(t) + 6);
        ("═".repeat(total / 2), "═".repeat(total - total / 2))
    };

    let title = clip_title(title);
    let (l, r) = centered(&title);
    rows.push(format!("╔{l}═ {title} ═{r}╗"));

    if !summary.is_empty() {
        let kw = summary.iter().map(|(k, _)| k.width()).max().unwrap_or(0);
        for (k, v) in summary {
            let v_budget = content_w.saturating_sub(kw + 2);
            for (i, piece) in wrap_line(v, v_budget.max(8)).into_iter().enumerate() {
                let head = if i == 0 {
                    format!("{}  ", pad_display(k, kw))
                } else {
                    " ".repeat(kw + 2)
                };
                rows.push(format!("║ {} ║", pad_display(&format!("{head}{piece}"), content_w)));
            }
        }
    }

    for (n, sec) in sections.iter().enumerate() {
        if n > 0 || !summary.is_empty() {
            rows.push(format!("║{}║", " ".repeat(width - 2)));
        }
        let stitle = clip_title(&sec.title);
        let (l, r) = centered(&stitle);
        rows.push(format!("╠{l}═ {stitle} ═{r}╣"));
        for line in &sec.lines {
            for piece in wrap_line_ansi(line, content_w) {
                rows.push(format!("║ {} ║", pad_display(&piece, content_w)));
            }
        }
    }

    rows.push(format!("╚{}╝", "═".repeat(width - 2)));
    rows
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

    // --- panels ---------------------------------------------------------

    fn strip_ansi(s: &str) -> String {
        let mut out = String::new();
        let mut in_esc = false;
        for c in s.chars() {
            if in_esc {
                if c == 'm' {
                    in_esc = false;
                }
            } else if c == '\x1b' {
                in_esc = true;
            } else {
                out.push(c);
            }
        }
        out
    }

    #[test]
    fn panel_rows_are_uniform_and_ansi_aware() {
        let secs = vec![
            PanelSection {
                title: "机器实测".into(),
                lines: vec![
                    format!("{}  12", pad_display("有效行数", 10)),
                    // Hard-coded SGR (tests run with ANSI off): proves
                    // escape sequences don't break column math.
                    format!("约束满足  \x1b[32m✓\x1b[0m"),
                ],
            },
            PanelSection { title: "下一步".into(), lines: vec!["下一概念".into()] },
        ];
        let rows = panel(
            "复盘剧场 · 《测试题》",
            &[("判定".into(), "通过·写法地道".into()), ("解释校核".into(), "✓ 命中".into())],
            &secs,
        );
        let w = panel_width();
        for r in &rows {
            assert_eq!(visible_width(r), w, "row not uniform: {r:?}");
        }
        assert!(rows[0].starts_with('╔') && rows[0].ends_with('╗'));
        assert!(rows.last().unwrap().starts_with('╚'));
        // Plain rows are untouched by ANSI logic.
        assert!(strip_ansi(&rows[1]).contains("判定"));
        // Colored body lines keep their escape sequences but still fit.
        let colored = rows.iter().find(|r| r.contains("\x1b[32m")).expect("colored row kept");
        assert_eq!(visible_width(colored), w);
    }

    #[test]
    fn panel_wraps_overlong_lines_and_summary() {
        let long = "很长很长的评语".repeat(30);
        let secs = vec![PanelSection { title: "四维".into(), lines: vec![long.clone()] }];
        let rows = panel("题", &[], &secs);
        let w = panel_width();
        for r in &rows {
            assert_eq!(visible_width(r), w);
        }
        // Body lines WRAP instead of being truncated: the full text
        // survives across rows and no … marker appears (9.6 实测: 复盘
        // 分析被截断不可接受).
        let joined: String = rows
            .iter()
            .map(|r| strip_ansi(r).replace(['║', ' '], ""))
            .collect();
        assert!(joined.contains(&long), "full analysis must survive wrapping");
        assert!(!rows.iter().any(|r| r.contains('…')), "no truncation in body: {rows:?}");
        let body_rows = rows.len() - 2; // minus top/bottom bars
        assert!(body_rows >= 2, "long line needs several rows: {rows:?}");

        // Summary values longer than the budget wrap with a hanging indent.
        let kv = vec![("触发".into(), "长".repeat(80))];
        let rows = panel("卡", &kv, &[]);
        let w = panel_width();
        assert!(rows.len() >= 4, "wrapped summary needs several rows: {rows:?}");
        for r in &rows {
            assert_eq!(visible_width(r), w);
        }
    }

    #[test]
    fn panel_titles_are_centered() {
        // "╔═══ title ═══╗" → the part between the border chars.
        let inner_of = |bar: &str| {
            let cs: Vec<char> = bar.chars().collect();
            cs[1..cs.len() - 1].iter().collect::<String>()
        };
        let rows = panel("复盘剧场", &[], &[]);
        let top = &rows[0];
        assert!(top.starts_with('╔') && top.ends_with('╗'));
        let inner = inner_of(top);
        let pos = inner.find("复盘剧场").expect("title present");
        let count_bar = |s: &str| s.chars().filter(|c| *c == '═').count();
        let left = count_bar(&inner[..pos]);
        let right = count_bar(&inner[pos + "复盘剧场".len()..]);
        assert!(left.abs_diff(right) <= 1, "title not centered: {top:?}");
        // The `═ title ═` separators hug the title on both sides.
        assert!(inner[..pos].ends_with("═ ") && inner[pos + "复盘剧场".len()..].starts_with(" ═"));
        // Section title bars are centered too.
        let secs = vec![PanelSection { title: "机器实测".into(), lines: vec!["x".into()] }];
        let rows = panel("题", &[], &secs);
        let bar = rows.iter().find(|r| r.contains("机器实测")).expect("section bar");
        let inner = inner_of(bar);
        let pos = inner.find("机器实测").unwrap();
        let left = count_bar(&inner[..pos]);
        let right = count_bar(&inner[pos + "机器实测".len()..]);
        assert!(left.abs_diff(right) <= 1, "section title not centered: {bar:?}");
    }

    #[test]
    fn wrap_line_ansi_skips_escape_width_and_closes_open_styles() {
        // Plain long text wraps like wrap_line would by visible width.
        let line = "一二三四五六七八九十".repeat(3);
        let pieces = wrap_line_ansi(&line, 8);
        assert!(pieces.iter().all(|p| visible_width(p) <= 8));
        assert_eq!(pieces.concat(), line);
        // A leading SGR is zero-width: the same text fits per piece.
        let colored = format!("\x1b[31m{line}\x1b[0m");
        let pieces = wrap_line_ansi(&colored, 8);
        assert!(pieces.iter().all(|p| visible_width(p) <= 8));
        assert_eq!(strip_ansi(&pieces.concat()), line);
        // A break inside an open style closes it on that piece.
        let open = format!("\x1b[1m{}{}", "字".repeat(10), "尾");
        let pieces = wrap_line_ansi(&open, 6);
        assert!(pieces.len() >= 2);
        assert!(pieces[0].ends_with("\x1b[0m"), "open style closed: {:?}", pieces[0]);
    }

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
