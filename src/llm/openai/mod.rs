//! OpenAI 协议家族 — 两个实现 + 一层协商。
//!
//! - [`compat`]：`/chat/completions` 兼容客户端（`OpenAICustomCompatible`），
//!   自研实现，处理任意 OpenAI 兼容网关的流式/非流式请求；
//! - [`responses_auto`]：协议自动协商包装层（`OpenAiAutoLlm`）——首次调用前
//!   探测端点是否支持 Responses API（`/responses`），支持则优先走 adk 的
//!   `OpenAIResponsesClient`，否则回落 [`compat`]。**依赖方向：
//!   responses_auto 包装 compat**，反之无依赖。
//!
//! 选型在 [`crate::llm::factory`]（供应商配置 `openai_compat` 协议时进入本家族，
//! `CORTEX_DISABLE_OPENAI_RESPONSES=1` 可跳过协商直用 compat）。
//!
//! 与 [`crate::llm::anthropic_custom`]（单客户端家族）平级；Anthropic 家族
//! 无协商层，故直接平铺于 `llm/` 下。

pub mod compat;
pub mod responses_auto;
