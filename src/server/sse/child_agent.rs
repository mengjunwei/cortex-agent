//! 子 agent（spawn_agent）活动事件 → SSE 事件桥接。
//!
//! `SseChildEventSink` 实现 `ChildEventSink` trait，被 CortexAgent 注入子 agent 后台任务，
//! 把 `ChildAgentEvent` 转成 `CHILD_AGENT_ACTIVITY` SSE 事件转发给前端。
//! Finished 事件保证送达（否则前端子任务面板永远卡 running），其余事件可丢。

use axum::response::sse::Event as SseEvent;
use tokio::sync::mpsc::Sender;

use super::types::SseEventMsg;
use crate::agent::runtime::cortex_agent::{ChildAgentEvent, ChildEventSink};

/// 子 agent 活动事件出口：把 ChildAgentEvent 转 SSE 事件发前端。
pub(super) struct SseChildEventSink {
    tx: Sender<SseEvent>,
}
impl SseChildEventSink {
    pub(super) fn new(tx: Sender<SseEvent>) -> Self {
        Self { tx }
    }
}
impl ChildEventSink for SseChildEventSink {
    fn emit(&self, event: ChildAgentEvent) {
        // Finished 事件必须送达（否则前端子任务面板永远卡 running）；其余可丢。
        let is_finished = matches!(event, ChildAgentEvent::Finished { .. });
        let msg = child_agent_event_to_sse(event);
        let sse = SseEvent::default().data(msg.to_sse_data());
        if is_finished {
            // send().await 带背压，保证送达；spawn 避免在同步 emit 里阻塞。
            let tx = self.tx.clone();
            tokio::spawn(async move {
                let _ = tx.send(sse).await;
            });
        } else {
            // try_send 同步非阻塞；通道满则丢弃（不阻塞子 agent 后台 drain）。
            let _ = self.tx.try_send(sse);
        }
    }
}

fn child_agent_event_to_sse(ev: ChildAgentEvent) -> SseEventMsg {
    use ChildAgentEvent as E;
    match ev {
        E::Started { task_name } => SseEventMsg::ChildAgentActivity {
            task_name, kind: "started".into(),
            tool_call_id: None, name: None, delta: None, args: None, content: None, ok: None, result: None,
        },
        E::Text { task_name, delta } => SseEventMsg::ChildAgentActivity {
            task_name, kind: "text".into(), delta: Some(delta),
            tool_call_id: None, name: None, args: None, content: None, ok: None, result: None,
        },
        E::ToolCall { task_name, tool_call_id, name, args } => SseEventMsg::ChildAgentActivity {
            task_name, kind: "tool_call".into(),
            tool_call_id: Some(tool_call_id), name: Some(name), args: Some(args),
            delta: None, content: None, ok: None, result: None,
        },
        E::ToolResult { task_name, tool_call_id, name, content } => SseEventMsg::ChildAgentActivity {
            task_name, kind: "tool_result".into(),
            tool_call_id: Some(tool_call_id), name: Some(name), content: Some(content),
            delta: None, args: None, ok: None, result: None,
        },
        E::Finished { task_name, ok, result } => SseEventMsg::ChildAgentActivity {
            task_name, kind: "finished".into(), ok: Some(ok), result: Some(result),
            tool_call_id: None, name: None, delta: None, args: None, content: None,
        },
    }
}
