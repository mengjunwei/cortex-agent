//! 健康探测：手动探测、启动首轮探测、后台 probe 循环（Hybrid 策略）。

use std::sync::Arc;
use std::time::Duration;

use tokio::time::sleep;

use crate::domain::mcp::dto::McpServerResponse;
use crate::domain::mcp::enums::Status;
use crate::domain::mcp::models::{McpServer, ServerHealth};
use crate::domain::mcp::store::McpServerStore;
use crate::error::AppError;

use super::sanitize::sanitize_reason;
use super::{
    ConnHealth, FAILURE_THRESHOLD, IDLE_REAP_TTL, McpClientEntry, McpManager, PROBE_INTERVAL,
    PROBE_TIMEOUT,
};

impl McpManager {
    pub async fn batch_probe(
        &self,
        ids: &[String],
        user_id: &str,
        is_admin: bool,
    ) -> Vec<McpServerResponse> {
        let mut results = Vec::with_capacity(ids.len());
        for id in ids {
            match self.probe_server(id, user_id, is_admin).await {
                Ok(resp) => results.push(resp),
                Err(e) => tracing::warn!("[MCP] 批量探测失败 id={id}: {e}"),
            }
        }
        results
    }

    pub async fn probe_server(
        &self,
        server_id: &str,
        user_id: &str,
        is_admin: bool,
    ) -> Result<McpServerResponse, AppError> {
        let server = self
            .store
            .get_by_id(server_id)
            .await?
            .ok_or_else(|| AppError::NotFoundError("MCP Server 不存在".into()))?;
        // 归属校验：仅归属人/管理员可手动探测
        if !Self::allows(&server, user_id, is_admin) {
            return Err(AppError::NotFoundError("MCP Server 不存在".into()));
        }
        self.probe_server_inner(&server).await
    }

    /// 探测实际工作（evict + 连接 + 工具发现 + 组装响应），不做归属校验。
    /// 供 API 探测（先校验归属）与启动后台探测（系统任务，探测全部）共用。
    async fn probe_server_inner(&self, server: &McpServer) -> Result<McpServerResponse, AppError> {
        self.evict(&server.id).await;
        match self.ensure_connected_and_list(server).await {
            Ok(tools) => {
                let health = self.peek_health(&server.id).await;
                let resp = McpServerStore::to_response(server, health);
                tracing::info!(
                    "[MCP] 手动探测成功 slug={} tools={}",
                    server.slug,
                    tools.len()
                );
                Ok(resp)
            }
            Err(e) => {
                let reason = sanitize_reason(&e.to_string());
                if let Some(entry) = self.clients.read().await.get(&server.id) {
                    *entry.health.write().await = ConnHealth::Unhealthy;
                    *entry.failure_reason.write().await = reason.clone();
                    *entry.last_probe.write().await = Some(chrono::Utc::now());
                }
                // 返回 Ok + Unhealthy 健康状态，让前端能看到具体失败原因
                let now = chrono::Utc::now().to_rfc3339();
                let health = ServerHealth::Unhealthy {
                    reason,
                    last_check: now,
                };
                let resp = McpServerStore::to_response(server, health);
                tracing::warn!(
                    "[MCP] 探测失败 slug={} error={}",
                    server.slug,
                    e
                );
                Ok(resp)
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
            // 系统启动探测：不做归属校验（管理员视角，探测所有已启用服务）
            let timeout_result =
                tokio::time::timeout(Duration::from_secs(15), self.probe_server_inner(s)).await;
            match timeout_result {
                Ok(Ok(resp)) => {
                    // 检查返回的 health 状态：Unhealthy 视为失败
                    if let ServerHealth::Unhealthy { reason, .. } = &resp.health {
                        tracing::warn!(
                            "[MCP] 启动探测：服务「{}」不健康: {}",
                            s.slug,
                            reason
                        );
                        fail += 1;
                    } else {
                        ok += 1;
                    }
                }
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
