//! 多智能体动态并行——V2 工具集（对齐 codex `multi_agents_v2`）。
//!
//! 模型在运行时自主 fork 子 agent，子 agent 是**持久会话循环**（不是一次性 run）：
//! spawn 后常驻，可收 `followup_task` 触发新 turn、收 `send_message` 排队下轮注入，
//! 最终答案经 mailbox 以 `Message Type: FINAL_ANSWER` 信封投回父 agent 的 conv。
//!
//! 与 codex 的基建差异取舍：
//! - codex 每个子 agent 是独立 CodexThread（rollout 持久化 + residency 换出）；
//!   cortex 是进程内 tokio task + Weak 注册表（主 run 结束树即消亡，无需换出）。
//! - codex 的 wait_agent 等 input_queue activity；cortex 等价的树级 activity watch。
//! - codex 的 AgentPath 加密通信（encrypted_content）；cortex 单进程明文即可。
//!
//! 子模块划分（原单文件 2500 行按分节横幅拆分）：
//! - [`envelope`]：InterAgent 信封格式（Message Type: ... 渲染与截断）
//! - [`status`]：子 agent 运行状态 + spawn 深度校验
//! - [`fork`]：fork_turns 解析与 fork 历史过滤
//! - [`tree`]：昵称池 + AgentTree 全树注册表（句柄/信箱/activity 信号）
//! - [`mailbox`]：ParentMailbox（主循环 conv 注入队列）
//! - [`session`]：ChildSession（子 agent 持久历史）
//! - [`blueprint`]：AgentBlueprint（从父克隆构建参数）
//! - [`factory`]：ChildAgentFactory（spawn/消息/中断/列表逻辑）
//! - [`child_loop`]：子 agent 持久会话循环（turn → FINAL_ANSWER → 等 trigger）
//! - [`tools`]：六个模型侧工具（spawn/send_message/followup/wait/interrupt/list）

mod blueprint;
mod child_loop;
mod envelope;
mod factory;
mod fork;
mod mailbox;
mod session;
mod status;
mod tools;
mod tree;

pub(crate) use envelope::{envelope_type_of, InterAgentMessageType};
pub(crate) use factory::ChildAgentFactory;
pub(crate) use mailbox::ParentMailbox;
pub(crate) use tree::{AgentTree, ChildHandle};

use std::sync::atomic::AtomicU64;
use std::sync::Arc;

/// 子 agent token 用量共享累加器（进程内原子累计，SSE 层只读轮询）。
///
/// 语义（对齐 codex `token_info` 的 add_assign 累计）：每个子 agent 在自己的 run 循环里，
/// 把**每次 LLM 请求末帧的真实 usage**（流式下末帧即该请求总量，取 last-wins 防中间帧重复计数）
/// `fetch_add` 进来；父 agent（spawn_depth=0）不写 —— 父用量由 SSE 从主事件流读取，
/// 两者数据源不相交 → 不会双重计数。孙 agent 经 [`AgentBlueprint`] 继承同一 Arc → 全树只计一次。
pub type ChildUsageTotal = Arc<AtomicU64>;

// ============================================================================
// 子 agent 事件出口（① 可视化：子 agent 活动转发到主 SSE 流）
// ============================================================================

/// 子 agent 活动事件。
#[derive(Clone, Debug)]
pub enum ChildAgentEvent {
    /// 子 agent 开始运行。
    Started { task_name: String },
    /// 子 agent 的一段文本输出（增量 delta）。
    Text { task_name: String, delta: String },
    /// 子 agent 发起一次工具调用。
    ToolCall {
        task_name: String,
        tool_call_id: String,
        name: String,
        args: String,
    },
    /// 子 agent 的工具调用返回结果。
    ToolResult {
        task_name: String,
        tool_call_id: String,
        name: String,
        content: String,
    },
    /// 子 agent 运行结束（ok=true 成功；result=最终文本或错误说明）。
    Finished {
        task_name: String,
        ok: bool,
        result: String,
    },
}

/// 子 agent 事件出口。server 层实现转 SSE；默认 [`NoopChildEventSink`] 丢弃。
pub trait ChildEventSink: Send + Sync {
    fn emit(&self, event: ChildAgentEvent);
}

/// 空 sink：非 SSE 场景 / 测试用，丢弃所有事件。
pub struct NoopChildEventSink;
impl ChildEventSink for NoopChildEventSink {
    fn emit(&self, _event: ChildAgentEvent) {}
}
