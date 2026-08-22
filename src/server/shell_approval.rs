//! Shell 命令审批 — REST handler + re-export
//!
//! 审批注册表已移至 `tools::shell_approval`（领域归属），此处保留 re-export
//! 以最小化 server/mod.rs 的改动面。

// Re-export 审批注册表与决策类型（从 tools 层）
pub use crate::tools::shell_command::approval::{ApprovalDecision, ShellApprovalRegistry};
