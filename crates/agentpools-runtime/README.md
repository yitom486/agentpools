# agentpools-runtime

`agentpools-runtime` 为语言绑定和多协议调度方提供统一的 Agent 运行时适配器（API v2）。调度核心 `agentpools` 保持与协议无关，本 crate 负责抹平不同协议的报文差异。

## 支持的运行时

| 运行时标识 | 协议类型 | 会话模型 | 取消与中断 |
| --- | --- | --- | --- |
| `acp` | ACP v1 stdio (JSON-RPC 2.0) | `initialize` -> `session/new` -> `session/prompt` | 发送 `session/cancel` |
| `codexAppServer` | Codex app-server 原生协议 | `initialize` -> `thread/start` -> `turn/start` | 发送 `turn/interrupt` |
| `piRpc` | Pi RPC 模式 (`--mode rpc`) | `get_state` -> `prompt` | 发送 `abort` |

底层跨平台的子进程拉起、换行缓冲解析（CRLF/LF）与退出强杀保障由 [`agentpools-transport`](../agentpools-transport/README.md) 提供。

## 统一数据结构

- **`RuntimePrompt`**：接收 `content` 数组，当前原生适配器统一接受文本块（`RuntimePrompt::text(...)`）。
- **`RuntimeResponse`**：返回统一的文本响应 `{ text: String, stop_reason: String }`。
- **`RuntimeError`**：统一错误枚举，涵盖子进程拉起失败、I/O 故障、超时、取消及 Agent 远端错误（`Remote`）。
- **`McpServer` & `McpTransport`**：统一的 MCP 工具服务器抽象，支持 stdio 子进程与 http/sse 远端端点。

## 统一 MCP 工具注入

无论是 ACP 还是 Codex app-server，均原生支持挂载 MCP（Model Context Protocol）工具。`agentpools-runtime` 提供跨运行时的双向兼容转换：

- **ACP 运行时**：自动将 MCP 配置转换为 `session/new` 的 `params.mcpServers` 列表。
- **Codex app-server**：自动将 MCP 配置转换为 `thread/start` 的 `params.config.mcp_servers` 字典，并自动完成名称转义。
- **格式归一化**：在 API v2 配置中，无论是传入数组格式 `[{ "name": "sqlite", "command": "uvx" }]` 还是字典格式 `{ "sqlite": { "command": "uvx" } }`，系统均能自动兼容识别并抹平差异。

```rust
use agentpools_runtime::{McpServer, NativeConfig};

// 通过 Rust Builder 声明
let mcp = McpServer::stdio("sqlite", "uvx")
    .with_arg("mcp-server-sqlite")
    .with_env("SQLITE_PATH", "./app.db");

let codex_config = NativeConfig::codex("codex", "/path/to/project")
    .with_arg("app-server")
    .with_mcp_server(mcp);
```

## 会话恢复与真实取消

- **真实取消下发**：当外部调用方发起协作取消时，适配器不仅在 Rust 本地标记中断，还会向外部子进程下发协议级取消信号（Codex 的 `turn/interrupt`、Pi 的 `abort`、ACP 的 `session/cancel`），避免后台子进程持续消耗 GPU 算力与模型 Token。
- **会话容错与复用（`can_retry_after`）**：远端业务错误（例如 API 429 限流或参数提示错误）发生时，底层的 stdio 连接保持同步，适配器允许在同一个 worker 会话上继续执行下一次问询，避免频繁冷启动和历史上下文丢失。
