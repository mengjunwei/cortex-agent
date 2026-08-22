//! 连接管理：惰性建连、TTL 软刷新、工具清单抓取、连接回收。

use std::sync::Arc;
use std::time::Instant;

use serde_json::Value;

use crate::domain::mcp::models::{McpServer, McpToolInfo, ServerHealth, namespaced_tool_name};
use crate::domain::mcp::transport;
use crate::error::AppError;

use super::sanitize::sanitize_reason;
use super::{HEALTH_TTL, McpClientEntry, McpManager, PROBE_TIMEOUT};

impl McpManager {
    pub(super) async fn ensure_connected_and_list(
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

    pub(super) async fn get_or_create_entry(
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

    pub(super) async fn ensure_connected(
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

        *entry.health.write().await = super::ConnHealth::Connecting;

        match transport::connect(server, self.stdio_inherit_env).await {
            Ok(running) => {
                let shared = Arc::new(tokio::sync::Mutex::new(running));
                *entry.client.write().await = Some(shared);
                *entry.health.write().await = super::ConnHealth::Healthy;
                *entry.failure_reason.write().await = String::new();
                *entry.last_probe.write().await = Some(chrono::Utc::now());
                tracing::info!("[MCP] 连接成功 slug={}", server.slug);
                Ok(())
            }
            Err(e) => {
                let reason = sanitize_reason(&e.to_string());
                *entry.health.write().await = super::ConnHealth::Unhealthy;
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
        // list_all_tools 短重试：远程 streamable_http MCP 在瞬时 5xx/限流/抖动下会失败，
        // 重试 2 次（250ms / 500ms 阶梯）通常即可恢复——对齐 codex rmcp-client 对
        // tools/list（发现类、只读）的重试策略（call_tool 不重试，避免副作用放大）。
        //
        // 每次调用必须带超时（PROBE_TIMEOUT，与 probe_one 一致）：若 server 接受连接但
        // 不响应 tools/list（半开/卡死），裸 await 会无限挂起 → shared 锁永不释放 → 该
        // server 上所有工具调用（call_tool_by_slug / ManagedMcpTool::execute）全部死锁，
        // 且 agent 会话构建（build_toolsets → ensure_connected_and_list → fetch_tools）整体
        // 卡死。加超时后最坏情况为 3×PROBE_TIMEOUT + 750ms 即放弃，锁随之释放。
        let mcp_tools = {
            let mut last_err: Option<String> = None;
            let mut tools = None;
            for attempt in 0u8..3u8 {
                match tokio::time::timeout(PROBE_TIMEOUT, running.list_all_tools()).await {
                    Ok(Ok(t)) => {
                        tools = Some(t);
                        break;
                    }
                    Ok(Err(e)) => last_err = Some(format!("{e}")),
                    Err(_elapsed) => {
                        last_err = Some(format!("tools/list 超时（{}s）", PROBE_TIMEOUT.as_secs()))
                    }
                }
                if attempt < 2 {
                    tokio::time::sleep(std::time::Duration::from_millis(if attempt == 0 {
                        250
                    } else {
                        500
                    }))
                    .await;
                }
            }
            match tools {
                Some(t) => t,
                None => {
                    return Err(AppError::NetworkError(format!(
                        "list_all_tools 失败（已重试 2 次）: {}",
                        last_err.unwrap_or_default()
                    )));
                }
            }
        };

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

    pub(super) async fn peek_health(&self, server_id: &str) -> ServerHealth {
        let clients = self.clients.read().await;
        if let Some(entry) = clients.get(server_id) {
            return entry.to_server_health().await;
        }
        ServerHealth::Unknown
    }

    pub(super) async fn evict(&self, server_id: &str) {
        let entry = {
            let mut clients = self.clients.write().await;
            clients.remove(server_id)
        };
        if let Some(entry) = entry {
            let client = entry.client.write().await.take();
            drop(client); // Arc 析构时若有唯一引用则 RunningService::drop 清理
        }
    }

    pub(super) async fn evict_all(&self) {
        let ids: Vec<String> = {
            let clients = self.clients.read().await;
            clients.keys().cloned().collect()
        };
        for id in ids {
            self.evict(&id).await;
        }
    }
}
