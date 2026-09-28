# agentpools-codex

Native stdio Codex app-server adapter for [`agentpools`](file:///d:/project/rust/agentpools/README.md).

## 核心特性

- **双运行模式**：
  - **独立模式 (`CodexBackend`)**：每个 Worker 独享一个 `codex app-server` 子进程，进程崩溃互不影响。
  - **单进程多会话共享模式 (`SharedCodexBackend`)**：整个池子只常驻一个 `codex app-server` 后台守护进程，多任务通过独立的 `threadId` 虚拟并发，节省 35%~50% 物理内存并避免子进程频繁启停开销。
- **阅后即焚落盘清理 (`ephemeral: true`，默认开启)**：
  - Codex 默认会将每轮对话历史落盘到 `~/.codex/sessions/*.jsonl`。
  - 在批处理和自动化池化场景下，开启 `ephemeral: true`（默认）会在会话关闭（`close`）时自动向 Codex 发送原生的 `thread/delete` 请求，彻底清除磁盘文件，零垃圾、零隐私泄露。
- **协议级任务中断**：
  - 取消任务时发送原生的 `turn/interrupt` 请求，无需暴力杀进程。
- **动态 MCP 工具注入**：
  - 支持在启动会话时动态注入 MCP 服务器配置，通过 `thread/start.params.config.mcp_servers` 传递给模型。

## 架构

```
               ┌────────────────────────────────────────────────────────┐
               │              AgentPool<SharedCodexBackend>             │
               └──────────────────────────┬─────────────────────────────┘
                                          │
                   ┌──────────────────────┴──────────────────────┐
                   │               SharedCodexProcess            │
                   │       (单进程守护 + 后台 stdout 事件分流)        │
                   └──────┬──────────────────────┬───────────────┘
                          │                      │
                   threadId: th_001       threadId: th_002
                          ▼                      ▼
                   Worker 1 (Session)     Worker 2 (Session)
                          │                      │
                  [close: thread/delete]  [close: thread/delete]
```

## 快速使用

### 1. 独立进程模式 (Standalone)

```rust
use std::time::Duration;
use agentpools::AgentPool;
use agentpools_codex::{CodexBackend, CodexConfig, CodexPrompt};

let config = CodexConfig::new("codex", "/workspace")
    .with_arg("app-server")
    .with_model("o3-mini")
    .with_ephemeral(true); // 默认开启：会话结束自动从磁盘删除记录

let pool = AgentPool::new(CodexBackend, config, 4, 32).unwrap();
let response = pool.execute(CodexPrompt::text("Hello Codex!")).unwrap();
println!("Answer: {}", response.text);
```

### 2. 单进程多会话共享模式 (Shared Multiplexing)

```rust
use agentpools::AgentPool;
use agentpools_codex::{SharedCodexBackend, CodexConfig, CodexPrompt};

let config = CodexConfig::new("codex", "/workspace")
    .with_arg("app-server")
    .with_ephemeral(true);

let backend = SharedCodexBackend::new();
let agents = vec![
    (backend.clone(), config.clone()),
    (backend.clone(), config.clone()),
];

let pool = AgentPool::with_agents(agents, 32).unwrap();
```
