//! `show` / `exec` 类命令的表格输出过滤
//!
//! 借鉴 ecotokens 的 filter/db.rs。网络设备的 `show` 命令输出通常是
//! 带表头的 ASCII 表格（如 `show interface`、`show ip route`），完整输出
//! 可达数千行。本过滤器保留表头与分隔线，仅截取前 N 行数据，附加汇总提示。
//!
//! 识别的表格分隔符：
//! - Unicode 制表线：`─` `│` `├` `┼`
//! - ASCII 对齐线：连续 3+ 个 `-`，或 `-+-` 交叉

use super::floor_char_boundary;

/// 表格数据行保留上限
const MAX_DATA_ROWS: usize = 30;
/// 行截断阈值（超出则按 UTF-8 边界截断）
const MAX_LINE_LEN: usize = 200;

/// 过滤 `show` 类表格文本
///
/// 非表格文本原样返回（交由上层硬截断）。
///
/// 表格分阶段处理：
/// 1. **表头区**：首条分隔线之前的所有行（含分隔线本身）一律保留。
/// 2. **数据区**：分隔线之后的非空行按 `MAX_DATA_ROWS` 截断，超限部分计数。
pub(crate) fn filter(text: &str) -> String {
    if !looks_like_table(text) {
        return text.to_string();
    }

    let mut out = String::with_capacity(text.len().min(8 * 1024));
    let mut data_rows = 0usize;
    let mut skipped = 0usize;
    let mut separator_seen = false;

    for line in text.lines() {
        let is_separator = is_separator_line(line);
        let is_blank = line.trim().is_empty();

        if !separator_seen {
            // 表头区：分隔线之前（含分隔线本身）全部保留
            push_line(&mut out, line);
            if is_separator {
                separator_seen = true;
            }
            continue;
        }

        // 数据区：跳过空行；数据行按上限截断
        if is_blank {
            continue;
        }

        if data_rows < MAX_DATA_ROWS {
            push_line(&mut out, line);
            data_rows += 1;
        } else {
            skipped += 1;
        }
    }

    if skipped > 0 {
        out.push_str(&format!(
            "\n[... 表格已截断：省略 {} 行数据，原共 {} 行 ...]",
            skipped,
            skipped + MAX_DATA_ROWS
        ));
    }
    out
}

fn push_line(out: &mut String, line: &str) {
    if line.len() <= MAX_LINE_LEN {
        out.push_str(line);
    } else {
        let cut = floor_char_boundary(line, MAX_LINE_LEN);
        out.push_str(&line[..cut]);
        out.push_str(" …");
    }
    out.push('\n');
}

/// 判断整段文本是否像表格
///
/// 单次遍历：只有当"分隔线之后存在非空数据行"才判定为表格
/// （回归 B11：防止 Markdown 水平线 `---` 被误判为表格分隔线；
/// 纯分隔线收尾、其后无数据的文本不算表格）。
fn looks_like_table(text: &str) -> bool {
    let mut after_separator = false;
    for line in text.lines() {
        if is_separator_line(line) {
            after_separator = true;
            continue;
        }
        if after_separator && !line.trim().is_empty() {
            return true;
        }
    }
    false
}

/// 判断单行是否为表格分隔线
///
/// 识别两类分隔线：
/// - **Unicode 制表线**：含 `─` 或 `━`（网络设备 show 命令常见）
/// - **ASCII 多列分隔线**：由 `-` / 空格 / `+` 组成，且**必须包含空格或 `+`**
///   （多列对齐特征）。纯 `-` 序列（如 Markdown 水平线 `---`）**不算**表格
///   分隔线，避免把 Markdown 段落分隔误判为表格（回归 C3）。
fn is_separator_line(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return false;
    }
    // Unicode 制表线
    if trimmed.contains('─') || trimmed.contains('━') {
        return true;
    }
    // ASCII 多列分隔线：长度 ≥ 3，且必须含空格或 + 交叉点
    // （纯 --- 是 Markdown 水平线，不算表格分隔线）
    if trimmed.len() >= 3
        && trimmed.chars().all(|c| c == '-' || c == ' ' || c == '+')
        && trimmed.contains('-')
        && (trimmed.contains(' ') || trimmed.contains('+'))
    {
        return true;
    }
    if trimmed.contains("-+-") {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_table_passthrough() {
        let text = "Just some config text\nwithout table separators";
        assert_eq!(filter(text), text);
    }

    #[test]
    fn filters_ascii_table() {
        let text = "Interface       Status    IP-Address\n\
                    --------------- --------- -------------\n\
                    Gig0/0          up        10.0.0.1\n\
                    Gig0/1          up        10.0.0.2\n";
        let out = filter(text);
        // 表头与分隔线保留
        assert!(out.contains("Interface"));
        assert!(out.contains("Gig0/0"));
        // 数据未超限，无截断提示
        assert!(!out.contains("已截断"));
    }

    #[test]
    fn truncates_long_table_with_hint() {
        let mut text = String::from("IF   STAT\n---  ----\n");
        for i in 0..100 {
            text.push_str(&format!("Gi{}/0 up\n", i));
        }
        let out = filter(&text);
        assert!(out.contains("已截断"));
        assert!(out.contains("70 行数据"));
    }

    #[test]
    fn handles_unicode_box_drawing() {
        let text = "接口      状态\n\
                    ─────── ────\n\
                    Gi0/0    up\n\
                    Gi0/1    down\n";
        let out = filter(text);
        assert!(out.contains("Gi0/0"));
        assert!(out.contains("Gi0/1"));
    }

    #[test]
    fn long_line_gets_utf8_safe_cut() {
        let long_field = "中".repeat(300);
        // 回归 C3：分隔线须为多列格式（含空格），纯 --- 不再判为表格分隔线
        let text = format!("HDR   VAL\n---   ---\n{}\n", long_field);
        let out = filter(&text);
        // 被截断的行含省略号
        assert!(out.contains("…"));
    }

    #[test]
    fn pure_dash_rule_never_treated_as_table() {
        // 回归 C3：纯 --- （Markdown 水平线）即使后跟正文也不再判为表格分隔线。
        // 此前 is_separator_line 的 ASCII 分支会匹配纯 - 序列，导致
        // looks_like_table 误判；现在要求 ASCII 分隔线含空格或 +。
        let text = "标题段落\n---\n另一段正文继续，这里有很多内容。";
        let out = filter(text);
        // 不被当表格 → 原样返回，正文完整保留
        assert_eq!(out, text);
        assert!(!out.contains("已截断"));
        assert!(out.contains("另一段正文继续"));
    }

    #[test]
    fn markdown_horizontal_rule_not_treated_as_table() {
        // 回归 B11 + C3：Markdown 水平线 --- 系列场景均不应被误判为表格。
        // C3 后纯 --- 不再是分隔线，这些场景得到更强保障。
        let text = "一段引言文字。\n\n---\n";
        assert_eq!(filter(text), text);

        // 末尾只有分隔线、无后续数据行 → 非表格
        let text2 = "标题\n---";
        assert_eq!(filter(text2), text2);
    }
}
