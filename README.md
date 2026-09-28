# agentpools

一个独立的 Rust Agent 会话池。每个任务通过租用独占一个 worker，涵盖多轮请求与调用方校验，直到任务完成并释放。池支持有界队列、取消和关闭清理。旧的直接提交接口已归档。

## 源码架构

根 crate 的模块划分、各文件职责和请求流转过程见 [src/README.md](src/README.md)。拆分前的单文件版本保存在 [archive/lib.rs](archive/lib.rs)，供对照学习。

## 边界

API v1 保持 ACP 配置；API v2 可按 worker 选择 ACP、Codex app-server 或 Pi RPC。见[多运行时使用说明](docs/multi-runtime-usage.md)与[接入设计](docs/runtime-architecture.md)。

`agentpools` 只负责调度与会话所有权。调用方实现 `AgentBackend::open` 和 `AgentSession::{run, close}`，从而接入 ACP、HTTP API、本地进程或其他 Agent 协议。

**MCP 注入是透传的。** `AgentBackend::Config` 是适配器自定义的类型；池不会读取、修改或合并其中的 MCP server 列表、工具权限、凭据和模型设置。适配器在 `open` 中把调用方提供的配置传给实际 Agent。若某个协议不支持 MCP，适配器可以使用它自己的工具机制，或明确返回能力不支持的错误。可运行示例见 [`examples/mcp_passthrough.rs`](examples/mcp_passthrough.rs)。

任务开始时调用 acquire 或 request_lease，得到 SessionLease。租用期间可以多次 ask，并在两次调用之间校验、生成反馈；worker 始终属于这个任务。调用 finish 或丢弃租用后，该 worker 才会接下一项工作。其他 worker 可以并行处理自己的任务。

适配器仅在 can_retry_after 确认协议仍同步时允许同一会话继续。状态不明的错误关闭会话；租用仍占有原 worker，下一次 ask 会新建会话。

## ACP 适配器：crates/agentpools-acp

[`crates/agentpools-acp/`](crates/agentpools-acp/README.md) 是通用池与 ACP v1 stdio Agent 之间的适配层。它实现根 crate 的 `AgentBackend` / `AgentSession`：按需启动 Agent 进程，完成初始化、认证和会话创建，通过 JSON-RPC 发送 prompt、接收更新与结果，并处理取消、超时和会话关闭。根 crate 不依赖 ACP；Node.js 和 Python 的 API v1 通过多运行时入口使用这个适配层。

| 位置 | 主要内容 |
| --- | --- |
| [src/lib.rs](crates/agentpools-acp/src/lib.rs) | `AcpBackend`、`AcpConfig`、`AcpPrompt`、`AcpResponse`、`AcpSession` 和可选的宿主请求处理器。 |
| [src/transport.rs](crates/agentpools-acp/src/transport.rs) | ACP 子进程的 stdio 传输（基于 [agentpools-transport](crates/agentpools-transport/README.md)）。 |
| [src/shared_process.rs](crates/agentpools-acp/src/shared_process.rs) | 在 Agent 支持时，让多个独立会话共享一个进程。 |
| [src/interop.rs](crates/agentpools-acp/src/interop.rs) | 绑定使用的 `AcpPoolOptions` 等 JSON 配置类型及建池入口。 |
| [stdio 测试](crates/agentpools-acp/tests/stdio.rs)、[Codex 示例](crates/agentpools-acp/examples/codex.rs)、[批量示例](crates/agentpools-acp/examples/batch_add.rs) | 协议测试、真实 Agent 调用和批量任务报告。 |

应用仍需安装并指定 ACP Agent 的可执行入口。此适配层会把调用方提供的 `mcpServers` 配置传给 Agent；具体工具调用由 Agent 完成。详细配置和限制见 [ACP crate README](crates/agentpools-acp/README.md)。

## ACP 的两种运行模式

Rust 调度核心始终为每个 worker 启动一个线程；Agent 子进程的数量由 ACP 适配器决定：

| 模式 | Rust 入口 | 4 个 worker 均已启用时的 ACP Agent 子进程 / 会话 | 适用情况 |
| --- | --- | --- | --- |
| 独立进程（默认） | `AcpBackend`、`AcpPoolOptions::build()` | 4 个进程 / 4 个会话 | 每个 Agent 需要独立的进程环境，或尚未验证单进程并发会话能力 |
| 共享进程（显式选择） | `SharedAcpBackend`、`AcpPoolOptions::build_shared()` | 1 个进程 / 4 个独立会话 | ACP Agent 明确支持同一连接上的并发会话 |

两种模式都有 4 个 Rust worker 线程，各 worker 同时只运行一个任务。共享模式通过独立 `sessionId` 和请求 ID 路由回复；一个共享 Agent 进程故障会影响其所有会话。这里的进程数只指 ACP Agent 适配器启动的子进程，MCP server 和 Agent 自己派生的进程另计。共享模式要求各槽位的进程启动、认证及客户端能力配置一致；工作目录、MCP 工具和模型仍可按会话配置。Node.js 和 Python 绑定目前只暴露默认的独立进程模式。选择示例、限制和报告读取方法见[运行模式与测试报告](docs/execution-modes-and-reports.md)。

## 使用

```toml
[dependencies]
agentpools = { path = "../agentpools" }
```

实现 AgentBackend 与 AgentSession 后，创建池并在整个任务期间持有租用：

```rust,ignore
let mut pool = AgentPool::new(backend, session_config, PoolConfig {
    workers: 4,
    max_queued: 128,
})?;
let mut lease = pool.acquire()?;
let mut response = lease.ask(initial_request)?;
while let Some(feedback) = validate_in_business_code(&response) {
    response = lease.ask(make_correction_request(feedback))?;
}
lease.finish()?;
let report = pool.shutdown(ShutdownMode::Drain);
```

with_agents 可以给每个 worker 不同的 Agent 配置；acquire_to(index) 指定 worker。异步调度方可以先调用 request_lease()/request_lease_to(index)，随后用 wait() 获得 SessionLease。

SessionLease 不负责业务校验规则，只保证 worker 独占。调用 shutdown 前必须先释放所有租用，否则关闭会等待仍被占用的 worker。

## 取消与关闭

- LeaseHandle::cancel() 可取消排队的租用；运行中的 ask 可用 ask_with_cancellation 传入 CancellationToken，由适配器协作取消。
- shutdown(Drain) 处理完队列；shutdown(CancelPending) 取消排队的租用。两者都等待已获得的租用释放。
- 适配器应为阻塞 I/O 设置超时。不同 MCP 工具配置可用不同池，或通过 with_agents 和 acquire_to 指定 worker。

## npm 与 PyPI 接口

Node.js 和 Python 包都封装了 Rust 池与运行时适配器，使用同一套 camelCase 配置。以下是兼容的 ACP API v1；API v2 的 Codex app-server、Pi RPC 与混合配置见[多运行时使用说明](docs/multi-runtime-usage.md)。`apiVersion: 1` 是必填项；`agents` 中每项对应一个 worker，包含 Agent 程序 `program`、可选 `args`、绝对工作目录 `cwd`，以及可选的 `env`、`model`、`mcpServers` 和 `timeouts`。`maxQueued` 限制排队租用数。绑定当前使用默认的独立进程模式；共享进程模式可通过 Rust API 选择。完整字段见 [TypeScript 定义](bindings/node/api.d.ts)和[语言 API 契约](docs/language-api.md)。

| 操作 | npm 包（Node.js） | PyPI 包（Python） |
| --- | --- | --- |
| 创建池 | `new AgentPool(options)` | `AgentPool(options)` |
| 独占 worker | `await pool.acquire(agentIndex?)` | `await pool.acquire(agent_index=None)` |
| 调用 Agent | `await lease.ask(prompt)` | `await lease.ask(prompt)` |
| 查询所选 worker | `lease.agentIndex` | `lease.agent_index` |
| 归还 worker | `await lease.finish()` | `await lease.finish()`，也可用 `async with` |
| 状态与关闭 | `pool.status()`、`await pool.close({ drain: true })` | `pool.status()`、`await pool.close(drain=True)` |

`ask` 接受纯文本或含 `content` 数组的 prompt 对象，返回包含 `text` 和 `stopReason` 的响应。校验结果和发送反馈时继续使用同一个 `lease`；`finish` 后 worker 才接下一项租用。直接提交的 `submit` / `Task` 接口已归档。

下面以已安装的 `@agentclientprotocol/codex-acp` 为例。先把它的 `dist/index.js` 绝对路径设为 `CODEX_ACP_ENTRY`，并完成 Agent 所需的认证。**这些包包含绑定和适配器，不包含 Codex ACP 或其他 Agent 可执行程序。**

### Node.js（npm 包）

```js
const { AgentPool } = require('agentpools')

const entry = process.env.CODEX_ACP_ENTRY
if (!entry) throw new Error('set CODEX_ACP_ENTRY')

async function main() {
  const pool = new AgentPool({
    apiVersion: 1,
    maxQueued: 32,
    agents: [{ program: 'node', args: [entry], cwd: process.cwd() }],
  })
  try {
    const lease = await pool.acquire()
    try {
      const response = await lease.ask('Add 2 and 3.')
      console.log(response.text)
    } finally {
      await lease.finish()
    }
    console.log(pool.status())
  } finally {
    await pool.close({ drain: true })
  }
}

main().catch(console.error)
```

### Python（PyPI 包）

```python
import asyncio
import os
from pathlib import Path
from agentpools import AgentPool

async def main():
    async with AgentPool({
        "apiVersion": 1,
        "maxQueued": 32,
        "agents": [{
            "program": "node",
            "args": [os.environ["CODEX_ACP_ENTRY"]],
            "cwd": str(Path.cwd()),
        }],
    }) as pool:
        async with await pool.acquire() as lease:
            response = await lease.ask("Add 2 and 3.")
            print(response["text"])
        print(pool.status())

asyncio.run(main())
```

上述示例适用于本地构建或发布后的包。当前仓库可分别在 [bindings/node](bindings/node/README.md) 执行 `npm install`、`npm run build`，以及在 [bindings/python](bindings/python/README.md) 使用 `maturin develop` 本地构建。公开包的发布由 [docs/release.md](docs/release.md) 所述的手动工作流完成；默认运行只准备产物，不上传到 npm 或 PyPI。

## 验证

运行 `cargo test --workspace --all-features --offline`、`cargo clippy --workspace --all-targets --all-features --offline -- -D warnings`，以及两个语言包的 ACP mock 集成测试。4 worker / 16 道加法题的 mock 与真实 Agent 命令、通过条件、`report.json` / `report.html` 及 Windows 资源采样报告，见[运行模式与测试报告](docs/execution-modes-and-reports.md)。真实 Agent 测试需要认证并会消耗模型额度。
