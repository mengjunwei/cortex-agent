//! 压缩埋点——结构化 tracing 字段（OTLP 自动索引），零新依赖。
//!
//! 对齐 codex `CompactionAnalyticsAttempt`：记录每次压缩的 phase/reason/前后 token/
//! cache 命中/耗时/窗口号，便于离线分析压缩效果与调优。

/// 发一条结构化的压缩埋点。
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_compaction(
    phase: &str,
    reason: &str,
    before_tokens: usize,
    after_tokens: usize,
    cache_read_before: Option<i32>,
    elapsed_ms: u64,
    window_number: u32,
    retained_tail: usize,
) {
    tracing::info!(
        target: "cortex.compaction",
        phase = phase,
        reason = reason,
        before_tokens = before_tokens,
        after_tokens = after_tokens,
        cache_read_tokens_before = ?cache_read_before,
        elapsed_ms = elapsed_ms,
        window_number = window_number,
        retained_tail = retained_tail,
        "context compaction"
    );
}
