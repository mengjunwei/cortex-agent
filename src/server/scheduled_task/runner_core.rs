//! 定时任务核心执行链路（无 SSE 的后台 agent 运行）。
//!
//! 复用交互路径（`sse/mod.rs`）的公共件：[`resolve_run_model`] / [`build_agent_request`] /
//! `build_agent_for_session` / `build_user_content` / Runner 装配。事件汇采用「记录型 sink」——
//! 只累计 assistant 文本并落库（补偿 adk 持久化），不产生 SSE 帧。
//!
//! 调度器（`super::scheduler`）到点调用 [`run_scheduled_task`]。

use std::collections::HashMap;
use std::sync::Arc;

use tokio_stream::StreamExt;

use crate::agent::AgentContext;
use crate::domain::scheduled_task::{RunStatus, ScheduledTask};
use crate::server::AppState;
use crate::server::assistant::can_read;
use crate::server::sse::{build_agent_request, resolve_run_model};

/// 单次定时任务执行超时（30 分钟）。超时强杀并标记 `RunStatus::Timeout`。
const RUN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30 * 60);

/// 触发并执行一个定时任务（供调度闭包与 run-now 共用）。
///
/// 流程：查任务 → 助手可见性校验 → 建会话(source_type=1) → 跑 agent → 落库 → 记录结果 → 清理 30 天旧数据。
/// 任一失败路径都尽力记录 `last_run_status`，不静默。
///
/// `trigger_kind`：触发方式标识，写入会话配置供前端区分。取值 `cron`（到点定时）/
/// `catchup`（启动补偿补跑）/ `manual`（手动立即运行）。
pub async fn run_scheduled_task(state: Arc<AppState>, task_id: &str, trigger_kind: &str) {
    let Some(store) = state.scheduled_task_store.clone() else {
        tracing::warn!("[scheduled] 定时任务存储不可用，跳过 task_id={task_id}");
        return;
    };
    let task = match store.get(task_id).await {
        Ok(Some(t)) => t,
        Ok(None) => {
            tracing::warn!("[scheduled] 任务不存在（可能已删除），跳过 task_id={task_id}");
            return;
        }
        Err(e) => {
            tracing::error!("[scheduled] 加载任务失败 task_id={task_id}: {e}");
            return;
        }
    };
    if !task.enabled {
        tracing::info!("[scheduled] 任务已停用，跳过 task_id={task_id}");
        return;
    }

    match execute(&state, &task, trigger_kind).await {
        Ok(session_id) => {
            tracing::info!(
                "[scheduled] 任务运行成功 task_id={} session_id={}",
                task.id,
                session_id
            );
            let _ = store
                .record_run(&task.id, RunStatus::Success, Some(&session_id))
                .await;
            // 刷新下次触发时间（详情页展示）。
            refresh_next_run(state.clone(), &task).await;
            // 运行成功后顺手清理该任务 30 天前旧会话（含本次新会话之前的）。
            cleanup_old_runs(state.clone(), &task).await;
        }
        Err((status, failed_session)) => {
            tracing::warn!("[scheduled] 任务运行失败 task_id={} status={:?}", task.id, status);
            // 已建会话的失败（超时/中途出错）记录 session_id——失败会话真实存在且占资源，
            // 不记录则任务详情页无法定位排查（超时强杀的半成品会话尤其需要）。
            let _ = store.record_run(&task.id, status, failed_session.as_deref()).await;
            refresh_next_run(state.clone(), &task).await;
            // 助手不可见/被删 → 停用任务，避免每次到点都空跑报错。
            if status == RunStatus::Failed {
                if let Err(e) = store.set_enabled(&task.id, false).await {
                    tracing::warn!("[scheduled] 停用失败任务出错 task_id={}: {e}", task.id);
                }
            }
        }
    }
}

/// 刷新 next_run_at：用 chrono-tz 按 cron 重算下一次触发时间回填（详情页展示）。
async fn refresh_next_run(state: Arc<AppState>, task: &ScheduledTask) {
    let Some(store) = state.scheduled_task_store.clone() else {
        return;
    };
    match next_occurrence(&task.schedule_cron, &task.timezone) {
        Some(t) => {
            let _ = store.set_next_run(&task.id, Some(t)).await;
        }
        None => {
            tracing::warn!(
                "[scheduled] 计算下次触发时间失败 task_id={} cron={}",
                task.id,
                task.schedule_cron
            );
        }
    }
}

/// 归一化 cron 表达式为 `cron` crate 要求的 6 段（秒 分 时 日 月 周）。
/// 用户/模型常给标准 5 段（分 时 日 月 周），此处秒位补 0；已是 6/7 段则原样返回。
pub(crate) fn normalize_cron(cron: &str) -> String {
    let fields: Vec<&str> = cron.split_whitespace().collect();
    match fields.len() {
        5 => format!("0 {}", cron.trim()),
        _ => cron.trim().to_string(),
    }
}

/// 用 chrono-tz 解析 cron 并算下次触发时间（UTC）。供刷新 next_run_at 与校验用。
pub fn next_occurrence(cron: &str, timezone: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    use std::str::FromStr;
    let schedule = cron::Schedule::from_str(&normalize_cron(cron)).ok()?;
    let tz: chrono_tz::Tz = timezone.parse().unwrap_or(chrono_tz::Asia::Shanghai);
    schedule.upcoming(tz).next().map(|t| t.with_timezone(&chrono::Utc))
}

/// 计算未来 N 次触发时间（parse-schedule 预览用）。
pub fn preview_occurrences(
    cron: &str,
    timezone: &str,
    n: usize,
) -> Option<Vec<chrono::DateTime<chrono::Utc>>> {
    use std::str::FromStr;
    let schedule = cron::Schedule::from_str(&normalize_cron(cron)).ok()?;
    let tz: chrono_tz::Tz = timezone.parse().unwrap_or(chrono_tz::Asia::Shanghai);
    Some(
        schedule
            .upcoming(tz)
            .take(n)
            .map(|t| t.with_timezone(&chrono::Utc))
            .collect(),
    )
}

/// 执行核心：建会话 + 跑 agent + 落库。返回会话 id；失败返回 (状态, 已建的会话 id)——
/// 会话创建之后的失败（模型解析/构建/超时）会话已真实存在，带回供 record_run 落库定位。
async fn execute(
    state: &Arc<AppState>,
    task: &ScheduledTask,
    trigger_kind: &str,
) -> Result<String, (RunStatus, Option<String>)> {
    let user_id = &task.user_id;

    // 1. 加载助手 + 可见性校验（以创建者身份；不可见=助手被删/取消共享 → 失败并停用）
    let Some(assistant_store) = state.assistant_store.clone() else {
        tracing::error!("[scheduled] 助手存储不可用");
        return Err((RunStatus::Failed, None));
    };
    let assistant = match assistant_store.get(&task.assistant_id).await {
        Ok(Some(a)) => a,
        Ok(None) => {
            tracing::warn!("[scheduled] 助手不存在 task_id={} assistant_id={}", task.id, task.assistant_id);
            return Err((RunStatus::Failed, None));
        }
        Err(e) => {
            tracing::error!("[scheduled] 加载助手失败: {e}");
            return Err((RunStatus::Failed, None));
        }
    };
    if !can_read(&assistant, user_id, false) {
        tracing::warn!(
            "[scheduled] 创建者对助手不可见（已停用/取消共享），停用任务 task_id={}",
            task.id
        );
        return Err((RunStatus::Failed, None));
    }

    // 2. 建会话（source_type=1，标题 = 任务名 · 本地时间）
    let thread_id = uuid::Uuid::now_v7().to_string();
    let tz: chrono_tz::Tz = task.timezone.parse().unwrap_or(chrono_tz::Asia::Shanghai);
    let local_now = chrono::Utc::now().with_timezone(&tz);
    let title = format!("{} · {}", task.name, local_now.format("%Y-%m-%d %H:%M"));
    let agent_type = assistant.agent_type.dispatch_key();

    let create_req = adk_rust::session::CreateRequest {
        app_name: "cortex-agent".to_string(),
        user_id: user_id.to_string(),
        session_id: Some(thread_id.clone()),
        state: {
            let mut m = HashMap::new();
            m.insert(
                "agent_type".to_string(),
                serde_json::Value::String(agent_type.to_string()),
            );
            m.insert(
                "assistant_id".to_string(),
                serde_json::Value::String(task.assistant_id.clone()),
            );
            m
        },
    };
    if let Err(e) = state.adk_session_service.create(create_req).await {
        tracing::error!("[scheduled] 创建会话失败: {e}");
        return Err((RunStatus::Failed, None));
    }
    if let Some(ss) = &state.session_settings_store {
        if let Err(e) = ss
            .init_scheduled_session(
                &thread_id,
                user_id,
                &title,
                agent_type,
                Some(&task.assistant_id),
                &task.id,
                trigger_kind,
            )
            .await
        {
            tracing::warn!("[scheduled] 写会话配置失败: {e}");
        }
        // 无人值守：定时任务会话强制自动批准策略（仍受 dangerous 硬编码阻断 + 沙箱约束），
        // 否则 shell 等需审批命令会因无人响应而超时失败，任务跑不起来。
        if let Err(e) = ss
            .set_permission_policy(
                &thread_id,
                state.config.shell.permission_policy().sandbox_mode,
                crate::permissions::ApprovalPolicy::Auto,
            )
            .await
        {
            tracing::warn!("[scheduled] 写会话审批策略失败: {e}");
        }
    }

    // 3. 模型解析（assistant 模型 → DB 默认）
    let assistant_model_id = {
        let s = assistant.model_id.trim();
        if s.is_empty() { None } else { Some(s) }
    };
    let resolved_model =
        match resolve_run_model(state, &thread_id, None, assistant_model_id, user_id).await {
            Ok(m) => m,
            Err(_) => {
                tracing::error!("[scheduled] 模型解析失败 task_id={}", task.id);
                return Err((RunStatus::Failed, Some(thread_id.clone())));
            }
        };

    // 4. 取消令牌（30min 超时）+ 注册活跃 run
    let run_id = uuid::Uuid::now_v7().to_string();
    let cancel_token = tokio_util::sync::CancellationToken::new();
    if crate::infra::run_registry::register_active(
        &state.run_registry,
        &thread_id,
        &run_id,
        cancel_token.clone(),
    )
    .await
    .is_err()
    {
        tracing::warn!("[scheduled] 会话已有活跃 run（异常），继续 thread_id={thread_id}");
    }

    // 5. 装配 agent（无 SSE 前端 → 黑洞 channel：rx 立即 drop，tx 的 send().await
    //    因通道关闭立即返回 Err（各 sink 均 `let _ =` 吞掉），事件安全地丢弃。
    //    切勿持有 rx 不消费：channel 容量 8 且 send().await 带背压，第 9 个事件起
    //    工具闭包将永久挂起（≥9 个文件 artifact 的任务必超时）。
    let (sse_tx, _sse_rx) = {
        let (tx, rx) = tokio::sync::mpsc::channel::<axum::response::sse::Event>(8);
        drop(rx);
        (tx, ())
    };
    let messages: Vec<crate::server::sse::types::InputMessage> = vec![
        crate::server::sse::types::InputMessage {
            id: uuid::Uuid::now_v7().to_string(),
            role: "user".to_string(),
            content: task.instruction.clone(),
            mentions: Vec::new(),
            attachments: Vec::new(),
        },
    ];
    let mut user_text = task.instruction.clone();
    let agent_req = build_agent_request(
        state,
        &assistant,
        &thread_id,
        user_id,
        false,
        &mut user_text,
        &messages,
        resolved_model.id.clone(),
        cancel_token.clone(),
        None,
        &sse_tx,
    )
    .await;
    let agent_ctx = AgentContext {
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
        // 定时任务触发的顶层 run 也注入 app_state（允许 agent 在任务内继续管理任务）。
        app_state: Some(state.clone()),
    };
    let (agent, _budget, _child_usage) =
        match crate::agent::build_agent_for_session(&agent_ctx, &assistant, agent_req).await {
            Ok(t) => t,
            Err(e) => {
                tracing::error!("[scheduled] 构建 agent 失败: {e}");
                crate::infra::run_registry::deregister_active(
                    &state.run_registry,
                    &thread_id,
                    &run_id,
                )
                .await;
                return Err((RunStatus::Failed, Some(thread_id.clone())));
            }
        };

    // 6. 装配 Runner（与 SSE 路径一致；流式 SSE 模式，事件由下方循环消费落库）
    let run_config = Some(adk_rust::RunConfig {
        streaming_mode: adk_rust::StreamingMode::SSE,
        ..Default::default()
    });
    let runner_config = adk_rust::runner::RunnerConfig {
        app_name: "cortex-agent".to_string(),
        agent: agent.clone(),
        session_service: state.adk_session_service.clone(),
        artifact_service: state.artifact_service.clone(),
        memory_service: state.memory_service.clone(),
        plugin_manager: None,
        run_config,
        compaction_config: None,
        context_cache_config: None,
        cache_capable: None,
        request_context: None,
        cancellation_token: Some(cancel_token.clone()),
        intra_compaction_config: None,
        intra_compaction_summarizer: None,
    };
    let runner = match adk_rust::runner::Runner::new(runner_config) {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("[scheduled] 创建 Runner 失败: {e}");
            crate::infra::run_registry::deregister_active(&state.run_registry, &thread_id, &run_id)
                .await;
            return Err((RunStatus::Failed, Some(thread_id.clone())));
        }
    };

    let content = crate::server::sse::attachment::build_user_content(
        &user_text,
        &messages,
        state,
        &thread_id,
    )
    .await;
    let session_id_typed = adk_rust::SessionId::try_from(thread_id.clone())
        .unwrap_or_else(|_| adk_rust::SessionId::generate());

    // 7. 跑 agent（总预算 30min：启动阶段已消耗的部分从 drain 预算中扣除，
    //    两段各 30min 会让实际上限变成 60min，与注释/运维预期不符）
    let started_at = std::time::Instant::now();
    let run_fut = runner.run(adk_rust::UserId::new_unchecked(user_id), session_id_typed, content);
    let event_stream = match tokio::time::timeout(RUN_TIMEOUT, run_fut).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            tracing::error!("[scheduled] Runner.run 失败: {e}");
            crate::infra::run_registry::deregister_active(&state.run_registry, &thread_id, &run_id)
                .await;
            return Err((RunStatus::Failed, Some(thread_id.clone())));
        }
        Err(_) => {
            tracing::warn!("[scheduled] 任务启动超时 task_id={}", task.id);
            cancel_token.cancel();
            crate::infra::run_registry::deregister_active(&state.run_registry, &thread_id, &run_id)
                .await;
            return Err((RunStatus::Timeout, Some(thread_id.clone())));
        }
    };

    let outcome = drain_events(event_stream, &cancel_token, RUN_TIMEOUT.saturating_sub(started_at.elapsed())).await;

    crate::infra::run_registry::deregister_active(&state.run_registry, &thread_id, &run_id).await;

    let (assistant_text, agent_author) = match outcome {
        DrainOutcome::Completed(t, a) => (t, a),
        DrainOutcome::Timeout => {
            tracing::warn!("[scheduled] 任务执行超时（30min），已强杀 task_id={}", task.id);
            return Err((RunStatus::Timeout, Some(thread_id.clone())));
        }
        DrainOutcome::Failed(e) => {
            tracing::error!("[scheduled] 事件流错误 task_id={}: {e}", task.id);
            return Err((RunStatus::Failed, Some(thread_id.clone())));
        }
    };

    // 8. 落库 assistant 正文（补偿 adk 持久化，对齐 SSE 收尾）
    if !assistant_text.trim().is_empty() && !cancel_token.is_cancelled() {
        let cleaned = crate::server::sse::tool_display::strip_artifact_markers(&assistant_text);
        let mut event = adk_rust::Event::new(&run_id);
        event.author = if agent_author.is_empty() {
            "agent".to_string()
        } else {
            agent_author
        };
        let mut c = adk_rust::Content::new("model");
        c.parts = vec![adk_rust::Part::Text { text: cleaned }];
        event.llm_response.content = Some(c);
        event.llm_response.turn_complete = true;
        event.llm_response.partial = false;
        if let Err(e) = state.adk_session_service.append_event(&thread_id, event).await {
            tracing::warn!("[scheduled] 手动持久化 AI 回复失败: {e}");
        }
    }

    Ok(thread_id)
}

/// drain 结果。
enum DrainOutcome {
    /// 正常完成（assistant_text, agent_author）
    Completed(String, String),
    /// 超时（已 cancel）
    Timeout,
    /// 事件流错误
    Failed(String),
}

/// 消费 Runner 事件流：累计 assistant 正文（不推 SSE）。带剩余预算超时（总 30min 的尾段）。
async fn drain_events(
    mut event_stream: impl futures::Stream<Item = Result<adk_rust::Event, adk_rust::error::AdkError>> + Unpin,
    cancel_token: &tokio_util::sync::CancellationToken,
    budget: std::time::Duration,
) -> DrainOutcome {
    let mut assistant_text = String::new();
    let mut agent_author = String::new();

    let consume = async {
        while let Some(item) = event_stream.next().await {
            match item {
                Ok(event) => {
                    // 通用工具确认：无人值守不弹审批，跳过该请求让工具按默认（不放行）继续，
                    // 不产生确认挂起卡死事件流。注意 shell 命令不走这条路——已由会话级
                    // ApprovalPolicy::Auto 在工具内直接放行（见 execute 中 set_permission_policy）。
                    if event.actions.tool_confirmation.is_some() {
                        tracing::warn!("[scheduled] 定时任务遇通用工具确认，跳过（无人值守不审批）");
                        continue;
                    }
                    // user 角色（mailbox 注入）不入正文。
                    if event.author == "user" {
                        continue;
                    }
                    if let Some(content) = &event.llm_response.content {
                        for part in &content.parts {
                            if let adk_rust::Part::Text { text } = part {
                                assistant_text.push_str(text);
                                agent_author = event.author.clone();
                            }
                        }
                    }
                }
                Err(e) => return Err(e.to_string()),
            }
        }
        Ok(())
    };

    match tokio::time::timeout(budget, consume).await {
        Ok(Ok(())) => DrainOutcome::Completed(assistant_text, agent_author),
        Ok(Err(e)) => DrainOutcome::Failed(e),
        Err(_) => {
            cancel_token.cancel();
            DrainOutcome::Timeout
        }
    }
}

/// 清理某任务 30 天前的旧定时会话（session_settings 行 + adk 会话 + 沙箱目录）。
/// 每次运行成功后异步调用。
async fn cleanup_old_runs(state: Arc<AppState>, task: &ScheduledTask) {
    let Some(ss) = state.session_settings_store.clone() else {
        return;
    };
    let stale = match ss.delete_scheduled_older_than_30d(&task.id).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("[scheduled] 清理旧会话查询失败 task_id={}: {e}", task.id);
            return;
        }
    };
    if stale.is_empty() {
        return;
    }
    tracing::info!(
        "[scheduled] 清理任务 {} 的 {} 个过期会话（>30天）",
        task.id,
        stale.len()
    );
    for sid in stale {
        let del_req = adk_rust::session::DeleteRequest {
            app_name: "cortex-agent".to_string(),
            user_id: task.user_id.clone(),
            session_id: sid.clone(),
        };
        if let Err(e) = state.adk_session_service.delete(del_req).await {
            tracing::warn!("[scheduled] 删除过期 adk 会话失败 {sid}: {e}");
        }
        // 沙箱目录清理（若有）
        let sandbox_dir = state.config.workspace_session_dir(&sid);
        if sandbox_dir.exists() {
            let _ = tokio::fs::remove_dir_all(&sandbox_dir).await;
        }
    }
}
