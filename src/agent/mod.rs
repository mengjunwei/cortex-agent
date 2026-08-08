//! Agent 构建模块 — 仅用于自定义助手
//!
//! 主要使用 `build_agent_for_session`，不再提供旧 agent_type 分发功能，所有会话必须通过助手

pub mod assistant_generator;
pub mod custom;
pub mod device_command;
pub mod monitor_plugin;
pub mod query_understanding;
pub mod runtime;

// 自定义助手 + 会话分发器
pub use custom::{AgentContext, AgentRequest, build_agent_for_session, build_custom_agent};

// 为了保持 `build_agent_for_session` 内部能正常调用的子模块，继续保留。
