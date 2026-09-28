# ACP 运行模式与测试报告

`agentpools` 调度核心只管理 worker 与会话。下面的进程拓扑专指 `agentpools-acp` 的 stdio 适配器；自行实现的 HTTP 或其他 `AgentBackend` 可以采用不同拓扑。

## 两种拓扑

| 层次 | 独立进程（默认） | 共享进程（显式选择） |
| --- | --- | --- |
| Rust 调度方 | 每个 worker 一个 Rust 线程 | 每个 worker 一个 Rust 线程 |
| ACP Agent | 每个 worker 启动并复用一个子进程 | 同一 `SharedAcpBackend` 的槽位共用一个子进程 |
| ACP 会话 | 每个 worker 一个独立 `sessionId` | 每个 worker 一个独立 `sessionId` |
| 4 worker 均已启用的示例 | 4 个线程、4 个 ACP 子进程、4 个会话 | 4 个线程、1 个 ACP 子进程、4 个会话 |

会话按需打开；创建池时先启动 worker 线程，首次分配任务时才启动 Agent 并建立会话。共享后端用全局请求 ID 路由响应，用 `sessionId` 路由事件。每个 worker 仍只执行一个任务；队列、重试、租借和关闭语义不因进程拓扑改变。“单进程”仅指池直接启动的 ACP Agent 进程，不包含调用方进程、MCP server 或 Agent 派生的其他进程。

独立进程模式使用 `AcpBackend` 或 `AcpPoolOptions::build()`。共享模式使用同一个 `SharedAcpBackend` 的克隆实例，或 `AcpPoolOptions::build_shared()`：

```rust,ignore
let pool = options.build()?; // 默认：每个 worker 一个 ACP Agent 子进程
```

或在另一份 `options` 上选择：

```rust,ignore
let pool = options.build_shared()?; // 多个会话共用一个 ACP Agent 子进程
```

共享模式要求 Agent 支持一个 stdio 连接上的多个并发 ACP 会话。各槽位的 `program`、`args`、`env`、`auth_method`、`client_capabilities` 和 `inherit_stderr` 必须一致；`cwd`、`mcp_servers`、模型及超时可以不同。共享连接减少重复启动 Agent 的开销，但该进程断开会同时影响所有会话。独立进程提供进程级隔离，通常占用更多进程资源。两种模式都不会自动创建 worktree 或沙箱；若任务需要目录隔离，由调用方准备工作目录。

Node.js 和 Python 绑定的 ACP worker 目前仍使用独立进程模式；其 JSON 配置没有共享模式开关。`build_shared()` 是 Rust API，不能只在绑定配置中增加字段来启用。接口细节见[语言绑定契约](language-api.md)和[ACP 适配器说明](../crates/agentpools-acp/README.md)。

## 验证路径

以下命令在仓库根目录执行。确定性的 Rust 测试覆盖调度、独立 ACP 会话、共享进程的并发会话路由、MCP 透传以及 mock Agent 调用 MCP 工具：

```powershell
cargo test --workspace --all-features --offline
cargo clippy --workspace --all-targets --all-features --offline -- -D warnings
```

4 worker / 16 道加法题示例提供三种模式；`--mock` 不需要模型认证，`--codex` 和 `--codex-shared` 使用真实 Codex ACP 与真实 MCP 加法器：

```powershell
cargo build -p agentpools-acp --all-features --bins
cargo run -p agentpools-acp --example batch_add --all-features -- --mock

# 真实模式需先完成 codex login，并设置已安装的 ACP 入口绝对路径：
$env:CODEX_ACP_ENTRY = 'C:\absolute\path\to\node_modules\@agentclientprotocol\codex-acp\dist\index.js'
cargo run -p agentpools-acp --example batch_add --all-features -- --codex
cargo run -p agentpools-acp --example batch_add --all-features -- --codex-shared
```

真实模式要求每个会话选中示例指定的 `gpt-6-luna`，会消耗模型额度。示例核对不同 ACP 进程 PID 的数量、16 道题的数值答案和 16 次实际 MCP 工具调用；`format_compliant` 单独记录“只输出整数”的格式情况，**目前不属于示例的退出码通过条件**。因此退出码为 0 不代表 16 道题全部符合格式要求。

示例每次创建新的 `target/agentpools-batch-add/<模式>-<时间戳>/` 目录，输出：

- `report.json`：模式、模型、不同 ACP PID 数、总耗时、正确数、格式达标数、MCP 调用核对数，以及每道题的 worker、PID、时间戳、答案和错误。
- `report.html`：可直接打开的中文时间线与逐题报告，内容来自同次运行的 JSON。
- `agent-*-mcp.jsonl`：逐槽位的 MCP 调用记录，用于核对工具确实被调用。

若题目或 MCP 校验不通过，示例仍会先写报告再返回错误。启动参数、辅助程序或配置在生成报告前就失败时，可能没有报告文件。`target/` 已被 Git 忽略，清理构建目录会删除这些运行产物；需要留档时应自行归档对应运行目录。

Windows 上可用采样脚本比较真实模式。先构建示例，并在执行脚本的同一 Windows 用户身份下确认 `codex login status` 可用：

```powershell
cargo build -p agentpools-acp --example batch_add --all-features
.\examples\measure_batch_add.ps1 -Mode codex
.\examples\measure_batch_add.ps1 -Mode codex-shared
```

脚本在 `target/agentpools-batch-add-resource/` 写入 `.resources.json`、标准输出和错误输出。资源 JSON 包含退出码、墙钟耗时、采样到的进程树峰值进程数、工作集、私有内存和累计 CPU 秒数；传入 `-DetailedProcessTrace` 还会记录进程角色与寿命。采样间隔约 250 毫秒，峰值是采样值；进程树包含示例、ACP、MCP 及其子进程，因此不能把 `peak_process_count` 当成 ACP Agent 进程数。

## 现有本机报告示例

2026-09-26 运行 `--mock` 生成的 `mock-1790390482265/report.json` 记录了 4 个不同 ACP PID、16/16 数值正确、16/16 格式达标、16/16 MCP 调用核对通过，示例耗时 727 ms。这是无需模型认证的功能验证，不代表真实 Agent 的耗时。

2026-09-25 的一组真实模式本机运行留下了以下记录。数字来自 `target/` 中的报告，属于单机样本，不作为跨机器或多次运行的性能结论。

| 模式 | 加法报告目录 | ACP PID 数 | 正确 / MCP / 格式 | 示例总耗时 | 采样墙钟 / 进程树工作集峰值 |
| --- | --- | ---: | --- | ---: | ---: |
| 独立进程 | `codex-1790340738334` | 4 | 16/16 · 16/16 · 12/16 | 68,130 ms | 68,492 ms / 1,991.0 MB |
| 共享进程 | `codex-shared-1790340845810` | 1 | 16/16 · 16/16 · 13/16 | 73,165 ms | 73,309 ms / 1,306.5 MB |

对应的资源记录分别是 `codex-20260925-205218.resources.json` 与 `codex-shared-20260925-205405.resources.json`。两次运行时间相邻但并非受控基准测试；模型响应、后台进程及系统负载都会影响结果。评估自己的部署环境时，应保存原始报告、重复运行并同时检查答案质量、MCP 调用、耗时与资源采样。
