//! SSE 事件流核心：消费 adk-rust Runner 事件流，转换为前端 SSE 事件。
//!
//! [`create_event_stream`] 在独立 tokio 任务中运行 Agent；事件分派逻辑由
//! [`EventSink`] 承载——它收敛一次 run 内的可变状态（当前文本/思考块 id、
//! 已抑制的纯下载标记命令、累积的 assistant 正文等），并把每种 Part 的发送
//! 逻辑拆成独立方法，避免主循环膨胀。控制流（confirmation 的 return、
//! compaction / suppress 的 continue）留在主循环，用方法返回值表达。

use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::sync::{Arc, RwLock};

use axum::response::sse::Event as SseEvent;
use futures::Stream;
use serde_json::Value;
use tokio::sync::mpsc::Sender;
use tokio_stream::StreamExt;

use adk_rust::agent::Agent;

use super::attachment::build_user_content;
use super::error::send_run_error;
use super::screenshot;
use super::tool_display::{
    is_pure_artifact_command, mcp_server_name, strip_artifact_markers, tool_display_name,
};
use super::types::{InputMessage, SseEventMsg};
use crate::agent::runtime::cortex_agent::SharedBudget;
use crate::server::AppState;

/// 一次 run 内的 SSE 事件状态机：持有发送通道与可变状态，按 Part 类型分发发送逻辑。
struct EventSink<'a> {
    // 只读上下文
    tx: &'a Sender<SseEvent>,
    thread_id: &'a str,
    run_id: &'a str,
    mcp_slug_to_name: &'a HashMap<String, String>,
    state: &'a AppState,
    budget_handle: &'a Option<SharedBudget>,
    // 可变状态
    current_msg_id: String,
    text_open: bool,
    current_thinking_id: String,
    thinking_open: bool,
    assistant_text: String,
    agent_author: String,
    compaction_count: u32,
    /// 被抑制的纯下载标记命令 call_id 集合：FunctionCall 跳过发送时记入，
    /// 配对的 FunctionResponse 据此跳过 RESULT，使整条工具在前端不可见。
    suppressed_call_ids: Arc<RwLock<HashSet<String>>>,
}

impl<'a> EventSink<'a> {
    /// 推送一条 SSE 事件。
    async fn send(&self, msg: SseEventMsg) {
        let _ = self
            .tx
            .send(SseEvent::default().data(msg.to_sse_data()))
            .await;
    }

    /// 关闭打开的文本块（发 `TextMessageEnd`）。
    async fn close_text(&mut self) {
        if self.text_open {
            self.send(SseEventMsg::TextMessageEnd {
                message_id: self.current_msg_id.clone(),
            })
            .await;
            self.text_open = false;
        }
    }

    /// 关闭打开的思考块（发 `ThinkingMessageEnd`）。
    async fn close_thinking(&mut self) {
        if self.thinking_open {
            self.send(SseEventMsg::ThinkingMessageEnd {
                message_id: self.current_thinking_id.clone(),
            })
            .await;
            self.thinking_open = false;
        }
    }

    /// Token 用量上报（对齐 codex get_total_token_usage：真实 usage + 字节估算兜底）。
    ///
    /// 优先用 provider 返回的 usage_metadata；provider 不返回（如 mimo 流式 usage=null）时，
    /// 回退到 budget 快照的 effective_tokens（主循环混合估算：有 usage 用 usage，无则字符估算）。
    async fn emit_usage(&self, event: &adk_rust::Event) {
        let mut usage_total: u64 = 0;
        let mut usage_prompt: u64 = 0;
        let mut usage_completion: u64 = 0;
        if let Some(usage) = &event.llm_response.usage_metadata
            && usage.total_token_count > 0
        {
            usage_prompt = usage.prompt_token_count.max(0) as u64;
            usage_completion = usage.candidates_token_count.max(0) as u64;
            usage_total = usage.total_token_count.max(0) as u64;
        } else if let Some(budget) = self.budget_handle
            && let Ok(snap) = budget.read()
            && snap.effective_tokens > 0
        {
            // provider 无 usage → 用 budget 的 effective_tokens（混合估算值）
            usage_total = snap.effective_tokens as u64;
        }
        if usage_total > 0 {
            // 进度条分母 = 压缩软闸（context_window × 0.9，对齐 codex 窗口 90% 触发点）。
            // 从 budget 快照读；首帧尚无快照时回退 fallback 窗口 × 0.9。
            let threshold = self
                .budget_handle
                .as_ref()
                .and_then(|b| b.read().ok())
                .filter(|snap| snap.soft_gate > 0)
                .map(|snap| snap.soft_gate as u64)
                .unwrap_or_else(|| {
                    (self.state.config.context.fallback_context_window as f64 * 0.9) as u64
                });
            self.send(SseEventMsg::ContextUsage {
                prompt_tokens: usage_prompt,
                completion_tokens: usage_completion,
                total_tokens: usage_total,
                threshold,
            })
            .await;
        }
    }

    /// L3 压缩检查点（actions.compaction，content=None）：flush 打开的消息块 +
    /// 推 `CONTEXT_COMPACTED` 通知前端「上下文已自动整理」。调用方随后 `continue`。
    async fn on_compaction(&mut self) {
        self.close_text().await;
        self.close_thinking().await;
        self.compaction_count += 1;
        self.send(SseEventMsg::ContextCompacted {
            compaction_count: self.compaction_count,
        })
        .await;
    }

    /// 工具确认：发 `ToolConfirmation` + `RunFinished(tool_confirmation)`。
    /// 调用方随后负责移除 cancel_token 并 `return` 结束 spawn 任务。
    async fn on_confirmation(
        &self,
        tool_name: &str,
        function_call_id: &Option<String>,
        args: &Value,
    ) {
        self.send(SseEventMsg::ToolConfirmation {
            tool_name: tool_display_name(tool_name),
            function_call_id: function_call_id.clone().unwrap_or_default(),
            args: args.clone(),
        })
        .await;
        self.send(SseEventMsg::RunFinished {
            thread_id: self.thread_id.to_string(),
            run_id: self.run_id.to_string(),
            reason: "tool_confirmation".to_string(),
        })
        .await;
    }

    /// 处理一个 Text part：切关思考块、累积正文、剥 ARTIFACT 标记、推送文本分片。
    async fn on_text(&mut self, text: &str, author: &str) {
        if self.thinking_open {
            self.close_thinking().await;
        }
        if author != "user" {
            self.assistant_text.push_str(text);
            self.agent_author = author.to_string();
        }
        // 剥 [[ARTIFACT:...]] 标记：模型若把产物标记抄进正文，
        // 这里兜底剥掉再推前端，避免界面出现这串内部信号误导用户。
        let cleaned = strip_artifact_markers(text);
        if !self.text_open {
            self.current_msg_id = uuid::Uuid::now_v7().to_string();
            self.send(SseEventMsg::TextMessageStart {
                message_id: self.current_msg_id.clone(),
            })
            .await;
            self.text_open = true;
        }
        self.send(SseEventMsg::TextMessageContent {
            message_id: self.current_msg_id.clone(),
            delta: cleaned,
        })
        .await;
    }

    /// 处理一个 Thinking part：推送思考流分片（仅非 user、非空）。
    async fn on_thinking(&mut self, thinking: &str, author: &str) {
        if author != "user" && !thinking.is_empty() {
            // thinking 流直接推送，不再累积做重复检测：
            // 退化治理已回归 cortex_agent 主循环的协议驱动 + thinking budget + max_iterations。
            if !self.thinking_open {
                self.current_thinking_id = uuid::Uuid::now_v7().to_string();
                self.send(SseEventMsg::ThinkingMessageStart {
                    message_id: self.current_thinking_id.clone(),
                })
                .await;
                self.thinking_open = true;
            }
            self.send(SseEventMsg::ThinkingMessageContent {
                message_id: self.current_thinking_id.clone(),
                delta: thinking.to_string(),
            })
            .await;
        }
    }

    /// 处理一个 FunctionCall part。返回 `true` 表示是被抑制的纯下载标记命令
    /// （调用方应 `continue` 跳过），`false` 表示已正常推送。
    async fn on_function_call(&mut self, name: &str, args: &Value, id: &Option<String>) -> bool {
        let call_id = id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::now_v7().to_string());
        // 纯下载标记命令抑制：若 shell_command 的命令剥掉 [[ARTIFACT:...]] 及其 echo 后为空
        // （即整条只是 echo 标记），整条工具事件不发前端——它是脚本产物→文件卡片的内部信号，
        // 对用户无意义。标记的 call_id 记入 skip 集合，对应 FunctionResponse 也跳过。
        if name == "shell_command" && is_pure_artifact_command(args) {
            self.suppressed_call_ids
                .write()
                .expect("suppressed set lock")
                .insert(call_id.clone());
            tracing::info!(
                "[SSE] 抑制纯下载标记命令（不推前端）: call_id={}",
                call_id
            );
            return true;
        }
        self.close_thinking().await;
        self.close_text().await;
        self.send(SseEventMsg::ToolCallStart {
            tool_call_id: call_id.clone(),
            tool_call_name: tool_display_name(name),
            server_name: mcp_server_name(name, self.mcp_slug_to_name),
        })
        .await;
        let args_str = match args {
            Value::String(s) => s.clone(),
            other => serde_json::to_string(other).unwrap_or_default(),
        };
        if !args_str.is_empty() {
            self.send(SseEventMsg::ToolCallArgs {
                tool_call_id: call_id.clone(),
                delta: args_str,
            })
            .await;
        }
        self.send(SseEventMsg::ToolCallEnd {
            tool_call_id: call_id,
        })
        .await;
        false
    }

    /// 处理一个 FunctionResponse part。返回 `true` 表示是被抑制命令的配对响应
    /// （调用方应 `continue`），`false` 表示已正常推送。screenshot 工具走对象存储兜底。
    async fn on_function_response(
        &mut self,
        name: &str,
        response: &Value,
        call_id: String,
    ) -> bool {
        // 配对跳过被抑制的纯下载标记命令的响应
        if self
            .suppressed_call_ids
            .write()
            .expect("suppressed set lock")
            .remove(&call_id)
        {
            return true;
        }

        let result_str = if name.contains("screenshot") {
            tracing::info!(
                "[screenshot] SSE 兜底命中 screenshot 工具, object_store={}, resp_keys={:?}",
                self.state.object_store.is_some(),
                response
                    .as_object()
                    .map(|m| m.keys().collect::<Vec<_>>())
                    .unwrap_or_default()
            );
            // 工具层（TruncatingTool）或 ScreenshotToolWrapper 已注入 image_url 时直接透传：
            // 兼容顶层 image_url 与 output.image_url
            let already_has_image_url = response
                .get("image_url")
                .or_else(|| response.get("output").and_then(|o| o.get("image_url")))
                .is_some();
            if already_has_image_url {
                serde_json::to_string(response).unwrap_or_else(|_| response.to_string())
            } else {
                // 兜底：从 saved_path 或 base64 上传到对象存储
                let saved = match &self.state.object_store {
                    Some(os) => {
                        screenshot::save_screenshot_if_needed(
                            os,
                            self.thread_id,
                            response,
                            self.run_id,
                            &call_id,
                        )
                        .await
                    }
                    None => None,
                };
                match saved {
                    Some(filename) => {
                        let mut enriched = response.clone();
                        let saved_path =
                            format!("screenshots/{}/{filename}", self.thread_id);
                        if let Some(obj) = enriched.as_object_mut() {
                            obj.insert(
                                "image_url".to_string(),
                                serde_json::Value::String(format!(
                                    "/api/screenshots/{}/{filename}",
                                    self.thread_id
                                )),
                            );
                            obj.insert(
                                "saved_path".to_string(),
                                serde_json::Value::String(saved_path),
                            );
                        } else {
                            enriched = serde_json::json!({
                                "data": response,
                                "image_url": format!("/api/screenshots/{}/{filename}", self.thread_id),
                                "saved_path": saved_path
                            });
                        }
                        serde_json::to_string(&enriched).unwrap_or_else(|_| response.to_string())
                    }
                    None => {
                        serde_json::to_string(response).unwrap_or_else(|_| response.to_string())
                    }
                }
            }
        } else {
            serde_json::to_string(response).unwrap_or_else(|_| response.to_string())
        };

        self.send(SseEventMsg::ToolCallResult {
            tool_call_id: call_id,
            tool_name: tool_display_name(name),
            content: result_str,
        })
        .await;
        false
    }
}

/// 创建 SSE 事件流 — 在独立 tokio 任务中运行 Agent 并将事件转换为 SSE 格式。
///
/// 核心逻辑：
/// 1. 发送 `RUN_STARTED` 事件
/// 2. 配置 Runner（session/artifact/memory 服务、compaction、cancellation）
/// 3. 消费 Runner 事件流，将 LLM 响应和工具调用转换为 SSE 事件（由 [`EventSink`] 分派）
/// 4. 处理工具确认（tool_confirmation）— 发送确认请求后暂停等待用户响应
/// 5. 流结束后手动持久化 AI 回复到 PostgreSQL（补偿 adk-rust 的持久化行为）
/// 6. 发送 `RUN_FINISHED` 事件
#[allow(clippy::too_many_arguments)]
pub(super) fn create_event_stream(
    state: Arc<AppState>,
    agent: Arc<dyn Agent>,
    budget_handle: Option<SharedBudget>,
    thread_id: String,
    run_id: String,
    user_id: String,
    messages: Vec<InputMessage>,
    user_text: String,
    tool_decisions: Option<HashMap<String, String>>,
    model_id: Option<String>,
    tx: Sender<SseEvent>,
    rx: tokio::sync::mpsc::Receiver<SseEvent>,
    cancel_token: tokio_util::sync::CancellationToken,
) -> impl Stream<Item = Result<SseEvent, Infallible>> {
    tokio::spawn(async move {
        let start_ev = SseEventMsg::RunStarted {
            thread_id: thread_id.clone(),
            run_id: run_id.clone(),
        };
        let _ = tx
            .send(SseEvent::default().data(start_ev.to_sse_data()))
            .await;

        // 预取 MCP slug→server.name 映射，供工具卡展示来源
        // （一次 run 复用，避免每个工具事件都查库）
        let mcp_slug_to_name: HashMap<String, String> = match state.mcp_manager.as_ref() {
            Some(mgr) => mgr
                .list_servers()
                .await
                .map(|servers| servers.into_iter().map(|s| (s.slug, s.name)).collect())
                .unwrap_or_default(),
            None => Default::default(),
        };

        let session_service = state.adk_session_service.clone();

        // cancel_token 由 handle_run_sse 提前创建并注册到全局表（cancel 接口按 thread_id 找到），
        // 这里直接复用，注入 runner_config（runner 事件边界 is_cancelled 轮询 + agent 工具 select! 双保险）。

        let mut confirmation_decisions = HashMap::new();
        if let Some(decisions) = tool_decisions {
            for (tool_name, decision) in decisions {
                let decision = match decision.to_lowercase().as_str() {
                    "approve" | "yes" | "true" => adk_rust::ToolConfirmationDecision::Approve,
                    _ => adk_rust::ToolConfirmationDecision::Deny,
                };
                confirmation_decisions.insert(tool_name, decision);
            }
        }
        let run_config = Some(adk_rust::RunConfig {
            streaming_mode: adk_rust::StreamingMode::SSE,
            tool_confirmation_decisions: confirmation_decisions,
            ..Default::default()
        });

        let runner_config = adk_rust::runner::RunnerConfig {
            app_name: "cortex-agent".to_string(),
            agent: agent.clone(),
            session_service: session_service.clone(),
            artifact_service: state.artifact_service.clone(),
            memory_service: state.memory_service.clone(),
            plugin_manager: None,
            run_config,
            // 上下文压缩统一由 CortexAgent 的窗口阈值压缩接管（对齐 codex：仅按窗口 90% 触发），
            // 不再使用 adk-rust 的按轮数（L1）/单轮 token（L2）压缩。
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
                tracing::error!("创建 Runner 失败: {}", e);
                send_run_error(&tx, &thread_id, &run_id, e.to_string()).await;
                state.cancellation_tokens.lock().await.remove(&thread_id);
                return;
            }
        };

        // 用户输入使用外层构造好的 user_text（已过 mention XML 注入）。
        // 历史消息由 adk-rust 的 Session 服务负责持久化与回放，不应在此重复拼接——
        // 早期实现曾把 messages 全量 join 塞给 LLM（含历史 assistant 回复），
        // 会让模型误以为对话已完结而空转，事件流零输出（本次 bug 根因）。
        let content = build_user_content(&user_text, &messages);
        tracing::info!(
            "[RunSSE] 提交给 Runner 的 content: {} 字符（已注入 mention）",
            user_text.len()
        );

        let session_id = adk_rust::SessionId::try_from(thread_id.clone())
            .unwrap_or_else(|_| adk_rust::SessionId::generate());
        let mut event_stream = match runner
            .run(
                adk_rust::UserId::new_unchecked(&user_id),
                session_id,
                content,
            )
            .await
        {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("Runner.run() 失败: {}", e);
                send_run_error(&tx, &thread_id, &run_id, e.to_string()).await;
                state.cancellation_tokens.lock().await.remove(&thread_id);
                return;
            }
        };

        let mut sink = EventSink {
            tx: &tx,
            thread_id: &thread_id,
            run_id: &run_id,
            mcp_slug_to_name: &mcp_slug_to_name,
            state: &state,
            budget_handle: &budget_handle,
            current_msg_id: String::new(),
            text_open: false,
            current_thinking_id: String::new(),
            thinking_open: false,
            assistant_text: String::new(),
            agent_author: String::new(),
            compaction_count: 0,
            suppressed_call_ids: Arc::new(RwLock::new(HashSet::new())),
        };

        while let Some(event_result) = event_stream.next().await {
            match event_result {
                Ok(event) => {
                    tracing::info!(
                        "[SSE] event author={} turn_complete={} interrupted={}",
                        event.author,
                        event.llm_response.turn_complete,
                        event.llm_response.interrupted,
                    );

                    if let Some(ref confirmation) = event.actions.tool_confirmation {
                        tracing::info!(
                            "[SSE] tool_confirmation: name={} call_id={:?} args={}",
                            confirmation.tool_name,
                            confirmation.function_call_id,
                            serde_json::to_string(&confirmation.args).unwrap_or_default(),
                        );
                        sink.on_confirmation(
                            &confirmation.tool_name,
                            &confirmation.function_call_id,
                            &confirmation.args,
                        )
                        .await;
                        state.cancellation_tokens.lock().await.remove(&thread_id);
                        return;
                    }

                    sink.emit_usage(&event).await;

                    // 压缩检查点事件（L3 yield 的 actions.compaction，content=None）：
                    // flush 打开的消息块 + 推 CONTEXT_COMPACTED 通知前端「上下文已自动整理」。
                    // content=None 故不会进下面的正文分支，这里显式拦截发通知。
                    if event.actions.compaction.is_some() {
                        sink.on_compaction().await;
                        continue;
                    }

                    if let Some(content) = &event.llm_response.content {
                        tracing::info!("[SSE] content parts count={}", content.parts.len());
                        for part in &content.parts {
                            match part {
                                adk_rust::Part::Text { text } => {
                                    tracing::info!("[SSE] Text part: {} chars", text.len());
                                    tracing::debug!("[SSE] STREAM TEXT CHUNK: {}", text);
                                    sink.on_text(text, &event.author).await;
                                }
                                adk_rust::Part::FunctionCall { name, args, id, .. } => {
                                    tracing::info!(
                                        "[SSE] FunctionCall: name={} id={:?}",
                                        name,
                                        id
                                    );
                                    if sink.on_function_call(name, args, id).await {
                                        continue;
                                    }
                                }
                                adk_rust::Part::FunctionResponse {
                                    function_response,
                                    id,
                                    ..
                                } => {
                                    let call_id = id.clone().unwrap_or_default();
                                    tracing::info!(
                                        "[SSE] FunctionResponse: name={} call_id={} resp_len={}",
                                        function_response.name,
                                        call_id,
                                        serde_json::to_string(&function_response.response)
                                            .map(|s| s.len())
                                            .unwrap_or(0),
                                    );
                                    if sink
                                        .on_function_response(
                                            &function_response.name,
                                            &function_response.response,
                                            call_id,
                                        )
                                        .await
                                    {
                                        continue;
                                    }
                                }
                                adk_rust::Part::Thinking { thinking, signature } => {
                                    tracing::info!(
                                        "[SSE] Thinking part: {} chars signature={}",
                                        thinking.chars().count(),
                                        signature.as_ref().map(|s| s.len()).unwrap_or(0)
                                    );
                                    sink.on_thinking(thinking, &event.author).await;
                                }
                                adk_rust::Part::EmbeddedResource { .. } => {
                                    tracing::debug!("[SSE] EmbeddedResource part skipped");
                                }
                                adk_rust::Part::InlineData { mime_type, data, .. } => {
                                    tracing::info!(
                                        "[SSE] InlineData part: mime_type={} bytes={}",
                                        mime_type,
                                        data.len()
                                    );
                                }
                                adk_rust::Part::FileData {
                                    mime_type,
                                    file_uri,
                                    ..
                                } => {
                                    tracing::info!(
                                        "[SSE] FileData part: mime_type={} uri={}",
                                        mime_type,
                                        file_uri
                                    );
                                }
                                adk_rust::Part::ServerToolCall { server_tool_call } => {
                                    tracing::info!(
                                        "[SSE] ServerToolCall part: {}",
                                        serde_json::to_string(server_tool_call).unwrap_or_default()
                                    );
                                }
                                adk_rust::Part::ServerToolResponse {
                                    server_tool_response,
                                } => {
                                    tracing::info!(
                                        "[SSE] ServerToolResponse part: {}",
                                        serde_json::to_string(server_tool_response)
                                            .unwrap_or_default()
                                    );
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::error!("[SSE] 事件流错误: {}", e);
                    send_run_error(&tx, &thread_id, &run_id, e.to_string()).await;
                    state.cancellation_tokens.lock().await.remove(&thread_id);
                    return;
                }
            }
        }

        sink.close_thinking().await;

        // 空响应告警：如果 agent 跑完了但一个 token 都没吐出来，
        // 极可能是端点协议错配（Anthropic vs OpenAI）、API Key 失效、
        // 或模型服务返回了非流式空响应。此前这种情况前端会看到"消息发出去没反应"，
        // 极难排查——这里显式打日志 + 发一条错误事件到前端。
        if sink.assistant_text.trim().is_empty() && !cancel_token.is_cancelled() {
            tracing::warn!(
                "[SSE] agent 完成但未输出任何内容（可能是端点/密钥配置错误、模型服务空响应或被网关拦截）\
                 model_id={:?} thread_id={}",
                model_id,
                thread_id
            );
            sink.send(SseEventMsg::RunError {
                message: "模型未返回任何内容。可能原因：\
                         1) base_url 端点协议错配（如 GLM 的 Anthropic 端点需改为 OpenAI 兼容端点 `/api/paas/v4`）；\
                         2) API Key 失效或欠费；\
                         3) 网关拦截。\
                         请检查后端日志与「模型供应商管理」配置。"
                    .to_string(),
            })
            .await;
        }

        tracing::info!("[SSE] 事件流结束（agent 运行完成）");
        {
            let mut tokens = state.cancellation_tokens.lock().await;
            tokens.remove(&thread_id);
        }

        // 用户取消时不持久化半截回复（避免 session 历史里出现一条 turn_complete 的残缺消息）
        // 落库前剥 [[ARTIFACT:...]] 标记：跨片标记此时已完整拼好，整体剥一次最干净，
        // 保证历史会话恢复也不再出现这串内部信号。
        let assistant_text = strip_artifact_markers(&sink.assistant_text);
        if !assistant_text.is_empty() && !cancel_token.is_cancelled() {
            let parts: Vec<adk_rust::Part> = vec![adk_rust::Part::Text {
                text: assistant_text.clone(),
            }];

            let mut event = adk_rust::Event::new(&run_id);
            event.author = if sink.agent_author.is_empty() {
                "agent".to_string()
            } else {
                sink.agent_author.clone()
            };
            let mut content = adk_rust::Content::new("model");
            content.parts = parts;
            event.llm_response.content = Some(content);
            event.llm_response.turn_complete = true;
            event.llm_response.partial = false;

            tracing::info!(
                "[SSE] 手动持久化 AI 回复到 PG: text={} chars",
                assistant_text.len()
            );
            if let Err(e) = state
                .adk_session_service
                .append_event(&thread_id, event)
                .await
            {
                tracing::warn!("[SSE] 手动持久化失败: {}", e);
            }
        }

        sink.close_text().await;
        sink.send(SseEventMsg::RunFinished {
            thread_id: thread_id.clone(),
            run_id: run_id.clone(),
            reason: "complete".to_string(),
        })
        .await;

        // 沙箱快照上传(会话亲和容灾):本轮结束后异步上传,供节点故障切换时恢复
        if let Some(os) = state.object_store.clone() {
            let sandbox_dir = state.config.workspace_session_dir(&thread_id);
            if sandbox_dir.exists() {
                let sid = thread_id.clone();
                tokio::spawn(async move {
                    if let Err(e) =
                        crate::infra::workspace_snapshot::upload(&os, &sid, &sandbox_dir).await
                    {
                        tracing::warn!("[sse] 上传沙箱快照失败(可忽略): {e}");
                    }
                });
            }
        }
    });

    let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
    stream.map(Ok)
}
