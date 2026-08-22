//! 工具查询与工具集装配：list_tools / call_tool_by_slug / build_toolsets。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use adk_rust::tool::Toolset;
use rmcp::model::CallToolRequestParams;
use serde_json::Value;

use crate::domain::mcp::dto::McpToolsQuery;
use crate::domain::mcp::enums::Status;
use crate::domain::mcp::models::{McpServer, McpToolInfo};
use crate::error::AppError;

use super::toolset::ManagedMcpToolset;
use super::{McpManager, TOOL_TIMEOUT_MAX_SECS, tool_timeout_duration};

impl McpManager {
    pub async fn list_tools(
        &self,
        server_id: &str,
        user_id: &str,
        is_admin: bool,
    ) -> Result<Vec<McpToolInfo>, AppError> {
        let server = self
            .store
            .get_by_id(server_id)
            .await?
            .ok_or_else(|| AppError::NotFoundError("MCP Server 不存在".into()))?;
        // 归属校验：非归属人非管理员 → 视为不存在
        if !Self::allows(&server, user_id, is_admin) {
            return Err(AppError::NotFoundError("MCP Server 不存在".into()));
        }
        self.ensure_connected_and_list(&server).await
    }

    pub async fn list_tools_batch(
        &self,
        query: &McpToolsQuery,
        user_id: &str,
        is_admin: bool,
    ) -> Result<HashMap<String, Vec<McpToolInfo>>, AppError> {
        let mut out = HashMap::new();
        for id in &query.server_ids {
            match self.list_tools(id, user_id, is_admin).await {
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

    /// 按 slug 编程式调用某个已启用 MCP server 的工具（非 Agent 工具循环路径）。
    ///
    /// 供后端业务逻辑直接使用 MCP 工具——如文档上传后调 markitdown 的
    /// `convert_to_markdown` 把 Office/PDF 转成 markdown 注入对话。复用连接池；
    /// server 未找到 / 未启用 / 连接或调用失败均返回 Err，调用方按需降级。
    ///
    /// - `slug`：MCP server 的 slug（如 `markitdown`）
    /// - `tool_name`：MCP 工具原始名（**不带** `mcp__slug__` 前缀）
    /// - `arguments`：工具参数（JSON 对象）
    /// - `timeout`：单次调用超时；None 用 server 配置的 tool_timeout_secs
    ///
    /// 返回值：拼接所有文本 content 块得到的字符串（markitdown 返回单个文本块）。
    pub async fn call_tool_by_slug(
        &self,
        slug: &str,
        tool_name: &str,
        arguments: Value,
        timeout: Option<Duration>,
    ) -> Result<String, AppError> {
        let server = self
            .store
            .get_by_slug(slug)
            .await?
            .ok_or_else(|| AppError::NotFoundError(format!("MCP server '{slug}' 未找到")))?;
        if server.status != Status::Enabled {
            return Err(AppError::BusinessError(format!(
                "MCP server '{slug}' 未启用"
            )));
        }

        let entry = self.get_or_create_entry(&server).await?;
        self.ensure_connected(&server, &entry).await?;

        let shared = entry
            .client
            .read()
            .await
            .clone()
            .ok_or_else(|| AppError::BusinessError("MCP 连接未建立".into()))?;

        let mut params = CallToolRequestParams::new(tool_name.to_string());
        if let Value::Object(map) = &arguments
            && !map.is_empty()
        {
            params = params.with_arguments(map.clone());
        }

        let timeout = timeout
            .map(|t| t.min(Duration::from_secs(TOOL_TIMEOUT_MAX_SECS as u64)))
            .unwrap_or_else(|| tool_timeout_duration(server.tool_timeout_secs));
        // 锁等待也计入超时：并发调用（如多个文档同时解析）排队时，等待 shared 连接锁的时间
        // 不能无限累加——整体（取锁 + 执行）受 timeout 约束。
        let result = tokio::time::timeout(timeout, async {
            let running = shared.lock().await;
            running.call_tool(params).await
        })
        .await
        .map_err(|_| {
            AppError::NetworkError(format!(
                "MCP tool '{tool_name}' 执行超时（{}s）",
                timeout.as_secs()
            ))
        })?
        .map_err(|e| AppError::NetworkError(format!("MCP call_tool 失败: {e}")))?;

        if result.is_error.unwrap_or(false) {
            let mut msg = format!("MCP tool '{tool_name}' 执行失败");
            for c in &result.content {
                if let Some(t) = c.as_text() {
                    msg.push_str(": ");
                    msg.push_str(&t.text);
                    break;
                }
            }
            return Err(AppError::BusinessError(msg));
        }

        // 拼接所有文本内容块（markitdown 的 convert_to_markdown 返回单个文本块）
        let parts: Vec<String> = result
            .content
            .iter()
            .filter_map(|c| c.as_text().map(|t| t.text.clone()))
            .collect();
        Ok(parts.join("\n"))
    }

    // ===== Agent 工具集注入 =====

    /// 为会话装配 MCP 工具集（按归属人 `user_id` 隔离：仅装载归属人/管理员可见的 server）。
    pub async fn build_toolsets(
        self: &Arc<Self>,
        mcp_ids: &[String],
        user_id: &str,
        is_admin: bool,
    ) -> Vec<Arc<dyn Toolset>> {
        let mut toolsets = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for id in mcp_ids {
            // 去重，防止 enabled_mcps 中重复 ID 导致同名工具集
            if !seen.insert(id.clone()) {
                continue;
            }
            if let Some(ts) = self.build_one_toolset(id, user_id, is_admin).await {
                toolsets.push(ts);
            }
        }
        if toolsets.len() < mcp_ids.len() {
            // 差集 = enabled_mcps 里引用了已删除/未启用/未授权/连接失败的 server，
            // 单独 warn 便于排查「配了却没生效」（明细见上方 build_one_toolset 的逐条日志）。
            tracing::warn!(
                "[MCP] build_toolsets: {} 个 server 未注入（已删除/未启用/未授权/连接失败），请求={} 成功={}",
                mcp_ids.len() - toolsets.len(),
                mcp_ids.len(),
                toolsets.len()
            );
        }
        tracing::info!(
            "[MCP] build_toolsets 完成: 请求 {} 个 server，成功注入 {} 个工具集",
            mcp_ids.len(),
            toolsets.len()
        );
        toolsets
    }

    /// 为单个 MCP server 构建工具集；server 不存在/未启用/不属于调用者/连接失败时返回 None。
    async fn build_one_toolset(
        &self,
        id: &str,
        user_id: &str,
        is_admin: bool,
    ) -> Option<Arc<dyn Toolset>> {
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
        // 归属隔离：非归属人且非管理员的 server 不装载（防跨用户加载他人 MCP 工具）
        if !Self::allows(&server, user_id, is_admin) {
            tracing::warn!(
                "[MCP] build_toolsets: server_id={} 不属于调用者 {}，跳过",
                id,
                user_id
            );
            return None;
        }
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
                tool_timeout_duration(server.tool_timeout_secs),
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
                    tool_timeout_duration(server.tool_timeout_secs),
                )) as Arc<dyn Toolset>);
            }
        }
        tracing::error!(
            "[MCP] build_toolsets: server {} 强制重连仍失败，工具集丢失！",
            server.slug
        );
        None
    }
}
