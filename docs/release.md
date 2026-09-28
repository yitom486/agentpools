# 构建与多平台发布工作流指南

> 本指南介绍 `agentpools` 的自动化 CI/CD 流水线，以及如何向 crates.io、npm 和 PyPI 发布跨平台（Windows、Linux、macOS）的二进制扩展与包。

---

## 1. GitHub Actions 工作流总览

仓库内置了两个核心 GitHub Actions 工作流：

| 工作流文件 | 触发条件 | 主要任务 |
| --- | --- | --- |
| **[`ci.yml`](../.github/workflows/ci.yml)** | 任何推送到 `master`/`main` 分支或提交 Pull Request 时触发。 | 跨平台运行 Rust 单元/集成测试与 Clippy 检查；构建并运行 Node.js（5 个平台架构）与 Python（5 个平台架构）的绑定测试；将编译产物上传为 Artifacts。 |
| **[`release.yml`](../.github/workflows/release.yml)** | 手动触发（`workflow_dispatch`）。 | **安全两阶段发布**：<br/>• `publish=false`（演练模式）：构建全部 6 个 npm 包与 5 个 Python Wheel，打包 Artifacts 供人工审查。<br/>• `publish=true`（正式发布）：按顺序自动向 crates.io、npm 官方源和 PyPI 官方源发布产物。 |

---

## 2. 发布前检查清单（Checklist）

在触发正式发布前，必须完成以下 4 项检查：

### ① 版本一致性核验
整个工作区包含 11 个包清单，它们的版本号必须保持完全一致：
```bash
python scripts/check_release_versions.py 0.1.0
```
该脚本会自动校验根目录 `Cargo.toml`、3 个子 crate、Node.js 根包与 5 个平台架构包，以及 Python 的 `pyproject.toml`。

### ② 密钥与权限环境（GitHub Environments）
- **`crates-io`**：配置 `CARGO_REGISTRY_TOKEN` Secret；
- **`npm`**：配置 `NPM_TOKEN` Secret；
- **`pypi`**：配置 PyPI 的 **Trusted Publishing**（OIDC 认证，绑定 `release.yml` 与 `pypi` 环境，无需长期静态 Token）。

### ③ 演练预检（Dry Run）
1. 在 GitHub Actions 页面选择 **Prepare or publish release**；
2. 输入版本号（如 `0.1.0`），保持 `publish: false`；
3. 检查流水线是否全部绿灯，并下载 `release-npm` 与 `release-wheels` 产物解压校验。

### ④ 正式打 Tag 发布
1. 为经过审查的 commit 打上标准 Git Tag：
   ```bash
   git tag v0.1.0
   git push origin v0.1.0
   ```
2. 在该 Tag 上手动运行 **Prepare or publish release**，并选择 `publish: true`。

---

## 3. 发布顺序与原子性保证

由于跨包依赖与多个包管理器的特点，正式发布严格按以下顺序执行：
1. **Rust Crate 发布**：先向 crates.io 发布无依赖的底座 `agentpools` 和 `agentpools-transport`，等待 registry 索引生效后，再发布 `agentpools-acp`；
2. **npm 多平台发布**：先发布 5 个平台特定的二进制原生扩展包（`@agentpools/win32-x64-msvc`、`darwin-arm64` 等），最后发布作为入口的根包 `agentpools`；
3. **PyPI Wheel 发布**：基于 PyO3 的 `abi3-py39` 特性，一套 Wheel 自动覆盖 Python 3.9+ 的所有 CPython 版本。
