//! Practice sub-mode (M4, restructured in M4.5a): the rustlings-style
//! exercise browser and solve loop, reached from the REPL (`/practice`,
//! `/practice all`, a generated exercise, or an agent practice offer).
//!
//! M4.5a layout (docs/出题规划_M4.5.md §3.2):
//! - Three views: **home** (this session's exercises first, then a
//!   per-topic overview), **topic** (one topic's exercises), **all**
//!   (every learner exercise, grouped by topic). Seed fixtures live in
//!   their own topic and only appear under `/practice all`.
//! - Every exercise is backed by the exercise index (`ExerciseIndex`):
//!   status marks, attempts, last error code, quality feedback (`f`).
//! - The exercise page is a card (concepts / difficulty / source /
//!   trigger) and offers `[a] 问教练`: the current code is handed back
//!   to the conversation as a coach request (M5 review-gate entry).

use std::path::Path;

use crate::exercise::{self, index::ExerciseIndex, Exercise};
use crate::taxonomy::ConceptGraph;

use super::render;
use super::{read_line, Line};

pub(crate) struct PracticeCtx {
    /// `exercises/` directory.
    pub root: std::path::PathBuf,
    /// Repo root (holds `taxonomy/`); `root.parent()` in practice.
    pub repo_root: std::path::PathBuf,
    pub editor: Option<String>,
}

impl PracticeCtx {
    pub(crate) fn new(root: &Path, editor: Option<String>) -> Self {
        Self {
            root: root.to_path_buf(),
            repo_root: root.parent().map(Path::to_path_buf).unwrap_or_default(),
            editor,
        }
    }

    pub(crate) fn load_index(&self) -> ExerciseIndex {
        ExerciseIndex::load(&self.root)
    }
}

/// Entry options for the practice board.
#[derive(Debug, Clone, Copy)]
pub(crate) struct EnterOpts<'s> {
    /// `/practice all`: include the seed fixtures topic.
    pub include_fixtures: bool,
    /// This session's produced exercises (index keys, in order).
    pub session_paths: &'s [String],
}

/// One practiceable exercise + its index key.
struct Item {
    ex: Exercise,
    key: String,
}

/// Board views.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Mode {
    Home,
    /// Topic label, or "fixtures" for the seed group.
    Topic(String),
    All,
}

/// Fixture pseudo-topic label (not a real concept domain).
const FIXTURE_TOPIC: &str = "fixtures";

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

/// Enter the list menu. Returns Some(coach-message) when the user asked
/// the coach about an exercise (`[a]`); the REPL turns that into a
/// conversation turn.
pub(crate) fn enter(ctx: &PracticeCtx, opts: EnterOpts) -> Option<String> {
    let mut index = ctx.load_index();
    let items = load_items(ctx);
    let graph = load_graph(ctx);
    let mut mode = Mode::Home;
    loop {
        let board = Board::build(&items, &index, &graph, opts.session_paths, opts.include_fixtures);
        let visible = render_board(&board, &mode);
        let line = read_prompt("> ")?;
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        if line == "b" || line == "q" || line == "back" {
            return None;
        }
        match line.as_str() {
            "h" | "home" => mode = Mode::Home,
            "a" | "all" => mode = Mode::All,
            "t" => {
                list_topics(&board);
                continue;
            }
            "n" => {
                match first_pending(&items, &index, opts.include_fixtures) {
                    Some(idx) => {
                        if let Some(msg) = run_exercise(ctx, &mut index, idx, &items) {
                            return Some(msg);
                        }
                    }
                    None => println!("  所有练习已完成！"),
                }
                continue;
            }
            "v" | "verify" => {
                verify_all(&items, &mut index, opts.include_fixtures);
                continue;
            }
            other => {
                if (other == FIXTURE_TOPIC || other == "f") && opts.include_fixtures {
                    mode = Mode::Topic(FIXTURE_TOPIC.to_string());
                    continue;
                }
                if let Some(word) = other.strip_prefix('t').map(str::trim).filter(|w| !w.is_empty()) {
                    mode = match_topic(&board, word);
                    continue;
                }
                match other.parse::<usize>() {
                    Ok(n) if n >= 1 && n <= visible.len() => {
                        let idx = visible[n - 1];
                        if let Some(msg) = run_exercise(ctx, &mut index, idx, &items) {
                            return Some(msg);
                        }
                    }
                    _ => println!("未知命令: {other}（数字选题 / n 下一题 / t 主题 / a 全库 / b 返回）"),
                }
            }
        }
    }
}

/// Jump straight into one exercise (by path), then fall through to the
/// list menu. Used after a generated exercise.
pub(crate) fn enter_at(ctx: &PracticeCtx, path: &Path, opts: EnterOpts) -> Option<String> {
    let mut index = ctx.load_index();
    let items = load_items(ctx);
    let want = path.canonicalize().ok();
    let idx = items.iter().position(|i| i.ex.path.canonicalize().ok() == want);
    match idx {
        Some(i) => {
            if let Some(msg) = run_exercise(ctx, &mut index, i, &items) {
                return Some(msg);
            }
        }
        None => println!("  生成文件未出现在练习列表（意外），可手动打开 {}", path.display()),
    }
    enter(ctx, opts)
}

// ---------------------------------------------------------------------------
// Data assembly
// ---------------------------------------------------------------------------

fn load_items(ctx: &PracticeCtx) -> Vec<Item> {
    let mut v: Vec<Exercise> = exercise::discover_all(&ctx.root);
    v.sort_by(|a, b| a.category.cmp(&b.category).then(a.name.cmp(&b.name)));
    v.into_iter()
        .map(|ex| Item { key: ex.rel_path.clone(), ex })
        .collect()
}

fn load_graph(ctx: &PracticeCtx) -> Option<ConceptGraph> {
    ConceptGraph::load(&ctx.repo_root.join("taxonomy").join("concepts.toml")).ok()
}

/// Assembled board: grouping + session ordering, independent of the
/// current view.
struct Board<'a> {
    items: &'a [Item],
    index: &'a ExerciseIndex,
    /// Session exercises as positions in `items`, production order.
    session: Vec<usize>,
    /// Topic groups (non-fixture): (label, topic id, positions).
    topics: Vec<(String, String, Vec<usize>)>,
    /// Seed fixture positions.
    fixtures: Vec<usize>,
    include_fixtures: bool,
}

impl<'a> Board<'a> {
    fn build(
        items: &'a [Item],
        index: &'a ExerciseIndex,
        graph: &'a Option<ConceptGraph>,
        session_paths: &[String],
        include_fixtures: bool,
    ) -> Self {
        let pos_of: std::collections::HashMap<&str, usize> = items
            .iter()
            .enumerate()
            .map(|(i, it)| (it.key.as_str(), i))
            .collect();
        let session: Vec<usize> = session_paths
            .iter()
            .filter_map(|p| pos_of.get(p.as_str()).copied())
            .collect();

        let mut others: Vec<usize> = Vec::new();
        let mut fixtures: Vec<usize> = Vec::new();
        for (i, it) in items.iter().enumerate() {
            if it.ex.is_fixture {
                fixtures.push(i);
            } else {
                others.push(i);
            }
        }

        // Group by the exercise's first concept's top-level domain.
        let mut groups: Vec<(String, String, Vec<usize>)> = Vec::new(); // (label, top id, positions)
        for i in &others {
            let concept = index.get(&items[*i].key).and_then(|m| m.concepts.first());
            let (label, top) = topic_label(concept.map(String::as_str), graph.as_ref());
            match groups.iter_mut().find(|(l, _, _)| *l == label) {
                Some((_, _, positions)) => positions.push(*i),
                None => groups.push((label, top, vec![*i])),
            }
        }
        groups.sort_by(|a, b| a.0.cmp(&b.0));
        let topics = groups;

        Self { items, index, session, topics, fixtures, include_fixtures }
    }
}

/// (label, top-level domain id) for a concept id; empty → ("其他", "").
fn topic_label(concept: Option<&str>, graph: Option<&ConceptGraph>) -> (String, String) {
    let Some(c) = concept else { return ("其他".to_string(), String::new()) };
    let top = c.split('.').next().unwrap_or(c);
    let name = graph
        .and_then(|g| g.get(top))
        .map(|n| n.name.clone())
        .unwrap_or_else(|| top.to_string());
    (name, top.to_string())
}

/// Resolve a `t <word>` argument to a mode: exact label, exact top id,
/// or substring of the label. Falls back to Home with a notice.
fn match_topic(board: &Board, word: &str) -> Mode {
    for (label, top, _) in &board.topics {
        if label == word || top == word || label.contains(word) {
            return Mode::Topic(label.clone());
        }
    }
    if word == FIXTURE_TOPIC && board.include_fixtures {
        return Mode::Topic(FIXTURE_TOPIC.to_string());
    }
    println!("  没有主题「{word}」；输入 t 查看主题列表");
    Mode::Home
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Render the current view; returns the selectable item positions in
/// display order (1-based numbering maps onto this vector).
fn render_board(board: &Board, mode: &Mode) -> Vec<usize> {
    if render::ansi_enabled() {
        render::clear_viewport();
    }
    println!("{}", render::cyan("── 做题模式 ──"));
    let (done, total) = progress(board);
    println!("  进度 {}", render::progress_bar(done, total, 20));
    println!();
    match mode {
        Mode::Home => {
            let mut visible = Vec::new();
            if !board.session.is_empty() {
                println!("  {}（{}）", render::cyan("本会话"), board.session.len());
                for &i in &board.session {
                    print_row(board, i, visible.len() + 1);
                    visible.push(i);
                }
                println!();
            }
            println!("  {}", render::cyan("按主题"));
            for (label, _, ids) in &board.topics {
                let done_n = ids.iter().filter(|&&i| is_passed(board, i)).count();
                println!("    {label}({})  已过 {done_n}", ids.len());
            }
            if board.include_fixtures && !board.fixtures.is_empty() {
                let done_n = board.fixtures.iter().filter(|&&i| is_passed(board, i)).count();
                println!("    {FIXTURE_TOPIC}({})  已过 {done_n}   [t {FIXTURE_TOPIC}]", board.fixtures.len());
            }
            if board.session.is_empty() {
                println!("    （本会话还没有产出题目；对话中说「来一道 XX 的题」即可）");
            }
            println!();
            if visible.is_empty() {
                println!("  数字选题（本会话暂无题目；a 看全库后用数字选题）");
            } else {
                println!("  数字选题（本会话） ｜ a 全库 ｜ t <主题> ｜ n 下一题 ｜ v 验证 ｜ b 返回");
            }
            visible
        }
        Mode::Topic(label) => {
            let ids: Vec<usize> = if label == FIXTURE_TOPIC {
                board.fixtures.clone()
            } else {
                board.topics.iter().find(|(l, _, _)| l == label).map(|(_, _, ids)| ids.clone()).unwrap_or_default()
            };
            println!("  {}（{}）", render::cyan(label), ids.len());
            for (n, &i) in ids.iter().enumerate() {
                print_row(board, i, n + 1);
            }
            println!();
            println!("  数字选题 ｜ h 首页 ｜ a 全库 ｜ t <主题> ｜ b 返回");
            ids
        }
        Mode::All => {
            let mut visible = Vec::new();
            for (label, _, ids) in &board.topics {
                println!("  {}", render::cyan(label));
                for &i in ids {
                    print_row(board, i, visible.len() + 1);
                    visible.push(i);
                }
            }
            if board.include_fixtures && !board.fixtures.is_empty() {
                println!("  {}", render::cyan(FIXTURE_TOPIC));
                for &i in &board.fixtures {
                    print_row(board, i, visible.len() + 1);
                    visible.push(i);
                }
            }
            println!();
            println!("  数字选题 ｜ h 首页 ｜ t <主题> ｜ v 验证 ｜ b 返回");
            visible
        }
    }
}

/// One list row: number, status mark, title, meta summary.
fn print_row(board: &Board, i: usize, n: usize) {
    let it = &board.items[i];
    let meta = board.index.get(&it.key);
    let mark = meta.map(|m| m.status_mark()).unwrap_or_else(|| " ".into());
    let meta_txt = match meta {
        Some(m) => {
            let mut parts: Vec<String> = Vec::new();
            if let Some(d) = &m.difficulty {
                parts.push(difficulty_cn(d).to_string());
            }
            if let Some(code) = m.error_codes.first() {
                parts.push(code.clone());
            }
            parts.push(m.source.label_cn());
            parts.join(" · ")
        }
        None => "未跟踪".to_string(),
    };
    println!("    {:>2}. [{}] {:<26} {}", n, mark, truncate(&it.ex.title, 26), meta_txt);
}

fn truncate(s: &str, w: usize) -> String {
    if s.chars().count() <= w {
        s.to_string()
    } else {
        let head: String = s.chars().take(w - 1).collect();
        format!("{head}…")
    }
}

fn difficulty_cn(s: &str) -> &'static str {
    match s {
        "easy" => "简单",
        "medium" => "中等",
        "hard" => "困难",
        _ => "—",
    }
}

fn is_passed(board: &Board, i: usize) -> bool {
    board
        .index
        .get(&board.items[i].key)
        .map(|m| matches!(m.status, crate::exercise::index::Status::Passed))
        .unwrap_or(false)
}

/// (done, total) over learner exercises (fixtures excluded).
fn progress(board: &Board) -> (usize, usize) {
    let all: Vec<usize> = board.topics.iter().flat_map(|(_, _, ids)| ids.iter().copied()).collect();
    let done = all.iter().filter(|&&i| is_passed(board, i)).count();
    (done, all.len())
}

fn first_pending(items: &[Item], index: &ExerciseIndex, include_fixtures: bool) -> Option<usize> {
    items.iter().position(|it| {
        (include_fixtures || !it.ex.is_fixture)
            && index
                .get(&it.key)
                .map(|m| matches!(m.status, crate::exercise::index::Status::Pending | crate::exercise::index::Status::Failed { .. }))
                .unwrap_or(true)
    })
}

fn list_topics(board: &Board) {
    println!("  可用主题：");
    for (label, top, ids) in &board.topics {
        println!("    {label:<10} t {top:<14} {} 题", ids.len());
    }
    if board.include_fixtures && !board.fixtures.is_empty() {
        println!("    {FIXTURE_TOPIC:<10} t {FIXTURE_TOPIC:<14} {} 题", board.fixtures.len());
    }
}

// ---------------------------------------------------------------------------
// Solve loop
// ---------------------------------------------------------------------------

/// Practice prompts: Ctrl-C cancels the line (loop continues), EOF
/// leaves the page back to the chat.
fn read_prompt(prompt: &str) -> Option<String> {
    match read_line(prompt) {
        Line::Text(s) => Some(s),
        Line::Interrupted => {
            println!("  ^C 已取消本行输入");
            None
        }
        Line::Eof => None,
    }
}

/// Practice one exercise. Returns Some(coach-message) for `[a] 问教练`.
fn run_exercise(
    ctx: &PracticeCtx,
    index: &mut ExerciseIndex,
    idx: usize,
    items: &[Item],
) -> Option<String> {
    let mut cur = idx;
    loop {
        let item = &items[cur];
        let key = item.key.clone();
        let meta = index.get(&key).cloned();
        repaint_exercise(&item.ex, meta.as_ref(), None);
        loop {
            let res = exercise::compile_and_run(&item.ex);
            index.record_attempt(&key, res.passed, res.first_error.as_deref());
            let meta = index.get(&key).cloned();
            if res.passed {
                println!();
                println!("  练习 '{}' 完成 - 已标记。", item.ex.name);
            }
            println!();
            println!("  [r] 重跑   [e] 编辑   [a] 问教练   [f] 反馈   [n] 下一题   [b] 返回");
            let s = read_prompt("> ")?;
            match s.trim() {
                "r" | "" => repaint_exercise(&item.ex, meta.as_ref(), Some(&res)),
                "e" => {
                    exercise::open_editor(&item.ex.path, ctx.editor.as_deref());
                    repaint_exercise(&item.ex, meta.as_ref(), None);
                }
                "a" => {
                    if let Some(msg) = coach_request(&item.ex, meta.as_ref(), &res) {
                        return Some(msg);
                    }
                    repaint_exercise(&item.ex, meta.as_ref(), Some(&res));
                }
                "f" => {
                    feedback_loop(index, &key);
                }
                "n" => {
                    match next_pending(cur, items, index) {
                        Some(next) => {
                            cur = next;
                            break;
                        }
                        None => println!("  所有练习已完成！"),
                    }
                }
                "b" | "q" | "back" => return None,
                other => println!("未知命令: {other}"),
            }
        }
    }
}

/// Exercise page: viewport clear + title card (before compile output).
fn repaint_exercise(ex: &Exercise, meta: Option<&crate::exercise::index::ExerciseMeta>, last: Option<&exercise::RunResult>) {
    if render::ansi_enabled() {
        render::clear_viewport();
    }
    println!("{}", render::cyan(&format!("── 《{}》 ──", ex.title)));
    if let Some(m) = meta {
        let mut parts: Vec<String> = Vec::new();
        if !m.concepts.is_empty() {
            parts.push(format!("概念 {}", m.concepts.join("、")));
        }
        if let Some(d) = &m.difficulty {
            parts.push(format!("难度 {}", difficulty_cn(d)));
        }
        parts.push(format!("来源 {}", m.source.label_cn()));
        println!("  {}", parts.join(" ｜ "));
        if let Some(t) = &m.trigger {
            println!("  触发：{t}");
        }
        let mut st = format!("状态：{}（{} 次尝试）", m.status_line_cn(), m.attempts);
        if let Some(code) = &m.last_error {
            st.push_str(&format!(" ｜ 上次 {}", render::red(code)));
        } else if let Some(code) = last.as_ref().and_then(|r| r.first_error.as_ref()) {
            st.push_str(&format!(" ｜ 上次 {}", render::red(code)));
        }
        println!("  {st}");
    } else {
        println!("  {}", ex.path.display());
    }
    println!();
}

fn feedback_loop(index: &mut ExerciseIndex, key: &str) {
    println!("  这道题感觉如何？ 1 太简单   2 合适   3 太难   4 没意义");
    if let Some(c) = read_prompt("反馈> ") {
        match crate::exercise::index::Feedback::from_choice(c.trim()) {
            Some(f) => {
                if index.set_feedback(key, f) {
                    println!("  已记录：{}（用于题目质量统计）", f.label_cn());
                } else {
                    println!("  该题未在索引中，无法记录");
                }
            }
            None => println!("  已取消"),
        }
    }
}

/// Assemble the "ask the coach" message: current code + status + the
/// user's optional question, handed back to the conversation. Ctrl-C
/// cancels (None).
fn coach_request(
    ex: &Exercise,
    meta: Option<&crate::exercise::index::ExerciseMeta>,
    last: &exercise::RunResult,
) -> Option<String> {
    println!("  向教练描述你的问题（直接回车 = 只带代码；Ctrl-C 取消）");
    let raw = read_prompt("问题> ")?;
    let t = raw.trim().to_string();
    let q = if t.is_empty() { None } else { Some(t) };
    let content = std::fs::read_to_string(&ex.path).unwrap_or_else(|_| "(无法读取文件)".to_string());
    let status = meta
        .map(|m| m.status_line_cn())
        .unwrap_or_else(|| "未跟踪".to_string());
    let mut msg = format!(
        "我在做练习《{}》（{}，{}）时遇到困难。\n\n当前代码：\n```rust\n{}\n```\n",
        ex.title,
        ex.rel_path,
        status,
        content.trim_end()
    );
    if let Some(code) = &last.first_error {
        msg.push_str(&format!("\n最近一次编译报错：{code}\n"));
    }
    match q {
        Some(q) => msg.push_str(&format!("\n我的问题：{q}\n")),
        None => msg.push_str("\n请帮我分析我在哪里卡住了（可以先用 check_code 编译取证）。\n"),
    }
    Some(msg)
}

fn next_pending(from: usize, items: &[Item], index: &ExerciseIndex) -> Option<usize> {
    let n = items.len();
    for off in 1..=n {
        let i = (from + off) % n;
        let tracked = index.get(&items[i].key);
        let pending = tracked
            .map(|m| matches!(m.status, crate::exercise::index::Status::Pending | crate::exercise::index::Status::Failed { .. }))
            .unwrap_or(true);
        if pending {
            return Some(i);
        }
    }
    None
}

fn verify_all(items: &[Item], index: &mut ExerciseIndex, include_fixtures: bool) {
    let selected: Vec<usize> = items
        .iter()
        .enumerate()
        .filter(|(_, it)| include_fixtures || !it.ex.is_fixture)
        .map(|(i, _)| i)
        .collect();
    let total = selected.len();
    let mut pass = 0;
    for (k, &i) in selected.iter().enumerate() {
        if crate::agent::is_interrupted() {
            crate::agent::reset_interrupt();
            println!();
            println!("  已打断（{pass}/{k} 通过）。");
            return;
        }
        let it = &items[i];
        println!();
        println!("[{}/{}] {} - {}", k + 1, total, it.ex.name, it.ex.title);
        let res = exercise::compile_and_run(&it.ex);
        index.record_attempt(&it.key, res.passed, res.first_error.as_deref());
        if res.passed {
            pass += 1;
            println!("  {}", render::green("✓ 通过"));
        } else {
            println!("  {}", render::red("✗ 未通过"));
        }
    }
    println!();
    let summary = format!("==== 全部验证完成: {pass}/{total} 通过 ====");
    println!("{}", if pass == total { render::green(&summary) } else { summary });
}

// ---------------------------------------------------------------------------
// Tests (view assembly helpers)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exercise::index::{ExerciseMeta, Source, Status};

    fn meta(path: &str, concepts: &[&str], status: Status, seed: bool) -> ExerciseMeta {
        ExerciseMeta {
            path: path.into(),
            title: "题".into(),
            concepts: concepts.iter().map(|s| s.to_string()).collect(),
            error_codes: vec!["E0382".into()],
            difficulty: Some("easy".into()),
            source: if seed { Source::Seed } else { Source::TemplateFill { template_id: "t".into() } },
            session_id: None,
            trigger: None,
            created_at: None,
            attempts: 0,
            status,
            feedback: None,
            last_error: None,
        }
    }

    fn ex(rel: &str, fixture: bool) -> Item {
        Item {
            ex: Exercise {
                path: std::path::PathBuf::from(rel),
                rel_path: rel.into(),
                name: "n".into(),
                category: "c".into(),
                title: "题".into(),
                is_fixture: fixture,
            },
            key: rel.into(),
        }
    }

    #[test]
    fn topic_grouping_uses_top_level_domain() {
        let items = vec![
            ex("generated/a.rs", false),
            ex("generated/b.rs", false),
            ex("generated/c.rs", false),
            ex("fixtures/s.rs", true),
        ];
        let mut index = ExerciseIndex::load_from(std::env::temp_dir().join("rs_practice_nonexistent"));
        index.upsert(meta("generated/a.rs", &["ownership.move"], Status::Passed, false));
        index.upsert(meta("generated/b.rs", &["ownership.move"], Status::Pending, false));
        index.upsert(meta("generated/c.rs", &["borrow.shared-mut"], Status::Failed { times: 1 }, false));
        index.upsert(meta("fixtures/s.rs", &[], Status::Pending, true));

        let graph = None::<ConceptGraph>;
        let board = Board::build(&items, &index, &graph, &["generated/b.rs".into()], false);
        // Session order first, fixtures excluded from topics.
        assert_eq!(board.session, vec![1]);
        assert_eq!(board.fixtures, vec![3]);
        let labels: Vec<&str> = board.topics.iter().map(|(l, _, _)| l.as_str()).collect();
        assert_eq!(labels, vec!["borrow", "ownership"], "no graph → top ids as labels");
        // Passing seed flag through: learner progress excludes fixtures.
        let (done, total) = progress(&board);
        assert_eq!((done, total), (1, 3));
    }

    #[test]
    fn topic_label_maps_via_graph_names() {
        // No real graph available in unit tests: ids pass through.
        assert_eq!(topic_label(Some("traits.bounds"), None), ("traits".to_string(), "traits".to_string()));
        assert_eq!(topic_label(None, None), ("其他".to_string(), String::new()));
    }

    #[test]
    fn pending_detection_falls_back_to_untracked() {
        let items = vec![ex("generated/x.rs", false)];
        let index = ExerciseIndex::load_from(std::env::temp_dir().join("rs_practice_nonexistent2"));
        // Untracked → pending.
        assert_eq!(first_pending(&items, &index, false), Some(0));
    }
}
