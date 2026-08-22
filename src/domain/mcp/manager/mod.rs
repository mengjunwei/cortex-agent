//! MCP 管理器：连接池 + 健康探测 + 业务编排
//!
//! 核心职责（架构 §5）：
//! 1. **连接池**：`RwLock<HashMap<id, Arc<McpClientEntry>>>`，惰性建连
//! 2. **健康探测**：Hybrid 策略（设计 §5.3）——后台 120s 探测 + TTL 30s 软刷新
//! 3. **CRUD 编排**：在 Store 之上做缓存失效、连接重建
//! 4. **工具集注入**：为 Agent 提供 `Arc<dyn Toolset>`（含命名空间前缀）
//!
//! 命名空间：工具 `read_file` → `mcp__{slug}__read_file`，与内置工具隔离。
//!
//! 注意：`RunningService` 不实现 `Clone`，故连接池存 `Arc<Mutex<RunningService>>`，
//! 健康探测与 Agent 工具执行共享同一连接。
//!
//! 拆分说明（架构 §4 拆分范例）：本目录由单文件 `manager.rs` 拆分而来，
//! `mod.rs` 仅保留结构体定义、跨子模块共享的内部状态（`McpClientEntry` /
//! `ConnHealth` / `SharedClient` / 探测常量）与对外导出；方法实现按职责下沉到
//! 子模块（同一 `McpManager` 的多个 `impl` 块分散在各文件）：
//! - `crud.rs`     CRUD 编排（create/update/delete/list/batch_*）+ 归属校验
//! - `tools.rs`    工具查询与工具集装配（list_tools/call_tool_by_slug/build_toolsets）
//! - `connect.rs`  连接管理（ensure_connected/fetch_tools/evict/TTL 软刷新）
//! - `probe.rs`    健康探测（手动探测 + 启动探测 + 后台 probe 循环）
//! - `sanitize.rs` 失败原因清洗 + 单元测试

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

mod connect;
mod crud;
mod probe;
mod sanitize;
mod toolset;
mod tools;

pub use toolset::ManagedMcpToolset;

use rmcp::RoleClient;
use rmcp::service::RunningService;
use tokio::sync::{Mutex, RwLock};

use crate::domain::mcp::models::ServerHealth;
use crate::domain::mcp::store::McpServerStore;
use crate::error::AppError;

// ============================== 探测参数 ==============================

/// 后台探测间隔（Hybrid 策略：120s 推送式保活）
const PROBE_INTERVAL: Duration = Duration::from_secs(120);
/// 单次探测超时
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// 连续失败阈值：超过则标记 Unhealthy
const FAILURE_THRESHOLD: u8 = 2;
/// 工具清单缓存 TTL（list_all_tools 的软刷新窗口）
const HEALTH_TTL: Duration = Duration::from_secs(30);
/// 空闲回收：30 分钟无 get_toolsets 调用则断开连接
const IDLE_REAP_TTL: Duration = Duration::from_secs(1800);
/// 单次工具调用超时上限（秒）：tool_timeout_secs 界面可配，无封顶的极端值
/// （如误填 i64::MAX）会把共享连接锁永久钉死。1 小时对齐 wait_agent 的
/// WAIT_MAX_TIMEOUT_MS 上限。
const TOOL_TIMEOUT_MAX_SECS: i64 = 3600;

/// 配置的 tool_timeout_secs → Duration，夹到 [1s, TOOL_TIMEOUT_MAX_SECS]。
fn tool_timeout_duration(secs: i64) -> Duration {
    Duration::from_secs(secs.clamp(1, TOOL_TIMEOUT_MAX_SECS) as u64)
}

/// 共享连接类型别名
type SharedClient = Arc<Mutex<RunningService<RoleClient, ()>>>;

// ============================== 内部状态结构 ==============================

/// 连接健康子状态（内部记账）
#[derive(Debug, Clone, PartialEq, Default)]
enum ConnHealth {
    #[default]
    Disconnected,
    Connecting,
    Healthy,
    Degraded {
        consecutive_failures: u8,
    },
    Unhealthy,
}

/// 单个 MCP Server 的缓存条目
struct McpClientEntry {
    #[allow(dead_code)]
    server_id: String,
    #[allow(dead_code)]
    slug: String,
    /// 共享连接（None=已断开）
    client: RwLock<Option<SharedClient>>,
    health: RwLock<ConnHealth>,
    last_probe: RwLock<Option<chrono::DateTime<chrono::Utc>>>,
    last_tools_fetch: RwLock<Option<Instant>>,
    cached_tools: RwLock<Vec<crate::domain::mcp::models::McpToolInfo>>,
    failure_reason: RwLock<String>,
}

impl McpClientEntry {
    fn new(server_id: String, slug: String) -> Self {
        Self {
            server_id,
            slug,
            client: RwLock::new(None),
            health: RwLock::new(ConnHealth::Disconnected),
            last_probe: RwLock::new(None),
            last_tools_fetch: RwLock::new(None),
            cached_tools: RwLock::new(Vec::new()),
            failure_reason: RwLock::new(String::new()),
        }
    }

    async fn to_server_health(&self) -> ServerHealth {
        let h = self.health.read().await.clone();
        let last = self
            .last_probe
            .read()
            .await
            .map(|t| t.to_rfc3339())
            .unwrap_or_default();
        match h {
            ConnHealth::Disconnected | ConnHealth::Connecting => ServerHealth::Unknown,
            ConnHealth::Healthy => {
                let count = self.cached_tools.read().await.len();
                ServerHealth::Healthy {
                    tools_count: count,
                    last_check: last,
                }
            }
            ConnHealth::Degraded {
                consecutive_failures,
            } => ServerHealth::Degraded {
                consecutive_failures,
                last_check: last,
            },
            ConnHealth::Unhealthy => {
                let reason = self.failure_reason.read().await.clone();
                ServerHealth::Unhealthy {
                    reason,
                    last_check: last,
                }
            }
        }
    }
}

// ============================== McpManager ==============================

/// MCP 管理器
pub struct McpManager {
    store: Arc<McpServerStore>,
    clients: RwLock<HashMap<String, Arc<McpClientEntry>>>,
    probe_started: std::sync::atomic::AtomicBool,
    /// stdio 子进程是否继承宿主全量环境（config `[mcp] stdio_inherit_env`，
    /// 默认 false = 白名单收紧；见 transport::resolve_child_env）
    stdio_inherit_env: bool,
}

impl McpManager {
    pub async fn new(
        store: Arc<McpServerStore>,
        stdio_inherit_env: bool,
    ) -> Result<Arc<Self>, AppError> {
        Ok(Arc::new(Self {
            store,
            clients: RwLock::new(HashMap::new()),
            probe_started: std::sync::atomic::AtomicBool::new(false),
            stdio_inherit_env,
        }))
    }

    pub fn store(&self) -> &Arc<McpServerStore> {
        &self.store
    }
}

// ============================== Tests ==============================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conn_health_default_is_disconnected() {
        assert_eq!(ConnHealth::default(), ConnHealth::Disconnected);
    }
}
