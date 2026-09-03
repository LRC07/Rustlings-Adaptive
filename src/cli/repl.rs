//! Conversation REPL (M4): the default first screen. Plain input goes
//! to the agent (model + local tools); slash commands open the
//! built-in pages. No screen clearing — the transcript scrolls like a
//! chat, which also fixes the old "menu wipe swallows the usage page"
//! behavior.
//!
//! Agent turns run on a worker thread with a live spinner (R4); Ctrl-C
//! abandons the turn and returns to the prompt. Every completed turn
//! is appended to the session JSON file (R5).

use std::path::PathBuf;
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::agent::{self, session::Session, AgentEnv};
use crate::config::ModelConfig;
use crate::llm::LlmClient;
use crate::usage::UsageTracker;

use super::{generate, make_client, practice, read_line_trimmed, spinner::Spinner};

pub(crate) fn run() {
    let root = PathBuf::from(".");
    if !root.join("exercises").is_dir() {
        eprintln!("错误：找不到 'exercises' 目录（请在项目根目录运行）");
        std::process::exit(1);
    }
    let mut cfg = match ModelConfig::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("配置加载失败：{e:#}");
            eprintln!("请检查项目根目录的 config.toml（删除它可恢复默认配置）。");
            std::process::exit(1);
        }
    };
    let tracker = Arc::new(Mutex::new(UsageTracker::load_or_create()));
    let mut client = make_client(&cfg);
    install_ctrlc();

    println!();
    println!("  欢迎使用 my_rustlings —— 对话式 Rust 诊断教练");
    println!(
        "  模型：{} ｜ 直接输入问题开始对话，/help 查看全部命令",
        if client.is_some() { cfg.model.as_str() } else { "未配置（/config 填 API Key）" }
    );

    // R5: resume the newest session so a restart continues the talk.
    let mut session = match Session::resume_latest() {
        Some(s) if !s.messages.is_empty() => {
            println!(
                "  已恢复上次会话 {}（{} 条消息；/new 开新会话）",
                s.id,
                s.messages.len()
            );
            s
        }
        _ => Session::new(&cfg.model),
    };
    println!();

    let mut practice_ctx = practice::PracticeCtx::new(&root.join("exercises"), cfg.editor.clone());

    loop {
        if agent::is_interrupted() {
            agent::reset_interrupt();
            println!("  ^C（任务执行中按 Ctrl-C 打断；输入 /exit 退出）");
        }
        let Some(line) = read_line_trimmed("你> ") else {
            println!();
            break;
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // ```-fenced paste mode: lines between two ``` lines become ONE
        // message, so pasting multi-line code does not fire one turn
        // per line (the "paste a snippet" scenario is core).
        let line = if line.starts_with("```") {
            match read_paste() {
                Some(code) if !code.trim().is_empty() => code,
                _ => continue,
            }
        } else {
            line.to_string()
        };
        match parse_command(&line) {
            Cmd::Exit => break,
            Cmd::Help => print_help(),
            Cmd::New => {
                let _ = session.save();
                session = Session::new(&cfg.model);
                println!("  已开启新会话 {}（旧会话仍在 /sessions 中可回看）", session.id);
            }
            Cmd::Practice => {
                agent::reset_interrupt();
                practice::enter(&practice_ctx);
            }
            Cmd::Generate(arg) => {
                agent::reset_interrupt();
                generate::cmd_generate(&cfg, &tracker, &client, &practice_ctx, arg);
            }
            Cmd::Usage => print_usage(&cfg, &tracker),
            Cmd::Config => {
                cmd_config(&mut cfg, &mut client);
                practice_ctx.editor = cfg.editor.clone();
            }
            Cmd::Sessions(arg) => sessions_page(arg.as_deref()),
            Cmd::Unknown(raw) => println!("  未知命令 {raw}（/help 查看命令列表）"),
            Cmd::Chat => agent_turn(&mut session, &line, &cfg, &tracker, &client, &practice_ctx),
        }
    }

    if let Err(e) = session.save() {
        eprintln!("会话保存失败：{e:#}");
    }
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

enum Cmd<'a> {
    Exit,
    Help,
    New,
    Practice,
    Generate(Option<&'a str>),
    Usage,
    Config,
    Sessions(Option<String>),
    Unknown(String),
    Chat,
}

fn parse_command(line: &str) -> Cmd<'_> {
    if !line.starts_with('/') {
        return Cmd::Chat;
    }
    let (cmd, arg) = match line[1..].split_once(char::is_whitespace) {
        Some((c, a)) => (c, Some(a.trim())),
        None => (&line[1..], None),
    };
    let arg = arg.filter(|a| !a.is_empty());
    match (cmd, arg) {
        ("exit", _) | ("quit", _) | ("q", _) => Cmd::Exit,
        ("help", _) | ("h", _) | ("?", _) => Cmd::Help,
        ("new", _) => Cmd::New,
        ("practice", _) | ("p", _) => Cmd::Practice,
        ("generate", a) | ("g", a) => Cmd::Generate(a),
        ("usage", _) | ("u", _) => Cmd::Usage,
        ("config", _) | ("c", _) => Cmd::Config,
        ("sessions", a) | ("s", a) => Cmd::Sessions(a.map(str::to_string)),
        (raw, _) => Cmd::Unknown(format!("/{raw}")),
    }
}

fn print_help() {
    println!();
    println!("  对话：直接输入问题 / 贴报错或代码。教练会锚定错误码与概念，");
    println!("        需要时本地编译你的代码取证（check_code），或生成一道可开练");
    println!("        的小练习（generate_exercise）。任务执行中可随时 Ctrl-C 打断。");
    println!("  命令：");
    println!("    /new        开启新会话（旧会话落盘可回看）");
    println!("    /practice   做题模式（练习列表，数字选题 / n 下一题 / v 全部验证）");
    println!("    /generate   直接生成练习（可带主题：/g E0382；离线也可用）");
    println!("    /usage      用量与花费（本次会话 / 累计 / 预算余量）");
    println!("    /config     模型配置页（endpoint / model / api_key / 预算 / 编辑器）");
    println!("    /sessions   会话列表；/sessions <序号> 查看该会话的完整轨迹");
    println!("    /exit       退出");
    println!("  模型调用的累计花费达到预算上限时会被自动拦截。");
    println!();
}

// ---------------------------------------------------------------------------
// Agent turn (worker thread + spinner + interrupt, R4)
// ---------------------------------------------------------------------------

fn agent_turn(
    session: &mut Session,
    input: &str,
    cfg: &ModelConfig,
    tracker: &Arc<Mutex<UsageTracker>>,
    client: &Option<LlmClient>,
    practice_ctx: &practice::PracticeCtx,
) {
    let Some(cl) = client else {
        println!();
        println!("  尚未配置 API Key，无法对话。两种方式：");
        println!("    1. 复制 config.example.toml 为 config.toml，填入 api_key / endpoint / model");
        println!("    2. 在项目根目录创建 .env，写入 RUSTLINGS_API_KEY=sk-...");
        println!("  也可以输入 /config 交互填写。离线可用：/practice 做题、/generate 出题。");
        return;
    };

    agent::reset_interrupt();
    let env = AgentEnv {
        caller: Arc::new(cl.clone()),
        tracker: tracker.clone(),
        cfg: cfg.clone(),
        root: PathBuf::from("."),
    };
    let history = session.messages.clone();
    let input = input.to_string();

    let (spinner, status_slot) = Spinner::start("思考中…");
    let (tx, rx) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        let progress = move |s: &str| {
            if let Ok(mut slot) = status_slot.lock() {
                *slot = s.to_string();
            }
        };
        let result = agent::run_turn(&history, &input, &env, &progress);
        let _ = tx.send(result);
    });

    // Poll for the turn result, watching the interrupt flag (R4).
    let mut outcome = None;
    loop {
        match rx.recv_timeout(Duration::from_millis(150)) {
            Ok(res) => {
                outcome = Some(res);
                break;
            }
            Err(RecvTimeoutError::Timeout) => {
                if agent::is_interrupted() {
                    super::flush_stdin();
                    println!("  已打断（本回合中止；后台调用完成后仍会计入用量）。");
                    break;
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    spinner.stop();
    let _ = handle; // abandoned threads finish in the background

    match outcome {
        Some(Ok(turn)) => {
            session.messages = turn.history;
            println!();
            if let Some(text) = &turn.reply {
                println!("{text}");
            }
            for note in &turn.tool_notes {
                println!("  · {note}");
            }
            // R6: per-turn usage footer.
            let total = tracker.lock().expect("usage lock").all_totals();
            print!(
                "  ─ 本回合: {} 次调用 ｜ 输入 {} tok ｜ 输出 {} tok ｜ ${:.6}",
                turn.calls, turn.input_tokens, turn.output_tokens, turn.cost_usd
            );
            match cfg.budget_usd() {
                Some(b) => println!("  ｜ 累计 ${:.4} / 预算 ${:.2}", total.cost_usd, b),
                None => println!("  ｜ 累计 ${:.4}", total.cost_usd),
            }
            if let Err(e) = session.save() {
                println!("  会话保存失败：{e:#}");
            }

            // Practice offer from generate_exercise.
            if let Some(offer) = turn.practice {
                println!();
                println!("  题目已就绪：《{}》（{}）", offer.title, offer.difficulty);
                match read_line_trimmed("  回车开始做题，输入 n 留在对话> ") {
                    None => {}
                    Some(ans) => {
                        let a = ans.trim().to_ascii_lowercase();
                        if a.is_empty() || a == "y" || a == "yes" || a == "是" {
                            practice::enter_at(practice_ctx, &offer.path);
                        }
                    }
                }
            }
        }
        Some(Err(e)) => println!("  出错：{e:#}"),
        None => {} // interrupted: nothing to show; history untouched
    }
    println!();
}

// ---------------------------------------------------------------------------
// Pages: usage / config / sessions
// ---------------------------------------------------------------------------

/// `/usage` — cost report (R6). Prints inline, never clears the screen.
fn print_usage(cfg: &ModelConfig, tracker: &Arc<Mutex<UsageTracker>>) {
    let t = tracker.lock().expect("usage lock");
    let s = t.session_totals();
    let a = t.all_totals();
    println!();
    println!("── 用量与花费 ──");
    println!(
        "  本次会话: {} 次调用 ｜ 输入 {} tok ｜ 输出 {} tok ｜ ${:.4}",
        s.calls, s.input_tokens, s.output_tokens, s.cost_usd
    );
    for (phase, pt) in t.session_by_phase() {
        println!(
            "    · {phase:<10} {} 次 ｜ 输入 {} tok ｜ 输出 {} tok ｜ ${:.6}",
            pt.calls, pt.input_tokens, pt.output_tokens, pt.cost_usd
        );
    }
    println!(
        "  历史累计: {} 次调用 ｜ 输入 {} tok ｜ 输出 {} tok ｜ ${:.4}",
        a.calls, a.input_tokens, a.output_tokens, a.cost_usd
    );
    println!("  明细文件: {}", t.path().display());
    match cfg.budget_usd() {
        Some(b) => {
            let remaining = (b - a.cost_usd).max(0.0);
            let pct = if b > 0.0 { a.cost_usd / b * 100.0 } else { f64::INFINITY };
            println!("  预算: ${:.2} ｜ 已用 {:.1}% ｜ 剩余 ${:.4}", b, pct, remaining);
        }
        None => println!("  预算: 未设置（/config 中可设置；达到上限后自动拦截调用）"),
    }
}

/// `/config` — interactive model config page (R3). Edits are written
/// back to config.toml; the client is rebuilt so changes apply at once.
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
        let Some(line) = read_line_trimmed("配置> ") else { return };
        match line.as_str() {
            "" => return,
            "1" => {
                if let Some(v) = read_line_trimmed("新 endpoint: ") {
                    cfg.endpoint = v;
                    save_and_rebuild(cfg, client);
                }
            }
            "2" => {
                if let Some(v) = read_line_trimmed("新 model: ") {
                    cfg.model = v;
                    save_and_rebuild(cfg, client);
                }
            }
            "3" => {
                if let Some(v) = read_line_trimmed("新 api_key（输入明文，回车确认）: ") {
                    cfg.api_key = v;
                    cfg.key_source = crate::config::KeySource::ConfigFile;
                    save_and_rebuild(cfg, client);
                }
            }
            "4" => {
                if let Some(v) = read_line_trimmed("新预算（数字=USD；'无' 取消预算）: ") {
                    if v == "无" || v == "off" || v == "none" {
                        cfg.budget = None;
                    } else if let Ok(n) = v.parse::<f64>() {
                        if n < 0.0 {
                            println!("  预算需 ≥ 0");
                            continue;
                        }
                        cfg.budget = Some(crate::config::Budget { usd: n });
                    } else {
                        println!("  无法识别: {v}（输入数字或'无'）");
                        continue;
                    }
                    save_and_rebuild(cfg, client);
                }
            }
            "5" => {
                if let Some(v) = read_line_trimmed("新编辑器命令（如 'code --wait'；'无' 恢复自动）: ") {
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
        Ok(()) => println!("  已写入 {}", crate::config::CONFIG_FILE),
        Err(e) => println!("  写入配置失败：{e:#}"),
    }
    *client = make_client(cfg);
}

/// `/sessions` — list saved sessions or print one session's trajectory
/// (R5: the agent's actual workflow, inspectable).
fn sessions_page(arg: Option<&str>) {
    let infos = Session::list();
    if infos.is_empty() {
        println!("  还没有会话记录（对话后自动保存在 ~/.rustlings_adaptive/sessions/）。");
        return;
    }
    if let Some(arg) = arg {
        match arg.parse::<usize>() {
            Ok(n) if n >= 1 && n <= infos.len() => {
                let info = &infos[n - 1];
                match Session::load(&info.path) {
                    Ok(s) => {
                        println!();
                        println!("── 会话 {}（{} 条消息，开始于 {}）──", s.id, s.messages.len(),
                            s.started_at.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M:%S"));
                        print!("{}", agent::session::trajectory_text(&s.messages, 500));
                        println!();
                    }
                    Err(e) => println!("  读取会话失败：{e:#}"),
                }
            }
            _ => println!("  序号需在 1..={} 内（/sessions 不带参数查看列表）", infos.len()),
        }
        return;
    }
    println!();
    println!("── 会话列表（/sessions <序号> 查看轨迹）──");
    for (i, info) in infos.iter().enumerate() {
        println!(
            "  {:>2}. {} ｜ {} 条消息 ｜ 开始 {}",
            i + 1,
            info.id,
            info.messages,
            info.started_at.with_timezone(&chrono::Local).format("%m-%d %H:%M")
        );
    }
}

fn install_ctrlc() {
    if let Err(e) = ctrlc::set_handler(crate::agent::set_interrupt) {
        eprintln!("（提示：Ctrl-C 打断不可用：{e}）");
    }
}

/// Multi-line paste mode: collect lines until the closing ``` line.
fn read_paste() -> Option<String> {
    println!("  ─ 粘贴模式：继续粘贴代码，单独一行 ``` 结束 ─");
    let mut buf = String::new();
    loop {
        match read_line_trimmed("") {
            None => return None,
            Some(l) => {
                if l.trim() == "```" {
                    return Some(buf);
                }
                buf.push_str(&l);
                buf.push('\n');
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_parsing() {
        assert!(matches!(parse_command("你好"), Cmd::Chat));
        assert!(matches!(parse_command("/exit"), Cmd::Exit));
        assert!(matches!(parse_command("/q"), Cmd::Exit));
        assert!(matches!(parse_command("/help"), Cmd::Help));
        assert!(matches!(parse_command("/p"), Cmd::Practice));
        assert!(matches!(parse_command("/usage"), Cmd::Usage));
        assert!(matches!(parse_command("/u"), Cmd::Usage));
        assert!(matches!(parse_command("/config"), Cmd::Config));
        assert!(matches!(parse_command("/new"), Cmd::New));
        assert!(matches!(parse_command("/sessions"), Cmd::Sessions(None)));
        assert!(matches!(parse_command("/sessions 3"), Cmd::Sessions(Some(_))));
        assert!(matches!(parse_command("/g"), Cmd::Generate(None)));
        assert!(matches!(parse_command("/g E0382"), Cmd::Generate(Some("E0382"))));
        assert!(matches!(parse_command("/helo"), Cmd::Unknown(_)));
        // A lone slash command word with no meaning is unknown.
        assert!(matches!(parse_command("/ "), Cmd::Unknown(_)));
    }
}
