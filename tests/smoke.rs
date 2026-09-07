//! End-to-end smoke test (M8): spawn the real binary, drive the REPL
//! over a pipe (non-tty → zero-ANSI output, `read_line` fallback) and
//! assert the offline pages render. No LLM calls are involved — the
//! smoke covers startup, command parsing and the deterministic pages
//! only. Every run gets an ISOLATED temp HOME so sessions/profile/
//! usage never touch real user data.

use std::process::{Command, Stdio};
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// Spawn the REPL with a fresh temp `$HOME`; return its full stdout.
fn run_repl(input: &str) -> String {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let home = std::env::temp_dir().join(format!("rs_smoke_home_{nanos}"));
    let out = run_repl_with_home(input, &home);
    let _ = std::fs::remove_dir_all(&home);
    out
}

fn run_repl_with_home(input: &str, home: &std::path::Path) -> String {
    let mut child = Command::new(env!("CARGO_BIN_EXE_rustlings-adaptive"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .env("HOME", home)
        .spawn()
        .expect("spawn binary");
    {
        let stdin = child.stdin.as_mut().expect("stdin piped");
        stdin.write_all(input.as_bytes()).expect("write stdin");
    }
    let out = child.wait_with_output().expect("wait");
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// Pre-seed two saved sessions (000001 older, 000002 newer) so the
/// archive-flow test starts from a deterministic list. The NEWER one
/// is what startup resumes, so archiving the older one is allowed.
fn seed_sessions(home: &std::path::Path) -> PathBuf {
    let dir = home.join(".rustlings_adaptive").join("sessions");
    std::fs::create_dir_all(&dir).unwrap();
    for (id, ts, text) in [
        ("session_20260908_000001", "2026-09-08T00:00:01Z", "第一条"),
        ("session_20260908_000002", "2026-09-08T00:00:02Z", "第二条"),
    ] {
        let body = format!(
            r#"{{"id":"{id}","started_at":"{ts}","model":"test","messages":[{{"role":"user","content":"{text}"}}]}}"#
        );
        std::fs::write(dir.join(format!("{id}.json")), body).unwrap();
    }
    dir
}

#[test]
fn repl_offline_pages_render() {
    let out = run_repl("/help\n/topics\n/usage\n/exit\n");
    assert!(out.contains("欢迎使用 Rustlings-Adaptive"), "banner missing: {out}");
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

/// 0909 反馈: `c <n>` archived the session but the panel redrew the
/// STALE list, so the archived session still appeared and the archive
/// looked like a no-op. The panel must re-list after every action.
#[test]
fn sessions_panel_archive_refreshes_the_list() {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let home = std::env::temp_dir().join(format!("rs_smoke_home_{nanos}"));
    let sessions = seed_sessions(&home);

    // Current session at startup = the NEWEST (000002) → archiving #1
    // is allowed. `q` leaves the panel; /exit saves and quits.
    let out = run_repl_with_home("/sessions\nc 1\ny\nq\n/exit\n", &home);

    assert!(
        out.contains("已归档会话 session_20260908_000001"),
        "archive confirmation missing: {out}"
    );
    // The RE-RENDERED panel (the last one in the transcript) must NOT
    // list the archived session anymore — with the stale-list bug the
    // redraw still carried it.
    let last_panel = out.rsplit("── 会话列表").next().unwrap_or("");
    assert!(
        !last_panel.contains("session_20260908_000001"),
        "archived session must vanish from the redrawn panel: {out}"
    );
    assert!(last_panel.contains("session_20260908_000002"), "remaining session still listed: {out}");
    // And the files really moved into the archive, not deleted.
    assert!(!sessions.join("session_20260908_000001.json").exists(), "source file still there");
    let archive = home.join(".rustlings_adaptive").join("archive");
    let moved = std::fs::read_dir(&archive)
        .expect("archive dir")
        .flatten()
        .find(|e| e.path().join("sessions").join("session_20260908_000001.json").exists());
    assert!(moved.is_some(), "archived file not found under {archive:?}");
    let _ = std::fs::remove_dir_all(&home);
}
