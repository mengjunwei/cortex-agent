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

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use adk_rust::tool::{Tool, Toolset};
use adk_rust::{AdkError, ReadonlyContext, Result as AdkResult, ToolContext};
use async_trait::async_trait;
use rmcp::RoleClient;
use rmcp::model::CallToolRequestParams;
use rmcp::service::RunningService;
use serde_json::Value;
use tokio::sync::{Mutex, RwLock};
use tokio::time::sleep;

use crate::domain::mcp::dto::{
    CreateMcpServerInput, McpServerResponse, McpToolsQuery, UpdateMcpServerInput,
};
use crate::domain::mcp::enums::Status;
use crate::domain::mcp::models::{McpServer, McpToolInfo, ServerHealth, namespaced_tool_name};
use crate::domain::mcp::store::McpServerStore;
use crate::domain::mcp::transport;
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
    cached_tools: RwLock<Vec<McpToolInfo>>,
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
}

impl McpManager {
    pub async fn new(store: Arc<McpServerStore>) -> Result<Arc<Self>, AppError> {
        Ok(Arc::new(Self {
            store,
            clients: RwLock::new(HashMap::new()),
            probe_started: std::sync::atomic::AtomicBool::new(false),
        }))
    }

    pub fn store(&self) -> &Arc<McpServerStore> {
        &self.store
    }

    // ===== CRUD 编排 =====

    pub async fn create_server(
        &self,
        input: &CreateMcpServerInput,
    ) -> Result<McpServerResponse, AppError> {
        let server = self.store.create(input).await?;
        Ok(McpServerStore::to_response(&server, ServerHealth::Unknown))
    }

    pub async fn update_server(
        &self,
        id: &str,
        input: &UpdateMcpServerInput,
    ) -> Result<Option<McpServerResponse>, AppError> {
        let server = self.store.update(id, input).await?;
        match server {
            Some(s) => {
                self.evict(&s.id).await;
                let health = self.peek_health(&s.id).await;
                Ok(Some(McpServerStore::to_response(&s, health)))
            }
            None => Ok(None),
        }
    }

    pub async fn delete_server(&self, id: &str) -> Result<bool, AppError> {
        self.evict(id).await;
        self.store.delete(id).await
    }

    pub async fn list_servers(&self) -> Result<Vec<McpServerResponse>, AppError> {
        let servers = self.store.list_all().await?;
        let mut out = Vec::with_capacity(servers.len());
        for s in servers {
            let health = if s.status == Status::Enabled {
                self.peek_health(&s.id).await
            } else {
                ServerHealth::Unknown
            };
            out.push(McpServerStore::to_response(&s, health));
        }
        Ok(out)
    }

    pub async fn list_servers_paged(
        &self,
        page: usize,
        page_size: usize,
        keyword: Option<&str>,
    ) -> Result<(Vec<McpServerResponse>, i64), AppError> {
        let (servers, total) = self.store.list_paged(page, page_size, keyword).await?;
        let mut out = Vec::with_capacity(servers.len());
        for s in servers {
            let health = if s.status == Status::Enabled {
                self.peek_health(&s.id).await
            } else {
                ServerHealth::Unknown
            };
            out.push(McpServerStore::to_response(&s, health));
        }
        Ok((out, total))
    }

    pub async fn get_server(&self, id: &str) -> Result<Option<McpServerResponse>, AppError> {
        let server = self.store.get_by_id(id).await?;
        match server {
            Some(s) => {
                let health = if s.status == Status::Enabled {
                    self.peek_health(&s.id).await
                } else {
                    ServerHealth::Unknown
                };
                Ok(Some(McpServerStore::to_response(&s, health)))
            }
            None => Ok(None),
        }
    }

    pub async fn batch_set_status(
        &self,
        ids: Option<&[String]>,
        keyword: Option<&str>,
        status_val: i16,
    ) -> Result<usize, AppError> {
        match ids {
            Some(id_list) => {
                self.store.set_status_batch(id_list, status_val).await?;
                // 清理被改动的连接
                for id in id_list {
                    self.evict(id).await;
                }
                Ok(id_list.len())
            }
            None => {
                let affected = self.store.set_status_by_filter(keyword, status_val).await?;
                // 清理所有连接（安全起见）
                self.evict_all().await;
                Ok(affected)
            }
        }
    }

    pub async fn batch_delete(
        &self,
        ids: Option<&[String]>,
        keyword: Option<&str>,
    ) -> Result<usize, AppError> {
        match ids {
            Some(id_list) => {
                for id in id_list {
                    self.evict(id).await;
                }
                self.store.delete_batch(id_list).await
            }
            None => {
                self.evict_all().await;
                self.store.delete_by_filter(keyword).await
            }
        }
    }

    pub async fn batch_probe(&self, ids: &[String]) -> Vec<McpServerResponse> {
        let mut results = Vec::with_capacity(ids.len());
        for id in ids {
            match self.probe_server(id).await {
                Ok(resp) => results.push(resp),
                Err(e) => tracing::warn!("[MCP] 批量探测失败 id={id}: {e}"),
            }
        }
        results
    }

    // ===== 工具查询 & 连接探测 =====

    pub async fn list_tools(&self, server_id: &str) -> Result<Vec<McpToolInfo>, AppError> {
        let server = self
            .store
            .get_by_id(server_id)
            .await?
            .ok_or_else(|| AppError::NotFoundError("MCP Server 不存在".into()))?;
        self.ensure_connected_and_list(&server).await
    }

    pub async fn list_tools_batch(
        &self,
        query: &McpToolsQuery,
    ) -> Result<HashMap<String, Vec<McpToolInfo>>, AppError> {
        let mut out = HashMap::new();
        for id in &query.server_ids {
            match self.list_tools(id).await {
                Ok(tools) => {
                    out.insert(id.clone(), tools);
                }
                Err(e) => {
                    tracing::warn!("[MCP] list_tools 失败 server_id={id}: {e}");
                    out.insert(id.clone(), Vec::new());
                }
            }
        }
        Ok(out)
    }

    pub async fn probe_server(&self, server_id: &str) -> Result<McpServerResponse, AppError> {
        let server = self
            .store
            .get_by_id(server_id)
            .await?
            .ok_or_else(|| AppError::NotFoundError("MCP Server 不存在".into()))?;
        self.evict(&server.id).await;
        match self.ensure_connected_and_list(&server).await {
            Ok(tools) => {
                let health = self.peek_health(&server.id).await;
                let resp = McpServerStore::to_response(&server, health);
                tracing::info!(
                    "[MCP] 手动探测成功 slug={} tools={}",
                    server.slug,
                    tools.len()
                );
                Ok(resp)
            }
            Err(e) => {
                if let Some(entry) = self.clients.read().await.get(&server.id) {
                    let reason = sanitize_reason(&e.to_string());
                    *entry.health.write().await = ConnHealth::Unhealthy;
                    *entry.failure_reason.write().await = reason;
                    *entry.last_probe.write().await = Some(chrono::Utc::now());
                }
                Err(e)
            }
        }
    }

    /// 启动后立即对所有「已启用」的服务做首轮探测。
    ///
    /// 解决问题：启动时 `clients` map 为空，列表接口返回的 health 全部是 Unknown、
    /// 工具数为 0，用户必须手动点「探测」才能看到状态。本方法在后台异步执行，
    /// 不阻塞 HTTP 服务启动；探测完成后列表自动刷新出真实状态。
    ///
    /// 每个 server 独立带超时，单个失败不阻塞其他。
    pub async fn probe_all_enabled(&self) {
        let servers = match self.store.list_all().await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("[MCP] 启动探测：读取服务列表失败: {e}");
                return;
            }
        };
        let enabled: Vec<_> = servers
            .into_iter()
            .filter(|s| s.status == Status::Enabled)
            .collect();
        if enabled.is_empty() {
            return;
        }

        tracing::info!("[MCP] 启动探测：开始探测 {} 个已启用服务", enabled.len());

        let mut ok = 0usize;
        let mut fail = 0usize;
        for s in &enabled {
            let timeout_result =
                tokio::time::timeout(Duration::from_secs(15), self.probe_server(&s.id)).await;
            match timeout_result {
                Ok(Ok(_)) => ok += 1,
                Ok(Err(e)) => {
                    tracing::warn!("[MCP] 启动探测：服务「{}」失败: {}", s.slug, e);
                    fail += 1;
                }
                Err(_) => {
                    tracing::warn!("[MCP] 启动探测：服务「{}」超时(15s)", s.slug);
                    fail += 1;
                }
            }
        }
        tracing::info!("[MCP] 启动探测完成：成功 {ok}，失败 {fail}");
    }

    // ===== Agent 工具集注入 =====

    pub async fn build_toolsets(self: &Arc<Self>, mcp_ids: &[String]) -> Vec<Arc<dyn Toolset>> {
        let mut toolsets = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for id in mcp_ids {
            // 去重，防止 enabled_mcps 中重复 ID 导致同名工具集
            if !seen.insert(id.clone()) {
                continue;
            }
            if let Some(ts) = self.build_one_toolset(id).await {
                toolsets.push(ts);
            }
        }
        tracing::info!(
            "[MCP] build_toolsets 完成: 请求 {} 个 server，成功注入 {} 个工具集",
            mcp_ids.len(),
            toolsets.len()
        );
        toolsets
    }

    /// 为单个 MCP server 构建工具集；server 不存在/未启用/连接失败时返回 None。
    async fn build_one_toolset(&self, id: &str) -> Option<Arc<dyn Toolset>> {
        let server = match self.store.get_by_id(id).await {
            Ok(Some(s)) if s.status == Status::Enabled => s,
            _ => {
                tracing::error!(
                    "[MCP] build_toolsets: server_id={} 未找到或未启用，跳过",
                    id
                );
                return None;
            }
        };
        match self.ensure_connected_and_list(&server).await {
            Ok(tools) => {
                tracing::info!(
                    "[MCP] build_toolsets: server {} 已连接，工具数={}",
                    server.slug,
                    tools.len()
                );
            }
            Err(e) => {
                tracing::error!(
                    "[MCP] build_toolsets: server {} 连接失败，跳过: {e}",
                    server.slug
                );
                return None;
            }
        }
        // 获取共享连接的 Arc 克隆（先复制 entry 指针释放读锁，再读 client）
        let entry = {
            let clients = self.clients.read().await;
            clients.get(&server.id).cloned()
        };
        let client = match entry {
            Some(e) => e.client.read().await.clone(),
            None => None,
        };
        if let Some(client) = client {
            return Some(Arc::new(ManagedMcpToolset::new(
                client,
                server.slug.clone(),
                Duration::from_secs(server.tool_timeout_secs.max(1) as u64),
            )) as Arc<dyn Toolset>);
        }
        // client 为空（竞态）：兜底强制重连
        self.force_reconnect_toolset(&server).await
    }

    /// client 为空时的兜底：强制重连一次并构造工具集；仍失败返回 None。
    async fn force_reconnect_toolset(&self, server: &McpServer) -> Option<Arc<dyn Toolset>> {
        tracing::warn!(
            "[MCP] build_toolsets: server {} client 为空（竞态），尝试强制重连",
            server.slug
        );
        let Some(entry) = self.get_or_create_entry(server).await.ok() else {
            tracing::error!(
                "[MCP] build_toolsets: server {} 强制重连仍失败，工具集丢失！",
                server.slug
            );
            return None;
        };
        if self.ensure_connected(server, &entry).await.is_ok() {
            if let Some(client) = entry.client.read().await.clone() {
                tracing::info!("[MCP] build_toolsets: server {} 强制重连成功", server.slug);
                return Some(Arc::new(ManagedMcpToolset::new(
                    client,
                    server.slug.clone(),
                    Duration::from_secs(server.tool_timeout_secs.max(1) as u64),
                )) as Arc<dyn Toolset>);
            }
        }
        tracing::error!(
            "[MCP] build_toolsets: server {} 强制重连仍失败，工具集丢失！",
            server.slug
        );
        None
    }

    // ===== 内部：连接管理 =====

    async fn ensure_connected_and_list(
        &self,
        server: &McpServer,
    ) -> Result<Vec<McpToolInfo>, AppError> {
        let entry = self.get_or_create_entry(server).await?;

        // TTL 软刷新：仅在 client 仍然存在时才信任缓存，
        // 否则 probe_one 可能已 take 走 client（标记 Unhealthy），
        // 缓存命中会导致 build_toolsets 拿到 None client 而静默丢失工具集
        {
            let last_fetch = entry.last_tools_fetch.read().await;
            if let Some(t) = *last_fetch {
                if t.elapsed() < HEALTH_TTL {
                    let client_guard = entry.client.read().await;
                    if client_guard.is_some() {
                        let tools = entry.cached_tools.read().await;
                        if !tools.is_empty() {
                            return Ok(tools.clone());
                        }
                    } else {
                        tracing::warn!(
                            "[MCP] slug={} TTL 缓存命中但 client 已断开，强制重连",
                            server.slug
                        );
                    }
                }
            }
        }

        self.ensure_connected(server, &entry).await?;
        self.fetch_tools(server, &entry).await
    }

    async fn get_or_create_entry(
        &self,
        server: &McpServer,
    ) -> Result<Arc<McpClientEntry>, AppError> {
        let mut clients = self.clients.write().await;
        if let Some(e) = clients.get(&server.id) {
            return Ok(e.clone());
        }
        let entry = Arc::new(McpClientEntry::new(server.id.clone(), server.slug.clone()));
        clients.insert(server.id.clone(), entry.clone());
        Ok(entry)
    }

    async fn ensure_connected(
        &self,
        server: &McpServer,
        entry: &Arc<McpClientEntry>,
    ) -> Result<(), AppError> {
        {
            let client = entry.client.read().await;
            if client.is_some() {
                return Ok(());
            }
        }

        *entry.health.write().await = ConnHealth::Connecting;

        match transport::connect(server).await {
            Ok(running) => {
                let shared = Arc::new(Mutex::new(running));
                *entry.client.write().await = Some(shared);
                *entry.health.write().await = ConnHealth::Healthy;
                *entry.failure_reason.write().await = String::new();
                *entry.last_probe.write().await = Some(chrono::Utc::now());
                tracing::info!("[MCP] 连接成功 slug={}", server.slug);
                Ok(())
            }
            Err(e) => {
                let reason = sanitize_reason(&e.to_string());
                *entry.health.write().await = ConnHealth::Unhealthy;
                *entry.failure_reason.write().await = reason.clone();
                *entry.last_probe.write().await = Some(chrono::Utc::now());
                tracing::warn!("[MCP] 连接失败 slug={}: {reason}", server.slug);
                Err(e)
            }
        }
    }

    async fn fetch_tools(
        &self,
        server: &McpServer,
        entry: &Arc<McpClientEntry>,
    ) -> Result<Vec<McpToolInfo>, AppError> {
        let client_guard = entry.client.read().await;
        let shared = client_guard
            .as_ref()
            .ok_or_else(|| AppError::BusinessError("MCP 连接未建立".into()))?;
        let shared = shared.clone();
        drop(client_guard);

        let running = shared.lock().await;
        let mcp_tools = running
            .list_all_tools()
            .await
            .map_err(|e| AppError::NetworkError(format!("list_all_tools 失败: {e}")))?;

        let tools: Vec<McpToolInfo> = mcp_tools
            .iter()
            .map(|t| {
                let tool_name = t.name.to_string();
                let namespaced = namespaced_tool_name(&server.slug, &tool_name);
                McpToolInfo {
                    server_id: server.id.clone(),
                    server_name: server.name.clone(),
                    slug: server.slug.clone(),
                    tool_name,
                    namespaced_name: namespaced,
                    description: t
                        .description
                        .as_ref()
                        .map(|d| d.to_string())
                        .unwrap_or_default(),
                    input_schema: Value::Object(t.input_schema.as_ref().clone()),
                }
            })
            .collect();

        *entry.cached_tools.write().await = tools.clone();
        *entry.last_tools_fetch.write().await = Some(Instant::now());
        Ok(tools)
    }

    async fn peek_health(&self, server_id: &str) -> ServerHealth {
        let clients = self.clients.read().await;
        if let Some(entry) = clients.get(server_id) {
            return entry.to_server_health().await;
        }
        ServerHealth::Unknown
    }

    async fn evict(&self, server_id: &str) {
        let entry = {
            let mut clients = self.clients.write().await;
            clients.remove(server_id)
        };
        if let Some(entry) = entry {
            let client = entry.client.write().await.take();
            drop(client); // Arc 析构时若有唯一引用则 RunningService::drop 清理
        }
    }

    async fn evict_all(&self) {
        let ids: Vec<String> = {
            let clients = self.clients.read().await;
            clients.keys().cloned().collect()
        };
        for id in ids {
            self.evict(&id).await;
        }
    }

    pub fn start_probe_loop(self: &Arc<Self>) {
        if self
            .probe_started
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            return;
        }
        let mgr = self.clone();
        tokio::spawn(async move {
            // 启动后立即做首轮探测，让 MCP 列表尽快显示健康状态和工具数
            mgr.probe_all_enabled().await;

            loop {
                sleep(PROBE_INTERVAL).await;
                if let Err(e) = mgr.probe_cycle().await {
                    tracing::warn!("[MCP] 健康探测循环出错: {e}");
                }
            }
        });
        tracing::info!(
            "[MCP] 后台健康探测已启动 (interval={}s)，首轮探测将在启动后立即执行",
            PROBE_INTERVAL.as_secs()
        );
    }

    async fn probe_cycle(&self) -> Result<(), AppError> {
        let snapshot: Vec<(String, Arc<McpClientEntry>)> = {
            let clients = self.clients.read().await;
            clients
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        };

        for (id, entry) in snapshot {
            // 空闲回收
            let last_fetch = *entry.last_tools_fetch.read().await;
            let idle = last_fetch.map(|t| t.elapsed()).unwrap_or(IDLE_REAP_TTL);
            if idle >= IDLE_REAP_TTL {
                tracing::info!("[MCP] 空闲回收 server_id={id}");
                self.evict(&id).await;
                continue;
            }

            let has_client = entry.client.read().await.is_some();
            if !has_client {
                continue;
            }

            self.probe_one(&id, &entry).await;
        }
        Ok(())
    }

    /// 探测单个 entry：用 list_all_tools 做 keepalive
    async fn probe_one(&self, server_id: &str, entry: &Arc<McpClientEntry>) {
        let client = {
            let guard = entry.client.read().await;
            guard.clone()
        };
        let Some(shared) = client else {
            return;
        };

        // 【参考 codex/claurst】非阻塞探测：try_lock 拿不到（工具调用正占连接）就跳过本次，
        // 不累计失败、不打断工具。旧逻辑用 timeout(lock().await) 会和工具调用互抢同一把锁，
        // 工具忙时 probe 等锁超时→误判失败→连续 2 次 take() 掉 client→kill excel 子进程→
        // 有状态 MCP 的内存工作簿全部丢失。codex/claurst 根本不做定时探测；cortex 保留健康
        // 显示，但绝不与工具调用互抢锁。
        let probe_result = match shared.try_lock() {
            Ok(running) => tokio::time::timeout(PROBE_TIMEOUT, running.list_all_tools()).await,
            Err(_) => {
                tracing::trace!(
                    "[MCP] server_id={server_id} 探测跳过：连接正被工具调用占用（try_lock）"
                );
                return; // 不更新 last_probe、不累计失败
            }
        };

        *entry.last_probe.write().await = Some(chrono::Utc::now());

        let mut health = entry.health.write().await;
        match probe_result {
            Ok(Ok(_tools)) => {
                *health = ConnHealth::Healthy;
                entry.failure_reason.write().await.clear();
            }
            _ => {
                // Ok(Err(_)) 或 Err(_超时)
                let reason = match &probe_result {
                    Ok(Err(e)) => sanitize_reason(&e.to_string()),
                    Err(_) => "探测超时".into(),
                    _ => "未知错误".into(),
                };
                let failures = match *health {
                    ConnHealth::Healthy => 1u8,
                    ConnHealth::Degraded {
                        consecutive_failures,
                    } => consecutive_failures.saturating_add(1),
                    _ => 1,
                };
                if failures >= FAILURE_THRESHOLD {
                    *health = ConnHealth::Unhealthy;
                    *entry.failure_reason.write().await = reason;
                    tracing::warn!(
                        "[MCP] server_id={server_id} 连续失败 {failures} 次，标记 Unhealthy 并断开连接"
                    );
                    drop(health);
                    // 断开坏连接（取走 Arc，若唯一引用则自然清理）
                    let old = entry.client.write().await.take();
                    drop(old);
                    // 关键修复：清除工具缓存和 last_tools_fetch，
                    // 防止后续 ensure_connected_and_list 的 TTL 缓存命中后跳过重连，
                    // 导致 build_toolsets 拿到 None client 而静默丢失工具集
                    *entry.last_tools_fetch.write().await = None;
                    *entry.cached_tools.write().await = Vec::new();
                } else {
                    *health = ConnHealth::Degraded {
                        consecutive_failures: failures,
                    };
                    tracing::debug!(
                        "[MCP] server_id={server_id} 探测失败 {failures}/{FAILURE_THRESHOLD}"
                    );
                }
            }
        }
    }

    pub async fn shutdown(&self) {
        let entries: Vec<(String, Arc<McpClientEntry>)> = {
            let mut clients = self.clients.write().await;
            clients.drain().collect()
        };
        for (id, entry) in entries {
            let client = entry.client.write().await.take();
            drop(client); // Arc 析构时清理
            tracing::info!("[MCP] 已关闭连接 server_id={id}");
        }
    }
}

fn sanitize_reason(raw: &str) -> String {
    let cleaned = raw.replace(['\n', '\r'], " ");
    let cleaned = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    if cleaned.chars().count() > 200 {
        let truncated: String = cleaned.chars().take(200).collect();
        format!("{truncated}...")
    } else {
        cleaned
    }
}

// ============================== ManagedMcpToolset ==============================

/// 自定义 MCP Toolset：共享 `Arc<Mutex<RunningService>>`，
/// 实现 adk `Toolset` trait 并自动给工具名加 `mcp__{slug}__` 前缀。
///
/// 由于 `RunningService` 不实现 `Clone`，无法直接使用 adk_tool 的 `McpToolset`
/// （它按值持有 `RunningService`）。本实现用 `Arc<Mutex<>>` 共享连接，
/// 允许连接池（健康探测）和 Agent（工具执行）共享同一连接。
pub struct ManagedMcpToolset {
    client: SharedClient,
    slug: String,
    /// 单次工具调用超时（来自 McpServer.tool_timeout_secs，界面可配）
    tool_timeout: Duration,
}

impl ManagedMcpToolset {
    pub fn new(client: SharedClient, slug: String, tool_timeout: Duration) -> Self {
        Self {
            client,
            slug,
            tool_timeout,
        }
    }
}

#[async_trait]
impl Toolset for ManagedMcpToolset {
    fn name(&self) -> &str {
        &self.slug
    }

    async fn tools(&self, _ctx: Arc<dyn ReadonlyContext>) -> AdkResult<Vec<Arc<dyn Tool>>> {
        let running = self.client.lock().await;
        let mcp_tools = running
            .list_all_tools()
            .await
            .map_err(|e| AdkError::tool(format!("MCP list_all_tools 失败: {e}")))?;

        let slug = self.slug.clone();
        let client = self.client.clone();
        let tools = mcp_tools
            .into_iter()
            .map(|t| {
                let tool_name = t.name.to_string();
                let namespaced = namespaced_tool_name(&slug, &tool_name);
                let description = t
                    .description
                    .as_ref()
                    .map(|d| d.to_string())
                    .unwrap_or_default();
                let input_schema = Some(Value::Object(t.input_schema.as_ref().clone()));
                Arc::new(ManagedMcpTool {
                    client: client.clone(),
                    tool_name,
                    namespaced_name: namespaced,
                    description,
                    input_schema,
                    tool_timeout: self.tool_timeout,
                }) as Arc<dyn Tool>
            })
            .collect();
        Ok(tools)
    }
}

/// 单个 MCP 工具包装：通过共享连接执行 `call_tool`
struct ManagedMcpTool {
    client: SharedClient,
    tool_name: String,
    namespaced_name: String,
    description: String,
    input_schema: Option<Value>,
    tool_timeout: Duration,
}

#[async_trait]
impl Tool for ManagedMcpTool {
    fn name(&self) -> &str {
        &self.namespaced_name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn is_builtin(&self) -> bool {
        false
    }

    fn parameters_schema(&self) -> Option<Value> {
        self.input_schema.clone()
    }

    fn declaration(&self) -> Value {
        serde_json::json!({
            "name": self.namespaced_name,
            "description": self.description,
            // 字段名必须是 "parameters"（OpenAI tool 格式），LLM client 的 convert_tools
            // 用 decl.get("parameters") 取 schema；写成 "input_schema" 会导致参数 schema
            // 取空，LLM 收到无参工具 → 调用时传空 {} → 有参工具（如 save_workbook）报
            // "missing field"。见 openai_custom.rs::convert_tools / anthropic convert_tools。
            "parameters": self.input_schema.clone().unwrap_or_else(|| serde_json::json!({})),
        })
    }

    async fn execute(&self, _ctx: Arc<dyn ToolContext>, args: Value) -> AdkResult<Value> {
        tracing::info!("[ManagedMcpTool] 原始参数: {:?}", args);
        let cleaned_args = sanitize_tool_args(args);
        // 截图类工具：去掉 filename，强制 MCP 端回传 base64 图片块（而非只存盘），
        // 使 cortex 与 MCP 跨机器（不共享文件系统）时也能拿到图片字节内联显示。
        let cleaned_args = strip_screenshot_filename(&self.tool_name, cleaned_args);
        tracing::info!("[ManagedMcpTool] 清理后参数: {:?}", cleaned_args);

        let mut params = CallToolRequestParams::new(self.tool_name.clone());
        match cleaned_args {
            Value::Object(ref map) if !map.is_empty() => {
                params = params.with_arguments(map.clone());
            }
            Value::Object(_) => {}
            Value::Null => {}
            _ => {
                return Err(AdkError::tool(format!(
                    "MCP tool '{}' 参数必须是 JSON 对象",
                    self.tool_name
                )));
            }
        }
        let running = self.client.lock().await;
        // 【参考 codex DEFAULT_TOOL_TIMEOUT】call_tool 加超时，防止 MCP 工具卡死
        // （如 excel-mcp-server 的 write_cells 在某些 cell 组合下挂起）导致 SSE 无限阻塞。
        // 超时返回错误，agent 可重试/换法，而不是前端永远转圈。
        let result = tokio::time::timeout(self.tool_timeout, running.call_tool(params))
            .await
            .map_err(|_| {
                AdkError::tool(format!(
                    "MCP tool '{}' 执行超时（{}s）",
                    self.tool_name,
                    self.tool_timeout.as_secs()
                ))
            })?
            .map_err(|e| AdkError::tool(format!("MCP call_tool 失败: {e}")))?;

        if result.is_error.unwrap_or(false) {
            let mut msg = format!("MCP tool '{}' 执行失败", self.tool_name);
            for content in &result.content {
                if let Some(text) = content.as_text() {
                    msg.push_str(": ");
                    msg.push_str(&text.text);
                    break;
                }
            }
            return Err(AdkError::tool(msg));
        }

        tracing::info!("[ManagedMcpTool] 执行结果: {:?}", result);

        // 优先返回 structured_content
        if let Some(structured) = result.structured_content {
            return Ok(serde_json::json!({ "output": structured }));
        }

        // 否则拼接文本内容。截图类工具额外保留 image content block 的 base64
        // （挂到 out.image），交由截图管线（tools::screenshot::process_screenshot_response）
        // 解码落盘 + 注入 image_url，使截图能在聊天界面内联显示。需配合 MCP 端
        // `--image-responses allow` 让工具回传图片块。门控在 screenshot 工具名上，
        // 避免非截图工具的图片块把巨大 base64 灌进 LLM 上下文。
        let is_screenshot_tool = self.tool_name.contains("screenshot");
        let mut parts: Vec<String> = Vec::new();
        let mut image_data: Option<(String, String)> = None; // (base64, mime_type)
        for content in &result.content {
            if let Some(text) = content.as_text() {
                parts.push(text.text.clone());
            } else if is_screenshot_tool {
                if let Some(img) = content.as_image() {
                    image_data.get_or_insert((img.data.clone(), img.mime_type.clone()));
                }
            }
            // 其他类型（resource 等）忽略：原占位 "[非文本内容]" 无信息量
        }
        let mut out = serde_json::json!({ "output": parts.join("\n") });
        if let Some((data, mime)) = image_data {
            if let Some(obj) = out.as_object_mut() {
                obj.insert(
                    "image".to_string(),
                    serde_json::json!({ "mime_type": mime, "data": data }),
                );
            }
        }
        Ok(out)
    }
}

/// 截图类工具去掉 `filename` 参数：Playwright 等在传入 filename 时只存盘、不回传
/// base64 图片块；去掉后改为内联回传 base64，供 cortex 解码显示。cortex 与 MCP 跨
/// 机器、不共享文件系统时尤其必需——否则 cortex 拿不到图片字节。
fn strip_screenshot_filename(tool_name: &str, mut value: Value) -> Value {
    if !tool_name.to_ascii_lowercase().contains("screenshot") {
        return value;
    }
    if let Some(obj) = value.as_object_mut() {
        if obj.remove("filename").is_some() {
            tracing::info!(
                "[ManagedMcpTool] 已去掉截图工具 `{}` 的 filename 参数，强制内联 base64",
                tool_name
            );
        }
    }
    value
}

// 清理工具参数的辅助函数
fn sanitize_tool_args(value: Value) -> Value {
    tracing::info!("[sanitize_tool_args] 清理前: {:?}", value);
    let result = match value {
        Value::String(s) => {
            let cleaned = s.trim_matches('`').to_string();
            tracing::info!("[sanitize_tool_args] 清理字符串: {:?} -> {:?}", s, cleaned);
            Value::String(cleaned)
        }
        Value::Object(mut map) => {
            tracing::info!("[sanitize_tool_args] 清理对象: {:?}", map);
            for (_key, val) in map.iter_mut() {
                *val = sanitize_tool_args(val.clone());
            }
            Value::Object(map)
        }
        Value::Array(arr) => {
            tracing::info!("[sanitize_tool_args] 清理数组: {:?}", arr);
            let cleaned_arr: Vec<Value> = arr.into_iter().map(sanitize_tool_args).collect();
            Value::Array(cleaned_arr)
        }
        _ => value,
    };
    tracing::info!("[sanitize_tool_args] 清理后: {:?}", result);
    result
}

// ============================== Tests ==============================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_reason_truncates_long() {
        let long = "x".repeat(300);
        let s = sanitize_reason(&long);
        assert!(s.chars().count() <= 203);
        assert!(s.ends_with("..."));
    }

    #[test]
    fn sanitize_reason_collapses_whitespace() {
        let s = sanitize_reason("line1\nline2\r\nline3");
        assert_eq!(s, "line1 line2 line3");
    }

    #[test]
    fn sanitize_reason_short_passthrough() {
        let s = sanitize_reason("connection refused");
        assert_eq!(s, "connection refused");
    }

    #[test]
    fn conn_health_default_is_disconnected() {
        assert_eq!(ConnHealth::default(), ConnHealth::Disconnected);
    }
}
