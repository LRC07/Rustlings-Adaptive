//! Agent environment & loop (M4): the conversation engine that turns
//! user input into model calls plus local tool executions (design
//! §7.3). One `run_turn` = one user turn; it may loop through several
//! model calls while tools run, and always returns the updated history
//! so the REPL can persist it as the session trajectory (R5).
//!
//! Design notes:
//! - The caller is an injected trait (`ChatTurnCaller`): production
//!   wires the blocking `LlmClient`, tests wire mocks.
//! - Tool calls are consumed from the OpenAI `tool_calls` field; models
//!   without native function calling can fall back to a JSON directive
//!   in the text (system prompt documents it).
//! - R6: every model call's usage is recorded (phase `chat` / per-tool
//!   `generate`) and summed per turn for the CLI footer line.
//! - R4: the loop checks the global interrupt flag between steps so
//!   Ctrl-C aborts a turn promptly (an in-flight HTTP call finishes in
//!   the background; its usage is still recorded).

pub mod session;
pub mod tools;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};

use crate::config::ModelConfig;
use crate::llm::{ChatMessage, Tool, TurnOutput};
use crate::usage::{self, UsageTracker};

/// System prompt: role, teaching stance, tool protocol (incl. the text
/// fallback for models without native function calling).
pub const SYSTEM_PROMPT: &str = "\
You are the coach of Rustlings-Adaptive, a Rust diagnostics tutor \
running inside a local CLI. The user is a Rust learner stuck on \
ownership/borrowing/lifetimes/traits/generics-style issues. Reply in \
简体中文, compactly (terminal UI).

Principles:
- Anchor diagnoses to facts: rustc error codes (E0xxx) plus the \
fine-grained concept ids from the local taxonomy. Never invent codes.
- Real execution over speculation: when the user pastes code or an \
error, call `check_code` to compile it locally first and explain from \
the real diagnostics.
- Teach, don't dump solutions: give hints and next steps first.
- Stay in role: this is a RUST coach. For requests in other \
languages (algorithms, snippets), give a brief explanation of the \
idea and, at most, a SHORT Rust version for comparison — do not \
write long implementations in other languages; steer back to Rust.
- Tool discipline: at most a few tool calls per turn; never call the \
same tool twice with identical arguments. If `check_code` fails two \
rounds in a row, STOP experimenting and explain from the diagnostics \
you already have — the user must always get a conclusion, not a \
silence. When the user wants to practice, or a quick focused exercise \
would verify their understanding, call `generate_exercise` with a \
topic (a concept id from `list_concepts`, an error code like E0382, \
or free text) and a short `reason` (one line: why this exercise now, \
derived from the conversation). After it succeeds, say the exercise \
is ready and can be started immediately. If generation FAILS, briefly \
tell the user why and suggest retrying / changing the topic / \
switching models (`/model`) — NEVER write an exercise yourself in the \
reply: an exercise without local triple verification is worthless here.
- Exercise precision (考察点): when the learner names a SPECIFIC \
technique or behavior to practice (e.g. the entry API, lazy \
unwrap_or_else, splitting borrows across fields), pass it verbatim in \
the `focus` argument — `topic` only anchors the domain. Before \
presenting the generated exercise, CHECK whether it actually trains \
what they asked. If it clearly does not, do NOT pretend it does and \
do NOT discuss internal matching — instead say: 练你点名的那个手法需要\
现生成一道，可能要等一两分钟，是否可以接受？ or offer the ready \
exercise as an alternative, and let them choose.

Tools:
- `list_concepts` {} — list the concept ids covered by the taxonomy.
- `generate_exercise` {\"topic\": string, \"focus\": string, \
\"reason\": string, \"mode\": \"auto|free\"} — generate a small 10-40 \
line fill-in exercise, triple-verified locally (compiles / reference \
solution passes all tests / unfinished template fails). auto (default) \
falls through template-fill → adapted → free generation, always \
through the same local quality gate; free skips templates and writes \
one from scratch — use it ONLY when the user explicitly asks for \
no-template / free generation (repeated template matches that miss \
their point are a strong signal to offer it). `focus` (optional but \
REQUIRED when the user named a specific technique) makes the whole \
pipeline aim at exactly that technique. It is written to the exercise \
directory; the user can start at once.
- `check_code` {\"code\": string} — compile a Rust snippet with local \
rustc and return real diagnostics (codes, messages, lines).
- `learner_profile` {} — the learner's local stats: weakest concepts, \
SM-2 due reviews, top error codes, most-failed exercises. Call it when \
the user asks what they are weak at or what to review, or before \
picking a topic when history could steer the choice.
- `borrowlab` {\"code\": string, \"hypothesis\": string} — the \
hypothesis lab: compiles the learner's code AND the hypothetical \
rewrite locally, returns the error-code diff (new / resolved). Use it \
for what-if questions (make t a reference instead, move vs borrow, \
...) — let the borrow checker provide the evidence, then explain the \
rule behind the change; never guess.

If your runtime cannot emit native tool calls, output a single JSON \
object on its own line instead: {\"tool\": \"<name>\", \"arguments\": \
{...}} — the harness runs it and feeds the result back.";

/// Blocking chat-turn caller; trait so tests can mock the model.
pub trait ChatTurnCaller: Send + Sync {
    fn chat_turn(&self, messages: &[ChatMessage], tools: &[Tool]) -> Result<TurnOutput>;

    /// Bounded variant for long-output generation (M4.7). The default
    /// ignores the cap so mocks stay trivial; the real client applies
    /// `max_tokens` on the wire.
    fn chat_turn_bounded(
        &self,
        messages: &[ChatMessage],
        tools: &[Tool],
        max_tokens: Option<u32>,
    ) -> Result<TurnOutput> {
        let _ = max_tokens;
        self.chat_turn(messages, tools)
    }

    /// Streaming variant (C1): content deltas flow through
    /// `out.on_content` as they arrive. The default delegates to the
    /// bounded turn and emits the whole content once — mocks and
    /// non-streaming callers are unaffected; the real client speaks
    /// SSE and falls back here exactly once when streaming is not
    /// supported by the endpoint.
    fn chat_turn_streaming(
        &self,
        messages: &[ChatMessage],
        tools: &[Tool],
        max_tokens: Option<u32>,
        out: &mut crate::llm::StreamOut,
    ) -> Result<TurnOutput> {
        let result = self.chat_turn_bounded(messages, tools, max_tokens);
        if let Ok(o) = &result
            && let Some(c) = &o.content
            && !c.is_empty()
        {
            (out.on_content)(c);
        }
        result
    }
}

impl ChatTurnCaller for crate::llm::LlmClient {
    fn chat_turn(&self, messages: &[ChatMessage], tools: &[Tool]) -> Result<TurnOutput> {
        crate::llm::LlmClient::chat_turn(self, messages, tools)
    }

    fn chat_turn_bounded(
        &self,
        messages: &[ChatMessage],
        tools: &[Tool],
        max_tokens: Option<u32>,
    ) -> Result<TurnOutput> {
        crate::llm::LlmClient::chat_turn_bounded(self, messages, tools, max_tokens)
    }

    fn chat_turn_streaming(
        &self,
        messages: &[ChatMessage],
        tools: &[Tool],
        max_tokens: Option<u32>,
        out: &mut crate::llm::StreamOut,
    ) -> Result<TurnOutput> {
        match crate::llm::LlmClient::chat_turn_streaming(self, messages, tools, max_tokens, out) {
            Ok(o) => Ok(o),
            // A real interruption propagates; anything else (endpoint
            // without SSE / incompatible stream_options / parse
            // hiccups) falls back ONCE to the non-streaming turn,
            // which also restores exact usage accounting.
            Err(e) if is_interrupted() => Err(e),
            Err(_) => crate::llm::LlmClient::chat_turn_bounded(self, messages, tools, max_tokens),
        }
    }
}

// ---------------------------------------------------------------------------
// Interrupt flag (R4). The ctrlc handler in the CLI sets it; the agent
// loop and long-running tools poll it.
// ---------------------------------------------------------------------------

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

pub fn set_interrupt() {
    INTERRUPTED.store(true, Ordering::SeqCst);
}

pub fn reset_interrupt() {
    INTERRUPTED.store(false, Ordering::SeqCst);
}

pub fn is_interrupted() -> bool {
    INTERRUPTED.load(Ordering::SeqCst)
}

// ---------------------------------------------------------------------------
// Environment & turn
// ---------------------------------------------------------------------------

/// Everything a turn (and its tools) needs from the outside.
pub struct AgentEnv {
    pub caller: Arc<dyn ChatTurnCaller>,
    /// Generate-phase caller (M9l routing): used by the
    /// `generate_exercise` tool so the generation model can differ from
    /// the chat model. Falls back to `caller` construction semantics —
    /// the CLI always fills it (chat caller when generate is unrouted).
    pub gen_caller: Arc<dyn ChatTurnCaller>,
    pub gen_cfg: ModelConfig,
    pub tracker: Arc<Mutex<UsageTracker>>,
    pub cfg: ModelConfig,
    /// Repo root (templates/, taxonomy/, exercises/ live under it).
    pub root: PathBuf,
    /// Current session id (M4.5a): stamped onto generated exercises in
    /// the index so the board can attribute them.
    pub session_id: Option<String>,
    /// Practice-state summary (M4.5a state back-flow), appended to the
    /// system prompt for every turn. Computed per turn by the CLI.
    pub practice_note: Option<String>,
    /// Open code threads (M5, retro §6.3): recently pasted code the
    /// coach proposed changes for but which were never re-verified.
    /// Computed per turn by the CLI; drives the follow-up principle.
    pub open_loop_note: Option<String>,
}

/// Offer to jump into the practice sub-mode after an exercise was
/// generated inside a turn.
#[derive(Debug, Clone)]
pub struct PracticeOffer {
    pub title: String,
    pub path: PathBuf,
    pub difficulty: String,
    /// Concept ids of the generated exercise (M4.5a card).
    pub concepts: Vec<String>,
    /// Why this exercise was produced (M4.5a card, from tool `reason`).
    pub trigger: Option<String>,
}

/// Result of one completed user turn.
#[derive(Debug)]
pub struct TurnOutcome {
    /// Full updated history (including system prompt, user input, tool
    /// traffic, final assistant reply) — the session persists this.
    pub history: Vec<ChatMessage>,
    /// Final assistant text to display (None when aborted early).
    pub reply: Option<String>,
    /// Human-readable tool trace lines ("生成成功：…").
    pub tool_notes: Vec<String>,
    pub practice: Option<PracticeOffer>,
    /// Usage of this turn (direct calls + tool-internal calls).
    pub calls: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
    pub cost_usd: f64,
    /// M9h: at least one call in this turn was billed from a byte
    /// estimate (endpoint streamed without a usage tail) — the footer
    /// marks it so the numbers are not mistaken for exact (R6).
    pub usage_estimated: bool,
}

/// How many model⇄tool round trips per user turn before bailing out.
pub const MAX_TOOL_ROUNDS: u32 = 5;
/// Marker of the per-turn open-loop note (M5): messages carrying it are
/// stripped from the returned history so the note never accumulates.
const OPEN_LOOP_MARKER: &str = "[Open code threads";

fn strip_open_loop(msgs: Vec<ChatMessage>) -> Vec<ChatMessage> {
    msgs.into_iter()
        .filter(|m| {
            !(m.role == "system"
                && m.content.as_deref().map(|c| c.starts_with(OPEN_LOOP_MARKER)).unwrap_or(false))
        })
        .collect()
}
/// Hard cap on sent messages regardless of the token budget (keeps
/// the request bounded even with a huge configured context).
pub const WINDOW_MESSAGES: usize = 40;
/// Tool results are trimmed to this length before entering history.
const TOOL_RESULT_CHARS: usize = 1500;

/// Run one user turn. `progress` receives short status strings for the
/// CLI spinner ("思考中…", "工具 check_code…"); `on_delta` (C1) receives
/// content deltas the moment they stream in — the CLI renders them
/// live, or ignores them entirely when stdout is not a terminal.
pub fn run_turn(
    history: &[ChatMessage],
    input: &str,
    env: &AgentEnv,
    progress: &dyn Fn(&str),
    on_delta: &mut dyn FnMut(&str),
) -> Result<TurnOutcome> {
    let mut msgs: Vec<ChatMessage> = history.to_vec();
    // The system prompt is rebuilt from the canonical constant on every
    // turn, so the per-turn practice note (M4.5a) never accumulates
    // across a persisted history.
    let mut sys = SYSTEM_PROMPT.to_string();
    if let Some(note) = &env.practice_note {
        sys.push_str("\n\n");
        sys.push_str(note);
    }
    if msgs.first().map(|m| m.role.as_str()) == Some("system") {
        msgs[0] = ChatMessage::system(sys);
    } else {
        msgs.insert(0, ChatMessage::system(sys));
    }
    msgs.push(ChatMessage::user(input));
    // Open code threads (M5, retro §6.3) ride at the END of the message
    // list — right next to the user's question — because trailing
    // instructions get far better adherence than system-header rules
    // (实测 9.4 深夜: header 版两次被模型无视). The message is peeled
    // off before the history is returned (see strip_open_loop), so it
    // never accumulates in the persisted session.
    if let Some(note) = &env.open_loop_note {
        msgs.push(ChatMessage::system(note.clone()));
    }

    let mut totals = tools::UsageAcc::default();
    let mut estimated = false;
    let mut tool_notes = Vec::new();
    let mut practice = None;
    let mut reply: Option<String> = None;

    for _round in 0..MAX_TOOL_ROUNDS {
        if is_interrupted() {
            return Err(anyhow!("已打断"));
        }

        // R6: budget gate before every direct model call.
        if let Err(e) = usage::check_budget(
            env.tracker.lock().unwrap_or_else(|p| p.into_inner()).all_totals().cost_usd,
            env.cfg.budget_usd(),
        ) {
            reply = Some(format!("（模型调用被拦截：{e}。可在 /config 调整预算或查看 /usage。）"));
            msgs.push(ChatMessage::assistant(reply.clone().unwrap()));
            return Ok(TurnOutcome {
                history: strip_open_loop(msgs),
                reply,
                tool_notes,
                practice,
                calls: totals.calls,
                input_tokens: totals.input_tokens,
                output_tokens: totals.output_tokens,
                reasoning_tokens: totals.reasoning_tokens,
                cost_usd: totals.cost_usd,
                usage_estimated: false,
            });
        }

        progress("思考中…");
        // C1: streaming call — content deltas flow through on_delta;
        // the trait impl falls back to a non-streaming turn once when
        // the endpoint cannot stream. Usage accounting is unchanged
        // (usage arrives in the stream tail or via the fallback).
        let out = env
            .caller
            .chat_turn_streaming(
                &window(&msgs, env.cfg.context_len),
                &tools::tool_schemas(),
                None,
                &mut crate::llm::StreamOut { on_content: on_delta, should_stop: &is_interrupted },
            )
            .map_err(|e| {
                if is_interrupted() {
                    anyhow!("已打断")
                } else {
                    e
                }
            })?;
        record(env, out.usage, "chat", &mut totals);
        estimated |= out.usage_estimated;
        // M9h: the stream was interrupted mid-read; the usage above is
        // the byte estimate (recorded — the caller paid for it). Abort
        // the turn as before (partial content stays out of history).
        if out.interrupted {
            return Err(anyhow!("已打断"));
        }

        if !out.has_tool_calls() {
            // Text-protocol fallback: a JSON {"tool": ...} directive.
            if let Some((name, args, raw)) = out.content.as_deref().and_then(parse_text_directive) {
                msgs.push(ChatMessage::assistant(raw.clone()));
                progress(&format!("工具 {name}…"));
                let result = run_tool(&name, &args, env, progress, &mut totals, &mut tool_notes, &mut practice);
                msgs.push(ChatMessage::user(format!("[工具 {name} 结果]\n{result}")));
                continue;
            }
            // Final answer for this turn.
            let text = out
                .content
                .filter(|c| !c.trim().is_empty())
                .unwrap_or_else(|| "（模型返回了空回复，请重试）".to_string());
            msgs.push(ChatMessage::assistant(text.clone()));
            reply = Some(text);
            break;
        }

        // Native tool calls: answer them via role:"tool" messages.
        msgs.push(ChatMessage::assistant_with_calls(out.tool_calls.clone()));
        for call in &out.tool_calls {
            if is_interrupted() {
                return Err(anyhow!("已打断"));
            }
            progress(&format!("工具 {}…", call.name));
            let result = run_tool(
                &call.name,
                &call.arguments,
                env,
                progress,
                &mut totals,
                &mut tool_notes,
                &mut practice,
            );
            msgs.push(ChatMessage::tool_result(call.id.clone(), result));
        }
    }

    if reply.is_none() {
        // Tool-round budget exhausted: force ONE final tool-less call
        // demanding a conclusion. A canned stub left the user with
        // nothing (9.4 session 27: five check_code experiments burned
        // the budget and the question went unanswered).
        msgs.push(ChatMessage::user(
            "（系统）工具调用轮次已达上限。不要再调用任何工具，立即基于已有的诊断与观察，\
             直接给出你对问题的结论与解释。"
                .to_string(),
        ));
        progress("整理结论…");
        match env.caller.chat_turn(&window(&msgs, env.cfg.context_len), &[]) {
            Ok(out) => {
                record(env, out.usage, "chat", &mut totals);
                let text = out
                    .content
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or_else(|| {
                        "（工具调用轮次已达上限，模型未能给出结论；请继续追问或换个问法。）".to_string()
                    });
                msgs.push(ChatMessage::assistant(text.clone()));
                reply = Some(text);
            }
            Err(_) => {
                let text =
                    "（本回合的工具调用轮次已达上限，先回答到这里；可以继续追问或换个问法。）".to_string();
                msgs.push(ChatMessage::assistant(text.clone()));
                reply = Some(text);
            }
        }
    }

    Ok(TurnOutcome {
        history: strip_open_loop(msgs),
        reply,
        tool_notes,
        practice,
        calls: totals.calls,
        input_tokens: totals.input_tokens,
        output_tokens: totals.output_tokens,
        reasoning_tokens: totals.reasoning_tokens,
        cost_usd: totals.cost_usd,
        usage_estimated: estimated,
    })
}

/// Execute one tool call: on success return its JSON; on failure
/// return an error JSON (the model can react, e.g. retry with
/// different arguments).
fn run_tool(
    name: &str,
    arguments: &str,
    env: &AgentEnv,
    progress: &dyn Fn(&str),
    totals: &mut tools::UsageAcc,
    tool_notes: &mut Vec<String>,
    practice: &mut Option<PracticeOffer>,
) -> String {
    match tools::execute(name, arguments, env, progress) {
        Ok(outcome) => {
            totals.calls += outcome.usage.calls;
            totals.input_tokens += outcome.usage.input_tokens;
            totals.output_tokens += outcome.usage.output_tokens;
            totals.reasoning_tokens += outcome.usage.reasoning_tokens;
            totals.cost_usd += outcome.usage.cost_usd;
            if let Some(n) = outcome.note {
                tool_notes.push(n);
            }
            if outcome.practice.is_some() {
                *practice = outcome.practice;
            }
            ellipsize(&outcome.value.to_string(), TOOL_RESULT_CHARS)
        }
        Err(e) => {
            let msg = format!("工具执行失败：{e:#}");
            tool_notes.push(msg.clone());
            serde_json::json!({ "ok": false, "error": ellipsize(&msg, TOOL_RESULT_CHARS) }).to_string()
        }
    }
}

/// Record one model call into the usage tracker (R6).
fn record(env: &AgentEnv, u: crate::llm::Usage, phase: &str, totals: &mut tools::UsageAcc) {
    let cost = usage::cost_usd(u.prompt_tokens, u.completion_tokens, env.cfg.prices.input, env.cfg.prices.output);
    env.tracker
        .lock()
        .expect("usage lock")
        .record(&env.cfg.model, u.prompt_tokens, u.completion_tokens, u.reasoning_tokens, cost, phase);
    totals.add(u.prompt_tokens, u.completion_tokens, u.reasoning_tokens, cost);
}

// ---------------------------------------------------------------------------
// Context window & text protocol
// ---------------------------------------------------------------------------

/// Rough token estimate for mixed CJK/ASCII text: ASCII ≈ 4 chars per
/// token, CJK ≈ 1 char per token. Deterministic and documented — good
/// enough for windowing (R3: the configured `context_len` finally
/// drives the conversation window).
pub fn estimate_tokens(s: &str) -> u64 {
    let mut ascii = 0u64;
    let mut wide = 0u64;
    for c in s.chars() {
        if (c as u32) < 0x80 {
            ascii += 1;
        } else {
            wide += 1;
        }
    }
    ascii / 4 + wide
}

/// Outgoing message window (R3): system prompt + as many of the latest
/// messages as fit ~¾ of the configured context (the rest is headroom
/// for the reply), capped at `WINDOW_MESSAGES` total. Oversized
/// contents are trimmed; leading orphan tool results (cut loose by the
/// window) are dropped so the wire format stays valid.
pub fn window(msgs: &[ChatMessage], context_len: u32) -> Vec<ChatMessage> {
    let budget = (context_len.max(1024) as u64) * 3 / 4;
    let mut out: Vec<ChatMessage> = if msgs.len() <= WINDOW_MESSAGES + 1 {
        msgs.to_vec()
    } else {
        let mut v = vec![msgs[0].clone()];
        v.extend(msgs[msgs.len() - WINDOW_MESSAGES..].iter().cloned());
        v
    };
    // Drop from the front (after the system prompt) until the estimate
    // fits — but always keep at least the latest message.
    while out.len() > 2 {
        let total: u64 = out.iter().map(message_tokens).sum();
        if total <= budget {
            break;
        }
        out.remove(1);
    }
    // Drop tool results whose assistant tool_calls message was cut off.
    while out.len() > 1 && out[1].role == "tool" {
        out.remove(1);
    }
    for m in &mut out {
        if let Some(c) = m.content.as_mut().filter(|c| c.chars().count() > TOOL_RESULT_CHARS) {
            *c = ellipsize(c, TOOL_RESULT_CHARS);
        }
    }
    out
}

fn message_tokens(m: &ChatMessage) -> u64 {
    let content = m.content.as_deref().unwrap_or("");
    let args: u64 = m.tool_calls.iter().map(|c| estimate_tokens(&c.arguments) + 4).sum();
    estimate_tokens(content) + args + 4
}

/// Detect the text-protocol directive `{"tool": ..., "arguments": ...}`
/// in a model reply. Returns (name, arguments-json, raw-text).
fn parse_text_directive(content: &str) -> Option<(String, String, String)> {
    let start = content.find('{')?;
    let end = content.rfind('}')?;
    if end < start {
        return None;
    }
    let candidate = &content[start..=end];
    let v: serde_json::Value = serde_json::from_str(candidate).ok()?;
    let name = v.get("tool")?.as_str()?.to_string();
    let args = v
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()))
        .to_string();
    Some((name, args, content.to_string()))
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
    use crate::llm::{ToolCall, Usage};

    fn reply(content: &str) -> TurnOutput {
        TurnOutput { content: Some(content.into()), tool_calls: vec![], usage: Usage { prompt_tokens: 10, completion_tokens: 5, reasoning_tokens: 0 }, finish_reason: Some("stop".into()), interrupted: false, usage_estimated: false }
    }

    fn calls_output(calls: Vec<ToolCall>) -> TurnOutput {
        TurnOutput { content: None, tool_calls: calls, usage: Usage { prompt_tokens: 10, completion_tokens: 5, reasoning_tokens: 0 }, finish_reason: Some("tool_calls".into()), interrupted: false, usage_estimated: false }
    }

    fn call(id: &str, name: &str, args: &str) -> ToolCall {
        ToolCall { id: id.into(), name: name.into(), arguments: args.into() }
    }

    /// Scripted model: pops queued outputs in order.
    struct Mock {
        turns: std::sync::Mutex<Vec<Result<TurnOutput>>>,
    }
    impl Mock {
        fn new(turns: Vec<Result<TurnOutput>>) -> Self {
            Self { turns: std::sync::Mutex::new(turns) }
        }
    }
    impl ChatTurnCaller for Mock {
        fn chat_turn(&self, _m: &[ChatMessage], _t: &[Tool]) -> Result<TurnOutput> {
            self.turns.lock().unwrap().remove(0)
        }
    }

    fn test_env(caller: Arc<dyn ChatTurnCaller>) -> AgentEnv {
        AgentEnv {
            gen_caller: caller.clone(),
            gen_cfg: ModelConfig::default(),
            caller,
            tracker: Arc::new(Mutex::new(usage::UsageTracker::from_path(
                std::env::temp_dir().join(format!("rs_agent_usage_{}.json", std::process::id())),
            ))),
            cfg: ModelConfig::default(),
            root: PathBuf::from("."),
            session_id: None,
            practice_note: None,
            open_loop_note: None,
        }
    }

    fn noop(_: &str) {}

    #[test]
    fn plain_reply_updates_history_and_records_usage() {
        let env = test_env(Arc::new(Mock::new(vec![Ok(reply("E0382 是所有权移动"))])));
        let out = run_turn(&[], "为什么报错", &env, &noop, &mut |_| {}).unwrap();
        assert_eq!(out.reply.as_deref(), Some("E0382 是所有权移动"));
        assert!(out.tool_notes.is_empty());
        assert_eq!(out.calls, 1);
        assert_eq!(out.input_tokens, 10);
        // system + user + assistant
        assert_eq!(out.history.len(), 3);
        assert_eq!(out.history[0].role, "system");
        assert_eq!(out.history[2].role, "assistant");
        // usage recorded into the tracker
        assert_eq!(env.tracker.lock().unwrap().session_totals().calls, 1);
        assert!(env.tracker.lock().unwrap().session_totals().cost_usd > 0.0);
    }

    #[test]
    fn tool_round_trips_then_answers() {
        let env = test_env(Arc::new(Mock::new(vec![
            Ok(calls_output(vec![call("c1", tools::TOOL_LIST_CONCEPTS, "{}")])),
            Ok(reply("概念图谱有这些主题…")),
        ])));
        let out = run_turn(&[], "能出什么题", &env, &noop, &mut |_| {}).unwrap();
        assert_eq!(out.reply.as_deref(), Some("概念图谱有这些主题…"));
        assert_eq!(out.history.len(), 5); // sys + user + asst(calls) + tool + asst
        assert_eq!(out.history[3].role, "tool");
        assert_eq!(out.history[3].tool_call_id.as_deref(), Some("c1"));
        assert!(out.history[3].content.as_deref().unwrap().contains("concepts"));
        assert_eq!(out.calls, 2);
        assert!(!out.tool_notes.is_empty());
    }

    #[test]
    fn text_protocol_fallback_runs_tools() {
        let env = test_env(Arc::new(Mock::new(vec![
            Ok(reply("{\"tool\": \"list_concepts\", \"arguments\": {}}")),
            Ok(reply("这些是可用主题")),
        ])));
        let out = run_turn(&[], "主题", &env, &noop, &mut |_| {}).unwrap();
        assert_eq!(out.reply.as_deref(), Some("这些是可用主题"));
        // fallback feeds results as user messages
        assert_eq!(out.history.len(), 5);
        assert_eq!(out.history[3].role, "user");
        assert!(out.history[3].content.as_deref().unwrap().starts_with("[工具 list_concepts 结果]"));
    }

    #[test]
    fn failed_tool_returns_error_json_and_loop_continues() {
        let env = test_env(Arc::new(Mock::new(vec![
            Ok(calls_output(vec![call("c1", "no_such_tool", "{}")])),
            Ok(reply("明白了，换个方式")),
        ])));
        let out = run_turn(&[], "x", &env, &noop, &mut |_| {}).unwrap();
        assert_eq!(out.history[3].role, "tool");
        assert!(out.history[3].content.as_deref().unwrap().contains("\"ok\":false"));
        assert!(out.tool_notes.iter().any(|n| n.contains("工具执行失败")));
    }

    #[test]
    fn budget_exhausted_blocks_before_calling() {
        let mut env = test_env(Arc::new(Mock::new(vec![Ok(reply("不该被调用"))])));
        env.cfg.budget = Some(crate::config::Budget { usd: 0.0 });
        let out = run_turn(&[], "问个问题", &env, &noop, &mut |_| {}).unwrap();
        assert!(out.reply.as_deref().unwrap().contains("被拦截"), "{out:?}");
        assert_eq!(out.calls, 0);
        assert!(out.history.iter().any(|m| m.role == "assistant" && m.content.as_deref().unwrap().contains("被拦截")));
    }

    #[test]
    fn window_keeps_system_and_latest() {
        let msgs: Vec<ChatMessage> = std::iter::once(ChatMessage::system("sys"))
            .chain((0..50).map(|i| ChatMessage::user(format!("m{i}"))))
            .collect();
        let w = window(&msgs, 128_000);
        assert_eq!(w.len(), WINDOW_MESSAGES + 1);
        assert_eq!(w[0].content.as_deref(), Some("sys"));
        assert_eq!(w[1].content.as_deref(), Some("m10"));
        assert_eq!(w.last().unwrap().content.as_deref(), Some("m49"));
    }

    #[test]
    fn estimate_tokens_mixed_cjk_ascii() {
        assert_eq!(estimate_tokens("hello"), 1); // 5 ascii / 4
        assert_eq!(estimate_tokens("你好"), 2); // wide chars ≈ 1 tok each
        assert_eq!(estimate_tokens("a你b"), 1); // ascii 2/4=0, CJK 1
    }

    #[test]
    fn window_trims_to_configured_context() {
        // A tiny configured context forces the window down to
        // system + the latest message (always kept).
        let msgs = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("短"),
            ChatMessage::assistant("回"),
            ChatMessage::user("又一条"),
        ];
        let w = window(&msgs, 1024); // budget 768 tokens; messages are tiny
        assert_eq!(w.len(), 4); // tiny history fits easily
        // Now flood with huge messages: must trim aggressively.
        let big = "x".repeat(10_000); // ≈2500 tokens
        let msgs = vec![
            ChatMessage::system("sys"),
            ChatMessage::user(&big),
            ChatMessage::assistant(&big),
            ChatMessage::user(&big),
            ChatMessage::assistant(&big),
            ChatMessage::user("latest"),
        ];
        let w = window(&msgs, 4096); // budget 3072 < 4×2500
        assert!(w.len() < msgs.len(), "must trim, got {}", w.len());
        assert_eq!(w.last().unwrap().content.as_deref(), Some("latest"));
        assert_eq!(w[0].role, "system");
    }

    #[test]
    fn window_drops_orphan_tool_results() {
        // Defensive shape: a tool result whose assistant tool_calls
        // message was cut off (boundary case) must be dropped, or the
        // request would be wire-invalid.
        let msgs = vec![
            ChatMessage::system("sys"),
            ChatMessage::tool_result("c1", "r"),
            ChatMessage::user("later"),
        ];
        let w = window(&msgs, 128_000);
        assert!(w.iter().all(|m| m.role != "tool"), "orphan tool result must be dropped");
        assert_eq!(w.len(), 2);
        assert_eq!(w[0].role, "system");
    }

    #[test]
    fn long_tool_results_are_trimmed() {
        let big = "x".repeat(5000);
        let msgs = vec![
            ChatMessage::system("s"),
            ChatMessage::user("q"),
            ChatMessage::assistant_with_calls(vec![call("c1", "t", "{}")]),
            ChatMessage::tool_result("c1", big),
        ];
        let w = window(&msgs, 128_000);
        let tool_msg = w.iter().find(|m| m.role == "tool").unwrap();
        assert!(tool_msg.content.as_deref().unwrap().chars().count() <= TOOL_RESULT_CHARS + 2);
    }

    #[test]
    fn text_directive_parsing() {
        let (name, args, raw) = parse_text_directive("好的 {\"tool\": \"check_code\", \"arguments\": {\"code\": \"fn main(){}\"}} 就绪").unwrap();
        assert_eq!(name, "check_code");
        assert!(args.contains("fn main"));
        assert!(raw.contains("就绪"));
        assert!(parse_text_directive("没有 JSON").is_none());
        assert!(parse_text_directive("{\"other\": 1}").is_none());
    }
}
