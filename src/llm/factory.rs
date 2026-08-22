//! 模型工厂 — 从 ModelProviderStore 解析并构造 `Arc<dyn Llm>`。
//!
//! 从 llm/mod.rs 拆出:协议客户端在子模块,本文件只做「按配置选型 + 构造」。

use std::sync::Arc;
use std::time::Duration;

use crate::llm::anthropic_custom::{AnthropicClient, AnthropicConfig};
use adk_rust::model::openai::{OpenAIResponsesClient, OpenAIResponsesConfig};
use adk_rust::model::retry::RetryConfig;
use adk_rust::{GenerateContentConfig, Llm};

use crate::llm::openai::compat::{OpenAICustomCompatible, OpenAICustomCompatibleConfig};
use crate::domain::model_provider::ResolvedLlmConfig;
use crate::domain::model_provider::enums::ProviderProtocol;
use crate::domain::model_provider::store::ModelProviderStore;

/// 创建默认模型（DB 供应商存储中 `user_id` 的默认模型）
///
/// 使用 OpenAICustomCompatible 直接调用 OpenAI Compatible API，
/// 流式路径不经过 ADK ToolCallBuffer，避免 `<`/`[` 字符触发误缓冲截断。
/// GLM 通过结构化 tool_calls 字段发起工具调用，移除文本标签嗅探是安全的。
///
/// **按 user_id 隔离**：`user_id` 决定从哪个用户的桶里解析默认模型及其 API Key，
/// 杜绝跨用户密钥串用。无用户上下文的 boot 场景传 `""`（系统桶）。
pub fn make_model(store: &ModelProviderStore, user_id: &str) -> anyhow::Result<Arc<dyn Llm>> {
    make_model_by_id(store, None, user_id)
}

/// boot 期创建模型：供无用户上下文的系统级服务（如 `query_understanding` 单例）使用。
///
/// 与 [`make_model`] 的区别：系统桶（user_id=""）无可用模型时回退到任意已启用模型，
/// 避免管理员删除/禁用种子模型后启动失败。**仅供 boot**；运行时模型解析必须走
/// [`make_model`]（严格按 user_id 隔离）。详见 `ModelProviderStore::resolve_model_for_boot`。
pub fn make_model_boot(store: &ModelProviderStore) -> anyhow::Result<Arc<dyn Llm>> {
    let resolved = store.resolve_model_for_boot()?;
    make_model_from_resolved(&resolved)
}

/// 按 `model_id` 创建模型实例；`None` 解析该用户的默认模型。
///
/// 模型选择的唯一数据源是传入的 `store`（DB 供应商存储）。
/// 若指定模型已被禁用/不存在/不属于本用户，`store` 内部自动回退到默认/任意已启用模型。
pub fn make_model_by_id(
    store: &ModelProviderStore,
    model_id: Option<&str>,
    user_id: &str,
) -> anyhow::Result<Arc<dyn Llm>> {
    let resolved = store.resolve_model(model_id, user_id)?;
    make_model_from_resolved(&resolved)
}

/// 按 `model_id` 创建模型实例，并返回其完整解析配置（含 `context_window` 等元数据）。
///
/// 供需要模型元数据的调用方使用（如 CortexAgent 动态压缩阈值需要 context_window）。
pub fn make_model_and_meta(
    store: &ModelProviderStore,
    model_id: Option<&str>,
    user_id: &str,
) -> anyhow::Result<(Arc<dyn Llm>, ResolvedLlmConfig)> {
    let resolved = store.resolve_model(model_id, user_id)?;
    let model = make_model_from_resolved(&resolved)?;
    Ok((model, resolved))
}

/// 解析默认模型的完整描述（含模型名），供需要 `LlmRequest.model` 字符串的内部服务使用。
pub fn resolve_default_model(
    store: &ModelProviderStore,
    user_id: &str,
) -> anyhow::Result<ResolvedLlmConfig> {
    store.resolve_model(None, user_id)
}

pub fn make_model_from_resolved(resolved: &ResolvedLlmConfig) -> anyhow::Result<Arc<dyn Llm>> {
    // 增强重试配置：针对 429 Rate Limit 加大退避，后端静默重试不抛给前端（两种协议共用）
    let retry_config = RetryConfig::default()
        .with_max_retries(5)
        .with_initial_delay(Duration::from_secs(1))
        .with_max_delay(Duration::from_secs(30))
        .with_backoff_multiplier(2.0);

    match resolved.protocol {
        ProviderProtocol::Anthropic => {
            // Anthropic Messages 协议：走本地 super::super::anthropic_custom::AnthropicClient（抄自 adk-model，
            // 本地修了 base_url bug 与 SSE UTF-8 分包 bug；adk 原生 AnthropicClient 已弃用）。
            // base_url 为空时默认官方 https://api.anthropic.com（client 内部追加 /v1/messages）。
            let base_url = if resolved.base_url.trim().is_empty() {
                "https://api.anthropic.com".to_string()
            } else {
                resolved.base_url.trim().to_string()
            };
            let config =
                AnthropicConfig::new(&resolved.api_key, &resolved.model).with_base_url(&base_url);
            let client = AnthropicClient::new(config)
                .map_err(|e| anyhow::anyhow!("Anthropic 客户端初始化失败: {e}"))?;
            Ok(Arc::new(client.with_retry_config(retry_config)))
        }
        ProviderProtocol::OpenAiCompat => {
            // OpenAI Compatible 协议：端点前置校验，拦截 Anthropic 端点误配。
            // 部分厂商（GLM/智谱）同时提供 Anthropic Messages API 端点（不同请求/响应格式），
            // 若误配到 Anthropic 端点，OpenAI 客户端会静默失败（525ms 空响应），
            // 前端零输出，极难排查。这里显式拦截，给出可操作提示。
            if let Err(msg) = validate_openai_compatible_base_url(&resolved.base_url) {
                anyhow::bail!(
                    "模型 {} 的 base_url 配置错误：{}。请在「模型供应商管理」中修改。",
                    resolved.name,
                    msg
                );
            }
            let config = OpenAICustomCompatibleConfig::new(
                &resolved.api_key,
                &resolved.model,
                &resolved.base_url,
            )
            .with_provider_name(&resolved.provider_name);
            let compat =
                OpenAICustomCompatible::new(config).with_retry_config(retry_config.clone());

            // 协议自动协商（可用 CORTEX_DISABLE_OPENAI_RESPONSES=1 关闭）：
            // 首次调用前探测端点是否支持 OpenAI Responses API（/responses），
            // 支持则优先走 adk-rust 的 OpenAIResponsesClient（结构化 FC、原生
            // reasoning summary），否则回落本地 compat 客户端。结果按
            // base_url|model|SHA-256(api_key) 全局缓存（肯定/确定性否定长期
            // 有效，瞬时写短 TTL）；运行时自愈降级集合：401/403/404/405/501
            // 及 parse 错误（非 JSON 错误体无 status 码时的唯一信号）。
            if crate::llm::openai::responses_auto::disabled_by_env() {
                return Ok(Arc::new(compat));
            }
            let responses_config = OpenAIResponsesConfig::new(&resolved.api_key, &resolved.model)
                .with_base_url(resolved.base_url.trim_end_matches('/'))
                // 放宽空 API key 校验（Ollama/vLLM 等本地端点，探测时也不发鉴权头）
                .with_open_responses_mode(true);
            match OpenAIResponsesClient::new(responses_config) {
                Ok(responses) => Ok(Arc::new(crate::llm::openai::responses_auto::OpenAiAutoLlm::new(
                    compat,
                    // 与 compat 同样的增强重试（上游 responses 客户端仅非流式路径生效）
                    responses.with_retry_config(retry_config),
                    &resolved.base_url,
                    &resolved.api_key,
                    &resolved.model,
                ))),
                Err(_) => Ok(Arc::new(compat)), // responses 客户端构建失败：纯 compat 兜底
            }
        }
    }
}

/// 检查 base_url 是否是 OpenAI Compatible 端点。
///
/// 拦截明确不兼容的场景，避免静默失败：
/// - 含 `anthropic` 路径段（Claude Messages API，格式完全不同）
/// - 含 `/messages` 路径段（同上）
/// - 空串
///
/// 只做保守拦截；不认识的自定义域名/路径都放行。
pub fn validate_openai_compatible_base_url(base_url: &str) -> Result<(), String> {
    let s = base_url.trim().to_lowercase();
    if s.is_empty() {
        return Err("base_url 为空".to_string());
    }
    if s.contains("/anthropic") || s.ends_with("/anthropic") {
        return Err(format!(
            "检测到 Anthropic 端点 `{base_url}`，本项目只支持 OpenAI Compatible 协议。\
             例如 GLM 应改为 `https://open.bigmodel.cn/api/paas/v4`"
        ));
    }
    if s.ends_with("/v1/messages") || s.ends_with("/messages") {
        return Err(format!(
            "检测到 `/messages` 端点 `{base_url}`（可能是 Anthropic Messages API），\
             本项目只支持 OpenAI Compatible 协议（`/chat/completions` 路径）"
        ));
    }
    Ok(())
}

/// 构建 GenerateContentConfig
///
/// 只用 typed field max_output_tokens（序列化为 max_completion_tokens），
/// 不用 extensions.max_tokens（会与 max_completion_tokens 冲突导致 Ark API 返回空）。
pub fn make_gen_config(max_tokens: i32) -> GenerateContentConfig {
    GenerateContentConfig {
        max_output_tokens: Some(max_tokens),
        temperature: Some(0.3),
        // 保守的重复惩罚：抑制 token 级重复退化（degeneration），从源头降低死循环概率。
        // 推理模型思考模式可能被 API 静默忽略（无副作用）。
        frequency_penalty: Some(0.4),
        presence_penalty: Some(0.3),
        ..Default::default()
    }
}

/// 自定义助手的参数化生成配置（M3，计划 §7 / 设计 §8.2）
///
/// - 三参数全部可选：助手配置时未设置则走默认值
/// - `max_tokens = None` → 默认 16384（对齐 codex「给够预算」；高思考级别 thinking budget 从中扣，4096 太小会截断 thinking 致死循环）
/// - `temperature = None` → 不设置（由模型/API 默认值决定；避免对不支持 temperature 的模型如 O1/O3 报错）
/// - `top_p = None` → 不设置（由模型/API 默认值决定）
pub fn make_gen_config_from(
    max_tokens: Option<i32>,
    temperature: Option<f64>,
    top_p: Option<f64>,
    thinking_level: Option<&str>,
) -> GenerateContentConfig {
    // GenerateContentConfig 用 f32（ADK 对接 OpenAI/Ark 的内部精度），
    // Assistant 领域模型用 f64（JSON 数字常用类型），在此做一次显式收窄。
    let mut cfg = GenerateContentConfig {
        max_output_tokens: max_tokens.or(Some(16384)),
        temperature: temperature.map(|v| v as f32),
        top_p: top_p.map(|v| v as f32),
        // 保守的重复惩罚：抑制 token 级重复退化，降低死循环概率（对齐 make_gen_config）
        frequency_penalty: Some(0.4),
        presence_penalty: Some(0.3),
        ..Default::default()
    };
    // 思考级别写入 extensions（双协议键都塞，发送端各取所需）：
    //  - OpenAI：extensions["openai"] 后门会自动合并进请求 body（reasoning_effort）
    //  - Anthropic：client.build_message_params 从 extensions["anthropic"].effort 读取
    // 值统一 low/medium/high；不支持的模型由错误兜底（阶段3）静默降级。
    if let Some(level) = thinking_level {
        // Anthropic effort 支持 low/medium/high/xhigh/max 全档
        cfg.extensions.insert(
            "anthropic".to_string(),
            adk_rust::serde_json::json!({ "effort": level }),
        );
        // OpenAI reasoning_effort 支持 low/medium/high/xhigh（codex 风格 4 档）；
        // max 是 Anthropic 专属，OpenAI 不发（走模型默认）。
        // Responses 路径的嵌套 reasoning.effort 键由 openai_responses_auto 包装层
        // 按需转换（chat 路径会把整个 extensions["openai"] 合并进 body，不能在此
        // 塞嵌套键污染 /chat/completions 请求）。
        if matches!(level, "low" | "medium" | "high" | "xhigh") {
            cfg.extensions.insert(
                "openai".to_string(),
                adk_rust::serde_json::json!({ "reasoning_effort": level }),
            );
        }
    }
    cfg
}

