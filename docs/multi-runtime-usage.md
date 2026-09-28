# 多运行时使用说明

`agentpools` 的 `apiVersion: 1` 配置保持原有 ACP 用法。`apiVersion: 2` 在每个 `agents` 条目上增加 `runtime`：`acp`、`codexAppServer` 或 `piRpc`。同一个池可以混合这些 worker。`acquire(index)` 中的索引对应 `agents` 数组的位置；租用期间的多次 `ask` 留在同一个 worker 和会话。只有会话状态不明的失败才会关闭旧会话，并在下一次调用时于原 worker 上重建。

调用方需要另行安装并配置对应 Agent。`program` 指向可执行文件，`args` 是启动参数，`cwd` 必须是绝对路径。以下示例假设 `codex` 与 `pi` 已在 `PATH` 中，且各自已完成所需的认证和模型配置。Windows 上可将 Pi 的 `program` 指向 npm 安装生成的 `pi.cmd` 绝对路径。

当前原生适配器支持文本内容块和最终文本结果；`ask()` 返回 `{ text, stopReason }`。ACP 仍保留其原有内容块和 MCP 配置。Codex app-server 使用 `initialize`、`thread/start`、`turn/start` 和 `turn/completed`；Pi 使用 `--mode rpc` 的 JSONL `prompt`、消息事件和 `agent_settled`。原生适配器不会处理交互式审批或扩展 UI 请求：Codex 会话启动时指定 `approvalPolicy: "never"`，意外收到宿主交互请求会返回错误并关闭会话。为阻塞 I/O 设置有限的 `timeouts.handshakeMs` 和 `timeouts.promptMs`；默认为 30 秒和 300 秒。

## Node.js

在仓库的 `bindings/node` 中运行 `npm install`、`npm run build`，或安装已构建的 npm 包。

```js
const { AgentPool } = require('agentpools')

async function main() {
  const pool = new AgentPool({
    apiVersion: 2,
    maxQueued: 16,
    agents: [
      { runtime: 'codexAppServer', program: 'codex', args: ['app-server'], cwd: process.cwd() },
      { runtime: 'piRpc', program: 'pi', args: ['--mode', 'rpc', '--no-session'], cwd: process.cwd() },
    ],
  })
  try {
    for (const index of [0, 1]) {
      const lease = await pool.acquire(index)
      try {
        const first = await lease.ask('请简单介绍你自己。')
        const second = await lease.ask('请接着上一轮，补充一句。')
        console.log(index, first.text, second.text)
      } finally {
        await lease.finish()
      }
    }
  } finally {
    await pool.close({ drain: true })
  }
}

main().catch(console.error)
```

## Python

在仓库的 `bindings/python` 中运行 `maturin develop`，或安装已构建的 wheel。

```python
import asyncio
import os
from agentpools import AgentPool


async def main():
    async with AgentPool({
        "apiVersion": 2,
        "maxQueued": 16,
        "agents": [
            {"runtime": "codexAppServer", "program": "codex", "args": ["app-server"], "cwd": os.getcwd()},
            {"runtime": "piRpc", "program": "pi", "args": ["--mode", "rpc", "--no-session"], "cwd": os.getcwd()},
        ],
    }) as pool:
        for index in (0, 1):
            async with await pool.acquire(index) as lease:
                first = await lease.ask("请简单介绍你自己。")
                second = await lease.ask("请接着上一轮，补充一句。")
                print(index, first["text"], second["text"])


asyncio.run(main())
```

## ACP 与混合配置

在 API v2 中，ACP worker 的字段沿用 API v1，只增加 `runtime: "acp"`：

```json
{
  "apiVersion": 2,
  "agents": [
    { "runtime": "acp", "program": "codex-acp", "args": [], "cwd": "/absolute/project" },
    { "runtime": "codexAppServer", "program": "codex", "args": ["app-server"], "cwd": "/absolute/project" },
    { "runtime": "piRpc", "program": "pi", "args": ["--mode", "rpc"], "cwd": "/absolute/project" }
  ]
}
```

原来的无 `runtime`、`apiVersion: 1` ACP 配置继续可用。原生适配器目前只接收 `content` 中的文本块；图片等非文本内容会得到明确错误，且不会发送到 Agent。`model` 可用于 Codex app-server；Pi 的模型通过启动参数 `--model` 指定。
