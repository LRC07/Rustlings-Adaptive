//! Abstract modification constraints on exercises (v3 design §7.2):
//! machine-checkable "design habit" rules like "no clone", "iterators
//! only", "max N lines". The static subset is checked here with a
//! token-level scan after stripping line comments.
//!
//! Known limitation (accepted for the initial version): matches inside
//! string literals can false-positive; upgrading to `syn`-based AST
//! checks is planned as F11.

use serde::{Deserialize, Serialize};

/// A constraint attached to a template. Static variants are checked
/// here; LLM-judged ones (SinglePass final say, CustomJudged) arrive
/// with the review gate in M5.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Constraint {
    /// Forbid `.clone()` / `.to_owned()` — forces borrowing thinking.
    NoClone,
    /// Forbid `.unwrap()` / `.expect()` — error-handling exercises.
    NoUnwrap,
    /// Forbid `for` / `while` / `loop` — push towards iterator chains.
    IteratorOnly,
    /// Limit the number of effective (non-comment, non-empty) lines.
    MaxLines(u32),
    /// Forbid arbitrary token sequences (substring match).
    CustomStatic { ban: Vec<String> },
}

impl Constraint {
    /// Stable id used in templates (`templates/*.toml`, M3).
    pub fn spec_id(&self) -> String {
        match self {
            Constraint::NoClone => "no-clone".to_string(),
            Constraint::NoUnwrap => "no-unwrap".to_string(),
            Constraint::IteratorOnly => "iterator-only".to_string(),
            Constraint::MaxLines(n) => format!("max-lines={n}"),
            Constraint::CustomStatic { ban } => format!("ban={}", ban.join(",")),
        }
    }

    /// Parse a spec string back into a constraint (inverse of spec_id).
    pub fn from_spec(spec: &str) -> Option<Constraint> {
        let spec = spec.trim();
        if let Some(n) = spec.strip_prefix("max-lines=") {
            return n.trim().parse::<u32>().ok().map(Constraint::MaxLines);
        }
        if let Some(ban) = spec.strip_prefix("ban=") {
            let toks: Vec<String> = ban
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
            if toks.is_empty() {
                return None;
            }
            return Some(Constraint::CustomStatic { ban: toks });
        }
        match spec {
            "no-clone" => Some(Constraint::NoClone),
            "no-unwrap" => Some(Constraint::NoUnwrap),
            "iterator-only" => Some(Constraint::IteratorOnly),
            _ => None,
        }
    }

    /// Chinese description for the CLI (AGENTS.md: CLI 文案用中文).
    pub fn name_cn(&self) -> String {
        match self {
            Constraint::NoClone => "禁止使用 .clone() / .to_owned()".to_string(),
            Constraint::NoUnwrap => "禁止使用 .unwrap() / .expect()".to_string(),
            Constraint::IteratorOnly => "禁止 for / while / loop（请用迭代器）".to_string(),
            Constraint::MaxLines(n) => format!("有效代码不超过 {n} 行"),
            Constraint::CustomStatic { ban } => format!("禁止出现 {}", ban.join("、")),
        }
    }
}

/// One constraint violation found by `check`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Violation {
    /// The constraint's spec id.
    pub constraint: String,
    /// 1-based line number; 0 means "whole file" (e.g. MaxLines).
    pub line: usize,
    /// The offending line (trimmed, length-capped).
    pub snippet: String,
    /// User-facing message (Chinese).
    pub message: String,
}

/// Check `code` against the given static constraints.
pub fn check(code: &str, constraints: &[Constraint]) -> Vec<Violation> {
    let mut out = Vec::new();
    let stripped_text = strip_line_comments(code);
    let stripped: Vec<&str> = stripped_text.lines().collect();

    for c in constraints {
        match c {
            Constraint::NoClone | Constraint::NoUnwrap | Constraint::IteratorOnly => {
                let needles: &[&str] = match c {
                    Constraint::NoClone => &[".clone(", ".to_owned("],
                    Constraint::NoUnwrap => &[".unwrap(", ".expect("],
                    Constraint::IteratorOnly => &[],
                    _ => unreachable!(),
                };
                if c == &Constraint::IteratorOnly {
                    for (i, line) in stripped.iter().enumerate() {
                        for kw in ["for", "while", "loop"] {
                            if let Some(col) = find_keyword(line, kw) {
                                out.push(violation(c, i + 1, line, col));
                                break;
                            }
                        }
                    }
                } else {
                    for (i, line) in stripped.iter().enumerate() {
                        for needle in needles {
                            if let Some(col) = line.find(needle) {
                                out.push(violation(c, i + 1, line, col));
                                break;
                            }
                        }
                    }
                }
            }
            Constraint::MaxLines(max) => {
                let effective = stripped.iter().filter(|l| !l.trim().is_empty()).count();
                if effective > *max as usize {
                    out.push(Violation {
                        constraint: c.spec_id(),
                        line: 0,
                        snippet: String::new(),
                        message: format!(
                            "有效代码 {} 行，超过限制 {} 行",
                            effective, max
                        ),
                    });
                }
            }
            Constraint::CustomStatic { ban } => {
                for (i, line) in stripped.iter().enumerate() {
                    for tok in ban {
                        if let Some(col) = line.find(tok.as_str()) {
                            out.push(violation(c, i + 1, line, col));
                            break;
                        }
                    }
                }
            }
        }
    }
    out
}

fn violation(c: &Constraint, line: usize, src_line: &str, _col: usize) -> Violation {
    let mut snippet = src_line.trim().to_string();
    if snippet.chars().count() > 80 {
        snippet = snippet.chars().take(77).collect::<String>() + "...";
    }
    Violation {
        constraint: c.spec_id(),
        line,
        snippet,
        message: format!("违反约束「{}」", c.name_cn()),
    }
}

/// Remove `//` line comments (naive: ignores string literals; documented).
fn strip_line_comments(code: &str) -> String {
    code.lines()
        .map(|l| match l.find("//") {
            Some(idx) if !in_string_literal(&l[..idx]) => &l[..idx],
            _ => l,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Whether `idx` sits inside a double-quoted string literal (rough scan).
fn in_string_literal(prefix: &str) -> bool {
    prefix.matches('"').count() % 2 == 1
}

/// Find a keyword with word-boundary check (prev/next char not alnum/_).
fn find_keyword(line: &str, kw: &str) -> Option<usize> {
    let bytes = line.as_bytes();
    let mut start = 0;
    while let Some(rel) = line[start..].find(kw) {
        let pos = start + rel;
        let end = pos + kw.len();
        let prev_ok = pos == 0 || !is_word_byte(bytes[pos - 1]);
        let next_ok = end >= bytes.len() || !is_word_byte(bytes[end]);
        if prev_ok && next_ok {
            return Some(pos);
        }
        start = pos + 1;
    }
    None
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_clone_catches_clone_and_to_owned() {
        let code = "fn f(s: &str) -> String {\n    let a = s.to_owned();\n    let b = String::from(s).clone();\n    a + &b\n}\n";
        let v = check(code, &[Constraint::NoClone]);
        assert_eq!(v.len(), 2, "{v:?}");
        assert_eq!(v[0].line, 2);
        assert_eq!(v[1].line, 3);
    }

    #[test]
    fn comments_do_not_trigger() {
        let code = "// we could .clone() here but must not\nfn f() {}\n";
        assert!(check(code, &[Constraint::NoClone]).is_empty());
        assert!(check(code, &[Constraint::NoUnwrap]).is_empty());
    }

    #[test]
    fn no_unwrap_catches_expect() {
        let code = "fn f(x: Option<u32>) -> u32 {\n    x.expect(\"boom\")\n}\n";
        let v = check(code, &[Constraint::NoUnwrap]);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].line, 2);
    }

    #[test]
    fn iterator_only_keyword_boundaries() {
        let code = "fn f(v: Vec<u32>) -> u32 {\n    let before = 1;\n    for x in &v { before += x; }\n    while before > 0 { before -= 1; }\n    before\n}\n";
        let v = check(code, &[Constraint::IteratorOnly]);
        // "before" must NOT trigger; for/while must trigger once each line
        assert_eq!(v.len(), 2, "{v:?}");
        assert_eq!(v[0].line, 3);
        assert_eq!(v[1].line, 4);
    }

    #[test]
    fn max_lines_counts_effective_lines() {
        let code = "// header comment\nfn f() {\n    // inner comment\n    1\n}\n\n";
        let v = check(code, &[Constraint::MaxLines(2)]);
        assert_eq!(v.len(), 1);
        assert!(v[0].message.contains("3"), "{v:?}");
        assert!(check(code, &[Constraint::MaxLines(3)]).is_empty());
    }

    #[test]
    fn custom_static_ban() {
        let code = "fn f() -> u32 { unsafe { 1 } }\n";
        let v = check(code, &[Constraint::CustomStatic { ban: vec!["unsafe".into()] }]);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].line, 1);
    }

    #[test]
    fn spec_roundtrip() {
        for c in [
            Constraint::NoClone,
            Constraint::NoUnwrap,
            Constraint::IteratorOnly,
            Constraint::MaxLines(42),
            Constraint::CustomStatic { ban: vec!["unsafe".into(), "impl Trait".into()] },
        ] {
            let spec = c.spec_id();
            assert_eq!(Constraint::from_spec(&spec), Some(c), "spec: {spec}");
        }
        assert!(Constraint::from_spec("unknown-thing").is_none());
        assert!(Constraint::from_spec("max-lines=abc").is_none());
    }

    /// Integration-style: a solution that PASSES its tests but violates
    /// NoClone must be caught by the constraint gate (design §7.4/§8 M2).
    #[test]
    fn passing_solution_with_clone_is_caught() {
        let code = r#"
fn double(v: &[i32]) -> Vec<i32> {
    let owned: Vec<i32> = v.to_owned();
    owned.iter().map(|x| x * 2).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn t() {
        assert_eq!(double(&[1, 2]), vec![2, 4]);
    }
}
"#;
        let v = check(code, &[Constraint::NoClone]);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].constraint, "no-clone");
    }
}
