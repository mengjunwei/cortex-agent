//! Shell 命令审批注册表 — 管理待审批的 shell 命令生命周期
//!
//! 当 shell_command 工具判定命令需要用户审批时：
//! 1. 生成 approval_id
//! 2. 在 registry 注册一个 oneshot channel
//! 3. 通过 SSE 发送审批请求到前端
//! 4. await oneshot receiver（带超时）
//! 5. 用户通过 HTTP 端点回填决定 → resolve() 唤醒 oneshot

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{Mutex, oneshot};

/// 用户审批决定
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalDecision {
    Approved,
    Rejected,
}

/// 全局审批注册表，存在 AppState 中
#[derive(Default)]
pub struct ShellApprovalRegistry {
    /// approval_id → (session_id, oneshot sender)
    ///
    /// 记录 session_id 是为了支持「按 session 取消」：用户点停止时，cancel 接口按 session_id
    /// 找出所有 pending sender 并 drop，让对应的 oneshot receiver 立即返回 Err，解锁卡在
    /// `request_approval` 的工具（见 [`ShellApprovalRegistry::cancel_session`]）。
    pending: Mutex<HashMap<String, (String, oneshot::Sender<ApprovalDecision>)>>,
}

impl ShellApprovalRegistry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// 注册一个待审批项
    ///
    /// 返回 receiver，调用方 await 它等待用户决定。
    /// 如果同一 approval_id 已存在，旧的会被丢弃（sender drop → receiver 返回 Err）。
    pub async fn register(
        &self,
        approval_id: &str,
        session_id: &str,
    ) -> oneshot::Receiver<ApprovalDecision> {
        let (tx, rx) = oneshot::channel();
        let mut guard = self.pending.lock().await;
        guard.insert(approval_id.to_string(), (session_id.to_string(), tx));
        rx
    }

    /// 用户审批结果回填（由 HTTP 端点调用）
    ///
    /// 返回 true 表示成功唤醒等待方，false 表示 approval_id 不存在（可能已超时）。
    pub async fn resolve(&self, approval_id: &str, decision: ApprovalDecision) -> bool {
        let mut guard = self.pending.lock().await;
        match guard.remove(approval_id) {
            Some((_, tx)) => {
                let _ = tx.send(decision);
                true
            }
            None => false,
        }
    }

    /// 清理指定 approval_id（超时时调用）
    pub async fn remove(&self, approval_id: &str) {
        let mut guard = self.pending.lock().await;
        guard.remove(approval_id);
    }

    /// 取消某 session 的所有待审批项（用户点"停止"时由 cancel 接口调用）。
    ///
    /// sender 被 drop → 对应 oneshot receiver 立即返回 Err → `request_approval` 的
    /// select!/timeout 分支解锁，工具立即返回。返回被取消的数量。
    pub async fn cancel_session(&self, session_id: &str) -> usize {
        let mut guard = self.pending.lock().await;
        let to_remove: Vec<String> = guard
            .iter()
            .filter(|(_, (sid, _))| sid == session_id)
            .map(|(id, _)| id.clone())
            .collect();
        let n = to_remove.len();
        for id in to_remove {
            guard.remove(&id);
        }
        n
    }

    /// 当前待审批数量（用于监控/日志）
    pub async fn pending_count(&self) -> usize {
        self.pending.lock().await.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn register_and_resolve_approved() {
        let registry = ShellApprovalRegistry::new();
        let rx = registry.register("test-1", "s").await;
        assert!(registry.resolve("test-1", ApprovalDecision::Approved).await);
        let decision = rx.await.unwrap();
        assert_eq!(decision, ApprovalDecision::Approved);
    }

    #[tokio::test]
    async fn register_and_resolve_rejected() {
        let registry = ShellApprovalRegistry::new();
        let rx = registry.register("test-2", "s").await;
        registry.resolve("test-2", ApprovalDecision::Rejected).await;
        let decision = rx.await.unwrap();
        assert_eq!(decision, ApprovalDecision::Rejected);
    }

    #[tokio::test]
    async fn resolve_nonexistent_returns_false() {
        let registry = ShellApprovalRegistry::new();
        assert!(!registry.resolve("bogus", ApprovalDecision::Approved).await);
    }

    #[tokio::test]
    async fn receiver_returns_err_when_dropped_without_resolve() {
        let registry = ShellApprovalRegistry::new();
        let rx = registry.register("test-3", "s").await;
        registry.remove("test-3").await;
        assert!(rx.await.is_err());
    }

    #[tokio::test]
    async fn pending_count_tracks_registrations() {
        let registry = ShellApprovalRegistry::new();
        assert_eq!(registry.pending_count().await, 0);
        let _rx1 = registry.register("a", "s").await;
        let _rx2 = registry.register("b", "s").await;
        assert_eq!(registry.pending_count().await, 2);
        registry.resolve("a", ApprovalDecision::Approved).await;
        assert_eq!(registry.pending_count().await, 1);
    }

    #[tokio::test]
    async fn duplicate_id_replaces_old() {
        let registry = ShellApprovalRegistry::new();
        let rx1 = registry.register("dup", "s").await;
        let rx2 = registry.register("dup", "s").await;
        // Old receiver should get error (sender dropped)
        assert!(rx1.await.is_err());
        // New receiver should work
        registry.resolve("dup", ApprovalDecision::Approved).await;
        assert_eq!(rx2.await.unwrap(), ApprovalDecision::Approved);
    }

    #[tokio::test]
    async fn timeout_simulated() {
        let registry = ShellApprovalRegistry::new();
        let rx = registry.register("timeout-test", "s").await;
        // Simulate timeout by never resolving — use tokio timeout
        let result = tokio::time::timeout(Duration::from_millis(50), rx).await;
        assert!(result.is_err(), "should timeout");
    }

    #[tokio::test]
    async fn cancel_session_drops_pending_senders() {
        let registry = ShellApprovalRegistry::new();
        let rx_a = registry.register("a1", "sess-1").await;
        let rx_b = registry.register("b1", "sess-1").await;
        let _rx_c = registry.register("c1", "sess-2").await;
        assert_eq!(registry.pending_count().await, 3);
        // 取消 sess-1 的全部待审批 → a1/b1 的 sender drop，receiver 返回 Err
        let n = registry.cancel_session("sess-1").await;
        assert_eq!(n, 2);
        assert_eq!(registry.pending_count().await, 1);
        assert!(rx_a.await.is_err());
        assert!(rx_b.await.is_err());
        // sess-2 不受影响
        assert_eq!(registry.pending_count().await, 1);
    }
}
