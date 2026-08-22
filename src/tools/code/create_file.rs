//! `create_file` 工具 — 在工作区内创建、覆盖或追加文件。
//!
//! 设计参考 Zed `crates/agent/src/tools/edit_file_tool.rs` 的写入部分与
//! Claude Code 的 Write/append 语义：
//! - 自动创建父目录
//! - overwrite=false（默认）且文件已存在时报错（防误覆盖）
//! - `append=true`：向已存在文件**末尾追加**（文件不存在则等同新建）；
//!   对齐 shell `>>`，避免模型为追加一行而整文件重写
//! - 原子写入（先 .cortex-tmp 再 rename；追加路径读旧内容后同原子写）
//! - 覆盖/追加均返回 unified diff（新建文件全 `+` 行），前端渲染红绿 diff 视图

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use adk_rust::ToolContext;
use adk_rust::serde_json::{Value, json};
use adk_rust::tool::FunctionTool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex as TokioMutex;

/// 同文件写锁表（canonical path → tokio Mutex）。
///
/// append 是 read-modify-write：单 agent 内副作用工具串行无碍，但 spawn_agent 的
/// 并行子 agent 共享同一 workspace，两个子 agent 并发 append 同文件会都读到同一
/// 旧内容、后写覆盖先写，静默丢一次追加。按路径加进程内互斥锁，把整个
/// read→拼→write 区间串行化。临界区含 async IO，故用 tokio::sync::Mutex
/// （锁表自身短暂持锁、无 await，用 std Mutex 即可）。edit_file 的读-改-写同享
/// 此风险，同一张锁表（见 edit_file.rs）。
static FILE_WRITE_LOCKS: OnceLock<Mutex<HashMap<PathBuf, Arc<TokioMutex<()>>>>> = OnceLock::new();

/// 拿指定路径的写锁 clone（锁表自身短暂持锁，路径锁长期持有）。
///
/// key 归一：resolve_safe_path 对「不存在的文件」返回 `canon_root.join(rel)`
/// （未 canonicalize），「已存在的文件」返回 canonicalize 结果（解析了符号链接/
/// 短名），两种来源对同一物理文件可能得到不同 PathBuf → 两把锁，互斥失效
/// （符号链接目录 dirA→dirB：append 走 join、edit 走 canonicalize，key 分裂）。
/// 故锁 key 独立归一：canonicalize 父目录（存在，能解析符号链接）+ 词法清理 +
/// Windows 小写（API 大小写不敏感）。
pub(crate) fn path_write_lock(abs: &Path) -> Arc<TokioMutex<()>> {
    let key = lock_key(abs);
    let table = FILE_WRITE_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = table.lock().expect("file lock table poisoned");
    guard
        .entry(key)
        .or_insert_with(|| Arc::new(TokioMutex::new(())))
        .clone()
}

/// 归一化锁 key：父目录 canonicalize（解析符号链接/8.3 短名）+ 末端文件名词法
/// 归一 + Windows 小写。canonicalize 失败（极端：父目录被并发删建）回退纯词法。
fn lock_key(abs: &Path) -> PathBuf {
    let mut key = match abs.parent() {
        Some(parent) => match parent.canonicalize() {
            Ok(canon_parent) => canon_parent.join(abs.file_name().unwrap_or_default()),
            Err(_) => abs.to_path_buf(),
        },
        None => abs.to_path_buf(),
    };
    key = lexical_clean(key);
    #[cfg(windows)]
    {
        key = PathBuf::from(key.to_string_lossy().to_lowercase());
    }
    key
}

/// 词法清理路径：剥 `.` 分量、折叠 `..`（不触文件系统，纯 Components 迭代）。
fn lexical_clean(p: PathBuf) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct CreateFileParams {
    /// 相对工作区根目录的文件路径
    pub path: String,
    /// 文件内容（append=true 时为要追加的内容）
    pub content: String,
    /// 已存在时是否覆盖（默认 false）
    #[serde(default)]
    pub overwrite: Option<bool>,
    /// 追加模式：向已存在文件末尾追加（不存在则新建）。与 overwrite 互斥。
    #[serde(default)]
    pub append: Option<bool>,
}

pub fn create_create_file_tool(root_path: Arc<PathBuf>) -> FunctionTool {
    let root = root_path.clone();
    FunctionTool::new(
        "create_file",
        "Create a new file in the workspace (parent directories are created automatically). By default refuses to overwrite an existing file; set `overwrite=true` to overwrite, or `append=true` to append to the end of an existing file (creates it if missing). Returns a unified diff of the change. The workspace is the only writable area — create all file artifacts here, and never modify read-only dependencies (skills, system files).",
        move |_ctx: Arc<dyn ToolContext>, args: Value| {
            let root = root.clone();
            async move {
                let p: CreateFileParams = match serde_json::from_value(args) {
                    Ok(v) => v,
                    Err(e) => return Ok(json!({ "ok": false, "error": format!("invalid arguments: {e}") })),
                };
                Ok(create_file_impl(&root, &p).await)
            }
        },
    )
    .with_parameters_schema::<CreateFileParams>()
}

pub async fn create_file_impl(root: &Path, p: &CreateFileParams) -> Value {
    let abs = match super::resolve_safe_write_path(root, &p.path) {
        Ok(x) => x,
        Err(e) => return json!({ "ok": false, "error": e }),
    };

    let overwrite = p.overwrite.unwrap_or(false);
    let append = p.append.unwrap_or(false);
    if overwrite && append {
        return json!({
            "ok": false,
            "error": "overwrite and append are mutually exclusive — pick one",
            "path": p.path,
        });
    }

    // 同文件写锁：串行化并发的 append/覆盖 read→拼→write 区间（见 FILE_WRITE_LOCKS
    // 文档）。临界区含 async IO，tokio::sync::Mutex 守卫可跨 await 持有。
    let lock = path_write_lock(&abs);
    let _guard = lock.lock().await;

    let existed = abs.exists();
    // 已存在时读旧内容：append 的拼接基线 + diff 的旧侧（不存在则 None=新建，diff 全 + 行）。
    // 读失败分两种：
    // - append 需要旧内容做拼接 → 硬失败（无法安全追加）；
    // - overwrite 只需要旧内容做 diff 展示 → 降级为 None（diff 空缺），不阻断写入
    //   （对齐旧行为：overwrite 从不因旧文件不可读而失败，二进制/非 UTF-8 也能覆盖）。
    let original = if existed {
        match tokio::fs::read_to_string(&abs).await {
            Ok(c) => Some(c),
            Err(e) if append => {
                return json!({
                    "ok": false,
                    "error": format!("failed to read existing file (append needs a UTF-8 text file): {e}"),
                    "path": p.path,
                });
            }
            Err(_) => None,
        }
    } else {
        None
    };

    if !overwrite && !append && existed {
        return json!({
            "ok": false,
            "error": "file already exists (set overwrite=true to overwrite, or append=true to append)",
            "path": p.path,
        });
    }

    // 追加模式拼接旧内容（旧内容末尾无换行则补一个，保证追加块从新行开始）
    let updated = if append && let Some(orig) = &original {
        let mut s = String::with_capacity(orig.len() + p.content.len());
        s.push_str(orig);
        if !orig.is_empty() && !orig.ends_with('\n') {
            s.push('\n');
        }
        s.push_str(&p.content);
        s
    } else {
        p.content.clone()
    };

    // 创建父目录
    if let Some(parent) = abs.parent() {
        if let Err(e) = tokio::fs::create_dir_all(parent).await {
            return json!({ "ok": false, "error": format!("failed to create parent directories: {e}") });
        }
    }

    // 原子写入
    if let Err(e) = atomic_write(&abs, &updated).await {
        return json!({ "ok": false, "error": format!("failed to write file: {e}") });
    }

    let lines = updated.lines().count();
    // overwrite 降级读旧内容失败时 original=None，diff 以空为旧侧（呈现为新建）——
    // 标记 partial 提示前端/模型 diff 不完整（旧内容被覆盖的事实不可见）。
    let diff_partial = existed && original.is_none();
    let diff_text =
        super::diff::make_unified_diff(original.as_deref().unwrap_or(""), &updated, &p.path);
    json!({
        "ok": true,
        "path": p.path,
        "bytes": updated.len(),
        "lines": lines,
        "created": !existed,
        "appended": append && existed,
        "diff": diff_text,
        "diff_partial": diff_partial,
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
                append: None,
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
                append: None,
            },
        )
        .await;
        assert_eq!(r["ok"], false);
        assert!(r["error"].as_str().unwrap().contains("already exists"));
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
                append: None,
            },
        )
        .await;
        assert_eq!(r["ok"], true);
        let content = std::fs::read_to_string(root.join("ow.rs")).unwrap();
        assert_eq!(content, "brand new\n");
        // 覆盖应返回旧→新的 unified diff
        let diff = r["diff"].as_str().unwrap();
        assert!(diff.contains("-old"), "覆盖 diff 应含删除行: {diff}");
        assert!(diff.contains("+brand new"), "覆盖 diff 应含新增行: {diff}");
        assert_eq!(r["created"], false);
    }

    #[tokio::test]
    async fn new_file_diff_is_all_additions() {
        // 新建文件：diff 全 + 行，无 - 行
        let ws = TmpWs::new();
        let root = ws.canon();
        let r = create_file_impl(
            &root,
            &CreateFileParams {
                path: "n.txt".into(),
                content: "hello\nworld\n".into(),
                overwrite: None,
                append: None,
            },
        )
        .await;
        assert_eq!(r["ok"], true);
        assert_eq!(r["created"], true);
        let diff = r["diff"].as_str().unwrap();
        assert!(diff.contains("+hello"));
        assert!(diff.contains("+world"));
        // 无行首 - 的删除行（diff 头 `--- a/` 不算删除行）
        assert!(
            !diff
                .lines()
                .any(|l| l.starts_with('-') && !l.starts_with("---")),
            "新建 diff 不应有删除行: {diff}"
        );
    }

    #[tokio::test]
    async fn appends_to_existing_file() {
        let ws = TmpWs::new();
        ws.write("log.txt", "line1\nline2\n");
        let root = ws.canon();
        let r = create_file_impl(
            &root,
            &CreateFileParams {
                path: "log.txt".into(),
                content: "line3\n".into(),
                overwrite: None,
                append: Some(true),
            },
        )
        .await;
        assert_eq!(r["ok"], true, "追加应成功: {r}");
        assert_eq!(r["appended"], true);
        assert_eq!(r["created"], false);
        let content = std::fs::read_to_string(root.join("log.txt")).unwrap();
        assert_eq!(content, "line1\nline2\nline3\n");
        // 追加 diff：只有尾部 + 行，旧行保留为上下文/不动
        let diff = r["diff"].as_str().unwrap();
        assert!(diff.contains("+line3"), "追加 diff 应含新行: {diff}");
        assert!(!diff.contains("-line1"), "追加不应删除旧行: {diff}");
    }

    #[tokio::test]
    async fn append_to_file_without_trailing_newline_inserts_one() {
        // 旧内容末尾无换行 → 追加前补换行，保证追加块从新行开始
        let ws = TmpWs::new();
        ws.write("raw.txt", "no newline");
        let root = ws.canon();
        let r = create_file_impl(
            &root,
            &CreateFileParams {
                path: "raw.txt".into(),
                content: "appended".into(),
                overwrite: None,
                append: Some(true),
            },
        )
        .await;
        assert_eq!(r["ok"], true);
        let content = std::fs::read_to_string(root.join("raw.txt")).unwrap();
        assert_eq!(content, "no newline\nappended");
    }

    #[tokio::test]
    async fn append_creates_missing_file() {
        // append 且文件不存在 → 等同新建
        let ws = TmpWs::new();
        let root = ws.canon();
        let r = create_file_impl(
            &root,
            &CreateFileParams {
                path: "missing.log".into(),
                content: "first\n".into(),
                overwrite: None,
                append: Some(true),
            },
        )
        .await;
        assert_eq!(r["ok"], true);
        assert_eq!(r["created"], true);
        assert_eq!(r["appended"], false);
        let content = std::fs::read_to_string(root.join("missing.log")).unwrap();
        assert_eq!(content, "first\n");
    }

    #[tokio::test]
    async fn rejects_overwrite_and_append_together() {
        let ws = TmpWs::new();
        ws.write("x.txt", "x\n");
        let root = ws.canon();
        let r = create_file_impl(
            &root,
            &CreateFileParams {
                path: "x.txt".into(),
                content: "y".into(),
                overwrite: Some(true),
                append: Some(true),
            },
        )
        .await;
        assert_eq!(r["ok"], false);
        assert!(r["error"].as_str().unwrap().contains("mutually exclusive"));
        // 文件未被修改
        let content = std::fs::read_to_string(root.join("x.txt")).unwrap();
        assert_eq!(content, "x\n");
    }

    #[tokio::test]
    async fn concurrent_appends_do_not_lose_data() {
        // 并行子 agent 共享 workspace 并发 append 同文件：写锁串行化
        // read→拼→write，两次追加都保留（回归点：无锁时后写覆盖先写丢一次）
        let ws = TmpWs::new();
        ws.write("c.log", "start\n");
        let root = ws.canon();
        let p1 = CreateFileParams {
            path: "c.log".into(),
            content: "from-A\n".into(),
            overwrite: None,
            append: Some(true),
        };
        let p2 = CreateFileParams {
            path: "c.log".into(),
            content: "from-B\n".into(),
            overwrite: None,
            append: Some(true),
        };
        let (r1, r2) = tokio::join!(create_file_impl(&root, &p1), create_file_impl(&root, &p2),);
        assert_eq!(r1["ok"], true, "{r1}");
        assert_eq!(r2["ok"], true, "{r2}");
        let content = std::fs::read_to_string(root.join("c.log")).unwrap();
        assert!(content.contains("from-A"), "A 的追加不应丢失: {content}");
        assert!(content.contains("from-B"), "B 的追加不应丢失: {content}");
        assert_eq!(content.lines().count(), 3);
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
                append: None,
            },
        )
        .await;
        assert_eq!(r["ok"], false);
    }

    #[tokio::test]
    async fn rejects_creating_in_git_dir() {
        // 写入版本控制元数据目录会破坏仓库完整性（注入恶意 hook 等），应被拒
        let ws = TmpWs::new();
        let root = ws.canon();
        let r = create_file_impl(
            &root,
            &CreateFileParams {
                path: ".git/hooks/pre-commit".into(),
                content: "evil".into(),
                overwrite: None,
                append: None,
            },
        )
        .await;
        assert_eq!(r["ok"], false);
        assert!(
            r["error"].as_str().unwrap().contains("VCS metadata"),
            "应拒绝写入 .git: {r}"
        );
        // 文件未被创建
        assert!(!root.join(".git").join("hooks").join("pre-commit").exists());
    }

    #[tokio::test]
    async fn allows_writing_gitignore_like_names() {
        // .gitignore / .github 等含 ".git" 前缀但非 VCS 元数据目录的合法路径不应被误伤
        let ws = TmpWs::new();
        let root = ws.canon();
        let r = create_file_impl(
            &root,
            &CreateFileParams {
                path: ".gitignore".into(),
                content: "/target\n".into(),
                overwrite: None,
                append: None,
            },
        )
        .await;
        assert_eq!(r["ok"], true, "应允许写 .gitignore: {r}");
        let content = std::fs::read_to_string(root.join(".gitignore")).unwrap();
        assert!(content.contains("target"));
    }
}
