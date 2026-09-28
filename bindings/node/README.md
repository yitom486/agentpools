# `agentpools` Node.js 客户端指南

> 基于底层 Rust 高性能调度内核的 Node.js 客户端。
>
> 专为需要在 Node.js / TypeScript 环境下**高效、稳定管理多个长连接 AI Agent**（Codex、Pi、ACP）而设计，提供原生 Promise / async-await 接口。

---

## 1. 核心特性

- **高性能底层驱动**：直接基于 Rust 原生线程与管道通信（通过 Node-API），性能高且内存开销极小。
- **独占租用（Lease）保障**：支持多轮对话、中间异步校验，期间 Worker 槽位绝对锁定，杜绝并发插队污染上下文。
- **多运行时与统一 MCP**：完全支持 API v2，可在同一个池中混合调度 Codex app-server、Pi RPC 和 ACP，并统一注入 MCP 工具。

---

## 2. 本地开发与构建

环境要求：Node.js 20+，Rust 稳定版。

```bash
# 进入目录并安装依赖
cd bindings/node
npm install

# 编译底层原生扩展
npm run build

# 运行自动化单元测试
npm test
```

---

## 3. 典型使用示例

### ① 基础使用与多轮交互
```javascript
const { AgentPool } = require('agentpools');

async function main() {
  // 1. 初始化池（支持 API v1 与 API v2）
  const pool = new AgentPool({
    apiVersion: 2,
    maxQueued: 32,
    agents: [
      {
        runtime: 'codexAppServer',
        program: 'codex',
        args: ['app-server'],
        cwd: process.cwd(),
      },
    ],
  });

  try {
    // 2. 租借一个 Worker
    const lease = await pool.acquire();
    try {
      // 第一轮问答
      const res1 = await lease.ask('请写一个快速排序算法。');
      console.log('回复:', res1.text);

      // 第二轮追问（历史上下文完整保留在同一进程中）
      const res2 = await lease.ask('请为它添加单元测试用例。');
      console.log('追问回复:', res2.text);
    } finally {
      // 3. 释放租借，Worker 归还池中
      await lease.finish();
    }
  } finally {
    // 4. 优雅关闭池
    await pool.close({ drain: true });
  }
}

main().catch(console.error);
```

### ② 多轮校验与循环修复（Code Review 模式）
```javascript
const lease = await pool.acquire();
try {
  let reply = await lease.ask('实现用户注册接口');
  
  // 外部做业务沙箱测试，测试不通过则在同一会话中追问修改
  while (await runTestSuitFailed(reply.text)) {
    reply = await lease.ask('测试未通过，请检查边界条件并修复代码。');
  }
} finally {
  await lease.finish();
}
```

---

## 4. 详细文档指引

- [npm 打包原理与多平台使用指南（Node.js / Bun / TypeScript）](../../docs/npm-packaging-and-usage.md)
- [多运行时配置完全指南](../../docs/multi-runtime-usage.md)
- [跨语言绑定契约](../../docs/language-api.md)
- [调度核心设计](../../src/README.md)
