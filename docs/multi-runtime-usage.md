# 多运行时（API v2）使用完全指南

> 本指南介绍如何在同一个 Agent 进程池中混合调度 **Codex app-server**、**Pi RPC** 和 **ACP**，并在 Node.js、Python 及 Rust 中使用统一的数据结构与 MCP 工具。

---

## 1. 核心概念与版本演进

- **`apiVersion: 1`（ACP 专有模式）**：适用于全量采用 ACP stdio 协议的场景，每个 Worker 独占一个 ACP 进程与会话。
- **`apiVersion: 2`（多运行时统一模式）**：
  - 允许在配置数组的每个 agent 节点上标注 `runtime` 字段（`codexAppServer`、`piRpc`、`acp`）；
  - **支持同一池内异构混合**：Worker 0 可以是 Codex，Worker 1 可以是 Pi，Worker 2 可以是 ACP；
  - **支持统一 MCP 工具注入**：用一套通用的 `mcpServers` 配置，底层自动转译适配；
  - **支持定向获取**：`acquire(index)` 精确租借指定位置的 Worker。

---

## 2. 统一配置参考格式

```json
{
  "apiVersion": 2,
  "maxQueued": 32,
  "agents": [
    {
      "runtime": "codexAppServer",
      "program": "codex",
      "args": ["app-server"],
      "cwd": "C:/work/project",
      "model": "gpt-6-luna",
      "mcpServers": [
        {
          "name": "sqlite",
          "command": "uvx",
          "args": ["mcp-server-sqlite", "--db-path", "./app.db"]
        }
      ]
    },
    {
      "runtime": "piRpc",
      "program": "pi",
      "args": ["--mode", "rpc", "--model", "gemini-2.5-flash"],
      "cwd": "C:/work/project"
    },
    {
      "runtime": "acp",
      "program": "npx",
      "args": ["-y", "@agentclientprotocol/codex-acp"],
      "cwd": "C:/work/project"
    }
  ]
}
```

> [!NOTE]
> `cwd` 必须为绝对路径。Windows 环境下，如果某些 CLI 是通过 npm 全局安装的命令（如 `pi.cmd`），建议直接传入可解析的命令名或全路径。

---

## 3. Node.js 完整接入示例

安装与依赖：在项目根目录引用预编译好的 `agentpools` 包。

```javascript
const { AgentPool } = require('agentpools');

async function run() {
  // 1. 初始化多运行时池
  const pool = new AgentPool({
    apiVersion: 2,
    maxQueued: 16,
    agents: [
      {
        runtime: 'codexAppServer',
        program: 'codex',
        args: ['app-server'],
        cwd: process.cwd(),
        mcpServers: [
          { name: 'sqlite', command: 'uvx', args: ['mcp-server-sqlite'] }
        ]
      },
      {
        runtime: 'piRpc',
        program: 'pi',
        args: ['--mode', 'rpc'],
        cwd: process.cwd()
      }
    ]
  });

  try {
    // 2. 独占获取第一个 Worker（Codex）
    const lease = await pool.acquire(0);
    try {
      // 连续多轮交互，上下文保持连贯
      const reply1 = await lease.ask('请设计一个简单的数据库表结构。');
      console.log('第一轮回复:', reply1.text);

      const reply2 = await lease.ask('根据上面的表结构，写一个插入测试数据的 SQL。');
      console.log('第二轮回复:', reply2.text);
    } finally {
      // 必须显式释放，Worker 才能接待下一个排队任务
      await lease.finish();
    }
  } finally {
    // 优雅停机：等待排队请求完成后退出
    await pool.close({ drain: true });
  }
}

run().catch(console.error);
```

---

## 4. Python 完整接入示例

基于 Python 的 `async with` 上下文管理器，资源的释放更加优雅与自动化：

```python
import asyncio
import os
from agentpools import AgentPool

async def main():
    config = {
        "apiVersion": 2,
        "maxQueued": 16,
        "agents": [
            {
                "runtime": "codexAppServer",
                "program": "codex",
                "args": ["app-server"],
                "cwd": os.getcwd(),
                "mcpServers": [
                    {"name": "sqlite", "command": "uvx", "args": ["mcp-server-sqlite"]}
                ]
            },
            {
                "runtime": "piRpc",
                "program": "pi",
                "args": ["--mode", "rpc"],
                "cwd": os.getcwd()
            }
        ]
    }

    # 1. 启动 Agent 进程池
    async with AgentPool(config) as pool:
        # 2. 定向获取第二个 Worker（Pi）
        async with await pool.acquire(1) as lease:
            reply = await lease.ask("请简要介绍你自己以及擅长的工作。")
            print("Pi Agent 回复:", reply["text"])
            
            # 多轮对话保持同一会话状态
            follow_up = await lease.ask("请用一句话总结。")
            print("总结:", follow_up["text"])

asyncio.run(main())
```

---

## 5. 错误处理与重试语义

- **`Remote` 错误（如模型限流 429）**：
  - 底层 stdio 管道仍保持正常同步，调用方**无需重新 acquire**，直接在原 lease 上继续发送下一次 `ask` 即可重试，不会丢掉历史上下文。
- **不可恢复错误（如子进程崩溃）**：
  - 适配器会自动关闭旧管道，但当前 Worker 依然为该租用保留；下一次 `ask` 会自动触发安全冷启动，重新建立新会话，防止排队的其他任务插队。
