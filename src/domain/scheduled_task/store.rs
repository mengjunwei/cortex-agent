//! 定时任务数据存储层（diesel-async）。
//!
//! 范式同 [`crate::domain::assistant::store`]：私有 `get_conn`、SMALLINT 枚举、
//! `sql_query` + `QueryableByName`（架构 §8.2）。建表 DDL 见 `migrations/schema.sql`。

use std::sync::Arc;

use diesel::sql_types;
use diesel_async::RunQueryDsl;

use crate::domain::scheduled_task::models::{
    RunStatus, ScheduledTask, ScheduledTaskInput, ScheduledTaskRow, TASK_COLUMNS,
};
use crate::error::AppError;
use crate::infra::db::DbPool;
use crate::infra::store_base::{Store, new_id};

/// 计数行（分页 total）。
#[derive(diesel::QueryableByName)]
struct CountRow {
    #[diesel(sql_type = sql_types::BigInt)]
    cnt: i64,
}

/// 定时任务存储。
pub struct ScheduledTaskStore {
    pool: DbPool,
}

#[async_trait::async_trait]
impl Store for ScheduledTaskStore {
    fn pool(&self) -> &DbPool {
        &self.pool
    }
}

impl ScheduledTaskStore {
    pub fn new(pool: DbPool) -> Arc<Self> {
        Arc::new(Self { pool })
    }

    fn row_to_task(r: ScheduledTaskRow) -> ScheduledTask {
        r.into()
    }

    /// 创建任务（不含调度器注册——注册由 server 层做并回填 `scheduler_job_id`）。
    pub async fn insert(
        &self,
        user_id: &str,
        input: &ScheduledTaskInput,
    ) -> Result<ScheduledTask, AppError> {
        let mut c = self.get_conn().await?;
        let id = new_id();
        let row: ScheduledTaskRow = diesel::sql_query(format!(
            "INSERT INTO scheduled_tasks \
             (id, user_id, assistant_id, name, instruction, schedule_cron, timezone, enabled) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,true) RETURNING {TASK_COLUMNS}"
        ))
        .bind::<sql_types::Text, _>(&id)
        .bind::<sql_types::Text, _>(user_id)
        .bind::<sql_types::Text, _>(&input.assistant_id)
        .bind::<sql_types::Text, _>(&input.name)
        .bind::<sql_types::Text, _>(&input.instruction)
        .bind::<sql_types::Varchar, _>(&input.schedule_cron)
        .bind::<sql_types::Varchar, _>(&input.timezone)
        .get_result(&mut c)
        .await?;
        Ok(Self::row_to_task(row))
    }

    pub async fn get(&self, id: &str) -> Result<Option<ScheduledTask>, AppError> {
        let mut c = self.get_conn().await?;
        let rows: Vec<ScheduledTaskRow> = diesel::sql_query(format!(
            "SELECT {TASK_COLUMNS} FROM scheduled_tasks WHERE id = $1"
        ))
        .bind::<sql_types::Text, _>(id)
        .get_results(&mut c)
        .await?;
        Ok(rows.into_iter().next().map(Self::row_to_task))
    }

    /// 按调度库 job UUID 反查业务任务 id（重启恢复后内存映射丢失时的兜底）。
    pub async fn find_by_scheduler_job(&self, job_uuid: &str) -> Result<Option<String>, AppError> {
        let mut c = self.get_conn().await?;
        #[derive(diesel::QueryableByName)]
        struct IdRow {
            #[diesel(sql_type = sql_types::Text)]
            id: String,
        }
        let rows: Vec<IdRow> = diesel::sql_query(
            "SELECT id FROM scheduled_tasks WHERE scheduler_job_id = $1 AND enabled = true",
        )
        .bind::<sql_types::Text, _>(job_uuid)
        .get_results(&mut c)
        .await?;
        Ok(rows.into_iter().next().map(|r| r.id))
    }

    /// 列表：admin_view=true 看全部，否则按归属人过滤。按创建时间倒序。
    pub async fn list_for_owner(
        &self,
        user_id: &str,
        admin_view: bool,
    ) -> Result<Vec<ScheduledTask>, AppError> {
        let mut c = self.get_conn().await?;
        let rows: Vec<ScheduledTaskRow> = diesel::sql_query(format!(
            "SELECT {TASK_COLUMNS} FROM scheduled_tasks \
             WHERE ($1 OR user_id = $2) ORDER BY created_at DESC"
        ))
        .bind::<sql_types::Bool, _>(admin_view)
        .bind::<sql_types::Text, _>(user_id)
        .get_results(&mut c)
        .await?;
        Ok(rows.into_iter().map(Self::row_to_task).collect())
    }

    /// 分页列表（归属过滤同上）。返回 (当前页数据, 总条数)。
    pub async fn list_for_owner_paged(
        &self,
        user_id: &str,
        admin_view: bool,
        page: i64,
        page_size: i64,
    ) -> Result<(Vec<ScheduledTask>, i64), AppError> {
        let mut c = self.get_conn().await?;
        let cnt: CountRow = diesel::sql_query(
            "SELECT COUNT(*) AS cnt FROM scheduled_tasks WHERE ($1 OR user_id = $2)",
        )
        .bind::<sql_types::Bool, _>(admin_view)
        .bind::<sql_types::Text, _>(user_id)
        .get_result(&mut c)
        .await?;
        let total = cnt.cnt;

        let offset = (page.max(1) - 1) * page_size;
        let rows: Vec<ScheduledTaskRow> = diesel::sql_query(format!(
            "SELECT {TASK_COLUMNS} FROM scheduled_tasks \
             WHERE ($1 OR user_id = $2) ORDER BY created_at DESC LIMIT $3 OFFSET $4"
        ))
        .bind::<sql_types::Bool, _>(admin_view)
        .bind::<sql_types::Text, _>(user_id)
        .bind::<sql_types::BigInt, _>(page_size)
        .bind::<sql_types::BigInt, _>(offset)
        .get_results(&mut c)
        .await?;
        Ok((rows.into_iter().map(Self::row_to_task).collect(), total))
    }

    /// 所有启用任务（启动恢复/对账用）。
    pub async fn list_enabled(&self) -> Result<Vec<ScheduledTask>, AppError> {
        let mut c = self.get_conn().await?;
        let rows: Vec<ScheduledTaskRow> = diesel::sql_query(format!(
            "SELECT {TASK_COLUMNS} FROM scheduled_tasks WHERE enabled = true ORDER BY created_at ASC"
        ))
        .get_results(&mut c)
        .await?;
        Ok(rows.into_iter().map(Self::row_to_task).collect())
    }

    /// 更新业务字段（cron/时区/名称/指令/助手）。不含 enabled（启停走 set_enabled）。
    pub async fn update_fields(
        &self,
        id: &str,
        input: &ScheduledTaskInput,
    ) -> Result<bool, AppError> {
        let mut c = self.get_conn().await?;
        let n = diesel::sql_query(
            "UPDATE scheduled_tasks SET assistant_id=$2, name=$3, instruction=$4, \
             schedule_cron=$5, timezone=$6, updated_at=now() WHERE id=$1",
        )
        .bind::<sql_types::Text, _>(id)
        .bind::<sql_types::Text, _>(&input.assistant_id)
        .bind::<sql_types::Text, _>(&input.name)
        .bind::<sql_types::Text, _>(&input.instruction)
        .bind::<sql_types::Varchar, _>(&input.schedule_cron)
        .bind::<sql_types::Varchar, _>(&input.timezone)
        .execute(&mut c)
        .await?;
        Ok(n > 0)
    }

    /// 启停开关。
    pub async fn set_enabled(&self, id: &str, enabled: bool) -> Result<bool, AppError> {
        let mut c = self.get_conn().await?;
        let n = diesel::sql_query(
            "UPDATE scheduled_tasks SET enabled=$2, updated_at=now() WHERE id=$1",
        )
        .bind::<sql_types::Text, _>(id)
        .bind::<sql_types::Bool, _>(enabled)
        .execute(&mut c)
        .await?;
        Ok(n > 0)
    }

    /// 回填调度器 job id 与下次触发时间（注册后调用）。
    pub async fn set_scheduler_job(
        &self,
        id: &str,
        scheduler_job_id: Option<&str>,
        next_run_at: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<(), AppError> {
        let mut c = self.get_conn().await?;
        diesel::sql_query(
            "UPDATE scheduled_tasks SET scheduler_job_id=$2, next_run_at=$3, updated_at=now() \
             WHERE id=$1",
        )
        .bind::<sql_types::Text, _>(id)
        .bind::<sql_types::Nullable<sql_types::Varchar>, _>(scheduler_job_id)
        .bind::<sql_types::Nullable<sql_types::Timestamptz>, _>(next_run_at)
        .execute(&mut c)
        .await?;
        Ok(())
    }

    /// 仅刷新下次触发时间（每次触发后由调度器重算回填）。
    pub async fn set_next_run(
        &self,
        id: &str,
        next_run_at: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<(), AppError> {
        let mut c = self.get_conn().await?;
        diesel::sql_query("UPDATE scheduled_tasks SET next_run_at=$2 WHERE id=$1")
            .bind::<sql_types::Text, _>(id)
            .bind::<sql_types::Nullable<sql_types::Timestamptz>, _>(next_run_at)
            .execute(&mut c)
            .await?;
        Ok(())
    }

    /// 记录一次运行结果。
    pub async fn record_run(
        &self,
        id: &str,
        status: RunStatus,
        session_id: Option<&str>,
    ) -> Result<(), AppError> {
        let mut c = self.get_conn().await?;
        diesel::sql_query(
            "UPDATE scheduled_tasks SET last_run_at=now(), last_run_status=$2, last_session_id=$3, \
             updated_at=now() WHERE id=$1",
        )
        .bind::<sql_types::Text, _>(id)
        .bind::<sql_types::Int2, _>(status.as_i16())
        .bind::<sql_types::Nullable<sql_types::Varchar>, _>(session_id)
        .execute(&mut c)
        .await?;
        Ok(())
    }

    pub async fn delete(&self, id: &str) -> Result<bool, AppError> {
        let mut c = self.get_conn().await?;
        let n = diesel::sql_query("DELETE FROM scheduled_tasks WHERE id=$1")
            .bind::<sql_types::Text, _>(id)
            .execute(&mut c)
            .await?;
        Ok(n > 0)
    }

    /// 归属校验辅助：返回任务的 user_id（供 server 层判 can_access）。
    pub async fn owner_of(&self, id: &str) -> Result<Option<String>, AppError> {
        let mut c = self.get_conn().await?;
        let rows: Vec<OwnerRow> = diesel::sql_query(
            "SELECT user_id FROM scheduled_tasks WHERE id=$1",
        )
        .bind::<sql_types::Text, _>(id)
        .get_results(&mut c)
        .await?;
        Ok(rows.into_iter().next().map(|r| r.user_id))
    }
}

#[derive(Debug, diesel::deserialize::QueryableByName)]
struct OwnerRow {
    #[diesel(sql_type = sql_types::Text)]
    user_id: String,
}
