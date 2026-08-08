//! 三级软着陆——硬压缩前给模型主动收尾的机会（对齐 codex token_budget 三级）。
//!
//! 把「正常 → 直接 LLM 摘要替换」的一刀切，改成：
//! ① 接近软闸时提醒模型自己收尾 → ② 到软闸但在 buffer 区借最后一轮 → ③ 撞硬闸才强制压缩。
//! 避免用户体感「突然失忆」。

use super::window::WindowState;

/// 软着陆决策。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SoftLandingDecision {
    /// token 充裕，正常发请求。
    Nominal,
    /// 接近软闸（剩余 ≤ 提醒阈值）且本窗未提醒过 → 注入提醒，让模型主动写收尾/笔记。
    Remind,
    /// 已到软闸但还在 buffer 区（soft ≤ used < hard）且本窗未借过 → 借最后一轮。
    BorrowOneTurn,
    /// 已撞硬闸或已借过 → 走 LLM 摘要压缩。
    ForceCompact,
}

/// 评估软着陆决策。
///
/// - `effective_tokens`：扣掉缓存前缀后的净 token（BodyAfterPrefix）。
/// - `soft_gate`：软闸（context_window × 0.9），到软闸进入 buffer 区。
/// - `hard_gate`：硬闸（context_window × 0.95），到硬闸强制压缩。
/// - `reminder_threshold_tokens`：提醒阈值（剩余 token ≤ 此值时提醒，约窗口的 15%）。
/// - `window`：per-window 一次性 flag。
pub(super) fn evaluate_soft_landing(
    effective_tokens: usize,
    soft_gate: usize,
    hard_gate: usize,
    reminder_threshold_tokens: usize,
    window: &WindowState,
) -> SoftLandingDecision {
    // 已撞硬闸 → 强制压缩
    if effective_tokens >= hard_gate {
        return SoftLandingDecision::ForceCompact;
    }
    // 在 buffer 区（soft ≤ used < hard）→ 借一轮（每窗最多一次），已借过则压缩
    if effective_tokens >= soft_gate {
        return if window.borrowed {
            SoftLandingDecision::ForceCompact
        } else {
            SoftLandingDecision::BorrowOneTurn
        };
    }
    // 接近软闸：剩余 ≤ 提醒阈值 → 提醒（每窗最多一次）
    if soft_gate.saturating_sub(effective_tokens) <= reminder_threshold_tokens
        && !window.reminder_shown
    {
        return SoftLandingDecision::Remind;
    }
    SoftLandingDecision::Nominal
}

/// 软着陆提醒模板（注入 user-role，告诉模型窗口将满、笔记/记忆会跨压缩持久，鼓励主动收尾）。
///
/// 对齐 codex 最新 `DEFAULT_TOKEN_BUDGET_REMINDER_MESSAGE_TEMPLATE` 的关键改动：
/// 强调「笔记/历史项跨窗口持久」——引导模型在压缩前把关键信息落盘，而非指望摘要兜底，
/// 显著减轻"压缩后失忆"。本项目走 LLM 摘要（非纯重置），故保留 summarized 措辞。
pub(super) fn reminder_message(remaining_tokens: usize, window_number: u32) -> String {
    format!(
        "Your context window is nearly exhausted (only about {remaining_tokens} tokens remaining) \
         and will be automatically compacted soon. Once compacted, older messages in this window \
         will be summarized, but your notes, memory, and the task context persist across \
         compaction. Please proactively wrap up the current sub-task, record any key \
         results/decisions/data as a concise note so they survive the compaction, and avoid \
         starting new long-running work. (context window #{window_number})"
    )
}

/// 「借最后一轮」提示（注入 user-role，让模型在压缩前写好交接）。
///
/// 补「笔记跨压缩持久」引导（对齐 codex fallback 提示的持久化语义）。
pub(super) fn borrow_message() -> &'static str {
    "You are in the final turn before an automatic context compaction. Use this turn to finish \
     the in-flight work and write a clear handoff (progress, key decisions, next steps, critical \
     data) — record anything that must survive as a note, since notes persist across compaction \
     while older messages get summarized. Do not start new open-ended tasks."
}
