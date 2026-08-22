//! 会话运行注册表 — per-session 活跃 run 登记 + steer（运行中追加输入）队列。
//!
//! 对齐 codex 的 `InputQueue` / `TurnInputMode::StartOrSteer` 语义（core/src/session/input_queue.rs）：
//! - 会话同一时刻至多一个活跃 run（`ActiveRun`），重复启动被拒（「忙」）；
//! - 运行中提交的用户消息进 FIFO steer 队列，由 agent 主循环在**下一次模型请求前**
//!   drain 注入（对齐 codex「pending input drained into history before building the next
//!   model request」）；
//! - 模型回合结束（无工具调用）时若队列非空 → 续跑（对齐 codex
//!   `needs_follow_up = model_needs_follow_up || has_pending_input`）；
//! - 终局判定（`finish`）与 enqueue 在**同一把会话锁**下完成，封住「提交恰逢 run
//!   收尾」的竞态（对齐 codex `RegularTask` 在任务返回后再查一次 pending input）；
//! - 取消（cancel）清空未消费队列（对齐 codex interrupt 的 `clear_pending`）。
//!
//! 生命周期边界（对齐 codex「rollout 按事件先落库、turn 后才收尾」）：
//! - `finish` 判定队列空时**不注销**，只标记 `draining`（agent 循环已结束，流侧还在
//!   持久化 assistant 正文）；draining 期间拒绝新 steer（返回 false，前端回退正常
//!   发送）。注销统一发生在 stream 侧**持久化完成后**——保证后继 run 读历史时上一条
//!   助手回复已落库，否则忙拒绝放行的新 run 会读到缺尾的历史；
//! - [`ActiveRunGuard`]（Drop 守卫）随 run 的 spawn 任务存活：客户端断开由流侧
//!   send 失败路径注销，但任务 **panic** 时所有注销点都会被跳过——守卫在 panic
//!   unwind 析构时兜底「cancel + 清队列 + 注销」，防幽灵 active run 把会话卡死。
//!
//! 本模块只做无业务的运行时协调原语：server 层登记/注销/入队，agent 层持
//! [`SteerPort`] drain。进程内存态（重启清空），条目极小不主动回收。

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use adk_rust::Content;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

/// 一条 steer 输入：提交时已解析完毕（@mention XML + 附件降采样）的 user Content。
#[derive(Clone, Debug)]
pub struct SteerItem {
    /// 提交方生成的客户端 run_id（诊断/日志定位用，非活跃 run id）
    pub run_id: String,
    /// 完整 user 内容（文本 parts + 附件 parts），注入 conv 时原样 push
    pub content: Content,
}

/// 单会话运行状态：活跃 run（唯一）+ steer 队列（FIFO）。
#[derive(Default)]
pub struct SessionRunState {
    pub active: Option<ActiveRun>,
    steer_queue: VecDeque<SteerItem>,
}

/// 活跃 run 登记：run_id + 取消令牌 + 是否已进入收尾。
pub struct ActiveRun {
    pub run_id: String,
    pub cancel_token: CancellationToken,
    /// agent 循环已结束、流侧仍在持久化（finish 判定队列空时置位）。置位期间拒绝
    /// 新 steer（对齐 codex turn 收尾后 input 转为新 turn 提交），注销由流侧完成。
    pub draining: bool,
}

/// 模型回合结束时的终局判定结果（[`SteerPort::finish`]）。
pub enum SteerFinish {
    /// 队列非空且未取消：取走全部排队项，主循环注入后续跑（active 保留，run 继续）。
    Continue(Vec<SteerItem>),
    /// 队列空或已取消：active 标记 `draining`（注销统一延迟到流侧持久化完成后），
    /// 主循环正常收尾。
    Stop,
}

impl SessionRunState {
    /// 登记活跃 run。已有活跃 run → `Err(现有 run_id)`（调用方报「忙」，修掉并发
    /// run + 取消令牌被覆盖两个历史 bug——此前第二个 POST 直接覆盖 token，旧 run
    /// 从此无法取消）。
    pub fn register_active(
        &mut self,
        run_id: &str,
        token: CancellationToken,
    ) -> Result<(), String> {
        if let Some(existing) = &self.active {
            return Err(existing.run_id.clone());
        }
        self.active = Some(ActiveRun {
            run_id: run_id.to_string(),
            cancel_token: token,
            draining: false,
        });
        Ok(())
    }

    /// 注销活跃 run（仅 run_id 匹配才注，防误删后继 run）。返回被注销的取消令牌。
    pub fn deregister_active(&mut self, run_id: &str) -> Option<CancellationToken> {
        if self.active.as_ref().is_some_and(|a| a.run_id == run_id) {
            self.active.take().map(|a| a.cancel_token)
        } else {
            None
        }
    }

    /// 运行中提交 → 入队（对齐 codex `steer_input`：append 进 pending_input）。
    /// 无活跃 run 或已 draining（agent 循环已收尾）→ `false`（调用方回退正常启动
    /// 路径，对齐 `NotSubmitted::NoActiveTurn`——draining 期提交等价于「turn 已结束，
    /// input 应作为新 turn 提交」）。
    fn enqueue_steer(&mut self, item: SteerItem) -> bool {
        if self.active.as_ref().is_none_or(|a| a.draining) {
            return false;
        }
        self.steer_queue.push_back(item);
        true
    }

    /// drain 全部排队输入（主循环每轮模型请求前调用；与 mailbox drain 同构）。
    /// 仅当 active 归属本 run 时才取——取消/换代后僵尸 agent 的循环顶 drain 不得
    /// 偷走后继 run 已入队的输入。
    fn drain_steer(&mut self, run_id: &str) -> Vec<SteerItem> {
        if self.active.as_ref().is_some_and(|a| a.run_id == run_id) {
            self.steer_queue.drain(..).collect()
        } else {
            Vec::new()
        }
    }

    /// 模型回合结束时的终局判定（与 enqueue 同锁，封收尾竞态）：
    /// - active 已换代（本 run 早被注销/取消）→ 不碰新 run 的任何状态 → `Stop`；
    /// - 已取消 → 清队列 + 注销 → `Stop`；
    /// - 队列非空 → 取走全部（active 保留）→ `Continue`；
    /// - 队列空 → 标记 `draining`（不注销，注销统一延迟到流侧持久化完成后）→ `Stop`。
    fn finish(&mut self, run_id: &str, cancelled: bool) -> SteerFinish {
        if !self.active.as_ref().is_some_and(|a| a.run_id == run_id) {
            // active 已换代或为空（本 run 早被取消/注销）：僵尸 port 的终局判定
            // 不得触碰新 run 的队列与登记
            return SteerFinish::Stop;
        }
        if cancelled {
            self.steer_queue.clear();
            self.deregister_active(run_id);
            return SteerFinish::Stop;
        }
        let items: Vec<SteerItem> = self.steer_queue.drain(..).collect();
        if items.is_empty() {
            if let Some(active) = &mut self.active {
                active.draining = true;
            }
            SteerFinish::Stop
        } else {
            SteerFinish::Continue(items)
        }
    }

    /// 中止指定 run（run_id 匹配才动手）：cancel 令牌 + 清空队列 + 注销。
    /// [`ActiveRunGuard`] 的 Drop 兜底用——客户端断开等导致流侧注销点未执行时，
    /// 防止幽灵 active run 把会话永久卡在「忙」。
    pub fn abort_active(&mut self, run_id: &str) -> bool {
        if !self.active.as_ref().is_some_and(|a| a.run_id == run_id) {
            return false;
        }
        self.cancel_active();
        true
    }

    /// 取消活跃 run：cancel 令牌 + 清空未消费 steer 队列（对齐 codex interrupt 的
    /// `clear_pending`——被打断的 turn 不复活排队消息）。返回 (run_id, 令牌, 清掉的条数)。
    pub fn cancel_active(&mut self) -> Option<(String, CancellationToken, usize)> {
        let active = self.active.take()?;
        let cleared = self.steer_queue.len();
        self.steer_queue.clear();
        active.cancel_token.cancel();
        Some((active.run_id, active.cancel_token, cleared))
    }
}

/// 进程级注册表：thread_id → 会话运行状态句柄。
///
/// 外层 map 锁只管 get-or-create；所有状态变迁（登记/入队/drain/finish）都在
/// per-session 的 [`SessionRunState`] 锁下完成，保证 steer 提交与 run 收尾的原子性。
#[derive(Default)]
pub struct RunRegistry {
    sessions: Mutex<HashMap<String, Arc<Mutex<SessionRunState>>>>,
}

impl RunRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 取（或建）某会话的运行状态句柄。
    pub async fn session(&self, thread_id: &str) -> Arc<Mutex<SessionRunState>> {
        let mut map = self.sessions.lock().await;
        Arc::clone(map.entry(thread_id.to_string()).or_default())
    }
}

/// agent 主循环消费 steer 队列的句柄（注入 CortexAgent，仅 root run 持有）。
///
/// server 层在启动 run 时创建并交给 agent；run 期间 agent 通过它 drain/终局判定。
pub struct SteerPort {
    state: Arc<Mutex<SessionRunState>>,
    run_id: String,
}

impl SteerPort {
    pub fn new(state: Arc<Mutex<SessionRunState>>, run_id: &str) -> Self {
        Self {
            state,
            run_id: run_id.to_string(),
        }
    }

    /// drain 全部排队输入（主循环每轮模型请求前，与 mailbox drain 并列调用）。
    /// active 已换代/注销（本 run 被取消）时返回空——不碰后继 run 的队列。
    pub async fn drain(&self) -> Vec<SteerItem> {
        self.state.lock().await.drain_steer(&self.run_id)
    }

    /// 模型回合结束（无工具调用）时的终局判定：队列非空且未取消 → `Continue(排队项)`
    /// 由调用方注入后续跑；否则注销 active 返回 `Stop`。详见 [`SessionRunState::finish`]。
    pub async fn finish(&self, cancelled: bool) -> SteerFinish {
        self.state.lock().await.finish(&self.run_id, cancelled)
    }
}

/// server 层便捷方法：登记活跃 run（忙碌 → `Err(现有 run_id)`）。
pub async fn register_active(
    registry: &RunRegistry,
    thread_id: &str,
    run_id: &str,
    token: CancellationToken,
) -> Result<(), String> {
    registry
        .session(thread_id)
        .await
        .lock()
        .await
        .register_active(run_id, token)
}

/// server 层便捷方法：注销活跃 run（run_id 匹配才注，幂等安全网——正常路径已由
/// agent 侧 `finish` 注销，这里兜早退路径：Runner 构建失败 / run 启动失败等）。
pub async fn deregister_active(registry: &RunRegistry, thread_id: &str, run_id: &str) {
    registry
        .session(thread_id)
        .await
        .lock()
        .await
        .deregister_active(run_id);
}

/// server 层便捷方法（steer 端点用）：有活跃 run → 入队返回 true；否则 false
/// （调用方回退正常启动）。入队与活跃判定同锁原子。
pub async fn enqueue_steer(
    registry: &RunRegistry,
    thread_id: &str,
    item: SteerItem,
) -> bool {
    registry
        .session(thread_id)
        .await
        .lock()
        .await
        .enqueue_steer(item)
}

/// server 层便捷方法（cancel 端点用）：取消活跃 run + 清空 steer 队列。
pub async fn cancel_active(
    registry: &RunRegistry,
    thread_id: &str,
) -> Option<(String, CancellationToken, usize)> {
    registry.session(thread_id).await.lock().await.cancel_active()
}

/// 活跃 run 的 Drop 守卫：随 run 的 spawn 任务存活（`create_event_stream` 任务体首行
/// 持有），panic unwind / 任何跳过注销点的异常退出时兜底「cancel + 清队列 + 注销」。
///
/// 客户端断开不经过守卫（run 任务独立于 SSE 连接继续跑完，注销由流侧 send 失败
/// 或正常收尾路径执行——对齐 codex：客户端视图消失不中断服务端 turn）。
/// 注销是 run_id 匹配的幂等操作：正常路径流侧已注销 → Drop 兜底 no-op。
pub struct ActiveRunGuard {
    registry: std::sync::Arc<RunRegistry>,
    thread_id: String,
    run_id: String,
}

impl ActiveRunGuard {
    pub fn new(registry: std::sync::Arc<RunRegistry>, thread_id: &str, run_id: &str) -> Self {
        Self {
            registry,
            thread_id: thread_id.to_string(),
            run_id: run_id.to_string(),
        }
    }
}

impl Drop for ActiveRunGuard {
    fn drop(&mut self) {
        // Drop 里不能 await：转交 tokio 任务执行；run_id 匹配保证对已收尾/已换代的
        // run 是 no-op。无运行时（进程退出）时 spawn 失败可接受——注册表随进程消失。
        let registry = self.registry.clone();
        let (thread_id, run_id) = (self.thread_id.clone(), self.run_id.clone());
        // 裸语句立即丢弃 JoinHandle = 分离任务（这正是想要的：守卫析构不等任务完成）
        tokio::spawn(async move {
            let session = registry.session(&thread_id).await;
            session.lock().await.abort_active(&run_id);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(text: &str) -> SteerItem {
        SteerItem {
            run_id: format!("steer-{text}"),
            content: Content::new("user").with_text(text),
        }
    }

    #[tokio::test]
    async fn enqueue_requires_active_run() {
        let reg = RunRegistry::new();
        // 空闲 → 拒绝入队（调用方回退正常启动）
        assert!(!enqueue_steer(&reg, "s1", item("hi")).await);
        // 登记 run 后 → 入队成功
        register_active(&reg, "s1", "run-1", CancellationToken::new())
            .await
            .unwrap();
        assert!(enqueue_steer(&reg, "s1", item("hi")).await);
    }

    #[tokio::test]
    async fn register_rejects_second_run_and_restores_after_deregister() {
        let reg = RunRegistry::new();
        register_active(&reg, "s1", "run-1", CancellationToken::new())
            .await
            .unwrap();
        // 忙：第二个 run 被拒（修掉历史上 token 被覆盖、旧 run 失去取消入口的问题）
        let err = register_active(&reg, "s1", "run-2", CancellationToken::new()).await;
        assert_eq!(err.unwrap_err(), "run-1");
        // 注销 run-1（run_id 不匹配的注销是 no-op）后可重新登记
        deregister_active(&reg, "s1", "run-9").await;
        let err = register_active(&reg, "s1", "run-2", CancellationToken::new()).await;
        assert_eq!(err.unwrap_err(), "run-1");
        deregister_active(&reg, "s1", "run-1").await;
        register_active(&reg, "s1", "run-2", CancellationToken::new())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn finish_marks_draining_and_rejects_steer_until_stream_deregisters() {
        let reg = RunRegistry::new();
        let session = reg.session("s1").await;
        let port = SteerPort::new(session.clone(), "run-1");
        register_active(&reg, "s1", "run-1", CancellationToken::new())
            .await
            .unwrap();

        // 队列空 → Stop，但只标记 draining（流侧还在持久化），不注销
        assert!(matches!(port.finish(false).await, SteerFinish::Stop));
        {
            let st = reg.session("s1").await;
            let guard = st.lock().await;
            assert!(guard.active.as_ref().is_some_and(|a| a.draining));
        }
        // draining 期提交被拒（对齐 codex：turn 已收尾，input 应作为新 turn 提交）
        assert!(!enqueue_steer(&reg, "s1", item("late")).await);
        // draining 期新 run 仍被忙拒绝（注销由流侧持久化完成后统一执行）
        assert_eq!(
            register_active(&reg, "s1", "run-2", CancellationToken::new())
                .await
                .unwrap_err(),
            "run-1"
        );
        // 流侧注销 → 会话空闲，可重新登记
        deregister_active(&reg, "s1", "run-1").await;
        register_active(&reg, "s1", "run-2", CancellationToken::new())
            .await
            .unwrap();

        // 入队 → Continue 带走全部排队项（FIFO 保序）
        enqueue_steer(&reg, "s1", item("a")).await;
        enqueue_steer(&reg, "s1", item("b")).await;
        let port_r2 = SteerPort::new(reg.session("s1").await, "run-2");
        match port_r2.finish(false).await {
            SteerFinish::Continue(items) => {
                let texts: Vec<String> = items
                    .iter()
                    .filter_map(|i| {
                        i.content.parts.iter().find_map(|p| match p {
                            adk_rust::Part::Text { text } => Some(text.clone()),
                            _ => None,
                        })
                    })
                    .collect();
                assert_eq!(texts, vec!["a".to_string(), "b".to_string()]);
            }
            _ => panic!("expected Continue"),
        }
        // Continue 分支保留 active 且不 draining（run 续跑中）
        let st = reg.session("s1").await;
        let guard = st.lock().await;
        assert!(guard.active.as_ref().is_some_and(|a| !a.draining));
    }

    #[tokio::test]
    async fn zombie_port_drain_never_steals_successor_queue() {
        // run-1 取消后循环熔断前的最后一次 drain，不得偷走 run-2 已入队的输入
        let reg = RunRegistry::new();
        register_active(&reg, "s1", "run-1", CancellationToken::new())
            .await
            .unwrap();
        let zombie = SteerPort::new(reg.session("s1").await, "run-1");
        cancel_active(&reg, "s1").await; // run-1 注销
        register_active(&reg, "s1", "run-2", CancellationToken::new())
            .await
            .unwrap();
        assert!(enqueue_steer(&reg, "s1", item("run2-msg")).await);

        // 僵尸 drain 返回空，run-2 的队列完好
        assert!(zombie.drain().await.is_empty());
        let port2 = SteerPort::new(reg.session("s1").await, "run-2");
        assert_eq!(port2.drain().await.len(), 1);
    }

    #[tokio::test]
    async fn drain_returns_and_clears_queue() {
        let reg = RunRegistry::new();
        let session = reg.session("s1").await;
        let port = SteerPort::new(session.clone(), "run-1");
        register_active(&reg, "s1", "run-1", CancellationToken::new())
            .await
            .unwrap();
        enqueue_steer(&reg, "s1", item("x")).await;
        assert_eq!(port.drain().await.len(), 1);
        assert!(port.drain().await.is_empty());
    }

    #[tokio::test]
    async fn cancel_clears_queue_and_token() {
        let reg = RunRegistry::new();
        register_active(&reg, "s1", "run-1", CancellationToken::new())
            .await
            .unwrap();
        enqueue_steer(&reg, "s1", item("doomed")).await;
        let (run_id, token, cleared) = cancel_active(&reg, "s1").await.unwrap();
        assert_eq!(run_id, "run-1");
        assert!(token.is_cancelled());
        assert_eq!(cleared, 1);
        // 取消后队列已清、active 已空 → steer 端点回退正常启动路径
        assert!(!enqueue_steer(&reg, "s1", item("after")).await);
    }

    #[tokio::test]
    async fn finish_when_cancelled_clears_and_deregisters() {
        let reg = RunRegistry::new();
        let token = CancellationToken::new();
        register_active(&reg, "s1", "run-1", token.clone())
            .await
            .unwrap();
        enqueue_steer(&reg, "s1", item("stale")).await;
        let session = reg.session("s1").await;
        let port = SteerPort::new(session.clone(), "run-1");
        token.cancel();
        assert!(matches!(port.finish(true).await, SteerFinish::Stop));
        assert!(reg.session("s1").await.lock().await.active.is_none());
    }

    #[tokio::test]
    async fn zombie_port_finish_never_touches_successor_run() {
        // run-1 被取消、run-2 接管后，run-1 的僵尸 agent 才走到终局判定——
        // 不得清掉 run-2 已入队的 steer、不得注销 run-2
        let reg = RunRegistry::new();
        let token = CancellationToken::new();
        register_active(&reg, "s1", "run-1", token.clone())
            .await
            .unwrap();
        let zombie = SteerPort::new(reg.session("s1").await, "run-1");
        token.cancel();
        cancel_active(&reg, "s1").await; // run-1 注销
        register_active(&reg, "s1", "run-2", CancellationToken::new())
            .await
            .unwrap();
        assert!(enqueue_steer(&reg, "s1", item("run2-msg")).await);

        // 僵尸 port 的终局判定（取消/未取消两态都要验）：全是 Stop 且不碰 run-2
        assert!(matches!(zombie.finish(true).await, SteerFinish::Stop));
        assert!(matches!(zombie.finish(false).await, SteerFinish::Stop));
        let session = reg.session("s1").await;
        {
            let st = session.lock().await;
            assert!(st.active.as_ref().is_some_and(|a| a.run_id == "run-2"));
        }
        // run-2 的 steer 完好，可正常 drain
        let port2 = SteerPort::new(session.clone(), "run-2");
        assert_eq!(port2.drain().await.len(), 1);
    }

    #[tokio::test]
    async fn guard_drop_aborts_orphaned_run_and_is_noop_after_clean_deregister() {
        // 场景一：run 任务 panic 等异常退出（守卫析构 = 注册表仍挂着本 run）——
        // 兜底中止，会话不被幽灵 active run 卡死
        let reg = Arc::new(RunRegistry::new());
        let token = CancellationToken::new();
        register_active(&reg, "s1", "run-1", token.clone())
            .await
            .unwrap();
        enqueue_steer(&reg, "s1", item("queued")).await;
        drop(ActiveRunGuard::new(reg.clone(), "s1", "run-1"));
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        {
            let st = reg.session("s1").await;
            let guard = st.lock().await;
            assert!(guard.active.is_none());
        }
        assert!(token.is_cancelled());
        // 中止已清队列 → 后继 steer 走回退路径，新 run 可正常登记
        assert!(!enqueue_steer(&reg, "s1", item("after")).await);
        register_active(&reg, "s1", "run-2", CancellationToken::new())
            .await
            .unwrap();

        // 场景二：正常收尾（流侧已注销）后守卫 drop → run_id 不匹配，no-op，
        // 不误伤后继 run
        deregister_active(&reg, "s1", "run-2").await;
        register_active(&reg, "s1", "run-3", CancellationToken::new())
            .await
            .unwrap();
        drop(ActiveRunGuard::new(reg.clone(), "s1", "run-2"));
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let st = reg.session("s1").await;
        let guard = st.lock().await;
        assert!(guard.active.as_ref().is_some_and(|a| a.run_id == "run-3"));
    }
}
