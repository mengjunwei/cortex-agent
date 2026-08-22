//! 子 agent 持久会话循环：turn（agent.run drain）→ 终态投 FINAL_ANSWER 回父 →
//! 等 mailbox 新 trigger → 复活跑新 turn。父 cancel 级联退出；interrupt 取消当轮后
//! 由 followup 换新 token 复活。

use std::sync::{Arc, Mutex as StdMutex, OnceLock};

use adk_rust::async_trait;
use adk_rust::{
    Agent, Artifacts, CallbackContext, Content, EventStream, InvocationContext, Memory, Part,
    ReadonlyContext, RunConfig, Session, SharedState,
};
use futures::StreamExt;
use tokio_util::sync::CancellationToken;

use super::envelope::{InterAgentMessageType, render_inter_agent_message};
use super::mailbox::ParentMailbox;
use super::session::ChildSession;
use super::status::ChildStatus;
use super::tree::{AgentTree, ChildHandle, MailboxItem};
use super::{ChildAgentEvent, ChildEventSink};

/// 驱动子 agent 的持久循环：turn（agent.run drain）→ 终态投递 FINAL_ANSWER 回父 →
/// 等 mailbox 新 trigger → 复活跑新 turn。父 cancel 级联退出；interrupt 取消当轮后
/// 由 followup 换新 token 复活。
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_child_loop(
    agent: Arc<dyn Agent>,
    session: ChildSession,
    child: Arc<ChildHandle>,
    tree: Arc<AgentTree>,
    sink: Arc<dyn ChildEventSink>,
    canonical: &str,
    parent_path: &str,
    parent_mailbox: Option<Arc<ParentMailbox>>,
    parent_cancel: CancellationToken,
) {
    // 初始任务已在 spawn 侧投 inbox + notify（permit 存储，notified().await 立即返回）
    loop {
        // 等 wake（初始任务 / followup_task）。父 cancel 参与竞速——CancellationToken
        // cancel 不会唤醒 Notify，不 select 会在主 run 结束后永久挂起（task 泄漏）。
        // interrupt 不在此分支：对齐 codex「interrupt 只作用于进行中的 turn，打断空闲
        // agent 是 no-op」——cancelled 的 token 若挂进 select 会恒 ready 造成忙旋。
        tokio::select! {
            _ = child.wake.notified() => {}
            _ = parent_cancel.cancelled() => break,
        }
        if parent_cancel.is_cancelled() {
            break;
        }
        // 每轮从 handle 取当前 interrupt token（复活时被整体换新，旧 cancel 不影响新轮）
        let interrupt_token = child
            .interrupt
            .lock()
            .expect("interrupt lock poisoned")
            .clone();
        if interrupt_token.is_cancelled() {
            continue; // 等 followup 换新 token 后的 wake（此时 wake 已有 permit，不忙旋）
        }

        // drain mailbox：全部消息随本轮注入（trigger_turn=true 的消息驱动本轮启动，
        // QueueOnly 的搭车；对齐 codex「turn 结束后 drain 全部 pending」）。
        let mut pending: Vec<String> = Vec::new();
        {
            let mut inbox = child.inbox.lock().expect("child inbox poisoned");
            while let Some(item) = inbox.pop_front() {
                pending.push(item.rendered);
            }
        }
        if pending.is_empty() {
            continue; // 虚假唤醒
        }
        for rendered in &pending {
            session.push(Content {
                role: "user".to_string(),
                parts: vec![Part::Text {
                    text: rendered.clone(),
                }],
            });
        }
        // 注：这里不 notify_activity——drain 消费不是新投递，广播会让其它 agent 的
        // wait_agent 假唤醒（对齐 codex：activity 只在 enqueue 时触发）。

        // 状态置 Running
        let _ = child.status.send_replace(ChildStatus::Running);

        // 事件聚合键用 canonical path（树内唯一——尾段名可撞，如 /root/task_1/sub 与
        // /root/task_2/sub 同叫 sub，用尾段会让前端把两个 agent 的活动混进同一面板）。
        let status = run_one_turn(&agent, &session, &sink, canonical, &interrupt_token).await;

        // 终态：投递 FINAL_ANSWER 回父（对齐 codex forward_child_completion_to_parent）。
        // 父是 root → 投 ParentMailbox（主循环 drain 注入 conv）；
        // 父是子 agent → 投其 ChildHandle.inbox（其会话循环 drain）。
        if let Some(payload) = status.completion_payload() {
            let rendered = render_inter_agent_message(
                InterAgentMessageType::FinalAnswer,
                parent_path,
                canonical,
                &payload,
            );
            if let Some(mb) = &parent_mailbox {
                mb.push(rendered);
            } else if tree.get(parent_path).is_some() {
                let _ = tree.deliver(
                    parent_path,
                    MailboxItem {
                        rendered,
                        trigger_turn: false,
                    },
                );
            }
            tree.notify_activity();
        }
        // 轮内注入的消息（孙 agent FINAL_ANSWER / 兄弟 MESSAGE，由 run() 每轮 drain 注入
        // 其局部 conv）落回 session——局部 conv 随 turn 结束丢弃，不落回则上下文断裂。
        let injected =
            std::mem::take(&mut *child.injected_log.lock().expect("injected log poisoned"));
        for c in injected {
            session.push(c);
        }

        let (ok, result_text) = match &status {
            ChildStatus::Completed(Some(s)) => (true, s.clone()),
            ChildStatus::Completed(None) => (true, String::new()),
            ChildStatus::Errored(e) => (false, e.clone()),
            ChildStatus::Interrupted => (false, "interrupted".to_string()),
            ChildStatus::PendingInit | ChildStatus::Running => (true, String::new()),
        };
        sink.emit(ChildAgentEvent::Finished {
            task_name: canonical.to_string(),
            ok,
            result: result_text,
        });
        let _ = child.status.send_replace(status);

        // 循环回 wait：无 followup 则永久等待（树 drop → 父 cancel 级联退出）
    }
}

/// 跑一个 turn：agent.run() → drain 事件流（转发 sink + 提取终答 + 写回 session）。
async fn run_one_turn(
    agent: &Arc<dyn Agent>,
    session: &ChildSession,
    sink: &Arc<dyn ChildEventSink>,
    display_name: &str,
    interrupt_token: &CancellationToken,
) -> ChildStatus {
    // 事件收集器：终答 = 最后一个非空文本 turn
    let collector = TurnCollector::new();
    let ctx = SimpleChildCtx::new(session, agent.clone());
    let interrupt_clone = interrupt_token.clone();
    let run_fut = agent.run(Arc::new(ctx));
    tokio::pin!(run_fut);
    // interrupt 竞速：interrupt_agent cancel → 尽快结束本 turn
    let stream = tokio::select! {
        r = &mut run_fut => match r {
            Ok(s) => s,
            Err(e) => return ChildStatus::Errored(format!("agent run failed: {e}")),
        },
        _ = interrupt_clone.cancelled() => {
            return ChildStatus::Interrupted;
        }
    };
    drain_child_stream(
        stream,
        sink.clone(),
        display_name,
        collector,
        session,
        interrupt_token,
    )
    .await
}

/// 单 turn 事件收集（终答提取 + 转发 sink + conv 写回 session）。
struct TurnCollector {
    current: StdMutex<String>,
    final_answer: StdMutex<String>,
}

impl TurnCollector {
    fn new() -> Self {
        Self {
            current: StdMutex::new(String::new()),
            final_answer: StdMutex::new(String::new()),
        }
    }
}

/// 消费子 agent 的事件流：转发活动到 sink，提取最终文本，把模型产出写回 session 历史。
async fn drain_child_stream(
    mut stream: EventStream,
    sink: Arc<dyn ChildEventSink>,
    task_name: &str,
    collector: TurnCollector,
    session: &ChildSession,
    interrupt_token: &CancellationToken,
) -> ChildStatus {
    let mut interrupted = false;
    loop {
        // interrupt 竞速：不等当前事件（正在跑的工具/LLM 调用完成可能要 tool_timeout）
        // ——cancel 即刻打断 drain（对齐 codex interrupt 立即中止当前 turn）。
        let ev_result = tokio::select! {
            r = stream.next() => match r {
                Some(r) => r,
                None => break,
            },
            _ = interrupt_token.cancelled() => {
                interrupted = true;
                break;
            }
        };
        let ev = match ev_result {
            Ok(ev) => ev,
            Err(e) => return ChildStatus::Errored(format!("stream error: {e}")),
        };
        if let Some(content) = ev.llm_response.content.as_ref() {
            // 模型产出 + 工具结果都写回 session（持久历史）。FR 不写回则跨 turn 失忆——
            // followup 的新 turn 里孤儿 FC 会被 normalize 补「aborted」占位，模型误以为
            // 上轮工具调用全失败（对齐 codex rollout 全量记录语义）。
            if (content.role == "model" || content.role == "function") && !content.parts.is_empty()
            {
                session.push(content.clone());
            }
            for p in &content.parts {
                match p {
                    Part::Text { text } => {
                        if !text.is_empty() {
                            collector
                                .current
                                .lock()
                                .expect("collector poisoned")
                                .push_str(text);
                            sink.emit(ChildAgentEvent::Text {
                                task_name: task_name.to_string(),
                                delta: text.clone(),
                            });
                        }
                    }
                    Part::FunctionCall { name, args, id, .. } => {
                        sink.emit(ChildAgentEvent::ToolCall {
                            task_name: task_name.to_string(),
                            tool_call_id: id.clone().unwrap_or_default(),
                            name: name.clone(),
                            args: args.to_string(),
                        });
                    }
                    Part::FunctionResponse {
                        function_response,
                        id,
                        ..
                    } => {
                        sink.emit(ChildAgentEvent::ToolResult {
                            task_name: task_name.to_string(),
                            tool_call_id: id.clone().unwrap_or_default(),
                            name: function_response.name.clone(),
                            content: function_response.response.to_string(),
                        });
                    }
                    _ => {}
                }
            }
        }
        if ev.llm_response.turn_complete || ev.llm_response.finish_reason.is_some() {
            let mut cur = collector.current.lock().expect("collector poisoned");
            if !cur.trim().is_empty() {
                *collector.final_answer.lock().expect("collector poisoned") =
                    std::mem::take(&mut *cur);
            } else {
                cur.clear();
            }
        }
    }
    if interrupted {
        return ChildStatus::Interrupted;
    }
    let answer = collector
        .final_answer
        .lock()
        .expect("collector poisoned")
        .clone();
    if answer.trim().is_empty() {
        ChildStatus::Completed(None)
    } else {
        ChildStatus::Completed(Some(answer))
    }
}

// ============================================================================
// 简化子 ctx —— 仅 Session + Agent（CortexAgent.run 实际只用 session/artifacts 等，
// 子 agent 的工具执行能力来自 build 时注册的工具集（与父同构），无需透传父 ctx。）
// ============================================================================

/// 占位 ctx：CortexAgent.run 不读 user_content（从 session.conversation_history 起步），
/// 但 trait 实现必须有。用独立类型把 unreachable 改成返回静态占位。
struct SimpleChildCtx {
    session: *const ChildSession,
    agent: Arc<dyn Agent>,
    placeholder: Content,
}

impl SimpleChildCtx {
    fn new(session: &ChildSession, agent: Arc<dyn Agent>) -> Self {
        Self {
            session: session as *const ChildSession,
            agent,
            placeholder: Content {
                role: "user".to_string(),
                parts: vec![Part::Text {
                    text: String::new(),
                }],
            },
        }
    }
    fn sess(&self) -> &ChildSession {
        unsafe { &*self.session }
    }
}

// SAFETY: session 引用与 ctx 同生命周期（run_one_turn 持有两者）。
unsafe impl Send for SimpleChildCtx {}
unsafe impl Sync for SimpleChildCtx {}

#[async_trait]
impl ReadonlyContext for SimpleChildCtx {
    fn invocation_id(&self) -> &str {
        "child"
    }
    fn agent_name(&self) -> &str {
        self.agent.name()
    }
    fn user_id(&self) -> &str {
        self.sess().user_id()
    }
    fn app_name(&self) -> &str {
        self.sess().app_name()
    }
    fn session_id(&self) -> &str {
        self.sess().id()
    }
    fn branch(&self) -> &str {
        "main"
    }
    fn user_content(&self) -> &Content {
        &self.placeholder
    }
}

#[async_trait]
impl CallbackContext for SimpleChildCtx {
    fn artifacts(&self) -> Option<Arc<dyn Artifacts>> {
        None
    }
    fn shared_state(&self) -> Option<Arc<SharedState>> {
        None
    }
}

#[async_trait]
impl InvocationContext for SimpleChildCtx {
    fn agent(&self) -> Arc<dyn Agent> {
        self.agent.clone()
    }
    fn memory(&self) -> Option<Arc<dyn Memory>> {
        None
    }
    fn session(&self) -> &dyn Session {
        self.sess()
    }
    fn run_config(&self) -> &RunConfig {
        static RUN_CONFIG: OnceLock<RunConfig> = OnceLock::new();
        RUN_CONFIG.get_or_init(RunConfig::default)
    }
    fn end_invocation(&self) {}
    fn ended(&self) -> bool {
        false
    }
}
