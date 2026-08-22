//! 上下文窗口治理状态——压缩开窗计数 + 软着陆 per-window 一次性 flag。
//!
//! 对齐 codex 的 `AutoCompactWindow`：每次压缩视为「开新窗」，窗口号单调递增，
//! 软着陆的 reminder/borrow flag 在开新窗时复位（每窗各最多一次）。
//!
//! 关键语义：**窗口跨 run（用户轮次）存活**。codex 把它挂在会话级 `SessionState`
//! （`state/auto_compact_window.rs`），本项目 agent 无 AppState 访问权，故由 server 层
//! 按 thread_id 维护 [`SharedWindowState`] 经 builder 注入；子 agent 不注入（各自独立窗口）。

/// 会话级持久窗口快照（跨 run 存活；进程内存态，重启清空——与 `session_token_usage` 同取舍）。
///
/// 对齐 codex `AutoCompactWindow::restore`：每 run 开始时从共享句柄恢复，flag 变更/开窗时写回。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WindowStateSnapshot {
    /// 当前窗口号，从 1 开始，每次压缩 +1。
    pub window_number: u32,
    /// 本窗是否已发过软着陆提醒（每窗最多一次）。
    pub reminder_shown: bool,
    /// 本窗是否已借过一轮（buffer 区每窗最多借一次）。
    pub borrowed: bool,
    /// 会话内累计压缩次数（≥2 时前端提示用户新建会话）。
    pub compaction_count: u32,
    /// 最近一次完成请求的 gross total_tokens（跨 run 种子）。
    ///
    /// 没有 it，`last_usage_tokens` 每 run 从 None 起、闸门判定只剩字符估算兜底
    /// （FC args 固定记 64 严重低估工具密集会话）→ borrow 之后 run 一结束，
    /// 下一 run 又回到「估算够不着闸」→ ForceCompact 永不触发。压缩开窗时清空
    /// （旧值对应被重写前的历史，留着会立刻误触发二次压缩）。
    pub last_usage_total: Option<i32>,
    /// 种子记录时的模型上下文窗口（token）。
    ///
    /// 会话不随模型切换重建（按 thread_id 存），但闸门按「本次 run 的模型」计算。
    /// 种子带着旧窗口恢复到新窗口时（如 1M 模型用到 500K 后切 128K 模型），
    /// 占用对旧窗口只是 50%、对新窗口已超硬闸——首个循环即 ForceCompact。
    /// 记录窗口供 run 开头识别失配并告警/刷新前端显示，消除「看似凭空压缩」。
    pub context_window_at_seed: Option<usize>,
}

/// 会话级共享窗口状态句柄（server 层按 thread_id 从 map 取出注入 agent）。
pub type SharedWindowState = std::sync::Arc<std::sync::Mutex<WindowStateSnapshot>>;

/// 单次 run 内的窗口状态（从 [`SharedWindowState`] 恢复；未注入则为全新窗口）。
pub(super) struct WindowState {
    /// 当前窗口号，从 1 开始，每次压缩 +1。
    pub window_number: u32,
    /// 当前窗口 id（UUIDv7，时间有序）。
    pub window_id: String,
    /// 本窗是否已发过软着陆提醒（每窗最多一次）。
    pub reminder_shown: bool,
    /// 本窗是否已借过一轮（buffer 区每窗最多借一次）。
    pub borrowed: bool,
    /// 本 run 内累计压缩次数（≥2 时前端提示用户新建会话）。
    pub compaction_count: u32,
    /// 最近一次完成请求的 gross total_tokens（run 开头从快照恢复作种子，
    /// 见 [`WindowStateSnapshot::last_usage_total`]；子 agent 无共享句柄，仅 run 内有效）。
    pub last_usage_total: Option<i32>,
    /// 种子记录时的窗口（仅 run 内用于失配日志；恢复/开窗语义同上）。
    pub context_window_at_seed: Option<usize>,
}

impl WindowState {
    pub(super) fn new() -> Self {
        Self {
            window_number: 1,
            window_id: uuid::Uuid::now_v7().to_string(),
            reminder_shown: false,
            borrowed: false,
            compaction_count: 0,
            last_usage_total: None,
            context_window_at_seed: None,
        }
    }

    /// 从会话级持久快照恢复（run 开始时；对齐 codex `AutoCompactWindow::restore`）。
    ///
    /// 窗口 id 不持久（进程内标识），恢复时换新 id。
    pub(super) fn restore(snap: WindowStateSnapshot) -> Self {
        Self {
            window_number: snap.window_number.max(1),
            window_id: uuid::Uuid::now_v7().to_string(),
            reminder_shown: snap.reminder_shown,
            borrowed: snap.borrowed,
            compaction_count: snap.compaction_count,
            last_usage_total: snap.last_usage_total,
            context_window_at_seed: snap.context_window_at_seed,
        }
    }

    /// 导出持久快照（flag 变更 / 开窗 / 记录 usage 后写回共享句柄）。
    pub(super) fn snapshot(&self) -> WindowStateSnapshot {
        WindowStateSnapshot {
            window_number: self.window_number,
            reminder_shown: self.reminder_shown,
            borrowed: self.borrowed,
            compaction_count: self.compaction_count,
            last_usage_total: self.last_usage_total,
            context_window_at_seed: self.context_window_at_seed,
        }
    }

    /// 压缩后开新窗：窗口号 +1、新窗口 id、复位软着陆 flag、压缩计数 +1、
    /// 清空跨 run usage 种子（旧 total 对应压缩前历史，留着会误触发二次压缩）。
    pub(super) fn advance(&mut self) {
        self.window_number = self.window_number.saturating_add(1);
        self.window_id = uuid::Uuid::now_v7().to_string();
        self.reminder_shown = false;
        self.borrowed = false;
        self.compaction_count = self.compaction_count.saturating_add(1);
        self.last_usage_total = None;
        self.context_window_at_seed = None;
    }
}

/// 把窗口状态写回会话级共享句柄（软着陆 flag 置位 / 开窗后调用；None=子 agent，no-op）。
pub(super) fn persist_window(shared: &Option<SharedWindowState>, window: &WindowState) {
    if let Some(s) = shared {
        *s.lock().unwrap_or_else(|e| e.into_inner()) = window.snapshot();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restore_roundtrips_flags_and_counters() {
        let mut w = WindowState::new();
        w.reminder_shown = true;
        w.borrowed = true;
        w.advance(); // 开新窗：flag 复位、计数 +1、usage 种子清空
        w.reminder_shown = true; // 新窗又提醒 + 借了一轮
        w.borrowed = true;
        w.last_usage_total = Some(98_765); // 新窗请求记录了 usage
        w.context_window_at_seed = Some(128_000); // 且记下当时的窗口
        let snap = w.snapshot();

        // 跨 run 恢复：flag/计数/usage 种子（含窗口）原样带回
        let restored = WindowState::restore(snap);
        assert_eq!(restored.window_number, 2);
        assert!(restored.reminder_shown);
        assert!(restored.borrowed);
        assert_eq!(restored.compaction_count, 1);
        assert_eq!(restored.last_usage_total, Some(98_765));
        assert_eq!(restored.context_window_at_seed, Some(128_000));
        assert_eq!(restored.snapshot(), snap);
    }

    #[test]
    fn advance_clears_usage_seed() {
        let mut w = WindowState::new();
        w.last_usage_total = Some(120_000); // 压缩前的满窗 total
        w.context_window_at_seed = Some(1_000_000);
        w.advance(); // 压缩开新窗
        assert_eq!(w.last_usage_total, None); // 旧值必须作废，否则误触发二次压缩
        assert_eq!(w.context_window_at_seed, None); // 窗口记录一并作废
    }

    #[test]
    fn persist_writes_through_shared_handle() {
        let shared: SharedWindowState = Default::default();
        let mut w = WindowState::new();
        w.reminder_shown = true;
        persist_window(&Some(shared.clone()), &w);
        assert_eq!(*shared.lock().unwrap(), w.snapshot());

        // 再模拟一个「下一 run」：从句柄恢复 → 借一轮 → 写回
        let mut next = WindowState::restore(*shared.lock().unwrap());
        next.borrowed = true;
        persist_window(&Some(shared.clone()), &next);
        assert!(shared.lock().unwrap().borrowed);
    }

    #[test]
    fn persist_none_is_noop_for_children() {
        persist_window(&None, &WindowState::new()); // 不 panic 即通过
    }
}
