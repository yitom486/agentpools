# 运行模式评测与性能报告指南

> 本指南介绍如何对 `agentpools` 进行压力评测、资源监控，并解读自动化生成的 HTML 时间线与基准测试报告。

---

## 1. 两种拓扑的资源对比（真实环境数据）

在实际部署中，多个 Agent 并发运行时最核心的考量是**内存开销**与**进程数**：

| 模式 | 运行机制 | 4 个并发 Worker 峰值内存 (RSS) | 进程总数 | 适用场景 |
| --- | --- | ---: | ---: | --- |
| **独立进程模式 (1:1)** | 每个 Worker 独占一个底层的 Agent 子进程 | ~1,990 MB | 4 个主 Agent + 派生进程 | 任务需要物理安全隔离、多租户场景。 |
| **共享进程模式 (1:N)** | 多个 Worker 复用同一个底层 Agent 进程 | ~1,306 MB | 1 个主 Agent + 派生进程 | 单机资源紧张、高频创建会话、内存敏感型场景。 |

*注：以上数据来自 2026 年在 Windows 环境上针对真实 Codex ACP 运行 16 题并发评测的单机采样，共享进程节省了约 **35% 的工作集内存**。*

---

## 2. 自动化基准测试套件（`batch_add`）

代码库提供了一个标准的基准测试套件：**4 个 Worker 并发解答 16 道算术题，且每道题必须实际调用基于官方 Rust MCP SDK 编写的加法器工具。**

### ① 零成本本地 Mock 测试（无需任何 API Key）
```powershell
# 编译测试二进制
cargo build -p agentpools-acp --all-features --bins

# 运行 4 worker 模拟评测
cargo run -p agentpools-acp --example batch_add --all-features -- --mock
```

### ② 真实大模型压力评测（Codex ACP）
```powershell
# 指定本地已构建的 codex-acp 入口
$env:CODEX_ACP_ENTRY = 'C:\path\to\@agentclientprotocol\codex-acp\dist\index.js'

# 独立进程模式压测
cargo run -p agentpools-acp --example batch_add --all-features -- --codex

# 共享进程模式压测
cargo run -p agentpools-acp --example batch_add --all-features -- --codex-shared
```

---

## 3. 测试报告与指标解读

每次评测运行完毕后，程序会自动在 `target/agentpools-batch-add/` 创建以时间戳命名的目录：

- **`report.html`**：
  - **交互式时间线**：展示 4 个 Worker 何时起止、何时发生重试与并发；
  - **甘特图视图**：直观展示任务排队耗时与 Agent 思考执行耗时；
  - **题目校验清单**：展示每道题的输入参数、LLM 思考输出以及底层 MCP 工具的真实调用记录。
- **`report.json`**：包含机器可读的结构化数据，用于 CI 流水线指标对比。
- **`agent-*-mcp.jsonl`**：记录每个 Worker 通道上的 MCP 工具调用报文，用于审查是否发生工具漏调用。

---

## 4. 跨平台资源采样脚本（`measure_batch_add.py`）

为了在 Linux、macOS 和 Windows 上获取真实的 CPU 与内存峰值，仓库内置了无外部第三方依赖的跨平台采样工具：

```bash
# 启动资源采样（采样间隔 250ms，自动记录进程树的峰值内存与 CPU 使用）
python examples/measure_batch_add.py --mode mock
```

对于真实 Codex 评测：
```bash
python examples/measure_batch_add.py --mode codex
python examples/measure_batch_add.py --mode codex-shared
```

生成的 `.resources.json` 会详细记录：
- 墙钟耗时（Wall-clock time）；
- 峰值工作集（Peak Working Set / RSS）；
- 活跃子进程峰值数量；
- 累计 CPU 占用秒数。
