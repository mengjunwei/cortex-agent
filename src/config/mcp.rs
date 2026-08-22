//! MCP 配置段 — `[mcp]` 种子服务器

use serde::Deserialize;

/// MCP 配置（`[mcp]` 段）— 预配置 MCP 服务器种子
#[derive(Debug, Clone, Default, Deserialize)]
pub struct McpConfig {
    /// 预配置 MCP 服务器列表（启动时 upsert 到 DB）
    #[serde(default)]
    pub seeds: Vec<McpSeedConfig>,
    /// stdio 子进程环境策略：
    ///
    /// - false=收紧（默认）：仅透传基础白名单 PATH/HOME/LANG 等 + 该 server 显式 env，
    ///   防宿主凭证泄漏给第三方 MCP 进程（白名单见 transport::CORE_ENV_VARS）
    /// - true=继承宿主全量环境（兼容依赖非常规环境变量的旧部署）
    #[serde(default)]
    pub stdio_inherit_env: bool,
}

/// 单个 MCP 种子配置
#[derive(Debug, Clone, Deserialize)]
pub struct McpSeedConfig {
    /// 唯一标识（slug，用于 DB upsert 匹配）
    pub slug: String,
    /// 显示名称
    pub name: String,
    /// 传输方式：1=stdio, 2=streamable_http
    #[serde(default = "default_mcp_transport")]
    pub transport: i16,
    /// stdio: 命令路径; http: URL
    pub endpoint: String,
    /// 启动参数（JSON 数组字符串）
    #[serde(default = "default_mcp_args")]
    pub args: String,
    /// 单次工具调用超时（秒），缺省 60
    #[serde(default)]
    pub tool_timeout_secs: Option<i64>,
}

fn default_mcp_transport() -> i16 {
    1
}
fn default_mcp_args() -> String {
    "[]".to_string()
}
