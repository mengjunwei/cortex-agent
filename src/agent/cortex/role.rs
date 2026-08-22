//! 角色系统 — 子 agent 的预定义「人格/职责」模板（对齐 codex `agent/role.rs`）。
//!
//! 角色 = description（引导主 agent 何时选它）+ instruction（注入子 agent 的特化指令）+ nickname_candidates（昵称池覆盖）。
//! 内置 default / explorer / worker 三个角色（描述原文照抄 codex），用户可在
//! config.toml 的 `[agents.roles.<name>]` 自定义。
//!
//! 与 codex 的差异：codex 角色是独立 TOML config 层（可覆盖 model/effort 等），
//! cortex 的模型体系挂在 ModelProviderStore（DB 按用户隔离），不适合进程内角色层
//! 覆盖——故角色只做 instruction + description + 昵称池，模型覆盖走 spawn 的
//! `model` 参数（按 user_id 从 DB 解析）。

use std::collections::BTreeMap;
use std::sync::LazyLock;

use crate::config::AgentRoleToml;

/// 默认角色名（省略 agent_type 时使用，对齐 codex DEFAULT_ROLE_NAME）。
pub(crate) const DEFAULT_ROLE_NAME: &str = "default";

/// 内置角色（description 原文照抄 codex role.rs built_in）。
static BUILT_IN_ROLES: LazyLock<BTreeMap<String, AgentRole>> = LazyLock::new(|| {
    BTreeMap::from([
        (
            DEFAULT_ROLE_NAME.to_string(),
            AgentRole {
                description: "Default agent.".to_string(),
                instruction: None,
                nickname_candidates: None,
            },
        ),
        (
            "explorer".to_string(),
            AgentRole {
                description: r#"Use `explorer` for specific codebase questions.
Explorers are fast and authoritative.
They must be used to ask specific, well-scoped questions on the codebase.
Rules:
- In order to avoid redundant work, you should avoid exploring the same problem that explorers have already covered. Typically, you should trust the explorer results without additional verification. You are still allowed to inspect the code yourself to gain the needed context!
- You are encouraged to spawn up multiple explorers in parallel when you have multiple distinct questions to ask about the codebase that can be answered independently. This allows you to get more information faster without waiting for one question to finish before asking the next. While waiting for the explorer results, you can continue working on other local tasks that do not depend on those results. This parallelism is a key advantage of delegation, so use it whenever you have multiple questions to ask.
- Reuse existing explorers for related questions."#
                    .to_string(),
                // 注：codex 内置 explorer 的 config_file 是空文件（零 instruction），
                // 以下特化指令为 cortex 自加（对齐其“快速只读调研”定位），非 codex 原文。
                instruction: Some(
                    "You are an explorer agent. Answer specific, well-scoped questions about the \
                     codebase. Read the code, report authoritative findings. Do not modify files \
                     unless explicitly instructed."
                        .to_string(),
                ),
                nickname_candidates: None,
            },
        ),
        (
            "worker".to_string(),
            AgentRole {
                description: r#"Use for execution and production work.
Typical tasks:
- Implement part of a feature
- Fix tests or bugs
- Split large refactors into independent chunks
Rules:
- Explicitly assign **ownership** of the task (files / responsibility). When the subtask involves code changes, you should clearly specify which files or modules the worker is responsible for. This helps avoid merge conflicts and ensures accountability. For example, you can say "Worker 1 is responsible for updating the authentication module, while Worker 2 will handle the database layer." By defining clear ownership, you can delegate more effectively and reduce coordination overhead.
- Always tell workers they are **not alone in the codebase**, and they should not revert the edits made by others, and they should adjust their implementation to accommodate the changes made by others. This is important because there may be multiple workers making changes in parallel, and they need to be aware of each other's work to avoid conflicts and and to ensure a cohesive final product."#
                    .to_string(),
                // 同上：codex 内置 worker 零 instruction，此特化指令为 cortex 自加。
                instruction: Some(
                    "You are a worker agent responsible for execution and production work. You are \
                     NOT alone in the codebase: other agents may be editing other files in \
                     parallel. Never revert edits made by others; adjust your implementation to \
                     accommodate their changes. Stay strictly within the file scope assigned to \
                     you and list the files you changed in your final answer."
                        .to_string(),
                ),
                nickname_candidates: None,
            },
        ),
    ])
});

/// 一个 agent 角色：description 引导主 agent 选择，instruction 注入子 agent。
#[derive(Debug, Clone)]
pub(crate) struct AgentRole {
    pub(crate) description: String,
    /// 注入子 agent system prompt 的特化指令（None=不注入，子 agent 用父 instruction）
    pub(crate) instruction: Option<String>,
    /// 昵称候选池覆盖（None=用默认全局昵称池）
    pub(crate) nickname_candidates: Option<Vec<String>>,
}

/// 解析角色：用户自定义优先于内置（对齐 codex resolve_role_config）。
pub(crate) fn resolve_role(
    name: &str,
    user_roles: &BTreeMap<String, AgentRoleToml>,
) -> Option<AgentRole> {
    if let Some(t) = user_roles.get(name) {
        return Some(AgentRole {
            description: t.description.clone(),
            instruction: t.instruction.clone(),
            nickname_candidates: t.nickname_candidates.clone(),
        });
    }
    BUILT_IN_ROLES.get(name).cloned()
}

/// 生成 spawn_agent 工具 `agent_type` 参数描述中的可用角色列表
/// （对齐 codex spawn_tool_spec::build：用户角色在前，内置在后，BTreeMap 序）。
pub(crate) fn agent_type_description(user_roles: &BTreeMap<String, AgentRoleToml>) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut push_role = |name: &str, role: &AgentRole| {
        if role.description.is_empty() {
            lines.push(format!("{name}: no description"));
        } else {
            lines.push(format!("{name}: {{\n{}\n}}", role.description));
        }
    };
    for (name, t) in user_roles {
        push_role(
            name,
            &AgentRole {
                description: t.description.clone(),
                instruction: t.instruction.clone(),
                nickname_candidates: t.nickname_candidates.clone(),
            },
        );
    }
    for (name, role) in BUILT_IN_ROLES.iter() {
        // 用户与内置重名时用户优先（上面已 push），跳过内置
        if !user_roles.contains_key(name) {
            push_role(name, role);
        }
    }
    format!("Available roles:\n{}", lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_roles_present() {
        assert!(BUILT_IN_ROLES.contains_key(DEFAULT_ROLE_NAME));
        assert!(BUILT_IN_ROLES.contains_key("explorer"));
        assert!(BUILT_IN_ROLES.contains_key("worker"));
        // explorer/worker 有特化指令，default 无
        assert!(BUILT_IN_ROLES[DEFAULT_ROLE_NAME].instruction.is_none());
        assert!(BUILT_IN_ROLES["explorer"].instruction.is_some());
        assert!(BUILT_IN_ROLES["worker"].instruction.is_some());
    }

    #[test]
    fn user_role_overrides_builtin() {
        let mut roles = BTreeMap::new();
        roles.insert(
            "explorer".to_string(),
            AgentRoleToml {
                description: "custom".to_string(),
                instruction: Some("be quick".to_string()),
                nickname_candidates: None,
            },
        );
        let r = resolve_role("explorer", &roles).unwrap();
        assert_eq!(r.description, "custom");
        // 未覆盖的内置角色仍可解析
        let w = resolve_role("worker", &roles).unwrap();
        assert!(w.description.contains("execution and production work"));
        assert!(resolve_role("nope", &roles).is_none());
    }

    #[test]
    fn agent_type_description_lists_user_first() {
        let mut roles = BTreeMap::new();
        roles.insert(
            "researcher".to_string(),
            AgentRoleToml {
                description: "Research-focused role.".to_string(),
                instruction: None,
                nickname_candidates: None,
            },
        );
        let desc = agent_type_description(&roles);
        assert!(desc.starts_with("Available roles:"));
        // 用户角色在前
        let researcher_idx = desc.find("researcher").unwrap();
        let default_idx = desc.find("default").unwrap();
        assert!(researcher_idx < default_idx);
        assert!(desc.contains("Research-focused role."));
    }
}
