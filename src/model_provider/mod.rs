//! 模型供应商管理模块
//!
//! 取代配置文件 `[llm]` 段作为模型列表的唯一数据源：
//! - [`store::ModelProviderStore`]：DB 存储 + 内存缓存 + 模型解析
//! - [`crypto::AesCodec`]：API Key 的 AES-256-GCM 加密
//! - [`enums::Status`]：数字枚举（0/1）与前后端映射
//! - [`dto`]：HTTP 请求/响应结构（API Key 永不外泄）
//!
//! ## 依赖注入
//!
//! 历史版本曾用进程级全局 `GLOBAL_STORE` + `set_global_store` 共享存储，
//! 现已移除（违反架构 §5.1）。当前由 [`crate::bootstrap::build_app_deps`]
//! 装配后通过 [`crate::bootstrap::AppDeps`] 字段注入到所有调用点。
//! LLM 客户端工厂 [`crate::llm::make_model_by_id`] 也已改为接收 `&ModelProviderStore` 参数。

pub mod crypto;
pub mod dto;
pub mod enums;
pub mod probe;
pub mod store;

pub use probe::{ProbeKind, ProbeResult, ProbeStatus, ResolvedForProbe};
pub use store::ResolvedLlmConfig;
