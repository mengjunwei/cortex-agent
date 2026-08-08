//! 命令执行的权限策略领域模型 —— 对齐 codex 的 `sandbox_mode` + `approval_policy`。
//!
//! B 阶段先作为「策略层」：[`SandboxMode`]/[`ApprovalPolicy`] 决定 shell 命令的分类与审批
//! 流程，并驱动 system prompt 的权限说明注入（接上 `prompts::permissions` 模板）。
//! C/D 阶段再把它升级为真 OS 强制（Linux/macOS via adk-sandbox、Windows 原生受限令牌）。

use serde::{Deserialize, Serialize};

/// 文件系统沙箱模式（对齐 codex `sandbox_mode` 三档）。
///
/// - B 阶段：决定命令分类（read-only 下写命令归入审批/阻断）。
/// - C/D 阶段：映射为真 OS 强制（bwrap / seatbelt / Windows 受限令牌）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxMode {
    /// 只读：禁止任何写/副作用命令。
    ///
    /// 注意：B 阶段的策略层（`shell_command::decide_with_policy`）仅在 NeedsPrompt 分支拦截
    /// 写命令，用户 Allow 规则 / safelist(Allowed) 路径不二次裁决——故 ReadOnly 在策略层是
    /// best-effort（Windows 无 OS 兜底时尤甚，safelist 里的 cargo build 等仍可能执行）。
    /// 真正的 OS 级只读强制由 C（Linux/macOS bwrap/seatbelt）/ D（Windows 原生）阶段保证。
    ReadOnly,
    /// 工作区写：允许写工作目录 + writable_roots，其余写需审批（默认，接近现状）。
    #[default]
    WorkspaceWrite,
    /// 完全访问：无沙箱约束（仍受 dangerous 硬编码 + 审批策略约束）。
    DangerFullAccess,
}

impl SandboxMode {
    /// 返回对齐 codex 的 `sandbox_mode` 字符串标识（用于 prompt 模板选择与日志）。
    pub fn codex_id(&self) -> &'static str {
        match self {
            SandboxMode::ReadOnly => "read-only",
            SandboxMode::WorkspaceWrite => "workspace-write",
            SandboxMode::DangerFullAccess => "danger-full-access",
        }
    }

    /// 由 codex 标识串反解（与 [`codex_id`] 对称），从 DB / 接口入参还原枚举；未知串返回 None。
    pub fn from_codex_id(s: &str) -> Option<Self> {
        match s {
            "read-only" => Some(SandboxMode::ReadOnly),
            "workspace-write" => Some(SandboxMode::WorkspaceWrite),
            "danger-full-access" => Some(SandboxMode::DangerFullAccess),
            _ => None,
        }
    }

    /// 是否允许写副作用（read-only 为 false，其余为 true）。
    pub fn allows_write(&self) -> bool {
        !matches!(self, SandboxMode::ReadOnly)
    }
}

/// 命令审批策略（对齐 codex `approval_policy` 四档）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ApprovalPolicy {
    /// 从不审批：需审批的命令直接拒绝并回填模型（never）。
    Never,
    /// 模型主动请求审批时才审批（on-request）。
    OnRequest,
    /// 规则匹配 + 模型请求组合审批（on-request-rule-request-permission）。
    OnRequestRuleRequestPermission,
    /// 除 safelist 只读命令外，其余都需审批（unless-trusted，默认，接近现状）。
    #[default]
    UnlessTrusted,
}

impl ApprovalPolicy {
    /// 返回对齐 codex 的 `approval_policy` 字符串标识。
    pub fn codex_id(&self) -> &'static str {
        match self {
            ApprovalPolicy::Never => "never",
            ApprovalPolicy::OnRequest => "on-request",
            ApprovalPolicy::OnRequestRuleRequestPermission => "on-request-rule-request-permission",
            ApprovalPolicy::UnlessTrusted => "unless-trusted",
        }
    }

    /// 由 codex 标识串反解（与 [`codex_id`] 对称），从 DB / 接口入参还原枚举；未知串返回 None。
    pub fn from_codex_id(s: &str) -> Option<Self> {
        match s {
            "never" => Some(ApprovalPolicy::Never),
            "on-request" => Some(ApprovalPolicy::OnRequest),
            "on-request-rule-request-permission" => {
                Some(ApprovalPolicy::OnRequestRuleRequestPermission)
            }
            "unless-trusted" => Some(ApprovalPolicy::UnlessTrusted),
            _ => None,
        }
    }
}

/// 一个会话/助手的完整权限策略（沙箱模式 + 审批策略 + 网络开关）。
///
/// 用于在 config → 会话 → Agent → shell 工具 / prompt 注入 之间传递。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PermissionPolicy {
    pub sandbox_mode: SandboxMode,
    pub approval_policy: ApprovalPolicy,
    /// 是否允许命令访问网络（填充 permissions 模板的 `{{ network_access }}` 占位符）。
    #[serde(default)]
    pub network_access: bool,
}

impl PermissionPolicy {
    pub fn new(
        sandbox_mode: SandboxMode,
        approval_policy: ApprovalPolicy,
        network_access: bool,
    ) -> Self {
        Self {
            sandbox_mode,
            approval_policy,
            network_access,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandbox_mode_default_is_workspace_write() {
        assert_eq!(SandboxMode::default(), SandboxMode::WorkspaceWrite);
    }

    #[test]
    fn approval_policy_default_is_unless_trusted() {
        assert_eq!(ApprovalPolicy::default(), ApprovalPolicy::UnlessTrusted);
    }

    #[test]
    fn sandbox_mode_codex_ids() {
        assert_eq!(SandboxMode::ReadOnly.codex_id(), "read-only");
        assert_eq!(SandboxMode::WorkspaceWrite.codex_id(), "workspace-write");
        assert_eq!(
            SandboxMode::DangerFullAccess.codex_id(),
            "danger-full-access"
        );
    }

    #[test]
    fn sandbox_mode_allows_write() {
        assert!(!SandboxMode::ReadOnly.allows_write());
        assert!(SandboxMode::WorkspaceWrite.allows_write());
        assert!(SandboxMode::DangerFullAccess.allows_write());
    }

    #[test]
    fn approval_policy_codex_ids() {
        assert_eq!(ApprovalPolicy::Never.codex_id(), "never");
        assert_eq!(ApprovalPolicy::UnlessTrusted.codex_id(), "unless-trusted");
    }

    #[test]
    fn sandbox_mode_from_codex_id_roundtrip() {
        for mode in [
            SandboxMode::ReadOnly,
            SandboxMode::WorkspaceWrite,
            SandboxMode::DangerFullAccess,
        ] {
            assert_eq!(SandboxMode::from_codex_id(mode.codex_id()), Some(mode));
        }
        assert_eq!(SandboxMode::from_codex_id("bogus"), None);
    }

    #[test]
    fn approval_policy_from_codex_id_roundtrip() {
        for pol in [
            ApprovalPolicy::Never,
            ApprovalPolicy::OnRequest,
            ApprovalPolicy::OnRequestRuleRequestPermission,
            ApprovalPolicy::UnlessTrusted,
        ] {
            assert_eq!(ApprovalPolicy::from_codex_id(pol.codex_id()), Some(pol));
        }
        assert_eq!(ApprovalPolicy::from_codex_id("bogus"), None);
    }

    #[test]
    fn serde_kebab_case_roundtrip() {
        let json = serde_json::to_string(&SandboxMode::ReadOnly).unwrap();
        assert_eq!(json, "\"read-only\"");
        let m: SandboxMode = serde_json::from_str("\"workspace-write\"").unwrap();
        assert_eq!(m, SandboxMode::WorkspaceWrite);
    }
}
