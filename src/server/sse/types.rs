//! SSE 请求/响应类型与事件消息定义。
//!
//! 收纳传输层 DTO（`RunRequest` / `InputMessage` / `InputAttachment`）与推送给前端的
//! 事件枚举 `SseEventMsg`，供 handler、事件流、子 agent 桥接等子模块共享。

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::tools::code::MentionRef;

/// SSE 对话请求体
#[derive(Debug, Deserialize)]
pub struct RunRequest {
    /// 会话 ID（对应 adk-rust SessionId）
    pub thread_id: String,
    /// 运行 ID（可选，不传则自动生成 UUID）
    pub run_id: Option<String>,
    /// 用户输入消息列表
    pub messages: Vec<InputMessage>,
    /// 工具确认决策（工具名 → approve/deny）
    pub tool_decisions: Option<std::collections::HashMap<String, String>>,
    #[serde(default)]
    pub model_id: Option<String>,
    /// 助手 ID（必填）
    pub assistant_id: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct InputMessage {
    #[allow(dead_code)]
    pub id: String,
    #[allow(dead_code)]
    pub role: String,
    pub content: String,
    /// 用户在输入框 @ 引用的上下文（文件/符号/选区），可选。
    /// 由前端构造，经 `tools::code::render_mentions` 注入为 XML 上下文块。
    #[serde(default)]
    pub mentions: Vec<MentionRef>,
    /// 用户上传的图片附件（多模态输入），可选。
    /// 每个 attachment.url 形如 `data:image/png;base64,...` 或外链 `https://...`，
    /// 会在 handle_run_sse 中转成 adk_rust Part::InlineData / FileData。
    #[serde(default)]
    pub attachments: Vec<InputAttachment>,
}

/// 多模态附件描述符（仅 image，扩展时可加 audio/document 等）
#[derive(Debug, Deserialize, Clone)]
pub struct InputAttachment {
    /// `data:<mime>;base64,...`（本地上传）或 `https://...`（外链）
    pub url: String,
    /// MIME 类型，如 `image/png`
    pub mime_type: String,
}

/// SSE 事件消息枚举 — 序列化为 JSON 推送给前端
///
/// 使用 `#[serde(tag = "type")]` 将枚举变体名序列化为 `type` 字段。
#[derive(Debug, Serialize, Clone)]
#[serde(tag = "type")]
pub enum SseEventMsg {
    #[serde(rename = "RUN_STARTED")]
    RunStarted { thread_id: String, run_id: String },
    #[serde(rename = "TEXT_MESSAGE_START")]
    TextMessageStart { message_id: String },
    #[serde(rename = "TEXT_MESSAGE_CONTENT")]
    TextMessageContent { message_id: String, delta: String },
    #[serde(rename = "TEXT_MESSAGE_END")]
    TextMessageEnd { message_id: String },
    #[serde(rename = "THINKING_MESSAGE_START")]
    ThinkingMessageStart { message_id: String },
    #[serde(rename = "THINKING_MESSAGE_CONTENT")]
    ThinkingMessageContent { message_id: String, delta: String },
    #[serde(rename = "THINKING_MESSAGE_END")]
    ThinkingMessageEnd { message_id: String },
    #[serde(rename = "TOOL_CALL_START")]
    ToolCallStart {
        tool_call_id: String,
        tool_call_name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        server_name: Option<String>,
    },
    #[serde(rename = "TOOL_CALL_ARGS")]
    ToolCallArgs { tool_call_id: String, delta: String },
    #[serde(rename = "TOOL_CALL_END")]
    ToolCallEnd { tool_call_id: String },
    #[serde(rename = "TOOL_CALL_RESULT")]
    ToolCallResult {
        tool_call_id: String,
        #[serde(skip_serializing_if = "String::is_empty")]
        tool_name: String,
        content: String,
    },
    #[serde(rename = "TOOL_CONFIRMATION")]
    ToolConfirmation {
        tool_name: String,
        function_call_id: String,
        args: Value,
    },
    #[serde(rename = "SHELL_APPROVAL_REQUEST")]
    ShellApprovalRequest {
        approval_id: String,
        command: String,
        session_id: String,
    },
    #[serde(rename = "CONTEXT_USAGE")]
    ContextUsage {
        prompt_tokens: u64,
        completion_tokens: u64,
        total_tokens: u64,
        threshold: u64,
    },
    /// 上下文已压缩（L3 intra-turn 压缩检查点）：前端可据此提示「上下文已自动整理」
    #[serde(rename = "CONTEXT_COMPACTED")]
    ContextCompacted {
        /// 本次 run 内累计压缩次数（≥2 前端可提示「建议新建会话」）
        compaction_count: u32,
    },
    #[serde(rename = "RUN_FINISHED")]
    RunFinished {
        thread_id: String,
        run_id: String,
        reason: String,
    },
    #[serde(rename = "RUN_ERROR")]
    RunError { message: String },
    /// 会话工作区产物文件就绪(报表/导出等),前端据此出文件卡片。
    #[serde(rename = "FILE_ARTIFACT")]
    FileArtifact {
        /// 工作区相对路径,如 reports/H3C_x.html(前端拼下载 URL: /api/sessions/{sid}/files/{path})
        path: String,
        /// 文件名(卡片显示 + 下载默认名)
        filename: String,
        /// 标题,如 "H3C CPU内存报表"
        title: String,
        /// MIME,如 text/html
        mime: String,
        /// 字节数
        size: u64,
    },
    /// 子 agent（spawn_agent）的活动事件：前端按 task_name 聚合渲染成「子任务」面板。
    /// kind ∈ started | text | tool_call | tool_result | finished。
    #[serde(rename = "CHILD_AGENT_ACTIVITY")]
    ChildAgentActivity {
        task_name: String,
        kind: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_call_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        delta: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        args: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        ok: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        result: Option<String>,
    },
}

impl SseEventMsg {
    pub fn to_sse_data(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".to_string())
    }
}
