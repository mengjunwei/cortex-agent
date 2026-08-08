# MCP 管理功能设计方案

> **状态**：已实现（以代码为准，本文为设计存档）
> **作者**：—
> **日期**：2026-06-29
> **依据**：[docs/architecture.md](../architecture.md) v1.1
> **参考实现**：[src/model_provider/](../../src/model_provider/)（模型供应商管理）
>
> ⚠️ **实现期演进（2026-08 核对，阅读时以代码为准）**：
> - 浏览器能力（`zendriver`）已整体移除——文中 §1.1 背景、§5.3.4 / §5.4 / §8 等处引用的 `src/infra/zendriver_mcp.rs` 均为死链，仅作"模式参考"；实际 MCP 工具命名空间包装见 `src/domain/mcp/`。
> - **`tool_timeout_secs`**：`McpServer` / `mcp_servers` 表新增「单次工具调用超时」列（默认 60s，界面可配），`call_tool` 按此超时（防卡死阻塞 SSE）——本文档撰写时尚无此字段。
> - **并发模型**：连接以 `Arc<Mutex<RunningService>>` 串行化，探测用 `try_lock`（不抢锁、不累计失败）；**无指数退避重连**（见 §5.3.3 / §5.3.5 已修订）。
> - **工具装配**：`build_custom_agent` 不直接调 `mgr.get_toolsets`，而是由 SSE 层预先 `build_toolsets` 经 `AgentRequest.mcp_toolsets` 传入；`list_tools`（`tools` Query）只返回内置注册表 `custom_options`，**未做 MCP 分组**（前端 MCP 勾选数据来自 `mcpServers` 接口）。
> - **前端已落地**：§1.3「非目标」所列"不做前端"在设计期后被突破，MCP 管理页已实现（`frontend/src/views/McpServerPage.vue`，含批量启停 / 删除 / 探测）；§1.3 该条仅作设计期边界记录。

---

## 1. 背景与目标

### 1.1 背景

adk-rust 已启用 `mcp` feature（见 [Cargo.toml](../../Cargo.toml#L7-L24)），项目当前仅有一种 MCP 用法：[src/infra/zendriver_mcp.rs](../../src/infra/zendriver_mcp.rs) 通过 `tokio::io::duplex` 把 `zendriver-mcp` 作为**同进程 MCP Server** 挂载到浏览器助手（硬编码、不可配置）。

MCP 生态中存在大量可复用的 Server（GitHub / Slack / 文件系统 / 数据库 / 自定义），目前项目**无法在运行时动态接入外部 MCP Server**，导致 Agent 能力被锁定在少数内置工具上。

### 1.2 目标

| # | 目标 | 衡量标准 |
|---|------|----------|
| G1 | 用户可通过 GraphQL 增删改查 MCP Server 配置（含密钥） | API 接口可用、数据持久化 |
| G2 | 支持主流的两种传输方式：`stdio`（子进程）+ `streamable_http`（远程 HTTP） | 两种方式均能成功 list_tools |
| G3 | 自定义助手可勾选已启用的 MCP Server，会话运行时其工具自动注入 | 助手 `enabled_mcps` 字段生效 |
| G4 | MCP 工具与现有内置工具（web_search / search_kb 等）无冲突共存 | 工具命名空间隔离 |
| G5 | 敏感字段（env / headers 中的 token）加密存储，永不外泄 | 复用 AesCodec |
| G6 | 全部遵循 [architecture.md](../architecture.md)，无新增技术债 | CR checklist 全绿 |

### 1.3 非目标（Out of Scope）

- ❌ MCP Server 的**发现/市场**（marketplace）—— 本期只做用户自填
- ❌ OAuth 等复杂鉴权流 —— 仅支持静态 token / API Key（置于 env 或 header）
- ❌ MCP Server 侧开发 —— 本项目始终作为 MCP **Client**
- ❌ 会话级 MCP 动态增删 —— MCP 绑定在助手维度，会话内不可变
- ❌ 前端实现 —— 本方案仅交付后端 API + 数据契约

---

## 2. 架构归属（按 §3 决策树裁决）

| 代码 | 裁决 | 依据 |
|------|------|------|
| GraphQL resolver / DTO | `src/server/mcp.rs` + `src/server/graphql.rs` | §3 Q1：HTTP/GraphQL 协议细节 |
| MCP Server 配置存储 + 连接池 + 生命周期 | `src/domain/mcp/` | §3 Q3：领域模型 + Repository + 外部网关封装 |
| stdio / HTTP 传输适配（与 `rmcp` 直接交互） | `src/domain/mcp/transport.rs` | §2.3：外部网关客户端归属领域层（封装第三方驱动） |
| AES 加密复用 | `src/model_provider/crypto.rs`（既有） | §8.7、§9 #16：跨簇复用，无需新建 |
| 助手绑定 MCP（DB 列） | `src/domain/assistant/store.rs`（既有，扩展） | §2.3：共置在 assistant 簇 |
| Agent 注入 MCP Toolset | `src/agent/custom.rs`（既有，扩展） | §3 Q2：Agent 行为 |
| AppDeps 新字段 `mcp_manager` | `src/bootstrap.rs`（AppDeps；`src/server/mod.rs` 仅 `pub use ... as AppState` 别名） | §5.2：跨切依赖 ≥3 个 |

> **重要**：本项目当前 AppState 接近 §5.3 Level 3（≥10 字段）。MCP 管理器作为**单一服务**加入，无需拆子 struct；后续若再增 2 项跨切服务，应启动 Level 3 拆分。

---

## 3. 领域模型设计

### 3.1 核心类型（`src/domain/mcp/models.rs`）

```rust
/// MCP 传输方式（DB 存 SMALLINT，见 §8.3）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportKind {
    Stdio = 1,            // 子进程（stdio 传输）
    StreamableHttp = 2,   // 远程 HTTP（streamable-http 传输）
}

/// MCP Server 运行状态（运行时探测，不落库）
///
/// 状态机（探测策略见 §5.3）：
/// ```text
/// Disconnected ─(首次 get_toolsets)─> Connecting ─成功─> Healthy
///                                          │
///                                          └失败─> Unhealthy ─(重连成功)─> Healthy
/// Healthy ─(ping 失败×1)─> Degraded ─(ping 失败×2)─> Unhealthy + 触发重连
/// ```
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ServerHealth {
    /// 从未连接过（status=Enabled 但未被任何会话使用）
    Unknown,
    /// 当前在线，工具清单可用
    Healthy {
        tools_count: usize,
        last_check: String,   // RFC3339，前端展示"X 秒前"
    },
    /// 单次 ping 失败（容忍中，仍在用）；前端显示黄色
    Degraded {
        consecutive_failures: u8,
        last_check: String,
    },
    /// 连续失败超阈值，已触发重连；前端显示红色
    Unhealthy {
        reason: String,       // 截断的错误描述（不含敏感字段）
        last_check: String,
    },
}

/// 领域实体：MCP Server 配置（含解密后的敏感字段，仅存活于内存）
#[derive(Debug, Clone)]
pub struct McpServer {
    pub id: String,
    pub name: String,                  // 用户可读名，如 "GitHub MCP"
    pub slug: String,                  // 工具命名空间标识（创建时后端生成，不可变）：mcp__{slug}__{tool}
    pub transport: TransportKind,
    /// stdio: 可执行命令，如 ["npx", "-y", "@modelcontextprotocol/server-github"]
    /// http : 远程 URL，如 "https://mcp.example.com/mcp"
    pub endpoint: String,              // 命令整体或 URL
    /// stdio: 启动参数（JSON 数组）；http: 额外 query 参数
    pub args: Vec<String>,
    /// 环境变量（KEY=VALUE）；value 中可能含密钥，DB 存密文，内存存明文
    pub env: HashMap<String, String>,
    /// http 专用：自定义请求头（如 Authorization）
    pub headers: HashMap<String, String>,
    pub status: Status,                // 复用 model_provider::enums::Status (0/1)
    pub tool_timeout_secs: i64,        // 单次工具调用超时（秒），默认 60，界面可配（防卡死阻塞 SSE）
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
```

### 3.2 DTO（`src/domain/mcp/dto.rs`）

> **安全约定（与 model_provider 一致）**：`env` / `headers` 中的敏感 value 在**写入**时接收明文，在**读取**时仅返回掩码（`****<末4位>`），明文永不外泄。

```rust
#[derive(Debug, Deserialize)]
pub struct CreateMcpServerInput {
    pub name: String,
    pub transport: TransportKind,
    pub endpoint: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,      // 明文传入，加密存储
    #[serde(default)]
    pub headers: HashMap<String, String>,  // 明文传入，加密存储
    #[serde(default)]
    pub status: Status,
}

#[derive(Debug, Deserialize)]
pub struct UpdateMcpServerInput {
    pub name: String,
    pub transport: TransportKind,
    pub endpoint: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// `Some`: 覆盖；`None`: 保持原值（前端只显式传需要改的）
    pub env: Option<HashMap<String, String>>,
    pub headers: Option<HashMap<String, String>>,
    #[serde(default)]
    pub status: Status,
}

/// 响应：env/headers value 已脱敏
#[derive(Debug, Serialize)]
pub struct McpServerResponse {
    pub id: String,
    pub name: String,
    pub transport: TransportKind,        // 序列化为 i16（与 Status 同风格）
    pub endpoint: String,
    pub args: Vec<String>,
    /// value 已掩码，如 "****abcd"
    pub env: HashMap<String, String>,
    pub headers: HashMap<String, String>,
    pub status: Status,
    pub created_at: String,
    pub updated_at: String,
    /// 运行时探测结果（list 时附带，可能为 Unknown）
    pub health: ServerHealth,
}

#[derive(Debug, Serialize)]
pub struct McpToolInfo {
    pub server_id: String,
    pub server_name: String,
    pub tool_name: String,        // MCP 工具原始名
    pub namespaced_name: String,  // 注入 Agent 时用的名：mcp__{server_slug}__{tool_name}
    pub description: String,
    pub input_schema: serde_json::Value,
}
```

---

## 4. 数据库设计（遵循 §8）

### 4.1 表结构（`migrations/schema.sql`）

> 注：Phase A 后建表 DDL 统一外移到 `migrations/schema.sql`，store 不再内联 `ensure_schema()`（见架构 §8.5）。

```sql
CREATE TABLE IF NOT EXISTS mcp_servers (
    id              VARCHAR(36)   PRIMARY KEY,
    name            VARCHAR(128)  NOT NULL,
    slug            VARCHAR(64)   NOT NULL,        -- 用于工具命名空间，仅 [a-z0-9_]
    transport       SMALLINT      NOT NULL,        -- 1=stdio, 2=streamable_http
    endpoint        VARCHAR(1024) NOT NULL,
    args            TEXT          NOT NULL DEFAULT '[]',     -- JSON 数组
    env_enc         TEXT          NOT NULL DEFAULT '',       -- AES 加密的 JSON map（整体加密）
    env_mask        TEXT          NOT NULL DEFAULT '{}',     -- 脱敏后的掩码 JSON（前端展示用）
    headers_enc     TEXT          NOT NULL DEFAULT '',       -- AES 加密的 JSON map
    headers_mask    TEXT          NOT NULL DEFAULT '{}',     -- 脱敏后的掩码 JSON
    status          SMALLINT      NOT NULL DEFAULT 1,
    tool_timeout_secs INT         NOT NULL DEFAULT 60,  -- 单次工具调用超时（秒），界面可配
    created_at      TIMESTAMPTZ   NOT NULL DEFAULT NOW(),
    updated_at      TIMESTAMPTZ   NOT NULL DEFAULT NOW(),
    CONSTRAINT uq_mcp_servers_slug UNIQUE (slug),
    CONSTRAINT chk_mcp_transport CHECK (transport IN (1, 2)),
    CONSTRAINT chk_mcp_status   CHECK (status   IN (0, 1))
);
CREATE INDEX IF NOT EXISTS idx_mcp_servers_status ON mcp_servers(status);
```

**合规检查（对照 §8）**：
- ✅ 主键 `VARCHAR(36)` + 应用层 `Uuid::now_v7().to_string()`（§8.1）
- ✅ 枚举 `SMALLINT`（§8.3）
- ✅ 复杂结构 `TEXT` 存 JSON（§8.2）—— `args` / `env_mask` / `headers_mask`
- ✅ 敏感字段 `AesCodec` 加密（§8.7）—— `env_enc` / `headers_enc`
- ✅ 时间 `TIMESTAMPTZ DEFAULT NOW()`（§8.4）
- ✅ 幂等 `CREATE TABLE IF NOT EXISTS`（§8.5）
- ✅ `slug` 唯一约束保证工具命名空间稳定

> **为何 `slug` 单独建列而不从 `name` 派生？** 工具命名空间 `mcp__{slug}__{tool}` 一旦确定就**不可变**（否则历史会话的工具调用记录会失配），因此 `slug` 在创建时由后端生成（snake_case + 随机后缀），更新接口**不接受** slug 字段。

### 4.2 助手绑定扩展（`src/domain/assistant/`）

`assistants` 表**新增一列**（`ALTER TABLE` 幂等升级写入 `migrations/schema.sql`，Phase A 后 store 不再内联建表 / 加列）：

```sql
ALTER TABLE assistants ADD COLUMN IF NOT EXISTS enabled_mcps TEXT NOT NULL DEFAULT '[]';
-- JSON 数组，存 mcp_servers.id，如 ["01H...", "01H..."]
```

助手领域模型 [models.rs](../../src/domain/assistant/models.rs) `Assistant` struct 同步新增 `pub enabled_mcps: Vec<String>` 字段，`AssistantRow` 派生并加 `#[sql_type = "Text"]` 映射。`insert` / `update_custom` 的 SQL 与 bind 列表同步追加一列。

---

## 5. 领域服务：McpManager（`src/domain/mcp/manager.rs`）

### 5.1 职责

```rust
pub struct McpManager {
    pool: DbPool,
    codec: AesCodec,                          // 复用 model_provider::crypto
    clients: RwLock<HashMap<String, Arc<McpClientEntry>>>,  // server_id → 连接
}

/// 一个已建立的 MCP 连接（含 stdio 子进程或 http client）
struct McpClientEntry {
    /// adk-rust 的 Toolset 包装，供 Agent 直接使用
    toolset: Arc<dyn adk_rust::Toolset>,
    /// 健康探测时缓存的工具清单
    tools_cache: RwLock<Vec<McpToolInfo>>,
    /// 当前健康状态 + 最近一次探测时间 + 连续失败计数
    health: RwLock<ConnectionHealth>,
    /// stdio 模式下持有子进程句柄，drop 时自动 kill
    _child_guard: Option<tokio::process::Child>,
}

/// 连接级运行时状态（与 ServerHealth 的区别：这是内部记账，ServerHealth 是对外投影）
#[derive(Debug, Clone)]
struct ConnectionHealth {
    state: HealthState,             // Disconnected/Healthy/Degraded/Unhealthy
    consecutive_failures: u8,       // 连续 ping 失败次数，成功则归零
    last_check: Option<Instant>,    // 最近一次 ping 时刻（用于 TTL 判断）
    last_tools_count: usize,        // 最近一次成功探测的 tools 数
    last_reason: String,            // 最近一次失败的截断 reason
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HealthState {
    Disconnected,  // 未建立连接
    Healthy,
    Degraded,      // 1 次失败
    Unhealthy,     // ≥2 次失败，已触发重连
}
```

### 5.2 公开 API（应用层调用面）

| 方法 | 职责 | 失败策略 |
|------|------|----------|
| `new(pool, security) -> Arc<Self>` | 建表 + 加载所有 `status=Enabled` 的 server，预连接 | 单个失败仅日志，不阻塞启动 |
| `create_server(input) -> Result<String>` | 校验 + 生成 slug + 加密敏感字段 + 入库 + 预连接 | AppError |
| `update_server(id, input) -> Result<UpdateOutcome>` | 更新；若 endpoint/args/env 变更则关闭旧连接重建 | AppError |
| `delete_server(id) -> Result<bool>` | 关闭连接 + 删除记录；同时清理助手的 `enabled_mcps` 引用 | AppError |
| `list_servers() -> Result<Vec<McpServerResponse>>` | 列表（env/headers 已脱敏） | AppError |
| `get_server(id) -> Result<McpServerResponse>` | 单条详情 | AppError |
| `probe_server(id) -> Result<McpServerResponse>` | 主动 list_tools 探测，刷新 tools_cache（响应内含 health） | AppError |
| `list_tools(server_id) -> Result<Vec<McpToolInfo>>` | 单个 server 列工具（触发连接 + 工具发现） | AppError |
| `list_tools_batch(query: &McpToolsQuery) -> Result<HashMap<String, Vec<McpToolInfo>>>` | 批量列工具（按 server_id 分桶，单个失败填空数组） | AppError |
| `build_toolsets(self: &Arc<Self>, mcp_ids: &[String]) -> Vec<Arc<dyn Toolset>>` | 供 Agent 构建时注入；未连接/失败的 server 跳过并告警（async） | 无 Result，尽力返回 |

### 5.3 连接生命周期与健康探测策略

> **决策（2026-06-29）**：采用 **混合模式** —— 仅对"已建立连接（在用）"的 server 定时 ping 保活；未连接的保持 lazy（首次 `get_toolsets` 触发连接）；列表查询走 TTL 软刷新。
>
> 否决方案：
> - ❌ 纯 Lazy：前端列表永远 `Unknown`，体验差；死连接发现滞后
> - ❌ 纯 Push 定时：对未使用的 server（可能占多数）产生无意义流量；stdio 子进程被打扰
> - ❌ 全员 ping：stdio 子进程是独占资源，频繁 ping 干扰工具调用

#### 5.3.1 三类时机

```
① 首次连接（Lazy）
   触发：助手构建调用 get_toolsets(server_ids) 时，对应 server 尚无 client
   动作：connect() → list_tools() 填充 tools_cache + health=Healthy
   失败：health=Unhealthy，记录 reason；本次 get_toolsets 跳过该 server（不抛错）

② 保活探测（Push，仅对已连接的 client）
   触发：后台任务每 120s 遍历 clients map
   动作：对每个 entry 调用 ping（list_tools 轻量版，超时 5s）
        ├─ 成功 → consecutive_failures=0, state=Healthy, 刷新 tools_cache + last_check
        └─ 失败 → consecutive_failures += 1
                  ├─ ==1 → state=Degraded（前端黄色）
                  └─ >=2 → state=Unhealthy（前端红色）+ 立即触发 reconnect()

③ 重连（Backoff）
   触发：state 进入 Unhealthy，或 ping 连续失败
   动作：shutdown 旧连接 → 重新 connect()
        退避序列：30s → 1min → 5min（封顶，直到下次 ping 成功归零）
   失败到底：保持 Unhealthy，记录 reason；不影响其他 server
```

#### 5.3.2 列表查询的 TTL 软刷新（避免 API 阻塞）

`list_servers()` / `get_server(id)` 返回 `health` 字段时：

```
若 last_check 距今 < 30s（HEALTH_TTL）：
    直接返回缓存的 health（不阻塞，API 响应快）
否则：
    本次仍返回旧 health（避免用户等待探测）
    后台 spawn 一次 probe_server(id)（fire-and-forget）
    前端下次刷新即可看到新值
```

> 这与 §6 `probeMcpServer` Mutation 的区别：Mutation 是**用户主动**点击"测试连接"按钮，要求**同步等待**结果；列表查询是浏览行为，不阻塞。

#### 5.3.3 参数汇总（常量定义在 `src/domain/mcp/manager.rs`）

| 常量 | 值 | 说明 |
|------|-----|------|
| `PROBE_INTERVAL` | `120s` | 后台 ping 间隔（>反代 idle timeout × 1.5） |
| `PROBE_TIMEOUT` | `5s` | 单次 ping 超时 |
| `FAILURE_THRESHOLD` | `2` | 进入 Unhealthy 的连续失败次数 |
| `HEALTH_TTL` | `30s` | 列表查询 health 缓存有效期 |
| `RECONNECT_BACKOFF` | ⚠️ **未实现** | 实际无指数退避；连续失败 ≥ `FAILURE_THRESHOLD` → 标记 Unhealthy + 断开连接清缓存，重连在下次使用时惰性重建（`force_reconnect_toolset`） |
| `IDLE_REAP_TTL` | `1800s` | 空闲回收（30min 未用关闭连接，与 [zendriver SESSION_IDLE_TIMEOUT](../../src/infra/zendriver_mcp.rs#L34) 对齐） |

#### 5.3.4 后台任务实现要点

参考 [zendriver_mcp.rs 的 cleanup_idle_sessions 后台模式](../../src/infra/zendriver_mcp.rs#L132)：在 `McpManager::new` 末尾 `tokio::spawn` 一个循环任务，持有 `Arc<weak>` 避免内存泄漏：

```rust
// 伪代码
let weak = Arc::downgrade(&manager);
tokio::spawn(async move {
    let mut ticker = tokio::time::interval(PROBE_INTERVAL);
    loop {
        ticker.tick().await;
        let Some(mgr) = weak.upgrade() else { break };
        mgr.probe_all_connected().await;   // 内部并发 ping 所有 entry
        mgr.reap_idle_clients(IDLE_REAP_TTL).await;
    }
});
```

`probe_all_connected` 内部用 `futures::future::join_all` 并发 ping，单个失败不影响其他；ping 操作走 `tokio::time::timeout(PROBE_TIMEOUT, ...)` 包裹。

#### 5.3.5 并发约束

- stdio 子进程是**独占资源**。同一 server_id 的 client 只有一份（全局共享），多会话复用同一 client（MCP 协议本身支持并发请求）。这与 [zendriver_mcp.rs](../../src/infra/zendriver_mcp.rs) 的"单 Chrome 多 tab"模式一致。
- ping 与工具调用共享同一 client，连接以 `Arc<Mutex<RunningService>>` **串行化**访问；探测改用 `try_lock`——拿不到锁（工具正在调用）直接跳过本轮探测、**不累计失败**，避免抢锁误判而 kill 掉有状态 MCP（如 excel 类）的子进程。

### 5.4 工具命名空间隔离（G4）

为避免不同 MCP Server 暴露同名工具（如多个 server 都有 `search`）冲突，注入 Agent 时对工具名做命名空间改写：

```
原始名：search
注入名：mcp__github__search      （slug=github）
```

实现方式：参考 [zendriver_mcp.rs::BrowserToolset](../../src/infra/zendriver_mcp.rs#L453) 的包装模式，新增 `ManagedMcpToolset`：

```rust
struct ManagedMcpToolset {
    inner: Arc<McpToolset>,
    slug: String,
}

#[async_trait]
impl Toolset for ManagedMcpToolset {
    async fn tools(&self, ctx: Arc<dyn ReadonlyContext>) -> Result<Vec<Arc<dyn Tool>>> {
        let tools = self.inner.tools(ctx).await?;
        Ok(tools.into_iter().map(|t| {
            Arc::new(RenamedTool::new(t, format!("mcp__{}__{}", self.slug, t.name()))) as Arc<dyn Tool>
        }).collect())
    }
    // ...
}
```

> **LLM 系统提示词配合**：Agent 的 instruction 中需追加一段说明 `mcp__*` 工具的用途，本期通过助手 `system_prompt` 自填，不在框架层硬编码。

---

## 6. GraphQL API 设计

所有接口挂在既有 `POST /api/graphql` 单一入口（§2.1、ADR-004），遵循 [graphql.rs](../../src/server/graphql.rs) 的 `Json` 标量透传 + `{code, message, data}` 信封（[response.rs](../../src/server/response.rs)）约定。

### 6.1 Query

| 字段 | 入参 | 返回（data 字段） | 说明 |
|------|------|-------------------|------|
| `mcpServers` | `page?, pageSize?, keyword?` | `{ servers, total, page, pageSize, totalPages }` | 分页列表（含健康状态，env/headers 脱敏）；keyword 模糊匹配 |
| `mcpServer` | `id: String` | `{ server: McpServerResponse }` | 单条详情 |
| `mcpTools` | `input: JSON`（`McpToolsQuery { serverIds }`） | `{ tools: HashMap<server_id, [McpToolInfo]> }` | 批量列工具（按 server 分桶，用于助手勾选预览） |

### 6.2 Mutation

| 字段 | 入参 | 返回 | 说明 |
|------|------|------|------|
| `createMcpServer` | `input: JSON` (CreateMcpServerInput) | `{ server }` | 新建 |
| `updateMcpServer` | `id: String, input: JSON` | `{ server }` | 更新（含敏感字段时整体覆盖） |
| `deleteMcpServer` | `id: String` | `{ deleted }` | 删除（联动清理 assistant.enabled_mcps） |
| `probeMcpServer` | `id: String` | `{ server }` | 主动健康探测（强制重连 + 工具发现，server 含 health） |
| `batchSetMcpStatus` | `input: JSON`（`{ ids?, keyword?, status }`） | `{ affected }` | 批量改状态；`ids` 为 null 时按 keyword 全选匹配项 |
| `batchDeleteMcpServers` | `input: JSON`（`{ ids?, keyword? }`） | `{ affected }` | 批量删除；`ids` 为 null 时按 keyword 全选匹配项 |
| `batchProbeMcpServers` | `input: JSON`（`{ ids }`） | `{ servers }` | 批量探测（仅支持指定 ID 列表，ids 不可为空） |

### 6.3 Resolver 实现要点（参考 [model_provider.rs](../../src/server/model_provider.rs)）

```rust
// src/server/graphql.rs（QueryRoot / MutationRoot 各追加若干方法）

async fn mcp_servers(
    &self, ctx: &Context<'_>,
    page: Option<usize>, page_size: Option<usize>, keyword: Option<String>,
) -> Json {
    Json(super::mcp::list_servers_paged(state_of(ctx), page, page_size, keyword).await)
}
async fn mcp_server(&self, ctx: &Context<'_>, id: String) -> Json {
    Json(super::mcp::get_server(state_of(ctx), &id).await)
}
async fn mcp_tools(&self, ctx: &Context<'_>, input: Json) -> Json {
    // input 在 handler 内解析为 McpToolsQuery，内部走 list_tools_batch
    Json(super::mcp::list_tools(state_of(ctx), input.0).await)
}
// Mutation 省略，结构与 model_provider 一致（含 batchSetMcpStatus / batchDeleteMcpServers / batchProbeMcpServers）
```

错误码映射复用 [response::code](../../src/server/response.rs#L37)：
- 入参非法 → `INVALID_PARAMS (1001)` / `PARSE_ERROR (1002)`
- 资源不存在 → `NOT_FOUND (2002)`
- slug 冲突 / 子进程启动失败 → `BUSINESS (2001)`
- DB 不可用 → `DATABASE (3001)`
- MCP 连接/探测失败 → `NETWORK (4001)`

---

## 7. 与 Agent 集成（`src/agent/custom.rs`）

### 7.1 注入点

在 [build_custom_agent](../../src/agent/custom.rs#L64) 中，`assistant.enabled_mcps` 非空时，为每个 server_id 获取 `toolset` 并通过 `builder.toolset()` 注册：

```rust
// 伪代码：build_custom_agent 内
if !assistant.enabled_mcps.is_empty() {
    if let Some(mgr) = mcp_manager.as_ref() {
        let toolsets = mgr.get_toolsets(&assistant.enabled_mcps);
        for ts in toolsets {
            let wrapped = crate::tools::wrap_toolset_with_truncation(
                Some(ts), cfg.context.tool_max_output_bytes,
            );
            if let Some(ts) = wrapped {
                builder = builder.toolset(ts);
            }
        }
    }
}
```

`build_agent_for_session` / `build_builtin` 签名需追加 `mcp_manager: Option<Arc<McpManager>>` 参数（§5.5 AppDeps 注入）。`browser` 等内置助手暂不接入 MCP（保留硬编码行为）。

### 7.2 工具白名单校验

`enabled_mcps` 不走 [registry.rs](../../src/tools/registry.rs) 的白名单（那是内置工具的），而是**独立校验**：server 必须 `status=Enabled` 且健康。前端 `tools` Query（列可勾选工具）扩展返回 MCP 类目，分组展示：

```json
{
  "builtin": [ { "key": "web_search", ... } ],
  "mcp": [ { "server_id": "...", "server_name": "GitHub", "tools": [...] } ]
}
```

---

## 8. 传输层适配（`src/domain/mcp/transport.rs`）

封装 `rmcp` 两种标准客户端 transport，对 `McpManager` 屏蔽差异：

```rust
pub(crate) async fn connect_stdio(
    cmd: &str, args: &[String], env: &HashMap<String, String>,
) -> anyhow::Result<(RunningService<RoleClient, ()>, Child)> { ... }

pub(crate) async fn connect_http(
    url: &str, headers: &HashMap<String, String>,
) -> anyhow::Result<RunningService<RoleClient, ()>> { ... }
```

- stdio：`TokioChildProcess` + `rmcp::serve_client`；`Child` 句柄由 `McpClientEntry::_child_guard` 持有
- http：`StreamableHttpClientTransport`（需在 Cargo.toml 为 `rmcp` 开启 `transport-streamable-http-client` feature）

> **依赖变更（唯一）**：[Cargo.toml](../../Cargo.toml#L25) 的 `rmcp = "1.8.0"` 改为带 feature 列表：
> ```toml
> rmcp = { version = "1.8.0", features = ["client", "transport-child-process", "transport-streamable-http-client", "transport-streamable-http-client-reqwest"] }
> ```

---

## 9. AppState 集成（§5）

`AppDeps` 定义在 [src/bootstrap.rs](../../src/bootstrap.rs)（[src/server/mod.rs](../../src/server/mod.rs#L81) 仅 `pub use crate::bootstrap::AppDeps as AppState;` 别名复用，保留 `AppState` 名以减少历史 handler 签名改动），追加字段：

```rust
pub struct AppDeps {
    // ... 既有字段
    /// MCP Server 管理器（连接池 + 健康探测；DB 不可用时为 None，MCP 功能整体降级）
    pub mcp_manager: Option<Arc<crate::domain::mcp::McpManager>>,
}
```

初始化在 [`build_app_deps`](../../src/bootstrap.rs) 中（非 `run()`，组合根分离见架构 §3 Q6/§11），先建 `McpServerStore` 再构造 `McpManager`，并启动后台探测循环：

```rust
let mcp_manager = match &db_pool {
    Some(pool) => match McpServerStore::new(pool.clone(), &cfg.security).await {
        Ok(store) => match McpManager::new(store).await {
            Ok(mgr) => { mgr.start_probe_loop(); Some(mgr) }
            Err(e) => { tracing::warn!("[infra] MCP 管理器初始化失败({e})"); None }
        },
        Err(e) => { tracing::warn!("[infra] MCP Store 初始化失败({e})"); None }
    },
    None => None,
};
```

> **降级策略（与既有体系一致）**：DB 不可用 → `mcp_manager = None` → 所有 MCP GraphQL 接口返回 `DATABASE (3001)`；既有助手功能不受影响。
>
> 另：`build_app_deps` 在管理器就绪后会按 `cfg.mcp.seeds` upsert 种子服务器（按 `slug` 匹配，存在则更新 endpoint/args/transport，不存在则创建），实现配置驱动的预置 MCP 注册。

---

## 10. 安全设计

| 威胁 | 缓解措施 | 依据 |
|------|----------|------|
| 敏感 token 泄露（DB 被拖） | `env_enc` / `headers_enc` 用 AesCodec 加密；明文仅在内存 | §8.7 |
| 日志泄露 token | `tracing::info!` 严禁打印 env/headers 全量；探测失败只记 `reason` | §7 |
| 任意命令执行（stdio 注入） | `endpoint` 仅允许**白名单可执行文件名**或绝对路径；`args` 不走 shell（用 `Command::arg` 逐个传） | 新增约束 |
| SSRF（http 传输） | 校验 endpoint 必须为 `https://` 或 `http://localhost`；可选 IP 黑名单（本期不做，留 TODO） | 新增约束 |
| 工具名冲突污染 LLM | `mcp__{slug}__` 前缀强制隔离 | §5.4 |
| 子进程僵尸 | `McpClientEntry` 持有 `Child`，drop 时 `kill` + `wait`；后台巡检 | §5.3 |
| 探测 reason 泄露敏感字段 | `Unhealthy.reason` 仅记错误类型 + 截断（≤200 字符）；env/headers 的值绝不进入 reason；`tracing::warn!` 同样脱敏 | §5.3、§7 |

---

## 11. 测试策略

| 层 | 测试 | 位置 |
|----|------|------|
| 单元 | `TransportKind` / `Status` 枚举映射、slug 生成、敏感字段掩码 | `src/domain/mcp/{enums,models}.rs` 内 `#[cfg(test)]` |
| 单元 | `ManagedMcpToolset` 工具名改写 | `src/domain/mcp/manager.rs` 内 |
| 单元 | `ConnectionHealth` 状态机转移（Healthy→Degraded→Unhealthy→重连→Healthy） | `src/domain/mcp/manager.rs` 内 |
| 集成 | TTL 软刷新：列表查询不阻塞、过期触发后台 probe（用 `tokio::time::pause` 模拟） | `tests/mcp_test.rs`（新建） |
| 集成 | `McpManager` CRUD（mock DB，参考 `tests/knowledge_test.rs`） | `tests/mcp_test.rs`（新建） |
| 端到端 | stdio 传输：启动 `npx -y @modelcontextprotocol/server-everything` 探测 list_tools | `tests/mcp_stdio_e2e.rs`（mark `#[ignore]`，CI 可选） |
| 端到端 | http 传输：用 `wiremock` 起 mock MCP server | 同上 |
| GraphQL | resolver 信封 `{code,message,data}` 正确性 | `tests/mcp_graphql_test.rs` |

> **既有约束**：测试框架遵循 [tests/](../../tests/) 现有风格（纯 Rust test，无额外 harness）。运行 `cargo test mcp`。

---

## 12. 文件清单（实施时落地）

### 新增

| 文件 | 职责 |
|------|------|
| `src/domain/mcp/mod.rs` | 模块入口（`//!` 文档注释） |
| `src/domain/mcp/enums.rs` | `TransportKind` |
| `src/domain/mcp/models.rs` | `McpServer` / `ServerHealth` |
| `src/domain/mcp/dto.rs` | 请求/响应 DTO |
| `src/domain/mcp/store.rs` | DB CRUD（`McpServerStore`），DDL 内联幂等 |
| `src/domain/mcp/transport.rs` | stdio/http 传输适配（封装 rmcp） |
| `src/domain/mcp/manager.rs` | `McpManager`（连接池 + 业务编排 + `ManagedMcpToolset`） |
| `src/server/mcp.rs` | GraphQL handler 函数（参考 `server/model_provider.rs`） |
| `migrations/schema.sql` | `mcp_servers` 建表记录 |
| `tests/mcp_test.rs` | 集成测试 |

### 修改

| 文件 | 改动 |
|------|------|
| [Cargo.toml](../../Cargo.toml#L25) | `rmcp` 加 features：`client`、`transport-child-process`、`transport-streamable-http-client`、`transport-streamable-http-client-reqwest` |
| [src/domain/mod.rs](../../src/domain/mod.rs) | `pub mod mcp;` |
| [src/domain/assistant/models.rs](../../src/domain/assistant/models.rs) | `Assistant` / `AssistantRow` 新增 `enabled_mcps` 字段 |
| [src/domain/assistant/store.rs](../../src/domain/assistant/store.rs) | `ensure_schema` 追加 `ALTER TABLE`；`insert` / `update_custom` 追加列 |
| [src/server/mod.rs](../../src/server/mod.rs#L39) | `pub(crate) mod mcp;` + `AppState.mcp_manager` + `run()` 初始化 |
| [src/server/graphql.rs](../../src/server/graphql.rs) | QueryRoot / MutationRoot 追加 10 个 resolver（3 Query + 7 Mutation，含 3 个批量接口） |
| [src/agent/custom.rs](../../src/agent/custom.rs) | `build_custom_agent` / `build_agent_for_session` 注入 MCP toolsets |
| [src/server/assistant.rs](../../src/server/assistant.rs) | `list_tools` 扩展返回 MCP 工具分组 |

---

## 13. 风险与取舍

| 风险 | 评估 | 应对 |
|------|------|------|
| `rmcp` feature 开启后编译时间增加 | 中 | 可接受；features 仅影响 transport 子模块 |
| stdio 子进程在容器内权限受限 | 中 | 文档说明；提供 `streamable_http` 作为兜底 |
| 同一 MCP Server 工具数量过多导致 LLM token 爆炸 | 高 | 复用既有 `wrap_toolset_with_truncation`；后续可加"工具白名单" |
| `enabled_mcps` 引用失效（server 被删） | 低 | `delete_server` 内联动清理所有 `assistant.enabled_mcps` |
| AppState 字段数继续膨胀 | 中 | 本期不拆，但记录为 Level 3 拆分触发点（§5.3） |
| MCP 协议演进（spec 版本） | 低 | `rmcp` 已封装；锁定 `=1.8.0` |

---

## 14. 推进路线（建议分 3 个 PR）

| PR | 范围 | 可独立合入 |
|----|------|------------|
| PR-1 | §4 DDL + §3 领域模型 + §5 store（纯领域层，无 UI） | ✅ |
| PR-2 | §8 传输适配 + §5 manager（连接池 + 探测） | ✅（依赖 PR-1） |
| PR-3 | §6 GraphQL + §7 Agent 集成 + §9 AppState | ✅（依赖 PR-1/2） |

每个 PR 均满足 [§10 Code Review Checklist](../architecture.md#10-code-review-checklist) 全部条目。

---

## 15. 待确认问题（Open Questions）

1. **stdio 命令白名单**：是否需要内置一份允许的可执行文件白名单（如 `npx` / `uvx` / `python -m`）？还是完全由运维通过配置 `mcp.allowed_executables` 控制？
2. **MCP 工具 token 预算**：是否在助手维度限制单次会话可注入的 MCP 工具总数上限（如 ≤20）？
3. ~~**健康探测频率**：后台巡检是 push 还是 pull？~~ —— **已决策（2026-06-29）**：采用混合模式，详见 §5.3。要点：
   - 后台每 `120s` ping **已连接**的 client（不在用的 server 不主动探测）
   - 单次 ping 超时 `5s`；连续 `2` 次失败 → `Unhealthy` + 触发重连（退避 30s→1min→5min）
   - 列表查询走 `30s` TTL 软刷新，不阻塞 API
   - `IDLE_REAP_TTL = 1800s` 未用的 client 关闭连接（与 zendriver 对齐）
4. **多用户隔离**：当前所有 MCP Server 全局共享。未来是否需要按用户/组织维度隔离（`owner` 列）？

> 剩余问题不影响接口契约，可在实施阶段决策。
