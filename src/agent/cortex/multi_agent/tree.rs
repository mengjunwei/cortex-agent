//! AgentTree —— 全树共享注册表（对齐 codex AgentRegistry + AgentControl，进程内版）
//! + 昵称池（对齐 codex agent_names.txt + reserve_agent_nickname）。

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use adk_rust::tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use super::super::role::AgentRole;
use super::mailbox::ParentMailbox;
use super::status::ChildStatus;

const AGENT_NAMES: &str = include_str!("agent_names.txt");

/// 从候选池随机挑一个未占用昵称；池耗尽时清空已用集重掷（对齐 codex 行为，
/// 但不加序号后缀——进程内树生命周期短，冲突概率可忽略；codex 的
/// `{name} the {n}th` 处理依赖跨会话持久 registry，此处无此场景）。
fn reserve_nickname(
    used: &mut std::collections::HashSet<String>,
    candidates: &[String],
) -> Option<String> {
    // 简单确定性选择：从池头开始找第一个未用的（进程内树生命周期短，无需真随机；
    // codex 用 rand 随机是给长生命周期 registry 均匀分布，此处顺序取即可且可测试）。
    for _ in 0..2 {
        for name in candidates {
            if !used.contains(name) {
                used.insert(name.clone());
                return Some(name.clone());
            }
        }
        used.clear(); // 池一轮耗尽：清空重置（第二轮必命中）
    }
    None
}

/// 子 agent 的 mailbox 消息（对齐 codex PendingMailboxCommunication）。
pub(crate) struct MailboxItem {
    /// 信封渲染文本（Message Type: ... 格式，注入子 agent conv）
    pub rendered: String,
    /// 是否触发新 turn（followup_task=true / send_message=false / FINAL_ANSWER 回投=false）。
    /// 投递方在入队前按它决定是否 wake；入队后 drain 侧不再读（语义标注保留，
    /// 供未来「QueueOnly 延迟到下轮」的 codex 对齐行为使用）。
    #[allow(dead_code)]
    pub trigger_turn: bool,
}

/// activity 信号（对齐 codex InputQueueActivity：Mailbox 唤醒 wait_agent）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TreeActivity {
    Mailbox,
}

/// 树内一个子 agent 的共享句柄。
pub(crate) struct ChildHandle {
    /// canonical path（如 `/root/task_1/sub`；句柄自描述，树以 map key 寻址）
    #[allow(dead_code)]
    pub path: String,
    pub nickname: Option<String>,
    pub status: watch::Sender<ChildStatus>,
    /// mailbox：待注入子 agent conv 的消息队列（无界，对齐 codex VecDeque）
    pub inbox: StdMutex<VecDeque<MailboxItem>>,
    /// 触发子 agent 循环新一轮（followup_task / spawn 初始任务）
    pub wake: tokio::sync::Notify,
    /// 中断当前 turn（interrupt_agent）；复活（followup 终态重启）时整体换新 token。
    /// 放 Mutex 供 Arc 外换新（CancellationToken 本体无 reset）。
    pub interrupt: StdMutex<CancellationToken>,
    /// 轮内注入日志：run() 每轮 drain inbox 注入 conv 的消息副本。run() 的局部 conv 随
    /// turn 结束丢弃，不落回 session 则 followup 的新 turn 丢失这些上下文——
    /// run_child_loop 在 turn 结束后 take 并写入 session。
    pub injected_log: StdMutex<Vec<adk_rust::Content>>,
}

impl ChildHandle {
    pub(super) fn push_mailbox(&self, item: MailboxItem) {
        self.inbox
            .lock()
            .expect("child inbox poisoned")
            .push_back(item);
    }
}

/// 树内 agent 的共享句柄（强引用在后台 task 闭包 + 树注册表中持有；
/// 主 run 结束树 drop → cancel_token 级联取消子循环）。
type WeakChild = Arc<ChildHandle>;

/// agent 树：path 寻址 + 状态/信箱/活动信号 + 并发闸门。
///
/// 生命周期：主 run 的 registry 持 Arc<AgentTree>，子 agent 后台 task 持 Weak →
/// 主 run 结束 drop 树 → cancel_token 级联取消全部子循环，无泄漏。
pub(crate) struct AgentTree {
    inner: StdMutex<TreeInner>,
    /// 并发上限（0=不限；对齐 codex effective_agent_max_threads 语义——只限子 agent，
    /// root 不占槽）。真正的闸在 try_register 的锁内原子检查（对齐 codex registry 的
    /// try_increment_spawned CAS），不再用信号量（旧 exec_semaphore 从未 acquire 是死代码）。
    max_concurrent: usize,
    /// 全树共享的 activity 信号（wait_agent 订阅；对齐 codex InputQueueActivity watch）
    activity_tx: watch::Sender<TreeActivity>,
    /// 已用昵称集（树级去重）
    used_nicknames: StdMutex<std::collections::HashSet<String>>,
    /// root 收件箱：子 agent 显式 send_message("/root") 的投递目标（root run 时设置；
    /// 树本身被 root 与全部子 agent 共享，故由树代持）。
    root_mailbox: StdMutex<Option<Arc<ParentMailbox>>>,
}

struct TreeInner {
    /// canonical path → child（含祖先链：/root/task_1 与 /root/task_1/sub 都在）
    children: HashMap<String, WeakChild>,
    /// 树内 spawn 总数（不限并发上限时也计数，供诊断）
    total_spawned: AtomicU64,
}

impl AgentTree {
    pub(crate) fn new(max_concurrent: usize) -> Self {
        let (activity_tx, _) = watch::channel(TreeActivity::Mailbox);
        Self {
            inner: StdMutex::new(TreeInner {
                children: HashMap::new(),
                total_spawned: AtomicU64::new(0),
            }),
            max_concurrent,
            activity_tx,
            used_nicknames: StdMutex::new(std::collections::HashSet::new()),
            root_mailbox: StdMutex::new(None),
        }
    }

    /// 绑定 root 收件箱（root 的 run() 调用；子 agent 继承树时不覆盖）。
    pub(crate) fn bind_root_mailbox(&self, mb: Arc<ParentMailbox>) {
        let mut slot = self.root_mailbox.lock().expect("root mailbox poisoned");
        if slot.is_none() {
            *slot = Some(mb);
        }
    }

    /// 原子「容量检查 + 注册」（对齐 codex registry::try_increment_spawned 的 CAS 占位）：
    /// 同一把 inner 锁内完成非终态计数 + 插入 + 计数递增，消除裸预检与注册之间的
    /// TOCTOU 窗口（root 与子 agent 可并发 spawn，分离的 check-then-act 会双双通过）。
    pub(crate) fn try_register_with_capacity(
        &self,
        path: &str,
        child: WeakChild,
    ) -> std::result::Result<(), usize> {
        let mut inner = self.inner.lock().expect("tree lock poisoned");
        if self.max_concurrent > 0 {
            let running = inner
                .children
                .values()
                .filter(|c| !c.status.borrow().is_terminal())
                .count();
            if running >= self.max_concurrent {
                return Err(self.max_concurrent);
            }
        }
        inner.children.insert(path.to_string(), child);
        inner.total_spawned.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// 复活容量检查（对齐 codex trigger_turn 投递前的 ensure_execution_capacity_for_turn_start：
    /// 终态 agent 被 followup/同名 spawn 复活时重新占一个运行位——复活后 Running，
    /// 不检查则可借「反复复活」无限突破 max_concurrent_children）。
    pub(crate) fn check_revive_capacity(&self) -> std::result::Result<(), usize> {
        if self.max_concurrent == 0 {
            return Ok(());
        }
        let inner = self.inner.lock().expect("tree lock poisoned");
        let running = inner
            .children
            .values()
            .filter(|c| !c.status.borrow().is_terminal())
            .count();
        if running >= self.max_concurrent {
            return Err(self.max_concurrent);
        }
        Ok(())
    }

    /// 取子 agent 句柄。
    pub(crate) fn get(&self, path: &str) -> Option<WeakChild> {
        self.inner
            .lock()
            .expect("tree lock poisoned")
            .children
            .get(path)
            .cloned()
    }

    /// 列出树内全部子 agent（canonical path 字典序，对齐 codex list_agents 排序）。
    pub(crate) fn list_paths(&self) -> Vec<String> {
        let mut paths: Vec<String> = self
            .inner
            .lock()
            .expect("tree lock poisoned")
            .children
            .keys()
            .cloned()
            .collect();
        paths.sort();
        paths
    }

    /// 树内所有 inbox 的待处理消息总数（wait_agent 的「已有 pending」即时检测；
    /// 对齐 codex subscribe_activity 的 pending_activity 语义——watch subscribe 后
    /// has_changed 恒 false，不能靠它检测投递早于订阅的消息）。
    pub(crate) fn pending_mail_count(&self) -> usize {
        let inner = self.inner.lock().expect("tree lock poisoned");
        inner
            .children
            .values()
            .map(|c| c.inbox.lock().expect("child inbox poisoned").len())
            .sum()
    }

    /// 预留一个昵称（角色候选优先，回落全局池；对齐 codex agent_nickname_candidates）。
    pub(crate) fn reserve_nickname(&self, role: &AgentRole) -> Option<String> {
        let candidates: Vec<String> = role.nickname_candidates.clone().unwrap_or_else(|| {
            AGENT_NAMES
                .lines()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect()
        });
        let mut used = self.used_nicknames.lock().expect("nickname lock poisoned");
        reserve_nickname(&mut used, &candidates)
    }

    /// 投递消息到目标 mailbox 并广播 activity（对齐 codex enqueue_mailbox_communication）。
    /// trigger_turn=true 时唤醒子 agent 循环。
    pub(crate) fn deliver(
        &self,
        target_path: &str,
        item: MailboxItem,
    ) -> std::result::Result<(), String> {
        // root 特判：root 不在 children map（它是树的持有者），显式 send_message("/root")
        // 投到 root 邮箱（对齐 codex register_session_root 的可寻址性）。
        if target_path == "/root" {
            let mb = self
                .root_mailbox
                .lock()
                .expect("root mailbox poisoned")
                .clone();
            if let Some(mb) = mb {
                mb.push(item.rendered);
                let _ = self.activity_tx.send_replace(TreeActivity::Mailbox);
                return Ok(());
            }
            return Err("live agent path `/root` not found".to_string());
        }
        let child = self
            .get(target_path)
            .ok_or_else(|| format!("live agent path `{target_path}` not found"))?;
        child.push_mailbox(item);
        let _ = self.activity_tx.send_replace(TreeActivity::Mailbox);
        Ok(())
    }

    /// 广播 mailbox 活动（子 agent 自己 drain inbox 时也调用，唤醒 wait_agent）。
    pub(crate) fn notify_activity(&self) {
        let _ = self.activity_tx.send_replace(TreeActivity::Mailbox);
    }

    /// wait_agent 订阅 activity。
    pub(crate) fn subscribe_activity(&self) -> watch::Receiver<TreeActivity> {
        self.activity_tx.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nickname_reservation() {
        let mut used = std::collections::HashSet::new();
        let pool = vec!["Euclid".to_string(), "Archimedes".to_string()];
        let a = reserve_nickname(&mut used, &pool).unwrap();
        let b = reserve_nickname(&mut used, &pool).unwrap();
        assert_ne!(a, b);
        // 池耗尽 → 清空重置后仍能取到
        let c = reserve_nickname(&mut used, &pool).unwrap();
        assert!(pool.contains(&c));
    }
}
