//! 工具注册表 — 自定义助手的工具白名单与元数据
//!
//! 设计目标：
//! - 提供工具的稳定 `key`（用于助手表 `enabled_tools` 列持久化）
//! - 区分"用户可勾选"（`custom_enabled = true`）与"仅内置"（`custom_enabled = false`）
//! - 校验自定义助手勾选的工具白名单（防止前端传入任意 key）
//!
//! 与 agent 路由（`src/agent/mod.rs`）的关系：
//! - `registry()` 中的 `key` 与各 agent 内部 `FunctionTool::new(key, ...)` 的第一参数一一对应
//! - 新增可勾选工具时，需同步在 `src/agent/custom.rs::push_tool_for_key` 注册构造逻辑

/// 单个工具的静态描述
#[derive(Debug, Clone)]
pub struct ToolDescriptor {
    /// 稳定标识，与 `FunctionTool::new(key, ...)` 第一参数一致
    pub key: &'static str,
    /// 中文展示名称
    pub name: &'static str,
    /// 简短描述（前端 tooltip / 工具卡片）
    pub description: &'static str,
    /// 是否允许自定义助手勾选；false 表示仅内置助手可用
    pub custom_enabled: bool,
}

/// 全量工具注册表（顺序即前端展示顺序）
pub fn registry() -> &'static [ToolDescriptor] {
    &[
        ToolDescriptor {
            key: "search_kb",
            name: "知识库检索",
            description: "检索 Dify 知识库（网络设备运维配置）",
            custom_enabled: true,
        },
        ToolDescriptor {
            key: "query_device_catalog",
            name: "设备目录查询",
            description: "厂商 / 设备类型模糊匹配",
            custom_enabled: true,
        },
        ToolDescriptor {
            key: "shell_command",
            name: "命令执行",
            description: "在沙箱中执行 shell 命令（安全白名单自动放行 + 危险命令拦截 + 其余需审批）",
            custom_enabled: true,
        },
        ToolDescriptor {
            key: "read_file",
            name: "读取文件",
            description: "读取沙箱工作区文件（带行号）",
            custom_enabled: true,
        },
        ToolDescriptor {
            key: "list_directory",
            name: "浏览目录",
            description: "列出沙箱工作区目录结构",
            custom_enabled: true,
        },
        ToolDescriptor {
            key: "grep",
            name: "内容搜索",
            description: "在沙箱工作区正则搜索",
            custom_enabled: true,
        },
        ToolDescriptor {
            key: "edit_file",
            name: "编辑文件",
            description: "替换沙箱工作区文件内容（含 diff）",
            custom_enabled: true,
        },
        ToolDescriptor {
            key: "create_file",
            name: "创建文件",
            description: "在沙箱工作区创建/覆盖文件",
            custom_enabled: true,
        },
        ToolDescriptor {
            key: "validate_monitor_plugin",
            name: "监控插件校验",
            description: "Rhai 监控插件三层校验（仅内置）",
            custom_enabled: false,
        },
    ]
}

/// 仅返回允许自定义助手勾选的工具
pub fn custom_options() -> Vec<&'static ToolDescriptor> {
    registry().iter().filter(|t| t.custom_enabled).collect()
}

/// 过滤自定义助手勾选的工具白名单
///
/// - 保留 `custom_enabled = true` 的 key
/// - 去重（保持首次出现顺序）
/// - 丢弃未知 key 与仅内置 key（防止前端越权勾选）
pub fn sanitize_custom_tools(keys: &[String]) -> Vec<String> {
    let allowed: std::collections::HashSet<&str> = custom_options().iter().map(|t| t.key).collect();
    let mut seen = std::collections::HashSet::new();
    keys.iter()
        .filter(|k| allowed.contains(k.as_str()))
        .filter(|k| seen.insert(k.as_str()))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_keys_are_unique() {
        let keys: Vec<&str> = registry().iter().map(|t| t.key).collect();
        let mut sorted = keys.clone();
        sorted.sort();
        let dedup_len = sorted.windows(2).filter(|w| w[0] == w[1]).count();
        assert_eq!(dedup_len, 0, "duplicate tool keys: {:?}", keys);
    }

    #[test]
    fn custom_options_excludes_builtin_only() {
        let keys: Vec<&str> = custom_options().iter().map(|t| t.key).collect();
        assert!(keys.contains(&"search_kb"));
        assert!(keys.contains(&"query_device_catalog"));
        assert!(keys.contains(&"shell_command"));
        assert!(!keys.contains(&"browser"));
        assert!(!keys.contains(&"validate_monitor_plugin"));
    }

    #[test]
    fn sanitize_filters_invalid_and_builtin_only() {
        let out = sanitize_custom_tools(&["search_kb".into(), "browser".into(), "bogus".into()]);
        assert_eq!(out, vec!["search_kb".to_string()]);
    }

    #[test]
    fn sanitize_dedupes() {
        let out = sanitize_custom_tools(&[
            "search_kb".into(),
            "search_kb".into(),
            "shell_command".into(),
        ]);
        assert_eq!(
            out,
            vec!["search_kb".to_string(), "shell_command".to_string()]
        );
    }

    #[test]
    fn sanitize_preserves_first_seen_order() {
        let out = sanitize_custom_tools(&["search_kb".into(), "shell_command".into()]);
        assert_eq!(
            out,
            vec!["search_kb".to_string(), "shell_command".to_string()]
        );
    }
}
