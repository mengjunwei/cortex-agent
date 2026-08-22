//! 工具名前端友好展示 + MCP 命名空间 / ARTIFACT 标记处理。
//!
//! 一组纯函数：被 SSE 事件流（`stream`）与会话历史回放（`server::session`）复用。
//! - `tool_display_name`：英文工具名 → 前端中文短词标签；
//! - `strip_artifact_markers`：剥 `[[ARTIFACT:...]]` 内部标记（文本出口 + 落库前兜底）；
//! - `is_pure_artifact_command` / `mcp_server_name`：工具事件展示过滤与来源标注。

use serde_json::Value;
use std::collections::HashMap;

/// 将工具英文名转为前端友好的中文名
pub fn tool_display_name(name: &str) -> String {
    match name {
        "search_kb" => "检索知识库".to_string(),
        "query_device_catalog" => "查询设备目录".to_string(),
        "validate_monitor_plugin" => "校验监控插件".to_string(),
        "register_monitor_plugin" => "注册监控插件".to_string(),
        "lookup_device_id" => "查询设备ID".to_string(),
        "snmp_test_collect" => "SNMP采集测试".to_string(),
        // 内置代码/文件工具：语义化短词标签（对齐 codex exec_cell 渲染的 Read/List/Search/Edit/Write），
        // 不再让前端拿到一串相同的英文函数名而显示成同样的标签。
        "read_file" => "Read".to_string(),
        "glob" => "Glob".to_string(),
        "grep" => "Search".to_string(),
        "edit_file" => "Edit".to_string(),
        "create_file" => "Write".to_string(),
        // MCP 工具：剥离 `mcp__{slug}__` 命名空间前缀，只展示工具名。
        // 命名空间仅用于 LLM 调用时的全局唯一性（见 domain::mcp::models::namespaced_tool_name），
        // 展示层不需要；来源（server 名）由 TOOL_CALL_START 事件的 server_name 字段单独携带。
        other => strip_mcp_namespace(other),
    }
}

/// 剥离 MCP 工具的 `mcp__{slug}__` 命名空间前缀，只保留工具名。
///
/// `slug` 经 `slugify` 保证不含连续下划线，故 `mcp__{slug}__{tool}` 中紧跟
/// `mcp__` 之后的第一个 `__` 即命名空间分隔符，其后的整体为工具名。
/// 非 MCP 工具（无 `mcp__` 前缀）原样返回。
fn strip_mcp_namespace(name: &str) -> String {
    let Some(rest) = name.strip_prefix("mcp__") else {
        return name.to_string();
    };
    match rest.find("__") {
        Some(pos) => rest[pos + 2..].to_string(),
        None => rest.to_string(),
    }
}

/// 剥离 `[[ARTIFACT:path|title|mime]]` 标记。
///
/// 该标记是脚本产物→前端文件卡片的内部信号（见 `shell_command::emit_artifacts_and_strip`），
/// 仅用于界面，对用户无意义。工具层已剥工具输出里的标记，但模型偶尔会把上下文中的标记
/// 原文抄进回复正文，故在文本出口（推前端 + 落库前）再兜底剥一道，避免误导用户。
///
/// 仅做正则替换、其余原样返回——刻意不做逐行 trim / 空行收敛：流式时每个分片单独
/// 过本函数，任何按行重组（`.lines().join`）都会吞掉分片末尾换行，把相邻表格行拼成
/// `| a || b |` 破坏 Markdown 结构。剥标记残留的行尾空格 / 空行由前端 Markdown 渲染
/// 自然吸收，不值得为此动换行（trim_end 还会误吞模型故意的「两个行尾空格 = 换行」）。
pub(crate) fn strip_artifact_markers(text: &str) -> String {
    use regex::Regex;
    use std::sync::OnceLock;
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"\[\[ARTIFACT:[^|\]]*\|[^|\]]*\|[^|\]]*\]\]").unwrap());
    re.replace_all(text, "").into_owned()
}

/// 判断一条 shell_command 调用是否「只是打印下载标记」（剥掉 `echo "[[ARTIFACT:...]]"`
/// 及裸标记、连接符后命令为空）。这类命令对用户无信息量，整条工具事件不发前端。
///
/// 形如 `echo "[[ARTIFACT:x|t|m]]"` → true；`gen.py && echo "[[ARTIFACT:..]]"` → false（保留）。
pub(super) fn is_pure_artifact_command(args: &Value) -> bool {
    let Some(cmd) = args
        .get("command")
        .and_then(|v| v.as_str())
        .map(str::to_string)
    else {
        return false;
    };
    use regex::Regex;
    use std::sync::OnceLock;
    static ECHO_RE: OnceLock<Regex> = OnceLock::new();
    static MARK_RE: OnceLock<Regex> = OnceLock::new();
    let echo_re = ECHO_RE.get_or_init(|| {
        Regex::new(r#"echo\s+"?\[\[ARTIFACT:[^|\]]*\|[^|\]]*\|[^|\]]*\]\]"?"#).unwrap()
    });
    let mark_re =
        MARK_RE.get_or_init(|| Regex::new(r"\[\[ARTIFACT:[^|\]]*\|[^|\]]*\|[^|\]]*\]\]").unwrap());
    let cleaned = echo_re.replace_all(&cmd, "");
    let cleaned = mark_re.replace_all(&cleaned, "");
    // 剥连接符与空白后若为空 → 纯标记命令
    cleaned
        .replace(['&', ';', ' ', '\t', '\n', '\r'], "")
        .is_empty()
}

/// 配合 [`tool_display_name`] 使用：前者剥离前缀显示工具名，本函数提供来源。
/// 非 MCP 工具或映射未命中时返回 `None`（前端不显示来源标记）。
pub(super) fn mcp_server_name(name: &str, slug_map: &HashMap<String, String>) -> Option<String> {
    let rest = name.strip_prefix("mcp__")?;
    let slug = match rest.find("__") {
        Some(p) => &rest[..p],
        None => rest,
    };
    slug_map.get(slug).cloned()
}
