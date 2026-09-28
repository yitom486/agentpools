# agentpools

> **专为 AI Agent（智能体）打造的多轮独占式会话池与调度引擎**  
> 像管理数据库连接一样管理你的本地/云端 Agent，完美支持**多轮交互、本地校验、会话热复用与跨语言调用**。  
> 适用于 Rust、Node.js (`npm`) 与 Python (`pip`)，支持 Windows、Linux 与 macOS 全平台。

---

## 💡 为什么需要 agentpools？

### 传统连接池（如数据库连接池）的尴尬：
传统的连接池（如 HikariCP、r2d2）设计是 **“借出 $\to$ 执行一条 SQL $\to$ 立即归还”**。但 AI Agent 的工作流完全不同：
1. **冷启动极慢**：启动一个 Agent 子进程（Python/Node 环境、加载模型上下文、认证）往往需要几秒甚至十几秒，不能每次问答都重启；
2. **多轮业务闭环**：外部代码向 Agent 提问后，常常需要在本地跑测试、检查代码格式、做业务校验，然后再把报错反馈给**同一个 Agent** 继续修改；
3. **上下文独占**：在整个多轮修改和校验期间，这个 Agent **绝对不能被其他并发任务抢走**，否则对话上下文就串了。

### `agentpools` 的解法：“独占长租约（Lease）”模型
- 🎯 **租用（Acquire）**：任务开始时租下一个 Worker，这个 Worker 及背后的 Agent 进程就**完全属于你**；
- 🔄 **多轮交互（Multi-turn）**：你在两次提问之间，哪怕本地卡住校验 10 秒钟，别人也抢不走这个 Worker；
- ⚡ **热复用（Reuse）**：任务搞定调用 `finish()` 释放后，Agent 进程保持热启动状态，立刻借给下一个任务，零冷启动开销；
- 🛡️ **智能容错**：遇到 API 限流等普通错误保留会话；遇到进程崩溃或断连自动在原槽位透明重建。

---

## 🚀 30 秒极速上手

### 1. Node.js (JavaScript / TypeScript)

```javascript
const { AgentPool } = require('agentpools');

async function main() {
  // 1. 创建池子：配置 4 个 Worker
  const pool = new AgentPool({
    apiVersion: 2,
    maxQueued: 32,
    agents: [
      { runtime: 'codexAppServer', program: 'codex', args: ['app-server'], cwd: process.cwd() },
      { runtime: 'acp', program: 'npx', args: ['@agentclientprotocol/codex-acp'], cwd: process.cwd() }
    ],
  });

  // 2. 租下一个独占 Worker（如果都在忙则进入排队）
  const lease = await pool.acquire();
  try {
    // 3. 第一轮交互
    let response = await lease.ask('请用 Rust 写一个快速排序');
    console.log('Agent 响应:', response.text);

    // 4. 本地执行你的业务校验（期间 Worker 仍然被你独占锁定）
    // while (testFailed(response.text)) {
    //   response = await lease.ask('刚才的代码有编译报错，请修复...');
    // }
  } finally {
    // 5. 任务结束，归还 Worker 供后续任务复用
    await lease.finish();
  }

  // 关闭池子（等待正在运行的任务完成）
  await pool.close({ drain: true });
}

main().catch(console.error);
```

### 2. Python (async/await)

```python
import asyncio
import os
from agentpools import AgentPool

async def main():
    # 支持 async with 上下文管理器，异常自动安全释放 Worker
    async with AgentPool({
        "apiVersion": 2,
        "maxQueued": 32,
        "agents": [{
            "runtime": "codexAppServer",
            "program": "codex",
            "args": ["app-server"],
            "cwd": os.getcwd(),
        }],
    }) as pool:
        async with await pool.acquire() as lease:
            res = await lease.ask("介绍一下 Rust 的所有权机制")
            print(res["text"])
            # 可以在同一个 lease 里继续多轮 ask(...)

asyncio.run(main())
```

### 3. Rust (原生)

```rust
use agentpools::{AgentPool, PoolConfig, ShutdownMode};

let mut pool = AgentPool::new(backend, config, PoolConfig {
    workers: 4,
    max_queued: 128,
})?;

// 1. 取得独占租约
let mut lease = pool.acquire()?;

// 2. 多轮提问与校验环路
let mut res = lease.ask(initial_prompt)?;
while let Some(feedback) = validate_in_business(&res) {
    res = lease.ask(feedback)?;
}

// 3. 归还并优雅关停
lease.finish()?;
pool.shutdown(ShutdownMode::Drain);
```

---

## 🌟 核心特性与架构

```
┌─────────────────────────────────────────────────────────────┐
│  语言绑定 (Node.js / Python)                                 │
│  提供熟悉的 async/await、Promise 与 async with 上下文管理器      │
├─────────────────────────────────────────────────────────────┤
│  多运行时适配层 (crates/agentpools-runtime)                   │
│  统一抹平 ACP、Codex app-server 原生协议与 Pi RPC 的报文差异    │
├─────────────────────────────────────────────────────────────┤
│  协议适配层 (crates/agentpools-acp)                          │
│  实现 ACP 规范，支持独立进程模式与单连接单进程多会话共享模式    │
├─────────────────────────────────────────────────────────────┤
│  跨平台传输底层 (crates/agentpools-transport)                 │
│  处理 Linux/macOS/Windows 进程管道、换行流缓冲与优雅退出强杀    │
├─────────────────────────────────────────────────────────────┤
│  调度内核 (agentpools 根 crate)                              │
│  纯标准库同步原语 (Condvar + Mutex + mpsc)，管理有界队列与租约  │
└─────────────────────────────────────────────────────────────┘
```

1. **多协议开箱支持（API v2）**：
   - **ACP (Agent Client Protocol)**：工业级标准，支持流式内容块与反向权限审批；
   - **Codex app-server**：OpenAI / Codex 原生客户端协议；
   - **Pi RPC**：Pi Agent 的 `--mode rpc` 模式；
   - **混部调度**：同一个池内支持配置不同类型的 Agent，按需分配。
2. **MCP (Model Context Protocol) 统一注入与双向转译**：
   - 支持跨 ACP 与 Codex app-server 的统一工具注入；无论传入数组格式还是字典格式，适配层自动归一化转译为对应协议所需格式，无感挂载。
3. **全平台进程生命周期与真·取消保障**：
   - 传统取消只是在宿主端提前返回，后台子进程依然在疯狂消耗 Token；
   - `agentpools` 在调用取消时，会真正向子进程下发协议级取消信号（Codex 的 `turn/interrupt`、Pi 的 `abort`、ACP 的 `session/cancel`），不浪费 GPU 算力。
4. **支持 ACP 共享进程模式（省内存）**：
   - 默认模式：每个 Worker 启动一个独立进程（进程级绝对隔离）；
   - 共享模式：当 Agent 自身支持多会话时，多个 Worker 可共享同一个长驻进程的 stdio 连接，通过 `sessionId` 多路复用，节省数十倍内存。

---

## 📂 模块导航

| 模块 | 目录 | 职责说明 |
| :--- | :--- | :--- |
| **调度核心** | [`src/`](src/README.md) | 纯粹的调度器，管理线程池、等待队列与租约生命周期（零外部依赖）。 |
| **子 Crate 索引** | [`crates/`](crates/README.md) | 包含底层管道、ACP 协议适配与多运行时适配器的架构索引。 |
| **底层传输** | [`crates/agentpools-transport/`](crates/agentpools-transport/README.md) | 跨平台子进程拉起、换行缓冲与退出强杀保护。 |
| **ACP 适配器** | [`crates/agentpools-acp/`](crates/agentpools-acp/README.md) | ACP v1 协议实现，包含单进程多路复用共享后端。 |
| **多运行时** | [`crates/agentpools-runtime/`](crates/agentpools-runtime/README.md) | 统一 ACP、Codex app-server 与 Pi RPC 的文本消息、真实取消与 MCP 注入。 |
| **Node.js 绑定** | [`bindings/node/`](bindings/node/README.md) | 基于 `napi-rs`，提供完整的 TypeScript 类型定义。 |
| **Python 绑定** | [`bindings/python/`](bindings/python/README.md) | 基于 `PyO3`，支持 `async with` 上下文管理。 |

---

## 📖 深入文档

- 🧭 [多运行时接入设计 (架构设计思路)](docs/runtime-architecture.md)
- 📝 [多运行时配置与使用说明 (API v2 指南)](docs/multi-runtime-usage.md)
- 📊 [运行模式与跨平台测试报告 (共享模式与基准压测)](docs/execution-modes-and-reports.md)
- 📜 [语言绑定接口契约规范](docs/language-api.md)
- 🚀 [构建与跨平台发版指南](docs/release.md)

---

## 🧪 验证与测试

在仓库根目录下运行全套测试：

```bash
# 1. Rust 核心与全适配器测试
cargo test --workspace --all-features --offline
cargo clippy --workspace --all-targets --all-features --offline -- -D warnings

# 2. Node.js 扩展测试
cd bindings/node && npm test

# 3. Python 扩展测试
cd bindings/python && python -m unittest discover -s test

# 4. 运行跨平台（Linux、macOS、Windows）并发与内存监控压测
cargo build -p agentpools-acp --example batch_add --all-features
python examples/measure_batch_add.py --mode mock
```
