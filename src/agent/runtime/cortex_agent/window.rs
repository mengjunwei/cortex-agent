//! 上下文窗口治理状态——压缩开窗计数 + 软着陆 per-window 一次性 flag。
//!
//! 对齐 codex 的 `AutoCompactWindow`：每次压缩视为「开新窗」，窗口号单调递增，
//! 软着陆的 reminder/borrow flag 在开新窗时复位（每窗各最多一次）。

/// 单次 run 内的窗口状态（局部，不持久化——持久化由 `actions.compaction` 承载）。
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
}

impl WindowState {
    pub(super) fn new() -> Self {
        Self {
            window_number: 1,
            window_id: uuid::Uuid::now_v7().to_string(),
            reminder_shown: false,
            borrowed: false,
            compaction_count: 0,
        }
    }

    /// 压缩后开新窗：窗口号 +1、新窗口 id、复位软着陆 flag、压缩计数 +1。
    pub(super) fn advance(&mut self) {
        self.window_number = self.window_number.saturating_add(1);
        self.window_id = uuid::Uuid::now_v7().to_string();
        self.reminder_shown = false;
        self.borrowed = false;
        self.compaction_count = self.compaction_count.saturating_add(1);
    }
}
