//! `edit_file` 工具 — 替换工作区内文件内容（含 diff 生成）。
//!
//! 设计参考 Zed `crates/agent/src/tools/edit_file_tool.rs`：
//! - `old_text` 必须在文件中**唯一匹配**；多次出现则报错（要求更具体上下文）
//! - 原子写入：先写 `.cortex-tmp`，再 rename（防中途崩溃损坏原文件）
//! - 返回 unified diff（`diff` crate 生成），前端据此渲染 diff 视图
//! - occurrence 参数：当同一文本需多次替换时，指定替换第几次出现（默认 1）

use std::path::{Path, PathBuf};
use std::sync::Arc;

use adk_rust::ToolContext;
use adk_rust::serde_json::{Value, json};
use adk_rust::tool::FunctionTool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct EditFileParams {
    /// 相对工作区根目录的文件路径
    pub path: String,
    /// 要替换的旧文本（必须唯一匹配；多次出现请加更多上下文）
    pub old_text: String,
    /// 替换为的新文本
    pub new_text: String,
    /// 替换第几次出现（默认 1；用于同一文本多次出现时精确指定）
    #[serde(default)]
    pub occurrence: Option<u32>,
}

pub fn create_edit_file_tool(root_path: Arc<PathBuf>) -> FunctionTool {
    let root = root_path.clone();
    FunctionTool::new(
        "edit_file",
        "Replace text in a workspace file. `old_text` must uniquely match text in the file (multiple matches error — include more surrounding context). Returns a unified diff of the change. The workspace is the only writable area — make all file changes here, and never modify read-only dependencies (skills, system files).",
        move |_ctx: Arc<dyn ToolContext>, args: Value| {
            let root = root.clone();
            async move {
                let p: EditFileParams = match serde_json::from_value(args) {
                    Ok(v) => v,
                    Err(e) => return Ok(json!({ "ok": false, "error": format!("参数错误: {e}") })),
                };
                Ok(edit_file_impl(&root, &p).await)
            }
        },
    )
    .with_parameters_schema::<EditFileParams>()
}

pub async fn edit_file_impl(root: &Path, p: &EditFileParams) -> Value {
    if p.old_text == p.new_text {
        return json!({ "ok": false, "error": "old_text 与 new_text 相同，无需替换" });
    }
    if p.old_text.is_empty() {
        return json!({ "ok": false, "error": "old_text 不能为空" });
    }

    let abs = match super::resolve_safe_path(root, &p.path) {
        Ok(x) => x,
        Err(e) => return json!({ "ok": false, "error": e }),
    };

    let original = match tokio::fs::read_to_string(&abs).await {
        Ok(c) => c,
        Err(e) => return json!({ "ok": false, "error": format!("读取失败: {e}") }),
    };

    // 统计 old_text 出现次数
    let occurrences: Vec<usize> = original
        .match_indices(&p.old_text)
        .map(|(i, _)| i)
        .collect();
    if occurrences.is_empty() {
        return json!({
            "ok": false,
            "error": "old_text 在文件中未找到（请检查缩进/空格/换行是否完全一致）",
            "path": p.path,
        });
    }

    let occ_idx = p.occurrence.unwrap_or(1) as usize;
    if occ_idx > occurrences.len() {
        return json!({
            "ok": false,
            "error": format!(
                "occurrence={} 超出实际出现次数 {}",
                occ_idx,
                occurrences.len()
            ),
        });
    }
    if occurrences.len() > 1 && p.occurrence.is_none() {
        return json!({
            "ok": false,
            "error": format!(
                "old_text 在文件中出现 {} 次，无法唯一匹配。请扩大 old_text 上下文使其唯一，或用 occurrence 参数指定第几次出现。",
                occurrences.len()
            ),
            "occurrences": occurrences.len(),
        });
    }

    let replace_at = occurrences[occ_idx - 1];
    let mut updated = String::with_capacity(original.len() + p.new_text.len());
    updated.push_str(&original[..replace_at]);
    updated.push_str(&p.new_text);
    updated.push_str(&original[replace_at + p.old_text.len()..]);

    // 原子写入
    if let Err(e) = atomic_write(&abs, &updated).await {
        return json!({ "ok": false, "error": format!("写入失败: {e}") });
    }

    // 生成 unified diff
    let diff_text = make_unified_diff(&original, &updated, &p.path);

    json!({
        "ok": true,
        "path": p.path,
        "applied": true,
        "occurrence_replaced": occ_idx,
        "diff": diff_text,
    })
}

async fn atomic_write(path: &Path, content: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("cortex-tmp");
    tokio::fs::write(&tmp, content).await?;
    tokio::fs::rename(&tmp, path).await?;
    Ok(())
}

/// 生成 unified diff（`--- a/path` / `+++ b/path` 头 + hunk）
fn make_unified_diff(old: &str, new: &str, path: &str) -> String {
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();
    let hunks = diff::slice(&old_lines, &new_lines);

    let mut out = String::new();
    out.push_str(&format!("--- a/{path}\n+++ b/{path}\n"));

    // 简化的 hunk 头：计算连续变更段的起止行
    let mut old_start = 1usize;
    let mut new_start = 1usize;
    let mut hunk_lines: Vec<String> = Vec::new();
    let mut hunk_old_count = 0usize;
    let mut hunk_new_count = 0usize;
    let mut has_change = false;

    for change in &hunks {
        match change {
            diff::Result::Left(l) => {
                hunk_lines.push(format!("-{l}"));
                old_start = old_start.max(1);
                hunk_old_count += 1;
                has_change = true;
            }
            diff::Result::Both(l, _) => {
                if has_change && hunk_lines.len() > 30 {
                    flush_hunk(
                        &mut out,
                        &hunk_lines,
                        old_start,
                        new_start,
                        hunk_old_count,
                        hunk_new_count,
                    );
                    hunk_lines.clear();
                    hunk_old_count = 0;
                    hunk_new_count = 0;
                    has_change = false;
                }
                hunk_lines.push(format!(" {l}"));
                if !has_change {
                    old_start += 1;
                    new_start += 1;
                } else {
                    hunk_old_count += 1;
                    hunk_new_count += 1;
                }
            }
            diff::Result::Right(r) => {
                hunk_lines.push(format!("+{r}"));
                hunk_new_count += 1;
                has_change = true;
            }
        }
    }
    if has_change || !hunk_lines.is_empty() {
        flush_hunk(
            &mut out,
            &hunk_lines,
            old_start,
            new_start,
            hunk_old_count,
            hunk_new_count,
        );
    }
    out.trim_end().to_string()
}

fn flush_hunk(
    out: &mut String,
    lines: &[String],
    old_start: usize,
    new_start: usize,
    old_count: usize,
    new_count: usize,
) {
    if lines.is_empty() {
        return;
    }
    out.push_str(&format!(
        "@@ -{},{} +{},{} @@\n",
        old_start, old_count, new_start, new_count
    ));
    for l in lines {
        out.push_str(l);
        out.push('\n');
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::code::tests_helpers::TmpWs;

    #[tokio::test]
    async fn replaces_unique_match() {
        let ws = TmpWs::new();
        ws.write("e.rs", "fn foo() {\n    let x = 1;\n}\n");
        let root = ws.canon();
        let r = edit_file_impl(
            &root,
            &EditFileParams {
                path: "e.rs".into(),
                old_text: "let x = 1;".into(),
                new_text: "let x = 2;".into(),
                occurrence: None,
            },
        )
        .await;
        assert_eq!(r["ok"], true);
        assert_eq!(r["applied"], true);
        // 验证文件确实被改了
        let content = std::fs::read_to_string(root.join("e.rs")).unwrap();
        assert!(content.contains("let x = 2;"));
        assert!(!content.contains("let x = 1;"));
        // diff 应包含 + 和 -
        let diff = r["diff"].as_str().unwrap();
        assert!(diff.contains("-    let x = 1;"));
        assert!(diff.contains("+    let x = 2;"));
    }

    #[tokio::test]
    async fn rejects_multiple_matches_without_occurrence() {
        let ws = TmpWs::new();
        ws.write("m.rs", "todo\ntodo\ntodo\n");
        let root = ws.canon();
        let r = edit_file_impl(
            &root,
            &EditFileParams {
                path: "m.rs".into(),
                old_text: "todo".into(),
                new_text: "done".into(),
                occurrence: None,
            },
        )
        .await;
        assert_eq!(r["ok"], false);
        assert!(r["error"].as_str().unwrap().contains("3 次"));
        // 文件应未被修改
        let content = std::fs::read_to_string(root.join("m.rs")).unwrap();
        assert_eq!(content.matches("todo").count(), 3);
    }

    #[tokio::test]
    async fn replaces_specific_occurrence() {
        let ws = TmpWs::new();
        ws.write("o.rs", "todo\ntodo\ntodo\n");
        let root = ws.canon();
        let r = edit_file_impl(
            &root,
            &EditFileParams {
                path: "o.rs".into(),
                old_text: "todo".into(),
                new_text: "done".into(),
                occurrence: Some(2),
            },
        )
        .await;
        assert_eq!(r["ok"], true);
        let content = std::fs::read_to_string(root.join("o.rs")).unwrap();
        assert_eq!(content.matches("todo").count(), 2);
        assert_eq!(content.matches("done").count(), 1);
    }

    #[tokio::test]
    async fn rejects_no_match() {
        let ws = TmpWs::new();
        ws.write("n.rs", "fn foo() {}\n");
        let root = ws.canon();
        let r = edit_file_impl(
            &root,
            &EditFileParams {
                path: "n.rs".into(),
                old_text: "nonexistent".into(),
                new_text: "whatever".into(),
                occurrence: None,
            },
        )
        .await;
        assert_eq!(r["ok"], false);
        assert!(r["error"].as_str().unwrap().contains("未找到"));
    }

    #[tokio::test]
    async fn rejects_identical_text() {
        let ws = TmpWs::new();
        ws.write("same.rs", "same\n");
        let root = ws.canon();
        let r = edit_file_impl(
            &root,
            &EditFileParams {
                path: "same.rs".into(),
                old_text: "same".into(),
                new_text: "same".into(),
                occurrence: None,
            },
        )
        .await;
        assert_eq!(r["ok"], false);
    }

    #[tokio::test]
    async fn rejects_empty_old_text() {
        let ws = TmpWs::new();
        ws.write("empty.rs", "x\n");
        let root = ws.canon();
        let r = edit_file_impl(
            &root,
            &EditFileParams {
                path: "empty.rs".into(),
                old_text: "".into(),
                new_text: "y".into(),
                occurrence: None,
            },
        )
        .await;
        assert_eq!(r["ok"], false);
    }

    #[tokio::test]
    async fn rejects_path_outside_workspace() {
        let ws = TmpWs::new();
        let root = ws.canon();
        let r = edit_file_impl(
            &root,
            &EditFileParams {
                path: "../../../etc/passwd".into(),
                old_text: "x".into(),
                new_text: "y".into(),
                occurrence: None,
            },
        )
        .await;
        assert_eq!(r["ok"], false);
    }

    #[tokio::test]
    async fn occurrence_out_of_range_fails() {
        let ws = TmpWs::new();
        ws.write("or.rs", "dup\ndup\n");
        let root = ws.canon();
        let r = edit_file_impl(
            &root,
            &EditFileParams {
                path: "or.rs".into(),
                old_text: "dup".into(),
                new_text: "unique".into(),
                occurrence: Some(99),
            },
        )
        .await;
        assert_eq!(r["ok"], false);
    }

    #[test]
    fn unified_diff_format() {
        let d = make_unified_diff("a\nb\nc\n", "a\nB\nc\n", "f.rs");
        assert!(d.starts_with("--- a/f.rs"));
        assert!(d.contains("+++ b/f.rs"));
        assert!(d.contains("@@"));
        assert!(d.contains("-b"));
        assert!(d.contains("+B"));
    }
}
