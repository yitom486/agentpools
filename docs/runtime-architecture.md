# 多运行时接入设计

## 当前边界

`agentpools` 是调度核心：`AgentPool<B>` 独占、复用和关闭 worker 会话；`B: AgentBackend` 决定实际运行时的配置、请求、响应和错误类型。核心不依赖 ACP。`agentpools-acp` 是已经实现的 ACP stdio 适配器。Node.js 和 Python 的 API v1 使用 ACP 配置；API v2 使用 `AgentPool<RuntimeBackend>`，可在同一池中按 worker 选择协议。

| 接入方式 | Rust 核心 | 现成适配器 | Node.js / Python |
| --- | --- | --- | --- |
| ACP stdio | 可接入 | `agentpools-acp` | 支持 |
| Codex app-server 原生协议 | 已接入 | `agentpools-runtime` | API v2 支持文本请求 |
| Pi 原生 RPC | 已接入 | `agentpools-runtime` | API v2 支持文本请求 |

运行时和协议是两个不同概念。一个运行时如果提供符合 ACP 的入口，可以经 ACP 适配器使用；使用其原生接口则需要对应的原生适配器。不能仅把 ACP 配置中的 `program` 改成另一种协议的程序。

## 实现方式

1. `agentpools-runtime` 将 ACP、Codex app-server 和 Pi RPC 包装为统一的 `RuntimeBackend`；进程启动、握手、事件读取、取消和超时留在适配器内。
2. 纯 Rust 调用方仍可为不同后端分别建池并保留各自的请求、响应类型；绑定的 API v2 使用统一文本请求与结果类型，在一个池中混合多个运行时。可运行的独立池示例见 [`multiple_runtimes.rs`](../examples/multiple_runtimes.rs)。
3. 对外语言绑定使用带运行时标识的 API v2 配置；API v1 的 ACP 配置语义保持不变。具体字段和示例见[多运行时使用说明](multi-runtime-usage.md)。

`with_agents` 在同一个后端类型下为各 worker 提供不同配置。`RuntimeBackend` 正是统一 ACP、Codex 和 Pi 的枚举适配器，因此 API v2 可以在同一个池中混合运行时；worker 索引对应 `agents` 数组的位置。如果未来需要暴露某个原生协议的完整请求、响应类型，可以再提供独立的类型化适配器。

## 适配器需要遵守的语义

- `open` 创建一个可复用的会话；同一次租用的多次 `ask` 由同一个 worker 执行。运行时自身可以有不同的进程与会话关系，但适配器必须明确管理它们。
- `run` 应处理协议消息、完成事件、超时和协作取消。等待 I/O 时不能无限阻塞池的关闭。
- `can_retry_after` 只在**同一会话仍可安全继续**时返回 `true`。返回 `false` 后，池会关闭旧会话；租用仍占着原 worker，但下一次 `ask` 会打开新会话。因此“同一 worker”不自动等于“同一会话”。
- 原生协议的审批、工具、流式事件和特殊内容不一定能无损统一。跨运行时接口可以先统一最基本的文本请求与结果，再为专有能力提供明确的配置或事件接口。

接入每个新运行时时，应通过故障注入验证同会话重试、会话失效后重建、取消和关闭；还要确认一个租用进行多轮请求和业务校验期间，其他任务无法取得其 worker。
