# AGENTS.md

本仓库是课程 AI Agent 大作业：Rustlings-Adaptive（对话式 Rust 诊断教练）。

## 隐私红线（最高优先级）

- **不要读取、打印、复制或提交 `config.toml` 与 `.env`**：它们含用户的
  API Key 等敏感信息。需要了解配置结构时看 `config.example.toml`；
  需要修改配置时让用户本人操作
- `~/.rustlings_adaptive/` 下是用户本地数据（用量、会话历史），同样不要读取
- **不要读取 `sessions/`**：这里是用户的**开发过程记录**（与 AI 的开发
  对话，比如当前这段），不是程序的测试数据；**一律不得 commit**
  （已在 .gitignore）
- **查看程序的测试记录（对话轨迹）用程序本身**：`cargo run` 后用
  `/sessions` 列表 / `/sessions <序号>` 回看 / `/sessions export <序号>`
  导出，**不要直接读 `~/.rustlings_adaptive/sessions/` 下的 JSON**；
  需要离线、无副作用地查询时可用管道驱动（如
  `printf '/sessions\n/exit\n' | cargo run -q`），且不得借此触发 LLM 调用

## 开始任何工作前

1. 读 `README.md` —— 当前开发状态表与"换一个 session 继续开发"一节
2. 读 `docs/设计文档_v3.md` —— 设计基线；动手前看 §8 对应里程碑与 §8.2 交接纪律
3. 作业硬要求 R1–R6：`agent/requirements.md` §三

## 约定

- 一次只做一个里程碑，不跨里程碑；先接口与单测，后实现
- 每步保持 `cargo check` / `cargo test` 通过；milestone 完成后更新
  README 状态表并 git commit（格式 `M<n>: 摘要`）
- 实现与设计文档冲突时，同步更新 `docs/设计文档_v3.md`
- `exercises/` 下的练习经 `rustc --test` 验证；不要提交 `target/` 与
  `exercises/.progress`（已在 .gitignore）
- 与用户交流、文档、CLI 文案均用中文；代码注释按仓库现状用英文
