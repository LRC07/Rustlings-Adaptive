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

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use serde_json::Value;

use crate::llm::{extract_json, LlmReply};
use crate::taxonomy::ConceptGraph;
use crate::{constraints, template, verifier};

/// How many slot-fill/verify rounds before giving up.
pub const MAX_ATTEMPTS: u32 = 3;
/// Category directory (under `exercises/`) for generated exercises.
pub const OUT_CATEGORY: &str = "generated";

/// Learner-profile signals steering L2/L3 draft difficulty and scenario
/// choice (M9h, closes the M8.1 "level 信号" gap): built by the CLI /
/// agent tool from the learner's local profile + exercise index and
/// rendered into the draft prompt. Tier 1 stays profile-free — the
/// pick-precision rules from M4.10/M4.16 must not be perturbed.
#[derive(Debug, Clone, Default)]
pub struct LearnerContext {
    /// (concept id, fails, attempts), most failed first.
    pub weak: Vec<(String, u32, u32)>,
    /// SM-2 concepts whose review is due.
    pub due: Vec<String>,
    /// (error code, count), most frequent first.
    pub codes: Vec<(String, u32)>,
    /// Concepts the learner marked 太难 in feedback.
    pub too_hard: Vec<String>,
    /// Concepts the learner marked 太简单 in feedback.
    pub too_easy: Vec<String>,
}

impl LearnerContext {
    pub fn is_empty(&self) -> bool {
        self.weak.is_empty()
            && self.due.is_empty()
            && self.codes.is_empty()
            && self.too_hard.is_empty()
            && self.too_easy.is_empty()
    }

    /// Collect from the learner's local data (`~/.rustlings_adaptive/`
    /// profile + the exercise index's feedback marks). Never fails:
    /// missing data → empty context (offline / fresh install).
    pub fn from_local(exercises_dir: &Path) -> Self {
        let mut ctx = Self::default();
        let store = crate::profile::ProfileStore::load_or_create();
        ctx.weak = store.profile.weakest(5);
        ctx.due = store
            .profile
            .due_concepts(chrono::Utc::now())
            .into_iter()
            .take(5)
            .collect();
        ctx.codes = store.profile.top_error_codes(4);
        let index = crate::exercise::index::ExerciseIndex::load(exercises_dir);
        for meta in index.iter() {
            match meta.feedback {
                Some(crate::exercise::index::Feedback::TooHard) => {
                    ctx.too_hard.extend(meta.concepts.iter().cloned())
                }
                Some(crate::exercise::index::Feedback::TooEasy) => {
                    ctx.too_easy.extend(meta.concepts.iter().cloned())
                }
                _ => {}
            }
        }
        for list in [&mut ctx.too_hard, &mut ctx.too_easy] {
            list.sort();
            list.dedup();
            list.truncate(5);
        }
        ctx
    }

    /// The `## Learner profile` prompt block ("" when nothing to say).
    pub fn prompt_block(&self) -> String {
        if self.is_empty() {
            return String::new();
        }
        let mut s =
            String::from("\n## Learner profile (calibrate difficulty and scenario to this)\n");
        if !self.weak.is_empty() {
            let list = self
                .weak
                .iter()
                .map(|(c, f, a)| format!("{c} ({f} fails / {a} attempts)"))
                .collect::<Vec<_>>()
                .join(", ");
            s.push_str(&format!("- Weakest concepts (most failed): {list}\n"));
        }
        if !self.due.is_empty() {
            s.push_str(&format!("- Due for spaced review: {}\n", self.due.join(", ")));
        }
        if !self.codes.is_empty() {
            let list =
                self.codes.iter().map(|(c, n)| format!("{c} ×{n}")).collect::<Vec<_>>().join(", ");
            s.push_str(&format!("- Frequent error codes: {list}\n"));
        }
        if !self.too_hard.is_empty() {
            s.push_str(&format!(
                "- The learner marked these concepts TOO HARD: {}\n",
                self.too_hard.join(", ")
            ));
        }
        if !self.too_easy.is_empty() {
            s.push_str(&format!(
                "- The learner marked these concepts TOO EASY: {}\n",
                self.too_easy.join(", ")
            ));
        }
        s.push_str(
            "Aim at the learner's edge: one small step beyond what the profile says they can \
             already do; do not re-teach what they have repeatedly passed.\n",
        );
        s
    }
}

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
/// Also carries the RECENTLY TRAINED concepts across ALL tiers
/// (0909_2 反馈 P1): same-template variants were detected, but
/// cross-template and cross-layer same-concept repeats were not —
/// the pick prompt now sees them.
#[derive(Debug, Clone, Default)]
pub struct GenHistory {
    used: std::collections::BTreeMap<String, TemplateUse>,
    /// Concepts of the learner's most recent exercises (newest first,
    /// deduped), across every generation tier.
    recent_concepts: Vec<String>,
}

/// How many recent exercises feed the concept-recency signal.
const RECENT_EXERCISE_LIMIT: usize = 5;

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
        // Recent concepts (0909_2 P1): newest first by created_at —
        // only generated entries carry it; index order is by path and
        // says nothing about recency.
        let mut dated: Vec<&crate::exercise::index::ExerciseMeta> =
            index.iter().filter(|m| m.created_at.is_some()).collect();
        dated.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        let mut recent_concepts: Vec<String> = Vec::new();
        for meta in dated.into_iter().take(RECENT_EXERCISE_LIMIT) {
            for c in &meta.concepts {
                if !recent_concepts.contains(c) {
                    recent_concepts.push(c.clone());
                }
            }
        }
        Self { used, recent_concepts }
    }

    pub fn times(&self, template_id: &str) -> u32 {
        self.used.get(template_id).map(|u| u.times).unwrap_or(0)
    }

    pub fn get(&self, template_id: &str) -> Option<&TemplateUse> {
        self.used.get(template_id)
    }

    /// Prompt block listing the recently trained concepts (0909_2 P1),
    /// or "" when the index has no dated entries yet.
    fn recent_block(&self) -> String {
        if self.recent_concepts.is_empty() {
            return String::new();
        }
        format!(
            "\n\nRecently trained concepts (the learner's LAST exercises, newest first): {}.\n\
             Do NOT re-serve these same concepts again unless the user EXPLICITLY asks for \
             that exact topic — cross-template same-concept repeats waste practice time. \
             Prefer an adjacent-but-different concept, or say no_match (the free tier will \
             build a fresh scenario).\n",
            self.recent_concepts.join(", ")
        )
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
    /// Hidden reference solution (M5.1: persisted in the exercise index
    /// so the review gate / debrief can compare against it later).
    pub reference: String,
    /// Constraint spec strings of the exercise (M5.1: persisted for the
    /// review gate's static layer and the debrief comparison table).
    pub constraints: Vec<String>,
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

/// Per-tier caller routing (0909_2 反馈 职能分开): the template path
/// (tier-1 pick + slot fill, tier-2 adaptation) and the free-form
/// tier-3 draft have OPPOSITE capability profiles — a cheap fast model
/// excels at the former (micro-classification, small JSON), a strong
/// patient one at the latter (long-output creation). Callers that do
/// not split just return themselves for both (blanket impl below), so
/// every existing single-caller site keeps working unchanged.
pub trait TieredLlmCaller {
    /// Tier-1 pick/fill and tier-2 adaptation.
    fn template_path(&mut self) -> &mut dyn LlmCaller;
    /// Tier-3 free-form draft.
    fn free_path(&mut self) -> &mut dyn LlmCaller;
}

impl<T: LlmCaller> TieredLlmCaller for T {
    fn template_path(&mut self) -> &mut dyn LlmCaller {
        self
    }
    fn free_path(&mut self) -> &mut dyn LlmCaller {
        self
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
///
/// `adopt_first_error` (9.6 实测): for LLM drafts (tiers 2/3) the
/// declared error codes are the model's GUESS, while the unfinished
/// body's real first error is knowable — a mismatch is no longer a
/// rejected round but a self-correcting adoption (reality wins). Tier 1
/// keeps the strict check: a hand template's declared codes are
/// curated metadata (题卡 / 反查路由 / fixture 轮转测试都依赖它).
pub fn gate_draft(d: &mut template::ExerciseDraft, workdir: &Path) -> Result<verifier::VerifyReport> {
    gate_draft_with_policy(d, workdir, false)
}

/// Policy switch of `gate_draft` (`adopt = true` → L2/L3 self-correcting
/// first-error handling, see above).
pub fn gate_draft_with_policy(
    d: &mut template::ExerciseDraft,
    workdir: &Path,
    adopt_first_error: bool,
) -> Result<verifier::VerifyReport> {
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
    if adopt_first_error {
        adopt_real_first_error(d, &report);
    } else {
        template::first_error_matches(&d.error_codes, &report)?;
    }
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

/// Self-correcting first-error adoption (9.6 实测): the exercise's
/// essence is the KNOWLEDGE POINT, not which compile error happens to
/// surface first — an LLM draft whose declared codes miss the body's
/// real first error gets its declaration replaced by the observed code
/// instead of burning a repair round. Returns true when adopted.
/// Todo-type drafts (body compiles, tests fail) are untouched.
fn adopt_real_first_error(d: &mut template::ExerciseDraft, report: &verifier::VerifyReport) -> bool {
    let Some(actual) = &report.first_error_code else {
        return false;
    };
    if d.error_codes.iter().any(|c| c == actual) {
        return false;
    }
    d.error_codes = vec![actual.clone()];
    true
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
    llm: Option<&mut dyn TieredLlmCaller>,
    progress: Option<&mut dyn FnMut(GenerateStage)>,
) -> Result<Outcome> {
    generate_with_history(topic, paths, &GenHistory::default(), None, llm, progress)
}

/// History-aware entry (M4.10): `history` carries which templates the
/// learner already received (from the exercise index). Unused
/// candidates rank first; a repeated template becomes a slot-rotated
/// variant instead of the same question again. `learner` (M9h) carries
/// the learner-profile signals for tiers 2/3 (`None` = offline/no data).
pub fn generate_with_history(
    topic: &Topic,
    paths: &Paths,
    history: &GenHistory,
    learner: Option<&LearnerContext>,
    llm: Option<&mut dyn TieredLlmCaller>,
    progress: Option<&mut dyn FnMut(GenerateStage)>,
) -> Result<Outcome> {
    generate_with_focus(topic, None, paths, history, learner, llm, progress)
}

/// Focus-aware entry (考察点精度): `focus` carries the SPECIFIC
/// technique the learner asked for ("entry API 的 or_insert/and_modify
/// 单次查找"), while `topic` stays the domain anchor (concept id /
/// error code / free text). When focus is present every tier treats
/// the request as precision-sensitive: tier 1 must pick a template
/// that trains exactly that technique (else no_match → tiers 2/3),
/// and tiers 2/3 put it in the draft prompt.
pub fn generate_with_focus(
    topic: &Topic,
    focus: Option<&str>,
    paths: &Paths,
    history: &GenHistory,
    learner: Option<&LearnerContext>,
    llm: Option<&mut dyn TieredLlmCaller>,
    progress: Option<&mut dyn FnMut(GenerateStage)>,
) -> Result<Outcome> {
    generate_with_mode(topic, focus, GenerateMode::Auto, paths, history, learner, llm, progress)
}

/// Full-parameter entry for the agent tool: `mode` here is only the
/// user-intent relay from M4.14 ("free" skips tiers 1/2 when the user
/// explicitly asked for no-template generation); the strategy stays
/// program-controlled otherwise. `free_preset_fail` (0907 反馈 P4) is
/// the PREVIOUS free-round rejection reason: free generation runs ONE
/// round per call and the tool asks the user before spending another —
/// this preset is how the repair feedback survives across calls.
#[allow(clippy::too_many_arguments)]
pub(crate) fn generate_full(
    topic: &Topic,
    focus: Option<&str>,
    mode: GenerateMode,
    paths: &Paths,
    history: &GenHistory,
    learner: Option<&LearnerContext>,
    llm: Option<&mut dyn TieredLlmCaller>,
    progress: Option<&mut dyn FnMut(GenerateStage)>,
    free_preset_fail: &str,
) -> Result<Outcome> {
    if matches!(mode, GenerateMode::Free) {
        let Some(call) = llm else {
            bail!("该模式需要 LLM；未配置 API Key");
        };
        let mut prog = progress;
        if let Some(cb) = prog.as_mut() {
            cb(GenerateStage { attempt: 1, total_attempts: 1, stage: "自由生成", note: None });
        }
        return generate_free(topic, focus, &load_linked_graph(paths)?, paths, learner, call.free_path(), &mut prog, 1, free_preset_fail);
    }
    generate_with_mode(topic, focus, mode, paths, history, learner, llm, progress)
}

/// Mode-parameterized entry — crate-internal (tests). The public
/// surface always runs the automatic fall-through: the user and the
/// coach model never pick the tier (M4.7 decision).
#[allow(clippy::too_many_arguments)]
pub(crate) fn generate_with_mode(
    topic: &Topic,
    focus: Option<&str>,
    mode: GenerateMode,
    paths: &Paths,
    history: &GenHistory,
    learner: Option<&LearnerContext>,
    mut llm: Option<&mut dyn TieredLlmCaller>,
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

    let graph = load_linked_graph(paths)?;
    let templates = template::load_dir(&paths.templates_dir).context("模板库加载失败")?;
    if templates.is_empty() {
        bail!("模板库为空（{}）", paths.templates_dir.display());
    }

    // Tier 1 always runs first (cheap; the other tiers need an LLM).
    if matches!(mode, GenerateMode::Auto | GenerateMode::Matched) {
        let sel_call: Option<&mut dyn LlmCaller> = match llm.as_mut() {
            Some(c) => Some(c.template_path()),
            None => None,
        };
        stage!("选模板", 1, MAX_ATTEMPTS);
        match generate_matched(
            topic,
            focus,
            &templates,
            &graph,
            paths,
            history,
            sel_call,
            &mut progress,
        ) {
            Ok(o) => Ok(o),
            Err(tier1_err) => {
                // Demand telemetry (§7.5 flywheel): a tier-1 miss with a
                // focus means the library has no template training that
                // SPECIFIC technique (or all candidates are exhausted)
                // — batch planning reads this log.
                log_generate_miss(paths, topic, focus, &tier1_err);
                if matches!(mode, GenerateMode::Matched) {
                    Err(tier1_err)
                } else {
                let Some(call) = llm.as_deref_mut() else {
                    bail!(
                        "模板直配失败：{tier1_err:#}\n（未配置 LLM 时只能用模板直配；配置 Key 后可用改编/自由生成）"
                    );
                };
                stage!("模板改编", 1, LLM_ATTEMPTS);
                match generate_adapted(topic, focus, &templates, &graph, paths, learner, call.template_path(), &mut progress) {
                    Ok(o) => Ok(o),
                    Err(tier2_err) => {
                        // Free tier: ONE round (0907 反馈 P4) — burning
                        // three expensive rounds inside the tool was the
                        // "重试风暴" cost sink; further rounds are the
                        // user's explicit choice (checkpoint in the tool).
                        stage!("自由生成", 1, 1);
                        generate_free(topic, focus, &graph, paths, learner, call.free_path(), &mut progress, 1, "")
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
            generate_adapted(topic, focus, &templates, &graph, paths, learner, call.template_path(), &mut progress)
        } else {
            stage!("自由生成", 1, 1);
            generate_free(topic, focus, &graph, paths, learner, call.free_path(), &mut progress, 1, "")
        }
    }
}

/// Graph + template linkage shared by the mode entries (the draft
/// prompt needs the concept id list; normalize_concepts needs the
/// graph).
fn load_linked_graph(paths: &Paths) -> Result<ConceptGraph> {
    let mut graph = ConceptGraph::load(&paths.taxonomy_file).context("概念图谱加载失败")?;
    let templates = template::load_dir(&paths.templates_dir).context("模板库加载失败")?;
    if templates.is_empty() {
        bail!("模板库为空（{}）", paths.templates_dir.display());
    }
    let items: Vec<(&str, &[String])> =
        templates.iter().map(|t| (t.id.as_str(), t.concepts.as_slice())).collect();
    graph.link_templates(items).context("模板与概念图谱对不上")?;
    Ok(graph)
}

/// Append a tier-1 miss to the miss log (best-effort). Two kinds:
/// `template_no_match` (nothing trains the requested technique) and
/// `template_exhausted` (all candidates served, no slot variation left).
fn log_generate_miss(paths: &Paths, topic: &Topic, focus: Option<&str>, err: &anyhow::Error) {
    let Some(path) = paths.miss_log.as_ref() else { return };
    let kind = if format!("{err:#}").contains("都已出过") {
        "template_exhausted"
    } else {
        "template_no_match"
    };
    let event = serde_json::json!({
        "ts": chrono::Utc::now().to_rfc3339(),
        "kind": kind,
        "topic": topic.prompt_text(),
        "focus": focus.unwrap_or_default(),
        "error": err.to_string().chars().take(160).collect::<String>(),
    });
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        use std::io::Write;
        let _ = writeln!(f, "{event}");
    }
}

/// Tier 1: pick a template and fill its slots (the M3 pipeline).
#[allow(clippy::too_many_arguments)]
fn generate_matched(
    topic: &Topic,
    focus: Option<&str>,
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

    let pick = choose_template(templates, graph, topic, focus, history, sel_llm.as_deref_mut())?;
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
        let gated = gate_draft(&mut draft, &workdir);
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
                reference: draft.reference.clone(),
                constraints: draft.constraints.clone(),
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
pub const LLM_ATTEMPTS: u32 = 3;

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
#[allow(clippy::too_many_arguments)]
fn generate_adapted(
    topic: &Topic,
    focus: Option<&str>,
    templates: &[template::Template],
    graph: &ConceptGraph,
    paths: &Paths,
    learner: Option<&LearnerContext>,
    call: &mut dyn LlmCaller,
    progress: &mut Option<&mut dyn FnMut(GenerateStage)>,
) -> Result<Outcome> {
    let base = nearest_template(templates, graph, topic)?;
    let DraftResult { draft, module_name, hints, attempts } = llm_draft_loop(
        &topic.prompt_text(),
        focus,
        Some(base),
        graph,
        paths,
        learner,
        call,
        progress,
        "模板改编",
        LLM_ATTEMPTS,
        "",
        DRAFT_MAX_TOKENS,
    )?;
    finish_draft(paths, draft, Tier::Adapted { base: base.id.clone() }, module_name, hints, attempts)
}

/// Tier 3: LLM produces an exercise from scratch. `max_rounds` is 1 in
/// production (0907 反馈 P4: free rounds are the expensive kind — more
/// rounds are the user's explicit choice via the tool's checkpoint);
/// `preset_fail` carries the previous call's rejection reason so the
/// repair feedback survives the per-round checkpoint loop.
#[allow(clippy::too_many_arguments)]
fn generate_free(
    topic: &Topic,
    focus: Option<&str>,
    graph: &ConceptGraph,
    paths: &Paths,
    learner: Option<&LearnerContext>,
    call: &mut dyn LlmCaller,
    progress: &mut Option<&mut dyn FnMut(GenerateStage)>,
    max_rounds: u32,
    preset_fail: &str,
) -> Result<Outcome> {
    let DraftResult { draft, module_name, hints, attempts } = llm_draft_loop(
        &topic.prompt_text(),
        focus,
        None,
        graph,
        paths,
        learner,
        call,
        progress,
        "自由生成",
        max_rounds,
        preset_fail,
        FREE_DRAFT_MAX_TOKENS,
    )?;
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
        reference: draft.reference.clone(),
        constraints: draft.constraints.clone(),
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
/// Among candidates the MOST SPECIFIC hit wins (0909 反馈 A: score =
/// keyword-hit count of its covering concept; ties keep file order).
fn nearest_template<'a>(
    templates: &'a [template::Template],
    graph: &ConceptGraph,
    topic: &Topic,
) -> Result<&'a template::Template> {
    let pool = keyword_candidates(templates, graph, &topic.prompt_text());
    let mut best: Option<(&template::Template, u32)> = None;
    for t in templates.iter().filter(|t| pool.ids.contains(&t.id)) {
        let s = pool.scores.get(&t.id).copied().unwrap_or(0);
        match best {
            Some((_, bs)) if bs >= s => {}
            _ => best = Some((t, s)),
        }
    }
    best.map(|(t, _)| t).ok_or_else(|| anyhow!("没有关键词命中的模板可作改编骨架"))
}

/// Hard cap on one draft round's completion tokens (M4.7): the whole
/// exercise JSON must fit; "length" truncations are fed back to the
/// model with a demand to compress. Template path (tier-2 adaptation)
/// only — it has a skeleton, 3000 has been comfortable.
pub const DRAFT_MAX_TOKENS: u32 = 3000;
/// Free-form tier-3 cap (0909_2 round4): loose from DRAFT_MAX_TOKENS —
/// glm-52-low truncated at exactly 3000 twice and failed, and some
/// endpoints count THINKING tokens against this budget (M9r: reasoning
/// 699/700). A ceiling, not a target: it costs nothing unless the model
/// rambles, while a truncation-retry round costs a full call.
pub const FREE_DRAFT_MAX_TOKENS: u32 = 5000;
/// Wall-clock budget for the whole draft repair loop (M4.7): on slow
/// endpoints, stop with a clear report instead of burning rounds.
pub const DRAFT_TIME_BUDGET: Duration = Duration::from_secs(420);

/// The repair loop: prompt → parse → normalize → gate; failures (with
/// the real rustc diagnostics) are fed back for the next round.
/// `max_rounds` bounds the loop (3 for adapted; 1 for free — more free
/// rounds are the user's explicit choice, 0907 反馈 P4). `preset_fail`
/// seeds `last_fail` so a preset (cross-call) reason reaches even the
/// first prompt. `max_tokens` is the per-round completion cap —
/// DRAFT_MAX_TOKENS for the template path, FREE_DRAFT_MAX_TOKENS for
/// the free tier (0909_2 round4).
#[allow(clippy::too_many_arguments)]
fn llm_draft_loop(
    request: &str,
    focus: Option<&str>,
    base: Option<&template::Template>,
    graph: &ConceptGraph,
    paths: &Paths,
    learner: Option<&LearnerContext>,
    call: &mut dyn LlmCaller,
    progress: &mut Option<&mut dyn FnMut(GenerateStage)>,
    stage_label: &'static str,
    max_rounds: u32,
    preset_fail: &str,
    max_tokens: u32,
) -> Result<DraftResult> {
    let concept_ids: Vec<String> = graph.ids().cloned().collect();
    let started = Instant::now();
    let mut last_fail = preset_fail.to_string();
    let mut prev_fail = String::new();
    for attempt in 1..=max_rounds {
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
        // Cost guard (9.6 实测"重试风暴"：4 轮 11 次调用 307s): if the
        // previous round failed for the SAME reason, feeding it back
        // again is very unlikely to help — stop burning calls and let
        // the designed degradation path (structured fallback → suggest
        // a concrete topic / faster model) take over.
        if attempt > 2 && !last_fail.is_empty() && last_fail == prev_fail {
            bail!(
                "连续两轮因同一原因被拒（{last_fail}）——继续重试意义不大。\
                 建议换一个更具体的主题或错误码（如 E0382，走模板直配），\
                 或 /model 切换更快的模型后重试。"
            );
        }
        prev_fail = last_fail.clone();
        if let Some(cb) = progress.as_mut() {
            let note = (!last_fail.is_empty())
                .then(|| last_fail.chars().take(110).collect::<String>());
            cb(GenerateStage { attempt, total_attempts: max_rounds, stage: stage_label, note });
        }

        let prompt = draft_prompt(request, focus, base, &concept_ids, attempt, &last_fail, learner);
        let reply = call.call_bounded(&prompt, max_tokens).map_err(|e| anyhow!("LLM 调用失败：{e:#}"))?;
        if reply.finish_reason.as_deref() == Some("length") {
            last_fail = format!(
                "上一轮输出在 {max_tokens} tokens 处被截断：整题 JSON 必须更精简\
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

        let pre_codes = draft.error_codes.clone();
        let workdir = fresh_workdir("rustlings_llm")?;
        let gated = gate_draft_with_policy(&mut draft, &workdir, true);
        let _ = fs::remove_dir_all(&workdir);
        match gated {
            Ok(_report) => {
                // 0909_2 反馈 P2: the gate may have REPLACED the declared
                // codes with the real first error (adopt_first_error) —
                // if the body/comments still mention a replaced code the
                // label would mislead the learner. ONE best-effort sync
                // round asks the model to align labels; on any failure
                // the gate-passed original (whose codes are already the
                // REAL ones) ships anyway.
                let replaced: Vec<String> =
                    pre_codes.iter().filter(|c| !draft.error_codes.contains(*c)).cloned().collect();
                let stale = !replaced.is_empty()
                    && replaced.iter().any(|c| draft.body.contains(c.as_str()));
                if stale {
                    let sync_fail = format!(
                        "你标注的错误码 {} 与实际诊断 {} 不符——已被按实际改判。更新题面注释与 \
                         error_codes 保持一致后，重新输出完整 JSON",
                        replaced.join("、"),
                        draft.error_codes.join("、")
                    );
                    let sync_prompt =
                        draft_prompt(request, focus, base, &concept_ids, attempt, &sync_fail, learner);
                    if let Ok(reply) = call.call_bounded(&sync_prompt, max_tokens)
                        && reply.finish_reason.as_deref() != Some("length")
                        && let Some(json) = extract_json(&reply.content)
                        && let Ok(wire) = serde_json::from_str::<DraftWire>(json)
                    {
                        let concepts2 = normalize_concepts(&wire.concepts, graph, paths);
                        if !concepts2.is_empty() {
                            let mut d2 = template::ExerciseDraft {
                                title: wire.title,
                                concepts: concepts2,
                                error_codes: wire.error_codes,
                                difficulty: wire.difficulty,
                                constraints: wire.constraints,
                                body: wire.body,
                                tests: wire.tests,
                                reference: wire.reference,
                            };
                            ensure_ban_constraints(&mut d2);
                            let wd2 = fresh_workdir("rustlings_llm_sync")?;
                            let ok2 = match gate_draft_with_policy(&mut d2, &wd2, true) {
                                Ok(_) => true,
                                Err(e) => { eprintln!("SYNC GATE ERR: {e:#}"); false }
                            };
                            let _ = fs::remove_dir_all(&wd2);
                            if ok2 {
                                calibrate_difficulty(&mut d2);
                                return Ok(DraftResult {
                                    draft: d2,
                                    module_name,
                                    hints: draft_hints,
                                    attempts: attempt,
                                });
                            }
                        }
                    }
                }
                let mut draft = draft;
                calibrate_difficulty(&mut draft);
                return Ok(DraftResult { draft, module_name, hints: draft_hints, attempts: attempt });
            }
            Err(e) => {
                last_fail = format!("{e:#}");
            }
        }
    }
    bail!(
        "连续 {max_rounds} 轮未产出合格题目（最后原因：{last_fail}）。\
         建议换一个更具体的主题或错误码（如 E0382，走模板直配），或 /model 切换更快的模型后重试。"
    )
}

/// Difficulty floor for generated drafts (9.6 实测 B3：综合题被 LLM 自
/// 标"简单"——它的难度语义只看"应用几个已知修复"，不看综合范围）。
/// A draft touching ≥2 concepts or carrying ≥2 TRAINING constraints is
/// at least medium, whatever the model claimed. The tier-2/3 safety
/// bans (`ban=…` appended by `ensure_ban_constraints`) are guardrails,
/// not difficulty contributors, and are excluded; tier-1 template
/// fills keep their hand-rated difficulty.
fn calibrate_difficulty(d: &mut template::ExerciseDraft) {
    let training = d.constraints.iter().filter(|c| !c.starts_with("ban=")).count();
    if d.difficulty == template::Difficulty::Easy
        && (d.concepts.len() >= 2 || training >= 2)
    {
        d.difficulty = template::Difficulty::Medium;
    }
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

/// The draft prompt: request, focus and rubric (spec C1–C7,
/// compressed), plus an optional reference skeleton (tier 2), the
/// retry feedback and the learner-profile block (M9h level 信号).
#[allow(clippy::too_many_arguments)]
fn draft_prompt(
    request: &str,
    focus: Option<&str>,
    base: Option<&template::Template>,
    concept_ids: &[String],
    attempt: u32,
    last_fail: &str,
    learner: Option<&LearnerContext>,
) -> String {
    let learner_block = match learner {
        Some(l) => l.prompt_block(),
        None => String::new(),
    };
    let skeleton = match base {
        Some(b) => format!(
            "## Reference exercise (adapt its STRUCTURE to the topic below; do NOT copy it verbatim)\n\
             title: {}\n--- body ---\n{}\n--- tests ---\n{}\n--- reference ---\n{}\n",
            b.title, b.body, b.tests, b.reference
        ),
        None => "## Reference exercise: none — free-form. Write the smallest exercise that \
                 teaches the topic.\n\
                 Discrimination is the whole point (0907 实测): the unfinished body must fail \
                 to COMPILE with exactly the declared beginner mistake — a body that compiles, \
                 or whose first error is unrelated to the topic/concepts, trains nothing and \
                 is auto-rejected by the gate.\n"
            .into(),
    };
    // 0909_2 反馈（测试者建议）: constructive requests must be SHRUNK to
    // one error slice — the 44-line todo-manager rejection was the model
    // drafting the whole app instead of one borrow-error fragment.
    let shrink = if looks_constructive(request) {
        "\n\n## Constructive request — shrink it to ONE error slice\n\
         The user asked to BUILD something (实现/写一个/做一个…). Do NOT draft the whole \
         application: imagine it, then cut everything that does not carry the error — \
         keep ONE small slice (a single function or tiny struct, well within the line \
         cap) with ONE plausible beginner mistake that teaches ONE Rust lesson.\n"
        .to_string()
    } else {
        String::new()
    };
    let retry = if attempt > 1 || !last_fail.is_empty() {
        format!(
            "\n## Your previous attempt FAILED the quality gate. Fix ALL of it:\n{last_fail}\n\
             Change your approach where needed; do not repeat it.\n"
        )
    } else {
        String::new()
    };
    let ids = concept_ids.join(", ");
    let focus_block = match focus {
        Some(f) => format!(
            "\n## Specific technique to train (the whole exercise must center on it)\n\
             {f}\n\
             The unfinished body must make the learner WRITE code that uses this exact \
             technique; a scenario that merely shares the topic but trains something \
             else is a failed draft.\n"
        ),
        None => String::new(),
    };
    format!(
        "You are writing ONE small Rust practice exercise for a learner who already codes \
         (Python/Java/Go/C++) but is confused by Rust's ownership/borrowing/lifetimes/traits.\n\
         {learner_block}\n\
         Topic / user request: {request}\n\
         {focus_block}\n\
         {skeleton}{shrink}{retry}\n\
         ## Hard requirements (a local gate will REJECT the draft otherwise)\n\
         1. Single root cause: the exercise's UNFINISHED body must fail to COMPILE with one \
         plausible beginner mistake, and the first rustc error must be one of the codes you \
         declare in error_codes. Do not use todo!() for the hole — leave real, plausible \
         learner code that triggers the error. Syntax errors, unused-import noise or several \
         unrelated errors at once all count as a failed draft. error_codes must be the code \
         the unfinished body ACTUALLY fails with — the gate verifies it against real rustc \
         output and a mislabel is rejected. Comments may only mention codes that appear in \
         error_codes (0909_2 反馈 P2: 注释里的错误码标签失真，用户按标签排查会扑空).\n\
         2. Real scenario: the body does one small, humanly describable task; the first comment \
         lines say what the code is trying to do and what is wrong (in 简体中文, like the \
         reference exercise).\n\
         3. Idiomatic fix: there is one clean idiomatic fix; the reference solution implements \
         it, passes ALL tests, and satisfies every constraint you declare. The gate COMPILES \
         the reference and statically checks it: under `no-clone` the reference must not call \
         .clone()/.to_owned() anywhere; under `iterator-only` it must contain no for/while \
         loop; declaring a constraint your own reference violates is an automatic rejection.\n\
         4. Tests: a #[cfg(test)] mod tests with 2+ tests asserting observable behavior; \
         they must FAIL on the unfinished body and PASS on the reference. Tests must not \
         contain meta-commentary (about the task, grading, or \"tests may need adjustment\") \
         and no empty or endless placeholder loops (0909_2 反馈 P9).\n\
         5. std-only, single file: no external crates, no unsafe, no std::process, no std::fs, \
         no file/network IO. body: 5–50 non-empty lines (instruction comments \
         count; tests are separate and uncounted); include the literal line \
         `// I AM NOT DONE` at the end of the body.\n\
         6. constraints: choose from [\"no-clone\", \"no-unwrap\", \"iterator-only\"]; \
         do NOT declare \"max-lines\" (a 50-line cap applies automatically).\n\
         7. concepts: 1–2 ids from EXACTLY this list: [{ids}]\n\
         8. difficulty: \"easy\" (apply one known fix) | \"medium\" (choose between 2–3 \
         plausible fixes, or non-obvious error) | \"hard\" (restructure).\n\
         9. Keep it COMPACT: body 8–45 lines (hard cap 50), tests ≤20 lines, \
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
    focus: Option<&str>,
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
            // Precision-sensitive request (M4.16 focus): when the learner
            // named a SPECIFIC technique, the domain's first candidate is
            // NOT good enough — run the LLM picker with the focus so a
            // same-domain-different-technique template is rejected
            // (no_match → tiers 2/3). Without focus, keep the cheap path.
            if let (Some(f), Some(call)) = (focus, llm) {
                if let Some(p) = llm_pick_template(templates, &topic.prompt_text(), Some(f), history, call, Some(&ids), None)? {
                    return Ok(p);
                }
                bail!("概念「{id}」下没有训练「{f}」的模板；转为改编/自由生成");
            }
            match pick_candidate(templates, &ids, None, history) {
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
            if let (Some(f), Some(call)) = (focus, llm) {
                if let Some(p) = llm_pick_template(templates, &topic.prompt_text(), Some(f), history, call, Some(&ids), None)? {
                    return Ok(p);
                }
                bail!("错误码 {code} 相关模板没有训练「{f}」的；转为改编/自由生成");
            }
            match pick_candidate(templates, &ids, None, history) {
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
            // Keyword pool first (cheap; also the drift anchor + the
            // specificity ranking for the LLM pick), then the LLM
            // (understands loose Chinese), then the deterministic pick
            // over the pool — highest score first.
            let pool = keyword_candidates(templates, graph, text);
            if let Some(call) = llm {
                // Empty pool → no anchor and FULL catalog (pool mode with
                // zero members would blank the catalog out).
                let pool_ref = if pool.ids.is_empty() { None } else { Some(&pool) };
                if let Some(p) = llm_pick_template(templates, text, focus, history, call, pool_ref.as_ref().map(|p| &p.ids), pool_ref)? {
                    return Ok(p);
                }
                if focus.is_some() {
                    // A focused free-text request the picker rejected:
                    // serving the keyword fallback would ignore the
                    // learner's specific technique.
                    bail!(
                        "没有训练「{}」的模板；转为改编/自由生成",
                        focus.unwrap_or_default()
                    );
                }
            }
            pick_candidate(templates, &pool.ids, Some(&pool.scores), history).ok_or_else(|| {
                if pool.ids.is_empty() {
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
/// `scores` (0909 反馈 A) ranks candidates by keyword-hit specificity
/// WITHIN each group (unused / used) — file order stays the final
/// tie-break. None = no ranking signal (concept/error-code pools).
fn pick_candidate<'a>(
    templates: &'a [template::Template],
    ids: &BTreeSet<String>,
    scores: Option<&BTreeMap<String, u32>>,
    history: &GenHistory,
) -> Option<Pick<'a>> {
    let mut cands: Vec<&template::Template> =
        templates.iter().filter(|t| ids.contains(&t.id)).collect();
    let score_of = |t: &template::Template| scores.and_then(|s| s.get(&t.id)).copied().unwrap_or(0);
    cands.sort_by(|a, b| {
        let ua = history.times(&a.id) != 0;
        let ub = history.times(&b.id) != 0;
        ua.cmp(&ub).then_with(|| score_of(b).cmp(&score_of(a)))
    });
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

/// Free-text keyword pool with evidence (0909 反馈 A+B): "HashMap 所有权"
/// used to drag the whole ownership subtree into an UNORDERED pool whose
/// file-order pick was closure-fn-kinds. Now every candidate carries a
/// specificity score — the needle-hit count of the most specific concept
/// node covering it — and the LLM picker gets the hit reasons.
struct KeywordPool {
    /// Candidate ids (set semantics; the anchor only checks membership).
    ids: BTreeSet<String>,
    /// template id → score = needle-hit count of the most specific hit
    /// node covering it (a node hitting BOTH keywords beats one hitting
    /// a single generic keyword, regardless of subtree size).
    scores: BTreeMap<String, u32>,
    /// (concept id, concept name, matched needles) — the prompt's
    /// keyword-analysis block.
    reasons: Vec<(String, String, Vec<String>)>,
}

/// Deterministic free-text matching: taxonomy concept names/ids first
/// (Chinese keywords live there), then template titles/ids/codes.
fn keyword_candidates(
    templates: &[template::Template],
    graph: &ConceptGraph,
    text: &str,
) -> KeywordPool {
    let needles: Vec<String> = text
        .split(|c: char| c.is_whitespace() || "，。、？！,.:?？()（）".contains(c))
        .map(str::trim)
        .filter(|w| w.chars().count() >= 2)
        .map(|w| w.to_lowercase())
        .collect();

    let mut ids = BTreeSet::new();
    let mut scores: BTreeMap<String, u32> = BTreeMap::new();
    let mut reasons: Vec<(String, String, Vec<String>)> = Vec::new();
    for node in graph.ids().filter_map(|id| graph.get(id)) {
        let hay = format!("{} {}", node.id, node.name).to_lowercase();
        let matched: Vec<String> =
            needles.iter().filter(|n| hay.contains(*n)).cloned().collect();
        if matched.is_empty() {
            continue;
        }
        let k = matched.len() as u32;
        reasons.push((node.id.clone(), node.name.clone(), matched));
        let mut covered = BTreeSet::new();
        collect_concept_templates(graph, &node.id, &mut covered);
        for t in covered {
            // Score = the MOST specific hit node covering the template.
            scores.entry(t.clone()).and_modify(|s| *s = (*s).max(k)).or_insert(k);
            ids.insert(t);
        }
    }
    if ids.is_empty() {
        // No concept matched: fall back to template title/id/error-code
        // matching (score 1 — no specificity signal beyond the hit).
        for t in templates {
            let hay = format!("{} {} {}", t.id, t.title, t.error_codes.join(" ")).to_lowercase();
            if needles.iter().any(|n| hay.contains(n)) {
                scores.insert(t.id.clone(), 1);
                ids.insert(t.id.clone());
            }
        }
    }
    KeywordPool { ids, scores, reasons }
}

// ---------------------------------------------------------------------------
// LLM helpers
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn llm_pick_template<'a>(
    templates: &'a [template::Template],
    request: &str,
    focus: Option<&str>,
    history: &GenHistory,
    call: &mut (dyn LlmCaller + '_),
    // 0908 反馈 [主题漂移]: "HashMap 所有权" got closure-fn-kinds — the
    // LLM picker ignored the request. When keyword matching has a
    // candidate pool, the pick must come from it; None = no anchor
    // (loose requests keep full LLM judgment).
    anchor: Option<&BTreeSet<String>>,
    // 0909 反馈 B: when the anchor comes from keyword matching, the
    // picker sees ONLY pool members (ranked by specificity) plus the
    // hit reasons — a bare 60-template catalog is how the request's
    // signal got drowned in the first place.
    pool: Option<&KeywordPool>,
) -> Result<Option<Pick<'a>>> {
    let catalog: String = match pool {
        Some(p) => {
            // Ranked: specificity score desc, then file order (stable).
            let mut ranked: Vec<&template::Template> =
                templates.iter().filter(|t| p.ids.contains(&t.id)).collect();
            ranked.sort_by(|a, b| {
                let sa = p.scores.get(&a.id).copied().unwrap_or(0);
                let sb = p.scores.get(&b.id).copied().unwrap_or(0);
                sb.cmp(&sa)
            });
            ranked
                .into_iter()
                .map(|t| {
                    let used_note = match history.get(&t.id) {
                        Some(u) => format!(" | ALREADY USED x{}", u.times),
                        None => String::new(),
                    };
                    let score = p.scores.get(&t.id).copied().unwrap_or(0);
                    format!(
                        "- {} | {} | {} | {} | keyword-score {score}{}",
                        t.id,
                        t.title,
                        t.concepts.join(","),
                        t.error_codes.join(","),
                        used_note
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
        None => templates
            .iter()
            .map(|t| {
                // M4.10: mark templates the learner already received so the
                // pick can prefer fresh ones.
                let used_note = match history.get(&t.id) {
                    Some(u) => format!(" | ALREADY USED x{} ({})",
                        u.times,
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
            .join("\n"),
    };
    // 0909 反馈 B: the keyword-analysis block — which concepts the
    // request's words actually hit, so the model can weigh specificity
    // instead of vibing its way through the catalog.
    let analysis = match pool {
        Some(p) if !p.reasons.is_empty() => {
            let lines = p
                .reasons
                .iter()
                .map(|(id, name, words)| format!("- \"{}\" hits concept {}（{}）", words.join("\", \""), id, name))
                .collect::<Vec<_>>()
                .join("\n");
            format!(
                "\n\nKeyword analysis of the request (hit concepts):\n{lines}\n\
                 A concept hit by MORE of the request's keywords is the more specific match — \
                 keyword-score in the catalog reflects this. Strongly prefer templates whose \
                 concepts top this ranking.\n"
            )
        }
        _ => String::new(),
    };
    let focus_block = match focus {
        Some(f) => format!(
            "\n\nSpecific technique the learner wants to train: {f}\n\
             Judge ONLY against this technique: a template that does not train \
             exactly it is a WRONG answer even if it shares the concept domain — \
             say no_match."
        ),
        None => String::new(),
    };
    // 0909_2 反馈 P1: concept-recency signal (cross-template AND
    // cross-layer repeats were not deduped before).
    let recent = history.recent_block();
    // 0909_2 反馈 P5: constructive requests ("实现X/写一个Y") want a
    // SPECIFIC artifact — an eager near-miss template produces a
    // scenario the user never asked for. Say no_match and let the free
    // tier build it.
    let constructive = if looks_constructive(request) {
        "\n\nThis looks like a CONSTRUCTIVE request (the user wants a specific artifact \
         built: 实现/写一个/做一个…). A template almost always serves a DIFFERENT scenario — \
         say no_match unless a template trains EXACTLY this artifact; the pipeline will \
         then generate one from scratch. Do NOT settle for a merely concept-adjacent \
         template.\n"
        .to_string()
    } else {
        String::new()
    };
    let usage_note = if pool.is_some() { " | keyword-score" } else { " | usage" };
    let prompt = format!(
        "You are choosing a Rust practice exercise template for a learner.\n\n\
         User request: {request}{focus_block}{analysis}{recent}{constructive}\n\n\
         Available templates (id | title | concepts | error-codes{usage_note}):\n{catalog}\n\n\
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
    Ok(id
        .and_then(|id| templates.iter().find(|t| t.id == id))
        .filter(|t| anchor.map(|a| a.contains(&t.id)).unwrap_or(true))
        .map(|t| Pick {
            t,
            used_llm: true,
            variant: history.times(&t.id) > 0,
        }))
}

/// Heuristic intent check (0909_2 反馈 P5): does the request ask to
/// BUILD something ("实现一个待办管理器" / "写一个 CSV 解析器")? Such
/// constructive requests are better served by the free tier than by an
/// eager near-miss template match.
fn looks_constructive(request: &str) -> bool {
    const MARKERS: [&str; 9] =
        ["实现", "写一个", "写个", "做一个", "弄一个", "搭一个", "implement", "create a", "build a"];
    let lower = request.to_lowercase();
    MARKERS.iter().any(|m| lower.contains(m))
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
//!
//! The mods here are intentionally NOT gated by `#[cfg(rust_analyzer)]`:
//! cargo (and thus rust-analyzer's flycheck on save) must see the
//! learner's unsolved exercises so borrow-checker errors (E0499/E0382/
//! E0502) — which rust-analyzer cannot produce on its own — show up
//! inline. The `exercises` member is not in `default-members`, so plain
//! `cargo build` / `cargo test` / `cargo run` on the repo root are
//! unaffected; only `cargo check`-ing this crate surfaces the errors.
";

/// Rebuild the IDE-only wiring file from the `category` directory
/// (self-healing): every `*.rs` under `exercises/<category>/` gets a
/// `#[cfg(rust_analyzer)] #[path] mod` block, sorted by module name.
/// Files deleted out-of-band drop out automatically — the old
/// append-only scheme accumulated stale entries whose missing files
/// showed up as permanent E0583 red in rust-analyzer (9.5 试用发现).
pub fn wire_lib_rs(wiring_rs: &Path, module: &str, category: &str) -> Result<()> {
    let dir = wiring_rs
        .parent()
        .with_context(|| format!("wiring 文件 {} 没有父目录", wiring_rs.display()))?
        .join(category);

    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .with_context(|| format!("读取 {} 失败", dir.display()))?
        .flatten()
        .filter(|e| e.path().extension().and_then(|e| e.to_str()) == Some("rs"))
        .filter_map(|e| e.path().file_stem().and_then(|s| s.to_str().map(str::to_string)))
        .collect();
    if !names.iter().any(|n| n == module) {
        names.push(module.to_string());
    }
    names.sort();
    names.dedup();

    let mut content = String::from(WIRING_HEADER);
    content.push('\n');
    for name in &names {
        // No `#[cfg(rust_analyzer)]` gate here (see WIRING_HEADER): the
        // whole point is for cargo check to see the unsolved exercises.
        content.push_str(&format!(
            "\n#[path = \"{category}/{name}.rs\"]\nmod {name};\n"
        ));
    }
    fs::write(wiring_rs, content)
        .with_context(|| format!("写入 {} 失败", wiring_rs.display()))
}

/// Ensure the wiring file exists (fresh clones have none — it's
/// gitignored). Called at REPL startup so an ungated
/// `mod lib_generated;` in exercises/lib.rs never resolves to a
/// missing file. Idempotent.
pub fn ensure_wiring_file(wiring_rs: &Path) {
    if !wiring_rs.exists()
        && let Err(e) = fs::write(wiring_rs, format!("{WIRING_HEADER}\n"))
    {
        eprintln!("  （接线文件创建失败：{e}）");
    }
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

    #[test]
    fn generated_difficulty_has_a_floor_for_composite_drafts() {        let mk = |concepts: usize, constraints: usize| template::ExerciseDraft {
            title: "t".into(),
            concepts: (0..concepts).map(|i| format!("c.{i}")).collect(),
            error_codes: vec![],
            difficulty: template::Difficulty::Easy,
            constraints: (0..constraints).map(|i| format!("ban-{i}")).collect(),
            body: "fn f() {}".into(),
            tests: String::new(),
            reference: String::new(),
        };
        let mut d = mk(2, 0);
        calibrate_difficulty(&mut d);
        assert_eq!(d.difficulty, template::Difficulty::Medium, "two concepts → medium");
        let mut d = mk(1, 2);
        calibrate_difficulty(&mut d);
        assert_eq!(d.difficulty, template::Difficulty::Medium, "two constraints → medium");
        let mut d = mk(1, 0);
        calibrate_difficulty(&mut d);
        assert_eq!(d.difficulty, template::Difficulty::Easy, "simple draft keeps easy");
        let mut d = mk(3, 3);
        d.difficulty = template::Difficulty::Hard;
        calibrate_difficulty(&mut d);
        assert_eq!(d.difficulty, template::Difficulty::Hard, "never downgrades");
        // Safety bans appended by ensure_ban_constraints are guardrails,
        // not difficulty contributors: 1 concept + only bans stays easy.
        let mut d = mk(1, 0);
        ensure_ban_constraints(&mut d);
        calibrate_difficulty(&mut d);
        assert_eq!(d.difficulty, template::Difficulty::Easy, "safety bans don't raise difficulty");
    }

    #[test]
    fn llm_draft_adopts_the_real_first_error_code() {
        let mk = |codes: &[&str]| template::ExerciseDraft {
            title: "t".into(),
            concepts: vec!["test.concept".into()],
            error_codes: codes.iter().map(|s| s.to_string()).collect(),
            difficulty: template::Difficulty::Easy,
            constraints: vec![],
            body: "fn f() {}".into(),
            tests: String::new(),
            reference: String::new(),
        };
        let report = |code: Option<&str>| crate::verifier::VerifyReport {
            compiles: true,
            ref_solution_passes: true,
            template_fails: true,
            first_error_code: code.map(str::to_string),
            template: None,
            reference: None,
        };
        // Declared E0382, body actually fails with E0308 → adopted.
        let mut d = mk(&["E0382"]);
        assert!(adopt_real_first_error(&mut d, &report(Some("E0308"))));
        assert_eq!(d.error_codes, vec!["E0308".to_string()], "reality wins");
        // Declared codes already contain the actual → untouched.
        let mut d = mk(&["E0382", "E0308"]);
        assert!(!adopt_real_first_error(&mut d, &report(Some("E0308"))));
        assert_eq!(d.error_codes, vec!["E0382".to_string(), "E0308".to_string()]);
        // Todo-type (compiles, tests fail) → untouched.
        let mut d = mk(&["E0382"]);
        assert!(!adopt_real_first_error(&mut d, &report(None)));
        assert_eq!(d.error_codes, vec!["E0382".to_string()]);
        // Empty declaration + compile failure → filled with the actual.
        let mut d = mk(&[]);
        assert!(adopt_real_first_error(&mut d, &report(Some("E0599"))));
        assert_eq!(d.error_codes, vec!["E0599".to_string()]);
    }

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
        let out = generate_with_mode(&Topic::ErrorCode("e0308".into()), None, GenerateMode::Auto, &fx.paths(), &GenHistory::default(), None, None, None).unwrap();
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

    // ---- focus (考察点精度, M4.16) ----

    #[test]
    fn draft_prompt_embeds_focus_block() {
        let p = draft_prompt("collections.hashmap", Some("entry API 的 or_insert 单次查找"), None, &["test.concept".into()], 1, "", None);
        assert!(p.contains("Specific technique to train"), "{p}");
        assert!(p.contains("entry API 的 or_insert 单次查找"), "{p}");
        let p2 = draft_prompt("test", None, None, &[], 1, "", None);
        assert!(!p2.contains("Specific technique to train"), "{p2}");
    }

    #[test]
    fn draft_prompt_embeds_learner_profile_block() {
        let learner = LearnerContext {
            weak: vec![("ownership.move".into(), 3, 5)],
            due: vec!["traits.assoc-types".into()],
            codes: vec![("E0382".into(), 4)],
            too_hard: vec!["collections.hashmap".into()],
            too_easy: vec![],
        };
        let p = draft_prompt("test", None, None, &[], 1, "", Some(&learner));
        assert!(p.contains("## Learner profile"), "{p}");
        assert!(p.contains("ownership.move (3 fails / 5 attempts)"), "{p}");
        assert!(p.contains("E0382 ×4"), "{p}");
        assert!(p.contains("TOO HARD"), "{p}");
        // No learner / empty profile → no block (offline parity).
        let p2 = draft_prompt("test", None, None, &[], 1, "", None);
        assert!(!p2.contains("## Learner profile"), "{p2}");
        let p3 = draft_prompt("test", None, None, &[], 1, "", Some(&LearnerContext::default()));
        assert!(!p3.contains("## Learner profile"), "{p3}");
    }

    #[test]
    fn focus_concept_branch_runs_the_precision_picker() {
        let fx = Fixture::new();
        let paths = fx.paths();
        // Picker sees the focus and rejects the only (domain-matching but
        // technique-mismatched) template → tier 1 must bail → L2 kicks in.
        let mut picker_calls = 0u32;
        let mut call = |prompt: &str| -> Result<LlmReply> {
            if prompt.contains("choosing a Rust practice") {
                picker_calls += 1;
                assert!(prompt.contains("Specific technique the learner wants to train"), "{prompt}");
                assert!(prompt.contains("or_insert 单次查找"), "{prompt}");
                Ok(reply(r#"{"no_match": true}"#))
            } else {
                Ok(reply(VALID_DRAFT_JSON))
            }
        };
        let out = generate_with_focus(
            &Topic::Concept("test.concept".into()),
            Some("entry API 的 or_insert 单次查找"),
            &paths,
            &GenHistory::default(),
        None, // learner
            Some(&mut call),
            None,
        )
        .unwrap();
        assert_eq!(picker_calls, 1, "focus must route through the picker");
        assert_eq!(out.tier, Tier::Adapted { base: "mini-add".into() });
    }

    #[test]
    fn no_focus_keeps_the_cheap_path() {
        let fx = Fixture::new();
        let paths = fx.paths();
        // Without focus the Concept branch must not call the picker at all.
        let mut picker_calls = 0u32;
        let mut call = |prompt: &str| -> Result<LlmReply> {
            if prompt.contains("choosing a Rust practice") {
                picker_calls += 1;
            }
            Ok(reply(VALID_DRAFT_JSON))
        };
        let out = generate(&Topic::Concept("test.concept".into()), &paths, Some(&mut call), None).unwrap();
        assert_eq!(out.tier, Tier::Matched { template_id: "mini-add".into() });
        assert_eq!(picker_calls, 0);
    }

    #[test]
    fn tier1_miss_with_focus_is_logged_for_batch_planning() {
        let fx = Fixture::new();
        let mut paths = fx.paths();
        let log_path = fx.root.join("miss_log.json");
        paths.miss_log = Some(log_path.clone());
        let mut call = |prompt: &str| -> Result<LlmReply> {
            if prompt.contains("choosing a Rust practice") {
                Ok(reply(r#"{"no_match": true}"#))
            } else {
                Ok(reply(VALID_DRAFT_JSON))
            }
        };
        generate_with_focus(
            &Topic::Concept("test.concept".into()),
            Some("一个题库没有的手法"),
            &paths,
            &GenHistory::default(),
        None, // learner
            Some(&mut call),
            None,
        )
        .unwrap();
        let log = fs::read_to_string(&log_path).unwrap();
        assert!(log.contains("template_no_match"), "{log}");
        assert!(log.contains("一个题库没有的手法"), "{log}");
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
            None,
            GenerateMode::Free,
            &paths,
            &GenHistory::default(),
        None, // learner
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
            None,
            GenerateMode::Adapted,
            &paths,
            &GenHistory::default(),
        None, // learner
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
            None,
            GenerateMode::Auto,
            &paths,
            &GenHistory::default(),
        None, // learner
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
            None,
            GenerateMode::Free,
            &paths,
            &GenHistory::default(),
        None, // learner
            Some(&mut call),
            None,
        )
        .unwrap();
        assert_eq!(out.concepts, vec!["test.concept".to_string()]);
    }

    /// M4.7: the draft loop must (a) request a token cap on the wire
    /// and (b) feed "length" truncations back as a compress-demand.
    /// Runs on the ADAPTED tier: free is single-round since 0907 反馈 P4.
    #[test]
    fn draft_loop_bounds_tokens_and_handles_truncation() {
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
            &Topic::FreeText("加法".into()),
            None,
            GenerateMode::Adapted,
            &paths,
            &GenHistory::default(),
        None, // learner
            Some(&mut call),
            None,
        )
        .unwrap();
        assert_eq!(out.tier, Tier::Adapted { base: "mini-add".into() });
        let caps = caps.borrow();
        assert!(caps.iter().all(|&c| c == DRAFT_MAX_TOKENS), "cap must be {DRAFT_MAX_TOKENS}, got {caps:?}");
        assert_eq!(caps.len(), 2, "truncation triggered exactly one retry");
    }

    #[test]
    fn repair_loop_feeds_gate_failures_back() {
        let fx = Fixture::new();
        let paths = fx.paths();
        // First round: a draft whose body compiles (gate rejects: no
        // compile failure → rule/gate failure). Second round: valid.
        // Runs on the ADAPTED tier: free is single-round since 0907 反馈 P4.
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
            &Topic::FreeText("加法".into()),
            None,
            GenerateMode::Adapted,
            &paths,
            &GenHistory::default(),
        None, // learner
            Some(&mut call),
            None,
        )
        .unwrap();
        assert_eq!(*calls.borrow(), 2, "repair loop retried once");
        assert_eq!(out.attempts, 2);
        assert_eq!(out.tier, Tier::Adapted { base: "mini-add".into() });
    }

    /// 0908 反馈 [主题漂移]: when keyword anchoring yields a candidate
    /// pool, an LLM pick OUTSIDE it must be rejected (fall back to the
    /// deterministic pick / no_match) instead of serving an off-topic
    /// template ("HashMap 所有权" → closure-fn-kinds).
    #[test]
    fn llm_template_pick_respects_the_keyword_anchor() {
        let fx = Fixture::new();
        let templates = template::load_dir(&fx.paths().templates_dir).unwrap();
        let mut call = |_prompt: &str| -> Result<LlmReply> {
            Ok(reply(r#"{"template_id": "mini-add"}"#))
        };
        // Without an anchor the pick is accepted as before.
        let p = llm_pick_template(&templates, "任意", None, &GenHistory::default(), &mut call, None, None)
            .unwrap();
        assert!(p.is_some());
        // Anchor containing the pick → accepted.
        let mut ok_anchor = BTreeSet::new();
        ok_anchor.insert("mini-add".to_string());
        let p = llm_pick_template(
            &templates,
            "任意",
            None,
            &GenHistory::default(),
            &mut call,
            Some(&ok_anchor),
            None,
        )
        .unwrap();
        assert!(p.is_some());
        // Anchor WITHOUT the pick → rejected as no_match.
        let mut other_anchor = BTreeSet::new();
        other_anchor.insert("some-other-template".to_string());
        let p = llm_pick_template(
            &templates,
            "HashMap 所有权",
            None,
            &GenHistory::default(),
            &mut call,
            Some(&other_anchor),
            None,
        )
        .unwrap();
        assert!(p.is_none(), "off-topic pick must be rejected");
    }

    /// 0909 反馈 A（真实库数据门）: "HashMap 所有权" 的池里
    /// hashmap-entry-count 必须以唯一最高分（双词命中 collections.hashmap）
    /// 排在整棵 ownership 子树的模板（单词命中，1 分）之前——
    /// 确定性落点从字母序的 closure-fn-kinds 变为 entry 模板。
    #[test]
    fn keyword_pool_ranks_by_specificity() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut graph =
            crate::taxonomy::ConceptGraph::load(&root.join("taxonomy/concepts.toml")).unwrap();
        let templates = template::load_dir(&root.join("templates")).unwrap();
        let items: Vec<(&str, &[String])> =
            templates.iter().map(|t| (t.id.as_str(), t.concepts.as_slice())).collect();
        graph.link_templates(items).unwrap();

        let pool = keyword_candidates(&templates, &graph, "HashMap 所有权");
        assert!(pool.ids.contains("hashmap-entry-count"), "pool: {:?}", pool.ids);
        assert!(pool.ids.contains("closure-fn-kinds"), "ownership subtree stays in the anchor pool");
        assert_eq!(pool.scores.get("hashmap-entry-count"), Some(&2), "double keyword hit");
        assert_eq!(pool.scores.get("closure-fn-kinds"), Some(&1), "single generic keyword");
        let pick = pick_candidate(&templates, &pool.ids, Some(&pool.scores), &GenHistory::default())
            .unwrap();
        assert_eq!(pick.t.id, "hashmap-entry-count", "deterministic pick follows the score");
    }

    /// 0909 反馈 B（真实库数据门）: anchored picker sees ONLY pool
    /// members (score-annotated, ranked) plus the keyword-analysis
    /// block — the bare 60-template catalog drowned the request.
    #[test]
    fn llm_pick_prompt_carries_keyword_analysis() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut graph =
            crate::taxonomy::ConceptGraph::load(&root.join("taxonomy/concepts.toml")).unwrap();
        let templates = template::load_dir(&root.join("templates")).unwrap();
        let items: Vec<(&str, &[String])> =
            templates.iter().map(|t| (t.id.as_str(), t.concepts.as_slice())).collect();
        graph.link_templates(items).unwrap();
        let pool = keyword_candidates(&templates, &graph, "HashMap 所有权");

        let prompts = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let rec = prompts.clone();
        let mut call = move |p: &str| -> Result<LlmReply> {
            rec.borrow_mut().push(p.to_string());
            Ok(reply(r#"{"template_id": "hashmap-entry-count"}"#))
        };
        let pick = llm_pick_template(
            &templates,
            "HashMap 所有权",
            None,
            &GenHistory::default(),
            &mut call,
            Some(&pool.ids),
            Some(&pool),
        )
        .unwrap()
        .unwrap();
        assert_eq!(pick.t.id, "hashmap-entry-count");

        let prompt = prompts.borrow()[0].clone();
        assert!(prompt.contains("Keyword analysis"), "{prompt}");
        assert!(prompt.contains("collections.hashmap（HashMap 与所有权）"), "{prompt}");
        assert!(prompt.contains("keyword-score 2"), "{prompt}");
        assert!(!prompt.contains("as-cast-truncation"), "pool-filtered catalog: {prompt}");
        let entry = prompt.find("hashmap-entry-count").unwrap();
        let closure = prompt.find("closure-fn-kinds").unwrap();
        assert!(entry < closure, "ranked catalog: entry (score 2) before closure (score 1)");
    }

    /// 0909_2 round4: the FREE tier rides the looser token cap —
    /// glm-52-low truncated at exactly 3000 twice and failed there.
    #[test]
    fn free_tier_gets_the_looser_token_cap() {
        use std::cell::Cell;
        use std::rc::Rc;
        let fx = Fixture::new();
        let paths = fx.paths();
        struct CapRecorder {
            cap: Rc<Cell<u32>>,
        }
        impl LlmCaller for CapRecorder {
            fn call(&mut self, _p: &str) -> Result<LlmReply> {
                unreachable!("the free draft loop must use call_bounded")
            }
            fn call_bounded(&mut self, _p: &str, max_tokens: u32) -> Result<LlmReply> {
                self.cap.set(max_tokens);
                Ok(reply(VALID_DRAFT_JSON))
            }
        }
        let cap = Rc::new(Cell::new(0));
        let mut call = CapRecorder { cap: cap.clone() };
        let out = generate_full(
            &Topic::FreeText("取余".into()),
            None,
            GenerateMode::Free,
            &paths,
            &GenHistory::default(),
            None, // learner
            Some(&mut call),
            None,
            "",
        )
        .unwrap();
        assert_eq!(out.tier, Tier::Free);
        assert_eq!(cap.get(), FREE_DRAFT_MAX_TOKENS, "free tier must ride the looser cap");
    }

    /// 0909_2 反馈 P1: recent_concepts collects the LAST exercises'
    /// concepts across ALL tiers (newest first, deduped) — cross-template
    /// and cross-layer same-concept repeats were invisible before.
    #[test]
    fn gen_history_tracks_recent_concepts_across_tiers() {
        use crate::exercise::index::{ExerciseIndex, ExerciseMeta, Source, Status};
        let mut idx = ExerciseIndex::load_from(std::env::temp_dir().join("rs_gen_recent_nonexistent"));
        let mk = |path: &str, concepts: &[&str], source: Source, created: Option<&str>| ExerciseMeta {
            path: path.into(),
            title: path.into(),
            concepts: concepts.iter().map(|s| s.to_string()).collect(),
            error_codes: vec![],
            difficulty: None,
            source,
            session_id: None,
            trigger: None,
            created_at: created.map(str::to_string),
            attempts: 0,
            status: Status::Pending,
            last_error: None,
            last_fail_error: None,
            hints: Vec::new(),
            feedback: None,
            slots: Default::default(),
            reference: None,
            constraints: Vec::new(),
            review_verdict: None,
        };
        idx.upsert(mk("generated/old.rs", &["ownership.move"], Source::Free, Some("2026-09-09T01:00:00Z")));
        idx.upsert(mk(
            "generated/new.rs",
            &["collections.hashmap", "borrowing.shared-mut"],
            Source::TemplateFill { template_id: "hashmap-entry-count".into() },
            Some("2026-09-09T02:00:00Z"),
        ));
        idx.upsert(mk("generated/nodate.rs", &["traits.basics"], Source::Free, None));
        let h = GenHistory::from_index(&idx);
        // Newest first (new.rs 02:00 before old.rs 01:00), deduped,
        // undated entries skipped.
        assert_eq!(
            h.recent_block(),
            "\n\nRecently trained concepts (the learner's LAST exercises, newest first): \
             collections.hashmap, borrowing.shared-mut, ownership.move.\n\
             Do NOT re-serve these same concepts again unless the user EXPLICITLY asks for \
             that exact topic — cross-template same-concept repeats waste practice time. \
             Prefer an adjacent-but-different concept, or say no_match (the free tier will \
             build a fresh scenario).\n"
        );
    }

    /// 0909_2 反馈 P5 + P1: constructive requests and recently trained
    /// concepts both surface in the pick prompt.
    #[test]
    fn pick_prompt_carries_recent_and_constructive_signals() {
        let fx = Fixture::new();
        let paths = fx.paths();
        let mut graph = crate::taxonomy::ConceptGraph::load(&paths.taxonomy_file).unwrap();
        let templates = template::load_dir(&paths.templates_dir).unwrap();
        let items: Vec<(&str, &[String])> =
            templates.iter().map(|t| (t.id.as_str(), t.concepts.as_slice())).collect();
        graph.link_templates(items).unwrap();
        // history WITH a recent-concept signal: an index whose latest
        // entry trained t.c.
        let mut idx = crate::exercise::index::ExerciseIndex::load_from(std::env::temp_dir().join("rs_gen_recent_fx"));
        idx.upsert(crate::exercise::index::ExerciseMeta {
            path: "generated/x.rs".into(),
            title: "x".into(),
            concepts: vec!["t.c".into()],
            error_codes: vec![],
            difficulty: None,
            source: crate::exercise::index::Source::Free,
            session_id: None,
            trigger: None,
            created_at: Some("2026-09-09T02:00:00Z".into()),
            attempts: 0,
            status: crate::exercise::index::Status::Pending,
            last_error: None,
            last_fail_error: None,
            hints: Vec::new(),
            feedback: None,
            slots: Default::default(),
            reference: None,
            constraints: Vec::new(),
            review_verdict: None,
        });
        let h = GenHistory::from_index(&idx);

        let prompts = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let rec = prompts.clone();
        let mut call = move |p: &str| -> Result<LlmReply> {
            rec.borrow_mut().push(p.to_string());
            Ok(reply(r#"{"template_id": "mini-add"}"#))
        };
        llm_pick_template(
            &templates,
            "写一个交通灯状态机",
            None,
            &h,
            &mut call,
            None,
            None,
        )
        .unwrap();
        let prompt = prompts.borrow()[0].clone();
        assert!(prompt.contains("Recently trained concepts"), "{prompt}");
        assert!(prompt.contains("t.c"), "{prompt}");
        assert!(prompt.contains("CONSTRUCTIVE request"), "{prompt}");
    }

    /// 0909_2 反馈 P2: when the gate's first-error adoption REPLACES the
    /// declared codes and the body/comments still mention the replaced
    /// code, ONE best-effort sync round aligns the labels; the synced
    /// draft must re-pass the gate. (Free tier: max_rounds=1, the sync
    /// is a bonus round.)
    #[test]
    fn adopted_label_mismatch_gets_a_sync_round() {
        let fx = Fixture::new();
        let paths = fx.paths();
        // Round 1: declares E0499 while the body actually fails E0308,
        // and the comment mentions E0499 — gate adopts E0308, comment
        // goes stale.
        let wrong = VALID_DRAFT_JSON
            .replace(r#""error_codes": ["E0308"]"#, r#""error_codes": ["E0499"]"#)
            .replace("// 计算 a 除以 b 的余数。", "// 这里会报 E0499。\\n// 计算 a 除以 b 的余数。");
        // Sync round: aligned labels, comment mentions the REAL code.
        let fixed = VALID_DRAFT_JSON
            .replace("// 计算 a 除以 b 的余数。", "// 这里会报 E0308。\\n// 计算 a 除以 b 的余数。");
        let replies = std::cell::RefCell::new(vec![wrong.clone(), fixed.clone()]);
        let prompts = std::rc::Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
        let rec = prompts.clone();
        let mut call = move |prompt: &str| -> Result<LlmReply> {
            rec.borrow_mut().push(prompt.to_string());
            Ok(reply(&replies.borrow_mut().remove(0)))
        };
        let out = generate_full(
            &Topic::FreeText("取余".into()),
            None,
            GenerateMode::Free,
            &paths,
            &GenHistory::default(),
            None, // learner
            Some(&mut call),
            None,
            "",
        )
        .unwrap();
        let ps = prompts.borrow();
        assert_eq!(ps.len(), 2, "sync round must fire exactly once: {:?}", ps.len());
        assert!(ps[1].contains("已被按实际改判"), "sync prompt carries the mismatch: {}", ps[1]);
        assert_eq!(out.error_codes, vec!["E0308".to_string()]);
        let src = fs::read_to_string(&out.path).unwrap();
        assert!(!src.contains("E0499"), "stale label replaced: {src}");
    }

    /// 0907 反馈 P4: free generation is ONE round per call — a rejected
    /// draft must not silently burn more rounds; the preset failure
    /// (previous call's rejection) reaches the round-1 prompt.
    #[test]
    fn free_generation_is_single_round_with_preset_feedback() {
        let fx = Fixture::new();
        let paths = fx.paths();
        // A draft whose body compiles → the gate rejects it every time.
        let bad = VALID_DRAFT_JSON.replace(r"let q: String = a;\n    q", "a % b");
        let calls = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let rec = calls.clone();
        let mut call = move |prompt: &str| -> Result<LlmReply> {
            rec.borrow_mut().push(prompt.to_string());
            Ok(reply(&bad))
        };
        let err = generate_full(
            &Topic::FreeText("取余".into()),
            None,
            GenerateMode::Free,
            &paths,
            &GenHistory::default(),
            None, // learner
            Some(&mut call),
            None,
            "上一轮：题目没有区分度（unfinished body 能编译）",
        )
        .unwrap_err();
        let prompts = calls.borrow();
        assert_eq!(prompts.len(), 1, "free tier must run exactly ONE round, got {}", prompts.len());
        assert!(
            prompts[0].contains("previous attempt FAILED"),
            "preset failure must reach the round-1 prompt"
        );
        assert!(prompts[0].contains("题目没有区分度"), "preset reason text must be carried");
        assert!(err.to_string().contains("1 轮"), "{err}");
    }

    #[test]
    fn tier3_requires_llm() {
        let fx = Fixture::new();
        let err = generate_with_mode(
            &Topic::FreeText("取余".into()),
            None,
            GenerateMode::Free,
            &fx.paths(),
            &GenHistory::default(),
        None, // learner
            None,
            None,
        )
        .unwrap_err();
        assert!(err.to_string().contains("LLM"), "{err}");
    }

    #[test]
    fn wire_lib_rs_is_idempotent_and_prunes_dead_entries() {
        let fx = Fixture::new();
        let paths = fx.paths();
        let gen_dir = paths.wiring_rs.parent().unwrap().join(OUT_CATEGORY);
        fs::create_dir_all(&gen_dir).unwrap();
        fs::write(gen_dir.join("live.rs"), "// 练习\n").unwrap();

        // Pre-existing wiring with a STALE entry (file deleted out-of-band)
        // must be pruned on rebuild; the live module must survive.
        fs::write(
            &paths.wiring_rs,
            concat!(
                "//! old header\n",
                "\n#[cfg(rust_analyzer)]\n#[path = \"generated/dead.rs\"]\nmod dead;\n"
            ),
        )
        .unwrap();

        wire_lib_rs(&paths.wiring_rs, "live", OUT_CATEGORY).unwrap();
        let content = fs::read_to_string(&paths.wiring_rs).unwrap();
        assert!(content.contains("mod live;"), "{content}");
        assert!(!content.contains("mod dead;"), "stale entry pruned: {content}");
        assert!(content.contains("Auto-generated exercise wiring"), "header kept: {content}");
        // Generated exercises are UNGATED so cargo check / flycheck can
        // surface borrow-checker errors to the editor (9.5).
        assert!(!content.contains("
#[cfg(rust_analyzer)]"), "ungated mods: {content}");

        // Idempotent: rewriting changes nothing (module list is sorted).
        wire_lib_rs(&paths.wiring_rs, "live", OUT_CATEGORY).unwrap();
        assert_eq!(fs::read_to_string(&paths.wiring_rs).unwrap(), content);
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
            None, // learner
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
            None, // learner
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
            None, // learner
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
            None, // learner
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

    /// Live experiment (M4.12, 9.4 晚): does reasoning effort balance
    /// draft quality against time/cost? The Rc<RefCell<T>> topic failed
    /// 4/4 rounds with thinking off (user session 20260904_202243).
    /// Matrix: off (baseline repro) vs on+low; on+high already has two
    /// successful smoke data points (2 rounds/6m10s and 1 round/2m40s).
    /// Prints metrics only. Run:
    /// cargo test live_draft_effort -- --ignored --nocapture
    #[test]
    #[ignore]
    fn live_draft_effort() {
        use std::time::Instant;
        let cfg = crate::config::ModelConfig::load().expect("config");
        if cfg.api_key.trim().is_empty() {
            eprintln!("no key configured; skipping");
            return;
        }
        let timeout =
            std::time::Duration::from_secs(cfg.llm_timeout_secs.unwrap_or(480));
        let paths = Paths::from_root(Path::new("."));
        let topic = Topic::FreeText("Rc<RefCell<T>> 共享可变状态与运行时借用".into());

        for (name, thinking, effort) in [
            ("off", Some(crate::llm::Thinking::Disabled), None),
            ("on+low", Some(crate::llm::Thinking::Enabled), Some("low".to_string())),
        ] {
            for run in 1..=2 {
                let client = crate::llm::LlmClient::with_timeout(
                    &cfg.endpoint, &cfg.api_key, &cfg.model, timeout,
                )
                .with_thinking(thinking)
                .with_reasoning_effort(effort.clone());
                let log: std::rc::Rc<std::cell::RefCell<Vec<(u64, u64, u64)>>> =
                    Default::default();
                let log2 = log.clone();
                let client2 = client.clone();
                let mut call = move |prompt: &str| -> Result<LlmReply> {
                    let out = client2.chat_turn_bounded(
                        &[crate::llm::ChatMessage::user(prompt.to_string())],
                        &[],
                        Some(FREE_DRAFT_MAX_TOKENS),
                    )?;
                    log2.borrow_mut().push((
                        out.usage.prompt_tokens,
                        out.usage.completion_tokens,
                        out.usage.reasoning_tokens,
                    ));
                    Ok(LlmReply {
                        content: out.content.unwrap_or_default(),
                        usage: out.usage,
                        finish_reason: out.finish_reason,
                    })
                };
                let t0 = Instant::now();
                let result = generate_with_mode(
                    &topic,
                    None,
                    GenerateMode::Free,
                    &paths,
                    &GenHistory::default(),
        None, // learner
                    Some(&mut call),
                    None,
                );
                let dt = t0.elapsed();
                let calls = log.borrow();
                let total_out: u64 = calls.iter().map(|c| c.1).sum();
                let total_reason: u64 = calls.iter().map(|c| c.2).sum();
                match result {
                    Ok(o) => println!(
                        "== {name} run{run}: PASS attempts={} {dt:.1?} out={total_out} (reason={total_reason}) tok, calls={}",
                        o.attempts, calls.len()
                    ),
                    Err(e) => println!(
                        "== {name} run{run}: FAIL {dt:.1?} out={total_out} (reason={total_reason}) tok, calls={} — {}",
                        calls.len(),
                        e.to_string().lines().next().unwrap_or("")
                    ),
                }
            }
        }
    }
}
