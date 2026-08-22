//! 定时任务调度引擎：接入 tokio-cron-scheduler（postgres_storage 持久化调度元数据）。
//!
//! ## 架构（设计 §4）
//! - 调度元数据（cron / next_tick）由库的 `job` 表持有，**重启自动恢复**，无需我们轮询。
//! - 业务实体（assistant/instruction/归属）在 `scheduled_tasks` 表，两者经 job UUID ↔
//!   `scheduled_tasks.scheduler_job_id` 关联（进程内 `job_task_map` 加速反查）。
//! - **统一执行闭包**：所有任务共用一段「按 task_id 查库跑 agent」的逻辑。库的 `SimpleJobCode`
//!   只在内存存闭包、重启丢失（issue #84），故用自定义 [`AgentJobCode`]：对每个 job UUID
//!   都返回同一个捕获了 AppState 的闭包，天然解决重启闭包重注册问题。
//! - 时区用 `chrono-tz`（`Job::new_cron_job_async_tz`），避开库默认 UTC 的坑。
//!
//! ## 部署约束
//! postgres `job` 表无分布式锁：**仅支持单实例部署**。两个实例共享同一 job 表时
//! 各自 tick、各自触发同一 job（双执行）。若未来多实例，需引入 job 级抢占锁。

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::RwLock;
use tokio_cron_scheduler::{
    Context, Job, JobCode, JobScheduler, JobToRunAsync, PinnedGetFuture,
    PostgresMetadataStore, PostgresNotificationStore, SimpleNotificationCode, ToCode,
};
use uuid::Uuid;

use crate::domain::scheduled_task::ScheduledTask;
use crate::error::AppError;
use crate::server::AppState;

/// 任务 id（`scheduled_tasks.id`）→ 库 job UUID 的正向映射，用于停用/删除/改 cron 时 remove job。
/// 反向（触发时 job UUID → task_id）也存一份（`uuid→task_id`），供统一闭包定位任务。
#[derive(Default)]
struct JobMaps {
    /// task_id → job_uuid
    task_to_job: HashMap<String, Uuid>,
    /// job_uuid → task_id
    job_to_task: HashMap<Uuid, String>,
}

/// 定时任务调度器（持有 tokio-cron-scheduler 实例 + 双向映射）。
pub struct SchedulerEngine {
    sched: JobScheduler,
    maps: Arc<RwLock<JobMaps>>,
    state: Arc<AppState>,
}

/// 自定义 JobCode：对任何 job UUID 返回同一个「按映射查 task_id 跑 agent」的闭包。
///
/// 关键：库重启恢复调度元数据后会对持久化的 job UUID 调 `get(uuid)` 拿闭包——
/// 我们不依赖启动时的映射（那时可能还没重建），而是在闭包内**运行时**查 `job_to_task`
/// 映射（启动恢复阶段已重建）；查不到则告警跳过（业务任务已删但库 job 残留，由对账清理）。
struct AgentJobCode {
    maps: Arc<RwLock<JobMaps>>,
    state: Arc<AppState>,
}

impl ToCode<Box<JobToRunAsync>> for AgentJobCode {
    fn init(
        &mut self,
        _context: &Context,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<(), tokio_cron_scheduler::JobSchedulerError>> + Send>,
    > {
        Box::pin(async { Ok(()) })
    }

    fn get(&mut self, uuid: Uuid) -> PinnedGetFuture<Box<JobToRunAsync>> {
        let maps = self.maps.clone();
        let state = self.state.clone();
        Box::pin(async move {
            let job: Box<JobToRunAsync> = Box::new(move |job_uuid: Uuid, _sched| {
                let maps = maps.clone();
                let state = state.clone();
                Box::pin(async move {
                    let task_id = {
                        let m = maps.read().await;
                        m.job_to_task.get(&job_uuid).cloned()
                    };
                    // 进程重启后内存映射丢失：先查内存，查不到则用 job UUID 反查业务表
                    // `scheduler_job_id`（重启恢复时旧 job 仍在库里按 cron 触发）。
                    let task_id = match task_id {
                        Some(t) => Some(t),
                        None => match &state.scheduled_task_store {
                            Some(store) => match store.find_by_scheduler_job(&job_uuid.to_string()).await {
                                Ok(t) => t,
                                Err(e) => {
                                    tracing::warn!("[scheduler] 反查 scheduler_job 失败 uuid={job_uuid}: {e}");
                                    None
                                }
                            },
                            None => None,
                        },
                    };
                    match task_id {
                        Some(tid) => {
                            // 顺带补建内存映射，后续触发直接命中。
                            maps.write().await.job_to_task.insert(job_uuid, tid.clone());
                            super::runner_core::run_scheduled_task(state, &tid, "cron").await;
                        }
                        None => {
                            tracing::warn!(
                                "[scheduler] 触发到未知 job（uuid={}），业务任务可能已删除，跳过",
                                job_uuid
                            );
                        }
                    }
                }) as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
            });
            let _ = uuid;
            Ok(Some(Arc::new(RwLock::new(job))))
        })
    }
}

impl JobCode for AgentJobCode {}

impl SchedulerEngine {
    /// 构造调度器（连库 + 自定义 JobCode），并启动 tick 循环。
    ///
    /// `database_url`：tokio-postgres 连接串（`postgres://user:pass@host:port/db`）。
    /// 库会通过环境变量 `POSTGRES_INIT_METADATA=true` 自动建 `job` 表（调用方需提前 set_var）。
    pub async fn start(state: Arc<AppState>, database_url: &str) -> anyhow::Result<Arc<Self>> {
        // 库从环境变量读连接与建表开关（postgres_storage 的既定约定）。
        // tokio-postgres 不识别 sqlx 风格的查询参数（如 statement_timeout），
        // 而 `db.url()` 会附带它们；调度器只需基础连接串，剥离 `?` 之后的部分。
        let base_url = database_url.split('?').next().unwrap_or(database_url);
        unsafe {
            // Rust 2024: set_var 是 unsafe（可能与其他线程并发读冲突）。此处启动早期单线程设置，安全。
            if std::env::var_os("POSTGRES_URL").is_none() {
                std::env::set_var("POSTGRES_URL", base_url);
            }
            if std::env::var_os("POSTGRES_INIT_METADATA").is_none() {
                std::env::set_var("POSTGRES_INIT_METADATA", "true");
            }
            if std::env::var_os("POSTGRES_INIT_NOTIFICATIONS").is_none() {
                std::env::set_var("POSTGRES_INIT_NOTIFICATIONS", "true");
            }
        }

        let maps = Arc::new(RwLock::new(JobMaps::default()));
        let metadata = PostgresMetadataStore::default();
        let notification = PostgresNotificationStore::default();
        let job_code = AgentJobCode {
            maps: maps.clone(),
            state: state.clone(),
        };
        let notification_code = SimpleNotificationCode::default();

        let sched = JobScheduler::new_with_storage_and_code(
            Box::new(metadata),
            Box::new(notification),
            Box::new(job_code),
            Box::new(notification_code),
            200,
        )
        .await
        .map_err(|e| anyhow::anyhow!("创建定时任务调度器失败: {e}"))?;

        let engine = Arc::new(Self {
            sched: sched.clone(),
            maps,
            state,
        });

        // 启动恢复：对账（移除停机前旧 job 行）+ 重建映射 + 补跑遗漏。
        // **必须先 recover 再 start**：tick 循环 ~500ms 首扫，若先 start，扫到停机前
        // 残留的旧 job 行（next_tick 已过期）会立即触发——闭包反查业务表
        // scheduler_job_id（尚指向旧 uuid、enabled=true）命中 → 以 "cron" 跑一次；
        // 随后 recover 又按 next_run_at<now 以 "catchup" 补跑一次 → 同一错过点双跑。
        engine.recover().await;

        sched
            .start()
            .await
            .map_err(|e| anyhow::anyhow!("启动定时任务调度器失败: {e}"))?;

        Ok(engine)
    }

    /// 启动恢复：把库中持久化的 job 与 `scheduled_tasks` 对账。
    ///
    /// - 业务表 enabled 任务：注册进调度器（cron 不变则库已恢复，重建映射即可；
    ///   但保险起见按当前 cron 重新 add——ON CONFLICT 幂等），并补跑 `next_run_at < now` 的遗漏。
    /// - 库里有但业务表无/停用的 job：由上面注册逻辑覆盖（业务表为准），孤儿 job 在首次
    ///   触发时闭包查不到映射即告警跳过（不再额外删，简化）。
    async fn recover(self: &Arc<Self>) {
        let Some(store) = self.state.scheduled_task_store.clone() else {
            return;
        };
        let tasks = match store.list_enabled().await {
            Ok(t) => t,
            Err(e) => {
                tracing::error!("[scheduler] 启动恢复：加载启用任务失败: {e}");
                return;
            }
        };
        let now = chrono::Utc::now();
        let mut recovered = 0usize;
        for task in tasks {
            // 先移除停机前的旧 job 行：库的 add_or_update 按 job UUID 写入（每次注册
            // 是新 UUID），旧行不会被覆盖、也不自动清理；残留行的 next_tick 到期时
            // 触发闭包反查 scheduler_job_id 已指向新 uuid → 查不到空转 warn。此处
            // 按业务表记录的旧 uuid 主动 remove，避免 job 表垃圾行随重启累积。
            if let Some(old) = &task.scheduler_job_id {
                if let Ok(u) = Uuid::parse_str(old) {
                    if let Err(e) = self.sched.remove(&u).await {
                        tracing::warn!(
                            "[scheduler] 恢复时移除旧 job 行失败 task_id={} uuid={}: {e}",
                            task.id,
                            u
                        );
                    }
                }
            }
            // 注册（每次 add 是新 UUID 新行，与旧行互不干扰）。
            if let Err(e) = self.register_job(&task).await {
                tracing::error!("[scheduler] 恢复注册任务失败 task_id={}: {e}", task.id);
                continue;
            }
            recovered += 1;
            // 启动补偿：错过的（next_run_at < now）补跑一次。
            if let Some(next) = task.next_run_at {
                if next < now {
                    tracing::info!(
                        "[scheduler] 启动补偿：补跑错过任务 task_id={} next_run_at={}",
                        task.id,
                        next
                    );
                    let state = self.state.clone();
                    let tid = task.id.clone();
                    tokio::spawn(async move {
                        super::runner_core::run_scheduled_task(state, &tid, "catchup").await;
                    });
                }
            } else {
                // 从未算过 next_run_at（旧数据）→ 视为待跑，补一次并回填。
                tracing::info!("[scheduler] 任务无 next_run_at，补跑并回填 task_id={}", task.id);
                let state = self.state.clone();
                let tid = task.id.clone();
                tokio::spawn(async move {
                    super::runner_core::run_scheduled_task(state, &tid, "catchup").await;
                });
            }
        }
        tracing::info!("[scheduler] 启动恢复完成：重建 {} 个启用任务", recovered);
    }

    /// 注册任务到调度器（建 cron job 并记录映射 + 回填 next_run_at）。
    pub async fn register_job(&self, task: &ScheduledTask) -> Result<(), AppError> {
        let tz: chrono_tz::Tz = task
            .timezone
            .parse()
            .map_err(|_| AppError::BusinessError(format!("非法时区: {}", task.timezone)))?;

        // 注意：maps 必须用 sched.add() 的返回值（库内部 job.guid()，即 JobLocked 构造时
        // 生成的 Uuid::new_v4()）。自造 UUID 会导致触发时映射 miss、remove 删不掉库 job 行、
        // 改 cron 后新旧两个 job 同时调度（重复执行）。
        let cron6 = super::runner_core::normalize_cron(&task.schedule_cron);
        let job = Job::new_cron_job_async_tz(cron6, tz, move |_uuid, _sched| {
            // 统一闭包体在 AgentJobCode::get 提供；此处的闭包仅作占位（库 add 时需要带 run，
            // 但触发时实际执行的是 JobCode::get 返回的闭包）。为空实现即可。
            Box::pin(async {})
        })
        .map_err(|e| AppError::BusinessError(format!("cron 表达式非法: {} ({e})", task.schedule_cron)))?;

        let job_uuid = self
            .sched
            .add(job)
            .await
            .map_err(|e| AppError::BusinessError(format!("注册调度任务失败: {e}")))?;

        {
            let mut m = self.maps.write().await;
            m.task_to_job.insert(task.id.clone(), job_uuid);
            m.job_to_task.insert(job_uuid, task.id.clone());
        }

        // 回填 scheduler_job_id + next_run_at。
        let next = super::runner_core::next_occurrence(&task.schedule_cron, &task.timezone);
        if let Some(store) = self.state.scheduled_task_store.clone() {
            store
                .set_scheduler_job(&task.id, Some(&job_uuid.to_string()), next)
                .await?;
        }
        tracing::info!(
            "[scheduler] 注册任务 task_id={} job_uuid={} cron={} tz={} next={:?}",
            task.id,
            job_uuid,
            task.schedule_cron,
            task.timezone,
            next
        );
        Ok(())
    }

    /// 从调度器移除任务（停用/删除/改 cron 前调用）。
    pub async fn remove_job(&self, task_id: &str) -> Result<(), AppError> {
        let job_uuid = {
            let mut m = self.maps.write().await;
            let u = m.task_to_job.remove(task_id);
            if let Some(u) = u {
                m.job_to_task.remove(&u);
            }
            u
        };
        // 进程内映射没有（如重启后尚未重建、或从未注册）→ 从业务表读 scheduler_job_id 兜底。
        let job_uuid = match job_uuid {
            Some(u) => Some(u),
            None => {
                if let Some(store) = self.state.scheduled_task_store.clone() {
                    store
                        .get(task_id)
                        .await?
                        .and_then(|t| t.scheduler_job_id)
                        .and_then(|s| Uuid::parse_str(&s).ok())
                } else {
                    None
                }
            }
        };
        if let Some(u) = job_uuid {
            if let Err(e) = self.sched.remove(&u).await {
                tracing::warn!("[scheduler] 移除调度任务失败 task_id={} uuid={}: {e}", task_id, u);
            } else {
                tracing::info!("[scheduler] 移除任务 task_id={} uuid={}", task_id, u);
            }
        }
        Ok(())
    }

    /// 启停切换：enabled=true → 注册；false → 移除。
    pub async fn set_enabled(&self, task: &ScheduledTask) -> Result<(), AppError> {
        if task.enabled {
            self.register_job(task).await
        } else {
            self.remove_job(&task.id).await
        }
    }

    /// 计算未来 N 次触发时间（透传 runner_core）。
    pub fn preview(cron: &str, timezone: &str, n: usize) -> Option<Vec<String>> {
        super::runner_core::preview_occurrences(cron, timezone, n)
            .map(|v| v.into_iter().map(|t| t.to_rfc3339()).collect())
    }
}

/// 校验 cron 合法性（创建/更新前置）。复用 runner_core::next_occurrence。
pub fn validate_cron(cron: &str, timezone: &str) -> Result<(), AppError> {
    if super::runner_core::next_occurrence(cron, timezone).is_none() {
        return Err(AppError::BusinessError(format!(
            "cron 表达式非法或时区无效: cron={cron} tz={timezone}"
        )));
    }
    Ok(())
}
