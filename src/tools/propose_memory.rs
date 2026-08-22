//! `propose_memory` 工具 — 让 LLM 主动提议一条「值得长期记住」的记忆。
//!
//! 常驻注册在每个 custom agent 上（不受 enabled_tools 白名单约束）。
//! 模型识别出用户习惯/偏好或该避开的坑时调用本工具 → 写入 memory_proposals（待确认）→
//! 前端渲染「建议记忆」卡片 → 用户点「加入」转正入 memories / 「忽略」丢弃。
//!
//! 注意：本工具只「提议」，不直接落库为正式记忆——是否记录由用户决定（半自动、可控、透明，
//! 对齐用户偏好的"提示要不要加入记忆"交互）。user_id / session_id 从 ToolContext 取
//! （SSE 已贯通真实登录用户）；assistant_id 由工具工厂在构建 agent 时闭包捕获
//! （scope=assistant 时填入）。

use std::sync::Arc;

use adk_rust::tool::FunctionTool;
use adk_rust::{
    ToolContext,
    serde_json::{Value, json},
};
use schemars::JsonSchema;
use serde::Serialize;

use crate::domain::memory::{MemoryProposalStore, mem_type, scope};

#[derive(Debug, Serialize, JsonSchema)]
struct ProposeMemoryParams {
    /// 记忆类型："preference"=用户习惯/偏好；"pitfall"=该避开的坑
    #[serde(rename = "type")]
    kind: String,
    /// 记忆正文：一句陈述句，将来会原样注入 system prompt。要含足够上下文
    /// （例："用简体中文回复" / "本项目用 PostgreSQL，建表走 migrations/schema.sql"）。
    content: String,
    /// 为什么值得记（一句话理由，会展示在卡片上帮用户判断要不要采纳）
    reason: String,
    /// 作用域："user"=跨所有助手（默认）；"assistant"=仅当前助手。不确定就填 "user"。
    #[serde(default)]
    scope: Option<String>,
}

/// 构造 propose_memory 工具。
///
/// - `store`：记忆建议存储。
/// - `assistant_id`：当前助手 id（scope=assistant 时写入建议的 assistant_id 字段）。
pub fn create_propose_memory_tool(
    store: Arc<MemoryProposalStore>,
    assistant_id: String,
) -> FunctionTool {
    FunctionTool::new(
        "propose_memory",
        "当你识别出一条【值得长期记住】的用户习惯/偏好(preference)或该避开的坑(pitfall)时调用。\
         它会生成一张「建议记忆」卡片，用户确认后才真正记入长期记忆，并在此后每次对话自动带上。\
         仅在确实有价值时调用：信息必须明确、可跨会话复用、对未来对话有帮助；\
         不要为一次性琐事、未确认的猜测、或对方随口一提的内容调用。\
         content 用一句陈述句，reason 说明为什么值得记。",
        move |ctx: Arc<dyn ToolContext>, args: Value| {
            let store = store.clone();
            let assistant_id = assistant_id.clone();
            async move {
                let user_id = ctx.user_id().to_string();
                let session_id = ctx.session_id().to_string();

                let content = match args["content"].as_str() {
                    Some(s) if !s.trim().is_empty() => s.trim().to_string(),
                    _ => return Ok(json!({ "ok": false, "message": "content 不能为空" })),
                };
                let reason = args["reason"].as_str().unwrap_or("").trim().to_string();
                let kind = args["type"].as_str().unwrap_or("preference").to_lowercase();
                let mt = if kind == "pitfall" {
                    mem_type::PITFALL
                } else {
                    mem_type::PREFERENCE
                };
                let scope_val = args["scope"].as_str().unwrap_or("user").to_lowercase();
                let (scope_v, assistant_opt) = if scope_val == "assistant" {
                    (scope::ASSISTANT, Some(assistant_id.as_str()))
                } else {
                    (scope::USER, None)
                };

                match store
                    .create(
                        &user_id,
                        &session_id,
                        assistant_opt,
                        scope_v,
                        mt,
                        &content,
                        &reason,
                    )
                    .await
                {
                    Ok(p) => Ok(json!({
                        "ok": true,
                        "proposal_id": p.id,
                        "message": format!("已生成建议记忆卡片，等待用户确认：{content}"),
                    })),
                    Err(e) => Ok(json!({ "ok": false, "message": format!("保存建议失败: {e}") })),
                }
            }
        },
    )
    .with_parameters_schema::<ProposeMemoryParams>()
}
