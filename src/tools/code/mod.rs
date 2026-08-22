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
//! | `glob` | 按 pattern 匹配文件路径 | 只读 |
//! | `grep` | 正则搜索工作区 | 只读 |
//! | `edit_file` | 替换文件内容（含 diff） | 编辑 |
//! | `create_file` | 创建/覆盖文件 | 编辑 |
//!
//! 设计参考 Zed `crates/agent/src/tools/`（read_file_tool.rs 等），
//! 但适配 Web 服务形态：工具根路径来自会话绑定的 workspace，而非用户本地磁盘。

pub mod create_file;
pub mod diff;
pub mod edit_file;
pub mod glob;
pub mod grep;
pub mod mention;
pub mod read_file;

pub use create_file::create_create_file_tool;
pub use edit_file::create_edit_file_tool;
pub use glob::create_glob_tool;
pub use grep::create_grep_tool;
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
        return Err("path must not be empty".into());
    }
    // 拒绝 Windows 绝对路径（如 C:\）和 UNC 路径
    if rel.len() >= 2 {
        let bytes = rel.as_bytes();
        if bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
            return Err(format!("absolute paths are not allowed: {rel}"));
        }
    }
    if rel.starts_with(r"\\") {
        return Err(format!("UNC paths are not allowed: {rel}"));
    }

    // 词法预检（仅相对路径）：`..` 不得越过工作区根。
    // canonicalize 对「不存在的目标」无能为力，check_partial_symlinks 又不规范化 `..`，
    // 于是 `../escape.rs` 这类指向不存在目标的逃逸会漏过（且行为依赖 temp_dir 父目录是否
    // 符号链接 → 测试 flaky）。这里用深度计数在词法层直接拒：任一时刻 `..` 使深度变负即越界。
    // 绝对路径跳过此检查，交由下方 join+canonicalize+starts_with 判定（join 遇绝对路径会替换根，
    // canonicalize 后不在 canon_root 下即拒）。
    let rel_path = Path::new(rel);
    if !rel_path.is_absolute() {
        let mut depth: i32 = 0;
        for comp in rel_path.components() {
            match comp {
                std::path::Component::ParentDir => {
                    depth -= 1;
                    if depth < 0 {
                        return Err(format!(
                            "path escapes the workspace (.. traversal outside the workspace root): {rel}"
                        ));
                    }
                }
                std::path::Component::Normal(_) => depth += 1,
                std::path::Component::CurDir
                | std::path::Component::RootDir
                | std::path::Component::Prefix(_) => {}
            }
        }
    }

    let canon_root = root_path
        .canonicalize()
        .map_err(|e| format!("invalid workspace root: {e}"))?;

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
        return Err(format!("path is outside the workspace: {rel}"));
    }
    Ok(target)
}

/// 在多个允许根中找到第一个能安全解析 `rel` 的根，返回该根（用于只读工具读取 skill 目录
/// 等额外只读根）。每个根走 [`resolve_safe_path`] 的完整校验；全部拒绝时返回 `None`。
/// 调用方拿到根后再以单根方式调用 `*_impl`（impl 内部会再次校验，幂等）。
///
/// **绝对路径例外**：skill 目录等额外只读根在 workspace 之外，模型从 skill 输出里
/// 拿到的是绝对路径（Windows 上 canonicalize 还带 `\\?\` verbatim 前缀）——
/// [`resolve_safe_path`] 一律拒绝绝对路径的规则对它们失效，此前导致 match_safe_root
/// 在绝对路径输入下永远 None（read_file 读 skill 文件报 outside the workspace）。
/// 这里对绝对路径做专门处理：canonicalize 后落在**某个**允许根之下即命中该根；
/// 不落在任何根下（真越界/符号链接逃逸）返回 None。canonicalize 失败（不存在）
/// 同样逐根做 starts_with 前缀判断。
pub fn match_safe_root<'a>(roots: &'a [PathBuf], rel: &str) -> Option<&'a Path> {
    let rel = rel.trim();
    let is_absolute = Path::new(rel).is_absolute() || is_windows_drive_path(rel);
    if !is_absolute {
        return roots
            .iter()
            .find_map(|r| resolve_safe_path(r, rel).ok().map(|_| r.as_path()));
    }
    // 绝对路径：逐根 canonicalize 匹配（canonicalize 消除 ..、符号链接、
    // Windows verbatim 前缀差异——两侧都 canonicalize 后前缀比较可靠）
    let target = PathBuf::from(rel);
    let canon_target = target.canonicalize().ok();
    roots.iter().find_map(|r| {
        let canon_root = r.canonicalize().ok()?;
        let hit = canon_target
            .as_ref()
            .map(|t| t.starts_with(&canon_root))
            .unwrap_or_else(|| {
                // 目标不存在：词法比较（父目录 canonicalize 后拼目标名）
                target.starts_with(&canon_root)
            });
        hit.then_some(r.as_path())
    })
}

/// Windows 盘符路径判定（`C:\x` / `C:/x`）：`Path::is_absolute` 在部分上下文
/// （UNC/verbatim）已覆盖，但正斜杠盘符形态（`C:/x`）在 Windows 的 is_absolute
/// 为 true、在 Unix 为 false——统一按前缀判定，跨平台行为一致。
fn is_windows_drive_path(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/')
}

/// 受保护的版本控制元数据目录名——写入工具不得改动。
///
/// 对齐 codex `default_read_only_subpaths_for_writable_root`（`.git`/`.codex`/`.agents`
/// 置为只读）：模型若改写 `.git/config` 指向别处 remote、或植入 `.git/hooks/post-checkout`
/// 做持久化，是静默且不可逆的破坏。cortex 只关心 VCS 元数据，故仅列 `.git`/`.hg`/`.svn`。
const PROTECTED_VCS_DIRS: &[&str] = &[".git", ".hg", ".svn"];

/// 写入工具（edit_file/create_file）用的安全路径解析：在 [`resolve_safe_path`] 之上，
/// 额外拒绝落到 VCS 元数据目录内的路径。只读工具仍用 [`resolve_safe_path`] 读取这些目录。
pub fn resolve_safe_write_path(root_path: &Path, rel: &str) -> Result<PathBuf, String> {
    let abs = resolve_safe_path(root_path, rel)?;
    let canon_root = root_path
        .canonicalize()
        .map_err(|e| format!("invalid workspace root: {e}"))?;
    // abs 来自 resolve_safe_path：存在则已 canonicalize，不存在则为 canon_root.join(rel)。
    // 两者都在 canon_root 之下，strip_prefix 取相对部分逐段查 VCS 目录名。
    let rel_part = abs
        .strip_prefix(&canon_root)
        .map_err(|_| "path prefix does not match workspace root".to_string())?;
    for comp in rel_part.components() {
        if let std::path::Component::Normal(seg) = comp {
            if let Some(seg_str) = seg.to_str() {
                if PROTECTED_VCS_DIRS.contains(&seg_str) {
                    return Err(format!(
                        "writing into VCS metadata directory ({seg_str}) is forbidden to protect repository integrity: {rel}"
                    ));
                }
            }
        }
    }
    Ok(abs)
}

/// 对部分路径（祖先存在、末端不存在）做逐段符号链接校验。
fn check_partial_symlinks(canon_root: &Path, joined: &Path) -> Result<(), String> {
    let rel_part = joined
        .strip_prefix(canon_root)
        .map_err(|_| "path prefix does not match workspace root".to_string())?;

    let mut acc = canon_root.to_path_buf();
    for comp in rel_part.components() {
        acc.push(comp);
        if acc.is_symlink() {
            match acc.canonicalize() {
                Ok(real) => {
                    if !real.starts_with(canon_root) {
                        return Err(format!(
                            "symlink escapes the workspace: {}",
                            joined.display()
                        ));
                    }
                    acc = real;
                }
                Err(_) => {
                    // 链接目标不存在，保守拒绝
                    return Err(format!("invalid symlink target: {}", joined.display()));
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

    #[test]
    fn match_safe_root_falls_back_to_extra_read_only_root() {
        // 模拟只读工具读 skill 目录：workspace 根下找不到，但额外只读根（skill_dir）下能找到。
        let ws = TmpWs::new();
        let ws_root = ws.canon();
        // 额外只读根：独立目录，放一个 skill 脚本
        let skill_root = std::env::temp_dir().join(format!(
            "cortex-skill-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(skill_root.join("scripts")).unwrap();
        std::fs::write(skill_root.join("scripts").join("gen.py"), "print(1)\n").unwrap();
        let canon_skill = skill_root.canonicalize().unwrap();
        // 绝对路径指向 skill 脚本：workspace 根解析失败，skill 根应命中
        let abs = canon_skill.join("scripts").join("gen.py");
        let roots = vec![ws_root, canon_skill];
        let matched = match_safe_root(&roots, &abs.to_string_lossy());
        assert!(matched.is_some(), "应在 skill 只读根命中");
        // 相对路径仍在 workspace 根命中（优先级在前）
        let matched_ws = match_safe_root(&roots, "main.rs");
        assert!(matched_ws.is_some());
        // 越界路径：两根都拒绝
        let outside = match_safe_root(&roots, "../escape.rs");
        assert!(outside.is_none(), "应拒绝 .. 逃逸");
        let _ = std::fs::remove_dir_all(&skill_root);
    }

    #[test]
    fn match_safe_root_rejects_absolute_path_outside_all_roots() {
        // 绝对路径但不落在任何允许根下（真越界）→ None
        let ws = TmpWs::new();
        let ws_root = ws.canon();
        let roots = vec![ws_root];
        let outside = std::env::temp_dir().join("cortex-definitely-outside-xyz.py");
        let r = match_safe_root(&roots, &outside.to_string_lossy());
        assert!(r.is_none(), "越界绝对路径应被拒: {r:?}");
    }

    #[test]
    fn match_safe_root_windows_verbatim_prefix_hits_root() {
        // Windows canonicalize 产生 \\?\C:\ verbatim 前缀（此前被 UNC 分支误拒，
        // read_file 读 skill 文件恒失败的根因）。构造：先 canonicalize 拿 verbatim
        // 形态再喂回去，应仍命中工作区根。
        let ws = TmpWs::new();
        ws.write("v.rs", "x\n");
        let ws_root = ws.canon();
        let verbatim = ws_root.join("v.rs").canonicalize().unwrap();
        let roots = vec![ws_root];
        let r = match_safe_root(&roots, &verbatim.to_string_lossy());
        assert!(
            r.is_some(),
            "verbatim 前缀路径应命中根（Windows 复现点）: {verbatim:?}"
        );
    }
}
