//! 工具输出语义过滤模块 — 按工具家族（family）做结构化压缩
//!
//! 借鉴 ecotokens 的 filter/ 分派思路：不同工具的输出结构差异巨大，
//! 一刀切的字节截断会破坏关键结构（表格表头、markdown 标题层级、grep 分组）。
//! 本模块按工具名分派到不同的语义过滤器，在硬截断之前先做结构化压缩。
//!
//! ```text
//! 工具原始文本
//!   ├─ apply_filter(text, family, soft_budget)
//!   │     ├─ 文本 ≤ soft_budget  → 原样返回（无需压缩）
//!   │     └─ 文本 > soft_budget  → 按 family 分派
//!   │           ├─ ShowTable   → show_table::filter   (保留表头 + 前 N 数据行)
//!   │           ├─ MarkdownToc → markdown_toc::filter (目录 + 首节)
//!   │           ├─ GrepLines   → grep_lines::filter   (按文件分组 + 限条数)
//!   │           └─ Generic     → 原样返回（交给 truncate_text 兜底）
//!   └─ 后续由 truncate_text 做 UTF-8 安全硬截断（精度兜底）
//! ```
//!
//! 归属：应用层（`src/tools/`）。过滤器只处理纯文本，不解析 JSON 结构，
//! JSON 内部字符串的提取由 [`crate::tools::truncating`] 负责。

pub mod grep_lines;
pub mod markdown_toc;
pub mod show_table;

/// 工具家族，决定使用哪种语义过滤器
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FilterFamily {
    /// `show` / `exec` / `shell` 类命令输出（含表格）
    ShowTable,
    /// `search` / `kb` / `retrieve` 类知识库文档（markdown）
    MarkdownToc,
    /// `grep` / `find` / `rg` 类检索输出（多文件行匹配）
    GrepLines,
    /// 未匹配到的通用工具，不做语义压缩
    Generic,
}

/// 已知工具名的精确家族映射（全名大小写不敏感）
///
/// **登记规范**（项目约定）：新增任何本地 [`FunctionTool`] 时，**必须**在此登记其
/// 全名 → 家族映射，使其归类成为权威无歧义的显式声明。这样所有已上线工具都有
/// 明确归属，[`detect_family`] 的子串回退纯作未来未知工具的兜底，不参与线上工具
/// 的实际分派决策——避免子串启发式因关键词重叠产生误判（如 `browser_find` 含
/// `find` 但并非代码检索；`snmp_test_collect` 若未来给子串表加入 `test` 关键词
/// 也不会被误伤）。
///
/// 当前覆盖全部 8 个本地 FunctionTool + 通过 [`match_prefix`] 的 `browser_` 前缀
/// 规则覆盖所有 `browser_*` 前缀的 MCP 浏览器工具（具体数量由运行时加载决定）。
const EXACT_FAMILY_MAP: &[(&str, FilterFamily)] = &[
    // ── MarkdownToc：知识库 / 文档检索（markdown 结构输出）──
    ("search_kb", FilterFamily::MarkdownToc),
    // ── Generic：结构化 JSON / 状态返回，无需语义压缩（交 truncate_text 兜底）──
    ("query_device_catalog", FilterFamily::Generic),
    ("validate_monitor_plugin", FilterFamily::Generic),
    ("lookup_device_id", FilterFamily::Generic),
    ("snmp_test_collect", FilterFamily::Generic),
    ("register_monitor_plugin", FilterFamily::Generic),
];

/// 前缀规则：按工具名前缀做大类归类，消除大类歧义
///
/// 返回 `Some(family)` 表示命中前缀规则；`None` 表示未命中，继续后续匹配。
fn match_prefix(lower: &str) -> Option<FilterFamily> {
    // 浏览器工具（browser_*）一律 Generic：DOM 操作 / 截图 / 导航，
    // 既非表格、也非文档、亦非代码检索。特别地 `browser_find` 含子串 find，
    // 若无此规则会被子串启发式误判为 GrepLines。
    if lower.starts_with("browser_") {
        return Some(FilterFamily::Generic);
    }
    None
}

/// 按工具名识别家族
///
/// **三级匹配策略**（逐级降级，消除 C2 子串歧义）：
///
/// 1. **精确表** [`EXACT_FAMILY_MAP`]：已知工具名全名匹配，权威无歧义。
///    **覆盖全部 8 个本地工具**，是线上工具唯一的实际分派路径。
/// 2. **前缀规则** [`match_prefix`]：按 `browser_` 等前缀归大类，覆盖所有 `browser_*` MCP 工具。
/// 3. **子串回退**：**纯兜底**，仅对未来新增但尚未登记的工具按关键词启发式（大小写
///    不敏感）临时归类。关键词按"精确性"排序：先判代码检索（`grep`/`find`/`rg_`/
///    `search_code`/`code_search`），再判文档检索（`search`/`retrieve`/`kb`/`doc`/
///    `knowledge`），最后判命令类（`command`/`exec`/`shell`/`console`/`terminal`/`show`）。
///    新工具上线后应迁入精确表，使此分支回归"零命中"。
///
/// 三级都未命中返回 [`FilterFamily::Generic`]。
pub(crate) fn detect_family(tool_name: &str) -> FilterFamily {
    // [1] 精确表：权威映射，新增工具首选登记点。
    // 字面量均为小写，用 eq_ignore_ascii_case 零分配比对——线上工具 100% 走此分支，
    // 避免每次调用都 to_ascii_lowercase 分配 String（与 redact.rs 的 Cow 优化同精神）。
    for (name, family) in EXACT_FAMILY_MAP {
        if tool_name.eq_ignore_ascii_case(name) {
            return *family;
        }
    }

    // [2]/[3] 落到前缀规则或子串回退才需要小写形式，惰性分配
    let lower = tool_name.to_ascii_lowercase();

    // [2] 前缀规则：消除大类歧义
    if let Some(family) = match_prefix(&lower) {
        return family;
    }

    // [3] 子串回退：未知工具的启发式分派
    if contains_any(
        &lower,
        &["grep", "find", "rg_", "search_code", "code_search"],
    ) {
        FilterFamily::GrepLines
    } else if contains_any(&lower, &["search", "retrieve", "kb", "doc", "knowledge"]) {
        FilterFamily::MarkdownToc
    } else if contains_any(
        &lower,
        &["command", "exec", "shell", "console", "terminal", "show"],
    ) {
        FilterFamily::ShowTable
    } else {
        FilterFamily::Generic
    }
}

/// 应用语义过滤
///
/// - `text`: 待过滤文本
/// - `family`: 工具家族
/// - `soft_budget`: 软预算（字节）。文本短于该值则原样返回，避免无谓压缩。
///
/// 返回值可能仍长于 `soft_budget`，由调用方通过 `truncate_text` 做硬截断兜底。
pub(crate) fn apply_filter(text: &str, family: FilterFamily, soft_budget: usize) -> String {
    if text.len() <= soft_budget {
        return text.to_string();
    }
    match family {
        FilterFamily::ShowTable => show_table::filter(text),
        FilterFamily::MarkdownToc => markdown_toc::filter(text),
        FilterFamily::GrepLines => grep_lines::filter(text),
        FilterFamily::Generic => text.to_string(),
    }
}

/// UTF-8 安全的字符边界下取：把 `idx` 回退到最近的字符边界
///
/// 委托 [`str::floor_char_boundary`]（自 Rust 1.80 起稳定；本项目工具链 ≥ 1.85，
/// 见 `rustc --version`）。保留此薄封装是为了集中调用点，便于 grep 与一致性兜底。
pub(crate) fn floor_char_boundary(s: &str, idx: usize) -> usize {
    s.floor_char_boundary(idx)
}

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| haystack.contains(n))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_family_search_kb() {
        assert_eq!(detect_family("search_kb"), FilterFamily::MarkdownToc);
        assert_eq!(detect_family("retrieve_doc"), FilterFamily::MarkdownToc);
    }

    #[test]
    fn detect_family_command() {
        assert_eq!(detect_family("device_command"), FilterFamily::ShowTable);
        assert_eq!(detect_family("exec_shell"), FilterFamily::ShowTable);
    }

    #[test]
    fn detect_family_grep() {
        assert_eq!(detect_family("grep_code"), FilterFamily::GrepLines);
        // 回归 B1：search_code 含子串 search，必须判为 GrepLines 而非 MarkdownToc
        assert_eq!(detect_family("search_code"), FilterFamily::GrepLines);
        assert_eq!(detect_family("code_search"), FilterFamily::GrepLines);
    }

    #[test]
    fn detect_family_generic() {
        // 命中前缀规则（browser_）
        assert_eq!(detect_family("browser_snapshot"), FilterFamily::Generic);
        // 子串回退未命中任何关键词 → Generic 兜底（用明确虚构名，避免与真实工具混淆）
        assert_eq!(detect_family("totally_unknown_xyz"), FilterFamily::Generic);
    }

    #[test]
    fn detect_family_browser_prefix_is_generic() {
        // 回归 C2：browser_ 前缀一律 Generic，消除 browser_find 误判
        // （browser_find 含子串 find，若无前缀规则会被子串回退判为 GrepLines）
        // 这也是"前缀/精确表优先于子串回退"策略唯一的真实分歧样例。
        assert_eq!(detect_family("browser_find"), FilterFamily::Generic);
        assert_eq!(detect_family("browser_navigate"), FilterFamily::Generic);
        assert_eq!(detect_family("browser_click"), FilterFamily::Generic);
        assert_eq!(detect_family("browser_evaluate"), FilterFamily::Generic);
        assert_eq!(detect_family("browser_screenshot"), FilterFamily::Generic);
    }

    #[test]
    fn detect_family_substring_fallback_for_unknown_tools() {
        // 回归 C2：未知工具仍走子串启发式
        assert_eq!(detect_family("grep_code"), FilterFamily::GrepLines);
        assert_eq!(detect_family("device_command"), FilterFamily::ShowTable);
        assert_eq!(detect_family("retrieve_doc"), FilterFamily::MarkdownToc);
        assert_eq!(detect_family("totally_unknown"), FilterFamily::Generic);
    }

    #[test]
    fn detect_family_all_local_tools_have_explicit_mapping() {
        // 全部 8 个本地 FunctionTool 的精确表映射快照。
        //
        // 注意：这是一张"当前状态快照"，**不能**自动检测未来新增工具是否漏登记
        // （Rust 测试无法编译期交叉校验 FunctionTool::new 的调用点）。
        // 它守护的是两个真实不变量：
        //  1. 已登记的 8 个工具不被误改家族归属（回归保护）；
        //  2. 大小写不敏感（精确表经 to_ascii_lowercase 比对）。
        // "新增工具须同步登记"靠 code review 强制，见 EXACT_FAMILY_MAP 文档。
        //
        // MarkdownToc：文档/检索类（markdown 结构输出）
        assert_eq!(detect_family("search_kb"), FilterFamily::MarkdownToc);
        // Generic：结构化 JSON / 状态返回
        assert_eq!(detect_family("query_device_catalog"), FilterFamily::Generic);
        assert_eq!(
            detect_family("validate_monitor_plugin"),
            FilterFamily::Generic
        );
        assert_eq!(detect_family("lookup_device_id"), FilterFamily::Generic);
        assert_eq!(detect_family("snmp_test_collect"), FilterFamily::Generic);
        assert_eq!(
            detect_family("register_monitor_plugin"),
            FilterFamily::Generic
        );
        // 大小写不敏感（精确表 to_ascii_lowercase 后比较）
        assert_eq!(detect_family("SEARCH_KB"), FilterFamily::MarkdownToc);
        assert_eq!(
            detect_family("Validate_Monitor_Plugin"),
            FilterFamily::Generic
        );
    }

    #[test]
    fn apply_filter_skips_short_text() {
        let out = apply_filter("short", FilterFamily::MarkdownToc, 100);
        assert_eq!(out, "short");
    }

    #[test]
    fn floor_char_boundary_handles_multibyte() {
        // 中文每个字符 3 字节
        let s = "你好世界";
        // 在第 4 字节（"好" 中间）下取，应回到 3（"好" 起点）
        assert_eq!(floor_char_boundary(s, 4), 3);
        // 越界返回长度
        assert_eq!(floor_char_boundary(s, 100), s.len());
    }
}
