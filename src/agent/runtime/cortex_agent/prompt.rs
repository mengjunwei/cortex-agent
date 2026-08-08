//! system prompt 分层构建 — stable 前缀（命中厂商缓存）+ volatile 段 + skill 正文 preamble。
//!
//! 层次设计对齐 codex：跨请求字节不变的部分进 stable 前缀以命中厂商 cache_control / 前缀缓存；
//! 每请求变化的部分（时间、@ 提及的 skill 正文）独立成段，不污染 stable。

use adk_rust::{Content, Part};

use crate::domain::permissions::PermissionPolicy;

/// 构建 stable 前缀（跨请求字节不变 → 命中厂商缓存）。
///
/// 层次：
/// 1. instruction — 调用方特化指令（自定义助手=用户人设，内置助手=专业 prompt）；为空则跳过
/// 2. BASE_INSTRUCTION — CortexAgent 固定注入的通用行为基线（始终追加）
/// 3. environment — OS（不含时间，时间移至 volatile 避免击穿缓存前缀）
/// 4. permissions — sandbox_mode + approval_policy
/// 5. skill catalog — 可用 skill 的 name + desc（启动时构建，跨请求稳定）
///
/// 注意：用户 $ 提及的 skill 正文（skill_bodies）**不在此处** —— 它每请求变化，
/// 放进 stable 会击穿缓存。它由 [`build_preamble`] 以独立 user-role 消息注入
/// （对齐 codex body=user 语义，详见 [`build_preamble`] 文档）。
pub(super) fn build_stable_prefix(
    instruction: &Option<String>,
    memory_block: &Option<String>,
    skill_catalog: &Option<String>,
    policy: PermissionPolicy,
) -> String {
    let mut layers: Vec<String> = Vec::new();

    if let Some(i) = instruction {
        if !i.trim().is_empty() {
            layers.push(i.clone());
        }
    }
    // 跨会话记忆（用户的习惯/坑）：紧贴人设注入。同一用户+助手在单会话内稳定 → 不击穿缓存。
    if let Some(m) = memory_block {
        if !m.trim().is_empty() {
            layers.push(m.clone());
        }
    }
    layers.push(crate::prompts::BASE_INSTRUCTION.to_string());
    layers.push(render_environment_layer());
    layers.push(render_permissions_layer(policy));
    if let Some(catalog) = skill_catalog {
        if !catalog.is_empty() {
            layers.push(catalog.clone());
        }
    }
    layers.join("\n\n")
}

/// 构建 volatile 段（每次刷新；当前仅时间）。单独成 system 消息，不进 stable 前缀，
/// 保证 stable 字节级稳定以命中厂商缓存。
pub(super) fn build_volatile_context() -> String {
    render_current_time()
}

/// 构建 preamble：`[system(stable), system(volatile), user(bodies)?]`。
///
/// skill 正文（skill_bodies）以 **user-role** 第三条消息注入，而非塞进 stable 前缀。对齐 codex
/// （codex 的 skill body 即 role=user 的 ContextualUserFragment）：
/// 1. **稳定缓存** — stable 前缀（前两条 system）跨请求字节不变，命中厂商 cache_control / 前缀缓存；
///    body 每请求变化（取决于用户这轮 @ 了什么），独立成条不污染 stable。
/// 2. **role 语义** — body 作为用户提供的上下文，user-role 比 system 更贴切（codex 同款）。
/// 3. **不持久化 / 不污染气泡** — preamble 是 `run()` 局部 `Vec<Content>`，Runner 只持久化 user event
///    与模型 event，preamble 不进 `conversation_history`（对齐 codex fragment 不持久化），
///    也不拼进 user_text（前端 fetchHistory 回显的是 history 里的 user 消息）。
///
/// body 为空/纯空白时不 push user 消息，避免空 message 触发厂商 "empty message" 报错。
/// 返回长度动态（2 或 3），compaction 据此保护整个 preamble 不被压缩。
pub(super) fn build_preamble(
    stable: String,
    volatile: String,
    bodies: &Option<String>,
) -> Vec<Content> {
    let mut preamble = vec![
        Content {
            role: "system".to_string(),
            parts: vec![Part::Text { text: stable }],
        },
        Content {
            role: "system".to_string(),
            parts: vec![Part::Text { text: volatile }],
        },
    ];
    if let Some(b) = bodies {
        if !b.trim().is_empty() {
            preamble.push(Content {
                role: "user".to_string(),
                parts: vec![Part::Text { text: b.clone() }],
            });
        }
    }
    preamble
}

/// 渲染 environment 静态层(OS + 默认 shell;不含时间/cwd,时间移至 volatile,cwd 需会话注入)。
///
/// 对齐 codex `<environment_context>` 的 `<shell>`:模型据此用对应 shell 语法
/// (Windows=PowerShell,Unix=$SHELL 或 bash)。codex 不显式注入 "OS: Windows",
/// 而是通过 shell 类型 + cwd 路径风格隐式传递;此处保留 OS 显式行 + 补 shell。
fn render_environment_layer() -> String {
    let os = if cfg!(target_os = "windows") {
        "Windows"
    } else if cfg!(target_os = "macos") {
        "macOS"
    } else if cfg!(target_os = "linux") {
        "Linux"
    } else {
        "Unknown"
    };
    let shell = if cfg!(target_os = "windows") {
        "powershell".to_string()
    } else {
        // 与 execute_command 的 Unix 分支一致(硬编码 `sh -c`):不读 $SHELL,
        // 否则模型据 $SHELL(可能是 zsh/fish)写的特有语法会在 sh -c 下失败
        "sh".to_string()
    };
    format!("## Environment\n\nOperating System: {os}\nDefault Shell: {shell}")
}

/// 渲染 volatile 层（当前时间）。每次请求刷新，单独成段，不进 stable 前缀。
fn render_current_time() -> String {
    let now = chrono::Utc::now().format("%Y-%m-%d %H:%M UTC");
    format!("## Current time\n\n{now}")
}

/// 渲染 permissions 层：按当前 [`PermissionPolicy`] 选 codex 风格的 sandbox/approval 模板，
/// 并填充 `{{ network_access }}` 占位符（对齐 codex permissions context）。
fn render_permissions_layer(policy: PermissionPolicy) -> String {
    use crate::domain::permissions::{ApprovalPolicy, SandboxMode};
    let network = if policy.network_access {
        "enabled"
    } else {
        "disabled"
    };
    let sandbox = match policy.sandbox_mode {
        SandboxMode::ReadOnly => crate::prompts::SANDBOX_READ_ONLY,
        SandboxMode::WorkspaceWrite => crate::prompts::SANDBOX_WORKSPACE_WRITE,
        SandboxMode::DangerFullAccess => crate::prompts::SANDBOX_DANGER_FULL_ACCESS,
    };
    let approval = match policy.approval_policy {
        ApprovalPolicy::Never => crate::prompts::APPROVAL_NEVER,
        ApprovalPolicy::OnRequest => crate::prompts::APPROVAL_ON_REQUEST,
        ApprovalPolicy::OnRequestRuleRequestPermission => {
            crate::prompts::APPROVAL_ON_REQUEST_RULE_REQUEST_PERMISSION
        }
        ApprovalPolicy::UnlessTrusted => crate::prompts::APPROVAL_UNLESS_TRUSTED,
    };
    // sandbox 模板含 {{ network_access }} 占位符；approval 模板不含（已核对），无需 replace
    let sandbox = sandbox.replace("{{ network_access }}", network);
    format!("## Permissions\n\n### Sandbox mode\n{sandbox}\n\n### Approval policy\n{approval}")
}

#[cfg(test)]
mod prompt_injection_tests {
    use super::*;
    use crate::domain::permissions::PermissionPolicy;

    #[test]
    fn stable_prefix_is_deterministic_and_time_free() {
        // 相同输入两次构建必须字节一致（跨请求稳定 → 缓存命中前提）
        let a = build_stable_prefix(&None, &None, &None, PermissionPolicy::default());
        let b = build_stable_prefix(&None, &None, &None, PermissionPolicy::default());
        assert_eq!(a, b, "stable prefix 必须跨调用字节一致");
        assert!(!a.contains("Current time"), "stable 前缀不得含时间");
        assert!(a.contains("Operating System"), "stable 前缀应含 OS");
    }

    #[test]
    fn volatile_context_contains_only_time() {
        let v = build_volatile_context();
        assert!(v.contains("Current time"), "volatile 段应含时间");
        assert!(!v.contains("Operating System"), "volatile 段不得含 OS");
    }

    #[test]
    fn stable_prefix_includes_instruction_first() {
        let instr = Some("你是翻译助手".to_string());
        let s = build_stable_prefix(&instr, &None, &None, PermissionPolicy::default());
        assert!(
            s.starts_with("你是翻译助手"),
            "instruction 必须在 stable 前缀最前"
        );
    }

    #[test]
    fn build_preamble_injects_user_role_body() {
        let stable = build_stable_prefix(&None, &None, &None, PermissionPolicy::default());
        let volatile = build_volatile_context();

        // 有 body → preamble 3 条，第 3 条 role=user（对齐 codex body=user）
        let pre = build_preamble(
            stable.clone(),
            volatile.clone(),
            &Some("UNIQUE_BODY_MARKER".to_string()),
        );
        assert_eq!(pre.len(), 3, "有 body 时 preamble 应为 3 条");
        assert_eq!(pre[0].role, "system", "第 1 条为 stable system");
        assert_eq!(pre[1].role, "system", "第 2 条为 volatile system");
        assert_eq!(pre[2].role, "user", "body 应以 user-role 注入");
        // body 只在 user 段，不渗入 stable system 段（保护缓存前缀稳定）
        let stable_text = match &pre[0].parts[0] {
            Part::Text { text } => text.as_str(),
            _ => "",
        };
        assert!(
            !stable_text.contains("UNIQUE_BODY_MARKER"),
            "body 不得渗入 stable system 段"
        );

        // 无 body → 2 条
        let pre_none = build_preamble(stable.clone(), volatile.clone(), &None);
        assert_eq!(pre_none.len(), 2, "无 body 时 preamble 为 2 条");

        // 空/纯空白 body → 2 条（不 push 空 user 消息，避免厂商报错）
        let pre_empty = build_preamble(stable, volatile, &Some("   ".to_string()));
        assert_eq!(pre_empty.len(), 2, "空 body 不应 push user 消息");
    }
}
