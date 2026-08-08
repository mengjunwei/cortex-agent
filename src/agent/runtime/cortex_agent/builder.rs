//! `CortexAgent` 与 `CortexAgentBuilder` — agent 的配置与构建。
//!
//! `CortexAgent` 的 `run` 主循环在 [`super::mod`]；本模块只承载字段定义与 builder 链式装配。

use std::sync::Arc;
use std::time::Duration;

use adk_rust::{Agent, GenerateContentConfig, Llm, Tool, Toolset};
use tokio_util::sync::CancellationToken;

use crate::config::ContextConfig;
use crate::domain::permissions::PermissionPolicy;

use super::hook::CompactionHook;
use super::multi_agent::{ChildEventSink, NoopChildEventSink};

/// 默认最大迭代轮数。
///
/// 对齐 adk-rust `LlmAgent`（默认 100）与 codex（无硬上限，靠 compaction 兜底）。
/// 取 80：复杂多步任务（含 sub agent 并行编排）需足够轮次余量，弱模型折腾也不易过早触顶；
/// 异常死循环则由 compaction + 用户中断兜底。自定义助手可在 builder 层按需覆盖。
const DEFAULT_MAX_ITERATIONS: u32 = 80;
const DEFAULT_LLM_TIMEOUT: Duration = Duration::from_secs(120);
const DEFAULT_TOOL_TIMEOUT: Duration = Duration::from_secs(300);

/// 轮次上限软降级提示（直接采用 opencode `max-steps.ts` 原文，英文）：达到 max_iterations
/// 时注入 conv，同时关闭工具，强制模型用纯文本总结已完成工作、剩余任务与下一步建议。
/// 模型在无工具约束下只能文字回复，自然收敛到 turn_complete。
pub(super) const MAX_STEPS_PROMPT: &str = "CRITICAL - MAXIMUM STEPS REACHED\n\nThe maximum number of steps allowed for this task has been reached. Tools are disabled until next user input. Respond with text only.\n\nSTRICT REQUIREMENTS:\n1. Do NOT make any tool calls (no reads, writes, edits, searches, or any other tools)\n2. MUST provide a text response summarizing work done so far\n3. This constraint overrides ALL other instructions, including any user requests for edits or tool use\n\nResponse must include:\n- Statement that maximum steps for this agent have been reached\n- Summary of what has been accomplished so far\n- List of any remaining tasks that were not completed\n- Recommendations for what should be done next\n\nAny attempt to use tools is a critical violation. Respond with text ONLY.";

// ============================================================================
// 重复退化检测机制已移除（对齐 codex / opencode 的循环设计哲学）。
// ----------------------------------------------------------------------------
// 历史上这里有一套「文本指纹检测 → 注入重导向 prompt → 硬跳过 → 累计停止」
// 的运行时补丁，用来对抗模型（尤指 Anthropic 协议 + thinking 模式）在 thinking /
// 正文阶段的复读退化。实践证明这是「在下游和模型对抗」：
//   1. 尾部指纹检测永远存在漏检 + 误检；
//   2. 注入「请跳出死循环」的重导向 prompt 对 thinking 碎碎念退化基本无效；
//   3. 清空上下文硬跳过后，模型撞同一个任务卡点又会退化；
//   4. 每复发一次就再加一层补丁（正文检测 → thinking 检测 → 碎碎念加速 → 硬跳过
//      → 累计停止），根因（采样参数 / 上下文管理）从未被触及。
//
// 现回归「信任协议信号」的循环：turn 是否继续只由模型的 finish_reason /
// turn_complete + 是否还有 tool call 决定（见 run 主循环与 fcs.is_empty()），
// 流式 delta 只用于展示、不参与循环控制 —— 与 codex(opensource) / opencode 一致。
// 退化兜底改由 max_iterations 轮次上限的友好软降级承担；源头由 thinking budget
// 硬上限（client 层）约束。
// ============================================================================

pub struct CortexAgent {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) model: Arc<dyn Llm>,
    pub(crate) instruction: Option<String>,
    pub(crate) memory_block: Option<String>,
    pub(crate) skill_catalog: Option<String>,
    pub(crate) skill_bodies: Option<String>,
    pub(crate) config: Option<GenerateContentConfig>,
    pub(crate) tools: Vec<Arc<dyn Tool>>,
    pub(crate) toolsets: Vec<Arc<dyn Toolset>>,
    pub(crate) sub_agents: Vec<Arc<dyn Agent>>,
    pub(crate) max_iterations: u32,
    pub(crate) llm_timeout: Duration,
    pub(crate) tool_timeout: Duration,
    pub(crate) policy: PermissionPolicy,
    /// 取消令牌：run() 内的工具执行用 select! 监听 cancelled()，用户点停止时立即解锁
    /// （对齐 codex 的 CancellationToken child_token 级联）。
    pub(crate) cancel_token: CancellationToken,
    /// 模型上下文窗口（token），用于动态压缩阈值；None=用 context_config.fallback_context_window
    pub(crate) context_window: Option<usize>,
    /// 压缩专用模型（通常比主模型便宜），None=用主模型压缩
    pub(crate) compact_model: Option<Arc<dyn Llm>>,
    /// context 治理配置（动态压缩阈值/软着陆/截断/chars_per_token）
    pub(crate) context_config: ContextConfig,
    /// 压缩 hook（pre 可 veto、post 通知）
    pub(crate) hooks: Vec<Arc<dyn CompactionHook>>,
    /// spawn 嵌套深度：顶层 agent = 0，每 spawn 一个子 agent +1。受 max_spawn_depth 限制。
    pub(crate) spawn_depth: u32,
    /// 子 agent 活动事件出口（转发到主 SSE 流供前端可视化）；默认 Noop 不转发。
    pub(crate) child_event_sink: Arc<dyn ChildEventSink>,
    /// 上下文预算句柄（effective_tokens / context_window / 窗口号）。
    /// build 时创建，run 复用；SSE 层经 budget() 只读轮询，向前端推 token 用量
    /// （对齐 codex get_total_token_usage：真实 usage + 字节估算兜底）。
    pub(crate) budget_handle: super::context_tool::SharedBudget,
}

pub struct CortexAgentBuilder {
    name: String,
    description: String,
    model: Option<Arc<dyn Llm>>,
    instruction: Option<String>,
    memory_block: Option<String>,
    skill_catalog: Option<String>,
    skill_bodies: Option<String>,
    config: Option<GenerateContentConfig>,
    tools: Vec<Arc<dyn Tool>>,
    toolsets: Vec<Arc<dyn Toolset>>,
    sub_agents: Vec<Arc<dyn Agent>>,
    max_iterations: u32,
    llm_timeout: Duration,
    tool_timeout: Duration,
    policy: PermissionPolicy,
    cancel_token: CancellationToken,
    context_window: Option<usize>,
    compact_model: Option<Arc<dyn Llm>>,
    context_config: ContextConfig,
    hooks: Vec<Arc<dyn CompactionHook>>,
    spawn_depth: u32,
    child_event_sink: Arc<dyn ChildEventSink>,
    /// 上下文预算句柄（build 时创建，传给 CortexAgent；SSE 层经 budget() 只读轮询）。
    budget_handle: super::context_tool::SharedBudget,
}

impl CortexAgentBuilder {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            description: String::new(),
            model: None,
            instruction: None,
            memory_block: None,
            skill_catalog: None,
            skill_bodies: None,
            config: None,
            tools: Vec::new(),
            toolsets: Vec::new(),
            sub_agents: Vec::new(),
            max_iterations: DEFAULT_MAX_ITERATIONS,
            llm_timeout: DEFAULT_LLM_TIMEOUT,
            tool_timeout: DEFAULT_TOOL_TIMEOUT,
            policy: PermissionPolicy::default(),
            cancel_token: CancellationToken::new(),
            context_window: None,
            compact_model: None,
            context_config: ContextConfig::default(),
            hooks: Vec::new(),
            spawn_depth: 0,
            child_event_sink: Arc::new(NoopChildEventSink),
            budget_handle: super::context_tool::new_shared_budget(),
        }
    }
    pub fn description(mut self, d: &str) -> Self {
        self.description = d.to_string();
        self
    }
    pub fn model(mut self, m: Arc<dyn Llm>) -> Self {
        self.model = Some(m);
        self
    }
    pub fn instruction(mut self, i: impl Into<String>) -> Self {
        self.instruction = Some(i.into());
        self
    }
    pub fn skill_catalog(mut self, c: impl Into<String>) -> Self {
        self.skill_catalog = Some(c.into());
        self
    }
    /// 被 @ 提及的 skill 正文（注入 system prompt，不进 user message，避免污染持久化/前端回显）
    pub fn skill_bodies(mut self, b: impl Into<String>) -> Self {
        self.skill_bodies = Some(b.into());
        self
    }
    /// 跨会话记忆块（习惯/坑，注入 stable prefix）。
    ///
    /// 同一用户 + 助手在单次会话内记忆不变 → 放 stable 段不击穿缓存前缀（对齐 codex
    /// 把 memory_summary 注入 developer 段的做法）。跨用户/助手才变化，属于不同 run。
    pub fn memory_block(mut self, m: impl Into<String>) -> Self {
        self.memory_block = Some(m.into());
        self
    }
    pub fn generate_content_config(mut self, c: GenerateContentConfig) -> Self {
        self.config = Some(c);
        self
    }
    pub fn tool(mut self, t: Arc<dyn Tool>) -> Self {
        self.tools.push(t);
        self
    }
    pub fn toolset(mut self, ts: Arc<dyn Toolset>) -> Self {
        self.toolsets.push(ts);
        self
    }
    pub fn sub_agent(mut self, a: Arc<dyn Agent>) -> Self {
        self.sub_agents.push(a);
        self
    }
    pub fn max_iterations(mut self, n: u32) -> Self {
        self.max_iterations = n;
        self
    }
    pub fn llm_timeout(mut self, d: Duration) -> Self {
        self.llm_timeout = d;
        self
    }

    /// 设置单工具执行超时（含建连），默认 300s。
    pub fn tool_timeout(mut self, d: Duration) -> Self {
        self.tool_timeout = d;
        self
    }

    /// 设置权限策略（沙箱模式 + 审批策略 + 网络开关），驱动 shell 决策与 prompt 权限层注入。
    pub fn policy(mut self, p: PermissionPolicy) -> Self {
        self.policy = p;
        self
    }

    /// 设置取消令牌（用户点停止时 cancel，run() 内的工具执行 select! 监听它）。
    /// 未设置则用一个独立的永不取消的 token（工具执行不响应停止）。
    pub fn cancel_token(mut self, t: CancellationToken) -> Self {
        self.cancel_token = t;
        self
    }

    /// 注入模型上下文窗口（token），用于动态压缩阈值；None=用 context_config.fallback_context_window。
    pub fn context_window(mut self, w: usize) -> Self {
        self.context_window = Some(w);
        self
    }

    /// 注入压缩专用模型（通常比主模型便宜），None=用主模型压缩。
    pub fn compact_model(mut self, m: Arc<dyn Llm>) -> Self {
        self.compact_model = Some(m);
        self
    }

    /// 注入 context 治理配置（动态压缩阈值/软着陆/截断/chars_per_token），驱动 run() 的 intra-turn 压缩。
    pub fn context_config(mut self, c: ContextConfig) -> Self {
        self.context_config = c;
        self
    }

    /// 注册压缩 hook（pre 可 veto 中止压缩、post 通知）。默认无 hook。
    pub fn compaction_hook(mut self, h: Arc<dyn CompactionHook>) -> Self {
        self.hooks.push(h);
        self
    }

    /// 设置 spawn 嵌套深度（顶层 agent 不用设，默认 0；子 agent 由 ChildAgentFactory 传入 depth+1）。
    pub fn spawn_depth(mut self, d: u32) -> Self {
        self.spawn_depth = d;
        self
    }

    /// 注入子 agent 活动事件出口（server 层传 SSE 转发 sink；不设则用 Noop，子 agent 活动不转发前端）。
    pub fn child_event_sink(mut self, sink: Arc<dyn ChildEventSink>) -> Self {
        self.child_event_sink = sink;
        self
    }

    pub fn build(self) -> anyhow::Result<CortexAgent> {
        let model = self
            .model
            .ok_or_else(|| anyhow::anyhow!("CortexAgent requires a model"))?;
        Ok(CortexAgent {
            name: self.name,
            description: self.description,
            model,
            instruction: self.instruction,
            memory_block: self.memory_block,
            skill_catalog: self.skill_catalog,
            skill_bodies: self.skill_bodies,
            config: self.config,
            tools: self.tools,
            toolsets: self.toolsets,
            sub_agents: self.sub_agents,
            max_iterations: self.max_iterations,
            llm_timeout: self.llm_timeout,
            tool_timeout: self.tool_timeout,
            policy: self.policy,
            cancel_token: self.cancel_token,
            context_window: self.context_window,
            compact_model: self.compact_model,
            context_config: self.context_config,
            hooks: self.hooks,
            spawn_depth: self.spawn_depth,
            child_event_sink: self.child_event_sink,
            budget_handle: self.budget_handle,
        })
    }
}
