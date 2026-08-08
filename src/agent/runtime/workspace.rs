//! 工作区模式（WorkspaceMode）— Agent 沙箱编排的共享基础设施。
//!
//! 自定义助手（含 shell_command 工具）的沙箱编排（`custom` / `orchestration` / `sse`）复用。

use std::path::{Path, PathBuf};

/// 工作区模式 — 决定 Agent 的能力档位
///
/// - [`WorkspaceMode::ChatOnly`][]: 纯对话，无文件工具
/// - [`WorkspaceMode::Sandbox`][]: session 级临时沙箱目录
#[derive(Debug, Clone)]
pub enum WorkspaceMode {
    /// 未启用沙箱 → 纯对话
    ChatOnly,
    /// session 级临时沙箱目录
    Sandbox(PathBuf),
}

impl WorkspaceMode {
    /// 工具实际操作的根目录（ChatOnly 为 None）
    pub fn root_path(&self) -> Option<&Path> {
        match self {
            WorkspaceMode::ChatOnly => None,
            WorkspaceMode::Sandbox(p) => Some(p),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_mode_root_path() {
        // ChatOnly 无根目录
        assert!(WorkspaceMode::ChatOnly.root_path().is_none());
        // Sandbox 有根目录
        let sandbox = WorkspaceMode::Sandbox(PathBuf::from("/tmp/sbx"));
        assert_eq!(sandbox.root_path(), Some(Path::new("/tmp/sbx")));
    }
}
