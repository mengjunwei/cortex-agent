//! send_user_message_async 工具消息 → SSE 事件桥接。
//!
//! `SseAsyncMessageSink` 实现 `AsyncUserMessageSink` trait，被 agent 构建时捕获进
//! 工具闭包，把模型发的中途用户消息转成 `ASYNC_USER_MESSAGE` SSE 事件推给前端。
//! 消息必须送达（丢了用户就看不到这条中途气泡，模型的后续叙述会对不上），
//! 故用 spawn + send().await 而非 try_send。

use axum::response::sse::Event as SseEvent;
use tokio::sync::mpsc::Sender;

use super::types::SseEventMsg;
use crate::tools::send_user_message_async::AsyncUserMessageSink;

/// 异步用户消息出口：把消息转 SSE 事件发前端。
pub(super) struct SseAsyncMessageSink {
    tx: Sender<SseEvent>,
}
impl SseAsyncMessageSink {
    pub(super) fn new(tx: Sender<SseEvent>) -> Self {
        Self { tx }
    }
}
impl AsyncUserMessageSink for SseAsyncMessageSink {
    fn emit(&self, message: String) {
        let sse = SseEvent::default()
            .data(SseEventMsg::AsyncUserMessage { message }.to_sse_data());
        // spawn + send().await 带背压，保证送达；不在同步 emit 里阻塞调用方
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let _ = tx.send(sse).await;
        });
    }
}
