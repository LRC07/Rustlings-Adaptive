//! End-to-end smoke test (M8): spawn the real binary, drive the REPL
//! over a pipe (non-tty → zero-ANSI output, `read_line` fallback) and
//! assert the offline pages render. No LLM calls are involved — the
//! smoke covers startup, command parsing and the deterministic pages
//! only.

use std::process::{Command, Stdio};
use std::io::Write;

fn run_repl(input: &str) -> String {
    let mut child = Command::new(env!("CARGO_BIN_EXE_my_rustlings"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn binary");
    {
        let stdin = child.stdin.as_mut().expect("stdin piped");
        stdin.write_all(input.as_bytes()).expect("write stdin");
    }
    let out = child.wait_with_output().expect("wait");
    String::from_utf8_lossy(&out.stdout).to_string()
}

#[test]
fn repl_offline_pages_render() {
    let out = run_repl("/help\n/topics\n/usage\n/exit\n");
    assert!(out.contains("欢迎使用 my_rustlings"), "banner missing: {out}");
    assert!(out.contains("对话：直接输入问题"), "help missing");
    assert!(out.contains("概念图谱"), "topics page missing");
    assert!(out.contains("用量与花费"), "usage page missing");
    // Pipe output must be ANSI-free (M4.2 gate).
    assert!(!out.contains('\x1B'), "raw ANSI escape in piped output");
}

#[test]
fn repl_stats_page_and_unknown_command_hint() {
    let out = run_repl("/stats\n/halp\n/exit\n");
    // /stats works whether or not the profile has signals.
    assert!(out.contains("学习画像"), "stats page missing: {out}");
    // Near-miss suggestion for a mistyped command (M4.2).
    assert!(out.contains("help"), "near-miss suggestion missing: {out}");
}
