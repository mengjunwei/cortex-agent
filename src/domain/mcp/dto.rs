//! MCP Server 请求/响应 DTO
//!
//! 安全约定（与 [`crate::model_provider::dto`] 一致）：
//! - 写入接口（create/update）接收 env/headers 的**明文**
//! - 读取接口（list/get 响应）的 env/headers value 已**脱敏**
//! - 明文永不外泄

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::enums::{Status, TransportKind};
use super::models::ServerHealth;

/// 新建 MCP Server 输入
#[derive(Debug, Deserialize)]
pub struct CreateMcpServerInput {
    pub name: String,
    pub transport: TransportKind,
    pub endpoint: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// 明文传入，加密存储
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// 明文传入，加密存储
    #[serde(default)]
    pub headers: HashMap<String, String>,
    #[serde(default)]
    pub status: Status,
    /// 单次工具调用超时（秒），默认 60（界面可配）
    #[serde(default = "default_tool_timeout")]
    pub tool_timeout_secs: i64,
}

/// MCP 工具调用超时默认值（秒）
fn default_tool_timeout() -> i64 {
    60
}

/// 更新 MCP Server 输入
#[derive(Debug, Deserialize)]
pub struct UpdateMcpServerInput {
    pub name: String,
    pub transport: TransportKind,
    pub endpoint: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// `Some`：覆盖明文并重新加密；`None`：保持原值（前端只传需改的）
    pub env: Option<HashMap<String, String>>,
    pub headers: Option<HashMap<String, String>>,
    #[serde(default)]
    pub status: Status,
    /// 单次工具调用超时（秒）
    #[serde(default = "default_tool_timeout")]
    pub tool_timeout_secs: i64,
}

/// MCP Server 响应（env/headers value 已脱敏）
#[derive(Debug, Serialize)]
pub struct McpServerResponse {
    pub id: String,
    pub name: String,
    pub slug: String,
    pub transport: TransportKind,
    pub endpoint: String,
    pub args: Vec<String>,
    /// value 已掩码
    pub env: HashMap<String, String>,
    /// value 已掩码
    pub headers: HashMap<String, String>,
    pub status: Status,
    /// 单次工具调用超时（秒）
    pub tool_timeout_secs: i64,
    pub created_at: String,
    pub updated_at: String,
    pub health: ServerHealth,
}

/// mcpTools Query 入参
#[derive(Debug, Deserialize)]
pub struct McpToolsQuery {
    pub server_ids: Vec<String>,
}
