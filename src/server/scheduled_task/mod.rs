//! 定时任务服务端模块：REST API + 调度引擎 + 核心执行链路。
//!
//! - [`handler`]：`/api/scheduled-tasks/*` REST（CRUD / parse-schedule / runs / run-now）
//! - [`scheduler`]：tokio-cron-scheduler 引擎接入（postgres_storage 持久化 + 重启恢复）
//! - [`runner_core`]：无 SSE 的后台 agent 执行（建会话→跑→落库→清理）

pub mod handler;
pub mod runner_core;
pub mod scheduler;

pub use handler::routes;
pub use scheduler::SchedulerEngine;
