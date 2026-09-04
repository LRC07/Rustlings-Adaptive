//! Borrow-checker hypothesis lab (M7, design §4.5/§8): the signature
//! "what if" tool. Compile the user's code as-is and once more with a
//! hypothetical change applied, then diff the rustc error codes — the
//! borrow checker itself answers "what happens if I change X to Y".
//!
//! Deterministic: two real rustc runs, no LLM inside. The coach (or
//! the /lab page) narrates the diff.

use anyhow::Result;

use serde::Serialize;

use crate::verifier;

/// One side of the comparison (baseline or hypothesis).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct LabSide {
    pub compiles: bool,
    /// Error codes in first-appearance order, deduplicated.
    pub codes: Vec<String>,
    /// First error-level rendered message (for quick reading).
    pub first_error: Option<String>,
}

/// Result of one hypothesis run.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LabReport {
    pub baseline: LabSide,
    pub hypothesis: LabSide,
    /// Codes present in the hypothesis build but not the baseline
    /// (with the count they gained).
    pub new_errors: CodeDelta,
    /// Codes present in the baseline but resolved by the hypothesis.
    pub resolved_errors: CodeDelta,
}

impl LabReport {
    /// True when the hypothesis changed nothing observable.
    pub fn no_change(&self) -> bool {
        self.new_errors.is_empty() && self.resolved_errors.is_empty()
    }
}

/// Compile one source the same way the practice loop / check_code does
/// (`--test` when it carries tests, bin otherwise) and reduce it to a
/// `LabSide`.
pub fn compile_side(code: &str, tag: &str) -> Result<LabSide> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("rustlings_lab_{tag}_{nanos}"));
    std::fs::create_dir_all(&dir)?;
    let src = dir.join(format!("{tag}.rs"));
    let bin = dir.join(format!("{tag}.bin"));
    std::fs::write(&src, code)?;

    let mut cmd = std::process::Command::new("rustc");
    cmd.args(["--edition", "2024", "-A", "warnings", "--error-format=json"]);
    if code.contains("#[test]") {
        cmd.arg("--test");
    } else {
        cmd.args(["--crate-type", "bin"]);
    }
    cmd.arg(&src).arg("-o").arg(&bin);
    cmd.stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());

    let result = verifier::run_with_timeout(cmd, std::time::Duration::from_secs(30));
    let _ = std::fs::remove_dir_all(&dir);
    let out = match result? {
        verifier::RunOutcome::Done(o) => o,
        verifier::RunOutcome::TimedOut => {
            return Ok(LabSide {
                compiles: false,
                codes: vec![],
                first_error: Some("编译超时（30s），已中止".to_string()),
            });
        }
    };
    let stderr = String::from_utf8_lossy(&out.stderr);
    let diags = verifier::parse_diagnostics(&stderr);
    let mut codes: Vec<String> = Vec::new();
    let mut first_error = None;
    for d in &diags {
        if d.level != "error" {
            continue;
        }
        if d.code.is_some() && !codes.contains(d.code.as_ref().unwrap()) {
            codes.push(d.code.clone().unwrap());
        }
        if first_error.is_none()
            && let Some(r) = &d.rendered
        {
            first_error = Some(r.chars().take(300).collect());
        }
    }
    // "aborting due to previous errors" style markers carry no code and
    // no rendered text of their own; fall back to any rendered text.
    let first_error = first_error.or_else(|| {
        diags.iter().find(|d| d.level == "error").and_then(|d| d.rendered.clone())
    });
    Ok(LabSide { compiles: out.status.success(), codes, first_error })
}

/// Count occurrences of each code.
fn counts(codes: &[String]) -> std::collections::BTreeMap<String, u32> {
    let mut m: std::collections::BTreeMap<String, u32> = Default::default();
    for c in codes {
        *m.entry(c.clone()).or_default() += 1;
    }
    m
}

/// Error-code delta list: (code, count changed).
pub type CodeDelta = Vec<(String, u32)>;

/// Pure diff over the two code lists (deterministic; unit-tested).
pub fn diff_codes(baseline: &[String], hypothesis: &[String]) -> (CodeDelta, CodeDelta) {
    let b = counts(baseline);
    let h = counts(hypothesis);
    let mut new_errors = Vec::new();
    let mut resolved_errors = Vec::new();
    for (code, n) in &b {
        let delta = h.get(code).copied().unwrap_or(0);
        if delta < *n {
            resolved_errors.push((code.clone(), n - delta));
        }
    }
    for (code, n) in &h {
        let delta = b.get(code).copied().unwrap_or(0);
        if delta < *n {
            new_errors.push((code.clone(), n - delta));
        }
    }
    new_errors.sort();
    resolved_errors.sort();
    (new_errors, resolved_errors)
}

/// Run the lab: compile both sides and diff.
pub fn apply_and_check(code: &str, hypothesis: &str) -> Result<LabReport> {
    let baseline = compile_side(code, "baseline")?;
    let hypothesis_side = compile_side(hypothesis, "hyp")?;
    let (new_errors, resolved_errors) =
        diff_codes(&baseline.codes, &hypothesis_side.codes);
    Ok(LabReport { baseline, hypothesis: hypothesis_side, new_errors, resolved_errors })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_codes_tracks_multiplicity() {
        let (new, resolved) = diff_codes(
            &["E0382".into(), "E0382".into(), "E0599".into()],
            &["E0382".into(), "E0502".into(), "E0502".into()],
        );
        assert_eq!(new, vec![("E0502".to_string(), 2)]);
        // E0382 went 2→1 (one instance resolved) and E0599 1→0.
        assert_eq!(resolved, vec![("E0382".to_string(), 1), ("E0599".to_string(), 1)]);
    }

    #[test]
    fn diff_codes_empty_when_identical() {
        let (new, resolved) = diff_codes(&["E0308".into()], &["E0308".into()]);
        assert!(new.is_empty() && resolved.is_empty());
    }

    /// Real rustc run: the classic move-then-borrow snippet; switching
    /// to a borrow must resolve E0382.
    #[test]
    fn borrow_hypothesis_resolves_e0382() {
        let broken = "fn main() {\n    let s = String::from(\"hi\");\n    let t = s;\n    println!(\"{}\", s);\n}\n";
        let fixed = "fn main() {\n    let s = String::from(\"hi\");\n    let t = &s;\n    println!(\"{}\", s);\n}\n";
        let report = apply_and_check(broken, fixed).unwrap();
        assert!(!report.baseline.compiles);
        assert!(report.baseline.codes.contains(&"E0382".to_string()));
        assert!(report.hypothesis.compiles, "borrow variant should compile: {:?}", report.hypothesis);
        assert!(report.resolved_errors.iter().any(|(c, _)| c == "E0382"), "{:?}", report.resolved_errors);
    }

    /// A hypothesis that breaks working code surfaces the new code.
    #[test]
    fn breaking_hypothesis_surfaces_new_error() {
        let ok = "fn main() {\n    let s = String::from(\"hi\");\n    println!(\"{}\", s);\n}\n";
        let broken = "fn main() {\n    let s = String::from(\"hi\");\n    let t = s;\n    println!(\"{}\", s);\n}\n";
        let report = apply_and_check(ok, broken).unwrap();
        assert!(report.baseline.compiles);
        assert!(!report.hypothesis.compiles);
        assert!(report.new_errors.iter().any(|(c, _)| c == "E0382"), "{:?}", report.new_errors);
    }
}
