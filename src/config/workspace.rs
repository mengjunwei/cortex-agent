//! 工作区与 Shell 配置段 — `[workspace]` / `[shell]`

use serde::Deserialize;

/// 代码助手配置（`[workspace]` 段）— session 沙箱与工具开关
///
/// 注：原 Git workspace（clone/pull）基础设施已移除，
/// 代码助手统一使用 session 级临时沙箱目录（`{data_dir}/workspaces/sessions/{session_id}/`）。
/// `data_dir` 由 `AppConfig.data_dir` 统一管理，此处不再保留独立的 data_dir 字段。
#[derive(Debug, Clone, Deserialize)]
pub struct WorkspaceConfig {
    /// 是否为代码助手会话自动创建临时沙箱目录
    ///
    /// 开启后，代码助手会话在首次运行时会在 `{data_dir}/workspaces/sessions/{session_id}/`
    /// 创建一个临时目录作为沙箱，会话删除时同步清理。
    /// 关闭则降级为 T0 聊天档（纯对话，无文件工具）。
    #[serde(default = "default_enable_session_sandbox")]
    pub enable_session_sandbox: bool,
}

fn default_enable_session_sandbox() -> bool {
    true
}

impl Default for WorkspaceConfig {
    fn default() -> Self {
        Self {
            enable_session_sandbox: default_enable_session_sandbox(),
        }
    }
}

/// Shell 命令工具配置（`[shell]` 段）
#[derive(Debug, Clone, Deserialize)]
pub struct ShellConfig {
    /// 命令执行默认超时（毫秒）
    #[serde(default = "default_shell_default_timeout_ms")]
    pub default_timeout_ms: u64,
    /// 命令执行超时上限（毫秒）
    #[serde(default = "default_shell_max_timeout_ms")]
    pub max_timeout_ms: u64,
    /// 审批等待超时（秒，用户不响应时自动拒绝）
    #[serde(default = "default_shell_approval_timeout_secs")]
    pub approval_timeout_secs: u64,
    /// 文件系统沙箱模式（对齐 codex `sandbox_mode`，默认 workspace-write）。
    /// B 阶段为策略层（决定命令分类/审批）；C/D 阶段升级为真 OS 强制。
    #[serde(default)]
    pub sandbox_mode: crate::permissions::SandboxMode,
    /// 命令审批策略（对齐 codex `approval_policy`，默认 unless-trusted）。
    #[serde(default)]
    pub approval_policy: crate::permissions::ApprovalPolicy,
    /// 是否允许命令访问网络（默认 false，对齐 codex：默认禁网，需联网装包时显式开启）。
    /// 填充 prompt 的 `{{ network_access }}` 占位符，由 OS 沙箱强制（Linux bwrap unshare-net）。
    /// 安全：bwrap 网络只能全开/全断（无域级隔离），默认关网是防凭证外泄的关键一道闸——
    /// 真正防外泄依赖「默认关网 + safety 层命令名拦截 + 用户审批」多层叠加。
    #[serde(default)]
    pub network_access: bool,
}

impl ShellConfig {
    /// 聚合为 [`crate::permissions::PermissionPolicy`]，便于在会话/工具间传递。
    pub fn permission_policy(&self) -> crate::permissions::PermissionPolicy {
        crate::permissions::PermissionPolicy::new(
            self.sandbox_mode,
            self.approval_policy,
            self.network_access,
        )
    }
}

fn default_shell_default_timeout_ms() -> u64 {
    30000
}
fn default_shell_max_timeout_ms() -> u64 {
    120000
}
fn default_shell_approval_timeout_secs() -> u64 {
    120
}

impl Default for ShellConfig {
    fn default() -> Self {
        Self {
            default_timeout_ms: default_shell_default_timeout_ms(),
            max_timeout_ms: default_shell_max_timeout_ms(),
            approval_timeout_secs: default_shell_approval_timeout_secs(),
            sandbox_mode: Default::default(),
            approval_policy: Default::default(),
            network_access: false,
        }
    }
}
