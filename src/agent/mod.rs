//! Agent 构建模块 — 内置/自定义助手统一构建
//!
//! 主要使用 `build_agent_for_session`，所有会话（含内置助手）统一走 `build_custom_agent`。

pub mod assistant_generator;
pub mod builder;
pub mod cortex;
pub mod query_understanding;
pub mod workspace;

// 自定义助手 + 会话分发器
pub use builder::{AgentContext, AgentRequest, build_agent_for_session, build_custom_agent};
