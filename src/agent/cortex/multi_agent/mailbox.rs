//! 主循环 mailbox 消费 —— 父 agent 的 conv 注入（对齐 codex record_inter_agent_communication）。

use std::collections::VecDeque;
use std::sync::Mutex as StdMutex;

/// 待注入主 agent conv 的消息（SSE 层无感知；由 run 主循环每轮 drain）。
pub(crate) struct ParentMailbox {
    items: StdMutex<VecDeque<String>>,
}

impl ParentMailbox {
    pub(crate) fn new() -> Self {
        Self {
            items: StdMutex::new(VecDeque::new()),
        }
    }
    /// 投递一条渲染好的消息（子 agent FINAL_ANSWER / 兄弟 agent MESSAGE）。
    pub(crate) fn push(&self, rendered: String) {
        self.items
            .lock()
            .expect("parent mailbox poisoned")
            .push_back(rendered);
    }
    /// drain 全部待注入消息（主循环每轮调用，注入为 user-role 消息）。
    pub(crate) fn drain(&self) -> Vec<String> {
        let mut q = self.items.lock().expect("parent mailbox poisoned");
        q.drain(..).collect()
    }
    /// 当前待注入条数（wait_agent 的 pending 检测 + 诊断/测试）。
    pub(crate) fn len(&self) -> usize {
        self.items.lock().expect("parent mailbox poisoned").len()
    }
    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parent_mailbox_drain() {
        let mb = ParentMailbox::new();
        assert!(mb.is_empty());
        mb.push("m1".into());
        mb.push("m2".into());
        assert_eq!(mb.len(), 2);
        assert_eq!(mb.drain(), vec!["m1", "m2"]);
        assert!(mb.is_empty());
    }
}
