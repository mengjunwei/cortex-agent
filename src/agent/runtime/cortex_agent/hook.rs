//! 压缩 hook——pre/post compact 扩展点（pre 可 veto 中止压缩）。
//!
//! 对齐 codex 的 pre/post compact hook。不复用 monitor 插件（SNMP 专用、无事件总线、
//! 无 veto 通道）。默认无 hook；业务方可注册审计 / 强制保留某些消息 / 拒绝压缩等逻辑。

use adk_rust::async_trait;

/// 压缩上下文（传给 [`CompactionHook::pre_compact`] / [`CompactionHook::post_compact`]）。
///
/// 用值字段（不借用 WindowState），避免与 `window.advance()` 的可变借用冲突。
/// 字段供 hook 实现者读取；默认实现不读，故 allow dead_code。
#[allow(dead_code)]
pub struct CompactionContext {
    pub window_number: u32,
    pub compaction_count: u32,
    pub before_tokens: usize,
}

/// 压缩后的结果（传给 [`CompactionHook::post_compact`]）。
#[allow(dead_code)]
pub struct CompactionResult {
    pub after_tokens: usize,
    pub window_number: u32,
}

/// pre_compact 的决策。`Abort` 变体预留给 veto 场景（默认实现返回 `Proceed`）。
#[allow(dead_code)]
pub enum CompactionDecision {
    /// 继续压缩。
    Proceed,
    /// 中止压缩（veto）——保持原历史不变。
    Abort,
}

/// 压缩 hook trait。默认实现：pre 放行、post 无操作。
#[async_trait]
pub trait CompactionHook: Send + Sync {
    /// 压缩前调用；返回 [`CompactionDecision::Abort`] 可阻止压缩（在改 history 之前）。
    async fn pre_compact(&self, _ctx: &CompactionContext) -> CompactionDecision {
        CompactionDecision::Proceed
    }
    /// 压缩成功后调用（history 已替换）。
    async fn post_compact(&self, _ctx: &CompactionContext, _result: &CompactionResult) {}
}
