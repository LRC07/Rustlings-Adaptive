//! Interactive CLI. Since M4 the conversation REPL (`repl`) is the
//! default first screen; the rustlings-style practice flow
//! (`practice`) is a sub-mode entered from the REPL (or right after a
//! generated exercise). There is no screen clearing anywhere: output
//! scrolls like a chat, which also fixes the old "usage page gets
//! wiped by the next menu render" problem.

use crate::config::ModelConfig;
use crate::llm::LlmClient;

mod debrief;
mod generate;
mod input;
mod md;
mod practice;
mod repl;
pub(crate) mod render;
mod spinner;

pub(crate) use input::{read_line, Line};

pub fn run() {
    repl::run();
}

/// Build the LLM client when a key is configured (None → offline).
pub(crate) fn make_client(cfg: &ModelConfig) -> Option<LlmClient> {
    if cfg.api_key.trim().is_empty() {
        None
    } else {
        let timeout = cfg
            .llm_timeout_secs
            .map(std::time::Duration::from_secs)
            .unwrap_or(crate::llm::default_timeout());
        Some(LlmClient::with_timeout(&cfg.endpoint, &cfg.api_key, &cfg.model, timeout)
            .with_thinking(cfg.think_mode.to_thinking())
            .with_reasoning_effort(cfg.reasoning_effort.clone()))
    }
}

/// Discard stale terminal input (keys typed while an editor or a long
/// task held the foreground). Root fix for the buffered-keystrokes
/// problem reported during M3 trials; a no-op off unix.
pub(crate) fn flush_stdin() {
    #[cfg(unix)]
    unsafe {
        libc::tcflush(0, libc::TCIFLUSH);
    }
    #[cfg(not(unix))]
    let _ = ();
}

/// Convenience for "line or give up" call sites (paste mode, sub-pages
/// where Ctrl-C/EOF both mean "leave this page").
pub(crate) fn read_line_or_leave(prompt: &str) -> Option<String> {
    match read_line(prompt) {
        Line::Text(s) => Some(s),
        Line::Interrupted | Line::Eof => None,
    }
}
