# 源码架构

本目录是 `agentpools` 根 crate 的实现。`lib.rs` 是公共 API 入口：它声明内部模块，并重新导出公开类型，因此调用方仍使用 `agentpools::AgentPool`、`agentpools::AgentBackend` 等路径。

| 文件 | 主要内容 | 职责 |
| --- | --- | --- |
| [`lib.rs`](lib.rs) | 模块声明与 `pub use` | 保持稳定的 crate 入口。 |
| [`backend.rs`](backend.rs) | `AgentBackend`、`AgentSession` | 定义创建、执行和关闭会话所需的适配器接口；协议与会话配置由适配器负责。 |
| [`cancellation.rs`](cancellation.rs) | `CancellationToken` | 在线程之间传递协作式取消信号。 |
| [`error.rs`](error.rs) | `BuildError`、`SubmitError`、`TaskError`、`AcquireError` | 区分建池、提交、执行和租用阶段的失败。提交被拒绝时，`SubmitError` 保留原请求。 |
| [`task.rs`](task.rs) | `TaskHandle`、`SubmitResult`、`collect_ordered` | 等待或取消单个任务，并按句柄输入顺序收集结果。 |
| [`lease.rs`](lease.rs) | `LeaseHandle`、`SessionLease`、租用命令 | 让调用方独占一个 worker 和会话，在多次 `ask` 之间进行业务校验。 |
| [`pool.rs`](pool.rs) | `AgentPool`、队列、worker 循环 | 控制并发数和队列容量，分派任务、复用会话、处理重试及关闭。 |

## 请求如何流转

```text
调用方 ── submit ──> 有界队列 ──> 可处理该请求的 worker
                                       │
                                       ├─ 首次使用时 AgentBackend::open
                                       └─ AgentSession::run
调用方 <── TaskHandle::wait <─────────── 结果
```

每个 worker 同时只处理一项工作。成功执行后，它保留会话供后续任务复用；执行失败时，只有适配器确认会话仍可继续，任务才能在同一会话上重试，否则会话会关闭。`submit_to` 将请求交给指定 worker。`acquire` 和 `request_lease` 则将整个 worker 暂时交给 `SessionLease`，直到 `finish` 或丢弃租用对象。

`CancellationToken` 负责传达取消意图；正在执行的协议调用需要由 `AgentSession::run` 配合检查。关闭任务池时，`Drain` 会处理完队列，`CancelPending` 会取消尚未开始的工作；两种模式都会等待正在执行的调用结束。

拆分前的完整单文件版本保存在 [`../archive/lib.rs`](../archive/lib.rs)，仅供对照学习，不参与编译。行为验证见 [`../tests/pool.rs`](../tests/pool.rs)；项目用法和适配器边界见根目录 [`README.md`](../README.md)。
