//! Interactive CLI: exercise menu/loop plus the model-facing commands
//! (`a` ask / `g` generate / `u` usage / `c` config) required by
//! R2/R3/R6. The `g` flow lives in `cli/generate.rs`.

use std::io::{self, IsTerminal, Write};
use std::path::Path;

use crate::config::{self, ModelConfig};
use crate::exercise::{self, Exercise};
use crate::llm::LlmClient;
use crate::usage;

mod generate;

pub fn run() {
    let root = Path::new("exercises");
    if !root.exists() {
        eprintln!("错误：找不到 '{}' 目录（请在项目根目录运行）", root.display());
        std::process::exit(1);
    }
    let mut exercises = exercise::discover(root);
    exercises.sort_by(|a, b| a.category.cmp(&b.category).then(a.name.cmp(&b.name)));
    if exercises.is_empty() {
        eprintln!("在 {} 下没有找到练习", root.display());
        std::process::exit(1);
    }

    // Model config / usage tracker / LLM client (M1).
    let mut cfg = match config::ModelConfig::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("配置加载失败：{e:#}");
            eprintln!("请检查项目根目录的 config.toml（删除它可恢复默认配置）。");
            std::process::exit(1);
        }
    };
    let mut tracker = usage::UsageTracker::load_or_create();
    let mut client = make_client(&cfg);

    let progress_path = root.join(".progress");
    let mut progress = load_progress(&progress_path);

    println!();
    println!("  欢迎使用 my_rustlings —— 对话式 Rust 诊断教练");
    println!(
        "  共 {} 道练习 ｜ 模型：{} ｜ [h] 帮助",
        exercises.len(),
        if client.is_some() {
            cfg.model.as_str()
        } else {
            "未配置（[c] 填 API Key）"
        }
    );
    println!();

    loop {
        clear_screen();
        show_menu(&exercises, &progress);
        print!("> ");
        io::stdout().flush().ok();
        let mut input = String::new();
        if io::stdin().read_line(&mut input).unwrap_or(0) == 0 {
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
                    run_exercise(idx, &exercises, &mut progress, &progress_path, cfg.editor.as_deref());
                } else {
                    println!();
                    println!("  所有练习已完成！");
                    println!();
                }
            }
            "a" => cmd_ask(&cfg, &mut tracker, &client),
            "g" => generate::cmd_generate(
                &cfg,
                &mut tracker,
                &client,
                &mut exercises,
                &mut progress,
                &progress_path,
            ),
            "u" => cmd_usage(&cfg, &tracker),
            "c" => cmd_config(&mut cfg, &mut client),
            "h" => print_help(),
            s => match s.parse::<usize>() {
                Ok(n) if n >= 1 && n <= exercises.len() => {
                    run_exercise(n - 1, &exercises, &mut progress, &progress_path, cfg.editor.as_deref());
                }
                _ => println!("未知命令: {s}（可用：数字、n、v、a、g、u、c、h、q）"),
            },
        }
    }
}

/// Wipe the terminal before each menu render. Only for real terminals;
/// piped output (tests, logs) keeps everything for readability.
fn clear_screen() {
    if io::stdout().is_terminal() {
        print!("\x1B[2J\x1B[1;1H");
        io::stdout().flush().ok();
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
    println!("======== 进度: {}/{} ({}%) ========", done, exercises.len(), pct);
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
    println!("  <数字> 选题   n 下一题   v 全部验证   a 问模型   g 生成练习   u 用量   c 配置   h 帮助   q 退出");
}

fn print_help() {
    println!();
    println!("  使用说明：");
    println!("    - 输入编号选择练习。");
    println!("    - 做题界面内：[r] 重新编译运行   [e] 编辑（编辑器解析链：");
    println!("                  $EDITOR → $VISUAL → 配置 editor → code --wait → vi，[c] 中可改）");
    println!("                  [n] 下一题         [b] 返回菜单");
    println!("    - 全部测试通过后自动标记完成；n 跳到下一道未完成；v 全部跑一遍。");
    println!("    - a 问模型：一问一答（输入问题回车发送；多轮对话在后续版本提供）");
    println!("    - g 生成练习：输入主题（概念/错误码/关键词），自动出题并进入做题。");
    println!("      生成 = 选模板 → 填槽 → 三重校验，最多重试 3 轮，成功后可立即开练。");
    println!("      u 用量：本次会话与历史累计的 token/花费、预算余量");
    println!("      c 配置：查看/修改 endpoint、model、api_key、预算（写回 config.toml）");
    println!("    - 模型调用的累计花费达到预算上限时会被自动拦截。");
    println!();
}

// ---------------------------------------------------------------------------
// Model-facing commands (M1: R3 配置 / R6 计费)
// ---------------------------------------------------------------------------

fn make_client(cfg: &ModelConfig) -> Option<LlmClient> {
    if cfg.api_key.trim().is_empty() {
        None
    } else {
        Some(LlmClient::new(&cfg.endpoint, &cfg.api_key, &cfg.model))
    }
}

fn read_value(prompt: &str) -> Option<String> {
    print!("{prompt} ");
    io::stdout().flush().ok();
    let mut s = String::new();
    if io::stdin().read_line(&mut s).unwrap_or(0) == 0 {
        return None;
    }
    let t = s.trim().to_string();
    if t.is_empty() {
        None
    } else {
        Some(t)
    }
}

/// `a` — one-shot question to the model with token/cost report (R6).
fn cmd_ask(cfg: &ModelConfig, tracker: &mut usage::UsageTracker, client: &Option<LlmClient>) {
    println!();
    println!("── 向模型提问（一问一答；输入问题后回车发送，直接回车返回）──");
    print!("问题> ");
    io::stdout().flush().ok();
    let mut q = String::new();
    if io::stdin().read_line(&mut q).unwrap_or(0) == 0 {
        return;
    }
    let q = q.trim();
    if q.is_empty() {
        return;
    }

    // R6: budget gate before every call.
    if let Err(e) = usage::check_budget(tracker.all_totals().cost_usd, cfg.budget_usd()) {
        println!();
        println!("  调用被拦截：{e}");
        println!("  可在 [c] 配置或 config.toml 的 [budget] 调整预算；[u] 查看用量。");
        return;
    }

    let Some(cl) = client.as_ref() else {
        println!();
        println!("  尚未配置 API Key，无法调用模型。两种方式：");
        println!("    1. 复制 config.example.toml 为 config.toml，填入 api_key / endpoint / model");
        println!("    2. 在项目根目录创建 .env，写入 RUSTLINGS_API_KEY=sk-...");
        println!("  也可以回到菜单按 [c] 交互填写。详见 README「配置」一节。");
        return;
    };

    println!("  思考中…（首次调用可能较慢）");
    match cl.chat(q) {
        Ok(reply) => {
            println!();
            println!("{}", reply.content);
            let cost = usage::cost_usd(
                reply.usage.prompt_tokens,
                reply.usage.completion_tokens,
                cfg.prices.input,
                cfg.prices.output,
            );
            tracker.record(
                &cfg.model,
                reply.usage.prompt_tokens,
                reply.usage.completion_tokens,
                cost,
                "chat",
            );
            let total = tracker.all_totals();
            println!();
            print!(
                "  ─ 本条: 输入 {} tok / 输出 {} tok / 花费 ${:.6}",
                reply.usage.prompt_tokens, reply.usage.completion_tokens, cost
            );
            if reply.usage.prompt_tokens == 0 && reply.usage.completion_tokens == 0 {
                print!("（API 未返回用量，按 0 计）");
            }
            match cfg.budget_usd() {
                Some(b) => println!("  ｜ 累计 ${:.4} / 预算 ${:.2}", total.cost_usd, b),
                None => println!("  ｜ 累计 ${:.4}", total.cost_usd),
            }
        }
        Err(e) => {
            println!();
            println!("  调用失败：{e:#}");
        }
    }
}

/// `u` — usage & cost report (R6).
fn cmd_usage(cfg: &ModelConfig, tracker: &usage::UsageTracker) {
    let s = tracker.session_totals();
    let a = tracker.all_totals();
    println!();
    println!("── 用量与花费 ──");
    println!(
        "  本次会话: {} 次调用 ｜ 输入 {} tok ｜ 输出 {} tok ｜ ${:.4}",
        s.calls, s.input_tokens, s.output_tokens, s.cost_usd
    );
    println!(
        "  历史累计: {} 次调用 ｜ 输入 {} tok ｜ 输出 {} tok ｜ ${:.4}",
        a.calls, a.input_tokens, a.output_tokens, a.cost_usd
    );
    println!("  明细文件: {}", tracker.path().display());
    match cfg.budget_usd() {
        Some(b) => {
            let remaining = (b - a.cost_usd).max(0.0);
            let pct = if b > 0.0 { a.cost_usd / b * 100.0 } else { f64::INFINITY };
            println!("  预算: ${:.2} ｜ 已用 {:.1}% ｜ 剩余 ${:.4}", b, pct, remaining);
        }
        None => println!("  预算: 未设置（[c] 中可设置；达到上限后自动拦截调用）"),
    }
}

/// `c` — interactive model config page (R3). Edits are written back to
/// config.toml; the LLM client is rebuilt so changes take effect at once.
fn cmd_config(cfg: &mut ModelConfig, client: &mut Option<LlmClient>) {
    loop {
        println!();
        println!("── 模型配置 ──");
        println!("  1. endpoint : {}", cfg.endpoint);
        println!("  2. model    : {}", cfg.model);
        if cfg.api_key.is_empty() {
            println!("  3. api_key  : 未设置");
        } else {
            println!("  3. api_key  : {}（{}）", cfg.masked_key(), cfg.key_source_cn());
        }
        println!(
            "  4. 预算 USD : {}",
            cfg.budget
                .as_ref()
                .map(|b| format!("${:.2}", b.usd))
                .unwrap_or_else(|| "未设置".to_string())
        );
        println!(
            "  5. 编辑器 : {}",
            cfg.editor
                .as_deref()
                .map(|e| e.to_string())
                .unwrap_or_else(|| "未设置（自动：$EDITOR → $VISUAL → code --wait → vi）".to_string())
        );
        println!(
            "  · 价格      : 输入 ${:.2}/1M ｜ 输出 ${:.2}/1M（在 config.toml 中修改）",
            cfg.prices.input, cfg.prices.output
        );
        println!(
            "  · 上下文    : {} tokens ｜ 思考模式: {}（预留字段）",
            cfg.context_len,
            if cfg.think_mode { "开" } else { "关" }
        );
        println!("  输入编号修改对应项（1/2/3/4/5），回车返回；修改会写回 config.toml");
        print!("配置> ");
        io::stdout().flush().ok();
        let mut line = String::new();
        if io::stdin().read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        match line.trim() {
            "" => return,
            "1" => {
                if let Some(v) = read_value("新 endpoint:") {
                    cfg.endpoint = v;
                    save_and_rebuild(cfg, client);
                }
            }
            "2" => {
                if let Some(v) = read_value("新 model:") {
                    cfg.model = v;
                    save_and_rebuild(cfg, client);
                }
            }
            "3" => {
                if let Some(v) = read_value("新 api_key（输入明文，回车确认）:") {
                    cfg.api_key = v;
                    cfg.key_source = config::KeySource::ConfigFile;
                    save_and_rebuild(cfg, client);
                }
            }
            "4" => {
                if let Some(v) = read_value("新预算（数字=USD；'无' 取消预算）:") {
                    if v == "无" || v == "off" || v == "none" {
                        cfg.budget = None;
                    } else if let Ok(n) = v.parse::<f64>() {
                        if n < 0.0 {
                            println!("  预算需 ≥ 0");
                            continue;
                        }
                        cfg.budget = Some(config::Budget { usd: n });
                    } else {
                        println!("  无法识别: {v}（输入数字或'无'）");
                        continue;
                    }
                    save_and_rebuild(cfg, client);
                }
            }
            "5" => {
                if let Some(v) = read_value("新编辑器命令（如 'code --wait'；'无' 恢复自动）:") {
                    if v == "无" || v == "none" {
                        cfg.editor = None;
                    } else {
                        cfg.editor = Some(v);
                    }
                    save_and_rebuild(cfg, client);
                }
            }
            other => println!("  未知选项: {other}"),
        }
    }
}

fn save_and_rebuild(cfg: &ModelConfig, client: &mut Option<LlmClient>) {
    match cfg.save_to_default_file() {
        Ok(()) => println!("  已写入 {}", config::CONFIG_FILE),
        Err(e) => println!("  写入配置失败：{e:#}"),
    }
    *client = make_client(cfg);
}

// ---------------------------------------------------------------------------
// Exercise flow
// ---------------------------------------------------------------------------

pub(super) fn run_exercise(
    idx: usize,
    exercises: &[Exercise],
    progress: &mut Vec<String>,
    progress_path: &Path,
    editor: Option<&str>,
) {
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
            println!("  练习 '{}' 完成 - 已标记。", ex.name);
        }
        println!();
        println!("  [r] 重跑   [e] 编辑   [n] 下一题   [b] 返回");
        print!("> ");
        io::stdout().flush().ok();
        let mut s = String::new();
        if io::stdin().read_line(&mut s).unwrap_or(0) == 0 {
            return;
        }
        match s.trim() {
            "r" | "" => continue,
            "e" => exercise::open_editor(&ex.path, editor),
            "n" => {
                if let Some(next) = next_pending(idx, exercises, progress) {
                    run_exercise(next, exercises, progress, progress_path, editor);
                }
                return;
            }
            "b" | "q" | "back" => return,
            other => {
                println!("未知命令: {other}");
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
    println!("==== 全部验证完成: {pass}/{total} 通过 ====");
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
