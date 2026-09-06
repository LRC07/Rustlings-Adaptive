//! Model configuration (assignment requirement R3): loads from
//! `config.toml` in the project root, with `.env` overrides for secrets
//! (`RUSTLINGS_API_KEY` / `RUSTLINGS_ENDPOINT` / `RUSTLINGS_MODEL`).
//!
//! Format (M9c, 方案 C — single source of truth): the file holds ONLY
//! named model profiles (`[[models]]`) plus a pointer to the active
//! one. Every model appears exactly once; switching is a pointer move,
//! `/config` edits edit the active profile in place. The fields the
//! rest of the code reads (`cfg.model`, `cfg.api_key`, …) are a
//! runtime snapshot materialized from the active profile and skipped
//! in serialization. Legacy layouts (top-level endpoint/model/…) are
//! migrated once, automatically, on load.
//!
//! Priority: built-in defaults < active profile < environment variables.
//! The file holds the user's API key and must never be committed
//! (gitignored); `config.example.toml` documents every default.

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
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub mode: String,
}

impl UiConfig {
    pub fn mode_view(&self) -> bool {
        !self.mode.trim().eq_ignore_ascii_case("scroll")
    }

    fn is_default(&self) -> bool {
        self.mode.is_empty()
    }
}

/// A named model profile (M4.6, reworked M9c): the `[[models]]` array
/// is the ONLY place a model is described. Fields left at their
/// built-in defaults apply those defaults — there is no hidden
/// "inherit from the top level" rule anymore; fill each profile in
/// full (`config.example.toml` documents every default).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ModelProfile {
    /// Switch target: `/model fast`, and the `active` pointer value.
    pub name: String,
    #[serde(default = "default_endpoint")]
    pub endpoint: String,
    /// Required for cloud endpoints; local runtimes (Ollama/vLLM) take
    /// any placeholder (e.g. "ollama").
    #[serde(default)]
    pub api_key: String,
    #[serde(default = "default_model")]
    pub model: String,
    /// Context window (tokens); drives the chat-window trim (M4.3).
    #[serde(default = "default_context_len")]
    pub context_len: u32,
    /// Per-request LLM timeout in seconds; None → built-in default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm_timeout_secs: Option<u64>,
    /// Pricing for the spend meter; zero prices mean "costs are
    /// recorded as 0" — copy the vendor's price page for real numbers.
    #[serde(default)]
    pub prices: Prices,
    /// Per-profile thinking switch (M4.14): reasoning models differ —
    /// DeepSeek V4 on/off × low/high/max, Kimi K3 always-on ×
    /// low/high/max (default max!), GLM on/off with no effort knob.
    /// Legacy `bool` form accepted.
    #[serde(default)]
    pub think_mode: ThinkMode,
    /// None → no effort parameter is sent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
}

/// Model configuration (R3). FILE SHAPE (M9c): `active` + global
/// settings + `[[models]]` — the model fields below are NOT serialized;
/// they are the runtime snapshot of the active profile, materialized by
/// [`ModelConfig::materialize`] so every existing reader
/// (`cfg.model`, `cfg.api_key`, …) keeps working unchanged.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelConfig {
    // ---- runtime snapshot of the ACTIVE profile (skipped in files) ----
    #[serde(skip, default = "default_endpoint")]
    pub endpoint: String,
    #[serde(skip, default)]
    pub api_key: String,
    #[serde(skip, default = "default_model")]
    pub model: String,
    #[serde(skip, default = "default_context_len")]
    pub context_len: u32,
    #[serde(skip, default)]
    pub think_mode: ThinkMode,
    #[serde(skip, default)]
    pub reasoning_effort: Option<String>,
    #[serde(skip, default)]
    pub prices: Prices,
    #[serde(skip, default)]
    pub llm_timeout_secs: Option<u64>,

    // ---- file fields ----
    /// Which profile is in effect; None/default = the first profile.
    /// Never a copy — switching moves this pointer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<String>,
    /// Cumulative spend cap (checked before every LLM call). Global.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<Budget>,
    /// Editor override (design §4.1 chain: $EDITOR → $VISUAL → this →
    /// `code --wait` → vi). Global.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub editor: Option<String>,
    /// UI preferences (M4.2). Global.
    #[serde(default, skip_serializing_if = "UiConfig::is_default")]
    pub ui: UiConfig,
    /// The profiles. Every model lives here, exactly once.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<ModelProfile>,

    #[serde(skip, default)]
    pub key_source: KeySource,
    /// Set when `load()` migrated a legacy layout (caller persists).
    #[serde(skip, default)]
    pub migrated: bool,
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
            llm_timeout_secs: None,
            active: None,
            budget: None,
            editor: None,
            ui: UiConfig::default(),
            models: Vec::new(),
            key_source: KeySource::None,
            migrated: false,
        }
    }
}

impl ModelConfig {
    /// Load config: built-in defaults <- active profile <- env
    /// overrides. Legacy layouts (top-level endpoint/model/…) are
    /// detected and migrated once (`cfg.migrated` tells the caller to
    /// persist the upgraded file). Also loads `.env` from the project
    /// root if present.
    pub fn load() -> Result<Self> {
        dotenvy::dotenv().ok();
        let text = fs::read_to_string(CONFIG_FILE).unwrap_or_default();
        let mut cfg = if text.trim().is_empty() {
            Self::default()
        } else {
            let raw: toml::Value =
                toml::from_str(&text).with_context(|| format!("解析 {CONFIG_FILE} 失败，请检查其语法"))?;
            let legacy = raw.get("endpoint").is_some() || raw.get("model").is_some();
            if legacy {
                Self::from_legacy(&raw)?
            } else {
                raw.try_into()
                    .with_context(|| format!("解析 {CONFIG_FILE} 失败，请检查其语法"))?
            }
        };
        cfg.materialize();
        cfg.key_source = if cfg.api_key.trim().is_empty() {
            KeySource::None
        } else {
            KeySource::ConfigFile
        };
        cfg.apply_env_overrides("RUSTLINGS_");
        Ok(cfg)
    }

    /// Migrate a legacy layout: top-level model fields + `[[models]]`.
    /// The top-level fields become a profile (unless one with the same
    /// endpoint+model already exists); the profiles' "empty = inherit
    /// from the top level" fields are filled in with the top-level
    /// value, so the migrated file is fully explicit (方案 C: no hidden
    /// inheritance). The pointer lands on the profile matching the old
    /// top-level model.
    fn from_legacy(raw: &toml::Value) -> Result<Self> {
        #[derive(Deserialize)]
        struct Legacy {
            #[serde(default = "default_endpoint")]
            endpoint: String,
            #[serde(default)]
            api_key: String,
            #[serde(default = "default_model")]
            model: String,
            #[serde(default = "default_context_len")]
            context_len: u32,
            #[serde(default)]
            think_mode: ThinkMode,
            #[serde(default)]
            reasoning_effort: Option<String>,
            #[serde(default)]
            prices: Prices,
            #[serde(default)]
            budget: Option<Budget>,
            #[serde(default)]
            editor: Option<String>,
            #[serde(default)]
            ui: UiConfig,
            #[serde(default)]
            llm_timeout_secs: Option<u64>,
        }
        let lg: Legacy = raw.clone().try_into().context("旧版 config.toml 解析失败")?;

        let mut models: Vec<ModelProfile> = Vec::new();
        if let Some(arr) = raw.get("models").and_then(|v| v.as_array()) {
            for mv in arr {
                let mut p: ModelProfile =
                    mv.clone().try_into().with_context(|| "解析 [[models]] 档案失败")?;
                // Fill in whatever the legacy profile left empty with the
                // top-level value it used to inherit from.
                let v = mv;
                if v.get("api_key").and_then(|x| x.as_str()).unwrap_or("").trim().is_empty() {
                    p.api_key = lg.api_key.clone();
                }
                if v.get("context_len").is_none() {
                    p.context_len = lg.context_len;
                }
                if v.get("llm_timeout_secs").is_none() {
                    p.llm_timeout_secs = lg.llm_timeout_secs;
                }
                if v.get("prices").is_none() {
                    p.prices = lg.prices.clone();
                }
                if v.get("think_mode").is_none() {
                    p.think_mode = lg.think_mode;
                }
                if v.get("reasoning_effort").is_none() {
                    p.reasoning_effort = lg.reasoning_effort.clone();
                }
                if !models.iter().any(|m| m.name == p.name) {
                    models.push(p);
                }
            }
        }

        // The old top-level config becomes a profile of its own — unless
        // a profile already describes the very same endpoint+model, in
        // which case the pointer just lands there.
        let active = match models.iter().find(|m| m.endpoint == lg.endpoint && m.model == lg.model) {
            Some(m) => m.name.clone(),
            None => {
                let base: String = lg
                    .model
                    .chars()
                    .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
                    .collect();
                let base = base.trim_matches('-').to_string();
                let base = if base.is_empty() { "default".to_string() } else { base };
                let mut name = base.clone();
                let mut n = 2;
                while models.iter().any(|m| m.name == name) {
                    name = format!("{base}-{n}");
                    n += 1;
                }
                models.push(ModelProfile {
                    name: name.clone(),
                    endpoint: lg.endpoint,
                    api_key: lg.api_key,
                    model: lg.model,
                    context_len: lg.context_len,
                    llm_timeout_secs: lg.llm_timeout_secs,
                    prices: lg.prices,
                    think_mode: lg.think_mode,
                    reasoning_effort: lg.reasoning_effort,
                });
                name
            }
        };

        Ok(Self {
            active: Some(active),
            models,
            budget: lg.budget,
            editor: lg.editor,
            ui: lg.ui,
            migrated: true,
            ..Self::default()
        })
    }

    /// Fill the runtime snapshot from the active profile. Guarantees at
    /// least one profile exists and the pointer is valid, so `/model`
    /// always shows exactly one active marker and `/config` always has
    /// a concrete profile to edit.
    pub fn materialize(&mut self) {
        if self.models.is_empty() {
            self.models.push(ModelProfile {
                name: "main".into(),
                endpoint: default_endpoint(),
                api_key: String::new(),
                model: default_model(),
                context_len: default_context_len(),
                llm_timeout_secs: None,
                prices: Prices::default(),
                think_mode: ThinkMode::Auto,
                reasoning_effort: None,
            });
        }
        let name = match &self.active {
            Some(a) if self.models.iter().any(|m| &m.name == a) => a.clone(),
            _ => self.models[0].name.clone(),
        };
        let p = self.models.iter().find(|m| m.name == name).cloned().unwrap();
        self.endpoint = p.endpoint;
        self.api_key = p.api_key;
        self.model = p.model;
        self.context_len = p.context_len;
        self.think_mode = p.think_mode;
        self.reasoning_effort = p.reasoning_effort;
        self.prices = p.prices;
        self.llm_timeout_secs = p.llm_timeout_secs;
        self.active = Some(name);
    }

    /// Write the snapshot back into the active profile, then serialize.
    /// (An env-provided key stays out of the file.)
    pub fn save_to_default_file(&self) -> Result<()> {
        let mut out = self.clone();
        out.sync_snapshot_to_active();
        let text = toml::to_string_pretty(&out).context("序列化配置失败")?;
        fs::write(CONFIG_FILE, text).with_context(|| format!("写入 {CONFIG_FILE} 失败"))
    }

    fn sync_snapshot_to_active(&mut self) {
        let Some(name) = self.active.clone() else { return };
        let Some(p) = self.models.iter_mut().find(|m| m.name == name) else { return };
        p.endpoint = self.endpoint.clone();
        p.model = self.model.clone();
        p.context_len = self.context_len;
        p.think_mode = self.think_mode;
        p.reasoning_effort = self.reasoning_effort.clone();
        p.prices = self.prices.clone();
        p.llm_timeout_secs = self.llm_timeout_secs;
        if self.key_source != KeySource::Env {
            p.api_key = self.api_key.clone();
        }
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
            KeySource::ConfigFile => "来自当前档案",
            KeySource::Env => "来自环境变量",
        }
    }

    pub fn budget_usd(&self) -> Option<f64> {
        self.budget.as_ref().map(|b| b.usd)
    }

    /// Point the active marker at profile `name` and re-materialize the
    /// snapshot. No field copying between profiles — a pointer move.
    pub fn apply_profile(&mut self, name: &str) -> Result<()> {
        if !self.models.iter().any(|m| m.name == name) {
            anyhow::bail!("没有名为「{name}」的模型档案（/model 查看列表）");
        }
        self.active = Some(name.to_string());
        self.materialize();
        Ok(())
    }

    /// Whether profile `p` is the one the pointer names — always at
    /// most one `*` in the listing (9.6 实测：同端点同模型 id 的档案
    /// 曾被字段值比对同时点亮).
    pub fn is_active_profile(&self, p: &ModelProfile) -> bool {
        self.active.as_deref() == Some(p.name.as_str())
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

}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_example_layout() {
        // M9c: the file holds ONLY [[models]] + a pointer + globals.
        let text = r#"
active = "local"
editor = "code --wait"

[budget]
usd = 2.5

[[models]]
name = "local"
endpoint = "http://localhost:11434/v1"
api_key = "ollama"
model = "qwen2.5:7b"
context_len = 32000
prices = { input = 0.0, output = 0.0 }
"#;
        let mut cfg: ModelConfig = toml::from_str(text).unwrap();
        cfg.materialize();
        assert_eq!(cfg.active.as_deref(), Some("local"));
        assert_eq!(cfg.endpoint, "http://localhost:11434/v1");
        assert_eq!(cfg.model, "qwen2.5:7b");
        assert_eq!(cfg.api_key, "ollama");
        assert_eq!(cfg.context_len, 32000);
        assert_eq!(cfg.budget_usd(), Some(2.5));
        assert_eq!(cfg.prices.input, 0.0);
        assert_eq!(cfg.editor.as_deref(), Some("code --wait"));
        // Serialized file never contains the runtime snapshot fields
        // at the top level (inside [[models]] they are the real data).
        let text2 = toml::to_string_pretty(&cfg).unwrap();
        let pre_models = &text2[..text2.find("[[models]]").unwrap()];
        assert!(!pre_models.contains("endpoint"), "snapshot leaked: {text2}");
        assert!(text2.contains("active = \"local\""));
    }

    #[test]
    fn legacy_toml_migrates_to_profiles() {
        // Pre-M9c layout: top-level model fields + [[models]] with
        // "empty = inherit from top level" semantics.
        let text = r#"
endpoint = "https://api.example.com/v1"
api_key = "sk-live"
model = "GLM-5.3-Flash"
context_len = 60000

[budget]
usd = 3.0

[[models]]
name = "cloud"
endpoint = "https://api.example.com/v1"
model = "GLM-5.3-Flash"
api_key = ""

[[models]]
name = "small"
endpoint = "https://other.example.com/v1"
api_key = "sk-other"
model = "mini"
"#;
        let raw: toml::Value = toml::from_str(text).unwrap();
        let mut cfg = ModelConfig::from_legacy(&raw).unwrap();
        assert!(cfg.migrated);
        // Top-level model matches profile "cloud" (same endpoint+model)
        // → pointer lands there; NO duplicate implicit profile.
        assert_eq!(cfg.models.len(), 2);
        assert_eq!(cfg.active.as_deref(), Some("cloud"));
        cfg.materialize();
        // "empty = inherit" fields were filled from the top level.
        let cloud = cfg.models.iter().find(|m| m.name == "cloud").unwrap();
        assert_eq!(cloud.api_key, "sk-live", "inherited key written explicitly");
        assert_eq!(cloud.context_len, 60000);
        // Pointer materialized the snapshot.
        assert_eq!(cfg.api_key, "sk-live");
        assert_eq!(cfg.model, "GLM-5.3-Flash");
        // A legacy layout whose top level matches nothing appends it.
        let text2 = {
            // Replace ONLY the top-level lines (before the first [[models]]).
            let (head, tail) =
                text.split_once("[[models]]").expect("test text has profiles");
            let head = head
                .replace("endpoint = \"https://api.example.com/v1\"", "endpoint = \"https://x.example/v1\"")
                .replace("api_key = \"sk-live\"", "api_key = \"sk-x\"")
                .replace("model = \"GLM-5.3-Flash\"", "model = \"x-model\"");
            format!("{head}[[models]]{tail}")
        };
        let raw2: toml::Value = toml::from_str(&text2).unwrap();
        let cfg2 = ModelConfig::from_legacy(&raw2).unwrap();
        assert_eq!(cfg2.models.len(), 3, "top-level model becomes its own profile");
        assert!(cfg2.models.iter().any(|m| m.name == "x-model"));
        assert_eq!(cfg2.active.as_deref(), Some("x-model"));
    }

    #[test]
    fn env_overrides_win() {
        // SAFETY: single-threaded access to these unique test-only vars.
        unsafe {
            std::env::set_var("RUSTLINGS_TESTX_API_KEY", "env-key-123");
            std::env::set_var("RUSTLINGS_TESTX_MODEL", "env-model");
        }
        let mut cfg = ModelConfig {
            api_key: "file-key".to_string(),
            key_source: KeySource::ConfigFile,
            ..Default::default()
        };
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
        // Files without [ui] still load; default is view. (A stray
        // legacy `model` key is simply ignored — migration handles the
        // real legacy layouts.)
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
        // M9c: profiles are the single source of truth; switching moves
        // the pointer and the snapshot is the FULL profile — no hidden
        // "empty inherits from active" rules anymore.
        let text = r#"
[[models]]
name = "fast"
endpoint = "https://fast.example.com/v1"
api_key = "sk-fast"
model = "fast-model"
llm_timeout_secs = 90
prices = { input = 0.5, output = 1.5 }

[[models]]
name = "local"
endpoint = "http://localhost:11434/v1"
api_key = "ollama"
model = "qwen2.5:7b"
"#;
        let mut cfg: ModelConfig = toml::from_str(text).unwrap();
        assert_eq!(cfg.models.len(), 2);
        assert_eq!(cfg.models[0].prices.input, 0.5);
        // No `active` key → defaults to the first profile.
        assert_eq!(cfg.active, None);
        cfg.materialize();
        assert_eq!(cfg.active.as_deref(), Some("fast"));
        assert_eq!(cfg.api_key, "sk-fast");
        assert_eq!(cfg.llm_timeout_secs, Some(90));
        assert_eq!(cfg.prices.output, 1.5);

        // Switch onto "local": snapshot = that profile, verbatim.
        cfg.apply_profile("local").unwrap();
        assert_eq!(cfg.endpoint, "http://localhost:11434/v1");
        assert_eq!(cfg.model, "qwen2.5:7b");
        assert_eq!(cfg.api_key, "ollama");
        assert_eq!(cfg.llm_timeout_secs, None, "no inherit — profile default applies");
        assert_eq!(cfg.prices.input, default_input_price());
        assert_eq!(cfg.context_len, default_context_len());

        // Unknown name → clear error.
        let err = cfg.apply_profile("nope").unwrap_err().to_string();
        assert!(err.contains("nope"), "{err}");

        // Roundtrip: the pointer is written, snapshot fields are NOT.
        let text2 = toml::to_string_pretty(&cfg).unwrap();
        assert!(text2.contains("active = \"local\""), "{text2}");
        let pre_models = &text2[..text2.find("[[models]]").unwrap()];
        assert!(!pre_models.contains("endpoint"), "snapshot leaked: {text2}");
        let cfg2: ModelConfig = toml::from_str(&text2).unwrap();
        assert_eq!(cfg2.models.len(), 2);
        // Exactly ONE active marker, by NAME (not by field values).
        assert!(cfg2.is_active_profile(&cfg2.models[1]));
        assert!(!cfg2.is_active_profile(&cfg2.models[0]));
        // save() syncs the snapshot back into the ACTIVE profile only.
        let mut cfg3 = cfg2.clone();
        cfg3.api_key = "sk-edited".into();
        cfg3.sync_snapshot_to_active();
        assert_eq!(cfg3.models[1].api_key, "sk-edited");
        assert_eq!(cfg3.models[0].api_key, "sk-fast", "other profile untouched");
    }

    #[test]
    fn old_configs_load_without_profiles() {
        let cfg: ModelConfig = toml::from_str("model = \"m\"").unwrap();
        assert!(cfg.models.is_empty());
    }

    #[test]
    fn think_mode_legacy_bool_and_string_compat() {
        // Legacy bool: `false` behaved exactly like Auto (nothing was
        // ever sent); `true` = force on.
        let pm = |v: &str| -> ThinkMode {
            let cfg: ModelConfig =
                toml::from_str(&format!("[[models]]\nname = \"m\"\nthink_mode = {v}")).unwrap();
            cfg.models[0].think_mode
        };
        assert_eq!(pm("false"), ThinkMode::Auto);
        assert_eq!(pm("true"), ThinkMode::On);
        // New string form.
        for (word, want) in [
            ("auto", ThinkMode::Auto),
            ("on", ThinkMode::On),
            ("off", ThinkMode::Off),
            ("OFF", ThinkMode::Off),
        ] {
            assert_eq!(pm(&format!("\"{word}\"")), want, "word={word}");
        }
        // Wire mapping.
        assert_eq!(ThinkMode::Auto.to_thinking(), None);
        assert_eq!(ThinkMode::On.to_thinking(), Some(crate::llm::Thinking::Enabled));
        assert_eq!(ThinkMode::Off.to_thinking(), Some(crate::llm::Thinking::Disabled));
        // Roundtrip writes the string form back (profile field).
        let cfg: ModelConfig =
            toml::from_str("[[models]]\nname = \"m\"\nthink_mode = \"off\"").unwrap();
        let text = toml::to_string(&cfg).unwrap();
        assert!(text.contains("think_mode = \"off\""), "{text}");
        // Unknown words fall back to Auto instead of failing the load.
        let cfg: ModelConfig =
            toml::from_str("[[models]]\nname = \"m\"\nthink_mode = \"whatever\"").unwrap();
        assert_eq!(cfg.models[0].think_mode, ThinkMode::Auto);
    }
}
