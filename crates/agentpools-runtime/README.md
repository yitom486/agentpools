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

## 会话恢复与真实取消

- **真实取消下发**：当外部调用方发起协作取消时，适配器不仅在 Rust 本地标记中断，还会向外部子进程下发协议级取消信号（Codex 的 `turn/interrupt`、Pi 的 `abort`、ACP 的 `session/cancel`），避免后台子进程持续消耗 GPU 算力与模型 Token。
- **会话容错与复用（`can_retry_after`）**：远端业务错误（例如 API 429 限流或参数提示错误）发生时，底层的 stdio 连接保持同步，适配器允许在同一个 worker 会话上继续执行下一次问询，避免频繁冷启动和历史上下文丢失。
