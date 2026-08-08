# 融入 zavora-ai excel-mcp-server 设计（编译期嵌入方案）

> **状态**：⚠️ 已回退（2026-08，commit 4278010）——编译期嵌入 `excel-mcp-server` 已实现后因 `write_cells` 在 header + 浮点 + 公式混合组合下卡死（zavora-xlsx 重算 bug）而回退移除，`build.rs` / `src/infra/embedded_bin.rs` / `embedded://` 解析均已删。接入 excel 请走通用 MCP 子进程方式（`cargo install` + `[[mcp.seeds]]`），**勿按本文重新嵌入**。本文仅作历史快照保留。
> **日期**：2026-08-02
> **依据**：[architecture.md](../../architecture.md) v1.5、[mcp-management.md](../design/mcp-management.md)

---

## 1. 背景

cortex-agent 基于 zavora-ai 旗舰项目 `adk-rust` 构建，且**已实现完整的 MCP 管理功能**（前后端齐备：`src/domain/mcp/` + `src/server/mcp.rs` + `frontend/src/views/McpServerPage.vue` + 种子机制 `cfg.mcp.seeds`）。zavora-ai 组织下存在多个纯 Rust MCP 服务器可作为外部工具源接入。

经调研（zavora-ai 组织 12 个仓库），按融入难度分四档：

| 档 | 仓库 | 处置 |
|---|---|---|
| **A 纯 Rust MCP 服务器** | `excel-mcp-server`(74工具) · `mcp_slides`(71) · `mcp-session-memory`(13) · `mcp-erp`(44) · `adk-rust-mcp-toolkit`(~45) | 可接入，本次做 excel，其余入清单 |
| **B 纯 Rust 库** | `zavora-xlsx` · `zavora-slide` | 跳过（A 类的底层，引它=造轮子）|
| **C 需 Node 运行时** | `computer-use-mcp` | 入清单（以后按需）|
| **D 独立大应用** | `zavora-cli` · `work` · `zavora-era` · `gitclaw` | 不融入，仅参考 |

本次选定 `excel-mcp-server` 作为首个融入样板。该 crate **不提供预编译二进制**（GitHub release 仅源码包），只能源码编译；release 体积 **9.8 MB**。

## 2. 目标

| # | 目标 | 衡量标准 |
|---|------|----------|
| G1 | excel-mcp-server 在**编译期**由 build.rs 自动从 crates.io 拉源码、当前平台编译、`include_bytes!` 嵌入 cortex 二进制 | cortex 单一二进制内含 excel 字节；产物不入 git |
| G2 | cortex 运行时把内嵌字节释放为可执行文件,以 stdio 子进程方式接入 | `McpServerPage` 显示绿色健康 + 74 工具可见 |
| G3 | 助手可勾选 excel（`enabled_mcps`），聊天中真实调用 `mcp__excel__*` 工具 | ChatPage 生成 xlsx 文件 |
| G4 | 交付「可融入清单」文档,记录其余仓库的接入方式与前置条件 | 文档存在 |
| G5 | 跨平台：Windows / Linux / macOS 各自原生编译各自嵌入 | build.rs 在各平台产出对应格式二进制 |

## 3. 非目标（Out of Scope）

- ❌ **不**把 `zavora-xlsx` 作为 crate 依赖重写为 cortex 内置 FunctionTool —— `excel-mcp-server` 已基于它提供 74 工具，重写违反 YAGNI
- ❌ **不**做进程内 MCP（同进程嵌入 lib）—— 违反 mcp-management.md §1.3「cortex 始终作为 MCP Client」定位；zendriver 进程内是浏览器自动化的历史特例,不复制
- ❌ 本次不接入 `mcp-session-memory` / `mcp-erp` / `adk-rust-mcp-toolkit` / `mcp_slides` / `computer-use-mcp`（仅写入清单）
- ❌ 不支持交叉编译场景（host ≠ target）—— 各平台各自原生编译即可
- ❌ 不改 MCP 管理功能的现有契约（GraphQL / DTO / store），仅在传输层增加 `embedded://` 定位识别

## 4. 核心机制（编译期构建 → 嵌入 → 运行时释放 → 占位符定位）

```text
[cargo build cortex]
        │
        ▼
 ① build.rs 执行
    ├─ 检查 OUT_DIR/mcp-vendor/bin/excel-mcp-server(.exe) 是否存在
    ├─ 不存在或有新版 → cargo install excel-mcp-server
    │     --root   = OUT_DIR/mcp-vendor        （二进制装这里）
    │     --target-dir = $TMP/cortex-mcp-build （编译中间产物丢系统 temp，隔离 cargo 锁）
    │     （无 --version → 拉 crates.io 最新；已是最新则 cargo 自动跳过）
    └─ println!("cargo:rustc-env=EMBEDDED_EXCEL_MCP_BIN={path}")
        │
        ▼
 ② cortex 正常编译
    src/infra/embedded_bin.rs:
      const EXCEL_BYTES: &[u8] = include_bytes!(env!("EMBEDDED_EXCEL_MCP_BIN"));
    → excel exe 的 9.8MB 字节被物理 bake 进 cortex 可执行文件
        │
        ▼
 ③ cortex 运行（启动 / 首次连接 excel MCP）
    embedded_bin::ensure_excel(data_dir):
      target = {data_dir}/mcp-vendor/excel-mcp-server(.exe)
      若 target 存在且大小 == EXCEL_BYTES.len() → 跳过
      否则写 EXCEL_BYTES → target；Unix chmod 0o755
        │
        ▼
 ④ McpManager 连接 excel 时
    endpoint = "embedded://excel-mcp-server"（DB 里的占位符）
    → 传输层识别 embedded:// → embedded_bin::ensure_excel() 拿真实路径
    → tokio::process::Command::new(真实路径) 拉起 stdio 子进程
```

## 5. 架构归属（按 architecture.md §3 决策树裁决）

| 代码 | 归属 | 依据 |
|------|------|------|
| `build.rs`（编译期拉源码 + 编译 + 传 env） | 项目根（组合根性质） | §3 Q6：组合根逻辑 |
| `src/infra/embedded_bin.rs`（include_bytes + 释放 + 权限） | 基础设施层 `src/infra/` | §2.4：与业务无关的通用技术能力（嵌入式二进制释放）|
| `embedded://` 占位符识别 + 路径解析 | `src/domain/mcp/`（manager 或 transport）| §2.3：外部网关适配（封装「如何定位并启动 MCP 子进程」）|
| excel 种子 endpoint 占位符 | `config/config.local.toml` + `config/config.toml` | 横切配置层 |
| AppDeps | 无需新增字段（释放惰性触发，复用既有 `data_dir`）| §5 |

> **不引入新全局**：embedded_bin 的 `include_bytes!` 是编译期常量（不可变进程级常量，§5.4 例外第 3 类），合法。

## 6. 详细设计

### 6.1 `build.rs`（新增，项目根）

职责：编译期确保 excel-mcp-server 二进制存在，并把路径通过 rustc-env 传给 cortex。

```rust
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR 未设置"));
    let vendor_root = out_dir.join("mcp-vendor");              // 二进制安装根
    let vendor_target = std::env::temp_dir().join("cortex-mcp-build"); // 编译中间产物(系统 temp,隔离锁)

    let exe_name = if cfg!(target_os = "windows") {
        "excel-mcp-server.exe"
    } else {
        "excel-mcp-server"
    };
    let exe_path = vendor_root.join("bin").join(exe_name);

    // 缓存：已存在则跳过（cargo install 自身也会判最新版跳过）
    if !exe_path.exists() {
        let status = Command::new("cargo")
            .args([
                "install", "excel-mcp-server",
                "--root", vendor_root.to_str().unwrap(),
                "--target-dir", vendor_target.to_str().unwrap(),
                // 不写 --version：跟 crates.io 最新；不写 --locked：外部 crate 无 lock
            ])
            .status()
            .expect("build.rs: cargo install excel-mcp-server 失败，请检查网络/cargo");
        assert!(status.success(), "cargo install excel-mcp-server 编译失败");
    }

    // 路径传给 cortex 编译期
    println!("cargo:rustc-env=EMBEDDED_EXCEL_MCP_BIN={}", exe_path.display());
    // excel exe 内容变化（新版）→ 触发 cortex 重编 include_bytes
    println!("cargo:rerun-if-changed={}", exe_path.display());
    println!("cargo:rerun-if-changed=build.rs");
}
```

**关键点**：
- **不写死版本号** → 每次 build 跟 crates.io 最新；cargo install 自身缓存：已是最新则秒过，有新版才重编
- `--target-dir` 指向系统 temp → 编译中间产物**不进 cortex 的 target 目录**，规避 cargo 锁冲突
- 二进制装 `OUT_DIR/mcp-vendor/bin/`（OUT_DIR 在 target 内但 per-crate 隔离，clean 时随之清理）
- 产物在 target/ 内 → **天然不入 git**（.gitignore 已含 target/）

### 6.2 `src/infra/embedded_bin.rs`（新增）

职责：持有内嵌字节，运行时释放为可执行文件。

```rust
//! 嵌入式 MCP 二进制：编译期 include_bytes! 嵌入，运行时释放到 data_dir。
//! 当前内置 excel-mcp-server。新增嵌入二进制时扩展本模块。

use std::path::{Path, PathBuf};
use crate::error::AppError;

/// 编译期嵌入的 excel-mcp-server 字节（build.rs 通过 EMBEDDED_EXCEL_MCP_BIN 指定来源）
const EXCEL_MCP_BYTES: &[u8] = include_bytes!(env!("EMBEDDED_EXCEL_MCP_BIN"));

const EXCEL_BIN_NAME: &str = if cfg!(target_os = "windows") {
    "excel-mcp-server.exe"
} else {
    "excel-mcp-server"
};

/// 释放 excel-mcp-server 到 {data_dir}/mcp-vendor/，返回可执行文件绝对路径。
/// 幂等：文件存在且大小一致则跳过写入；否则覆盖（含版本升级场景）。
pub fn ensure_excel(data_dir: &Path) -> Result<PathBuf, AppError> {
    let dir = data_dir.join("mcp-vendor");
    std::fs::create_dir_all(&dir).map_err(|e| AppError::InternalError(format!("创建 mcp-vendor 目录失败: {e}")))?;
    let target = dir.join(EXCEL_BIN_NAME);

    let need_write = match std::fs::metadata(&target) {
        Ok(m) => m.len() as usize != EXCEL_MCP_BYTES.len(),  // 大小不同→新版→覆盖
        Err(_) => true,                                       // 不存在→写入
    };
    if need_write {
        std::fs::write(&target, EXCEL_MCP_BYTES)
            .map_err(|e| AppError::InternalError(format!("释放 excel-mcp-server 失败: {e}")))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755))
                .map_err(|e| AppError::InternalError(format!("设置可执行权限失败: {e}")))?;
        }
    }
    Ok(target)
}
```

> **缓存判断用「文件大小」**：同版本大小恒定 → 跳过；版本升级大小大概率变化 → 覆盖。简单且无 hash 开销。若需更严谨，未来可改为 sha256 比对（YAGNI，暂不做）。

### 6.3 `embedded://` 占位符识别（改 `src/domain/mcp/`）

在 McpManager 建立连接前（`get_or_connect` 或 `connect` 调用前），识别 endpoint 占位符并解析为释放后的真实路径。

**实现选择**：在 `manager.rs` 的连接入口处理（manager 持有运行时上下文，能拿 `data_dir`），保持 `transport.rs::connect` 纯粹（只收已解析的真实路径/命令）。具体：manager 连接前若 `server.endpoint == "embedded://excel-mcp-server"`，调 `embedded_bin::ensure_excel(&data_dir)` 拿路径，构造一个 endpoint 替换为该路径的临时 `McpServer` 副本再交给 transport。

> data_dir 来源：AppDeps 已有 `data_dir`（或从 config 读）。manager 连接时传入。若 manager 当前不持有 data_dir，通过 AppDeps 字段或连接参数补齐（实现时定，优先复用既有字段，不新增全局）。

### 6.4 配置改动

`config/config.local.toml` 与 `config/config.toml` 的 excel 种子 endpoint 由 `"excel-mcp-server"` 改为占位符：

```toml
[[mcp.seeds]]
slug = "excel"
name = "Excel 报表工具"
transport = 1
endpoint = "embedded://excel-mcp-server"   # ← 占位符，运行时由 cortex 释放的内嵌二进制解析
args = "[]"
```

DB 里 `mcp_servers.endpoint` 存占位符（稳定，不随机器路径变）；路径解析集中在运行时。

## 7. 跨平台说明

- build.rs 中 `cfg!(target_os = "windows")` 决定 exe 名（带不带 `.exe`）
- `cargo install` 在 host 平台编译 host 平台二进制 → Windows 编 win exe、Linux 编 ELF、macOS 编 Mach-O
- `include_bytes!` 嵌入的是当前平台编译产物 → 各平台 cortex 内嵌对应格式 excel
- **不支持交叉编译**（如 linux 上 `--target x86_64-pc-windows-gnu`）：build.rs 跑在 host，会嵌 host 平台 exe 进 target 平台 cortex，运行时无法执行。本次明确不支持，文档注明（YAGNI，用户需求是各平台各自原生编译）。

## 8. 版本策略（每次最新版）

- build.rs **不写 --version** → `cargo install excel-mcp-server` 拉 crates.io 最新
- cargo install 自身逻辑：若 `--root` 下已装版本 == crates.io 最新 → 输出 "already installed" 跳过编译（秒过）；若有新版 → 重编安装
- `cargo:rerun-if-changed=<exe>` → excel exe 内容变化（新版）时触发 cortex 重编 `include_bytes!`
- **日常 build（无 excel 新版）**：build.rs 跑 cargo install → 秒过缓存 → exe 未变 → cortex 不重嵌。零额外开销
- **有 excel 新版**：cargo install 重编（~1min）→ exe 变 → cortex 重嵌（+重编 cortex 相关 crate）

> 若未来要求「每次 build 强制重下载即使同版本」，build.rs 加 `--force`（代价：每次 build +1min）。默认不做。

## 9. 验收标准（「界面测试通了」硬指标）

1. **编译**：`cargo build --release --bin cortex-agent` 成功，产出的 cortex 二进制体积较前 +约 9.8MB（验证嵌入生效）
2. **McpServerPage**：excel 行显示**绿色健康**，点开能看到 74 个工具
3. **AssistantEditPage**：能勾选 excel（`enabled_mcps` 含 excel 的 id）
4. **ChatPage**：发「在 `data/test.xlsx` 写入表头 姓名/分数 并填两行示例」→ agent 调用 `mcp__excel__*` 工具 → `data/test.xlsx` 真实生成且内容正确

## 10. 风险与对策

| 风险 | 等级 | 对策 |
|------|------|------|
| **build.rs 调 cargo install 锁冲突**（cargo 官方不推荐 build.rs 内调 cargo）| 中 | `--target-dir` 指系统 temp 完全隔离；首个执行验证点即测此。**备选**：退化为 `xtask`/`scripts/build-mcp.sh`，build 前手动跑一次（牺牲自动化） |
| 每次 build 查 crates.io 索引需网络 | 低 | 在线自动跟最新；离线 build.rs 加 `--offline` 回退（用本地缓存版本）。实现时加 try/回退 |
| cortex 二进制 +9.8MB | 低 | 用户已知并接受 |
| 交叉编译嵌错平台 exe | 低 | 明确不支持，文档注明 |
| excel-mcp-server 未来改 crate 结构/二进制名 | 低 | build.rs exe 名常量化，改动局部 |
| `embedded://` 与未来其他嵌入二进制扩展 | 低 | embedded_bin 模块化设计，新增二进制扩展 const + ensure_xxx + scheme 路由 |

## 11. 文件清单

### 新增
| 文件 | 职责 |
|------|------|
| `build.rs` | 编译期 cargo install excel-mcp-server + rustc-env 传路径 |
| `src/infra/embedded_bin.rs` | include_bytes! 嵌入 + 运行时释放 + 权限 |
| `docs/zavora-mcp-integration.md` | 其余可融入仓库的接入清单 |

### 修改
| 文件 | 改动 |
|------|------|
| `src/infra/mod.rs` | `pub mod embedded_bin;` |
| `src/domain/mcp/manager.rs`（或 transport.rs） | 连接前识别 `embedded://` → 调 `embedded_bin::ensure_excel` 解析路径 |
| `config/config.local.toml` | excel 种子 endpoint → `embedded://excel-mcp-server` |
| `config/config.toml` | 同上（默认模板） |

### 不改
- MCP 管理 GraphQL / DTO / store / 前端 McpServerPage（契约不变）
- Cargo.toml（不加 excel-mcp-server 为依赖；它是外部 bin，由 build.rs 编译，不入 cargo 依赖树）

## 12. 附带交付：可融入清单文档大纲（`docs/zavora-mcp-integration.md`）

记录其余仓库的一行式接入命令 + 前置条件，供以后按需启用（这些走标准 MCP 子进程通道，**不**走 embedded 嵌入，仅 cargo install / npx 后在界面添加或加种子）：

- **A 类（纯 Rust，cargo install）**：
  - `mcp-session-memory`：`cargo install mcp-session-memory`，slug 例 `memory`，stdio，13 工具，无凭据。⚠️ 与 cortex 已有跨会话记忆功能重叠
  - `mcp-erp`：`cargo install mcp-erp`，slug 例 `erp`，stdio，44+10 工具，需 SAP/NetSuite/Odoo/Zoho/BC 凭据（env）
  - `adk-rust-mcp-toolkit`：`cargo install adk-rust-mcp-image`（等 11 个 crate），需 `GEMINI_API_KEY` / Vertex 凭据，avtool 需 FFmpeg
  - `mcp_slides`：源码 `cargo build --release`（未上 crates.io），71 工具，PPT
- **C 类（需 Node）**：
  - `computer-use-mcp`：`npx -y @zavora-ai/computer-use-mcp`，slug 例 `desktop`，stdio，64 工具，需 Node 18+，Linux 需 xdotool/wmctrl/xclip/scrot
- **D 类（不融入，仅参考）**：`zavora-cli` / `work` / `zavora-era` / `gitclaw`
- **通用接入法**：`cargo install` / `npx` → `config.local.toml` 加 `[[mcp.seeds]]`（或界面新建）→ 重启 → McpServerPage 验证健康 → 助手勾选

## 13. 推进路线

| 步骤 | 范围 | 验证 |
|----|------|------|
| S1 | `build.rs` + `embedded_bin.rs` + `infra/mod.rs` | `cargo build` 成功，cortex 体积 +9.8MB |
| S2 | manager/transport `embedded://` 识别 + config 占位符 | 单测：embedded endpoint 解析为释放路径 |
| S3 | 启动 cortex，DB upsert excel 种子，McpServerPage 验证健康 + 74 工具 | 界面绿 |
| S4 | 助手勾选 excel，ChatPage 调用工具生成 xlsx | 文件生成且内容正确 |
| S5 | `docs/zavora-mcp-integration.md` 清单文档 | 文档存在 |

每步满足 [architecture.md §10 CR Checklist](../../architecture.md#10-code-review-checklist)。
