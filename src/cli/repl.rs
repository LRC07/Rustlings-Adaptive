//! Conversation REPL (M4/M4.2): the default first screen. Plain input
//! goes to the agent (model + local tools); slash commands open the
//! built-in pages.
//!
//! Rendering (M4.2): two modes. "view" (default) repaints a clean
//! viewport before every turn — page header + a dim recap of the last
//! few entries — so the window never drowns in old output; clearing
//! uses ESC[2J only, scrollback is preserved. "scroll" keeps the plain
//! scrolling transcript. Piped output never emits ANSI.
//!
//! Agent turns run on a worker thread with a live spinner (R4); Ctrl-C
//! abandons the turn and returns to the prompt. Every completed turn
//! is appended to the session JSON file (R5).

use anyhow::Context;
use std::path::{Path, PathBuf};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use unicode_width::UnicodeWidthStr;

use crate::agent::{self, session::Session, AgentEnv};
use crate::config::ModelConfig;
use crate::llm::{ChatMessage, LlmClient};
use crate::usage::UsageTracker;

use super::render::{self, chat_header, chat_tail, clear_all, clear_viewport};
use super::{generate, make_client, practice, read_line, read_line_or_leave, spinner::Spinner, Line};

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

    // M9c (方案 C): legacy config layouts are migrated on load; persist
    // the upgraded file once and say so.
    if cfg.migrated {
        match cfg.save_to_default_file() {
            Ok(()) => println!("  配置文件已升级：模型统一为 [[models]] 档案 + active 指针（原顶层配置已成为一个档案）。"),
            Err(e) => eprintln!("  （配置升级写回失败：{e:#}）"),
        }
    }

    // R5: resume the newest session so a restart continues the talk.
    let existing = Session::list();
    let mut session = match Session::resume_latest() {
        Some(s) if !s.messages.is_empty() => {
            println!();
            s
        }
        _ => Session::new(&cfg.model),
    };
    let resumed = !session.messages.is_empty();
    let resumed_msgs = session.messages.len();

    // Viewport-first impression (view mode): clear, then banner page.
    print_banner(
        &session,
        &cfg,
        first_run(existing.is_empty(), resumed),
        resumed,
        resumed_msgs,
        client.is_some(),
    );
    println!();

    let mut practice_ctx = practice::PracticeCtx::new(&root.join("exercises"), cfg.editor.clone());
    // Open code threads: how many turns have surfaced one (footer/
    // context repetition is capped — see agent_turn).
    let mut open_loop_seen = 0usize;

    // M4.5a: reconcile the exercise index with disk (adds entries for
    // untracked exercises, recovering template provenance) and consume
    // the legacy `.progress` file once.
    {
        // Generated exercises are wired UNGATED into exercises/lib.rs
        // (9.5: cargo check / flycheck must see them to surface
        // borrowck errors) — on a fresh clone the gitignored wiring
        // file doesn't exist yet, which would be an E0583.
        crate::generator::ensure_wiring_file(
            &root.join("exercises").join("lib_generated.rs"),
        );
        let mut index = crate::exercise::index::ExerciseIndex::load(&practice_ctx.root);
        let discovered = crate::exercise::discover_all(&practice_ctx.root);
        let templates = crate::template::load_dir(&root.join("templates")).unwrap_or_default();
        index.reconcile(&discovered, &templates);
        crate::exercise::index::migrate_progress(&mut index, &practice_ctx.root, &discovered);
    }

    loop {
        if agent::is_interrupted() {
            agent::reset_interrupt();
            println!("  ^C（任务执行中按 Ctrl-C 打断；输入 /exit 退出）");
        }
        let line = match read_line("你> ") {
            Line::Text(s) => s,
            Line::Interrupted => {
                println!("  ^C 已取消本行输入");
                continue;
            }
            Line::Eof => {
                println!();
                break;
            }
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // Muscle-memory exits from the practice page / other REPLs:
        // a bare q/quit/exit is never worth a model round-trip.
        if matches!(line, "q" | "quit" | "exit" | "退出") {
            break;
        }
        // ```-fence paste mode: lines between two ``` lines become ONE
        // message. Only a SINGLE-line ``` opens the interactive fence
        // collector — a pasted multi-line block (M4.9 paste
        // aggregation) is already one message and may itself contain
        // fences; sending it verbatim is correct.
        let line = if line.starts_with("```") && !line.contains('\n') {
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
            Cmd::Clear(deep) => {
                if deep {
                    clear_all();
                } else {
                    clear_viewport();
                }
                println!("{}", chat_header(&short_id(&session.id), &cfg.model, current_spent(&tracker), cfg.budget_usd()));
            }
            Cmd::Ui(arg) => switch_ui(&mut cfg, arg),
            Cmd::Topics => topics_page(),
            Cmd::Practice(arg) => {
                agent::reset_interrupt();
                let opts = practice::EnterOpts {
                    include_fixtures: arg == Some("all"),
                    session_paths: &session.exercises,
                };
                let deps = debrief_deps(&cfg, &tracker, &client);
                match practice::enter(&practice_ctx, opts, Some(&deps)) {
                    Some(msg) => {
                        repaint_chat(&session, &cfg, &tracker);
                        println!("{} [问教练] 把练习代码带回对话", super::render::bold("你>"));
                        agent_turn(&mut session, &msg, &cfg, &tracker, &client, &practice_ctx, &mut open_loop_seen);
                    }
                    None => repaint_chat(&session, &cfg, &tracker),
                }
            }
            Cmd::Generate(arg) => {
                agent::reset_interrupt();
                let gen_path = generate::cmd_generate(
                    &cfg,
                    &tracker,
                    &client,
                    &practice_ctx,
                    Some((session.id.as_str(), session.exercises.as_slice())),
                    arg,
                );
                if let Some(p) = gen_path {
                    register_session_exercise(&mut session, &practice_ctx, &p);
                }
                repaint_chat(&session, &cfg, &tracker);
            }
            Cmd::Usage => print_usage(&cfg, &tracker),
            Cmd::Stats(arg) => print_stats(&practice_ctx, arg),
            Cmd::Model(arg) => {
                // Result page (M4.2 convention): prints inline and must
                // NOT be wiped by a viewport repaint right after.
                handle_model(arg, &mut cfg, &mut client);
            }
            Cmd::Config => {
                cmd_config(&mut cfg, &mut client);
                practice_ctx.editor = cfg.editor.clone();
                repaint_chat(&session, &cfg, &tracker);
            }
            Cmd::Sessions(arg) => handle_sessions(arg.as_deref(), &mut session, &cfg, &tracker),
            Cmd::Reset => cmd_reset(&mut session, &cfg, &practice_ctx, &tracker),
            Cmd::Unknown(raw) => {
                let hint = render::suggest_command(&raw, KNOWN_COMMANDS)
                    .map(|s| format!("你是不是想用 {s}？"))
                    .unwrap_or_default();
                println!("  未知命令 {raw}（{hint}/help 查看命令列表）");
            }
            Cmd::Chat => {
                // View mode: fresh viewport per turn — header, dim recap
                // of the last entries, then the echoed input.
                repaint_chat(&session, &cfg, &tracker);
                println!("{} {}", super::render::bold("你>"), line);
                agent_turn(&mut session, &line, &cfg, &tracker, &client, &practice_ctx, &mut open_loop_seen);
            }
        }
    }

    if let Err(e) = session.save() {
        eprintln!("会话保存失败：{e:#}");
    }
    println!("  再见——会话已保存，随时回来继续（/sessions 可回看历史）。");
}

// ---------------------------------------------------------------------------
// Viewport rendering (M4.2)
// ---------------------------------------------------------------------------

/// Fresh review-gate/debrief dependencies (M5.2): resolved per entry so
/// a `/model` switch and budget updates take effect immediately.
fn debrief_deps<'a>(
    cfg: &'a ModelConfig,
    tracker: &'a Arc<Mutex<UsageTracker>>,
    client: &'a Option<LlmClient>,
) -> super::debrief::DebriefDeps<'a> {
    super::debrief::DebriefDeps {
        client: client.as_ref(),
        cfg,
        tracker: tracker.clone(),
        editor: cfg.editor.as_deref(),
    }
}

/// Redraw the chat page: clear viewport (view mode, tty only) →
/// header → dim recap of the last few entries. No-op in scroll mode
/// and on piped output.
fn repaint_chat(session: &Session, cfg: &ModelConfig, tracker: &Arc<Mutex<UsageTracker>>) {
    if !cfg.ui.mode_view() || !render::ansi_enabled() {
        return;
    }
    clear_viewport();
    println!("{}", chat_header(&short_id(&session.id), &cfg.model, current_spent(tracker), cfg.budget_usd()));
    for (who, text) in chat_tail(&session.messages, 3, 80) {
        println!("{}", render::dim(&format!(" {who}: {text}")));
    }
    println!();
}

fn current_spent(tracker: &Arc<Mutex<UsageTracker>>) -> f64 {
    tracker.lock().unwrap_or_else(|p| p.into_inner()).all_totals().cost_usd
}

/// `/model` — list, switch, create (`new`) or remove (`rm <名>`) named
/// model profiles (M4.6 + M9d). Switching moves the pointer, writes it
/// back and rebuilds the client so the next turn uses the new endpoint.
fn handle_model(arg: Option<&str>, cfg: &mut ModelConfig, client: &mut Option<LlmClient>) {
    let arg = arg.map(str::trim).filter(|s| !s.is_empty());
    match arg {
        Some("new") => return model_new(cfg, client),
        Some(rest) if rest == "rm" || rest.starts_with("rm ") => {
            let name = rest.strip_prefix("rm").map(str::trim).unwrap_or("");
            return model_remove(cfg, client, name);
        }
        _ => {}
    }
    if cfg.models.is_empty() {
        println!("  尚未配置模型档案：/model new 交互创建，或在 config.toml 里加");
        println!("  [[models]]（示例见 config.example.toml）；配好后 `/model <名>` 切换。");
        return;
    }
    let Some(name) = arg else {
        model_panel(cfg, client);
        return;
    };
    match cfg.apply_profile(name) {
        Ok(()) => {
            match cfg.save_to_default_file() {
                Ok(()) => println!(
                    "  已切换到「{name}」：{} @ {}（超时 {}s｜价格 输入 ${:.2}/输出 ${:.2} 每 1M tok）",
                    cfg.model,
                    host_of(&cfg.endpoint),
                    cfg.llm_timeout_secs.unwrap_or(480),
                    cfg.prices.input,
                    cfg.prices.output
                ),
                Err(e) => println!("  已切换但写回 config.toml 失败：{e:#}"),
            }
            *client = make_client(cfg);
        }
        Err(e) => {
            println!("  切换失败：{e:#}");
            println!("  输入 /model 查看可用档案。");
        }
    }
}

/// `/model` with no argument (M9f): an interactive panel — everything
/// (switch / create / remove) happens INSIDE the panel, so the hints
/// never lure the learner into typing bare keywords ("new") into the
/// chat. Switching exits back to the conversation; create/remove stay
/// in the panel for follow-up actions.
fn model_panel(cfg: &mut ModelConfig, client: &mut Option<LlmClient>) {
    loop {
        if render::ansi_enabled() {
            clear_viewport();
        }
        println!();
        println!("{}", render::header("模型档案"));
        // Column widths from actual content (display cells, CJK-aware)
        // so mixed Chinese/ASCII rows line up.
        let name_w = cfg.models.iter().map(|m| m.name.width()).max().unwrap_or(4).max(4);
        let model_w = cfg.models.iter().map(|m| m.model.width()).max().unwrap_or(5).max(5);
        println!(
            "  {}  {}  端点主机",
            render::pad_display("档案名", name_w),
            render::pad_display("模型 id", model_w)
        );
        for m in &cfg.models {
            let mark = if cfg.is_active_profile(m) { render::green("*") } else { " ".to_string() };
            println!(
                "  {mark} {}  {}  {}",
                render::pad_display(&m.name, name_w),
                render::pad_display(&m.model, model_w),
                host_of(&m.endpoint)
            );
        }
        println!();
        println!("  [数字] 切换 ｜ [n] 新建 ｜ [d <名>] 删除 ｜ [q] 返回对话");
        let Some(line) = read_line_or_leave("模型> ") else { return };
        let t = line.trim();
        if t.starts_with('/') {
            println!("  面板内不处理斜杠命令——按 q 返回对话后再使用（/exit 同）。");
            continue;
        }
        match t {
            "" | "q" | "b" | "back" => return,
            "n" => model_new(cfg, client),
            t if t == "d" || t.starts_with('d') => {
                let name = t.strip_prefix('d').map(str::trim).unwrap_or("");
                model_remove(cfg, client, name);
            }
            t => match t.parse::<usize>() {
                Ok(i) if (1..=cfg.models.len()).contains(&i) => {
                    let name = cfg.models[i - 1].name.clone();
                    if cfg.apply_profile(&name).is_ok() {
                        match cfg.save_to_default_file() {
                            Ok(()) => println!(
                                "  已切换到「{name}」：{} @ {}（超时 {}s｜价格 输入 ${:.2}/输出 ${:.2} 每 1M tok）",
                                cfg.model,
                                host_of(&cfg.endpoint),
                                cfg.llm_timeout_secs.unwrap_or(480),
                                cfg.prices.input,
                                cfg.prices.output
                            ),
                            Err(e) => println!("  已切换但写回 config.toml 失败：{e:#}"),
                        }
                        *client = make_client(cfg);
                        return; // switched → back to the conversation
                    }
                }
                Ok(_) => println!("  序号超出范围：共 {} 个档案。", cfg.models.len()),
                _ => println!("  未知输入：数字切换 / n 新建 / d <名> 删除 / q 返回"),
            },
        }
    }
}

/// `/model new` (M9d): a four-question wizard. prices/think_mode etc.
/// start at their built-in defaults and can be tuned afterwards (hand
/// edit or `/config`).
fn model_new(cfg: &mut ModelConfig, client: &mut Option<LlmClient>) {
    println!();
    println!("{}", render::header("新建模型档案"));
    println!("  逐项填写，回车确认（Ctrl-C 取消）。prices / think_mode 等先用内置默认，");
    println!("  之后可在 config.toml 或 /config 里调整。");
    let Some(name) = read_line_or_leave("  1/4 档案名（用于 /model <名> 切换，如 openai / local）: ") else {
        return;
    };
    let name = name.trim().to_string();
    if name.is_empty() {
        println!("  档案名不能为空，已取消。");
        return;
    }
    if cfg.models.iter().any(|m| m.name == name) {
        println!("  已存在同名档案「{name}」（/model 查看），换一个名字。");
        return;
    }
    let Some(endpoint) =
        read_line_or_leave("  2/4 endpoint（OpenAI 兼容地址，回车 = OpenAI 官方）: ")
    else {
        return;
    };
    let Some(model) = read_line_or_leave("  3/4 模型 id（回车 = gpt-4o-mini）: ") else { return };
    let Some(api_key) = read_line_or_leave("  4/4 api_key（本地端点如 Ollama 可填占位，如 ollama）: ")
    else {
        return;
    };
    let profile = crate::config::ModelProfile {
        name: name.clone(),
        endpoint: if endpoint.trim().is_empty() {
            crate::config::default_endpoint()
        } else {
            endpoint.trim().to_string()
        },
        api_key: api_key.trim().to_string(),
        model: if model.trim().is_empty() { crate::config::default_model() } else { model.trim().to_string() },
        stream: None,
        ..Default::default()
    };
    if let Err(e) = cfg.create_profile(profile) {
        println!("  创建失败：{e:#}");
        return;
    }
    if let Err(e) = cfg.save_to_default_file() {
        println!("  已创建但写回失败：{e:#}");
        return;
    }
    println!("  ✓ 档案「{name}」已创建。");
    match read_line_or_leave("  立即切换到该档案？[Y/n] ") {
        Some(s) if s.trim().eq_ignore_ascii_case("n") => {
            println!("  保留当前档案；随时 /model {name} 切换。");
        }
        _ => {
            if cfg.apply_profile(&name).is_ok() {
                if let Err(e) = cfg.save_to_default_file() {
                    println!("  切换成功但写回失败：{e:#}");
                }
                *client = make_client(cfg);
                println!("  当前档案：「{name}」｜ {} @ {}", cfg.model, host_of(&cfg.endpoint));
            }
        }
    }
}

/// `/model rm <名>` (M9d): show the full profile, require explicit
/// confirmation, refuse to remove the last one; removing the active
/// profile falls the pointer back to the first one.
fn model_remove(cfg: &mut ModelConfig, client: &mut Option<LlmClient>, name: &str) {
    if name.is_empty() {
        println!("  用法：/model rm <档案名>（/model 查看列表）");
        return;
    }
    let Some(p) = cfg.models.iter().find(|m| m.name == name).cloned() else {
        println!("  没有名为「{name}」的档案（/model 查看列表）。");
        return;
    };
    println!();
    println!("  即将删除档案「{}」：", render::bold(&p.name));
    println!("    endpoint : {}", p.endpoint);
    println!("    model    : {}", p.model);
    println!(
        "    api_key  : {}",
        if p.api_key.is_empty() { "（档案内无 key）".to_string() } else { cfg_mask_of(&p.api_key) }
    );
    if cfg.is_active_profile(&p) {
        println!("    （当前正在使用该档案，删除后回退到第一个档案）");
    }
    let Some(ans) = read_line_or_leave("  确认删除？此操作不可撤销 [y/N] ") else { return };
    if !ans.trim().eq_ignore_ascii_case("y") {
        println!("  已取消。");
        return;
    }
    match cfg.remove_profile(name) {
        Ok(_) => {
            cfg.materialize();
            match cfg.save_to_default_file() {
                Ok(()) => println!("  已删除「{name}」。当前档案：「{}」", cfg.active.clone().unwrap_or_default()),
                Err(e) => println!("  已删除但写回失败：{e:#}"),
            }
            *client = make_client(cfg);
        }
        Err(e) => println!("  删除失败：{e:#}"),
    }
}

/// Mask for an arbitrary key (profile display): same shape as
/// `ModelConfig::masked_key` but works without a config around.
fn cfg_mask_of(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() <= 8 {
        "****".to_string()
    } else {
        format!(
            "{}****{}",
            chars[..3].iter().collect::<String>(),
            chars[chars.len() - 4..].iter().collect::<String>()
        )
    }
}

/// `https://api.x.com/v1` → `api.x.com`（显示用，不泄露路径细节）。
fn host_of(endpoint: &str) -> String {
    let rest = endpoint
        .strip_prefix("https://")
        .or_else(|| endpoint.strip_prefix("http://"))
        .unwrap_or(endpoint);
    rest.split('/').next().unwrap_or(rest).to_string()
}

/// Append a generated exercise to the session's exercise list (M4.5a):
/// the list holds index keys in production order; the exercise index
/// carries the metadata itself.
fn register_session_exercise(session: &mut Session, ctx: &practice::PracticeCtx, path: &Path) {
    if let Some(key) = crate::exercise::index::key_for(&ctx.root, path)
        && !session.exercises.contains(&key)
    {
        session.exercises.push(key);
    }
    if let Err(e) = session.save() {
        eprintln!("  （会话保存失败：{e:#}）");
    }
}

fn short_id(id: &str) -> String {
    id.strip_prefix("session_").unwrap_or(id).to_string()
}

/// Startup page (view mode clears first): title, model line, resumed
/// note, and onboarding hints on a first run.
fn print_banner(
    session: &Session,
    cfg: &ModelConfig,
    first_run: bool,
    resumed: bool,
    resumed_msgs: usize,
    has_key: bool,
) {
    if render::ansi_enabled() && cfg.ui.mode_view() {
        clear_viewport();
    }
    println!("{}", render::cyan("  欢迎使用 Rustlings-Adaptive —— 对话式 Rust 诊断教练"));
    println!(
        "  模型：{} ｜ /help 查看全部命令",
        if has_key { cfg.model.as_str() } else { "未配置" }
    );
    if !has_key {
        // A1 onboarding: the very first thing a new user hits — point
        // straight at the wizard instead of letting them hunt for it.
        println!("  还没有配置模型。推荐现在输入 /model new，四个问题建好第一个档案；");
        println!("  或复制 config.example.toml 为 config.toml 手工填写（每个字段有说明）。");
    }
    if resumed {
        println!("  已恢复上次会话 {}（{resumed_msgs} 条消息；/new 开新会话）", session.id);
    }
    if first_run {
        println!("{}", render::dim("  试试：直接提问（如「什么是所有权」）、贴一段报错代码、"));
        println!("{}", render::dim("  或说「来一道 E0382 的题」。Enter 发送；Ctrl+J 换行；"));
        println!("{}", render::dim("  多行代码直接粘贴，自动并为一条消息。"));
    }
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

enum Cmd<'a> {
    Exit,
    Help,
    New,
    Clear(bool),
    Ui(Option<&'a str>),
    Topics,
    Practice(Option<&'a str>),
    Generate(Option<&'a str>),
    Model(Option<&'a str>),
    Usage,
    Stats(Option<&'a str>),
    Config,
    Sessions(Option<String>),
    Reset,
    Unknown(String),
    Chat,
}

/// Full command words offered for near-miss suggestions (M4.2).
const KNOWN_COMMANDS: &[&str] =
    &["new", "clear", "practice", "generate", "model", "usage", "stats", "config", "sessions", "topics", "reset", "help", "exit"];

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
        ("clear", a) => Cmd::Clear(a == Some("all")),
        ("ui", a) => Cmd::Ui(a),
        ("topics", _) => Cmd::Topics,
        ("practice", a) | ("p", a) => Cmd::Practice(a),
        ("generate", a) | ("g", a) => Cmd::Generate(a),
        ("model", a) | ("m", a) => Cmd::Model(a),
        ("usage", _) | ("u", _) => Cmd::Usage,
        ("stats", a) => Cmd::Stats(a),
        ("config", _) | ("c", _) => Cmd::Config,
        ("sessions", a) | ("s", a) => Cmd::Sessions(a.map(str::to_string)),
        ("reset", _) => Cmd::Reset,
        (raw, _) => Cmd::Unknown(raw.to_string()),
    }
}

fn print_help() {
    println!();
    println!("  对话：直接输入问题 / 贴报错或代码。Enter 发送；Ctrl+J 换行；");
    println!("        多行粘贴自动并为一条消息，也可以用两行 ``` 围住（围栏结束");
    println!("        后可继续补充问题，回车发送）。教练会锚定错误码与概念，需要");
    println!("        时本地编译你的代码取证（check_code），或生成一道可开练的");
    println!("        小练习（generate_exercise）。任务执行中可随时 Ctrl-C 打断。");
    println!("  命令：");
    println!("    /new        开启新会话（旧会话落盘可回看）");
    println!("    /clear      清屏（/clear all 连同回滚缓冲区一起清）");
    println!("    /ui         界面模式：/ui view 视口重绘（默认）｜ /ui scroll 滚动");
    println!("    /topics     查看概念图谱（出题主题的权威列表）");
    println!("    /practice   做题模式（本会话/按主题/全库分区，/practice all 含种子题；");
    println!("                题目页可 [a] 问教练、[f] 反馈难度）");
    println!("    /generate   直接生成练习（可带主题：/g E0382；离线也可用）");
    println!("    /model      模型档案：/model 列表，/model <名> 切换；new 新建、rm <名> 删除");
    println!("    /usage      用量与花费（本次会话 / 累计 / 预算余量）");
    println!("    /stats      学习画像：SM-2 到期复习、概念弱项、高频错误码、错题本");
    println!("                （/stats wrong <概念|错误码> 过滤错题本）");
    println!("    /reset      学习记录重置：会话/画像/做题状态 → 归档（练习保留）");
    println!("    /config     模型配置页（endpoint / model / api_key / 预算 / 编辑器）");
    println!("    /sessions   会话列表；/sessions <序号> 查看轨迹；clear <序号>|all 归档清理");
    println!("    /exit       退出");
    println!("  模型调用的累计花费达到预算上限时会被自动拦截。");
    println!();
}

// ---------------------------------------------------------------------------
// Agent turn (worker thread + spinner + interrupt, R4)
// ---------------------------------------------------------------------------

/// Detect unresolved code threads (M5, retro §6.3): a user message with
/// a fenced code block that FAILED to compile locally, followed by a
/// coach reply proposing changes (also fenced), and no later user code
/// paste that could have carried the verification. Injected as a
/// per-turn note so the coach asks about pending verification instead
/// of silently dropping it.
///
/// 9.5 实测校准：a compiling paste is usually a "why does this work?"
/// question — nagging about verification there was reported as
/// bottomless fatigue (the thread can never resolve), so a failed
/// check_code run is REQUIRED evidence for an open thread.
fn open_loop_note(messages: &[ChatMessage]) -> Option<String> {
    // User code messages: pasted code arrives AGGREGATED without fence
    // markers (M4.9), so fall back to a code-shape heuristic; typed
    // ``` fences still count. Coach rewrites keep their fences (the
    // history stores the raw markdown).
    let user_code = |m: &ChatMessage| {
        m.role == "user"
            && m.content.as_deref().map(|c| {
                c.contains("```")
                    || (c.lines().count() >= 3
                        && (c.contains("fn ") || c.contains("let ") || c.contains(';')))
            })
            .unwrap_or(false)
    };
    let user_code_turns: Vec<usize> =
        messages.iter().enumerate().filter(|(_, m)| user_code(m)).map(|(i, _)| i).collect();

    // Report the most recent unresolved thread (the salient one);
    // older threads stay in history anyway. The coach's fenced rewrite
    // may come several turns after the paste (思路 → 追问 → 方案), so
    // the window is "any later assistant code block, with no further
    // user code paste after it".
    let mut last: Option<(usize, String)> = None;
    for &ui in user_code_turns.iter().rev().take(2) {
        // The paste must have failed a local compile before any fix
        // proposal; everything up to the next user message counts
        // (check_code may run after some coach prose).
        let failed_compile = messages[ui + 1..]
            .iter()
            .take_while(|m| m.role != "user")
            .any(|m| {
                m.role == "tool"
                    && m.content.as_deref().map(|c| c.contains("\"compiles\":false")).unwrap_or(false)
            });
        if !failed_compile {
            continue;
        }
        let Some(aj) = (ui + 1..messages.len()).rfind(|&i| {
            messages[i].role == "assistant"
                && messages[i].content.as_deref().map(|c| c.contains("```")).unwrap_or(false)
        }) else {
            continue;
        };
        // Resolved when the user later pasted new code (likely the
        // rewritten version brought back for verification).
        if (aj + 1..messages.len()).any(|i| user_code(&messages[i])) {
            continue;
        }
        let content = messages[ui].content.as_deref().unwrap_or_default();
        let snippet: String = if content.contains("```") {
            content
                .split("```")
                .nth(1)
                .and_then(|block| {
                    block.lines().find(|l| !l.trim().is_empty() && !l.trim().starts_with("```"))
                })
                .map(|l| l.trim().chars().take(40).collect())
                .unwrap_or_default()
        } else {
            content.lines().map(str::trim).find(|l| !l.is_empty()).map(|l| l.chars().take(40).collect()).unwrap_or_default()
        };
        last = Some((ui, snippet));
        break;
    }

    let (ui, snippet) = last?;
    Some(format!(
        "[Open code threads — follow-up rule] turn {ui}: the learner pasted code（“{snippet}…”）, \
         you proposed a fix, and it was NEVER re-verified. In THIS reply you must also explicitly \
         offer to verify that fix together (e.g. 「先把之前那段改好的代码用 check_code 跑一遍验证？」), \
         unless the learner's current message is already about that verification. Do not drop the thread silently."
    ))
}

/// Compose the per-turn system-prompt note: exercise index state
/// (M4.5a) + learner profile weak/due concepts and the top wrong-book
/// entries (M6). None when there is nothing to report. Pure for
/// testability.
fn build_practice_note(
    index_note: Option<String>,
    profile: &crate::profile::Profile,
    notebook: &[crate::profile::NotebookEntry],
) -> Option<String> {
    let mut profile_bits: Vec<String> = Vec::new();
    let weak: Vec<String> = profile
        .weakest(3)
        .into_iter()
        .map(|(c, f, _)| format!("{c} ({f} fails)"))
        .collect();
    if !weak.is_empty() {
        profile_bits.push(format!(
            "[Profile] Weakest concepts (cite them when the learner asks about weaknesses): {}.",
            weak.join(", ")
        ));
    }
    let due = profile.due_concepts(chrono::Utc::now());
    if !due.is_empty() {
        profile_bits.push(format!(
            "[Profile] SM-2 reviews due now: {} — offering a variant for one of these is a good default.",
            due.join(", ")
        ));
    }
    if !notebook.is_empty() {
        let lines: Vec<String> = notebook
            .iter()
            .take(3)
            .map(|e| {
                format!(
                    "- \"{}\" ({} attempts{}, concepts: {})",
                    e.title,
                    e.attempts,
                    e.last_error
                        .as_deref()
                        .map(|c| format!(", last {c}"))
                        .unwrap_or_default(),
                    e.concepts.join(", ")
                )
            })
            .collect();
        profile_bits.push(format!("[Profile] Most-failed exercises:\n{}", lines.join("\n")));
    }
    if profile_bits.is_empty() {
        return index_note;
    }
    Some(
        format!("{}\n{}", index_note.unwrap_or_default(), profile_bits.join("\n"))
            .trim()
            .to_string(),
    )
}

fn agent_turn(
    session: &mut Session,
    input: &str,
    cfg: &ModelConfig,
    tracker: &Arc<Mutex<UsageTracker>>,
    client: &Option<LlmClient>,
    practice_ctx: &practice::PracticeCtx,
    open_loop_seen: &mut usize,
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
    // M4.5a state back-flow: append the practice-state summary to the
    // system prompt for this turn (recomputed each turn). M6: add the
    // learner profile's weak/due concepts so the coach steers towards
    // them when picking topics.
    let practice_note = {
        let practice_ctx_index = practice::PracticeCtx::load_index(practice_ctx);
        let index_note = practice_ctx_index.practice_note(&session.exercises);
        let notebook = crate::profile::notebook_from_index(&practice_ctx_index, None);
        build_practice_note(
            index_note,
            &crate::profile::ProfileStore::load_or_create().profile,
            &notebook,
        )
    };
    // M5 (retro §6.3): pending code threads the coach should follow up.
    // 9.5 实测校准：inject the thread into the model context only on
    // the FIRST detection and show the footer reminder at most twice —
    // every-turn repetition made the coach itself nag in each reply,
    // which testers reported as fatigue with no way to silence it.
    let detected_note = open_loop_note(&session.messages);
    if detected_note.is_some() {
        *open_loop_seen += 1;
    }
    let open_loop_note = if *open_loop_seen == 1 { detected_note.clone() } else { None };
    let remind_in_footer = detected_note.is_some() && *open_loop_seen <= 2;
    drop(detected_note);
    let env = AgentEnv {
        caller: Arc::new(cl.clone()),
        tracker: tracker.clone(),
        cfg: cfg.clone(),
        root: PathBuf::from("."),
        session_id: Some(session.id.clone()),
        practice_note,
        open_loop_note,
    };
    let history = session.messages.clone();
    let input = input.to_string();
    let input_had_code = input.contains("```");

    let (sp, status_slot) = Spinner::start("思考中…");
    let mut spinner = Some(sp);
    enum WorkerMsg {
        Delta(String),
        /// Boxed: the outcome is large and rare, the delta is small
        /// and hot (clippy::large_enum_variant).
        Done(Box<anyhow::Result<crate::agent::TurnOutcome>>),
    }
    let (tx, rx) = std::sync::mpsc::channel::<WorkerMsg>();
    let handle = std::thread::spawn(move || {
        let progress = move |s: &str| {
            if let Ok(mut slot) = status_slot.lock() {
                *slot = s.to_string();
            }
        };
        // C1: content deltas stream out through the channel the moment
        // they arrive; the main thread renders them live (terminal) or
        // drops them (pipes — the whole reply renders on Done).
        let mut on_delta = |d: &str| {
            let _ = tx.send(WorkerMsg::Delta(d.to_string()));
        };
        let result = agent::run_turn(&history, &input, &env, &progress, &mut on_delta);
        let _ = tx.send(WorkerMsg::Done(Box::new(result)));
    });

    // Poll for the turn result, watching the interrupt flag (R4).
    // Content deltas that arrive first make the spinner step aside and
    // render live through the streaming markdown renderer (C1).
    let mut outcome = None;
    let mut stream = super::md::StreamMd::new(render::term_width(), render::ansi_enabled());
    let mut has_streamed = false;
    let print_out = |s: &str| {
        print!("{s}");
        let _ = std::io::Write::flush(&mut std::io::stdout());
    };
    loop {
        match rx.recv_timeout(Duration::from_millis(150)) {
            Ok(WorkerMsg::Delta(d)) => {
                if !has_streamed {
                    has_streamed = true;
                    if let Some(sp) = spinner.take() {
                        sp.stop();
                    }
                }
                print_out(&stream.feed(&d));
            }
            Ok(WorkerMsg::Done(res)) => {
                outcome = Some(*res);
                break;
            }
            Err(RecvTimeoutError::Timeout) => {
                if agent::is_interrupted() {
                    super::flush_stdin();
                    let tail = stream.finish();
                    print_out(&tail);
                    println!("  已打断（本回合中止，已流出的内容未计入会话；后台调用完成后仍会计入用量）。");
                    break;
                }
            }
            // Worker died without sending (panic): surface it instead
            // of masquerading as an interruption.
            Err(RecvTimeoutError::Disconnected) => {
                outcome = Some(Err(anyhow::anyhow!("后台任务异常结束（工作线程崩溃，已恢复）")));
                break;
            }
        }
    }
    if let Some(sp) = spinner.take() {
        sp.stop();
    }
    super::flush_stdin();
    let _ = handle; // abandoned threads finish in the background

    match outcome {
        Some(Ok(turn)) => {
            session.messages = turn.history;
            // Persist BEFORE rendering: if stdout dies mid-render (SSH
            // drop), the turn's messages must not be lost (9.5 实测:
            // pty 主端关闭 → println! EIO panic → 会话丢最后一回合).
            if let Err(e) = session.save() {
                println!("  会话保存失败：{e:#}");
            }
            if has_streamed {
                // Live-rendered already; just flush the unterminated tail.
                print_out(&stream.finish());
            } else {
                println!();
            }
            if let Some(text) = &turn.reply
                && !has_streamed
            {
                // M4.3: markdown → ANSI (fenced code kept verbatim),
                // width-aware wrapping; raw source on pipes.
                print!("{}", super::md::render(text, render::term_width(), render::ansi_enabled()));
            }
            for note in &turn.tool_notes {
                println!("  · {note}");
            }
            // R6: per-turn usage footer.
            let total = tracker.lock().unwrap_or_else(|p| p.into_inner()).all_totals();
            let reasoning = if turn.reasoning_tokens > 0 {
                format!("（其中推理 {}）", turn.reasoning_tokens)
            } else {
                String::new()
            };
            print!(
                "  ─ 本回合: {} 次调用 ｜ 输入 {} tok ｜ 输出 {} tok{reasoning} ｜ ${:.6}",
                turn.calls, turn.input_tokens, turn.output_tokens, turn.cost_usd
            );
            match cfg.budget_usd() {
                Some(b) => println!("  ｜ 累计 ${:.4} / 预算 ${:.2}", total.cost_usd, b),
                None => println!("  ｜ 累计 ${:.4}", total.cost_usd),
            }
            // M5 deterministic open-thread reminder (retro §6.3): the
            // model-side note is best-effort (adherence varies by
            // model), so the CLI guarantees the thread is never lost —
            // but only for the first two turns (limitation 9.5 实测).
            // Hidden when this turn itself pasted code (likely the
            // verification paste).
            if remind_in_footer && !input_had_code {
                println!(
                    "  {} 之前贴的代码改动还没验证过——把改好的代码贴回来我帮你跑 check_code。（若这条消息与代码无关，忽略即可）",
                    render::dim("↻")
                );
            }

            // Practice offer from generate_exercise (M4.5a: card with
            // concepts/difficulty/trigger + session registration).
            if let Some(offer) = turn.practice {
                register_session_exercise(session, practice_ctx, &offer.path);
                println!();
                println!("  题目已就绪：《{}》", offer.title);
                let mut line = format!("    概念 {} ｜ 难度 {}", offer.concepts.join("、"), offer.difficulty);
                if offer.concepts.is_empty() {
                    line = format!("    难度 {}", offer.difficulty);
                }
                println!("{line}");
                if let Some(t) = &offer.trigger {
                    println!("    触发：{t}");
                }
                match read_line_or_leave("  回车开始做题，输入 n 留在对话> ") {
                    None => {
                        // EOF（程序重启/管道结束）：offer 只出现一次，
                        // 必须指路，否则用户不知道题在哪（试用反馈）。
                        println!("  （题目已进「本会话」列表：/practice 随时可继续）");
                    }
                    Some(ans) => {
                        let a = ans.trim().to_ascii_lowercase();
                        if a == "n" || a == "no" {
                            println!("  （题目已进「本会话」列表：/practice 随时可继续）");
                        }
                        if a.is_empty() || a == "y" || a == "yes" || a == "是" {
                            let opts = practice::EnterOpts {
                                include_fixtures: false,
                                session_paths: &session.exercises,
                            };
                            let deps = debrief_deps(cfg, tracker, client);
                            if let Some(msg) = practice::enter_at(practice_ctx, &offer.path, opts, Some(&deps)) {
                                repaint_chat(session, cfg, tracker);
                                println!("{} [问教练] 把练习代码带回对话", super::render::bold("你>"));
                                agent_turn(session, &msg, cfg, tracker, client, practice_ctx, open_loop_seen);
                                return;
                            }
                            repaint_chat(session, cfg, tracker);
                        }
                    }
                }
            }
        }
        Some(Err(e)) => {
            println!("  出错：{e:#}");
            repaint_chat(session, cfg, tracker);
        }
        None => repaint_chat(session, cfg, tracker), // interrupted: history untouched
    }
    println!();
}

// ---------------------------------------------------------------------------
// Pages: usage / config / sessions
// ---------------------------------------------------------------------------

/// `/usage` — cost report (R6). Prints inline, never clears the screen.
fn print_usage(cfg: &ModelConfig, tracker: &Arc<Mutex<UsageTracker>>) {
    let t = tracker.lock().unwrap_or_else(|p| p.into_inner());
    let s = t.session_totals();
    let a = t.all_totals();
    println!();
    println!("{}", render::header("用量与花费"));
    let reasoning_note = |r: u64| {
        if r > 0 { format!("（其中推理 {r}）") } else { String::new() }
    };
    println!(
        "  本次会话: {} 次调用 ｜ 输入 {} tok ｜ 输出 {} tok{} ｜ ${:.4}",
        s.calls, s.input_tokens, s.output_tokens, reasoning_note(s.reasoning_tokens), s.cost_usd
    );
    for (phase, pt) in t.session_by_phase() {
        println!(
            "    · {phase:<10} {} 次 ｜ 输入 {} tok ｜ 输出 {} tok{} ｜ ${:.6}",
            pt.calls, pt.input_tokens, pt.output_tokens, reasoning_note(pt.reasoning_tokens), pt.cost_usd
        );
    }
    println!(
        "  历史累计: {} 次调用 ｜ 输入 {} tok ｜ 输出 {} tok{} ｜ ${:.4}",
        a.calls, a.input_tokens, a.output_tokens, reasoning_note(a.reasoning_tokens), a.cost_usd
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

/// `/stats` — learner profile page (M6): SM-2 due overview, weakest
/// concepts, top error codes and the wrong-answer notebook (filterable
/// via `/stats wrong <概念|错误码>`). Result-page convention: inline
/// print, never clears the screen.
fn print_stats(practice_ctx: &practice::PracticeCtx, arg: Option<&str>) {
    println!();
    println!("{}", render::header("学习画像"));
    // 9.6 实测：`/stats 显示到期…`（疑问句）silently ran the bare
    // page, swallowing the question. Reject unknown subcommands
    // instead — and point at the "drop the slash to ASK the coach" path.
    if let Some(a) = arg
        && !a.is_empty()
        && a.strip_prefix("wrong").is_none()
    {
        println!("  未知子命令「{a}」。用法：/stats ｜ /stats wrong <概念|错误码>");
        println!("  如果你想问的是「{}」，请去掉行首的 / 直接发给教练。", a);
        return;
    }
    let store = crate::profile::ProfileStore::load_or_create();
    let profile = &store.profile;

    if profile.concepts.is_empty() && profile.error_codes.is_empty() {
        println!("  还没有学习信号：做题（/practice）或对话出题后，这里会出现");
        println!("  概念掌握度（SM-2）、高频错误码与错题本。");
        return;
    }

    // Wrongbook-only view.
    if let Some(rest) = arg.and_then(|a| a.strip_prefix("wrong").map(str::trim)) {
        let index = crate::exercise::index::ExerciseIndex::load(&practice_ctx.root);
        let nb = crate::profile::notebook_from_index(&index, Some(rest));
        println!("  复习队列（{} 题，过滤「{rest}」）：", nb.len());
        if nb.is_empty() {
            println!("    （没有匹配的错题）");
        }
        for e in &nb {
            print_notebook_row(e);
        }
        return;
    }

    let graph =
        crate::taxonomy::ConceptGraph::load(&practice_ctx.repo_root.join("taxonomy").join("concepts.toml")).ok();
    let cname = |id: &str| -> String {
        match graph.as_ref().and_then(|g| g.get(id)) {
            Some(n) => format!("{}（{id}）", n.name),
            None => id.to_string(),
        }
    };

    // SM-2 due overview.
    let due = profile.due_concepts(chrono::Utc::now());
    if due.is_empty() {
        println!("  到期复习：暂无（SM-2 会为已学概念安排变式巩固节奏）");
    } else {
        println!("  {} 到期复习：", render::bold(&due.len().to_string()));
        for c in &due {
            println!("    · {}", cname(c));
        }
        println!("    （对话中说「来一道 XX 的变式题」即可巩固）");
    }

    // Weakest concepts.
    println!("  概念弱项（按失败次数）：");
    let weak = profile.weakest(5);
    if weak.is_empty() {
        println!("    （还没有失败记录，状态不错）");
    } else {
        println!("{}", render::dim("    （强度 = SM-2 的 EF 值，1.3–2.5，越高记得越牢；复习 = 下次到期日）"));
    }
    // Align the concept column so the metric columns line up.
    let names: Vec<String> = weak.iter().map(|(c, _, _)| cname(c)).collect();
    let name_w = names.iter().map(|n| n.width()).max().unwrap_or(0);
    for ((c, fails, attempts), name) in weak.iter().zip(&names) {
        let s = profile.concepts.get(c);
        let ef = s.map(|s| format!("强度 {:.1}", s.sm2.ef)).unwrap_or_default();
        let due_str =
            s.and_then(|s| s.sm2.due.as_deref()).map(due_cn).unwrap_or_else(|| "—".into());
        println!(
            "    {} ｜ 失败 {fails}/{attempts} ｜ {ef} ｜ 复习 {due_str}",
            render::pad_display(name, name_w)
        );
    }

    // Top error codes (coarse track).
    let codes = profile.top_error_codes(5);
    if !codes.is_empty() {
        let list: Vec<String> = codes.iter().map(|(c, n)| format!("{c} ×{n}")).collect();
        println!("  高频错误码：{}", list.join(" · "));
    }

    // Wrong-answer notebook.
    let index = crate::exercise::index::ExerciseIndex::load(&practice_ctx.root);
    let nb = crate::profile::notebook_from_index(&index, None);
    println!(
        "  复习队列（{} 题，含已通过 ✓——过了的也留着巩固；/stats wrong <概念|错误码> 过滤）：",
        nb.len()
    );
    for e in nb.iter().take(8) {
        print_notebook_row(e);
    }
    if nb.len() > 8 {
        println!("    …共 {} 题（用 /stats wrong 过滤）", nb.len());
    }
}

fn due_cn(due: &str) -> String {
    match chrono::DateTime::parse_from_rfc3339(due) {
        Ok(d) => {
            let days = (d.with_timezone(&chrono::Utc) - chrono::Utc::now()).num_days();
            match days {
                ..=0 => render::red("已到期").to_string(),
                1 => "明天".to_string(),
                n => format!("{n} 天后"),
            }
        }
        Err(_) => "—".to_string(),
    }
}

fn print_notebook_row(e: &crate::profile::NotebookEntry) {
    let mark = if e.passed {
        render::green("✓").to_string()
    } else {
        render::red(&format!("✗{}", e.attempts))
    };
    let mut meta = e.concepts.join("、");
    if let Some(code) = &e.last_error {
        meta.push_str(&format!(" ｜ {code}"));
    }
    println!("    {} 《{}》  {}", mark, e.title, meta);
}

/// `/config` — interactive model config page (R3). Page-framed: the
/// viewport clears on every redraw so edits never scroll away. Edits
/// are written back to config.toml; the client is rebuilt so changes
/// apply at once.
fn cmd_config(cfg: &mut ModelConfig, client: &mut Option<LlmClient>) {
    loop {
        if render::ansi_enabled() {
            clear_viewport();
        }
        println!("{}", render::header("模型配置"));
        let active_name = cfg.active.clone().unwrap_or_default();
        println!(
            "  输入编号修改对应项（1..6），回车返回；修改写回当前档案「{}」",
            render::cyan(&active_name)
        );
        println!();
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
            "  · 上下文    : {} tokens ｜ 思考模式: {}（/config 6 可改）",
            cfg.context_len,
            cfg.think_mode.label_cn()
        );
        println!(
            "  · 界面      : {}（/ui view｜scroll 可切换）",
            if cfg.ui.mode_view() { "视口重绘" } else { "滚动" }
        );
        if cfg.models.len() > 1 {
            let names: Vec<String> = cfg
                .models
                .iter()
                .map(|m| {
                    if cfg.is_active_profile(m) {
                        format!("{}{}", m.name, render::green("*"))
                    } else {
                        m.name.clone()
                    }
                })
                .collect();
            println!("  · 模型档案  : {}（/model <名> 切换）", names.join(" / "));
        }
        let Some(line) = read_line_or_leave("配置> ") else { return };
        match line.as_str() {
            "" => return,
            "1" => {
                if let Some(v) = read_line_or_leave("新 endpoint: ") {
                    cfg.endpoint = v;
                    save_and_rebuild(cfg, client);
                }
            }
            "2" => {
                if let Some(v) = read_line_or_leave("新 model: ") {
                    cfg.model = v;
                    save_and_rebuild(cfg, client);
                }
            }
            "3" => {
                if let Some(v) = read_line_or_leave("新 api_key（输入明文，回车确认）: ") {
                    cfg.api_key = v;
                    cfg.key_source = crate::config::KeySource::ConfigFile;
                    save_and_rebuild(cfg, client);
                }
            }
            "4" => {
                if let Some(v) = read_line_or_leave("新预算（数字=USD；'无' 取消预算）: ") {
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
                if let Some(v) = read_line_or_leave("新编辑器命令（如 'code --wait'；'无' 恢复自动）: ") {
                    if v == "无" || v == "none" {
                        cfg.editor = None;
                    } else {
                        cfg.editor = Some(v);
                    }
                    save_and_rebuild(cfg, client);
                }
            }
            "6" => {
                println!("  说明：推理型模型（如 DeepSeek V4）默认开思考且思维链按输出 token 计费、");
                println!("  不受 max_tokens 约束；关掉可立刻省下大量 token 与等待时间。");
                if let Some(v) = read_line_or_leave("新思考模式（auto=沿用端点默认 / on / off）: ") {
                    match crate::config::ThinkMode::from_word(&v) {
                        Some(m) => {
                            cfg.think_mode = m;
                            save_and_rebuild(cfg, client);
                        }
                        None => println!("  无法识别: {v}（auto / on / off）"),
                    }
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

/// `/sessions` — list, view one trajectory, switch into a past
/// session, or export it as markdown (R5).
fn handle_sessions(arg: Option<&str>, session: &mut Session, cfg: &ModelConfig, tracker: &Arc<Mutex<UsageTracker>>) {
    let infos = Session::list();
    if infos.is_empty() {
        println!("  还没有会话记录（对话后自动保存在 ~/.rustlings_adaptive/sessions/）。");
        return;
    }
    let Some(arg) = arg else {
        sessions_list(session, cfg, tracker);
        return;
    };
    let mut words = arg.split_whitespace();
    match words.next().unwrap_or("") {
        "clear" => {
            // M9d: session cleanup with ARCHIVE semantics (nothing is
            // truly deleted — files move to ~/.rustlings_adaptive/archive).
            let word = words.next().unwrap_or("");
            sessions_clear(session, cfg, tracker, word, infos.len());
        }
        "load" => {
            match parse_index(words.next(), infos.len()) {
                Some(n) => match Session::load(&infos[n - 1].path) {
                    Ok(loaded) => {
                        let _ = session.save();
                        *session = loaded;
                        println!(
                            "  已切换到会话 {}（{} 条消息；继续对话即可延续该上下文）",
                            render::cyan(&session.id),
                            session.messages.len()
                        );
                        repaint_chat(session, cfg, tracker);
                    }
                    Err(e) => println!("  读取会话失败：{e:#}"),
                },
                None => println!("  用法：/sessions load <序号>（1..={}）", infos.len()),
            }
        }
        "export" => {
            match parse_index(words.next(), infos.len()) {
                Some(n) => match Session::load(&infos[n - 1].path) {
                    Ok(s) => match s.export_markdown() {
                        Ok(path) => println!("  已导出：{}", path.display()),
                        Err(e) => println!("  导出失败：{e:#}"),
                    },
                    Err(e) => println!("  读取会话失败：{e:#}"),
                },
                None => println!("  用法：/sessions export <序号>（1..={}）", infos.len()),
            }
        }
        _ => {
            // Bare number = view the trajectory read-only, then stay
            // inside the sessions UI (9.5 实测：回看后直接回主对话，
            // 用户随手键入的 export/load/b 被当消息发给模型).
            match arg.parse::<usize>() {
                Ok(n) if n >= 1 && n <= infos.len() => match Session::load(&infos[n - 1].path) {
                    Ok(s) => {
                        println!();
                        println!(
                            "── 会话 {}（{} 条消息，开始于 {}；/sessions load {} 可切换）──",
                            render::cyan(&s.id),
                            s.messages.len(),
                            s.started_at.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M:%S"),
                            n
                        );
                        print!("{}", agent::session::trajectory_text(&s.messages, 500));
                        println!();
                        if read_line_or_leave("  回车返回会话列表 > ").is_some() {
                            sessions_list(session, cfg, tracker);
                        }
                    }
                    Err(e) => println!("  读取会话失败：{e:#}"),
                },
                _ => println!("  用法：/sessions ｜ /sessions <序号> ｜ /sessions load <序号> ｜ /sessions export <序号>"),
            }
        }
    }
}

/// Shared session-clear flow (M9f): archive one session or all of
/// them, with confirmation. `word` = "" | "<序号>" | "all".
fn sessions_clear(
    session: &mut Session,
    cfg: &ModelConfig,
    tracker: &Arc<Mutex<UsageTracker>>,
    word: &str,
    total: usize,
) {
    if total == 0 {
        println!("  没有会话可清理。");
        return;
    }
    match word {
        "all" => {
            println!(
                "  将把全部 {total} 个会话移入归档（~/.rustlings_adaptive/archive/，可找回），"
            );
            let Some(ans) = read_line_or_leave("  然后开启一个新会话。确认？[y/N] ") else { return };
            if !ans.trim().eq_ignore_ascii_case("y") {
                println!("  已取消。");
                return;
            }
            match archive_dir_for() {
                Ok(dir) => match move_session_files(&dir.join("sessions"), &|_| true) {
                    Ok(n) => {
                        *session = Session::new(&cfg.model);
                        println!(
                            "  已归档 {n} 个会话 → {}；当前为新会话。",
                            dir.join("sessions").display()
                        );
                        repaint_chat(session, cfg, tracker);
                    }
                    Err(e) => println!("  归档失败：{e:#}"),
                },
                Err(e) => println!("  归档失败：{e:#}"),
            }
        }
        n => match n.parse::<usize>() {
            Ok(i) if (1..=total).contains(&i) => {
                let infos = Session::list();
                let Some(target) = infos.get(i - 1) else {
                    println!("  序号超出范围。");
                    return;
                };
                if target.id == session.id {
                    println!("  当前会话不能直接清除——先 /new 开新会话再来。");
                    return;
                }
                println!(
                    "  将归档会话 {}（{} 条消息，可在 archive/ 找回）。",
                    target.id, target.messages
                );
                let Some(ans) = read_line_or_leave("  确认？[y/N] ") else { return };
                if !ans.trim().eq_ignore_ascii_case("y") {
                    println!("  已取消。");
                    return;
                }
                match archive_dir_for() {
                    Ok(dir) => {
                        match move_session_files(&dir.join("sessions"), &|id| id == target.id) {
                            Ok(1) => println!("  已归档会话 {}。", target.id),
                            Ok(n) => println!("  已归档 {n} 个文件。"),
                            Err(e) => println!("  归档失败：{e:#}"),
                        }
                    }
                    Err(e) => println!("  归档失败：{e:#}"),
                }
            }
            _ => println!("  用法：clear <序号> ｜ clear all"),
        },
    }
}

fn parse_index(word: Option<&str>, len: usize) -> Option<usize> {
    let n = word?.parse::<usize>().ok()?;
    (n >= 1 && n <= len).then_some(n)
}

// ---------------------------------------------------------------------------
// Data archiving (M9d): "clear" never deletes — files move into
// ~/.rustlings_adaptive/archive/<timestamp>/ and can be recovered by hand.
// ---------------------------------------------------------------------------

/// Timestamped archive directory under the data root.
fn archive_dir_for() -> anyhow::Result<std::path::PathBuf> {
    let sessions = agent::session::sessions_dir();
    let base = sessions.parent().context("无法定位数据目录")?;
    let stamp = chrono::Local::now().format("%Y%m%d_%H%M%S");
    Ok(base.join("archive").join(stamp.to_string()))
}

/// Move every file in `sessions_dir` whose session id matches `keep`
/// into `dest_dir`. Returns the number of files moved (json + md).
fn move_session_files(dest_dir: &Path, keep: &dyn Fn(&str) -> bool) -> anyhow::Result<usize> {
    use std::fs;
    let sessions = agent::session::sessions_dir();
    let mut moved = 0;
    if !sessions.exists() {
        return Ok(0);
    }
    std::fs::create_dir_all(dest_dir)?;
    for entry in fs::read_dir(&sessions)?.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
        let Some(id) = name.strip_suffix(".json") else { continue };
        if !keep(id) {
            continue;
        }
        let dest = dest_dir.join(name);
        fs::rename(&path, &dest).with_context(|| format!("移动 {}", path.display()))?;
        moved += 1;
        // The exported markdown, if any, travels with it.
        let md = path.with_extension("md");
        if md.exists()
            && let Ok(()) = fs::rename(&md, dest_dir.join(md.file_name().unwrap_or_default()))
        {
            moved += 1;
        }
    }
    Ok(moved)
}

/// `/reset` (M9d): a full, guarded reset of LEARNER data — session
/// history, the profile (SM-2/notebook/error codes) and the exercise
/// solve state — by archiving all of it. Exercise FILES survive (only
/// their status zeroes out); usage.json (the cost ledger) survives.
fn cmd_reset(
    session: &mut Session,
    cfg: &ModelConfig,
    practice_ctx: &practice::PracticeCtx,
    tracker: &Arc<Mutex<UsageTracker>>,
) {
    println!();
    println!("{}", render::header("重置学习记录"));
    let sessions_dir = agent::session::sessions_dir();
    let Some(base) = sessions_dir.parent() else { return };
    let n_sessions = Session::list().len();
    let index = crate::exercise::index::ExerciseIndex::load(&practice_ctx.root);
    let n_exercises = index.iter().filter(|m| !m.path.starts_with("fixtures")).count();
    println!("  将把以下数据移入归档目录（不删除，可手动找回；练习文件本身保留）：");
    println!("    · 会话历史 ×{n_sessions}");
    println!("    · 学习画像 profile.json / miss_log.json（SM-2、错题本、错误码统计）");
    println!("    · 做题状态 index.json（尝试次数 / ✗ 次数 / 反馈，{n_exercises} 题）");
    println!("  用量账本 usage.json 保留，不受影响。");
    let Some(ans) = read_line_or_leave("  确认重置？[y/N] ") else { return };
    if !ans.trim().eq_ignore_ascii_case("y") {
        println!("  已取消。");
        return;
    }
    let Ok(archive_dir) = archive_dir_for() else {
        println!("  无法定位数据目录。");
        return;
    };
    let mut moved = 0usize;
    match move_session_files(&archive_dir.join("sessions"), &|_| true) {
        Ok(n) => moved += n,
        Err(e) => println!("  （会话归档失败：{e:#}）"),
    }
    // Existence must be checked BEFORE the rename (afterwards the file
    // is gone and the count silently misses it — M9d 冒烟抓到).
    for file in ["profile.json", "miss_log.json"] {
        let p = base.join(file);
        if !p.exists() {
            continue;
        }
        match std::fs::rename(&p, archive_dir.join(file)) {
            Ok(()) => moved += 1,
            Err(e) => println!("  （{file} 归档失败：{e:#}）"),
        }
    }
    let index_path = crate::exercise::index::index_path(&practice_ctx.root);
    if index_path.exists() {
        match std::fs::rename(&index_path, archive_dir.join("index.json")) {
            Ok(()) => moved += 1,
            Err(e) => println!("  （index.json 归档失败：{e:#}）"),
        }
    }
    *session = Session::new(&cfg.model);
    println!("  已归档 {moved} 项 → {}。", archive_dir.display());
    println!("  学习记录从零开始：/stats、/sessions 均为空；生成的练习仍在，可重做。");
    repaint_chat(session, cfg, tracker);
}

/// `/sessions` with no argument: paged interactive list. Global
/// numbering (1..=N) stays stable so `/sessions <n>` / `load` / `export`
/// keep working from anywhere; the view defaults to the LAST page (the
/// newest sessions are what the user usually wants).
fn sessions_list(
    session: &mut Session,
    cfg: &ModelConfig,
    tracker: &Arc<Mutex<UsageTracker>>,
) {
    let infos = Session::list();
    if infos.is_empty() {
        println!("  还没有会话记录（对话后自动保存在 ~/.rustlings_adaptive/sessions/）。");
        return;
    }
    const PAGE: usize = 10;
    let total_pages = infos.len().div_ceil(PAGE);
    let mut page = total_pages.saturating_sub(1);
    loop {
        if render::ansi_enabled() {
            clear_viewport();
        }
        println!();
        println!(
            "{} 第 {}/{} 页 ｜ 共 {} 条（全局序号可直接用于 /sessions <n> / load / export）",
            render::header("会话列表"),
            page + 1,
            total_pages,
            infos.len()
        );
        let start = page * PAGE;
        for (i, info) in infos
            .iter()
            .enumerate()
            .skip(start)
            .take(PAGE)
        {
            let title = info.title.clone().unwrap_or_else(|| "—".to_string());
            println!(
                "  {:>2}. {} ｜ {} 条消息 ｜ 「{}」 ｜ 开始 {}",
                i + 1,
                info.id,
                info.messages,
                title,
                info.started_at.with_timezone(&chrono::Local).format("%m-%d %H:%M")
            );
        }
        println!();
        println!(
            "  [n] 更晚 ｜ [p] 更早 ｜ [数字] 回看 ｜ [load n] 切换 ｜ [export n] 导出"
        );
        println!("  [c <n>|all] 归档清理 ｜ [q] 返回");
        match read_line("会话> ") {
            Line::Text(s) => {
                let t = s.trim();
                match t {
                    "n" | "N" => {
                        page = (page + 1).min(total_pages - 1);
                    }
                    "p" | "P" => {
                        page = page.saturating_sub(1);
                    }
                    "q" | "b" | "back" | "" => break,
                    _ => {
                        // Inline handlers reuse the same semantics as
                        // `/sessions load|export|clear|<n>`; view then redraw.
                        let words: Vec<&str> = t.split_whitespace().collect();
                        match words.first().copied().unwrap_or("") {
                            "c" | "clear" => {
                                sessions_clear(
                                    session,
                                    cfg,
                                    tracker,
                                    words.get(1).copied().unwrap_or(""),
                                    infos.len(),
                                );
                            }
                            "load" => {
                                if let Some(n) = parse_index(words.get(1).copied(), infos.len()) {
                                    match Session::load(&infos[n - 1].path) {
                                        Ok(loaded) => {
                                            let _ = session.save();
                                            *session = loaded;
                                            println!(
                                                "  已切换到会话 {}（{} 条消息；继续对话即可延续该上下文）",
                                                render::cyan(&session.id),
                                                session.messages.len()
                                            );
                                            repaint_chat(session, cfg, tracker);
                                            return;
                                        }
                                        Err(e) => println!("  读取会话失败：{e:#}"),
                                    }
                                } else {
                                    println!("  用法：load <序号>（1..={}）", infos.len());
                                }
                            }
                            "export" => {
                                if let Some(n) = parse_index(words.get(1).copied(), infos.len())
                                    && let Ok(s) = Session::load(&infos[n - 1].path)
                                {
                                    match s.export_markdown() {
                                        Ok(path) => println!("  已导出：{}", path.display()),
                                        Err(e) => println!("  导出失败：{e:#}"),
                                    }
                                } else {
                                    println!("  用法：export <序号>（1..={}）", infos.len());
                                }
                            }
                                _ => match t.parse::<usize>() {
                                    Ok(n) if n >= 1 && n <= infos.len() => {
                                        match Session::load(&infos[n - 1].path) {
                                            Ok(s) => {
                                                println!();
                                                println!(
                                                    "── 会话 {}（{} 条消息，开始于 {}；/sessions load {n} 可切换）──",
                                                    render::cyan(&s.id),
                                                    s.messages.len(),
                                                    s.started_at
                                                        .with_timezone(&chrono::Local)
                                                        .format("%Y-%m-%d %H:%M:%S"),
                                                );
                                                print!(
                                                    "{}",
                                                    agent::session::trajectory_text(&s.messages, 500)
                                                );
                                                println!();
                                                // Hold the trajectory on screen until
                                                // the learner acknowledges — the loop
                                                // redraw would wipe it immediately.
                                                if read_line_or_leave("  回车返回列表 > ").is_none() {
                                                    break;
                                                }
                                            }
                                            Err(e) => println!("  读取会话失败：{e:#}"),
                                        }
                                    }
                                    _ => println!("  未知输入：数字回看 / load n / export n / n / p / q"),
                                },
                        }
                    }
                }
            }
            Line::Interrupted => {
                println!("  ^C 已取消本行输入");
            }
            Line::Eof => break,
        }
    }
}


/// `/ui` — switch rendering mode (M4.2): view = repaint viewport per
/// turn, scroll = plain transcript. Persisted to config.toml. With no
/// argument this is an interactive mini-panel (M9f), so the hints never
/// make the learner type bare "view"/"scroll" into the chat.
fn switch_ui(cfg: &mut ModelConfig, arg: Option<&str>) {
    let mode = arg.unwrap_or("").trim();
    match mode {
        "view" | "scroll" => apply_ui_mode(cfg, mode),
        "" => {
            loop {
                println!();
                println!(
                    "{}",
                    render::header(&format!(
                        "界面模式（当前：{}）",
                        if cfg.ui.mode_view() { "视口重绘" } else { "滚动" }
                    ))
                );
                println!("  [1] 视口重绘（默认：每回合刷新视口，回滚缓冲区保留）");
                println!("  [2] 滚动（纯聊天流，不做视口重绘）");
                println!("  [q] 返回对话");
                let Some(line) = read_line_or_leave("界面> ") else { return };
                if line.trim().starts_with('/') {
                    println!("  面板内不处理斜杠命令——按 q 返回对话后再使用（/exit 同）。");
                    continue;
                }
                match line.trim() {
                    "" | "q" | "b" => return,
                    "1" => {
                        apply_ui_mode(cfg, "view");
                        return;
                    }
                    "2" => {
                        apply_ui_mode(cfg, "scroll");
                        return;
                    }
                    _other => println!("  未知输入：1 / 2 / q"),
                }
            }
        }
        other => println!("  未知模式「{other}」：/ui 进入面板选择，或 /ui view｜/ui scroll"),
    }
}

fn apply_ui_mode(cfg: &mut ModelConfig, mode: &str) {
    cfg.ui.mode = mode.to_string();
    match cfg.save_to_default_file() {
        Ok(()) => println!(
            "  界面模式已切换为 {}（已写回 config.toml）",
            if mode == "view" { "视口重绘" } else { "滚动" }
        ),
        Err(e) => println!("  已切换但写回配置失败：{e:#}"),
    }
}

/// `/topics` — the taxonomy listing, exposed for humans (the agent has
/// the `list_concepts` tool; this is the same data).
fn topics_page() {
    match crate::taxonomy::ConceptGraph::load(std::path::Path::new("taxonomy/concepts.toml")) {
        Ok(g) => {
            println!();
            println!("{}", render::header("概念图谱（出题主题的权威列表）"));
            for id in g.ids() {
                let name = g.get(id).map(|n| n.name.as_str()).unwrap_or("");
                for piece in render::wrap_line(&format!("  {id} ｜ {name}"), render::term_width()) {
                    println!("{piece}");
                }
            }
            println!();
        }
        Err(e) => println!("  概念图谱加载失败：{e:#}"),
    }
}

/// First run = no saved sessions and nothing resumed: show onboarding.
fn first_run(no_saved: bool, resumed: bool) -> bool {
    no_saved && !resumed
}

fn install_ctrlc() {
    if let Err(e) = ctrlc::set_handler(crate::agent::set_interrupt) {
        eprintln!("（提示：Ctrl-C 打断不可用：{e}）");
    }
}

/// Multi-line paste mode: collect lines until the closing ``` line.
/// After the fence closes, keep reading until an empty line: the
/// learner usually wants to add a question with the code (9.5 实测：
/// 围栏结束即提交，把追问拆成两条消息，多烧一次调用). Ctrl-C/EOF
/// cancels the whole message.
fn read_paste() -> Option<String> {
    println!("  ─ 粘贴模式：继续粘贴代码，单独一行 ``` 结束 ─");
    let mut buf = String::new();
    loop {
        let l = read_line_or_leave("")?;
        if l.trim() == "```" {
            println!("  （代码已收下；可继续补充问题。输完后再按一次回车——空行即发送）");
            loop {
                let l = read_line_or_leave("")?;
                if l.trim().is_empty() {
                    return Some(buf);
                }
                buf.push_str(&l);
                buf.push('\n');
            }
        }
        buf.push_str(&l);
        buf.push('\n');
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::ChatMessage;

    fn user(text: &str) -> ChatMessage {
        ChatMessage::user(text.to_string())
    }
    fn assistant(text: &str) -> ChatMessage {
        ChatMessage::assistant(text.to_string())
    }
    /// check_code round that FAILED (the evidence an open fix-verify
    /// thread requires).
    fn failed_check(code: &str) -> Vec<ChatMessage> {
        vec![
            ChatMessage::assistant_with_calls(vec![crate::llm::ToolCall {
                id: "check_code_0".into(),
                name: "check_code".into(),
                arguments: format!("{{\"code\":\"{code}\"}}"),
            }]),
            ChatMessage::tool_result(
                "check_code_0",
                "{\"compiles\":false,\"error_codes\":[\"E0382\"]}",
            ),
        ]
    }

    #[test]
    fn open_loop_detects_unverified_rewrite() {
        // Pasted code arrives WITHOUT fence markers (M4.9 aggregation).
        let mut msgs = vec![
            user("什么是所有权"),
            assistant("解释…"),
            user("fn main() {\n    let s = String::from(\"a\");\n    let t = s;\n    println!(\"{}\", s);\n}"),
        ];
        msgs.extend(failed_check("fn main() {}"));
        msgs.extend([
            assistant("本地编译报 E0382…"),
            assistant("方案：\n```rust\nlet t = s.clone();\n```"),
            user("顺便问，String 和 &str 有什么区别"),
        ]);
        let note = open_loop_note(&msgs).expect("should detect the open thread");
        assert!(note.contains("[Open code threads"), "{note}");
        assert!(note.contains("NEVER re-verified"), "{note}");
        assert!(note.contains("fn main()"), "snippet preview present: {note}");
    }

    #[test]
    fn open_loop_detects_rewrite_after_followup_questions() {
        // 贴码 → 教练思路（无码）→ 用户追问（无码）→ 教练方案（有码）→ 用户转话题
        let mut msgs = vec![
            user("fn main() {\n    let s = String::from(\"a\");\n    let t = s;\n    println!(\"{}\", s);\n}"),
        ];
        msgs.extend(failed_check("fn main() {}"));
        msgs.extend([
            assistant("本地编译报 E0382；修复思路有三种……"),
            user("直接给我最小改动"),
            assistant("最小改动：\n```rust\nlet t = s.clone();\n```"),
            user("顺便问，String 和 &str 有什么区别"),
        ]);
        let note = open_loop_note(&msgs).expect("rewrite two turns later still counts");
        assert!(note.contains("NEVER re-verified"), "{note}");
    }

    #[test]
    fn open_loop_none_when_paste_compiled_clean() {
        // 9.5 实测校准：a COMPILING paste is a "why does this work?"
        // question — no verification nagging. (Regression for the
        // bottomless ↻ reminder fatigue.)
        let mut msgs = vec![
            user("fn main() {\n    let s = String::from(\"a\");\n    let t = &s;\n    println!(\"{}\", s);\n}"),
            ChatMessage::assistant_with_calls(vec![crate::llm::ToolCall {
                id: "check_code_0".into(),
                name: "check_code".into(),
                arguments: "{\"code\":\"fn main() {}\"}".into(),
            }]),
            ChatMessage::tool_result("check_code_0", "{\"compiles\":true,\"error_codes\":[]}"),
            assistant("这段能编译通过 ✅\n```rust\nlet t = &s;\n```"),
            user("顺便问，String 和 &str 有什么区别"),
        ];
        let _ = &mut msgs;
        assert!(open_loop_note(&msgs).is_none());
    }

    #[test]
    fn open_loop_resolved_when_user_pastes_again() {
        let msgs = vec![
            user("```\nfn f() {}\n```怎么改"),
            assistant("方案：\n```rust\nfn f() {}\n```"),
            user("```\nfn f() { let x = 1; }\n```这样对吗"),
        ];
        assert!(open_loop_note(&msgs).is_none());
    }

    #[test]
    fn open_loop_none_without_code_exchange() {
        let msgs = vec![user("你好"), assistant("你好！")];
        assert!(open_loop_note(&msgs).is_none());
    }

    #[test]
    fn practice_note_combines_index_and_profile() {
        use crate::exercise::index::{ExerciseIndex, ExerciseMeta, Source, Status};
        use crate::profile::{Profile, ProfileStore};
        let empty = Profile::default();
        // No index note, no profile signals → None.
        assert!(build_practice_note(None, &empty, &[]).is_none());
        // Index note passes through untouched.
        assert_eq!(
            build_practice_note(Some("[Practice status] x".into()), &empty, &[]).as_deref(),
            Some("[Practice status] x")
        );
        // Profile signals appended (weak + due + notebook).
        let mut p = Profile::default();
        p.record_attempt(&["c.weak".into()], Some("E0382"), false);
        p.record_debrief(&["c.due".into()], false, Some(true), 4);
        p.record_debrief(&["c.due".into()], false, Some(true), 4);
        p.concepts.get_mut("c.due").unwrap().sm2.due =
            Some((chrono::Utc::now() - chrono::Duration::days(1)).to_rfc3339());
        let mut idx = ExerciseIndex::load_from(std::env::temp_dir().join("rs_note_notebook"));
        idx.upsert(ExerciseMeta {
            path: "generated/x.rs".into(),
            title: "难倒我的题".into(),
            concepts: vec!["c.weak".into()],
            error_codes: vec![],
            difficulty: None,
            source: Source::TemplateFill { template_id: "t".into() },
            session_id: None,
            trigger: None,
            created_at: None,
            attempts: 3,
            status: Status::Failed { times: 2 },
            last_error: Some("E0382".into()),
            hints: Vec::new(),
            feedback: None,
            slots: Default::default(),
            reference: None,
            constraints: Vec::new(),
            review_verdict: None,
        });
        let nb = crate::profile::notebook_from_index(&idx, None);
        let note = build_practice_note(None, &p, &nb).unwrap();
        assert!(note.contains("Weakest concepts") && note.contains("c.weak (1 fails)"), "{note}");
        assert!(note.contains("SM-2 reviews due now: c.due"), "{note}");
        assert!(note.contains("Most-failed exercises"), "{note}");
        assert!(note.contains("难倒我的题") && note.contains("E0382"), "{note}");
        // Smoke: the store roundtrip used by the real turn.
        let _ = ProfileStore::load_or_create();
    }

    #[test]
    fn command_parsing() {
        assert!(matches!(parse_command("你好"), Cmd::Chat));
        assert!(matches!(parse_command("/exit"), Cmd::Exit));
        assert!(matches!(parse_command("/q"), Cmd::Exit));
        assert!(matches!(parse_command("/help"), Cmd::Help));
        assert!(matches!(parse_command("/p"), Cmd::Practice(None)));
        assert!(matches!(parse_command("/practice all"), Cmd::Practice(Some("all"))));
        assert!(matches!(parse_command("/usage"), Cmd::Usage));
        assert!(matches!(parse_command("/u"), Cmd::Usage));
        assert!(matches!(parse_command("/config"), Cmd::Config));
        assert!(matches!(parse_command("/new"), Cmd::New));
        assert!(matches!(parse_command("/topics"), Cmd::Topics));
        assert!(matches!(parse_command("/clear"), Cmd::Clear(false)));
        assert!(matches!(parse_command("/clear all"), Cmd::Clear(true)));
        assert!(matches!(parse_command("/ui"), Cmd::Ui(None)));
        assert!(matches!(parse_command("/ui scroll"), Cmd::Ui(Some("scroll"))));
        assert!(matches!(parse_command("/sessions"), Cmd::Sessions(None)));
        assert!(matches!(parse_command("/sessions 3"), Cmd::Sessions(Some(_))));
        assert!(matches!(parse_command("/g"), Cmd::Generate(None)));
        assert!(matches!(parse_command("/g E0382"), Cmd::Generate(Some("E0382"))));
        assert!(matches!(parse_command("/model"), Cmd::Model(None)));
        assert!(matches!(parse_command("/model fast"), Cmd::Model(Some("fast"))));
        assert!(matches!(parse_command("/m"), Cmd::Model(None)));
        assert!(matches!(parse_command("/helo"), Cmd::Unknown(_)));
        // A lone slash command word with no meaning is unknown.
        assert!(matches!(parse_command("/ "), Cmd::Unknown(_)));
    }

    #[test]
    fn sessions_index_parsing() {
        assert_eq!(parse_index(Some("2"), 5), Some(2));
        assert_eq!(parse_index(Some("0"), 5), None);
        assert_eq!(parse_index(Some("6"), 5), None);
        assert_eq!(parse_index(Some("x"), 5), None);
        assert_eq!(parse_index(None, 5), None);
    }
}
