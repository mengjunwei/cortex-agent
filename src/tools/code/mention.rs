//! @mention 上下文解析器 — 把用户输入框 @ 引用的文件/符号注入为 XML 上下文块。
//!
//! 设计参考 Zed `crates/agent/src/thread.rs` 的 `UserMessage::to_request`：
//! 把 @mention 按类别分桶包进 `<files>` / `<symbols>` 等 XML 标签，
//! 便于模型区分上下文来源、利于 prompt 缓存命中。
//!
//! 类型 `MentionRef` 定义在此处（而非 `server/sse.rs`），避免 server→tools 循环依赖；
//! `sse::InputMessage` 通过 `mentions: Vec<MentionRef>` 引用。

use std::path::Path;

use serde::{Deserialize, Serialize};

/// 一条 @mention 引用（由前端在用户输入框 @ 触发时构造）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum MentionRef {
    /// @文件（整个文件内容注入）
    File { path: String },
    /// @符号（文件 + 行范围）
    Symbol {
        path: String,
        start_line: u32,
        end_line: u32,
    },
    /// @选区（同 Symbol，语义上表示用户当前选中的代码）
    Selection {
        path: String,
        start_line: u32,
        end_line: u32,
    },
}

/// 单个 mention 注入时的最大字符数（防 token 爆炸）
const MAX_MENTION_CHARS: usize = 8000;

/// 把 @mention 列表渲染为注入到 user_text 末尾的 XML 上下文块。
///
/// 输出形如：
/// ```text
/// <files>
/// <file path="src/main.rs">
/// fn main() {}
/// </file>
/// </files>
///
/// <symbols>
/// <symbol path="src/lib.rs" lines="10-30">
/// pub fn foo() -> u32 { 42 }
/// </symbol>
/// </symbols>
/// ```
///
/// 若所有 mention 解析失败（文件不存在/越界），返回空字符串。
pub fn render_mentions(mentions: &[MentionRef], root_path: &Path) -> String {
    let mut files: Vec<String> = Vec::new();
    let mut symbols: Vec<String> = Vec::new();

    for m in mentions {
        match m {
            MentionRef::File { path } => {
                if let Some(content) = read_file_for_mention(root_path, path) {
                    files.push(format!("<file path=\"{path}\">\n{content}\n</file>"));
                }
            }
            MentionRef::Symbol {
                path,
                start_line,
                end_line,
            } => {
                if let Some(content) =
                    read_range_for_mention(root_path, path, *start_line, *end_line)
                {
                    symbols.push(format!(
                        "<symbol path=\"{path}\" lines=\"{start_line}-{end_line}\">\n{content}\n</symbol>"
                    ));
                }
            }
            MentionRef::Selection {
                path,
                start_line,
                end_line,
            } => {
                if let Some(content) =
                    read_range_for_mention(root_path, path, *start_line, *end_line)
                {
                    symbols.push(format!(
                        "<selection path=\"{path}\" lines=\"{start_line}-{end_line}\">\n{content}\n</selection>"
                    ));
                }
            }
        }
    }

    let mut out = String::new();
    if !files.is_empty() {
        out.push_str("<files>\n");
        out.push_str(&files.join("\n"));
        out.push_str("\n</files>\n");
    }
    if !symbols.is_empty() {
        out.push_str("<symbols>\n");
        out.push_str(&symbols.join("\n"));
        out.push_str("\n</symbols>\n");
    }
    out
}

fn read_file_for_mention(root: &Path, rel: &str) -> Option<String> {
    let abs = crate::tools::code::resolve_safe_path(root, rel).ok()?;
    let content = std::fs::read_to_string(&abs).ok()?;
    Some(truncate_str(&content, MAX_MENTION_CHARS))
}

fn read_range_for_mention(root: &Path, rel: &str, start: u32, end: u32) -> Option<String> {
    let abs = crate::tools::code::resolve_safe_path(root, rel).ok()?;
    let content = std::fs::read_to_string(&abs).ok()?;
    let lines: Vec<&str> = content.lines().collect();
    if start == 0 || end < start {
        return None;
    }
    let s = (start as usize).saturating_sub(1).min(lines.len());
    let e = (end as usize).min(lines.len());
    if s >= e {
        return None;
    }
    let body: String = lines[s..e]
        .iter()
        .enumerate()
        .map(|(i, l)| format!("{}→{}", start as usize + i, l))
        .collect::<Vec<_>>()
        .join("\n");
    Some(truncate_str(&body, MAX_MENTION_CHARS))
}

fn truncate_str(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut t = s[..max].to_string();
        t.push_str("\n... [已截断]");
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::code::tests_helpers::TmpWs;

    #[test]
    fn renders_file_mention_in_files_bucket() {
        let ws = TmpWs::new();
        let root = ws.canon();
        let out = render_mentions(
            &[MentionRef::File {
                path: "src/lib.rs".into(),
            }],
            &root,
        );
        assert!(out.contains("<files>"));
        assert!(out.contains("<file path=\"src/lib.rs\">"));
        assert!(out.contains("pub fn x()"));
        assert!(!out.contains("<symbols>"));
    }

    #[test]
    fn renders_symbol_mention_in_symbols_bucket() {
        let ws = TmpWs::new();
        ws.write("sym.rs", "line1\nline2\nTARGET\nline4\n");
        let root = ws.canon();
        let out = render_mentions(
            &[MentionRef::Symbol {
                path: "sym.rs".into(),
                start_line: 2,
                end_line: 3,
            }],
            &root,
        );
        assert!(out.contains("<symbols>"));
        assert!(out.contains("<symbol path=\"sym.rs\" lines=\"2-3\">"));
        assert!(out.contains("TARGET"));
        assert!(!out.contains("<files>"));
    }

    #[test]
    fn renders_selection_mention() {
        let ws = TmpWs::new();
        ws.write("sel.rs", "a\nb\nc\n");
        let root = ws.canon();
        let out = render_mentions(
            &[MentionRef::Selection {
                path: "sel.rs".into(),
                start_line: 1,
                end_line: 2,
            }],
            &root,
        );
        assert!(out.contains("<selection path=\"sel.rs\" lines=\"1-2\">"));
    }

    #[test]
    fn mixed_mentions_produce_both_buckets() {
        let ws = TmpWs::new();
        ws.write("f.rs", "file content\n");
        ws.write("s.rs", "l1\nl2\n");
        let root = ws.canon();
        let out = render_mentions(
            &[
                MentionRef::File {
                    path: "f.rs".into(),
                },
                MentionRef::Symbol {
                    path: "s.rs".into(),
                    start_line: 1,
                    end_line: 2,
                },
            ],
            &root,
        );
        assert!(out.contains("<files>"));
        assert!(out.contains("<symbols>"));
    }

    #[test]
    fn empty_mentions_returns_empty() {
        let ws = TmpWs::new();
        let root = ws.canon();
        let out = render_mentions(&[], &root);
        assert!(out.is_empty());
    }

    #[test]
    fn skips_mention_path_outside_workspace() {
        let ws = TmpWs::new();
        let root = ws.canon();
        let out = render_mentions(
            &[MentionRef::File {
                path: "../../../etc/passwd".into(),
            }],
            &root,
        );
        // 越界路径被静默跳过，不注入
        assert!(out.is_empty() || !out.contains("passwd"));
    }

    #[test]
    fn skips_missing_file_mention() {
        let ws = TmpWs::new();
        let root = ws.canon();
        let out = render_mentions(
            &[MentionRef::File {
                path: "nonexistent.rs".into(),
            }],
            &root,
        );
        assert!(out.is_empty());
    }

    #[test]
    fn skips_invalid_line_range() {
        let ws = TmpWs::new();
        ws.write("x.rs", "only one line\n");
        let root = ws.canon();
        let out = render_mentions(
            &[MentionRef::Symbol {
                path: "x.rs".into(),
                start_line: 5,
                end_line: 10,
            }],
            &root,
        );
        assert!(out.is_empty());
    }
}
