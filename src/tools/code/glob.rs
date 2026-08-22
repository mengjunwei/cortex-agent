//! `glob` 工具 — 按 pattern 匹配工作区内文件路径（对齐 Claude Code Glob）。
//!
//! 取代原 `list_directory`：Claude Code 没有"列目录"工具，找文件一律用 Glob
//! （`**/*.rs` 这类 pattern → 按修改时间排序的路径列表），模型意图表达更直接，
//! 也省 token（不必拉整棵目录树）。
//!
//! - `pattern`：`*` 不跨目录、`**` 跨任意层（gitignore 语义，复用 grep 的 `compile_glob`）
//! - 结果按修改时间**升序**（对齐 Claude Code Glob "sorted by modification time"）
//! - 安全：resolve_safe_path 防逃逸；跳过符号链接 / 隐藏目录 / node_modules / target

use std::path::{Path, PathBuf};
use std::sync::Arc;

use adk_rust::ToolContext;
use adk_rust::serde_json::{Value, json};
use adk_rust::tool::FunctionTool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// 结果条数硬上限（防 token 爆炸；Claude Code Glob 内部同样有截断）
const MAX_MATCHES: usize = 1000;

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct GlobParams {
    /// Glob 模式，如 "**/*.rs"、"src/**/*.ts"、"*.json"（`*` 不跨目录，`**` 跨任意层）
    pub pattern: String,
    /// 限定搜索的子目录（相对路径），默认整个工作区
    #[serde(default)]
    pub path: Option<String>,
}

#[derive(Debug, serde::Serialize)]
struct GlobEntry {
    /// 相对搜索根的完整路径（'/' 分隔）
    path: String,
    kind: &'static str,
    size: Option<u64>,
    /// 修改时间（Unix 秒）——排序依据，也便于模型判断新旧
    mtime: Option<u64>,
}

pub fn create_glob_tool(root_path: Arc<PathBuf>, extra_read_roots: Vec<PathBuf>) -> FunctionTool {
    // 允许根：工作区根 + 额外只读根（skill 目录等），对齐 shell_command 只读可见范围
    let mut roots: Vec<PathBuf> = vec![root_path.as_ref().clone()];
    roots.extend(extra_read_roots);
    let roots = Arc::new(roots);
    FunctionTool::new(
        "glob",
        "Find files by glob pattern in the workspace (e.g. \"**/*.rs\", \"src/**/*.ts\", \"*.json\"). Returns matching paths sorted by modification time (oldest first). Cheaper than listing directories — use this to locate files, then read_file the ones you need. `*` does not cross directory boundaries; `**` matches any depth. At most 1000 results are returned.",
        move |_ctx: Arc<dyn ToolContext>, args: Value| {
            let roots = roots.clone();
            async move {
                let p: GlobParams = match serde_json::from_value(args) {
                    Ok(v) => v,
                    Err(e) => {
                        return Ok(json!({ "ok": false, "error": format!("invalid arguments: {e}") }))
                    }
                };
                let rel = p.path.as_deref().unwrap_or(".").trim();
                let Some(root) = super::match_safe_root(&roots, rel) else {
                    return Ok(json!({ "ok": false, "error": "path is outside the workspace (and not in any read-only root)" }));
                };
                Ok(glob_impl(root, &p))
            }
        },
    )
    .with_parameters_schema::<GlobParams>()
}

pub fn glob_impl(root: &Path, p: &GlobParams) -> Value {
    let rel = p.path.as_deref().unwrap_or(".").trim();
    let search_root = match super::resolve_safe_path(root, rel) {
        Ok(x) => x,
        Err(e) => return json!({ "ok": false, "error": e }),
    };

    let pattern = p.pattern.trim();
    if pattern.is_empty() {
        return json!({ "ok": false, "error": "pattern must not be empty" });
    }
    let glob_re = match super::grep::compile_glob(pattern) {
        Ok(r) => r,
        Err(e) => return json!({ "ok": false, "error": e }),
    };

    let mut entries: Vec<GlobEntry> = Vec::new();
    let mut truncated = false;
    let mut stack = vec![search_root.clone()];
    while let Some(dir) = stack.pop() {
        // 已达上限：整体退出，不再逐目录空转 read_dir（内层 break 只退出当前 for）
        if entries.len() >= MAX_MATCHES {
            truncated = true;
            break;
        }
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            if entries.len() >= MAX_MATCHES {
                truncated = true;
                break;
            }
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            // 跳过隐藏目录 / .git / node_modules / target（与 grep 扫描范围一致）
            if name.starts_with('.') || name == "node_modules" || name == "target" {
                continue;
            }
            // 安全：symlink_metadata 不跟随符号链接，防止逃逸/循环
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if meta.file_type().is_symlink() {
                continue;
            }
            let is_dir = meta.is_dir();
            let rel_path = path
                .strip_prefix(&search_root)
                .ok()
                .map(|x| x.to_string_lossy().replace('\\', "/"))
                .unwrap_or_else(|| name.clone());
            // 目录无条件入栈继续下钻（匹配只决定是否收录，不决定是否遍历），
            // 否则 "*.rs" 这类不命中目录名的 pattern 永远走不进子目录
            if is_dir {
                stack.push(path.clone());
            }
            // glob 匹配（统一 '/' 分隔；目录追加以命中「目录前缀」型 pattern）
            let candidate = if is_dir {
                format!("{rel_path}/")
            } else {
                rel_path.clone()
            };
            if !super::grep::glob_matches(&glob_re, &rel_path) && !glob_re.is_match(&candidate) {
                continue;
            }
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs());
            entries.push(GlobEntry {
                path: rel_path,
                kind: if is_dir { "dir" } else { "file" },
                size: if is_dir { None } else { Some(meta.len()) },
                mtime,
            });
        }
    }

    // 按修改时间升序（对齐 Claude Code Glob）；取不到 mtime 的排最后，路径序稳定。
    // 注意 Option<u64> 的 Ord 是 None < Some，需显式把 None 压到末尾。
    entries.sort_by(|a, b| {
        a.mtime
            .is_none()
            .cmp(&b.mtime.is_none())
            .then_with(|| a.mtime.cmp(&b.mtime))
            .then_with(|| a.path.cmp(&b.path))
    });

    json!({
        "ok": true,
        "pattern": pattern,
        "path": rel,
        "matches": entries,
        "total_matches": entries.len(),
        "truncated": truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::code::tests_helpers::TmpWs;

    fn params(pattern: &str) -> GlobParams {
        GlobParams {
            pattern: pattern.into(),
            path: None,
        }
    }

    #[test]
    fn matches_by_basename_pattern() {
        // "*.rs" 应命中任意深度的 .rs 文件（basename 匹配，对齐 rg --glob）
        let ws = TmpWs::new();
        let root = ws.canon();
        let r = glob_impl(&root, &params("*.rs"));
        assert_eq!(r["ok"], true);
        let paths: Vec<String> = r["matches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["path"].as_str().unwrap().to_string())
            .collect();
        assert!(paths.iter().any(|p| p == "main.rs"), "paths={paths:?}");
        assert!(paths.iter().any(|p| p == "src/lib.rs"), "paths={paths:?}");
    }

    #[test]
    fn matches_with_directory_prefix() {
        // "src/**/*.rs" 命中 src 下任意深度（含直接子文件），不命中顶层
        let ws = TmpWs::new();
        let root = ws.canon();
        let r = glob_impl(&root, &params("src/**/*.rs"));
        let paths: Vec<String> = r["matches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["path"].as_str().unwrap().to_string())
            .collect();
        assert!(paths.iter().any(|p| p == "src/lib.rs"));
        assert!(paths.iter().any(|p| p == "src/sub/mod.rs"));
        assert!(!paths.iter().any(|p| p == "main.rs"));
    }

    #[test]
    fn returns_kind_and_size() {
        let ws = TmpWs::new();
        let root = ws.canon();
        let r = glob_impl(&root, &params("src"));
        assert_eq!(r["ok"], true);
        let m = &r["matches"][0];
        assert_eq!(m["kind"], "dir");
        assert_eq!(m["path"], "src");
    }

    #[test]
    fn sorts_by_mtime_ascending() {
        // 先写旧文件、sleep 后写新文件：结果应旧→新（对齐 Claude Code Glob）
        let ws = TmpWs::new();
        ws.write("old_file.rs", "a");
        std::thread::sleep(std::time::Duration::from_millis(1100));
        ws.write("new_file.rs", "b");
        let root = ws.canon();
        let r = glob_impl(&root, &params("*_file.rs"));
        assert_eq!(r["ok"], true);
        let paths: Vec<&str> = r["matches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["path"].as_str().unwrap())
            .collect();
        assert_eq!(paths, vec!["old_file.rs", "new_file.rs"], "应按 mtime 升序");
    }

    #[test]
    fn skips_hidden_and_ignored_dirs() {
        let ws = TmpWs::new();
        ws.write(".git/config", "x");
        ws.write("node_modules/pkg.js", "x");
        ws.write("target/out.js", "x");
        ws.write("visible.js", "x");
        let root = ws.canon();
        let r = glob_impl(&root, &params("**/*"));
        let paths: Vec<String> = r["matches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["path"].as_str().unwrap().to_string())
            .collect();
        assert!(paths.iter().any(|p| p == "visible.js"));
        assert!(!paths.iter().any(|p| p.starts_with(".git")));
        assert!(!paths.iter().any(|p| p.starts_with("node_modules")));
        assert!(!paths.iter().any(|p| p.starts_with("target")));
    }

    #[test]
    fn empty_pattern_errors() {
        let ws = TmpWs::new();
        let root = ws.canon();
        let r = glob_impl(&root, &params("  "));
        assert_eq!(r["ok"], false);
        assert!(r["error"].as_str().unwrap().contains("empty"));
    }

    #[test]
    fn scoped_to_subdirectory() {
        let ws = TmpWs::new();
        let root = ws.canon();
        let r = glob_impl(
            &root,
            &GlobParams {
                pattern: "*.rs".into(),
                path: Some("src".into()),
            },
        );
        let paths: Vec<String> = r["matches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["path"].as_str().unwrap().to_string())
            .collect();
        // 相对搜索根（src），不含顶层 main.rs
        assert!(paths.iter().any(|p| p == "lib.rs"));
        assert!(!paths.iter().any(|p| p == "main.rs"));
    }

    #[test]
    #[cfg(unix)]
    fn skips_symlinks() {
        use std::os::unix::fs::symlink;
        let ws = TmpWs::new();
        symlink("/etc", ws.root.join("evil")).ok();
        let root = ws.canon();
        let r = glob_impl(&root, &params("**/*"));
        let paths: Vec<String> = r["matches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["path"].as_str().unwrap().to_string())
            .collect();
        assert!(!paths.iter().any(|p| p.starts_with("evil")));
    }

    #[test]
    fn rejects_path_outside_workspace() {
        let ws = TmpWs::new();
        let root = ws.canon();
        let r = glob_impl(
            &root,
            &GlobParams {
                pattern: "*.rs".into(),
                path: Some("../".into()),
            },
        );
        assert_eq!(r["ok"], false);
    }
}
