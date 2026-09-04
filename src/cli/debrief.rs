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

    // 题面: the exact rendered template body when possible (recorded
    // slot values first), else the file's instruction prefix.
    let body = tpl
        .and_then(|t| {
            let values =
                if meta.slots.is_empty() { template::fill_for_attempt(t, 0) } else { meta.slots.clone() };
            template::render(t, &values).ok().map(|r| r.body)
        })
        .unwrap_or_else(|| body_from_file(user_code));

    review::ReviewInput {
        title: meta.title.clone(),
        concepts: meta.concepts.clone(),
        body,
        anti_patterns: tpl.map(|t| t.anti_patterns.clone()).unwrap_or_default(),
        review_hints: tpl.map(|t| review::ReviewHintsSeed {
            root_cause: t.review_hints.as_ref().map(|h| h.root_cause.clone()).unwrap_or_default(),
            misconceptions: t.review_hints.as_ref().map(|h| h.misconceptions.clone()).unwrap_or_default(),
        }),
        confusion: tpl.map(|t| t.confusion.clone()),
        reference,
        user_code: user_code.to_string(),
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
/// Returns a follow-up coach message when the debrief decided to hand
/// back to the conversation (M5.4).
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
) -> Option<String> {
    let Ok(content) = std::fs::read_to_string(&ex.path) else {
        println!("  （无法读取练习文件，跳过评审门）");
        return None;
    };
    let attempts_before = meta.attempts.saturating_sub(1);
    let mut input = build_input(repo_root, meta, &content, attempts_before);

    println!();
    println!("{}", render::cyan("── 解答评审门 ──"));

    let caller = make_caller(deps);
    let input2 = input.clone();
    let outcome = run_with_spinner("评审：准备…", |progress| {
        review::run_gate(caller, input2, progress)
    })?;
    render_gate(&outcome);
    index.set_review_verdict(key, outcome.verdict.key());

    // ── Debrief (§4.3) ──

    // Step 1: explanation check (understanding quiz).
    let explanation_hit = step1_explanation_check(deps, &input, last_fail);

    // Step 2: better-solution challenge (triggered when not clean).
    let mut outcome = outcome;
    if outcome.verdict != review::Verdict::Clean
        && let Some((updated, code)) = step2_challenge(deps, index, key, ex, &mut input, &outcome)
    {
        outcome = updated;
        input.user_code = code;
    }

    // Step 3: two-dimensional comparison (machine + LLM).
    step3_comparison(deps, &input);

    // Step 4: follow-up decision (deterministic) + optional handback.
    let follow_up = review::decide_follow_up(&review::FollowUpInput {
        attempts_before,
        used_hints,
        explanation_hit,
        verdict: outcome.verdict,
        had_violations: outcome.statics.has_constraint_violations(),
    });
    step4_follow_up(deps, meta, &outcome, explanation_hit, last_fail, follow_up)
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
            println!("    · {0}", r.summary);
        }
        for f in &r.findings {
            println!("    · [{}] {}", severity_kind_cn(f), f.message);
            if let Some(w) = &f.better_way {
                println!("      更优：{w}");
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
    match read_line(prompt) {
        Line::Text(s) => Some(s),
        Line::Interrupted => {
            println!("  ^C 已取消本行输入");
            None
        }
        Line::Eof => None,
    }
}

/// Step 1: understanding quiz (LLM-generated options anchored on the
/// template's root-cause/misconception seeds; free text judged by the
/// model). Returns Some(hit/miss); None = skipped.
fn step1_explanation_check(
    deps: &DebriefDeps,
    input: &review::ReviewInput,
    last_fail: Option<&str>,
) -> Option<bool> {
    let Some(mut caller) = make_caller(deps) else {
        println!();
        println!("  复盘（离线）：跳过理解校核。");
        return None;
    };
    println!();
    println!("{}", render::cyan("── 复盘 · 理解校核 ──"));

    let input2 = input.clone();
    let lf = last_fail.map(str::to_string);
    let quiz = run_with_spinner("复盘：生成理解校核题…", move |progress| {
        progress("生成校核题…");
        review::llm_quiz(&mut *caller, &input2, lf.as_deref())
    });
    let quiz = match quiz {
        Some(Ok(q)) => q,
        Some(Err(_)) | None => {
            println!("  （未能生成校核题，跳过本步）");
            return None;
        }
    };

    // Deterministic rotation so the correct option is not always #1.
    let off = (input.attempts as usize) % quiz.options.len();
    let options: Vec<review::QuizOption> = {
        let mut v = quiz.options.clone();
        v.rotate_left(off);
        v
    };

    println!();
    println!("  {}", render::bold(&quiz.question));
    for (i, o) in options.iter().enumerate() {
        println!("    {}. {}", i + 1, o.text);
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
                println!("  {} 解释命中：{}", render::green("✓"), picked.explain);
                return Some(true);
            }
            let correct = options.iter().find(|o| o.correct)?;
            println!("  {} 未命中。正确理解是：{}", render::red("✗"), correct.text);
            println!("    {}", correct.explain);
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
                    println!("  {} 解释命中：{}", render::green("✓"), j.why);
                } else {
                    println!("  {} 未命中：{}", render::red("✗"), j.why);
                    if let Some(expl) = quiz.correct_explanation() {
                        println!("    正确理解：{expl}");
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
    println!("{}", render::cyan("── 复盘 · 更优解挑战 ──"));
    println!("  你的解法已通过测试，但还有更地道的方向（不给答案，只给方向）：");
    for (i, h) in hints.iter().take(3).enumerate() {
        println!("    {}. {h}", i + 1);
    }

    loop {
        println!();
        println!("  [r] 改好了，重跑并重新评审   [e] 编辑   [s] 看参考解   [Enter] 结束复盘");
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
// Debrief Step 3: comparison table (machine + LLM, §4.3 定稿)
// ---------------------------------------------------------------------------

/// Step 3: measure both sides with the real toolchain, then ask the
/// model for the four judged dimensions. Offline → machine-only table.
fn step3_comparison(deps: &DebriefDeps, input: &review::ReviewInput) {
    println!();
    println!("{}", render::cyan("── 复盘 · 对比总结 ──"));

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
    let Some(machine) = machine else { return };

    let llm_cmp: Option<review::LlmComparison> = make_caller(deps).and_then(|mut caller| {
        let input2 = input.clone();
        let m = machine.clone();
        run_with_spinner("对比：LLM 四维评审…", move |progress| {
            progress("四维评审…");
            review::llm_comparison(&mut *caller, &input2, &m)
        })
        .and_then(|r| r.ok())
    });

    render_comparison(&machine, llm_cmp.as_ref(), input.reference.is_some());
}

fn render_comparison(m: &review::MachineComparison, llm: Option<&review::LlmComparison>, has_ref: bool) {
    let ref_cell = |v: String| if has_ref { v } else { "—".to_string() };
    let ok = |b: bool| if b { render::green("✓").to_string() } else { render::red("✗").to_string() };
    let dim_w = 10usize;

    let row = |dim: &str, user: String, reference: String| {
        let clip = |s: &str| render::truncate_display(s, 24);
        println!(
            "    {}  {}  {}",
            render::pad_display(dim, dim_w),
            render::pad_display(&clip(&user), 26),
            render::pad_display(&clip(&reference), 26)
        );
    };

    println!("    {}  {}  {}", render::pad_display("维度", dim_w), render::pad_display("用户解", 26), render::pad_display("参考解", 26));
    row("有效行数", m.user.effective_lines.to_string(), ref_cell(m.reference.as_ref().map(|r| r.effective_lines.to_string()).unwrap_or_default()));
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
    row("clippy", kinds, ref_cell(m.reference.as_ref().map(|r| format!("{} 条", r.clippy_count)).unwrap_or_default()));
    row("编译耗时", format!("{}ms", m.user.compile_ms), ref_cell(m.reference.as_ref().map(|r| format!("{}ms", r.compile_ms)).unwrap_or_default()));
    row("测试耗时", format!("{}ms", m.user.test_ms), ref_cell(m.reference.as_ref().map(|r| format!("{}ms", r.test_ms)).unwrap_or_default()));
    row("约束满足", ok(m.user.constraints_ok), ref_cell(m.reference.as_ref().map(|r| ok(r.constraints_ok)).unwrap_or_default()));

    match llm {
        Some(c) if !c.rows.is_empty() => {
            // List blocks instead of table cells: the notes carry the
            // substance (短评 + 具体改法) and must not be squeezed.
            for r in &c.rows {
                let score = |s: Option<u8>| s.map(|n| format!("{n}/5")).unwrap_or_else(|| "?".into());
                println!("    {}（用户 {} ｜ 参考 {}）", render::bold(dim_cn(&r.dim)), score(r.user_score), score(r.ref_score));
                if !r.user_note.is_empty() {
                    println!("      用户：{}", r.user_note);
                }
                if has_ref && !r.ref_note.is_empty() {
                    println!("      参考：{}", r.ref_note);
                }
            }
            if !c.takeaway.is_empty() {
                println!("    点评：{}", c.takeaway);
            }
        }
        _ => println!("    （LLM 维度评审不可用——未配置 Key 或输出解析失败）"),
    }
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

/// Step 4: deterministic follow-up decision, then offer to hand the
/// result back to the conversation (the coach then arranges the next
/// exercise through the history-aware generator). Some(msg) = handback.
fn step4_follow_up(
    deps: &DebriefDeps,
    meta: &ExerciseMeta,
    outcome: &review::GateOutcome,
    explanation_hit: Option<bool>,
    last_fail: Option<&str>,
    follow_up: review::FollowUp,
) -> Option<String> {
    println!();
    println!("{}", render::cyan("── 复盘 · 下一步 ──"));
    println!("  {}", follow_up.label_cn());
    println!("  [Enter] 回到对话让教练安排下一题   [n] 留在做题页   [q] 退出做题");

    let ans = match read_line("复盘> ") {
        Line::Text(s) => s.trim().to_ascii_lowercase(),
        Line::Interrupted | Line::Eof => "n".to_string(),
    };
    match ans.as_str() {
        "" | "y" | "yes" => {
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
            let fail_note = last_fail
                .map(|c| format!("\n我之前失败时的报错：{c}"))
                .unwrap_or_default();
            let ask = match &follow_up {
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
            Some(format!(
                "我刚完成练习《{}》的复盘。\n· 评审判定：{verdict_cn}\n· {check}{hint_note}{fail_note}\n· 系统建议：{}\n\n{ask}",
                meta.title,
                follow_up.label_cn(),
            ))
        }
        _ => None,
    }
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
            hints: Vec::new(),
            feedback: None,
            slots: Default::default(),
            reference: Some("fn exact() {}".into()),
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
        let _ = std::fs::remove_dir_all(&dir);
    }
}
