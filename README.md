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
| M4 | 对话 REPL + Agent 工具环 + 进度/打断 + 会话历史 | R2/R4/R5 | ✅ 完成 |
| M4.2 | 界面体验打磨：视口重绘清屏策略 + 宽字符折行 + 页面化 | — | ✅ 完成 |
| M4.3 | MD 渲染 + 会话切换/导出 + context_len 接线 + 输入层加固 | R3/R5 | ✅ 完成 |
| M4.5 | 分层出题：模板改编 + 自由生成（同一质量门收口，§7.5） | — | ⬜ **下一个** |
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

启动即进入**对话 REPL**（M4 默认首屏）：直接输入问题 / 贴报错或代码（多行
代码用 ``` 围栏包裹，即两行 ``` 之间的内容合为一条消息）。教练会锚定
错误码与概念、必要时本地编译取证；说"来一道 XX 的题"即可生成可开练的
练习并进入做题。

**回复渲染（M4.3）**：教练回复按 Markdown 渲染（粗体/行内代码/标题/
列表/引用着色，代码块保持原样），按终端宽度折行；管道输出保持源码。
**输入加固（M4.3）**：raw-mode 行编辑器——中文退格一次删净（按显示
宽度擦除）、方向键等转义序列不进内容、非 UTF-8 终端输入只提示不退出
（修复"输入中文问题直接退出"与"退格要删两次"）；Ctrl-C 取消本行；
Ctrl-D 退出。

**界面模式（M4.2）**：默认**视口重绘**——每回合开始清一次屏幕视口
（仅 `ESC[2J`，终端回滚缓冲区原样保留，随时上滚可查历史），再渲染
页眉（会话 · 模型 · 累计花费/预算）与最近几条对话摘要（dim），窗口
永远只呈现当前语境，不被旧输出淹没；`/ui scroll` 可切回纯滚动，
`/clear` 手动清屏（`/clear all` 连回滚缓冲区一起清）。中文/emoji 按
显示宽度折行，代码块内不折行。管道输出（测试/录制）自动零 ANSI。

REPL 命令（斜杠命令，输错有就近提示）：

```
  /new 新会话  /clear 清屏  /ui 界面模式  /topics 概念图谱  /practice 做题
  /generate 出题  /usage 用量  /config 配置  /sessions 会话轨迹  /help /exit
```

- **做题子模式**（`/practice` 或对话出题后进入）：页面化呈现（标题 +
  `[████░░] 5/12` 进度条），`<数字>` 选题 / `r` 重跑 / `e` 编辑（编辑器
  解析链：$EDITOR → $VISUAL → config `editor` → 自动探测 `code --wait`
  → vi，`/config` 可改；编辑器返回后自动重绘并重编译）/ `n` 下一题 /
  `v` 全部验证（✓/✗ 着色）/ `b` 返回对话
- **Agent 工具环**：教练可调用三个本地工具——`check_code`（rustc 真实
  诊断取证）、`generate_exercise`（M3 生成管线 + 三重校验，出题后可直接
  开练）、`list_concepts`（概念图谱查询）；累计花费达到预算会被拦截
- **进度与打断（R4）**：LLM 调用/生成/本地编译显示实时 spinner（含
  耗时与"第 n/3 轮"进度），Ctrl-C 随时打断回合回到输入提示
- **会话历史（R5）**：每回合（用户输入 / 教练回复 / 工具调用与结果）
  落盘 `~/.rustlings_adaptive/sessions/`，重启自动恢复上次会话；
  `/sessions <序号>` 回看完整轨迹，`/sessions load <序号>` **切换到
  历史会话继续对话**，`/sessions export <序号>` 导出为 Markdown 轨迹
- `/usage`：本次会话与历史累计的调用次数 / token / 花费 / 预算余量
  （明细持久化在 `~/.rustlings_adaptive/usage.json`）
- `/generate [主题]`：离线可用的直接出题（未配置 Key 自动默认填槽）；
  覆盖不足的主题由 M4.5"分层出题"承接

## 演示用例

1. **对话诊断与计费（R1/R2/R6）**：`cargo run` → 直接输入"用一句话解释
   什么是所有权"→ 看到回复 + "本回合: N 次调用 ｜ 输入/输出 tok ｜
   花费 $x ｜ 累计 $y"；
2. **工具环取证（M4）**：贴一段会报 E0382 的代码（``` 围栏多行粘贴）→
   教练调用 `check_code` 本地编译 → 基于真实诊断解释（轨迹行
   "· 本地编译失败（E0382）"可见）；
3. **对话出题并开练（M3/M4）**：输入"出一道 E0382 相关的练习"→ spinner
   显示"生成练习：填槽+校验（第 n/3 轮）"→ 生成报告 + 回车进入做题 →
   `e` 编辑补全 → `r` 重跑 → 通过后自动标记，`b` 返回对话；
4. **会话轨迹回看（R5）**：`/sessions` 列表 → `/sessions <序号>` 回看
   该会话的完整轨迹（用户/教练/工具调用与结果）；重启后自动恢复上次
   会话；
5. **进度与打断（R4）**：出题或问答进行中观察 spinner 的实时状态与
   耗时 → Ctrl-C 打断回合，立即回到输入提示；
6. **预算中断（R6）**：把 config.toml 的 `[budget].usd` 改成一个比累计
   花费小的数（或 `/config` 改）→ 再发消息 → 提示"调用被拦截"；
7. **配置切换（R3）**：`/config` → 选 `2` 换一个 model（如换成
   `deepseek-chat`）→ 再发消息，新模型立即生效；
8. **离线出题（M3）**：未配置 Key 时 `/generate Box<dyn Error>` →
   默认填槽 → 三重校验 → 可立即开练。

## 目录结构

```
agent/                 作业要求与背景（requirements.md 是硬要求 R1-R6 的出处）
docs/                  设计文档（v3 为当前基线，v1/v2 为历史）；选题各版本
exercises/             练习仓 + IDE-only 子 crate（rust-analyzer 分析用，cargo 不编译）
src/main.rs            薄入口
src/cli/               交互 CLI：对话 REPL（默认首屏）、渲染/MD/输入层基建、做题子模式、出题入口
src/agent/             Agent 环：工具注册/调度、会话轨迹落盘与回看（M4）
src/exercise/          练习发现、标题解析、rustc --test 运行器
src/config/            模型配置加载（config.toml + .env 覆盖，R3）
src/llm/               OpenAI 兼容 chat 客户端 + 多轮/tool calling + usage 解析（R1）
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

1. **读本文件的状态表**，确定下一个里程碑（当前：M4.5；其后 M5）。
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
