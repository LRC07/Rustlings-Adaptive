//! Minimal markdown → ANSI rendering for coach replies (M4.3).
//!
//! Scope is deliberately the subset the model actually emits in this
//! chat: fenced code blocks, `#` headings, `-`/`*`/ordered lists, `>`
//! quotes, `**bold**`, inline `` `code` ``. Hand-rolled instead of a
//! markdown crate: ~200 lines, no new dependency, and the lookahead
//! scanner never swallows literal markers (e.g. `a ** b` or `3*4`).
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
    // Fence left open by the model: close it visibly instead of
    // styling the rest of the transcript as code forever.
    if st.in_fence {
        let p = Painter { ansi };
        out.push_str(&p.dim("```"));
        out.push('\n');
    }
    out
}

/// Single-line rendering state shared by `render` (whole reply) and
/// `StreamMd` (streaming deltas) — one code path, one visual language.
pub(crate) struct LineRenderer {
    width: usize,
    ansi: bool,
    in_fence: bool,
}

impl LineRenderer {
    pub(crate) fn new(width: usize, ansi: bool) -> Self {
        Self { width, ansi, in_fence: false }
    }

    /// Render one complete line (caller consumed its '\n'); output
    /// includes the trailing newline.
    pub(crate) fn line(&mut self, line: &str) -> String {
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
/// (Wired into the REPL in block 4 — temporary allow.)
#[allow(dead_code)]
pub(crate) struct StreamMd {
    ansi: bool,
    buf: String,
    st: LineRenderer,
}

impl StreamMd {
    /// (Wired into the REPL in block 4 — temporary allow.)
    #[allow(dead_code)]
    pub(crate) fn new(width: usize, ansi: bool) -> Self {
        Self { ansi, buf: String::new(), st: LineRenderer::new(width, ansi) }
    }

    /// Feed one content delta; returns everything printable now.
    #[allow(dead_code)]
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
    #[allow(dead_code)]
    pub(crate) fn finish(&mut self) -> String {
        let mut out = String::new();
        if !self.ansi {
            return out;
        }
        if !self.buf.is_empty() {
            let tail = std::mem::take(&mut self.buf);
            out.push_str(&self.st.line(&tail));
        }
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
    let text = text.trim_end().to_string();
    if text.is_empty() {
        return;
    }
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
}
