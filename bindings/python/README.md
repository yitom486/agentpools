# `agentpools` Python 客户端指南

> 基于底层 Rust 高性能调度内核的 Python 异步客户端。
>
> 专为需要在 Python / asyncio 环境下**高效、稳定管理多个长连接 AI Agent**（Codex、Pi、ACP）而设计，提供优雅的原生 `async with` 上下文管理。

---

## 1. 核心特性

- **极致轻量高效**：基于 PyO3 与 Rust 原生线程池，避开 Python GIL 锁限制，多 Worker 并发性能极佳。
- **上下文管理器完美支持**：使用 `async with AgentPool(...)` 与 `async with await pool.acquire()`，自动管理获取与安全归还，无句柄泄漏之忧。
- **多运行时与统一 MCP**：完全支持 API v2，可在同一个池中混合调度 Codex app-server、Pi RPC 和 ACP，并统一注入 MCP 工具。

---

## 2. 本地开发与构建

环境要求：Python 3.9+，Rust 稳定版，`maturin`。

```bash
# 进入目录并安装 maturin
cd bindings/python
pip install maturin

# 本地快速构建并安装到当前虚拟环境
maturin develop

# 运行自动化测试套件
python -m unittest discover -s test -p "test_*.py"
```

---

## 3. 典型使用示例

### ① 基础使用与上下文管理
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
            }
        ]
    }

    # 1. 启动池
    async with AgentPool(config) as pool:
        # 2. 独占借出一个 Worker
        async with await pool.acquire() as lease:
            reply1 = await lease.ask("帮我写一个二分查找算法。")
            print("第一轮回复:", reply1["text"])

            # 在同一会话与上下文中追问
            reply2 = await lease.ask("请加上详细的注释和复杂度分析。")
            print("追问回复:", reply2["text"])
        # 离开 lease 上下文时，Worker 自动归还池中

asyncio.run(main())
```

### ② 多轮校验与循环修复（Code Review 模式）
```python
async with await pool.acquire() as lease:
    res = await lease.ask("设计一个用户登录组件")
    while needs_fix(res["text"]):
        # 依然在原会话中根据反馈调整
        res = await lease.ask("密码校验规则不符合要求，请调整后重新输出。")
```

---

## 4. 详细文档指引

- [多运行时配置完全指南](../../docs/multi-runtime-usage.md)
- [跨语言绑定契约](../../docs/language-api.md)
- [调度核心设计](../../src/README.md)
