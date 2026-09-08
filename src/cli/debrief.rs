//! Post-pass review gate + interactive debrief (M5, design §4.2/§4.3).
//!
//! Called from the practice loop the first time an exercise passes:
//! the gate (static + LLM review + probe) runs here behind a spinner,
//! the verdict is rendered and persisted onto the index entry, and the
//! debrief steps (explanation check → better-solution challenge →
//! comparison → follow-up decision) follow. Everything the model sees
//! comes from real artifacts (template / index meta / the user's file).

use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::Result;

use crate::agent::ChatTurnCaller;
use crate::config::ModelConfig;
use crate::exercise::index::{ExerciseIndex, ExerciseMeta, Source};
use crate::exercise::Exercise;
use crate::llm::{ChatMessage, LlmClient, LlmReply};
use crate::review::{self, ReviewCaller};
use crate::template;
use crate::usage::UsageTracker;

use super::md::paint_line;
use super::render;
use super::spinner::Spinner;
use super::{read_line, Line};

/// What the review gate / debrief needs from the REPL (built fresh at
/// each entry so a `/model` switch takes effect).
pub(crate) struct DebriefDeps<'a> {
    pub client: Option<&'a LlmClient>,
    pub cfg: &'a ModelConfig,
    pub tracker: Arc<Mutex<UsageTracker>>,
    /// Editor for the challenge loop's `[e]` (falls back to the
    /// practice-level resolution when absent).
    pub editor: Option<&'a str>,
}

/// CLI bridge for the review/debrief LLM steps: budget gate (R6) +
/// usage recording under a per-step phase tag ("review" / "probe" /
/// "debrief"), then the shared chat caller.
struct CliReviewCaller {
    caller: Option<Arc<dyn ChatTurnCaller>>,
    model: String,
    price_in: f64,
    price_out: f64,
    budget: Option<f64>,
    tracker: Arc<Mutex<UsageTracker>>,
}

impl ReviewCaller for CliReviewCaller {
    fn call(&mut self, phase: &'static str, system: &str, user: &str, max_tokens: u32) -> Result<LlmReply> {
        let Some(caller) = self.caller.as_ref() else {
            anyhow::bail!("未配置 LLM（离线模式只跑静态评审）");
        };
        let totals =
            self.tracker.lock().unwrap_or_else(|p| p.into_inner()).all_totals().cost_usd;
        crate::usage::check_budget(totals, self.budget)?;
        let out = caller.chat_turn_bounded(
            &[ChatMessage::system(system.to_string()), ChatMessage::user(user.to_string())],
            &[],
            Some(max_tokens),
        )?;
        let cost = crate::usage::cost_usd(
            out.usage.prompt_tokens,
            out.usage.completion_tokens,
            self.price_in,
            self.price_out,
        );
        self.tracker.lock().unwrap_or_else(|p| p.into_inner()).record(
            &self.model,
            out.usage.prompt_tokens,
            out.usage.completion_tokens,
            out.usage.reasoning_tokens,
            cost,
            phase,
        );
        Ok(LlmReply {
            content: out.content.unwrap_or_default(),
            usage: out.usage,
            finish_reason: out.finish_reason,
        })
    }
}

// ---------------------------------------------------------------------------
// Review-input assembly (real artifacts only)
// ---------------------------------------------------------------------------

/// Build the review input for a passed exercise: template metadata
/// (confusion / anti-patterns / rendered body) when the source is
/// recoverable, index-persisted reference & constraints (M5.1) as the
/// primary source with a template backfill, and the user's file.
fn build_input(
    repo_root: &Path,
    meta: &ExerciseMeta,
    user_code: &str,
    attempts_before: u32,
) -> review::ReviewInput {
    let templates = template::load_dir(&repo_root.join("templates")).unwrap_or_default();
    let tpl = template_id_of(meta).and_then(|id| templates.iter().find(|t| t.id == id));

    let reference = meta.reference.clone().or_else(|| {
        tpl.and_then(|t| {
            template::render(t, &template::fill_for_attempt(t, 0))
                .ok()
                .map(|r| r.reference.trim_end().to_string())
        })
    });

    let constraint_specs = if meta.constraints.is_empty() {
        tpl.map(|t| t.constraints.clone()).unwrap_or_default()
    } else {
        meta.constraints.clone()
    };

    // 题面: M9a5 — the PRISTINE body recorded at generation time is the
    // authoritative input (review gate + quiz). The old rebuild
    // re-rendered the BASE template with recorded slots: for adapted
    // (tier-2) and free exercises there is no matching template body,
    // so the quiz/review saw the wrong scenario entirely (实测: 改编题
    // 的校核题按模板生成). Fallback chain: recorded body → template
    // render (legacy tier-1 entries) → the file's instruction prefix.
    let body = meta
        .body
        .clone()
        .filter(|b| !b.trim().is_empty())
        .or_else(|| {
            tpl.and_then(|t| {
                let values =
                    if meta.slots.is_empty() { template::fill_for_attempt(t, 0) } else { meta.slots.clone() };
                template::render(t, &values).ok().map(|r| r.body)
            })
        })
        .unwrap_or_else(|| body_from_file(user_code));

    review::ReviewInput {
        title: meta.title.clone(),
        concepts: meta.concepts.clone(),
        body,
        // Review sees the implementation view (no test module, no
        // I AM NOT DONE marker) — see review::review_view.
        user_code: review::review_view(user_code),
        anti_patterns: tpl.map(|t| t.anti_patterns.clone()).unwrap_or_default(),
        review_hints: tpl.map(|t| review::ReviewHintsSeed {
            root_cause: t.review_hints.as_ref().map(|h| h.root_cause.clone()).unwrap_or_default(),
            misconceptions: t.review_hints.as_ref().map(|h| h.misconceptions.clone()).unwrap_or_default(),
        }),
        confusion: tpl.map(|t| t.confusion.clone()),
        reference,
        constraint_specs,
        attempts: attempts_before,
    }
}

fn template_id_of(meta: &ExerciseMeta) -> Option<&str> {
    match &meta.source {
        Source::TemplateFill { template_id } => Some(template_id),
        Source::Adapted { base } => Some(base),
        _ => None,
    }
}

/// Instruction prefix of a written exercise file: everything before
/// the test module, minus the leading title line (`write_exercise`
/// layout). Fallback for exercises without a recoverable template.
fn body_from_file(content: &str) -> String {
    let end = content.find("#[cfg(test)]").unwrap_or(content.len());
    let mut lines: Vec<&str> = content[..end].lines().collect();
    if !lines.is_empty() {
        lines.remove(0); // title line
    }
    lines.join("\n").trim().to_string()
}

// ---------------------------------------------------------------------------
// The gate, behind a spinner (R4)
// ---------------------------------------------------------------------------

/// Run `f` on a worker thread behind a spinner; polls the interrupt
/// flag, so Ctrl-C abandons the result (the LLM call finishes in the
/// background and is accounted as usual). None = interrupted/panic.
fn run_with_spinner<T, F>(label: &str, f: F) -> Option<T>
where
    T: Send + 'static,
    F: FnOnce(&dyn Fn(&str)) -> T + Send + 'static,
{
    let (sp, slot) = Spinner::start(label);
    let slot2 = slot.clone();
    let handle = std::thread::spawn(move || {
        f(&move |s: &str| {
            if let Ok(mut g) = slot2.lock() {
                *g = s.to_string();
            }
        })
    });
    let res = loop {
        if handle.is_finished() {
            break handle.join().ok();
        }
        if crate::agent::is_interrupted() {
            sp.stop();
            println!("  （已打断；LLM 调用将在后台结束并照常记账）");
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(80));
    };
    sp.stop();
    res
}

/// Fresh caller with the current config/budget/prices (R6 phases).
fn make_caller(deps: &DebriefDeps) -> Option<Box<dyn ReviewCaller + Send>> {
    deps.client.as_ref().map(|c| {
        Box::new(CliReviewCaller {
            caller: Some(Arc::new((*c).clone())),
            model: deps.cfg.model.clone(),
            price_in: deps.cfg.prices.input,
            price_out: deps.cfg.prices.output,
            budget: deps.cfg.budget_usd(),
            tracker: deps.tracker.clone(),
        }) as Box<dyn ReviewCaller + Send>
    })
}

/// Run the review gate after a first-time pass, render the verdict and
/// persist it on the index entry, then walk the debrief steps (§4.3).
#[allow(clippy::too_many_arguments)]
pub(crate) fn after_pass(
    deps: &DebriefDeps,
    index: &mut ExerciseIndex,
    key: &str,
    meta: &ExerciseMeta,
    ex: &Exercise,
    repo_root: &Path,
    last_fail: Option<&str>,
    used_hints: bool,
) -> DebriefExit {
    let Ok(content) = std::fs::read_to_string(&ex.path) else {
        println!("  （无法读取练习文件，跳过评审门）");
        return DebriefExit::Stay;
    };
    let attempts_before = meta.attempts.saturating_sub(1);
    let mut input = build_input(repo_root, meta, &content, attempts_before);

    println!();
    println!("{}", render::header("解答评审门"));

    let caller = make_caller(deps);
    let quiz_caller = make_caller(deps);
    let cmp_caller = make_caller(deps);
    let input2 = input.clone();
    // 9.5 实测：the gate's LLM call can run for minutes on slow
    // endpoints — set the expectation up front.
    println!("  （评审门与理解校核、四维对比并行调用模型，端点慢时可能需要 1–2 分钟）");
    // M9a4 校核∥评审并行 + M9a5 对比预取: quiz generation, the review
    // gate AND the four-dimension comparison are INDEPENDENT (all eat
    // only ReviewInput against the ORIGINAL code), so all three run
    // beside each other behind ONE spinner. Wall time drops from three
    // serial stages to max(them). The side threads stay silent (the
    // spinner line belongs to the gate); a quiz failure or panic
    // degrades to exactly the old "跳过本步" path. Interrupt semantics
    // unchanged: the spinner abandons all, in-flight calls finish in
    // the background and still bill. The comparison prefetch is valid
    // only while the code stays the ORIGINAL one — step2's challenge
    // path re-runs it when the learner actually edited (see below).
    let input_quiz = input.clone();
    let input_cmp = input.clone();
    let lf = last_fail.map(str::to_string);
    let (outcome, quiz_pre, cmp_pre) = match run_with_spinner(
        "评审+校核+对比（并行）：评审解答 / 生成校核题 / 四维评审…",
        move |progress| {
            let quiz_thread = quiz_caller.map(|mut c| {
                std::thread::spawn(move || review::llm_quiz(&mut *c, &input_quiz, lf.as_deref()))
            });
            let cmp_thread = std::thread::spawn(move || run_comparison_raw(cmp_caller, &input_cmp));
            let outcome = review::run_gate(caller, input2, progress);
            if quiz_thread.is_some() {
                progress("评审完成，等待校核题/对比收尾…");
            }
            let quiz_pre = quiz_thread.and_then(|h| h.join().ok());
            let cmp_pre = cmp_thread.join().ok().flatten();
            (outcome, quiz_pre, cmp_pre)
        },
    ) {
        Some((o, q, c)) => (o, q, c),
        None => return DebriefExit::Stay,
    };
    render_gate(&outcome);
    index.set_review_verdict(key, outcome.verdict.key());

    // ── Debrief (§4.3) ──

    // Step 1: explanation check (understanding quiz) — the quiz was
    // already fetched beside the gate; only the interactive part and
    // the optional free-text judging happen here.
    let explanation_hit = step1_explanation_check(deps, &input, quiz_pre);

    // Step 2: better-solution challenge (triggered when not clean).
    let mut outcome = outcome;
    let original_code = input.user_code.clone();
    let mut cmp = cmp_pre;
    if outcome.verdict != review::Verdict::Clean
        && let Some((updated, code)) = step2_challenge(deps, index, key, ex, &mut input, &outcome)
    {
        outcome = updated;
        input.user_code = code.clone();
        // The prefetch ran against the ORIGINAL code; an actual edit
        // invalidates it — re-run the comparison on the new code (the
        // "r" path already re-reviewed it above).
        if review::review_view(&code) != original_code {
            cmp = step3_compare(deps, &input);
        }
    }

    // Step 3: two-dimensional comparison (machine + LLM). M9b: data
    // only — the rendering happens inside the debrief-theatre panel.
    // M9a5: usually already prefetched beside the gate; only the
    // challenge-with-edit path pays a fresh (spinner) run here.

    // Step 4: follow-up decision (deterministic) + optional handback.
    let follow_up = review::decide_follow_up(&review::FollowUpInput {
        attempts_before,
        used_hints,
        explanation_hit,
        verdict: outcome.verdict,
        had_violations: outcome.statics.has_constraint_violations(),
    });

    // M6.2: the debrief is complete — update the concept profile (SM-2
    // + M5 signal counters) and persist.
    let quality = crate::profile::debrief_quality(
        attempts_before,
        used_hints,
        explanation_hit,
        outcome.verdict == review::Verdict::Clean,
    );
    crate::profile::ProfileStore::load_or_create().record_debrief(
        &meta.concepts,
        used_hints,
        explanation_hit,
        quality,
    );

    step4_follow_up(
        deps,
        meta,
        &outcome,
        explanation_hit,
        last_fail,
        follow_up,
        cmp,
        attempts_before,
        used_hints,
    )
}

/// The debrief tail's three exits (0909 反馈): the learner here has
/// exactly THREE decisions — go back to the chat WITHOUT any injected
/// message, stay in the practice pages, or hand the next-exercise
/// request to the coach in one keypress. The old [Enter]/[n]/[q] row
/// mapped onto these badly: [Enter] FORCED an injected message and
/// [n]/[q] were the same action with two labels.
pub(crate) enum DebriefExit {
    /// Back to the exercise menu.
    Stay,
    /// Back to the chat, nothing sent.
    Chat,
    /// Back to the chat, auto-send the next-exercise request.
    AskNext(String),
}

/// Render the gate outcome (静态逐项 → LLM 评审 → probe → 最终判定).
fn render_gate(o: &review::GateOutcome) {
    let s = &o.statics;
    let todo_txt = if s.has_todo_residue() {
        format!("todo! 残留 {} 行", render::red(&s.todo_residue.len().to_string()))
    } else {
        "todo! 残留无".to_string()
    };
    let constraint_txt = if s.has_constraint_violations() {
        format!("约束 {}", render::red(&format!("✗ {}", s.violations.len())))
    } else {
        format!("约束 {}", render::green("✓"))
    };
    let clippy_txt = if s.clippy_available {
        format!("clippy {} 条", s.clippy_lints.len())
    } else {
        "clippy 未安装，已跳过".to_string()
    };
    println!("  · 静态：{todo_txt} ｜ {constraint_txt} ｜ {clippy_txt} ｜ 有效行数 {}", s.effective_lines);
    for v in &s.violations {
        println!("    - {}（第 {} 行）", v.message, v.line);
    }
    for l in &s.clippy_lints {
        println!(
            "    - [{}] {}（第 {} 行）",
            l.lint.trim_start_matches("clippy::"),
            l.message,
            l.line
        );
    }

    if let Some(e) = &o.llm_error {
        println!("  · LLM 评审跳过：{e}");
    }
    if let Some(r) = &o.llm {
        // One-line summary even when there are no findings — otherwise
        // a clean verdict looks like the LLM layer never ran.
        if !r.summary.is_empty() && r.findings.is_empty() {
            println!("    · {0}", paint_line(&r.summary));
        }
        for f in &r.findings {
            println!("    · [{}] {}", severity_kind_cn(f), paint_line(&f.message));
            if let Some(w) = &f.better_way {
                println!("      更优：{}", paint_line(w));
            }
        }
    } else if o.llm_error.is_none() {
        println!("  · LLM 评审：未配置 Key，仅静态评审");
    }

    if let Some(p) = &o.probe {
        match p.passed {
            Some(true) => println!("  · 附加验证测试：{} 通过 —— 怀疑解除", render::green("✓")),
            Some(false) => {
                println!("  · 附加验证测试：{} 未通过", render::red("✗"));
                if let Some(f) = &p.failure {
                    for line in f.lines().take(6) {
                        println!("      {line}");
                    }
                }
            }
            None => println!("  · 附加验证测试：无法执行（probe 本身未编译成功，不计入判定）"),
        }
        // Transparency: show what the extra test actually checked.
        println!("    附加测试内容：");
        for line in p.test_code.trim().lines().take(10) {
            println!("      {line}");
        }
    }

    println!("  · 最终判定：{}", render::bold(o.verdict.label_cn()));
    if o.llm.is_some() {
        println!(
            "{}",
            render::dim("    （模型单次评审，判定与分数可能有波动；四维分数供参考）")
        );
    }
    // 9.6 实测 B5：a passing verdict with constraint violations felt
    // too lenient — make the warning loud at the verdict itself.
    if s.has_constraint_violations() {
        println!(
            "  {} 存在约束违例（{} 处）——通过记录保留，但请按上面的建议改进；\
             复习时系统会建议附加更严约束的变式。",
            render::yellow("⚠"),
            s.violations.len()
        );
    }
    if !o.counted_as_mastery() {
        println!("    （本题不计入掌握；建议 [a] 问教练弄懂后再练变式）");
    }
}

fn severity_kind_cn(f: &review::Finding) -> String {
    let sev = match f.severity.as_str() {
        "major" => "重要",
        "minor" => "次要",
        _ => "提示",
    };
    let kind = match f.kind.as_str() {
        "idiom" => "惯用性",
        "readability" => "可读性",
        "maintenance" => "可维护性",
        "design" => "设计",
        "logic" => "逻辑",
        _ => f.kind.as_str(),
    };
    format!("{sev}·{kind}")
}

// ---------------------------------------------------------------------------
// Debrief Step 1: explanation check (§4.3)
// ---------------------------------------------------------------------------

/// Prompt that never leaves the debrief: Ctrl-C cancels the line,
/// EOF/empty handled by the caller's own semantics.
fn ask(prompt: &str) -> Option<String> {
    // M9l bug6 fix: keystrokes typed while an LLM spinner held the
    // foreground must not leak into the debrief prompts — a stray
    // Enter here hand-motivates a whole paid exercise generation.
    crate::cli::flush_stdin();
    match read_line(prompt) {
        Line::Text(s) => Some(s),
        Line::Interrupted => {
            println!("  ^C 已取消本行输入");
            crate::cli::flush_stdin();
            None
        }
        Line::Eof => None,
    }
}

/// Step 1: understanding quiz (LLM-generated options anchored on the
/// template's root-cause/misconception seeds; free text judged by the
/// model). Returns Some(hit/miss); None = skipped.
///
/// M9a4: the quiz itself is generated BESIDE the review gate (see
/// after_pass) and arrives here as `pre` — None = offline, Some(Err) =
/// generation failed (both degrade to the old skip messages); only the
/// interactive part and the optional free-text judging remain here.
/// Stable pseudo-random rotation offset for the quiz options (M9a5):
/// hash of (title, attempts) — deterministic per (exercise, attempt)
/// and spread across exercises. `DefaultHasher::new()` uses fixed keys,
/// so the same input hashes the same in every run.
fn quiz_rotation_offset(title: &str, attempts: u32, len: usize) -> usize {
    use std::hash::{Hash, Hasher};
    if len <= 1 {
        return 0;
    }
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (title, attempts).hash(&mut h);
    (h.finish() as usize) % len
}

fn step1_explanation_check(
    deps: &DebriefDeps,
    input: &review::ReviewInput,
    pre: Option<Result<review::Quiz, anyhow::Error>>,
) -> Option<bool> {
    let Some(quiz) = pre else {
        println!();
        println!("  复盘（离线）：跳过理解校核。");
        return None;
    };
    println!();
    println!("{}", render::header("复盘 · 理解校核"));

    let quiz = match quiz {
        Ok(q) => q,
        Err(_) => {
            println!("  （未能生成校核题，跳过本步）");
            return None;
        }
    };

    // Deterministic rotation so the correct option is not always #1.
    // M9a5: models put the correct option FIRST almost always, and the
    // old rotation (attempts % len) degenerated to no-rotation on
    // first-try passes (attempts=0) — 实测正确项常年 1 号. Rotate by a
    // stable hash of (title, attempts): the same exercise+attempt keeps
    // one order, different exercises/attempts spread evenly.
    let off = quiz_rotation_offset(&input.title, input.attempts, quiz.options.len());
    let options: Vec<review::QuizOption> = {
        let mut v = quiz.options.clone();
        v.rotate_left(off);
        v
    };

    println!();
    println!("  {}", render::bold(&paint_line(&quiz.question)));
    for (i, o) in options.iter().enumerate() {
        println!("    {}. {}", i + 1, paint_line(&o.text));
    }
    println!("  输入数字选择；或直接输入你的理解；回车跳过。");

    loop {
        let ans = ask("复盘> ")?;
        let t = ans.trim();
        if t.is_empty() {
            return None;
        }
        if let Ok(n) = t.parse::<usize>()
            && n >= 1
            && n <= options.len()
        {
            let picked = &options[n - 1];
            if picked.correct {
                println!("  {} 解释命中：{}", render::green("✓"), paint_line(&picked.explain));
                return Some(true);
            }
            let correct = options.iter().find(|o| o.correct)?;
            println!("  {} 未命中。正确理解是：{}", render::red("✗"), paint_line(&correct.text));
            println!("    {}", paint_line(&correct.explain));
            return Some(false);
        }
        // Free text → LLM judging (§4.3).
        let Some(mut judge) = make_caller(deps) else {
            println!("  （离线无法判读自由输入，请输入数字选项）");
            continue;
        };
        let answer = t.to_string();
        let quiz2 = quiz.clone();
        let judged = run_with_spinner("复盘：判定你的回答…", move |progress| {
            progress("判定回答…");
            review::judge_free_input(&mut *judge, &quiz2, &answer)
        });
        match judged {
            Some(Ok(j)) => {
                if j.hit {
                    println!("  {} 解释命中：{}", render::green("✓"), paint_line(&j.why));
                } else {
                    println!("  {} 未命中：{}", render::red("✗"), paint_line(&j.why));
                    if let Some(expl) = quiz.correct_explanation() {
                        println!("    正确理解：{}", paint_line(expl));
                    }
                }
                return Some(j.hit);
            }
            Some(Err(e)) => {
                println!("  （判定失败：{e:#}；可输入数字选项重试）");
            }
            None => return None, // interrupted
        }
    }
}

// ---------------------------------------------------------------------------
// Debrief Step 2: better-solution challenge (§4.3)
// ---------------------------------------------------------------------------

/// Step 2, offered when the verdict is not clean: directional hints
/// only (finding messages — `better_way` stays for the comparison),
/// then `[r]` re-run + re-review, `[e]` edit, `[s]` show the reference.
/// Returns the re-run gate outcome + updated code when the challenge
/// produced a fresh review.
fn step2_challenge(
    deps: &DebriefDeps,
    index: &mut ExerciseIndex,
    key: &str,
    ex: &Exercise,
    input: &mut review::ReviewInput,
    outcome: &review::GateOutcome,
) -> Option<(review::GateOutcome, String)> {
    // Direction hints: LLM finding messages; fall back to constraint
    // violations when the LLM layer was unavailable.
    let hints: Vec<String> = match &outcome.llm {
        Some(r) if !r.findings.is_empty() => {
            r.findings.iter().map(|f| f.message.clone()).collect()
        }
        _ => outcome
            .statics
            .violations
            .iter()
            .map(|v| v.message.clone())
            .collect(),
    };
    if hints.is_empty() {
        return None;
    }

    println!();
    println!("{}", render::header("复盘 · 更优解挑战"));
    println!("  你的解法已通过测试，但还有更地道的方向（不给答案，只给方向）：");
    for (i, h) in hints.iter().take(3).enumerate() {
        println!("    {}. {h}", i + 1);
    }

    loop {
        println!();
        println!("  [r] 改好了，重跑并重新评审   [e] 编辑   [s] 看参考解   [Enter] 跳过挑战，继续复盘");
        let ans = ask("挑战> ")?;
        match ans.trim() {
            "" | "b" | "q" => return None,
            "e" => {
                crate::exercise::open_editor(&ex.path, deps.editor);
            }
            "s" => match &input.reference {
                Some(r) => {
                    println!();
                    println!("  参考解：");
                    for line in r.trim().lines().take(40) {
                        println!("    {line}");
                    }
                }
                None => println!("  （本题没有持久化的参考解）"),
            },
            other if other.starts_with('/') => {
                println!("  复盘页内不处理斜杠命令——按 Enter 跳过挑战，复盘结束后回对话再使用（/exit 同）。")
            }
            "r" => {
                let res = crate::exercise::compile_and_run(ex);
                if !res.passed {
                    println!("  {} 尚未通过{}", render::red("✗"), res.first_error.as_deref().unwrap_or(""));
                    continue;
                }
                println!("  {} 测试通过，重新评审…", render::green("✓"));
                let Ok(content) = std::fs::read_to_string(&ex.path) else {
                    println!("  （无法读取练习文件）");
                    continue;
                };
                input.user_code = content.clone();
                let caller = make_caller(deps);
                let input2 = input.clone();
                let outcome2 = run_with_spinner("评审：重新评审…", move |progress| {
                    review::run_gate(caller, input2, progress)
                })?;
                render_gate(&outcome2);
                index.set_review_verdict(key, outcome2.verdict.key());
                return Some((outcome2, content));
            }
            other => println!("  未知输入: {other}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Debrief Step 3/4: comparison data + the debrief "theatre" panel
// (machine + LLM, §4.3 定稿; M9b)
// ---------------------------------------------------------------------------

/// Step 3 data pass: measure both sides with the real toolchain and ask
/// the model for the four judged dimensions. M9b: rendering moved into
/// the step-4 theatre panel (`step4_follow_up`).
/// M9a5: the comparison WITHOUT its spinners — the prefetch runs on a
/// side thread beside the review gate, where the spinner line belongs
/// to the gate. Same data as `step3_compare`, silent.
fn run_comparison_raw(
    caller: Option<Box<dyn review::ReviewCaller + Send>>,
    input: &review::ReviewInput,
) -> Option<(review::MachineComparison, Option<review::LlmComparison>, bool)> {
    let machine =
        review::machine_metrics(&input.user_code, input.reference.as_deref(), &input.constraint_specs);
    let llm_cmp =
        caller.and_then(|mut c| review::llm_comparison(&mut *c, input, &machine).ok());
    Some((machine, llm_cmp, input.reference.is_some()))
}

fn step3_compare(
    deps: &DebriefDeps,
    input: &review::ReviewInput,
) -> Option<(review::MachineComparison, Option<review::LlmComparison>, bool)> {
    let machine = {
        let input2 = input.clone();
        run_with_spinner("对比：本地实测（编译/测试/clippy）…", move |progress| {
            progress("本地实测…");
            review::machine_metrics(
                &input2.user_code,
                input2.reference.as_deref(),
                &input2.constraint_specs,
            )
        })
    };
    let machine = machine?;

    let llm_cmp: Option<review::LlmComparison> = make_caller(deps).and_then(|mut caller| {
        let input2 = input.clone();
        let m = machine.clone();
        run_with_spinner("对比：LLM 四维评审（可能较慢）…", move |progress| {
            progress("四维评审…");
            review::llm_comparison(&mut *caller, &input2, &m)
        })
        .and_then(|r| r.ok())
    });

    Some((machine, llm_cmp, input.reference.is_some()))
}

/// Machine-measured rows of the comparison table.
fn machine_rows(m: &review::MachineComparison, has_ref: bool) -> Vec<String> {
    let ref_cell = |v: String| if has_ref { v } else { "—".to_string() };
    let ok = |b: bool| if b { render::green("✓").to_string() } else { render::red("✗").to_string() };
    let dim_w = 10usize;
    let row = |dim: &str, user: String, reference: String| {
        let clip = |s: &str| render::truncate_display(s, 24);
        format!(
            "{}  {}  {}",
            render::pad_display(dim, dim_w),
            render::pad_display(&clip(&user), 26),
            render::pad_display(&clip(&reference), 26)
        )
    };

    let mut rows = vec![format!(
        "{}  {}  {}",
        render::pad_display("维度", dim_w),
        render::pad_display("用户解", 26),
        render::pad_display("参考解", 26)
    )];
    rows.push(render::dim(&"─".repeat(dim_w + 2 + 26 + 2 + 26)));
    rows.push(row("有效行数", m.user.effective_lines.to_string(), ref_cell(m.reference.as_ref().map(|r| r.effective_lines.to_string()).unwrap_or_default())));
    let kinds = if m.user_clippy_kinds.is_empty() {
        "0 条".to_string()
    } else {
        let list: Vec<String> = m
            .user_clippy_kinds
            .iter()
            .take(3)
            .map(|(k, n)| format!("{} ×{}", k.trim_start_matches("clippy::"), n))
            .collect();
        format!("{}（{}）", m.user.clippy_count, list.join("、"))
    };
    rows.push(row("clippy", kinds, ref_cell(m.reference.as_ref().map(|r| format!("{} 条", r.clippy_count)).unwrap_or_default())));
    rows.push(row("编译耗时", format!("{}ms", m.user.compile_ms), ref_cell(m.reference.as_ref().map(|r| format!("{}ms", r.compile_ms)).unwrap_or_default())));
    rows.push(row("测试耗时", format!("{}ms", m.user.test_ms), ref_cell(m.reference.as_ref().map(|r| format!("{}ms", r.test_ms)).unwrap_or_default())));
    rows.push(row("约束满足", ok(m.user.constraints_ok), ref_cell(m.reference.as_ref().map(|r| ok(r.constraints_ok)).unwrap_or_default())));
    rows
}

/// LLM four-dimension rows: list blocks (the notes carry the substance
/// — 短评 + 具体改法 — and must not be squeezed into cells).
fn llm_rows(llm: Option<&review::LlmComparison>, has_ref: bool) -> Vec<String> {
    let Some(c) = llm else {
        return vec!["（LLM 维度评审不可用——未配置 Key 或输出解析失败）".into()];
    };
    if c.rows.is_empty() {
        return vec!["（LLM 维度评审不可用——未配置 Key 或输出解析失败）".into()];
    }
    let mut rows = Vec::new();
    for r in &c.rows {
        let score = |s: Option<u8>| s.map(|n| format!("{n}/5")).unwrap_or_else(|| "?".into());
        rows.push(format!("{}（用户 {} ｜ 参考 {}）", render::bold(dim_cn(&r.dim)), score(r.user_score), score(r.ref_score)));
        if !r.user_note.is_empty() {
            rows.push(format!("  用户：{}", paint_line(&r.user_note)));
        }
        if has_ref && !r.ref_note.is_empty() {
            rows.push(format!("  参考：{}", paint_line(&r.ref_note)));
        }
    }
    if !c.takeaway.is_empty() {
        rows.push(format!("点评：{}", paint_line(&c.takeaway)));
    }
    rows
}

fn dim_cn(dim: &str) -> &str {
    match dim {
        "idiom" => "惯用性",
        "readability" => "可读性",
        "maintenance" => "可维护性",
        "design" => "设计习惯",
        other => other,
    }
}

// ---------------------------------------------------------------------------
// Debrief Step 4: follow-up decision + handback (§4.3)
// ---------------------------------------------------------------------------

/// Step 4: deterministic follow-up decision, then the tail menu. The
/// three exits map one-to-one onto the learner's decisions (0909 反馈):
/// [Enter]/[q] back to chat with nothing sent, [n] stays in practice,
/// [g] auto-sends the next-exercise request.
#[allow(clippy::too_many_arguments)]
fn step4_follow_up(
    deps: &DebriefDeps,
    meta: &ExerciseMeta,
    outcome: &review::GateOutcome,
    explanation_hit: Option<bool>,
    last_fail: Option<&str>,
    follow_up: review::FollowUp,
    cmp: Option<(review::MachineComparison, Option<review::LlmComparison>, bool)>,
    attempts_before: u32,
    used_hints: bool,
) -> DebriefExit {
    println!();
    // ── M9b: the debrief theatre — verdict, comparison and the next
    // step in ONE framed panel (fixed content, single screen).
    let explanation_cell = match explanation_hit {
        Some(true) => render::green("✓ 命中"),
        Some(false) => render::red("✗ 未命中（已补充讲解）"),
        None => "—（一次通过，无失败快照）".to_string(),
    };
    let hints_cell =
        if used_hints { render::yellow("用了分级提示").to_string() } else { "未使用".to_string() };
    let mut summary = vec![
        (
            "最终判定".to_string(),
            render::bold(&format!(
                "{}（模型单次评审，可能有波动）",
                outcome.verdict.label_cn()
            )),
        ),
        ("解释校核".to_string(), explanation_cell),
        ("分级提示".to_string(), hints_cell),
        (
            "尝试次数".to_string(),
            format!("{} 次（本次复盘前）", attempts_before),
        ),
    ];
    if outcome.statics.has_constraint_violations() {
        summary.push((
            "约束违例".to_string(),
            render::yellow(&format!("⚠ {} 处——建议按改进项修改", outcome.statics.violations.len())),
        ));
    }
    let mut sections: Vec<render::PanelSection> = Vec::new();
    if let Some((machine, llm, has_ref)) = &cmp {
        sections.push(render::PanelSection {
            title: "机器实测（用户解 ｜ 参考解）".into(),
            lines: machine_rows(machine, *has_ref),
        });
        sections.push(render::PanelSection {
            title: "LLM 四维评审".into(),
            lines: llm_rows(llm.as_ref(), *has_ref),
        });
    }
    sections.push(render::PanelSection {
        title: "下一步建议".into(),
        lines: vec![follow_up.label_cn()],
    });
    for row in render::panel(&format!("复盘剧场 · 《{}》", meta.title), &summary, &sections) {
        println!("{row}");
    }
    println!("  [Enter] 回到对话（不附加消息）   [n] 留在做题页");
    println!("          [g] 让教练推荐下一题（自动发一条消息）   [q] 同 [Enter]");

    let ans = match read_line("复盘> ") {
        Line::Text(s) => s.trim().to_ascii_lowercase(),
        Line::Interrupted | Line::Eof => "n".to_string(),
    };
    match ans.as_str() {
        "" | "q" => DebriefExit::Chat,
        "g" | "y" | "yes" => {
            DebriefExit::AskNext(build_ask_next(deps, meta, outcome, explanation_hit, last_fail, &follow_up))
        }
        _ => DebriefExit::Stay,
    }
}

/// The auto-sent coach request for the debrief tail's [g] (the old
/// [Enter] behavior).
fn build_ask_next(
    deps: &DebriefDeps,
    meta: &ExerciseMeta,
    outcome: &review::GateOutcome,
    explanation_hit: Option<bool>,
    last_fail: Option<&str>,
    follow_up: &review::FollowUp,
) -> String {
    let verdict_cn = outcome.verdict.label_cn();
    let check = match explanation_hit {
        Some(true) => "解释校核：命中",
        Some(false) => "解释校核：未命中（已给出正确理解）",
        None => "解释校核：跳过",
    };
    let hint_note = if deps.client.is_some() {
        String::new()
    } else {
        "（离线复盘：无 LLM 评审）".to_string()
    };
    let fail_note =
        last_fail.map(|c| format!("\n我之前失败时的报错：{c}")).unwrap_or_default();
    let ask = match follow_up {
        review::FollowUp::NextConcept => {
            "请结合我的错误画像，推荐并生成下一个概念的新练习（调用 generate_exercise）。".to_string()
        }
        review::FollowUp::Variant { extra_constraint: true } => format!(
            "请为概念「{}」生成一道变式练习，并附加一个更严的约束（调用 generate_exercise，\
             这是系统根据评审判定给出的建议）。",
            meta.concepts.join("、")
        ),
        review::FollowUp::Variant { extra_constraint: false } => format!(
            "请为概念「{}」生成一道变式练习（同概念换场景/换值，调用 generate_exercise）。",
            meta.concepts.join("、")
        ),
        review::FollowUp::EasierVariant => format!(
            "请为概念「{}」生成一道更简单的变式练习（调用 generate_exercise），\
             并帮我回看之前的报错理解薄弱点。",
            meta.concepts.join("、")
        ),
    };
    format!(
        "我刚完成练习《{}》的复盘。\n· 评审判定：{verdict_cn}\n· {check}{hint_note}{fail_note}\n· 系统判断：{}\n\n{ask}\n（如果你不想继续做题，直接告诉我即可。）",
        meta.title,
        follow_up.label_cn(),
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_from_file_strips_title_and_keeps_instruction() {
        let content = "// 所有权移动题\n//\n// 场景：把值交给函数\nstruct S;\nfn f() { todo!() }\n\n#[cfg(test)]\nmod tests {}\n";
        let body = body_from_file(content);
        assert!(body.contains("场景"), "{body}");
        assert!(body.contains("struct S"));
        assert!(!body.starts_with("所有权"), "title line removed");
        assert!(!body.contains("#[cfg(test)]"));
    }

    #[test]
    fn build_input_prefers_persisted_reference_and_constraints() {
        let dir = std::env::temp_dir().join(format!(
            "rs_debrief_{}_{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(dir.join("templates")).unwrap();
        std::fs::write(
            dir.join("templates").join("t.toml"),
            r##"
id = "t"
title = "T"
concepts = ["c.one"]
error_codes = ["E0308"]
difficulty = "easy"
confusion = "C 的值语义"
anti_patterns = ["clone 逃逸"]
constraints = ["no-clone"]

body = '''
// 场景：实现一个函数
// 目标：按题目要求补全
// 约束：见题面
// 说明：不要修改测试
fn f(x: u32) -> u32 {
    todo!()
}
// 补充说明一
// 补充说明二
// 补充说明三
// 补充说明四
'''

tests = '''
#[cfg(test)]
mod t {
    #[test]
    fn smoke() {
        assert_eq!(1, 1);
    }
}
'''

reference = '''
fn f(x: u32) -> u32 {
    x
}
'''
"##,
        )
        .unwrap();

        let meta = ExerciseMeta {
            path: "generated/x.rs".into(),
            title: "题".into(),
            concepts: vec!["c.one".into()],
            error_codes: vec![],
            difficulty: Some("easy".into()),
            source: Source::TemplateFill { template_id: "t".into() },
            session_id: None,
            trigger: None,
            created_at: None,
            attempts: 1,
            status: crate::exercise::index::Status::Passed,
            last_error: None,
            last_fail_error: None,
            hints: Vec::new(),
            feedback: None,
            slots: Default::default(),
            reference: Some("fn exact() {}".into()),
            body: None,
            constraints: vec!["max-lines=20".into()],
            review_verdict: None,
        };
        let input = build_input(&dir, &meta, "fn f() {}", 0);
        assert_eq!(input.reference.as_deref(), Some("fn exact() {}"), "persisted wins");
        assert_eq!(input.constraint_specs, vec!["max-lines=20"], "persisted wins");
        assert!(input.body.contains("场景"), "rendered template body");
        assert_eq!(input.anti_patterns, vec!["clone 逃逸".to_string()]);
        assert_eq!(input.confusion.as_deref(), Some("C 的值语义"));

        // Unknown source → file-prefix body, no template metadata.
        let mut meta2 = meta.clone();
        meta2.source = Source::Unknown;
        meta2.reference = None;
        let input2 = build_input(&dir, &meta2, "// 标题\n// 指令\nfn f() {}\n\n#[cfg(test)]\nmod t {}", 0);
        assert_eq!(input2.reference, None);
        assert!(input2.anti_patterns.is_empty());
        assert!(input2.body.contains("指令"), "{:?}", input2.body);

        // M9a5: a persisted PRISTINE body wins over the template render —
        // the adapted (tier-2) case where the base template's body is the
        // WRONG scenario (实测: 改编题的校核题按模板生成).
        let mut meta3 = meta.clone();
        meta3.source = Source::Adapted { base: "t".into() };
        meta3.body = Some("// 真实改编题面\nfn adapted() {}".into());
        let input3 = build_input(&dir, &meta3, "fn f() {}", 0);
        assert!(input3.body.contains("真实改编题面"), "{:?}", input3.body);
        assert!(!input3.body.contains("场景"), "template body must NOT win: {:?}", input3.body);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// M9a5: the correct option used to sit at #1 almost always — the
    /// old rotation was `attempts % len`, i.e. ZERO on every first-try
    /// pass. The hash-based offset must stay stable per (title,
    /// attempts) yet spread across exercises.
    #[test]
    fn quiz_rotation_spreads_by_title_and_attempts() {
        // Same input → same offset (stable across renders/runs).
        assert_eq!(quiz_rotation_offset("题A", 0, 4), quiz_rotation_offset("题A", 0, 4));
        // Single-option quizzes never rotate.
        assert_eq!(quiz_rotation_offset("题A", 0, 1), 0);
        // Different exercises spread where the old logic was stuck at 0.
        let offs: std::collections::BTreeSet<usize> = ["题A", "题B", "题C", "题D", "题E", "题F", "题G", "题H"]
            .iter()
            .map(|t| quiz_rotation_offset(t, 0, 4))
            .collect();
        assert!(offs.len() >= 3, "offsets must spread, got {offs:?}");
    }
}
