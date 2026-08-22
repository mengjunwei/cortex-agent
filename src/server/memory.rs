//! 跨会话记忆业务接口（GraphQL resolver 调用）。
//!
//! 所有操作按 user_id 隔离（防越权）：list/create 按 user_id 过滤；
//! update/delete/claim/reject 在 store SQL 层带 user_id 条件（见 domain::memory）。

use serde_json::{Value, json};

use super::AppState;
use super::response;
use super::response::code;
use crate::domain::memory::{self, mem_type, scope};

/// 取得记忆存储；不可用时返回错误信封
fn memory_store(state: &AppState) -> Result<&std::sync::Arc<memory::MemoryStore>, Value> {
    state
        .memory_store
        .as_ref()
        .ok_or_else(|| response::err(code::DATABASE, "记忆存储未初始化（数据库未启用）"))
}

fn proposal_store(state: &AppState) -> Result<&std::sync::Arc<memory::MemoryProposalStore>, Value> {
    state
        .memory_proposal_store
        .as_ref()
        .ok_or_else(|| response::err(code::DATABASE, "记忆建议存储未初始化（数据库未启用）"))
}

/// 入参 `type` 字段 → mem_type：兼容数字(0/1,前端 radio value)与字符串("preference"/"pitfall")
fn parse_mem_type(body: &Value) -> i16 {
    match body["type"].as_i64() {
        Some(1) => mem_type::PITFALL,
        Some(_) => mem_type::PREFERENCE,
        None => match body["type"].as_str().map(str::to_lowercase).as_deref() {
            Some("pitfall") => mem_type::PITFALL,
            _ => mem_type::PREFERENCE,
        },
    }
}

/// 入参 `scope` 字段 → (scope, assistant_id)：兼容数字(0/1)与字符串("user"/"assistant")。
/// assistant 级需要 assistant_id；用户级忽略 assistant_id。
fn parse_scope(body: &Value) -> (i16, Option<String>) {
    let scope_v = match body["scope"].as_i64() {
        Some(1) => scope::ASSISTANT,
        Some(_) => scope::USER,
        None => match body["scope"].as_str().map(str::to_lowercase).as_deref() {
            Some("assistant") => scope::ASSISTANT,
            _ => scope::USER,
        },
    };
    let aid = if scope_v == scope::ASSISTANT {
        body["assistant_id"]
            .as_str()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    } else {
        None
    };
    (scope_v, aid)
}

/// 列出全部记忆（管理页）：普通用户仅自己的；管理员看全部。
pub async fn list_memories(state: &AppState, user_id: &str, is_admin: bool) -> Value {
    let store = match memory_store(state) {
        Ok(s) => s,
        Err(v) => return v,
    };
    match store.list_for_user(user_id, is_admin).await {
        Ok(list) => {
            let mut items = serde_json::to_value(&list).unwrap_or_else(|_| json!([]));
            if let Some(arr) = items.as_array_mut() {
                super::owner::inject_owners(state.db_pool.as_ref(), is_admin, arr, "user_id").await;
            }
            response::ok(json!({ "items": items }))
        }
        Err(e) => response::from_app_error(&e),
    }
}

/// 列出待确认记忆建议（卡片）：普通用户仅自己的；管理员看全部。
pub async fn list_memory_proposals(state: &AppState, user_id: &str, is_admin: bool) -> Value {
    let store = match proposal_store(state) {
        Ok(s) => s,
        Err(v) => return v,
    };
    match store.list_pending(user_id, is_admin).await {
        Ok(list) => {
            let mut items = serde_json::to_value(&list).unwrap_or_else(|_| json!([]));
            if let Some(arr) = items.as_array_mut() {
                super::owner::inject_owners(state.db_pool.as_ref(), is_admin, arr, "user_id").await;
            }
            response::ok(json!({ "items": items }))
        }
        Err(e) => response::from_app_error(&e),
    }
}

/// 手动新增记忆（管理页）
pub async fn create_memory(state: &AppState, user_id: &str, body: Value) -> Value {
    let store = match memory_store(state) {
        Ok(s) => s,
        Err(v) => return v,
    };
    let content = match body["content"]
        .as_str()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        Some(c) => c.to_string(),
        None => return response::err(code::INVALID_PARAMS, "content 不能为空"),
    };
    let mem_t = parse_mem_type(&body);
    let (scope_v, assistant_id) = parse_scope(&body);
    match store
        .create(
            user_id,
            scope_v,
            assistant_id.as_deref(),
            mem_t,
            &content,
            None,
        )
        .await
    {
        Ok(m) => response::ok(json!(m)),
        Err(e) => response::from_app_error(&e),
    }
}

/// 编辑记忆正文/类型（普通用户仅自己的；管理员可改任意）
pub async fn update_memory(
    state: &AppState,
    user_id: &str,
    is_admin: bool,
    id: String,
    body: Value,
) -> Value {
    let store = match memory_store(state) {
        Ok(s) => s,
        Err(v) => return v,
    };
    let content = match body["content"]
        .as_str()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        Some(c) => c.to_string(),
        None => return response::err(code::INVALID_PARAMS, "content 不能为空"),
    };
    let mem_t = parse_mem_type(&body);
    match store.update(&id, user_id, is_admin, mem_t, &content).await {
        Ok(_) => response::ok(json!({ "id": id })),
        Err(e) => response::from_app_error(&e),
    }
}

/// 删除记忆（普通用户仅自己的；管理员可删任意）
pub async fn delete_memory(state: &AppState, user_id: &str, is_admin: bool, id: String) -> Value {
    let store = match memory_store(state) {
        Ok(s) => s,
        Err(v) => return v,
    };
    match store.delete(&id, user_id, is_admin).await {
        Ok(_) => response::ok(json!({ "id": id })),
        Err(e) => response::from_app_error(&e),
    }
}

/// 采纳记忆建议：claim（乐观锁 + 防越权）→ 转正写入 memories（管理员可采纳任意人的建议）
pub async fn accept_memory_proposal(
    state: &AppState,
    user_id: &str,
    is_admin: bool,
    id: String,
) -> Value {
    let p_store = match proposal_store(state) {
        Ok(s) => s,
        Err(v) => return v,
    };
    let m_store = match memory_store(state) {
        Ok(s) => s,
        Err(v) => return v,
    };
    let proposal = match p_store.claim(&id, user_id, is_admin).await {
        Ok(Some(p)) => p,
        Ok(None) => return response::err(code::NOT_FOUND, "建议不存在、非本人或已处理"),
        Err(e) => return response::from_app_error(&e),
    };
    match m_store
        .create(
            &proposal.user_id,
            proposal.scope,
            proposal.assistant_id.as_deref(),
            proposal.mem_type,
            &proposal.content,
            Some(&proposal.session_id),
        )
        .await
    {
        Ok(m) => response::ok(json!(m)),
        Err(e) => response::from_app_error(&e),
    }
}

/// 忽略记忆建议（管理员可忽略任意人的建议）
pub async fn reject_memory_proposal(
    state: &AppState,
    user_id: &str,
    is_admin: bool,
    id: String,
) -> Value {
    let store = match proposal_store(state) {
        Ok(s) => s,
        Err(v) => return v,
    };
    match store.reject(&id, user_id, is_admin).await {
        Ok(true) => response::ok(json!({ "id": id })),
        Ok(false) => response::err(code::NOT_FOUND, "建议不存在、非本人或已处理"),
        Err(e) => response::from_app_error(&e),
    }
}
