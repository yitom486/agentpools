# `agentpools-transport` 跨平台进程与管道底座

> 这里是 `agentpools` 生态的跨平台进程管理与 stdio 通信底座。
>
> 专为解决长生命周期 Agent 子进程（Codex app-server、Pi RPC、ACP Agent 等）在 Windows、macOS 与 Linux 上的**管道阻塞、换行符差异、进程僵尸与强杀保底**而设计。

---

## 1. 为什么需要独立抽离这一层？

在多运行时架构接入过程中，各个 Agent 适配器（ACP、Codex、Pi）都需要拉起子进程并通过标准输入输出（stdio）进行 JSON 报文通信。然而，跨平台子进程通信存在大量极其隐蔽的系统级痛点：

| 痛点场景 | 平台表现 | `agentpools-transport` 的解法 |
| --- | --- | --- |
| **换行符差异** | Windows 输出 `\r\n`，Linux/macOS 输出 `\n`，且某些 Agent 可能输出空行 | 自动剥除回车符，自动跳过空行，严谨解析每一行 JSON。 |
| **主线程读取阻塞** | 若在工作线程直接阻塞读管道，超时和协作取消无法及时响应 | **后台专用线程异步读取** + `mpsc` 通道通信，主线程非阻塞轮询并支持精确超时与取消。 |
| **事件与响应乱序** | Agent 往往在响应最终答案前推送大量流式通知（如 `item/started`） | 内置 **`pending` 乱序事件缓冲队列**，精准按 Request ID 提取响应，通知事件不丢失。 |
| **Windows 终止进程报错** | 在 Windows 上，若子进程已经自然退出，调用 `child.kill()` 会抛出 `InvalidInput` 或 `PermissionDenied` | 在 [`terminate_child`](src/lib.rs) 中抹平平台错误，优雅退出与硬杀双重保底。 |
| **僵尸进程与句柄泄漏** | 如果只关闭管道而没有 `join` 读者线程或 `wait` 退出码，系统资源持续泄漏 | 保证 Reader 线程安全退出并回收句柄，防止长期运行耗尽 OS 资源。 |

通过抽离本 crate，我们在 `agentpools-acp` 和 `agentpools-runtime` 中去除了约 **350 行重复且容易踩坑的低级管道代码**。

---

## 2. 核心组件与 API

### ① 跨平台启动配置：[`ProcessConfig`](src/lib.rs)
统一封装跨平台的启动参数，避免各处手写 `Command::new`：

```rust
use agentpools_transport::ProcessConfig;

let config = ProcessConfig::new("codex", "/path/to/project")
    .inherit_stderr(false); // 避免子进程无关日志污染控制台
```

### ② 专用 JSON-RPC 通信通道：[`JsonProcess`](src/lib.rs)
负责单个子进程的完整 stdio 通信生命周期：

- **`spawn(config)`**：启动子进程，接管标准输入输出，拉起后台行缓冲 Reader 线程。
- **`send(&Value)`**：线程安全序列化为单行 JSON，自动附加 `\n` 并显式 `flush`。
- **`response(id, deadline, cancellation)`**：阻塞等待特定 Request ID 的响应，在此期间到达的通知事件自动暂存至 `pending` 队列。
- **`next_event(deadline, cancellation)`**：优先消费暂存的通知事件，支持协作式 `CancellationToken`。
- **`close()`**：优雅关闭 stdin 触发 EOF，若超时未退出则强制终止。

### ③ 安全进程回收保底：[`terminate_child`](src/lib.rs)
提供生产级的四步安全停机机制：
1. **即时探测**：调用 `child.try_wait()`，若已退出直接进入回收；
2. **软等宽限期**：在给定的 `grace_period` 内轮询，等待进程自然退出；
3. **强制终止（Kill）**：若仍未退出，执行 `child.kill()`，并自动过滤 Windows 平台特有的假报错；
4. **收尾回收**：执行 `child.wait()` 释放僵尸进程，并 `join()` 读者线程。

---

## 3. 典型使用模式

```rust,ignore
use std::time::{Duration, Instant};
use agentpools_transport::{JsonProcess, ProcessConfig};
use serde_json::json;

// 1. 启动子进程通道
let config = ProcessConfig::new("my-agent", "/project");
let mut process = JsonProcess::spawn(&config)?;

// 2. 发送请求并等待指定 ID 的回包
process.send(&json!({"id": 1, "method": "initialize"}))?;
let reply = process.response(1, Instant::now() + Duration::from_secs(5), None)?;

// 3. 读取事件流
let event = process.next_event(Instant::now() + Duration::from_secs(30), &cancellation)?;

// 4. 安全关闭
process.close()?;
```

---

## 4. 依赖与集成

- **依赖项**：仅依赖 `agentpools`（取消令牌）、`serde`、`serde_json`，零多余网络依赖。
- **上层使用者**：
  - [`agentpools-acp`](../agentpools-acp/README.md)：ACP 协议 stdio 传输。
  - [`agentpools-runtime`](../agentpools-runtime/README.md)：Codex app-server 与 Pi RPC 的底层通信。
