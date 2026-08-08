//! LLM 调用辅助 —— 带指数退避的重试，以及纯文本收尾事件的构造。

use std::sync::Arc;
use std::time::Duration;

use adk_rust::{Content, Event, Llm, LlmRequest, Part, Result};
use tokio_util::sync::CancellationToken;

/// 构造一条纯文本 model 事件（turn_complete），用于软降级/失败收尾。
pub(super) fn make_text_event(invocation_id: &str, author: &str, text: &str) -> Event {
    let mut ev = Event::new(invocation_id);
    ev.author = author.to_string();
    ev.llm_response.content = Some(Content {
        role: "model".to_string(),
        parts: vec![Part::Text {
            text: text.to_string(),
        }],
    });
    ev.llm_response.turn_complete = true;
    ev
}

/// 带指数退避重试的 LLM 调用。
///
/// 对齐 codex 的流式重试策略：超时或调用错误按 200ms × 2^(n-1) 退避重试，最多
/// `max_retries` 次；用尽后返回最后一次错误，由调用方决定回退（回填文本事件 + 结束 turn）。
pub(super) async fn generate_with_retry(
    model: &Arc<dyn Llm>,
    request: LlmRequest,
    timeout_dur: Duration,
    max_retries: u32,
    cancel_token: &CancellationToken,
) -> Result<adk_rust::LlmResponseStream> {
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        // 建连阶段：select! 同时 await「LLM 建连（带超时）」与「取消」，对齐 codex model stream 的 or_cancel
        let connected = tokio::select! {
            res = tokio::time::timeout(timeout_dur, model.generate_content(request.clone(), true)) => res,
            _ = cancel_token.cancelled() => {
                tracing::info!("[cortex_agent] LLM 建连被用户取消");
                return Err(adk_rust::AdkError::agent("LLM 调用被用户取消".to_string()));
            }
        };
        match connected {
            Ok(Ok(s)) => return Ok(s),
            Ok(Err(e)) => {
                if attempt > max_retries {
                    return Err(e);
                }
                let delay = Duration::from_millis(200u64 << (attempt - 1).min(6));
                tracing::warn!(
                    "[cortex_agent] LLM 调用失败, 重试 {attempt}/{max_retries} (延迟 {delay:?}): {e}"
                );
                // 退避 sleep 也要响应取消，否则重试等待期间停止不生效
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {}
                    _ = cancel_token.cancelled() => return Err(adk_rust::AdkError::agent("LLM 调用被用户取消".to_string())),
                }
            }
            Err(_) => {
                if attempt > max_retries {
                    return Err(adk_rust::AdkError::agent(format!(
                        "LLM 调用超时 (>{timeout_dur:?}), 已重试 {max_retries} 次"
                    )));
                }
                let delay = Duration::from_millis(200u64 << (attempt - 1).min(6));
                tracing::warn!("[cortex_agent] LLM 调用超时, 重试 {attempt}/{max_retries}");
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {}
                    _ = cancel_token.cancelled() => return Err(adk_rust::AdkError::agent("LLM 调用被用户取消".to_string())),
                }
            }
        }
    }
}
