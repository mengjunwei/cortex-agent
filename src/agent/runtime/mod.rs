//! Agent 运行时基础组件（与具体业务 agent 分层）
//!
//! 这里放的是 agent 运行时的**基础组件**，被具体业务 agent（device_command /
//! monitor_plugin / custom 等）复用，本身不属于某个具体助手：
//!
//! - [`cortex_agent`]：`CortexAgent`（adk `Agent` trait 实现 + system prompt 分层构建）
//! - [`workspace`]：`WorkspaceMode`（沙箱编排模式）

pub mod cortex_agent;
pub mod workspace;
