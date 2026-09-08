//! Verifier (assignment core R1): runs candidate exercise code through
//! `rustc --test` with JSON diagnostics, parses rustc's line-delimited
//! JSON output, parses libtest results (including failure sections with
//! assert left/right values), and applies the triple gate:
//!
//! 1. template (with todo) must NOT pass,
//! 2. reference solution must compile AND pass all tests,
//! 3. hence the reference compiles.
//!
//! Everything here is offline (no LLM); it is the deterministic base the
//! generator (M3) and review gate (M5) rely on.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Diagnostics (rustc --error-format=json)
// ---------------------------------------------------------------------------

/// A source span, simplified from rustc's JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Span {
    pub file: String,
    pub line_start: usize,
    pub line_end: usize,
    pub col_start: usize,
    pub col_end: usize,
}

/// One rustc diagnostic (error/warning/note/help).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Diagnostic {
    /// Error code like "E0308"; None for plain messages.
    pub code: Option<String>,
    pub level: String,
    pub message: String,
    pub spans: Vec<Span>,
    /// rustc's pre-rendered human-readable text (shown to users as-is).
    pub rendered: Option<String>,
}

impl Diagnostic {
    #[allow(dead_code)] // part of the signal API used from M4/M5
    pub fn is_error(&self) -> bool {
        self.level == "error"
    }
}

/// Collect error codes from a diagnostic list, e.g. ["E0308", "E0382"].
#[allow(dead_code)] // part of the signal API used from M4/M5
pub fn error_codes(diags: &[Diagnostic]) -> Vec<String> {
    let mut codes: Vec<String> = diags.iter().filter_map(|d| d.code.clone()).collect();
    codes.sort();
    codes.dedup();
    codes
}

#[derive(Deserialize, Default)]
struct DiagWire {
    #[serde(default)]
    message: String,
    #[serde(default)]
    code: Option<CodeWire>,
    #[serde(default)]
    level: String,
    #[serde(default)]
    spans: Vec<SpanWire>,
    #[serde(default)]
    rendered: Option<String>,
}

#[derive(Deserialize)]
struct CodeWire {
    #[serde(default)]
    code: Option<String>,
}

#[derive(Deserialize, Default)]
struct SpanWire {
    #[serde(default)]
    file_name: String,
    #[serde(default)]
    line_start: usize,
    #[serde(default)]
    line_end: usize,
    #[serde(default)]
    column_start: usize,
    #[serde(default)]
    column_end: usize,
}

#[derive(Deserialize)]
struct LineWire {
    /// Present when rustc runs under cargo ("compiler-message").
    #[serde(default)]
    reason: Option<String>,
    /// Present for plain `rustc --error-format=json` ("diagnostic").
    #[serde(default, rename = "$message_type")]
    message_type: Option<String>,
    #[serde(flatten)]
    diag: DiagWire,
}

/// Parse rustc's line-delimited JSON stderr into diagnostics. Lines that
/// are not compiler messages (or fail to parse) are skipped.
pub fn parse_diagnostics(stderr: &str) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for line in stderr.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(wire) = serde_json::from_str::<LineWire>(line) else {
            continue;
        };
        // Accept both output shapes: cargo-wrapped ("reason") and plain
        // rustc ("$message_type": "diagnostic"); skip everything else.
        let is_msg = matches!(
            (wire.reason.as_deref(), wire.message_type.as_deref()),
            (Some("compiler-message"), _) | (_, Some("diagnostic")) | (None, None)
        );
        if !is_msg {
            continue;
        }
        // Drop marker-only lines ("build-finished", …) with no content.
        if wire.diag.message.is_empty() && wire.diag.code.is_none() {
            continue;
        }
        out.push(Diagnostic {
            code: wire.diag.code.and_then(|c| c.code),
            level: wire.diag.level,
            message: wire.diag.message,
            spans: wire
                .diag
                .spans
                .into_iter()
                .map(|s| Span {
                    file: s.file_name,
                    line_start: s.line_start,
                    line_end: s.line_end,
                    col_start: s.column_start,
                    col_end: s.column_end,
                })
                .collect(),
            rendered: wire.diag.rendered,
        });
    }
    out
}

// ---------------------------------------------------------------------------
// Test run parsing (libtest human output)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TestFailure {
    pub test: String,
    /// Captured section for this test (panic message, location, …).
    pub output: String,
    /// assert_eq!/assert_ne! left/right values when present.
    pub left: Option<String>,
    pub right: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TestRun {
    pub ok: bool,
    pub passed: u32,
    pub failed: u32,
    pub failures: Vec<TestFailure>,
}

/// Parse libtest's human output (`test foo ... ok`, failure sections,
/// `test result: ...` summary).
pub fn parse_test_output(stdout: &str, exit_ok: bool) -> TestRun {
    let mut run = TestRun {
        ok: exit_ok,
        ..Default::default()
    };
    let lines: Vec<&str> = stdout.lines().collect();
    let mut failures: Vec<TestFailure> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];

        // Failure section: "---- <name> stdout ----" followed by the
        // captured panic output, until the next section / summary.
        if let Some(name) = line.strip_prefix("---- ").and_then(|s| s.strip_suffix(" stdout ----")) {
            let mut body = String::new();
            i += 1;
            while i < lines.len() {
                let l = lines[i];
                if (l.starts_with("---- ") && l.ends_with(" stdout ----"))
                    || l.starts_with("failures:")
                    || l.starts_with("test result:")
                    || l.starts_with("error:")
                {
                    break;
                }
                body.push_str(l);
                body.push('\n');
                i += 1;
            }
            failures.push(TestFailure {
                test: name.to_string(),
                left: extract_side(&body, "left:"),
                right: extract_side(&body, "right:"),
                output: body,
            });
            continue; // re-process the line that stopped the capture
        }

        if line.starts_with("test result:") {
            // "test result: ok. 3 passed; 0 failed; 0 ignored; ..."
            for part in line.split(';') {
                let part = part.trim();
                if let Some(tail) = part.strip_suffix(" passed") {
                    if let Some(n) = tail.rsplit(' ').next().and_then(|s| s.parse::<u32>().ok()) {
                        run.passed = n;
                    }
                } else if let Some(n) = part
                    .strip_suffix(" failed")
                    .and_then(|tail| tail.rsplit(' ').next())
                    .and_then(|s| s.parse::<u32>().ok())
                {
                    run.failed = n;
                }
            }
        } else if let Some(res) = line.strip_prefix("test ") {
            // "test foo ... ok" / "test foo ... FAILED"
            if let Some(idx) = res.find(" ... ") {
                match &res[idx + 5..] {
                    "ok" => run.passed += 1,
                    "FAILED" => run.failed += 1,
                    _ => {}
                }
            }
        }
        i += 1;
    }
    run.failures = failures;
    run
}

fn extract_side(body: &str, prefix: &str) -> Option<String> {
    for line in body.lines() {
        let t = line.trim_start();
        if let Some(v) = t.strip_prefix(prefix) {
            let v = v.trim();
            let v = v.trim_start_matches('`').trim_end_matches('`');
            return Some(v.to_string());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// rustc runner + triple verification
// ---------------------------------------------------------------------------

/// Result of compiling + running one candidate source.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CompileRun {
    pub compiled: bool,
    pub diagnostics: Vec<Diagnostic>,
    pub test: Option<TestRun>,
}

/// Triple verification of a template/reference pair.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VerifyReport {
    /// Reference solution compiles.
    pub compiles: bool,
    /// Reference solution compiles AND all tests pass.
    pub ref_solution_passes: bool,
    /// Template must NOT pass (compile error or test failure).
    pub template_fails: bool,
    /// First error-level code the *unfinished* template produced
    /// (M4.5b quality gate C1: must be one of the declared
    /// `error_codes` whenever the template fails to compile). None
    /// when the template compiles but its tests fail (the
    /// `todo!()`-style failure shape).
    pub first_error_code: Option<String>,
    pub template: Option<CompileRun>,
    pub reference: Option<CompileRun>,
}

impl VerifyReport {
    /// All three gates satisfied → the exercise is machine-solvable.
    pub fn all_pass(&self) -> bool {
        self.compiles && self.ref_solution_passes && self.template_fails
    }
}

/// Hardening (M4.5c 前置): a pathological reference/template (e.g. a
/// `loop {}` test) must never hang the CLI. Compile and test runs are
/// polled and killed on timeout; a timed-out test counts as failed.
pub(crate) const RUSTC_TIMEOUT: Duration = Duration::from_secs(60);
pub(crate) const TEST_TIMEOUT: Duration = Duration::from_secs(15);

/// Outcome of a polled subprocess run.
pub(crate) enum RunOutcome {
    Done(std::process::Output),
    TimedOut,
}

/// Spawn `cmd`, poll for exit, kill on timeout. stdout/stderr are
/// drained via threads so large output cannot deadlock the poll loop.
/// pub(crate) so the M5 review gate can run clippy under the same
/// hardening.
pub(crate) fn run_with_timeout(mut cmd: Command, timeout: Duration) -> Result<RunOutcome> {
    use std::io::Read;
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("启动 {} 失败", cmd.get_program().to_string_lossy()))?;
    let mut stderr_pipe = child.stderr.take().expect("stderr piped");
    let mut stdout_pipe = child.stdout.take().expect("stdout piped");
    let err_t = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr_pipe.read_to_end(&mut buf);
        buf
    });
    let out_t = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout_pipe.read_to_end(&mut buf);
        buf
    });

    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let stderr = err_t.join().unwrap_or_default();
                let stdout = out_t.join().unwrap_or_default();
                return Ok(RunOutcome::Done(std::process::Output { status, stdout, stderr }));
            }
            Ok(None) => {}
            Err(e) => return Err(anyhow::anyhow!("等待进程失败：{e}")),
        }
        if start.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(RunOutcome::TimedOut);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn timeout_diag(what: &str) -> Diagnostic {
    Diagnostic {
        code: None,
        level: "error".to_string(),
        message: format!("{what}，已中止"),
        spans: Vec::new(),
        rendered: None,
    }
}

/// Compile `source` with `rustc --test --error-format=json` and, on
/// success, run the test binary (both under hard timeouts). Files are
/// written into `workdir` (created by the caller).
pub fn run_test_flow(source: &str, workdir: &Path, name: &str) -> Result<CompileRun> {
    run_test_flow_timed(source, workdir, name).map(|(run, _, _)| run)
}

/// Same flow with wall-clock timings (M5 debrief comparison table):
/// returns `(run, compile_ms, test_ms)`.
pub fn run_test_flow_timed(
    source: &str,
    workdir: &Path,
    name: &str,
) -> Result<(CompileRun, u64, u64)> {
    run_test_flow_with_timeouts(source, workdir, name, RUSTC_TIMEOUT, TEST_TIMEOUT)
}

fn run_test_flow_with_timeouts(
    source: &str,
    workdir: &Path,
    name: &str,
    rustc_timeout: Duration,
    test_timeout: Duration,
) -> Result<(CompileRun, u64, u64)> {
    let src_path: PathBuf = workdir.join(format!("{name}.rs"));
    let bin_path = workdir.join(format!("{name}.bin"));
    fs::write(&src_path, source).with_context(|| format!("写入 {} 失败", src_path.display()))?;

    let mut cmd = Command::new("rustc");
    cmd.args(["--edition", "2024", "--test", "-A", "warnings", "--error-format=json"])
        .arg(&src_path)
        .arg("-o")
        .arg(&bin_path);
    let t0 = std::time::Instant::now();
    let out = match run_with_timeout(cmd, rustc_timeout)? {
        RunOutcome::Done(o) => o,
        RunOutcome::TimedOut => {
            let _ = fs::remove_file(&bin_path);
            return Ok((
                CompileRun {
                    compiled: false,
                    diagnostics: vec![timeout_diag(&format!(
                        "编译超过 {}s",
                        rustc_timeout.as_secs()
                    ))],
                    test: None,
                },
                0,
                0,
            ));
        }
    };
    let compile_ms = t0.elapsed().as_millis() as u64;
    let stderr = String::from_utf8_lossy(&out.stderr);
    let diagnostics = parse_diagnostics(&stderr);
    if !out.status.success() {
        let _ = fs::remove_file(&bin_path);
        return Ok((CompileRun { compiled: false, diagnostics, test: None }, compile_ms, 0));
    }

    let t1 = std::time::Instant::now();
    let test = match run_with_timeout(Command::new(&bin_path), test_timeout)? {
        RunOutcome::Done(run) => {
            let stdout = String::from_utf8_lossy(&run.stdout);
            parse_test_output(&stdout, run.status.success())
        }
        RunOutcome::TimedOut => TestRun {
            ok: false,
            passed: 0,
            failed: 1,
            failures: vec![TestFailure {
                test: "<run>".to_string(),
                output: format!(
                    "测试运行超过 {}s（可能死循环），已中止",
                    test_timeout.as_secs()
                ),
                left: None,
                right: None,
            }],
        },
    };
    let test_ms = t1.elapsed().as_millis() as u64;
    let _ = fs::remove_file(&bin_path);
    Ok((CompileRun { compiled: true, diagnostics, test: Some(test) }, compile_ms, test_ms))
}

/// Triple verification of a template (with todo) against a reference
/// solution. Both are run in sequence inside `workdir`.
pub fn verify_exercise(template_code: &str, reference_code: &str, workdir: &Path) -> Result<VerifyReport> {
    let t = run_test_flow(template_code, workdir, "template")?;
    let r = run_test_flow(reference_code, workdir, "reference")?;
    let first_error_code = t
        .diagnostics
        .iter()
        .find(|d| d.is_error())
        .and_then(|d| d.code.clone());
    let report = VerifyReport {
        compiles: r.compiled,
        ref_solution_passes: r.compiled && r.test.as_ref().is_some_and(|t| t.ok),
        template_fails: !(t.compiled && t.test.as_ref().is_some_and(|t| t.ok)),
        first_error_code,
        template: Some(t),
        reference: Some(r),
    };
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let d = std::env::temp_dir().join(format!("rustlings_verify_{tag}_{nanos}"));
        fs::create_dir_all(&d).unwrap();
        d
    }

    const TEMPLATE_TODO: &str = r#"
fn add(a: i32, b: i32) -> i32 {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn t() {
        assert_eq!(add(1, 2), 3);
    }
}
"#;

    const REFERENCE_OK: &str = r#"
fn add(a: i32, b: i32) -> i32 {
    a + b
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn t() {
        assert_eq!(add(1, 2), 3);
    }
}
"#;

    const TYPE_ERROR: &str = r#"
fn add(a: i32, b: i32) -> i32 {
    a + b
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn t() {
        let x: String = add(1, 2);
        println!("{x}");
    }
}
"#;

    #[test]
    fn triple_gate_on_inline_fixtures() {
        let wd = temp_dir("inline");
        let report = verify_exercise(TEMPLATE_TODO, REFERENCE_OK, &wd).unwrap();
        assert!(report.all_pass(), "report: {report:?}");
        let _ = fs::remove_dir_all(&wd);
    }

    #[test]
    fn type_error_yields_error_code() {
        let wd = temp_dir("typeerr");
        let run = run_test_flow(TYPE_ERROR, &wd, "x").unwrap();
        assert!(!run.compiled);
        let codes = error_codes(&run.diagnostics);
        assert!(codes.contains(&"E0308".to_string()), "codes: {codes:?}");
        let _ = fs::remove_dir_all(&wd);
    }

    #[test]
    fn timed_out_test_counts_as_failed() {
        // M4.5c hardening: a `loop {}` test must be killed, not hang.
        let wd = temp_dir("timeout");
        let src = "#[test]\nfn spins() { loop {} }\n";
        let (run, _, _) = run_test_flow_with_timeouts(
            src,
            &wd,
            "spin",
            Duration::from_secs(60),
            Duration::from_secs(1),
        )
        .unwrap();
        assert!(run.compiled, "infinite loop still compiles");
        let test = run.test.expect("test ran");
        assert!(!test.ok, "timed-out test must fail");
        assert!(test.failures.iter().any(|f| f.output.contains("死循环") || f.output.contains("已中止")));
        let _ = fs::remove_dir_all(&wd);
    }

    #[test]
    fn verifier_end_to_end_on_inline_samples() {
        // Replaces the former seed-fixture test (the shipped seeds were
        // removed, 0907 反馈 P1): an unsolved sample must fail to compile
        // with an extractable error code; a solved sample must pass all
        // of its tests.
        let unsolved = "\
fn longest(a: String, b: String) -> String {
    let keep = a;
    let _again = a;
    keep
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn t1() { assert_eq!(longest(\"a\".into(), \"bc\".into()), \"bc\"); }
}
";
        let solved = "\
// 完整解样例：全部测试通过。
fn sum_all(v: &[i32]) -> i32 {
    v.iter().sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn t1() { assert_eq!(sum_all(&[]), 0); }
    #[test]
    fn t2() { assert_eq!(sum_all(&[1]), 1); }
    #[test]
    fn t3() { assert_eq!(sum_all(&[1, 2]), 3); }
    #[test]
    fn t4() { assert_eq!(sum_all(&[-1, 1]), 0); }
    #[test]
    fn t5() { assert_eq!(sum_all(&[1, 2, 3]), 6); }
}
";
        let wd = temp_dir("inline_samples");
        let t = run_test_flow(unsolved, &wd, "unsolved").unwrap();
        assert!(!t.compiled, "unsolved sample should not compile");
        assert!(t.diagnostics.iter().any(|d| d.is_error()));
        // M4.5b C1: the first error-level code is extractable.
        let first = t
            .diagnostics
            .iter()
            .find(|d| d.is_error())
            .and_then(|d| d.code.clone());
        assert!(matches!(first, Some(ref c) if c.starts_with('E')), "got {first:?}");

        let g = run_test_flow(solved, &wd, "solved").unwrap();
        assert!(g.compiled);
        let test = g.test.unwrap();
        assert!(test.ok);
        assert_eq!(test.passed, 5);
        assert_eq!(test.failed, 0);
        let _ = fs::remove_dir_all(&wd);
    }

    #[test]
    fn parses_failure_sections_with_left_right() {
        let stdout = "\
running 2 tests
test t_ok ... ok
test t_bad ... FAILED

failures:

---- t_bad stdout ----
thread 't_bad' panicked at src/lib.rs:10:5:
assertion `left == right` failed
  left: `3`
 right: `5`
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace

failures:
    t_bad

test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
";
        let run = parse_test_output(stdout, false);
        assert!(!run.ok);
        assert_eq!(run.passed, 1);
        assert_eq!(run.failed, 1);
        assert_eq!(run.failures.len(), 1);
        let f = &run.failures[0];
        assert_eq!(f.test, "t_bad");
        assert_eq!(f.left.as_deref(), Some("3"));
        assert_eq!(f.right.as_deref(), Some("5"));
    }

    #[test]
    fn parses_ok_summary() {
        let stdout = "\
running 3 tests
test a ... ok
test b ... ok
test c ... ok

test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
";
        let run = parse_test_output(stdout, true);
        assert!(run.ok);
        assert_eq!(run.passed, 3);
        assert_eq!(run.failed, 0);
        assert!(run.failures.is_empty());
    }

    #[test]
    fn parses_rustc_json_lines() {
        let stderr = concat!(
            r#"{"message":"mismatched types","code":{"code":"E0308","explanation":"x"},"level":"error","spans":[{"file_name":"t.rs","byte_start":0,"byte_end":1,"line_start":2,"line_end":2,"column_start":3,"column_end":4}],"rendered":"error[E0308]: mismatched types\n"}"#,
            "\n",
            r#"{"reason":"build-finished","success":false}"#,
            "\n",
            "not json at all\n"
        );
        let diags = parse_diagnostics(stderr);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].code.as_deref(), Some("E0308"));
        assert_eq!(diags[0].spans[0].line_start, 2);
        assert!(diags[0].rendered.as_deref().unwrap().starts_with("error[E0308]"));
    }
}
