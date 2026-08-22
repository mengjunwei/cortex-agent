//! 领域层 — 业务规则、领域模型、领域服务与 Repository
//!
//! 按"限界上下文"组织（见 docs/architecture.md §2.3）。依赖方向：可被应用层/传输层
//! 引用，自身只依赖基础设施层与横切层。

pub mod assistant;
pub mod audit;
pub mod auth;
pub mod device_catalog;
pub mod knowledge;
pub mod mcp;
pub mod memory;
pub mod model_provider;
pub mod monitor;
pub mod scheduled_task;
pub mod session;
pub mod shell_rules;
pub mod skill;
