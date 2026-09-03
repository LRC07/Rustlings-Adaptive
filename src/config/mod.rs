//! Model configuration (assignment requirement R3): loads from
//! `config.toml` in the project root, with `.env` overrides for secrets
//! (`RUSTLINGS_API_KEY` / `RUSTLINGS_ENDPOINT` / `RUSTLINGS_MODEL`).
//!
//! Priority: built-in defaults < config.toml < environment variables.
//! The file `config.toml` holds the user's API key and must never be
//! committed (it is gitignored); `config.example.toml` documents it.

use std::fs;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const CONFIG_FILE: &str = "config.toml";

fn default_endpoint() -> String {
    "https://api.openai.com/v1".to_string()
}

fn default_model() -> String {
    "gpt-4o-mini".to_string()
}

fn default_context_len() -> u32 {
    128_000
}

fn default_input_price() -> f64 {
    0.15
}

fn default_output_price() -> f64 {
    0.6
}

/// Prices in USD per 1M tokens.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Prices {
    #[serde(default = "default_input_price")]
    pub input: f64,
    #[serde(default = "default_output_price")]
    pub output: f64,
}

impl Default for Prices {
    fn default() -> Self {
        Self {
            input: default_input_price(),
            output: default_output_price(),
        }
    }
}

/// Cumulative spend cap in USD (checked before every LLM call).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Budget {
    pub usd: f64,
}

/// Where the effective API key came from (for display only).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum KeySource {
    #[default]
    None,
    ConfigFile,
    Env,
}

/// Model configuration — R3: endpoint / key / model / context length /
/// thinking mode / prices / budget. `think_mode` and `context_len` are
/// stored now and consumed by later milestones.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelConfig {
    #[serde(default = "default_endpoint")]
    pub endpoint: String,
    #[serde(default)]
    pub api_key: String,
    #[serde(default = "default_model")]
    pub model: String,
    #[serde(default = "default_context_len")]
    pub context_len: u32,
    #[serde(default)]
    pub think_mode: bool,
    #[serde(default)]
    pub prices: Prices,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<Budget>,
    #[serde(skip, default)]
    pub key_source: KeySource,
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            endpoint: default_endpoint(),
            api_key: String::new(),
            model: default_model(),
            context_len: default_context_len(),
            think_mode: false,
            prices: Prices::default(),
            budget: None,
            key_source: KeySource::None,
        }
    }
}

impl ModelConfig {
    /// Load config: defaults <- config.toml <- env overrides.
    /// Also loads `.env` from the project root if present.
    pub fn load() -> Result<Self> {
        dotenvy::dotenv().ok();
        let mut cfg = match fs::read_to_string(CONFIG_FILE) {
            Ok(text) => toml::from_str::<ModelConfig>(&text)
                .with_context(|| format!("解析 {CONFIG_FILE} 失败，请检查其语法"))?,
            Err(_) => ModelConfig::default(),
        };
        cfg.key_source = if cfg.api_key.trim().is_empty() {
            KeySource::None
        } else {
            KeySource::ConfigFile
        };
        cfg.apply_env_overrides("RUSTLINGS_");
        Ok(cfg)
    }

    /// Env overrides: RUSTLINGS_API_KEY / RUSTLINGS_ENDPOINT / RUSTLINGS_MODEL.
    /// Prefixed so tests can exercise the logic without touching real vars.
    fn apply_env_overrides(&mut self, prefix: &str) {
        let get = |name: &str| std::env::var(format!("{prefix}{name}")).ok().filter(|v| !v.trim().is_empty());
        if let Some(k) = get("API_KEY") {
            self.api_key = k.trim().to_string();
            self.key_source = KeySource::Env;
        }
        if let Some(e) = get("ENDPOINT") {
            self.endpoint = e.trim().to_string();
        }
        if let Some(m) = get("MODEL") {
            self.model = m.trim().to_string();
        }
    }

    /// Write the current config back to `config.toml` (CLI settings page).
    pub fn save_to_default_file(&self) -> Result<()> {
        let text = toml::to_string_pretty(self).context("序列化配置失败")?;
        fs::write(CONFIG_FILE, text).with_context(|| format!("写入 {CONFIG_FILE} 失败"))
    }

    /// Masked API key for display: `sk-****abcd`.
    pub fn masked_key(&self) -> String {
        if self.api_key.is_empty() {
            return "未设置".to_string();
        }
        let chars: Vec<char> = self.api_key.chars().collect();
        if chars.len() <= 8 {
            "****".to_string()
        } else {
            format!("{}****{}", chars[..3].iter().collect::<String>(), chars[chars.len() - 4..].iter().collect::<String>())
        }
    }

    pub fn key_source_cn(&self) -> &'static str {
        match self.key_source {
            KeySource::None => "未设置",
            KeySource::ConfigFile => "来自 config.toml",
            KeySource::Env => "来自环境变量",
        }
    }

    pub fn budget_usd(&self) -> Option<f64> {
        self.budget.as_ref().map(|b| b.usd)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_example_layout() {
        let text = r#"
endpoint = "http://localhost:11434/v1"
api_key = "ollama"
model = "qwen2.5:7b"
context_len = 32000
think_mode = false

[prices]
input = 0.0
output = 0.0

[budget]
usd = 2.5
"#;
        let cfg: ModelConfig = toml::from_str(text).unwrap();
        assert_eq!(cfg.endpoint, "http://localhost:11434/v1");
        assert_eq!(cfg.model, "qwen2.5:7b");
        assert_eq!(cfg.context_len, 32000);
        assert_eq!(cfg.budget_usd(), Some(2.5));
        assert_eq!(cfg.prices.input, 0.0);
    }

    #[test]
    fn defaults_fill_missing_fields() {
        let cfg: ModelConfig = toml::from_str("model = \"m1\"").unwrap();
        assert_eq!(cfg.model, "m1");
        assert_eq!(cfg.endpoint, default_endpoint());
        assert_eq!(cfg.context_len, default_context_len());
        assert_eq!(cfg.budget_usd(), None);
        assert_eq!(cfg.prices.input, default_input_price());
    }

    #[test]
    fn env_overrides_win() {
        // SAFETY: single-threaded access to these unique test-only vars.
        unsafe {
            std::env::set_var("RUSTLINGS_TESTX_API_KEY", "env-key-123");
            std::env::set_var("RUSTLINGS_TESTX_MODEL", "env-model");
        }
        let mut cfg = ModelConfig::default();
        cfg.api_key = "file-key".to_string();
        cfg.key_source = KeySource::ConfigFile;
        cfg.apply_env_overrides("RUSTLINGS_TESTX_");
        unsafe {
            std::env::remove_var("RUSTLINGS_TESTX_API_KEY");
            std::env::remove_var("RUSTLINGS_TESTX_MODEL");
        }
        assert_eq!(cfg.api_key, "env-key-123");
        assert_eq!(cfg.key_source, KeySource::Env);
        assert_eq!(cfg.model, "env-model");
    }

    #[test]
    fn masked_key_never_leaks() {
        let mut cfg = ModelConfig::default();
        assert_eq!(cfg.masked_key(), "未设置");
        cfg.api_key = "short".to_string();
        assert_eq!(cfg.masked_key(), "****");
        cfg.api_key = "sk-1234567890abcdef".to_string();
        assert_eq!(cfg.masked_key(), "sk-****cdef");
    }
}
