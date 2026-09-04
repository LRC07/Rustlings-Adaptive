//! Exercise generator (v3 design §8 M3): picks a template for the
//! user's topic (concept id / rustc error code / free text), fills its
//! slots (LLM when available, deterministic rotation otherwise), and
//! gates every candidate through the triple verification plus the
//! template's own constraints before writing it into `exercises/
//! generated/` and wiring it into the IDE-only, gitignored
//! `exercises/lib_generated.rs`.
//!
//! The LLM is optional: with `llm: None` generation is fully offline
//! (defaults + candidate rotation), which keeps tests cheap and the
//! tool usable without a key.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use serde_json::Value;

use crate::llm::LlmReply;
use crate::taxonomy::ConceptGraph;
use crate::{constraints, template, verifier};

/// How many slot-fill/verify rounds before giving up.
pub const MAX_ATTEMPTS: u32 = 3;
/// Category directory (under `exercises/`) for generated exercises.
pub const OUT_CATEGORY: &str = "generated";

/// Progress event emitted while a generation run is in flight (M4:
/// rendered by the CLI's spinner so long runs feel alive, R4).
#[derive(Debug, Clone)]
pub struct GenerateStage {
    /// 1-based attempt number currently running.
    pub attempt: u32,
    pub total_attempts: u32,
    /// Human-readable stage label ("选模板", "填槽+校验").
    pub stage: &'static str,
    /// Last gate failure summary for repair-loop rounds (tiers 2/3).
    pub note: Option<String>,
}

/// Filesystem layout used by one generation run (parameterizable for
/// tests; the CLI passes the repo root).
#[derive(Debug, Clone)]
pub struct Paths {
    pub templates_dir: PathBuf,
    pub taxonomy_file: PathBuf,
    pub exercises_dir: PathBuf,
    /// IDE-only wiring file for generated exercises
    /// (`exercises/lib_generated.rs`, gitignored, user-local).
    pub wiring_rs: PathBuf,
    /// Append-only concept-miss log (user-local data flywheel, §7.5).
    /// None in tests.
    pub miss_log: Option<PathBuf>,
}

impl Paths {
    pub fn from_root(root: &Path) -> Self {
        let miss_log = std::env::var("HOME")
            .ok()
            .map(|h| PathBuf::from(h).join(".rustlings_adaptive").join("miss_log.json"));
        Self {
            templates_dir: root.join("templates"),
            taxonomy_file: root.join("taxonomy").join("concepts.toml"),
            exercises_dir: root.join("exercises"),
            wiring_rs: root.join("exercises").join("lib_generated.rs"),
            miss_log,
        }
    }
}

/// What the user asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
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

    /// Heuristic topic parsing shared by the CLI and the agent tool:
    /// `E0382`-style inputs become an error-code request, dotted ids
    /// like `traits.associated-types` become a concept request,
    /// everything else is free text.
    pub fn from_input(input: &str) -> Topic {
        let t = input.trim();
        let is_code = t.len() == 5
            && (t.starts_with('E') || t.starts_with('e'))
            && t[1..].chars().all(|c| c.is_ascii_digit());
        let is_concept_id = t.contains('.')
            && t.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'));
        if is_code {
            Topic::ErrorCode(t.to_uppercase())
        } else if is_concept_id {
            Topic::Concept(t.to_string())
        } else {
            Topic::FreeText(t.to_string())
        }
    }
}

/// Which tier produced a successful exercise (design §7.5). Also the
/// mapping into the exercise index's `Source` (M4.5a).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tier {
    /// Level 1: a hand-written template instantiated with slots.
    Matched { template_id: String },
    /// Level 2: LLM adaptation of a nearby template's skeleton.
    Adapted { base: String },
    /// Level 3: free-form LLM generation.
    Free,
}

impl Tier {
    pub fn to_source(&self) -> crate::exercise::index::Source {
        match self {
            Tier::Matched { template_id } => {
                crate::exercise::index::Source::TemplateFill { template_id: template_id.clone() }
            }
            Tier::Adapted { base } => crate::exercise::index::Source::Adapted { base: base.clone() },
            Tier::Free => crate::exercise::index::Source::Free,
        }
    }

    pub fn label_cn(&self) -> String {
        match self {
            Tier::Matched { template_id } => format!("模板 {template_id}"),
            Tier::Adapted { base } => format!("改编自 {base}"),
            Tier::Free => "自由生成".to_string(),
        }
    }
}

/// Which tiers the caller allows (design §7.5). Crate-internal test
/// seam: the public entry always runs `Auto` (matched → adapted → free
/// fall-through); neither the user nor the coach model picks a tier
/// (M4.7 decision).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[allow(dead_code)] // variants exercised via tests
pub enum GenerateMode {
    #[default]
    Auto,
    Matched,
    Adapted,
    Free,
}

/// How a template was used before (M4.10), distilled from the exercise
/// index. Drives two behaviors: unused candidates rank first, and an
/// unavoidable repeat becomes a slot-rotated *variant* instead of a
/// silent re-serve of the same question.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TemplateUse {
    /// Exercises already generated from this template.
    pub times: u32,
    /// True when every one of them passed (nothing left to learn here
    /// without a variant).
    pub all_passed: bool,
    /// Slot values used by past generations (index order), so an LLM
    /// fill can be told which values to avoid.
    pub prev_slot_values: Vec<std::collections::BTreeMap<String, String>>,
}

/// Generation history view (M4.10): template_id → how it was used.
#[derive(Debug, Clone, Default)]
pub struct GenHistory {
    used: std::collections::BTreeMap<String, TemplateUse>,
}

impl GenHistory {
    /// Build from the exercise index (tier-1 fills only; tiers 2/3
    /// produce fresh scenarios and need no dedup).
    pub fn from_index(index: &crate::exercise::index::ExerciseIndex) -> Self {
        let mut used: std::collections::BTreeMap<String, TemplateUse> = Default::default();
        for meta in index.iter() {
            let crate::exercise::index::Source::TemplateFill { template_id } = &meta.source
            else {
                continue;
            };
            let entry = used.entry(template_id.clone()).or_default();
            entry.times += 1;
            entry.all_passed &= meta.status == crate::exercise::index::Status::Passed;
            if !meta.slots.is_empty() {
                entry.prev_slot_values.push(meta.slots.clone());
            }
        }
        Self { used }
    }

    pub fn times(&self, template_id: &str) -> u32 {
        self.used.get(template_id).map(|u| u.times).unwrap_or(0)
    }

    pub fn get(&self, template_id: &str) -> Option<&TemplateUse> {
        self.used.get(template_id)
    }
}

/// Result of a successful generation.
#[derive(Debug, Clone)]
pub struct Outcome {
    pub path: PathBuf,
    /// Exercise name == file stem == rust module name.
    pub name: String,
    pub tier: Tier,
    pub title: String,
    pub concepts: Vec<String>,
    pub error_codes: Vec<String>,
    pub difficulty: template::Difficulty,
    /// Slot values (tier 1 only; empty for tiers 2/3).
    pub slots: std::collections::BTreeMap<String, String>,
    /// Tiered hints from the draft/template (M4.8; tiers 2/3 only).
    pub hints: Vec<String>,
    pub attempts: u32,
    /// Whether any LLM call actually succeeded (selection/fill/draft).
    pub used_llm: bool,
    /// True when a previously-used template was intentionally reused
    /// and the question was freshened via slot rotation / a different
    /// LLM fill (M4.10). The caller should surface this ("变式").
    pub variant: bool,
}

/// Blocking LLM caller provided by the CLI (handles config/budget/usage).
/// Implemented for any `FnMut` closure.
pub trait LlmCaller {
    fn call(&mut self, prompt: &str) -> Result<LlmReply>;

    /// Bounded variant (M4.7): ask the endpoint to cap the completion at
    /// `max_tokens` so a rambling draft cannot burn minutes per round.
    /// Default ignores the cap so closure/mock implementations stay
    /// trivial; the real bridge forwards it onto the wire.
    fn call_bounded(&mut self, prompt: &str, max_tokens: u32) -> Result<LlmReply> {
        let _ = max_tokens;
        self.call(prompt)
    }
}

impl<F: FnMut(&str) -> Result<LlmReply>> LlmCaller for F {
    fn call(&mut self, prompt: &str) -> Result<LlmReply> {
        self(prompt)
    }
}

// ---------------------------------------------------------------------------
// The single quality gate (all three tiers converge here)
// ---------------------------------------------------------------------------

/// The one quality gate (design §7.4 + spec C1): rule filter →
/// constraint self-consistency → triple verification → first-error
/// match. Hand-filled templates (tier 1), LLM adaptations (tier 2) and
/// free-form generations (tier 3) all present as an `ExerciseDraft`
/// and leave through this function; on failure the error text is what
/// the repair loop feeds back to the model.
pub fn gate_draft(d: &template::ExerciseDraft, workdir: &Path) -> Result<verifier::VerifyReport> {
    // Gate 2: static rule filter.
    let violations = template::rule_filter_draft(d);
    if !violations.is_empty() {
        bail!("规则过滤未通过：{}", violations.join("；"));
    }

    // Gate 3: the reference solution must satisfy its own constraints.
    let mut cs = Vec::new();
    for spec in &d.constraints {
        let c = constraints::Constraint::from_spec(spec)
            .with_context(|| format!("约束 '{spec}' 无法解析"))?;
        cs.push(c);
    }
    let rendered = d.render();
    let violations = constraints::check(&rendered.reference_file(), &cs);
    if !violations.is_empty() {
        let msgs: Vec<String> = violations.iter().map(|v| v.message.clone()).collect();
        bail!("参考解违反约束：{}", msgs.join("；"));
    }

    // Gate 1: triple verification (+ C1 first-error-code match).
    let report = verifier::verify_exercise(
        &rendered.user_file(),
        &rendered.reference_file(),
        workdir,
    )
    .map_err(|e| anyhow!("校验执行失败：{e:#}"))?;
    template::first_error_matches(&d.error_codes, &report)?;
    if !report.all_pass() {
        // Attach the unfinished template's real rustc diagnostics so
        // the repair loop (§7.5) can feed them back to the model.
        let diags = report
            .template
            .as_ref()
            .map(|t| {
                t.diagnostics
                    .iter()
                    .filter_map(|d| d.rendered.as_deref())
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        let diags: String = diags.chars().take(1200).collect();
        bail!("{}\n[未完成模板的 rustc 诊断]\n{diags}", failure_reason(&report));
    }
    Ok(report)
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Generate an exercise for `topic`, honoring `mode` (design §7.5):
///
/// - tier 1 `Matched`: template pick + slot fill + gate (offline-capable)
/// - tier 2 `Adapted`: LLM rewrites a nearby template's skeleton to the
///   user's topic
/// - tier 3 `Free`: LLM produces an `ExerciseDraft` from scratch
///
/// Every tier leaves through `gate_draft`; on failure the rustc
/// diagnostics are fed back to the model (repair loop, ≤4 rounds).
/// Tiers 2/3 require an LLM caller; `Auto` falls through 1 → 2 → 3.
/// Generate an exercise for `topic` (no history awareness — the
/// convenience/test entry; production callers use
/// `generate_with_history`).
#[allow(dead_code)]
pub fn generate(
    topic: &Topic,
    paths: &Paths,
    llm: Option<&mut dyn LlmCaller>,
    progress: Option<&mut dyn FnMut(GenerateStage)>,
) -> Result<Outcome> {
    generate_with_history(topic, paths, &GenHistory::default(), llm, progress)
}

/// History-aware entry (M4.10): `history` carries which templates the
/// learner already received (from the exercise index). Unused
/// candidates rank first; a repeated template becomes a slot-rotated
/// variant instead of the same question again.
pub fn generate_with_history(
    topic: &Topic,
    paths: &Paths,
    history: &GenHistory,
    llm: Option<&mut dyn LlmCaller>,
    progress: Option<&mut dyn FnMut(GenerateStage)>,
) -> Result<Outcome> {
    generate_with_mode(topic, GenerateMode::Auto, paths, history, llm, progress)
}

/// Mode-parameterized entry — crate-internal (tests). The public
/// surface always runs the automatic fall-through: the user and the
/// coach model never pick the tier (M4.7 decision).
pub(crate) fn generate_with_mode(
    topic: &Topic,
    mode: GenerateMode,
    paths: &Paths,
    history: &GenHistory,
    mut llm: Option<&mut dyn LlmCaller>,
    mut progress: Option<&mut dyn FnMut(GenerateStage)>,
) -> Result<Outcome> {
    macro_rules! stage {
        ($stage:expr, $attempt:expr, $total:expr) => {
            if let Some(cb) = progress.as_mut() {
                cb(GenerateStage {
                    attempt: $attempt,
                    total_attempts: $total,
                    stage: $stage,
                    note: None,
                });
            }
        };
    }

    let graph = ConceptGraph::load(&paths.taxonomy_file).context("概念图谱加载失败")?;
    let mut graph = graph;
    let templates = template::load_dir(&paths.templates_dir).context("模板库加载失败")?;
    if templates.is_empty() {
        bail!("模板库为空（{}）", paths.templates_dir.display());
    }
    let items: Vec<(&str, &[String])> =
        templates.iter().map(|t| (t.id.as_str(), t.concepts.as_slice())).collect();
    graph.link_templates(items).context("模板与概念图谱对不上")?;

    // Tier 1 always runs first (cheap; the other tiers need an LLM).
    if matches!(mode, GenerateMode::Auto | GenerateMode::Matched) {
        let sel_call: Option<&mut dyn LlmCaller> = match llm.as_mut() {
            Some(c) => Some(&mut **c),
            None => None,
        };
        stage!("选模板", 1, MAX_ATTEMPTS);
        match generate_matched(topic, &templates, &graph, paths, history, sel_call, &mut progress) {
            Ok(o) => Ok(o),
            Err(tier1_err) => {
                if matches!(mode, GenerateMode::Matched) {
                    Err(tier1_err)
                } else {
                let Some(call) = llm.as_deref_mut() else {
                    bail!(
                        "模板直配失败：{tier1_err:#}\n（未配置 LLM 时只能用模板直配；配置 Key 后可用改编/自由生成）"
                    );
                };
                stage!("模板改编", 1, LLM_ATTEMPTS);
                match generate_adapted(topic, &templates, &graph, paths, call, &mut progress) {
                    Ok(o) => Ok(o),
                    Err(tier2_err) => {
                        stage!("自由生成", 1, LLM_ATTEMPTS);
                        generate_free(topic, &graph, paths, call, &mut progress)
                            .map_err(|tier3_err| {
                                anyhow!(
                                    "三层出题均失败。\n· 模板直配：{tier1_err:#}\n· 模板改编：{tier2_err:#}\n· 自由生成：{tier3_err:#}"
                                )
                            })
                    }
                }
                }
            }
        }
    } else {
        // Explicit adapted/free mode: straight to the LLM tiers.
        let Some(call) = llm else {
            bail!("该模式需要 LLM；未配置 API Key");
        };
        if matches!(mode, GenerateMode::Adapted) {
            stage!("模板改编", 1, LLM_ATTEMPTS);
            generate_adapted(topic, &templates, &graph, paths, call, &mut progress)
        } else {
            stage!("自由生成", 1, LLM_ATTEMPTS);
            generate_free(topic, &graph, paths, call, &mut progress)
        }
    }
}

/// Tier 1: pick a template and fill its slots (the M3 pipeline).
fn generate_matched(
    topic: &Topic,
    templates: &[template::Template],
    graph: &ConceptGraph,
    paths: &Paths,
    history: &GenHistory,
    mut sel_llm: Option<&mut dyn LlmCaller>,
    progress: &mut Option<&mut dyn FnMut(GenerateStage)>,
) -> Result<Outcome> {
    macro_rules! stage {
        ($stage:expr, $attempt:expr) => {
            if let Some(cb) = progress.as_mut() {
                cb(GenerateStage {
                    attempt: $attempt,
                    total_attempts: MAX_ATTEMPTS,
                    stage: $stage,
                    note: None,
                });
            }
        };
    }

    let pick = choose_template(templates, graph, topic, history, sel_llm.as_deref_mut())?;
    let t = pick.t;
    // Variant of a previously-served template (M4.10): start the slot
    // rotation at the number of past generations so the fill — and
    // with it the scenario — genuinely differs from what the learner
    // already saw (attempt 0 = defaults would re-serve the same one).
    let variant = pick.variant;
    let rotation_base = if variant { history.times(&t.id) as usize } else { 0 };

    let mut last_fail = String::from("尚未尝试");
    let mut used_llm = pick.used_llm;
    for attempt in 0..MAX_ATTEMPTS {
        if crate::agent::is_interrupted() {
            crate::agent::reset_interrupt();
            bail!("已打断");
        }
        stage!("填槽+校验", attempt + 1);
        // Base values: defaults (attempt 0) then deterministic rotation.
        let mut values = template::fill_for_attempt(t, rotation_base + attempt as usize);

        // LLM fills slots on the first two attempts; later attempts rely
        // on rotation so a stuck LLM cannot loop forever.
        if attempt <= 1
            && let Some(call) = sel_llm.as_deref_mut()
            && let Ok(filled) = llm_fill_slots(
                t,
                &topic.prompt_text(),
                Some(&last_fail),
                variant.then(|| history.get(&t.id)).flatten(),
                call,
            )
        {
            for (k, v) in filled {
                values.insert(k, v);
            }
            used_llm = true;
            // A failed fill falls back silently; verify is the gate.
        }

        let rendered = match template::render(t, &values) {
            Ok(r) => r,
            Err(e) => {
                last_fail = format!("填槽渲染失败：{e:#}");
                continue;
            }
        };

        // Assemble the draft view with the rendered parts (the same
        // shape tiers 2/3 produce) and run the single quality gate.
        let mut draft = t.draft();
        draft.body = rendered.body.clone();
        draft.tests = rendered.tests.clone();
        draft.reference = rendered.reference.clone();
        let workdir = fresh_workdir("rustlings_generate")?;
        let gated = gate_draft(&draft, &workdir);
        let _ = fs::remove_dir_all(&workdir);
        let report = match gated {
            Ok(r) => r,
            Err(e) => {
                last_fail = format!("{e:#}");
                continue;
            }
        };

        if report.all_pass() {
            let name = write_exercise(paths, &draft, &sanitize_module_name(&t.id))?;
            return Ok(Outcome {
                path: paths.exercises_dir.join(OUT_CATEGORY).join(format!("{name}.rs")),
                name,
                tier: Tier::Matched { template_id: t.id.clone() },
                title: t.title.clone(),
                concepts: t.concepts.clone(),
                error_codes: t.error_codes.clone(),
                difficulty: t.difficulty,
                slots: values,
                hints: t.hints.clone(),
                attempts: attempt + 1,
                used_llm,
                variant,
            });
        }
        last_fail = failure_reason(&report);
    }

    bail!("连续 {MAX_ATTEMPTS} 次生成均未通过校验（模板 {}）。最后失败原因：{last_fail}", t.id)
}

// ---------------------------------------------------------------------------
// Tiers 2/3: LLM drafts (adapted / free) with a rustc repair loop
// ---------------------------------------------------------------------------

/// LLM draft rounds (design §7.5 修复环).
pub const LLM_ATTEMPTS: u32 = 4;

/// Wire shape of the LLM's draft JSON.
#[derive(Debug, Deserialize)]
struct DraftWire {
    title: String,
    #[serde(default)]
    file_hint: String,
    #[serde(default)]
    concepts: Vec<String>,
    #[serde(default)]
    error_codes: Vec<String>,
    #[serde(default)]
    difficulty: template::Difficulty,
    #[serde(default)]
    constraints: Vec<String>,
    #[serde(default)]
    hints: Vec<String>,
    body: String,
    tests: String,
    reference: String,
}

struct DraftResult {
    draft: template::ExerciseDraft,
    module_name: String,
    hints: Vec<String>,
    attempts: u32,
}

/// Tier 2: LLM rewrites a nearby template's skeleton to the user's topic.
fn generate_adapted(
    topic: &Topic,
    templates: &[template::Template],
    graph: &ConceptGraph,
    paths: &Paths,
    call: &mut dyn LlmCaller,
    progress: &mut Option<&mut dyn FnMut(GenerateStage)>,
) -> Result<Outcome> {
    let base = nearest_template(templates, graph, topic)?;
    let DraftResult { draft, module_name, hints, attempts } = llm_draft_loop(
        &topic.prompt_text(),
        Some(base),
        graph,
        paths,
        call,
        progress,
        "模板改编",
    )?;
    finish_draft(paths, draft, Tier::Adapted { base: base.id.clone() }, module_name, hints, attempts)
}

/// Tier 3: LLM produces an exercise from scratch.
fn generate_free(
    topic: &Topic,
    graph: &ConceptGraph,
    paths: &Paths,
    call: &mut dyn LlmCaller,
    progress: &mut Option<&mut dyn FnMut(GenerateStage)>,
) -> Result<Outcome> {
    let DraftResult { draft, module_name, hints, attempts } =
        llm_draft_loop(&topic.prompt_text(), None, graph, paths, call, progress, "自由生成")?;
    finish_draft(paths, draft, Tier::Free, module_name, hints, attempts)
}

/// Persist a gated draft: write + wire + outcome.
fn finish_draft(
    paths: &Paths,
    draft: template::ExerciseDraft,
    tier: Tier,
    module_name: String,
    hints: Vec<String>,
    attempts: u32,
) -> Result<Outcome> {
    let name = write_exercise(paths, &draft, &module_name)?;
    Ok(Outcome {
        path: paths.exercises_dir.join(OUT_CATEGORY).join(format!("{name}.rs")),
        name,
        tier,
        title: draft.title.clone(),
        concepts: draft.concepts.clone(),
        error_codes: draft.error_codes.clone(),
        difficulty: draft.difficulty,
        slots: Default::default(),
        hints,
        attempts,
        used_llm: true,
        // Tiers 2/3 write a fresh scenario by construction — no
        // repeat-detection needed (M4.10).
        variant: false,
    })
}

/// Pick the skeleton for adaptation: keyword match on the request only.
/// (The LLM's template picker is deliberately NOT used here — it is
/// already strict at tier 1; a loosely-picked skeleton risks dragging
/// the draft off-topic. No keyword hit → fall through to tier 3.)
fn nearest_template<'a>(
    templates: &'a [template::Template],
    graph: &ConceptGraph,
    topic: &Topic,
) -> Result<&'a template::Template> {
    let ids = keyword_candidates(templates, graph, &topic.prompt_text());
    templates
        .iter()
        .find(|t| ids.contains(&t.id))
        .ok_or_else(|| anyhow!("没有关键词命中的模板可作改编骨架"))
}

/// Hard cap on one draft round's completion tokens (M4.7): the whole
/// exercise JSON must fit; "length" truncations are fed back to the
/// model with a demand to compress.
pub const DRAFT_MAX_TOKENS: u32 = 3000;
/// Wall-clock budget for the whole draft repair loop (M4.7): on slow
/// endpoints, stop with a clear report instead of burning rounds.
pub const DRAFT_TIME_BUDGET: Duration = Duration::from_secs(420);

/// The repair loop: prompt → parse → normalize → gate; failures (with
/// the real rustc diagnostics) are fed back for the next round.
fn llm_draft_loop(
    request: &str,
    base: Option<&template::Template>,
    graph: &ConceptGraph,
    paths: &Paths,
    call: &mut dyn LlmCaller,
    progress: &mut Option<&mut dyn FnMut(GenerateStage)>,
    stage_label: &'static str,
) -> Result<DraftResult> {
    let concept_ids: Vec<String> = graph.ids().cloned().collect();
    let started = Instant::now();
    let mut last_fail = String::new();
    for attempt in 1..=LLM_ATTEMPTS {
        if crate::agent::is_interrupted() {
            crate::agent::reset_interrupt();
            bail!("已打断");
        }
        if started.elapsed() > DRAFT_TIME_BUDGET {
            bail!(
                "出题耗时超过预算（{}s，已尝试 {} 轮）。端点较慢时每轮长输出可能需要数分钟；\
                 建议 /model 切换更快的模型，或稍后重试。",
                DRAFT_TIME_BUDGET.as_secs(),
                attempt - 1
            );
        }
        if let Some(cb) = progress.as_mut() {
            let note = (attempt > 1 && !last_fail.is_empty())
                .then(|| last_fail.chars().take(110).collect::<String>());
            cb(GenerateStage { attempt, total_attempts: LLM_ATTEMPTS, stage: stage_label, note });
        }

        let prompt = draft_prompt(request, base, &concept_ids, attempt, &last_fail);
        let reply = call.call_bounded(&prompt, DRAFT_MAX_TOKENS).map_err(|e| anyhow!("LLM 调用失败：{e:#}"))?;
        if reply.finish_reason.as_deref() == Some("length") {
            last_fail = format!(
                "上一轮输出在 {DRAFT_MAX_TOKENS} tokens 处被截断：整题 JSON 必须更精简\
                 （测试只留 2 个、注释删减、字段紧凑），重新输出完整 JSON"
            );
            continue;
        }
        let Some(json) = extract_json(&reply.content) else {
            last_fail = "上一轮回复中没有 JSON 对象；请只输出一个 JSON 对象".into();
            continue;
        };
        let wire: DraftWire = match serde_json::from_str(json) {
            Ok(w) => w,
            Err(e) => {
                last_fail = format!(
                    "上一轮 JSON 不符合 schema（{e}）；字段需要 title/file_hint/concepts/\
                     error_codes/difficulty/constraints/body/tests/reference"
                );
                continue;
            }
        };

        // Concepts must land on the taxonomy (misses fall back to the
        // top-level domain and are logged for the data flywheel).
        let concepts = normalize_concepts(&wire.concepts, graph, paths);
        if concepts.is_empty() {
            last_fail = "concepts 没有落在概念图谱上；请从提供的 id 列表中选择 1–2 个".into();
            continue;
        }
        let module_name = module_name_for(&wire, &concepts);
        let draft_hints = wire.hints.clone();

        let draft = template::ExerciseDraft {
            title: wire.title,
            concepts,
            error_codes: wire.error_codes,
            difficulty: wire.difficulty,
            constraints: wire.constraints,
            body: wire.body,
            tests: wire.tests,
            reference: wire.reference,
        };
        let mut draft = draft;
        ensure_ban_constraints(&mut draft);

        let workdir = fresh_workdir("rustlings_llm")?;
        let gated = gate_draft(&draft, &workdir);
        let _ = fs::remove_dir_all(&workdir);
        match gated {
            Ok(_report) => {
                return Ok(DraftResult { draft, module_name, hints: draft_hints, attempts: attempt });
            }
            Err(e) => {
                last_fail = format!("{e:#}");
            }
        }
    }
    bail!("连续 {LLM_ATTEMPTS} 轮未产出合格题目。最后失败原因：{last_fail}")
}

/// Concept normalization (§7.5): resolve on the graph; unknown ids fall
/// back to the longest existing prefix (`test.concept.sub` →
/// `test.concept`), and every miss is logged for the flywheel.
fn normalize_concepts(raw: &[String], graph: &ConceptGraph, paths: &Paths) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for c in raw {
        let c = c.trim();
        if c.is_empty() {
            continue;
        }
        if let Some(id) = graph.resolve(c) {
            let id = id.to_string();
            if !out.contains(&id) {
                out.push(id);
            }
            continue;
        }
        // Longest existing dotted prefix.
        let mut prefix = c.to_string();
        let mut fallback = None;
        while let Some((p, _)) = prefix.rsplit_once('.') {
            prefix = p.to_string();
            if graph.get(&prefix).is_some() {
                fallback = Some(prefix);
                break;
            }
        }
        match fallback {
            Some(id) => {
                log_concept_miss(paths, c);
                if !out.contains(&id) {
                    out.push(id);
                }
            }
            None => log_concept_miss(paths, c),
        }
    }
    out
}

/// English kebab module name: the LLM's file_hint if sane, else the
/// first concept id sanitized (`ownership.move` → `ownership_move`).
fn module_name_for(wire: &DraftWire, concepts: &[String]) -> String {
    let hint = sanitize_module_name(&wire.file_hint);
    if !hint.is_empty() && hint != "ex_" {
        return hint;
    }
    sanitize_module_name(concepts.first().map(String::as_str).unwrap_or("exercise"))
}

/// Append-only miss log (user-local data flywheel, §7.5). Best-effort.
fn log_concept_miss(paths: &Paths, concept: &str) {
    let Some(path) = paths.miss_log.as_ref() else { return };
    let ts = chrono::Utc::now().to_rfc3339();
    let line = format!("{{\"ts\":\"{ts}\",\"concept\":\"{}\"}}\n", concept.replace('"', "'"));
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        use std::io::Write;
        let _ = f.write_all(line.as_bytes());
    }
}

/// Hardened defaults for LLM drafts (§7.5 前置硬化): ban unsafe /
/// std::process / std::fs and cap the size, on top of whatever the
/// model declared.
fn ensure_ban_constraints(d: &mut template::ExerciseDraft) {
    for required in ["ban=unsafe", "ban=std::process", "ban=std::fs"] {
        if !d.constraints.iter().any(|c| c == required) {
            d.constraints.push(required.to_string());
        }
    }
}

/// The draft prompt: request + rubric (spec C1–C7, compressed) + an
/// optional reference skeleton (tier 2) + the retry feedback.
fn draft_prompt(
    request: &str,
    base: Option<&template::Template>,
    concept_ids: &[String],
    attempt: u32,
    last_fail: &str,
) -> String {
    let skeleton = match base {
        Some(b) => format!(
            "## Reference exercise (adapt its STRUCTURE to the topic below; do NOT copy it verbatim)\n\
             title: {}\n--- body ---\n{}\n--- tests ---\n{}\n--- reference ---\n{}\n",
            b.title, b.body, b.tests, b.reference
        ),
        None => "## Reference exercise: none — free-form. Write the smallest exercise that \
                 teaches the topic.\n"
            .into(),
    };
    let retry = if attempt > 1 {
        format!(
            "\n## Your previous attempt FAILED the quality gate. Fix ALL of it:\n{last_fail}\n\
             Change your approach where needed; do not repeat it.\n"
        )
    } else {
        String::new()
    };
    let ids = concept_ids.join(", ");
    format!(
        "You are writing ONE small Rust practice exercise for a learner who already codes \
         (Python/Java/Go/C++) but is confused by Rust's ownership/borrowing/lifetimes/traits.\n\n\
         Topic / user request: {request}\n\n\
         {skeleton}{retry}\n\
         ## Hard requirements (a local gate will REJECT the draft otherwise)\n\
         1. Single root cause: the exercise's UNFINISHED body must fail to COMPILE, and the \
         first rustc error must be one of the codes you declare in error_codes. Do not use \
         todo!() for the hole — leave real, plausible learner code that triggers the error.\n\
         2. Real scenario: the body does one small, humanly describable task; the first comment \
         lines say what the code is trying to do and what is wrong (in 简体中文, like the \
         reference exercise).\n\
         3. Idiomatic fix: there is one clean idiomatic fix; the reference solution implements \
         it, passes ALL tests, and satisfies every constraint you declare.\n\
         4. Tests: a #[cfg(test)] mod tests with 2+ tests asserting observable behavior; \
         they must FAIL on the unfinished body and PASS on the reference.\n\
         5. std-only, single file: no external crates, no unsafe, no std::process, no std::fs, \
         no file/network IO. body: 10–40 non-empty lines; include the literal line \
         `// I AM NOT DONE` at the end of the body.\n\
         6. constraints: choose from [\"no-clone\", \"no-unwrap\", \"iterator-only\"]; \
         do NOT declare \"max-lines\" (a 40-line cap applies automatically).\n\
         7. concepts: 1–2 ids from EXACTLY this list: [{ids}]\n\
         8. difficulty: \"easy\" (apply one known fix) | \"medium\" (choose between 2–3 \
         plausible fixes, or non-obvious error) | \"hard\" (restructure).\n\
         9. Keep it COMPACT: body 15–35 lines (hard cap 40), tests ≤20 lines, \
         reference ≤20 lines. Trim blank lines and repetitive tests.\n\
         10. hints: 1–3 SHORT graded Chinese hints (方向 → 具体 → 接近正确写法), \
         each one sentence, never giving away the answer or the exact line to write.\n\n\
         ## Output format\n\
         Answer with ONLY one JSON object (no markdown fence needed):\n\
         {{\"title\": \"中文标题\", \"file_hint\": \"english-kebab-name\", \
         \"concepts\": [\"...\"], \"error_codes\": [\"E0xxx\"], \"difficulty\": \"easy\", \
         \"constraints\": [\"no-clone\"], \"hints\": [\"...\"], \
         \"body\": \"...\", \"tests\": \"...\", \"reference\": \"...\"}}\n\
         body/tests/reference are plain Rust source strings (escape newlines as \\n in JSON)."
    )
}

// ---------------------------------------------------------------------------
// Template selection
// ---------------------------------------------------------------------------

/// A tier-1 template selection (M4.10 shape).
struct Pick<'a> {
    t: &'a template::Template,
    /// The LLM made the selection (vs deterministic fallback).
    used_llm: bool,
    /// The template was served before — the fill must be freshened.
    variant: bool,
}

fn choose_template<'a>(
    templates: &'a [template::Template],
    graph: &ConceptGraph,
    topic: &Topic,
    history: &GenHistory,
    llm: Option<&mut (dyn LlmCaller + '_)>,
) -> Result<Pick<'a>> {
    match topic {
        Topic::Concept(q) => {
            let id = graph
                .resolve(q)
                .ok_or_else(|| anyhow!("无法把「{q}」解析为概念；可用概念如：{}",
                    graph.ids().take(5).cloned().collect::<Vec<_>>().join("、")))?;
            let mut ids = BTreeSet::new();
            collect_concept_templates(graph, id, &mut ids);
            match pick_candidate(templates, &ids, history) {
                Some(p) => Ok(p),
                None if ids.is_empty() => {
                    bail!("概念「{id}」还没有可用模板")
                }
                // All candidates served and slot-less: a repeat would
                // be the identical question — hand off to L2/L3.
                None => bail!(
                    "概念「{id}」的模板题都已出过，且没有槽位可生成变式；\
                     配置 LLM 后会自动转为改编/自由生成新场景"
                ),
            }
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
            match pick_candidate(templates, &ids, history) {
                Some(p) => Ok(p),
                None if ids.is_empty() => {
                    bail!("错误码 {code} 相关概念还没有可用模板")
                }
                None => bail!(
                    "错误码 {code} 相关的模板题都已出过，且没有槽位可生成变式；\
                     配置 LLM 后会自动转为改编/自由生成新场景"
                ),
            }
        }
        Topic::FreeText(text) => {
            // LLM first (understands loose Chinese requests), then a
            // deterministic keyword score over taxonomy names + titles.
            if let Some(call) = llm
                && let Some(p) = llm_pick_template(templates, text, history, call)?
            {
                return Ok(p);
            }
            let ids = keyword_candidates(templates, graph, text);
            pick_candidate(templates, &ids, history).ok_or_else(|| {
                if ids.is_empty() {
                    anyhow!(
                        "没能根据「{text}」挑出模板；试试概念（如 trait.associated-types）、\
                         错误码（如 E0382）或更具体的关键词"
                    )
                } else {
                    anyhow!(
                        "「{text}」命中的模板题都已出过，且没有槽位可生成变式；\
                         配置 LLM 后会自动转为改编/自由生成新场景"
                    )
                }
            })
        }
    }
}

/// Choose among candidate template ids (M4.10): unused templates rank
/// first (stable order otherwise); a used template is only eligible as
/// an explicit variant — and only when its slots can produce a
/// different fill (a slot-less repeat would be the identical question).
fn pick_candidate<'a>(
    templates: &'a [template::Template],
    ids: &BTreeSet<String>,
    history: &GenHistory,
) -> Option<Pick<'a>> {
    let mut cands: Vec<&template::Template> =
        templates.iter().filter(|t| ids.contains(&t.id)).collect();
    cands.sort_by_key(|t| history.times(&t.id) != 0); // stable: unused first
    cands.into_iter().find_map(|t| {
        let times = history.times(&t.id);
        if times == 0 {
            Some(Pick { t, used_llm: false, variant: false })
        } else if !t.slots.is_empty() {
            Some(Pick { t, used_llm: false, variant: true })
        } else {
            None // already served, cannot vary: skip (falls to L2/L3)
        }
    })
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
    history: &GenHistory,
    call: &mut (dyn LlmCaller + '_),
) -> Result<Option<Pick<'a>>> {
    let catalog: String = templates
        .iter()
        .map(|t| {
            // M4.10: mark templates the learner already received so the
            // pick can prefer fresh ones.
            let used_note = match history.get(&t.id) {
                Some(u) => format!(" | ALREADY USED x{} ({})", u.times,
                    if u.all_passed { "all passed" } else { "not all passed" }),
                None => String::new(),
            };
            format!(
                "- {} | {} | {} | {}{}",
                t.id,
                t.title,
                t.concepts.join(","),
                t.error_codes.join(","),
                used_note
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let prompt = format!(
        "You are choosing a Rust practice exercise template for a learner.\n\n\
         User request: {request}\n\n\
         Available templates (id | title | concepts | error-codes | usage):\n{catalog}\n\n\
         Pick a template ONLY if it trains the SPECIFIC technique or behavior the request \
         asks for. A template that merely lives in the same concept domain while training \
         a different technique is a WRONG answer (e.g. an and_then+map chain exercise for \
         an unwrap_or-laziness request) — say no_match instead. Templates merely touching \
         adjacent keywords are also WRONG answers.\n\n\
         Prefer templates NOT marked ALREADY USED; the learner should not re-serve the \
         same question. Pick a used one only when it is clearly the best precision match \
         — the pipeline will then produce a fresh slot-rotated variant.\n\n\
         Answer with ONLY a JSON object:\n\
         {{\"template_id\": \"<id>\"}}  or  {{\"no_match\": true}}"
    );
    let reply = call.call(&prompt)?;
    let Some(json) = extract_json(&reply.content) else {
        return Ok(None);
    };
    let v: Value = match serde_json::from_str(json) {
        Ok(v) => v,
        Err(_) => return Ok(None),
    };
    if v.get("no_match").and_then(Value::as_bool) == Some(true) {
        return Ok(None);
    }
    let id = v.get("template_id").and_then(Value::as_str).map(str::to_string);
    Ok(id.and_then(|id| templates.iter().find(|t| t.id == id)).map(|t| Pick {
        t,
        used_llm: true,
        variant: history.times(&t.id) > 0,
    }))
}

fn llm_fill_slots(
    t: &template::Template,
    request: &str,
    last_fail: Option<&str>,
    prev_use: Option<&TemplateUse>,
    call: &mut (dyn LlmCaller + '_),
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
    // M4.10 variant: the learner already saw fills like these — pick
    // fresh values so the scenario differs, not the same question.
    let variant_note = prev_use
        .filter(|u| !u.prev_slot_values.is_empty())
        .map(|u| {
            let past: Vec<String> = u
                .prev_slot_values
                .iter()
                .map(|vals| {
                    vals.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(", ")
                })
                .collect();
            format!(
                "\nThis template was already used for {} earlier exercise(s). \
                 Past slot fills: [{}]. Choose DIFFERENT values (within the allowed \
                 lists) so this becomes a new scenario, not a repeat.\n",
                u.times,
                past.join(" ; ")
            )
        })
        .unwrap_or_default();
    let prompt = format!(
        "You are generating a Rust practice exercise by filling named slots.\n\n\
         Template: {} ({})\nUser request: {request}\n{retry_note}{variant_note}\n\
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
// Output: exercise file + generated-exercises wiring
// ---------------------------------------------------------------------------

/// Rust module/file-stem name for a template id (`own-closure-capture`
/// → `own_closure_capture`). Public so the exercise index can map a
/// generated file name back to its template during reconciliation.
pub fn sanitize_module_name(id: &str) -> String {
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

/// Write the exercise file (concrete draft, no slots) and wire it into
/// the IDE-only lib. `module_name` must already be sanitized.
fn write_exercise(
    paths: &Paths,
    draft: &template::ExerciseDraft,
    module_name: &str,
) -> Result<String> {
    let dir = paths.exercises_dir.join(OUT_CATEGORY);
    fs::create_dir_all(&dir).with_context(|| format!("创建 {} 失败", dir.display()))?;
    let name = pick_name(&dir, module_name);
    let path = dir.join(format!("{name}.rs"));
    let content = format!(
        "// {}\n//\n{}\n\n{}\n",
        draft.title,
        draft.body.trim_end(),
        draft.tests.trim()
    );
    fs::write(&path, content).with_context(|| format!("写入 {} 失败", path.display()))?;
    wire_lib_rs(&paths.wiring_rs, &name, OUT_CATEGORY)
        .with_context(|| format!("接线 {} 失败", paths.wiring_rs.display()))?;
    Ok(name)
}

/// Header written when the wiring file does not exist yet.
const WIRING_HEADER: &str = "\
//! Auto-generated exercise wiring — maintained by the generator (M3).
//! Gitignored: references user-local generated exercises only.
";

/// Append an IDE-only module entry for `module` to the generated
/// exercises wiring file (idempotent). rust-analyzer then analyzes the
/// generated exercise; cargo never compiles it (cfg-gated).
pub fn wire_lib_rs(wiring_rs: &Path, module: &str, category: &str) -> Result<()> {
    let mut content = fs::read_to_string(wiring_rs).unwrap_or_default();
    if content.is_empty() {
        content.push_str(WIRING_HEADER);
        content.push('\n');
    }
    let marker = format!("mod {module};");
    if content.lines().any(|l| l.trim() == marker) {
        return Ok(());
    }
    if !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str(&format!(
        "\n#[cfg(rust_analyzer)]\n#[path = \"{category}/{module}.rs\"]\nmod {module};\n"
    ));
    fs::write(wiring_rs, content)
        .with_context(|| format!("写入 {} 失败", wiring_rs.display()))
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
confusion = "Python/JS 的 + 对数字字符串自动转换，初学者以为 i32 加法不挑类型"

body = '''
// 两个 i32 相加的小练习。
// add 目前只有占位宏，测试会 panic。
// TODO: 把占位换成 a + b。
// 提示 1：完成后两个测试都应通过。
// 提示 2：本题也用于生成器的离线冒烟测试。
// 说明 1：无槽位，默认值即可通过三重校验。
// 说明 2：练习文件会写入 generated 分类并自动接线。
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
            let mut p = Paths::from_root(&self.root);
            // Tests must never touch the user's real miss log.
            p.miss_log = None;
            p
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
        let out = generate(&Topic::Concept("test.concept".into()), &paths, None, None).unwrap();

        assert_eq!(out.name, "mini_add");
        assert_eq!(out.tier, Tier::Matched { template_id: "mini-add".into() });
        assert!(!out.used_llm);
        assert!(out.slots.is_empty());
        assert!(out.path.exists(), "{}", out.path.display());

        let src = fs::read_to_string(&out.path).unwrap();
        assert!(src.starts_with("// 迷你加法"));
        assert!(src.contains("#[cfg(test)]"));
        assert!(src.contains("I AM NOT DONE"));

        // Wired into the gitignored wiring file for rust-analyzer; the
        // header comment is created on first generation.
        let lib = fs::read_to_string(&paths.wiring_rs).unwrap();
        assert!(lib.contains("Auto-generated exercise wiring"));
        assert!(lib.contains("#[path = \"generated/mini_add.rs\"]"));
        assert!(lib.contains("mod mini_add;"));
        // The versioned lib.rs is never touched by the generator.
        let versioned = fs::read_to_string(fx.root.join("exercises/lib.rs")).unwrap();
        assert!(!versioned.contains("mini_add"));
    }

    #[test]
    fn second_generation_picks_free_name_and_wires_both() {
        let fx = Fixture::new();
        let paths = fx.paths();
        let out1 = generate(&Topic::Concept("test".into()), &paths, None, None).unwrap();
        let out2 = generate(&Topic::ErrorCode("E0308".into()), &paths, None, None).unwrap();
        assert_eq!(out1.name, "mini_add");
        assert_eq!(out2.name, "mini_add_2");
        let lib = fs::read_to_string(&paths.wiring_rs).unwrap();
        assert!(lib.contains("mod mini_add;"));
        assert!(lib.contains("mod mini_add_2;"));
    }

    #[test]
    fn error_code_routes_via_reverse_index() {
        let fx = Fixture::new();
        let out = generate_with_mode(&Topic::ErrorCode("e0308".into()), GenerateMode::Auto, &fx.paths(), &GenHistory::default(), None, None).unwrap();
        assert_eq!(out.tier, Tier::Matched { template_id: "mini-add".into() });
    }

    #[test]
    fn unknown_concept_and_code_are_clear_errors() {
        let fx = Fixture::new();
        let paths = fx.paths();
        let err = generate(&Topic::Concept("没有的东西".into()), &paths, None, None).unwrap_err();
        assert!(err.to_string().contains("解析为概念"), "{err}");
        let err = generate(&Topic::ErrorCode("E9999".into()), &paths, None, None).unwrap_err();
        assert!(err.to_string().contains("E9999"), "{err}");
    }

    #[test]
    fn free_text_matches_by_taxonomy_name_or_title() {
        let fx = Fixture::new();
        let paths = fx.paths();
        let out = generate(&Topic::FreeText("来一道 测试概念 的题".into()), &paths, None, None).unwrap();
        assert_eq!(out.tier, Tier::Matched { template_id: "mini-add".into() });
        let out = generate(&Topic::FreeText("加法".into()), &paths, None, None).unwrap();
        assert_eq!(out.tier, Tier::Matched { template_id: "mini-add".into() });
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
        let out = generate(&Topic::FreeText("随便来一道".into()), &paths, Some(&mut call), None).unwrap();
        assert_eq!(out.tier, Tier::Matched { template_id: "mini-add".into() });
        assert_eq!(out.slots.get("word").map(String::as_str), Some("b"));
        assert!(out.used_llm);
        assert!(*calls.borrow() >= 2);
    }

    #[test]
    fn garbage_llm_output_falls_back_to_defaults() {
        let fx = Fixture::new();
        let paths = fx.paths();
        let mut call = |_prompt: &str| -> Result<LlmReply> { Ok(reply("这不是 JSON")) };
        let out = generate(&Topic::Concept("test.concept".into()), &paths, Some(&mut call), None).unwrap();
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

    /// A gate-passing draft JSON (compile-fails with E0308, reference
    /// passes both tests) used by the tier-2/3 e2e tests below.
    const VALID_DRAFT_JSON: &str = r##"{
        "title": "迷你取余",
        "file_hint": "mini-rem",
        "concepts": ["test.concept"],
        "error_codes": ["E0308"],
        "difficulty": "easy",
        "constraints": [],
        "body": "// 计算 a 除以 b 的余数。\n// 说明行二。\n// 说明行三。\n// 说明行四。\n// 说明行五。\n// 说明行六。\nfn rem(a: i32, b: i32) -> i32 {\n    let q: String = a;\n    q\n}\n// I AM NOT DONE",
        "tests": "#[cfg(test)]\nmod tests {\n    use super::*;\n    #[test]\n    fn t1() {\n        assert_eq!(rem(7, 3), 1);\n    }\n    #[test]\n    fn t2() {\n        assert_eq!(rem(-7, 3), -1);\n    }\n}",
        "reference": "fn rem(a: i32, b: i32) -> i32 {\n    a % b\n}\n"
    }"##;

    #[test]
    fn free_generation_produces_a_gated_exercise() {
        let fx = Fixture::new();
        let paths = fx.paths();
        let mut call = |_prompt: &str| -> Result<LlmReply> { Ok(reply(VALID_DRAFT_JSON)) };
        let out = generate_with_mode(
            &Topic::FreeText("出一道取余的题".into()),
            GenerateMode::Free,
            &paths,
            &GenHistory::default(),
            Some(&mut call),
            None,
        )
        .unwrap();
        assert_eq!(out.tier, Tier::Free);
        assert_eq!(out.name, "mini_rem");
        assert_eq!(out.difficulty, template::Difficulty::Easy);
        assert_eq!(out.concepts, vec!["test.concept".to_string()]);
        assert!(out.used_llm);
        let src = fs::read_to_string(&out.path).unwrap();
        assert!(src.starts_with("// 迷你取余"), "{src}");
        assert!(src.contains("I AM NOT DONE"));
    }

    #[test]
    fn adapted_generation_uses_nearby_template_as_skeleton() {
        let fx = Fixture::new();
        let paths = fx.paths();
        // "加法" keyword-hits the mini-add template (its title/tests).
        let mut call = |_prompt: &str| -> Result<LlmReply> { Ok(reply(VALID_DRAFT_JSON)) };
        let out = generate_with_mode(
            &Topic::FreeText("加法".into()),
            GenerateMode::Adapted,
            &paths,
            &GenHistory::default(),
            Some(&mut call),
            None,
        )
        .unwrap();
        assert_eq!(out.tier, Tier::Adapted { base: "mini-add".into() });
        assert_eq!(out.name, "mini_rem");
    }

    #[test]
    fn auto_mode_falls_through_to_free_when_no_template_matches() {
        let fx = Fixture::new();
        let paths = fx.paths();
        // pick fails (no keyword hit), then the draft loop succeeds.
        let mut call = |prompt: &str| -> Result<LlmReply> {
            if prompt.contains("choosing a Rust practice") {
                Ok(reply(r#"{"template_id": "no-such-template"}"#))
            } else {
                Ok(reply(VALID_DRAFT_JSON))
            }
        };
        let out = generate_with_mode(
            &Topic::FreeText("完全无关的主题词汇".into()),
            GenerateMode::Auto,
            &paths,
            &GenHistory::default(),
            Some(&mut call),
            None,
        )
        .unwrap();
        assert_eq!(out.tier, Tier::Free);
    }

    #[test]
    fn tier2_concept_miss_falls_back_to_top_domain() {
        let fx = Fixture::new();
        let paths = fx.paths();
        let mut json = VALID_DRAFT_JSON.to_string();
        // test.concept.sub is not in the graph → falls back to test.concept.
        json = json.replace("\"concepts\": [\"test.concept\"]", "\"concepts\": [\"test.concept.sub\"]");
        let mut call = move |_prompt: &str| -> Result<LlmReply> { Ok(reply(&json)) };
        let out = generate_with_mode(
            &Topic::FreeText("任意".into()),
            GenerateMode::Free,
            &paths,
            &GenHistory::default(),
            Some(&mut call),
            None,
        )
        .unwrap();
        assert_eq!(out.concepts, vec!["test.concept".to_string()]);
    }

    /// M4.7: the draft loop must (a) request a token cap on the wire
    /// and (b) feed "length" truncations back as a compress-demand.
    #[test]
    fn tier3_bounds_tokens_and_handles_truncation() {
        let fx = Fixture::new();
        let paths = fx.paths();
        let caps = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let rec = caps.clone();
        struct BoundedRecorder {
            rec: std::rc::Rc<std::cell::RefCell<Vec<u32>>>,
            round: std::cell::Cell<u32>,
        }
        impl LlmCaller for BoundedRecorder {
            fn call(&mut self, prompt: &str) -> Result<LlmReply> {
                self.call_bounded(prompt, 1)
            }
            fn call_bounded(&mut self, _prompt: &str, max_tokens: u32) -> Result<LlmReply> {
                self.rec.borrow_mut().push(max_tokens);
                let n = self.round.get();
                self.round.set(n + 1);
                if n == 0 {
                    // Round 1: truncated output.
                    Ok(LlmReply {
                        content: "{\"title\": \"被截断\".to_st".to_string(),
                        usage: Default::default(),
                        finish_reason: Some("length".to_string()),
                    })
                } else {
                    Ok(reply(VALID_DRAFT_JSON))
                }
            }
        }
        let mut call = BoundedRecorder { rec, round: std::cell::Cell::new(0) };
        let out = generate_with_mode(
            &Topic::FreeText("取余".into()),
            GenerateMode::Free,
            &paths,
            &GenHistory::default(),
            Some(&mut call),
            None,
        )
        .unwrap();
        assert_eq!(out.tier, Tier::Free);
        let caps = caps.borrow();
        assert!(caps.iter().all(|&c| c == DRAFT_MAX_TOKENS), "cap must be {DRAFT_MAX_TOKENS}, got {caps:?}");
        assert_eq!(caps.len(), 2, "truncation triggered exactly one retry");
    }

    #[test]
    fn tier3_repair_loop_feeds_gate_failures_back() {
        let fx = Fixture::new();
        let paths = fx.paths();
        // First round: a draft whose body compiles (gate rejects: no
        // compile failure → rule/gate failure). Second round: valid.
        let bad = VALID_DRAFT_JSON.replace(r"let q: String = a;\n    q", "a % b");
        let calls = std::rc::Rc::new(std::cell::RefCell::new(0u32));
        let counter = calls.clone();
        let mut call = move |prompt: &str| -> Result<LlmReply> {
            let mut n = counter.borrow_mut();
            *n += 1;
            if *n == 1 {
                assert!(!prompt.contains("previous attempt FAILED"), "round 1 must be a fresh ask");
                Ok(reply(&bad))
            } else {
                assert!(prompt.contains("previous attempt FAILED"), "round 2 must carry the failure");
                Ok(reply(VALID_DRAFT_JSON))
            }
        };
        let out = generate_with_mode(
            &Topic::FreeText("取余".into()),
            GenerateMode::Free,
            &paths,
            &GenHistory::default(),
            Some(&mut call),
            None,
        )
        .unwrap();
        assert_eq!(*calls.borrow(), 2, "repair loop retried once");
        assert_eq!(out.attempts, 2);
        assert_eq!(out.tier, Tier::Free);
    }

    #[test]
    fn tier3_requires_llm() {
        let fx = Fixture::new();
        let err = generate_with_mode(
            &Topic::FreeText("取余".into()),
            GenerateMode::Free,
            &fx.paths(),
            &GenHistory::default(),
            None,
            None,
        )
        .unwrap_err();
        assert!(err.to_string().contains("LLM"), "{err}");
    }

    #[test]
    fn wire_lib_rs_is_idempotent() {
        let fx = Fixture::new();
        let paths = fx.paths();
        wire_lib_rs(&paths.wiring_rs, "thing", OUT_CATEGORY).unwrap();
        let once = fs::read_to_string(&paths.wiring_rs).unwrap();
        wire_lib_rs(&paths.wiring_rs, "thing", OUT_CATEGORY).unwrap();
        let twice = fs::read_to_string(&paths.wiring_rs).unwrap();
        assert_eq!(once, twice);
        assert_eq!(once.matches("mod thing;").count(), 1);
    }

    #[test]
    fn extract_json_variants() {
        assert_eq!(extract_json("前言 {\"a\": 1} 后记"), Some("{\"a\": 1}"));
        assert_eq!(extract_json("```json\n{\"b\": 2}\n```"), Some("{\"b\": 2}"));
        assert_eq!(extract_json("没有对象"), None);
    }

    // ------------------------------------------------------------------
    // M4.10: history-aware generation (dedup + variants)
    // ------------------------------------------------------------------

    /// Append a `word` slot to the mini template (same trick as
    /// `free_text_uses_llm_choice_and_slot_fill`).
    fn make_slotted(fx: &Fixture) {
        let mut with_slot = MINI_TEMPLATE.to_string();
        with_slot.push_str(
            "\n[[slots]]\nname = \"word\"\nkind = \"literal\"\nvalues = [\"a\", \"b\"]\ndefault = \"a\"\n",
        );
        let with_slot = with_slot.replace("// 提示 1：完成后两个测试都应通过。", "// 填槽值：{{word}}。");
        fs::write(fx.root.join("templates/mini-add.toml"), with_slot).unwrap();
    }

    /// A second template matching the same concept, with a `word` slot.
    fn add_second_template(fx: &Fixture) {
        let toml = MINI_TEMPLATE
            .replace("mini-add", "mini-sub")
            .replace("迷你加法", "迷你减法")
            // Subtraction semantics: fix the expectations, not just the
            // operator (the gate would rightly reject a + b tests).
            .replace("assert_eq!(add(1, 2), 3);", "assert_eq!(add2(1, 2), -1);")
            .replace("assert_eq!(add(-1, 1), 0);", "assert_eq!(add2(-1, 1), -2);")
            .replace("fn add(", "fn add2(")
            .replace("a + b", "a - b")
            .replace("// 两个 i32 相加的小练习。", "// 两个 i32 相减的小练习。")
            // The slot must appear in the body (rule filter checks it).
            .replace("// 提示 1：完成后两个测试都应通过。", "// 填槽值：{{word}}。")
            + "\n[[slots]]\nname = \"word\"\nkind = \"literal\"\nvalues = [\"x\", \"y\"]\ndefault = \"x\"\n";
        fs::write(fx.root.join("templates/mini-sub.toml"), toml).unwrap();
    }

    fn history_with(template_id: &str, times: u32, prev: &[(&str, &str)]) -> GenHistory {
        let mut h = GenHistory::default();
        h.used.insert(
            template_id.to_string(),
            TemplateUse {
                times,
                all_passed: true,
                prev_slot_values: vec![prev
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect()],
            },
        );
        h
    }

    #[test]
    fn unused_template_ranks_first() {
        let fx = Fixture::new();
        make_slotted(&fx);
        add_second_template(&fx);
        let h = history_with("mini-add", 1, &[("word", "a")]);
        let out = generate_with_history(
            &Topic::Concept("test.concept".into()),
            &fx.paths(),
            &h,
            None,
            None,
        )
        .unwrap();
        // mini-add was already served → the fresh sibling wins.
        assert_eq!(out.tier, Tier::Matched { template_id: "mini-sub".into() });
        assert!(!out.variant);
    }

    #[test]
    fn reused_template_becomes_rotated_variant() {
        let fx = Fixture::new();
        make_slotted(&fx);
        let h = history_with("mini-add", 1, &[("word", "a")]);
        let out = generate_with_history(
            &Topic::Concept("test.concept".into()),
            &fx.paths(),
            &h,
            None,
            None,
        )
        .unwrap();
        assert!(out.variant, "a reused template must be flagged as variant");
        // Rotation base = past generations (1) → not the default fill "a".
        assert_eq!(out.slots.get("word").map(String::as_str), Some("b"));
    }

    #[test]
    fn used_slotless_template_skips_to_llm_tiers() {
        let fx = Fixture::new();
        let h = history_with("mini-add", 1, &[]);
        // mini-add has no slots: a repeat would be the identical
        // question, so tier 1 declines and — offline — the auto chain
        // stops with the structured "matched failed" error.
        let err = generate_with_history(
            &Topic::Concept("test.concept".into()),
            &fx.paths(),
            &h,
            None,
            None,
        )
        .unwrap_err();
        assert!(err.to_string().contains("模板直配失败"), "{err}");
    }

    #[test]
    fn llm_pick_prompt_carries_usage_and_precision_rules() {
        let fx = Fixture::new();
        make_slotted(&fx);
        let h = history_with("mini-add", 1, &[("word", "a")]);
        let prompts = std::cell::RefCell::new(Vec::<String>::new());
        let mut call = |prompt: &str| -> Result<LlmReply> {
            prompts.borrow_mut().push(prompt.to_string());
            if prompt.contains("choosing a Rust practice") {
                Ok(reply(r#"{"template_id": "mini-add"}"#))
            } else {
                Ok(reply(r#"{"slots": {"word": "b"}}"#))
            }
        };
        let out = generate_with_history(
            &Topic::FreeText("随便来一道".into()),
            &fx.paths(),
            &h,
            Some(&mut call),
            None,
        )
        .unwrap();
        let picks = prompts.borrow();
        let pick_prompt = picks.iter().find(|p| p.contains("choosing a Rust practice")).unwrap();
        // §6.2 precision tightening + usage annotation both present.
        assert!(pick_prompt.contains("SPECIFIC technique"), "precision rule missing");
        assert!(pick_prompt.contains("ALREADY USED x1"), "usage annotation missing");
        assert!(pick_prompt.contains("NOT marked ALREADY USED"), "preference rule missing");
        // Variant fill prompt cites the previous values.
        let fill_prompt = picks.iter().find(|p| p.contains("filling named slots")).unwrap();
        assert!(fill_prompt.contains("already used"), "{fill_prompt}");
        assert!(fill_prompt.contains("word=a"), "past fill values missing");
        assert!(out.variant);
    }

    #[test]
    fn gen_history_from_index() {
        let dir = std::env::temp_dir().join(format!("rustlings_hist_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("index.json");
        fs::write(
            &path,
            r#"{
  "generated/a.rs": {"path": "generated/a.rs", "title": "a",
    "source": {"kind": "template-fill", "template_id": "t1"},
    "status": "passed", "slots": {"word": "a"}},
  "generated/b.rs": {"path": "generated/b.rs", "title": "b",
    "source": {"kind": "template-fill", "template_id": "t1"},
    "status": {"failed": {"times": 1}}},
  "generated/c.rs": {"path": "generated/c.rs", "title": "c",
    "source": {"kind": "free"}, "status": "passed"},
  "fixtures/d.rs": {"path": "fixtures/d.rs", "title": "d",
    "source": {"kind": "seed"}, "status": "pending"}
}"#,
        )
        .unwrap();
        let index = crate::exercise::index::ExerciseIndex::load_from(path);
        let h = GenHistory::from_index(&index);
        let t1 = h.get("t1").unwrap();
        assert_eq!(t1.times, 2);
        assert!(!t1.all_passed, "one failed attempt → not all passed");
        assert_eq!(t1.prev_slot_values.len(), 1);
        assert!(h.get("t2").is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    fn reply(content: &str) -> LlmReply {
        LlmReply {
            content: content.to_string(),
            usage: Default::default(),
            finish_reason: Some("stop".to_string()),
        }
    }
}
