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

use serde::Deserialize;
use serde_json::{Value, json};

use super::AppState;
use super::response;
use crate::domain::permissions::{ApprovalPolicy, SandboxMode};

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

// ========================================================================
//  会话 CRUD + 历史读取
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
            persist_session_settings(state, &session_id, user_id, agent_type, custom_title, &model_id, &assistant_id).await;
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

#[derive(Debug, Clone, Deserialize, Default)]
pub struct SessionListParams {
    pub page: Option<usize>,
    pub page_size: Option<usize>,
    pub keyword: Option<String>,
    pub agent_type: Option<String>,
    pub kind: Option<i16>,
    /// 按绑定的助手 ID 过滤（含运行时切换后的绑定，新表为权威来源）
    pub assistant_id: Option<String>,
}

pub async fn list_sessions(state: &AppState, user_id: &str, is_admin: bool, params: SessionListParams) -> Value {
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
            let title = if r.title.is_empty() { r.sid.clone() } else { r.title };
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
    Some(secs.saturating_mul(1000).saturating_add((nanos / 1_000_000) as u64))
}

pub async fn delete_session(state: &AppState, user_id: &str, is_admin: bool, id: &str) -> Value {
    // 有效用户：管理员删除他人会话时，截图清理与 ADK 删除都要用**归属者**的 user_id
    // （这两处按 user_id 做归属校验/定位），删除的是归属者名下的会话，归属不变。
    // 归属查不到 → 非归属者删除会落空（ADK 删不到），退化为调用者自身（删不到即无操作）。
    let effective_user = resolve_effective_user(state, user_id, is_admin, id)
        .await
        .unwrap_or_else(|| user_id.to_string());
    // 先清理该会话关联的截图对象（透传 user_id 做归属校验，防跨用户删除）
    if let Some(os) = &state.object_store {
        crate::infra::screenshot_cleanup::delete_session_screenshots(
            &state.adk_session_service,
            &effective_user,
            id,
            os,
        )
        .await;
    } else {
        tracing::warn!("[删除] 对象存储未启用，跳过会话 {} 截图清理", id);
    }

    let delete_req = adk_rust::session::DeleteRequest {
        app_name: "cortex-agent".to_string(),
        user_id: effective_user,
        session_id: id.to_string(),
    };
    if let Err(e) = state.adk_session_service.delete(delete_req).await {
        tracing::warn!("[删除] PostgreSQL 会话删除失败: {}", e);
    }

    // 清理会话级配置（session_settings 整行：模型/助手/思考级别/沙箱审批/标题）
    if let Some(store) = &state.session_settings_store {
        if let Err(e) = store.delete(id).await {
            tracing::warn!("[删除] 清理会话配置失败: {}", e);
        }
    }

    // 清理代码助手 session 级沙箱目录（{data_dir}/workspaces/sessions/{session_id}）
    // 失败仅告警，不阻断会话删除
    let sandbox_dir = state.config.workspace_session_dir(id);
    if sandbox_dir.exists() {
        match tokio::fs::remove_dir_all(&sandbox_dir).await {
            Ok(_) => tracing::info!("[删除] 已清理 session 沙箱目录: {}", sandbox_dir.display()),
            Err(e) => tracing::warn!(
                "[删除] 清理 session 沙箱目录失败（可忽略）: {} - {e}",
                sandbox_dir.display()
            ),
        }
    }

    // 清理对象存储中的沙箱快照(workspaces/{sid}/ 前缀)
    if let Some(os) = &state.object_store {
        crate::infra::workspace_snapshot::delete(os, id).await;
    }

    // 清理节点本地 shell 环境快照（{data_dir}/shell_snapshots/{sid}.sh）
    crate::infra::shell_snapshot::delete(std::path::Path::new(&state.config.data_dir), id).await;

    // 原 REST 返回 204 No Content（无 body）。GraphQL 统一返回 JSON。
    response::ok(json!({ "deleted": true, "id": id }))
}

pub async fn rename_session(state: &AppState, id: &str, title: &str) -> Value {
    let new_title = title.trim().to_string();

    if new_title.is_empty() {
        return response::err(response::code::INVALID_PARAMS, "标题不能为空");
    }

    // 通过 append_event + state_delta 持久化自定义标题
    let event = adk_rust::Event {
        id: uuid::Uuid::now_v7().to_string(),
        timestamp: chrono::Utc::now(),
        invocation_id: uuid::Uuid::now_v7().to_string(),
        branch: String::new(),
        author: "system".to_string(),
        llm_response: adk_rust::LlmResponse {
            content: None,
            ..Default::default()
        },
        actions: adk_rust::EventActions {
            state_delta: {
                let mut m = std::collections::HashMap::new();
                m.insert(
                    "app:title".to_string(),
                    serde_json::Value::String(new_title.clone()),
                );
                m
            },
            ..Default::default()
        },
        long_running_tool_ids: vec![],
        llm_request: None,
        provider_metadata: Default::default(),
    };

    match state.adk_session_service.append_event(id, event).await {
        Ok(_) => {
            // 同步物化标题到 session_settings（列表 SQL 直接读取，避免从事件/state 反解）
            if let Some(store) = &state.session_settings_store {
                if let Err(e) = store.set_title(id, &new_title).await {
                    tracing::warn!("[重命名] 会话 {} 标题落库失败: {}", id, e);
                }
            }
            tracing::info!("[重命名] 会话 {} 标题更新为: {}", id, new_title);
            response::ok(json!({ "title": new_title }))
        }
        Err(e) => {
            tracing::warn!("[重命名] 会话 {} 重命名失败: {}", id, e);
            response::err(response::code::DATABASE, e.to_string())
        }
    }
}

/// 更新会话绑定的模型
///
/// - `model_id` 为具体 UUID 时持久化绑定
/// - `model_id` 为 `"default"` / `"auto"` / 空 时解除绑定（置 NULL，运行时解析默认）
pub async fn update_session_model(state: &AppState, id: &str, model_id: &str) -> Value {
    let model_id = model_id.trim();

    let bound: Option<&str> =
        if model_id.is_empty() || model_id == "default" || model_id == "auto" {
            None
        } else {
            Some(model_id)
        };

    let Some(store) = &state.session_settings_store else {
        return response::err(response::code::DATABASE, "数据库未启用，无法持久化模型绑定");
    };
    match store.set_model(id, bound).await {
        Ok(_) => {
            match bound {
                Some(m) => tracing::info!("[模型绑定] 会话 {} 绑定模型: {}", id, m),
                None => tracing::info!("[模型绑定] 会话 {} 解除模型绑定（回退全局默认）", id),
            }
            response::ok(json!({ "model_id": bound }))
        }
        Err(e) => response::err(response::code::DATABASE, e.to_string()),
    }
}

/// 读取会话级思考级别（不存在或读取失败 → 默认 high）
pub async fn get_session_thinking_level(state: &AppState, id: &str) -> Value {
    let level = match &state.session_settings_store {
        Some(store) => match store.get_thinking_level(id).await {
            Ok(Some(lvl)) => lvl,
            _ => "high".to_string(),
        },
        None => "high".to_string(),
    };
    response::ok(json!({ "thinking_level": level }))
}

/// 更新会话级思考级别（low/medium/high/xhigh/max）
pub async fn update_session_thinking_level(state: &AppState, id: &str, level: &str) -> Value {
    let level = level.trim();
    if !matches!(level, "low" | "medium" | "high" | "xhigh" | "max") {
        return response::err(
            response::code::INVALID_PARAMS,
            format!("非法的思考级别: {level}"),
        );
    }
    match &state.session_settings_store {
        Some(store) => match store.set_thinking_level(id, level).await {
            Ok(_) => {
                tracing::info!("[思考级别] 会话 {} 设置: {}", id, level);
                response::ok(json!({ "thinking_level": level }))
            }
            Err(e) => response::err(response::code::DATABASE, e.to_string()),
        },
        None => response::err(response::code::DATABASE, "数据库未启用，无法持久化思考级别"),
    }
}

/// 读取会话级审批方式（未设置 / 读取失败 → 全局 [shell] 配置默认值）。
/// network_access 不在会话级存储（始终由全局 config 决定），故不在此返回。
pub async fn get_session_permission_policy(state: &AppState, id: &str) -> Value {
    let (sandbox_mode, approval_policy) = match &state.session_settings_store {
        Some(store) => match store.get_permission_policy(id).await {
            Ok(Some(p)) => p,
            _ => config_permission_defaults(state),
        },
        None => config_permission_defaults(state),
    };
    response::ok(json!({
        "sandbox_mode": sandbox_mode.codex_id(),
        "approval_policy": approval_policy.codex_id(),
    }))
}

/// 全局 [shell] 配置的默认审批方式（沙箱模式 + 审批策略）
fn config_permission_defaults(state: &AppState) -> (SandboxMode, ApprovalPolicy) {
    let p = state.config.shell.permission_policy();
    (p.sandbox_mode, p.approval_policy)
}

/// 更新会话级审批方式（沙箱模式 + 审批策略）
pub async fn update_session_permission_policy(
    state: &AppState,
    id: &str,
    sandbox_mode: &str,
    approval_policy: &str,
) -> Value {
    let Some(sm) = SandboxMode::from_codex_id(sandbox_mode.trim()) else {
        return response::err(
            response::code::INVALID_PARAMS,
            format!("非法的 sandbox_mode: {sandbox_mode}"),
        );
    };
    let Some(ap) = ApprovalPolicy::from_codex_id(approval_policy.trim()) else {
        return response::err(
            response::code::INVALID_PARAMS,
            format!("非法的 approval_policy: {approval_policy}"),
        );
    };
    match &state.session_settings_store {
        Some(store) => match store.set_permission_policy(id, sm, ap).await {
            Ok(_) => {
                tracing::info!(
                    "[审批方式] 会话 {} 设置: sandbox={}, approval={}",
                    id,
                    sm.codex_id(),
                    ap.codex_id()
                );
                response::ok(json!({
                    "sandbox_mode": sm.codex_id(),
                    "approval_policy": ap.codex_id(),
                }))
            }
            Err(e) => response::err(response::code::DATABASE, e.to_string()),
        },
        None => response::err(response::code::DATABASE, "数据库未启用，无法持久化审批方式"),
    }
}

pub async fn get_session_history(state: &AppState, user_id: &str, is_admin: bool, id: &str) -> Value {
    // 有效用户：管理员解析为会话归属者（读其 ADK 会话，不改归属）；普通用户=自己。
    // 归属查不到 → 按空历史返回（不泄露是否存在，也不越权）。
    let effective_user = resolve_effective_user(state, user_id, is_admin, id)
        .await
        .unwrap_or_else(|| user_id.to_string());
    let get_req = adk_rust::session::GetRequest {
        app_name: "cortex-agent".to_string(),
        user_id: effective_user,
        session_id: id.to_string(),
        num_recent_events: None,
        after: None,
    };

    let (mut agent_type, mut assistant_id): (Option<String>, Option<String>) = (None, None);
    let mut messages: Vec<Value> = Vec::new();
    let mut pending_confirmation: Option<Value> = None;
    match state.adk_session_service.get(get_req).await {
        Ok(session) => {
            // 先提取 agent_type / assistant_id（owned String），结束对 session.state 的借用
            agent_type = session
                .state()
                .get("agent_type")
                .and_then(|v| v.as_str().map(String::from));
            assistant_id = session
                .state()
                .get("assistant_id")
                .and_then(|v| v.as_str().map(String::from));
            let events = session.events();
            tracing::info!("[history] session={} events={}", id, events.len());
            let (msgs, pc) = collect_history_messages(events);
            messages = msgs;
            pending_confirmation = pc;
        }
        Err(e) => tracing::warn!("[history] 读取会话失败: {}", e),
    }

    // 注入会话绑定的 model_id（session_settings 权威来源）
    let bound_model_id = match &state.session_settings_store {
        Some(store) => store.get_model(id).await.unwrap_or(None),
        None => None,
    };
    // 注入运行时切换后的 assistant_id（session_settings 权威来源，覆盖 session.state 的初始绑定）
    if let Some(store) = &state.session_settings_store {
        if let Some(aid) = store.get_assistant(id).await.unwrap_or(None) {
            assistant_id = Some(aid);
        }
    }
    // 注入助手名称与类型（kind），供前端展示当前会话绑定的助手信息
    let (assistant_name, assistant_kind) = resolve_assistant_meta(state, &assistant_id).await;

    response::ok(json!({
        "id": id,
        "agent_type": agent_type,
        "assistant_id": assistant_id,
        "assistant_name": assistant_name,
        "assistant_kind": assistant_kind,
        "messages": messages,
        "pending_confirmation": pending_confirmation,
        "model_id": bound_model_id,
    }))
}

// ===== get_session_history 辅助函数 =====

/// 遍历会话事件，收集对话消息 + 当前待确认工具调用
fn collect_history_messages(events: &dyn adk_rust::session::Events) -> (Vec<Value>, Option<Value>) {
    let mut result: Vec<Value> = Vec::new();
    let mut pending_confirmation: Option<Value> = None;
    for i in 0..events.len() {
        let Some(event) = events.at(i) else {
            continue;
        };
        let ts = event.timestamp.to_rfc3339();
        // 压缩检查点（L1 跨轮 / L3 intra-turn）：渲染成「已压缩」分隔标记，
        // 不把摘要当 assistant 正文（修此前 collect_history_messages 的 UX bug）
        if event.actions.compaction.is_some() {
            result.push(json!({ "role": "compacted", "timestamp": ts }));
            continue;
        }
        if let Some(ref tc) = event.actions.tool_confirmation {
            pending_confirmation = Some(json!({
                "tool_name": tc.tool_name,
                "function_call_id": tc.function_call_id.clone().unwrap_or_default(),
                "args": tc.args,
            }));
        } else {
            pending_confirmation = None;
        }

        if let Some(content) = &event.llm_response.content {
            for part in &content.parts {
                match part {
                    adk_rust::Part::Text { text } if !text.is_empty() => {
                        let role = if event.author == "user" {
                            "user"
                        } else {
                            "assistant"
                        };
                        result.push(json!({
                            "role": role,
                            "content": text,
                            "timestamp": ts,
                        }));
                    }
                    adk_rust::Part::FunctionCall { name, args, id, .. } => {
                        result.push(json!({
                            "role": "tool",
                            "name": super::sse::tool_display_name(name),
                            "tool_call_id": id.clone().unwrap_or_default(),
                            "args": args,
                            "status": "calling",
                            "timestamp": ts,
                        }));
                    }
                    adk_rust::Part::FunctionResponse {
                        function_response,
                        id,
                        ..
                    } => {
                        result.push(json!({
                            "role": "tool_result",
                            "name": super::sse::tool_display_name(&function_response.name),
                            "tool_call_id": id.clone().unwrap_or_default(),
                            "content": function_response.response,
                            "status": "done",
                            "timestamp": ts,
                        }));
                    }
                    _ => {}
                }
            }
        }
    }
    (result, pending_confirmation)
}

/// 查询会话当前助手的 name/kind（无助手或未找到返回 (None, None)）
async fn resolve_assistant_meta(
    state: &AppState,
    assistant_id: &Option<String>,
) -> (Option<String>, Option<i16>) {
    let (Some(store), Some(aid)) = (&state.assistant_store, assistant_id) else {
        return (None, None);
    };
    match store.get(aid).await {
        Ok(Some(a)) => (Some(a.name.clone()), Some(a.kind.as_i16())),
        _ => (None, None),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn session_id_generation_is_uuid_v7() {
        let id = uuid::Uuid::now_v7().to_string();
        let parsed = uuid::Uuid::parse_str(&id).unwrap();
        assert_eq!(parsed.get_version_num(), 7);
        // Check RFC 4122 variant
        let bytes = parsed.as_bytes();
        let variant_byte = bytes[8];
        assert!(
            (variant_byte & 0b11000000 == 0b10000000) || (variant_byte & 0b11000000 == 0b11000000),
            "must be RFC4122 variant (bit 6 set, or 7 set for legacy)"
        );
    }

    #[test]
    fn uuid_v7_millis_extracts_timestamp() {
        let id = uuid::Uuid::now_v7().to_string();
        let ms = super::uuid_v7_millis(&id).expect("v7 id 应能解出毫秒");
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        // 解出的创建毫秒应贴近当前时间（容差 1 分钟）
        assert!(
            ms.abs_diff(now_ms) < 60_000,
            "解出的毫秒 {ms} 与当前 {now_ms} 相差过大"
        );
    }

    #[test]
    fn uuid_v7_millis_rejects_non_v7() {
        // v4 id 无时间戳，应返回 None
        let v4 = uuid::Uuid::new_v4().to_string();
        assert!(super::uuid_v7_millis(&v4).is_none());
        // 非法字符串
        assert!(super::uuid_v7_millis("not-a-uuid").is_none());
    }

    #[test]
    fn uuid_v7_string_descending_is_creation_descending() {
        // 连续生成两个 v7 id（后者更新），字符串倒序应把更新的排前面
        let older = uuid::Uuid::now_v7().to_string();
        // 确保跨过毫秒边界，避免同毫秒随机位影响
        std::thread::sleep(std::time::Duration::from_millis(2));
        let newer = uuid::Uuid::now_v7().to_string();
        let mut ids = vec![older.clone(), newer.clone()];
        ids.sort_by(|a, b| b.cmp(a)); // 与列表排序同逻辑
        assert_eq!(ids[0], newer, "倒序后最新创建的应排第一");
        assert_eq!(ids[1], older);
    }
}
