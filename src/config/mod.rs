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

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

pub const CONFIG_FILE: &str = "config.toml";

pub fn default_endpoint() -> String {
    "https://api.openai.com/v1".to_string()
}

pub fn default_model() -> String {
    "gpt-4o-mini".to_string()
}

fn default_context_len() -> u32 {
    128_000
}

/// Streaming is ON unless a profile explicitly opts out.
fn default_streaming() -> bool {
    true
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

impl Default for ModelProfile {
    fn default() -> Self {
        Self {
            name: String::new(),
            endpoint: default_endpoint(),
            api_key: String::new(),
            model: default_model(),
            context_len: default_context_len(),
            llm_timeout_secs: None,
            prices: Prices::default(),
            think_mode: ThinkMode::Auto,
            reasoning_effort: None,
            stream: None,
        }
    }
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
///
/// Default is implemented BY HAND (not derived): `context_len: u32`
/// would otherwise default to 0 — `#[serde(default = …)]` does not
/// apply to `..Default::default()` constructors, and a zero context
/// window silently breaks the chat trim (M9d 冒烟抓到).
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// Streaming (C1): true/default = SSE streaming for coach replies;
    /// set false for endpoints that mishandle `stream: true` — the
    /// client also falls back automatically once on stream errors.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
}

/// Which call site a model is picked for (M9l per-scenario routing).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Coach conversation, tool loop, borrowlab interpretation.
    Chat,
    /// Exercise generation, template path (tier-1 pick + slot fill,
    /// tier-2 adaptation) — small structured calls.
    Generate,
    /// Exercise generation, free-form tier-3 draft — long-output
    /// creation with an opposite capability profile (0909_2 反馈:
    /// 职能分开，各自配模型/超时).
    GenerateFree,
    /// Review gate + debrief (four-dimension comparison, explanation
    /// check, challenge hints).
    Review,
}

/// Per-scenario model routing (M9l): each phase may point at a
/// different `[[models]]` profile; unset phases fall back to `active`.
/// Optional entirely — with no `[routing]` table every phase uses the
/// active profile exactly as before. `generate_free` additionally falls
/// back to `generate` when unset (M9u 职能分开): one slot for the whole
/// generate path stays a valid configuration.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Routing {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generate: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generate_free: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<String>,
}

impl Routing {
    pub fn is_empty(&self) -> bool {
        self.chat.is_none()
            && self.generate.is_none()
            && self.generate_free.is_none()
            && self.review.is_none()
    }

    fn name_for(&self, phase: Phase) -> Option<&str> {
        match phase {
            Phase::Chat => self.chat.as_deref(),
            Phase::Generate => self.generate.as_deref(),
            // Unset free slot → the generate slot (single-slot setups
            // keep working unchanged).
            Phase::GenerateFree => {
                self.generate_free.as_deref().or(self.generate.as_deref())
            }
            Phase::Review => self.review.as_deref(),
        }
    }
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
    /// Snapshot of the active profile's streaming switch (default on).
    #[serde(skip, default = "default_streaming")]
    pub streaming: bool,

    // ---- file fields ----
    /// Which profile is in effect; None/default = the first profile.
    /// Never a copy — switching moves this pointer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<String>,

    /// Per-scenario model routing (M9l): which profile serves chat /
    /// generate / review. Unset phases fall back to `active`.
    #[serde(default, skip_serializing_if = "Routing::is_empty")]
    pub routing: Routing,
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
            streaming: default_streaming(),
            active: None,
            routing: Routing::default(),
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
        cfg.clear_invalid_routing();
        Ok(cfg)
    }

    /// Routing names must point at existing profiles — a typo, or a
    /// `[routing]` table copied from someone else's example, must not
    /// brick startup: warn once and fall back to `active` for that
    /// phase. (The /model routing panel still BLOCKS saving an invalid
    /// pick via [`ModelConfig::validate_routing`].)
    fn clear_invalid_routing(&mut self) {
        for (phase, field) in [
            ("chat", &mut self.routing.chat),
            ("generate", &mut self.routing.generate),
            ("generate_free", &mut self.routing.generate_free),
            ("review", &mut self.routing.review),
        ] {
            if let Some(n) = field
                && !self.models.iter().any(|m| &m.name == n)
            {
                eprintln!(
                    "警告：[routing] {phase} = \"{n}\" 没有对应的 [[models]] 档案，该环节回落当前档案（可用：{}）",
                    self.models.iter().map(|m| m.name.as_str()).collect::<Vec<_>>().join("、")
                );
                *field = None;
            }
        }
    }

    /// Strict check used when SAVING from the /model routing panel: a
    /// typo'd name must not land in the file.
    pub fn validate_routing(&self) -> Result<()> {
        for (phase, name) in [
            ("chat", &self.routing.chat),
            ("generate", &self.routing.generate),
            ("generate_free", &self.routing.generate_free),
            ("review", &self.routing.review),
        ] {
            if let Some(n) = name
                && !self.models.iter().any(|m| &m.name == n)
            {
                bail!(
                    "[routing] {phase} = \"{n}\" 没有对应的 [[models]] 档案；可用：{}",
                    self.models.iter().map(|m| m.name.as_str()).collect::<Vec<_>>().join("、")
                );
            }
        }
        Ok(())
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
                    stream: None,
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
    ///
    /// Env overrides (`.env` / `RUSTLINGS_*`) are applied HERE, at the
    /// end of every materialization — they are the highest-priority
    /// GLOBAL overlay and must survive `/model` switching (9.6 复核：
    /// apply_profile re-materializes the snapshot; applying env only in
    /// load() made an .env-provided key silently fall back to the
    /// target profile's key after a switch). key_source follows.
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
                stream: None,
            });
        }
        let name = match &self.active {
            Some(a) if self.models.iter().any(|m| &m.name == a) => a.clone(),
            _ => self.models[0].name.clone(),
        };
        let p = self.models.iter().find(|m| m.name == name).cloned().unwrap();
        self.active = Some(name);
        self.materialize_profile(p);
    }

    /// Copy one profile into the runtime snapshot fields, then apply
    /// the env overlay. Shared by `materialize` (active) and
    /// `snapshot_for_phase` (per-scenario routing, M9l).
    fn materialize_profile(&mut self, p: ModelProfile) {
        self.endpoint = p.endpoint;
        self.api_key = p.api_key;
        self.model = p.model;
        self.context_len = p.context_len;
        self.think_mode = p.think_mode;
        self.reasoning_effort = p.reasoning_effort;
        self.prices = p.prices;
        self.llm_timeout_secs = p.llm_timeout_secs;
        self.streaming = p.stream != Some(false);
        // Env overlay on top of the profile, then derive the key source
        // from whether THIS materialization actually had an env key.
        let env_key = self.apply_env_overrides("RUSTLINGS_");
        self.key_source = if env_key {
            KeySource::Env
        } else if self.api_key.trim().is_empty() {
            KeySource::None
        } else {
            KeySource::ConfigFile
        };
    }

    /// Materialized snapshot for a routed phase (M9l): profile fields
    /// (model/key/endpoint/prices/thinking/stream) come from the
    /// profile the phase routes to; falls back to `self` unchanged when
    /// that phase is not routed (zero-config parity). Clients built
    /// from the result bill under the routed model's own prices.
    pub fn snapshot_for_phase(&self, phase: Phase) -> ModelConfig {
        let Some(name) = self.routing.name_for(phase) else {
            return self.clone();
        };
        let mut out = self.clone();
        match self.models.iter().find(|m| m.name == name) {
            Some(p) => out.materialize_profile(p.clone()),
            // Load-time validation makes this unreachable; be lenient.
            None => out.routing = Routing::default(),
        }
        out
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
        // M9a6: the streaming toggle never persisted — `stream` was the
        // one field sync missed, so broken-SSE users who disabled
        // streaming got it silently re-enabled on restart (or at once
        // when a routed profile re-derived it). Profile semantics: None
        // = default (streaming), Some(false) = off.
        p.stream = if self.streaming { None } else { Some(false) };
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

    /// Add a new profile (M9d): name must be unique and non-empty. The
    /// caller decides whether to switch to it.
    pub fn create_profile(&mut self, p: ModelProfile) -> Result<()> {
        let name = p.name.trim().to_string();
        if name.is_empty() {
            anyhow::bail!("档案名不能为空");
        }
        if self.models.iter().any(|m| m.name == name) {
            anyhow::bail!("已存在同名档案「{name}」");
        }
        self.models.push(ModelProfile { name, ..p });
        Ok(())
    }

    /// Remove a profile (M9d). Refuses to remove the last one (a config
    /// with zero profiles has no active model); returns the removed
    /// profile. The CALLER moves the pointer if the removed profile was
    /// active — re-materializing afterwards.
    pub fn remove_profile(&mut self, name: &str) -> Result<ModelProfile> {
        if self.models.len() <= 1 {
            anyhow::bail!("至少要保留一个模型档案");
        }
        let idx = self
            .models
            .iter()
            .position(|m| m.name == name)
            .with_context(|| format!("没有名为「{name}」的模型档案"))?;
        let removed = self.models.remove(idx);
        if self.active.as_deref() == Some(&removed.name) {
            self.active = None; // falls back to the first profile on materialize
        }
        Ok(removed)
    }

    /// Env overrides: RUSTLINGS_API_KEY / RUSTLINGS_ENDPOINT / RUSTLINGS_MODEL.
    /// Prefixed so tests can exercise the logic without touching real vars.
    /// Returns whether the API key was overridden (drives key_source and
    /// keeps an env-provided key out of the file on save).
    fn apply_env_overrides(&mut self, prefix: &str) -> bool {
        let get = |name: &str| std::env::var(format!("{prefix}{name}")).ok().filter(|v| !v.trim().is_empty());
        let mut key_from_env = false;
        if let Some(k) = get("API_KEY") {
            self.api_key = k.trim().to_string();
            key_from_env = true;
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
        key_from_env
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
    fn routing_falls_back_and_snapshots_profiles() {
        let text = r#"
active = "a"

[routing]
chat = "a"
review = "b"

[[models]]
name = "a"
endpoint = "https://x"
api_key = "k"
model = "m-a"
prices = { input = 0.1, output = 0.2 }

[[models]]
name = "b"
endpoint = "https://y"
api_key = "k"
model = "m-b"
prices = { input = 2.0, output = 3.0 }
think_mode = "on"
reasoning_effort = "low"
"#;
        let mut cfg: ModelConfig = toml::from_str(text).unwrap();
        cfg.materialize(); // production load() always materializes first
        assert_eq!(cfg.routing.chat.as_deref(), Some("a"));
        assert!(cfg.routing.generate.is_none());
        // Unrouted phase → falls back to the active snapshot unchanged.
        let gen_snap = cfg.snapshot_for_phase(Phase::Generate);
        assert_eq!(gen_snap.model, "m-a");
        assert_eq!(gen_snap.prices.input, 0.1);
        // Routed phase → the routed profile's own fields.
        let rev = cfg.snapshot_for_phase(Phase::Review);
        assert_eq!(rev.model, "m-b");
        assert_eq!(rev.prices.input, 2.0);
        assert_eq!(rev.think_mode, ThinkMode::On);
        // 0909_2 职能分开: generate_free unset → rides the generate
        // slot; set → its own profile.
        let free_snap = cfg.snapshot_for_phase(Phase::GenerateFree);
        assert_eq!(free_snap.model, "m-a", "unset free slot rides generate");
        cfg.routing.generate_free = Some("b".into());
        let free_snap = cfg.snapshot_for_phase(Phase::GenerateFree);
        assert_eq!(free_snap.model, "m-b");
        // Routing must survive a save round-trip.
        let out = toml::to_string_pretty(&cfg).unwrap();
        assert!(out.contains("[routing]"), "{out}");
        assert!(out.contains("review = \"b\""));
        assert!(out.contains("generate_free = \"b\""));
    }

    #[test]
    fn routing_rejects_unknown_profile_names() {
        let mut cfg = ModelConfig::default();
        cfg.models.push(ModelProfile {
            name: "a".into(),
            ..Default::default()
        });
        cfg.routing.generate = Some("nope".into());
        assert!(cfg.validate_routing().is_err());
        cfg.routing.generate = Some("a".into());
        assert!(cfg.validate_routing().is_ok());
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
            ..Default::default()
        };
        let key_from_env = cfg.apply_env_overrides("RUSTLINGS_TESTX_");
        unsafe {
            std::env::remove_var("RUSTLINGS_TESTX_API_KEY");
            std::env::remove_var("RUSTLINGS_TESTX_MODEL");
        }
        assert!(key_from_env);
        assert_eq!(cfg.api_key, "env-key-123");
        assert_eq!(cfg.model, "env-model");
    }

    #[test]
    fn create_and_remove_profiles() {
        let mut cfg = ModelConfig::default();
        cfg.materialize(); // one default profile
        // Create: unique name enforced.
        cfg.create_profile(ModelProfile {
            name: "second".into(),
            endpoint: "https://b/v1".into(),
            api_key: "sk-b".into(),
            model: "m-b".into(),
            ..Default::default()
        })
        .unwrap();
        let dup = cfg.create_profile(ModelProfile {
            name: " second ".into(),
            ..Default::default()
        })
        .unwrap_err();
        assert!(dup.to_string().contains("同名"), "{dup}");
        let empty = cfg.create_profile(ModelProfile { name: "  ".into(), ..Default::default() }).unwrap_err();
        assert!(empty.to_string().contains("不能为空"), "{empty}");

        // Remove the ACTIVE profile → pointer falls back to the first.
        cfg.apply_profile("second").unwrap();
        let removed = cfg.remove_profile("second").unwrap();
        assert_eq!(removed.model, "m-b");
        assert_eq!(cfg.models.len(), 1);
        assert_eq!(cfg.active.as_deref(), None, "pointer reset for fallback");
        cfg.materialize();
        assert_eq!(cfg.active.as_deref(), Some("main"));
        assert!(cfg.is_active_profile(&cfg.models[0]));

        // Unknown name → clear error.
        assert!(cfg.remove_profile("nope").is_err());

        // Last profile is protected.
        let mut single = ModelConfig::default();
        single.materialize();
        assert!(single.remove_profile("main").is_err());
    }

    #[test]
    fn env_key_survives_profile_switch() {
        // .env 用户的核心场景（9.6 复核发现）：env 是最高优先级的全局
        // 覆盖——/model 切换重新物化快照后必须仍然生效，而不是回落到
        // 目标档案的 key（空档案会直接变离线）。
        let mut cfg = ModelConfig {
            models: vec![
                ModelProfile {
                    name: "a".into(),
                    endpoint: "https://a/v1".into(),
                    api_key: "sk-a".into(),
                    model: "m-a".into(),
                    ..Default::default()
                },
                ModelProfile {
                    name: "b".into(),
                    endpoint: "https://b/v1".into(),
                    api_key: String::new(),
                    model: "m-b".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        cfg.materialize();
        assert_eq!(cfg.api_key, "sk-a");
        assert_eq!(cfg.key_source, KeySource::ConfigFile);

        // SAFETY: unique test-only var; no other test reads RUSTLINGS_API_KEY.
        unsafe {
            std::env::set_var("RUSTLINGS_API_KEY", "sk-from-env");
        }
        cfg.materialize();
        assert_eq!(cfg.api_key, "sk-from-env");
        assert_eq!(cfg.key_source, KeySource::Env);

        // Switch to profile "b" (empty key): the env key MUST stay.
        cfg.apply_profile("b").unwrap();
        assert_eq!(cfg.api_key, "sk-from-env", "env overlay survives the switch");
        assert_eq!(cfg.key_source, KeySource::Env);
        // And an env key never lands in the file.
        cfg.sync_snapshot_to_active();
        assert!(cfg.models.iter().find(|m| m.name == "b").unwrap().api_key.is_empty());
        unsafe {
            std::env::remove_var("RUSTLINGS_API_KEY");
        }
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
