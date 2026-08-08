//! 基础设施模块 — 日志、数据库、Redis、代码沙箱等底层服务初始化
//!
//! | 模块 | 说明 |
//! |------|------|
//! | [`log_util`] | tracing-subscriber 日志 + OTLP 遥测初始化（控制台 / 滚动文件 / OpenObserve）|
//! | [`db`] | PostgreSQL 初始化（预留） |
//! | [`redis`] | Redis 初始化（预留） |
//! | [`object_store`] | 对象存储（S3 兼容，截图/上传图/artifact/沙箱快照共用，presigned URL） |
//! | [`workspace_snapshot`] | 沙箱工作目录会话亲和容灾（tar.zst 快照 + 原子解包 + tar slipping 防护） |
//! | [`screenshot_cleanup`] | 截图按会话前缀清理（孤儿对象交对象存储生命周期规则） |
//! | [`sandbox`] | adk-sandbox 封装：隔离子进程执行 Rhai 脚本（验证 Layer 2） |
//! | [`shell_sandbox`] | bwrap 沙箱封装：shell_command 写仅 session + 读白名单 |
//! | [`shell_snapshot`] | 会话级 shell 环境快照（捕获 PATH/venv，每条命令 source，避免重复探测） |
//! | [`code_exec`] | adk-code 封装：RustExecutor 完整管线执行（验证 Layer 3） |
//! | [`store_base`] | Store 基座机制（`Store` trait 连接池样板 + `new_id` + `is_unique_violation`） |

pub mod code_exec;
pub mod db;
pub mod log_util;
pub mod object_store;
pub mod redis;
pub mod sandbox;
pub mod screenshot_cleanup;
pub mod shell_sandbox;
pub mod shell_snapshot;
pub mod store_base;
pub mod workspace_snapshot;
