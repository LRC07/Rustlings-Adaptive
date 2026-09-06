//! OpenAI-compatible chat client (assignment requirement R1: the core
//! LLM call orchestration lives in Rust). Blocking reqwest keeps the
//! REPL simple; async/streaming can be introduced later if needed.
//!
//! M4 adds multi-turn conversations and function/tool calling: the
//! agent loop (M4) sends the full message history plus a tool schema
//! list and receives either text or `tool_calls`. The internal
//! `ChatMessage`/`ToolCall` types are flat; the OpenAI wire shape
//! (nested `function` objects, `tool_call_id`) is only produced while
//! building/parsing HTTP bodies.

use anyhow::{anyhow, bail, Context, Result};
use std::io::Read;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;

/// 240s: long LLM tasks (tier-2/3 exercise drafts output 3–8k tokens)
/// can exceed two minutes; short chat turns are unaffected in practice.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(480);

/// The built-in default request timeout (used when config has no
/// `llm_timeout_secs`).
pub fn default_timeout() -> Duration {
    DEFAULT_TIMEOUT
}
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const ERROR_BODY_SNIPPET: usize = 500;

/// Token usage as reported by the API (R6 relies on these numbers).
/// `reasoning_tokens` (thinking-mode CoT) is optional: only some
/// endpoints report the breakdown, others leave it 0.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    #[serde(default)]
    pub reasoning_tokens: u64,
}

#[derive(Debug, Clone)]
pub struct LlmReply {
    pub content: String,
    pub usage: Usage,
    /// "length" when the completion was cut off by max_tokens (M4.7).
    pub finish_reason: Option<String>,
}

/// Thinking-mode switch for hybrid-reasoning models (M4.12, R3).
/// DeepSeek V4: `{"thinking": {"type": "enabled"/"disabled"}}`, and the
/// endpoint default is *enabled* with effort *high* — so "not sending
/// anything" silently burns reasoning tokens on every call. Absent =
/// follow the endpoint default (non-DeepSeek endpoints never see the
/// parameter).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Thinking {
    Enabled,
    Disabled,
}

/// Extract the outermost JSON object substring from a model reply
/// (tolerates prose / markdown fences around it). Shared by the
/// generator's draft loop and the M5 review/debrief JSON steps.
pub fn extract_json(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    (end >= start).then_some(&text[start..=end])
}

// ---------------------------------------------------------------------------
// Conversation types (M4: agent loop + tool calling)
// ---------------------------------------------------------------------------

/// One conversation message. `role` is `system` | `user` | `assistant`
/// | `tool`. Tool calls live on assistant messages; tool results are
/// `role: "tool"` messages referencing the call id.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// Present on `tool` messages (the id of the call being answered).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl ChatMessage {
    pub fn system(text: impl Into<String>) -> Self {
        Self { role: "system".into(), content: Some(text.into()), tool_calls: vec![], tool_call_id: None }
    }
    pub fn user(text: impl Into<String>) -> Self {
        Self { role: "user".into(), content: Some(text.into()), tool_calls: vec![], tool_call_id: None }
    }
    pub fn assistant(text: impl Into<String>) -> Self {
        Self { role: "assistant".into(), content: Some(text.into()), tool_calls: vec![], tool_call_id: None }
    }
    /// Assistant message carrying tool calls (content may be None).
    pub fn assistant_with_calls(calls: Vec<ToolCall>) -> Self {
        Self { role: "assistant".into(), content: None, tool_calls: calls, tool_call_id: None }
    }
    /// Tool result message answering `call_id`.
    pub fn tool_result(call_id: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            role: "tool".into(),
            content: Some(text.into()),
            tool_calls: vec![],
            tool_call_id: Some(call_id.into()),
        }
    }
}

/// One tool invocation requested by the model (flat form; the wire
/// format nests it under `function`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// Raw JSON string, exactly as the model produced it.
    pub arguments: String,
}

/// A tool offered to the model (OpenAI function schema).
#[derive(Debug, Clone, Serialize)]
pub struct Tool {
    pub name: String,
    pub description: String,
    /// JSON Schema for the parameters object.
    pub parameters: serde_json::Value,
}

/// Result of one chat turn: text and/or tool calls, plus usage.
#[derive(Debug, Clone, Default)]
pub struct TurnOutput {
    pub content: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Usage,
    /// choices[0].finish_reason ("stop" | "length" | "tool_calls" | …);
    /// "length" means the output was cut off by max_tokens (M4.7).
    pub finish_reason: Option<String>,
}

impl TurnOutput {
    pub fn has_tool_calls(&self) -> bool {
        !self.tool_calls.is_empty()
    }
}

#[derive(Clone)]
pub struct LlmClient {
    http: Client,
    endpoint: String,
    api_key: String,
    model: String,
    /// M4.12 (R3): thinking-mode switch sent on every chat request.
    /// `None` = send nothing (endpoint default).
    thinking: Option<Thinking>,
    /// Reasoning effort ("low"/"high"/"max") when thinking is on;
    /// `None` = endpoint default (high on DeepSeek V4).
    reasoning_effort: Option<String>,
}

/// Build the chat/completions URL from a base endpoint. Accepts both
/// `https://host/v1` and a full `.../chat/completions` URL.
fn chat_url(endpoint: &str) -> String {
    let e = endpoint.trim().trim_end_matches('/');
    if e.ends_with("/chat/completions") {
        e.to_string()
    } else {
        format!("{e}/chat/completions")
    }
}

impl LlmClient {
    /// Same client with an explicit per-request timeout (R3: slow /
    /// congested endpoints need a tunable ceiling; default
    /// `default_timeout()`).
    pub fn with_timeout(endpoint: &str, api_key: &str, model: &str, timeout: Duration) -> Self {
        let http = Client::builder()
            .timeout(timeout)
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .expect("failed to build HTTP client");
        Self {
            http,
            endpoint: endpoint.to_string(),
            api_key: api_key.to_string(),
            model: model.to_string(),
            thinking: None,
            reasoning_effort: None,
        }
    }

    /// M4.12 (R3): set the thinking-mode switch (from config
    /// `think_mode`). Builder style — `make_client` chains it.
    pub fn with_thinking(mut self, thinking: Option<Thinking>) -> Self {
        self.thinking = thinking;
        self
    }

    /// M4.12: reasoning-effort knob ("low"/"high"/"max") to balance
    /// latency/cost against output quality on thinking models.
    pub fn with_reasoning_effort(mut self, effort: Option<String>) -> Self {
        self.reasoning_effort = effort;
        self
    }

    /// One conversation turn with the full history and an optional tool
    /// schema list (M4 agent loop). Returns text and/or tool calls.
    pub fn chat_turn(&self, messages: &[ChatMessage], tools: &[Tool]) -> Result<TurnOutput> {
        self.chat_turn_bounded(messages, tools, None)
    }

    /// Same turn with an optional `max_tokens` cap on the completion
    /// (M4.7): long-output generation (exercise drafts) needs a hard
    /// bound so a rambling model cannot burn minutes per round.
    pub fn chat_turn_bounded(
        &self,
        messages: &[ChatMessage],
        tools: &[Tool],
        max_tokens: Option<u32>,
    ) -> Result<TurnOutput> {
        let effort = self.reasoning_effort.as_deref();
        let body = build_request_body(&self.model, messages, tools, max_tokens, self.thinking, effort);
        let resp = self
            .http
            .post(chat_url(&self.endpoint))
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .context("网络请求失败（检查 endpoint 与网络连接）")?;
        let status = resp.status();
        let text = resp.text().context("读取模型响应失败")?;
        if !status.is_success() {
            let snippet: String = text.chars().take(ERROR_BODY_SNIPPET).collect();
            bail!("模型服务返回 {status}：{snippet}");
        }
        parse_turn_response(&text)
    }

    /// Streaming variant (C1): content deltas flow through
    /// `out.on_content` as they arrive; the fully aggregated output
    /// (content, tool calls, terminal usage) is returned exactly like
    /// `chat_turn`. `out.should_stop` aborts the read mid-stream — the
    /// caller treats that as an interruption.
    ///
    /// Requires an OpenAI-compatible SSE stream
    /// (`stream_options.include_usage`). Incompatibility is the
    /// CALLER's problem: the trait impl falls back to the non-streaming
    /// turn once (which also restores exact usage accounting).
    pub fn chat_turn_streaming(
        &self,
        messages: &[ChatMessage],
        tools: &[Tool],
        max_tokens: Option<u32>,
        out: &mut StreamOut,
    ) -> Result<TurnOutput> {
        let mut body = build_request_body(
            &self.model,
            messages,
            tools,
            max_tokens,
            self.thinking,
            self.reasoning_effort.as_deref(),
        );
        body["stream"] = serde_json::json!(true);
        body["stream_options"] = serde_json::json!({ "include_usage": true });
        let resp = self
            .http
            .post(chat_url(&self.endpoint))
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .context("网络请求失败（检查 endpoint 与网络连接）")?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().unwrap_or_default();
            let snippet: String = text.chars().take(ERROR_BODY_SNIPPET).collect();
            bail!("模型服务返回 {status}：{snippet}");
        }

        let mut sse = SseBuffer::default();
        let mut content = String::new();
        let mut calls = ToolCallAggregator::default();
        let mut usage: Option<Usage> = None;
        let mut finish_reason: Option<String> = None;
        // blocking Response implements std::io::Read: each read returns
        // whatever has arrived — exactly the streaming granularity we
        // need.
        let mut raw = resp;
        let mut buf = [0u8; 16 * 1024];
        loop {
            if (out.should_stop)() {
                bail!("已打断");
            }
            let n = raw.read(&mut buf).context("读取流式响应失败")?;
            if n == 0 {
                break;
            }
            for payload in sse.feed(&buf[..n]) {
                match parse_stream_payload(&payload)? {
                    Some(StreamFrame::Usage(u)) => usage = Some(u),
                    Some(StreamFrame::Delta { content: c, tool_call_frags, finish_reason: fr }) => {
                        if !c.is_empty() {
                            (out.on_content)(&c);
                            content.push_str(&c);
                        }
                        calls.feed(&tool_call_frags);
                        if fr.is_some() {
                            finish_reason = fr;
                        }
                    }
                    None => {}
                }
            }
        }
        let usage = usage.unwrap_or_else(|| estimate_usage(messages, &content));
        Ok(TurnOutput {
            content: Some(content),
            tool_calls: calls.take(),
            usage,
            finish_reason,
        })
    }
}

/// Build the OpenAI-compatible request body from internal message
/// types (flat → wire mapping happens here).
pub fn build_request_body(
    model: &str,
    messages: &[ChatMessage],
    tools: &[Tool],
    max_tokens: Option<u32>,
    thinking: Option<Thinking>,
    reasoning_effort: Option<&str>,
) -> serde_json::Value {
    let msgs: Vec<serde_json::Value> = messages
        .iter()
        .map(|m| {
            let mut v = serde_json::json!({ "role": m.role });
            if let Some(c) = &m.content {
                v["content"] = serde_json::Value::String(c.clone());
            }
            if !m.tool_calls.is_empty() {
                v["tool_calls"] = serde_json::Value::Array(
                    m.tool_calls
                        .iter()
                        .map(|t| {
                            serde_json::json!({
                                "id": t.id,
                                "type": "function",
                                "function": { "name": t.name, "arguments": t.arguments },
                            })
                        })
                        .collect(),
                );
            }
            if let Some(id) = &m.tool_call_id {
                v["tool_call_id"] = serde_json::Value::String(id.clone());
            }
            v
        })
        .collect();

    let mut body = serde_json::json!({ "model": model, "messages": msgs });
    if let Some(n) = max_tokens {
        body["max_tokens"] = serde_json::json!(n);
    }
    // M4.12: thinking-mode switch (hybrid-reasoning models). Omitted
    // entirely for Auto so plain chat endpoints never see the key.
    if let Some(t) = thinking {
        body["thinking"] = serde_json::json!({
            "type": match t { Thinking::Enabled => "enabled", Thinking::Disabled => "disabled" }
        });
    }
    if let Some(e) = reasoning_effort {
        body["reasoning_effort"] = serde_json::json!(e);
    }
    if !tools.is_empty() {
        let wire_tools: Vec<serde_json::Value> = tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.parameters,
                    },
                })
            })
            .collect();
        body["tools"] = serde_json::Value::Array(wire_tools);
        body["tool_choice"] = serde_json::Value::String("auto".into());
    }
    body
}

/// Parse an OpenAI-compatible chat completion response body into a
/// turn output (flat tool calls, usage).
pub fn parse_turn_response(body: &str) -> Result<TurnOutput> {
    #[derive(Deserialize)]
    struct Wire {
        #[serde(default)]
        choices: Vec<ChoiceWire>,
        // Kept raw: Usage is built from it so the nested reasoning
        // breakdown (`completion_tokens_details.reasoning_tokens`,
        // M4.12) can be lifted to the top level.
        #[serde(default)]
        usage: Option<Value>,
    }
    #[derive(Deserialize)]
    struct ChoiceWire {
        #[serde(default)]
        message: Option<MessageWire>,
        #[serde(default)]
        finish_reason: Option<String>,
    }
    #[derive(Deserialize)]
    struct MessageWire {
        #[serde(default)]
        content: Option<String>,
        #[serde(default)]
        tool_calls: Vec<ToolCallWire>,
    }
    #[derive(Deserialize)]
    struct ToolCallWire {
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        function: Option<FunctionWire>,
    }
    #[derive(Deserialize)]
    struct FunctionWire {
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        arguments: Option<String>,
    }

    let resp: Wire = serde_json::from_str(body).context("解析模型响应 JSON 失败")?;
    let choice = resp
        .choices
        .first()
        .ok_or_else(|| anyhow!("模型响应中没有回复内容"))?;
    let msg = choice
        .message
        .as_ref()
        .ok_or_else(|| anyhow!("模型响应中没有回复内容"))?;
    let tool_calls = msg
        .tool_calls
        .iter()
        .filter_map(|t| {
            let f = t.function.as_ref()?;
            Some(ToolCall {
                id: t.id.clone().unwrap_or_else(|| format!("call_{}", f.name.as_deref().unwrap_or("0"))),
                name: f.name.clone()?,
                arguments: f.arguments.clone().unwrap_or_default(),
            })
        })
        .collect();
    Ok(TurnOutput {
        content: msg.content.clone(),
        tool_calls,
        usage: resp
            .usage
            .map(|raw| {
                let mut usage: Usage =
                    serde_json::from_value(raw.clone()).unwrap_or_default();
                usage.reasoning_tokens =
                    raw["completion_tokens_details"]["reasoning_tokens"].as_u64().unwrap_or(0);
                usage
            })
            .unwrap_or_default(),
        finish_reason: choice.finish_reason.clone(),
    })
}

// ---------------------------------------------------------------------------
// Streaming (C1): pure SSE machinery + the wire callbacks. The network
// loop lives on `LlmClient::chat_turn_streaming`.
// ---------------------------------------------------------------------------

/// Wire callbacks of a streaming turn.
/// (Wired into the agent in block 2 — temporary allow.)
#[allow(dead_code)]
pub struct StreamOut<'a> {
    /// Called for every non-empty content delta, in order. The fully
    /// aggregated content is ALSO in the returned TurnOutput.
    pub on_content: &'a mut dyn FnMut(&str),
    /// Polled between chunks: true aborts the turn mid-stream.
    pub should_stop: &'a dyn Fn() -> bool,
}

/// One parsed `data:` payload of the SSE stream. Tool-call fragments
/// stay RAW here — they may split across frames, so aggregation owns
/// the whole stream (the network loop's ToolCallAggregator), not a
/// single frame.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamFrame {
    Delta {
        content: String,
        tool_call_frags: Vec<serde_json::Value>,
        finish_reason: Option<String>,
    },
    Usage(Usage),
}

/// Reassembles `data:` payloads from arbitrary network chunks: chunks
/// may split frames mid-line — and even mid-UTF-8-character — so the
/// buffer is byte-level and lines are only decoded when complete (a
/// line ending at '\n' is always valid UTF-8: 0x0A never appears
/// inside a multi-byte sequence).
pub struct SseBuffer {
    buf: Vec<u8>,
}

impl Default for SseBuffer {
    fn default() -> Self {
        Self { buf: Vec::new() }
    }
}

impl SseBuffer {
    /// Feed one raw chunk; returns complete `data:` payloads (prefix
    /// and surrounding whitespace stripped, empty/keep-alive lines
    /// dropped).
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let line_bytes: Vec<u8> = self.buf.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line_bytes);
            let line = line.trim();
            if let Some(payload) = line.strip_prefix("data:") {
                let payload = payload.trim();
                if !payload.is_empty() {
                    out.push(payload.to_string());
                }
            }
        }
        out
    }
}

/// Parse one `data:` payload: `[DONE]` → None, otherwise a Delta or
/// Usage frame (the include_usage tail carries an empty choices array).
pub fn parse_stream_payload(payload: &str) -> Result<Option<StreamFrame>> {
    let p = payload.trim();
    if p == "[DONE]" {
        return Ok(None);
    }
    let v: serde_json::Value =
        serde_json::from_str(p).with_context(|| format!("流式帧解析失败：{p}"))?;

    // Usage tail: usage present and no choice carries a delta.
    let choices = v["choices"].as_array();
    let has_delta = choices.is_none_or(|cs| cs.iter().any(|c| c.get("delta").is_some()));
    if !has_delta
        && let Some(u) = v.get("usage").filter(|u| u.is_object())
    {
        return Ok(Some(StreamFrame::Usage(parse_usage_value(u))));
    }

    let Some(choice) = choices.and_then(|cs| cs.first()) else {
        // No choices and no usable usage — ignore the frame.
        return Ok(None);
    };
    let delta = &choice["delta"];
    let content = delta["content"].as_str().unwrap_or_default().to_string();
    let finish_reason = choice["finish_reason"].as_str().map(str::to_string);
    let tool_call_frags = delta["tool_calls"].as_array().cloned().unwrap_or_default();
    Ok(Some(StreamFrame::Delta {
        content,
        tool_call_frags,
        finish_reason,
    }))
}

/// Accumulates streaming tool-call fragments (they arrive split across
/// deltas: id/name first, arguments in pieces) keyed by `index`.
#[derive(Default)]
pub struct ToolCallAggregator {
    slots: Vec<ToolCallSlot>,
}

#[derive(Default)]
struct ToolCallSlot {
    id: String,
    name: String,
    arguments: String,
}

impl ToolCallAggregator {
    /// Feed one delta's `tool_calls` array (raw wire values).
    pub fn feed(&mut self, tcs: &[serde_json::Value]) {
        for tc in tcs {
            let idx = tc.get("index").and_then(|i| i.as_u64()).unwrap_or(self.slots.len() as u64) as usize;
            while self.slots.len() <= idx {
                self.slots.push(ToolCallSlot::default());
            }
            let slot = &mut self.slots[idx];
            if let Some(id) = tc["id"].as_str().filter(|s| !s.is_empty()) {
                slot.id = id.to_string();
            }
            if let Some(n) = tc["function"]["name"].as_str().filter(|s| !s.is_empty()) {
                slot.name = n.to_string();
            }
            if let Some(a) = tc["function"]["arguments"].as_str() {
                slot.arguments.push_str(a);
            }
        }
    }

    /// Finish: complete tool calls in index order (incomplete ones —
    /// no name yet — are dropped, same tolerance as the non-streaming
    /// parser).
    pub fn take(self) -> Vec<ToolCall> {
        self.slots
            .into_iter()
            .filter(|s| !s.name.is_empty())
            .map(|s| ToolCall {
                id: if s.id.is_empty() { format!("call_{}", s.name) } else { s.id },
                name: s.name,
                arguments: s.arguments,
            })
            .collect()
    }
}

/// Shared usage parsing (non-streaming tail + streaming tail): lifts
/// the nested reasoning breakdown to the top level.
fn parse_usage_value(v: &serde_json::Value) -> Usage {
    let mut usage: Usage = serde_json::from_value(v.clone()).unwrap_or_default();
    usage.reasoning_tokens =
        v["completion_tokens_details"]["reasoning_tokens"].as_u64().unwrap_or(0);
    usage
}

/// Fallback accounting when an endpoint streams without the usage tail:
/// rough byte-based estimates (ASCII ≈4 chars/token, CJK ≈1.5) — good
/// enough to keep the meter honest in magnitude, never exact.
fn estimate_usage(messages: &[ChatMessage], content: &str) -> Usage {
    let prompt: usize = messages
        .iter()
        .map(|m| m.content.as_deref().map(str::len).unwrap_or(0) + 8)
        .sum();
    Usage {
        prompt_tokens: (prompt / 4) as u64,
        completion_tokens: (content.len() / 3) as u64,
        reasoning_tokens: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- streaming machinery (C1) ---------------------------------------

    #[test]
    fn sse_buffer_reassembles_frames_across_chunks() {
        let mut sse = SseBuffer::default();
        // A frame split mid-JSON, a CRLF frame, keep-alive lines, and a
        // partial multi-byte char at the chunk boundary.
        let a = "data: {\"choices\":[{\"delta\":{\"content\":\"你好";
        let b = "世界\"}]}}\n\ndata: [DONE]\r\n\r\n";
        // Feed a byte at a time to prove mid-UTF-8 safety.
        let mut payloads = Vec::new();
        let mut all = a.as_bytes().to_vec();
        all.extend_from_slice(b.as_bytes());
        for b in &all {
            payloads.extend(sse.feed(std::slice::from_ref(b)));
        }
        assert_eq!(payloads.len(), 2, "{payloads:?}");
        assert_eq!(payloads[0], "{\"choices\":[{\"delta\":{\"content\":\"你好世界\"}]}}");
        assert_eq!(payloads[1], "[DONE]");
    }

    #[test]
    fn parse_stream_payload_content_tool_calls_and_usage_tail() {
        // Content delta.
        let f = parse_stream_payload(
            r#"{"choices":[{"delta":{"content":"hel"},"finish_reason":null}]}"#,
        )
        .unwrap()
        .unwrap();
        match f {
            StreamFrame::Delta { content, tool_call_frags, finish_reason } => {
                assert_eq!(content, "hel");
                assert!(tool_call_frags.is_empty());
                assert_eq!(finish_reason, None);
            }
            other => panic!("{other:?}"),
        }
        // Tool-call fragment delta stays raw.
        let f = parse_stream_payload(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"check_code","arguments":"{\"co"}}]},"finish_reason":null}]}"#,
        )
        .unwrap()
        .unwrap();
        match f {
            StreamFrame::Delta { content, tool_call_frags, .. } => {
                assert!(content.is_empty());
                assert_eq!(tool_call_frags.len(), 1);
                assert_eq!(tool_call_frags[0]["function"]["name"], "check_code");
            }
            other => panic!("{other:?}"),
        }
        // Usage tail (empty choices) + [DONE].
        let f = parse_stream_payload(
            r#"{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5}}"#,
        )
        .unwrap()
        .unwrap();
        assert_eq!(f, StreamFrame::Usage(Usage { prompt_tokens: 10, completion_tokens: 5, reasoning_tokens: 0 }));
        assert!(parse_stream_payload("[DONE]").unwrap().is_none());
    }

    #[test]
    fn tool_call_aggregator_joins_fragments_across_frames() {
        let mut agg = ToolCallAggregator::default();
        agg.feed(&[serde_json::json!({"index":0,"id":"call_1","function":{"name":"check_code"}})]);
        agg.feed(&[serde_json::json!({"index":0,"function":{"arguments":"{\"co"}}),
                   serde_json::json!({"index":0,"function":{"arguments":"de\"}"}})]);
        agg.feed(&[serde_json::json!({"index":1,"id":"call_2","function":{"name":"list_concepts"}})]);
        let calls = agg.take();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].name, "check_code");
        assert_eq!(calls[0].arguments, "{\"code\"}");
        assert_eq!(calls[1].name, "list_concepts");
    }

    #[test]
    fn estimate_usage_is_magnitude_correct() {
        let msgs = vec![ChatMessage::user("a".repeat(400))];
        let u = estimate_usage(&msgs, "b".repeat(300).as_str());
        assert!(u.prompt_tokens >= 100 && u.prompt_tokens <= 200, "{u:?}");
        assert!(u.completion_tokens >= 80 && u.completion_tokens <= 120, "{u:?}");
    }

    /// Live probe (9.4): why did a request with max_tokens=3000 come
    /// back with 19.5k completion tokens on DeepSeek V4 Flash?
    /// Findings (kept as documentation):
    /// A) thinking defaults to ENABLED (effort high): a 1-char answer
    ///    cost 20 completion tokens, 18 of them reasoning.
    /// B) `thinking: {"type":"disabled"}` is honored: 1 completion
    ///    token, zero reasoning.
    /// C) max_tokens does NOT bound the CoT in thinking mode: with
    ///    max_tokens=32 a "hard" question returned 54 completion
    ///    tokens (52 reasoning), finish_reason=stop — the cap applies
    ///    to the final answer only, so a reasoning model can "naturally"
    ///    overshoot any max_tokens we send.
    /// Run: cargo test live_probe_thinking -- --ignored --nocapture
    /// Prints usage metadata only — never the key.
    #[test]
    #[ignore]
    fn live_probe_thinking() {
        let cfg = crate::config::ModelConfig::load().expect("config");
        if cfg.api_key.trim().is_empty() {
            eprintln!("no key configured; skipping");
            return;
        }
        let http = reqwest::blocking::Client::new();
        let url = format!("{}/chat/completions", cfg.endpoint.trim_end_matches('/'));
        let base = serde_json::json!({
            "model": cfg.model,
            "messages": [{"role": "user", "content": "用一个词回答：1+1等于几？"}],
            "max_tokens": 32,
        });
        for (name, extra) in [
            ("A thinking默认(不传) + max_tokens=32", serde_json::json!({})),
            ("B thinking=disabled + max_tokens=32",
             serde_json::json!({"thinking": {"type": "disabled"}})),
        ] {
            let mut body = base.clone();
            for (k, v) in extra.as_object().unwrap() {
                body[k.as_str()] = v.clone();
            }
            let resp = http
                .post(&url)
                .bearer_auth(&cfg.api_key)
                .json(&body)
                .send()
                .expect("request");
            let status = resp.status();
            let text = resp.text().expect("body");
            let v: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
            let choice = &v["choices"][0];
            let reasoning_len = choice["message"]["reasoning_content"].as_str().map(|s| s.len()).unwrap_or(0);
            let content_len = choice["message"]["content"].as_str().map(|s| s.len()).unwrap_or(0);
            println!("== {name}");
            println!("   status={status} finish_reason={:?}", choice["finish_reason"].as_str());
            println!("   usage={}", v["usage"]);
            println!("   content_len={content_len} chars, reasoning_len={reasoning_len} chars");
        }
    }


    #[test]
    fn url_join_variants() {
        assert_eq!(chat_url("https://api.openai.com/v1"), "https://api.openai.com/v1/chat/completions");
        assert_eq!(chat_url("https://api.openai.com/v1/"), "https://api.openai.com/v1/chat/completions");
        assert_eq!(
            chat_url("http://localhost:11434/v1/chat/completions"),
            "http://localhost:11434/v1/chat/completions"
        );
    }

    #[test]
    fn parses_standard_response() {
        let body = r#"{
            "id": "x", "object": "chat.completion",
            "choices": [{ "index": 0, "finish_reason": "stop",
                "message": { "role": "assistant", "content": "你好" } }],
            "usage": { "prompt_tokens": 12, "completion_tokens": 34, "total_tokens": 46 }
        }"#;
        let out = parse_turn_response(body).unwrap();
        assert_eq!(out.content.as_deref(), Some("你好"));
        assert!(out.tool_calls.is_empty());
        assert_eq!(out.usage.prompt_tokens, 12);
        assert_eq!(out.usage.completion_tokens, 34);
    }

    #[test]
    fn parses_tool_call_response() {
        let body = r#"{
            "choices": [{ "finish_reason": "tool_calls", "message": {
                "role": "assistant", "content": null,
                "tool_calls": [{ "id": "call_1", "type": "function",
                    "function": { "name": "check_code", "arguments": "{\"code\":\"fn main(){}\"}" } }]
            } }],
            "usage": { "prompt_tokens": 5, "completion_tokens": 7 }
        }"#;
        let out = parse_turn_response(body).unwrap();
        assert_eq!(out.content, None);
        assert_eq!(out.tool_calls.len(), 1);
        let call = &out.tool_calls[0];
        assert_eq!(call.id, "call_1");
        assert_eq!(call.name, "check_code");
        assert_eq!(call.arguments, r#"{"code":"fn main(){}"}"#);
    }

    #[test]
    fn missing_usage_defaults_to_zero() {
        let body = r#"{ "choices": [{ "message": { "role": "assistant", "content": "ok" } }] }"#;
        let out = parse_turn_response(body).unwrap();
        assert_eq!(out.content.as_deref(), Some("ok"));
        assert_eq!(out.usage.prompt_tokens, 0);
    }

    #[test]
    fn empty_choices_is_an_error() {
        assert!(parse_turn_response(r#"{ "choices": [] }"#).is_err());
        assert!(parse_turn_response("not json").is_err());
    }

    #[test]
    fn thinking_switch_reaches_the_wire() {
        let msgs = [ChatMessage::user("q")];
        let body = build_request_body("m1", &msgs, &[], None, Some(Thinking::Disabled), None);
        assert_eq!(body["thinking"]["type"], "disabled");
        let body = build_request_body("m1", &msgs, &[], None, Some(Thinking::Enabled), None);
        assert_eq!(body["thinking"]["type"], "enabled");
        // Auto/None: the key must be absent for plain endpoints.
        let body = build_request_body("m1", &msgs, &[], None, None, None);
        assert!(body.get("thinking").is_none());
        // Effort rides along only when set; it is endpoint-specific.
        let body = build_request_body("m1", &msgs, &[], None, Some(Thinking::Enabled), Some("low"));
        assert_eq!(body["reasoning_effort"], "low");
    }

    #[test]
    fn reasoning_tokens_lifted_from_details() {
        let out = parse_turn_response(
            r#"{
              "choices": [{"finish_reason": "stop", "message": {"content": "ok"}}],
              "usage": {"prompt_tokens": 10, "completion_tokens": 54,
                        "total_tokens": 64,
                        "completion_tokens_details": {"reasoning_tokens": 52}}
            }"#,
        )
        .unwrap();
        assert_eq!(out.usage.completion_tokens, 54);
        assert_eq!(out.usage.reasoning_tokens, 52);
        // Endpoints without the breakdown stay at 0.
        let out = parse_turn_response(
            r#"{ "choices": [{"finish_reason": "stop", "message": {"content": "ok"}}],
                 "usage": {"prompt_tokens": 1, "completion_tokens": 2, "total_tokens": 3} }"#,
        )
        .unwrap();
        assert_eq!(out.usage.reasoning_tokens, 0);
    }

    #[test]
    fn request_body_maps_wire_shapes() {
        let msgs = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("hi"),
            ChatMessage::assistant_with_calls(vec![ToolCall {
                id: "c1".into(),
                name: "list_concepts".into(),
                arguments: "{}".into(),
            }]),
            ChatMessage::tool_result("c1", "[]"),
        ];
        let tools = vec![Tool {
            name: "list_concepts".into(),
            description: "d".into(),
            parameters: serde_json::json!({"type": "object"}),
        }];
        let body = build_request_body("m1", &msgs, &tools, None, None, None);
        assert_eq!(body["model"], "m1");
        assert_eq!(body["messages"].as_array().unwrap().len(), 4);
        let asst = &body["messages"][2];
        assert_eq!(asst["tool_calls"][0]["type"], "function");
        assert_eq!(asst["tool_calls"][0]["function"]["name"], "list_concepts");
        assert_eq!(asst["tool_calls"][0]["function"]["arguments"], "{}");
        let tool = &body["messages"][3];
        assert_eq!(tool["role"], "tool");
        assert_eq!(tool["tool_call_id"], "c1");
        assert_eq!(body["tools"][0]["function"]["name"], "list_concepts");
        assert_eq!(body["tool_choice"], "auto");
    }

    #[test]
    fn request_without_tools_omits_tool_keys() {
        let body = build_request_body("m1", &[ChatMessage::user("q")], &[], None, None, None);
        assert!(body.get("tools").is_none());
        assert!(body.get("tool_choice").is_none());
    }

    #[test]
    fn tool_call_wire_defaults_are_tolerated() {
        // Some providers omit id / arguments; we must not drop the call.
        let body = r#"{ "choices": [{ "message": { "tool_calls": [
            { "function": { "name": "list_concepts" } }] } }] }"#;
        let out = parse_turn_response(body).unwrap();
        assert_eq!(out.tool_calls.len(), 1);
        assert_eq!(out.tool_calls[0].name, "list_concepts");
        assert_eq!(out.tool_calls[0].arguments, "");
        assert!(out.tool_calls[0].id.starts_with("call_"));
    }
}
