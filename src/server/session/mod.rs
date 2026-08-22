//! 会话管理模块 — 会话 CRUD 与历史记录读取
//!
//! 基于 adk-rust SessionService（PostgreSQL 持久化），支持：
//! - 创建会话（指定 agent_type，返回欢迎消息）
//! - 分页列表（按最后更新时间倒序，自动提取会话标题）
//! - 删除会话
//! - 重命名会话（通过 state_delta 持久化自定义标题）
//! - 获取会话历史（包含文本消息、工具调用、工具结果、待确认项）
//!
//! 原 REST 路由 `/api/sessions*` 已全部迁移到 GraphQL `sessions*` 系列字段。
//!
//! 拆分说明（架构 §4 拆分范例）：本目录由单文件 `session.rs` 拆分而来，
//! `mod.rs` 保留对外公共入口——鉴权（`resolve_effective_user` / `check_session_access`）、
//! 创建会话、分页列表，并对子模块做 `pub use` re-export 以保持对外路径不变：
//! - `settings.rs` 会话级设置改写（删除/重命名/模型绑定/思考级别/审批策略）
//! - `history.rs`  会话历史读取（`get_session_history` + 事件收集）
//! - `tests.rs`    体外单元测试（对齐 grep_tests.rs 先例）

use serde_json::{Value, json};

use super::AppState;
use super::response;

mod history;
mod settings;

#[cfg(test)]
mod tests;

pub use history::get_session_history;
pub use settings::{
    delete_session, get_session_permission_policy, get_session_thinking_level, rename_session,
    update_session_model, update_session_permission_policy, update_session_thinking_level,
};

/// 解析「有效用户」——决定用哪个 user_id 读写 ADK session。
///
/// - 普通用户 / 未登录：恒等于自己（`user_id`），无法越权。
/// - 管理员：取**会话归属者**（`session_settings.user_id` 反解）。ADK 表按 user_id 隔离，
///   必须用归属者 id 才能读到会话、且写入仍归在归属者名下——不改归属、不串记忆。
///   归属查不到（会话不存在）时返回 None，调用方按"会话不存在"处理（不泄露、不越权）。
///
/// 这是管理员「可见所有会话并可操作」的核心：管理员并不切换身份，而是对每个目标会话
/// 解析出它的真实归属者，以归属者名义完成 ADK 读写，归属关系保持不变。
pub(crate) async fn resolve_effective_user(
    state: &AppState,
    caller_user_id: &str,
    is_admin: bool,
    session_id: &str,
) -> Option<String> {
    if !is_admin {
        return Some(caller_user_id.to_string());
    }
    match &state.session_settings_store {
        Some(store) => store.get_owner(session_id).await.ok().flatten(),
        None => None,
    }
}

/// 会话写权限校验：仅归属人/管理员可改写会话设置（模型/思考级别/审批策略/标题）。
///
/// 与 [`resolve_effective_user`] 不同，后者对非管理员恒返回调用者本人（靠 ADK user_id 隔离
/// 隐式保护读取）；而 `session_settings` 表按 `session_id` 写入，**无 user_id 过滤**，
/// 故写操作必须显式反查归属（`get_owner`）校验。
///
/// **fail-closed**：归属查不到（无 settings 行，如尚未落档的新会话）→ **一律拒绝**（无法
/// 证明归属即不放过，含管理员）；新会话应先经 `init_session` 落档后再改设置。返回 `Err` 时已
/// 封装好错误响应（NOT_FOUND，不泄露存在性）。
pub(crate) async fn check_session_access(
    state: &AppState,
    caller_user_id: &str,
    is_admin: bool,
    session_id: &str,
) -> Result<(), Value> {
    let Some(store) = &state.session_settings_store else {
        return Err(response::err(response::code::DATABASE, "数据库未启用"));
    };
    match store.get_owner(session_id).await {
        Ok(Some(owner)) if is_admin || owner == caller_user_id => Ok(()),
        Ok(_) => Err(response::err(
            response::code::NOT_FOUND,
            "会话不存在或无权操作",
        )),
        Err(e) => Err(response::err(
            response::code::DATABASE,
            format!("查询失败: {e}"),
        )),
    }
}

// ========================================================================
//  会话 CRUD：创建 + 分页列表
// ========================================================================

pub async fn create_session(state: &AppState, user_id: &str, body: Value) -> Value {
    let agent_type = body
        .get("agent_type")
        .and_then(|v| v.as_str())
        .unwrap_or("custom");
    // 支持前端传入 session_id 和 title
    let session_id = body
        .get("session_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| uuid::Uuid::now_v7().to_string());
    let custom_title = body.get("title").and_then(|v| v.as_str()).unwrap_or("");
    // 前端可携带初始 model_id（具体 UUID、'default' 或缺省）
    let model_id = body
        .get("model_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim().to_string());
    // 前端可携带 assistant_id（绑定自定义/内置助手到会话）
    let assistant_id = body
        .get("assistant_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim().to_string());
    let now = chrono::Utc::now().to_rfc3339();

    let create_req = adk_rust::session::CreateRequest {
        app_name: "cortex-agent".to_string(),
        user_id: user_id.to_string(),
        session_id: Some(session_id.clone()),
        state: build_initial_state(agent_type, &assistant_id, custom_title),
    };

    match state.adk_session_service.create(create_req).await {
        Ok(_) => {
            persist_session_settings(
                state,
                &session_id,
                user_id,
                agent_type,
                custom_title,
                &model_id,
                &assistant_id,
            )
            .await;
            let welcome = fetch_welcome(state, &assistant_id).await;
            let title = if custom_title.is_empty() {
                session_id.clone()
            } else {
                custom_title.to_string()
            };
            response::ok(json!({
                "id": session_id,
                "title": title,
                "agent_type": agent_type,
                "assistant_id": assistant_id,
                "model_id": model_id,
                "created_at": now,
                "welcome_message": welcome,
            }))
        }
        Err(e) => response::err(response::code::DATABASE, e.to_string()),
    }
}

// ===== create_session 辅助函数 =====

/// 构造会话初始 state（agent_type 必填；assistant_id / 自定义标题按需写入）
fn build_initial_state(
    agent_type: &str,
    assistant_id: &Option<String>,
    custom_title: &str,
) -> std::collections::HashMap<String, serde_json::Value> {
    let mut state = std::collections::HashMap::new();
    state.insert(
        "agent_type".to_string(),
        serde_json::Value::String(agent_type.to_string()),
    );
    if let Some(aid) = assistant_id {
        state.insert(
            "assistant_id".to_string(),
            serde_json::Value::String(aid.clone()),
        );
    }
    if !custom_title.is_empty() {
        state.insert(
            "app:title".to_string(),
            serde_json::Value::String(custom_title.to_string()),
        );
    }
    state
}

/// 创建会话时落初始配置行（session_settings 大表）。
/// user_id / title / agent_type / model_id / assistant_id 一次写入；
/// model_id 为 'default'/'auto'/None 时不绑定（置 NULL，运行时解析全局默认）。
async fn persist_session_settings(
    state: &AppState,
    session_id: &str,
    user_id: &str,
    agent_type: &str,
    title: &str,
    model_id: &Option<String>,
    assistant_id: &Option<String>,
) {
    let Some(store) = &state.session_settings_store else {
        return;
    };
    let bound_model = model_id
        .as_deref()
        .filter(|m| *m != "default" && *m != "auto");
    if let Err(e) = store
        .init_session(
            session_id,
            user_id,
            title,
            agent_type,
            bound_model,
            assistant_id.as_deref(),
        )
        .await
    {
        tracing::warn!("[create_session] 写入会话配置失败: {}", e);
    }
}

/// 欢迎语取自数据库助手 greeting 字段（无助手 / 未找到则空串）
async fn fetch_welcome(state: &AppState, assistant_id: &Option<String>) -> String {
    let (Some(store), Some(aid)) = (&state.assistant_store, assistant_id) else {
        return String::new();
    };
    match store.get(aid).await {
        Ok(Some(a)) => a.greeting,
        _ => String::new(),
    }
}

#[derive(Debug, Clone, serde::Deserialize, Default)]
pub struct SessionListParams {
    pub page: Option<usize>,
    pub page_size: Option<usize>,
    pub keyword: Option<String>,
    pub agent_type: Option<String>,
    pub kind: Option<i16>,
    /// 按绑定的助手 ID 过滤（含运行时切换后的绑定，新表为权威来源）
    pub assistant_id: Option<String>,
}

pub async fn list_sessions(
    state: &AppState,
    user_id: &str,
    is_admin: bool,
    params: SessionListParams,
) -> Value {
    let filters = parse_list_filters(&params);
    let Some(store) = &state.session_settings_store else {
        return response::err(response::code::DATABASE, "数据库未启用，无法列出会话");
    };
    let (rows, total) = match store
        .list_page(
            user_id,
            is_admin,
            filters.page,
            filters.page_size,
            filters.keyword.as_deref(),
            filters.agent_filter.as_deref(),
            filters.assistant_filter.as_deref(),
            filters.kind_filter,
        )
        .await
    {
        Ok(v) => v,
        Err(e) => return response::err(response::code::DATABASE, e.to_string()),
    };

    let total = total as usize;
    let total_pages = if total.is_multiple_of(filters.page_size) {
        total / filters.page_size
    } else {
        total / filters.page_size + 1
    };

    let result: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            let updated_at = r.updated_at.to_rfc3339();
            // 创建时间从 session_id（UUID v7）解出，重命名/更新后保持不变；
            // 解析失败（非 v7 id）时回退 updated_at，保证字段始终有值。
            let created_at = uuid_v7_millis(&r.sid)
                .and_then(|ms| chrono::DateTime::from_timestamp_millis(ms as i64))
                .map(|dt| dt.to_rfc3339())
                .unwrap_or_else(|| updated_at.clone());
            // 标题空串（未设置）时回退显示 session_id
            let title = if r.title.is_empty() {
                r.sid.clone()
            } else {
                r.title
            };
            let mut obj = json!({
                "id": r.sid,
                "title": title,
                "agent_type": r.agent_type,
                "assistant_id": r.aid,
                "assistant_name": r.assistant_name,
                "assistant_kind": r.assistant_kind,
                "model_id": r.mid,
                "created_at": created_at,
                "updated_at": updated_at,
            });
            // 管理员视图标注归属（谁的会话），便于列表区分他人会话
            if is_admin {
                obj["owner"] = json!(r.owner);
            }
            obj
        })
        .collect();

    response::ok(json!({
        "sessions": result,
        "total": total,
        "page": filters.page,
        "page_size": filters.page_size,
        "total_pages": total_pages,
    }))
}

// ===== list_sessions 辅助函数 =====

struct ListFilters {
    page_size: usize,
    page: usize,
    keyword: Option<String>,
    agent_filter: Option<String>,
    kind_filter: Option<i16>,
    assistant_filter: Option<String>,
}

fn parse_list_filters(params: &SessionListParams) -> ListFilters {
    let trim_some = |s: &Option<String>| {
        s.as_deref()
            .map(str::trim)
            .filter(|x| !x.is_empty())
            .map(|x| x.to_string())
    };
    ListFilters {
        page_size: params.page_size.unwrap_or(20).clamp(1, 100),
        page: params.page.unwrap_or(1).max(1),
        keyword: params
            .keyword
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string()),
        agent_filter: trim_some(&params.agent_type),
        kind_filter: params.kind,
        assistant_filter: trim_some(&params.assistant_id),
    }
}

/// 从 UUID v7 字符串解析出创建毫秒时间戳（前 48 位）。
/// 仅当确为 v7 时返回 Some；非 v7 / 非法 id 返回 None。
fn uuid_v7_millis(id: &str) -> Option<u64> {
    let u = uuid::Uuid::parse_str(id).ok()?;
    let (secs, nanos) = u.get_timestamp()?.to_unix();
    Some(
        secs.saturating_mul(1000)
            .saturating_add((nanos / 1_000_000) as u64),
    )
}
