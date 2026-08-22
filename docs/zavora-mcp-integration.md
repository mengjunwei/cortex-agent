# zavora-ai MCP 服务器融入清单

> 本项目（cortex-agent）基于 zavora-ai 旗舰项目 [`adk-rust`](https://github.com/zavora-ai/adk-rust) 构建，且已内置完整 MCP 管理功能（前后端 + 种子机制，见 [mcp-management.md](./design/mcp-management.md)——该设计文档为存档，实现演进以代码为准）。
>
> zavora-ai 组织下有多个纯 Rust MCP 服务器可作为「外部工具源」接入。本文记录各仓库的融入方式与前置条件，供按需启用。
>
> ⚠️ 本文各仓库的**工具数与安装命令为 2026-08 快照**，上游随时漂移，接入前以各仓库 README 为准。
>
> ⚠️ **`excel-mcp-server` 曾编译期内置，已移除**：其 `write_cells` 在某些 cell 组合（header + 浮点 + 公式混合）下卡死（zavora-xlsx 重算 bug，独立 probe 稳定复现）。cortex 已移除内置，保留通用 MCP 修复。详见 [§2](#2--excel-mcp-server曾内置已移除) 与废弃 [设计 spec](./superpowers/specs/2026-08-02-zavora-excel-mcp-embedded-design.md)。

---

## 1. 仓库分档总览

| 档 | 仓库 | 处置 |
|---|---|---|
| **A 纯 Rust MCP 服务器** | `excel-mcp-server`(⚠️有bug，见§2) · `mcp-session-memory` · `mcp-erp` · `adk-rust-mcp-toolkit` · `mcp_slides` | 可接入（excel 走标准通道且不推荐，其余按需）|
| **B 纯 Rust 库** | `zavora-xlsx` · `zavora-slide` | 跳过（A 类底层，引它=造轮子）|
| **C 需 Node 运行时** | `computer-use-mcp` | 可接入（需装 Node）|
| **D 独立大应用** | `zavora-cli` · `work` · `zavora-era` · `gitclaw` | 不融入，仅参考 |

---

## 2. ⚠️ excel-mcp-server：曾内置，已移除

`excel-mcp-server`（74 工具，基于 `zavora-xlsx`）曾通过 `build.rs` 编译期嵌入 cortex 二进制。**已移除**，原因：

- **`write_cells` 在某些 cell 组合下卡死**：独立 probe `excel-mcp-server.exe`（不经 cortex）稳定复现——纯值/简单组合 0.03s，但「header + 浮点 + 公式混合」的组合 60s+ 无响应。根因是 `zavora-xlsx` 公式/重算 bug（上游，cortex 无法修）。

**cortex 保留的通用 MCP 修复**（移除 excel 时保留，对所有 MCP 工具生效）：
- MCP 工具 `declaration` 字段名 `parameters`（修复 LLM 收到空 schema、不传参 → `missing field`）
- probe 改 `try_lock`（不与工具调用抢锁误杀有状态 MCP，参考 codex/claurst）
- `call_tool` 按服务配置的超时（`tool_timeout_secs`，界面/seed 可配，默认 60s；MCP 工具卡死时返回错误，不无限阻塞 SSE，参考 codex `DEFAULT_TOOL_TIMEOUT`；client 锁等待也计入超时）

> 如仍想用 excel：`cargo install excel-mcp-server` + 加 `[[mcp.seeds]]`（endpoint=`excel-mcp-server`）。但建议避开「多列 + 公式」写入组合。

---

## 3. 可按需融入（A 类：纯 Rust，`cargo install` 即装）

> 接入后通过「助手编辑页勾选」使用，工具以 `mcp__<slug>__<tool>` 命名空间注入。**这些走标准 MCP 子进程通道，不需要改 Rust 代码**，仅 `cargo install` + 加 config 种子（或界面新建）。

| 仓库 | 安装命令 | slug 示例 | 传输 | 工具数 | 前置条件 / 注意 |
|---|---|---|---|---|---|
| `mcp-session-memory` | `cargo install mcp-session-memory` | `memory` | stdio / http | 13 | 无外部凭据；本地 SQLite 嵌入。⚠️ 与 cortex **已有的跨会话记忆功能重叠**，引入前评估是否冗余 |
| `mcp-erp` | `cargo install mcp-erp` | `erp` | stdio | 44 + 10 会计扩展 | 需 ERP 凭据（SAP / NetSuite / Odoo / Zoho / BC），走 env |
| `adk-rust-mcp-toolkit` | `cargo install adk-rust-mcp-image`（共 11 个 crate，按媒体类型选）| `media` | stdio / http / sse | ~45 | 需 `GEMINI_API_KEY` 或 Vertex AI 凭据；`avtool` 子工具需 FFmpeg |
| `mcp_slides` | 源码 `git clone` + `cargo build --release`（**未上 crates.io**）| `slides` | stdio | 71 | 无外部凭据；PPT 全功能。需手动编译 |

---

## 4. C 类（需 Node 运行时）

| 仓库 | 启动命令 | slug 示例 | 传输 | 工具数 | 前置条件 |
|---|---|---|---|---|---|
| `computer-use-mcp` | `npx -y @zavora-ai/computer-use-mcp` | `desktop` | stdio | 64 | **Node 18+**；Linux 还需 `xdotool` / `wmctrl` / `xclip` / `scrot`。桌面控制（截屏/鼠标/键盘/窗口/AppleScript/PowerShell），无需外部凭据 |

> cortex 的 stdio 启动用 `Command::new(endpoint).args(...)`（不走 shell），`npx` 作为 endpoint 时需确保 `npx` 在 PATH，或在 config 种子 `endpoint` 用绝对路径。

---

## 5. D 类（不融入，仅参考）

| 仓库 | 性质 | 参考价值 |
|---|---|---|
| `zavora-cli` | 基于 ADK-Rust 的命令行 AI Agent（与 cortex 同级）| 工具安全流水线、多 provider 路由 |
| `work` | Electron 桌面 App（表格/文档/幻灯片 specialist）| specialist 分层设计 |
| `zavora-era` | 完整复式记账 ERP + Amos AI Agent | Rust Agent + MCP + 业务领域融合范式 |
| `gitclaw` | AI Agent Git 协作平台（HTTP/REST，非 MCP）| Agent 间协作 / 提 PR（调其 Rust SDK）|

---

## 6. 通用接入法（A / C 类标准流程）

1. **装二进制**：`cargo install <name>`（A 类）或 `npx -y <pkg>`（C 类）
2. **注册到 cortex**（二选一；当前仓库配置未预置任何 `[[mcp.seeds]]`）：
   - **配置驱动**：实际加载的 config 文件（开发默认 `config/config_1.toml`）加 `[[mcp.seeds]]`，启动自动 upsert（覆盖 name/endpoint/args/transport/超时，env/headers 不受影响）：
     ```toml
     [[mcp.seeds]]
     slug = "memory"
     name = "会话记忆"
     transport = 1            # 1=stdio, 2=http
     endpoint = "mcp-session-memory"
     args = "[]"
     ```
   - **界面驱动**：`MCP 服务` 页「新建 MCP 服务」
3. **重启 cortex** → `MCP 服务` 页验证健康（绿色 + 工具数）
4. **助手勾选**：`助手编辑页` 勾选该 MCP → 会话中工具以 `mcp__<slug>__<tool>` 注入

---

## 7. 嵌入式接入机制（已下线，仅存档）

> excel 曾用此机制内置，因 bug 已移除（`build.rs` / `src/infra/embedded_bin.rs` / `embedded://` 解析已删）。机制本身可行，未来若有可靠的无状态 MCP 可复用，思路：

1. **`build.rs`**：`cargo install <name>` 到 `OUT_DIR/mcp-vendor`（编译中间产物丢系统 temp 隔离锁），通过 `cargo:rustc-env=EMBEDDED_<NAME>_BIN=<path>` 传路径
2. **`src/infra/embedded_bin.rs`**：`include_bytes!(env!("EMBEDDED_<NAME>_BIN"))` + `ensure_<name>(data_dir)` 释放 + `resolve_embedded` 路由
3. **McpManager**：持 `data_dir`，连接前解析 `embedded://<key>` 为真实路径
4. **config 种子**：`endpoint = "embedded://<key>"`

> 选型教训：编译期嵌入只适合**经过验证、无状态、无 bug** 的核心工具。excel 未经充分测试就嵌入，结果卡死 bug 拖累整个 MCP 链路——已回退为「需要才 `cargo install`」的标准子进程通道。

---

## 8. 其它外部 MCP（非 zavora 仓库）

非 zavora-ai 组织的标准 MCP 服务器，同样可通过 §6 的通用接入法（标准 MCP 子进程 / HTTP 通道，零代码）接入。已整理部署文档的：

| MCP | 用途 | 传输 | 文档 |
|---|---|---|---|
| **Playwright MCP**（微软官方）| 无图形界面 Linux 上的浏览器自动化（打开网页 / 点击 / 截图 / 抓取内容 / 生成 PDF），替代需要 GUI 的 mcp-chrome | `streamable_http`（`transport=2`，endpoint `http://<host>:8931/mcp`）| [安装指南](./playwright-mcp-install.md) |

> 接入流程与 §6 一致：部署服务 → 加 `[[mcp.seeds]]`（或界面新建）→ 重启 cortex 验证健康 → 助手勾选，工具以 `mcp__<slug>__<tool>` 注入。
