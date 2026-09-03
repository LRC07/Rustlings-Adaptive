//! Exercise domain: discovery of `exercises/**/*.rs`, title parsing, and
//! running a single exercise through `rustc --test`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

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

    println!("  正在编译 {} ...", ex.path.file_name().unwrap().to_string_lossy());
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
            eprintln!("  调用 rustc 失败: {e}");
            return false;
        }
        Ok(out) if !out.status.success() => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            // Trim the noisy "error: aborting due to ..." tail slightly.
            print_indented(&stderr);
            println!();
            println!("  编译失败。请修正上方错误后重试。");
            return false;
        }
        Ok(_) => {}
    }

    let run = Command::new(&tmp).output();
    let _ = fs::remove_file(&tmp);
    let _ = fs::remove_file(&depinfo);
    match run {
        Err(e) => {
            eprintln!("  运行测试二进制失败: {e}");
            false
        }
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            if !stdout.is_empty() {
                print_indented(&stdout);
            }
            if !stderr.is_empty() {
                print_indented(&stderr);
            }
            if out.status.success() {
                println!("  全部测试通过。");
                true
            } else {
                println!("  测试未通过（退出码 {:?}）。", out.status.code());
                false
            }
        }
    }
}

fn print_indented(s: &str) {
    // Indent compiler/test output so it lines up under the menu prompt.
    for line in s.lines() {
        println!("  {line}");
    }
}

/// Resolve the editor command (design §4.1 chain):
/// `$EDITOR` → `$VISUAL` → config `editor` → `code --wait` (if VS Code
/// is on PATH; `--wait` blocks until the editor tab closes) → `vi`.
/// Splitting is plain whitespace (no quoting support — documented).
pub fn editor_command(config_editor: Option<&str>) -> Vec<String> {
    for key in ["EDITOR", "VISUAL"] {
        if let Ok(v) = std::env::var(key) {
            let parts = shell_words(&v);
            if !parts.is_empty() {
                return parts;
            }
        }
    }
    if let Some(e) = config_editor.map(str::trim).filter(|s| !s.is_empty()) {
        let parts = shell_words(e);
        if !parts.is_empty() {
            return parts;
        }
    }
    if command_on_path("code") {
        return vec!["code".to_string(), "--wait".to_string()];
    }
    vec!["vi".to_string()]
}

fn shell_words(s: &str) -> Vec<String> {
    s.split_whitespace().map(str::to_string).collect()
}

fn command_on_path(cmd: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(cmd).is_file()))
        .unwrap_or(false)
}

/// Editors that open a GUI window instead of taking over the terminal.
/// For these we detach stdin (they must not consume/hold our tty input;
/// matters especially under WSL interop) and print a hint, because
/// `--wait` blocks until the file tab is closed — which otherwise looks
/// like the CLI froze.
const GUI_EDITORS: &[&str] = &["code", "code-insiders", "codium", "subl", "gedit", "notepad"];

fn is_gui_editor(prog: &str) -> bool {
    let stem = std::path::Path::new(prog)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(prog);
    GUI_EDITORS.iter().any(|g| g.eq_ignore_ascii_case(stem))
}

pub fn open_editor(path: &Path, config_editor: Option<&str>) {
    let argv = editor_command(config_editor);
    let (prog, args) = argv.split_first().expect("editor command is never empty");
    let gui = is_gui_editor(prog);
    if gui {
        println!(
            "  已用 {prog} 打开 {}：关闭该文件的编辑器标签页后这里会继续（等待中…）",
            path.display()
        );
        println!("  提示：等待期间敲入的按键会被缓存，回来后如有多余输出按一次回车即可。");
    }
    let mut cmd = Command::new(prog);
    cmd.args(args).arg(path);
    if gui {
        cmd.stdin(Stdio::null());
    }
    match cmd.status() {
        Ok(s) if s.success() => {}
        Ok(s) => println!("  编辑器以 {:?} 退出", s.code()),
        Err(e) => println!("  无法启动编辑器 '{prog}': {e}"),
    }
    // Root fix for the buffered-input problem: keys typed while the
    // editor held the foreground sit in the tty line buffer; flush
    // them so they never execute as commands afterwards.
    crate::cli::flush_stdin();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mutating EDITOR/VISUAL/PATH here is safe: this is the only test
    /// that touches these variables (config tests use prefixed names),
    /// and it restores everything before returning.
    #[test]
    fn gui_editor_detection() {
        // WSL interop path: /mnt/c/.../bin/code → stem "code".
        assert!(is_gui_editor("code"));
        assert!(is_gui_editor("/mnt/c/Programs/Microsoft VS Code/bin/code"));
        assert!(is_gui_editor("CODE"));
        assert!(is_gui_editor("code.cmd"));
        assert!(!is_gui_editor("vi"));
        assert!(!is_gui_editor("vim -R")); // args never reach here; stem is "vim"
        assert!(!is_gui_editor("nano"));
    }

    #[test]
    fn editor_resolution_chain() {
        let saved = (
            std::env::var("EDITOR").ok(),
            std::env::var("VISUAL").ok(),
            std::env::var("PATH").ok(),
        );
        unsafe {
            std::env::remove_var("EDITOR");
            std::env::remove_var("VISUAL");
            // Hide `code` so the auto-detect leg cannot fire.
            std::env::set_var("PATH", "/nonexistent-editor-test");
        }

        // 1) Nothing set anywhere → vi fallback.
        assert_eq!(editor_command(None), vec!["vi".to_string()]);

        // 2) config editor wins over auto-detection, keeps arguments.
        assert_eq!(editor_command(Some("kate -n")), vec!["kate".to_string(), "-n".to_string()]);

        // 3) $VISUAL beats config.
        unsafe { std::env::set_var("VISUAL", "nano"); }
        assert_eq!(editor_command(Some("kate")), vec!["nano".to_string()]);

        // 4) $EDITOR beats $VISUAL.
        unsafe { std::env::set_var("EDITOR", "vim -R"); }
        assert_eq!(editor_command(None), vec!["vim".to_string(), "-R".to_string()]);

        // 5) Empty strings are skipped, not used.
        unsafe { std::env::set_var("EDITOR", "  "); }
        assert_eq!(editor_command(None), vec!["nano".to_string()]);

        unsafe {
            std::env::remove_var("EDITOR");
            std::env::remove_var("VISUAL");
            if let Some(p) = saved.2 {
                std::env::set_var("PATH", p);
            }
            if let Some(e) = saved.0 {
                std::env::set_var("EDITOR", e);
            }
            if let Some(v) = saved.1 {
                std::env::set_var("VISUAL", v);
            }
        }
    }
}
