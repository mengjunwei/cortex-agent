//! Anthropic/Claude provider 本地副本（抄自 adk-model 1.0.0 `src/anthropic/`）。
//!
//! 唯一改动：修复 `AnthropicClient::new` 的 base_url bug —— adk-model 1.0.0 建底层
//! `Anthropic` client 时只传 api_key、忽略 `AnthropicConfig.base_url`，导致 Anthropic 协议
//! 永远打官方 `https://api.anthropic.com`，数据库配置的中转地址完全不生效。
//!
//! 存在原因：adk-model 字段 `pub(super)`、无公开 setter，项目层无法修复；新版本未发布。
//! 等 adk-model 修复后可删除本模块，恢复使用 `adk_rust::model::anthropic`。

mod attachment;
mod client;
mod config;
mod convert;
mod error;
mod models;
mod rate_limit;
pub mod schema_adapter;
mod sse_stream;
mod token_count;

pub use client::AnthropicClient;
pub use config::{AnthropicConfig, Effort, ThinkingMode};
pub use error::{AnthropicApiError, ConversionError};
pub use models::ModelInfo;
pub use rate_limit::RateLimitInfo;
pub use schema_adapter::AnthropicSchemaAdapter;
pub use token_count::TokenCount;

// Re-export ToolSearchConfig from adk-anthropic for convenience.
pub use adk_anthropic::ToolSearchConfig;
