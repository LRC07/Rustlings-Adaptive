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
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
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
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
}

#[derive(Debug, Clone)]
pub struct LlmReply {
    pub content: String,
    pub usage: Usage,
    /// "length" when the completion was cut off by max_tokens (M4.7).
    pub finish_reason: Option<String>,
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
        }
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
        let body = build_request_body(&self.model, messages, tools, max_tokens);
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
}

/// Build the OpenAI-compatible request body from internal message
/// types (flat → wire mapping happens here).
pub fn build_request_body(
    model: &str,
    messages: &[ChatMessage],
    tools: &[Tool],
    max_tokens: Option<u32>,
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
        #[serde(default)]
        usage: Option<Usage>,
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
        usage: resp.usage.unwrap_or_default(),
        finish_reason: choice.finish_reason.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let body = build_request_body("m1", &msgs, &tools, None);
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
        let body = build_request_body("m1", &[ChatMessage::user("q")], &[], None);
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
