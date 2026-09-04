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

/// Thinking-mode switch (M4.12, R3): hybrid-reasoning models (e.g.
/// DeepSeek V4) think by DEFAULT at effort `high`, and the chain of
/// thought is billed as completion tokens without being bounded by
/// `max_tokens` (live-probed 9.4, see 复盘 §4.5). So "not sending
/// anything" can silently multiply cost.
///
/// - `Auto`（默认）：不发送参数，沿用端点默认（兼容旧配置的 `false`）
/// - `On`：显式开思考（兼容旧配置的 `true`）
/// - `Off`：显式关思考——出题/问答的成本与延迟立刻数倍下降
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThinkMode {
    #[default]
    Auto,
    On,
    Off,
}

impl ThinkMode {
    pub fn label_cn(self) -> &'static str {
        match self {
            Self::Auto => "auto（沿用端点默认）",
            Self::On => "on（强制开思考）",
            Self::Off => "off（强制关思考，省 token）",
        }
    }

    pub fn from_word(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" | "默认" => Some(Self::Auto),
            "on" | "true" | "yes" | "开" => Some(Self::On),
            "off" | "false" | "no" | "关" => Some(Self::Off),
            _ => None,
        }
    }

    /// Wire mapping: `Auto` sends nothing.
    pub fn to_thinking(self) -> Option<crate::llm::Thinking> {
        match self {
            Self::Auto => None,
            Self::On => Some(crate::llm::Thinking::Enabled),
            Self::Off => Some(crate::llm::Thinking::Disabled),
        }
    }
}

impl Serialize for ThinkMode {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let s = match self {
            Self::Auto => "auto",
            Self::On => "on",
            Self::Off => "off",
        };
        serializer.serialize_str(s)
    }
}

impl<'de> Deserialize<'de> for ThinkMode {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // Accept the legacy bool (`false` behaved exactly like Auto:
        // nothing was ever sent) and the new string form.
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Bool(bool),
            Str(String),
        }
        Ok(match Raw::deserialize(deserializer)? {
            Raw::Bool(false) => Self::Auto,
            Raw::Bool(true) => Self::On,
            Raw::Str(s) => Self::from_word(&s).unwrap_or(Self::Auto),
        })
    }
}

/// Editor override source of truth is `editor`; UI preferences live
/// here (M4.2): `mode` = "view" (viewport repaint, default) or "scroll"
/// (plain scrolling transcript).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UiConfig {
    #[serde(default)]
    pub mode: String,
}

impl UiConfig {
    pub fn mode_view(&self) -> bool {
        !self.mode.trim().eq_ignore_ascii_case("scroll")
    }
}

/// A named switchable model profile (M4.6): the `[[models]]` array in
/// config.toml. `/model <name>` applies one onto the active config and
/// rebuilds the client — the fast way to hop between endpoints.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ModelProfile {
    /// Switch target: `/model fast`.
    pub name: String,
    #[serde(default = "default_endpoint")]
    pub endpoint: String,
    /// Empty → the active config's key is kept (shared-key setups).
    #[serde(default)]
    pub api_key: String,
    #[serde(default = "default_model")]
    pub model: String,
    /// Empty → the active timeout is kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm_timeout_secs: Option<u64>,
    /// Empty → the active [prices] is kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prices: Option<Prices>,
}

/// Model configuration — R3: endpoint / key / model / context length /
/// thinking mode / prices / budget. `context_len` drives the window
/// (M4.3); `think_mode` drives the wire (M4.12).
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
    pub think_mode: ThinkMode,
    /// Reasoning effort when thinking is on (M4.12): "low" | "high" |
    /// "max"（DeepSeek V4 语义；缺省不发 = 端点默认 high）。调节思考
    /// 深度以平衡时间成本与产出质量。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub prices: Prices,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<Budget>,
    /// Editor override (design §4.1 chain: $EDITOR → $VISUAL → this →
    /// `code --wait` → vi). Set from the `[c]` config page or by hand.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub editor: Option<String>,
    /// UI preferences (M4.2). Missing in old config files → default.
    #[serde(default)]
    pub ui: UiConfig,
    /// Per-request LLM timeout in seconds (R3; slow endpoints can raise
    /// it). Missing in old configs → default 240.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm_timeout_secs: Option<u64>,
    /// Named model profiles (M4.6): `/model <name>` switches among
    /// them. Missing in old configs → empty list.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<ModelProfile>,
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
            think_mode: ThinkMode::Auto,
            reasoning_effort: None,
            prices: Prices::default(),
            budget: None,
            editor: None,
            ui: UiConfig::default(),
            llm_timeout_secs: None,
            models: Vec::new(),
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
        if let Some(t) = get("THINK_MODE") {
            self.think_mode = ThinkMode::from_word(&t).unwrap_or(ThinkMode::Auto);
        }
        if let Some(e) = get("REASONING_EFFORT") {
            let e = e.trim().to_ascii_lowercase();
            if matches!(e.as_str(), "low" | "high" | "max") {
                self.reasoning_effort = Some(e);
            }
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

    /// Switch the active config onto profile `name` (M4.6): endpoint /
    /// model / timeout are always applied; an empty profile api_key
    /// keeps the current key; missing prices keep the current ones.
    pub fn apply_profile(&mut self, name: &str) -> Result<()> {
        let p = self
            .models
            .iter()
            .find(|m| m.name == name)
            .with_context(|| format!("没有名为「{name}」的模型档案"))?
            .clone();
        self.endpoint = p.endpoint;
        self.model = p.model;
        if !p.api_key.trim().is_empty() {
            self.api_key = p.api_key;
            self.key_source = KeySource::ConfigFile;
        }
        if let Some(t) = p.llm_timeout_secs {
            self.llm_timeout_secs = Some(t);
        }
        if let Some(pr) = p.prices {
            self.prices = pr;
        }
        Ok(())
    }

    /// Whether profile `p` is (field-wise) the active configuration —
    /// used for the (当前) marker in /model listing.
    pub fn is_active_profile(&self, p: &ModelProfile) -> bool {
        let key_matches = p.api_key.trim().is_empty() || p.api_key == self.api_key;
        p.endpoint == self.endpoint && p.model == self.model && key_matches
    }

    /// M4.8: make sure the ACTIVE configuration has a profile identity.
    /// Without this, `/model` lists only the user-declared `[[models]]`
    /// and switching is destructive (the original top-level config has
    /// no name to switch back to). Returns true when an implicit
    /// profile was appended (caller persists).
    pub fn ensure_active_profile_recorded(&mut self) -> bool {
        // Nothing meaningful to record (no key configured at all).
        if self.api_key.trim().is_empty() {
            return false;
        }
        if self.models.iter().any(|m| self.is_active_profile(m)) {
            return false;
        }
        // Name from the model id, made list-friendly and unique.
        let base: String = {
            let n: String = self
                .model
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
                .collect();
            let n = n.trim_matches('-').to_string();
            if n.is_empty() { "default".to_string() } else { n }
        };
        let mut name = base.clone();
        let mut n = 2;
        while self.models.iter().any(|m| m.name == name) {
            name = format!("{base}-{n}");
            n += 1;
        }
        self.models.push(ModelProfile {
            name,
            endpoint: self.endpoint.clone(),
            api_key: self.api_key.clone(),
            model: self.model.clone(),
            llm_timeout_secs: self.llm_timeout_secs,
            prices: Some(self.prices.clone()),
        });
        true
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
editor = "code --wait"

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
        assert_eq!(cfg.editor.as_deref(), Some("code --wait"));
    }

    #[test]
    fn defaults_fill_missing_fields() {
        let cfg: ModelConfig = toml::from_str("model = \"m1\"").unwrap();
        assert_eq!(cfg.model, "m1");
        assert_eq!(cfg.endpoint, default_endpoint());
        assert_eq!(cfg.context_len, default_context_len());
        assert_eq!(cfg.budget_usd(), None);
        assert_eq!(cfg.editor, None);
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

    #[test]
    fn ui_mode_defaults_and_roundtrips() {
        // Old config files without [ui] still load; default is view.
        let cfg: ModelConfig = toml::from_str("model = \"m\"").unwrap();
        assert!(cfg.ui.mode_view());
        // Explicit scroll roundtrips.
        let cfg: ModelConfig = toml::from_str("model = \"m\"\n\n[ui]\nmode = \"scroll\"\n").unwrap();
        assert!(!cfg.ui.mode_view());
        let text = toml::to_string_pretty(&cfg).unwrap();
        let cfg2: ModelConfig = toml::from_str(&text).unwrap();
        assert!(!cfg2.ui.mode_view());
        // Unknown values fall back to view.
        let cfg: ModelConfig = toml::from_str("[ui]\nmode = \"fancy\"\n").unwrap();
        assert!(cfg.ui.mode_view());
    }

    #[test]
    fn profiles_parse_apply_and_roundtrip() {
        let text = r#"
endpoint = "https://slow.example.com/v1"
api_key = "sk-active"
model = "slow-model"

[[models]]
name = "fast"
endpoint = "https://fast.example.com/v1"
api_key = "sk-fast"
model = "fast-model"
llm_timeout_secs = 90

[models.prices]
input = 0.5
output = 1.5

[[models]]
name = "local"
endpoint = "http://localhost:11434/v1"
model = "qwen2.5:7b"
"#;
        let mut cfg: ModelConfig = toml::from_str(text).unwrap();
        assert_eq!(cfg.models.len(), 2);
        assert_eq!(cfg.models[0].name, "fast");
        assert_eq!(cfg.models[0].prices.as_ref().unwrap().input, 0.5);
        assert!(cfg.models[1].api_key.is_empty());

        // Switch onto "local": empty api_key keeps the active key;
        // prices/timeout stay as they were.
        cfg.apply_profile("local").unwrap();
        assert_eq!(cfg.endpoint, "http://localhost:11434/v1");
        assert_eq!(cfg.model, "qwen2.5:7b");
        assert_eq!(cfg.api_key, "sk-active");
        assert_eq!(cfg.llm_timeout_secs, None);
        assert_eq!(cfg.prices.input, 0.15); // untouched default

        // Switch onto "fast": everything overridden.
        cfg.apply_profile("fast").unwrap();
        assert_eq!(cfg.endpoint, "https://fast.example.com/v1");
        assert_eq!(cfg.model, "fast-model");
        assert_eq!(cfg.api_key, "sk-fast");
        assert_eq!(cfg.key_source, KeySource::ConfigFile);
        assert_eq!(cfg.llm_timeout_secs, Some(90));
        assert_eq!(cfg.prices.output, 1.5);

        // Unknown name → clear error.
        let err = cfg.apply_profile("nope").unwrap_err().to_string();
        assert!(err.contains("nope"), "{err}");

        // Roundtrip keeps profiles.
        let text2 = toml::to_string_pretty(&cfg).unwrap();
        let cfg2: ModelConfig = toml::from_str(&text2).unwrap();
        assert_eq!(cfg2.models.len(), 2);
        assert_eq!(cfg2.models[0].name, "fast");
        // Active markers.
        assert!(cfg2.is_active_profile(&cfg2.models[0]));
        assert!(!cfg2.is_active_profile(&cfg2.models[1]));
    }

    #[test]
    fn old_configs_load_without_profiles() {
        let cfg: ModelConfig = toml::from_str("model = \"m\"").unwrap();
        assert!(cfg.models.is_empty());
    }

    #[test]
    fn active_config_gets_a_profile_identity() {
        let mut cfg = ModelConfig {
            api_key: "sk-live".into(),
            endpoint: "https://api.example.com/v1".into(),
            model: "GLM-5.3-Flash".into(),
            ..Default::default()
        };
        // Not yet recorded → records an implicit profile named after
        // the model.
        assert!(cfg.ensure_active_profile_recorded());
        assert_eq!(cfg.models.len(), 1);
        let p = &cfg.models[0];
        assert_eq!(p.name, "GLM-5-3-Flash");
        assert_eq!(p.endpoint, "https://api.example.com/v1");
        assert_eq!(p.api_key, "sk-live");
        assert!(cfg.is_active_profile(p));

        // Recorded → idempotent.
        assert!(!cfg.ensure_active_profile_recorded());

        // Empty key (nothing configured) → never recorded.
        let mut cfg = ModelConfig::default();
        assert!(!cfg.ensure_active_profile_recorded());

        // Name collision → suffix.
        let mut cfg = ModelConfig {
            api_key: "sk-live".into(),
            model: "fast".into(),
            ..Default::default()
        };
        cfg.models.push(ModelProfile {
            name: "fast".into(),
            endpoint: "https://other/v1".into(),
            api_key: "sk-other".into(),
            model: "fast".into(),
            llm_timeout_secs: None,
            prices: None,
        });
        assert!(cfg.ensure_active_profile_recorded());
        assert_eq!(cfg.models[1].name, "fast-2");
    }

    #[test]
    fn think_mode_legacy_bool_and_string_compat() {
        // Legacy bool: `false` behaved exactly like Auto (nothing was
        // ever sent); `true` = force on.
        let cfg: ModelConfig = toml::from_str("think_mode = false").unwrap();
        assert_eq!(cfg.think_mode, ThinkMode::Auto);
        let cfg: ModelConfig = toml::from_str("think_mode = true").unwrap();
        assert_eq!(cfg.think_mode, ThinkMode::On);
        // New string form.
        for (word, want) in [
            ("auto", ThinkMode::Auto),
            ("on", ThinkMode::On),
            ("off", ThinkMode::Off),
            ("OFF", ThinkMode::Off),
        ] {
            let cfg: ModelConfig = toml::from_str(&format!("think_mode = \"{word}\"")).unwrap();
            assert_eq!(cfg.think_mode, want, "word={word}");
        }
        // Wire mapping.
        assert_eq!(ThinkMode::Auto.to_thinking(), None);
        assert_eq!(ThinkMode::On.to_thinking(), Some(crate::llm::Thinking::Enabled));
        assert_eq!(ThinkMode::Off.to_thinking(), Some(crate::llm::Thinking::Disabled));
        // Roundtrip writes the string form back.
        let cfg: ModelConfig = toml::from_str("think_mode = \"off\"").unwrap();
        let text = toml::to_string(&cfg).unwrap();
        assert!(text.contains("think_mode = \"off\""), "{text}");
        // Unknown words fall back to Auto instead of failing the load.
        let cfg: ModelConfig = toml::from_str("think_mode = \"whatever\"").unwrap();
        assert_eq!(cfg.think_mode, ThinkMode::Auto);
    }
}
