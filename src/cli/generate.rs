//! `g` 生成练习命令（M3 出题基座的 CLI 入口）。
//!
//! Flow: topic input → template pick → slot fill (LLM if configured,
//! offline fallback otherwise) → triple verify → write into
//! `exercises/generated/` → optionally enter the practice flow at once.

use std::io::{self, Write};
use std::path::Path;

use crate::config::ModelConfig;
use crate::exercise::Exercise;
use crate::generator;
use crate::llm::LlmClient;
use crate::usage;

/// `g` — generate an exercise from a topic (M3 出题基座入口).
#[allow(clippy::too_many_arguments)]
pub(super) fn cmd_generate(
    cfg: &ModelConfig,
    tracker: &mut usage::UsageTracker,
    client: &Option<LlmClient>,
    exercises: &mut Vec<Exercise>,
    progress: &mut Vec<String>,
    progress_path: &Path,
) {
    println!();
    println!("── 生成练习 ──");
    println!("  输入主题：概念（如 trait 关联类型）、错误码（如 E0382）或关键词；直接回车返回。");
    print!("主题> ");
    io::stdout().flush().ok();
    let mut line = String::new();
    if io::stdin().read_line(&mut line).unwrap_or(0) == 0 {
        return;
    }
    let topic_text = line.trim().to_string();
    if topic_text.is_empty() {
        return;
    }

    // R6: budget gate before any LLM usage.
    if let Err(e) = usage::check_budget(tracker.all_totals().cost_usd, cfg.budget_usd()) {
        println!();
        println!("  LLM 调用被拦截：{e}");
        println!("  将使用离线模式生成（默认填槽，无 LLM 变体）。");
    }

    let topic = parse_topic(&topic_text);

    // LLM caller: enforces budget + records usage per call (R6). When no
    // key is configured the generator runs fully offline.
    let mut call = |prompt: &str| -> anyhow::Result<crate::llm::LlmReply> {
        usage::check_budget(tracker.all_totals().cost_usd, cfg.budget_usd())?;
        let cl = client
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("未配置 API Key，无法调用模型"))?;
        let reply = cl.chat(prompt)?;
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
            "generate",
        );
        println!(
            "  · LLM: 输入 {} tok / 输出 {} tok / ${:.6}",
            reply.usage.prompt_tokens, reply.usage.completion_tokens, cost
        );
        Ok(reply)
    };
    let llm: Option<&mut dyn generator::LlmCaller> = if client.is_some() {
        Some(&mut call)
    } else {
        println!("  未配置 API Key —— 使用离线模式（默认填槽）。");
        None
    };

    println!(
        "  正在生成：选模板 → 填槽 → 三重校验（最多 {} 轮）…",
        generator::MAX_ATTEMPTS
    );
    let paths = generator::Paths::from_root(Path::new("."));
    match generator::generate(&topic, &paths, llm) {
        Ok(out) => {
            let slots: Vec<String> =
                out.slots.iter().map(|(k, v)| format!("{k}={v}")).collect();
            println!();
            println!(
                "  ✔ 已生成（第 {} 轮通过）：{}（{}）",
                out.attempts,
                out.title,
                out.difficulty.name_cn()
            );
            println!("    模板 {} ｜ 概念 {}", out.template_id, out.concepts.join("、"));
            if slots.is_empty() {
                println!("    槽位 无");
            } else {
                println!(
                    "    槽位 {}（{}）",
                    slots.join(", "),
                    if out.used_llm { "LLM 填槽" } else { "默认填槽" }
                );
            }
            println!("    文件 {}（练习名 {}）", out.path.display(), out.name);
            println!();
            print!("  现在开始做这道题？[Y/n] ");
            io::stdout().flush().ok();
            let mut go = String::new();
            if io::stdin().read_line(&mut go).unwrap_or(0) == 0 {
                return;
            }
            let go = go.trim().to_ascii_lowercase();
            if go == "n" || go == "no" {
                return;
            }
            // Refresh the exercise list so the new file is discoverable.
            // Path comparison uses canonicalize(): the generator's path
            // carries a "./" prefix while discover() yields plain
            // relative paths, so raw equality would always miss.
            let mut fresh = crate::exercise::discover(Path::new("exercises"));
            fresh.sort_by(|a, b| a.category.cmp(&b.category).then(a.name.cmp(&b.name)));
            let want = out.path.canonicalize().ok();
            if let Some(idx) = fresh.iter().position(|e| e.path.canonicalize().ok() == want) {
                *exercises = fresh;
                super::run_exercise(idx, exercises, progress, progress_path, cfg.editor.as_deref());
            } else {
                println!("  生成文件未出现在练习列表（意外），可手动打开 {}", out.path.display());
            }
        }
        Err(e) => {
            println!();
            println!("  生成失败：{e:#}");
            println!("  可换一个主题重试，或检查 templates/ 与 taxonomy/ 的内容。");
        }
    }
}

/// Heuristic topic parsing: `E0382`-style inputs become an error-code
/// request, dotted ids like `traits.associated-types` become a concept
/// request, everything else is free text.
fn parse_topic(input: &str) -> generator::Topic {
    let t = input.trim();
    let is_code = t.len() == 5
        && (t.starts_with('E') || t.starts_with('e'))
        && t[1..].chars().all(|c| c.is_ascii_digit());
    let is_concept_id = t.contains('.')
        && t.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'));
    if is_code {
        generator::Topic::ErrorCode(t.to_uppercase())
    } else if is_concept_id {
        generator::Topic::Concept(t.to_string())
    } else {
        generator::Topic::FreeText(t.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generator::Topic;

    #[test]
    fn topic_parsing_heuristics() {
        assert_eq!(
            parse_topic("e0382"),
            Topic::ErrorCode("E0382".to_string())
        );
        assert_eq!(
            parse_topic("E0277"),
            Topic::ErrorCode("E0277".to_string())
        );
        assert_eq!(
            parse_topic("traits.associated-types"),
            Topic::Concept("traits.associated-types".to_string())
        );
        assert_eq!(
            parse_topic("来一道 Box<dyn Error> 的题"),
            Topic::FreeText("来一道 Box<dyn Error> 的题".to_string())
        );
        // Not a 4-digit code → free text.
        assert_eq!(parse_topic("E99999"), Topic::FreeText("E99999".to_string()));
    }
}
