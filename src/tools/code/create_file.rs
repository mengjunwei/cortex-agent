//! `create_file` 工具 — 在工作区内创建或覆盖文件。
//!
//! 设计参考 Zed `crates/agent/src/tools/edit_file_tool.rs` 的写入部分：
//! - 自动创建父目录
//! - overwrite=false（默认）且文件已存在时报错（防误覆盖）
//! - 原子写入（先 .cortex-tmp 再 rename）

use std::path::{Path, PathBuf};
use std::sync::Arc;

use adk_rust::ToolContext;
use adk_rust::serde_json::{Value, json};
use adk_rust::tool::FunctionTool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct CreateFileParams {
    /// 相对工作区根目录的文件路径
    pub path: String,
    /// 文件内容
    pub content: String,
    /// 已存在时是否覆盖（默认 false）
    #[serde(default)]
    pub overwrite: Option<bool>,
}

pub fn create_create_file_tool(root_path: Arc<PathBuf>) -> FunctionTool {
    let root = root_path.clone();
    FunctionTool::new(
        "create_file",
        "Create a new file in the workspace (parent directories are created automatically). By default refuses to overwrite an existing file; set `overwrite=true` to overwrite. The workspace is the only writable area — create all file artifacts here, and never modify read-only dependencies (skills, system files).",
        move |_ctx: Arc<dyn ToolContext>, args: Value| {
            let root = root.clone();
            async move {
                let p: CreateFileParams = match serde_json::from_value(args) {
                    Ok(v) => v,
                    Err(e) => return Ok(json!({ "ok": false, "error": format!("参数错误: {e}") })),
                };
                Ok(create_file_impl(&root, &p).await)
            }
        },
    )
    .with_parameters_schema::<CreateFileParams>()
}

pub async fn create_file_impl(root: &Path, p: &CreateFileParams) -> Value {
    let abs = match super::resolve_safe_path(root, &p.path) {
        Ok(x) => x,
        Err(e) => return json!({ "ok": false, "error": e }),
    };

    let overwrite = p.overwrite.unwrap_or(false);
    if !overwrite && abs.exists() {
        return json!({
            "ok": false,
            "error": "文件已存在（设置 overwrite=true 可覆盖）",
            "path": p.path,
        });
    }

    // 创建父目录
    if let Some(parent) = abs.parent() {
        if let Err(e) = tokio::fs::create_dir_all(parent).await {
            return json!({ "ok": false, "error": format!("创建父目录失败: {e}") });
        }
    }

    // 原子写入
    if let Err(e) = atomic_write(&abs, &p.content).await {
        return json!({ "ok": false, "error": format!("写入失败: {e}") });
    }

    let lines = p.content.lines().count();
    json!({
        "ok": true,
        "path": p.path,
        "bytes": p.content.len(),
        "lines": lines,
        "created": true,
    })
}

async fn atomic_write(path: &Path, content: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("cortex-tmp");
    tokio::fs::write(&tmp, content).await?;
    tokio::fs::rename(&tmp, path).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::code::tests_helpers::TmpWs;

    #[tokio::test]
    async fn creates_new_file_with_parent_dirs() {
        let ws = TmpWs::new();
        let root = ws.canon();
        let r = create_file_impl(
            &root,
            &CreateFileParams {
                path: "src/new/mod.rs".into(),
                content: "pub fn hello() {}\n".into(),
                overwrite: None,
            },
        )
        .await;
        assert_eq!(r["ok"], true);
        assert_eq!(r["created"], true);
        assert!(root.join("src").join("new").join("mod.rs").exists());
        let content = std::fs::read_to_string(root.join("src").join("new").join("mod.rs")).unwrap();
        assert!(content.contains("hello"));
    }

    #[tokio::test]
    async fn rejects_existing_without_overwrite() {
        let ws = TmpWs::new();
        ws.write("exists.rs", "old\n");
        let root = ws.canon();
        let r = create_file_impl(
            &root,
            &CreateFileParams {
                path: "exists.rs".into(),
                content: "new\n".into(),
                overwrite: None,
            },
        )
        .await;
        assert_eq!(r["ok"], false);
        assert!(r["error"].as_str().unwrap().contains("已存在"));
        // 原文件未被修改
        let content = std::fs::read_to_string(root.join("exists.rs")).unwrap();
        assert_eq!(content, "old\n");
    }

    #[tokio::test]
    async fn overwrites_when_requested() {
        let ws = TmpWs::new();
        ws.write("ow.rs", "old\n");
        let root = ws.canon();
        let r = create_file_impl(
            &root,
            &CreateFileParams {
                path: "ow.rs".into(),
                content: "brand new\n".into(),
                overwrite: Some(true),
            },
        )
        .await;
        assert_eq!(r["ok"], true);
        let content = std::fs::read_to_string(root.join("ow.rs")).unwrap();
        assert_eq!(content, "brand new\n");
    }

    #[tokio::test]
    async fn rejects_path_outside_workspace() {
        let ws = TmpWs::new();
        let root = ws.canon();
        let r = create_file_impl(
            &root,
            &CreateFileParams {
                path: "../escape.rs".into(),
                content: "x".into(),
                overwrite: None,
            },
        )
        .await;
        assert_eq!(r["ok"], false);
    }
}
