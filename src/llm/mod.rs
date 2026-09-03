//! OpenAI-compatible chat client (assignment requirement R1: the core
//! LLM call orchestration lives in Rust). Blocking reqwest keeps the
//! REPL simple; async/streaming can be introduced later if needed.

use anyhow::{anyhow, bail, Context, Result};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use std::time::Duration;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);
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
}

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
    pub fn new(endpoint: &str, api_key: &str, model: &str) -> Self {
        let http = Client::builder()
            .timeout(DEFAULT_TIMEOUT)
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

    /// One-shot chat completion for a single user prompt.
    pub fn chat(&self, user_prompt: &str) -> Result<LlmReply> {
        let body = serde_json::json!({
            "model": self.model,
            "messages": [{ "role": "user", "content": user_prompt }],
        });
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
        parse_response(&text)
    }
}

/// Parse an OpenAI-compatible chat completion response body.
fn parse_response(body: &str) -> Result<LlmReply> {
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
    }
    #[derive(Deserialize)]
    struct MessageWire {
        #[serde(default)]
        content: Option<String>,
    }

    let resp: Wire = serde_json::from_str(body).context("解析模型响应 JSON 失败")?;
    let content = resp
        .choices
        .first()
        .and_then(|c| c.message.as_ref())
        .and_then(|m| m.content.clone())
        .ok_or_else(|| anyhow!("模型响应中没有回复内容"))?;
    Ok(LlmReply {
        content,
        usage: resp.usage.unwrap_or_default(),
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
        let reply = parse_response(body).unwrap();
        assert_eq!(reply.content, "你好");
        assert_eq!(reply.usage.prompt_tokens, 12);
        assert_eq!(reply.usage.completion_tokens, 34);
    }

    #[test]
    fn missing_usage_defaults_to_zero() {
        let body = r#"{ "choices": [{ "message": { "role": "assistant", "content": "ok" } }] }"#;
        let reply = parse_response(body).unwrap();
        assert_eq!(reply.content, "ok");
        assert_eq!(reply.usage.prompt_tokens, 0);
        assert_eq!(reply.usage.completion_tokens, 0);
    }

    #[test]
    fn empty_choices_is_an_error() {
        assert!(parse_response(r#"{ "choices": [] }"#).is_err());
        assert!(parse_response("not json").is_err());
    }
}
