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

pub(crate) mod attachment;
mod child_agent;
mod error;
mod screenshot;
mod stream;
pub(crate) mod tool_display;
pub(crate) mod types;
mod user_message;

// 对外稳定路径：子模块拆分后，外部 `crate::server::sse::<Item>` 引用保持不变
pub use self::tool_display::tool_display_name;
pub use self::types::SseEventMsg;

use self::child_agent::SseChildEventSink;
use self::error::early_error_response;
use self::stream::create_event_stream;
use self::types::{InputMessage, RunRequest};

// ========================================================================
//  ToolEventSink 实现 — SSE 层注入，斩断 tools → server 反向依赖
// ========================================================================

/// SSE 传输层的 `ToolEventSink` 实现，封装 `mpsc::Sender<SseEvent>`。
///
/// 事件序列化复用 `SseEventMsg::to_sse_data`，JSON 格式与改前逐字节一致。
struct SseToolEventSink {
    tx: tokio::sync::mpsc::Sender<axum::response::sse::Event>,
}

impl SseToolEventSink {
    fn new(tx: tokio::sync::mpsc::Sender<axum::response::sse::Event>) -> Self {
        Self { tx }
    }
}

#[async_trait::async_trait]
impl crate::tools::shell_command::events::ToolEventSink for SseToolEventSink {
    async fn send_file_artifact(
        &self,
        path: String,
        filename: String,
        title: String,
        mime: String,
        size: u64,
    ) {
        let ev = SseEventMsg::FileArtifact { path, filename, title, mime, size };
        let _ = self.tx.send(axum::response::sse::Event::default().data(ev.to_sse_data())).await;
    }

    async fn send_approval_request(
        &self,
        approval_id: String,
        command: String,
        session_id: String,
    ) {
        let ev = SseEventMsg::ShellApprovalRequest { approval_id, command, session_id };
        let _ = self.tx.send(axum::response::sse::Event::default().data(ev.to_sse_data())).await;
    }
}

// ========================================================================
//  Handler
// ========================================================================

/// 取消正在运行的任务 — 通过 CancellationToken 终止指定会话的 Agent 执行
pub async fn cancel(state: &AppState, user_id: &str, is_admin: bool, thread_id: &str) -> Value {
    // 归属校验：仅归属人/管理员可取消（防他人恶意中断任务）
    if let Err(v) = super::session::check_session_access(state, user_id, is_admin, thread_id).await
    {
        return v;
    }
    // 取消活跃 run + 清空未消费 steer 队列（对齐 codex interrupt 的 clear_pending：
    // 被打断的 turn 不复活运行中提交的排队消息）。token 在 registry 内部 cancel。
    match crate::infra::run_registry::cancel_active(&state.run_registry, thread_id).await {
        Some((run_id, _token, cleared_steer)) => {
            // 双保险：清理该 session 的 pending shell 审批（oneshot sender drop → receiver 返 Err），
            // 让卡在 request_approval 的 select! 立即解锁（即使 cancel_token 竞争失败也能退出）。
            let cleared = state
                .shell_approval_registry
                .cancel_session(thread_id)
                .await;
            tracing::info!(
                "[取消] 已取消 session={} run={}（清理 {} 个待审批 / {} 条排队输入）",
                thread_id,
                run_id,
                cleared,
                cleared_steer
            );
            response::ok(json!({ "cancelled": true, "run_id": run_id }))
        }
        None => response::err(code::NOT_FOUND, "没有正在运行的任务"),
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
/// **按归属人 `user_id` 隔离**：仅从该用户的模型桶解析（含 API Key），杜绝跨用户串用。
/// 失败时返回错误流 Response（调用方直接 `return`）；成功返回解析后的模型配置。
pub(crate) async fn resolve_run_model(
    state: &AppState,
    thread_id: &str,
    request_model_id: Option<&str>,
    assistant_model_id: Option<&str>,
    user_id: &str,
) -> Result<crate::domain::model_provider::ResolvedLlmConfig, axum::response::Response> {
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

    // 模型解析：DB 供应商存储为唯一数据源；未初始化或该用户无可用模型时直接报错
    let resolve_result = state
        .model_provider_store
        .as_ref()
        .filter(|s| s.has_models(user_id))
        .ok_or_else(|| {
            anyhow::anyhow!("模型供应商存储未初始化或无可用模型，请在模型供应商管理中配置模型")
        })
        .and_then(|store| store.resolve_model(effective_model_id, user_id));

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

/// 计算 shell 沙箱/只读工具对 skill 目录的可见范围（第四出口收口）。
///
/// - 白名单为空（不限制）→ 整个 skill 根目录（原特性：模型可直读任意 skill 脚本）。
/// - 白名单非空（硬隔离）→ 仅白名单内 skill 的子目录。skill 有两层物理位置
///   （`<root>/<name>` 用户层 + `<root>/.builtin/<name>` 内置层，同名 User 覆盖
///   Builtin），两处都挂（各自存在与否由文件系统天然决定，目录不存在=空根无害）。
///   白名单里已被删除的 skill 名同样自然跳过（目录不存在）。
///
/// 未启用 skill 系统时返回空（无只读根）。
fn skill_readonly_roots(
    state: &AppState,
    enabled_skills: &[String],
) -> Vec<std::path::PathBuf> {
    let skill_dir = state.config.skill_dir();
    if enabled_skills.is_empty() {
        return vec![skill_dir];
    }
    let mut roots = Vec::with_capacity(enabled_skills.len() * 2);
    for name in enabled_skills {
        roots.push(skill_dir.join(name));
        roots.push(skill_dir.join(".builtin").join(name));
    }
    roots
}

/// 计算沙箱内需要 **mask**（内容隐藏）的 skill 子目录：白名单非空时，skill 根下
/// 除白名单成员与 `.builtin` 外的全部子目录，加上 `.builtin` 下非白名单的内置 skill。
/// 这是第五出口（整盘只读）的收口——bwrap `--ro-bind / /` 下 readonly_extra 收窄是
/// no-op，`cat <skill_dir>/被隐藏/SKILL.md` 仍可读全文，只有 mask（空 tmpfs 覆盖）
/// 能挡。白名单为空返回空（不 mask）。
fn skill_masked_paths(state: &AppState, enabled_skills: &[String]) -> Vec<std::path::PathBuf> {
    let skill_dir = state.config.skill_dir();
    if enabled_skills.is_empty() {
        return Vec::new();
    }
    let allowed: std::collections::HashSet<&str> =
        enabled_skills.iter().map(String::as_str).collect();
    let mut masked = Vec::new();
    // 用户层：skill 根的直接子目录（.builtin 除外），不在白名单 → mask。
    if let Ok(entries) = std::fs::read_dir(&skill_dir) {
        for e in entries.flatten() {
            let name = e.file_name();
            let name = name.to_string_lossy();
            if name == ".builtin" || name.starts_with('.') || !e.path().is_dir() {
                continue;
            }
            if !allowed.contains(name.as_ref()) {
                masked.push(e.path());
            }
        }
    }
    // 内置层：.builtin/ 下非白名单的 skill 目录 → mask。
    let builtin_dir = skill_dir.join(".builtin");
    if let Ok(entries) = std::fs::read_dir(&builtin_dir) {
        for e in entries.flatten() {
            let name = e.file_name();
            let name = name.to_string_lossy();
            if !e.path().is_dir() {
                continue;
            }
            if !allowed.contains(name.as_ref()) {
                masked.push(e.path());
            }
        }
    }
    masked
}

/// 装配提交给 [`crate::agent::build_agent_for_session`] 的 [`crate::agent::AgentRequest`]：
/// MCP 工具集、工作区模式（沙箱/聊天）、shell 工具依赖、skill 提及注入、跨会话记忆、
/// 会话级思考级别。`user_text` 会就地追加 @mention 渲染的上下文 XML（仅沙箱模式）。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn build_agent_request(
    state: &AppState,
    assistant: &crate::domain::assistant::Assistant,
    thread_id: &str,
    user_id: &str,
    is_admin: bool,
    user_text: &mut String,
    messages: &[InputMessage],
    resolved_model_id: String,
    cancel_token: tokio_util::sync::CancellationToken,
    steer_port: Option<std::sync::Arc<crate::infra::run_registry::SteerPort>>,
    sse_tx: &tokio::sync::mpsc::Sender<SseEvent>,
) -> crate::agent::AgentRequest {
    use crate::agent::workspace::WorkspaceMode;

    // 预构建 MCP 工具集（从 assistant.enabled_mcps 解析 + 连接池获取）
    let mcp_toolsets = if let Some(mgr) = state.mcp_manager.as_ref() {
        if assistant.enabled_mcps.is_empty() {
            Vec::new()
        } else {
            mgr.build_toolsets(&assistant.enabled_mcps, user_id, is_admin)
                .await
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
                    match crate::infra::sandbox::workspace_snapshot::restore(os, thread_id, &sandbox_dir)
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
        if !is_admin
            && matches!(
                p.sandbox_mode,
                crate::permissions::SandboxMode::DangerFullAccess
            )
        {
            tracing::warn!(
                "[sse] 非管理员会话 {} 请求完全访问，强制降级为 workspace-write",
                thread_id
            );
            p.sandbox_mode = crate::permissions::SandboxMode::WorkspaceWrite;
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
            let shell_snapshot = crate::infra::sandbox::shell_snapshot::build(
                std::path::Path::new(&state.config.data_dir),
                thread_id,
            )
            .await;
            // 助手级 skill 白名单硬隔离（第四出口收口）：白名单为空 → 整个 skill 根目录
            // 只读可见（模型可直读 skill 脚本/文档，原特性）；非空 → 只挂白名单内 skill
            // 的子目录——否则 read_file/glob/grep/shell 可枚举并直读被隐藏 skill 的
            // SKILL.md 全文，三出口（catalog/read_skill/$mention）的硬隔离即被绕过。
            let mut readonly_extra: Vec<std::path::PathBuf> =
                skill_readonly_roots(state, &assistant.enabled_skills);
            if let Some(p) = &shell_snapshot {
                readonly_extra.push(p.clone());
            }
            Some(std::sync::Arc::new(
                crate::tools::shell_command::ShellToolDeps {
                    sandbox_dir: std::sync::Arc::new(sandbox_dir.to_path_buf()),
                    max_timeout_ms: state.config.shell.max_timeout_ms,
                    approval_timeout_secs: state.config.shell.approval_timeout_secs,
                    approval_registry: state.shell_approval_registry.clone(),
                    event_sink: std::sync::Arc::new(SseToolEventSink::new(sse_tx.clone())),
                    session_id: thread_id.to_string(),
                    // artifact 事件持久化（刷新后恢复文件卡片）；AppState 恒有该服务
                    session_service: Some(state.adk_session_service.clone()),
                    rule_store: state.shell_rule_store.clone(),
                    cmd_history: std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new())),
                    policy: session_policy,
                    // 沙箱内额外只读可见:skill 目录(按白名单收窄) + shell 快照(截图已上对象存储,不再挂本地截图目录)
                    readonly_extra,
                    // skill 白名单外的子目录 mask 掉（整盘只读下唯一有效的隐藏手段）
                    masked_paths: skill_masked_paths(state, &assistant.enabled_skills),
                    shell_snapshot,
                    // 写入检测用的 skill 根保持完整（防止向任何 skill 目录写），
                    // 只读可见性由上方 readonly_extra 控制。
                    skill_dir: Some(state.config.skill_dir()),
                    // 助手级环境变量：注入子进程环境，供 skill 脚本等经 os.environ['KEY'] 读取。
                    // 注入前剥离劫持类变量（LD_PRELOAD/PYTHONPATH/NODE_OPTIONS…），保沙箱隔离边界。
                    extra_env: crate::tools::shell_command::sanitize_extra_env(
                        assistant
                            .env_vars
                            .iter()
                            .map(|(k, v)| (k.clone(), v.clone()))
                            .collect(),
                    ),
                    cancel_token: cancel_token.clone(),
                    recent_artifacts: std::sync::Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
                },
            ))
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
        // 助手级白名单硬隔离：空 = 全部可见(传 None)；非空 = 仅列出的可被 $mention 注入
        let allowed: Option<&[String]> = if assistant.enabled_skills.is_empty() {
            None
        } else {
            Some(assistant.enabled_skills.as_slice())
        };
        let blocks = svc.resolve_mentions_filtered(
            user_text.as_str(),
            state.config.skill.max_inject_chars,
            allowed,
        );
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

    // 会话级软着陆窗口状态句柄（remind/borrow flag 跨 run 存活；空 thread_id 不注入）。
    // entry().or_default()：Arc<Mutex<WindowStateSnapshot>> 首次访问时建空窗（窗口 1）。
    let session_window = if thread_id.is_empty() {
        None
    } else {
        let mut map = state.session_window_state.lock().await;
        Some(
            map.entry(thread_id.to_string())
                .or_default()
                .clone(),
        )
    };

    crate::agent::AgentRequest {
        model_id: Some(resolved_model_id),
        user_id: user_id.to_string(),
        workspace_mode,
        mcp_toolsets,
        shell_deps,
        skill_bodies: skill_mention_bodies,
        memory_block,
        session_thinking_level: Some(session_thinking_level),
        policy: session_policy,
        cancel_token,
        child_event_sink: Some(std::sync::Arc::new(SseChildEventSink::new(sse_tx.clone()))),
        async_message_sink: Some(std::sync::Arc::new(user_message::SseAsyncMessageSink::new(
            sse_tx.clone(),
        ))),
        steer_port,
        session_window,
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
    let caller_user_id = opt_user
        .map(|u| u.user_id)
        .unwrap_or_else(|| "user".to_string());
    let thread_id = input.thread_id.clone();
    let run_id = input
        .run_id
        .clone()
        .unwrap_or_else(|| uuid::Uuid::now_v7().to_string());
    // 归属校验：会话已存在（有 session_settings 行）则须归属人/管理员；新会话（无行）放行。
    // session 级资源（settings/沙箱目录/工作区快照）按 session_id 裸操作，须在此前置拦截，
    // 否则跨用户可读写他人沙箱文件 / 篡改 session_settings / 覆盖工作区快照。
    if let Some(ss) = &state.session_settings_store {
        match ss.get_owner(&thread_id).await {
            Ok(Some(owner)) if is_admin || owner == caller_user_id => {}
            Ok(Some(_)) => {
                return early_error_response(
                    &thread_id,
                    &run_id,
                    "会话不存在或无权访问".to_string(),
                );
            }
            Ok(None) => {} // 新会话：无既有资源可窃取，放行（调用者即将新建）
            Err(e) => tracing::warn!("[RunSSE] 会话归属校验查询失败（降级放行）: {e}"),
        }
        // 定时任务会话（source_type=1）拒绝交互 run：① 其 session_settings 持久化
        // approval_policy=auto（无人值守写入），复用该会话交互触发 = 绕过审批让命令
        // 静默自动执行（等同用户自助提权）；② 交互消息会污染任务回放数据。
        // 前端只读回放是纯 UI 兜底，此处为服务端硬拦截。get_source_info 失败时放行
        // （fail-open 仅影响此附加拦截，归属校验已在上方完成）。
        if let Ok(Some((source_type, _, _))) = ss.get_source_info(&thread_id).await {
            if source_type == 1 {
                return early_error_response(
                    &thread_id,
                    &run_id,
                    "定时任务会话为只读回放，不可发送消息".to_string(),
                );
            }
        }
    }
    // 有效用户：管理员进入他人会话发消息时，用**归属者**的 user_id 跑 ADK run——
    // 消息写回归属者名下会话（不改归属），记忆/历史按归属者隔离（不串管理员自己的记忆）。
    // 归属查不到（新会话尚未落 session_settings）→ 回退调用者自身（自己新建的会话）。
    let user_id =
        super::session::resolve_effective_user(&state, &caller_user_id, is_admin, &thread_id)
            .await
            .unwrap_or_else(|| caller_user_id.clone());
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
            Ok(Some(a)) => {
                // 可见性校验：私有 custom 助手仅归属人/管理员可用（防越权读取他人助手的
                // system_prompt / env_vars 明文 / 绑定的知识库）。对齐 GraphQL `assistant` query 的 can_read。
                if !crate::server::assistant::can_read(&a, &user_id, is_admin) {
                    return early_error_response(
                        &thread_id,
                        &run_id,
                        "该助手已被删除或不存在".to_string(),
                    );
                }
                a
            }
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
    let resolved_model = match resolve_run_model(
        &state,
        &thread_id,
        request_model_id,
        assistant_model_id,
        &user_id,
    )
    .await
    {
        Ok(m) => m,
        Err(resp) => return resp,
    };

    // 提前创建 SSE channel（shell_command 工具需要 tx 来发审批事件）
    let (sse_tx, sse_rx) = tokio::sync::mpsc::channel::<SseEvent>(100);

    // 提前创建取消令牌并登记到会话运行注册表（对齐 codex 单会话单活跃 turn）：
    // - 忙拒绝：已有活跃 run → 本请求被拒（此前第二个请求直接覆盖 token，旧 run 从此
    //   无法停止、两个 runner 并发写同一会话）；运行中提交新消息应走 steer 接口；
    // - cancel 接口按 thread_id 从注册表找到 token 并 cancel；
    // - 同一个 token 注入 ShellToolDeps / CortexAgent，让工具执行的 select! 监听它，
    //   用户点停止时立即解锁卡住的 agent（对齐 codex 的 CancellationToken 级联）。
    let cancel_token = tokio_util::sync::CancellationToken::new();
    if let Err(existing_run) = crate::infra::run_registry::register_active(
        &state.run_registry,
        &thread_id,
        &run_id,
        cancel_token.clone(),
    )
    .await
    {
        tracing::warn!(
            "[RunSSE] 会话 {} 已有活跃 run {}，拒绝并发启动",
            thread_id,
            existing_run
        );
        return early_error_response(
            &thread_id,
            &run_id,
            "会话已有正在运行的任务，请等待完成、点停止，或在新消息输入框继续发送（将自动追加到当前任务）".to_string(),
        );
    }
    // steer 消费句柄：CortexAgent 主循环经它在下轮模型请求前注入运行中提交的用户消息
    let steer_port = std::sync::Arc::new(crate::infra::run_registry::SteerPort::new(
        state.run_registry.session(&thread_id).await,
        &run_id,
    ));

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
        Some(steer_port),
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
        // 顶层会话注入 app_state（manage_scheduled_task 工具可用）
        app_state: Some(state.clone()),
    };
    let (agent, budget_handle, child_usage) =
        match crate::agent::build_agent_for_session(&agent_ctx, &assistant, agent_req).await {
            Ok((agent, budget, child_usage)) => (agent, budget, child_usage),
            Err(e) => {
                // agent 构建失败：注销已登记的活跃 run，避免残留（create_event_stream 未执行，
                // 不会走它的注销路径）
                crate::infra::run_registry::deregister_active(
                    &state.run_registry,
                    &thread_id,
                    &run_id,
                )
                .await;
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
        child_usage,
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

/// 运行中追加输入（steer）— 对齐 codex `TurnInputMode::StartOrSteer` 的 steer 分支。
///
/// 会话有活跃 run 时把用户消息构建成完整 user Content（@mention XML + 附件降采样，
/// 与 `/api/run_sse` 同一套 [`build_user_content`]）入队，由 CortexAgent 主循环在
/// **下一次模型请求前** drain 注入（对齐 codex pending_input 消费点）；模型回合结束
/// 时队列非空则续跑。无活跃 run → 返回 `steered:false`，前端回退走正常发送路径
/// （对齐 codex `NotSubmitted::NoActiveTurn` → start 分支，由调用方决策）。
///
/// 运行中提交不做会话归属外的校验（assistant/模型沿用当前 run 的），立即 ack —— 对齐
/// codex `Op::TurnInput` 的 oneshot 快速回复（不等 turn 真正消费输入）。
pub async fn steer(
    state: &AppState,
    user_id: &str,
    is_admin: bool,
    thread_id: &str,
    messages: Value,
    run_id: Option<String>,
) -> Value {
    // 归属校验：与 handle_run_sse 同款（已存在会话须归属人/管理员）
    if let Some(ss) = &state.session_settings_store {
        match ss.get_owner(thread_id).await {
            Ok(Some(owner)) if is_admin || owner == user_id => {}
            Ok(Some(_)) => {
                return response::err(code::NOT_FOUND, "会话不存在或无权访问");
            }
            Ok(None) => {
                return response::err(code::INVALID_PARAMS, "会话不存在，无法追加消息");
            }
            Err(e) => {
                return response::err(response::code::DATABASE, format!("查询失败: {e}"));
            }
        }
        // 定时任务会话拒绝 steer（与 handle_run_sse 的硬拦截一致）：定时 run 无人值守，
        // 注入消息既污染回放、也借 auto 审批策略借道执行指令。
        if let Ok(Some((source_type, _, _))) = ss.get_source_info(thread_id).await {
            if source_type == 1 {
                return response::err(code::BUSINESS, "定时任务会话为只读回放，不可追加消息");
            }
        }
    }
    // 解析消息列表（结构同 RunRequest.messages）
    let msgs: Vec<InputMessage> = match serde_json::from_value(messages) {
        Ok(v) => v,
        Err(e) => {
            return response::err(code::INVALID_PARAMS, format!("messages 格式错误: {e}"));
        }
    };
    // 有效用户：管理员进入他人会话时按归属者隔离（与 handle_run_sse 一致，仅用于日志口径）
    let effective_user =
        super::session::resolve_effective_user(state, user_id, is_admin, thread_id)
            .await
            .unwrap_or_else(|| user_id.to_string());

    // 组装 user_text：正文 + @mention 上下文 XML（沙箱目录已存在才渲染；正常路径在
    // build_agent_request 里做，steer 时 run 已在跑，目录应已就绪）
    let mut user_text = msgs
        .iter()
        .rev()
        .find(|m| m.role == "user")
        .map(|m| m.content.clone())
        .unwrap_or_default();
    if user_text.trim().is_empty() && !msgs.iter().any(|m| !m.attachments.is_empty()) {
        return response::err(code::INVALID_PARAMS, "追加消息不能为空");
    }
    let sandbox_dir = state.config.workspace_session_dir(thread_id);
    if sandbox_dir.exists()
        && let Some(last_user_msg) = msgs.iter().rev().find(|m| m.role == "user")
        && !last_user_msg.mentions.is_empty()
    {
        let mention_xml =
            crate::tools::code::render_mentions(&last_user_msg.mentions, &sandbox_dir);
        if !mention_xml.is_empty() {
            user_text.push_str("\n\n");
            user_text.push_str(&mention_xml);
        }
    }
    // 附件解析（图片降采样 / 文档 markitdown），与主路径同一入口
    let content = attachment::build_user_content(&user_text, &msgs, state, thread_id).await;
    let item = crate::infra::run_registry::SteerItem {
        run_id: run_id.unwrap_or_default(),
        content,
    };
    let steered =
        crate::infra::run_registry::enqueue_steer(&state.run_registry, thread_id, item).await;
    if steered {
        tracing::info!(
            "[Steer] session={} 追加输入（user={} text={} chars）",
            thread_id,
            effective_user,
            user_text.len()
        );
        response::ok(json!({ "steered": true }))
    } else {
        // 无活跃 run：前端回退正常发送（对齐 codex NoActiveTurn → start）
        tracing::info!("[Steer] session={} 无活跃 run，未入队（回退正常发送）", thread_id);
        response::ok(json!({ "steered": false }))
    }
}
