//! `/generate` — exercise generation entry (M3 pipeline; offline
//! capable). Shared by the REPL command and used by the agent's
//! `generate_exercise` tool (the tool calls `generator::generate`
//! directly). Progress prints stage lines; Ctrl-C aborts between
//! rounds (the interrupt flag is checked inside the generator).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::config::ModelConfig;
use crate::exercise::index;
use crate::generator;
use crate::llm::LlmClient;
use crate::usage::UsageTracker;

use super::{practice, read_line_or_leave};

/// `g` / `/generate` — generate an exercise from a topic. `arg` may
/// carry the topic directly (`/g E0382`); otherwise it is prompted.
/// Returns the generated exercise path (for session bookkeeping).
pub(crate) fn cmd_generate(
    cfg: &ModelConfig,
    tracker: &Arc<Mutex<UsageTracker>>,
    client: &Option<LlmClient>,
    ctx: &practice::PracticeCtx,
    session: Option<(&str, &[String])>,
    arg: Option<&str>,
) -> Option<PathBuf> {
    println!();
    println!("── 生成练习 ──");
    let topic_text = match arg {
        Some(t) => t.to_string(),
        None => {
            println!("  输入主题：概念（如 trait 关联类型）、错误码（如 E0382）或关键词；直接回车返回。");
            match read_line_or_leave("主题> ") {
                None => return None,
                Some(t) if t.is_empty() => return None,
                Some(t) => t,
            }
        }
    };

    let totals = tracker.lock().unwrap_or_else(|p| p.into_inner()).all_totals().cost_usd;
    // R6: budget gate before any LLM usage.
    if let Err(e) = crate::usage::check_budget(totals, cfg.budget_usd()) {
        println!();
        println!("  LLM 调用被拦截：{e}");
        println!("  将使用离线模式生成（默认填槽，无 LLM 变体）。");
    }

    let topic = generator::Topic::from_input(&topic_text);

    // LLM caller: enforces budget + records usage per call (R6). When
    // no key is configured the generator runs fully offline.
    let tracker2 = tracker.clone();
    let mut call = move |prompt: &str| -> anyhow::Result<crate::llm::LlmReply> {
        let totals = tracker2.lock().unwrap_or_else(|p| p.into_inner()).all_totals().cost_usd;
        crate::usage::check_budget(totals, cfg.budget_usd())?;
        let cl = client
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("未配置 API Key，无法调用模型"))?;
        let reply = cl.chat(prompt)?;
        let cost = crate::usage::cost_usd(
            reply.usage.prompt_tokens,
            reply.usage.completion_tokens,
            cfg.prices.input,
            cfg.prices.output,
        );
        tracker2.lock().unwrap_or_else(|p| p.into_inner()).record(
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
        "  正在生成分层出题：模板直配 → 模板改编 → 自由生成（每题过三重校验）…"
    );
    let paths = generator::Paths::from_root(Path::new("."));
    match generator::generate(
        &topic,
        &paths,
        llm,
        Some(&mut |stage: generator::GenerateStage| {
            match &stage.note {
                Some(n) => println!(
                    "  · {}（第 {}/{} 轮）上一轮被拒：{n}",
                    stage.stage, stage.attempt, stage.total_attempts
                ),
                None => println!(
                    "  · {}（第 {}/{} 轮）…",
                    stage.stage, stage.attempt, stage.total_attempts
                ),
            }
        }),
    ) {
        Ok(out) => {
            let slots: Vec<String> = out.slots.iter().map(|(k, v)| format!("{k}={v}")).collect();
            println!();
            println!(
                "  ✔ 已生成（第 {} 轮通过）：{}（{}）",
                out.attempts,
                out.title,
                out.difficulty.name_cn()
            );
            println!("    来源 {} ｜ 概念 {}", out.tier.label_cn(), out.concepts.join("、"));
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

            // M4.5a: register in the exercise index (with trigger
            // context) so the board attributes this exercise.
            let trigger = format!("你请求了「{topic_text}」");
            match index::register_generated(
                &ctx.root,
                &out.path,
                &out.title,
                &out.concepts,
                &out.error_codes,
                Some(out.difficulty.as_str()),
                out.tier.to_source(),
                session.map(|(id, _)| id),
                Some(&trigger),
            ) {
                Ok(_) => {}
                Err(e) => println!("  （index 登记失败：{e:#}）"),
            }

            let go = read_line_or_leave("  现在开始做这道题？[Y/n] ").unwrap_or_default();
            let go = go.to_ascii_lowercase();
            if go == "n" || go == "no" {
                return Some(out.path);
            }
            // Path comparison uses canonicalize(): the generator's path
            // carries a "./" prefix while discover() yields plain
            // relative paths, so raw equality would always miss.
            practice::enter_at(
                ctx,
                &out.path,
                practice::EnterOpts {
                    include_fixtures: false,
                    session_paths: session.map(|(_, paths)| paths).unwrap_or(&[]),
                },
            );
            Some(out.path)
        }
        Err(e) => {
            println!();
            println!("  生成失败：{e:#}");
            println!("  可换一个主题重试，或检查 templates/ 与 taxonomy/ 的内容。");
            println!("  提示：自由生成依赖 LLM 长输出；端点慢时可在 /config 调高");
            println!("  llm_timeout_secs（或换更快的模型）。");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn topic_parsing_heuristics_via_generator() {
        use crate::generator::Topic;
        assert_eq!(Topic::from_input("e0382"), Topic::ErrorCode("E0382".to_string()));
        assert_eq!(Topic::from_input("E0277"), Topic::ErrorCode("E0277".to_string()));
        assert_eq!(
            Topic::from_input("traits.associated-types"),
            Topic::Concept("traits.associated-types".to_string())
        );
        assert_eq!(
            Topic::from_input("来一道 Box<dyn Error> 的题"),
            Topic::FreeText("来一道 Box<dyn Error> 的题".to_string())
        );
        // Not a 4-digit code → free text.
        assert_eq!(Topic::from_input("E99999"), Topic::FreeText("E99999".to_string()));
    }
}
