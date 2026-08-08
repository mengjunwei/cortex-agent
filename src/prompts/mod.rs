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
