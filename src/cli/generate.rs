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

    // LLM caller: enforces budget + records usage per call (R6). Unlike
    // a bare closure it HONOURS `call_bounded` so the draft's
    // max_tokens cap actually reaches the wire — the default impl
    // silently ignores the cap, which is how `/g` drafts once burned
    // 27k output tokens per round (M4.7 regression caught in smoke).
    struct CliCaller<'a> {
        client: &'a LlmClient,
        model: &'a str,
        price_in: f64,
        price_out: f64,
        budget: Option<f64>,
        tracker: Arc<Mutex<UsageTracker>>,
    }

    impl generator::LlmCaller for CliCaller<'_> {
        fn call(&mut self, prompt: &str) -> anyhow::Result<crate::llm::LlmReply> {
            self.call_bounded(prompt, u32::MAX)
        }

        fn call_bounded(
            &mut self,
            prompt: &str,
            max_tokens: u32,
        ) -> anyhow::Result<crate::llm::LlmReply> {
            let totals =
                self.tracker.lock().unwrap_or_else(|p| p.into_inner()).all_totals().cost_usd;
            crate::usage::check_budget(totals, self.budget)?;
            let out = self.client.chat_turn_bounded(
                &[crate::llm::ChatMessage::user(prompt.to_string())],
                &[],
                (max_tokens != u32::MAX).then_some(max_tokens),
            )?;
            let reply = crate::llm::LlmReply {
                content: out.content.unwrap_or_default(),
                usage: out.usage,
                finish_reason: out.finish_reason,
            };
            let cost = crate::usage::cost_usd(
                reply.usage.prompt_tokens,
                reply.usage.completion_tokens,
                self.price_in,
                self.price_out,
            );
            self.tracker.lock().unwrap_or_else(|p| p.into_inner()).record(
                self.model,
                reply.usage.prompt_tokens,
                reply.usage.completion_tokens,
                reply.usage.reasoning_tokens,
                cost,
                "generate",
            );
            let reasoning = if reply.usage.reasoning_tokens > 0 {
                format!("（推理 {}）", reply.usage.reasoning_tokens)
            } else {
                String::new()
            };
            println!(
                "  · LLM: 输入 {} tok / 输出 {} tok{reasoning} / ${:.6}",
                reply.usage.prompt_tokens, reply.usage.completion_tokens, cost
            );
            Ok(reply)
        }
    }

    let llm: Option<&mut dyn generator::LlmCaller> = match client.as_ref() {
        Some(cl) => Some(&mut CliCaller {
            client: cl,
            model: &cfg.model,
            price_in: cfg.prices.input,
            price_out: cfg.prices.output,
            budget: cfg.budget_usd(),
            tracker: tracker.clone(),
        }),
        None => {
            println!("  未配置 API Key —— 使用离线模式（默认填槽）。");
            None
        }
    };

    println!(
        "  正在生成分层出题：模板直配 → 模板改编 → 自由生成（每题过三重校验）…"
    );
    let paths = generator::Paths::from_root(Path::new("."));
    // M4.10: past generations steer the pick (unused first, variants).
    let history = {
        let index = index::ExerciseIndex::load(&ctx.root);
        generator::GenHistory::from_index(&index)
    };
    match generator::generate_with_history(
        &topic,
        &paths,
        &history,
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
            if out.variant {
                println!("    ⚠ 同模板变式（此前已出过该模板的题，本次轮换了槽位）");
            }
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
                &out.hints,
                &out.slots,
                &out.reference,
                &out.constraints,
            ) {
                Ok(_) => {}
                Err(e) => println!("  （index 登记失败：{e:#}）"),
            }

            // EOF must stay conservative (don't drop into practice);
            // skipping gets a pointer to the board (trial feedback).
            match read_line_or_leave("  现在开始做这道题？[Y/n] ") {
                None => {
                    println!("  （题目已进练习库：/practice 随时可继续）");
                    return Some(out.path);
                }
                Some(go) => {
                    let go = go.to_ascii_lowercase();
                    if go == "n" || go == "no" {
                        println!("  （题目已进练习库：/practice 随时可继续）");
                        return Some(out.path);
                    }
                }
            }
            // Path comparison uses canonicalize(): the generator's path
            // carries a "./" prefix while discover() yields plain
            // relative paths, so raw equality would always miss.
            let deps = super::debrief::DebriefDeps {
                client: client.as_ref(),
                cfg,
                tracker: tracker.clone(),
                editor: cfg.editor.as_deref(),
            };
            practice::enter_at(
                ctx,
                &out.path,
                practice::EnterOpts {
                    include_fixtures: false,
                    session_paths: session.map(|(_, paths)| paths).unwrap_or(&[]),
                },
                Some(&deps),
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
