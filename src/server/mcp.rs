//! MCP Server 管理 — GraphQL handler 层
//!
//! ## GraphQL 字段
//!
//! | 字段 | 类型 | 说明 |
//! |------|------|------|
//! | `mcpServers` | Query | MCP Server 列表（含健康状态，env/headers 脱敏） |
//! | `mcpServer` | Query | 单个 MCP Server 详情 |
//! | `mcpTools` | Query | 指定 Server 的工具清单（触发连接 + 工具发现） |
//! | `createMcpServer` | Mutation | 新建 MCP Server |
//! | `updateMcpServer` | Mutation | 编辑 MCP Server（env/headers 可选覆盖） |
//! | `deleteMcpServer` | Mutation | 删除 MCP Server（级联清理 assistant 引用） |
//! | `probeMcpServer` | Mutation | 手动探测（强制重连 + 工具发现） |
//!
//! 安全约定：env/headers 的明文仅在 create/update 时接收，响应中始终脱敏。

use serde_json::{Value, json};

use crate::domain::mcp::dto::{CreateMcpServerInput, McpToolsQuery, UpdateMcpServerInput};
use crate::error::AppError;
use crate::server::AppState;

use super::response;
use super::response::code;

fn ok(data: Value) -> Value {
    response::ok(data)
}

fn fail(e: &AppError) -> Value {
    response::from_app_error(e)
}

fn db_unavailable() -> Value {
    response::err(code::DATABASE, "数据库不可用")
}

/// 单个 MCP Server 详情
pub async fn get_server(state: &AppState, id: &str) -> Value {
    let Some(mgr) = state.mcp_manager.as_ref() else {
        return db_unavailable();
    };
    match mgr.get_server(id).await {
        Ok(Some(s)) => ok(json!({ "server": s })),
        Ok(None) => response::err(code::NOT_FOUND, "MCP Server 不存在"),
        Err(e) => fail(&e),
    }
}

/// 工具清单查询（可批量）
pub async fn list_tools(state: &AppState, input: Value) -> Value {
    let Some(mgr) = state.mcp_manager.as_ref() else {
        return db_unavailable();
    };
    let query: McpToolsQuery = match serde_json::from_value(input) {
        Ok(q) => q,
        Err(e) => {
            return response::err(code::PARSE_ERROR, format!("参数解析失败: {e}"));
        }
    };
    match mgr.list_tools_batch(&query).await {
        Ok(map) => ok(json!({ "tools": map })),
        Err(e) => fail(&e),
    }
}

/// 新建 MCP Server
pub async fn create_server(state: &AppState, input: CreateMcpServerInput) -> Value {
    let Some(mgr) = state.mcp_manager.as_ref() else {
        return db_unavailable();
    };
    match mgr.create_server(&input).await {
        Ok(resp) => ok(json!({ "server": resp })),
        Err(e) => fail(&e),
    }
}

/// 编辑 MCP Server
pub async fn update_server(state: &AppState, id: &str, input: UpdateMcpServerInput) -> Value {
    let Some(mgr) = state.mcp_manager.as_ref() else {
        return db_unavailable();
    };
    match mgr.update_server(id, &input).await {
        Ok(Some(resp)) => ok(json!({ "server": resp })),
        Ok(None) => response::err(code::NOT_FOUND, "MCP Server 不存在"),
        Err(e) => fail(&e),
    }
}

/// 删除 MCP Server（force 省略/false=仅预检返回影响清单；force=true=执行清理+删除）
pub async fn delete_server(state: &AppState, id: &str, force: bool) -> Value {
    let Some(mgr) = state.mcp_manager.as_ref() else {
        return db_unavailable();
    };
    if !force {
        match mgr.store().impact_of_delete(id).await {
            Ok(assistants) => ok(json!({
                "deleted": false,
                "impact": { "assistants": assistants },
                "summary": if assistants > 0 {
                    format!("{} 个助手启用了该 MCP（将从中移除）", assistants)
                } else {
                    "无关联数据，可直接删除".to_string()
                },
            })),
            Err(e) => fail(&e),
        }
    } else {
        match mgr.delete_server(id).await {
            Ok(true) => ok(json!({ "deleted": true })),
            Ok(false) => response::err(code::NOT_FOUND, "MCP Server 不存在"),
            Err(e) => fail(&e),
        }
    }
}

/// 手动探测（强制重连 + 工具发现）
pub async fn probe_server(state: &AppState, id: &str) -> Value {
    let Some(mgr) = state.mcp_manager.as_ref() else {
        return db_unavailable();
    };
    match mgr.probe_server(id).await {
        Ok(resp) => ok(json!({ "server": resp })),
        Err(e) => fail(&e),
    }
}

/// 分页列表
pub async fn list_servers_paged(
    state: &AppState,
    page: Option<usize>,
    page_size: Option<usize>,
    keyword: Option<String>,
) -> Value {
    let Some(mgr) = state.mcp_manager.as_ref() else {
        return db_unavailable();
    };
    let page = page.unwrap_or(1);
    let page_size = page_size.unwrap_or(10);
    match mgr
        .list_servers_paged(page, page_size, keyword.as_deref())
        .await
    {
        Ok((servers, total)) => ok(json!({
            "servers": servers,
            "total": total,
            "page": page,
            "page_size": page_size,
            "total_pages": ((total as f64) / (page_size as f64)).ceil() as i64,
        })),
        Err(e) => fail(&e),
    }
}

/// 批量改状态
pub async fn batch_set_status(state: &AppState, input: Value) -> Value {
    let Some(mgr) = state.mcp_manager.as_ref() else {
        return db_unavailable();
    };
    let ids: Option<Vec<String>> = input
        .get("ids")
        .and_then(|v| serde_json::from_value(v.clone()).ok());
    let keyword: Option<String> = input
        .get("keyword")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let status_val: i16 = input.get("status").and_then(|v| v.as_i64()).unwrap_or(1) as i16;
    if status_val != 0 && status_val != 1 {
        return response::err(code::INVALID_PARAMS, "status 必须是 0 或 1");
    }
    match mgr
        .batch_set_status(ids.as_deref(), keyword.as_deref(), status_val)
        .await
    {
        Ok(affected) => ok(json!({ "affected": affected })),
        Err(e) => fail(&e),
    }
}

/// 批量删除
pub async fn batch_delete_servers(state: &AppState, input: Value) -> Value {
    let Some(mgr) = state.mcp_manager.as_ref() else {
        return db_unavailable();
    };
    let ids: Option<Vec<String>> = input
        .get("ids")
        .and_then(|v| serde_json::from_value(v.clone()).ok());
    let keyword: Option<String> = input
        .get("keyword")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    match mgr.batch_delete(ids.as_deref(), keyword.as_deref()).await {
        Ok(affected) => ok(json!({ "affected": affected })),
        Err(e) => fail(&e),
    }
}

/// 批量探测
pub async fn batch_probe_servers(state: &AppState, input: Value) -> Value {
    let Some(mgr) = state.mcp_manager.as_ref() else {
        return db_unavailable();
    };
    let ids: Vec<String> = input
        .get("ids")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();
    if ids.is_empty() {
        return response::err(code::INVALID_PARAMS, "ids 不能为空");
    }
    let servers = mgr.batch_probe(&ids).await;
    ok(json!({ "servers": servers }))
}
