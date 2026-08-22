//! 定时任务限界上下文（Scheduled Task）。
//!
//! 业务实体（[`models::ScheduledTask`]）是本领域的唯一数据源；cron 调度触发由
//! tokio-cron-scheduler 承担（其 `job` 表仅存调度元数据）。两者经 `scheduler_job_id` 关联。
//! 见 `docs/superpowers/specs/2026-08-22-scheduled-tasks-design.md`。

pub mod models;
pub mod store;

pub use models::{RunStatus, ScheduledTask, ScheduledTaskDto, ScheduledTaskInput};
pub use store::ScheduledTaskStore;
