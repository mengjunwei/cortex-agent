//! LLM 层 — 自定义模型客户端实现与配置
//!
//! 绕过 ADK 框架限制（如 ToolCallBuffer 截断问题），直接对接 OpenAI Compatible API。

pub mod anthropic_custom;
pub mod openai_custom;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// 全局单调合成工具调用 id 计数器。
///
/// 弱供应商/文本标签来源的 FunctionCall 可能全程不发 id（id=None 或空串）。这些 FC 需要补一个
/// 占位 id，否则后续回填的 FunctionResponse 拿到空 id，`normalize_function_pairs` 会把它当孤立
/// FR 误删 → 触发严格模式（Anthropic/OpenAI）400。
///
/// 关键不变量：**跨轮、跨 run、跨协议全局唯一**。若用 `call_{idx}` / `call_{iteration}_{n}` 这类
/// 局部序号，同一 id 会在不同迭代/不同运行里重复——压缩/历史拼接后 normalize 会把同 id 的旧
/// FC 与新 FC 混淆（删除/错配）。用全局单调原子计数器保证每个合成 id 只出现一次。
static SYNTHETIC_CALL_ID: AtomicU64 = AtomicU64::new(0);

/// 生成全局唯一合成工具调用 id（`call_s{n}`，n 单调递增）。
///
/// 仅在 FC 真实 id 缺失（None/空）时调用；真实 id 优先保留。`call_s` 前缀与供应商真实 id
/// （`call_xxx`、`toolu_xxx`）区分，便于日志辨认是合成占位。
pub fn next_synthetic_call_id() -> String {
    format!("call_s{}", SYNTHETIC_CALL_ID.fetch_add(1, Ordering::Relaxed))
}

use crate::llm::anthropic_custom::{AnthropicClient, AnthropicConfig};
use adk_rust::model::retry::RetryConfig;
use adk_rust::{GenerateContentConfig, Llm};

use crate::llm::openai_custom::{OpenAICustomCompatible, OpenAICustomCompatibleConfig};
use crate::model_provider::ResolvedLlmConfig;
use crate::model_provider::enums::ProviderProtocol;
use crate::model_provider::store::ModelProviderStore;

/// 创建默认模型（DB 供应商存储的默认模型）
///
/// 使用 OpenAICustomCompatible 直接调用 OpenAI Compatible API，
/// 流式路径不经过 ADK ToolCallBuffer，避免 `<`/`[` 字符触发误缓冲截断。
/// GLM 通过结构化 tool_calls 字段发起工具调用，移除文本标签嗅探是安全的。
///
/// 取代了历史 `make_model()` 无参版本：模型供应商存储现在通过参数显式注入，
/// 不再依赖 `model_provider::GLOBAL_STORE` 全局（见架构 §5.1）。
pub fn make_model(store: &ModelProviderStore) -> anyhow::Result<Arc<dyn Llm>> {
    make_model_by_id(store, None)
}

/// 按 `model_id` 创建模型实例；`None` 解析 DB 默认模型。
///
/// 模型选择的唯一数据源是传入的 `store`（DB 供应商存储）。
/// 若指定模型已被禁用或不存在，`store` 内部自动回退到默认/任意已启用模型。
pub fn make_model_by_id(
    store: &ModelProviderStore,
    model_id: Option<&str>,
) -> anyhow::Result<Arc<dyn Llm>> {
    let resolved = store.resolve_model(model_id)?;
    make_model_from_resolved(&resolved)
}

/// 按 `model_id` 创建模型实例，并返回其完整解析配置（含 `context_window` 等元数据）。
///
/// 供需要模型元数据的调用方使用（如 CortexAgent 动态压缩阈值需要 context_window）。
pub fn make_model_and_meta(
    store: &ModelProviderStore,
    model_id: Option<&str>,
) -> anyhow::Result<(Arc<dyn Llm>, ResolvedLlmConfig)> {
    let resolved = store.resolve_model(model_id)?;
    let model = make_model_from_resolved(&resolved)?;
    Ok((model, resolved))
}

/// 解析默认模型的完整描述（含模型名），供需要 `LlmRequest.model` 字符串的内部服务使用。
pub fn resolve_default_model(store: &ModelProviderStore) -> anyhow::Result<ResolvedLlmConfig> {
    store.resolve_model(None)
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
            // Anthropic Messages 协议：走本地 anthropic_custom::AnthropicClient（抄自 adk-model，
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
            Ok(Arc::new(
                OpenAICustomCompatible::new(config).with_retry_config(retry_config),
            ))
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
        if matches!(level, "low" | "medium" | "high" | "xhigh") {
            cfg.extensions.insert(
                "openai".to_string(),
                adk_rust::serde_json::json!({ "reasoning_effort": level }),
            );
        }
    }
    cfg
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_anthropic_endpoint() {
        // GLM 的 Anthropic 端点应被拦截
        let r = validate_openai_compatible_base_url("https://open.bigmodel.cn/api/anthropic");
        assert!(r.is_err());
        let msg = r.unwrap_err();
        assert!(msg.contains("Anthropic"));
        assert!(msg.contains("paas/v4"), "应给出正确端点提示: {msg}");
    }

    #[test]
    fn rejects_anthropic_with_trailing_slash() {
        let r = validate_openai_compatible_base_url("https://open.bigmodel.cn/api/anthropic/");
        assert!(r.is_err());
    }

    #[test]
    fn rejects_messages_endpoint() {
        let r = validate_openai_compatible_base_url("https://api.anthropic.com/v1/messages");
        assert!(r.is_err());
    }

    #[test]
    fn rejects_empty_base_url() {
        assert!(validate_openai_compatible_base_url("").is_err());
        assert!(validate_openai_compatible_base_url("   ").is_err());
    }

    #[test]
    fn accepts_openai_compatible_endpoints() {
        // OpenAI 官方
        assert!(validate_openai_compatible_base_url("https://api.openai.com/v1").is_ok());
        // DeepSeek
        assert!(validate_openai_compatible_base_url("https://api.deepseek.com/v1").is_ok());
        // GLM OpenAI 兼容
        assert!(
            validate_openai_compatible_base_url("https://open.bigmodel.cn/api/paas/v4").is_ok()
        );
        // Ollama 本地
        assert!(validate_openai_compatible_base_url("http://localhost:11434/v1").is_ok());
        // 通义千问
        assert!(
            validate_openai_compatible_base_url(
                "https://dashscope.aliyuncs.com/compatible-mode/v1"
            )
            .is_ok()
        );
    }

    #[test]
    fn case_insensitive_check() {
        // 大写变体也要拦截
        let r = validate_openai_compatible_base_url("https://open.bigmodel.cn/API/Anthropic");
        assert!(r.is_err());
    }

    #[test]
    fn make_gen_config_defaults_anti_repetition_penalty() {
        // 默认生成配置应携带保守的重复惩罚，从源头压低 LLM degeneration 概率
        let cfg = make_gen_config(1024);
        assert_eq!(
            cfg.frequency_penalty,
            Some(0.4),
            "默认应设 frequency_penalty=0.4"
        );
        assert_eq!(
            cfg.presence_penalty,
            Some(0.3),
            "默认应设 presence_penalty=0.3"
        );
    }

    #[test]
    fn make_gen_config_from_defaults_anti_repetition_penalty() {
        let cfg = make_gen_config_from(None, None, None, None);
        assert_eq!(cfg.frequency_penalty, Some(0.4));
        assert_eq!(cfg.presence_penalty, Some(0.3));
    }
}
