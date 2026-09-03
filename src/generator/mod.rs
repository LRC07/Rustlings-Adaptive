//! Exercise generator (v3 design §8 M3): picks a template for the
//! user's topic (concept id / rustc error code / free text), fills its
//! slots (LLM when available, deterministic rotation otherwise), and
//! gates every candidate through the triple verification plus the
//! template's own constraints before writing it into `exercises/
//! generated/` and wiring it into the IDE-only `lib.rs`.
//!
//! The LLM is optional: with `llm: None` generation is fully offline
//! (defaults + candidate rotation), which keeps tests cheap and the
//! tool usable without a key.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;

use crate::llm::LlmReply;
use crate::taxonomy::ConceptGraph;
use crate::{constraints, template, verifier};

/// How many slot-fill/verify rounds before giving up.
pub const MAX_ATTEMPTS: u32 = 3;
/// Category directory (under `exercises/`) for generated exercises.
pub const OUT_CATEGORY: &str = "generated";

/// Filesystem layout used by one generation run (parameterizable for
/// tests; the CLI passes the repo root).
#[derive(Debug, Clone)]
pub struct Paths {
    pub templates_dir: PathBuf,
    pub taxonomy_file: PathBuf,
    pub exercises_dir: PathBuf,
    pub lib_rs: PathBuf,
}

impl Paths {
    pub fn from_root(root: &Path) -> Self {
        Self {
            templates_dir: root.join("templates"),
            taxonomy_file: root.join("taxonomy").join("concepts.toml"),
            exercises_dir: root.join("exercises"),
            lib_rs: root.join("exercises").join("lib.rs"),
        }
    }
}

/// What the user asked for.
#[derive(Debug, Clone)]
pub enum Topic {
    /// Concept id (or unique name/suffix) from the taxonomy.
    Concept(String),
    /// rustc error code like `E0382` (routed via the reverse index).
    ErrorCode(String),
    /// Free-form text (LLM picks the template, keyword scoring falls back).
    FreeText(String),
}

impl Topic {
    fn prompt_text(&self) -> String {
        match self {
            Topic::Concept(c) => format!("概念：{c}"),
            Topic::ErrorCode(c) => format!("错误码：{c}"),
            Topic::FreeText(t) => t.clone(),
        }
    }
}

/// Result of a successful generation.
#[derive(Debug, Clone)]
pub struct Outcome {
    pub path: PathBuf,
    /// Exercise name == file stem == rust module name.
    pub name: String,
    pub template_id: String,
    pub title: String,
    pub concepts: Vec<String>,
    pub difficulty: template::Difficulty,
    pub slots: std::collections::BTreeMap<String, String>,
    pub attempts: u32,
    /// Whether any LLM call actually succeeded (selection or fill).
    pub used_llm: bool,
}

/// Blocking LLM caller provided by the CLI (handles config/budget/usage).
/// Implemented for any `FnMut` closure.
pub trait LlmCaller {
    fn call(&mut self, prompt: &str) -> Result<LlmReply>;
}

impl<F: FnMut(&str) -> Result<LlmReply>> LlmCaller for F {
    fn call(&mut self, prompt: &str) -> Result<LlmReply> {
        self(prompt)
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub fn generate(topic: &Topic, paths: &Paths, mut llm: Option<&mut dyn LlmCaller>) -> Result<Outcome> {
    let graph = ConceptGraph::load(&paths.taxonomy_file)
        .context("概念图谱加载失败")?;
    let mut graph = graph;
    let templates = template::load_dir(&paths.templates_dir)
        .context("模板库加载失败")?;
    if templates.is_empty() {
        bail!("模板库为空（{}）", paths.templates_dir.display());
    }

    let items: Vec<(&str, &[String])> =
        templates.iter().map(|t| (t.id.as_str(), t.concepts.as_slice())).collect();
    graph.link_templates(items).context("模板与概念图谱对不上")?;

    let sel_call: Option<&mut dyn LlmCaller> = match llm.as_mut() {
        Some(c) => Some(&mut **c),
        None => None,
    };
    let (t, used_llm_select) = choose_template(&templates, &graph, topic, sel_call)?;

    // Parse constraints once (spec strings were validated at load).
    let constraint_list = template::constraints_of(&t)?;

    let mut last_fail = String::from("尚未尝试");
    let mut used_llm = used_llm_select;
    for attempt in 0..MAX_ATTEMPTS {
        // Base values: defaults (attempt 0) then deterministic rotation.
        let mut values = template::fill_for_attempt(&t, attempt as usize);

        // LLM fills slots on the first two attempts; later attempts rely
        // on rotation so a stuck LLM cannot loop forever.
        if attempt <= 1 {
            let call: Option<&mut dyn LlmCaller> = match llm.as_mut() {
                Some(c) => Some(&mut **c),
                None => None,
            };
            if let Some(call) = call {
                match llm_fill_slots(&t, &topic.prompt_text(), Some(&last_fail), call) {
                    Ok(filled) => {
                        for (k, v) in filled {
                            values.insert(k, v);
                        }
                        used_llm = true;
                    }
                    Err(_) => {} // fall back silently; verify is the gate
                }
            }
        }

        let rendered = match template::render(&t, &values) {
            Ok(r) => r,
            Err(e) => {
                last_fail = format!("填槽渲染失败：{e:#}");
                continue;
            }
        };

        // Constraint self-consistency (design §7.4 gate 3).
        let violations = constraints::check(&rendered.reference_file(), &constraint_list);
        if !violations.is_empty() {
            let msgs: Vec<String> = violations.iter().map(|v| v.message.clone()).collect();
            last_fail = format!("参考解违反约束：{}", msgs.join("；"));
            continue;
        }

        // Triple verification (design §7.4 gate 1).
        let workdir = fresh_workdir("rustlings_generate")?;
        let report = match verifier::verify_exercise(
            &rendered.user_file(),
            &rendered.reference_file(),
            &workdir,
        ) {
            Ok(r) => r,
            Err(e) => {
                let _ = fs::remove_dir_all(&workdir);
                last_fail = format!("校验执行失败：{e:#}");
                continue;
            }
        };
        let _ = fs::remove_dir_all(&workdir);

        if report.all_pass() {
            let name = write_exercise(paths, &t, &rendered)?;
            return Ok(Outcome {
                path: paths.exercises_dir.join(OUT_CATEGORY).join(format!("{name}.rs")),
                name,
                template_id: t.id.clone(),
                title: t.title.clone(),
                concepts: t.concepts.clone(),
                difficulty: t.difficulty,
                slots: values,
                attempts: attempt + 1,
                used_llm,
            });
        }
        last_fail = failure_reason(&report);
    }

    bail!("连续 {MAX_ATTEMPTS} 次生成均未通过校验（模板 {}）。最后失败原因：{last_fail}", t.id)
}

// ---------------------------------------------------------------------------
// Template selection
// ---------------------------------------------------------------------------

fn choose_template<'a>(
    templates: &'a [template::Template],
    graph: &ConceptGraph,
    topic: &Topic,
    llm: Option<&mut dyn LlmCaller>,
) -> Result<(&'a template::Template, bool)> {
    match topic {
        Topic::Concept(q) => {
            let id = graph
                .resolve(q)
                .ok_or_else(|| anyhow!("无法把「{q}」解析为概念；可用概念如：{}",
                    graph.ids().take(5).cloned().collect::<Vec<_>>().join("、")))?;
            let mut ids = BTreeSet::new();
            collect_concept_templates(graph, id, &mut ids);
            let t = pick_by_ids(templates, &ids)
                .ok_or_else(|| anyhow!("概念「{id}」还没有可用模板"))?;
            Ok((t, false))
        }
        Topic::ErrorCode(code) => {
            let concepts = graph.concepts_for_code(code);
            if concepts.is_empty() {
                bail!("错误码 {code} 不在概念图谱中，无法反查模板");
            }
            let mut ids = BTreeSet::new();
            for c in &concepts {
                collect_concept_templates(graph, c, &mut ids);
            }
            let t = pick_by_ids(templates, &ids)
                .ok_or_else(|| anyhow!("错误码 {code} 相关概念还没有可用模板"))?;
            Ok((t, false))
        }
        Topic::FreeText(text) => {
            // LLM first (understands loose Chinese requests), then a
            // deterministic keyword score over taxonomy names + titles.
            if let Some(call) = llm {
                if let Ok(Some(t)) = llm_pick_template(templates, text, call) {
                    return Ok((t, true));
                }
            }
            let ids = keyword_candidates(templates, graph, text);
            if let Some(t) = pick_by_ids(templates, &ids) {
                return Ok((t, false));
            }
            bail!(
                "没能根据「{text}」挑出模板；试试概念（如 trait.associated-types）、\
                 错误码（如 E0382）或更具体的关键词"
            )
        }
    }
}

/// All template ids covering `concept_id` and (transitively) its children.
fn collect_concept_templates(graph: &ConceptGraph, concept_id: &str, out: &mut BTreeSet<String>) {
    if let Some(node) = graph.get(concept_id) {
        out.extend(node.templates.iter().cloned());
        for child in graph.children_of(concept_id) {
            collect_concept_templates(graph, child, out);
        }
    }
}

fn pick_by_ids<'a>(
    templates: &'a [template::Template],
    ids: &BTreeSet<String>,
) -> Option<&'a template::Template> {
    templates.iter().find(|t| ids.contains(&t.id))
}

/// Deterministic free-text matching: taxonomy concept names/ids first
/// (Chinese keywords live there), then template titles/ids/codes.
fn keyword_candidates(
    templates: &[template::Template],
    graph: &ConceptGraph,
    text: &str,
) -> BTreeSet<String> {
    let needles: Vec<String> = text
        .split(|c: char| c.is_whitespace() || "，。、？！,.:?？()（）".contains(c))
        .map(str::trim)
        .filter(|w| w.chars().count() >= 2)
        .map(|w| w.to_lowercase())
        .collect();

    let mut ids = BTreeSet::new();
    for node in graph.ids().filter_map(|id| graph.get(id)) {
        let hay = format!("{} {}", node.id, node.name).to_lowercase();
        if needles.iter().any(|n| hay.contains(n)) {
            collect_concept_templates(graph, &node.id, &mut ids);
        }
    }
    if !ids.is_empty() {
        return ids;
    }
    for t in templates {
        let hay = format!("{} {} {}", t.id, t.title, t.error_codes.join(" ")).to_lowercase();
        if needles.iter().any(|n| hay.contains(n)) {
            ids.insert(t.id.clone());
        }
    }
    ids
}

// ---------------------------------------------------------------------------
// LLM helpers
// ---------------------------------------------------------------------------

fn llm_pick_template<'a>(
    templates: &'a [template::Template],
    request: &str,
    call: &mut dyn LlmCaller,
) -> Result<Option<&'a template::Template>> {
    let catalog: String = templates
        .iter()
        .map(|t| {
            format!(
                "- {} | {} | {} | {}",
                t.id,
                t.title,
                t.concepts.join(","),
                t.error_codes.join(",")
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let prompt = format!(
        "You are choosing a Rust practice exercise template for a learner.\n\n\
         User request: {request}\n\n\
         Available templates (id | title | concepts | error-codes):\n{catalog}\n\n\
         Answer with ONLY a JSON object: {{\"template_id\": \"<id>\"}}"
    );
    let reply = call.call(&prompt)?;
    let Some(json) = extract_json(&reply.content) else {
        return Ok(None);
    };
    let id = serde_json::from_str::<Value>(json)
        .ok()
        .and_then(|v| v.get("template_id")?.as_str().map(str::to_string));
    Ok(id.and_then(|id| templates.iter().find(|t| t.id == id)))
}

fn llm_fill_slots(
    t: &template::Template,
    request: &str,
    last_fail: Option<&str>,
    call: &mut dyn LlmCaller,
) -> Result<std::collections::BTreeMap<String, String>> {
    if t.slots.is_empty() {
        return Err(anyhow!("模板没有槽位"));
    }
    let spec: String = t
        .slots
        .iter()
        .map(|s| {
            format!(
                "- {} | kind={} | allowed: [{}] | default: {}",
                s.name,
                s.kind.name_cn(),
                if s.values.is_empty() { "(free)".to_string() } else { s.values.join(", ") },
                s.default
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let retry_note = last_fail
        .filter(|f| *f != "尚未尝试")
        .map(|f| format!("\nA previous fill failed: {f}\nChoose DIFFERENT values this time.\n"))
        .unwrap_or_default();
    let prompt = format!(
        "You are generating a Rust practice exercise by filling named slots.\n\n\
         Template: {} ({})\nUser request: {request}\n{retry_note}\n\
         Slots (name | kind | allowed values | default):\n{spec}\n\n\
         Rules: every value MUST come from the allowed list when one is given; \
         keep values short and valid Rust for their kind.\n\
         Answer with ONLY a JSON object: {{\"slots\": {{\"<name>\": \"<value>\"}}}}",
        t.id, t.title
    );
    let reply = call.call(&prompt)?;
    let json = extract_json(&reply.content).ok_or_else(|| anyhow!("回复中没有 JSON"))?;
    let v: Value = serde_json::from_str(json).context("槽位 JSON 解析失败")?;
    let map = v
        .get("slots")
        .and_then(|s| s.as_object())
        .ok_or_else(|| anyhow!("槽位 JSON 缺少 slots 对象"))?;

    let mut out = std::collections::BTreeMap::new();
    for (name, value) in map {
        let Some(val) = value.as_str() else { continue };
        let Some(spec) = t.slots.iter().find(|s| &s.name == name) else { continue };
        // Keep the deterministic base when the LLM strays outside the
        // declared candidates (defence against hallucinated values).
        if !spec.values.is_empty() && !spec.values.iter().any(|x| x == val) {
            continue;
        }
        template::validate_slot_value(spec.kind, val)?;
        out.insert(name.clone(), val.to_string());
    }
    if out.is_empty() {
        return Err(anyhow!("LLM 没有给出可用槽位值"));
    }
    Ok(out)
}

/// Extract the outermost JSON object substring from an LLM reply.
fn extract_json(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    (end >= start).then_some(&text[start..=end])
}

// ---------------------------------------------------------------------------
// Output: exercise file + lib.rs wiring
// ---------------------------------------------------------------------------

fn sanitize_module_name(id: &str) -> String {
    let name: String = id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let name = name.trim_matches('_').to_string();
    if name.is_empty() || name.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        format!("ex_{name}")
    } else {
        name
    }
}

/// First free file name: `<base>.rs`, then `<base>_2.rs`, `<base>_3.rs`…
fn pick_name(dir: &Path, base: &str) -> String {
    if !dir.join(format!("{base}.rs")).exists() {
        return base.to_string();
    }
    for n in 2.. {
        let candidate = format!("{base}_{n}");
        if !dir.join(format!("{candidate}.rs")).exists() {
            return candidate;
        }
    }
    unreachable!("pick_name loop always returns")
}

fn write_exercise(
    paths: &Paths,
    t: &template::Template,
    rendered: &template::RenderedExercise,
) -> Result<String> {
    let dir = paths.exercises_dir.join(OUT_CATEGORY);
    fs::create_dir_all(&dir).with_context(|| format!("创建 {} 失败", dir.display()))?;
    let name = pick_name(&dir, &sanitize_module_name(&t.id));
    let path = dir.join(format!("{name}.rs"));
    let content = format!(
        "// {}\n//\n{}\n\n{}\n",
        t.title,
        rendered.body.trim_end(),
        rendered.tests.trim()
    );
    fs::write(&path, content).with_context(|| format!("写入 {} 失败", path.display()))?;
    wire_lib_rs(&paths.lib_rs, &name, OUT_CATEGORY)
        .with_context(|| format!("接线 {} 失败", paths.lib_rs.display()))?;
    Ok(name)
}

/// Append an IDE-only module entry for `module` to the exercises
/// `lib.rs` (idempotent). rust-analyzer then analyzes the generated
/// exercise; cargo never compiles it (cfg-gated).
pub fn wire_lib_rs(lib_rs: &Path, module: &str, category: &str) -> Result<()> {
    let content = fs::read_to_string(lib_rs).unwrap_or_default();
    let marker = format!("mod {module};");
    if content.lines().any(|l| l.trim() == marker) {
        return Ok(());
    }
    let mut new = content;
    if !new.is_empty() && !new.ends_with('\n') {
        new.push('\n');
    }
    new.push_str(&format!(
        "\n#[cfg(rust_analyzer)]\n#[path = \"{category}/{module}.rs\"]\nmod {module};\n"
    ));
    fs::write(lib_rs, new).with_context(|| format!("写入 {} 失败", lib_rs.display()))
}

fn failure_reason(report: &verifier::VerifyReport) -> String {
    if !report.compiles {
        "参考解编译失败".to_string()
    } else if !report.ref_solution_passes {
        "参考解未通过全部测试".to_string()
    } else if !report.template_fails {
        "未完成的模板也能通过测试（题目没有区分度）".to_string()
    } else {
        "未知原因".to_string()
    }
}

fn fresh_workdir(tag: &str) -> Result<PathBuf> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("{tag}_{nanos}"));
    fs::create_dir_all(&dir).context("创建临时校验目录失败")?;
    Ok(dir)
}

// ---------------------------------------------------------------------------
// Tests (offline; a mini template + mini taxonomy in a temp repo)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const MINI_TAXONOMY: &str = r#"
[[concept]]
id = "test"
name = "测试根"

[[concept]]
id = "test.concept"
name = "测试概念"
parents = ["test"]
error_codes = ["E0308"]
"#;

    const MINI_TEMPLATE: &str = r#"
id = "mini-add"
title = "迷你加法"
concepts = ["test.concept"]
error_codes = ["E0308"]
difficulty = "easy"
constraints = ["no-clone"]

body = '''
// 两个 i32 相加的小练习。
// add 目前只有占位宏，测试会 panic。
// TODO: 把占位换成 a + b。
// 提示 1：完成后两个测试都应通过。
// 提示 2：本题也用于生成器的离线冒烟测试。
// 说明 1：无槽位，默认值即可通过三重校验。
// 说明 2：练习文件会写入 generated 分类并接线 lib.rs。
fn add(a: i32, b: i32) -> i32 {
    todo!()
}
// I AM NOT DONE
'''

tests = '''
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds() {
        assert_eq!(add(1, 2), 3);
    }

    #[test]
    fn adds_negative() {
        assert_eq!(add(-1, 1), 0);
    }
}
'''

reference = '''
fn add(a: i32, b: i32) -> i32 {
    a + b
}
'''
"#;

    struct Fixture {
        root: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir().join(format!("rustlings_gen_fx_{nanos}"));
            fs::create_dir_all(root.join("templates")).unwrap();
            fs::create_dir_all(root.join("taxonomy")).unwrap();
            fs::create_dir_all(root.join("exercises")).unwrap();
            fs::write(root.join("templates/mini-add.toml"), MINI_TEMPLATE).unwrap();
            fs::write(root.join("taxonomy/concepts.toml"), MINI_TAXONOMY).unwrap();
            fs::write(
                root.join("exercises/lib.rs"),
                "//! fixture lib\n#[cfg(rust_analyzer)]\n#[path = \"seed.rs\"]\nmod seed;\n",
            )
            .unwrap();
            Self { root }
        }

        fn paths(&self) -> Paths {
            Paths::from_root(&self.root)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn generates_with_defaults_offline() {
        let fx = Fixture::new();
        let paths = fx.paths();
        let out = generate(&Topic::Concept("test.concept".into()), &paths, None).unwrap();

        assert_eq!(out.name, "mini_add");
        assert_eq!(out.template_id, "mini-add");
        assert!(!out.used_llm);
        assert!(out.slots.is_empty());
        assert!(out.path.exists(), "{}", out.path.display());

        let src = fs::read_to_string(&out.path).unwrap();
        assert!(src.starts_with("// 迷你加法"));
        assert!(src.contains("#[cfg(test)]"));
        assert!(src.contains("I AM NOT DONE"));

        // Wired into lib.rs for rust-analyzer.
        let lib = fs::read_to_string(&paths.lib_rs).unwrap();
        assert!(lib.contains("#[path = \"generated/mini_add.rs\"]"));
        assert!(lib.contains("mod mini_add;"));
        // The pre-existing module is untouched.
        assert!(lib.contains("mod seed;"));
    }

    #[test]
    fn second_generation_picks_free_name_and_wires_both() {
        let fx = Fixture::new();
        let paths = fx.paths();
        let out1 = generate(&Topic::Concept("test".into()), &paths, None).unwrap();
        let out2 = generate(&Topic::ErrorCode("E0308".into()), &paths, None).unwrap();
        assert_eq!(out1.name, "mini_add");
        assert_eq!(out2.name, "mini_add_2");
        let lib = fs::read_to_string(&paths.lib_rs).unwrap();
        assert!(lib.contains("mod mini_add;"));
        assert!(lib.contains("mod mini_add_2;"));
    }

    #[test]
    fn error_code_routes_via_reverse_index() {
        let fx = Fixture::new();
        let out = generate(&Topic::ErrorCode("e0308".into()), &fx.paths(), None).unwrap();
        assert_eq!(out.template_id, "mini-add");
    }

    #[test]
    fn unknown_concept_and_code_are_clear_errors() {
        let fx = Fixture::new();
        let paths = fx.paths();
        let err = generate(&Topic::Concept("没有的东西".into()), &paths, None).unwrap_err();
        assert!(err.to_string().contains("解析为概念"), "{err}");
        let err = generate(&Topic::ErrorCode("E9999".into()), &paths, None).unwrap_err();
        assert!(err.to_string().contains("E9999"), "{err}");
    }

    #[test]
    fn free_text_matches_by_taxonomy_name_or_title() {
        let fx = Fixture::new();
        let paths = fx.paths();
        let out = generate(&Topic::FreeText("来一道 测试概念 的题".into()), &paths, None).unwrap();
        assert_eq!(out.template_id, "mini-add");
        let out = generate(&Topic::FreeText("加法".into()), &paths, None).unwrap();
        assert_eq!(out.template_id, "mini-add");
    }

    #[test]
    fn free_text_uses_llm_choice_and_slot_fill() {
        let fx = Fixture::new();
        let paths = fx.paths();

        // Mini template has no slots; extend it with one for this test.
        // TOML: table headers must come after every top-level plain key,
        // so the slot spec is appended at the end of the file.
        let mut with_slot = MINI_TEMPLATE.to_string();
        with_slot.push_str(
            "\n[[slots]]\nname = \"word\"\nkind = \"literal\"\nvalues = [\"a\", \"b\"]\ndefault = \"a\"\n",
        );
        // A slot in tests only would fail rule filter; instead replace
        // one comment line to reference the slot.
        let with_slot = with_slot.replace("// 提示 1：完成后两个测试都应通过。", "// 填槽值：{{word}}。");
        fs::write(paths.templates_dir.join("mini-add.toml"), with_slot).unwrap();

        let calls = std::cell::RefCell::new(0u32);
        let mut call = |prompt: &str| -> Result<LlmReply> {
            *calls.borrow_mut() += 1;
            if prompt.contains("choosing a Rust practice") {
                Ok(reply(r#"{"template_id": "mini-add"}"#))
            } else {
                Ok(reply(r#"{"slots": {"word": "b"}}"#))
            }
        };
        let out = generate(&Topic::FreeText("随便来一道".into()), &paths, Some(&mut call)).unwrap();
        assert_eq!(out.template_id, "mini-add");
        assert_eq!(out.slots.get("word").map(String::as_str), Some("b"));
        assert!(out.used_llm);
        assert!(*calls.borrow() >= 2);
    }

    #[test]
    fn garbage_llm_output_falls_back_to_defaults() {
        let fx = Fixture::new();
        let paths = fx.paths();
        let mut call = |_prompt: &str| -> Result<LlmReply> { Ok(reply("这不是 JSON")) };
        let out = generate(&Topic::Concept("test.concept".into()), &paths, Some(&mut call)).unwrap();
        assert!(!out.used_llm, "LLM 输出无效时不算用上 LLM");
        assert!(out.path.exists());
    }

    #[test]
    fn module_name_sanitizing() {
        assert_eq!(sanitize_module_name("mini-add"), "mini_add");
        assert_eq!(sanitize_module_name("a.b-c"), "a_b_c");
        assert_eq!(sanitize_module_name("9lives"), "ex_9lives");
        assert_eq!(sanitize_module_name("---"), "ex_");
    }

    #[test]
    fn wire_lib_rs_is_idempotent() {
        let fx = Fixture::new();
        let paths = fx.paths();
        wire_lib_rs(&paths.lib_rs, "thing", OUT_CATEGORY).unwrap();
        let once = fs::read_to_string(&paths.lib_rs).unwrap();
        wire_lib_rs(&paths.lib_rs, "thing", OUT_CATEGORY).unwrap();
        let twice = fs::read_to_string(&paths.lib_rs).unwrap();
        assert_eq!(once, twice);
        assert_eq!(once.matches("mod thing;").count(), 1);
    }

    #[test]
    fn extract_json_variants() {
        assert_eq!(extract_json("前言 {\"a\": 1} 后记"), Some("{\"a\": 1}"));
        assert_eq!(extract_json("```json\n{\"b\": 2}\n```"), Some("{\"b\": 2}"));
        assert_eq!(extract_json("没有对象"), None);
    }

    fn reply(content: &str) -> LlmReply {
        LlmReply {
            content: content.to_string(),
            usage: Default::default(),
        }
    }
}
