//! Exercise template library (v3 design §7.2, M3): hand-written TOML
//! files under `templates/` are the deterministic base for question
//! generation. A template is a small Rust snippet with `{{slot}}`
//! placeholders, shared tests, a reference solution and abstract
//! constraints.
//!
//! This module provides:
//! - loading + validating a directory of templates (rule filter from
//!   design §7.4-2: ≤2 concepts, 5–50 body lines, ≤2 todo!-macros,
//!   tests present, slot declarations consistent with placeholders),
//! - rendering (slot filling) with per-kind value validation,
//! - a deterministic default fill so generation also works offline
//!   (no LLM) and retries can rotate through the declared candidates.

use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::constraints::Constraint;

// ---------------------------------------------------------------------------
// Data model
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Difficulty {
    Easy,
    #[default]
    Medium,
    Hard,
}

impl Difficulty {
    pub fn name_cn(&self) -> &'static str {
        match self {
            Difficulty::Easy => "简单",
            Difficulty::Medium => "中等",
            Difficulty::Hard => "困难",
        }
    }

    /// Canonical lowercase key stored in the exercise index / JSON.
    pub fn as_str(&self) -> &'static str {
        match self {
            Difficulty::Easy => "easy",
            Difficulty::Medium => "medium",
            Difficulty::Hard => "hard",
        }
    }
}

/// What kind of value a slot expects (design §7.2: ident | type | literal).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SlotKind {
    /// A Rust identifier (e.g. a variable or function name).
    Ident,
    /// A type expression (e.g. `u32`, `Vec<String>`).
    Type,
    /// A literal / small expression used as a value.
    Literal,
}

impl SlotKind {
    pub fn name_cn(&self) -> &'static str {
        match self {
            SlotKind::Ident => "标识符",
            SlotKind::Type => "类型",
            SlotKind::Literal => "字面量",
        }
    }
}

/// One `{{name}}` hole in the template, with allowed candidate values
/// (for LLM-guided variety and retry rotation) and a deterministic
/// default (offline fallback).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SlotSpec {
    pub name: String,
    pub kind: SlotKind,
    #[serde(default)]
    pub values: Vec<String>,
    #[serde(default)]
    pub default: String,
}

/// Seeds for the M5 review gate (root-cause keywords, misconception
/// options). Optional; templates may omit it.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewHints {
    #[allow(dead_code)] // consumed by the M5 review gate
    #[serde(default)]
    pub root_cause: Vec<String>,
    #[allow(dead_code)] // consumed by the M5 review gate
    #[serde(default)]
    pub misconceptions: Vec<String>,
}

/// A validated, in-memory exercise template.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Template {
    pub id: String,
    /// Short Chinese title (also used as the first comment line of the
    /// generated exercise file).
    pub title: String,
    /// Concept ids from `taxonomy/concepts.toml` (≤2, rule filter).
    pub concepts: Vec<String>,
    /// Error codes this exercise typically triggers when solved wrong.
    #[serde(default)]
    pub error_codes: Vec<String>,
    #[serde(default)]
    pub difficulty: Difficulty,
    /// Instruction comments + code with `{{slot}}` placeholders and a
    /// TODO marker (what the user sees and edits).
    pub body: String,
    /// `#[cfg(test)] mod tests { … }` shared by template and reference.
    pub tests: String,
    /// Reference solution (same placeholders), must pass the triple gate
    /// and its own declared constraints.
    pub reference: String,
    #[serde(default)]
    pub slots: Vec<SlotSpec>,
    /// Constraint spec strings (`no-clone`, `max-lines=25`, …), parsed
    /// via `constraints::Constraint::from_spec`.
    #[serde(default)]
    pub constraints: Vec<String>,
    /// The language-transfer intuition that leads into this trap
    /// (M4.5b, spec C3: "from which language's which habit"). Required
    /// — a template without a confusion story is rarely worth keeping.
    #[allow(dead_code)] // consumed by the M5 review gate / prompt rubric
    pub confusion: String,
    /// Expected non-idiomatic "solutions" this exercise should force
    /// away (spec C4; compared by the M5 review gate).
    #[allow(dead_code)] // consumed by the M5 review gate
    #[serde(default)]
    pub anti_patterns: Vec<String>,
    /// Tiered static hints (M4.8): hints[0] is directional, later ones
    /// more specific. Revealed one per `[h]` press on the exercise
    /// page; the body's TODO must NOT leak them (the decisions stay
    /// with the learner).
    #[serde(default)]
    pub hints: Vec<String>,
    #[allow(dead_code)] // consumed by the M5 review gate
    #[serde(default)]
    pub review_hints: Option<ReviewHints>,
}

/// The result of filling all slots of a template.
#[derive(Debug, Clone, Default)]
pub struct RenderedExercise {
    pub body: String,
    pub tests: String,
    pub reference: String,
}

/// The tier-independent exercise representation (design §7.5, M4.5c):
/// hand-written templates, LLM adaptations and free-form generations
/// all reduce to this shape, and all pass through the same quality
/// gate (`generator::gate_draft`). A `Template` is a draft plus slot
/// specs, an id and authoring metadata (confusion/anti-patterns).
#[derive(Debug, Clone, Default)]
pub struct ExerciseDraft {
    pub title: String,
    /// Concept ids from the taxonomy (≤2, enforced by the rule filter).
    pub concepts: Vec<String>,
    pub error_codes: Vec<String>,
    pub difficulty: Difficulty,
    /// Constraint spec strings (`no-clone`, `max-lines=25`, …).
    pub constraints: Vec<String>,
    /// Instruction comments + code with a TODO marker (what the user
    /// sees and edits).
    pub body: String,
    /// `#[cfg(test)] mod tests { … }` shared by template and reference.
    pub tests: String,
    /// Hidden reference solution (same tests).
    pub reference: String,
}

impl ExerciseDraft {
    /// Assemble the two files the verifier sees (no slot substitution —
    /// drafts are fully concrete).
    pub fn render(&self) -> RenderedExercise {
        RenderedExercise {
            body: self.body.clone(),
            tests: self.tests.clone(),
            reference: self.reference.clone(),
        }
    }
}

impl RenderedExercise {
    /// The file the user edits (body + tests).
    pub fn user_file(&self) -> String {
        format!("{}\n{}\n", self.body.trim_end(), self.tests.trim())
    }

    /// The hidden reference file used by the triple gate (same tests).
    pub fn reference_file(&self) -> String {
        format!("{}\n{}\n", self.reference.trim_end(), self.tests.trim())
    }
}

// ---------------------------------------------------------------------------
// Loading + rule filter
// ---------------------------------------------------------------------------

/// Load every `*.toml` under `dir` (sorted by id). Each file must be a
/// single template and must pass the rule filter.
pub fn load_dir(dir: &Path) -> Result<Vec<Template>> {
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .with_context(|| format!("读取模板目录 {} 失败", dir.display()))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("toml"))
        .collect();
    files.sort();

    let mut out = Vec::new();
    let mut errors = Vec::new();
    for path in files {
        match load_file(&path) {
            Ok(t) => out.push(t),
            Err(e) => errors.push(format!("{}: {e:#}", path.display())),
        }
    }
    if let Some(dup) = find_duplicate_id(&out) {
        errors.push(format!("模板 id 重复: '{dup}'"));
    }
    if !errors.is_empty() {
        bail!(
            "模板库校验失败（{} 个文件）:\n  - {}",
            errors.len(),
            errors.join("\n  - ")
        );
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(out)
}

fn find_duplicate_id(templates: &[Template]) -> Option<String> {
    let mut seen = std::collections::BTreeSet::new();
    for t in templates {
        if !seen.insert(t.id.as_str()) {
            return Some(t.id.clone());
        }
    }
    None
}

/// Load and validate a single template file.
pub fn load_file(path: &Path) -> Result<Template> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("读取 {} 失败", path.display()))?;
    let t: Template =
        toml::from_str(&text).with_context(|| format!("TOML 解析失败（{}）", path.display()))?;
    if t.confusion.trim().is_empty() {
        bail!("模板 '{}' 缺少 confusion 字段（规格 C3：注明源语言直觉 → 掉坑路径）", t.id);
    }
    if t.hints.len() > 3 {
        bail!("模板 '{}' 的 hints 超过 3 条（分级提示：方向→具体→接近签名）", t.id);
    }
    let violations = rule_filter(&t);
    if !violations.is_empty() {
        bail!("规则过滤未通过:\n  - {}", violations.join("\n  - "));
    }
    // Parse constraint spec strings into real constraints (also
    // re-serializable for display).
    let _ = constraints_of(&t)?;
    Ok(t)
}

/// Quality gate, spec C1 machine-checkable part (docs/出题规划_M4.5.md
/// §2, M4.5b): whenever the *unfinished* template fails to COMPILE, the
/// first error code must be one of the declared `error_codes` — this
/// catches "claims to teach E0382, actually fails with E0308"
/// mismatches and structurally broken bodies. Templates whose body
/// compiles and fails via tests (`todo!()` style) carry no compile
/// error, so the check is vacuously satisfied there.
pub fn first_error_matches(error_codes: &[String], report: &crate::verifier::VerifyReport) -> Result<()> {
    let Some(code) = &report.first_error_code else {
        return Ok(());
    };
    if error_codes.is_empty() {
        bail!(
            "未完成模板编译报 {code}，但未声明 error_codes：编译失败型题目必须声明首错误码"
        );
    }
    if error_codes.contains(code) {
        Ok(())
    } else {
        bail!(
            "未完成模板的首错误码 {code} 不在声明的 error_codes {:?} 中（题目声称的坑与实际报错不一致）",
            error_codes
        )
    }
}

/// Parsed constraints of a template (spec strings → `Constraint`).
pub fn constraints_of(t: &Template) -> Result<Vec<Constraint>> {
    let mut out = Vec::new();
    for spec in &t.constraints {
        let c = Constraint::from_spec(spec)
            .with_context(|| format!("模板 '{}' 的约束 '{}' 无法解析", t.id, spec))?;
        out.push(c);
    }
    Ok(out)
}

impl Template {
    /// Extract the shared draft view (§7.5): everything the quality
    /// gate needs, minus template-only authoring metadata.
    pub fn draft(&self) -> ExerciseDraft {
        ExerciseDraft {
            title: self.title.clone(),
            concepts: self.concepts.clone(),
            error_codes: self.error_codes.clone(),
            difficulty: self.difficulty,
            constraints: self.constraints.clone(),
            body: self.body.clone(),
            tests: self.tests.clone(),
            reference: self.reference.clone(),
        }
    }
}

/// Rule filter from design §7.4-2 (static, boolean, no scoring).
/// Returns violation descriptions; empty means the template is fine.
pub fn rule_filter(t: &Template) -> Vec<String> {
    let mut v = rule_filter_draft(&t.draft());

    if t.id.trim().is_empty() {
        v.push("缺少 id".into());
    }

    // Slot declarations vs. placeholders must match exactly, and each
    // declared slot needs a usable default.
    let declared: std::collections::BTreeSet<&str> =
        t.slots.iter().map(|s| s.name.as_str()).collect();
    if declared.len() != t.slots.len() {
        v.push("slots 存在重名".into());
    }
    let mut used = std::collections::BTreeSet::new();
    for text in [&t.body, &t.tests, &t.reference] {
        for name in placeholders(text) {
            if !declared.contains(name.as_str()) {
                v.push(format!("占位符 {{{{{name}}}}} 未在 slots 中声明"));
            }
            used.insert(name);
        }
    }
    for s in &t.slots {
        if !used.contains(s.name.as_str()) {
            v.push(format!("槽位 '{}' 声明了但未被使用", s.name));
        }
        if s.default.trim().is_empty() {
            v.push(format!("槽位 '{}' 缺少 default（离线回退需要）", s.name));
        } else if !s.values.is_empty() && !s.values.iter().any(|x| x == &s.default) {
            v.push(format!("槽位 '{}' 的 default 不在 values 中", s.name));
        }
    }

    v
}

/// Rule-filter core shared by all three generation tiers (M4.5c):
/// every static §7.4-2 check except the template-only id/slots rules.
pub fn rule_filter_draft(d: &ExerciseDraft) -> Vec<String> {
    let mut v = Vec::new();

    if d.title.trim().is_empty() {
        v.push("缺少 title".into());
    }
    if d.concepts.is_empty() {
        v.push("至少标注 1 个概念".into());
    } else if d.concepts.len() > 2 {
        v.push(format!("概念数 {} 超过上限 2", d.concepts.len()));
    }
    for c in &d.concepts {
        if c.trim().is_empty() {
            v.push("存在空概念 id".into());
            break;
        }
    }

    // 5–50 non-empty body lines (instruction comments count: they are
    // part of the rendered snippet; tests are separate and uncounted).
    // 0909_2 反馈：40 行上限曾拒掉 44 行的多结构体场景、GLM 系大题
    // 成批被卡；50 是终端体验红线（题目页整文件渲染，再长就要滚屏）。
    let body_lines = d.body.lines().filter(|l| !l.trim().is_empty()).count();
    if !(5..=50).contains(&body_lines) {
        v.push(format!("body 非空行数 {body_lines} 不在 5–50 范围"));
    }

    let todos = count_todo(&d.body);
    if todos > 2 {
        v.push(format!("todo!/unimplemented! 出现 {todos} 次，超过上限 2"));
    }

    if d.tests.trim().is_empty() || !d.tests.contains("#[test]") {
        v.push("tests 缺失（需要含 #[test] 的测试模块）".into());
    }

    if d.reference.trim().is_empty() {
        v.push("reference 缺失".into());
    } else {
        if count_todo(&d.reference) > 0 {
            v.push("reference 不能包含 todo!/unimplemented!".into());
        }
        if d.reference.contains("I AM NOT DONE") {
            v.push("reference 不能包含 I AM NOT DONE".into());
        }
    }

    v
}

fn count_todo(text: &str) -> usize {
    // Count actual macro invocations, not mentions inside comments.
    let stripped = strip_line_comments(text);
    stripped.matches("todo!").count() + stripped.matches("unimplemented!").count()
}

/// Naive `//` line-comment stripper (same limitation as
/// `constraints::check`: string literals are not parsed).
fn strip_line_comments(code: &str) -> String {
    code.lines()
        .map(|l| match l.find("//") {
            Some(idx) if !l[..idx].matches('"').count() % 2 == 1 => &l[..idx],
            _ => l,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Extract all `{{name}}` placeholder names from a text.
pub fn placeholders(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("{{") {
        let after = &rest[start + 2..];
        if let Some(end) = after.find("}}") {
            let name = after[..end].trim();
            if !name.is_empty() {
                out.push(name.to_string());
            }
            rest = &after[end + 2..];
        } else {
            break;
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Slot filling / rendering
// ---------------------------------------------------------------------------

/// Validate one slot value against its declared kind (light checks; the
/// triple gate is the real safety net).
pub fn validate_slot_value(kind: SlotKind, value: &str) -> Result<()> {
    let bad = |why: &str| anyhow::anyhow!("槽位值 '{value}' 不是合法的{}：{why}", kind.name_cn());
    if value.trim().is_empty() {
        return Err(bad("为空"));
    }
    if value.contains("{{") || value.contains("}}") {
        return Err(bad("包含占位符记号"));
    }
    if value.lines().count() > 1 {
        return Err(bad("包含换行"));
    }
    match kind {
        SlotKind::Ident => {
            let mut chars = value.chars();
            let first = chars.next().unwrap();
            let ok = (first.is_alphabetic() || first == '_')
                && chars.all(|c| c.is_alphanumeric() || c == '_');
            if !ok {
                return Err(bad("标识符需以字母/下划线开头且只含字母数字下划线"));
            }
        }
        SlotKind::Type => {
            let mut depth: i32 = 0;
            for c in value.chars() {
                match c {
                    '<' => depth += 1,
                    '>' => {
                        depth -= 1;
                        if depth < 0 {
                            return Err(bad("尖括号不配对"));
                        }
                    }
                    _ => {}
                }
            }
            if depth != 0 {
                return Err(bad("尖括号不配对"));
            }
        }
        SlotKind::Literal => {}
    }
    Ok(())
}

/// Deterministic fill for attempt `attempt` (0-based). Attempt 0 uses
/// each slot's `default`; later attempts rotate through `values` so
/// retries naturally try different variations without an LLM.
pub fn fill_for_attempt(
    t: &Template,
    attempt: usize,
) -> std::collections::BTreeMap<String, String> {
    let mut out = std::collections::BTreeMap::new();
    for (i, slot) in t.slots.iter().enumerate() {
        let value = if attempt == 0 || slot.values.is_empty() {
            slot.default.clone()
        } else {
            // Rotate deterministically; attempt N tries a fresh combo.
            let idx = (attempt + i) % slot.values.len();
            slot.values[idx].clone()
        };
        out.insert(slot.name.clone(), value);
    }
    out
}

/// Render a template with the given slot values. Errors on unknown
/// slots, missing values, invalid values, or leftover placeholders.
pub fn render(
    t: &Template,
    values: &std::collections::BTreeMap<String, String>,
) -> Result<RenderedExercise> {
    for slot in &t.slots {
        let Some(v) = values.get(&slot.name) else {
            bail!("槽位 '{}' 没有提供值", slot.name);
        };
        validate_slot_value(slot.kind, v)
            .with_context(|| format!("模板 '{}' 的槽位 '{}'", t.id, slot.name))?;
    }
    for name in values.keys() {
        if !t.slots.iter().any(|s| &s.name == name) {
            bail!("槽位 '{}' 未在模板 '{}' 中声明", name, t.id);
        }
    }

    let fill = |text: &str| -> Result<String> {
        let mut out = text.to_string();
        for (k, v) in values {
            out = out.replace(&format!("{{{{{k}}}}}"), v);
        }
        // Also tolerate spaces inside braces, e.g. {{ name }}.
        if let Some(left) = leftovers(&out) {
            bail!("模板 '{}' 填充后仍有占位符: {{{{{left}}}}}", t.id);
        }
        Ok(out)
    };

    Ok(RenderedExercise {
        body: fill(&t.body)?,
        tests: fill(&t.tests)?,
        reference: fill(&t.reference)?,
    })
}

/// After substitution, find the first leftover `{{…}}` (if any). An
/// unterminated `{{` (no closing `}}`) is not counted.
fn leftovers(text: &str) -> Option<String> {
    let start = text.find("{{")?;
    let after = &text[start + 2..];
    let end = after.find("}}")?;
    Some(after[..end].trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
id = "own-move-struct"
title = "所有权移动：把值交给函数"
concepts = ["ownership.move"]
error_codes = ["E0382"]
difficulty = "easy"
constraints = ["no-clone", "max-lines=30"]
confusion = "Python/Java 的赋值是引用语义，初学者以为把 String 传进函数后原变量仍可用"

body = '''
// 把一个 String 交给 summarize，之后再使用它会触发 E0382。
// 请通过借用（&）或重建所有权来修复。
struct Item {
    name: String,
}

fn summarize(item: &Item) -> String {
    // TODO: 返回 item.name 的格式化描述
    format!("{}: {}", "{{prefix}}", item.name)
}
// I AM NOT DONE
'''

tests = '''
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summarizes() {
        let it = Item { name: "pen".to_string() };
        assert_eq!(summarize(&it), "item: pen");
        // name 仍可用：说明没有发生移动
        assert_eq!(it.name, "pen");
    }
}
'''

reference = '''
struct Item {
    name: String,
}

fn summarize(item: &Item) -> String {
    format!("{}: {}", "{{prefix}}", item.name)
}
'''

[[slots]]
name = "prefix"
kind = "literal"
values = ["item", "thing"]
default = "item"

[review_hints]
root_cause = ["借用 vs 移动"]
misconceptions = ["以为 String 赋值会深拷贝"]
"#;

    fn sample() -> Template {
        toml::from_str(SAMPLE).unwrap()
    }

    #[test]
    fn parses_template_and_constraints() {
        let t = sample();
        assert_eq!(t.id, "own-move-struct");
        assert_eq!(t.difficulty, Difficulty::Easy);
        assert_eq!(t.slots.len(), 1);
        assert_eq!(t.slots[0].kind, SlotKind::Literal);
        let cs = constraints_of(&t).unwrap();
        assert_eq!(cs.len(), 2);
        assert!(rule_filter(&t).is_empty());
    }

    #[test]
    fn rule_filter_flags_each_violation() {
        let mut t = sample();

        t.concepts = vec!["a".into(), "b".into(), "c".into()];
        assert!(rule_filter(&t)[0].contains("概念数"));

        t.concepts.clear();
        assert!(rule_filter(&t).iter().any(|s| s.contains("至少标注")));

        let mut t = sample();
        t.body = "too short".into();
        assert!(rule_filter(&t).iter().any(|s| s.contains("5–50")));

        let mut t = sample();
        t.tests = "no tests here".into();
        assert!(rule_filter(&t).iter().any(|s| s.contains("tests 缺失")));

        let mut t = sample();
        t.reference = "let x = todo!();".into();
        assert!(
            rule_filter(&t)
                .iter()
                .any(|s| s.contains("reference 不能包含"))
        );

        let mut t = sample();
        t.body = t.body.replace("{{prefix}}", "{{ghost}}");
        assert!(rule_filter(&t).iter().any(|s| s.contains("ghost")));

        let mut t = sample();
        t.slots[0].default = String::new();
        assert!(rule_filter(&t).iter().any(|s| s.contains("default")));

        let mut t = sample();
        t.slots[0].values = vec!["other".into()];
        assert!(
            rule_filter(&t)
                .iter()
                .any(|s| s.contains("default 不在 values"))
        );
    }

    #[test]
    fn placeholder_extraction() {
        assert_eq!(placeholders("a {{x}} b {{ y }} c {{ }} d"), vec!["x", "y"]);
        assert_eq!(placeholders("no slots"), Vec::<String>::new());
    }

    #[test]
    fn render_with_defaults_and_validation() {
        let t = sample();
        let values = fill_for_attempt(&t, 0);
        let r = render(&t, &values).unwrap();
        assert!(r.body.contains(r#"format!("{}: {}", "item""#));
        assert!(r.reference.contains(r#"format!("{}: {}", "item""#));
        assert!(r.user_file().contains("mod tests"));
        assert!(r.reference_file().ends_with("}\n"));
    }

    #[test]
    fn render_rejects_missing_unknown_and_invalid_values() {
        let t = sample();

        // Missing slot value.
        assert!(render(&t, &Default::default()).is_err());

        // Unknown slot.
        let mut values = fill_for_attempt(&t, 0);
        values.insert("ghost".into(), "x".into());
        assert!(render(&t, &values).is_err());

        // Invalid ident value (spaces / leading digit).
        let mut t2 = sample();
        t2.slots[0].kind = SlotKind::Ident;
        let mut v2 = fill_for_attempt(&t2, 0);
        v2.insert("prefix".into(), "9bad".into());
        assert!(render(&t2, &v2).is_err());
        v2.insert("prefix".into(), "ok_name".into());
        assert!(render(&t2, &v2).is_ok());
    }

    #[test]
    fn slot_kind_validation() {
        assert!(validate_slot_value(SlotKind::Ident, "_a1").is_ok());
        assert!(validate_slot_value(SlotKind::Ident, "1a").is_err());
        assert!(validate_slot_value(SlotKind::Ident, "a b").is_err());
        assert!(validate_slot_value(SlotKind::Type, "Vec<HashMap<u8, String>>").is_ok());
        assert!(validate_slot_value(SlotKind::Type, "Vec<u8").is_err());
        assert!(validate_slot_value(SlotKind::Type, "u8>").is_err());
        assert!(validate_slot_value(SlotKind::Literal, "42").is_ok());
        assert!(validate_slot_value(SlotKind::Literal, "").is_err());
        assert!(validate_slot_value(SlotKind::Type, "a\nb").is_err());
        assert!(validate_slot_value(SlotKind::Type, "x{{y}}").is_err());
    }

    #[test]
    fn attempt_rotation_varies_values() {
        let t = sample();
        let a0 = fill_for_attempt(&t, 0);
        let a1 = fill_for_attempt(&t, 1);
        assert_eq!(a0["prefix"], "item");
        assert_eq!(a1["prefix"], "thing");
        // Attempt 3 wraps back around (2 values).
        assert_eq!(fill_for_attempt(&t, 3)["prefix"], "thing");
    }

    #[test]
    fn leftover_placeholders_are_errors() {
        let t = sample();
        let mut values = fill_for_attempt(&t, 0);
        values.remove("prefix");
        assert!(render(&t, &values).is_err());

        // A value that itself reintroduces braces is caught.
        let mut values = fill_for_attempt(&t, 0);
        values.insert("prefix".into(), "{{prefix}}".into());
        assert!(render(&t, &values).is_err());
    }

    #[test]
    fn load_dir_reports_errors() {
        let dir = std::env::temp_dir().join(format!("rustlings_tpl_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        std::fs::write(dir.join("good.toml"), SAMPLE).unwrap();
        std::fs::write(dir.join("bad.toml"), "id = \"broken\"\nbody = \"x\"\n").unwrap();
        std::fs::write(dir.join("ignore.txt"), "not toml").unwrap();

        let err = load_dir(&dir).unwrap_err();
        assert!(err.to_string().contains("bad.toml"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn difficulty_names() {
        assert_eq!(Difficulty::Easy.name_cn(), "简单");
        assert_eq!(Difficulty::Medium.name_cn(), "中等");
        assert_eq!(Difficulty::Hard.name_cn(), "困难");
        let t: Template = toml::from_str(SAMPLE).unwrap();
        assert_eq!(t.difficulty, Difficulty::Easy);
        // Schema v2 (M4.5b): confusion is required, anti_patterns default.
        assert!(t.confusion.contains("引用语义"));
        assert!(t.anti_patterns.is_empty());
    }

    fn sample_template() -> Template {
        toml::from_str(SAMPLE).unwrap()
    }

    #[test]
    fn first_error_gate_accepts_declared_codes() {
        let t = sample_template(); // declares E0382
        let ok = crate::verifier::VerifyReport {
            first_error_code: Some("E0382".into()),
            ..Default::default()
        };
        assert!(first_error_matches(&t.error_codes, &ok).is_ok());

        let mismatch = crate::verifier::VerifyReport {
            first_error_code: Some("E0308".into()),
            ..Default::default()
        };
        let err = first_error_matches(&t.error_codes, &mismatch).unwrap_err().to_string();
        assert!(err.contains("E0308"), "{err}");
        assert!(err.contains("不一致"), "{err}");
    }

    #[test]
    fn first_error_gate_requires_codes_when_compile_fails() {
        let t = sample_template();
        // Empty codes + a compile error → must declare them instead.
        let mut no_codes = t.clone();
        no_codes.error_codes = vec![];
        let report = crate::verifier::VerifyReport {
            first_error_code: Some("E0382".into()),
            ..Default::default()
        };
        let err = first_error_matches(&no_codes.error_codes, &report).unwrap_err().to_string();
        assert!(err.contains("必须声明首错误码"), "{err}");

        // Test-failure-shaped template (todo!() style) → no compile
        // error, gate vacuously satisfied regardless of codes.
        let todo_report = crate::verifier::VerifyReport::default();
        assert!(first_error_matches(&t.error_codes, &todo_report).is_ok());
        assert!(first_error_matches(&no_codes.error_codes, &todo_report).is_ok());
    }

    #[test]
    fn missing_confusion_rejected_at_load() {
        let stripped = SAMPLE.replace(
            "confusion = \"Python/Java 的赋值是引用语义，初学者以为把 String 传进函数后原变量仍可用\"\n",
            "",
        );
        let dir = std::env::temp_dir().join(format!("rs_tpl_cf_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("broken.toml");
        std::fs::write(&p, &stripped).unwrap();
        let err = format!("{:#}", load_file(&p).unwrap_err());
        assert!(err.contains("confusion"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Repo-level fixture test: the whole hand-written template library
    /// must load cleanly, reference valid taxonomy concepts/codes, and —
    /// most importantly — every template must pass the triple gate with
    /// its default slot fill (design §7.4 gates 1–3). Every slot
    /// rotation (attempt 1..4) must ALSO pass the full gate — this is
    /// the data-side guard against the M3 "hardcoded expectation"
    /// bug class (expectations must be computed from the slots).
    #[test]
    fn repo_template_library_is_consistent_and_valid() {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let templates = load_dir(&root.join("templates")).unwrap();
        assert!(
            templates.len() >= 12,
            "模板库应 ≥12 个（M3 首批 12 + M4.5d 扩容），实际 {}",
            templates.len()
        );
        let graph =
            crate::taxonomy::ConceptGraph::load(&root.join("taxonomy/concepts.toml")).unwrap();
        assert!(
            graph.len() >= 50,
            "概念图谱应 ≥50 节点（M4.5d 扩容后），实际 {}",
            graph.len()
        );

        // Link templates into the graph; dangling references fail load.
        let items: Vec<(&str, &[String])> = templates
            .iter()
            .map(|t| (t.id.as_str(), t.concepts.as_slice()))
            .collect();
        let mut linked = graph.clone();
        linked.link_templates(items).unwrap();

        // Every template error code must exist somewhere in the graph.
        let all_codes = linked.all_error_codes();
        for t in &templates {
            for code in &t.error_codes {
                assert!(
                    all_codes.contains(code.as_str()),
                    "模板 {} 的错误码 {code} 不在概念图谱中",
                    t.id
                );
            }
        }

        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let wd = std::env::temp_dir().join(format!("rustlings_tpl_lib_{nanos}"));
        std::fs::create_dir_all(&wd).unwrap();

        for t in &templates {
            let cs = constraints_of(t).unwrap();

            // Every slot rotation must pass the FULL gate: the test
            // expectations must be computed from the slots, not pinned
            // to the default values (M3 lesson, now enforced).
            for attempt in 0..4 {
                let vals = fill_for_attempt(t, attempt);
                let r = render(t, &vals).unwrap();

                let v = crate::constraints::check(&r.reference_file(), &cs);
                assert!(
                    v.is_empty(),
                    "模板 {} 第 {attempt} 次填槽的参考解违反自身约束: {v:?}",
                    t.id
                );
                let report =
                    crate::verifier::verify_exercise(&r.user_file(), &r.reference_file(), &wd)
                        .unwrap();
                assert!(
                    report.all_pass(),
                    "模板 {} 第 {attempt} 次填槽未通过本地校验: {report:?}",
                    t.id
                );
                first_error_matches(&t.error_codes, &report).unwrap_or_else(|e| {
                    panic!("模板 {} 第 {attempt} 次填槽首错误码不一致: {e:#}", t.id)
                });
            }
        }
        let _ = std::fs::remove_dir_all(&wd);
    }

    /// Reference solutions must be lint-clean (9.5 试用反馈：参考解自带
    /// clippy lint 会在解答评审里如实报出，且教坏学习者)。机器可查 → 回归
    /// 测试兜底；3 处既有违例已随本测试落地修复。
    #[test]
    fn repo_reference_solutions_are_clippy_clean() {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let templates = load_dir(&root.join("templates")).unwrap();
        let mut offenders: Vec<String> = Vec::new();
        for t in &templates {
            if !crate::review::clippy_available() {
                eprintln!("  （clippy-driver 不可用，跳过参考解 lint 检查）");
                return;
            }
            let lints = crate::review::run_clippy(&t.reference).unwrap_or_default();
            if !lints.is_empty() {
                let names: Vec<String> =
                    lints.iter().map(|l| l.lint.clone()).collect();
                offenders.push(format!("{}: {}", t.id, names.join(", ")));
            }
        }
        assert!(
            offenders.is_empty(),
            "以下模板的参考解带 clippy lint:\n  {}",
            offenders.join("\n  ")
        );
    }
}
