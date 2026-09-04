//! Practice sub-mode (M4): the rustlings-style exercise list and
//! solve loop. Reached from the REPL (`/practice`, a generated
//! exercise, or an agent practice offer) — never the default screen.
//!
//! No screen clearing: the list prints once per entry, outputs scroll.

use std::path::{Path, PathBuf};

use crate::exercise::{self, Exercise};

/// `.progress` bookkeeping + editor chain, threaded through the mode.
pub(crate) struct PracticeCtx {
    pub root: PathBuf,
    pub progress_path: PathBuf,
    pub editor: Option<String>,
}

impl PracticeCtx {
    pub(crate) fn new(root: &Path, editor: Option<String>) -> Self {
        Self {
            root: root.to_path_buf(),
            progress_path: root.join(".progress"),
            editor,
        }
    }

    fn load_progress(&self) -> Vec<String> {
        std::fs::read_to_string(&self.progress_path)
            .map(|s| s.lines().map(str::to_string).collect())
            .unwrap_or_default()
    }
}

/// Enter the list menu with a freshly discovered exercise set (so
/// generated exercises appear without a restart). Page-framed: clears
/// the viewport, shows a progress bar, then the list.
pub(crate) fn enter(ctx: &PracticeCtx) {
    let exercises = fresh_list(&ctx.root);
    let progress = ctx.load_progress();
    page_frame(&exercises, &progress);
    show_menu(&exercises, &progress);
    loop {
        let Some(line) = read_prompt("> ") else { return };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line == "b" || line == "q" || line == "back" {
            return;
        }
        let mut progress = ctx.load_progress();
        match line {
            "v" | "verify" => verify_all(&exercises, &mut progress, &ctx.progress_path),
            "n" => match first_pending(&exercises, &progress) {
                Some(idx) => {
                    run_exercise(idx, &exercises, &mut progress, &ctx.progress_path, ctx.editor.as_deref());
                    page_frame(&exercises, &progress);
                }
                None => println!("  所有练习已完成！"),
            },
            "h" | "help" => println!("  做题模式：<数字> 选题   n 下一题   v 全部验证   b 返回对话"),
            s => match s.parse::<usize>() {
                Ok(n) if n >= 1 && n <= exercises.len() => {
                    run_exercise(n - 1, &exercises, &mut progress, &ctx.progress_path, ctx.editor.as_deref());
                    page_frame(&exercises, &progress);
                }
                _ => println!("未知命令: {s}（可用：数字、n、v、b）"),
            },
        }
    }
}

/// Jump straight into one exercise (by path), then fall through to the
/// list menu. Used after a generated exercise.
pub(crate) fn enter_at(ctx: &PracticeCtx, path: &Path) {
    let exercises = fresh_list(&ctx.root);
    let want = path.canonicalize().ok();
    match exercises.iter().position(|e| e.path.canonicalize().ok() == want) {
        Some(idx) => {
            let mut progress = ctx.load_progress();
            run_exercise(idx, &exercises, &mut progress, &ctx.progress_path, ctx.editor.as_deref());
        }
        None => println!("  生成文件未出现在练习列表（意外），可手动打开 {}", path.display()),
    }
    enter(ctx);
}

/// Discover + sort (category, name) — the canonical ordering.
pub(crate) fn fresh_list(root: &Path) -> Vec<Exercise> {
    let mut v = exercise::discover(root);
    v.sort_by(|a, b| a.category.cmp(&b.category).then(a.name.cmp(&b.name)));
    v
}

fn first_pending(exercises: &[Exercise], progress: &[String]) -> Option<usize> {
    exercises.iter().position(|e| !e.is_done(progress))
}

/// Practice prompts: Ctrl-C cancels the line (loop continues), EOF
/// leaves the page back to the chat.
fn read_prompt(prompt: &str) -> Option<String> {
    match super::read_line(prompt) {
        super::Line::Text(s) => Some(s),
        super::Line::Interrupted => {
            println!("  ^C 已取消本行输入");
            None
        }
        super::Line::Eof => None,
    }
}

fn show_menu(exercises: &[Exercise], progress: &[String]) {
    let mut cat = String::new();
    for (i, ex) in exercises.iter().enumerate() {
        if ex.category != cat {
            cat = ex.category.clone();
            println!("  {}:", capitalize(&cat));
        }
        let mark = if ex.is_done(progress) { 'x' } else { ' ' };
        println!("  {:>2}. [{}] {:<14} {}", i + 1, mark, ex.name, ex.title);
    }
    println!();
    println!("  <数字> 选题   n 下一题   v 全部验证   b 返回对话");
}

/// Page frame for the practice list (M4.2): viewport clear + title +
/// progress bar. Used on entry and after returning from an exercise.
fn page_frame(exercises: &[Exercise], progress: &[String]) {
    if super::render::ansi_enabled() {
        super::render::clear_viewport();
    }
    let done = exercises.iter().filter(|e| e.is_done(progress)).count();
    println!("{}", super::render::cyan("── 做题模式 ──"));
    println!(
        "  进度 {}",
        super::render::progress_bar(done, exercises.len(), 20)
    );
    println!();
}

/// Compile+run one exercise; `r` rerun, `e` edit, `n` next pending,
/// `b` back to the list. Each (re)entry repaints the exercise page:
/// viewport clear + title, so compiler output always starts clean.
pub(crate) fn run_exercise(
    idx: usize,
    exercises: &[Exercise],
    progress: &mut Vec<String>,
    progress_path: &Path,
    editor: Option<&str>,
) {
    let ex = &exercises[idx];
    repaint_exercise(ex);
    loop {
        let ok = exercise::compile_and_run(ex);
        if ok && !ex.is_done(progress) {
            progress.push(ex.path.to_string_lossy().into_owned());
            save_progress(progress_path, progress);
            println!();
            println!("  练习 '{}' 完成 - 已标记。", ex.name);
        }
        println!();
        println!("  [r] 重跑   [e] 编辑   [n] 下一题   [b] 返回");
        let Some(s) = read_prompt("> ") else { return };
        match s.trim() {
            "r" | "" => repaint_exercise(ex),
            "e" => {
                exercise::open_editor(&ex.path, editor);
                repaint_exercise(ex);
            }
            "n" => {
                let next = next_pending(idx, exercises, progress);
                if let Some(next) = next {
                    run_exercise(next, exercises, progress, progress_path, editor);
                }
                return;
            }
            "b" | "q" | "back" => return,
            other => println!("未知命令: {other}"),
        }
    }
}

/// Exercise page: viewport clear + title line (before compile output).
fn repaint_exercise(ex: &Exercise) {
    if super::render::ansi_enabled() {
        super::render::clear_viewport();
    }
    println!("--- {} ({}) ---", ex.name, ex.path.display());
}

fn next_pending(from: usize, exercises: &[Exercise], progress: &[String]) -> Option<usize> {
    let n = exercises.len();
    for off in 1..=n {
        let i = (from + off) % n;
        if !exercises[i].is_done(progress) {
            return Some(i);
        }
    }
    None
}

fn verify_all(exercises: &[Exercise], progress: &mut Vec<String>, progress_path: &Path) {
    let total = exercises.len();
    let mut pass = 0;
    for (i, ex) in exercises.iter().enumerate() {
        if crate::agent::is_interrupted() {
            crate::agent::reset_interrupt();
            println!();
            println!("  已打断（{pass}/{i} 通过）。");
            return;
        }
        println!();
        println!("[{}/{}] {} - {}", i + 1, total, ex.name, ex.title);
        let ok = exercise::compile_and_run(ex);
        if ok {
            pass += 1;
            if !ex.is_done(progress) {
                progress.push(ex.path.to_string_lossy().into_owned());
            }
            println!("  {}", super::render::green("✓ 通过"));
        } else {
            println!("  {}", super::render::red("✗ 未通过"));
        }
    }
    save_progress(progress_path, progress);
    println!();
    let summary = format!("==== 全部验证完成: {pass}/{total} 通过 ====");
    println!("{}", if pass == total { super::render::green(&summary) } else { summary });
}

fn save_progress(p: &Path, progress: &[String]) {
    let _ = std::fs::write(p, progress.join("\n"));
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => format!("{}{}", c.to_uppercase().collect::<String>(), chars.as_str()),
        None => String::new(),
    }
}
