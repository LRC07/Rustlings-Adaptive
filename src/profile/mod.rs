//! Learner profile (M6, design §4.5/§7.6): the "memory" ring that turns
//! practice signals into a two-track picture — rustc error-code counts
//! (coarse track) and per-concept SM-2 spaced-repetition state (fine
//! track) — plus the wrong-answer notebook distilled from the exercise
//! index.
//!
//! Persisted as ONE user-local file
//! (`~/.rustlings_adaptive/profile.json`; design §7.6 listed three
//! files, merged here to keep a single consistent writer — deviation
//! recorded in the design doc).
//!
//! Determinism rule (project-wide): everything here is derived from
//! observed signals; the LLM never writes the profile.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Data model
// ---------------------------------------------------------------------------

/// SM-2 state for one concept (fine track).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sm2 {
    /// Successful consecutive reviews (SM-2 "reps").
    #[serde(default)]
    pub reps: u32,
    /// Ease factor, clamped to ≥1.3.
    #[serde(default = "default_ef")]
    pub ef: f64,
    /// Current interval in days (0 = not yet learned).
    #[serde(default)]
    pub interval_days: u32,
    /// Next review due (RFC3339 date string, UTC). None = never seen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub due: Option<String>,
}

fn default_ef() -> f64 {
    2.5
}

impl Default for Sm2 {
    fn default() -> Self {
        Self { reps: 0, ef: default_ef(), interval_days: 0, due: None }
    }
}

/// Aggregate signals for one concept.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ConceptStat {
    #[serde(default)]
    pub attempts: u32,
    #[serde(default)]
    pub passes: u32,
    #[serde(default)]
    pub fails: u32,
    /// Exercises where a static hint was revealed before passing.
    #[serde(default)]
    pub used_hints: u32,
    /// Explanation-check results in the debrief (hit / miss).
    #[serde(default)]
    pub explanation_hits: u32,
    #[serde(default)]
    pub explanation_misses: u32,
    #[serde(default)]
    pub sm2: Sm2,
}

/// One wrong-answer notebook entry, distilled from the exercise index.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NotebookEntry {
    /// Index key ("generated/x.rs").
    pub path: String,
    pub title: String,
    pub concepts: Vec<String>,
    /// Last error code seen while solving (may be cleared once passed).
    pub last_error: Option<String>,
    /// Most recent failure's error code, surviving a later pass
    /// (0908 反馈 [/stats wrong]); drives wrongbook error-code filtering.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_fail_error: Option<String>,
    /// Solve attempts recorded so far.
    pub attempts: u32,
    /// "failed" | "passed" — entries stay in the notebook even after a
    /// pass (that is what makes it a notebook, not a queue).
    pub passed: bool,
    /// Review-gate verdict when one was produced (M5).
    pub review_verdict: Option<String>,
}

/// The whole profile.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Profile {
    /// Coarse track: rustc error code → how often it blocked the learner.
    #[serde(default)]
    pub error_codes: BTreeMap<String, u32>,
    /// Fine track: concept id → aggregated signals + SM-2.
    #[serde(default)]
    pub concepts: BTreeMap<String, ConceptStat>,
}

impl Profile {
    /// Merge one practice attempt (call for every compile_and_run).
    pub fn record_attempt(&mut self, concepts: &[String], error_code: Option<&str>, passed: bool) {
        for c in concepts {
            let s = self.concepts.entry(c.clone()).or_default();
            s.attempts += 1;
            if passed {
                s.passes += 1;
            } else {
                s.fails += 1;
            }
        }
        if !passed
            && let Some(code) = error_code
        {
            *self.error_codes.entry(code.to_string()).or_default() += 1;
        }
    }

    /// Merge the debrief outcome (M5 signals) into the profile and run
    /// the SM-2 update for the exercise's concepts.
    pub fn record_debrief(
        &mut self,
        concepts: &[String],
        used_hints: bool,
        explanation_hit: Option<bool>,
        quality: u8,
    ) {
        let now = chrono::Utc::now();
        for c in concepts {
            let s = self.concepts.entry(c.clone()).or_default();
            if used_hints {
                s.used_hints += 1;
            }
            match explanation_hit {
                Some(true) => s.explanation_hits += 1,
                Some(false) => s.explanation_misses += 1,
                None => {}
            }
            s.sm2 = sm2_update(&s.sm2, quality, now);
        }
    }

    /// Weakest concepts (most failures, then most attempts) for the
    /// stats page and the coach's context.
    pub fn weakest(&self, n: usize) -> Vec<(String, u32, u32)> {
        let mut v: Vec<(String, u32, u32)> = self
            .concepts
            .iter()
            .filter(|(_, s)| s.fails > 0)
            .map(|(c, s)| (c.clone(), s.fails, s.attempts))
            .collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then(b.2.cmp(&a.2)).then(a.0.cmp(&b.0)));
        v.truncate(n);
        v
    }

    /// Top error codes by frequency.
    pub fn top_error_codes(&self, n: usize) -> Vec<(String, u32)> {
        let mut v: Vec<(String, u32)> =
            self.error_codes.iter().map(|(c, n)| (c.clone(), *n)).collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        v.truncate(n);
        v
    }

    /// Concepts whose SM-2 review is due today or overdue. A concept
    /// with a failed recall (q<3 → reps reset to 0) but an elapsed due
    /// date counts too — it is the MOST in need of a review. This is
    /// the single authority for "到期" everywhere (stats page, coach
    /// tools); row formatters must not apply their own predicate
    /// (9.6 实测：三处口径不一致).
    pub fn due_concepts(&self, today: chrono::DateTime<chrono::Utc>) -> Vec<String> {
        self.concepts
            .iter()
            .filter(|(_, s)| {
                s.sm2
                    .due
                    .as_deref()
                    .and_then(|d| chrono::DateTime::parse_from_rfc3339(d).ok())
                    .map(|d| d <= today)
                    .unwrap_or(false)
            })
            .map(|(c, _)| c.clone())
            .collect()
    }
}

/// The profile plus its persistence location (save failures degrade to
/// a stderr warning, same policy as the exercise index).
pub struct ProfileStore {
    path: PathBuf,
    pub profile: Profile,
}

impl ProfileStore {
    pub fn load_or_create() -> Self {
        Self::from_path(default_path())
    }

    pub fn from_path(path: PathBuf) -> Self {
        let profile = fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Self { path, profile }
    }

    /// Record + persist one practice attempt (M6.2 signal tap in the
    /// practice loop).
    pub fn record_attempt(&mut self, concepts: &[String], error_code: Option<&str>, passed: bool) {
        self.profile.record_attempt(concepts, error_code, passed);
        self.save();
    }

    /// Record + persist one debrief outcome (M6.2 signal tap after the
    /// M5 debrief), including the SM-2 update.
    #[allow(clippy::too_many_arguments)]
    pub fn record_debrief(
        &mut self,
        concepts: &[String],
        used_hints: bool,
        explanation_hit: Option<bool>,
        quality: u8,
    ) {
        self.profile.record_debrief(concepts, used_hints, explanation_hit, quality);
        self.save();
    }

    pub fn save(&self) {
        if let Some(parent) = self.path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        match serde_json::to_string_pretty(&self.profile) {
            Ok(json) => {
                if let Err(e) = fs::write(&self.path, json) {
                    eprintln!("  （profile 写入失败：{e}）");
                }
            }
            Err(e) => eprintln!("  （profile 序列化失败：{e}）"),
        }
    }
}

fn default_path() -> PathBuf {
    let base = std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."));
    base.join(".rustlings_adaptive").join("profile.json")
}

// ---------------------------------------------------------------------------
// SM-2 (classic algorithm, quality 0–5)
// ---------------------------------------------------------------------------

/// One SM-2 update. Quality mapping from M5 debrief signals lives in
/// `debrief_quality`; this function is the pure scheduler.
pub fn sm2_update(prev: &Sm2, quality: u8, now: chrono::DateTime<chrono::Utc>) -> Sm2 {
    let q = quality.min(5) as f64;
    let mut next = prev.clone();
    // Ease factor adjustment (classic SM-2), clamped to [1.3, 2.5]:
    // the floor is SuperMemo's, the cap keeps a stream of perfect
    // reviews from inflating EF without bound (common implementations
    // hold EF at its initial maximum).
    next.ef = (prev.ef + (0.1 - (5.0 - q) * (0.08 + (5.0 - q) * 0.02))).clamp(1.3, 2.5);
    if q >= 3.0 {
        next.reps = prev.reps.saturating_add(1);
        next.interval_days = match next.reps {
            1 => 1,
            2 => 6,
            n => {
                let d = (prev.interval_days.max(1) as f64 * next.ef).round() as u32;
                d.max(n) // never shrink below the rep count
            }
        };
    } else {
        // Failed recall: reset the schedule (EF is kept in classic SM-2).
        next.reps = 0;
        next.interval_days = 0;
    }
    let due = now + chrono::Duration::days(next.interval_days.max(1) as i64);
    next.due = Some(due.to_rfc3339());
    next
}

/// Map M5 debrief signals to an SM-2 quality 0–5 (see 复盘_M5 §一):
/// first-try + hit + clean → 5; hit → 4; hints/variant → 3; miss → 2;
/// hard-fail (≥3 attempts) → 1.
pub fn debrief_quality(
    attempts_before: u32,
    used_hints: bool,
    explanation_hit: Option<bool>,
    clean_verdict: bool,
) -> u8 {
    if attempts_before >= 3 {
        return 1;
    }
    match explanation_hit {
        Some(false) => 2,
        Some(true) => {
            if attempts_before == 0 && clean_verdict && !used_hints {
                5
            } else if used_hints {
                3
            } else {
                4
            }
        }
        None => {
            if used_hints || attempts_before > 0 {
                3
            } else {
                4
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Wrong-answer notebook (distilled, not duplicated)
// ---------------------------------------------------------------------------

/// Distill the notebook from the exercise index: every exercise that
/// has ever failed (attempts recorded with a Failed status at some
/// point — approximated by `attempts > 0` for non-Pending entries, plus
/// anything currently Failed) — passed exercises stay with `passed:
/// true`. `filter` (optional) matches concept id / top domain / error
/// code substring.
pub fn notebook_from_index(
    index: &crate::exercise::index::ExerciseIndex,
    filter: Option<&str>,
) -> Vec<NotebookEntry> {
    let mut out: Vec<NotebookEntry> = index
        .iter()
        .filter(|m| {
            if m.attempts == 0 {
                return false;
            }
            true
        })
        .map(|m| NotebookEntry {
            path: m.path.clone(),
            title: m.title.clone(),
            concepts: m.concepts.clone(),
            last_error: m.last_error.clone(),
            last_fail_error: m.last_fail_error.clone(),
            attempts: m.attempts,
            passed: m.status == crate::exercise::index::Status::Passed,
            review_verdict: m.review_verdict.clone(),
        })
        .collect();
    if let Some(f) = filter {
        let f = f.trim();
        if !f.is_empty() {
            let fl = f.to_ascii_lowercase();
            out.retain(|e| {
                e.concepts.iter().any(|c| c.to_ascii_lowercase().contains(&fl))
                    || e.concepts
                        .iter()
                        .any(|c| c.split('.').next().map(|d| d.contains(&fl)).unwrap_or(false))
                    || e.last_error
                        .as_deref()
                        .map(|c| c.to_ascii_lowercase().contains(&fl))
                        .unwrap_or(false)
                    // 0908 反馈 [/stats wrong]: last_error is cleared on
                    // pass — without the surviving failure code, error-
                    // code filtering found nothing for solved exercises.
                    || e.last_fail_error
                        .as_deref()
                        .map(|c| c.to_ascii_lowercase().contains(&fl))
                        .unwrap_or(false)
                    || e.title.to_ascii_lowercase().contains(&fl)
            });
        }
    }
    // Failed first, then most attempts.
    out.sort_by(|a, b| a.passed.cmp(&b.passed).then(b.attempts.cmp(&a.attempts)));
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sm2_first_pass_then_second() {
        let now = chrono::Utc::now();
        let s0 = Sm2::default();
        let s1 = sm2_update(&s0, 5, now);
        assert_eq!(s1.reps, 1);
        assert_eq!(s1.interval_days, 1);
        assert!(s1.due.is_some());

        let s2 = sm2_update(&s1, 4, now);
        assert_eq!(s2.reps, 2);
        assert_eq!(s2.interval_days, 6);
        // q=4 leaves EF unchanged (2.5, at the cap after the q=5 pass).
        assert!((s2.ef - 2.5).abs() < 1e-9);
    }

    #[test]
    fn sm2_failure_resets_and_low_quality_never_drops_ef_below_floor() {
        let now = chrono::Utc::now();
        let s0 = sm2_update(&Sm2::default(), 5, now);
        let s1 = sm2_update(&s0, 6, now); // quality clamped to 5
        assert_eq!(s1.ef, s0.ef, "q>5 clamps to 5 → no change");
        let s2 = sm2_update(&s1, 1, now);
        assert_eq!(s2.reps, 0);
        assert_eq!(s2.interval_days, 0);
        // 20 consecutive q=1 must floor EF at 1.3.
        let mut s = Sm2::default();
        for _ in 0..20 {
            s = sm2_update(&s, 1, now);
        }
        assert!((s.ef - 1.3).abs() < 1e-9, "ef={}", s.ef);
    }

    #[test]
    fn debrief_quality_mapping() {
        // First try, hit, clean, no hints → 5.
        assert_eq!(debrief_quality(0, false, Some(true), true), 5);
        // Hit but suggestions → 4.
        assert_eq!(debrief_quality(0, false, Some(true), false), 4);
        // Hit with hints → 3.
        assert_eq!(debrief_quality(1, true, Some(true), true), 3);
        // Miss → 2 (even when clean).
        assert_eq!(debrief_quality(0, false, Some(false), true), 2);
        // ≥3 attempts → 1 regardless.
        assert_eq!(debrief_quality(3, false, Some(true), true), 1);
        // Skipped check, first try, clean → 4 (neutral-positive).
        assert_eq!(debrief_quality(0, false, None, true), 4);
    }

    #[test]
    fn record_attempt_tracks_both_tracks() {
        let mut p = Profile::default();
        p.record_attempt(&["ownership.move".into()], Some("E0382"), false);
        p.record_attempt(&["ownership.move".into()], None, true);
        assert_eq!(p.error_codes.get("E0382"), Some(&1));
        let s = p.concepts.get("ownership.move").unwrap();
        assert_eq!((s.attempts, s.passes, s.fails), (2, 1, 1));
        assert_eq!(p.weakest(5), vec![("ownership.move".to_string(), 1, 2)]);
    }

    #[test]
    fn record_debrief_updates_sm2_and_signals() {
        let mut p = Profile::default();
        p.record_debrief(&["c.one".into()], true, Some(false), 2);
        let s = p.concepts.get("c.one").unwrap();
        assert_eq!(s.used_hints, 1);
        assert_eq!((s.explanation_hits, s.explanation_misses), (0, 1));
        assert_eq!(s.sm2.reps, 0, "q=2 fails recall");
        assert!(s.sm2.due.is_some());
    }

    #[test]
    fn due_concepts_respects_reps_and_dates() {
        let mut p = Profile::default();
        let now = chrono::Utc::now();
        // Learned concept, due in 6 days → not due now.
        p.record_debrief(&["c.due-later".into()], false, Some(true), 4);
        p.record_debrief(&["c.due-later".into()], false, Some(true), 4);
        assert!(p.due_concepts(now).is_empty());
        // Learned concept, backdated due → due.
        let s = p.concepts.get_mut("c.due-later").unwrap();
        s.sm2.due = Some((now - chrono::Duration::days(1)).to_rfc3339());
        assert_eq!(p.due_concepts(now), vec!["c.due-later".to_string()]);
        // Never-learned concept (no due date at all) is never due.
        p.concepts.entry("c.fresh".to_string()).or_default();
        assert!(!p.due_concepts(now).contains(&"c.fresh".to_string()));
        // A concept whose recall FAILED (q<3 → reps reset to 0) but
        // whose due date has elapsed IS due — it needs review most
        // (the old reps>0 gate hid it from the queue while the stats
        // row still said 已到期: the reported口径 contradiction).
        p.record_debrief(&["c.reset".into()], false, Some(false), 2);
        assert_eq!(p.concepts.get("c.reset").unwrap().sm2.reps, 0);
        p.concepts.get_mut("c.reset").unwrap().sm2.due =
            Some((now - chrono::Duration::days(1)).to_rfc3339());
        assert!(p.due_concepts(now).contains(&"c.reset".to_string()));
    }

    #[test]
    fn notebook_distills_from_index_and_filters() {
        use crate::exercise::index::{ExerciseIndex, ExerciseMeta, Source, Status};
        let mut idx = ExerciseIndex::load_from(std::env::temp_dir().join("rs_profile_nb_nonexistent"));
        let mk = |path: &str, title: &str, concepts: &[&str], code: Option<&str>, status: Status, attempts: u32| ExerciseMeta {
            path: path.into(),
            title: title.into(),
            concepts: concepts.iter().map(|s| s.to_string()).collect(),
            error_codes: vec![],
            difficulty: None,
            source: Source::TemplateFill { template_id: "t".into() },
            session_id: None,
            trigger: None,
            created_at: None,
            attempts,
            status,
            last_error: code.map(str::to_string),
            last_fail_error: code.map(str::to_string),
            hints: Vec::new(),
            feedback: None,
            slots: Default::default(),
            reference: None,
            constraints: Vec::new(),
            review_verdict: None,
        };
        idx.upsert(mk("a", "题A", &["ownership.move"], Some("E0382"), Status::Failed { times: 2 }, 3));
        // Passed AFTER failing with E0384: last_error cleared, the
        // failure code survives (0908 反馈 [/stats wrong]).
        let mut b = mk("b", "题B", &["borrow.shared-mut"], None, Status::Passed, 2);
        b.last_error = None;
        b.last_fail_error = Some("E0384".into());
        idx.upsert(b);
        idx.upsert(mk("c", "题C", &["closures.traits"], None, Status::Passed, 0)); // never attempted

        let nb = notebook_from_index(&idx, None);
        assert_eq!(nb.len(), 2, "never-attempted excluded");
        assert_eq!(nb[0].path, "a", "failed first");
        assert!(!nb[0].passed);
        assert!(nb[1].passed);

        let by_code = notebook_from_index(&idx, Some("e0382"));
        assert_eq!(by_code.len(), 1);
        let by_domain = notebook_from_index(&idx, Some("borrow"));
        assert_eq!(by_domain.len(), 1);
        // Error-code filter must ALSO find solved exercises via the
        // surviving failure code (used to be always empty).
        let by_hist_code = notebook_from_index(&idx, Some("E0384"));
        assert_eq!(by_hist_code.len(), 1, "historical failure code matches");
        assert_eq!(by_hist_code[0].path, "b");
        assert!(by_hist_code[0].passed);
        let by_none = notebook_from_index(&idx, Some("closures"));
        assert!(by_none.is_empty(), "zero-attempt exercise is not in the notebook");
    }

    #[test]
    fn profile_persists_across_reload() {
        let path = std::env::temp_dir().join(format!(
            "rs_profile_test_{}.json",
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        {
            let mut store = ProfileStore::from_path(path.clone());
            store.profile.record_attempt(&["c.x".into()], Some("E0502"), false);
            store.save();
        }
        let store = ProfileStore::from_path(path.clone());
        assert_eq!(store.profile.error_codes.get("E0502"), Some(&1));
        let _ = fs::remove_file(&path);
    }
}
