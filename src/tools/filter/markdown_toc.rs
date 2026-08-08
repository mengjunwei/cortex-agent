//! Markdown 文档的目录抽取过滤
//!
//! 借鉴 ecotokens 的 filter/markdown.rs。知识库返回的长 markdown 文档
//! 往往包含大量小节，全文进入 LLM 上下文成本高昂。本过滤器抽取 H1–H3
//! 生成目录（ToC），并保留首个小节的开头作为"首节摘要"，让 LLM 既看到
//! 文档结构，又看到典型内容，再由上层决定是否需要进一步检索。

/// ToC 抽取的最大标题层级
const MAX_HEADING_LEVEL: usize = 3;
/// 首节摘要保留的最大行数
const FIRST_SECTION_MAX_LINES: usize = 50;

/// 过滤长 markdown：生成目录 + 保留首节摘要
///
/// 若文本不含任何标题（不像 markdown），原样返回。
pub(crate) fn filter(text: &str) -> String {
    let mut toc = String::from("# 文档目录\n\n");
    let mut first_section = String::new();
    let mut first_section_lines = 0usize;
    let mut heading_seen = false;
    let mut toc_entries = 0usize;

    for line in text.lines() {
        // 回归 B8：parse_heading 每行只调用一次，结果复用于 ToC 与首节判定
        let heading = parse_heading(line);
        let is_deep_heading = heading
            .as_ref()
            .map(|(level, _)| *level > MAX_HEADING_LEVEL)
            .unwrap_or(false);

        if let Some((level, title)) = heading {
            if level <= MAX_HEADING_LEVEL {
                let indent = "  ".repeat(level.saturating_sub(1));
                toc.push_str(&format!("{}- {}\n", indent, title));
                toc_entries += 1;
            }
            // 首个标题之后开始收集首节
            heading_seen = true;
        }

        if heading_seen && !is_deep_heading && first_section_lines < FIRST_SECTION_MAX_LINES {
            first_section.push_str(line);
            first_section.push('\n');
            first_section_lines += 1;
        }
    }

    // 无标题：不是结构化 markdown，原样返回
    if toc_entries == 0 {
        return text.to_string();
    }

    let mut out = String::with_capacity(toc.len() + first_section.len() + 64);
    out.push_str(&toc);
    if !first_section.is_empty() {
        out.push_str("\n# 首节摘要\n\n");
        out.push_str(first_section.trim_end());
        out.push('\n');
    }
    out
}

/// 解析 markdown ATX 标题行，返回 (层级, 标题文本)；非标题返回 None
fn parse_heading(line: &str) -> Option<(usize, String)> {
    let trimmed = line.trim_start();
    let hashes = trimmed.chars().take_while(|&c| c == '#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = &trimmed[hashes..];
    // '#' 后必须紧跟空格或行尾
    if !rest.is_empty() && !rest.starts_with(' ') {
        return None;
    }
    let title = rest.trim().trim_end_matches('#').trim();
    if title.is_empty() {
        return None;
    }
    Some((hashes, title.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_markdown_passthrough() {
        let text = "plain text\nno headings here";
        assert_eq!(filter(text), text);
    }

    #[test]
    fn extracts_toc_and_first_section() {
        let text = "# 设备运维手册\n\n## 概述\n\n这是概述内容。\n\n## 接口管理\n\n### 查看接口\n\ndetails...\n\n## 告警\n\nalarm config";
        let out = filter(text);
        assert!(out.contains("# 文档目录"));
        assert!(out.contains("- 设备运维手册"));
        assert!(out.contains("  - 概述"));
        assert!(out.contains("  - 接口管理"));
        assert!(out.contains("    - 查看接口"));
        assert!(out.contains("  - 告警"));
        assert!(out.contains("# 首节摘要"));
        assert!(out.contains("这是概述内容。"));
    }

    #[test]
    fn skips_h4_and_deeper() {
        let text = "# T\n\n## S1\n\n#### Deep\n\nbody";
        let out = filter(text);
        assert!(out.contains("- T"));
        assert!(out.contains("- S1"));
        assert!(!out.contains("Deep"));
    }

    #[test]
    fn ignores_hash_in_code_like_lines() {
        // 无空格的 # 不算标题
        let text = "#tag not heading\nsome #hashtag";
        assert_eq!(filter(text), text);
    }
}
