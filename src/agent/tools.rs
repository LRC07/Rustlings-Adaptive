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
                    "reason": {
                        "type": "string",
                        "description": "一句话说明为什么现在出这道题（结合对话语境，如「你贴的代码报 E0382」）；\
                                        会展示给用户并随题归档"
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
    ]
}

/// Aggregated usage of one tool execution (its internal LLM calls).
#[derive(Debug, Clone, Copy, Default)]
pub struct UsageAcc {
    pub calls: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
}

impl UsageAcc {
    pub(crate) fn add(&mut self, input: u64, output: u64, cost: f64) {
        self.calls += 1;
        self.input_tokens += input;
        self.output_tokens += output;
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
        other => Err(anyhow!("未知工具「{other}」；可用工具：{TOOL_LIST_CONCEPTS} / {TOOL_GENERATE_EXERCISE} / {TOOL_CHECK_CODE}")),
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
        let out: TurnOutput = self.caller.chat_turn(&[ChatMessage::user(prompt.to_string())], &[])?;
        let cost = usage::cost_usd(
            out.usage.prompt_tokens,
            out.usage.completion_tokens,
            self.input_price,
            self.output_price,
        );
        self.acc.add(out.usage.prompt_tokens, out.usage.completion_tokens, cost);
        Ok(LlmReply { content: out.content.unwrap_or_default(), usage: out.usage })
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
    progress(&format!("生成练习（{topic_text}）：选模板…"));

    let paths = Paths::from_root(&env.root);
    let mut bridge = CallerBridge {
        caller: env.caller.clone(),
        input_price: env.cfg.prices.input,
        output_price: env.cfg.prices.output,
        acc: UsageAcc::default(),
    };
    let outcome = generator::generate(
        &topic,
        &paths,
        Some(&mut bridge),
        Some(&mut |stage| {
            progress(&format!(
                "生成练习（{topic_text}）：{}（第 {}/{} 轮）",
                stage.stage, stage.attempt, stage.total_attempts
            ));
        }),
    )?;

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
    let exercises_dir = env.root.join("exercises");
    if let Err(e) = crate::exercise::index::register_generated(
        &exercises_dir,
        &outcome.path,
        &outcome.title,
        &outcome.concepts,
        &outcome.error_codes,
        Some(outcome.difficulty.as_str()),
        crate::exercise::index::Source::TemplateFill { template_id: outcome.template_id.clone() },
        env.session_id.as_deref(),
        Some(&trigger),
    ) {
        progress(&format!("index 登记失败：{e:#}"));
    }

    let value = json!({
        "ok": true,
        "title": outcome.title,
        "file": outcome.name,
        "path": outcome.path.display().to_string(),
        "concepts": outcome.concepts,
        "difficulty": outcome.difficulty.name_cn(),
        "attempts": outcome.attempts,
        "used_llm": outcome.used_llm,
        "trigger": trigger,
        "note": "题目已写入练习目录并接线，用户可以立即开始做题",
    });
    Ok(ToolOutcome {
        note: Some(format!(
            "生成成功：《{}》（{}，第 {} 轮通过）",
            outcome.title,
            outcome.difficulty.name_cn(),
            outcome.attempts
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
        }
    }

    #[test]
    fn schemas_cover_all_tools() {
        let schemas = tool_schemas();
        let names: Vec<&str> = schemas.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec![TOOL_LIST_CONCEPTS, TOOL_GENERATE_EXERCISE, TOOL_CHECK_CODE]);
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
            "id = \"mini\"\ntitle = \"迷你\"\nconcepts = [\"t.c\"]\nerror_codes = [\"E0308\"]\ndifficulty = \"easy\"\n\nbody = '''\n// 说明行。\n// 说明行二。\n// 说明行三。\n// 说明行四。\n// 说明行五。\n// 说明行六。\nfn add(a: i32, b: i32) -> i32 {\n    todo!()\n}\n// I AM NOT DONE\n'''\n\ntests = '''\n#[cfg(test)]\nmod tests {\n    use super::*;\n    #[test]\n    fn t() {\n        assert_eq!(add(1, 2), 3);\n    }\n}\n'''\n\nreference = '''\nfn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n'''\n",
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
