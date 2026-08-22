//! AgentBlueprint —— 从父 agent 克隆构建参数，用于 fork 同构子 agent。

use std::sync::Arc;
use std::time::Duration;

use adk_rust::{Agent, GenerateContentConfig, Llm, Tool, Toolset};
use tokio_util::sync::CancellationToken;

use super::super::builder::{CortexAgentBuilder, ModelResolver};
use super::super::hook::CompactionHook;
use super::mailbox::ParentMailbox;
use super::tree::{AgentTree, ChildHandle};
use super::{ChildEventSink, ChildUsageTotal};
use crate::config::{AgentsConfig, ContextConfig};
use crate::permissions::PermissionPolicy;

pub(crate) struct AgentBlueprint {
    description: String,
    model: Arc<dyn Llm>,
    instruction: Option<String>,
    memory_block: Option<String>,
    skill_catalog: Option<String>,
    skill_bodies: Option<String>,
    config: Option<GenerateContentConfig>,
    tools: Vec<Arc<dyn Tool>>,
    toolsets: Vec<Arc<dyn Toolset>>,
    max_iterations: u32,
    llm_timeout: Duration,
    /// wait_agent 的外层超时钳制读取（跨模块 tools.rs 访问）
    pub(super) tool_timeout: Duration,
    policy: PermissionPolicy,
    context_window: Option<usize>,
    compact_model: Option<Arc<dyn Llm>>,
    context_config: ContextConfig,
    hooks: Vec<Arc<dyn CompactionHook>>,
    sink: Arc<dyn ChildEventSink>,
    workspace_cwd: Option<String>,
    child_usage_total: ChildUsageTotal,
    /// 全树共享注册表（孙 agent 的 factory 经 build_child 继承同一 Arc）
    tree: Option<Arc<AgentTree>>,
    /// 父 mailbox（子 agent FINAL_ANSWER 投回本 agent 的队列；root 的由主循环 drain。
    /// 孙 agent 构建时 factory 直接用 self_mailbox，蓝图字段当前仅作配置快照保留）
    #[allow(dead_code)]
    parent_mailbox: Option<Arc<ParentMailbox>>,
    /// `[agents]` 配置（角色解析）
    agents_cfg: AgentsConfig,
    /// 会话级思考级别（孙 agent 构建 hint 用）
    session_thinking_level: Option<String>,
    /// spawn model 覆盖解析器（孙 agent 的 factory 继承）
    model_resolver: Option<ModelResolver>,
}

/// [`AgentBlueprint::build_child`] 的参数集（字段数多，收进结构体防
/// clippy too_many_arguments——同 `SandboxExec` 惯例；单一调用点）。
pub(super) struct BuildChildParams<'a> {
    /// spawn 任务名（子 agent name = 尾段）
    pub task_name: &'a str,
    /// 子 agent spawn 深度（父 + 1）
    pub depth: u32,
    /// 子树取消令牌（父 token 的 child_token）
    pub cancel: CancellationToken,
    /// spawn 指定 model 时替换蓝图模型（None=继承父）
    pub model_override: Option<Arc<dyn Llm>>,
    /// 角色特化指令（None=继承父 instruction）
    pub instruction_override: Option<String>,
    /// 子 agent 在树中的路径（孙 agent 的 spawn 挂在其下）
    pub canonical_path: &'a str,
    /// 子 agent 的 ChildHandle（run 循环轮内 drain inbox 用；先建 handle 再 build）
    pub handle: &'a Arc<ChildHandle>,
}

impl AgentBlueprint {
    /// 用蓝图构建一个同构子 agent（name = task_name 尾段，spawn 深度 +1）。
    pub(super) fn build_child(
        &self,
        BuildChildParams {
            task_name,
            depth,
            cancel,
            model_override,
            instruction_override,
            canonical_path,
            handle,
        }: BuildChildParams<'_>,
    ) -> std::result::Result<Arc<dyn Agent>, String> {
        let mut b = CortexAgentBuilder::new(task_name)
            .description(&self.description)
            .model(model_override.unwrap_or_else(|| self.model.clone()))
            .policy(self.policy)
            .cancel_token(cancel)
            .max_iterations(self.max_iterations)
            .llm_timeout(self.llm_timeout)
            .tool_timeout(self.tool_timeout)
            .context_config(self.context_config.clone())
            .spawn_depth(depth)
            .child_path(canonical_path)
            .self_inbox(handle.clone());
        if let Some(t) = &self.tree {
            b = b.inherited_tree(t.clone());
        }
        if let Some(r) = &self.model_resolver {
            b = b.model_resolver(r.clone());
        }
        // 角色 instruction 与父 instruction 拼接（对齐 codex 语义：base_instructions/
        // persona 保留，角色只覆盖 developer_instructions 层——整体替换会丢用户人设）。
        let instruction = match (self.instruction.as_deref(), instruction_override) {
            (Some(base), Some(role)) if !base.trim().is_empty() => {
                Some(format!("{base}\n\n{role}"))
            }
            (_, role) => role.or_else(|| self.instruction.clone()),
        };
        if let Some(c) = &self.config {
            b = b.generate_content_config(c.clone());
        }
        for t in &self.tools {
            b = b.tool(t.clone());
        }
        for ts in &self.toolsets {
            b = b.toolset(ts.clone());
        }
        if let Some(i) = instruction {
            b = b.instruction(i);
        }
        if let Some(m) = &self.memory_block {
            b = b.memory_block(m.clone());
        }
        if let Some(c) = &self.skill_catalog {
            b = b.skill_catalog(c.clone());
        }
        if let Some(s) = &self.skill_bodies {
            b = b.skill_bodies(s.clone());
        }
        if let Some(w) = self.context_window {
            b = b.context_window(w);
        }
        if let Some(cm) = &self.compact_model {
            b = b.compact_model(cm.clone());
        }
        for h in &self.hooks {
            b = b.compaction_hook(h.clone());
        }
        if let Some(w) = &self.workspace_cwd {
            b = b.workspace_cwd(w.clone());
        }
        b = b.child_usage_total(self.child_usage_total.clone());
        b = b.child_event_sink(self.sink.clone());
        // V2 追加：树/角色/思考级别/canonical 路径（孙 agent 构建时需要）
        if let Some(t) = &self.tree {
            b = b
                .agents_config(self.agents_cfg.clone())
                .session_thinking_level(self.session_thinking_level.clone());
            // child_path 由 build_child 的调用方（factory.spawn）经 child_path() 设置
            let _ = t; // 树引用经 factory 持有，蓝图只透传 agents/thinking
        }
        let agent = b.build().map_err(|e| e.to_string())?;
        Ok(Arc::new(agent) as Arc<dyn Agent>)
    }
}

impl super::super::CortexAgent {
    /// 从当前 agent 的配置克隆一份蓝图（供 fork 子 agent），带树与父 mailbox 引用。
    pub(crate) fn child_blueprint_with(
        &self,
        tree: Arc<AgentTree>,
        parent_mailbox: Arc<ParentMailbox>,
    ) -> AgentBlueprint {
        AgentBlueprint {
            description: self.description.clone(),
            model: self.model.clone(),
            instruction: self.instruction.clone(),
            memory_block: self.memory_block.clone(),
            skill_catalog: self.skill_catalog.clone(),
            skill_bodies: self.skill_bodies.clone(),
            config: self.config.clone(),
            tools: self.tools.clone(),
            toolsets: self.toolsets.clone(),
            max_iterations: self.max_iterations,
            llm_timeout: self.llm_timeout,
            tool_timeout: self.tool_timeout,
            policy: self.policy,
            context_window: self.context_window,
            compact_model: self.compact_model.clone(),
            context_config: self.context_config.clone(),
            hooks: self.hooks.clone(),
            sink: self.child_event_sink.clone(),
            workspace_cwd: self.workspace_cwd.clone(),
            child_usage_total: self.child_usage_total.clone(),
            tree: Some(tree),
            parent_mailbox: Some(parent_mailbox),
            agents_cfg: self.agents_config.clone(),
            session_thinking_level: self.session_thinking_level.clone(),
            model_resolver: self.model_resolver.clone(),
        }
    }
}
