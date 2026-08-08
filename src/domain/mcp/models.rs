//! MCP Server 领域模型
//!
//! - [`McpServer`]：领域实体（敏感字段解密后仅存活于内存）
//! - [`ServerHealth`]：运行时探测状态（不落库）
//! - [`McpToolInfo`]：MCP 工具元信息（含命名空间改写后的名字）

use std::collections::HashMap;

use serde::Serialize;

use super::enums::{Status, TransportKind};

/// 领域实体：MCP Server 配置
#[derive(Debug, Clone)]
pub struct McpServer {
    pub id: String,
    pub name: String,
    pub slug: String,
    pub transport: TransportKind,
    /// stdio: 可执行命令（如 `npx`）；http: 完整 URL
    pub endpoint: String,
    /// stdio: 启动参数；http: 留空
    pub args: Vec<String>,
    /// 环境变量（明文，仅内存）
    pub env: HashMap<String, String>,
    /// http 自定义请求头（明文，仅内存）
    pub headers: HashMap<String, String>,
    pub status: Status,
    /// 单次工具调用超时（秒），默认 60（界面可配，防止卡死 MCP 阻塞 SSE）
    pub tool_timeout_secs: i64,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// MCP Server 运行状态（运行时探测，不落库）
#[derive(Debug, Clone, Serialize, PartialEq, Default)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum ServerHealth {
    /// 从未探测过
    #[default]
    Unknown,
    /// 在线，工具清单可用
    Healthy {
        tools_count: usize,
        last_check: String,
    },
    /// 单次探测失败（容忍中）
    Degraded {
        consecutive_failures: u8,
        last_check: String,
    },
    /// 连续失败超阈值
    Unhealthy { reason: String, last_check: String },
}

/// MCP 工具元信息
#[derive(Debug, Clone, Serialize)]
pub struct McpToolInfo {
    pub server_id: String,
    pub server_name: String,
    pub slug: String,
    /// MCP 工具原始名
    pub tool_name: String,
    /// 注入 Agent 时用的命名空间名：`mcp__{slug}__{tool_name}`
    pub namespaced_name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

/// 生成工具命名空间改写后的名字：`mcp__{slug}__{tool}`
pub fn namespaced_tool_name(slug: &str, tool: &str) -> String {
    format!("mcp__{slug}__{tool}")
}

/// 将任意名称归一化为合法 slug（仅 `[a-z0-9_]`，小写、连续非法符合并为单个下划线）。
///
/// 返回值保证非空（全非法字符时回退为 `mcp`）。
pub fn slugify(name: &str) -> String {
    let base: String = name
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    // 合并连续下划线、去除首尾下划线
    let mut out = String::with_capacity(base.len());
    let mut prev_under = false;
    for c in base.chars() {
        if c == '_' {
            if !prev_under && !out.is_empty() {
                out.push('_');
            }
            prev_under = true;
        } else {
            out.push(c);
            prev_under = false;
        }
    }
    while out.ends_with('_') {
        out.pop();
    }
    if out.is_empty() {
        "mcp".to_string()
    } else {
        out
    }
}

/// 对敏感 value 做掩码：`****<末4位>`，短值（≤4）返回 `****`
pub fn mask_value(plain: &str) -> String {
    let trimmed = plain.trim();
    let len = trimmed.chars().count();
    if len <= 4 {
        "****".to_string()
    } else {
        let suffix: String = trimmed.chars().skip(len - 4).collect();
        format!("****{suffix}")
    }
}

/// 对 map 的所有 value 做掩码（key 保留），用于 env/headers 脱敏
pub fn mask_map(map: &HashMap<String, String>) -> HashMap<String, String> {
    map.iter()
        .map(|(k, v)| (k.clone(), mask_value(v)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_basic() {
        assert_eq!(slugify("GitHub MCP"), "github_mcp");
        assert_eq!(slugify("File System"), "file_system");
    }

    #[test]
    fn slugify_collapses_and_trims_separators() {
        assert_eq!(slugify("  A -- B  "), "a_b");
        assert_eq!(slugify("---"), "mcp");
        assert_eq!(slugify(""), "mcp");
        assert_eq!(slugify("_leading"), "leading");
        assert_eq!(slugify("trailing_"), "trailing");
    }

    #[test]
    fn slugify_lowercases_ascii() {
        assert_eq!(slugify("SlAcK"), "slack");
        // 非 ASCII 字符视为分隔符
        assert_eq!(slugify("数据库 工具"), "mcp");
    }

    #[test]
    fn mask_value_long() {
        assert_eq!(mask_value("sk-1234567890abcd"), "****abcd");
        assert_eq!(mask_value("secret-token-xyz"), "****-xyz");
    }

    #[test]
    fn mask_value_short_fully_masked() {
        assert_eq!(mask_value("abc"), "****");
        assert_eq!(mask_value("abcd"), "****");
        assert_eq!(mask_value(""), "****");
        assert_eq!(mask_value("  "), "****");
    }

    #[test]
    fn mask_map_preserves_keys() {
        let mut m = HashMap::new();
        m.insert("API_KEY".into(), "sk-1234567890abcd".into());
        m.insert("SHORT".into(), "ab".into());
        let masked = mask_map(&m);
        assert_eq!(masked.get("API_KEY").unwrap(), "****abcd");
        assert_eq!(masked.get("SHORT").unwrap(), "****");
    }

    #[test]
    fn namespaced_tool_name_format() {
        assert_eq!(
            namespaced_tool_name("github", "search"),
            "mcp__github__search"
        );
        assert_eq!(
            namespaced_tool_name("fs", "read_file"),
            "mcp__fs__read_file"
        );
    }

    #[test]
    fn server_health_serde_unknown() {
        let json = serde_json::to_string(&ServerHealth::Unknown).unwrap();
        assert!(json.contains("\"state\":\"unknown\""));
    }

    #[test]
    fn server_health_serde_healthy() {
        let h = ServerHealth::Healthy {
            tools_count: 5,
            last_check: "2026-06-29T00:00:00Z".into(),
        };
        let json = serde_json::to_string(&h).unwrap();
        assert!(json.contains("\"state\":\"healthy\""));
        assert!(json.contains("\"tools_count\":5"));
    }

    #[test]
    fn server_health_serde_unhealthy_has_reason() {
        let h = ServerHealth::Unhealthy {
            reason: "connection refused".into(),
            last_check: "2026-06-29T00:00:00Z".into(),
        };
        let json = serde_json::to_string(&h).unwrap();
        assert!(json.contains("\"state\":\"unhealthy\""));
        assert!(json.contains("connection refused"));
    }
}
