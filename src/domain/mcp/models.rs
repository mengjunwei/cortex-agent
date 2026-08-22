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
    /// 归属人（完全归属隔离：每人只看自己的；管理员看全部）
    pub user_id: String,
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
///
/// 超过 64 字节时截断前缀 + 追加全名 SHA-256 前 8 位 hex 后缀。原因：
/// adk-model 各 provider 的 `convert_tools` 把声明名经 `normalize_tool_name`
/// 截到 64 字节才发给 LLM，而 runner 按全名精确匹配 tool_map——超长名会
/// 「LLM 看到截断名、注册的是全名」，回调必然 miss（工具调用失败）。在生成侧
/// 就压进 64 字节（前缀保语义、哈希后缀防同前缀工具碰撞），两侧名字严格一致。
/// 真实工具名由 [`ManagedMcpTool`](super::manager::ManagedMcpTool) 单独持有，
/// 命名空间名只用于 LLM 侧注册/回调，无反向解析依赖。
pub fn namespaced_tool_name(slug: &str, tool: &str) -> String {
    /// 与 adk `SchemaAdapter::normalize_tool_name` 的字节上限一致
    const MAX_LEN: usize = 64;
    /// `_` + 8 hex chars
    const HASH_SUFFIX_LEN: usize = 9;

    let full = format!("mcp__{slug}__{tool}");
    if full.len() <= MAX_LEN {
        return full;
    }
    let mut end = MAX_LEN - HASH_SUFFIX_LEN;
    while end > 0 && !full.is_char_boundary(end) {
        end -= 1;
    }
    let digest = {
        use sha2::{Digest, Sha256};
        let h = Sha256::digest(full.as_bytes());
        h.iter().take(4).map(|b| format!("{b:02x}")).collect::<String>()
    };
    format!("{}_{}", &full[..end], digest)
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
    fn namespaced_tool_name_truncates_to_64_bytes() {
        // 恰好 64 字节：`mcp__s__` 8 字节 + 工具名 56 字节，不截断
        let tool56 = "t".repeat(56);
        assert_eq!(namespaced_tool_name("s", &tool56), format!("mcp__s__{tool56}"));
        // 超限：截断 + 哈希后缀——总长恰好 64、确定、命名空间前缀保留
        let long = namespaced_tool_name("s", &"t".repeat(80));
        assert_eq!(long.len(), 64, "截断后必须恰好 64 字节");
        assert!(long.starts_with("mcp__s__"));
        assert_eq!(long, namespaced_tool_name("s", &"t".repeat(80)), "同名必须确定");
        // 同 55 字节前缀、不同尾部的两个长工具 → 哈希后缀不同（不碰撞）
        let a = "a".repeat(55) + "x";
        let b = "a".repeat(55) + "y";
        let na = namespaced_tool_name("s", &a);
        let nb = namespaced_tool_name("s", &b);
        assert_eq!(na.len(), 64);
        assert_eq!(nb.len(), 64);
        assert_ne!(na, nb);
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
