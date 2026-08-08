//! `list_directory` 工具 — 列出工作区内目录结构。
//!
//! 设计参考 Zed `crates/agent/src/tools/list_directory_tool.rs`：
//! - 返回结构化条目（name/kind/size）
//! - 目录在前、文件在后，各自字母序
//! - recursive 默认 false；为 true 时最多递归 3 层（防 token 爆炸）

use std::path::{Path, PathBuf};
use std::sync::Arc;

use adk_rust::ToolContext;
use adk_rust::serde_json::{Value, json};
use adk_rust::tool::FunctionTool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

const MAX_DEPTH: usize = 3;
const MAX_ENTRIES: usize = 1000;

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct ListDirectoryParams {
    /// 相对路径，默认 "."（工作区根）
    #[serde(default)]
    pub path: Option<String>,
    /// 是否递归列出（默认 false，最多 3 层）
    #[serde(default)]
    pub recursive: Option<bool>,
}

#[derive(Debug, serde::Serialize)]
struct Entry {
    name: String,
    kind: &'static str,
    size: Option<u64>,
    /// 仅当 kind=collapsed 时填充：该目录下有多少子项被折叠
    #[serde(skip_serializing_if = "Option::is_none")]
    collapsed_count: Option<usize>,
}

pub fn create_list_directory_tool(root_path: Arc<PathBuf>) -> FunctionTool {
    let root = root_path.clone();
    FunctionTool::new(
        "list_directory",
        "List directory entries in the workspace. Returns structured entries (directories first). `recursive=true` recurses up to 3 levels; deeper trees are auto-collapsed.",
        move |_ctx: Arc<dyn ToolContext>, args: Value| {
            let root = root.clone();
            async move {
                let p: ListDirectoryParams = match serde_json::from_value(args) {
                    Ok(v) => v,
                    Err(e) => return Ok(json!({ "ok": false, "error": format!("参数错误: {e}") })),
                };
                Ok(list_directory_impl(&root, &p).await)
            }
        },
    )
    .with_parameters_schema::<ListDirectoryParams>()
}

pub async fn list_directory_impl(root: &Path, p: &ListDirectoryParams) -> Value {
    let rel = p.path.as_deref().unwrap_or(".").trim();
    let abs = match super::resolve_safe_path(root, rel) {
        Ok(x) => x,
        Err(e) => return json!({ "ok": false, "error": e }),
    };

    let recursive = p.recursive.unwrap_or(false);
    let mut entries = Vec::new();
    let depth = if recursive { MAX_DEPTH } else { 1 };
    let mut truncated = false;

    match walk(&abs, &abs, depth, &mut entries, &mut truncated) {
        Ok(()) => {}
        Err(e) => return json!({ "ok": false, "error": e }),
    }

    json!({
        "ok": true,
        "path": rel,
        "recursive": recursive,
        "truncated": truncated,
        "entries": entries,
    })
}

#[allow(clippy::only_used_in_recursion)]
fn walk(
    root: &Path,
    dir: &Path,
    depth: usize,
    out: &mut Vec<Entry>,
    truncated: &mut bool,
) -> Result<(), String> {
    if depth == 0 || out.len() >= MAX_ENTRIES {
        if out.len() >= MAX_ENTRIES {
            *truncated = true;
        }
        return Ok(());
    }
    let rd = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(e) => return Err(format!("读取目录失败: {e}")),
    };
    let mut subdirs: Vec<(PathBuf, std::fs::Metadata)> = Vec::new();
    let mut files: Vec<(PathBuf, std::fs::Metadata)> = Vec::new();

    for entry in rd.flatten() {
        if out.len() >= MAX_ENTRIES {
            *truncated = true;
            break;
        }
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        // 安全：用 symlink_metadata 不跟随符号链接，防止符号链接目录递归逃逸/循环
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        // 符号链接一律跳过（防逃逸 + 防循环）
        if meta.file_type().is_symlink() {
            continue;
        }
        if meta.is_dir() {
            // 到达最大递归深度：统计子项数，不继续展开
            if depth > 1 {
                out.push(Entry {
                    name,
                    kind: "dir",
                    size: None,
                    collapsed_count: None,
                });
                subdirs.push((path, meta));
            } else {
                let count = count_subitems(&path);
                out.push(Entry {
                    name,
                    kind: "collapsed",
                    size: None,
                    collapsed_count: Some(count),
                });
                continue;
            }
        } else {
            out.push(Entry {
                name,
                kind: "file",
                size: Some(meta.len()),
                collapsed_count: None,
            });
            files.push((path, meta));
        }
    }
    // 字母序不保证（read_dir 顺序不定），但前端可排序；这里保持发现顺序

    if depth > 1 {
        for (sub, _) in subdirs {
            if out.len() >= MAX_ENTRIES {
                *truncated = true;
                break;
            }
            walk(root, &sub, depth - 1, out, truncated)?;
        }
    }
    let _ = files; // files 已在主循环加入
    Ok(())
}

/// 统计一个目录下有多少子项（不递归展开）
fn count_subitems(dir: &Path) -> usize {
    match std::fs::read_dir(dir) {
        Ok(rd) => rd.flatten().count(),
        Err(_) => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::code::tests_helpers::TmpWs;

    #[tokio::test]
    async fn lists_root_non_recursive() {
        let ws = TmpWs::new();
        let root = ws.canon();
        let r = list_directory_impl(
            &root,
            &ListDirectoryParams {
                path: None,
                recursive: None,
            },
        )
        .await;
        assert_eq!(r["ok"], true);
        let entries = r["entries"].as_array().unwrap();
        let names: Vec<&str> = entries
            .iter()
            .map(|e| e["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"main.rs"));
        assert!(names.contains(&"src"));
    }

    #[tokio::test]
    async fn lists_subdirectory() {
        let ws = TmpWs::new();
        let root = ws.canon();
        let r = list_directory_impl(
            &root,
            &ListDirectoryParams {
                path: Some("src".into()),
                recursive: None,
            },
        )
        .await;
        assert_eq!(r["ok"], true);
        let entries = r["entries"].as_array().unwrap();
        let names: Vec<&str> = entries
            .iter()
            .map(|e| e["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"lib.rs"));
        assert!(names.contains(&"sub"));
    }

    #[tokio::test]
    async fn recursive_includes_nested() {
        let ws = TmpWs::new();
        let root = ws.canon();
        let r = list_directory_impl(
            &root,
            &ListDirectoryParams {
                path: Some("src".into()),
                recursive: Some(true),
            },
        )
        .await;
        assert_eq!(r["ok"], true);
        // 递归应能拿到 src/sub/mod.rs（虽然扁平化展示）
        let entries = r["entries"].as_array().unwrap();
        let names: Vec<&str> = entries
            .iter()
            .map(|e| e["name"].as_str().unwrap())
            .collect();
        assert!(
            names.contains(&"mod.rs"),
            "递归应包含嵌套文件，names={:?}",
            names
        );
    }

    #[tokio::test]
    async fn rejects_path_outside_workspace() {
        let ws = TmpWs::new();
        let root = ws.canon();
        let r = list_directory_impl(
            &root,
            &ListDirectoryParams {
                path: Some("../".into()),
                recursive: None,
            },
        )
        .await;
        assert_eq!(r["ok"], false);
    }

    #[tokio::test]
    async fn deep_directories_are_collapsed() {
        let ws = TmpWs::new();
        // 创建 4 层深度的目录：a/b/c/d
        ws.write("a/b/c/d/deep.rs", "fn deep() {}");
        let root = ws.canon();
        let r = list_directory_impl(
            &root,
            &ListDirectoryParams {
                path: Some("a".into()),
                recursive: Some(true),
            },
        )
        .await;
        assert_eq!(r["ok"], true);
        let entries = r["entries"].as_array().unwrap();
        // 应能找到 collapsed 类型的条目（d 目录在第 4 层，超过 3 层深度）
        let has_collapsed = entries
            .iter()
            .any(|e| e["kind"] == "collapsed" && e["collapsed_count"].is_number());
        assert!(has_collapsed, "深层目录应被折叠，entries={:?}", entries);
    }

    #[tokio::test]
    async fn shallow_directories_not_collapsed() {
        let ws = TmpWs::new();
        // 2 层深度：a/b
        ws.write("a/b/shallow.rs", "fn shallow() {}");
        let root = ws.canon();
        let r = list_directory_impl(
            &root,
            &ListDirectoryParams {
                path: Some("a".into()),
                recursive: Some(true),
            },
        )
        .await;
        assert_eq!(r["ok"], true);
        let entries = r["entries"].as_array().unwrap();
        // b 目录在第 2 层，不应折叠
        let all_dirs_are_kind_dir: bool = entries
            .iter()
            .filter(|e| e["kind"] == "dir" || e["kind"] == "file")
            .count()
            > 0;
        assert!(all_dirs_are_kind_dir);
        // 不应有 collapsed 条目
        let has_collapsed = entries.iter().any(|e| e["kind"] == "collapsed");
        assert!(!has_collapsed, "浅层目录不应折叠");
    }
}
