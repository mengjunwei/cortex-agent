//! system prompt 分层构建 — stable 前缀（命中厂商缓存）+ volatile 段 + skill 正文 preamble。
//!
//! 层次设计对齐 codex：跨请求字节不变的部分进 stable 前缀以命中厂商 cache_control / 前缀缓存；
//! 每请求变化的部分（时间、@ 提及的 skill 正文）独立成段，不污染 stable。

use adk_rust::{Content, Part};

use crate::permissions::PermissionPolicy;

/// 构建 stable 前缀（跨请求字节不变 → 命中厂商缓存）。
///
/// 层次：
/// 1. instruction — 调用方特化指令（自定义助手=用户人设，内置助手=专业 prompt）；为空则跳过
/// 2. BASE_INSTRUCTION — CortexAgent 固定注入的通用行为基线（始终追加）
/// 3. environment — OS（不含时间，时间移至 volatile 避免击穿缓存前缀）
/// 4. permissions — sandbox_mode + approval_policy
/// 5. sub-agents — 多智能体引导（仅 max_spawn_depth > 0 时注入，与工具注册同条件）
/// 6. skill catalog — 可用 skill 的 name + desc（启动时构建，跨请求稳定）
///
/// 注意：用户 $ 提及的 skill 正文（skill_bodies）**不在此处** —— 它每请求变化，
/// 放进 stable 会击穿缓存。它由 [`build_preamble`] 以独立 user-role 消息注入
/// （对齐 codex body=user 语义，详见 [`build_preamble`] 文档）。
/// [`build_stable_prefix`] 的参数集（字段数多，收进结构体防 clippy
/// too_many_arguments——同 `SandboxExec` 惯例；字段语义各异，具名传递防错位）。
pub(super) struct StablePrefixParams<'a> {
    /// 调用方特化指令（自定义助手=用户人设，内置助手=专业 prompt）；空则跳过
    pub(super) instruction: &'a Option<String>,
    /// 跨会话记忆块（用户习惯/坑）；空则跳过
    pub(super) memory_block: &'a Option<String>,
    /// skill 目录（name + desc，启动时构建，跨请求稳定）；空则跳过
    pub(super) skill_catalog: &'a Option<String>,
    /// sandbox_mode + approval_policy 渲染层
    pub(super) policy: PermissionPolicy,
    /// 工作区 cwd（None=不渲染该层）
    pub(super) workspace_cwd: Option<&'a str>,
    /// 多智能体 spawn 深度上限（0=不注入多智能体引导层）
    pub(super) max_spawn_depth: u32,
    /// 子 agent 并发上限
    pub(super) max_concurrent_children: usize,
    /// 多智能体模式提示（None=禁用，不注入）
    pub(super) mode_hint: Option<&'static str>,
    /// 是否子 agent（影响身份 hint：final answer 回投父）
    pub(super) is_subagent: bool,
}

pub(super) fn build_stable_prefix(params: StablePrefixParams<'_>) -> String {
    let StablePrefixParams {
        instruction,
        memory_block,
        skill_catalog,
        policy,
        workspace_cwd,
        max_spawn_depth,
        max_concurrent_children,
        mode_hint,
        is_subagent,
    } = params;
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
    layers.push(render_environment_layer(workspace_cwd));
    layers.push(render_permissions_layer(policy));
    // 多智能体引导：与 run 主循环的工具注册条件一致（max_spawn_depth > 0 才有协作工具）。
    // 不注入则模型（尤弱模型）不会主动发现无人引导的工具 → 子 agent 永远触发不了。
    if max_spawn_depth > 0 {
        layers.push(render_multi_agent_layer(
            max_spawn_depth,
            max_concurrent_children,
            mode_hint,
            is_subagent,
        ));
    }
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

/// 渲染 environment 静态层(OS + 默认 shell + 工作目录 + runtime 存在性;不含时间,时间移至 volatile)。
///
/// 对齐 codex `<environment_context>`：注入 shell + cwd 等，让模型瞬间定位环境。
/// 此处在 codex 基础上额外注入 runtime 存在性（python 是否虚拟环境 / node 版本），让模型知道
/// 「这里有哪些 runtime」；具体能力（pip 有无、全局包、某库可否 import）刻意不枚举，由模型
/// 自己按需确认 —— 对齐 codex「只给上下文、不过度枚举」并保持 stable 前缀极简。
/// cwd 与 manifest 均进程/会话级稳定 → 放 stable 段不击穿缓存前缀。
fn render_environment_layer(workspace_cwd: Option<&str>) -> String {
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
    let mut s = format!("## Environment\n\nOperating System: {os}\nDefault Shell: {shell}");
    // 注入会话工作目录（绝对路径）：模型据此用相对路径、把产物写到正确位置，避免盲写他处。
    if let Some(cwd) = workspace_cwd.filter(|c| !c.is_empty()) {
        s.push_str(&format!(
            "\nWorking Directory: {cwd} (session workspace — write generated files here, use relative paths)"
        ));
    }
    // 追加启动时探测的 runtime 清单（init 未跑/全失败时为空 → 跳过）。
    let manifest = super::env_probe::manifest();
    if !manifest.is_empty() {
        s.push_str(&format!("\n\n{manifest}"));
    }
    s
}

/// 渲染 volatile 层（当前时间）。每次请求刷新，单独成段，不进 stable 前缀。
fn render_current_time() -> String {
    let now = chrono::Utc::now().format("%Y-%m-%d %H:%M UTC");
    format!("## Current time\n\n{now}")
}

/// 渲染 permissions 层：按当前 [`PermissionPolicy`] 选 codex 风格的 sandbox/approval 模板，
/// 并填充 `{{ network_access }}` 占位符（对齐 codex permissions context）。
fn render_permissions_layer(policy: PermissionPolicy) -> String {
    use crate::permissions::{ApprovalPolicy, SandboxMode};
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
        ApprovalPolicy::Auto => crate::prompts::APPROVAL_AUTO,
    };
    // sandbox 模板含 {{ network_access }} 占位符；approval 模板不含（已核对），无需 replace
    let sandbox = sandbox.replace("{{ network_access }}", network);
    format!("## Permissions\n\n### Sandbox mode\n{sandbox}\n\n### Approval policy\n{approval}")
}

/// 渲染多智能体引导层（V2，对齐 codex usage hints + MultiAgentMode 提示词）。
///
/// 结构：
/// 1. root / subagent 身份段（对齐 codex DEFAULT_MULTI_AGENT_V2_*_USAGE_HINT_TEXT）
/// 2. 模式提示（对齐 codex MultiAgentModeInstructions：ExplicitRequestOnly/Proactive）
/// 3. 共享段：工具清单 + 共享文件系统说明 + 并发槽位（对齐 codex SHARED hint）
///
/// 工具注册与引导注入同条件（max_spawn_depth > 0）；值会话级稳定 → 不击穿缓存前缀。
#[allow(clippy::too_many_arguments)]
fn render_multi_agent_layer(
    max_spawn_depth: u32,
    max_concurrent_children: usize,
    mode_hint: Option<&'static str>,
    is_subagent: bool,
) -> String {
    let identity = if is_subagent {
        // 子 agent hint（对齐 codex subagent usage hint 语义）
        "You are an agent in a team of agents collaborating to complete a task.\n\n\
You can spawn sub-agents to handle subtasks, and those sub-agents can spawn their own sub-agents. \
All agents in the team, including the agents that you can assign tasks to, are equally intelligent \
and capable, and have access to the same set of tools.\n\n\
You can use `spawn_agent` to create a new agent, `followup_task` to give an existing agent a new \
task and trigger a turn, and `send_message` to pass a message to a running agent without \
triggering a turn.\n\n\
When you provide your final answer, that content is immediately delivered back to your parent agent.\n\n\
You will receive messages in the form:\n\
Message Type: MESSAGE | NEW_TASK | FINAL_ANSWER\n\
Task name: <recipient>\n\
Sender: <author>\n\
Payload:\n<payload text>\n\
They may be addressed to /root"
    } else {
        // root hint（对齐 codex root usage hint 语义）
        "You are `/root`, the primary agent in a team of agents collaborating to fulfill the user's goals.\n\n\
You can spawn sub-agents to handle subtasks, and those sub-agents can spawn their own sub-agents. \
All agents in the team, including the agents that you can assign tasks to, are equally intelligent \
and capable, and have access to the same set of tools.\n\n\
You can use `spawn_agent` to create a new agent, `followup_task` to give an existing agent a new \
task and trigger a turn, and `send_message` to pass a message to a running agent without \
triggering a turn.\n\n\
You can decide how much context you want to propagate to your sub-agents with the `fork_turns` \
parameter.\n\n\
You will receive messages in the form:\n\
Message Type: MESSAGE | FINAL_ANSWER\n\
Task name: <recipient>\n\
Sender: <author>\n\
Payload:\n<payload text>"
    };

    let mode = mode_hint
        .map(|m| format!("\n\n<multi_agent_mode>\n{m}\n</multi_agent_mode>"))
        .unwrap_or_default();

    // 并发槽位说明（对齐 codex「{max_concurrency} available concurrency slots」；
    // 0=不限 → 不写误导性数字）
    let concurrency = if max_concurrent_children > 0 {
        format!(
            "There are {max_concurrent_children} available concurrency slots, meaning that up to \
             {max_concurrent_children} sub-agents can be active at once."
        )
    } else {
        "There is no fixed concurrency limit on sub-agents, but keep it reasonable.".to_string()
    };

    format!(
        "## Sub-agents (agent team)\n\n\
{identity}{mode}\n\n\
All agents share the same directory and filesystem: edits made by one agent are immediately \
visible to all other agents. Never let two agents edit the same file.\n\n\
When calling `wait_agent`, prefer longer waits to avoid busy polling. After a wait completes, \
read the delivered agent messages from your conversation before continuing.\n\n\
{concurrency}\n\n\
Limits: sub-agents can nest up to {max_spawn_depth} level(s) deep. If a spawn is rejected, wait \
for running agents to finish or do the work yourself."
    )
}

#[cfg(test)]
mod prompt_injection_tests {
    use super::*;
    use crate::config::MultiAgentModeConfig;
    use crate::permissions::PermissionPolicy;

    /// 统一测试调用形状（默认显式模式 + root 身份）。
    fn build(
        instruction: &Option<String>,
        memory_block: &Option<String>,
        skill_catalog: &Option<String>,
        policy: PermissionPolicy,
        cwd: Option<&str>,
        depth: u32,
        concurrent: usize,
    ) -> String {
        build_stable_prefix(StablePrefixParams {
            instruction,
            memory_block,
            skill_catalog,
            policy,
            workspace_cwd: cwd,
            max_spawn_depth: depth,
            max_concurrent_children: concurrent,
            mode_hint: crate::agent::cortex::multi_agent_mode_hint_for_test(
                MultiAgentModeConfig::Explicit,
            ),
            is_subagent: false,
        })
    }

    #[test]
    fn stable_prefix_is_deterministic_and_time_free() {
        // 相同输入两次构建必须字节一致（跨请求稳定 → 缓存命中前提）
        let a = build(&None, &None, &None, PermissionPolicy::default(), None, 0, 0);
        let b = build(&None, &None, &None, PermissionPolicy::default(), None, 0, 0);
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
        let s = build(
            &instr,
            &None,
            &None,
            PermissionPolicy::default(),
            None,
            0,
            0,
        );
        assert!(
            s.starts_with("你是翻译助手"),
            "instruction 必须在 stable 前缀最前"
        );
    }

    #[test]
    fn stable_prefix_includes_multi_agent_layer_when_enabled() {
        // max_spawn_depth > 0（与工具注册同条件）→ 注入引导层，含工具名与实际上限值
        let s = build(&None, &None, &None, PermissionPolicy::default(), None, 3, 3);
        assert!(s.contains("Sub-agents"), "启用时应注入 Sub-agents 层");
        assert!(s.contains("spawn_agent"), "引导应提到 spawn_agent");
        assert!(s.contains("followup_task"), "V2 引导应提到 followup_task");
        assert!(s.contains("send_message"), "V2 引导应提到 send_message");
        assert!(s.contains("wait_agent"), "引导应提到 wait_agent");
        assert!(s.contains("3"), "引导应填入实际上限值");
        // 模式提示（默认 Explicit）
        assert!(s.contains("<multi_agent_mode>"), "V2 引导应含模式提示标记");
        // 禁用（0）→ 不注入，避免提示词残留无人注册的工具描述误导模型
        let disabled = build(&None, &None, &None, PermissionPolicy::default(), None, 0, 0);
        assert!(
            !disabled.contains("spawn_agent"),
            "禁用时不得注入多智能体引导"
        );
        // 同输入两次构建字节一致（引导层不引入每请求变化的值 → 不击穿缓存前缀）
        let again = build(&None, &None, &None, PermissionPolicy::default(), None, 3, 3);
        assert_eq!(s, again, "多智能体层必须保持 stable 前缀字节稳定");
        // 并发上限 0 = 不限制（factory 语义）→ 不得写出误导性的槽位数字
        let unlimited = build(&None, &None, &None, PermissionPolicy::default(), None, 3, 0);
        assert!(
            !unlimited.contains("0 available concurrency slots"),
            "并发 0=不限，不得误导为「0 个并发槽位」"
        );
        assert!(
            unlimited.contains("no fixed concurrency limit"),
            "并发 0 应说明不限制"
        );
    }

    #[test]
    fn multi_agent_layer_mode_and_identity_variants() {
        let explicit = build_stable_prefix(StablePrefixParams {
            instruction: &None,
            memory_block: &None,
            skill_catalog: &None,
            policy: PermissionPolicy::default(),
            workspace_cwd: None,
            max_spawn_depth: 3,
            max_concurrent_children: 3,
            mode_hint: Some(crate::prompts::MULTI_AGENT_MODE_EXPLICIT),
            is_subagent: false,
        });
        assert!(explicit.contains("Do not spawn sub-agents unless the user"));
        let proactive = build_stable_prefix(StablePrefixParams {
            instruction: &None,
            memory_block: &None,
            skill_catalog: &None,
            policy: PermissionPolicy::default(),
            workspace_cwd: None,
            max_spawn_depth: 3,
            max_concurrent_children: 3,
            mode_hint: Some(crate::prompts::MULTI_AGENT_MODE_PROACTIVE),
            is_subagent: false,
        });
        assert!(proactive.contains("Proactive multi-agent delegation is active"));
        // 子 agent 身份：subagent hint 语义（final answer 回投父）
        let sub = build_stable_prefix(StablePrefixParams {
            instruction: &None,
            memory_block: &None,
            skill_catalog: &None,
            policy: PermissionPolicy::default(),
            workspace_cwd: None,
            max_spawn_depth: 3,
            max_concurrent_children: 3,
            mode_hint: Some(crate::prompts::MULTI_AGENT_MODE_EXPLICIT),
            is_subagent: true,
        });
        assert!(sub.contains("delivered back to your parent agent"));
        assert!(sub.contains("NEW_TASK"), "子 agent 可见 NEW_TASK 消息类型");
        // root 身份：fork_turns 说明
        let root = build_stable_prefix(StablePrefixParams {
            instruction: &None,
            memory_block: &None,
            skill_catalog: &None,
            policy: PermissionPolicy::default(),
            workspace_cwd: None,
            max_spawn_depth: 3,
            max_concurrent_children: 3,
            mode_hint: Some(crate::prompts::MULTI_AGENT_MODE_EXPLICIT),
            is_subagent: false,
        });
        assert!(root.contains("fork_turns"), "root hint 应说明 fork_turns");
    }

    #[test]
    fn stable_prefix_includes_workspace_cwd_when_provided() {
        let s = build(
            &None,
            &None,
            &None,
            PermissionPolicy::default(),
            Some("/abs/data/workspaces/sessions/abc"),
            0,
            0,
        );
        assert!(
            s.contains("Working Directory: /abs/data/workspaces/sessions/abc"),
            "提供 cwd 时 stable 前缀应含 Working Directory 行"
        );
        // 无 cwd 时不出现该行
        let s_none = build(&None, &None, &None, PermissionPolicy::default(), None, 0, 0);
        assert!(
            !s_none.contains("Working Directory"),
            "无 cwd 时不应注入 Working Directory 行"
        );
    }

    #[test]
    fn build_preamble_injects_user_role_body() {
        let stable = build(&None, &None, &None, PermissionPolicy::default(), None, 0, 0);
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
