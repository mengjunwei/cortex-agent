// base
pub const BASE_INSTRUCTION: &str = include_str!("templates/base_instruction.md");

// apply_patch
pub const APPLY_PATCH_TOOL_INSTRUCTIONS: &str =
    include_str!("templates/apply_patch_tool_instructions.md");

// compact
pub const COMPACT_PROMPT: &str = include_str!("templates/compact/prompt.md");
pub const COMPACT_SUMMARY_PREFIX: &str = include_str!("templates/compact/summary_prefix.md");

// goals
pub const GOAL_CONTINUATION: &str = include_str!("templates/goals/continuation.md");
pub const GOAL_BUDGET_LIMIT: &str = include_str!("templates/goals/budget_limit.md");
pub const GOAL_OBJECTIVE_UPDATED: &str = include_str!("templates/goals/objective_updated.md");

// permissions - approval policy
pub const APPROVAL_NEVER: &str = include_str!("templates/permissions/approval_policy/never.md");
pub const APPROVAL_ON_REQUEST: &str =
    include_str!("templates/permissions/approval_policy/on_request.md");
pub const APPROVAL_ON_REQUEST_RULE_REQUEST_PERMISSION: &str =
    include_str!("templates/permissions/approval_policy/on_request_rule_request_permission.md");
pub const APPROVAL_UNLESS_TRUSTED: &str =
    include_str!("templates/permissions/approval_policy/unless_trusted.md");
pub const APPROVAL_AUTO: &str = include_str!("templates/permissions/approval_policy/auto.md");

// permissions - sandbox mode
pub const SANDBOX_DANGER_FULL_ACCESS: &str =
    include_str!("templates/permissions/sandbox_mode/danger_full_access.md");
pub const SANDBOX_READ_ONLY: &str = include_str!("templates/permissions/sandbox_mode/read_only.md");
pub const SANDBOX_WORKSPACE_WRITE: &str =
    include_str!("templates/permissions/sandbox_mode/workspace_write.md");

// realtime
pub const REALTIME_BACKEND_PROMPT: &str = include_str!("templates/realtime/backend_prompt.md");
pub const REALTIME_START: &str = include_str!("templates/realtime/realtime_start.md");
pub const REALTIME_END: &str = include_str!("templates/realtime/realtime_end.md");

// review
pub const REVIEW_RUBRIC: &str = include_str!("templates/review/rubric.md");
pub const REVIEW_EXIT_SUCCESS: &str = include_str!("templates/review/exit_success.xml");
pub const REVIEW_EXIT_INTERRUPTED: &str = include_str!("templates/review/exit_interrupted.xml");

// skills catalog header (cortex-specific, replaces codex's core-skills render.rs)
pub const SKILLS_CATALOG_HEADER: &str = include_str!("templates/skills_catalog.md");

// multi-agent V2（对齐 codex multi_agents_v2 提示词）
/// 模式提示：仅显式要求（对齐 codex EXPLICIT_REQUEST_ONLY_MULTI_AGENT_MODE_TEXT 原文）
pub const MULTI_AGENT_MODE_EXPLICIT: &str = "Any earlier instruction enabling proactive multi-agent delegation no longer applies. Do not spawn sub-agents unless the user or applicable instructions explicitly ask for sub-agents, delegation, or parallel agent work. Requests for depth, thoroughness, research, investigation, or detailed codebase analysis do not count as permission to spawn.";
/// 模式提示：主动委派（对齐 codex PROACTIVE_MULTI_AGENT_MODE_TEXT 原文）
pub const MULTI_AGENT_MODE_PROACTIVE: &str = "Proactive multi-agent delegation is active. Any earlier instruction requiring an explicit user request before spawning sub-agents no longer applies. Use sub-agents when parallel work would materially improve speed or quality. This mode remains active until a later multi-agent mode message changes it.";
