//! 会话历史读取：get_session_history + 事件收集（文本/工具调用/工具结果/待确认）。

use std::collections::HashSet;

use serde_json::{Value, json};

use super::super::AppState;
use super::super::response;
use super::resolve_effective_user;

pub async fn get_session_history(
    state: &AppState,
    user_id: &str,
    is_admin: bool,
    id: &str,
) -> Value {
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

    // 注入会话级 token 用量快照：重进会话时前端立即恢复「已用 / 阈值」显示（对齐 codex
    // 会话级 token_info 持久化）。total=0（未产生用量）→ null，前端回退到无数据态。
    let token_usage = match &state.session_settings_store {
        Some(store) => store.get_token_usage(id).await.unwrap_or(None),
        None => None,
    };
    let token_usage_json = token_usage
        .filter(|(total, _)| *total > 0)
        .map(|(total, threshold)| json!({ "total_tokens": total, "threshold": threshold }));

    response::ok(json!({
        "id": id,
        "agent_type": agent_type,
        "assistant_id": assistant_id,
        "assistant_name": assistant_name,
        "assistant_kind": assistant_kind,
        "messages": messages,
        "pending_confirmation": pending_confirmation,
        "model_id": bound_model_id,
        "token_usage": token_usage_json,
    }))
}

// ===== get_session_history 辅助函数 =====

/// 遍历会话事件，收集对话消息 + 当前待确认工具调用
fn collect_history_messages(events: &dyn adk_rust::session::Events) -> (Vec<Value>, Option<Value>) {
    let mut result: Vec<Value> = Vec::new();
    let mut pending_confirmation: Option<Value> = None;
    // 是否已有正文消息（user/assistant）：模型切换标记只在对话中途渲染，
    // 空会话先切模型再开聊时置顶一条分隔条无信息量，跳过。
    let mut has_conversation = false;
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
        // 模型切换标记（update_session_model 落的 system 事件 state_delta）：
        // 渲染成「模型已切换 A → B」分隔条（仅在已有对话后）。
        if event.author == "system"
            && let Some(sw) = event.actions.state_delta.get("app:model_switched")
            && has_conversation
        {
            result.push(json!({
                "role": "model_switched",
                "from": sw.get("from").cloned().unwrap_or(Value::Null),
                "to": sw.get("to").cloned().unwrap_or(Value::Null),
                "timestamp": ts,
            }));
            continue;
        }
        // 产物卡片（shell_command 落的 system 事件 state_delta["app:artifact"]）：
        // 恢复文件下载卡片，刷新页面后不丢失。与模型切换标记同为时间线标记，
        // 事件在工具执行期间落库，天然位于 tool 卡片之后、正确的时间线位置。
        if event.author == "system"
            && let Some(a) = event.actions.state_delta.get("app:artifact")
        {
            result.push(json!({
                "role": "artifact",
                "content": a.clone(),
                "timestamp": ts,
            }));
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
                        has_conversation = true;
                        result.push(json!({
                            "role": role,
                            "content": text,
                            "timestamp": ts,
                        }));
                    }
                    adk_rust::Part::FunctionCall { name, args, id, .. } => {
                        result.push(json!({
                            "role": "tool",
                            "name": super::super::sse::tool_display_name(name),
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
                            "name": super::super::sse::tool_display_name(&function_response.name),
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
    // 后处理：无配对 tool_result 的工具调用（用户取消 / runner 中断）→ 标记 aborted。
    // 否则前端按 "calling" → running 渲染，刷新历史后永久卡「沙箱执行中」。
    let answered: HashSet<String> = result
        .iter()
        .filter_map(|m| {
            if m.get("role").and_then(|r| r.as_str()) == Some("tool_result") {
                m.get("tool_call_id")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string())
            } else {
                None
            }
        })
        .collect();
    for m in result.iter_mut() {
        if m.get("role").and_then(|r| r.as_str()) == Some("tool") {
            let id = m.get("tool_call_id").and_then(|v| v.as_str()).unwrap_or("");
            // id 为空（无配对依据）或确认无配对 result → aborted
            if id.is_empty() || !answered.contains(id) {
                if let Some(obj) = m.as_object_mut() {
                    obj.insert("status".to_string(), json!("aborted"));
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
