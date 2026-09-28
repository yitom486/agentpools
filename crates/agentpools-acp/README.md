# agentpools-acp

`agentpools` 的 ACP v1 stdio 适配器。默认情况下，每个池 worker 独占一个 Agent 子进程和会话：按需启动，发送 `initialize`、可选 `authenticate`、`session/new`、`session/prompt`，并在关闭时按能力调用 `session/close`。

若 ACP Agent 的单个进程支持多个并行 session，可以显式选用 `SharedAcpBackend`，或对 JSON 配置调用 `AcpPoolOptions::build_shared()`。共享后端只启动一个 ACP 进程并复用该进程的认证连接；独立的 `sessionId`、请求 ID 路由和事件队列让各 worker 并发工作。各 agent 配置必须使用相同的程序、参数、环境变量、认证方式和客户端能力；`cwd`、`mcp_servers`、模型与会话超时可以各自不同。工作目录隔离仍由调用方提供；共享进程不会替调用方创建 worktree 或沙箱。原有 `AcpBackend` 和 `build()` 行为不变。

两种模式都有每个 worker 一个 Rust 线程。“一个共享进程”只指 ACP Agent 进程；MCP server 与 Agent 派生的子进程另计。拓扑选择、绑定支持范围和报告字段见[运行模式与测试报告](../../docs/execution-modes-and-reports.md)。

调用方提供 `AcpConfig` 的命令、工作目录、环境变量和 `mcp_servers`。MCP 配置作为 `session/new.params.mcpServers` 传给 Agent；默认不向 Agent 声明客户端文件系统或终端能力。需要响应 Agent 发起的权限、文件或终端请求时，调用方实现 `HostRequestHandler` 并显式设置相应的 `client_capabilities`。未配置处理器时，权限请求会取消，其他客户端请求会被拒绝。

## Codex ACP

`@agentclientprotocol/codex-acp` 是 npm 包，但 Rust 只需启动其可执行入口。推荐在产品中安装并固定版本，配置 `program = "node"`，将本地 `dist/index.js` 的绝对路径放入 `args`。这样运行时不需要 `npx` 下载，也不会隐式跟随 `@latest`。也可以把其他 ACP Agent 的可执行文件和参数交给相同的 `AcpConfig`。

设置 `AcpConfig.model` 后，适配器会在 `session/new` 之后发送 `session/set_config_option`，并检查 Agent 回传的 `model.currentValue`。若 Agent 不支持或没有选中要求的模型，会在提交 prompt 前失败。

设置已安装入口后，可运行最小真实会话：

```text
CODEX_ACP_ENTRY=/absolute/path/to/node_modules/@agentclientprotocol/codex-acp/dist/index.js
cargo run -p agentpools-acp --example codex
```

此示例会调用真实 Codex Agent，可能消耗模型额度；需要事先完成该 Agent 的认证。

## MCP 加法器与任务重试

工作区的 `agentpools-mcp-add` 是用[官方 Rust MCP SDK `rmcp`](https://github.com/modelcontextprotocol/rust-sdk) 实现的 stdio 加法器，公开 `add(a, b)` 工具。构建：

```text
cargo build -p agentpools-acp --features mcp-example --bin agentpools-mcp-add
```

将生成的可执行文件绝对路径设为 `AGENTPOOLS_MCP_ADD_BIN`，再运行上面的 Codex 示例，它会把加法器作为 `mcpServers` 注入并要求 Agent 调用 `add`。该真实 Agent 测试需要 Codex 认证。无需认证的确定性集成测试使用 mock ACP Agent 接收同一份 MCP 配置，再通过官方 SDK 客户端实际调用该加法器：

```text
cargo test -p agentpools-acp --all-features
```

mock 的 `recover-on-feedback` 场景先返回 ACP 错误，再接受包含错误反馈的第二条 prompt。调用方持有 SessionLease 并通过两次 ask 在同一个 worker 和 ACP session 内完成；只对完整收到的远端 JSON-RPC 错误允许就地重试，I/O、超时、取消和协议错误会关闭会话。

## 4 个 Agent、16 道加法题

先编译官方 MCP SDK 加法器和 mock ACP Agent：

```powershell
cargo build -p agentpools-acp --all-features --bins
cargo run -p agentpools-acp --example batch_add --all-features -- --mock
```

mock 模式会启动 4 个独立 ACP 进程，将 16 个任务依次入队，并实际调用 MCP 加法器。前 4 个任务分别指定给 4 个 worker，以确认每条通道都被启用。每次运行结束后，程序会自动在新的 `target/agentpools-batch-add/mock-<时间戳>/` 目录写入 `report.json` 和可直接打开的中文 `report.html`；即使任务失败，也会先保存报告再返回错误。报告包含汇总、按 Agent/PID 分组的时间线及每道题的结果，无需手工整理数据。

完成 `codex login` 后，可用同一程序运行真实 Codex ACP：

```powershell
$env:CODEX_ACP_ENTRY = 'C:\absolute\path\to\node_modules\@agentclientprotocol\codex-acp\dist\index.js'
cargo run -p agentpools-acp --example batch_add --all-features -- --codex
cargo run -p agentpools-acp --example batch_add --all-features -- --codex-shared
```

真实模式要求每个会话选中 `gpt-6-luna`，并核对 16 次结果和对应的 MCP 调用记录；模型未被选中时立即报错。它会消耗真实模型额度。两种真实模式每次运行都自动生成报告，分别写到新的 `target/agentpools-batch-add/codex-<时间戳>/` 和 `target/agentpools-batch-add/codex-shared-<时间戳>/` 目录。报告还记录整数格式达标数；示例退出码目前只检查 ACP 进程数、数值正确数和 MCP 调用数。

若要比较 4 个 ACP 进程与 1 个共享 ACP 进程，在完成 `cargo build -p agentpools-acp --example batch_add --all-features` 后，从**已经运行 `codex login` 的同一个 Windows 用户会话**执行：

```powershell
$env:CODEX_ACP_ENTRY = 'C:\absolute\path\to\node_modules\@agentclientprotocol\codex-acp\dist\index.js'
.\examples\measure_batch_add.ps1 -Mode codex
.\examples\measure_batch_add.ps1 -Mode codex-shared
```

采样脚本默认从当前用户的 Codex App 安装目录动态查找 `codex.exe`，升级后无需修改带版本编号的路径；需要指定其他 CLI 时可设置 `CODEX_PATH`。脚本先检查该 CLI 的 `codex login status`，再运行真实 ACP，并将耗时、进程数、工作集、私有内存及 CPU 采样写入 `target/agentpools-batch-add-resource/`。Codex 工具的受限执行环境可能使用另一 Windows 身份；即使它能读取同一 `auth.json`，也不代表它能复用用户登录态。认证失败时应在实际运行 Agent 的身份下检查 `whoami` 与 `codex login status`，不应复制登录令牌来规避身份差异。

## 当前兼容范围

- 支持 ACP v1 stdio 的基础会话和文本回复；拒绝不兼容的协议版本。
- MCP server JSON 透传；具体 stdio/HTTP 工具能力仍由 Agent 宣告和处理。
- `HostRequestHandler` 接管 Agent→Client 的权限及工具请求；库不会自动授予权限。
- 运行中的取消发出 `session/cancel`；独立进程模式关闭会话后终止其子进程，共享模式只关闭对应 session，进程由共享 backend 生命周期统一回收。
- 对不同 Agent 的认证方式、扩展方法、平台进程树清理与富媒体回复，还需要逐个验证。尤其 Windows 上多层子进程的完整回收尚未实现，不应据此声称支持所有 ACP Agent。

协议黑盒测试运行：`cargo test -p agentpools-acp --features test-support`。
