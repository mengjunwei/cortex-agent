//! 定时任务领域模型与 DB 行映射。
//!
//! - [`ScheduledTask`]：业务实体（任务定义唯一数据源）。
//! - [`ScheduledTaskRow`]：diesel `QueryableByName` 行，`sql_query` 反序列化（架构 §8.2 禁 JSONB）。
//!
//! 调度元数据（cron/next_tick）由 tokio-cron-scheduler 的 `job` 表持有，本表只存业务字段
//! + `scheduler_job_id` 关联（见设计 §3.3）。

use diesel::deserialize::QueryableByName;
use diesel::sql_types;
use serde::Serialize;

/// 运行结果状态（`last_run_status` SMALLINT）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunStatus {
    /// 成功
    Success = 0,
    /// 失败（助手不可见 / 模型解析失败 / Runner 错误）
    Failed = 1,
    /// 超时（30min 强杀）
    Timeout = 2,
}

impl RunStatus {
    pub fn as_i16(self) -> i16 {
        self as i16
    }

    pub fn from_i16(v: i16) -> Option<Self> {
        match v {
            0 => Some(Self::Success),
            1 => Some(Self::Failed),
            2 => Some(Self::Timeout),
            _ => None,
        }
    }
}

/// 定时任务业务实体。
#[derive(Debug, Clone)]
pub struct ScheduledTask {
    pub id: String,
    pub user_id: String,
    pub assistant_id: String,
    pub name: String,
    pub instruction: String,
    pub schedule_cron: String,
    pub timezone: String,
    pub enabled: bool,
    pub scheduler_job_id: Option<String>,
    pub next_run_at: Option<chrono::DateTime<chrono::Utc>>,
    pub last_run_at: Option<chrono::DateTime<chrono::Utc>>,
    pub last_run_status: Option<RunStatus>,
    pub last_session_id: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// 对外展示 DTO（含派生的 cron 人话描述由前端/parse 接口提供，此处不冗余）。
#[derive(Debug, Clone, Serialize)]
pub struct ScheduledTaskDto {
    pub id: String,
    pub user_id: String,
    pub assistant_id: String,
    pub assistant_name: String,
    pub name: String,
    pub instruction: String,
    pub schedule_cron: String,
    pub timezone: String,
    pub enabled: bool,
    pub scheduler_job_id: Option<String>,
    pub next_run_at: Option<String>,
    pub last_run_at: Option<String>,
    /// 0成功/1失败/2超时，None=未运行过
    pub last_run_status: Option<i16>,
    pub last_session_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

impl ScheduledTask {
    /// 转 DTO（assistant_name 由调用方批量注入，避免 N+1）。
    pub fn to_dto(&self, assistant_name: &str) -> ScheduledTaskDto {
        ScheduledTaskDto {
            id: self.id.clone(),
            user_id: self.user_id.clone(),
            assistant_id: self.assistant_id.clone(),
            assistant_name: assistant_name.to_string(),
            name: self.name.clone(),
            instruction: self.instruction.clone(),
            schedule_cron: self.schedule_cron.clone(),
            timezone: self.timezone.clone(),
            enabled: self.enabled,
            scheduler_job_id: self.scheduler_job_id.clone(),
            next_run_at: self.next_run_at.map(|t| t.to_rfc3339()),
            last_run_at: self.last_run_at.map(|t| t.to_rfc3339()),
            last_run_status: self.last_run_status.map(|s| s.as_i16()),
            last_session_id: self.last_session_id.clone(),
            created_at: self.created_at.to_rfc3339(),
            updated_at: self.updated_at.to_rfc3339(),
        }
    }
}

/// 创建/更新定时任务的输入。
#[derive(Debug, Clone)]
pub struct ScheduledTaskInput {
    pub assistant_id: String,
    pub name: String,
    pub instruction: String,
    pub schedule_cron: String,
    pub timezone: String,
}

/// diesel 行映射。
#[derive(Debug, QueryableByName)]
pub struct ScheduledTaskRow {
    #[diesel(sql_type = sql_types::Text)]
    pub id: String,
    #[diesel(sql_type = sql_types::Text)]
    pub user_id: String,
    #[diesel(sql_type = sql_types::Text)]
    pub assistant_id: String,
    #[diesel(sql_type = sql_types::Text)]
    pub name: String,
    #[diesel(sql_type = sql_types::Text)]
    pub instruction: String,
    #[diesel(sql_type = sql_types::Varchar)]
    pub schedule_cron: String,
    #[diesel(sql_type = sql_types::Varchar)]
    pub timezone: String,
    #[diesel(sql_type = sql_types::Bool)]
    pub enabled: bool,
    #[diesel(sql_type = sql_types::Nullable<sql_types::Varchar>)]
    pub scheduler_job_id: Option<String>,
    #[diesel(sql_type = sql_types::Nullable<sql_types::Timestamptz>)]
    pub next_run_at: Option<chrono::DateTime<chrono::Utc>>,
    #[diesel(sql_type = sql_types::Nullable<sql_types::Timestamptz>)]
    pub last_run_at: Option<chrono::DateTime<chrono::Utc>>,
    #[diesel(sql_type = sql_types::Nullable<sql_types::Int2>)]
    pub last_run_status: Option<i16>,
    #[diesel(sql_type = sql_types::Nullable<sql_types::Varchar>)]
    pub last_session_id: Option<String>,
    #[diesel(sql_type = sql_types::Timestamptz)]
    pub created_at: chrono::DateTime<chrono::Utc>,
    #[diesel(sql_type = sql_types::Timestamptz)]
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

impl From<ScheduledTaskRow> for ScheduledTask {
    fn from(r: ScheduledTaskRow) -> Self {
        Self {
            id: r.id,
            user_id: r.user_id,
            assistant_id: r.assistant_id,
            name: r.name,
            instruction: r.instruction,
            schedule_cron: r.schedule_cron,
            timezone: r.timezone,
            enabled: r.enabled,
            scheduler_job_id: r.scheduler_job_id,
            next_run_at: r.next_run_at,
            last_run_at: r.last_run_at,
            last_run_status: r.last_run_status.and_then(RunStatus::from_i16),
            last_session_id: r.last_session_id,
            created_at: r.created_at,
            updated_at: r.updated_at,
        }
    }
}

/// SELECT 列清单（`sql_query` 用，与 ScheduledTaskRow 字段序一致）。
pub const TASK_COLUMNS: &str = "id, user_id, assistant_id, name, instruction, schedule_cron, \
     timezone, enabled, scheduler_job_id, next_run_at, last_run_at, last_run_status, \
     last_session_id, created_at, updated_at";
