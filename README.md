# agentpools

一个独立的 Rust Agent 会话池与任务调度库。它接收陆续提交的任务，最多同时运行指定数量的 Agent 会话，复用空闲会话，并支持有界队列、取消、按提交顺序收集结果和关闭清理。

## 边界

`agentpools` 只负责调度与会话所有权。调用方实现 `AgentBackend::open` 和 `AgentSession::{run, close}`，从而接入 ACP、HTTP API、本地进程或其他 Agent 协议。

**MCP 注入是透传的。** `AgentBackend::Config` 是适配器自定义的类型；池不会读取、修改或合并其中的 MCP server 列表、工具权限、凭据和模型设置。适配器在 `open` 中把调用方提供的配置传给实际 Agent。若某个协议不支持 MCP，适配器可以使用它自己的工具机制，或明确返回能力不支持的错误。可运行示例见 [`examples/mcp_passthrough.rs`](examples/mcp_passthrough.rs)。

```text
调用方任务 ──submit──> 有界队列 ──> 空闲 worker ──> AgentBackend::open(config)
                                               └──> AgentSession::run(request)
按提交顺序收集 <── TaskHandle::wait() <──────────────┘
```

每个 worker 同时只执行一个任务。`submit_retrying` 把一次提交及其后续重试视为**同一个任务**：失败后先询问适配器当前会话是否可继续，再由调用方生成反馈请求；在最终成功、放弃或取消前，worker 不接收下一个任务。其他空闲 worker 仍可并行处理队列。普通 `submit` 是单次任务；它失败后会关闭状态不确定的会话。池不会擅自重试可能已有副作用的操作。

## 使用

```toml
[dependencies]
agentpools = { path = "../agentpools" }
```

实现 `AgentBackend` 与 `AgentSession` 后，可创建池并提交任务：

```rust,ignore
let mut pool = AgentPool::new(backend, session_config, PoolConfig {
    workers: 4,
    max_queued: 128,
})?;

let handles = batches.into_iter()
    .map(|batch| pool.submit(batch))
    .collect::<Result<Vec<_>, _>>()?;
let results = collect_ordered(handles);
let report = pool.shutdown(ShutdownMode::Drain);
```

`with_agents` 可以为各个 worker 指定不同的 Agent profile / 配置。`submit(request)` 交给任意空闲槽位；`submit_to(index, request)` 指定必须使用某个槽位及其工具配置。不同 Rust 类型的协议适配器可由调用方封装成统一的枚举适配器，共用请求、响应和错误类型。

`submit_retrying(request, max_attempts, next_request)` / `submit_retrying_to(index, ...)` 适用于需要把错误反馈给同一 Agent 的任务。`next_request` 会收到错误和失败次数，返回下一条请求或 `None` 放弃。适配器的 `AgentSession::can_retry_after` 只有确认会话仍可继续时才应返回 `true`；默认是 `false`。传输断开或协议状态不明时，该任务终止，池会关闭会话。

## 取消与关闭

- `TaskHandle::cancel()` 会取消尚未开始的任务；正在执行的任务会收到 `CancellationToken`，适配器负责将其转换成协议的取消操作。调用方仍需调用 `wait()` 获取最终状态。
- `shutdown(Drain)` 处理完队列；`shutdown(CancelPending)` 取消队列中的任务，两者都会等待正在运行的任务结束，然后关闭会话。
- Agent 适配器必须为阻塞 I/O 设置超时，否则关闭池可能一直等待。丢弃池时会使用 `CancelPending`。
- MCP 配置属于**会话级**配置。若不同任务需要不同的工具集合，应创建不同的池，或使用 `with_agents` 配置不同槽位，并通过 `submit_to` 指定目标；通用 `submit` 不会自行判断工具能力。

## Node.js 与 Python

Rust 调度核心和 [`agentpools-acp`](crates/agentpools-acp/README.md) ACP v1 stdio 适配器可以通过两个语言绑定使用。绑定共享带 `apiVersion` 的 JSON 配置，MCP server 定义原样透传给 ACP `session/new`。接口和重试语义见 [`docs/language-api.md`](docs/language-api.md)。

```js
const { AgentPool } = require('agentpools')

const pool = new AgentPool({
  apiVersion: 1,
  agents: [{ program: 'codex-acp', cwd: process.cwd(), model: 'gpt-6-luna' }],
})
const task = pool.submit('Add 2 and 3.')
const response = await task.result()
await pool.close({ drain: true })
```

```python
from pathlib import Path

from agentpools import AgentPool

async with AgentPool({
    "apiVersion": 1,
    "agents": [{"program": "codex-acp", "cwd": str(Path.cwd()), "model": "gpt-6-luna"}],
}) as pool:
    task = pool.submit("Add 2 and 3.")
    response = await task.result()
```

绑定目前源码可构建；npm 和 PyPI 尚未发布。当前本机已验证 Windows x64 构建，发布还需要在 CI 中构建并验证其余声明的平台目标。Cargo manifest 的许可证和仓库元数据也需要在发布前补齐。

- [Node.js package](bindings/node/README.md)：`npm install`、`npm run build`、`npm test`
- [Python package](bindings/python/README.md)：安装 maturin 后运行 `maturin develop`，集成测试命令见该 README。

## 验证

`cargo test --workspace --all-features --offline`、`cargo clippy --workspace --all-targets --all-features --offline -- -D warnings`，以及两个语言包的 ACP mock 集成测试。
