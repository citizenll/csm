# CSM

Codex Session Manager

[English README](README.md)

CSM 是一个严格基于 Codex 源码语义的 CLI / TUI 工具，用于查看、修复、
迁移 Codex 会话与 rollout 文件。它直接面向真实的会话文件与配置状态，同时在
压缩、回滚、fork、provider 迁移等关键操作上保持 Codex 原生行为。

直接运行二进制而不带参数时，会进入交互式 TUI。

## 仓库信息

- 仓库名
  - `csm`
- 项目全称
  - `Codex Session Manager`
- 推荐 GitHub 描述
  - `Source-backed CLI and TUI for inspecting, repairing, and migrating Codex sessions.`
- 推荐 Topics
  - `codex`
  - `codex-session-manager`
  - `session-manager`
  - `cli`
  - `tui`
  - `jsonl`
  - `migration`
  - `conversation-repair`

## 项目概览

CSM 面向需要低层控制 Codex 会话状态的开发者和运维场景。它提供线程浏览、
rollout 摘要、精确 JSONL 修复、原生 Codex 线程操作，以及一个更高层的
`smart` 切换流程，用于在 provider 和 model 之间安全切换线程。

这个项目不会伪造隐藏线程状态，也不会通过粗暴重写历史来“假装迁移成功”。
需要保持 Codex 原生语义的地方，CSM 会直接复用 Codex 自己的运行时逻辑。

这个工具明确建立在 Codex 现有 Rust 内部实现之上，而不是重新猜一套线程行为：

- `ThreadManager::fork_thread`
- `ThreadManager::resume_thread_from_rollout`
- `RolloutRecorder::get_rollout_history`
- `read_session_meta_line`
- `read_repair_rollout_path`
- `append_thread_name`
- `find_thread_path_by_id_str`
- `find_archived_thread_path_by_id_str`
- `find_thread_path_by_name_str`
- `resume_command`
- `ConfigEditsBuilder`

## 为什么用 CSM

- 查看 `$CODEX_HOME` 下真实线程与 rollout 的派生摘要
- 在不手工修改大段 JSONL 的情况下修复会话元数据
- 调用 Codex 原生的 compact、rollback、fork、migrate 等操作
- 在低层原语之上提供更高层的 `smart` 切换工作流

## 作用范围

它直接操作 `$CODEX_HOME` 下真实存在的 Codex rollout 文件，以及 Codex 的配置 / 状态，
但它本身是一个独立二进制工具，不驻留在主 Codex CLI 内部。

## 设计原则

- 严格基于源码
  - 每个操作都映射到真实的 Codex 存储语义或真实的 Codex 运行时 API。
  - 工具不会发明不存在的隐藏线程状态。
- 默认安全的精确修复
  - `rewrite-meta` 只修改第一条 `SessionMeta` 记录。
  - `repair-resume-state` 只修改 resume / fork 启动时会读取的持久化窗口提示。
  - `migrate` 会创建新线程，而不是把旧历史强行改造成不兼容的新运行时形态。
- 面向实际运维
  - 目标既可以用 rollout 路径定位，也可以用 thread id 或 thread name 定位。
  - 命令围绕修复、迁移、压缩、归档、恢复等真实场景设计。

## 代码结构

- `src/lib.rs`
  - 库入口，暴露可复用的 `run(Cli)` 接口。
- `src/main.rs`
  - 薄封装二进制入口，只负责解析 CLI 参数并调用库层。
- `src/commands.rs`
  - 命令编排与输出组织。
- `src/runtime.rs`
  - 运行时 / 配置 / profile 解析，以及 resume 命令相关辅助逻辑。
- `src/summary.rs`
  - 基于 rollout 推导线程摘要信息。
- `src/operations.rs`
  - 调用原生 Codex 线程操作，例如 repair、fork、compact、rollback、archive。
- `src/rollout_edit.rs`
  - 对 JSONL rollout 做原地修复，处理元数据与 resume-state 改写。
- `src/tests.rs`
  - 回归测试，包括 `test/` 目录中的真实 rollout 夹具。

## TUI 交互

- `codex-session-manager`
  - 不带任何子命令时直接进入两级交互式 TUI。
- 主界面
  - 展示当前所有 active + archived 的交互式线程，并按 provider 分组。
  - 标题优先使用 `session_index.jsonl` 中的 thread name；没有标题时，回退到首条用户消息摘要，再回退到 thread id / rollout 文件名。
  - 左侧是按 provider 分组的线程列表，右侧显示当前线程的 provider、归档状态、时间、路径、cwd、preview，以及当前模型、上下文窗口、token 摘要等信息。
  - 运行时摘要采用短暂延迟加载，避免快速滚动时卡住列表。
  - 界面语言会先按系统 locale 自动判断。
- 二级动作界面
  - 在线程上按 `Enter` 进入动作菜单，暴露与 CLI 相同的一组会话操作命令。
  - 需要额外参数的动作会弹出内联输入框，然后仍然走同一套 Clap 解析和命令实现，不会另造一套旁路逻辑。
  - 其中 `smart` 是高层切换向导：列表选择 provider / model，工具自动决定修复或迁移路径。
- 快捷键
  - `↑/↓` 移动，`Enter` 进入动作 / 确认，`Esc` 返回上一级，`r` 刷新线程列表，`F2` 在中英之间手动切换，`q` 退出。

## 命令

- `show`
  - 按 rollout 路径、thread id 或 thread name 解析目标。
  - 输出推导出的会话元数据，或使用 `--json` 输出结构化结果。
- `rename`
  - 更新 `session_index.jsonl` 中保存的线程名称。
- `repair`
  - 基于 rollout 历史重建 SQLite 线程元数据。
- `rewrite-meta`
  - 改写 rollout 文件中的第一条 `SessionMeta`，然后同步修复 SQLite 状态。
- `repair-resume-state`
  - 改写 rollout 内持久化的 `TurnStartedEvent.model_context_window` 与
    `TokenCountEvent.info.model_context_window`。
  - 当旧线程只有在把过大的上下文窗口提示修正后才能继续 resume / fork 时，使用这个命令。
- `fork`
  - 通过 Codex 原生线程管理器 fork 一个新线程，并支持 model / provider /
    context-window 覆盖。
- `archive` / `unarchive`
  - 按 Codex app-server 同样的路径规则，在活跃存储和归档存储之间移动 rollout 文件。
- `copy-session-id` / `copy-cwd` / `copy-rollout-path`
  - 解析目标，并输出 / 复制对应字段。
- `copy-deeplink`
  - 输出 / 复制由 Codex 源码生成的标准 `codex resume ...` 命令。
- `compact`
  - resume 一个线程，提交原生 `Op::Compact`，等待完成后再同步元数据。
- `rollback`
  - resume 一个线程，提交原生 `Op::ThreadRollback`，等待完成后再同步元数据。
- `migrate`
  - 先基于持久化的上下文 token 状态做目标窗口预检；必要时先 compact，再 fork 到新的
    provider / model / profile。
- `smart`
  - 打开一个交互式 provider / model 选择向导。
  - 最后的确认步骤会先显示执行计划预览，再真正执行。
  - provider 列表来自全局 `config.toml` 里的 `model_providers` 和各 profile 引用的 provider。
  - 同 provider 切换时，会按需要自动 compact、修复持久化上下文窗口提示，并写入运行时 profile。
  - 跨 provider 切换时，会自动走迁移路径并带上目标 provider/model/runtime 形态。

## 示例

```powershell
cargo run --
cargo run -- smart 019cd66f-f4ea-7022-802b-7007c11cea97
cargo run -- show 019cd66f-f4ea-7022-802b-7007c11cea97
cargo run -- show "my old thread" --json
cargo run -- rename 019cd66f-f4ea-7022-802b-7007c11cea97 "Provider migration"
cargo run -- repair 019cd66f-f4ea-7022-802b-7007c11cea97
cargo run -- rewrite-meta 019cd66f-f4ea-7022-802b-7007c11cea97 --provider openrouter --cwd D:\Dev\self\project
cargo run -- repair-resume-state 019cd66f-f4ea-7022-802b-7007c11cea97 --context-window 258400
cargo run -- fork 019cd66f-f4ea-7022-802b-7007c11cea97 --provider openrouter --model gpt-5 --context-window 256000 --thread-name "Forked to new provider"
cargo run -- copy-deeplink 019cd66f-f4ea-7022-802b-7007c11cea97
cargo run -- compact 019cd66f-f4ea-7022-802b-7007c11cea97
cargo run -- rollback 019cd66f-f4ea-7022-802b-7007c11cea97 2
cargo run -- migrate 019cd66f-f4ea-7022-802b-7007c11cea97 --provider openrouter --model gpt-5 --context-window 256000 --write-profile openrouter-256k --archive-source
```

## 典型工作流

- Provider 迁移
  - 当线程需要从大上下文 provider 迁移到小上下文 provider 时，用 `migrate`。
  - 工具会先检查持久化上下文大小；如果需要会先 compact，再以目标运行时 fork 新线程。
- Resume-state 修复
  - 当线程必须保留为“同一个线程”，但旧运行时留下的窗口提示会影响 resume / fork 时，
    用 `repair-resume-state`。
  - 只有当你明确希望旧 rollout 头部元数据改成另一个 provider，且又不想创建新线程时，
    才把它和 `rewrite-meta --provider ...` 配合使用。
- 会话修复
  - 当你手工恢复 rollout，或 SQLite 元数据与磁盘文件不一致时，用 `repair`。
  - `rewrite-meta` 只用于 provider / cwd / memory-mode 级别的元数据手术。
- 会话生命周期管理
  - 当迁移后的源线程应该退役时，用 `archive`。
  - 当你要把标准恢复命令交给别的 shell、脚本或同事时，用 `copy-deeplink`。

## 说明

- 当目标 provider / model 的上下文预算与原线程不同，正确迁移路径是 `fork` / `migrate`。
  它们会按 Codex 自己的 fork 语义创建新线程，而不是强改旧线程。
- `repair-resume-state` 是原地修复工具，不是迁移原语。
  它只改写 rollout 事件里的 resume / fork 窗口提示，不会直接改变真实运行时的
  provider / model 选择。
- `migrate` 使用 `last_token_usage.total_tokens` 作为当前上下文规模的预检信号，
  与 Codex core 的上下文统计方式保持一致。
- `rewrite-meta` 明确只做 `SessionMeta` 手术，不伪造、不重写对话历史。
- `repair` 与 `rewrite-meta` 在修改 rollout 后都会同步修复 SQLite 元数据，
  保证 Codex 的索引视图保持一致。
- `copy-deeplink` 复制的是 shell 命令，不是自定义 URI；
  Codex 源码定义的标准恢复入口就是 `codex resume ...`。
- 没有 `mark-unread` 命令，因为 Codex 源码没有持久化 unread / read 标记位。
