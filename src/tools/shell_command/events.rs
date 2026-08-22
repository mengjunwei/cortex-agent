//! 工具事件抽象 — 斩断 tools → server 的反向依赖
//!
//! `ToolEventSink` 定义工具需要推送的两类 SSE 事件（文件产物、Shell 审批请求）。
//! 工具层只依赖 trait，SSE 传输层提供实现并注入。

use async_trait::async_trait;

/// 工具事件推送接口，由 SSE 传输层实现。
///
/// 设计目标：tools 不 import `crate::server::sse::SseEventMsg`，事件序列化
/// 在 SSE 层完成（复用 `to_sse_data` 路径），工具层只传结构化数据。
/// 事件 JSON 格式与改前逐字节一致（`to_sse_data` 序列化路径原样复用）。
#[async_trait]
pub trait ToolEventSink: Send + Sync {
    /// 推送文件产物事件（`FILE_ARTIFACT`）。
    async fn send_file_artifact(
        &self,
        path: String,
        filename: String,
        title: String,
        mime: String,
        size: u64,
    );

    /// 推送 Shell 审批请求事件（`SHELL_APPROVAL_REQUEST`）。
    async fn send_approval_request(
        &self,
        approval_id: String,
        command: String,
        session_id: String,
    );
}
