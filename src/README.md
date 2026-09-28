# 源码架构

本目录是根 crate `agentpools` 的实现。`lib.rs` 声明内部模块并重新导出公共类型；调用方统一从 `agentpools::` 引用它们。

## 核心职责

池管理有界租用队列和固定数量的 worker 线程。一次租用独占一个 worker：调用方可以多次向 Agent 发送请求，并在请求之间校验结果、准备反馈；直到调用 `finish()` 或丢弃 `SessionLease`，worker 才接下一项租用。每个 worker 按需创建自己的会话，并可在后续租用中复用。

池负责调度和会话所有权。协议连接、工具、权限、模型及会话配置由 `AgentBackend` 和 `AgentSession` 的实现负责。ACP stdio 适配器位于 [agentpools-acp](../crates/agentpools-acp/README.md)；多运行时选择及 Codex app-server、Pi RPC 适配器位于 [agentpools-runtime](../crates/agentpools-runtime/src/lib.rs)。

多个不同的后端可以各自建池并同时运行。参见[多运行时接入设计](../docs/runtime-architecture.md)和[双后端示例](../examples/multiple_runtimes.rs)。

## 文件与职责

| 文件 | 主要内容 |
| --- | --- |
| [lib.rs](lib.rs) | 声明模块，导出公共 API。 |
| [backend.rs](backend.rs) | `AgentBackend` 创建会话；`AgentSession` 执行请求、关闭会话，并判断失败后能否复用。 |
| [cancellation.rs](cancellation.rs) | `CancellationToken` 跨线程传递协作式取消信号。 |
| [error.rs](error.rs) | 区分建池、取得租用和执行请求时的错误。 |
| [lease.rs](lease.rs) | `LeaseHandle` 等待排队申请；`SessionLease` 在独占 worker 期间发送请求并释放租用。 |
| [pool.rs](pool.rs) | 有界队列、worker 调度、会话复用、状态查询和关闭。 |

## 对外接口

| 阶段 | 接口 | 用途 |
| --- | --- | --- |
| 接入协议 | `AgentBackend::open`、`AgentSession::{run, close}` | 实现自己的 Agent 适配器；`can_retry_after` 可声明失败后原会话仍可使用。 |
| 创建池 | `AgentPool::new`、`AgentPool::with_agents` | 所有 worker 共享同一配置，或分别指定每个 worker 的 backend 与配置。 |
| 直接取得租用 | `acquire()`、`acquire_to(index)` | 等待某个 worker，或等待指定 worker，返回 `SessionLease`。 |
| 先排队再等待 | `request_lease()`、`request_lease_to(index)` | 立即得到 `LeaseHandle`，随后调用 `wait()`；排队期间可 `cancel()`。 |
| 使用租用 | `SessionLease::ask()`、`ask_with_cancellation()` | 在已租用的 worker 上执行一次请求；同一租用可重复调用。 |
| 归还与管理 | `finish()`、`status()`、`shutdown()` | 释放 worker、查看排队与占用数量、关闭线程池。 |

`PoolConfig` 设置 worker 数量与最大排队数。`BuildError`、`AcquireError`、`TaskError` 分别对应建池、取得租用、会话调用失败。`ShutdownReport` 保存关闭会话的错误和 worker panic 数量。

## 一次任务如何流转

1. 调用方创建 `AgentPool`。`new` 让所有 worker 共享 backend 和配置；`with_agents` 接收每个 worker 各自的配置。
2. 调用 `acquire` 或 `request_lease`。申请进入队列；可处理它的 worker 取出申请，并按需打开会话。
3. 调用方获得 `SessionLease` 后，多次 `ask`、校验和反馈。期间 worker 一直归这次租用使用，其他 worker 可以继续工作。
4. 调用 `finish` 会等待 worker 完成释放；丢弃租用对象也会请求释放。随后该 worker 才能接下一项租用。

```rust,ignore
let mut pool = AgentPool::new(
    backend,
    session_config,
    PoolConfig { workers: 4, max_queued: 128 },
)?;
let mut lease = pool.acquire()?;
let mut response = lease.ask(initial_request)?;
while let Some(feedback) = validate(&response) {
    response = lease.ask(make_follow_up(feedback))?;
}
lease.finish()?;
let report = pool.shutdown(ShutdownMode::Drain);
```

代码中的 `backend`、配置及校验函数由调用方提供。完整的适配器示例见 [mcp_passthrough.rs](../examples/mcp_passthrough.rs)。

## 并发、错误与关闭

`Shared` 中的 `Mutex<Queue<_>>` 只保护入队、出队和关闭标记。worker 使用 `Condvar` 等待新申请；执行 `run_lease` 时不持有队列锁。`status().active` 统计已占用的 worker，因此两次 `ask` 之间的业务校验期间仍计入占用。

`AgentSession::can_retry_after` 只有在确认失败后协议仍同步时才应返回 `true`。可恢复错误保留原会话；状态不明的错误会关闭会话，但当前租用仍占有同一 worker，下一次 `ask` 可在该 worker 打开新会话。

`LeaseHandle::cancel` 用于取消排队申请；正在运行的请求可通过 `ask_with_cancellation` 传入 `CancellationToken`，由适配器协作处理。`shutdown(Drain)` 处理完排队申请，`shutdown(CancelPending)` 取消它们；两者都等待已取得的租用释放。调用关闭前应先 `finish` 或丢弃所有租用，阻塞 I/O 也应由适配器设置有限超时。

拆分前的单文件版本见 [archive/lib.rs](../archive/lib.rs)。已淘汰的直接提交路径及旧测试见 [archive/task-path](../archive/task-path/README.md)，仅供学习，不参与编译。

当前调度与独占行为的测试见 [tests/pool.rs](../tests/pool.rs)；ACP 协议及同会话重试测试见 [stdio.rs](../crates/agentpools-acp/tests/stdio.rs)。
