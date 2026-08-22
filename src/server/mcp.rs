//! MCP Server 管理 — GraphQL handler 层
//!
//! ## GraphQL 字段
//!
//! | 字段 | 类型 | 说明 |
//!|------|------|------|
//! | `mcpServers` | Query | MCP Server 列表（含健康状态，env/headers 脱敏） |
//! | `mcpServer` | Query | 单个 MCP Server 详情 |
//! | `mcpTools` | Query | 指定 Server 的工具清单（触发连接 + 工具发现） |
//! | `createMcpServer` | Mutation | 新建 MCP Server |
//! | `updateMcpServer` | Mutation | 编辑 MCP Server（env/headers 按键合并：值级 null=保留、非空=覆盖、键缺席=删除；字段级 null=不动） |
//! | `deleteMcpServer` | Mutation | 删除 MCP Server（级联清理 assistant 引用） |
//! | `probeMcpServer` | Mutation | 手动探测（强制重连 + 工具发现） |
//!
//! 安全约定：env/headers 的明文仅在 create/update 时接收，响应中始终脱敏。
//! **完全归属隔离**：每人只看/操作自己的 MCP；管理员看全部。

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

/// 单个 MCP Server 详情（归属人/管理员可见，否则 404）
pub async fn get_server(state: &AppState, id: &str, user_id: &str, is_admin: bool) -> Value {
    let Some(mgr) = state.mcp_manager.as_ref() else {
        return db_unavailable();
    };
    match mgr.get_server(id, user_id, is_admin).await {
        Ok(Some(s)) => ok(json!({ "server": s })),
        Ok(None) => response::err(code::NOT_FOUND, "MCP Server 不存在"),
        Err(e) => fail(&e),
    }
}

/// 工具清单查询（可批量，仅归属人/管理员可见自己的 server）
pub async fn list_tools(state: &AppState, input: Value, user_id: &str, is_admin: bool) -> Value {
    let Some(mgr) = state.mcp_manager.as_ref() else {
        return db_unavailable();
    };
    let query: McpToolsQuery = match serde_json::from_value(input) {
        Ok(q) => q,
        Err(e) => {
            return response::err(code::PARSE_ERROR, format!("参数解析失败: {e}"));
        }
    };
    match mgr.list_tools_batch(&query, user_id, is_admin).await {
        Ok(map) => ok(json!({ "tools": map })),
        Err(e) => fail(&e),
    }
}

/// 新建 MCP Server（归属=当前用户）
pub async fn create_server(state: &AppState, input: CreateMcpServerInput, user_id: &str) -> Value {
    let Some(mgr) = state.mcp_manager.as_ref() else {
        return db_unavailable();
    };
    match mgr.create_server(&input, user_id).await {
        Ok(resp) => ok(json!({ "server": resp })),
        Err(e) => fail(&e),
    }
}

/// 编辑 MCP Server（归属人/管理员，否则 404）
pub async fn update_server(
    state: &AppState,
    id: &str,
    input: UpdateMcpServerInput,
    user_id: &str,
    is_admin: bool,
) -> Value {
    let Some(mgr) = state.mcp_manager.as_ref() else {
        return db_unavailable();
    };
    match mgr.update_server(id, &input, user_id, is_admin).await {
        Ok(Some(resp)) => ok(json!({ "server": resp })),
        Ok(None) => response::err(code::NOT_FOUND, "MCP Server 不存在"),
        Err(e) => fail(&e),
    }
}

/// 删除 MCP Server（force 省略/false=仅预检返回影响清单；force=true=执行清理+删除）
/// 仅归属人/管理员可操作；非归属人预检/删除均返回 404。
pub async fn delete_server(
    state: &AppState,
    id: &str,
    force: bool,
    user_id: &str,
    is_admin: bool,
) -> Value {
    let Some(mgr) = state.mcp_manager.as_ref() else {
        return db_unavailable();
    };
    if !force {
        if !mgr.can_modify(id, user_id, is_admin).await {
            return response::err(code::NOT_FOUND, "MCP Server 不存在");
        }
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
        match mgr.delete_server(id, user_id, is_admin).await {
            Ok(true) => ok(json!({ "deleted": true })),
            Ok(false) => response::err(code::NOT_FOUND, "MCP Server 不存在"),
            Err(e) => fail(&e),
        }
    }
}

/// 手动探测（强制重连 + 工具发现，归属人/管理员）
pub async fn probe_server(state: &AppState, id: &str, user_id: &str, is_admin: bool) -> Value {
    let Some(mgr) = state.mcp_manager.as_ref() else {
        return db_unavailable();
    };
    match mgr.probe_server(id, user_id, is_admin).await {
        Ok(resp) => ok(json!({ "server": resp })),
        Err(e) => fail(&e),
    }
}

/// 分页列表（按归属隔离）
pub async fn list_servers_paged(
    state: &AppState,
    page: Option<usize>,
    page_size: Option<usize>,
    keyword: Option<String>,
    user_id: &str,
    is_admin: bool,
) -> Value {
    let Some(mgr) = state.mcp_manager.as_ref() else {
        return db_unavailable();
    };
    let page = page.unwrap_or(1);
    let page_size = page_size.unwrap_or(10);
    match mgr
        .list_servers_paged(page, page_size, keyword.as_deref(), user_id, is_admin)
        .await
    {
        Ok((servers, total)) => {
            let mut items = serde_json::to_value(&servers).unwrap_or_else(|_| json!([]));
            if let Some(arr) = items.as_array_mut() {
                super::owner::inject_owners(state.db_pool.as_ref(), is_admin, arr, "user_id").await;
            }
            ok(json!({
                "servers": items,
                "total": total,
                "page": page,
                "page_size": page_size,
                "total_pages": ((total as f64) / (page_size as f64)).ceil() as i64,
            }))
        }
        Err(e) => fail(&e),
    }
}

/// 批量改状态（按归属隔离）
pub async fn batch_set_status(
    state: &AppState,
    input: Value,
    user_id: &str,
    is_admin: bool,
) -> Value {
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
        .batch_set_status(
            ids.as_deref(),
            keyword.as_deref(),
            status_val,
            user_id,
            is_admin,
        )
        .await
    {
        Ok(affected) => ok(json!({ "affected": affected })),
        Err(e) => fail(&e),
    }
}

/// 批量删除（按归属隔离）
pub async fn batch_delete_servers(
    state: &AppState,
    input: Value,
    user_id: &str,
    is_admin: bool,
) -> Value {
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
    match mgr
        .batch_delete(ids.as_deref(), keyword.as_deref(), user_id, is_admin)
        .await
    {
        Ok(affected) => ok(json!({ "affected": affected })),
        Err(e) => fail(&e),
    }
}

/// 批量探测（按归属隔离，非归属人的 id 静默跳过）
pub async fn batch_probe_servers(
    state: &AppState,
    input: Value,
    user_id: &str,
    is_admin: bool,
) -> Value {
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
    let servers = mgr.batch_probe(&ids, user_id, is_admin).await;
    ok(json!({ "servers": servers }))
}
