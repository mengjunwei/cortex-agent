//! MCP（Model Context Protocol）Server 管理模块
//!
//! 提供运行时动态接入外部 MCP Server 的能力，扩展 Agent 工具集。
//!
//! 分层（架构 §2.3 / §3）：
//! - [`enums`]：`TransportKind`（stdio/streamable_http）+ 复用 `Status`
//! - [`models`]：领域实体 `McpServer` / 运行时状态 `ServerHealth`
//! - [`dto`]：请求/响应 DTO（敏感字段脱敏）
//! - [`store`]：DB CRUD（AES-256-GCM 加密 env/headers）
//! - [`transport`]：rmcp 传输适配（封装 stdio/http 连接细节）
//! - [`manager`]：`McpManager`（连接池 + 健康探测 + 业务编排）
//!
//! 详见 [docs/design/mcp-management.md](../../../docs/design/mcp-management.md)

pub mod dto;
pub mod enums;
pub mod manager;
pub mod models;
pub mod store;
pub mod transport;

pub use enums::{Status, TransportKind};
pub use manager::McpManager;
pub use models::{McpServer, McpToolInfo, ServerHealth};
