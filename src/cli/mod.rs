//! Interactive CLI: progress menu, exercise loop, and (from M1 on) the
//! model-facing commands (`a` ask / `u` usage / `c` config).

use std::io::{self, Write};
use std::path::Path;

use crate::exercise::{self, Exercise};

pub fn run() {
    let root = Path::new("exercises");
    if !root.exists() {
        eprintln!("error: '{}' directory not found (run from the project root)", root.display());
        std::process::exit(1);
    }
    let mut exercises = exercise::discover(root);
    exercises.sort_by(|a, b| a.category.cmp(&b.category).then(a.name.cmp(&b.name)));
    if exercises.is_empty() {
        eprintln!("no exercises found under {}", root.display());
        std::process::exit(1);
    }

    let progress_path = root.join(".progress");
    let mut progress = load_progress(&progress_path);

    println!();
    println!("  Welcome to my_rustlings - Generics & Traits");
    println!("  {} exercises available", exercises.len());
    println!("  Edit a file, save, then press [r] to recompile.");
    println!();

    loop {
        show_menu(&exercises, &progress);
        print!("> ");
        io::stdout().flush().ok();
        let mut input = String::new();
        if io::stdin().read_line(&mut input).unwrap() == 0 {
            println!();
            break;
        }
        let input = input.trim();
        if input.is_empty() {
            continue;
        }
        match input {
            "q" | "quit" | "exit" => break,
            "v" | "verify" => verify_all(&exercises, &mut progress, &progress_path),
            "n" => {
                if let Some(idx) = exercises.iter().position(|e| !e.is_done(&progress)) {
                    run_exercise(idx, &exercises, &mut progress, &progress_path);
                } else {
                    println!();
                    println!("  All exercises complete!");
                    println!();
                }
            }
            "h" => print_help(),
            s => match s.parse::<usize>() {
                Ok(n) if n >= 1 && n <= exercises.len() => {
                    run_exercise(n - 1, &exercises, &mut progress, &progress_path);
                }
                _ => println!("unknown command: {s} (try a number, n, v, h, q)"),
            },
        }
    }
}

fn show_menu(exercises: &[Exercise], progress: &[String]) {
    let done = exercises.iter().filter(|e| e.is_done(progress)).count();
    let pct = if exercises.is_empty() {
        100
    } else {
        done * 100 / exercises.len()
    };
    println!();
    println!("======== Progress: {}/{} ({}%) ========", done, exercises.len(), pct);
    println!();
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
    println!("  <n> run   n=next   v=verify all   h=help   q=quit");
}

fn print_help() {
    println!();
    println!("  How to use:");
    println!("    - Pick an exercise by number.");
    println!("    - Inside the exercise prompt, press [e] to open the file in");
    println!("      $EDITOR (defaults to vi), fix the TODO, save, then press");
    println!("      [r] (or Enter) to recompile and run the tests.");
    println!("    - When all tests pass, the exercise is marked done.");
    println!("    - 'n' jumps to the next pending exercise.");
    println!("    - 'v' runs every exercise and reports the totals.");
    println!();
}

fn run_exercise(idx: usize, exercises: &[Exercise], progress: &mut Vec<String>, progress_path: &Path) {
    let ex = &exercises[idx];
    println!();
    println!("--- {} ({}) ---", ex.name, ex.path.display());
    println!();
    loop {
        let ok = exercise::compile_and_run(ex);
        if ok && !ex.is_done(progress) {
            progress.push(ex.path.to_string_lossy().into_owned());
            save_progress(progress_path, progress);
            println!();
            println!("  Exercise '{}' complete - marked done.", ex.name);
        }
        println!();
        println!("  [r] rerun   [e] edit   [n] next pending   [b] back");
        print!("> ");
        io::stdout().flush().ok();
        let mut s = String::new();
        if io::stdin().read_line(&mut s).unwrap() == 0 {
            return;
        }
        match s.trim() {
            "r" | "" => continue,
            "e" => exercise::open_editor(&ex.path),
            "n" => {
                if let Some(next) = next_pending(idx, exercises, progress) {
                    run_exercise(next, exercises, progress, progress_path);
                }
                return;
            }
            "b" | "q" | "back" => return,
            other => {
                println!("unknown: {other}");
            }
        }
    }
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
        println!();
        println!("[{}/{}] {} - {}", i + 1, total, ex.name, ex.title);
        let ok = exercise::compile_and_run(ex);
        if ok {
            pass += 1;
            if !ex.is_done(progress) {
                progress.push(ex.path.to_string_lossy().into_owned());
            }
        }
    }
    save_progress(progress_path, progress);
    println!();
    println!("==== verify complete: {pass}/{total} pass ====");
}

fn load_progress(p: &Path) -> Vec<String> {
    std::fs::read_to_string(p)
        .map(|s| s.lines().map(str::to_string).collect())
        .unwrap_or_default()
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
