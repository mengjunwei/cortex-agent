//! `read_file` 工具 — 读取工作区内文件（带行号；大文件降级）。
//!
//! 设计参考 Zed `crates/agent/src/tools/read_file_tool.rs`：
//! - 输出按 `cat -n` 风格加行号前缀（`{line_no}→{content}`）
//! - 支持 start_line/end_line 分段读取
//! - 大文件（超 MAX_LINES 且未指定范围）降级为头尾截断 + 行数提示
//!   （Zed 用 outline.rs 的符号大纲，Web 后端无 LSP 故用简化降级）

use std::path::{Path, PathBuf};
use std::sync::Arc;

use adk_rust::ToolContext;
use adk_rust::serde_json::{Value, json};
use adk_rust::tool::FunctionTool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// 单次读取的最大行数（超过则要求分段或降级）
const READ_FILE_MAX_LINES: usize = 2000;
/// 降级时返回的头部行数
const OUTLINE_HEAD_LINES: usize = 100;
/// 降级时返回的尾部行数
const OUTLINE_TAIL_LINES: usize = 50;
/// 单次返回正文的字节封顶（与 TruncatingToolset 默认值一致）。
///
/// read_file 未走 TruncatingToolset 包装（它是单 FunctionTool 而非 toolset），
/// 故在此自行封顶：带 start_line/end_line 的范围读取会原样返回整段，
/// 长行文件可瞬时返回数百 KB 直送模型触发提供商 500。超限时头尾保留中间省略。
const READ_FILE_MAX_OUTPUT_BYTES: usize = crate::tools::truncating::DEFAULT_MAX_OUTPUT_BYTES;

/// read_file 工具参数
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct ReadFileParams {
    /// 相对工作区根目录的文件路径，如 "src/main.rs"
    pub path: String,
    /// 起始行号（1-based，含），不填则从头读
    #[serde(default)]
    pub start_line: Option<u32>,
    /// 结束行号（1-based，含），不填则读到末尾
    #[serde(default)]
    pub end_line: Option<u32>,
}

pub fn create_read_file_tool(
    root_path: Arc<PathBuf>,
    extra_read_roots: Vec<PathBuf>,
) -> FunctionTool {
    // 允许根：工作区根（可写）+ 额外只读根（skill 目录等，与 shell_command 的 readonly_extra 对齐，
    // 让 read_file 能读 skill 脚本/文档，否则模型只能改用 head/cat 绕开）。
    let mut roots: Vec<PathBuf> = vec![root_path.as_ref().clone()];
    roots.extend(extra_read_roots);
    let roots = Arc::new(roots);
    FunctionTool::new(
        "read_file",
        "Read a file in the workspace (with line numbers). For large files, read in segments using `start_line`/`end_line`. Paths are relative to the workspace root.",
        move |_ctx: Arc<dyn ToolContext>, args: Value| {
            let roots = roots.clone();
            async move {
                let p: ReadFileParams = match serde_json::from_value(args) {
                    Ok(v) => v,
                    Err(e) => return Ok(json!({ "ok": false, "error": format!("invalid arguments: {e}") })),
                };
                let Some(root) = super::match_safe_root(&roots, &p.path) else {
                    return Ok(json!({ "ok": false, "error": "path is outside the workspace (and not in any read-only root)" }));
                };
                Ok(read_file_impl(root, &p).await)
            }
        },
    )
    .with_parameters_schema::<ReadFileParams>()
}

/// 纯函数实现（便于单测，不依赖 adk 闭包）
pub async fn read_file_impl(root: &Path, p: &ReadFileParams) -> Value {
    let abs = match super::resolve_safe_path(root, &p.path) {
        Ok(x) => x,
        Err(e) => return json!({ "ok": false, "error": e }),
    };

    let content = match read_text_safe(&abs).await {
        Ok(c) => c,
        Err(ReadErr::Io(e)) => {
            return json!({ "ok": false, "error": format!("failed to read file: {e}") });
        }
        Err(ReadErr::Binary { size }) => {
            // 二进制/非 UTF-8：给结构化信号而非笼统"读取失败"——否则模型分不清
            // "文件缺失/无权限"与"这是图片/二进制"，常进入重试循环。
            return json!({
                "ok": false,
                "binary": true,
                "error": "file is binary or not valid UTF-8 and cannot be read as text",
                "size": size,
                "path": p.path,
            });
        }
    };

    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len() as u32;

    // 空文件：直接返回成功 + 空内容。走下方 start/end 逻辑会算出 start(1) > end(0)，
    // 报 "start_line must not be greater than end_line" 的误导性错误。
    if total == 0 {
        return json!({
            "ok": true,
            "path": p.path,
            "total_lines": 0,
            "range": [0u32, 0u32],
            "truncated": false,
            "content": "",
        });
    }

    // 无行范围 + 大文件 → 降级
    if p.start_line.is_none() && p.end_line.is_none() && total as usize > READ_FILE_MAX_LINES {
        return outline_view(&p.path, &lines, total);
    }

    let start = p.start_line.unwrap_or(1).max(1);
    let end = p.end_line.unwrap_or(total).min(total);
    if start > total {
        return json!({
            "ok": false,
            "error": format!("start_line {start} exceeds the total number of lines ({total})")
        });
    }
    if start > end {
        return json!({
            "ok": false,
            "error": format!("start_line {start} must not be greater than end_line {end}")
        });
    }

    let body = format_range(&lines, start, end);
    let (body, byte_capped) = cap_body(body);
    json!({
        "ok": true,
        "path": p.path,
        "total_lines": total,
        "range": [start, end],
        "truncated": (end - start + 1) as usize >= READ_FILE_MAX_LINES || byte_capped,
        "content": body,
    })
}

/// 按行号格式化指定范围（`{line_no}→{content}`）
fn format_range(lines: &[&str], start: u32, end: u32) -> String {
    let mut out = String::new();
    for (i, line) in lines
        .iter()
        .enumerate()
        .skip(start as usize - 1)
        .take(end as usize - start as usize + 1)
    {
        // i 是 0-based 全局索引，行号 = i + 1
        out.push_str(&format!("{}→{}\n", i + 1, line));
    }
    out.trim_end_matches('\n').to_string()
}

/// 读取失败的归因：IO 错误 vs 二进制/非 UTF-8（后者给模型不同的信号）。
enum ReadErr {
    Io(std::io::Error),
    Binary { size: usize },
}

/// 读取文件为 UTF-8 文本；非 UTF-8（绝大多数二进制文件）归因为 [`ReadErr::Binary`，
/// 让调用方给出结构化信号，而非笼统的"读取失败"。
async fn read_text_safe(abs: &Path) -> Result<String, ReadErr> {
    let bytes = tokio::fs::read(abs).await.map_err(ReadErr::Io)?;
    match String::from_utf8(bytes) {
        Ok(s) => Ok(s),
        Err(e) => Err(ReadErr::Binary {
            size: e.into_bytes().len(),
        }),
    }
}

/// 把正文封顶到 [`READ_FILE_MAX_OUTPUT_BYTES`]：超限时头尾保留、中间省略。
/// 返回 (封顶后正文, 是否触发了字节封顶)。
fn cap_body(body: String) -> (String, bool) {
    if body.len() <= READ_FILE_MAX_OUTPUT_BYTES {
        (body, false)
    } else {
        (
            crate::tools::truncating::middle_truncate(&body, READ_FILE_MAX_OUTPUT_BYTES),
            true,
        )
    }
}

/// 大文件降级视图：头 N 行 + 省略提示 + 尾 M 行
fn outline_view(path: &str, lines: &[&str], total: u32) -> Value {
    let head_lines = OUTLINE_HEAD_LINES.min(total as usize) as u32;
    let head = format_range(lines, 1, head_lines);
    let tail_start = total.saturating_sub(OUTLINE_TAIL_LINES as u32) + 1;
    let tail = if tail_start <= head_lines {
        String::new()
    } else {
        format_range(lines, tail_start, total)
    };
    let body = if tail.is_empty() {
        head
    } else {
        let omitted = (total as usize).saturating_sub(OUTLINE_HEAD_LINES + OUTLINE_TAIL_LINES);
        format!(
            "{head}\n\n... [{omitted} lines omitted of {total} total; use start_line/end_line to read in segments] ...\n\n{tail}"
        )
    };
    // outline 头尾各 100/50 行，若单行极长仍可能超字节预算，兜底封顶
    let (body, _) = cap_body(body);
    json!({
        "ok": true,
        "path": path,
        "total_lines": total,
        "range": [1u32, total],
        "truncated": true,
        "outline": true,
        "content": body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::code::tests_helpers::TmpWs;

    #[tokio::test]
    async fn reads_file_with_line_numbers() {
        let ws = TmpWs::new();
        let root = ws.canon();
        let r = read_file_impl(
            &root,
            &ReadFileParams {
                path: "src/lib.rs".into(),
                start_line: None,
                end_line: None,
            },
        )
        .await;
        assert_eq!(r["ok"], true);
        let content = r["content"].as_str().unwrap();
        assert!(content.contains("→"), "应包含行号前缀，实际: {content}");
        assert!(content.contains("pub fn x()"));
    }

    #[tokio::test]
    async fn reads_specific_line_range() {
        let ws = TmpWs::new();
        ws.write("multi.rs", "line1\nline2\nline3\nline4\nline5\n");
        let root = ws.canon();
        let r = read_file_impl(
            &root,
            &ReadFileParams {
                path: "multi.rs".into(),
                start_line: Some(2),
                end_line: Some(4),
            },
        )
        .await;
        assert_eq!(r["ok"], true);
        let content = r["content"].as_str().unwrap();
        assert!(content.contains("line2"));
        assert!(content.contains("line4"));
        assert!(!content.contains("line1"));
        assert!(!content.contains("line5"));
        let range = r["range"].as_array().unwrap();
        assert_eq!(range[0], 2);
        assert_eq!(range[1], 4);
    }

    #[tokio::test]
    async fn rejects_path_outside_workspace() {
        let ws = TmpWs::new();
        let root = ws.canon();
        let r = read_file_impl(
            &root,
            &ReadFileParams {
                path: "../../../etc/passwd".into(),
                start_line: None,
                end_line: None,
            },
        )
        .await;
        assert_eq!(r["ok"], false, "应拒绝越界路径");
    }

    #[tokio::test]
    async fn reports_missing_file() {
        let ws = TmpWs::new();
        let root = ws.canon();
        let r = read_file_impl(
            &root,
            &ReadFileParams {
                path: "nonexistent.rs".into(),
                start_line: None,
                end_line: None,
            },
        )
        .await;
        assert_eq!(r["ok"], false);
        assert!(r["error"].as_str().unwrap().contains("failed to read"));
    }

    #[tokio::test]
    async fn rejects_start_beyond_total() {
        let ws = TmpWs::new();
        let root = ws.canon();
        let r = read_file_impl(
            &root,
            &ReadFileParams {
                path: "src/lib.rs".into(),
                start_line: Some(9999),
                end_line: None,
            },
        )
        .await;
        assert_eq!(r["ok"], false);
    }

    #[tokio::test]
    async fn outlines_large_file_when_no_range_given() {
        let ws = TmpWs::new();
        // 构造 2500 行大文件
        let big: String = (1..=2500).map(|i| format!("line{i}\n")).collect();
        ws.write("big.rs", &big);
        let root = ws.canon();
        let r = read_file_impl(
            &root,
            &ReadFileParams {
                path: "big.rs".into(),
                start_line: None,
                end_line: None,
            },
        )
        .await;
        assert_eq!(r["ok"], true);
        assert_eq!(r["outline"], true);
        assert_eq!(r["truncated"], true);
        assert_eq!(r["total_lines"], 2500);
        let content = r["content"].as_str().unwrap();
        assert!(content.contains("omitted"), "应包含降级提示");
    }

    #[tokio::test]
    async fn reports_binary_file_signal() {
        // 含 NUL/非法 UTF-8 字节的文件应被识别为二进制，返回结构化信号（而非把乱码当文本读）
        let ws = TmpWs::new();
        let root = ws.canon();
        std::fs::write(
            root.join("bin.dat"),
            [0x89u8, 0x50, 0x4e, 0x47, 0x00, 0x0d, 0xff, 0xfe],
        )
        .unwrap();
        let r = read_file_impl(
            &root,
            &ReadFileParams {
                path: "bin.dat".into(),
                start_line: None,
                end_line: None,
            },
        )
        .await;
        assert_eq!(r["ok"], false);
        assert_eq!(r["binary"], true, "应标记为二进制: {r}");
        assert!(r["error"].as_str().unwrap().contains("binary"));
        assert_eq!(r["size"], 8);
    }
}
