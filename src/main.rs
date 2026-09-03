use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{exit, Command};

struct Exercise {
    path: PathBuf,
    name: String,
    category: String,
    title: String,
}

impl Exercise {
    fn is_done(&self, progress: &[String]) -> bool {
        progress.iter().any(|p| p == self.path.to_string_lossy().as_ref())
    }
}

fn main() {
    let root = Path::new("exercises");
    if !root.exists() {
        eprintln!("error: '{}' directory not found (run from the project root)", root.display());
        exit(1);
    }
    let mut exercises = discover(root);
    exercises.sort_by(|a, b| a.category.cmp(&b.category).then(a.name.cmp(&b.name)));
    if exercises.is_empty() {
        eprintln!("no exercises found under {}", root.display());
        exit(1);
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
        let ok = compile_and_run(ex);
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
            "e" => open_editor(&ex.path),
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

fn compile_and_run(ex: &Exercise) -> bool {
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

fn verify_all(exercises: &[Exercise], progress: &mut Vec<String>, progress_path: &Path) {
    let total = exercises.len();
    let mut pass = 0;
    for (i, ex) in exercises.iter().enumerate() {
        println!();
        println!("[{}/{}] {} - {}", i + 1, total, ex.name, ex.title);
        let ok = compile_and_run(ex);
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

fn open_editor(path: &Path) {
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".into());
    match Command::new(&editor).arg(path).status() {
        Ok(s) if s.success() => {}
        Ok(s) => println!("  editor exited with {:?}", s.code()),
        Err(e) => println!("  could not launch '{editor}': {e}"),
    }
}

fn discover(root: &Path) -> Vec<Exercise> {
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

fn load_progress(p: &Path) -> Vec<String> {
    fs::read_to_string(p)
        .map(|s| s.lines().map(str::to_string).collect())
        .unwrap_or_default()
}

fn save_progress(p: &Path, progress: &[String]) {
    let _ = fs::write(p, progress.join("\n"));
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => format!("{}{}", c.to_uppercase().collect::<String>(), chars.as_str()),
        None => String::new(),
    }
}
