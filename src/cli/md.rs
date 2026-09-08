//! Minimal markdown → ANSI rendering for coach replies (M4.3).
//!
//! Scope is deliberately the subset the model actually emits in this
//! chat: fenced code blocks, `#` headings, `-`/`*`/ordered lists, `>`
//! quotes, `**bold**`, inline `` `code` ``, and GFM pipe tables (M9i:
//! buffered whole-table so column widths align; cells wrap inside
//! their column instead of being cut). Hand-rolled instead of a
//! markdown crate: no new dependency, and the lookahead scanner never
//! swallows literal markers (e.g. `a ** b` or `3*4`).
//!
//! Wrapping is width-aware (CJK) and happens per block after inline
//! segmentation, so ANSI codes never break the width math. Piped
//! output (`ansi = false`) returns the source unchanged — logs and
//! demo recordings keep the raw markdown.



#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Style {
    Plain,
    Bold,
    Code,
    Quote,
}

use crate::cli::render::{pad_display, visible_width, wrap_line_ansi};

/// Parameterized painter: honors the caller's ANSI decision (the
/// render.rs helpers use the global gate, which is off under tests).
struct Painter {
    ansi: bool,
}

impl Painter {
    fn paint(&self, code: &str, text: &str) -> String {
        if self.ansi {
            format!("\x1B[{code}m{text}\x1B[0m")
        } else {
            text.to_string()
        }
    }
    fn bold(&self, t: &str) -> String {
        self.paint("1", t)
    }
    fn cyan(&self, t: &str) -> String {
        self.paint("36", t)
    }
    fn dim(&self, t: &str) -> String {
        self.paint("2", t)
    }
}

/// Render markdown to a styled, wrapped string.
pub(crate) fn render(src: &str, width: usize, ansi: bool) -> String {
    if !ansi {
        return src.to_string();
    }
    let mut st = LineRenderer::new(width, true);
    let mut out = String::new();
    for line in src.lines() {
        out.push_str(&st.line(line));
    }
    // A table candidate held to the very end was just a text line.
    out.push_str(&st.flush_pending());
    // Fence left open by the model: close it visibly instead of
    // styling the rest of the transcript as code forever.
    if st.in_fence {
        let p = Painter { ansi };
        out.push_str(&p.dim("```"));
        out.push('\n');
    }
    out
}

/// Table assembly state (M9i): a table is a MULTI-line construct, but
/// `LineRenderer::line` is fed one line at a time — so a possible
/// header is held until the next line confirms (separator row) or
/// refutes it, and a confirmed table buffers rows until a non-row line
/// ends it. Living in `LineRenderer` keeps `render` and `StreamMd` on
/// one code path (the streaming renderer needs no extra machinery).
enum TableState {
    /// One line held as a table-header candidate (not yet confirmed).
    Candidate(String),
    /// Confirmed: header (first) + separator + body rows, raw.
    Rows(Vec<String>),
}

/// Single-line rendering state shared by `render` (whole reply) and
/// `StreamMd` (streaming deltas) — one code path, one visual language.
pub(crate) struct LineRenderer {
    width: usize,
    ansi: bool,
    in_fence: bool,
    table: Option<TableState>,
}

impl LineRenderer {
    pub(crate) fn new(width: usize, ansi: bool) -> Self {
        Self { width, ansi, in_fence: false, table: None }
    }

    /// Render one complete line (caller consumed its '\n'); output
    /// includes the trailing newline. May return "" while a table is
    /// being assembled (the rows render when the table ends).
    pub(crate) fn line(&mut self, line: &str) -> String {
        // A fence line never belongs to a table; it resolves any
        // pending state first (a fence inside a table ends the table).
        if line.trim_start().starts_with("```") {
            let mut out = self.flush_pending();
            self.in_fence = !self.in_fence;
            let p = Painter { ansi: self.ansi };
            out.push_str(&p.dim(line));
            out.push('\n');
            return out;
        }
        if self.in_fence {
            // Code keeps its shape: no wrap, no inline styling.
            return format!("{line}\n");
        }
        match self.table.take() {
            Some(TableState::Rows(mut rows)) => {
                if is_table_candidate(line) {
                    rows.push(line.to_string());
                    self.table = Some(TableState::Rows(rows));
                    String::new()
                } else {
                    // Table ended: render it, then the terminator line
                    // (which may itself open a new candidate).
                    let mut out = render_table(&rows, self.width, self.ansi);
                    if is_table_candidate(line) {
                        self.table = Some(TableState::Candidate(line.to_string()));
                    } else {
                        out.push_str(&self.render_line(line));
                    }
                    out
                }
            }
            Some(TableState::Candidate(head)) => {
                if is_separator_row(line) {
                    self.table = Some(TableState::Rows(vec![head, line.to_string()]));
                    String::new()
                } else {
                    // Not a table after all: the held line renders as
                    // plain text; this line may start a new candidate.
                    let mut out = self.render_line(&head);
                    if is_table_candidate(line) {
                        self.table = Some(TableState::Candidate(line.to_string()));
                    } else {
                        out.push_str(&self.render_line(line));
                    }
                    out
                }
            }
            None => {
                if is_table_candidate(line) {
                    self.table = Some(TableState::Candidate(line.to_string()));
                    String::new()
                } else {
                    self.render_line(line)
                }
            }
        }
    }

    /// Flush held table state at end-of-reply: a lone candidate was
    /// just a text line; a confirmed table renders in full.
    pub(crate) fn flush_pending(&mut self) -> String {
        match self.table.take() {
            None => String::new(),
            Some(TableState::Candidate(head)) => self.render_line(&head),
            Some(TableState::Rows(rows)) => render_table(&rows, self.width, self.ansi),
        }
    }

    /// The original single-line renderer (fences, headings, lists,
    /// quotes, plain paragraphs) — no table awareness.
    fn render_line(&mut self, line: &str) -> String {
        let p = Painter { ansi: self.ansi };
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            self.in_fence = !self.in_fence;
            return format!("{}\n", p.dim(line));
        }
        if self.in_fence {
            // Code keeps its shape: no wrap, no inline styling.
            return format!("{line}\n");
        }
        if trimmed.is_empty() {
            return "\n".into();
        }
        if let Some(rest) = trimmed.strip_prefix('#') {
            let text = rest.trim_start_matches('#').trim();
            return format!("{}\n", styled_line(&p, text, self.width, 0, Style::Bold, true));
        }
        if trimmed == "---" || trimmed == "***" {
            return format!("{}\n", p.dim(&"─".repeat(24)));
        }
        if let Some(rest) = trimmed.strip_prefix("> ").or(trimmed.strip_prefix('>')) {
            let mut out = String::new();
            for l in styled_line(&p, rest.trim(), self.width, 2, Style::Quote, false).lines() {
                out.push_str(&p.dim("│ "));
                out.push_str(l);
                out.push('\n');
            }
            return out;
        }
        // Lists: `- `/`* `/`+ ` become "• ", ordered items keep their
        // marker; continuation lines align under the marker (2 cols).
        let (marker, rest) = list_item(trimmed);
        let (bullet, indent) = match marker {
            Some(_) => ("• ", 2usize),
            None => ("", 0usize),
        };
        let body = styled_line(&p, rest, self.width, indent, Style::Plain, false);
        let mut out = String::new();
        if marker.is_some() {
            for (i, l) in body.lines().enumerate() {
                if i == 0 {
                    out.push_str("  ");
                    out.push_str(bullet);
                } else {
                    out.push_str("    ");
                }
                out.push_str(l);
                out.push('\n');
            }
        } else {
            out.push_str(&body);
            out.push('\n');
        }
        out
    }
}

/// Streaming markdown renderer (C1): feed content deltas, get styled
/// output as soon as a line completes. The trailing partial line is
/// held until it terminates (or `finish`). `ansi = false` (pipes) just
/// passes deltas through untouched.
pub(crate) struct StreamMd {
    ansi: bool,
    buf: String,
    st: LineRenderer,
}

impl StreamMd {
    pub(crate) fn new(width: usize, ansi: bool) -> Self {
        Self { ansi, buf: String::new(), st: LineRenderer::new(width, ansi) }
    }

    /// Feed one content delta; returns everything printable now.
    pub(crate) fn feed(&mut self, delta: &str) -> String {
        if !self.ansi {
            return delta.to_string();
        }
        self.buf.push_str(delta);
        let mut out = String::new();
        while let Some(pos) = self.buf.find('\n') {
            let line: String = self.buf.drain(..=pos).collect();
            let line = line.trim_end_matches('\n');
            out.push_str(&self.st.line(line));
        }
        out
    }

    /// End of the reply: flush the unterminated tail (and close an
    /// open fence visibly, like `render`).
    pub(crate) fn finish(&mut self) -> String {
        let mut out = String::new();
        if !self.ansi {
            return out;
        }
        if !self.buf.is_empty() {
            let tail = std::mem::take(&mut self.buf);
            out.push_str(&self.st.line(&tail));
        }
        out.push_str(&self.st.flush_pending());
        if self.st.in_fence {
            let p = Painter { ansi: self.ansi };
            out.push_str(&p.dim("```"));
            out.push('\n');
            self.st.in_fence = false;
        }
        out
    }
}

fn list_item(line: &str) -> (Option<char>, &str) {
    for m in ['-', '*', '+'] {
        if let Some(rest) = line.strip_prefix(m).filter(|r| r.starts_with(' ')) {
            return (Some(m), rest.trim_start());
        }
    }
    // Ordered: "12. text" / "1) text"
    let digits = line.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits > 0 {
        let after = &line[digits..];
        if (after.starts_with(". ") || after.starts_with(") ")) && digits <= 3 {
            return (None, line); // keep "12. text" as-is
        }
    }
    (None, line)
}

/// Parse inline markers, wrap by display width, emit ANSI per run.
fn styled_line(
    p: &Painter,
    line: &str,
    width: usize,
    indent: usize,
    base: Style,
    heading: bool,
) -> String {
    let spans = inline_parse(line, base);
    // Flatten to (char, style) cells and greedy-wrap.
    let mut cells: Vec<(char, Style)> = Vec::new();
    for (text, style) in &spans {
        for c in text.chars() {
            cells.push((c, *style));
        }
    }
    // Trim the LINE's trailing spaces only (0907 反馈 [渲染]: the old
    // per-run trim_end ate the space at span boundaries — "使用 `x` 来"
    // rendered as "使用x来"). Interior spaces, including between a code
    // span and CJK text, are content.
    while cells.last().is_some_and(|(c, _)| *c == ' ') {
        cells.pop();
    }
    let pad = " ".repeat(indent);
    let mut out = String::new();
    let mut i = 0;
    let mut first = true;
    while i < cells.len() {
        if !first {
            out.push('\n');
            out.push_str(&pad);
        }
        first = false;
        let mut w = indent;
        let mut run_start = i;
        let mut cur_style = cells[i].1;
        while i < cells.len() {
            let cw = unicode_width::UnicodeWidthChar::width(cells[i].0).unwrap_or(0);
            if w + cw > width && i > run_start {
                break;
            }
            w += cw;
            i += 1;
            if i < cells.len() && cells[i].1 != cur_style {
                emit_run(p, &cells[run_start..i], cur_style, heading, &mut out);
                run_start = i;
                cur_style = cells[i].1;
            }
        }
        emit_run(p, &cells[run_start..i], cur_style, heading, &mut out);
    }
    if cells.is_empty() {
        out.push_str(&pad);
    }
    out
}

fn emit_run(p: &Painter, cells: &[(char, Style)], style: Style, heading: bool, out: &mut String) {
    let text: String = cells.iter().map(|(c, _)| c).collect();
    let painted = match style {
        Style::Bold if heading => p.cyan(&text),
        Style::Bold => p.bold(&text),
        Style::Code => p.cyan(&text),
        Style::Plain | Style::Quote => text,
    };
    out.push_str(&painted);
}

/// Lookahead scanner: `**bold**` and `` `code` `` only become spans
/// when their closer exists; otherwise the marker characters are
/// emitted literally. No regex, no swallowed math stars.
fn inline_parse(line: &str, base: Style) -> Vec<(String, Style)> {
    let mut spans: Vec<(String, Style)> = Vec::new();
    let mut plain = String::new();
    let bytes = line;
    let mut i = 0;
    while i < bytes.len() {
        let rest = &bytes[i..];
        if let Some(after) = rest.strip_prefix("**")
            && let Some(end) = after.find("**")
        {
            flush(&mut plain, &mut spans, base);
            spans.push((after[..end].to_string(), Style::Bold));
            i += 2 + end + 2;
            continue;
        }
        if let Some(after) = rest.strip_prefix('`')
            && let Some(end) = after.find('`')
        {
            flush(&mut plain, &mut spans, base);
            spans.push((after[..end].to_string(), Style::Code));
            i += 1 + end + 1;
            continue;
        }
        let ch = rest.chars().next().unwrap();
        plain.push(ch);
        i += ch.len_utf8();
    }
    flush(&mut plain, &mut spans, base);
    spans
}

fn flush(plain: &mut String, spans: &mut Vec<(String, Style)>, base: Style) {
    if !plain.is_empty() {
        spans.push((std::mem::take(plain), base));
    }
}

// ---------------------------------------------------------------------------
// Tables (M9i): GFM pipe tables, buffered whole so column widths align.
// ---------------------------------------------------------------------------

/// A line that could take part in a table: non-empty, has a pipe, and
/// is not some other block construct (fence lines never get here).
fn is_table_candidate(line: &str) -> bool {
    let t = line.trim();
    if t.is_empty() || !t.contains('|') {
        return false;
    }
    if t.starts_with('#') || t.starts_with('>') {
        return false;
    }
    if t == "---" || t == "***" {
        return false;
    }
    list_item(t).0.is_none()
}

/// GFM alignment/dash separator row: only `| - :` and whitespace, with
/// at least one dash and one pipe (a bare `---` is a rule, not a row).
fn is_separator_row(line: &str) -> bool {
    let t = line.trim();
    t.contains('-')
        && t.contains('|')
        && t.chars().all(|c| matches!(c, '|' | '-' | ':' | ' ' | '\t'))
}

/// Split one row into cells: surrounding pipes stripped, cells
/// trimmed. Escaped `\|` is not supported (models don't emit it here).
fn split_row(line: &str) -> Vec<String> {
    let t = line.trim();
    let t = t.strip_prefix('|').unwrap_or(t);
    let t = t.strip_suffix('|').unwrap_or(t);
    t.split('|').map(|c| c.trim().to_string()).collect()
}

/// One line of LLM prose with inline markers styled — the SAME inline
/// path as chat replies. Review/debrief printouts used to skip this and
/// rendered bare `` `code` `` backticks (0909_2 同学反馈 1). No-op
/// without ANSI (pipe output stays raw).
pub(crate) fn paint_line(line: &str) -> String {
    if !crate::cli::render::ansi_enabled() {
        return line.to_string();
    }
    let p = Painter { ansi: true };
    paint_inline(&p, line, Style::Plain)
}

fn paint_inline(p: &Painter, line: &str, base: Style) -> String {
    inline_parse(line, base)
        .into_iter()
        .map(|(text, style)| match style {
            Style::Bold => p.bold(&text),
            Style::Code => p.cyan(&text),
            Style::Plain | Style::Quote => text,
        })
        .collect()
}

/// Render buffered table rows as an aligned text table: `│` column
/// separators (dim), a `─┼─` rule under the header, cells styled by the
/// inline scanner (header row bold), cells wider than their column
/// WRAP inside it instead of being cut. Column widths shrink
/// proportionally (floor 3 cells) when the table exceeds the width.
fn render_table(rows: &[String], width: usize, ansi: bool) -> String {
    let p = Painter { ansi };
    let grid: Vec<Vec<String>> = rows
        .iter()
        .filter(|r| !is_separator_row(r))
        .map(|r| split_row(r))
        .collect();
    if grid.is_empty() {
        return String::new();
    }
    let n = grid.iter().map(|r| r.len()).max().unwrap_or(0).max(1);
    let grid: Vec<Vec<String>> = grid
        .into_iter()
        .map(|mut r| {
            r.resize(n, String::new());
            r
        })
        .collect();
    // Painted cells (header bold) and their natural widths.
    let painted: Vec<Vec<String>> = grid
        .iter()
        .enumerate()
        .map(|(ri, row)| row.iter().map(|c| paint_inline(&p, c, if ri == 0 { Style::Bold } else { Style::Plain })).collect())
        .collect();
    let gap = 3usize; // " │ "
    let overhead = gap * n.saturating_sub(1);
    let mut widths: Vec<usize> = (0..n)
        .map(|j| painted.iter().map(|row| visible_width(&row[j])).max().unwrap_or(0).max(1))
        .collect();
    // Shrink proportionally (floor 3 cells) when the table is too wide.
    let avail = width.saturating_sub(overhead).max(n * 3);
    let total: usize = widths.iter().sum();
    if total > avail {
        let scaled: Vec<usize> =
            widths.iter().map(|w| (*w * avail / total).max(3)).collect();
        // Rounding plus the floor may overshoot: trim the widest until fit.
        let mut sum: usize = scaled.iter().sum();
        let mut out = scaled;
        while sum > avail {
            let (mi, _) = out.iter().enumerate().max_by_key(|(_, w)| *w).unwrap_or((0, &0));
            if out[mi] <= 3 {
                break;
            }
            out[mi] -= 1;
            sum -= 1;
        }
        widths = out;
    }
    // Emit rows: each row wraps to its column heights, zipped line-wise.
    let mut out = String::new();
    for (ri, row) in painted.iter().enumerate() {
        let wrapped: Vec<Vec<String>> = row
            .iter()
            .zip(&widths)
            .map(|(c, w)| wrap_line_ansi(c, *w))
            .collect();
        let height = wrapped.iter().map(|v| v.len()).max().unwrap_or(1);
        for k in 0..height {
            for (j, w) in widths.iter().enumerate() {
                let piece = wrapped[j].get(k).map(String::as_str).unwrap_or("");
                let cell = pad_display(piece, *w);
                if j > 0 {
                    out.push_str(&p.dim(" │ "));
                }
                out.push_str(&cell);
            }
            out.push('\n');
        }
        if ri == 0 {
            let sep: Vec<String> = widths.iter().map(|w| "─".repeat(*w)).collect();
            out.push_str(&p.dim(&sep.join("─┼─")));
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn stream_md_matches_whole_render() {
        // Feeding the same reply in arbitrary deltas must produce the
        // same output as rendering the whole reply at once.
        let src = "## 标题\n\n第一段：`E0382` 与 **重点**。\n\n- 列表一\n- 列表二\n\n```rust\nfn f() {}\n```\n尾行无换行";
        let want = render(src, 60, true);
        let mut sm = StreamMd::new(60, true);
        let mut got = String::new();
        // Deltas cut mid-line and mid-fence-marker.
        for chunk in ["## 标", "题\n\n第一段：`E0382` 与 **重", "点**。\n\n- 列表一\n- 列表", "二\n\n```rust\nfn f() {}\n``", "`\n尾行无换行"] {
            got.push_str(&sm.feed(chunk));
        }
        got.push_str(&sm.finish());
        assert_eq!(got, want, "streamed output must equal whole-render");
    }

    #[test]
    fn stream_md_finish_flushes_tail_and_open_fence() {
        let mut sm = StreamMd::new(60, true);
        let mut got = sm.feed("```rust\nfn f() {");
        got.push_str(&sm.finish());
        assert!(got.contains("fn f() {"), "{got}");
        assert!(got.contains("```"), "open fence closed: {got}");
    }

    #[test]
    fn stream_md_passthrough_without_ansi() {
        let mut sm = StreamMd::new(60, false);
        assert_eq!(sm.feed("abc"), "abc");
        assert_eq!(sm.feed("**x**"), "**x**");
        assert_eq!(sm.finish(), "");
    }

    #[test]
    fn plain_source_untouched_without_ansi() {
        let src = "**粗体** 和 `代码`";
        assert_eq!(render(src, 80, false), src);
    }

    #[test]
    fn bold_and_code_spans() {
        let out = render("这是 **重点** 与 `E0382` 说明", 200, true);
        assert!(out.contains("\x1B[1m重点\x1B[0m"), "{out}");
        assert!(out.contains("\x1B[36mE0382\x1B[0m"), "{out}");
        assert!(!out.contains("**"), "{out}");
    }

    /// 0907 反馈 [渲染]: the old per-run trim_end ate the space at
    /// style-span boundaries — CJK text next to a code/bold span lost
    /// its separating space on screen (the exported raw text was fine).
    #[test]
    fn inline_span_boundary_spaces_survive() {
        let out = render("使用 `Vec<i32>` 来存储元素", 200, true);
        assert!(out.contains("使用 \x1B[36mVec<i32>\x1B[0m 来存储元素"), "{out}");
        let out = render("先说 **所有权** 再说别的", 200, true);
        assert!(out.contains("先说 \x1B[1m所有权\x1B[0m 再说别的"), "{out}");
    }

    /// Trailing spaces at the very END of a line are still not shown.
    #[test]
    fn styled_line_trims_only_line_end_spaces() {
        let p = Painter { ansi: false };
        let out = styled_line(&p, "hello `x`  ", 80, 0, Style::Plain, false);
        assert_eq!(out, "hello x");
    }

    #[test]
    fn unmatched_markers_stay_literal() {
        let out = render("数学 3**4 与 2*3，a ` b", 200, true);
        assert!(out.contains("3**4"), "{out}");
        assert!(out.contains("2*3"), "{out}");
        assert!(out.contains('`'), "{out}");
    }

    #[test]
    fn headings_and_lists_and_quotes() {
        let src = "# 标题\n- 第一条\n- 第二条长一些需要折行的条目内容继续继续继续\n> 引用一句\n\n正文段落";
        let out = render(src, 40, true);
        assert!(out.contains("标题"), "{out}");
        assert!(out.contains("• 第一条"), "{out}");
        assert!(out.contains("│ "), "{out}");
        assert!(out.contains("正文段落"), "{out}");
    }

    #[test]
    fn ordered_lists_keep_marker() {
        let out = render("1. 第一步\n2. 第二步", 40, true);
        assert!(out.contains("1. 第一步"), "{out}");
        assert!(out.contains("2. 第二步"), "{out}");
    }

    #[test]
    fn fence_content_verbatim_and_wrapped_lines_align() {
        let src = "```rust\nfn very_long_function(a: u32, b: u32) -> u32 { a + b }\n```\n后记";
        let out = render(src, 12, true);
        assert!(out.contains("fn very_long_function(a: u32, b: u32) -> u32 { a + b }"), "{out}");
        assert!(out.contains("后记"), "{out}");
    }

    #[test]
    fn unclosed_fence_gets_closed() {
        let out = render("```rust\nfn x() {}", 40, true);
        assert_eq!(out.matches("```").count(), 2, "{out}"); // opened + auto-closed
    }

    #[test]
    fn cjk_wrapping_by_display_width() {
        let src = "一段很长的中文说明需要按照终端宽度正确折行处理不能超出版面";
        let out = render(src, 10, true);
        for l in out.lines() {
            assert!(l.width() <= 10, "line too wide: {l}");
        }
    }

    #[test]
    fn list_continuation_indents() {
        let src = "- 项目内容很长很长很长很长很长很长很长很长很长很长很长很长";
        let out = render(src, 12, true);
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines.len() >= 2, "{out}");
        assert!(lines[0].contains("• "));
        assert!(lines[1].starts_with("    "), "{out}");
    }

    // --- tables (M9i) ----------------------------------------------------

    fn strip_codes(s: &str) -> String {
        s.replace("\x1B[1m", "").replace("\x1B[36m", "").replace("\x1B[2m", "").replace("\x1B[0m", "")
    }

    #[test]
    fn tables_render_aligned_with_rule_and_no_raw_pipes() {
        let src = "| 维度 | 用户解 | 参考解 |\n|---|:---:|---:|\n| 行数 | 12 | 10 |\n| clippy | 0 条 | 0 条 |";
        let out = render(src, 80, true);
        let plain = strip_codes(&out);
        assert!(plain.contains('│'), "column separators: {plain}");
        assert!(plain.contains('┼'), "header rule: {plain}");
        assert!(!plain.contains('|'), "raw pipes gone: {plain}");
        assert!(plain.contains("维度"), "{plain}");
        assert!(plain.contains("参考解"), "{plain}");
        // Every physical line has the same display width (aligned).
        let ws: Vec<usize> = out.lines().map(visible_width).collect();
        assert!(ws.iter().all(|w| *w == ws[0]), "unaligned: {out:?}");
    }

    #[test]
    fn table_cells_wrap_inside_their_column() {
        let long = "很长的单元格内容".repeat(6);
        let src = format!("| 列 | 内容 |\n|---|---|\n| a | {long} |");
        let out = render(&src, 40, true);
        let ws: Vec<usize> = out.lines().map(visible_width).collect();
        assert!(ws.iter().all(|w| *w <= 40), "overflow: {out:?}");
        // The full cell text survives (wrapped, not truncated).
        let joined: String = strip_codes(&out).replace(['│', ' ', '\n'], "");
        assert!(joined.contains(&long), "cell content must survive: {out}");
        assert!(out.lines().count() > 3, "wrapped to several rows: {out:?}");
    }

    #[test]
    fn wide_table_shrinks_proportionally() {
        let src = "| aaaaaaaa | bbbbbbbb | cccccccc |\n|---|---|---|\n| 1 | 2 | 3 |";
        let out = render(src, 30, true);
        for l in out.lines() {
            assert!(visible_width(l) <= 30, "too wide: {l:?}");
        }
        assert!(out.contains('a'), "{out}");
    }

    #[test]
    fn pipe_sentence_is_not_a_table() {
        // A single pipe line with no separator after it stays text.
        let src = "对比 A | B 两种写法\n\n下一段正文";
        let out = render(src, 80, true);
        assert!(out.contains("对比 A | B 两种写法"), "{out}");
        assert!(!out.contains('│'), "{out}");
    }

    #[test]
    fn table_candidate_flushed_at_eof() {
        // Source ends on a held candidate line: render() and StreamMd
        // finish() must both flush it as plain text.
        let src = "正文\n结尾带竖线 | 的行";
        let want = render(src, 80, true);
        assert!(want.contains("结尾带竖线 | 的行"), "{want}");
        let mut sm = StreamMd::new(80, true);
        let mut got = sm.feed(src);
        got.push_str(&sm.finish());
        assert_eq!(got, want);
    }

    #[test]
    fn fence_lines_are_immune_to_tables() {
        let src = "| 表格前 |\n```rust\nlet v = a | b;\n```\n后文";
        let out = render(src, 80, true);
        assert!(out.contains("let v = a | b;"), "fence content verbatim: {out}");
        assert!(out.contains("后文"), "{out}");
    }

    #[test]
    fn table_inline_styling_survives_in_cells() {
        let src = "| 维度 | 结果 |\n|---|---|\n| 惯用性 | **4/5**，可用 `map` |";
        let out = render(src, 80, true);
        assert!(out.contains("\x1B[1m4/5\x1B[0m"), "bold in cell: {out}");
        assert!(out.contains("\x1B[36mmap\x1B[0m"), "code in cell: {out}");
    }

    #[test]
    fn stream_md_table_matches_whole_render() {
        // A table fed in nasty deltas (cut inside the separator row and
        // mid-cell) must equal the whole-render byte for byte.
        let src = "前言\n\n| 维度 | 用户解 | 参考解 |\n|---|:---:|---|\n| 行数 | 12 | 10 |\n\n尾行";
        let want = render(src, 60, true);
        let mut sm = StreamMd::new(60, true);
        let mut got = String::new();
        for chunk in ["前言\n\n| 维度 | 用", "户解 | 参考解|\n|---|:---", ":|---|\n| 行数 | 12 |", " 10 |\n\n尾行"] {
            got.push_str(&sm.feed(chunk));
        }
        got.push_str(&sm.finish());
        assert_eq!(got, want, "streamed table must equal whole-render");
    }

    #[test]
    fn table_ends_when_a_non_row_line_arrives() {
        let src = "| a | b |\n|---|---|\n| 1 | 2 |\n正文继续";
        let out = render(src, 80, true);
        let plain = strip_codes(&out);
        assert!(plain.contains('┼'), "{plain}");
        assert!(plain.contains("正文继续"), "{plain}");
    }
}
