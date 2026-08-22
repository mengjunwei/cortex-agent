//! 子 agent 运行状态（对齐 codex AgentStatus）。

use adk_rust::serde_json::{json, Value};

use super::envelope::truncate_middle_tokens;

/// FINAL_ANSWER 错误 payload 截断预算（对齐 codex COMPLETION_MESSAGE_MAX_TOKENS 体系：
/// 总 1000 token、信封预留 100、错误文本 900；正常完成 payload 不截断）。
const ERROR_PAYLOAD_MAX_TOKENS: usize = 900;

/// 子 agent 的运行状态。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ChildStatus {
    /// 尚未开始首个 turn（spawn 完成、循环未跑）
    PendingInit,
    Running,
    Completed(Option<String>),
    Errored(String),
    /// 被 interrupt_agent 打断（agent 仍可接收新任务）
    Interrupted,
}

impl ChildStatus {
    pub(crate) fn is_terminal(&self) -> bool {
        matches!(
            self,
            ChildStatus::Completed(_) | ChildStatus::Errored(_) | ChildStatus::Interrupted
        )
    }
    /// 是否有可投递的 FINAL_ANSWER payload（对齐 codex format_inter_agent_completion_message：
    /// PendingInit/Running/Interrupted **均不回传**——打断≠完成，投递噪音信封会诱导父追问）。
    pub(crate) fn completion_payload(&self) -> Option<String> {
        match self {
            ChildStatus::Completed(Some(msg)) => Some(msg.clone()),
            ChildStatus::Completed(None) => Some(String::new()),
            ChildStatus::Errored(e) => {
                let truncated = truncate_middle_tokens(e, ERROR_PAYLOAD_MAX_TOKENS);
                Some(format!(
                    "Agent errored: {truncated}\n\nThis agent's turn failed. If you still need this agent, use the available collaboration tools to give it another task."
                ))
            }
            ChildStatus::Interrupted | ChildStatus::PendingInit | ChildStatus::Running => None,
        }
    }
    /// list_agents / interrupt_agent 的状态值（对齐 codex oneOf schema：completed 带文本）。
    pub(super) fn status_value(&self) -> Value {
        match self {
            ChildStatus::Completed(Some(msg)) => json!({ "completed": msg }),
            ChildStatus::Completed(None) => json!({ "completed": null }),
            ChildStatus::Errored(e) => json!({ "errored": e }),
            ChildStatus::PendingInit => json!("pending_init"),
            ChildStatus::Running => json!("running"),
            ChildStatus::Interrupted => json!("interrupted"),
        }
    }
    #[allow(dead_code)]
    pub(crate) fn label(&self) -> &'static str {
        match self {
            ChildStatus::PendingInit => "pending_init",
            ChildStatus::Running => "running",
            ChildStatus::Completed(_) => "completed",
            ChildStatus::Errored(_) => "errored",
            ChildStatus::Interrupted => "interrupted",
        }
    }
}

/// 计算 child 深度并校验是否超过上限（防失控递归）。纯函数，便于测试。
pub(crate) fn validate_spawn_depth(depth: u32, max_depth: u32) -> std::result::Result<u32, String> {
    let child_depth = depth.saturating_add(1);
    if child_depth > max_depth {
        Err(format!(
            "Spawn depth limit ({}) reached. Solve the task yourself instead of spawning.",
            max_depth
        ))
    } else {
        Ok(child_depth)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn child_status_completion_payload() {
        // Completed 带文本：原文
        assert_eq!(
            ChildStatus::Completed(Some("answer".into())).completion_payload(),
            Some("answer".to_string())
        );
        // Errored：截断 + 后续建议（对齐 codex 格式）
        let p = ChildStatus::Errored("boom".into())
            .completion_payload()
            .unwrap();
        assert!(p.starts_with("Agent errored: boom"));
        assert!(p.contains("use the available collaboration tools"));
        // Running/PendingInit：不回传
        assert!(ChildStatus::Running.completion_payload().is_none());
        assert!(ChildStatus::PendingInit.completion_payload().is_none());
    }
}
