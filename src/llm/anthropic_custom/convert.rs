//! Type conversions between ADK and adk-anthropic types.

use super::attachment;
use super::error::ConversionError;
use adk_anthropic::ImageMediaType;
use adk_anthropic::{
    Base64ImageSource, Base64PdfSource, CacheControlEphemeral, ContentBlock, ContextManagement,
    DocumentBlock, ImageBlock, Message, MessageCreateParams, MessageParam, MessageRole, Model,
    PlainTextSource, StopReason, SystemPrompt, TextBlock, ToolParam, ToolResultBlock,
    ToolResultBlockContent, ToolUnionParam, ToolUseBlock, UrlImageSource, UrlPdfSource,
};
use adk_rust::{
    Content, FinishReason, LlmResponse, Part, SchemaAdapter, SchemaCache, UsageMetadata,
};
use serde_json::Value;
use std::collections::HashMap;

fn tool_result_content(value: &Value) -> ToolResultBlockContent {
    match value {
        Value::String(text) => ToolResultBlockContent::String(text.clone()),
        Value::Object(_) | Value::Array(_) => {
            ToolResultBlockContent::String(serde_json::to_string(value).unwrap_or_default())
        }
        other => ToolResultBlockContent::String(other.to_string()),
    }
}

/// Convert ADK Content to adk-anthropic MessageParam.
///
/// When `prompt_caching` is true, eligible content blocks will have
/// `cache_control: {"type": "ephemeral"}` set on them.
///
/// Returns `Err(ConversionError::UnsupportedMimeType)` if any part contains
/// an unsupported MIME type for `InlineData` or `FileData`.
pub fn content_to_message(
    content: &Content,
    _prompt_caching: bool,
) -> Result<MessageParam, ConversionError> {
    let role = match content.role.as_str() {
        "user" | "function" | "tool" => MessageRole::User,
        "model" | "assistant" => MessageRole::Assistant,
        _ => MessageRole::User,
    };

    // Note: cache_control is applied at the system prompt and top-level request
    // level only (max 4 blocks). Individual message blocks do not get cache_control
    // to avoid exceeding Anthropic's 4-block limit.

    let blocks: Vec<ContentBlock> = content
        .parts
        .iter()
        .filter_map(|part| match part {
            Part::Text { text } => {
                if text.is_empty() {
                    None
                } else {
                    Some(ContentBlock::Text(TextBlock::new(text.clone())))
                }
            }
            Part::FunctionCall { name, args, id, .. } => {
                let block = ToolUseBlock {
                    id: id
                        .clone()
                        .unwrap_or_else(crate::llm::next_synthetic_call_id),
                    name: name.clone(),
                    input: args.clone(),
                    cache_control: None,
                };
                Some(ContentBlock::ToolUse(block))
            }
            Part::FunctionResponse {
                function_response,
                id,
                ..
            } => Some(ContentBlock::ToolResult(ToolResultBlock {
                tool_use_id: id.clone().unwrap_or_else(|| "unknown".to_string()),
                content: Some(tool_result_content(&function_response.response)),
                is_error: None,
                cache_control: None,
            })),
            Part::EmbeddedResource { .. } => None,
            Part::InlineData {
                mime_type, data, ..
            } => {
                let media_type = match mime_type.as_str() {
                    "image/jpeg" => Some(ImageMediaType::Jpeg),
                    "image/png" => Some(ImageMediaType::Png),
                    "image/gif" => Some(ImageMediaType::Gif),
                    "image/webp" => Some(ImageMediaType::Webp),
                    _ => None,
                };
                if let Some(media_type) = media_type {
                    let encoded = attachment::encode_base64(data);
                    Some(ContentBlock::Image(ImageBlock::new_with_base64(
                        Base64ImageSource::new(encoded, media_type),
                    )))
                } else if mime_type == "application/pdf" {
                    let encoded = attachment::encode_base64(data);
                    Some(ContentBlock::Document(DocumentBlock::new_with_base64_pdf(
                        Base64PdfSource::new(encoded),
                    )))
                } else if mime_type.starts_with("text/") {
                    match String::from_utf8(data.clone()) {
                        Ok(text) => Some(ContentBlock::Document(
                            DocumentBlock::new_with_plain_text(PlainTextSource::new(text)),
                        )),
                        Err(_) => Some(ContentBlock::Text(TextBlock::new(
                            attachment::inline_attachment_to_text(mime_type, data),
                        ))),
                    }
                } else {
                    Some(ContentBlock::Text(TextBlock::new(
                        attachment::inline_attachment_to_text(mime_type, data),
                    )))
                }
            }
            Part::FileData {
                mime_type,
                file_uri,
                ..
            } => {
                if mime_type == "application/pdf" {
                    Some(ContentBlock::Document(DocumentBlock::new_with_url_pdf(
                        UrlPdfSource::new(file_uri.clone()),
                    )))
                } else if matches!(
                    mime_type.as_str(),
                    "image/jpeg" | "image/png" | "image/gif" | "image/webp"
                ) {
                    Some(ContentBlock::Image(ImageBlock::new_with_url(
                        UrlImageSource::new(file_uri.clone()),
                    )))
                } else {
                    Some(ContentBlock::Text(TextBlock::new(
                        attachment::file_attachment_to_text(mime_type, file_uri),
                    )))
                }
            }
            Part::Thinking { thinking, .. } => {
                if thinking.is_empty() {
                    None
                } else {
                    Some(ContentBlock::Text(TextBlock::new(thinking.clone())))
                }
            }
            // Server-side tool parts: convert back to Anthropic types when possible
            Part::ServerToolCall { server_tool_call } => serde_json::from_value::<
                adk_anthropic::ServerToolUseBlock,
            >(server_tool_call.clone())
            .ok()
            .map(ContentBlock::ServerToolUse),
            Part::ServerToolResponse {
                server_tool_response,
            } => serde_json::from_value::<adk_anthropic::WebSearchToolResultBlock>(
                server_tool_response.clone(),
            )
            .ok()
            .map(ContentBlock::WebSearchToolResult),
        })
        .collect();

    // If no blocks, add a placeholder for assistant messages
    let blocks = if blocks.is_empty() && role == MessageRole::Assistant {
        vec![ContentBlock::Text(TextBlock::new(" ".to_string()))]
    } else if blocks.is_empty() {
        vec![ContentBlock::Text(TextBlock::new("".to_string()))]
    } else {
        blocks
    };

    Ok(MessageParam::new_with_blocks(blocks, role))
}

/// Convert ADK tools to adk-anthropic ToolUnionParam format.
pub fn convert_tools(
    tools: &HashMap<String, Value>,
    adapter: &dyn SchemaAdapter,
    cache: &SchemaCache,
) -> Result<Vec<ToolUnionParam>, ConversionError> {
    tools
        .iter()
        .map(|(name, decl)| {
            if let Some(provider_tool) = decl.get("x-adk-anthropic-tool") {
                return serde_json::from_value::<ToolUnionParam>(provider_tool.clone()).map_err(
                    |error| {
                        ConversionError::InvalidToolDeclaration(format!(
                            "failed to deserialize Anthropic native tool '{name}': {error}"
                        ))
                    },
                );
            }

            let description = decl
                .get("description")
                .and_then(|d| d.as_str())
                .map(String::from);

            let input_schema = decl
                .get("parameters")
                .cloned()
                .unwrap_or_else(|| adapter.empty_schema());
            let normalized_schema = cache.normalize(&input_schema);

            let normalized_name = adapter.normalize_tool_name(name);

            let mut tool_param = ToolParam::new(normalized_name.into_owned(), normalized_schema);
            if let Some(desc) = description {
                tool_param = tool_param.with_description(desc);
            }

            Ok(ToolUnionParam::CustomTool(tool_param))
        })
        .collect()
}

/// Convert adk-anthropic Message to ADK LlmResponse.
pub fn from_anthropic_message(message: &Message) -> LlmResponse {
    let mut parts = Vec::new();

    for block in &message.content {
        match block {
            ContentBlock::Text(text_block) if !text_block.text.is_empty() => {
                parts.push(Part::Text {
                    text: text_block.text.clone(),
                });
            }
            ContentBlock::ToolUse(tool_use) => {
                parts.push(Part::FunctionCall {
                    name: tool_use.name.clone(),
                    args: tool_use.input.clone(),
                    id: Some(tool_use.id.clone()),
                    thought_signature: None,
                });
            }
            ContentBlock::Thinking(thinking_block) if !thinking_block.thinking.is_empty() => {
                parts.push(Part::Thinking {
                    thinking: thinking_block.thinking.clone(),
                    signature: if thinking_block.signature.is_empty() {
                        None
                    } else {
                        Some(thinking_block.signature.clone())
                    },
                });
            }
            ContentBlock::ServerToolUse(server_tool_use) => {
                if let Ok(val) = serde_json::to_value(server_tool_use) {
                    parts.push(Part::ServerToolCall {
                        server_tool_call: val,
                    });
                }
            }
            ContentBlock::WebSearchToolResult(web_search_result) => {
                if let Ok(val) = serde_json::to_value(web_search_result) {
                    parts.push(Part::ServerToolResponse {
                        server_tool_response: val,
                    });
                }
            }
            _ => {}
        }
    }

    let content = if parts.is_empty() {
        None
    } else {
        Some(Content {
            role: "model".to_string(),
            parts,
        })
    };

    // 折算成统一语义（input 含 cache）：见 unified_input_tokens 文档注释。
    let usage_in = unified_input_tokens(&message.usage);
    let usage_metadata = Some(UsageMetadata {
        prompt_token_count: usage_in,
        candidates_token_count: message.usage.output_tokens,
        total_token_count: usage_in.saturating_add(message.usage.output_tokens),
        cache_read_input_token_count: message.usage.cache_read_input_tokens,
        cache_creation_input_token_count: message.usage.cache_creation_input_tokens,
        ..Default::default()
    });

    let finish_reason = message.stop_reason.as_ref().map(|sr| match sr {
        StopReason::EndTurn => FinishReason::Stop,
        StopReason::MaxTokens => FinishReason::MaxTokens,
        StopReason::StopSequence => FinishReason::Stop,
        StopReason::ToolUse => FinishReason::Stop,
        _ => FinishReason::Stop,
    });

    LlmResponse {
        content,
        usage_metadata,
        finish_reason,
        citation_metadata: None,
        partial: false,
        turn_complete: true,
        interrupted: false,
        error_code: None,
        error_message: None,
        provider_metadata: None,
        interaction_id: None,
    }
}

/// Convert streaming text delta to ADK LlmResponse.
pub fn from_text_delta(text: &str) -> LlmResponse {
    LlmResponse {
        content: Some(Content {
            role: "model".to_string(),
            parts: vec![Part::Text {
                text: text.to_string(),
            }],
        }),
        usage_metadata: None,
        finish_reason: None,
        citation_metadata: None,
        partial: true,
        turn_complete: false,
        interrupted: false,
        error_code: None,
        error_message: None,
        provider_metadata: None,
        interaction_id: None,
    }
}

/// Convert streaming thinking delta to ADK LlmResponse.
pub fn from_thinking_delta(thinking_text: &str) -> LlmResponse {
    LlmResponse {
        content: Some(Content {
            role: "model".to_string(),
            parts: vec![Part::Thinking {
                thinking: thinking_text.to_string(),
                signature: None,
            }],
        }),
        partial: true,
        turn_complete: false,
        ..Default::default()
    }
}

/// Create an LlmResponse representing a streaming error event.
pub fn from_stream_error(error_type: &str, message: &str) -> LlmResponse {
    LlmResponse {
        content: None,
        usage_metadata: None,
        finish_reason: None,
        citation_metadata: None,
        partial: false,
        turn_complete: true,
        interrupted: false,
        error_code: Some(error_type.to_string()),
        error_message: Some(message.to_string()),
        provider_metadata: None,
        interaction_id: None,
    }
}

/// 折算 Anthropic usage 为统一（OpenAI/Gemini）语义的 input token 数。
///
/// Anthropic 语义：input / cache_read / cache_creation（及 1h 层）是并列字段，input
/// 不含 cache；OpenAI/Gemini 语义：cache 是 input 的子集。下游（compaction 的
/// effective = total − cache_read、SSE 显示、子 agent 计数）按统一语义计算，故
/// 在转换层折算：**input = input + cache_read + cache_creation(+1h)**，total =
/// input + output，cache_read 单独保留。否则下游双重扣除 cache → 压缩迟迟不触发 /
/// 显示低估。加法 saturating：token 值来自网络输入（第三方网关），防巨值回绕为负
/// 被 `total > 0` 过滤后静默回退字符估算。
pub fn unified_input_tokens(usage: &adk_anthropic::Usage) -> i32 {
    merge_unified_input(
        usage.input_tokens,
        usage.cache_read_input_tokens,
        usage.cache_creation_input_tokens,
        usage.cache_creation_input_tokens_1h,
    )
}

/// 流式路径版 [`unified_input_tokens`]：从字段级快照折算（快照存原始值，
/// MessageDelta cumulative 的 None 已在快照层被「沿用」处理，见 client.rs 注释）。
/// 语义与折算规则同上，MessageDeltaUsage 无 _1h 字段故 _1h 只来自 MessageStart。
pub fn merge_unified_input(
    input: i32,
    cache_read: Option<i32>,
    cache_creation: Option<i32>,
    cache_creation_1h: Option<i32>,
) -> i32 {
    input
        .saturating_add(cache_read.unwrap_or(0))
        .saturating_add(cache_creation.unwrap_or(0))
        .saturating_add(cache_creation_1h.unwrap_or(0))
}

/// Extract cache usage tokens from an adk-anthropic `Usage` into provider metadata.
pub fn extract_cache_usage(usage: &adk_anthropic::Usage) -> HashMap<String, String> {
    let mut metadata = HashMap::new();
    if let Some(tokens) = usage.cache_creation_input_tokens {
        metadata.insert(
            "anthropic.cache_creation_input_tokens".to_string(),
            tokens.to_string(),
        );
    }
    if let Some(tokens) = usage.cache_read_input_tokens {
        metadata.insert(
            "anthropic.cache_read_input_tokens".to_string(),
            tokens.to_string(),
        );
    }
    metadata
}

/// system prompt 的 stable/volatile 分段（用于 Anthropic 分 block 打 cache_control）。
/// stable 段打 cache_control（命中缓存），volatile 段（如时间）不打（每次刷新）。
pub struct SystemPromptSegments {
    pub stable: String,
    pub volatile: Option<String>,
}

/// Build MessageCreateParams from LlmRequest.
#[allow(clippy::too_many_arguments)]
pub fn build_message_params(
    model: &str,
    max_tokens: u32,
    messages: Vec<MessageParam>,
    tools: Vec<ToolUnionParam>,
    system_prompt: Option<SystemPromptSegments>,
    temperature: Option<f32>,
    top_p: Option<f32>,
    top_k: Option<i32>,
    prompt_caching: bool,
    thinking: Option<&super::config::ThinkingMode>,
    effort: Option<super::config::Effort>,
    fast_mode: bool,
    inference_geo: Option<&str>,
    service_tier: Option<&str>,
    context_management: Option<&ContextManagement>,
) -> MessageCreateParams {
    let mut params =
        MessageCreateParams::new(max_tokens, messages, Model::Custom(model.to_string()));

    if !tools.is_empty() {
        params.tools = Some(tools);
    }

    if let Some(sys) = system_prompt {
        let mut blocks = vec![];
        // stable 段：prompt_caching 时打 cache_control（缓存命中到此 block 末尾）
        let stable_block = if prompt_caching {
            TextBlock::new(sys.stable).with_cache_control(CacheControlEphemeral::new())
        } else {
            TextBlock::new(sys.stable)
        };
        blocks.push(stable_block);
        // volatile 段（时间等）：不打 cache_control，每次刷新
        if let Some(v) = sys.volatile {
            blocks.push(TextBlock::new(v));
        }
        params.system = Some(SystemPrompt::from_blocks(blocks));
    }

    if let Some(temp) = temperature {
        params.temperature = Some(temp);
    }

    if let Some(p) = top_p {
        params.top_p = Some(p);
    }

    if let Some(k) = top_k {
        params.top_k = Some(k as u32);
    }

    // Thinking mode
    match thinking {
        Some(super::config::ThinkingMode::Enabled { budget_tokens }) => {
            params.thinking = Some(adk_anthropic::ThinkingConfig::enabled(*budget_tokens));
        }
        Some(super::config::ThinkingMode::Adaptive) => {
            params.thinking = Some(adk_anthropic::ThinkingConfig::adaptive());
        }
        None => {}
    }

    // Effort → output_config.effort
    if let Some(effort) = effort {
        let level = match effort {
            super::config::Effort::Low => adk_anthropic::EffortLevel::Low,
            super::config::Effort::Medium => adk_anthropic::EffortLevel::Medium,
            super::config::Effort::High => adk_anthropic::EffortLevel::High,
            super::config::Effort::XHigh => adk_anthropic::EffortLevel::XHigh,
            super::config::Effort::Max => adk_anthropic::EffortLevel::Max,
        };
        params.output_config = Some(adk_anthropic::OutputConfig::with_effort(level));
    }

    // Fast mode
    if fast_mode {
        params.speed = Some(adk_anthropic::SpeedMode::Fast);
    }

    // Inference geo
    if let Some(geo) = inference_geo {
        params.inference_geo = Some(geo.to_string());
    }

    // Service tier
    if let Some(tier) = service_tier {
        params.service_tier = Some(tier.to_string());
    }

    // Automatic prompt caching (top-level cache_control)
    if prompt_caching {
        params.cache_control = Some(CacheControlEphemeral::new());
    }

    // Context management (beta)
    if let Some(cm) = context_management {
        params.context_management = Some(cm.clone());
    }

    // 运行时证据：打印实际发出的 system payload block 结构（验证 stable 缓存断点）。
    // 降 debug:每次 LLM 请求都执行,验证期结束后的常态运行只是重复刷屏。
    if let Some(adk_anthropic::SystemPrompt::Blocks(blocks)) = &params.system {
        let summary: Vec<(usize, bool)> = blocks
            .iter()
            .map(|b| (b.block.text.len(), b.block.cache_control.is_some()))
            .collect();
        tracing::debug!(
            "[prompt-cache] anthropic system blocks (chars, cached): {:?}",
            summary
        );
    }

    params
}

#[cfg(test)]
mod usage_semantics_tests {
    use super::*;
    use adk_anthropic::{Message, Model};

    /// 语义锁：Anthropic input 不含 cache（并列字段），折算后 prompt/total 必须含
    /// cache_read + cache_creation(+1h)，否则下游 effective = total − cache_read
    /// 双重扣除、压缩迟迟不触发（本修复要锁住的回归点，防上游同步时静默还原）。
    #[test]
    fn unified_input_includes_all_cache_tiers() {
        let usage = adk_anthropic::Usage::new(1000, 200)
            .with_cache_read_input_tokens(50_000)
            .with_cache_creation_input_tokens(2_000)
            .with_cache_creation_input_tokens_1h(500);
        assert_eq!(unified_input_tokens(&usage), 1000 + 50_000 + 2_000 + 500);
    }

    /// 无 cache 字段（第三方极简网关）→ 原样返回 input。
    #[test]
    fn unified_input_without_cache_is_passthrough() {
        let usage = adk_anthropic::Usage::new(300, 40);
        assert_eq!(unified_input_tokens(&usage), 300);
    }

    /// 恶意/异常巨值不回绕为负（被下游 `total > 0` 过滤后静默回退字符估算）。
    #[test]
    fn unified_input_saturates_instead_of_wraparound() {
        let usage =
            adk_anthropic::Usage::new(i32::MAX - 10, 1).with_cache_read_input_tokens(i32::MAX - 10);
        assert_eq!(unified_input_tokens(&usage), i32::MAX);
    }

    /// 流式快照折算：MessageDelta cumulative 各字段独立可选，None=未提供须沿用快照。
    /// 快照层已 merge（client.rs），此处锁 merge_unified_input 的折算口径与
    /// unified_input_tokens 一致（含 _1h，流式 delta 无 _1h 字段由快照补齐）。
    #[test]
    fn merge_unified_input_matches_struct_version() {
        let usage = adk_anthropic::Usage::new(1000, 200)
            .with_cache_read_input_tokens(50_000)
            .with_cache_creation_input_tokens(2_000)
            .with_cache_creation_input_tokens_1h(500);
        assert_eq!(
            merge_unified_input(
                usage.input_tokens,
                usage.cache_read_input_tokens,
                usage.cache_creation_input_tokens,
                usage.cache_creation_input_tokens_1h,
            ),
            unified_input_tokens(&usage)
        );
        // 各字段 None（极简网关）→ 原样返回 input。
        assert_eq!(merge_unified_input(300, None, None, None), 300);
    }

    /// 非流式转换：total = 折算 input + output，cache_read 单独保留。
    #[test]
    fn from_message_total_includes_cache() {
        let usage = adk_anthropic::Usage::new(1000, 200)
            .with_cache_read_input_tokens(50_000)
            .with_cache_creation_input_tokens(2_000);
        let msg = Message::new(
            "msg_test".to_string(),
            vec![],
            Model::Custom("claude-test".to_string()),
            usage,
        );
        let resp = from_anthropic_message(&msg);
        let m = resp.usage_metadata.unwrap();
        assert_eq!(m.prompt_token_count, 53_000);
        assert_eq!(m.total_token_count, 53_200);
        assert_eq!(m.cache_read_input_token_count, Some(50_000));
        assert_eq!(m.cache_creation_input_token_count, Some(2_000));
    }
}

#[cfg(test)]
mod system_cache_tests {
    use super::*;

    /// 证据测试：stable/volatile 分段 → Anthropic system 拆成两个 TextBlock，
    /// stable block 带 cache_control（命中缓存），volatile block（时间）不带。
    #[test]
    fn stable_block_gets_cache_control_volatile_does_not() {
        let seg = SystemPromptSegments {
            stable: "STABLE_BODY".to_string(),
            volatile: Some("VOLATILE_TIME".to_string()),
        };
        let params = build_message_params(
            "claude-test",
            1024,
            vec![],
            vec![],
            Some(seg),
            None,
            None,
            None,
            true, // prompt_caching = true
            None,
            None,
            false,
            None,
            None,
            None,
        );
        eprintln!("ANTHROPIC_SYSTEM_PAYLOAD = {:#?}", params.system);
        match params.system {
            Some(SystemPrompt::Blocks(blocks)) => {
                assert_eq!(blocks.len(), 2, "应为 stable + volatile 两个 block");
                assert_eq!(blocks[0].block.text, "STABLE_BODY");
                assert!(
                    blocks[0].block.cache_control.is_some(),
                    "stable block 必须带 cache_control"
                );
                assert_eq!(blocks[1].block.text, "VOLATILE_TIME");
                assert!(
                    blocks[1].block.cache_control.is_none(),
                    "volatile block 不得带 cache_control"
                );
            }
            other => panic!("期望 SystemPrompt::Blocks，实际: {other:?}"),
        }
    }

    /// prompt_caching=false 时，stable block 也不打 cache_control（向后兼容）。
    #[test]
    fn no_cache_control_when_prompt_caching_disabled() {
        let seg = SystemPromptSegments {
            stable: "S".to_string(),
            volatile: Some("V".to_string()),
        };
        let params = build_message_params(
            "claude-test",
            1024,
            vec![],
            vec![],
            Some(seg),
            None,
            None,
            None,
            false, // prompt_caching = false
            None,
            None,
            false,
            None,
            None,
            None,
        );
        match params.system {
            Some(SystemPrompt::Blocks(blocks)) => {
                assert_eq!(blocks.len(), 2);
                assert!(
                    blocks.iter().all(|b| b.block.cache_control.is_none()),
                    "prompt_caching=false 时所有 block 都不得带 cache_control"
                );
            }
            other => panic!("期望 Blocks，实际: {other:?}"),
        }
    }

    /// volatile=None 时只产出单个 stable block。
    #[test]
    fn volatile_none_yields_single_stable_block() {
        let seg = SystemPromptSegments {
            stable: "S".to_string(),
            volatile: None,
        };
        let params = build_message_params(
            "claude-test",
            1024,
            vec![],
            vec![],
            Some(seg),
            None,
            None,
            None,
            true,
            None,
            None,
            false,
            None,
            None,
            None,
        );
        match params.system {
            Some(SystemPrompt::Blocks(blocks)) => {
                assert_eq!(blocks.len(), 1, "volatile=None 时只有 stable 一个 block");
                assert!(blocks[0].block.cache_control.is_some());
            }
            other => panic!("期望 Blocks，实际: {other:?}"),
        }
    }
}
