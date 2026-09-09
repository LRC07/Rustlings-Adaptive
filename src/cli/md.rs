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
    fn green(&self, t: &str) -> String {
        self.paint("32", t)
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
        out.push_str(&p.dim("───"));
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
    /// Rust syntax-highlight carry-over inside a fence (0909_2 反馈:
    /// keyword coloring) — block comments / raw strings span lines.
    hl: HlState,
    hl_active: bool,
    table: Option<TableState>,
}

/// Carry-over tokenizer state for fenced Rust highlighting.
#[derive(Default, Clone, Copy, PartialEq)]
enum HlState {
    #[default]
    Normal,
    /// Inside a /* … */ block comment.
    BlockComment,
    /// Inside a raw string r#"…"# (depth 1..=255 tracked as count).
    RawString(u8),
}

impl LineRenderer {
    pub(crate) fn new(width: usize, ansi: bool) -> Self {
        Self { width, ansi, in_fence: false, hl: HlState::Normal, hl_active: false, table: None }
    }

    /// Render one complete line (caller consumed its '\n'); output
    /// includes the trailing newline. May return "" while a table is
    /// being assembled (the rows render when the table ends).
    pub(crate) fn line(&mut self, line: &str) -> String {
        // A fence line never belongs to a table; it resolves any
        // pending state first (a fence inside a table ends the table).
        if line.trim_start().starts_with("```") {
            let mut out = self.flush_pending();
            let opening = !self.in_fence;
            self.in_fence = opening;
            self.hl = HlState::Normal;
            self.hl_active = opening
                && matches!(lang_of(line).as_deref(), None | Some("rust"));
            let p = Painter { ansi: self.ansi };
            // 0909_2 反馈: the raw ```rust marker line read as noise —
            // fences now render as thin dim rules (language tagged).
            let lang = lang_of(line).unwrap_or_default();
            if opening {
                if lang.is_empty() {
                    out.push_str(&p.dim("───"));
                } else {
                    out.push_str(&p.dim(&format!("─── {lang}")));
                }
            } else {
                out.push_str(&p.dim("───"));
            }
            out.push('\n');
            return out;
        }
        if self.in_fence {
            // Code keeps its shape (no wrap); with a rust fence the
            // line goes through the syntax highlighter.
            if self.hl_active {
                let (painted, next) = hl_rust_line(line, self.hl, self.ansi);
                self.hl = next;
                return format!("{painted}\n");
            }
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
            let opening = !self.in_fence;
            self.in_fence = opening;
            self.hl = HlState::Normal;
            self.hl_active = opening && matches!(lang_of(line).as_deref(), None | Some("rust"));
            let lang = lang_of(line).unwrap_or_default();
            let marker = if opening && !lang.is_empty() {
                format!("─── {lang}")
            } else {
                "───".to_string()
            };
            return format!("{}\n", p.dim(&marker));
        }
        if self.in_fence {
            // Code keeps its shape (no wrap); rust fences get keywords.
            if self.hl_active {
                let (painted, next) = hl_rust_line(line, self.hl, self.ansi);
                self.hl = next;
                return format!("{painted}\n");
            }
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
/// trimmed. `\|` is the GFM cell escape — models emit it for Rust
/// closures (`|s|`) inside table cells (0909 录屏实测: rows with
/// `\|` split into extra columns and the whole table misaligned);
/// protect it before splitting, restore after.
fn split_row(line: &str) -> Vec<String> {
    const CELL_ESC: &str = "\u{0}";
    let t = line.trim();
    let t = t.strip_prefix('|').unwrap_or(t);
    let t = t.strip_suffix('|').unwrap_or(t);
    let protected = t.replace("\\|", CELL_ESC);
    protected.split('|').map(|c| c.trim().replace(CELL_ESC, "|")).collect()
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

/// Fence language tag ("```rust" → Some("rust")); None for a bare ``` fence.
fn lang_of(line: &str) -> Option<String> {
    let t = line.trim_start().strip_prefix("```")?.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_lowercase())
    }
}

const RUST_KEYWORDS: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum",
    "extern", "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move",
    "mut", "pub", "ref", "return", "self", "Self", "static", "struct", "super", "trait", "true",
    "type", "unsafe", "use", "where", "while",
];

/// Primitive type names — lowercase and NOT keywords in Rust's grammar,
/// so the keyword branch misses them (`i32` used to render plain).
const RUST_PRIMITIVES: &[&str] = &[
    "bool", "char", "str", "f32", "f64", "i8", "i16", "i32", "i64", "i128", "isize", "u8", "u16",
    "u32", "u64", "u128", "usize",
];

fn is_ident_start(c: char) -> bool {
    c.is_alphabetic() || c == '_'
}

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Position of the closing `"` + `k` `#`s of a raw string, scanning from
/// `from`; the returned index is the LAST `#`.
fn find_raw_close(chars: &[char], from: usize, k: u8) -> Option<usize> {
    let mut m = from;
    while m < chars.len() {
        if chars[m] == '"' {
            let mut hashes = 0u8;
            let mut q = m + 1;
            while q < chars.len() && chars[q] == '#' && hashes < k {
                hashes += 1;
                q += 1;
            }
            if hashes == k {
                return Some(q - 1);
            }
        }
        m += 1;
    }
    None
}

/// Highlight ONE line of fenced Rust (0909_2 反馈): keywords bold,
/// strings/chars green, comments/attributes dim, macros cyan. Pure and
/// streaming-safe — block comments and raw strings carry over lines via
/// `st`. Char literals with escapes stay plain (deliberate simplicity).
fn hl_rust_line(line: &str, st: HlState, ansi: bool) -> (String, HlState) {
    if !ansi {
        return (line.to_string(), st);
    }
    let p = Painter { ansi: true };
    let chars: Vec<char> = line.chars().collect();
    let mut out = String::new();
    let mut i = 0usize;
    let mut state = st;

    // Carry-over states first.
    match state {
        HlState::BlockComment => {
            if let Some(pos) = (i..chars.len().saturating_sub(1)).find(|&j| {
                chars[j] == '*' && chars.get(j + 1) == Some(&'/')
            }) {
                out.push_str(&p.dim(&chars[i..pos + 2].iter().collect::<String>()));
                i = pos + 2;
                state = HlState::Normal;
            } else {
                out.push_str(&p.dim(&chars[i..].iter().collect::<String>()));
                return (out, state);
            }
        }
        HlState::RawString(k) => {
            if let Some(close) = find_raw_close(&chars, i, k) {
                out.push_str(&p.green(&chars[i..=close].iter().collect::<String>()));
                i = close + 1;
                state = HlState::Normal;
            } else {
                out.push_str(&p.green(&chars[i..].iter().collect::<String>()));
                return (out, state);
            }
        }
        HlState::Normal => {}
    }

    while i < chars.len() {
        let c = chars[i];
        // Line comment → dim to EOL.
        if c == '/' && chars.get(i + 1) == Some(&'/') {
            out.push_str(&p.dim(&chars[i..].iter().collect::<String>()));
            return (out, state);
        }
        // Block comment (nesting-aware; carry to next line when open).
        if c == '/' && chars.get(i + 1) == Some(&'*') {
            let mut depth = 1usize;
            let mut j = i + 2;
            while j + 1 < chars.len() {
                if chars[j] == '*' && chars[j + 1] == '/' {
                    depth -= 1;
                    j += 2;
                    if depth == 0 {
                        break;
                    }
                } else if chars[j] == '/' && chars[j + 1] == '*' {
                    depth += 1;
                    j += 2;
                } else {
                    j += 1;
                }
            }
            if depth == 0 {
                let end = j.min(chars.len());
                out.push_str(&p.dim(&chars[i..end].iter().collect::<String>()));
                i = end;
            } else {
                out.push_str(&p.dim(&chars[i..].iter().collect::<String>()));
                return (out, HlState::BlockComment);
            }
            continue;
        }
        // Raw string r#"…"# (k hashes) — may span lines.
        if c == 'r' && matches!(chars.get(i + 1), Some('#')) {
            let mut k = 0usize;
            let mut j = i + 1;
            while chars.get(j) == Some(&'#') {
                k += 1;
                j += 1;
            }
            if chars.get(j) == Some(&'"') {
                if let Some(close) = find_raw_close(&chars, j + 1, k as u8) {
                    out.push_str(&p.green(&chars[i..=close].iter().collect::<String>()));
                    i = close + 1;
                } else {
                    out.push_str(&p.green(&chars[i..].iter().collect::<String>()));
                    return (out, HlState::RawString(k as u8));
                }
                continue;
            }
            // Not a raw string opener: fall through as an identifier.
        }
        // String literal with escapes (single line).
        if c == '"' {
            let mut j = i + 1;
            while j < chars.len() {
                if chars[j] == '\\' {
                    j += 2;
                    continue;
                }
                if chars[j] == '"' {
                    break;
                }
                j += 1;
            }
            if j < chars.len() {
                out.push_str(&p.green(&chars[i..=j].iter().collect::<String>()));
                i = j + 1;
            } else {
                out.push_str(&p.green(&chars[i..].iter().collect::<String>()));
                return (out, state);
            }
            continue;
        }
        // Identifier: keyword / macro / lifetime / plain.
        if is_ident_start(c) {
            let start = i;
            let mut j = i;
            while j < chars.len() && is_ident(chars[j]) {
                j += 1;
            }
            let word: String = chars[start..j].iter().collect();
            if chars.get(j) == Some(&'!') {
                out.push_str(&p.cyan(&format!("{word}!")));
                i = j + 1;
                continue;
            }
            if RUST_KEYWORDS.contains(&word.as_str()) {
                out.push_str(&p.bold(&word));
                i = j;
                continue;
            }
            // M9a3（"Vec<Shape> 未渲染" 反馈）：类型名上色。基本类型走
            // 词表；其余凡是首字母大写的标识符（std 类型/枚举变体/用户
            // 类型/常量）一律 cyan——Rust 命名约定下这几乎总是"类型级
            // 事物"，与宏、行内代码同色，语义一致。
            if RUST_PRIMITIVES.contains(&word.as_str())
                || word.chars().next().is_some_and(|c| c.is_uppercase())
            {
                out.push_str(&p.cyan(&word));
                i = j;
                continue;
            }
            out.push_str(&word);
            i = j;
            continue;
        }
        // Lifetime vs char literal: 'ident' (closed quote on this line)
        // is a char literal (green); otherwise a lifetime (plain).
        if c == '\'' {
            let ident: usize = chars[i + 1..].iter().take_while(|ch| is_ident(**ch)).count();
            if ident > 0 && chars.get(i + 1 + ident) == Some(&'\'') {
                out.push_str(&p.green(&chars[i..=i + ident + 1].iter().collect::<String>()));
                i = i + ident + 2;
                continue;
            }
            if ident > 0 {
                out.push_str(&chars[i..=i + ident].iter().collect::<String>());
                i = i + ident + 1;
                continue;
            }
            out.push(c);
            i += 1;
            continue;
        }
        out.push(c);
        i += 1;
    }
    (out, state)
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
        assert!(got.contains("f() {"), "{got}");
        assert!(got.contains("───"), "open fence closed with the rule marker: {got}");
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
        // `fn` is bold-wrapped and (M9a3) types are cyan — compare on
        // the stripped text; escapes may sit inside the line.
        let plain = strip_codes(&out);
        assert!(plain.contains("very_long_function(a: u32, b: u32) -> u32 { a + b }"), "{out}");
        assert!(out.contains("后记"), "{out}");
    }

    #[test]
    fn unclosed_fence_gets_closed() {
        let out = render("```rust\nfn x() {}", 40, true);
        assert_eq!(out.matches("───").count(), 2, "{out}"); // opened + auto-closed
        assert!(!out.contains("```"), "raw fence markers are replaced: {out}");
    }

    /// 0909_2 反馈: fence markers render as thin rules (language tagged)
    /// and fenced rust gets keyword/string/comment coloring.
    #[test]
    fn fence_marker_and_rust_highlighting() {
        let src = "```rust\nlet s = \"hi\"; // 注释\n```\n后记";
        let out = render(src, 80, true);
        assert!(out.contains("─── rust"), "{out}");
        assert!(!out.contains("```"), "{out}");
        assert!(out.contains("\x1B[1mlet"), "keyword bold: {out}");
        assert!(out.contains("\x1B[32m\"hi\"\x1B[0m"), "string green: {out}");
        assert!(out.contains("\x1B[2m// 注释"), "comment dim: {out}");
        // Streamed must match whole-render (shared path).
        let mut sm = StreamMd::new(80, true);
        let mut got = sm.feed(src);
        got.push_str(&sm.finish());
        assert_eq!(got, out, "stream == render");
    }

    #[test]
    fn highlight_state_carries_across_lines_and_langs() {        // Block comment spanning lines; macro cyan; non-rust fence plain.
        let src = "```rust\n/* 开头\n仍在注释 */\nprintln!(\"x\");\n```\n```json\n{ }\n```";
        let out = render(src, 80, true);
        assert!(out.contains("\x1B[2m/* 开头"), "{out}");
        assert!(out.contains("\x1B[2m仍在注释 */"), "{out}");
        assert!(out.contains("\x1B[36mprintln!"), "macro cyan: {out}");
        // json fence: no keyword coloring (fn wouldn't appear anyway;
        // assert the braces stayed raw).
        assert!(out.contains("{ }"), "{out}");
    }

    /// M9a3（"Vec<Shape> 未渲染" 反馈）: type names get cyan — std
    /// types, primitives, and user-defined types alike (uppercase
    /// heuristic); the code text itself stays untouched.
    #[test]
    fn rust_types_painted_in_fences() {
        let src = "```rust\nlet v: Vec<Shape> = Vec::new();\nlet n: i32 = f(3u8);\n```\n后记";
        let out = render(src, 80, true);
        assert!(out.contains("\x1B[36mVec\x1B[0m"), "std type cyan: {out}");
        assert!(out.contains("\x1B[36mShape\x1B[0m"), "user type cyan: {out}");
        assert!(out.contains("\x1B[36mi32\x1B[0m"), "primitive cyan: {out}");
        assert!(out.contains("\x1B[36mu8\x1B[0m"), "primitive cyan: {out}");
        let plain = strip_codes(&out);
        assert!(plain.contains("let v: Vec<Shape> = Vec::new();"), "text preserved: {plain}");
        assert!(plain.contains("let n: i32 = f(3u8);"), "text preserved: {plain}");
        // Streamed must match whole-render (shared path).
        let mut sm = StreamMd::new(80, true);
        let mut got = sm.feed(src);
        got.push_str(&sm.finish());
        assert_eq!(got, out, "stream == render");
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

    /// 0909 录屏实测: models emit `\|` for Rust closures (`|s|`) in
    /// table cells — the escaped pipe must stay INSIDE the cell, not
    /// split the row into extra columns (misaligning the table).
    #[test]
    fn table_escaped_pipes_stay_inside_cells() {
        let src = "| 方案 | key 存在时 | key 缺失时 |\n|---|---|---|\n\
                   | (1) `map(as_str)` | 2 | 1 |\n\
                   | (2) `unwrap_or_else(\\|s\\| …)` | 1 | 1 |";
        let out = render(src, 80, true);
        let plain = strip_codes(&out);
        assert!(
            plain.contains("unwrap_or_else(|s| …)"),
            "escaped pipe renders as a literal pipe inside the cell: {plain}"
        );
        assert!(!plain.contains('\\'), "no stray backslashes: {plain}");
        // Column count stays 3 for every row → aligned widths.
        let ws: Vec<usize> = out.lines().filter(|l| l.contains('│')).map(visible_width).collect();
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
        assert!(out.contains("v = a | b;"), "fence content verbatim: {out}");
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
