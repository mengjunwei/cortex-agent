//! V2 模型侧工具：spawn_agent / send_message / followup_task / wait_agent /
//! interrupt_agent / list_agents（schema 与描述对齐 codex multi_agents_v2）。

use std::sync::Arc;
use std::time::Duration;

use adk_rust::async_trait;
use adk_rust::serde_json::{json, Value};
use adk_rust::{Result, Tool, ToolContext};

use super::super::role;
use super::factory::{ChildAgentFactory, SpawnRequest};
use super::fork::parse_fork_turns;

pub(crate) const SPAWN_AGENT_TOOL_NAME: &str = "spawn_agent";
pub(crate) const WAIT_AGENT_TOOL_NAME: &str = "wait_agent";
pub(crate) const SEND_MESSAGE_TOOL_NAME: &str = "send_message";
pub(crate) const FOLLOWUP_TASK_TOOL_NAME: &str = "followup_task";
pub(crate) const INTERRUPT_AGENT_TOOL_NAME: &str = "interrupt_agent";
pub(crate) const LIST_AGENTS_TOOL_NAME: &str = "list_agents";

/// wait_agent 超时（毫秒）钳制边界（对齐 codex multi_agent_v2 默认值）。
pub(crate) const WAIT_DEFAULT_TIMEOUT_MS: i64 = 30_000;
pub(crate) const WAIT_MIN_TIMEOUT_MS: i64 = 10_000;
pub(crate) const WAIT_MAX_TIMEOUT_MS: i64 = 3_600_000;

// ============================================================================
// spawn_agent 工具（V2 schema）
// ============================================================================

const SPAWN_DESC_TEMPLATE: &str = r#"Spawns an agent to work on the specified task. If your current task is `/root/task1` and you spawn_agent with task_name "task_3" the agent will have canonical task name `/root/task1/task_3`.
You are then able to refer to this agent as `task_3` or `/root/task1/task_3` interchangeably. However an agent `/root/task2/task_3` would only be able to communicate with this agent via its canonical name `/root/task1/task_3`.
The spawned agent will have the same tools as you and the ability to spawn its own subagents.
Only call this tool for a concrete, bounded subtask that can run independently alongside useful local work; otherwise continue locally.
It will be able to send you and other running agents messages, and its final answer will be provided to you when it finishes.
The new agent's canonical task name will be provided to it along with the message.

Note that passing `fork_turns="none"` will not pass any surrounding context to the spawned subagent, which may cause the agent to lack the context it needs to complete its task, whereas `fork_turns="all"` will provide the subagent with all surrounding context."#;

pub(crate) struct SpawnAgentTool {
    factory: Arc<ChildAgentFactory>,
}

impl SpawnAgentTool {
    pub(crate) fn new(factory: Arc<ChildAgentFactory>) -> Self {
        Self { factory }
    }
}

#[async_trait]
impl Tool for SpawnAgentTool {
    fn name(&self) -> &str {
        SPAWN_AGENT_TOOL_NAME
    }
    fn description(&self) -> &str {
        SPAWN_DESC_TEMPLATE
    }
    fn parameters_schema(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": {
                "message": {
                    "type": "string",
                    "description": "Initial plain-text task for the new agent."
                },
                "task_name": {
                    "type": "string",
                    "description": "Task name for the new agent. Use lowercase letters, digits, and underscores."
                },
                "agent_type": {
                    "type": "string",
                    "description": format!("Agent type override for the new agent. Omit unless explicitly asked. The selected role applies regardless of how much parent history is inherited.\n{}",
                        role::agent_type_description(&self.factory.agents_cfg.roles))
                },
                "fork_turns": {
                    "type": "string",
                    "description": "Optional number of turns to fork. Defaults to `all`. Use `none`, `all`, or a positive integer string such as `3` to fork only the most recent turns."
                },
                "model": {
                    "type": "string",
                    "description": "Model override for the new agent. Omit unless an explicit override is needed."
                }
            },
            "required": ["task_name", "message"]
        }))
    }
    async fn execute(&self, _ctx: Arc<dyn ToolContext>, args: Value) -> Result<Value> {
        let task_name = args
            .get("task_name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let message = args
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let agent_type = args
            .get("agent_type")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from);
        let model_id = args
            .get("model")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from);
        let fork_raw = args
            .get("fork_turns")
            .and_then(|v| v.as_str())
            .map(str::trim);
        if task_name.is_empty() || message.is_empty() {
            return Ok(json!({ "error": "Both 'task_name' and 'message' are required." }));
        }
        let fork_mode = match parse_fork_turns(fork_raw) {
            Ok(m) => m,
            Err(e) => return Ok(json!({ "error": e })),
        };
        // model 覆盖解析（对齐 codex：args > default_subagent_model > 继承父）
        let model_override = match self.factory.resolve_model_override(model_id.as_deref()) {
            Ok(m) => m,
            Err(e) => return Ok(json!({ "error": e })),
        };
        // fork 输入：主循环 conv 增量快照（每轮刷新；spawn 即取当前值）
        let current_conv_tail = self
            .factory
            .conv_snapshot
            .lock()
            .expect("conv snapshot poisoned")
            .clone();
        let req = SpawnRequest {
            task_name: task_name.clone(),
            message,
            agent_type,
            model_override,
            fork_mode,
            current_conv_tail,
        };
        match self.factory.spawn(req).await {
            Ok((canonical, nickname)) => Ok(json!({
                "task_name": canonical,
                "nickname": nickname,
            })),
            Err(e) => Ok(json!({ "error": e })),
        }
    }
}

// ============================================================================
// send_message / followup_task 工具（同一路径，仅 trigger_turn 差异）
// ============================================================================

pub(crate) struct SendMessageTool {
    factory: Arc<ChildAgentFactory>,
    trigger_turn: bool,
}

impl SendMessageTool {
    pub(crate) fn new(factory: Arc<ChildAgentFactory>, trigger_turn: bool) -> Self {
        Self {
            factory,
            trigger_turn,
        }
    }
}

#[async_trait]
impl Tool for SendMessageTool {
    fn name(&self) -> &str {
        if self.trigger_turn {
            FOLLOWUP_TASK_TOOL_NAME
        } else {
            SEND_MESSAGE_TOOL_NAME
        }
    }
    fn description(&self) -> &str {
        if self.trigger_turn {
            "Send a follow-up task to an existing non-root target agent and trigger a turn if it is idle. If the target is already running, deliver the task promptly at message boundaries while sampling, or after the pending tool call completes."
        } else {
            "Send a message to an existing agent. The message will be delivered promptly. Does not trigger a new turn."
        }
    }
    fn parameters_schema(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": {
                "target": {
                    "type": "string",
                    "description": if self.trigger_turn {
                        "Agent id or canonical task name to send a follow-up task to (from spawn_agent)."
                    } else {
                        "Relative or canonical task name to message (from spawn_agent)."
                    }
                },
                "message": {
                    "type": "string",
                    "description": if self.trigger_turn {
                        "Message text to send to the target agent."
                    } else {
                        "Message text to queue on the target agent."
                    }
                }
            },
            "required": ["target", "message"]
        }))
    }
    async fn execute(&self, _ctx: Arc<dyn ToolContext>, args: Value) -> Result<Value> {
        let target = args
            .get("target")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let message = args
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if target.is_empty() {
            return Ok(json!({ "error": "'target' is required." }));
        }
        match self
            .factory
            .send_message(&target, &message, self.trigger_turn)
        {
            Ok(()) => Ok(json!({ "ok": true })),
            Err(e) => Ok(json!({ "error": e })),
        }
    }
}

// ============================================================================
// wait_agent 工具（V2：等任意 mailbox 活动，不返回内容）
// ============================================================================

/// 钳制 wait 超时（对齐 codex wait.rs 语义：>max 报错、<min 钳到 min、缺省 default）。
pub(crate) fn clamp_wait_timeout_ms(requested: Option<i64>) -> std::result::Result<i64, String> {
    match requested {
        Some(ms) if ms > WAIT_MAX_TIMEOUT_MS => {
            Err(format!("timeout_ms must be at most {WAIT_MAX_TIMEOUT_MS}"))
        }
        Some(ms) => Ok(ms.max(WAIT_MIN_TIMEOUT_MS)),
        None => Ok(WAIT_DEFAULT_TIMEOUT_MS),
    }
}

pub(crate) struct WaitAgentTool {
    factory: Arc<ChildAgentFactory>,
}

impl WaitAgentTool {
    pub(crate) fn new(factory: Arc<ChildAgentFactory>) -> Self {
        Self { factory }
    }
}

#[async_trait]
impl Tool for WaitAgentTool {
    fn name(&self) -> &str {
        WAIT_AGENT_TOOL_NAME
    }
    fn description(&self) -> &str {
        "Wait for a mailbox update from any live agent, including queued messages and final-status notifications. Does not return the content; returns either a summary of which agents have updates (if any) or a timeout summary if no activity arrives before the deadline. When the wait ends, drain pending agent messages from your conversation before continuing."
    }
    fn is_read_only(&self) -> bool {
        true
    }
    fn parameters_schema(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": {
                "timeout_ms": {
                    "type": "integer",
                    "description": format!("Timeout in milliseconds. Defaults to {WAIT_DEFAULT_TIMEOUT_MS}, min {WAIT_MIN_TIMEOUT_MS}, max {WAIT_MAX_TIMEOUT_MS}.")
                }
            }
        }))
    }
    async fn execute(&self, _ctx: Arc<dyn ToolContext>, args: Value) -> Result<Value> {
        let requested = args.get("timeout_ms").and_then(|v| v.as_i64());
        let timeout_ms = match clamp_wait_timeout_ms(requested) {
            Ok(t) => t,
            Err(e) => return Ok(json!({ "error": e })),
        };
        // 双重钳制：wait 还必须严格小于外层工具超时（tool_exec 硬杀），留 10s 余量。
        // cortex tool_timeout 默认 300s > max_wait 3600s 的场景下，以 min(两者) 为准。
        let outer_ms = self.factory.blueprint.tool_timeout.as_secs() as i64;
        // 外层余量钳制（codex 无此层：cortex 的 tool_exec 会硬杀超时工具，wait 内部
        // deadline 必须严格小于外层，否则外层先到点回 "Tool timed out" 丢结果）。
        let outer_cap_ms = if outer_ms > 10 {
            outer_ms.saturating_sub(10) * 1000
        } else {
            outer_ms.saturating_sub(1).max(0) * 1000
        };
        let effective_ms = timeout_ms.min(outer_cap_ms).max(0);

        let mut rx = self.factory.tree.subscribe_activity();
        let deadline =
            tokio::time::Instant::now() + Duration::from_millis(effective_ms.max(0) as u64);
        // 「已有 pending」即时检测（对齐 codex pending_activity）：投递早于本次订阅的
        // 消息不触发 watch changed，必须显式查——自己的 mailbox（root）或全树 inbox
        // 任一非空即立即返回，不等满 timeout。
        let already_pending = self
            .factory
            .self_mailbox
            .as_ref()
            .map(|mb| !mb.is_empty())
            .unwrap_or_else(|| self.factory.tree.pending_mail_count() > 0);
        let outcome = if already_pending {
            true
        } else {
            matches!(
                tokio::time::timeout_at(deadline, rx.changed()).await,
                Ok(Ok(_))
            )
        };
        let mut message = if outcome {
            "Wait completed.".to_string()
        } else {
            "Wait timed out.".to_string()
        };
        // 钳制附注（对齐 codex 文案；注明钳制来源，避免模型误判全局下限）。
        // 双钳制叠加（req < MIN 且外层预算把 effective 压到 < MIN）时统一用预算文案，
        // 不说 "minimum"（消除旧两分支都不命中的盲区）。
        if let Some(req) = requested
            && req != effective_ms
        {
            if effective_ms >= WAIT_MIN_TIMEOUT_MS && req < WAIT_MIN_TIMEOUT_MS {
                message.push_str(&format!(
                    "\n\nRequested timeout of {req}ms was clamped to the minimum of {effective_ms}ms."
                ));
            } else if req > effective_ms {
                message.push_str(&format!(
                    "\n\nRequested timeout of {req}ms was clamped to {effective_ms}ms to stay within the tool timeout budget."
                ));
            }
        }
        Ok(json!({
            "message": message,
            "timed_out": !outcome,
        }))
    }
}

// ============================================================================
// interrupt_agent 工具
// ============================================================================

pub(crate) struct InterruptAgentTool {
    factory: Arc<ChildAgentFactory>,
}

impl InterruptAgentTool {
    pub(crate) fn new(factory: Arc<ChildAgentFactory>) -> Self {
        Self { factory }
    }
}

#[async_trait]
impl Tool for InterruptAgentTool {
    fn name(&self) -> &str {
        INTERRUPT_AGENT_TOOL_NAME
    }
    fn description(&self) -> &str {
        "Interrupt an agent's current turn, if any, and return its previous status. The agent remains available for messages and follow-up tasks."
    }
    fn parameters_schema(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": {
                "target": {
                    "type": "string",
                    "description": "Agent id or canonical task name to interrupt (from spawn_agent)."
                }
            },
            "required": ["target"]
        }))
    }
    async fn execute(&self, _ctx: Arc<dyn ToolContext>, args: Value) -> Result<Value> {
        let target = args
            .get("target")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if target.is_empty() {
            return Ok(json!({ "error": "'target' is required." }));
        }
        match self.factory.interrupt(&target) {
            Ok(v) => Ok(v),
            Err(e) => Ok(json!({ "error": e })),
        }
    }
}

// ============================================================================
// list_agents 工具
// ============================================================================

pub(crate) struct ListAgentsTool {
    factory: Arc<ChildAgentFactory>,
}

impl ListAgentsTool {
    pub(crate) fn new(factory: Arc<ChildAgentFactory>) -> Self {
        Self { factory }
    }
}

#[async_trait]
impl Tool for ListAgentsTool {
    fn name(&self) -> &str {
        LIST_AGENTS_TOOL_NAME
    }
    fn description(&self) -> &str {
        "List live agents in the current root thread tree."
    }
    fn is_read_only(&self) -> bool {
        true
    }
    fn parameters_schema(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": {
                "path_prefix": {
                    "type": "string",
                    "description": "Task-path prefix filter without a trailing slash. Omit to list all live agents."
                }
            }
        }))
    }
    async fn execute(&self, _ctx: Arc<dyn ToolContext>, args: Value) -> Result<Value> {
        let prefix = args
            .get("path_prefix")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from);
        let mut result = self.factory.list_agents();
        if let Some(p) = prefix {
            let resolved = self.factory.resolve_target(&p);
            let agents = result
                .get("agents")
                .and_then(|a| a.as_array())
                .cloned()
                .unwrap_or_default();
            let filtered: Vec<Value> = agents
                .into_iter()
                .filter(|a| {
                    a.get("agent_name")
                        .and_then(|n| n.as_str())
                        .map(|name| name == resolved || name.starts_with(&format!("{resolved}/")))
                        .unwrap_or(false)
                })
                .collect();
            result = json!({ "agents": filtered });
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wait_timeout_clamping() {
        // >max 报错
        assert!(clamp_wait_timeout_ms(Some(3_600_001)).is_err());
        // <min 钳到 min
        assert_eq!(
            clamp_wait_timeout_ms(Some(100)).unwrap(),
            WAIT_MIN_TIMEOUT_MS
        );
        // 缺省 default
        assert_eq!(
            clamp_wait_timeout_ms(None).unwrap(),
            WAIT_DEFAULT_TIMEOUT_MS
        );
        // 正常值透传
        assert_eq!(clamp_wait_timeout_ms(Some(60_000)).unwrap(), 60_000);
    }
}
