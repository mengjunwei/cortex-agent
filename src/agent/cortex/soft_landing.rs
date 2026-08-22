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
/// - `effective_tokens`：gross 占用（上一响应 total_tokens + 其后新增条目的字符
///   估算，不减 cache_read——净口径会随缓存命中在软/硬闸间振荡，见 mod.rs 口径
///   注释）。
/// - `soft_gate`：软闸（context_window × 0.95），到软闸进入 buffer 区。
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

/// 软着陆提醒模板（注入 user-role）。
///
/// 对齐 codex `DEFAULT_TOKEN_BUDGET_REMINDER_MESSAGE_TEMPLATE`（config/mod.rs:1072）：
/// **纯信息性**——只说「将自动为你压缩 + 笔记跨压缩持久」，不含 wrap up / avoid
/// starting new work 之类指令。实测教训：指令式收束措辞会让模型把提醒当成「压缩
/// 是需等待配合的外部事件」而拒绝继续工作——提醒区（0.75~0.9×窗口）模型一旦停摆，
/// 上下文不再增长，软/硬闸永远够不着，压缩永不触发（死锁：模型等压缩、系统等
/// 模型用上下文）。差异仅一处：codex 走纯重置（cleared），本项目走 LLM 摘要
/// （summarized），措辞相应替换。
pub(super) fn reminder_message(remaining_tokens: usize, window_number: u32) -> String {
    format!(
        "Your context window is nearly exhausted (only about {remaining_tokens} tokens remaining) \
         and will be automatically compacted for you soon. Once compacted, older messages in \
         this window will be summarized, but your notes, memory, and the task context persist \
         across compaction. (context window #{window_number})"
    )
}

/// 「借最后一轮」提示（注入 user-role）。
///
/// codex 的 fallback prompt 由模型侧下发、仓库无硬编码默认；语义 = 到线后建议写
/// 交接笔记。同样保持信息性（无禁令、明确全自动），防止停摆误解。
pub(super) fn borrow_message() -> &'static str {
    "The context soft limit is reached and automatic compaction will run for you soon. Notes \
     and task files persist across compaction, so recording a concise handoff (progress, key \
     decisions, next steps, critical data) as a note is recommended. No action is required to \
     trigger the compaction — it is fully automatic."
}
