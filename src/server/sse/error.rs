//! SSE 错误流构造。
//!
//! 两处错误出口共用：
//! - `early_error_response`：Agent 启动前的早期错误（助手不存在 / 加载失败 / 存储不可用），
//!   返回一个 `RUN_ERROR + RUN_FINISHED(error)` 的 axum Response；
//! - `send_run_error`：Agent 执行链路中的错误（Runner 构造 / run / 事件流出错），
//!   向已建立的 SSE 通道推送同样的两条事件。

use axum::response::{
    IntoResponse,
    sse::{Event as SseEvent, Sse},
};
use std::convert::Infallible;
use tokio::sync::mpsc::Sender;

use super::types::SseEventMsg;

/// 构造「RUN_ERROR + RUN_FINISHED(reason=error)」错误流并返回 Response。
///
/// 用于 Agent 启动前的早期错误出口（助手不存在 / 加载失败 / 存储不可用）。
/// 关键：补发 RUN_FINISHED 让前端正常收尾——历史上助手被删除后命中此分支时只发
/// RUN_ERROR、不发 RUN_FINISHED，导致前端 loading 卡死、会话无法恢复。
pub(super) fn early_error_response(
    thread_id: &str,
    run_id: &str,
    message: impl Into<String>,
) -> axum::response::Response {
    let err_ev = SseEventMsg::RunError {
        message: message.into(),
    };
    let fin_ev = SseEventMsg::RunFinished {
        thread_id: thread_id.to_string(),
        run_id: run_id.to_string(),
        reason: "error".to_string(),
    };
    let stream = futures::stream::iter([
        Ok::<_, Infallible>(SseEvent::default().data(err_ev.to_sse_data())),
        Ok(SseEvent::default().data(fin_ev.to_sse_data())),
    ]);
    Sse::new(stream)
        .keep_alive(
            axum::response::sse::KeepAlive::new().interval(std::time::Duration::from_secs(5)),
        )
        .into_response()
}

/// 向 SSE 通道推送 `RunError` + `RunFinished(reason="error")` 两条事件 —— Agent 执行
/// 链路的三处错误出口（Runner 构造失败 / Runner.run 失败 / 事件流出错）共用此封装。
/// 调用方负责随后 `return` 终止 spawn 任务。
pub(super) async fn send_run_error(
    tx: &Sender<SseEvent>,
    thread_id: &str,
    run_id: &str,
    message: impl Into<String>,
) {
    let _ = tx
        .send(
            SseEvent::default().data(
                SseEventMsg::RunError {
                    message: message.into(),
                }
                .to_sse_data(),
            ),
        )
        .await;
    let _ = tx
        .send(
            SseEvent::default().data(
                SseEventMsg::RunFinished {
                    thread_id: thread_id.to_string(),
                    run_id: run_id.to_string(),
                    reason: "error".to_string(),
                }
                .to_sse_data(),
            ),
        )
        .await;
}
