# RustLings-Adaptive（my_rustlings）

**对话式 Rust 诊断教练 Agent** —— 课程 AI Agent 大作业项目。

把 rustlings 式"填空小练习"作为对话中的即时诊断工具：用户提问/贴报错 →
Agent 解释并锚定知识点（细分概念图谱 + rustc 错误码双轨）→ 出一道经本地
三重校验的小片段题 → 用户做题 → 解答评审门 + 交互式复盘（原理解释、
更优解对比）→ 归档进错误画像与错题本，定向巩固。

核心理念（小片段主义）：把复杂场景规约到一个个 10–40 行的可控片段，
质量易把握，便于针对性指导与统计数据。

## 当前开发状态

> 设计阶段已收敛，进入实现。**每完成一个里程碑更新此表。**

| 里程碑 | 内容 | 对应硬要求 | 状态 |
|---|---|---|---|
| M0 | CLI 骨架 + 8 道种子练习 + IDE 子 crate + rustc --test 跑练习 | — | ✅ 完成 |
| M1 | 模块拆分 + LLM 接入 + 模型配置 + token 计费 | R1/R3/R6 | ✅ 完成 |
| M2 | 验证器：rustc --json 解析 + 三重校验 + 约束静态检查 | R1 | ✅ 完成 |
| M3 | 模板库（TOML）+ 概念图谱（concepts.toml）+ 填槽生成 | — | ✅ 完成 |
| M4 | 对话 REPL + Agent 工具环 + 进度/打断 + 会话历史 | R2/R4/R5 | ⬜ **下一个** |
| M4.5 | 分层出题：模板改编 + 自由生成（同一质量门收口，§7.5） | — | ⬜ 9.5 展示基线加固 |
| M5 | 解答评审门 + 交互式复盘（解释/更优解挑战/对比表/再练决策） | — | ⬜ |
| M6 | 双轨画像（错误码 + 概念 SM-2）+ 错题本 | R5 | ⬜ |
| M7 | 借用检查器假设实验室（招牌，可砍） | — | ⬜ |
| M8 | 收尾：README 定稿、集成测试、演示脚本、文档对齐、开销表 | — | ⬜ |

关键时间节点：**9.6 公开展示**（设计文档摘要 + 项目链接，需基本功能）、
9.8 前试用 3 位同学作品、**9.10 课堂展示**（5 分钟演示 + 提问）。

## 编译

要求：Rust 1.85+（edition 2024），本机装有 `rustc`（练习用它编译运行）。

```bash
cargo build          # 或 cargo build --release
```

## 配置（R3/R6）

模型调用需要一个 OpenAI 兼容的 endpoint 与 API Key（支持 OpenAI、DeepSeek、
本地 Ollama / vLLM 等任意兼容服务）。三种配置方式，任选其一：

1. **配置文件（推荐）**：`cp config.example.toml config.toml`，然后编辑
   `endpoint` / `api_key` / `model`，按需修改价格表 `[prices]` 与预算 `[budget]`；
2. **环境变量**：在项目根目录建 `.env`，写 `RUSTLINGS_API_KEY=sk-...`
   （也可用 `RUSTLINGS_ENDPOINT` / `RUSTLINGS_MODEL` 覆盖对应项，
   优先级高于 config.toml）；
3. **程序内配置页**：运行后按 `c`，交互修改 endpoint / model / api_key /
   预算，修改会写回 config.toml。

> 注意：`config.toml` 与 `.env` 含 API Key，已被 `.gitignore` 排除，请勿提交。
> 预算（R6）：累计花费达到 `[budget].usd` 后，后续模型调用会被自动拦截。

## 运行

```bash
cargo run            # 必须在项目根目录运行
```

主菜单：

```
  <数字> 选题   n 下一题   v 全部验证   a 问模型   g 生成练习   u 用量   c 配置   h 帮助   q 退出
```

- 做题流内：`r` 重跑 / `e` 编辑（编辑器解析链：$EDITOR → $VISUAL →
  config `editor` → 自动探测 `code --wait` → vi，`[c]` 第 5 项可改）/
  `n` 下一题 / `b` 返回
- `a` 问模型：一问一答（输入问题回车发送），回复后打印本条与累计的
  token 用量和花费；累计花费达到预算会被拦截
- `u` 用量：本次会话与历史累计的调用次数 / token / 花费 / 预算余量
  （明细持久化在 `~/.rustlings_adaptive/usage.json`）
- `g` 生成练习：输入主题（概念如 `trait 关联类型` / 错误码如 `E0382` /
  关键词）→ 选模板 → 填槽（LLM，未配 Key 则离线默认填槽）→ 三重校验
  （最多 3 轮重试）→ 写入 `exercises/generated/` 并接线 lib.rs → 可立即开练
  （覆盖不足的主题由 M4.5"分层出题"承接：模板改编 + 自由生成）

## 演示用例

1. **模型问答与计费（R1/R3/R6）**：`cargo run` → 配置好 key 后按 `a` →
   输入"用一句话解释什么是所有权"→ 看到回复 + "本条: 输入 N tok /
   输出 M tok / 花费 $x ｜ 累计 $y"；
2. **预算中断（R6）**：把 config.toml 的 `[budget].usd` 改成一个比累计
   花费小的数（或 `c` 配置页改）→ 再按 `a` → 提示"调用被拦截"；
3. **配置切换（R3）**：按 `c` → 选 `2` 换一个 model（如换成
   `deepseek-chat`）→ 再按 `a`，新模型立即生效；
4. **做题流（M0）**：输入 `2` → 按 `e` 编辑练习补全 TODO → `r` 重跑 →
   全部测试通过后自动标记完成；`n` 跳下一题，`v` 全部验证。
5. **生成练习（M3）**：按 `g` → 输入 `Box<dyn Error>` 相关主题（或
   `E0382`、`ownership`、`错误传播`）→ 看到选模板/填槽/三重校验的生成
   报告 → 回车进入做题；未配置 API Key 时自动走离线模式（默认填槽）。

## 目录结构

```
agent/                 作业要求与背景（requirements.md 是硬要求 R1-R6 的出处）
docs/                  设计文档（v3 为当前基线，v1/v2 为历史）；选题各版本
exercises/             练习仓 + IDE-only 子 crate（rust-analyzer 分析用，cargo 不编译）
src/main.rs            薄入口
src/cli/               交互 CLI：菜单、做题流、问模型/用量/配置页
src/exercise/          练习发现、标题解析、rustc --test 运行器
src/config/            模型配置加载（config.toml + .env 覆盖，R3）
src/llm/               OpenAI 兼容 chat 客户端 + usage 解析（R1）
src/usage/             token/费用统计、预算拦截、JSON 持久化（R6）
src/verifier/          rustc --json 诊断解析、三重校验、测试失败解析（M2）
src/constraints/       抽象约束静态检查：no-clone 等（M2）
src/taxonomy/          概念图谱加载、校验（无环）、错误码反查索引（M3）
src/template/          模板库加载（TOML）、规则过滤、{{slot}} 填充渲染（M3）
src/generator/         选模板 + LLM 填槽 + 三重校验重试 + 写入并接线（M3）
config.example.toml    配置样例（复制为 config.toml 使用；后者已 gitignore）
templates/             手写题目模板 ×12（TOML 格式，M3+M3.1 错误处理）
taxonomy/              概念图谱 concepts.toml（37 节点，M3+M3.1）
```

`exercises/lib.rs` 用 `#[cfg(rust_analyzer)]` 接线所有种子练习：rust-analyzer
能全量分析，cargo 视其为空 lib，故意写残的模板不影响构建。CLI 直接用
`rustc --test` 编译运行每个练习，与该 crate 无关。`g` 生成的练习落在
`exercises/generated/` 并接线到 gitignored 的 `exercises/lib_generated.rs`
（均为用户本地运行时产物，不入库）。

## 换一个 session 继续开发

1. **读本文件的状态表**，确定下一个里程碑（当前：M4；其后 M4.5）。
2. 读 `docs/设计文档_v3.md`，尤其 §8 的对应里程碑（目标/产出/验收/
   提示要点）与 §8.2 交接纪律。
3. 硬要求对照：`agent/requirements.md` §三（R1–R6）。
4. 开发纪律：一次一个里程碑；先接口与单测后实现；每步 `cargo check`
   绿；完成后更新本表 + git commit（`M<n>: 摘要`）。
5. 滑期先看 v3 §8.1 砍刀预案，降级顺序已排好。

## 文档索引

| 文件 | 说明 |
|---|---|
| `docs/设计文档_v3.md` | **当前设计基线**（双轨锚点 / 评审门 / 交互式复盘 / 约束 / 小片段主义） |
| `docs/模板主题规划.md` | 模板批量扩容 backlog（三层漏斗底座，批次一/二/三主题表） |
| `docs/复盘_M0-M3.md` | 开发复盘：决策得失、踩坑记录、目标对齐自检 |
| `docs/设计文档_v2.md` | 历史版本（对话式诊断教练定位确立） |
| `docs/设计文档_v1.md` | 历史版本（自适应出题器初版） |
| `docs/选题发布版_v2.md` | 已发布到网络学堂的选题帖内容 |
| `agent/requirements.md` | 作业硬要求（R1–R6）、提交物、评分标准、时间节点 |
| `agent/quick-start.md` | 课程给的作业流程方法论 |
