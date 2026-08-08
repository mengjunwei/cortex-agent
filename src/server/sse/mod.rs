//! SSE（Server-Sent Events）流式对话处理模块
//!
//! 核心职责：
//! - 接收前端对话请求，通过 assistant_id 加载助手并构建 Agent
//! - 通过 adk-rust Runner 执行 Agent，将事件流转换为 SSE 格式推送到前端
//! - 处理工具调用确认（tool_confirmation）
//! - 支持任务取消（CancellationToken）
//! - 流结束后手动持久化 AI 回复到 PostgreSQL
//!
//! ## 模块结构
//!
//! | 子模块 | 职责 |
//! |--------|------|
//! | [`types`] | 请求/响应 DTO + `SseEventMsg` 事件枚举 |
//! | [`tool_display`] | 工具名前端展示 + MCP 命名空间 / ARTIFACT 标记处理 |
//! | [`child_agent`] | 子 agent 活动事件 → SSE 桥接 |
//! | [`attachment`] | 多模态附件（图片降采样 + Content 构建） |
//! | [`error`] | 错误流构造（早期 / 执行期） |
//! | [`screenshot`] | 截图兜底落盘 |
//! | [`stream`] | 事件流核心（`create_event_stream` + `EventSink` 状态机） |
//! | 本文件 | handler 入口（`handle_run_sse` / `cancel`）+ agent 构建编排 |
//!
//! ## SSE 事件类型
//!
//! | 事件 | 说明 |
//! |------|------|
//! | `RUN_STARTED` | 任务开始 |
//! | `TEXT_MESSAGE_START/CONTENT/END` | 文本消息（流式分片） |
//! | `THINKING_MESSAGE_START/CONTENT/END` | 模型思考过程（流式分片） |
//! | `TOOL_CALL_START/ARGS/END` | 工具调用 |
//! | `TOOL_CALL_RESULT` | 工具返回结果 |
//! | `TOOL_CONFIRMATION` | 需要用户确认工具调用 |
//! | `RUN_FINISHED` | 任务完成 |
//! | `RUN_ERROR` | 任务出错 |

use axum::{
    Json,
    extract::State,
    response::{
        IntoResponse,
        sse::{Event as SseEvent, Sse},
    },
};
use serde_json::{Value, json};
use std::convert::Infallible;
use std::sync::Arc;

use super::AppState;
use super::auth::OptionalAuthUser;
use super::response;
use super::response::code;

mod attachment;
mod child_agent;
mod error;
mod screenshot;
mod stream;
mod tool_display;
mod types;

// 对外稳定路径：子模块拆分后，外部 `crate::server::sse::<Item>` 引用保持不变
pub use self::tool_display::tool_display_name;
pub use self::types::SseEventMsg;

use self::child_agent::SseChildEventSink;
use self::error::early_error_response;
use self::stream::create_event_stream;
use self::types::{InputMessage, RunRequest};

// ========================================================================
//  Handler
// ========================================================================

/// 取消正在运行的任务 — 通过 CancellationToken 终止指定会话的 Agent 执行
pub async fn cancel(state: &AppState, thread_id: &str) -> Value {
    // 先从全局表取出 token 并释放锁，避免持锁期间 await registry 造成阻塞
    let token = {
        let mut tokens = state.cancellation_tokens.lock().await;
        tokens.remove(thread_id)
    };
    if let Some(token) = token {
        token.cancel();
        // 双保险：清理该 session 的 pending shell 审批（oneshot sender drop → receiver 返 Err），
        // 让卡在 request_approval 的 select! 立即解锁（即使 cancel_token 竞争失败也能退出）。
        let cleared = state.shell_approval_registry.cancel_session(thread_id).await;
        tracing::info!("[取消] 已取消 session={}（清理 {} 个待审批）", thread_id, cleared);
        response::ok(json!({ "cancelled": true }))
    } else {
        response::err(code::NOT_FOUND, "没有正在运行的任务")
    }
}

fn truncate_str(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect::<String>() + "..."
    }
}

/// 解析本次 run 使用的模型（四级优先级：request > session > assistant > DB 默认）。
///
/// 失败时返回错误流 Response（调用方直接 `return`）；成功返回解析后的模型配置。
async fn resolve_run_model(
    state: &AppState,
    thread_id: &str,
    request_model_id: Option<&str>,
    assistant_model_id: Option<&str>,
) -> Result<crate::model_provider::ResolvedLlmConfig, axum::response::Response> {
    // 会话级模型（UI 切换模型时存入 session_settings.model_id，按 thread_id 查）。
    // 此前只在 assistant 加载前查 request 级，导致 session/assistant 级完全失效；
    // UI 切换模型后仍走全局默认。
    let session_model_id: Option<String> = if !thread_id.is_empty() {
        match &state.session_settings_store {
            Some(store) => match store.get_model(thread_id).await {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!("[RunSSE] 查询会话级模型失败（忽略，继续降级）: {}", e);
                    None
                }
            },
            None => None,
        }
    } else {
        None
    };

    let effective_model_id = request_model_id
        .or(session_model_id.as_deref())
        .or(assistant_model_id);

    tracing::info!(
        "[RunSSE] 模型解析 effective_id={:?}（来源：request={:?}, session={:?}, assistant={:?}）",
        effective_model_id,
        request_model_id,
        session_model_id.as_deref(),
        assistant_model_id
    );

    // 模型解析：DB 供应商存储为唯一数据源；未初始化或无可用模型时直接报错
    let resolve_result = state
        .model_provider_store
        .as_ref()
        .filter(|s| s.has_models())
        .ok_or_else(|| {
            anyhow::anyhow!("模型供应商存储未初始化或无可用模型，请在模型供应商管理中配置模型")
        })
        .and_then(|store| store.resolve_model(effective_model_id));

    match resolve_result {
        Ok(model) => {
            tracing::info!(
                "[RunSSE] 使用模型 id={} name={} model={}",
                model.id,
                model.name,
                model.model
            );
            Ok(model)
        }
        Err(e) => {
            let ev = SseEventMsg::RunError {
                message: e.to_string(),
            };
            let stream = futures::stream::once(async move {
                Ok::<_, Infallible>(SseEvent::default().data(ev.to_sse_data()))
            });
            Err(Sse::new(stream)
                .keep_alive(
                    axum::response::sse::KeepAlive::new()
                        .interval(std::time::Duration::from_secs(5)),
                )
                .into_response())
        }
    }
}

/// 装配提交给 [`crate::agent::build_agent_for_session`] 的 [`crate::agent::AgentRequest`]：
/// MCP 工具集、工作区模式（沙箱/聊天）、shell 工具依赖、skill 提及注入、跨会话记忆、
/// 会话级思考级别。`user_text` 会就地追加 @mention 渲染的上下文 XML（仅沙箱模式）。
#[allow(clippy::too_many_arguments)]
async fn build_agent_request(
    state: &AppState,
    assistant: &crate::domain::assistant::Assistant,
    thread_id: &str,
    user_id: &str,
    is_admin: bool,
    user_text: &mut String,
    messages: &[InputMessage],
    resolved_model_id: String,
    cancel_token: tokio_util::sync::CancellationToken,
    sse_tx: &tokio::sync::mpsc::Sender<SseEvent>,
) -> crate::agent::AgentRequest {
    use crate::agent::runtime::workspace::WorkspaceMode;

    // 预构建 MCP 工具集（从 assistant.enabled_mcps 解析 + 连接池获取）
    let mcp_toolsets = if let Some(mgr) = state.mcp_manager.as_ref() {
        if assistant.enabled_mcps.is_empty() {
            Vec::new()
        } else {
            mgr.build_toolsets(&assistant.enabled_mcps).await
        }
    } else {
        Vec::new()
    };

    // 解析工作区模式（T0 聊天档 / T1 沙箱档）。
    // - custom agent + enabled_tools 含 "shell_command"
    //   → 在 {data_dir}/sessions/{session_id}/ 惰性创建沙箱目录 → T1 Sandbox
    // - 其他 → T0 ChatOnly。沙箱目录与 Git 工作区物理隔离，会话删除时由 delete_session 清理。
    let needs_sandbox = assistant.enabled_tools.iter().any(|t| t == "shell_command");
    let workspace_mode: WorkspaceMode = if needs_sandbox {
        let sandbox_dir = state.config.workspace_session_dir(thread_id);
        match tokio::fs::create_dir_all(&sandbox_dir).await {
            Ok(_) => {
                tracing::info!(
                    "[sse] session {} 沙箱目录已就绪: {}",
                    thread_id,
                    sandbox_dir.display()
                );
                // 会话亲和容灾:本地目录为空(节点切换后新建)时,从对象存储拉最新快照恢复
                if let Some(os) = &state.object_store {
                    match crate::infra::workspace_snapshot::restore(os, thread_id, &sandbox_dir)
                        .await
                    {
                        Ok(true) => tracing::info!("[sse] session {} 已从快照恢复沙箱", thread_id),
                        Ok(false) => {}
                        Err(e) => tracing::warn!(
                            "[sse] session {} 恢复沙箱快照失败(可忽略,从空工作区开始): {e}",
                            thread_id
                        ),
                    }
                }
                // @mention 上下文注入：把用户 @ 引用的沙箱内文件渲染为 XML 块追加到 user_text
                if let Some(last_user_msg) = messages.iter().rev().find(|m| m.role == "user") {
                    if !last_user_msg.mentions.is_empty() {
                        let mention_xml = crate::tools::code::render_mentions(
                            &last_user_msg.mentions,
                            &sandbox_dir,
                        );
                        if !mention_xml.is_empty() {
                            user_text.push_str("\n\n");
                            user_text.push_str(&mention_xml);
                        }
                    }
                }
                WorkspaceMode::Sandbox(sandbox_dir)
            }
            Err(e) => {
                tracing::warn!("[sse] 创建 session 沙箱目录失败: {e}");
                WorkspaceMode::ChatOnly
            }
        }
    } else {
        WorkspaceMode::ChatOnly
    };

    // 会话级审批策略（沙箱模式 + 审批策略）：会话级覆盖优先，未设置/读取失败 → 全局 [shell] 默认。
    // network_access 始终跟全局 config（会话级表不存网络开关）。
    let session_policy = if !thread_id.is_empty() {
        let mut p = state.config.shell.permission_policy();
        if let Some(store) = &state.session_settings_store {
            if let Ok(Some((sm, ap))) = store.get_permission_policy(thread_id).await {
                p.sandbox_mode = sm;
                p.approval_policy = ap;
            }
        }
        // 执行入口 fail-closed 强制：完全访问仅管理员可用。update 接口已拦非管理员设置，
        // 但 DB 可能被管理员设过/手工写脏——非管理员会话一旦读到 danger-full-access，
        // 降级为 workspace-write（安全方向，不放行特权）。
        if !is_admin && matches!(p.sandbox_mode, crate::domain::permissions::SandboxMode::DangerFullAccess) {
            tracing::warn!(
                "[sse] 非管理员会话 {} 请求完全访问，强制降级为 workspace-write",
                thread_id
            );
            p.sandbox_mode = crate::domain::permissions::SandboxMode::WorkspaceWrite;
        }
        p
    } else {
        state.config.shell.permission_policy()
    };

    // 构建 shell_command 工具依赖（当有沙箱目录时）
    let shell_deps = match workspace_mode.root_path() {
        Some(sandbox_dir) => {
            // 会话级 shell 环境快照：一次性捕获用户交互式 shell 的 PATH/venv（VIRTUAL_ENV 等），
            // 供沙箱内每条命令 source。节点本地文件（不进 workspace tar），加入 readonly_extra 让沙箱只读可见。
            // 构建失败→None，优雅降级（命令按原有白名单环境执行）。
            let shell_snapshot = crate::infra::shell_snapshot::build(
                std::path::Path::new(&state.config.data_dir),
                thread_id,
            )
            .await;
            let mut readonly_extra = vec![state.config.skill_dir()];
            if let Some(p) = &shell_snapshot {
                readonly_extra.push(p.clone());
            }
            Some(std::sync::Arc::new(crate::tools::shell_command::ShellToolDeps {
                sandbox_dir: std::sync::Arc::new(sandbox_dir.to_path_buf()),
                max_timeout_ms: state.config.shell.max_timeout_ms,
                approval_timeout_secs: state.config.shell.approval_timeout_secs,
                approval_registry: state.shell_approval_registry.clone(),
                sse_tx: sse_tx.clone(),
                session_id: thread_id.to_string(),
                rule_store: state.shell_rule_store.clone(),
                cmd_history: std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new())),
                policy: session_policy,
                // 沙箱内额外只读可见:skill 目录 + shell 快照(截图已上对象存储,不再挂本地截图目录)
                readonly_extra,
                shell_snapshot,
                skill_dir: Some(state.config.skill_dir()),
                cancel_token: cancel_token.clone(),
            }))
        }
        None => None,
    };

    // 解析用户消息中的 $skill 提及,渲染正文块。
    // 正文以 user-role preamble 注入(经 AgentRequest.skill_bodies → build_custom_agent →
    // CortexAgent build_preamble 第三条 user 消息),不 push 进 user_text —— user_text 会被
    // adk-runner 作为 user 事件持久化、前端 fetchHistory 原样回显,拼进去会污染用户消息气泡
    // (此前 bug 的根因)。对齐 codex:skill body 以 user-role 注入,不持久化、不污染气泡,
    // 且不进 system stable 缓存前缀(避免击穿 prompt cache)。
    let skill_mention_bodies = if let Some(svc) = state.skill_service.as_ref() {
        let blocks = svc.resolve_mentions(user_text.as_str(), state.config.skill.max_inject_chars);
        if !blocks.is_empty() {
            tracing::info!(
                "[sse] skill 提及注入: {} 个正文块(走 user-role preamble)",
                blocks.len()
            );
            Some(blocks.join("\n\n"))
        } else {
            None
        }
    } else {
        None
    };

    // 会话级思考级别（未设置或读取失败 → 默认 high）
    let session_thinking_level: String = if !thread_id.is_empty() {
        match &state.session_settings_store {
            Some(store) => match store.get_thinking_level(thread_id).await {
                Ok(Some(lvl)) => lvl,
                _ => "high".to_string(),
            },
            None => "high".to_string(),
        }
    } else {
        "high".to_string()
    };

    // 跨会话记忆：按 user_id + 当前助手拉取（scope=0 全部 + scope=1 命中），渲染成 stable prefix 注入块。
    // 失败不阻断对话（仅 warn + 本次不注入）。
    let memory_block = match &state.memory_store {
        Some(store) => match store.list_for_inject(user_id, &assistant.id).await {
            Ok(list) if !list.is_empty() => {
                tracing::info!("[sse] 注入记忆 {} 条（user={}）", list.len(), user_id);
                Some(crate::domain::memory::render_inject_block(&list))
            }
            Ok(_) => None,
            Err(e) => {
                tracing::warn!("[sse] 拉取记忆失败({}),本次不注入记忆", e);
                None
            }
        },
        None => None,
    };

    crate::agent::AgentRequest {
        model_id: Some(resolved_model_id),
        workspace_mode,
        mcp_toolsets,
        shell_deps,
        skill_bodies: skill_mention_bodies,
        memory_block,
        session_thinking_level: Some(session_thinking_level),
        policy: session_policy,
        cancel_token,
        child_event_sink: Some(std::sync::Arc::new(SseChildEventSink::new(sse_tx.clone()))),
    }
}

/// SSE 流式对话主入口
///
/// 编排流程：加载助手 → 解析模型（[`resolve_run_model`]）→ 装配 Agent 请求
/// （[`build_agent_request`]）→ 构建 Agent → 起事件流（[`create_event_stream`]）。
/// 任一前置步骤失败时返回一个只含 `RUN_ERROR` 的错误流。
pub async fn handle_run_sse(
    State(state): State<Arc<AppState>>,
    OptionalAuthUser(opt_user): OptionalAuthUser,
    Json(input): Json<RunRequest>,
) -> impl IntoResponse {
    // 当前登录用户（记忆按真实用户隔离）；auth 未启用 / 未登录时回退 "user"
    // is_admin 单独留存：执行入口对「完全访问」fail-closed 强制（防 DB 被写脏后绕过 update 接口）。
    let is_admin = opt_user.as_ref().is_some_and(|u| u.is_admin);
    let user_id = opt_user
        .map(|u| u.user_id)
        .unwrap_or_else(|| "user".to_string());
    let thread_id = input.thread_id.clone();
    // 有效用户：管理员进入他人会话发消息时，用**归属者**的 user_id 跑 ADK run——
    // 消息写回归属者名下会话（不改归属），记忆/历史按归属者隔离（不串管理员自己的记忆）。
    // 归属查不到（新会话尚未落 session_settings）→ 回退调用者自身（自己新建的会话）。
    let user_id = super::session::resolve_effective_user(&state, &user_id, is_admin, &thread_id)
        .await
        .unwrap_or(user_id);
    let run_id = input
        .run_id
        .clone()
        .unwrap_or_else(|| uuid::Uuid::now_v7().to_string());
    let messages = input.messages.clone();
    let tool_decisions = input.tool_decisions.clone();
    let request_model_id = input
        .model_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let mut user_text: String = messages
        .iter()
        .rfind(|m| m.role == "user")
        .map(|m| m.content.clone())
        .unwrap_or_default();

    tracing::info!(
        "[RunSSE] 收到请求 assistant_id={}, messages={}",
        input.assistant_id,
        messages.len()
    );

    // 加载助手（强制要求）；失败时统一走 early_error_response（RUN_ERROR + RUN_FINISHED）
    let assistant = match &state.assistant_store {
        Some(store) => match store.get(&input.assistant_id).await {
            Ok(Some(a)) => a,
            Ok(None) => {
                return early_error_response(
                    &thread_id,
                    &run_id,
                    format!(
                        "该助手已被删除或不存在（{}），请重新选择助手后重试",
                        input.assistant_id
                    ),
                );
            }
            Err(e) => {
                return early_error_response(&thread_id, &run_id, format!("加载助手失败: {}", e));
            }
        },
        None => {
            return early_error_response(&thread_id, &run_id, "助手存储不可用（数据库未启用）");
        }
    };

    // 持久化会话-助手绑定
    if !thread_id.is_empty() {
        if let Some(store) = &state.session_settings_store {
            if let Err(e) = store.set_assistant(&thread_id, Some(&assistant.id)).await {
                tracing::warn!("[RunSSE] 写回会话助手绑定失败: {}", e);
            }
        }
    }

    let agent_type = assistant.agent_type.dispatch_key().to_string();
    tracing::info!(
        "[RunSSE] 用户输入=\"{}\" → 使用助手 agent_type={}（assistant={}）",
        truncate_str(&user_text, 50),
        agent_type,
        assistant.id
    );

    // 模型解析（四级优先级：request > session > assistant > DB 默认）
    let assistant_model_id: Option<&str> = {
        let s = assistant.model_id.trim();
        if s.is_empty() { None } else { Some(s) }
    };
    let resolved_model =
        match resolve_run_model(&state, &thread_id, request_model_id, assistant_model_id).await {
            Ok(m) => m,
            Err(resp) => return resp,
        };

    // 提前创建 SSE channel（shell_command 工具需要 tx 来发审批事件）
    let (sse_tx, sse_rx) = tokio::sync::mpsc::channel::<SseEvent>(100);

    // 提前创建取消令牌并注册到全局表：cancel 接口按 thread_id 找到并 cancel；
    // 同一个 token 注入 ShellToolDeps / CortexAgent，让工具执行的 select! 监听它，
    // 用户点停止时立即解锁卡住的 agent（对齐 codex 的 CancellationToken 级联）。
    let cancel_token = tokio_util::sync::CancellationToken::new();
    {
        let mut tokens = state.cancellation_tokens.lock().await;
        tokens.insert(thread_id.clone(), cancel_token.clone());
    }

    // 装配 Agent 请求（工作区 / shell 依赖 / skill / 记忆 / 思考级别，含 @mention 注入）
    let agent_req = build_agent_request(
        &state,
        &assistant,
        &thread_id,
        &user_id,
        is_admin,
        &mut user_text,
        &messages,
        resolved_model.id.clone(),
        cancel_token.clone(),
        &sse_tx,
    )
    .await;

    // 构建 Agent（统一走 build_agent_for_session）
    let agent_ctx = crate::agent::AgentContext {
        cfg: &state.config,
        knowledge_manager: state.knowledge_manager.clone(),
        catalog: state.catalog.clone(),
        db_pool: state.db_pool.clone(),
        redis_pool: state.redis_pool.clone(),
        plugin_manager: Some(state.plugin_manager.clone()),
        skill_service: state.skill_service.clone(),
        memory_proposal_store: state.memory_proposal_store.clone(),
        model_provider_store: state.model_provider_store.clone(),
        object_store: state.object_store.clone(),
    };
    let (agent, budget_handle) = match crate::agent::build_agent_for_session(&agent_ctx, &assistant, agent_req).await
    {
        Ok((agent, budget)) => (agent, budget),
        Err(e) => {
            // agent 构建失败：清理已注册的 cancel_token，避免残留（create_event_stream 未执行，不会自动 remove）
            {
                let mut tokens = state.cancellation_tokens.lock().await;
                tokens.remove(&thread_id);
            }
            let ev = SseEventMsg::RunError {
                message: e.to_string(),
            };
            let stream = futures::stream::once(async move {
                Ok::<_, Infallible>(SseEvent::default().data(ev.to_sse_data()))
            });
            return Sse::new(stream)
                .keep_alive(
                    axum::response::sse::KeepAlive::new()
                        .interval(std::time::Duration::from_secs(5)),
                )
                .into_response();
        }
    };

    let event_stream = create_event_stream(
        state,
        agent,
        budget_handle,
        thread_id,
        run_id,
        user_id,
        messages,
        user_text,
        tool_decisions,
        Some(resolved_model.id),
        sse_tx,
        sse_rx,
        cancel_token,
    );

    Sse::new(event_stream)
        .keep_alive(
            axum::response::sse::KeepAlive::new().interval(std::time::Duration::from_secs(5)),
        )
        .into_response()
}
