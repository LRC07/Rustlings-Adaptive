# Rustlings-Adaptive

**对话式 Rust 诊断教练** —— 把 rustlings 式"填空小练习"当作对话里的即时诊断工具：

你提问或贴报错 → 教练锚定错误码与知识点（必要时本地 `rustc` 编译取证）→
生成一道经过**本地三重校验**的 10–40 行填空题 → 你在编辑器里补全 →
**解答评审门**（静态检查 + LLM 逻辑评审 + 存疑自动加测）→ **交互式复盘**
（原理解释校核、更优解挑战、双维度对比表）→ 归档进错误画像与错题本，
定向巩固。

核心理念是"小片段主义"：把复杂场景规约到一个个 10–40 行的可控片段，
质量容易把握，便于针对性指导与统计。

## 功能亮点

- **对话即诊断**：贴报错或代码，教练先让本地 `rustc` 说话，再解释——
  不臆测；还能对"如果我改成 X 会怎样"做**假设实验室**（双向编译 + 错误码 diff）。
- **三层出题 + 单一质量门**：模板直配 → 模板改编 → 自由生成，每道题都过
  「能编译 / 参考解全绿 / 未完成模板必失败」三重校验与约束检查；点名具体
  手法（如 entry API）会走**考察点精准匹配**。
- **解答评审门**：测试全绿不代表地道——todo! 残留、约束违例、clippy、
  LLM 逻辑评审三态判定，存疑自动加测验证。
- **交互式复盘**：解释校核（选择题/自由输入）、更优解挑战（只给方向）、
  机器实测 + LLM 四维对比表、下一步练习决策。
- **学习画像**：错误码计数 + 概念 SM-2 间隔复习 + 错题本；同概念反复
  失败会收到主动提示，教练可随时查询画像定向出题。
- **用量与预算**：每次调用按用途（对话/出题/评审/复盘）分项记账，
  实时展示花费，预算到顶自动拦截。
- **多模型 + 分场景路由**：OpenAI 兼容端点皆可（OpenAI / DeepSeek /
  Kimi / 本地 Ollama / vLLM…），`/model` 多档案切换（`new` 交互新建、
  `rm` 确认后删除），思考模式与推理强度按档案可配；`[routing]` 可把
  对话 / 出题 / 复盘评审分别路由到不同模型（对话用快的、复盘用强的，
  账本按各环节实际模型分项记账）。

## 安装与运行

要求：**Rust 1.85+**（edition 2024，`rustup` 一行装好，`rustc` 随之到位，
练习用它编译运行）。系统支持：**Linux**（主要开发与测试平台）、
**macOS**（终端机制一致，预期可用，欢迎反馈）；Windows 暂不支持，
可在 WSL 中完整使用（见文末"已知限制"）。

```bash
git clone https://git.tsinghua.edu.cn/rust-course/2026/agent/agent-lrc25.git rustlings-adaptive
cd rustlings-adaptive
cargo run --release     # 首次编译需几分钟；日常用 cargo run 即可
```

或者装成全局命令：

```bash
cargo install --git https://git.tsinghua.edu.cn/rust-course/2026/agent/agent-lrc25.git
rustlings-adaptive      # 注意：仍需在项目根目录（含 exercises/ 与 templates/）执行
```

## 配置

模型调用需要一个 **OpenAI 兼容的 endpoint 与 API Key**。模型统一写在
`[[models]]` 档案里（每个模型一段，字段含义与默认值见
`config.example.toml`），`active` 指向当前生效的档案。三种方式任选：

1. **配置文件（推荐）**：`cp config.example.toml config.toml`，在第一个
   `[[models]]` 里填好 `endpoint` / `api_key` / `model` 即可；多模型就
   各加一段，`/model <名>` 一键切换（移动 `active` 指针，档案内容永远
   不被覆盖）；`think_mode`（auto/on/off）、`reasoning_effort`、
   `context_len`、`prices` 均按档案独立配置；
2. **环境变量**：项目根目录建 `.env`，写 `RUSTLINGS_API_KEY=sk-...`
   （也可用 `RUSTLINGS_ENDPOINT` / `RUSTLINGS_MODEL` 覆盖，优先级更高）；
3. **程序内配置页**：运行后 `/config`，交互修改 endpoint / model /
   api_key / 预算 / 编辑器 / 思考模式 / 流式输出，修改写回当前档案。

**分功能配置模型（推荐）**：不同环节对模型的要求不同——对话要
响应快、成本低；出题（尤其自由生成）与复盘评审要能力强。在
`config.toml` 的 `[routing]` 表里按环节指定档案（省略的字段沿用
`active`）：

```toml
[routing]
chat = "Qwen3.8-Flash"      # 对话 / 工具环 / 假设实验室
generate = "Kimi-K3"        # 出题（自由生成对模型能力敏感）
review = "Kimi-K3"          # 评审门 + 复盘
```

各环节需要什么样的模型（示例配置已内置推荐档案，可自行替换）：

| 环节 | 需要什么样的模型 | 示例中的推荐 |
|---|---|---|
| 对话 | 响应快、成本低、日常够用 | Qwen3.8-Flash（想讲解更厚可换 MiniMax-M3）|
| 出题 | 能力强：自由生成无模板兜底，题目质量直接决定练习体验（低频环节，贵一点值得）| Kimi-K3（思考 low）|
| 评审门、复盘 | 能力强：评判细致、讲解透彻，可以慢一点 | Kimi-K3（开启思考）|

**校内同学（清华）零成本上手**：
1. 登录 [easycompute.cs.tsinghua.edu.cn](https://easycompute.cs.tsinghua.edu.cn/login)
   领取学校发放的免费 token 额度；
2. 在聚合平台 [ai.paratera.com](https://ai.paratera.com/#/lms/api)
   创建 API Key，复制到 `.env`（或示例配置的 `api_key` 字段）即可
   直接使用示例中的全部模型；
3. 其他来源：任意 OpenAI 兼容端点皆可（DeepSeek / Kimi 官方 /
   Ollama / vLLM…），替换 `endpoint` 与模型 id 即可。


> `config.toml` 与 `.env` 含 API Key，已被 `.gitignore` 排除，请勿提交。
> 预算：累计花费达到 `[budget].usd` 后，后续模型调用被自动拦截。
> **未配置 Key 也能玩**：做题与离线出题（模板直配）全程可用。
> **学习数据隔离**：会话、画像与用量按系统用户存储于
> `~/.rustlings_adaptive/`——不同系统账号互不可见；多人共用一台
> 机器时请使用各自的系统账号。

## 使用

启动即进入**对话 REPL**：直接输入问题 / 贴报错或代码（多行代码直接
粘贴，自动合并为一条消息；Enter 发送，Ctrl+J 换行）。
说"来一道 XX 的题"即可生成可开练的练习。

REPL 命令：

```
/new 新会话   /clear 清屏   /ui 界面模式   /topics 概念图谱
/practice 做题   /generate 出题   /model 档案管理   /usage 用量
/stats 学习画像   /reset 记录重置   /config 配置   /sessions 会话轨迹
/retry 重发上一条   /help /exit
```

- **做题子模式**（`/practice`）：本会话 / 按主题 / 全库三层分区；
  `<数字>` 选题、`e` 编辑（$EDITOR / VS Code 自动探测；VS Code 从
  **项目根目录**打开时，练习处于 rust-analyzer 分析范围内：类型错误
  实时显示，保存时的 cargo check 还会标出借用类错误——未完成的练习
  在问题面板里持续报错属正常现象）、`r` 运行、
  `h` 分级提示、`a` 问教练（代码+状态带回对话）、`f` 反馈难度、
  `v` 全部验证；通过后自动进入**评审门 + 复盘**。
- **进度与打断**：LLM 调用/生成/编译显示实时 spinner（含轮次与耗时），
  Ctrl-C 随时打断。
- **会话轨迹**：每回合落盘，重启自动恢复；`/sessions <n>` 回看、
  `load` 切换、`export` 导出 Markdown、`clear <n>|all` 归档清理。
- **学习记录重置**：`/reset` 把会话/画像/做题状态整体移入归档目录
  （不删除，可找回），从零开始；练习文件与用量账本保留。
- **学习画像**：`/stats` 查看 SM-2 到期复习、概念弱项、高频错误码、
  错题本（`/stats wrong <概念|错误码>` 过滤）。

## 演示用例

1. **对话问答与计费**：`cargo run` → 输入"用一句话解释什么是所有权" →
   回复 + 本回合 token/花费 footer；
2. **工具环取证**：贴一段会报 E0382 的代码（多行直接粘贴）→ 教练本地
   编译 → 基于真实诊断解释；
3. **出题并开练**：输入"出一道 E0382 相关的练习" → 题卡（概念/难度/
   触发语境）→ 回车进入做题 → `e` 编辑补全 → `r` 运行 → 通过后自动进入
   评审门（静态 + LLM 评审）→ 复盘（理解校核 → 更优解挑战 → 对比表）；
4. **考察点精准出题**：先聊某个具体手法（如 entry API），再说
   "我想多练练这个" → 题目正面训练该手法；
5. **假设实验室**：贴代码后问"如果我把它改成 &s 会怎样" → 教练双向
   rustc 取证，报告错误码增减并解读规则；
6. **学习画像**：做几道题后 `/stats` → 到期复习/弱项/错题本；
   问"我哪里薄弱" → 教练查询画像给建议；
7. **会话轨迹**：`/sessions` 列表 → `<n>` 回看完整轨迹（含工具调用）；
8. **进度与打断**：生成中观察 spinner → Ctrl-C 打断，回到输入提示；
9. **预算中断**：把 config.toml 的 `[budget].usd` 改小 → 再发消息 →
   提示"调用被拦截"；
10. **离线出题**：不配 Key 时 `/generate Box<dyn Error>` → 默认填槽 →
    三重校验 → 可立即开练。

## 项目结构

```
src/cli/        终端交互：对话 REPL、渲染、做题子模式、评审门+复盘交互
src/agent/      Agent 工具环（对话教练可调用本地工具）、会话轨迹
src/generator/  三层出题管线 + 单一质量门
src/review/     解答评审门逻辑（静态层 / LLM 评审 / 探测测试）
src/profile/    学习画像（错误码 + 概念 SM-2 + 错题本）
src/borrowlab/  假设实验室（双向编译取证的错误码 diff）
src/verifier/   rustc --json 诊断解析、三重校验
src/template/   练习模板库（TOML）加载与渲染
src/taxonomy/   概念图谱与错误码反查
src/llm/        OpenAI 兼容客户端
src/exercise/   练习发现、运行、题目索引
src/config/     模型配置加载
src/usage/      token/费用统计与预算拦截
templates/      手写练习模板 ×60（TOML，覆盖并发/测试/转换等课件主题）
taxonomy/       概念图谱定义
exercises/      练习仓（你生成的题落在 generated/，自动接线进 IDE）
```

## 测试

```bash
cargo test    # 完整测试套件（单元测试 + 端到端冒烟）
```

题库自洽由测试保证：对每个练习模板的候选变体执行真实的 rustc
编译与运行验证。

## 已知限制

- **Windows**：终端输入层依赖 unix 专属 API，暂不支持；WSL 中可完整使用。
- **模型端点差异**：分层出题的自由生成长输出对端点吞吐敏感；慢端点
  可调高 `llm_timeout_secs`，或 `/model` 切换更快的模型。
- 练习代码经本地 `rustc` 编译执行，与普通本地开发等同；请勿把服务
  暴露给不受信任的网络环境。
