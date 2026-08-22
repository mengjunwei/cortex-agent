//! 会话级设置改写：删除 / 重命名 / 模型绑定 / 思考级别 / 审批策略。

use serde_json::{Value, json};

use super::super::AppState;
use super::super::response;
use super::{check_session_access, resolve_effective_user};
use crate::permissions::{ApprovalPolicy, SandboxMode};

pub async fn delete_session(state: &AppState, user_id: &str, is_admin: bool, id: &str) -> Value {
    // 归属校验：仅归属人/管理员可删除（防他人销毁 session_settings/沙箱目录/工作区快照——
    // 这些资源按 session_id 裸操作，仅靠 ADK 的 user_id 隔离不足以保护）
    if let Err(v) = check_session_access(state, user_id, is_admin, id).await {
        return v;
    }
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
        crate::infra::sandbox::workspace_snapshot::delete(os, id).await;
    }

    // 清理节点本地 shell 环境快照（{data_dir}/shell_snapshots/{sid}.sh）
    crate::infra::sandbox::shell_snapshot::delete(std::path::Path::new(&state.config.data_dir), id).await;

    // 原 REST 返回 204 No Content（无 body）。GraphQL 统一返回 JSON。
    response::ok(json!({ "deleted": true, "id": id }))
}

pub async fn rename_session(
    state: &AppState,
    user_id: &str,
    is_admin: bool,
    id: &str,
    title: &str,
) -> Value {
    let new_title = title.trim().to_string();

    if new_title.is_empty() {
        return response::err(response::code::INVALID_PARAMS, "标题不能为空");
    }

    // 归属校验：仅归属人/管理员可重命名（防他人篡改会话标题）
    if let Err(v) = check_session_access(state, user_id, is_admin, id).await {
        return v;
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
pub async fn update_session_model(
    state: &AppState,
    user_id: &str,
    is_admin: bool,
    id: &str,
    model_id: &str,
) -> Value {
    let model_id = model_id.trim();

    // 归属校验：仅归属人/管理员可改会话绑定的模型
    if let Err(v) = check_session_access(state, user_id, is_admin, id).await {
        return v;
    }

    let bound: Option<&str> = if model_id.is_empty() || model_id == "default" || model_id == "auto"
    {
        None
    } else {
        Some(model_id)
    };

    let Some(store) = &state.session_settings_store else {
        return response::err(response::code::DATABASE, "数据库未启用，无法持久化模型绑定");
    };
    // 切换前的绑定（对比判断是否发生实际切换；解析失败按未绑定处理）
    let prev_bound: Option<String> = store.get_model(id).await.unwrap_or(None);
    match store.set_model(id, bound).await {
        Ok(_) => {
            match bound {
                Some(m) => tracing::info!("[模型绑定] 会话 {} 绑定模型: {}", id, m),
                None => tracing::info!("[模型绑定] 会话 {} 解除模型绑定（回退全局默认）", id),
            }
            // 模型发生实际切换 → 落一条 system 事件（时间线标记，对齐重命名的 state_delta 模式）：
            // 前端据此在会话详情渲染「模型已切换 A → B」分隔条；重进会话从 history 恢复。
            // state_delta 只进 session state、不进 LLM 回放上下文，不污染对话。
            let from_label = model_display_label(
                state.model_provider_store.as_deref(),
                user_id,
                prev_bound.as_deref(),
            );
            let to_label =
                model_display_label(state.model_provider_store.as_deref(), user_id, bound);
            if prev_bound.as_deref() != bound {
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
                                "app:model_switched".to_string(),
                                json!({ "from": from_label, "to": to_label }),
                            );
                            m
                        },
                        ..Default::default()
                    },
                    long_running_tool_ids: vec![],
                    llm_request: None,
                    provider_metadata: Default::default(),
                };
                if let Err(e) = state.adk_session_service.append_event(id, event).await {
                    tracing::warn!("[模型切换] 会话 {} 事件落库失败: {}", id, e);
                }
            }
            // from/to 恒返回（未切换时两者相等），前端比对不等即在时间线插分隔条
            response::ok(json!({ "model_id": bound, "from": from_label, "to": to_label }))
        }
        Err(e) => response::err(response::code::DATABASE, e.to_string()),
    }
}

/// 解析模型显示标签：`厂商/模型 · 协议`（如 `glm2/glm-5.3 · Anthropic`）
///
/// - `None` / 空（未绑定）→ `默认模型`（运行时才解析，协议未知不标注）
/// - 命中供应商内存缓存 → `provider_name/model · protocol中文标签`；解析失败回退原始 id
fn model_display_label(
    model_provider_store: Option<&crate::domain::model_provider::store::ModelProviderStore>,
    user_id: &str,
    model_id: Option<&str>,
) -> String {
    let Some(id) = model_id.map(str::trim).filter(|v| !v.is_empty()) else {
        return "默认模型".to_string();
    };
    let Some(store) = model_provider_store else {
        return id.to_string();
    };
    match store.resolve_model(Some(id), user_id) {
        Ok(m) => format!("{}/{} · {}", m.provider_name, m.model, m.protocol.label()),
        Err(_) => id.to_string(),
    }
}

/// 读取会话级思考级别（不存在或读取失败 → 默认 high）
pub async fn get_session_thinking_level(
    state: &AppState,
    user_id: &str,
    is_admin: bool,
    id: &str,
) -> Value {
    // 归属校验：非归属者非管理员 → 返回默认（不泄露他人配置）
    if check_session_access(state, user_id, is_admin, id)
        .await
        .is_err()
    {
        return response::ok(json!({ "thinking_level": "high" }));
    }
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
pub async fn update_session_thinking_level(
    state: &AppState,
    user_id: &str,
    is_admin: bool,
    id: &str,
    level: &str,
) -> Value {
    let level = level.trim();
    if !matches!(level, "low" | "medium" | "high" | "xhigh" | "max") {
        return response::err(
            response::code::INVALID_PARAMS,
            format!("非法的思考级别: {level}"),
        );
    }
    // 归属校验：仅归属人/管理员可改
    if let Err(v) = check_session_access(state, user_id, is_admin, id).await {
        return v;
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
pub async fn get_session_permission_policy(
    state: &AppState,
    user_id: &str,
    is_admin: bool,
    id: &str,
) -> Value {
    // 归属校验：非归属者非管理员 → 返回全局默认（不泄露他人配置）
    if check_session_access(state, user_id, is_admin, id)
        .await
        .is_err()
    {
        let (sm, ap) = config_permission_defaults(state);
        return response::ok(json!({
            "sandbox_mode": sm.codex_id(),
            "approval_policy": ap.codex_id(),
        }));
    }
    let (sandbox_mode, approval_policy) = match &state.session_settings_store {
        Some(store) => match store.get_permission_policy(id).await {
            Ok(Some(p)) => p,
            _ => config_permission_defaults(state),
        },
        None => config_permission_defaults(state),
    };
    // 会话来源（普通=0 / 定时任务=1），供前端把定时任务会话渲染为只读回放。
    // title 一并返回：定时会话不在普通会话列表（source_type=0 过滤），前端列表取不到标题，需此处兜底。
    let (source_type, schedule_task_id, title) = match &state.session_settings_store {
        Some(store) => store
            .get_source_info(id)
            .await
            .ok()
            .flatten()
            .unwrap_or((0, None, None)),
        None => (0, None, None),
    };
    response::ok(json!({
        "sandbox_mode": sandbox_mode.codex_id(),
        "approval_policy": approval_policy.codex_id(),
        "source_type": source_type,
        "schedule_task_id": schedule_task_id,
        "title": title,
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
    user_id: &str,
    is_admin: bool,
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
    // auto（自动批准）仅供无人值守定时任务由后端内部写入，交互会话不开放——
    // 否则用户可绕过审批让危险命令自动放行。
    if ap == ApprovalPolicy::Auto {
        return response::err(
            response::code::INVALID_PARAMS,
            "approval_policy=auto 仅定时任务可用".to_string(),
        );
    }
    // 归属校验：仅归属人/管理员可改（防他人篡改沙箱/审批策略 → 提权）
    if let Err(v) = check_session_access(state, user_id, is_admin, id).await {
        return v;
    }
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
