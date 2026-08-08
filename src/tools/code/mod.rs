//! 代码工具模块 — 代码助手专用工具集
//!
//! 提供沙箱内的文件读写、目录浏览、内容搜索、命令执行能力。
//! 根路径由 [`crate::agent::workspace::WorkspaceMode::Sandbox`] 注入，
//! 并经 [`resolve_safe_path`] 做路径安全校验，防止目录逃逸与符号链接攻击。
//!
//! ## 工具清单
//!
//! | 工具 | 说明 | 权限 |
//! |------|------|------|
//! | `read_file` | 读取文件（带行号；大文件降级） | 只读 |
//! | `list_directory` | 列出目录结构 | 只读 |
//! | `grep` | 正则搜索工作区 | 只读 |
//! | `edit_file` | 替换文件内容（含 diff） | 编辑 |
//! | `create_file` | 创建/覆盖文件 | 编辑 |
//!
//! 设计参考 Zed `crates/agent/src/tools/`（read_file_tool.rs 等），
//! 但适配 Web 服务形态：工具根路径来自会话绑定的 workspace，而非用户本地磁盘。

pub mod create_file;
pub mod edit_file;
pub mod grep;
pub mod list_directory;
pub mod mention;
pub mod read_file;

pub use create_file::create_create_file_tool;
pub use edit_file::create_edit_file_tool;
pub use grep::create_grep_tool;
pub use list_directory::create_list_directory_tool;
pub use mention::{MentionRef, render_mentions};
pub use read_file::create_read_file_tool;

use std::path::{Path, PathBuf};

/// 将工作区相对路径解析为受控绝对路径，防目录逃逸与符号链接攻击。
///
/// 校验规则：
/// 1. 拼接 `root_path.join(rel)`；
/// 2. canonicalize 后必须仍在 `root_path` 的 canonicalize 结果之下；
/// 3. 逐段检查符号链接，若链接目标越界则拒绝（防 TOCTOU）。
///
/// 返回：
/// - `Ok(abs)` — 安全的绝对路径
/// - `Err(msg)` — 路径越界或符号链接逃逸
pub fn resolve_safe_path(root_path: &Path, rel: &str) -> Result<PathBuf, String> {
    // 拒绝空路径
    let rel = rel.trim();
    if rel.is_empty() {
        return Err("路径不能为空".into());
    }
    // 拒绝 Windows 绝对路径（如 C:\）和 UNC 路径
    if rel.len() >= 2 {
        let bytes = rel.as_bytes();
        if bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
            return Err(format!("不允许绝对路径: {rel}"));
        }
    }
    if rel.starts_with(r"\\") {
        return Err(format!("不允许 UNC 路径: {rel}"));
    }

    let canon_root = root_path
        .canonicalize()
        .map_err(|e| format!("工作区根目录无效: {e}"))?;

    let joined = canon_root.join(rel);

    // canonicalize 失败（文件不存在）时，用父目录链式校验
    let target = match joined.canonicalize() {
        Ok(t) => t,
        Err(_) => {
            // 对不存在的路径，逐段校验已存在的祖先 + 符号链接
            check_partial_symlinks(&canon_root, &joined)?;
            // 返回 joined 本身（canonicalize 失败说明目标不存在，但路径本身安全）
            return Ok(joined);
        }
    };

    if !target.starts_with(&canon_root) {
        return Err(format!("路径越界（不在工作区内）: {rel}"));
    }
    Ok(target)
}

/// 对部分路径（祖先存在、末端不存在）做逐段符号链接校验。
fn check_partial_symlinks(canon_root: &Path, joined: &Path) -> Result<(), String> {
    let rel_part = joined
        .strip_prefix(canon_root)
        .map_err(|_| "路径前缀不匹配工作区根".to_string())?;

    let mut acc = canon_root.to_path_buf();
    for comp in rel_part.components() {
        acc.push(comp);
        if acc.is_symlink() {
            match acc.canonicalize() {
                Ok(real) => {
                    if !real.starts_with(canon_root) {
                        return Err(format!("符号链接逃逸: {}", joined.display()));
                    }
                    acc = real;
                }
                Err(_) => {
                    // 链接目标不存在，保守拒绝
                    return Err(format!("符号链接目标无效: {}", joined.display()));
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
pub mod tests_helpers {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    /// 临时工作区助手：用 std::env::temp_dir 建目录，返回 (root, cleanup_guard)
    pub struct TmpWs {
        pub root: PathBuf,
    }
    impl Default for TmpWs {
        fn default() -> Self {
            Self::new()
        }
    }

    impl TmpWs {
        pub fn new() -> Self {
            let n = SEQ.fetch_add(1, Ordering::SeqCst);
            let root =
                std::env::temp_dir().join(format!("cortex-ws-test-{}-{}", std::process::id(), n));
            fs::create_dir_all(&root).unwrap();
            fs::write(root.join("main.rs"), "fn main() {}\n").unwrap();
            fs::create_dir_all(root.join("src")).unwrap();
            fs::write(root.join("src").join("lib.rs"), "pub fn x() {}\n").unwrap();
            fs::create_dir_all(root.join("src").join("sub")).unwrap();
            fs::write(root.join("src").join("sub").join("mod.rs"), "// hi\n").unwrap();
            Self { root }
        }
        pub fn canon(&self) -> PathBuf {
            self.root.canonicalize().unwrap()
        }
        pub fn write(&self, rel: &str, content: &str) {
            let p = self.root.join(rel);
            if let Some(parent) = p.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(p, content).unwrap();
        }
    }
    impl Drop for TmpWs {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::tests_helpers::TmpWs;
    use super::*;

    #[test]
    fn resolves_normal_relative_path() {
        let ws = TmpWs::new();
        let root = ws.canon();
        let p = resolve_safe_path(&root, "main.rs").unwrap();
        assert!(p.ends_with("main.rs"));
    }

    #[test]
    fn resolves_nested_relative_path() {
        let ws = TmpWs::new();
        let root = ws.canon();
        let p = resolve_safe_path(&root, "src/lib.rs").unwrap();
        assert!(p.ends_with("lib.rs"));
    }

    #[test]
    fn rejects_dotdot_traversal() {
        let ws = TmpWs::new();
        let root = ws.canon();
        let outside = resolve_safe_path(&root, "../../../etc/passwd");
        assert!(outside.is_err(), "应拒绝 ../ 逃逸");
    }

    #[test]
    fn rejects_absolute_windows_path() {
        let ws = TmpWs::new();
        let root = ws.canon();
        let r = resolve_safe_path(&root, "C:/Windows/System32");
        assert!(r.is_err());
    }

    #[test]
    fn rejects_unc_path() {
        let ws = TmpWs::new();
        let root = ws.canon();
        let r = resolve_safe_path(&root, r"\\server\share\file");
        assert!(r.is_err());
    }

    #[test]
    fn rejects_empty_path() {
        let ws = TmpWs::new();
        let root = ws.canon();
        assert!(resolve_safe_path(&root, "").is_err());
        assert!(resolve_safe_path(&root, "   ").is_err());
    }

    #[test]
    fn allows_nonexistent_file_in_workspace() {
        let ws = TmpWs::new();
        let root = ws.canon();
        let p = resolve_safe_path(&root, "src/new_file.rs").unwrap();
        assert!(p.starts_with(&root));
        assert!(!p.exists());
    }

    #[test]
    fn rejects_nonexistent_path_outside_via_dotdot() {
        let ws = TmpWs::new();
        let root = ws.canon();
        let r = resolve_safe_path(&root, "../outside_new.rs");
        assert!(r.is_err());
    }

    #[test]
    #[cfg(unix)]
    fn rejects_symlink_escape() {
        use std::os::unix::fs::symlink;
        let ws = TmpWs::new();
        let root = &ws.root;
        symlink("/tmp", root.join("evil")).unwrap();
        let canon_root = root.canonicalize().unwrap();
        let r = resolve_safe_path(&canon_root, "evil/secret");
        assert!(r.is_err(), "应拒绝符号链接逃逸");
    }
}
