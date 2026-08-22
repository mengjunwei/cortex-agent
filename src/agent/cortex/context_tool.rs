//! `get_context_remaining` 工具——让模型自查剩余 token 预算，避免盲目撞墙触发中途压缩。
//!
//! 对齐 codex `get_context_remaining`（`core/src/tools/handlers/get_context_remaining.rs`）：
//! 模型可主动查询"我还剩多少 token / 是否即将压缩"，据此决定是否收尾、是否避免开启新的
//! 大型多步操作。直接减少浪费的轮次与"进行中工作被中途压缩截断"的尴尬，提升长任务的智能性。
//!
//! 机制：run 主循环每轮把当前 `effective_tokens`（gross 占用量）写进共享快照，
//! 工具只读快照。模型在一次请求中调用本工具时，读到的是本轮请求开始时的预算估计。

use std::sync::{Arc, RwLock};

use adk_rust::async_trait;
use adk_rust::serde_json::{Value, json};
use adk_rust::{Result, Tool, ToolContext};

/// 运行时预算快照（由 run 主循环每轮更新，工具只读）。
#[derive(Clone, Default)]
pub(crate) struct ContextBudgetSnapshot {
    /// 上下文占用 token（gross：上轮 total_tokens + 新增条目估算，不减 cache_read；
    /// 对齐 codex Total scope，与软着陆判定/CONTEXT_USAGE 同口径）。
    pub effective_tokens: usize,
    /// 模型上下文窗口（token）。
    pub context_window: usize,
    /// 软闸（context_window × 0.95），到软闸进入 buffer 区。
    pub soft_gate: usize,
    /// 硬闸（context_window × 0.95），到硬闸强制压缩。
    pub hard_gate: usize,
    /// 当前上下文窗口号（每次压缩 +1；会话级持久，跨 run 递增）。
    pub window_number: u32,
    /// 会话内累计压缩次数（≥2 前端提示新建会话；随窗口状态跨 run 持久）。
    pub compaction_count: u32,
}

/// 共享预算句柄：run 主循环写、`get_context_remaining` 工具读。
pub(crate) type SharedBudget = Arc<RwLock<ContextBudgetSnapshot>>;

/// 创建共享预算句柄（run 开始时调用）。
pub(crate) fn new_shared_budget() -> SharedBudget {
    Arc::new(RwLock::new(ContextBudgetSnapshot::default()))
}

pub(crate) struct GetContextRemainingTool {
    budget: SharedBudget,
}

impl GetContextRemainingTool {
    pub(crate) fn new(budget: SharedBudget) -> Self {
        Self { budget }
    }
}

pub(crate) const GET_CONTEXT_REMAINING_TOOL_NAME: &str = "get_context_remaining";

const TOOL_DESC: &str = "Check how many tokens remain in the current context window before it is automatically compacted (summarized). Call this BEFORE starting any large or multi-step operation, before reading many files, or whenever you are unsure how much context budget is left. Use the result to decide whether it is safe to start new work or whether you should wrap up the current sub-task first. Returns the remaining token budget, the total window size, and the current window number.";

#[async_trait]
impl Tool for GetContextRemainingTool {
    fn name(&self) -> &str {
        GET_CONTEXT_REMAINING_TOOL_NAME
    }
    fn description(&self) -> &str {
        TOOL_DESC
    }
    /// 纯只读：不改变任何状态，可并发。
    fn is_read_only(&self) -> bool {
        true
    }
    fn is_concurrency_safe(&self) -> bool {
        true
    }
    /// 无入参（给一个空 object schema，兼容要求 parameters 字段的 provider）。
    fn parameters_schema(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }))
    }
    async fn execute(&self, _ctx: Arc<dyn ToolContext>, _args: Value) -> Result<Value> {
        let snap = self.budget.read().map(|g| g.clone()).unwrap_or_default();
        Ok(budget_result_json(&snap))
    }
}

/// 纯函数：把预算快照转成回给模型的 JSON（execute 的可测试核心）。
pub(crate) fn budget_result_json(snap: &ContextBudgetSnapshot) -> Value {
    let remaining = snap.context_window.saturating_sub(snap.effective_tokens);
    // 接近/已达软闸 → 提醒模型避免开启新大任务、主动收尾并记录关键结果。
    let near_compaction = snap.effective_tokens >= snap.soft_gate;
    let will_compact_now = snap.effective_tokens >= snap.hard_gate;
    let suffix = if will_compact_now {
        " The context window is full and will be compacted on the next step. Do NOT start any new operation — finish the in-flight work and write a concise handoff (progress, key decisions, next steps, critical data) so the next window can continue."
    } else if near_compaction {
        " The window is nearly full and will be compacted soon. Avoid starting new large operations; wrap up the current sub-task and record key results as a note (notes persist across compaction)."
    } else {
        ""
    };
    let message = format!(
        "You have {remaining} tokens left in this context window (window #{win}, used ~{used}/{total}).{suffix}",
        win = snap.window_number,
        used = snap.effective_tokens,
        total = snap.context_window,
    );
    json!({
        "tokens_left": remaining,
        "context_window": snap.context_window,
        "used_tokens": snap.effective_tokens,
        "window_number": snap.window_number,
        "near_compaction": near_compaction,
        "will_compact_now": will_compact_now,
        "message": message,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(used: usize, window: usize, soft: usize, hard: usize) -> ContextBudgetSnapshot {
        ContextBudgetSnapshot {
            effective_tokens: used,
            context_window: window,
            soft_gate: soft,
            hard_gate: hard,
            window_number: 2,
            compaction_count: 1,
        }
    }

    #[test]
    fn nominal_reports_remaining_and_no_warning() {
        // 128K 窗口，用了 40K，软闸 115K → 充裕，无 near_compaction 引导。
        let r = budget_result_json(&snap(40_000, 128_000, 115_200, 121_600));
        assert_eq!(r["tokens_left"], 88_000);
        assert_eq!(r["near_compaction"], false);
        assert_eq!(r["will_compact_now"], false);
        let msg = r["message"].as_str().unwrap();
        assert!(msg.contains("88000 tokens left"));
        assert!(!msg.contains("compacted soon"));
    }

    #[test]
    fn near_soft_gate_flags_compaction_warning() {
        // 达到软闸 → near_compaction=true，引导避免新大任务。
        let r = budget_result_json(&snap(115_200, 128_000, 115_200, 121_600));
        assert_eq!(r["near_compaction"], true);
        assert_eq!(r["will_compact_now"], false);
        assert!(r["message"].as_str().unwrap().contains("compacted soon"));
    }

    #[test]
    fn at_hard_gate_demands_handoff() {
        // 撞硬闸 → will_compact_now=true，引导写交接而非开新操作。
        let r = budget_result_json(&snap(125_000, 128_000, 115_200, 121_600));
        assert_eq!(r["will_compact_now"], true);
        let msg = r["message"].as_str().unwrap();
        assert!(msg.contains("write a concise handoff"));
    }

    #[test]
    fn over_window_clamps_remaining_to_zero() {
        // 净用量超过窗口（异常但可能）→ remaining 不为负。
        let r = budget_result_json(&snap(200_000, 128_000, 115_200, 121_600));
        assert_eq!(r["tokens_left"], 0);
        assert_eq!(r["will_compact_now"], true);
    }
}
