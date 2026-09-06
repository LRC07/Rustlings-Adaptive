//! Agent tool registry & executors (M4). Three tools, all backed by
//! local execution (design principle "真实执行为据"):
//!
//! - `list_concepts`     — the taxonomy concept ids the generator accepts
//! - `generate_exercise` — the M3 generator pipeline (template pick →
//!   slot fill → triple verify), producing a practiceable exercise
//! - `check_code`        — compile a user snippet with rustc and return
//!   the real JSON diagnostics
//!
//! The registry is the schema list plus a dispatch in `execute()`; the
//! agent loop (`super::run_turn`) feeds model-requested calls through
//! here and reports results back to the model.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

use super::{is_interrupted, AgentEnv, PracticeOffer};
use crate::generator::{self, Paths, Topic};
use crate::llm::{ChatMessage, LlmReply, Tool, TurnOutput};
use crate::taxonomy::ConceptGraph;
use crate::usage;
use crate::verifier;

/// Compile timeout for `check_code` (protects the agent loop from
/// pathological snippets; the M4.5 verifier hardening is separate).
const CHECK_TIMEOUT: Duration = Duration::from_secs(30);
/// Max diagnostics reported back to the model (keeps context small).
const MAX_DIAGNOSTICS: usize = 8;
/// Max characters per diagnostic message.
const MAX_DIAG_CHARS: usize = 300;

// ---------------------------------------------------------------------------
// Tool registry
// ---------------------------------------------------------------------------

pub const TOOL_LIST_CONCEPTS: &str = "list_concepts";
pub const TOOL_GENERATE_EXERCISE: &str = "generate_exercise";
pub const TOOL_CHECK_CODE: &str = "check_code";
pub const TOOL_LEARNER_PROFILE: &str = "learner_profile";
pub const TOOL_BORROWLAB: &str = "borrowlab";

/// Schemas offered to the model (OpenAI function format).
pub fn tool_schemas() -> Vec<Tool> {
    vec![
        Tool {
            name: TOOL_LIST_CONCEPTS.into(),
            description: "列出本地概念图谱的全部概念 id（出题主题的权威来源）".into(),
            parameters: json!({"type": "object", "properties": {}}),
        },
        Tool {
            name: TOOL_GENERATE_EXERCISE.into(),
            description: "生成一道 10-40 行的 Rust 填空练习（本地三重校验：能编译/参考解全绿/未完成模板必失败）\
                          并写回练习目录，随后用户可立即开始做题"
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "topic": {
                        "type": "string",
                        "description": "题目主题：概念 id（用 list_concepts 查询，如 borrow.move-semantics）、\
                                        rustc 错误码（如 E0382）或自由文本关键词"
                    },
                    "focus": {
                        "type": "string",
                        "description": "要训练的具体手法/行为（可选，但用户点名具体技法时必填）：\
                                        如「entry API 的 or_insert/and_modify 单次查找」、\
                                        「unwrap_or_else 的惰性求值」。系统会保证题目正面训练它"
                    },
                    "reason": {
                        "type": "string",
                        "description": "一句话说明为什么现在出这道题（结合对话语境，如「你贴的代码报 E0382」）；\
                                        会展示给用户并随题归档"
                    },
                    "mode": {
                        "type": "string",
                        "enum": ["auto", "free"],
                        "description": "auto（默认）=分层出题：模板直配→改编→自由生成逐级回退；\
                                        free=跳过模板直接自由生成。仅当用户明确要求『不用模板/自由生成』时才用 free"
                    }
                },
                "required": ["topic"]
            }),
        },
        Tool {
            name: TOOL_CHECK_CODE.into(),
            description: "用本地 rustc 编译一段 Rust 代码并返回真实诊断（错误码/消息/行号）。\
                          解释用户贴的报错或代码前，先用它获得编译器证据"
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "code": {"type": "string", "description": "完整的 Rust 源码（需含 fn main 或 #[test]）"}
                },
                "required": ["code"]
            }),
        },
        Tool {
            name: TOOL_LEARNER_PROFILE.into(),
            description: "查询学习者的本地学习画像：最薄弱概念（按失败次数）、SM-2 到期复习的概念、\
                          高频错误码、错题本中最常失败的练习。当用户问「我哪里薄弱/该复习什么」或你想\
                          根据历史选题时调用它"
                .into(),
            parameters: json!({"type": "object", "properties": {}}),
        },
        Tool {
            name: TOOL_BORROWLAB.into(),
            description: "假设实验室：把学习者代码的假设改写同时交给本地 rustc 编译，返回两个版本错误码的\
                          增减 diff（新出现的错误 / 消除的错误）。用于回答「如果我改成 X 会怎样」「为什么\
                          这里必须借用」这类假设性问题——让借用检查器亲自给出证据，不要凭记忆臆断。\
                          hypothesis 必须是改写后的完整可编译源码（其他部分保持不变）"
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "code": {"type": "string", "description": "学习者当前的完整源码"},
                    "hypothesis": {"type": "string", "description": "假设改动后的完整源码（只改假设的部分）"}
                },
                "required": ["code", "hypothesis"]
            }),
        },
    ]
}

/// Aggregated usage of one tool execution (its internal LLM calls).
#[derive(Debug, Clone, Copy, Default)]
pub struct UsageAcc {
    pub calls: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
    pub cost_usd: f64,
}

impl UsageAcc {
    pub(crate) fn add(&mut self, input: u64, output: u64, reasoning: u64, cost: f64) {
        self.calls += 1;
        self.input_tokens += input;
        self.output_tokens += output;
        self.reasoning_tokens += reasoning;
        self.cost_usd += cost;
    }
}

/// Result of one tool execution: the JSON handed back to the model,
/// plus side channels for the CLI (human trace line, practice offer,
/// usage of internal LLM calls).
#[derive(Debug)]
pub struct ToolOutcome {
    pub value: Value,
    /// One-line human-readable trace (shown under the reply live).
    pub note: Option<String>,
    /// Set when an exercise was generated and the user may practice it.
    pub practice: Option<PracticeOffer>,
    pub usage: UsageAcc,
}

impl ToolOutcome {
    fn plain(value: Value) -> Self {
        Self { value, note: None, practice: None, usage: UsageAcc::default() }
    }
}

/// Dispatch a model-requested tool call. Unknown tools are an error
/// (the agent loop converts it into an error JSON for the model).
pub fn execute(name: &str, arguments: &str, env: &AgentEnv, progress: &dyn Fn(&str)) -> Result<ToolOutcome> {
    let args: Value = serde_json::from_str(arguments.trim()).unwrap_or_else(|_| json!({}));
    match name {
        TOOL_LIST_CONCEPTS => list_concepts(env),
        TOOL_GENERATE_EXERCISE => generate_exercise(&args, env, progress),
        TOOL_CHECK_CODE => check_code(&args, progress),
        TOOL_LEARNER_PROFILE => learner_profile(env),
        TOOL_BORROWLAB => borrowlab(&args, progress),
        other => Err(anyhow!("未知工具「{other}」；可用工具：{TOOL_LIST_CONCEPTS} / {TOOL_GENERATE_EXERCISE} / {TOOL_CHECK_CODE} / {TOOL_LEARNER_PROFILE} / {TOOL_BORROWLAB}")),
    }
}

// ---------------------------------------------------------------------------
// list_concepts
// ---------------------------------------------------------------------------

fn list_concepts(env: &AgentEnv) -> Result<ToolOutcome> {
    let graph = ConceptGraph::load(&taxonomy_path(env))?;
    let nodes: Vec<Value> = graph
        .ids()
        .filter_map(|id| graph.get(id).map(|n| json!({ "id": n.id, "name": n.name })))
        .collect();
    let count = nodes.len();
    let mut value = json!({ "concepts": nodes });
    value["note"] = json!("generate_exercise 的 topic 接受这些 id、其中文名、错误码（如 E0382）或自由文本");
    Ok(ToolOutcome {
        note: Some(format!("概念图谱共 {count} 个节点")),
        ..ToolOutcome::plain(value)
    })
}

fn taxonomy_path(env: &AgentEnv) -> PathBuf {
    env.root.join("taxonomy").join("concepts.toml")
}

// ---------------------------------------------------------------------------
// learner_profile (M6)
// ---------------------------------------------------------------------------

/// Read-only view of the learner profile: weakest concepts, SM-2 due
/// reviews, top error codes and the most-failed notebook exercises.
/// Deterministic local data — no LLM involved.
fn learner_profile(env: &AgentEnv) -> Result<ToolOutcome> {
    let store = crate::profile::ProfileStore::load_or_create();
    let profile = &store.profile;

    if profile.concepts.is_empty() && profile.error_codes.is_empty() {
        return Ok(ToolOutcome {
            value: json!({
                "empty": true,
                "note": "学习者画像还是空的（没有做题信号）；建议直接出题或请用户贴代码",
            }),
            note: Some("学习画像为空".to_string()),
            ..ToolOutcome::plain(json!({}))
        });
    }

    let graph = ConceptGraph::load(&taxonomy_path(env)).ok();
    let cname = |id: &str| -> Value {
        match graph.as_ref().and_then(|g| g.get(id)) {
            Some(n) => json!({"id": n.id, "name": n.name}),
            None => json!({"id": id, "name": id}),
        }
    };

    let index = crate::exercise::index::ExerciseIndex::load(&env.root.join("exercises"));
    let notebook = crate::profile::notebook_from_index(&index, None);

    let weakest: Vec<Value> = profile
        .weakest(5)
        .into_iter()
        .map(|(c, f, a)| json!({"concept": cname(&c), "fails": f, "attempts": a}))
        .collect();
    let due: Vec<Value> = profile.due_concepts(chrono::Utc::now()).iter().map(|c| cname(c)).collect();
    let codes: Vec<Value> = profile
        .top_error_codes(5)
        .into_iter()
        .map(|(c, n)| json!({"code": c, "count": n}))
        .collect();
    let notebook_json: Vec<Value> = notebook
        .iter()
        .take(5)
        .map(|e| {
            json!({
                "title": e.title,
                "attempts": e.attempts,
                "passed": e.passed,
                "last_error": e.last_error,
                "concepts": e.concepts,
            })
        })
        .collect();

    let mut value = json!({
        "weakest_concepts": weakest,
        "sm2_due_now": due,
        "top_error_codes": codes,
        "notebook_most_failed": notebook_json,
        "note": "数据来自本地练习索引与做题信号（确定性统计）。出题建议优先结合薄弱概念与到期复习；\
                 同概念已多次失败时给更简单的变式并引用之前的报错。",
    });
    if weakest.is_empty() && codes.is_empty() {
        value["note"] = json!("画像只有少量信号；按对话语境出题即可");
    }
    Ok(ToolOutcome {
        note: Some(format!(
            "学习画像：{} 个概念有记录，{} 个到期复习",
            profile.concepts.len(),
            due.len()
        )),
        ..ToolOutcome::plain(value)
    })
}

// ---------------------------------------------------------------------------
// generate_exercise
// ---------------------------------------------------------------------------

/// Bridge from the generator's one-shot `LlmCaller` to the shared
/// `ChatTurnCaller`, accounting usage (R6) while it goes.
struct CallerBridge {
    caller: std::sync::Arc<dyn super::ChatTurnCaller>,
    input_price: f64,
    output_price: f64,
    acc: UsageAcc,
}

impl generator::LlmCaller for CallerBridge {
    fn call(&mut self, prompt: &str) -> Result<LlmReply> {
        self.call_bounded(prompt, u32::MAX)
    }

    fn call_bounded(&mut self, prompt: &str, max_tokens: u32) -> Result<LlmReply> {
        let cap = (max_tokens < u32::MAX).then_some(max_tokens);
        let out: TurnOutput =
            self.caller.chat_turn_bounded(&[ChatMessage::user(prompt.to_string())], &[], cap)?;
        let cost = usage::cost_usd(
            out.usage.prompt_tokens,
            out.usage.completion_tokens,
            self.input_price,
            self.output_price,
        );
        self.acc.add(out.usage.prompt_tokens, out.usage.completion_tokens, out.usage.reasoning_tokens, cost);
        Ok(LlmReply {
            content: out.content.unwrap_or_default(),
            usage: out.usage,
            finish_reason: out.finish_reason,
        })
    }
}

fn generate_exercise(args: &Value, env: &AgentEnv, progress: &dyn Fn(&str)) -> Result<ToolOutcome> {
    let topic_text = args
        .get("topic")
        .and_then(|t| t.as_str())
        .ok_or_else(|| anyhow!("缺少 topic 参数（概念 id / 错误码 / 关键词）"))?
        .trim()
        .to_string();
    if topic_text.is_empty() {
        return Err(anyhow!("topic 不能为空"));
    }
    let topic = Topic::from_input(&topic_text);
    // M4.16 (考察点精度): the SPECIFIC technique the learner named rides
    // in `focus` — it steers the tier-1 pick (no_match → tiers 2/3) and
    // the tier-2/3 draft prompts. The trigger stays user-facing.
    let focus = args
        .get("focus")
        .and_then(|f| f.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    // M4.14: mode is a USER-INTENT relay, not a strategy pick — auto
    // stays the default (M4.7 decision); "free" only honours an
    // explicit no-template request, skipping tiers 1/2.
    let mode = match args.get("mode").and_then(|m| m.as_str()) {
        Some("free") => {
            progress("按用户要求自由生成（跳过模板）…");
            generator::GenerateMode::Free
        }
        _ => generator::GenerateMode::Auto,
    };
    progress(&format!("生成练习（{topic_text}）：选模板…"));

    let paths = Paths::from_root(&env.root);
    // M4.10: feed past generation history into the pick so the learner
    // does not silently re-serve the same template question.
    let history = {
        let index = crate::exercise::index::ExerciseIndex::load(&env.root.join("exercises"));
        generator::GenHistory::from_index(&index)
    };
    // M9h (level 信号): learner profile steers L2/L3 drafts.
    let learner = generator::LearnerContext::from_local(&env.root.join("exercises"));
    let mut bridge = CallerBridge {
        caller: env.caller.clone(),
        input_price: env.cfg.prices.input,
        output_price: env.cfg.prices.output,
        acc: UsageAcc::default(),
    };
    let outcome = match generator::generate_full(
        &topic,
        focus.as_deref(),
        mode,
        &paths,
        &history,
        Some(&learner),
        Some(&mut bridge),
        Some(&mut |stage: generator::GenerateStage| {
            // Round number and running cost FIRST: the spinner text is
            // width-truncated and each LLM round can run for minutes —
            // the 9.6 "重试风暴" felt like a hang because neither was
            // visible. The rejection reason goes last (truncatable).
            let cost = env
                .tracker
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .all_totals()
                .cost_usd;
            let base = format!(
                "出题「{topic_text}」{} 第{}/{}轮（累计 ${cost:.4}）",
                stage.stage, stage.attempt, stage.total_attempts
            );
            match &stage.note {
                Some(n) => progress(&format!("{base}｜上一轮被拒：{n}")),
                None => progress(&base),
            }
        }),
    ) {
        Ok(o) => o,
        Err(e) => {
            // M4.7 fallback gate: a failed generation must NEVER turn
            // into a hand-written exercise by the model (no local
            // triple-verification, no rustc evidence, no archiving).
            // Return a structured instruction instead of an error so
            // the model follows the designed degradation path.
            let full = format!("{e:#}");
            // The rustc diagnostics block (up to ~1200 chars) is repair-
            // loop food, not user reading material — the visible note
            // carries the failure reason in FULL (9.6 实测: 原因被省略
            // 号吃掉), only the model-facing copy is capped.
            let reason_note = full
                .split("\n[未完成模板的 rustc 诊断]")
                .next()
                .unwrap_or(&full)
                .to_string();
            let reason = ellipsize(&full, 400);
            return Ok(ToolOutcome {
                value: json!({
                    "ok": false,
                    "error": reason,
                    "fallback": "出题管线暂时失败。请向用户转述失败原因摘要，并建议：稍后重试、\
                                 换一个主题，或 /model 切换更快的模型。**不要自行在回复里编写练习题**\
                                 ——未经本地三重校验的题目不可靠，这不是合格的替代品。",
                }),
                note: Some(format!("出题失败：{reason_note}")),
                practice: None,
                usage: bridge.acc,
            });
        }
    };

    // M4.5a: register the exercise in the index (metadata + trigger
    // context + session attribution) so the practice board and the
    // coach's system prompt know about it.
    let trigger = args
        .get("reason")
        .and_then(|r| r.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("对话请求：{topic_text}"));
    let trigger = match &focus {
        Some(f) => format!("{trigger}｜考察：{f}"),
        None => trigger,
    };
    let exercises_dir = env.root.join("exercises");
    if let Err(e) = crate::exercise::index::register_generated(
        &exercises_dir,
        &outcome.path,
        &outcome.title,
        &outcome.concepts,
        &outcome.error_codes,
        Some(outcome.difficulty.as_str()),
        outcome.tier.to_source(),
        env.session_id.as_deref(),
        Some(&trigger),
        &outcome.hints,
        &outcome.slots,
        &outcome.reference,
        &outcome.constraints,
    ) {
        progress(&format!("index 登记失败：{e:#}"));
    }

    let mut value = json!({
        "ok": true,
        "title": outcome.title,
        "file": outcome.name,
        "path": outcome.path.display().to_string(),
        "concepts": outcome.concepts,
        "difficulty": outcome.difficulty.name_cn(),
        "attempts": outcome.attempts,
        "used_llm": outcome.used_llm,
        "variant": outcome.variant,
        "trigger": trigger,
        "note": "题目已写入练习目录并接线，用户可以立即开始做题",
    });
    if outcome.variant {
        // The coach must phrase this as a variant, not pretend it is a
        // brand-new exercise.
        value["note"] = json!(
            "这是同模板的变式（该模板此前已出过题，本次轮换了槽位/换了值）。\
             请在回复里向用户说明这一点。"
        );
    }
    Ok(ToolOutcome {
        note: Some(format!(
            "生成成功：《{}》（{}，{}，第 {} 轮{}）",
            outcome.title,
            outcome.difficulty.name_cn(),
            outcome.tier.label_cn(),
            outcome.attempts,
            if outcome.variant { "，同模板变式" } else { "" }
        )),
        practice: Some(PracticeOffer {
            title: outcome.title,
            path: outcome.path,
            difficulty: outcome.difficulty.name_cn().to_string(),
            concepts: outcome.concepts,
            trigger: Some(trigger),
        }),
        usage: bridge.acc,
        value,
    })
}

// ---------------------------------------------------------------------------
// borrowlab (M7)
// ---------------------------------------------------------------------------

/// Run the hypothesis lab: compile the learner's code and the assumed
/// rewrite side by side with the real rustc, diff the error codes.
fn borrowlab(args: &Value, progress: &dyn Fn(&str)) -> Result<ToolOutcome> {
    let code = args
        .get("code")
        .and_then(|c| c.as_str())
        .ok_or_else(|| anyhow!("缺少 code 参数（学习者当前源码）"))?;
    let hypothesis = args
        .get("hypothesis")
        .and_then(|c| c.as_str())
        .ok_or_else(|| anyhow!("缺少 hypothesis 参数（假设改动后的完整源码）"))?;
    if code.trim() == hypothesis.trim() {
        return Err(anyhow!("hypothesis 与 code 相同——假设改动必须真的改变代码"));
    }

    progress("假设实验室：双向 rustc 取证…");
    let report = crate::borrowlab::apply_and_check(code, hypothesis)?;

    let side_json = |s: &crate::borrowlab::LabSide| {
        json!({
            "compiles": s.compiles,
            "error_codes": s.codes,
            "first_error": s.first_error,
        })
    };
    let fmt = |list: &[(String, u32)]| -> Vec<Value> {
        list.iter().map(|(c, n)| json!({"code": c, "count": n})).collect()
    };

    let note = if report.hypothesis.compiles && !report.baseline.compiles {
        "假设改动后编译通过（基线失败）".to_string()
    } else if report.no_change() {
        "假设改动没有改变诊断结果".to_string()
    } else {
        let mut parts: Vec<String> = Vec::new();
        if !report.resolved_errors.is_empty() {
            let list: Vec<String> =
                report.resolved_errors.iter().map(|(c, _)| c.clone()).collect();
            parts.push(format!("消除 {}", list.join("、")));
        }
        if !report.new_errors.is_empty() {
            let list: Vec<String> = report.new_errors.iter().map(|(c, _)| c.clone()).collect();
            parts.push(format!("引入 {}", list.join("、")));
        }
        format!("假设改动{}", parts.join("，"))
    };

    let value = json!({
        "baseline": side_json(&report.baseline),
        "hypothesis": side_json(&report.hypothesis),
        "new_errors": fmt(&report.new_errors),
        "resolved_errors": fmt(&report.resolved_errors),
        "note": "以上是两次真实 rustc 编译的 diff。基于它解释「为什么」——错误码变化意味着\
                 哪条所有权/借用规则被满足了或被触犯了；如果引入了新错误，说明假设的改法\
                 触发了另一条规则，正好可以讲清楚两者的关系。",
    });

    Ok(ToolOutcome {
        note: Some(format!("假设实验室：{note}")),
        ..ToolOutcome::plain(value)
    })
}

// ---------------------------------------------------------------------------
// check_code
// ---------------------------------------------------------------------------

/// Compile a snippet with rustc (`--test` when it contains `#[test]`,
/// otherwise as a bin crate) and report the real diagnostics. Runs
/// under a timeout and honours the interrupt flag.
fn check_code(args: &Value, progress: &dyn Fn(&str)) -> Result<ToolOutcome> {
    let code = args
        .get("code")
        .and_then(|c| c.as_str())
        .ok_or_else(|| anyhow!("缺少 code 参数（完整 Rust 源码）"))?;

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("rustlings_check_{nanos}"));
    std::fs::create_dir_all(&dir)?;
    let src = dir.join("snippet.rs");
    let bin = dir.join("snippet.bin");
    std::fs::write(&src, code)?;

    progress("本地 rustc 编译中…");
    let mut cmd = Command::new("rustc");
    cmd.args(["--edition", "2024", "-A", "warnings", "--error-format=json"]);
    if code.contains("#[test]") {
        cmd.arg("--test");
    } else {
        cmd.args(["--crate-type", "bin"]);
    }
    cmd.arg(&src).arg("-o").arg(&bin);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let result = run_with_timeout(cmd);
    let _ = std::fs::remove_dir_all(&dir);
    let out = result?;

    let stderr = String::from_utf8_lossy(&out.stderr);
    let all = verifier::parse_diagnostics(&stderr);
    let diagnostics: Vec<Value> = all
        .iter()
        .take(MAX_DIAGNOSTICS)
        .map(|d| {
            json!({
                "code": d.code,
                "level": d.level,
                "message": ellipsize(&d.message, MAX_DIAG_CHARS),
                "line": d.spans.first().map(|s| s.line_start),
            })
        })
        .collect();
    let codes: Vec<String> = diagnostics
        .iter()
        .filter_map(|d| d.get("code").and_then(|c| c.as_str()).map(str::to_string))
        .collect();

    let value = json!({
        "compiles": out.status.success(),
        "diagnostic_count": all.len(),
        "diagnostics": diagnostics,
        "error_codes": codes,
        "note": "以上是本地 rustc 的真实诊断；基于它们解释，不要臆测。若片段缺 fn main 也无 #[test]，E0601 是预期现象，可提示用户补 main 或加 #[test]。",
    });
    Ok(ToolOutcome {
        note: Some(if out.status.success() {
            "本地编译通过".to_string()
        } else {
            format!("本地编译失败（{}）", if codes.is_empty() { "无错误码".to_string() } else { codes.join("、") })
        }),
        ..ToolOutcome::plain(value)
    })
}

/// Spawn a command, poll for exit, kill on timeout / interrupt.
fn run_with_timeout(mut cmd: Command) -> Result<std::process::Output> {
    use std::io::Read;
    let mut child = cmd.spawn().map_err(|e| anyhow!("调用 rustc 失败：{e}"))?;
    // Drain pipes via threads so large output cannot deadlock the poll
    // loop; join handles are collected after exit.
    let mut stderr_pipe = child.stderr.take().expect("stderr piped");
    let mut stdout_pipe = child.stdout.take().expect("stdout piped");
    let err_t = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr_pipe.read_to_end(&mut buf);
        buf
    });
    let out_t = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout_pipe.read_to_end(&mut buf);
        buf
    });

    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) => {}
            Err(e) => return Err(anyhow!("等待 rustc 失败：{e}")),
        }
        if start.elapsed() > CHECK_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            return Err(anyhow!("编译超过 {:?}，已中止（代码可能包含病态构造）", CHECK_TIMEOUT));
        }
        if is_interrupted() {
            let _ = child.kill();
            let _ = child.wait();
            return Err(anyhow!("已打断"));
        }
        std::thread::sleep(Duration::from_millis(100));
    };

    let stderr = err_t.join().unwrap_or_default();
    let stdout = out_t.join().unwrap_or_default();
    Ok(std::process::Output { status, stdout, stderr })
}

fn ellipsize(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let head: String = text.chars().take(max_chars).collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// Caller that always fails: forces the generator into offline mode.
    struct FailingCaller;
    impl super::super::ChatTurnCaller for FailingCaller {
        fn chat_turn(&self, _m: &[ChatMessage], _t: &[Tool]) -> Result<TurnOutput> {
            Err(anyhow!("no network in tests"))
        }
    }

    fn test_env(root: &std::path::Path) -> AgentEnv {
        let tracker = usage::UsageTracker::from_path(
            std::env::temp_dir().join(format!("rs_tools_usage_{}.json", std::process::id())),
        );
        AgentEnv {
            caller: Arc::new(FailingCaller),
            tracker: Arc::new(std::sync::Mutex::new(tracker)),
            cfg: crate::config::ModelConfig::default(),
            root: root.to_path_buf(),
            session_id: Some("session_test".to_string()),
            practice_note: None,
            open_loop_note: None,
        }
    }

    #[test]
    fn schemas_cover_all_tools() {
        let schemas = tool_schemas();
        let names: Vec<&str> = schemas.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                TOOL_LIST_CONCEPTS,
                TOOL_GENERATE_EXERCISE,
                TOOL_CHECK_CODE,
                TOOL_LEARNER_PROFILE,
                TOOL_BORROWLAB
            ]
        );
        for s in &schemas {
            assert!(s.parameters.is_object());
            assert!(!s.description.is_empty());
        }
    }

    #[test]
    fn unknown_tool_is_an_error() {
        let env = test_env(std::path::Path::new("."));
        let err = execute("nope", "{}", &env, &|_| {}).unwrap_err();
        assert!(err.to_string().contains("未知工具"), "{err}");
    }

    #[test]
    fn list_concepts_returns_taxonomy() {
        // Runs from the crate root: the real taxonomy is available.
        let env = test_env(std::path::Path::new("."));
        let out = execute(TOOL_LIST_CONCEPTS, "{}", &env, &|_| {}).unwrap();
        let ids: Vec<&str> = out.value["concepts"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|n| n["id"].as_str())
            .collect();
        assert!(!ids.is_empty());
        assert!(ids.iter().any(|i| i.contains('.')), "expected dotted ids, got {ids:?}");
    }

    #[test]
    fn check_code_reports_real_error_codes() {
        let env = test_env(std::path::Path::new("."));
        let args = json!({"code": "fn main() { let s = String::from(\"x\"); let t = s; println!(\"{}\", s); }"});
        let out = execute(TOOL_CHECK_CODE, &args.to_string(), &env, &|_| {}).unwrap();
        assert_eq!(out.value["compiles"], false);
        let codes: Vec<&str> = out.value["error_codes"].as_array().unwrap().iter().filter_map(|c| c.as_str()).collect();
        assert!(codes.contains(&"E0382"), "{codes:?}");
        assert!(out.note.as_deref().unwrap().contains("E0382"));
    }

    #[test]
    fn check_code_ok_snippet_and_test_mode() {
        let env = test_env(std::path::Path::new("."));
        let args = json!({"code": "fn main() { println!(\"hi\"); }"});
        let out = execute(TOOL_CHECK_CODE, &args.to_string(), &env, &|_| {}).unwrap();
        assert_eq!(out.value["compiles"], true);
        assert_eq!(out.note.as_deref(), Some("本地编译通过"));

        let args = json!({"code": "#[test]\nfn t() { assert_eq!(1+1, 2); }"});
        let out = execute(TOOL_CHECK_CODE, &args.to_string(), &env, &|_| {}).unwrap();
        assert_eq!(out.value["compiles"], true);
    }

    #[test]
    fn check_code_requires_code_arg() {
        let env = test_env(std::path::Path::new("."));
        let err = execute(TOOL_CHECK_CODE, "{}", &env, &|_| {}).unwrap_err();
        assert!(err.to_string().contains("code"), "{err}");
    }

    #[test]
    fn generate_exercise_offline_via_fixture() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("rs_tools_fx_{nanos}"));
        std::fs::create_dir_all(root.join("templates")).unwrap();
        std::fs::create_dir_all(root.join("taxonomy")).unwrap();
        std::fs::create_dir_all(root.join("exercises")).unwrap();
        std::fs::write(
            root.join("taxonomy/concepts.toml"),
            "[[concept]]\nid = \"t\"\nname = \"根\"\n\n[[concept]]\nid = \"t.c\"\nname = \"子概念\"\nparents = [\"t\"]\n",
        )
        .unwrap();
        std::fs::write(
            root.join("templates/mini.toml"),
            "id = \"mini\"\ntitle = \"迷你\"\nconcepts = [\"t.c\"]\nerror_codes = [\"E0308\"]\ndifficulty = \"easy\"\nconfusion = \"初学者以为函数传参总是复制\"\n\nbody = '''\n// 说明行。\n// 说明行二。\n// 说明行三。\n// 说明行四。\n// 说明行五。\n// 说明行六。\nfn add(a: i32, b: i32) -> i32 {\n    todo!()\n}\n// I AM NOT DONE\n'''\n\ntests = '''\n#[cfg(test)]\nmod tests {\n    use super::*;\n    #[test]\n    fn t() {\n        assert_eq!(add(1, 2), 3);\n    }\n}\n'''\n\nreference = '''\nfn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n'''\n",
        )
        .unwrap();
        std::fs::write(root.join("exercises/lib.rs"), "//! fixture\n").unwrap();

        let env = test_env(&root);
        let out = execute(TOOL_GENERATE_EXERCISE, r#"{"topic": "t.c"}"#, &env, &|_| {}).unwrap();
        assert_eq!(out.value["ok"], true);
        assert_eq!(out.value["title"], "迷你");
        let offer = out.practice.expect("practice offer set");
        assert_eq!(offer.title, "迷你");
        assert!(offer.path.exists(), "{}", offer.path.display());
        let _ = std::fs::remove_dir_all(&root);
    }
}
