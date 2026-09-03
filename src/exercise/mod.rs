//! Exercise domain: discovery of `exercises/**/*.rs`, title parsing, and
//! running a single exercise through `rustc --test`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct Exercise {
    pub path: PathBuf,
    pub name: String,
    pub category: String,
    pub title: String,
}

impl Exercise {
    pub fn is_done(&self, progress: &[String]) -> bool {
        progress.iter().any(|p| p == self.path.to_string_lossy().as_ref())
    }
}

pub fn discover(root: &Path) -> Vec<Exercise> {
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<Exercise>) {
    let rd = match fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return,
    };
    for entry in rd.flatten() {
        let p = entry.path();
        if p.is_dir() {
            walk(root, &p, out);
            continue;
        }
        if p.extension().and_then(|e| e.to_str()) == Some("rs") {
            // Skip crate/module wiring files (the IDE-only `lib.rs` lives
            // directly under `exercises/`). Exercises live in category
            // subdirectories, so their category is non-empty; anything
            // sitting in the root is not an exercise.
            let is_module_root = matches!(
                p.file_name().and_then(|s| s.to_str()),
                Some("lib.rs" | "main.rs" | "mod.rs")
            );
            let category = p
                .parent()
                .and_then(|parent| parent.strip_prefix(root).ok())
                .and_then(|rel| rel.to_str())
                .unwrap_or("")
                .to_string();
            if is_module_root || category.is_empty() {
                continue;
            }
            let name = p
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            let title = read_title(&p);
            out.push(Exercise {
                path: p,
                name,
                category,
                title,
            });
        }
    }
}

fn read_title(p: &Path) -> String {
    let Ok(content) = fs::read_to_string(p) else {
        return String::new();
    };
    for line in content.lines() {
        let t = line.trim_start();
        if t.is_empty() {
            continue;
        }
        if let Some(rest) = t.strip_prefix("//") {
            let r = rest.trim();
            if r.is_empty() {
                continue;
            }
            if r.contains("I AM NOT DONE") {
                continue;
            }
            return r.to_string();
        }
        // First non-comment, non-empty line ends the title scan.
        break;
    }
    String::new()
}

/// Compile the exercise with `rustc --test` and run its test binary.
/// Prints rustc stderr / test output as-is; returns whether everything
/// passed.
pub fn compile_and_run(ex: &Exercise) -> bool {
    let tmp = format!("/tmp/my_rustlings_{}", ex.name);
    let _ = fs::remove_file(&tmp);
    let depinfo = format!("{tmp}.d");
    let _ = fs::remove_file(&depinfo);

    println!("  Compiling {} ...", ex.path.file_name().unwrap().to_string_lossy());
    let compile = Command::new("rustc")
        .arg("--edition")
        .arg("2024")
        .arg("--test")
        .arg("-A")
        .arg("warnings")
        .arg(&ex.path)
        .arg("-o")
        .arg(&tmp)
        .output();

    match compile {
        Err(e) => {
            eprintln!("  failed to invoke rustc: {e}");
            return false;
        }
        Ok(out) if !out.status.success() => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            // Trim the noisy "error: aborting due to ..." tail slightly.
            print_stderr(&stderr);
            println!();
            println!("  Compilation failed. Fix the errors above and try again.");
            return false;
        }
        Ok(_) => {}
    }

    let run = Command::new(&tmp).output();
    let _ = fs::remove_file(&tmp);
    let _ = fs::remove_file(&depinfo);
    match run {
        Err(e) => {
            eprintln!("  failed to run test binary: {e}");
            false
        }
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            if !stdout.is_empty() {
                print!("{stdout}");
            }
            if !stderr.is_empty() {
                eprint!("{stderr}");
            }
            if out.status.success() {
                println!("  All tests passed.");
                true
            } else {
                println!("  Tests failed (exit {:?}).", out.status.code());
                false
            }
        }
    }
}

fn print_stderr(s: &str) {
    // Indent rustc's stderr so it lines up under the menu prompt.
    for line in s.lines() {
        println!("  {line}");
    }
}

pub fn open_editor(path: &Path) {
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".into());
    match Command::new(&editor).arg(path).status() {
        Ok(s) if s.success() => {}
        Ok(s) => println!("  editor exited with {:?}", s.code()),
        Err(e) => println!("  could not launch '{editor}': {e}"),
    }
}
