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

/// What the review gate / debrief needs from the REPL (built fresh at
/// each entry so a `/model` switch takes effect).
pub(crate) struct DebriefDeps<'a> {
    pub client: Option<&'a LlmClient>,
    pub cfg: &'a ModelConfig,
    pub tracker: Arc<Mutex<UsageTracker>>,
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

/// Run the review gate after a first-time pass, render the verdict and
/// persist it on the index entry. Returns a follow-up coach message
/// when the debrief decided to hand back to the conversation (M5.4).
pub(crate) fn after_pass(
    deps: &DebriefDeps,
    index: &mut ExerciseIndex,
    key: &str,
    meta: &ExerciseMeta,
    ex: &Exercise,
    repo_root: &Path,
    last_fail: Option<&str>,
) -> Option<String> {
    let Ok(content) = std::fs::read_to_string(&ex.path) else {
        println!("  （无法读取练习文件，跳过评审门）");
        return None;
    };
    let attempts_before = meta.attempts.saturating_sub(1);
    let input = build_input(repo_root, meta, &content, attempts_before);
    let _ = last_fail; // consumed by the debrief steps (M5.3)

    let caller: Option<Box<dyn ReviewCaller + Send>> = deps.client.as_ref().map(|c| {
        Box::new(CliReviewCaller {
            caller: Some(Arc::new((*c).clone())),
            model: deps.cfg.model.clone(),
            price_in: deps.cfg.prices.input,
            price_out: deps.cfg.prices.output,
            budget: deps.cfg.budget_usd(),
            tracker: deps.tracker.clone(),
        }) as Box<dyn ReviewCaller + Send>
    });

    println!();
    println!("{}", render::cyan("── 解答评审门 ──"));

    let (sp, slot) = Spinner::start("评审：准备…");
    let slot2 = slot.clone();
    let handle = std::thread::spawn(move || {
        review::run_gate(caller, input, &move |s: &str| {
            if let Ok(mut g) = slot2.lock() {
                *g = s.to_string();
            }
        })
    });
    let outcome = loop {
        if handle.is_finished() {
            break handle.join().ok();
        }
        if crate::agent::is_interrupted() {
            sp.stop();
            println!("  （评审已打断；LLM 调用将在后台结束并照常记账）");
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(80));
    };
    sp.stop();
    let Some(outcome) = outcome else {
        println!("  （评审线程异常结束，已跳过）");
        return None;
    };

    render_gate(&outcome);
    index.set_review_verdict(key, outcome.verdict.key());

    // The debrief steps hook in here (M5.3/M5.4).
    None
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
