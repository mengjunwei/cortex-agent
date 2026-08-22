//! `CortexAgent` 运行时主循环（adk `Agent` trait 实现）。
//!
//! 对外只导出 [`CortexAgent`] 与 [`CortexAgentBuilder`]（路径不变）：
//! `crate::agent::cortex::{CortexAgent, CortexAgentBuilder}`。
//!
//! 实现按职责拆到子模块：
//! - [`run`]:adk `Agent` trait 实现（`run` 主循环 + `RunEndGuard`）
//! - [`builder`]：`CortexAgent` / `CortexAgentBuilder` 字段定义与链式装配
//! - [`prompt`]：system prompt 分层构建（stable 前缀 / volatile 段 / skill 正文 preamble）
//! - [`compaction`]：上下文压缩（LLM 摘要）
//! - [`thinking`]：思考参数（thinking/effort/reasoning_effort）兜底重试
//! - [`llm_call`]：带指数退避的 LLM 调用 + 纯文本收尾事件
//! - [`tool_exec`]：单工具超时/panic 防护 + 工具上下文 `ToolCtx`

mod analytics;
mod builder;
mod compaction;
mod context_tool;
pub(crate) mod env_probe;
mod hook;
mod llm_call;
mod multi_agent;
mod prompt;
mod role;
mod run;
mod soft_landing;
mod thinking;
mod tool_exec;
mod trim;
mod window;

pub use builder::{CortexAgent, CortexAgentBuilder};

use std::sync::Arc;

use adk_rust::serde_json::json;
use adk_rust::{Content, FunctionResponseData, Part};

#[cfg(test)]
use tokio_util::sync::CancellationToken;

/// 上下文预算只读句柄（crate 内 SSE 层轮询推 token 用量）
pub(crate) use context_tool::SharedBudget;
pub use multi_agent::{ChildAgentEvent, ChildEventSink, ChildUsageTotal};
/// 会话级软着陆窗口状态（server 层按 thread_id 维护、经 builder 注入 root agent）
pub use window::{SharedWindowState, WindowStateSnapshot};

/// CortexAgent 额外方法（非 Agent trait）：暴露预算只读句柄。
#[allow(private_interfaces)]
impl CortexAgent {
    /// 上下文预算只读句柄（effective_tokens / context_window / 窗口号）。
    /// SSE 层在 run 期间轮询，向前端推 token 用量（对齐 codex token 显示）。
    pub fn budget(&self) -> SharedBudget {
        Arc::clone(&self.budget_handle)
    }

    /// 子 agent token 用量累加器只读句柄。SSE 层在 run 期间轮询，随 CONTEXT_USAGE 上报
    /// （父 agent 自身用量走主事件流，与此计数不相交 → 不双重计数；详见类型文档）。
    pub fn child_usage_total(&self) -> multi_agent::ChildUsageTotal {
        Arc::clone(&self.child_usage_total)
    }
}
/// 多智能体模式推导（对齐 codex effective_multi_agent_mode 的 effort 分支）。
///
/// - Explicit（默认）：仅用户明确要求才 spawn
/// - Proactive：主动委派
/// - Auto：thinking level = max → Proactive，否则 Explicit
///
/// 返回 None = 该 agent 不注入模式提示（禁用时无工具也无提示）。
pub(crate) fn multi_agent_mode_hint(
    mode: crate::config::MultiAgentModeConfig,
    thinking_level: Option<&str>,
) -> Option<&'static str> {
    let proactive = match mode {
        crate::config::MultiAgentModeConfig::Proactive => true,
        crate::config::MultiAgentModeConfig::Explicit => false,
        crate::config::MultiAgentModeConfig::Auto => {
            matches!(thinking_level, Some("max") | Some("ultra"))
        }
    };
    Some(if proactive {
        crate::prompts::MULTI_AGENT_MODE_PROACTIVE
    } else {
        crate::prompts::MULTI_AGENT_MODE_EXPLICIT
    })
}

/// prompt 测试辅助：显式模式的提示文本（供 build_stable_prefix 测试调用）。
#[cfg(test)]
pub(crate) fn multi_agent_mode_hint_for_test(
    mode: crate::config::MultiAgentModeConfig,
) -> Option<&'static str> {
    multi_agent_mode_hint(mode, None)
}

/// 按字符粗估一段对话的 token 数（无真实 usage 时的兜底）：Text/Thinking/FunctionResponse
/// 按字节计入；FunctionCall 的 args 按序列化字节数计入（工具密集会话里 FC args 承载
/// 整个文件内容/长参数，固定记 64 会把占大头的上下文估没了——软/硬闸因此永远够不着）；
/// 其余 part 记 64。总和除以 chars_per_token。与压缩分支的 after_tokens 估算同源。
pub(crate) fn estimate_conv_tokens(conv: &[Content], chars_per_token: usize) -> usize {
    conv.iter()
        .map(|c| {
            c.parts
                .iter()
                .map(|p| match p {
                    Part::Text { text } => text.len(),
                    Part::Thinking { thinking, .. } => thinking.len(),
                    Part::FunctionResponse {
                        function_response, ..
                    } => function_response.response.to_string().len(),
                    Part::FunctionCall { args, .. } => 64 + args.to_string().len(),
                    _ => 64,
                })
                .sum::<usize>()
        })
        .sum::<usize>()
        / chars_per_token.max(1)
}

/// 请求级 FunctionCall/FunctionResponse 配对归一化（对标 codex
/// `context_manager::normalize` 的 `ensure_call_outputs_present` + `remove_orphan_outputs`）。
///
/// 压缩切点、超窗删条、回滚等操作可能破坏 FC/FR 配对，导致发给 API 的历史出现「孤立
/// FunctionResponse（无对应 FunctionCall）」或「孤立 FunctionCall（无对应 FunctionResponse）」，
/// 触发 Anthropic/OpenAI 严格模式 400。本函数在每次发请求前就地清理 `conv`：
///
/// 1. **删孤立 FunctionResponse**：id 不在任何 FunctionCall 中的 FR（含空 id 的回填兜底）直接删除；
///    删空后若 function-role 消息无 parts，整条移除。
/// 2. **补孤立 FunctionCall**：id 不在任何 FunctionResponse 中的 FC，在其所在 model 消息之后
///    插入一条占位 FunctionResponse（标注 aborted），让配对闭合。
///
/// 仅在请求边界调用，不改 `conv` 的语义结构。
pub(crate) fn normalize_function_pairs(conv: &mut Vec<Content>) {
    use std::collections::HashSet;

    // 第一遍：收集所有非空 FunctionCall id
    let mut call_ids: HashSet<String> = HashSet::new();
    for c in conv.iter() {
        for p in &c.parts {
            if let Part::FunctionCall { id, .. } = p {
                if let Some(id) = id.as_ref().filter(|s| !s.is_empty()) {
                    call_ids.insert(id.clone());
                }
            }
        }
    }

    // 第二遍：删除孤立的 FunctionResponse，并记录已配对的 response id
    let mut matched_resp_ids: HashSet<String> = HashSet::new();
    for c in conv.iter_mut() {
        let mut orphan: Vec<usize> = Vec::new();
        for (i, p) in c.parts.iter().enumerate() {
            if let Part::FunctionResponse { id, .. } = p {
                let rid = id.as_deref().unwrap_or("");
                if rid.is_empty() || !call_ids.contains(rid) {
                    orphan.push(i); // 空 id 或无对应 FC → 删
                } else {
                    matched_resp_ids.insert(rid.to_string());
                }
            }
        }
        for i in orphan.into_iter().rev() {
            c.parts.remove(i);
        }
    }
    // 删除因孤立 FR 清空后的 function-role 消息
    conv.retain(|c| !(c.role == "function" && c.parts.is_empty()));

    // 第三遍：为孤立的 FunctionCall 补占位 FunctionResponse。
    // 空 id FC 先回写一个全局唯一合成 id 到本体——序列化端（llm/openai/compat 与 anthropic_custom）
    // 对 None id 的兜底是 `call_{name}`，若不回写，同消息里两个不同名的空 id FC 会各自生成
    // `call_shell`/`call_read`，而这里只用单个 `call_{name}` 占位，wire 层 mismatch 触发 400。
    // 回写后 FC 有了稳定 id，占位 FR 用同一个 id 即可配成对。
    let mut inserts: Vec<(usize, Content)> = Vec::new();
    for (i, c) in conv.iter_mut().enumerate() {
        if c.role != "model" {
            continue;
        }
        for p in c.parts.iter_mut() {
            if let Part::FunctionCall { name, id, .. } = p {
                // 空 id 回写全局唯一合成 id；有 id 且已配对则跳过
                let cid = match id.as_deref() {
                    Some(s) if !s.is_empty() => {
                        if matched_resp_ids.contains(s) {
                            continue;
                        }
                        s.to_string()
                    }
                    _ => {
                        let synth = crate::llm::next_synthetic_call_id();
                        *id = Some(synth.clone());
                        synth
                    }
                };
                let placeholder = Content {
                    role: "function".to_string(),
                    parts: vec![Part::FunctionResponse {
                        function_response: FunctionResponseData::new(
                            name.clone(),
                            json!({ "error": "[aborted: tool result missing after context compaction]" }),
                        ),
                        id: Some(cid),
                        annotations: None,
                    }],
                };
                inserts.push((i + 1, placeholder));
            }
        }
    }
    // 从后往前插入，保持下标稳定；同一 model 消息多个 FC 按原序排列。
    for (pos, content) in inserts.into_iter().rev() {
        conv.insert(pos, content);
    }
}

#[cfg(test)]
mod tests;

