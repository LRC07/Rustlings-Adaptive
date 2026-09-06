//! Exercise index (M4.5a): metadata & solve state for every practice
//! exercise, persisted as `exercises/generated/index.json` — the single
//! source of truth that replaces the old `.progress` file (M0).
//!
//! Design (docs/出题规划_M4.5.md §3.1):
//! - Keys are exercise paths relative to `exercises/` ("generated/x.rs"),
//!   so the index stays stable regardless of the CWD.
//! - The file system only stores code; everything the UI needs (title,
//!   concepts, difficulty, source, trigger, status, feedback) lives here.
//! - `reconcile` adds a Pending entry for any exercise found on disk
//!   that is not yet tracked, recovering provenance from the matching
//!   template when the file name is `<sanitized-template-id>[_N]`.
//! - Mutation methods persist immediately; save failures degrade to a
//!   stderr warning (never crash the REPL).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

use super::Exercise;
use crate::template::Template;

/// Location of the index file, derived from the exercises directory.
pub fn index_path(exercises_dir: &Path) -> PathBuf {
    exercises_dir.join("generated").join("index.json")
}

/// Canonical index key for an exercise file: its path relative to
/// `exercises_dir`, '/'-separated. Tolerates the generator's
/// "./"-prefixed paths by canonicalizing both sides first.
pub fn key_for(exercises_dir: &Path, ex_path: &Path) -> Option<String> {
    let base = exercises_dir.canonicalize().ok()?;
    let full = ex_path.canonicalize().ok()?;
    let rel = full.strip_prefix(&base).ok()?;
    Some(rel.to_string_lossy().replace('\\', "/"))
}

/// Shared status transition of a recorded run/attempt: a pass marks
/// the exercise Passed (sticky) and clears last_error; a failure only
/// counts up on non-Passed entries. `counts_failure=false` (a
/// verification run of UNCHANGED code) confirms an existing failure
/// without creating a new one — the ✗N marker must not grow either
/// (9.5 回归实测：h/r 重跑两次后 [✗3]).
fn apply_result(meta: &mut ExerciseMeta, passed: bool, first_error: Option<&str>, counts_failure: bool) {
    if passed {
        meta.status = Status::Passed;
        meta.last_error = None;
        return;
    }
    if meta.status == Status::Passed {
        return; // never demote
    }
    if let Some(code) = first_error {
        meta.last_error = Some(code.to_string());
    }
    match meta.status {
        Status::Failed { times } if counts_failure => {
            meta.status = Status::Failed { times: times + 1 };
        }
        Status::Failed { times } => meta.status = Status::Failed { times },
        // M9n 实测：进题自动首跑/批量验证的失败不是学习者的失败——
        // Pending 不因验证性运行转成 Failed（否则卡片显示
        // "✗1 次失败（0 次尝试）"）。真正的尝试才会建立失败计数。
        _ if counts_failure => meta.status = Status::Failed { times: 1 },
        _ => {}
    }
}

/// Where a tracked exercise came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Source {
    /// Seed fixture shipped with the repo (`exercises/fixtures/`).
    Seed,
    /// Level-1 generation: a template instantiated with slots.
    TemplateFill { template_id: String },
    /// Level-2 generation: adapted from a base template (M4.5c).
    Adapted { base: String },
    /// Level-3 generation: free-form (M4.5c).
    Free,
    /// Reconciled from disk without recoverable provenance.
    Unknown,
}

impl Source {
    /// Short human label for the exercise card / lists.
    pub fn label_cn(&self) -> String {
        match self {
            Source::Seed => "种子题".to_string(),
            Source::TemplateFill { template_id } => format!("模板 {template_id}"),
            Source::Adapted { base } => format!("改编自 {base}"),
            Source::Free => "自由生成".to_string(),
            Source::Unknown => "未知".to_string(),
        }
    }
}

/// Solve state of one exercise.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    #[default]
    Pending,
    Failed { times: u32 },
    Passed,
    Skipped,
}

/// One-shot user quality signal (§7.4 gate 4 data起点).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Feedback {
    TooEasy,
    JustRight,
    TooHard,
    Pointless,
}

impl Feedback {
    pub fn label_cn(&self) -> &'static str {
        match self {
            Feedback::TooEasy => "太简单",
            Feedback::JustRight => "合适",
            Feedback::TooHard => "太难",
            Feedback::Pointless => "没意义",
        }
    }

    pub fn from_choice(s: &str) -> Option<Self> {
        match s {
            "1" => Some(Feedback::TooEasy),
            "2" => Some(Feedback::JustRight),
            "3" => Some(Feedback::TooHard),
            "4" => Some(Feedback::Pointless),
            _ => None,
        }
    }
}

/// Metadata + state of one exercise.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExerciseMeta {
    /// Index key: path relative to `exercises/` ("generated/x.rs").
    pub path: String,
    pub title: String,
    #[serde(default)]
    pub concepts: Vec<String>,
    #[serde(default)]
    pub error_codes: Vec<String>,
    /// "easy" | "medium" | "hard"; None when unknown (seeds).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub difficulty: Option<String>,
    pub source: Source,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// One-line reason this exercise was produced ("你贴的代码报 E0382").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(default)]
    pub attempts: u32,
    #[serde(default)]
    pub status: Status,
    /// Last compile error code seen while solving ("E0382"); cleared on pass.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// Tiered static hints revealed one per `[h]` (M4.8); empty for
    /// free-form exercises without hints.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hints: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feedback: Option<Feedback>,
    /// Actual slot values this exercise was instantiated with (tier 1
    /// only; M4.10). Feeds the variant machinery: a repeated template
    /// must not re-serve the same fill.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub slots: std::collections::BTreeMap<String, String>,
    /// Hidden reference solution (M5.1): persisted at generation time
    /// so the review gate / debrief can compare against it later.
    /// Reconciled entries carry a best-effort backfill rendered from
    /// the template with the recorded (or default) slot values.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    /// Constraint spec strings of the exercise (M5.1): the review
    /// gate's static layer and the debrief comparison table consume
    /// these; empty when unknown (seed fixtures, pre-M5.1 entries).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub constraints: Vec<String>,
    /// Review-gate verdict of the passing run (M5.2):
    /// "clean" | "suggestions" | "suspicious"; None = not reviewed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_verdict: Option<String>,
}

impl ExerciseMeta {
    /// Compact state marker for list rows ("", "✗2", "✓").
    pub fn status_mark(&self) -> String {
        match &self.status {
            Status::Pending | Status::Skipped => " ".to_string(),
            Status::Failed { times } => format!("✗{times}"),
            Status::Passed => "✓".to_string(),
        }
    }

    /// Human one-liner for the exercise card.
    pub fn status_line_cn(&self) -> String {
        match &self.status {
            Status::Pending => "未做".to_string(),
            Status::Skipped => "已跳过".to_string(),
            Status::Failed { times } => format!("失败 {times} 次"),
            Status::Passed => "已通过".to_string(),
        }
    }
}

/// The in-memory index; persisted at `index_path`.
#[derive(Debug)]
pub struct ExerciseIndex {
    path: PathBuf,
    entries: BTreeMap<String, ExerciseMeta>,
}

impl ExerciseIndex {
    /// Load from the default location (empty when absent or corrupt —
    /// reconcile will rebuild entries from disk).
    pub fn load(exercises_dir: &Path) -> Self {
        Self::load_from(index_path(exercises_dir))
    }

    /// Injectable variant used by tests (and handy for future stores).
    pub fn load_from(path: PathBuf) -> Self {
        let entries = fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        Self { path, entries }
    }

    fn save(&self) {
        if let Some(parent) = self.path.parent()
            && let Err(e) = fs::create_dir_all(parent)
        {
            eprintln!("  （index 目录创建失败：{e}）");
            return;
        }
        match serde_json::to_string_pretty(&self.entries) {
            Ok(json) => {
                if let Err(e) = fs::write(&self.path, json) {
                    eprintln!("  （index 写入失败：{e}）");
                }
            }
            Err(e) => eprintln!("  （index 序列化失败：{e}）"),
        }
    }

    pub fn get(&self, key: &str) -> Option<&ExerciseMeta> {
        self.entries.get(key)
    }

    /// All entries in key order (used by the generator's history view).
    pub fn iter(&self) -> impl Iterator<Item = &ExerciseMeta> {
        self.entries.values()
    }

    /// Insert or replace one entry and persist.
    pub fn upsert(&mut self, meta: ExerciseMeta) {
        self.entries.insert(meta.path.clone(), meta);
        self.save();
    }

    /// Record one solve attempt: bumps `attempts`, transitions the
    /// status and remembers the first compile error code (cleared on a
    /// pass). Unknown keys are ignored (not a tracked exercise); an
    /// already-Passed entry is never demoted.
    pub fn record_attempt(&mut self, key: &str, passed: bool, first_error: Option<&str>) {
        let Some(meta) = self.entries.get_mut(key) else { return };
        meta.attempts += 1;
        apply_result(meta, passed, first_error, true);
        self.save();
    }

    /// Record a verification run of UNCHANGED code: status and
    /// last_error transition exactly like an attempt, but `attempts`
    /// is not bumped — re-running the same code (menu round-trips,
    /// hint/feedback pages, re-checks) must not inflate the
    /// attempt/failure counts that feed the profile and the debrief
    /// follow-up decision (9.5 实测反馈：一次通过的题显示"失败 7 次").
    pub fn record_run(&mut self, key: &str, passed: bool, first_error: Option<&str>) {
        let Some(meta) = self.entries.get_mut(key) else { return };
        apply_result(meta, passed, first_error, false);
        self.save();
    }

    /// Set the quality feedback for `key`; returns false when unknown.
    pub fn set_feedback(&mut self, key: &str, f: Feedback) -> bool {        match self.entries.get_mut(key) {
            Some(meta) => {
                meta.feedback = Some(f);
                self.save();
                true
            }
            None => false,
        }
    }

    /// Persist the review-gate verdict of a passed exercise (M5.2).
    /// Returns false when the key is unknown.
    pub fn set_review_verdict(&mut self, key: &str, verdict: &str) -> bool {
        match self.entries.get_mut(key) {
            Some(meta) => {
                meta.review_verdict = Some(verdict.to_string());
                self.save();
                true
            }
            None => false,
        }
    }

    /// Reconcile with the exercises found on disk: adds a Pending entry
    /// for every untracked exercise, recovering provenance from the
    /// matching template (file name `<sanitized-id>[_N]`). Returns the
    /// number of entries added. Entries whose file disappeared are kept
    /// (the UI simply never shows them).
    pub fn reconcile(&mut self, exercises: &[Exercise], templates: &[Template]) -> usize {
        let mut added = 0;
        for ex in exercises {
            let key = key_for_ex(ex);
            if self.entries.contains_key(&key) {
                continue;
            }
            let (source, concepts, error_codes, difficulty, hints, reference, constraints) =
                provenance_of(ex, templates);
            self.entries.insert(
                key.clone(),
                ExerciseMeta {
                    path: key,
                    title: ex.title.clone(),
                    concepts,
                    error_codes,
                    difficulty,
                    source,
                    session_id: None,
                    trigger: None,
                    created_at: None,
                    attempts: 0,
                    status: Status::Pending,
                    last_error: None,
                    hints,
                    feedback: None,
                    slots: Default::default(),
                    reference,
                    constraints,
                    review_verdict: None,
                },
            );
            added += 1;
        }
        if added > 0 {
            self.save();
        }
        added
    }

    /// Build the `[Practice status]` note injected into the agent system
    /// prompt (M4.5a state back-flow). None when there is nothing to
    /// report. English to match the system prompt's language.
    pub fn practice_note(&self, session_paths: &[String]) -> Option<String> {
        let tracked: Vec<&ExerciseMeta> = self.entries.values().collect();
        if tracked.is_empty() {
            return None;
        }

        let mut note = String::from("[Practice status]");

        // This session's exercises, in production order.
        let session: Vec<&ExerciseMeta> = session_paths
            .iter()
            .filter_map(|p| self.entries.get(p))
            .collect();
        if !session.is_empty() {
            note.push_str(" Session exercises:");
            for meta in session.iter().take(5) {
                let st = match &meta.status {
                    Status::Pending => "pending".into(),
                    Status::Skipped => "skipped".into(),
                    Status::Failed { times } => format!("failed {times}x"),
                    Status::Passed => "passed".into(),
                };
                note.push_str(&format!(" \"{}\" ({st});", meta.title));
            }
            if session.len() > 5 {
                note.push_str(&format!(" …and {} more;", session.len() - 5));
            }
        }

        // Library progress (seeds are fixtures, not learner exercises).
        let non_seed: Vec<&ExerciseMeta> = tracked.iter().copied().filter(|m| m.source != Source::Seed).collect();
        if !non_seed.is_empty() {
            let passed = non_seed.iter().filter(|m| m.status == Status::Passed).count();
            note.push_str(&format!(" Library: {passed}/{} passed.", non_seed.len()));
        }

        // Weakest concepts by failure count (rough signal until the M6
        // profile lands).
        let mut fails: BTreeMap<&str, u32> = BTreeMap::new();
        for m in &tracked {
            if let Status::Failed { times } = m.status {
                for c in &m.concepts {
                    *fails.entry(c.as_str()).or_default() += times;
                }
            }
        }
        let mut weak: Vec<(&str, u32)> = fails.into_iter().collect();
        weak.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        if !weak.is_empty() {
            let list: Vec<String> = weak.iter().take(3).map(|(c, n)| format!("{c} ({n} fails)")).collect();
            note.push_str(&format!(" Struggling with: {}.", list.join(", ")));
        }

        (note.len() > "[Practice status]".len()).then_some(note)
    }
}

/// Key of an already-discovered exercise: the relative path computed at
/// discovery time (no filesystem access needed).
fn key_for_ex(ex: &Exercise) -> String {
    ex.rel_path.clone()
}

/// Strip a trailing `_<digits>` counter suffix that the generator adds
/// for duplicate file names (`own_closure_capture_3` → base stem).
fn strip_counter_suffix(stem: &str) -> &str {
    match stem.rsplit_once('_') {
        Some((base, n)) if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) => base,
        _ => stem,
    }
}

/// Recover provenance for a discovered exercise. Seeds map to
/// `Source::Seed`; generated files map back to their template via the
/// sanitized module name (optionally with a `_N` counter suffix).
/// Template matches also backfill the reference solution (M5.1) by
/// rendering the template with the default slot fill — a best-effort
/// baseline for entries registered before the reference was persisted;
/// the exact (slot-recorded) reference is stored by
/// `register_generated` at generation time.
#[allow(clippy::type_complexity)]
fn provenance_of(
    ex: &Exercise,
    templates: &[Template],
) -> (Source, Vec<String>, Vec<String>, Option<String>, Vec<String>, Option<String>, Vec<String>) {
    if ex.is_fixture {
        return (Source::Seed, Vec::new(), Vec::new(), None, Vec::new(), None, Vec::new());
    }
    let stem = ex.name.as_str();
    let base = strip_counter_suffix(stem);
    for t in templates {
        let san = crate::generator::sanitize_module_name(&t.id);
        if stem == san || base == san {
            let reference = crate::template::render(t, &crate::template::fill_for_attempt(t, 0))
                .ok()
                .map(|r| r.reference.trim_end().to_string());
            return (
                Source::TemplateFill { template_id: t.id.clone() },
                t.concepts.clone(),
                t.error_codes.clone(),
                Some(t.difficulty.as_str().to_string()),
                t.hints.clone(),
                reference,
                t.constraints.clone(),
            );
        }
    }
    (Source::Unknown, Vec::new(), Vec::new(), None, Vec::new(), None, Vec::new())
}

/// One-shot migration of the legacy `.progress` file (M0): newline
/// separated paths of done exercises. Each path is canonicalized and
/// matched against the discovered exercises; matches are recorded as
/// Passed (with at least one attempt). The file is deleted afterwards.
/// Returns the number of migrated entries.
pub fn migrate_progress(
    index: &mut ExerciseIndex,
    exercises_dir: &Path,
    exercises: &[Exercise],
) -> usize {
    let progress_path = exercises_dir.join(".progress");
    let Ok(text) = fs::read_to_string(&progress_path) else { return 0 };
    let mut migrated = 0;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let p = Path::new(line);
        let Some(key) = key_for(exercises_dir, p) else { continue };
        if exercises.iter().any(|ex| key_for_ex(ex) == key) {
            let already = index
                .get(&key)
                .map(|m| m.status == Status::Passed)
                .unwrap_or(false);
            if !already
                && let Some(meta) = index.entries.get_mut(&key)
            {
                meta.status = Status::Passed;
                meta.attempts = meta.attempts.max(1);
                migrated += 1;
            }
        }
    }
    if migrated > 0 {
        index.save();
    }
    // Migration is one-shot: the file is consumed even when nothing
    // matched (the exercises it referenced are gone).
    let _ = fs::remove_file(&progress_path);
    migrated
}

// ---------------------------------------------------------------------------
// Registration entry point (CLI `/generate` + agent `generate_exercise`)
// ---------------------------------------------------------------------------

/// Register a freshly generated exercise: derive its index key from the
/// generator's output path, build the meta entry and persist it. Returns
/// the key (also what `Session.exercises` stores). `reference` is the
/// hidden solution and `constraints` the exercise's spec strings —
/// both consumed later by the M5 review gate / debrief.
#[allow(clippy::too_many_arguments)]
pub fn register_generated(
    exercises_dir: &Path,
    gen_path: &Path,
    title: &str,
    concepts: &[String],
    error_codes: &[String],
    difficulty: Option<&str>,
    source: Source,
    session_id: Option<&str>,
    trigger: Option<&str>,
    hints: &[String],
    slots: &std::collections::BTreeMap<String, String>,
    reference: &str,
    constraints: &[String],
) -> anyhow::Result<String> {
    let key = key_for(exercises_dir, gen_path)
        .context("无法定位生成的练习文件（路径解析失败）")?;
    let mut index = ExerciseIndex::load(exercises_dir);
    index.upsert(ExerciseMeta {
        path: key.clone(),
        title: title.to_string(),
        concepts: concepts.to_vec(),
        error_codes: error_codes.to_vec(),
        difficulty: difficulty.map(str::to_string),
        source,
        session_id: session_id.map(str::to_string),
        trigger: trigger.map(str::to_string),
        created_at: Some(chrono::Utc::now().to_rfc3339()),
        attempts: 0,
        status: Status::Pending,
        last_error: None,
        hints: hints.to_vec(),
        feedback: None,
        slots: slots.clone(),
        reference: (!reference.trim().is_empty()).then(|| reference.trim_end().to_string()),
        constraints: constraints.to_vec(),
        review_verdict: None,
    });
    Ok(key)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exercise::Exercise;
    use std::path::PathBuf;

    fn ex(dir: &str, name: &str, fixture: bool) -> Exercise {
        Exercise {
            path: PathBuf::from(format!("{dir}/{name}.rs")),
            name: name.to_string(),
            category: dir.to_string(),
            title: format!("题 {name}"),
            is_fixture: fixture,
            rel_path: format!("{dir}/{name}.rs"),
        }
    }

    fn template(id: &str) -> Template {
        // Difficulty defaults to Medium; serde parse a tiny TOML.
        toml::from_str(&format!(
            "id = \"{id}\"\ntitle = \"T\"\nconcepts = [\"c.one\"]\nerror_codes = [\"E0308\"]\ndifficulty = \"easy\"\nconfusion = \"初学者以为 x\"\n\nbody = '''\n// a\n'''\n\ntests = '''\n#[cfg(test)]\nmod t {{\n}}\n'''\n\nreference = '''\nfn x() {{}}\n'''\n"
        ))
        .unwrap()
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("rs_idx_{tag}_{}_{}", std::process::id(), nanos))
    }

    #[test]
    fn status_transitions() {
        let mut idx = ExerciseIndex::load_from(temp_dir("st").join("index.json"));
        let key = "generated/mini.rs";
        idx.upsert(ExerciseMeta {
            path: key.into(),
            title: "mini".into(),
            concepts: vec!["c.one".into()],
            error_codes: vec![],
            difficulty: Some("easy".into()),
            source: Source::TemplateFill { template_id: "mini".into() },
            session_id: Some("s1".into()),
            trigger: None,
            created_at: None,
            attempts: 0,
            status: Status::Pending,
            last_error: None,
            hints: Vec::new(),
            feedback: None,
            slots: Default::default(),
            reference: None,
            constraints: Vec::new(),
            review_verdict: None,
        });

        idx.record_attempt(key, false, Some("E0382"));
        assert_eq!(idx.get(key).unwrap().status, Status::Failed { times: 1 });
        assert_eq!(idx.get(key).unwrap().last_error.as_deref(), Some("E0382"));
        idx.record_attempt(key, false, None);
        assert_eq!(idx.get(key).unwrap().status, Status::Failed { times: 2 });
        assert_eq!(idx.get(key).unwrap().attempts, 2);
        idx.record_attempt(key, true, None);
        assert_eq!(idx.get(key).unwrap().status, Status::Passed);
        assert_eq!(idx.get(key).unwrap().last_error, None, "pass clears the error");
        // Passed is sticky; attempts still count.
        idx.record_attempt(key, false, Some("E0308"));
        assert_eq!(idx.get(key).unwrap().status, Status::Passed);
        assert_eq!(idx.get(key).unwrap().attempts, 4);
        assert_eq!(idx.get(key).unwrap().last_error, None, "no error recorded while Passed");

        // Unknown keys are silently ignored.
        idx.record_attempt("generated/nope.rs", true, None);
        assert!(idx.get("generated/nope.rs").is_none());
    }

    #[test]
    fn record_run_updates_state_without_counting_attempts() {
        let mut idx = ExerciseIndex::load_from(temp_dir("runs").join("index.json"));
        idx.upsert(ExerciseMeta {
            path: "generated/r.rs".into(),
            title: "r".into(),
            concepts: vec![],
            error_codes: vec![],
            difficulty: None,
            source: Source::Free,
            session_id: None,
            trigger: None,
            created_at: None,
            attempts: 0,
            status: Status::Pending,
            last_error: None,
            hints: Vec::new(),
            feedback: None,
            slots: Default::default(),
            reference: None,
            constraints: Vec::new(),
            review_verdict: None,
        });
        // Verification runs of unchanged code: attempts and ✗N never
        // move; a Pending entry even stays Pending (9.6 终测：进题
        // 自动首跑失败曾把卡片打成 "✗1 次失败（0 次尝试）").
        idx.record_run("generated/r.rs", false, Some("E0382"));
        assert_eq!(idx.get("generated/r.rs").unwrap().status, Status::Pending);
        assert_eq!(idx.get("generated/r.rs").unwrap().attempts, 0);
        assert_eq!(
            idx.get("generated/r.rs").unwrap().last_error.as_deref(),
            Some("E0382"),
            "错误码仍要可见（卡片展示首跑失败原因）"
        );
        idx.record_run("generated/r.rs", false, Some("E0382"));
        assert_eq!(idx.get("generated/r.rs").unwrap().status, Status::Pending);
        // A real attempt after edits counts — and only it establishes
        // the failure count.
        idx.record_attempt("generated/r.rs", false, Some("E0308"));
        assert_eq!(idx.get("generated/r.rs").unwrap().attempts, 1);
        assert_eq!(idx.get("generated/r.rs").unwrap().status, Status::Failed { times: 1 });
        // Unchanged rerun of a failed attempt: no inflation.
        idx.record_run("generated/r.rs", false, Some("E0308"));
        assert_eq!(idx.get("generated/r.rs").unwrap().attempts, 1);
        assert_eq!(idx.get("generated/r.rs").unwrap().status, Status::Failed { times: 1 });
        idx.record_run("generated/r.rs", true, None);
        assert_eq!(idx.get("generated/r.rs").unwrap().status, Status::Passed);
        assert_eq!(idx.get("generated/r.rs").unwrap().attempts, 1);
        // A real attempt after edits still counts, from any state.
        idx.record_attempt("generated/r.rs", true, None);
        assert_eq!(idx.get("generated/r.rs").unwrap().attempts, 2);
    }

    #[test]
    fn feedback_roundtrip() {
        let mut idx = ExerciseIndex::load_from(temp_dir("fb").join("index.json"));
        idx.upsert(ExerciseMeta {
            path: "generated/a.rs".into(),
            title: "a".into(),
            concepts: vec![],
            error_codes: vec![],
            difficulty: None,
            source: Source::Free,
            session_id: None,
            trigger: None,
            created_at: None,
            attempts: 0,
            status: Status::Pending,
            last_error: None,
            hints: Vec::new(),
            feedback: None,
            slots: Default::default(),
            reference: None,
            constraints: Vec::new(),
            review_verdict: None,
        });
        assert!(idx.set_feedback("generated/a.rs", Feedback::TooHard));
        assert_eq!(idx.get("generated/a.rs").unwrap().feedback, Some(Feedback::TooHard));
        assert!(!idx.set_feedback("generated/missing.rs", Feedback::TooHard));
    }

    #[test]
    fn persist_and_reload() {
        let dir = temp_dir("persist");
        let path = dir.join("index.json");
        {
            let mut idx = ExerciseIndex::load_from(path.clone());
            idx.upsert(ExerciseMeta {
                path: "generated/b.rs".into(),
                title: "b".into(),
                concepts: vec!["c.two".into()],
                error_codes: vec!["E0382".into()],
                difficulty: Some("medium".into()),
                source: Source::TemplateFill { template_id: "t".into() },
                session_id: Some("session_x".into()),
                trigger: Some("你贴的代码报 E0382".into()),
                created_at: Some("2026-09-04T00:00:00Z".into()),
                attempts: 3,
                status: Status::Failed { times: 2 },
                last_error: Some("E0382".into()),
                hints: Vec::new(),
                feedback: Some(Feedback::JustRight),
                slots: Default::default(),
            reference: None,
            constraints: Vec::new(),
            review_verdict: None,
            });
        }
        let idx = ExerciseIndex::load_from(path);
        let m = idx.get("generated/b.rs").unwrap();
        assert_eq!(m.source, Source::TemplateFill { template_id: "t".into() });
        assert_eq!(m.status, Status::Failed { times: 2 });
        assert_eq!(m.trigger.as_deref(), Some("你贴的代码报 E0382"));
        assert_eq!(m.feedback, Some(Feedback::JustRight));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn reconcile_recovers_template_provenance() {
        let mut idx = ExerciseIndex::load_from(temp_dir("rec").join("index.json"));
        let t = template("own-closure-capture");
        let exercises = vec![
            ex("generated", "own_closure_capture", false),
            ex("generated", "own_closure_capture_3", false),
            ex("fixtures/generics", "generics1", true),
            ex("generated", "mystery", false),
        ];
        let added = idx.reconcile(&exercises, std::slice::from_ref(&t));
        assert_eq!(added, 4);

        let m1 = idx.get("generated/own_closure_capture.rs").unwrap();
        assert_eq!(m1.source, Source::TemplateFill { template_id: "own-closure-capture".into() });
        assert_eq!(m1.concepts, vec!["c.one".to_string()]);
        assert_eq!(m1.difficulty.as_deref(), Some("easy"));
        assert_eq!(m1.status, Status::Pending);

        let m3 = idx.get("generated/own_closure_capture_3.rs").unwrap();
        assert_eq!(m3.source, Source::TemplateFill { template_id: "own-closure-capture".into() });

        let seed = idx.get("fixtures/generics/generics1.rs").unwrap();
        assert_eq!(seed.source, Source::Seed);

        let unk = idx.get("generated/mystery.rs").unwrap();
        assert_eq!(unk.source, Source::Unknown);

        // Reconcile is idempotent.
        assert_eq!(idx.reconcile(&exercises, &[t]), 0);
    }

    #[test]
    fn practice_note_reports_session_library_and_weakness() {
        let mut idx = ExerciseIndex::load_from(temp_dir("note").join("index.json"));
        let mk = |path: &str, title: &str, concepts: &[&str], status: Status, seed: bool| ExerciseMeta {
            path: path.into(),
            title: title.into(),
            concepts: concepts.iter().map(|s| s.to_string()).collect(),
            error_codes: vec![],
            difficulty: None,
            source: if seed { Source::Seed } else { Source::TemplateFill { template_id: "t".into() } },
            session_id: None,
            trigger: None,
            created_at: None,
            attempts: 0,
            status,
            last_error: None,
            hints: Vec::new(),
            feedback: None,
            slots: Default::default(),
            reference: None,
            constraints: Vec::new(),
            review_verdict: None,
        };
        idx.entries.insert("generated/1.rs".into(), mk("generated/1.rs", "题一", &["ownership.move"], Status::Passed, false));
        idx.entries.insert("generated/2.rs".into(), mk("generated/2.rs", "题二", &["borrow.shared-mut"], Status::Failed { times: 2 }, false));
        idx.entries.insert("generated/3.rs".into(), mk("generated/3.rs", "题三", &["borrow.shared-mut"], Status::Pending, false));
        idx.entries.insert("fixtures/x.rs".into(), mk("fixtures/x.rs", "种子", &[], Status::Passed, true));

        // No session paths → library + weakness only.
        let note = idx.practice_note(&[]).unwrap();
        assert!(note.starts_with("[Practice status]"), "{note}");
        assert!(note.contains("Library: 1/3 passed"), "{note}");
        assert!(note.contains("borrow.shared-mut (2 fails)"), "{note}");
        assert!(!note.contains("Session exercises"), "{note}");

        // With session paths → session section in order.
        let note = idx.practice_note(&["generated/2.rs".into(), "generated/1.rs".into()]).unwrap();
        assert!(note.contains("\"题二\" (failed 2x)"), "{note}");
        assert!(note.contains("\"题一\" (passed)"), "{note}");

        // Seeds only → nothing but seeds ⇒ still a note (library counts
        // exclude seeds), empty report returns None.
        // Seeds alone are nothing to report (library counts exclude
        // them): the note is None.
        let mut seed_only = ExerciseIndex::load_from(temp_dir("note2").join("index.json"));
        seed_only.entries.insert(
            "fixtures/only.rs".into(),
            ExerciseMeta {
                path: "fixtures/only.rs".into(),
                title: "s".into(),
                concepts: vec![],
                error_codes: vec![],
                difficulty: None,
                source: Source::Seed,
                session_id: None,
                trigger: None,
                created_at: None,
                attempts: 0,
                status: Status::Passed,
                last_error: None,
                hints: Vec::new(),
                feedback: None,
                slots: Default::default(),
            reference: None,
            constraints: Vec::new(),
            review_verdict: None,
            },
        );
        assert!(seed_only.practice_note(&[]).is_none());
    }

    #[test]
    fn migrate_progress_matches_and_consumes_file() {
        let dir = temp_dir("mig");
        let exercises_dir = dir.join("exercises");
        fs::create_dir_all(exercises_dir.join("generated")).unwrap();
        fs::write(exercises_dir.join("generated/done.rs"), "// 题目\n").unwrap();
        // Legacy progress: one real path, one stale path. Paths are
        // canonicalized against the exercises dir, so absolute entries
        // are used here (the runtime CLI always runs from the repo
        // root, where the relative "./exercises/..." form resolves the
        // same way).
        let done_abs = exercises_dir.canonicalize().unwrap().join("generated/done.rs");
        fs::write(
            exercises_dir.join(".progress"),
            format!("{}\n./exercises/generated/gone.rs\n", done_abs.display()),
        )
        .unwrap();
        let mut idx = ExerciseIndex::load_from(index_path(&exercises_dir));
        idx.reconcile(&[ex("generated", "done", false)], &[]);
        assert_eq!(idx.get("generated/done.rs").unwrap().status, Status::Pending);

        let n = migrate_progress(&mut idx, &exercises_dir, &[ex("generated", "done", false)]);
        assert_eq!(n, 1);
        assert_eq!(idx.get("generated/done.rs").unwrap().status, Status::Passed);
        assert!(idx.get("generated/done.rs").unwrap().attempts >= 1);
        assert!(!exercises_dir.join(".progress").exists(), "progress file consumed");
        let _ = fs::remove_dir_all(&dir);
    }
}
