//! 基础设施模块 — 日志、数据库、Redis、对象存储、沙箱等底层服务初始化
//!
//! | 模块 | 说明 |
//! |------|------|
//! | [`log_util`] | tracing-subscriber 日志 + OTLP 遥测初始化（控制台 / 滚动文件 / OpenObserve）|
//! | [`db`] | PostgreSQL 初始化（预留） |
//! | [`redis`] | Redis 初始化（预留） |
//! | [`object_store`] | 对象存储（S3 兼容，截图/上传图/artifact/沙箱快照共用，presigned URL） |
//! | [`screenshot_cleanup`] | 截图按会话前缀清理（孤儿对象交对象存储生命周期规则） |
//! | [`sandbox`] | 沙箱家族：shell_sandbox / sandbox_exec / shell_snapshot / workspace_snapshot / code_exec |
//! | [`store_base`] | Store 基座机制（`Store` trait 连接池样板 + `new_id` + `is_unique_violation`） |
//! | [`run_registry`] | 会话运行注册表（活跃 run 登记 + steer 运行中追加输入队列，对齐 codex InputQueue） |

pub mod db;
pub mod log_util;
pub mod run_registry;
pub mod object_store;
pub mod redis;
pub mod sandbox;
pub mod screenshot_cleanup;
pub mod store_base;
