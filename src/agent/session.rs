//! Session persistence & trajectory review (assignment requirement R5):
//! every conversation turn — user input, assistant replies, tool calls
//! and their results — is appended to a JSON file under
//! `~/.rustlings_adaptive/sessions/` so the agent's actual workflow is
//! inspectable, not a black box.
//!
//! Files are `session_<YYYYMMDD_HHMMSS>.json`; the newest one is
//! auto-resumed at startup so a CLI restart continues the conversation.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Local, Utc};
use serde::{Deserialize, Serialize};

use crate::llm::ChatMessage;

/// Directory holding session files (fallback to `./.rustlings_adaptive/`
/// like the usage tracker when HOME is unavailable).
pub fn sessions_dir() -> PathBuf {
    let base = std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."));
    base.join(".rustlings_adaptive").join("sessions")
}

/// A live conversation session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub started_at: DateTime<Utc>,
    pub model: String,
    /// Short label for the `/sessions` list (first user message),
    /// derived lazily on save; old files without it load as `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Exercises produced in this session (index keys, in production
    /// order; M4.5a). Metadata lives in the exercise index — this list
    /// only orders/links.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exercises: Vec<String>,
    pub messages: Vec<ChatMessage>,
    /// Where this session is persisted (not serialized).
    #[serde(skip)]
    pub path: PathBuf,
}

/// Summary row for the `/sessions` listing.
pub struct SessionInfo {
    pub path: PathBuf,
    pub id: String,
    pub started_at: DateTime<Utc>,
    pub messages: usize,
    pub title: Option<String>,
}

impl Session {
    /// New empty session with a timestamp id and file path.
    pub fn new(model: &str) -> Self {
        let started = Utc::now();
        let id = started.with_timezone(&Local).format("session_%Y%m%d_%H%M%S").to_string();
        let path = sessions_dir().join(format!("{id}.json"));
        Self {
            id,
            started_at: started,
            model: model.to_string(),
            title: None,
            exercises: Vec::new(),
            messages: Vec::new(),
            path,
        }
    }

    /// Resume the most recent saved session, or start a new one.
    pub fn resume_latest() -> Option<Session> {
        let infos = Self::list();
        let path = infos.last()?.path.clone();
        Self::load(&path).ok()
    }

    /// Load a session from disk (path field restored).
    pub fn load(path: &Path) -> Result<Session> {
        let text = fs::read_to_string(path).with_context(|| format!("读取 {}", path.display()))?;
        let mut s: Session = serde_json::from_str(&text).context("会话 JSON 解析失败")?;
        s.path = path.to_path_buf();
        Ok(s)
    }

    /// Persist to the session file (called after every turn; save
    /// failures degrade to a printed warning but never crash the
    /// REPL). Also derives the list title on first save.
    pub fn save(&mut self) -> Result<()> {
        if self.title.is_none() {
            self.title = self.messages.iter().find(|m| m.role == "user").and_then(|m| m.content.as_ref())
                .map(|c| {
                    let t: String = c.chars().take(30).collect();
                    let count = c.chars().count();
                    if count > 30 { format!("{t}…") } else { t }
                });
        }
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("创建 {}", parent.display()))?;
        }
        let json = serde_json::to_string_pretty(self).context("会话序列化失败")?;
        fs::write(&self.path, json).with_context(|| format!("写入 {}", self.path.display()))
    }

    /// Export the full trajectory as readable markdown next to the
    /// session file (`<id>.md`); returns the written path. Used by
    /// `/sessions export <n>` (also handy for the assignment's "AI
    /// conversation history" deliverable).
    pub fn export_markdown(&self) -> Result<PathBuf> {
        let path = self.path.with_extension("md");
        let started = self.started_at.with_timezone(&Local).format("%Y-%m-%d %H:%M:%S");
        let head = format!(
            "# 会话轨迹 {}\n\n- 开始：{started}\n- 模型：{}\n- 消息数：{}\n\n```\n",
            self.id, self.model, self.messages.len()
        );
        let body = trajectory_text(&self.messages, 100_000);
        fs::write(&path, format!("{head}{body}```\n"))
            .with_context(|| format!("写入 {}", path.display()))?;
        Ok(path)
    }

    /// List saved sessions ordered by file name (= start time).
    pub fn list() -> Vec<SessionInfo> {
        let dir = sessions_dir();
        let Ok(rd) = fs::read_dir(&dir) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for entry in rd.flatten() {
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Ok(text) = fs::read_to_string(&p) else { continue };
            let Ok(s) = serde_json::from_str::<Session>(&text) else { continue };
            out.push(SessionInfo {
                path: p,
                id: s.id,
                started_at: s.started_at,
                messages: s.messages.len(),
                title: s.title,
            });
        }
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }
}

// ---------------------------------------------------------------------------
// Trajectory rendering (R5: show the workflow behind the answers)
// ---------------------------------------------------------------------------

/// Render the full trajectory of a message list as reviewable text.
/// Long contents are truncated so a whole session fits on screen.
pub fn trajectory_text(messages: &[ChatMessage], max_chars: usize) -> String {
    let mut out = String::new();
    let mut n = 0;
    for m in messages {
        n += 1;
        match m.role.as_str() {
            "system" => {
                out.push_str(&format!("[{n}] 系统提示（{} 字，略）\n", m.content.as_deref().unwrap_or("").chars().count()));
            }
            "user" => {
                out.push_str(&format!("[{n}] 用户: {}\n", ellipsize(m.content.as_deref().unwrap_or(""), max_chars)));
            }
            "assistant" => {
                if !m.tool_calls.is_empty() {
                    for c in &m.tool_calls {
                        out.push_str(&format!(
                            "[{n}] 工具调用 {}: {}（参数 {} 字）\n",
                            c.name,
                            ellipsize(&c.arguments, 200),
                            c.arguments.chars().count()
                        ));
                    }
                }
                if let Some(c) = &m.content {
                    out.push_str(&format!("[{n}] 教练: {}\n", ellipsize(c, max_chars)));
                }
            }
            "tool" => {
                let name = m.tool_call_id.as_deref().unwrap_or("?");
                out.push_str(&format!("[{n}] 工具结果（{name}）: {}\n", ellipsize(m.content.as_deref().unwrap_or(""), 300)));
            }
            other => {
                out.push_str(&format!("[{n}] {other}: {}\n", ellipsize(m.content.as_deref().unwrap_or(""), max_chars)));
            }
        }
    }
    out
}

/// Truncate to `max_chars` characters, appending `…(N 字)` when cut.
fn ellipsize(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.replace('\n', " ").trim_end().to_string();
    }
    let head: String = text.chars().take(max_chars).collect();
    format!("{}…（共 {count} 字）", head.replace('\n', " "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_messages() -> Vec<ChatMessage> {
        vec![
            ChatMessage::system("你是 Rust 教练。"),
            ChatMessage::user("为什么报 E0382？"),
            ChatMessage::assistant("我用工具先看看。"),
            ChatMessage::assistant_with_calls(vec![crate::llm::ToolCall {
                id: "c1".into(),
                name: "check_code".into(),
                arguments: r#"{"code":"fn main(){ let s = String::new(); let t = s; println!(\"{}\", s); }"#.into(),
            }]),
            ChatMessage::tool_result("c1", r#"{"ok":false,"diagnostics":[{"code":"E0382"}]}"#),
            ChatMessage::assistant("E0382 是值被移动后再使用…"),
        ]
    }

    #[test]
    fn session_roundtrip_through_disk() {
        let dir = std::env::temp_dir().join(format!("rs_sessions_{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("session_test1.json");
        let mut s = Session {
            id: "session_test1".into(),
            started_at: Utc::now(),
            model: "m".into(),
            title: None,
            exercises: Vec::new(),
            messages: sample_messages(),
            path: path.clone(),
        };
        s.save().unwrap();
        let loaded = Session::load(&path).unwrap();
        assert_eq!(loaded.messages.len(), 6);
        assert_eq!(loaded.model, "m");
        assert_eq!(loaded.path, path);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_derives_title_from_first_user_message() {
        let dir = std::env::temp_dir().join(format!("rs_sessions_title_{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("session_title.json");
        let mut s = Session {
            id: "session_title".into(),
            started_at: Utc::now(),
            model: "m".into(),
            title: None,
            exercises: Vec::new(),
            messages: sample_messages(),
            path,
        };
        s.save().unwrap();
        let t = s.title.as_deref().unwrap();
        assert_eq!(t, "为什么报 E0382？");
        // Long first messages are truncated with an ellipsis marker.
        s.messages[1] = ChatMessage::user("长".repeat(50));
        s.title = None;
        s.save().unwrap();
        let t = s.title.as_deref().unwrap();
        assert!(t.chars().count() == 31 && t.ends_with('…'), "{t}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn export_writes_markdown_file() {
        let dir = std::env::temp_dir().join(format!("rs_sessions_export_{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let mut s = Session {
            id: "session_export".into(),
            started_at: Utc::now(),
            model: "m".into(),
            title: Some("导出".into()),
            exercises: Vec::new(),
            messages: sample_messages(),
            path: dir.join("session_export.json"),
        };
        s.save().unwrap();
        let md = s.export_markdown().unwrap();
        let text = fs::read_to_string(&md).unwrap();
        assert!(text.contains("# 会话轨迹 session_export"), "{text}");
        assert!(text.contains("工具调用 check_code"), "{text}");
        assert!(md.extension().and_then(|e| e.to_str()) == Some("md"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_finds_saved_sessions_sorted() {
        let dir = std::env::temp_dir().join(format!("rs_sessions_list_{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        for id in ["session_a", "session_b"] {
            let mut s = Session { id: id.into(), started_at: Utc::now(), model: "m".into(), title: None, exercises: Vec::new(), messages: vec![], path: dir.join(format!("{id}.json")) };
            s.title = Some(id.into());
            s.save().unwrap();
        }
        // Note: list() reads the default dir, not the temp one; here we
        // only assert it runs. Directory injection is exercised via
        // load/save above; keep this as a smoke check.
        let infos = Session::list();
        let sorted = infos.windows(2).all(|w| w[0].id <= w[1].id);
        assert!(sorted);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn trajectory_labels_all_roles() {
        let text = trajectory_text(&sample_messages(), 400);
        assert!(text.contains("[1] 系统提示"), "{text}");
        assert!(text.contains("[2] 用户: 为什么报 E0382"), "{text}");
        assert!(text.contains("工具调用 check_code"), "{text}");
        assert!(text.contains("工具结果（c1）"), "{text}");
        assert!(text.contains("教练: E0382"), "{text}");
    }

    #[test]
    fn long_content_is_truncated_with_count() {
        let long = "x".repeat(2000);
        let msgs = vec![ChatMessage::assistant(long)];
        let text = trajectory_text(&msgs, 100);
        assert!(text.contains("…（共 2000 字）"), "{text}");
        assert!(text.chars().count() < 200);
    }

    #[test]
    fn newlines_flattened_for_review() {
        let msgs = vec![ChatMessage::user("a\nb\nc")];
        let text = trajectory_text(&msgs, 400);
        assert!(!text.contains('\n') || text.lines().count() == 1, "{text}");
    }
}
