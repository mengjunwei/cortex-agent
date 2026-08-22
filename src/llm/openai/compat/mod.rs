//! 自定义 OpenAI Compatible 实现 — 加强版 ToolCallBuffer
//!
//! 抄自 adk-model `OpenAICompatible`，唯一区别：
//! 自定义 `ToolCallBuffer` 的 `has_partial_prefix()` 最小匹配长度从 1 提升到 3，
//! 避免单字符 `<` / `[` 误触发缓冲导致流式内容截断。
//! 解析逻辑复用 ADK 的 `parse_text_tool_calls`。

use std::collections::HashMap;
use std::sync::LazyLock;

use adk_rust::model::openai::ReasoningEffort;
use adk_rust::model::retry::{RetryConfig, execute_with_retry, is_retryable_model_error};
use adk_rust::model::usage_tracking::with_usage_tracking;
use adk_rust::telemetry;
use adk_rust::{
    AdkError, Content, ErrorCategory, ErrorComponent, FinishReason, GenericSchemaAdapter, Llm,
    LlmRequest, LlmResponse, LlmResponseStream, Part, SchemaAdapter, SchemaCache, UsageMetadata,
};
use async_openai::types::chat::{
    ChatCompletionMessageToolCall, ChatCompletionMessageToolCalls,
    ChatCompletionRequestAssistantMessageArgs, ChatCompletionRequestMessage,
    ChatCompletionRequestMessageContentPartImage, ChatCompletionRequestMessageContentPartText,
    ChatCompletionRequestSystemMessageArgs, ChatCompletionRequestToolMessageArgs,
    ChatCompletionRequestUserMessageArgs, ChatCompletionRequestUserMessageContent,
    ChatCompletionRequestUserMessageContentPart, ChatCompletionTool, ChatCompletionTools,
    CreateChatCompletionRequestArgs, FunctionCall, FunctionObject, ImageDetail, ImageUrl,
    ResponseFormat, ResponseFormatJsonSchema,
};
use async_stream::try_stream;
use async_trait::async_trait;
use base64::Engine as _;
use futures::StreamExt;
use serde::{Deserialize, Serialize};

// ========================================================================
//  配置 & 结构体
// ========================================================================

/// OpenAI Compatible 配置（对齐 adk-model OpenAICompatibleConfig）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAICustomCompatibleConfig {
    /// Provider 显示名（用于错误信息和 telemetry）
    pub provider_name: String,
    /// API key
    pub api_key: String,
    /// 模型名
    pub model: String,
    /// API base URL
    pub base_url: String,
    /// 可选 organization ID（OpenAI 多组织场景）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub organization_id: Option<String>,
    /// 可选 project ID
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    /// 可选 reasoning effort（OpenAI o-series 推理强度）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ReasoningEffort>,
}

impl OpenAICustomCompatibleConfig {
    /// 创建配置
    pub fn new(
        api_key: impl Into<String>,
        model: impl Into<String>,
        base_url: impl Into<String>,
    ) -> Self {
        Self {
            provider_name: "openai-custom".to_string(),
            api_key: api_key.into(),
            model: model.into(),
            base_url: base_url.into(),
            organization_id: None,
            project_id: None,
            reasoning_effort: None,
        }
    }

    /// 设置 provider 显示名
    pub fn with_provider_name(mut self, name: impl Into<String>) -> Self {
        self.provider_name = name.into();
        self
    }

    /// 设置 organization ID
    pub fn with_organization(mut self, org_id: impl Into<String>) -> Self {
        self.organization_id = Some(org_id.into());
        self
    }

    /// 设置 reasoning effort
    pub fn with_reasoning_effort(mut self, effort: ReasoningEffort) -> Self {
        self.reasoning_effort = Some(effort);
        self
    }
}

// ========================================================================
//  自定义 ToolCallBuffer — 加强版过滤规则
//  与 ADK 原版的唯一区别：has_partial_prefix() 最小匹配长度从 1 提升到 3，
//  避免单字符 `<` / `[` 误触发缓冲导致流式内容截断。
//  解析逻辑复用 ADK 的 parse_text_tool_calls。
// ========================================================================

/// 最小前缀匹配长度（ADK 原版为 1，这里提升到 3）
const MIN_PREFIX_LEN: usize = 3;

/// 工具调用前缀列表（与 ADK 原版一致）
const TOOL_CALL_PREFIXES: &[&str] = &[
    "<tool_call",
    "<|tool_call>",
    "<|python_tag|",
    "[TOOL_CALLS]",
    "<|action_start|>",
    "<\u{ff5c}\u{2581}tool", // <｜tool (DeepSeek full-width)
];

/// 缓冲区大小上限（安全阀，与 ADK 原版一致）
const MAX_BUFFER_SIZE: usize = 4096;

/// 自定义流式缓冲区
struct ToolCallBuffer {
    buffer: String,
    buffering: bool,
}

/// push() 返回的动作
enum BufferAction {
    /// 立即输出这些 parts
    Emit(Vec<Part>),
    /// 仍在累积中
    Buffering,
}

impl ToolCallBuffer {
    fn new() -> Self {
        Self {
            buffer: String::new(),
            buffering: false,
        }
    }

    /// 推入文本 chunk，返回动作
    fn push(&mut self, text: &str) -> BufferAction {
        self.buffer.push_str(text);

        if self.buffering {
            // 检查是否已有完整的工具调用
            if self.has_complete_tool_call() {
                return self.try_parse_and_emit();
            }
            // 安全阀：缓冲区过大，直接 flush
            if self.buffer.len() > MAX_BUFFER_SIZE {
                return self.flush_as_emit();
            }
            BufferAction::Buffering
        } else {
            // 检查是否出现完整前缀
            if self.starts_tool_call_prefix() {
                self.buffering = true;
                if self.has_complete_tool_call() {
                    return self.try_parse_and_emit();
                }
                BufferAction::Buffering
            } else if self.has_partial_prefix() {
                // 可能是跨 chunk 的前缀（如 "<tool" 然后 "_call>"）
                self.buffering = true;
                BufferAction::Buffering
            } else {
                // 普通文本 — 直接输出
                self.flush_as_emit()
            }
        }
    }

    /// 流结束时 flush 剩余内容
    fn flush(&mut self) -> Vec<Part> {
        if self.buffer.is_empty() {
            return Vec::new();
        }

        // 最后尝试解析为工具调用
        if let Some(parts) = adk_rust::model::tool_call_parser::parse_text_tool_calls(&self.buffer)
        {
            self.buffer.clear();
            self.buffering = false;
            return parts;
        }

        // 否则作为文本输出
        let text = std::mem::take(&mut self.buffer);
        self.buffering = false;
        if text.is_empty() {
            Vec::new()
        } else {
            vec![Part::Text { text }]
        }
    }

    fn starts_tool_call_prefix(&self) -> bool {
        TOOL_CALL_PREFIXES
            .iter()
            .any(|prefix| self.buffer.contains(prefix))
    }

    /// ★ 核心修改：最小匹配长度从 1 提升到 MIN_PREFIX_LEN(3)
    ///
    /// ADK 原版用 `for i in 1..` 导致单字符 `<` / `[` 就触发缓冲。
    /// 这里改为 `for i in MIN_PREFIX_LEN..`，只有至少匹配到
    /// `<to` / `[TO` / `<|t` 等才触发，避免普通文本被误缓冲。
    fn has_partial_prefix(&self) -> bool {
        let buf = &self.buffer;
        for prefix in TOOL_CALL_PREFIXES {
            let prefix_chars: Vec<char> = prefix.chars().collect();
            // ★ 关键修改：从 MIN_PREFIX_LEN 开始（原版为 1）
            for i in MIN_PREFIX_LEN..prefix_chars.len() {
                let partial: String = prefix_chars[..i].iter().collect();
                if buf.ends_with(&partial) {
                    return true;
                }
            }
        }
        false
    }

    fn has_complete_tool_call(&self) -> bool {
        (self.buffer.contains("<tool_call>") && self.buffer.contains("</tool_call>"))
            || (self.buffer.contains("<|tool_call>") && self.buffer.contains("<tool_call|>"))
            || (self.buffer.contains("<|python_tag|>")
                && self.buffer.contains('\n')
                && self.buffer.len() > "<|python_tag|>".len() + 5)
            || (self.buffer.contains("[TOOL_CALLS]")
                && self.buffer.contains(']')
                && self.buffer.rfind(']') > self.buffer.find("[TOOL_CALLS]").map(|i| i + 12))
            || (self.buffer.contains("```json") && self.buffer.matches("```").count() >= 2)
            || (self.buffer.contains("<|action_start|>") && self.buffer.contains("<|action_end|>"))
    }

    fn try_parse_and_emit(&mut self) -> BufferAction {
        if let Some(parts) = adk_rust::model::tool_call_parser::parse_text_tool_calls(&self.buffer)
        {
            self.buffer.clear();
            self.buffering = false;
            BufferAction::Emit(parts)
        } else {
            self.flush_as_emit()
        }
    }

    fn flush_as_emit(&mut self) -> BufferAction {
        let text = std::mem::take(&mut self.buffer);
        self.buffering = false;
        if text.is_empty() {
            BufferAction::Emit(Vec::new())
        } else {
            BufferAction::Emit(vec![Part::Text { text }])
        }
    }
}

/// 自定义 OpenAI Compatible 客户端 — 直接调用 API，不经过 ADK ToolCallBuffer。
///
/// `Clone` 供 openai_responses_auto 协商层在降级流中持有副本（字段全部廉价可克隆）。
#[derive(Clone)]
pub struct OpenAICustomCompatible {
    http: reqwest::Client,
    api_key: String,
    base_url: String,
    model: String,
    provider_name: String,
    retry_config: RetryConfig,
    reasoning_effort: Option<ReasoningEffort>,
    organization_id: Option<String>,
}

impl OpenAICustomCompatible {
    /// 创建客户端
    pub fn new(config: OpenAICustomCompatibleConfig) -> Self {
        Self {
            http: reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(30))
                .read_timeout(std::time::Duration::from_secs(120))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            api_key: config.api_key,
            base_url: config.base_url,
            model: config.model,
            provider_name: config.provider_name,
            retry_config: RetryConfig::default(),
            reasoning_effort: config.reasoning_effort,
            organization_id: config.organization_id,
        }
    }

    /// 设置 retry 配置（builder pattern）
    #[must_use]
    pub fn with_retry_config(mut self, retry_config: RetryConfig) -> Self {
        self.retry_config = retry_config;
        self
    }
}

// ========================================================================
//  辅助函数（抄自 adk-model openai/convert.rs + openai_compatible.rs）
//  原始模块为 pub(crate)，无法外部复用，因此内联到此处。
// ========================================================================

/// 序列化工具返回值（抄自 adk-model tool_result.rs）
fn serialize_tool_result(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Object(_) | serde_json::Value::Array(_) => {
            serde_json::to_string(value).unwrap_or_default()
        }
        other => other.to_string(),
    }
}

/// 从 parts 中提取文本（抄自 adk-model convert.rs）
fn extract_text(parts: &[Part]) -> String {
    parts
        .iter()
        .filter_map(|p| match p {
            Part::Text { text } => Some(text.clone()),
            Part::Thinking { thinking, .. } => Some(thinking.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 构造 user 消息：纯文本走 Text，含图片走 OpenAI Vision 的 Array(content parts)。
fn build_user_message(parts: &[Part]) -> ChatCompletionRequestMessage {
    let has_media = parts
        .iter()
        .any(|p| matches!(p, Part::InlineData { .. } | Part::FileData { .. }));

    if !has_media {
        let text = extract_text(parts);
        return ChatCompletionRequestUserMessageArgs::default()
            .content(ChatCompletionRequestUserMessageContent::Text(text))
            .build()
            .unwrap()
            .into();
    }

    let mut parts_out: Vec<ChatCompletionRequestUserMessageContentPart> = Vec::new();
    for p in parts {
        match p {
            Part::Text { text } => {
                parts_out.push(
                    ChatCompletionRequestMessageContentPartText { text: text.clone() }.into(),
                );
            }
            Part::Thinking { thinking, .. } => {
                parts_out.push(
                    ChatCompletionRequestMessageContentPartText {
                        text: thinking.clone(),
                    }
                    .into(),
                );
            }
            Part::InlineData {
                mime_type, data, ..
            } => {
                let b64 = base64::engine::general_purpose::STANDARD.encode(data);
                parts_out.push(image_content_part(&format!(
                    "data:{mime_type};base64,{b64}"
                )));
            }
            Part::FileData {
                mime_type: _,
                file_uri,
                ..
            } => {
                // 外链（http(s)://），交给上游 LLM 拉取
                parts_out.push(image_content_part(file_uri));
            }
            _ => {}
        }
    }

    ChatCompletionRequestUserMessageArgs::default()
        .content(ChatCompletionRequestUserMessageContent::Array(parts_out))
        .build()
        .unwrap()
        .into()
}

/// 构造 OpenAI Vision 的 image_url content part
fn image_content_part(url: &str) -> ChatCompletionRequestUserMessageContentPart {
    ChatCompletionRequestMessageContentPartImage {
        image_url: ImageUrl {
            url: url.to_string(),
            detail: Some(ImageDetail::Auto),
        },
    }
    .into()
}

/// ADK Content → OpenAI ChatCompletionRequestMessage
///
/// user 分支支持多模态：检测到 InlineData/FileData 时构造 OpenAI Vision 的 content 数组，
/// 把图片以 `data:image/xxx;base64,...` 形式内联（本地 /api/uploads/ 引用会被读成本地文件再转 base64）。
fn content_to_message(content: &Content) -> ChatCompletionRequestMessage {
    match content.role.as_str() {
        "user" => build_user_message(&content.parts),
        "model" | "assistant" => {
            let mut builder = ChatCompletionRequestAssistantMessageArgs::default();

            let text_content = extract_text(&content.parts);

            // 提取工具调用
            let tool_calls: Vec<_> = content
                .parts
                .iter()
                .filter_map(|part| {
                    if let Part::FunctionCall { name, args, id, .. } = part {
                        Some(ChatCompletionMessageToolCalls::Function(
                            ChatCompletionMessageToolCall {
                                id: id
                                    .clone()
                                    .unwrap_or_else(crate::llm::next_synthetic_call_id),
                                function: FunctionCall {
                                    name: name.clone(),
                                    arguments: serde_json::to_string(args).unwrap_or_default(),
                                },
                            },
                        ))
                    } else {
                        None
                    }
                })
                .collect();

            // OpenAI 要求 assistant 消息必须有 content 或 tool_calls
            if text_content.is_empty() && tool_calls.is_empty() {
                builder.content(" ".to_string());
            } else {
                if !text_content.is_empty() {
                    builder.content(text_content);
                }
                if !tool_calls.is_empty() {
                    builder.tool_calls(tool_calls);
                }
            }

            builder.build().unwrap().into()
        }
        "system" => {
            let text = extract_text(&content.parts);
            ChatCompletionRequestSystemMessageArgs::default()
                .content(text)
                .build()
                .unwrap()
                .into()
        }
        "function" | "tool" => {
            if let Some(Part::FunctionResponse {
                function_response,
                id,
                ..
            }) = content.parts.first()
            {
                let tool_call_id = id.clone().unwrap_or_else(|| "unknown".to_string());
                ChatCompletionRequestToolMessageArgs::default()
                    .tool_call_id(tool_call_id)
                    .content(serialize_tool_result(&function_response.response))
                    .build()
                    .unwrap()
                    .into()
            } else {
                ChatCompletionRequestUserMessageArgs::default()
                    .content(ChatCompletionRequestUserMessageContent::Text(String::new()))
                    .build()
                    .unwrap()
                    .into()
            }
        }
        _ => {
            let text = extract_text(&content.parts);
            ChatCompletionRequestUserMessageArgs::default()
                .content(ChatCompletionRequestUserMessageContent::Text(text))
                .build()
                .unwrap()
                .into()
        }
    }
}

/// ADK tools → OpenAI ChatCompletionTools（抄自 adk-model convert.rs）
fn convert_tools(
    tools: &HashMap<String, serde_json::Value>,
    adapter: &dyn SchemaAdapter,
    cache: &SchemaCache,
) -> Vec<ChatCompletionTools> {
    tools
        .iter()
        .map(|(name, decl)| {
            let description = decl
                .get("description")
                .and_then(|d| d.as_str())
                .map(String::from);
            let normalized_name = adapter.normalize_tool_name(name);
            let parameters = decl
                .get("parameters")
                .cloned()
                .map(|schema| cache.normalize(&schema))
                .or_else(|| Some(adapter.empty_schema()));

            ChatCompletionTools::Function(ChatCompletionTool {
                function: FunctionObject {
                    name: normalized_name.into_owned(),
                    description,
                    parameters,
                    strict: None,
                },
            })
        })
        .collect()
}

/// 构建 OpenAI Chat Completion 请求 JSON（抄自 adk-model openai_compatible.rs）
fn build_request_json(
    model: &str,
    request: &LlmRequest,
    reasoning_effort: &Option<ReasoningEffort>,
    adapter: &dyn SchemaAdapter,
    cache: &SchemaCache,
) -> Result<serde_json::Value, AdkError> {
    let messages: Vec<_> = request.contents.iter().map(content_to_message).collect();

    let mut request_builder = CreateChatCompletionRequestArgs::default();
    request_builder.model(model).messages(messages);

    if !request.tools.is_empty() {
        let tools = convert_tools(&request.tools, adapter, cache);
        request_builder.tools(tools);
        request_builder.parallel_tool_calls(true);
    }

    if let Some(config) = &request.config {
        if let Some(temp) = config.temperature {
            request_builder.temperature(temp);
        }
        if let Some(top_p) = config.top_p {
            request_builder.top_p(top_p);
        }
        if let Some(max_tokens) = config.max_output_tokens {
            request_builder.max_completion_tokens(max_tokens as u32);
        }
        // 重复惩罚：抑制 LLM token 级重复退化（degeneration），从源头降低死循环概率。
        // 对普通 chat 模型有效；推理模型思考模式可能被 API 静默忽略（无副作用）。
        if let Some(fp) = config.frequency_penalty {
            request_builder.frequency_penalty(fp);
        }
        if let Some(pp) = config.presence_penalty {
            request_builder.presence_penalty(pp);
        }

        if let Some(schema) = &config.response_schema {
            let mut schema_with_strict = schema.clone();
            if let Some(obj) = schema_with_strict.as_object_mut() {
                obj.insert("additionalProperties".to_string(), serde_json::json!(false));
            }
            // schema name 要求 ^[a-zA-Z0-9_-]{1,64}$：优先用真实模型名（部分调用方
            // 如 assistant_generator 以空 model 构造请求，空串会被严格网关 400），
            // 空/非常规时回退固定名（OpenAI 只要求 name 是标识符，不校验唯一性）
            let schema_name = {
                let raw = if request.model.is_empty() {
                    model
                } else {
                    &request.model
                };
                let sanitized: String = raw
                    .chars()
                    .map(|c| {
                        if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                            c
                        } else {
                            '_'
                        }
                    })
                    .collect();
                if sanitized.is_empty() {
                    "cortex_response_schema".to_string()
                } else {
                    sanitized.chars().take(64).collect()
                }
            };
            let json_schema = ResponseFormatJsonSchema {
                name: schema_name,
                description: None,
                // async-openai 0.41：schema 不再是 Option
                schema: schema_with_strict,
                strict: Some(true),
            };
            request_builder.response_format(ResponseFormat::JsonSchema { json_schema });
        }
    }

    let openai_request = request_builder
        .build()
        .map_err(|e| AdkError::model(format!("failed to build request: {e}")))?;

    let mut body = serde_json::to_value(&openai_request)
        .map_err(|e| AdkError::model(format!("failed to serialize request: {e}")))?;

    // f32→f64 精度修正：config 沿用 ADK 的 f32（0.9_f32 实为 0.899999976…），经
    // async-openai 转 f64 后序列化成一长串小数，会触发部分 API 的精度校验（如 GLM
    // 「top_p 限制小数点 2 位」HTTP 400）。在 JSON 层把这几个浮点字段规整到 2 位小数。
    if let Some(obj) = body.as_object_mut() {
        for k in [
            "temperature",
            "top_p",
            "frequency_penalty",
            "presence_penalty",
        ] {
            let rounded = obj
                .get(k)
                .and_then(|x| x.as_f64())
                .map(|v| (v * 100.0).round() / 100.0);
            if let Some(r) = rounded {
                obj.insert(k.to_string(), serde_json::json!(r));
            }
        }
    }

    // 移除 tool_choice 字段，兼容未启用 --enable-auto-tool-choice 的 vLLM/兼容端点
    // （如部分小米模型服务）。OpenAI 兼容 API 在不传 tool_choice 时默认行为即为 auto，
    // 模型仍可自主调用工具（含子智能体的 transfer_to_agent），功能不受影响。
    // 若用户在 config.extensions.openai 中显式指定了 tool_choice，后面会重新合并进来。
    if !request.tools.is_empty() {
        if let Some(body_obj) = body.as_object_mut() {
            body_obj.remove("tool_choice");
        }
    }

    // reasoning_effort 直接在 JSON 层面设置（绕过 builder 的 Clone 约束）
    if let Some(effort) = reasoning_effort
        && let Some(body_obj) = body.as_object_mut()
    {
        body_obj.insert(
            "reasoning_effort".to_string(),
            serde_json::to_value(effort).unwrap_or_default(),
        );
    }

    // 合并 provider-specific extensions
    if let Some(config) = &request.config
        && let Some(openai_ext) = config.extensions.get("openai")
    {
        if let (Some(body_obj), Some(ext_obj)) = (body.as_object_mut(), openai_ext.as_object()) {
            for (key, value) in ext_obj {
                body_obj.insert(key.clone(), value.clone());
            }
        }
    }

    // 【关键】extensions 可能用高精度浮点覆盖了上面的 top_p/temperature 等，合并完必须
    // 再规整一次，否则 glm 等 API 因 top_p 小数位 > 2 报 HTTP 400（"top_p参数非法：限制
    // 小数点[2]位"）。规整放最后，覆盖所有来源（config + extensions）。
    if let Some(obj) = body.as_object_mut() {
        for k in [
            "temperature",
            "top_p",
            "frequency_penalty",
            "presence_penalty",
        ] {
            let rounded = obj
                .get(k)
                .and_then(|x| x.as_f64())
                .map(|v| (v * 100.0).round() / 100.0);
            if let Some(r) = rounded {
                obj.insert(k.to_string(), serde_json::json!(r));
            }
        }
    }

    Ok(body)
}

/// 发送 HTTP 请求（抄自 adk-model openai_compatible.rs）
async fn send_request(
    http: &reqwest::Client,
    url: &str,
    api_key: &str,
    organization_id: &Option<String>,
    body: &serde_json::Value,
    provider_name: &str,
) -> Result<reqwest::Response, AdkError> {
    let mut http_req = http.post(url).bearer_auth(api_key).json(body);

    if let Some(org_id) = organization_id {
        http_req = http_req.header("OpenAI-Organization", org_id);
    }

    let http_resp = http_req.send().await.map_err(|e| {
        AdkError::new(
            ErrorComponent::Model,
            ErrorCategory::Unavailable,
            "model.openai_custom_compat.request",
            format!("{provider_name} request error: {e}"),
        )
        .with_provider(provider_name)
    })?;

    if !http_resp.status().is_success() {
        let status = http_resp.status();
        let status_code = status.as_u16();
        let body = http_resp.text().await.unwrap_or_default();
        let category = match status_code {
            401 => ErrorCategory::Unauthorized,
            403 => ErrorCategory::Forbidden,
            404 => ErrorCategory::NotFound,
            408 => ErrorCategory::Timeout,
            429 => ErrorCategory::RateLimited,
            503 | 529 => ErrorCategory::Unavailable,
            _ if status_code >= 500 => ErrorCategory::Internal,
            _ => ErrorCategory::InvalidInput,
        };

        // 检测 vLLM 未启用 function calling 的典型错误，给出可操作的中文提示
        let body_lower = body.to_lowercase();
        let is_tool_choice_error =
            body_lower.contains("tool choice") && body_lower.contains("enable-auto-tool-choice");
        let detail = if is_tool_choice_error {
            format!(
                "该模型/端点不支持函数调用（function calling）。\
当前编排使用了 transfer_to_agent 工具来路由子智能体，但上游服务未启用 --enable-auto-tool-choice。\n\n\
解决方法（任选其一）：\n\
1. 将助手的「编排方式」改为「智能路由 Router」——Router 使用意图分类，不依赖 function calling；\n\
2. 更换一个支持 function calling 的模型（如 GPT-4o、Claude 3.5、Qwen 等官方端点）。\n\n\
原始错误: {body}"
            )
        } else {
            format!("{provider_name} API error (HTTP {status}): {body}")
        };

        return Err(AdkError::new(
            ErrorComponent::Model,
            category,
            "model.openai_custom_compat.api_error",
            detail,
        )
        .with_upstream_status(status_code)
        .with_provider(provider_name));
    }

    Ok(http_resp)
}

fn parse_finish_reason(fr: &str) -> FinishReason {
    match fr {
        "stop" => FinishReason::Stop,
        "length" => FinishReason::MaxTokens,
        "tool_calls" => FinishReason::Stop,
        "content_filter" => FinishReason::Safety,
        "function_call" => FinishReason::Stop,
        _ => FinishReason::Stop,
    }
}

/// 解析 usage metadata（对齐 adk-model，含 audio token 统计）
fn parse_usage_from_chunk(chunk: &serde_json::Value) -> Option<UsageMetadata> {
    let u = chunk.get("usage")?;
    let prompt_tokens = u.get("prompt_tokens").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
    let completion_tokens = u
        .get("completion_tokens")
        .and_then(|v| v.as_i64())
        .unwrap_or(0) as i32;
    // 部分网关 total_tokens 恒 0、只填 prompt/completion：回退求和。否则上层
    // 「total>0 才采信」检查会把有效 usage 当占位丢弃 → 压缩闸门判定退回字符估算。
    let total_tokens = match u.get("total_tokens").and_then(|v| v.as_i64()) {
        Some(t) if t > 0 => t as i32,
        _ => prompt_tokens + completion_tokens,
    };

    let prompt_details = u.get("prompt_tokens_details");
    let completion_details = u.get("completion_tokens_details");

    Some(UsageMetadata {
        prompt_token_count: prompt_tokens,
        candidates_token_count: completion_tokens,
        total_token_count: total_tokens,
        cache_read_input_token_count: prompt_details
            .and_then(|d| d.get("cached_tokens"))
            .and_then(|v| v.as_i64())
            .map(|v| v as i32),
        thinking_token_count: completion_details
            .and_then(|d| d.get("reasoning_tokens"))
            .and_then(|v| v.as_i64())
            .map(|v| v as i32),
        audio_input_token_count: prompt_details
            .and_then(|d| d.get("audio_tokens"))
            .and_then(|v| v.as_i64())
            .map(|v| v as i32),
        audio_output_token_count: completion_details
            .and_then(|d| d.get("audio_tokens"))
            .and_then(|v| v.as_i64())
            .map(|v| v as i32),
        ..Default::default()
    })
}

/// 构造 LlmResponse（对齐 adk-model 字段）
#[allow(clippy::too_many_arguments)]
fn make_response(
    content: Option<Content>,
    usage_metadata: Option<UsageMetadata>,
    finish_reason: Option<FinishReason>,
    partial: bool,
    turn_complete: bool,
) -> LlmResponse {
    LlmResponse {
        content,
        usage_metadata,
        finish_reason,
        citation_metadata: None,
        partial,
        turn_complete,
        interrupted: false,
        error_code: None,
        error_message: None,
        provider_metadata: None,
        interaction_id: None,
    }
}

// ========================================================================
//  Llm trait 实现
// ========================================================================

static SCHEMA_CACHE: LazyLock<SchemaCache> = LazyLock::new(SchemaCache::new);

#[async_trait]
impl Llm for OpenAICustomCompatible {
    fn name(&self) -> &str {
        &self.model
    }

    async fn generate_content(
        &self,
        request: LlmRequest,
        stream: bool,
    ) -> Result<LlmResponseStream, AdkError> {
        let model = self.model.clone();
        let provider_name = self.provider_name.clone();
        let http = self.http.clone();
        let api_key = self.api_key.clone();
        let base_url = self.base_url.clone();
        let retry_config = self.retry_config.clone();
        let reasoning_effort = self.reasoning_effort;
        let organization_id = self.organization_id.clone();

        let adapter: &dyn SchemaAdapter = &GenericSchemaAdapter;
        let request_body =
            build_request_json(&model, &request, &reasoning_effort, adapter, &SCHEMA_CACHE)?;

        let usage_span = telemetry::llm_generate_span(&provider_name, &model, stream);

        if stream {
            // ── 流式路径（使用自定义 ToolCallBuffer） ────────────────
            let response_stream = try_stream! {
                let mut body = request_body.clone();
                if let Some(obj) = body.as_object_mut() {
                    obj.insert("stream".to_string(), serde_json::json!(true));
                    obj.insert(
                        "stream_options".to_string(),
                        serde_json::json!({"include_usage": true}),
                    );
                }

                let url = format!("{base_url}/chat/completions");

                // Retry 只覆盖初始 HTTP 请求，不覆盖流消费
                let response = execute_with_retry(&retry_config, is_retryable_model_error, || {
                    let http = http.clone();
                    let url = url.clone();
                    let api_key = api_key.clone();
                    let organization_id = organization_id.clone();
                    let body = body.clone();
                    let provider_name = provider_name.clone();
                    async move {
                        send_request(&http, &url, &api_key, &organization_id, &body, &provider_name).await
                    }
                })
                .await?;

                let mut byte_stream = response.bytes_stream();
                let mut buffer = String::new();
                let mut text_buffer = ToolCallBuffer::new();
                let mut tool_call_accumulators: HashMap<u32, (String, String, String)> =
                    HashMap::new();
                // 本次流是否已发出结束信号（turn_complete=true 或 finish_reason）。
                // provider 流式响应不发 finish_reason 时，靠它在流末尾补发，避免上层
                // cortex_agent 收不到结束信号而 continue 重调 LLM（死循环根因之一）。
                let mut ended = false;
                // 累积的 usage_metadata：OpenAI 流式 usage 在 finish_reason 之后单独一个
                // choices=[] 的 chunk 里，需跨 chunk 暂存，在 make_response 兜底填入。
                let mut pending_usage: Option<UsageMetadata> = None;
                // finish_reason 最终响应缓冲：usage chunk 在 finish 之后才到，立即 yield
                // 的话最终响应 usage=None（上层 cortex_agent 在 finish_reason/turn_complete
                // 处 break，事后补挂无人消费 → token 用量/CONTEXT_USAGE 全丢）。故先缓冲，
                // 读到流尾把 pending_usage 补挂后再 yield（见流末 flush 处注释）。
                let mut pending_final: Option<LlmResponse> = None;

                loop {
                    // finish 已到、只剩流尾 usage chunk：限时排空。个别网关不发 usage 也
                    // 不关连接时，避免无限拖住工具执行；超时按现状 yield，usage 缺席时
                    // 上层退化为字符估算（与修复前行为一致，不会更差）。
                    let next_chunk = if pending_final.is_some() {
                        match tokio::time::timeout(
                            std::time::Duration::from_secs(3),
                            byte_stream.next(),
                        )
                        .await
                        {
                            Ok(r) => r,
                            Err(_) => break,
                        }
                    } else {
                        byte_stream.next().await
                    };
                    let Some(chunk_result) = next_chunk else { break };
                    let chunk = chunk_result.map_err(|e| {
                        AdkError::model(format!("stream read error: {e}"))
                    })?;

                    buffer.push_str(&String::from_utf8_lossy(&chunk));

                    // 逐行处理 SSE
                    while let Some(line_end) = buffer.find('\n') {
                        let line = buffer[..line_end].trim().to_string();
                        buffer = buffer[line_end + 1..].to_string();

                        if line.is_empty() || line == "data: [DONE]" {
                            continue;
                        }

                        let Some(data) = line.strip_prefix("data: ") else {
                            continue;
                        };

                        let chunk_json: serde_json::Value = match serde_json::from_str(data) {
                            Ok(v) => v,
                            Err(e) => {
                                telemetry::warn!("failed to parse SSE chunk: {e} - {data}");
                                continue;
                            }
                        };

                        let Some(choice) = chunk_json.get("choices").and_then(|c| c.get(0)) else {
                            // OpenAI 流式：finish_reason 之后的最后一个 chunk choices=[] 但带 usage，
                            // 必须在此捕获，否则 token 用量永远拿不到。
                            if let Some(u) = parse_usage_from_chunk(&chunk_json) {
                                pending_usage = Some(u);
                            }
                            continue;
                        };
                        let Some(delta) = choice.get("delta") else {
                            continue;
                        };

                        let finish_reason_str = choice
                            .get("finish_reason")
                            .and_then(|v| v.as_str())
                            .map(String::from);

                        // 累积结构化 tool_calls
                        if let Some(tool_calls) =
                            delta.get("tool_calls").and_then(|v| v.as_array())
                        {
                            for tc in tool_calls {
                                let index =
                                    tc.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                                let entry = tool_call_accumulators
                                    .entry(index)
                                    .or_insert_with(|| {
                                        let call_id = tc
                                            .get("id")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("")
                                            .to_string();
                                        (call_id, String::new(), String::new())
                                    });

                                if let Some(id) = tc.get("id").and_then(|v| v.as_str()) && !id.is_empty() {
                                        entry.0 = id.to_string();
                                    }
                                if let Some(func) = tc.get("function") {
                                    if let Some(name) =
                                        func.get("name").and_then(|v| v.as_str())
                                    {
                                        entry.1 = name.to_string();
                                    }
                                    if let Some(args_chunk) =
                                        func.get("arguments").and_then(|v| v.as_str())
                                    {
                                        entry.2.push_str(args_chunk);
                                    }
                                }
                            }
                        }

                        // finish_reason → 缓冲最终响应（不立即 yield，等流尾 usage 补挂，
                        // 见 pending_final 声明处注释）
                        if let Some(ref fr) = finish_reason_str {
                            // 重复 finish chunk（病态流）不覆盖已缓冲的最终响应
                            if pending_final.is_some() {
                                continue;
                            }
                            ended = true; // provider 发了 finish_reason，本次流已有结束信号
                            let finish_reason = Some(parse_finish_reason(fr));
                            // usage 优先取本次 chunk 解析的，否则用流式后续累积的（OpenAI 末尾 chunk）
                            let usage_metadata = parse_usage_from_chunk(&chunk_json).or(pending_usage.take());

                            // 输出累积的工具调用
                            if !tool_call_accumulators.is_empty() {
                                let mut sorted_calls: Vec<_> =
                                    tool_call_accumulators.drain().collect();
                                sorted_calls.sort_by_key(|(idx, _)| *idx);

                                // MaxTokens 截断时，args 解析失败的 tool call 是残缺的——执行必报错，
                                // 会触发「截断 → 残缺工具 → 报错 → 模型重生成」死循环。故 MaxTokens 时
                                // 丢弃残缺的、仅 emit args 完整的；全残缺则按 MaxTokens 无 FC 正常结束。
                                let is_truncated =
                                    matches!(finish_reason, Some(FinishReason::MaxTokens));
                                let parts: Vec<Part> = sorted_calls
                                    .into_iter()
                                    .filter_map(|(_idx, (id, name, args_str))| {
                                        // 弱供应商可能全程不发 id（id 为空）：给 FC 补全局唯一合成 id，
                                        // 否则 cortex_agent 回填的 FR 拿到空 id，normalize 会把它当孤立
                                        // FR 误删 → 触发 400。用全局单调计数器而非 `call_{idx}`——后者
                                        // 跨轮/跨 run 重复，压缩拼接后 normalize 会混淆同 id 的旧新 FC。
                                        let call_id =
                                            if id.is_empty() { crate::llm::next_synthetic_call_id() } else { id };
                                        match serde_json::from_str::<serde_json::Value>(&args_str) {
                                            Ok(args) => Some(Part::FunctionCall {
                                                name,
                                                args,
                                                id: Some(call_id),
                                                thought_signature: None,
                                            }),
                                            Err(_) if is_truncated => {
                                                tracing::warn!(
                                                    "[openai-compat] MaxTokens 截断：丢弃残缺 tool call (name={name})，避免执行报错触发重生成循环"
                                                );
                                                None
                                            }
                                            Err(_) => Some(Part::FunctionCall {
                                                name,
                                                args: serde_json::json!({}),
                                                id: Some(call_id),
                                                thought_signature: None,
                                            }),
                                        }
                                    })
                                    .collect();

                                // parts 可能因残缺丢弃而变空：按 MaxTokens 正常结束（无 FC → 上层退出 loop）
                                let content = if parts.is_empty() {
                                    None
                                } else {
                                    Some(Content {
                                        role: "model".to_string(),
                                        parts,
                                    })
                                };
                                pending_final = Some(make_response(
                                    content,
                                    usage_metadata,
                                    finish_reason,
                                    false,
                                    true,
                                ));
                                continue;
                            }

                            // 无工具调用的最终响应
                            let mut parts = Vec::new();
                            if let Some(text) =
                                delta.get("content").and_then(|v| v.as_str())
                            {
                                if !text.is_empty() {
                                    parts.push(Part::Text {
                                        text: text.to_string(),
                                    });
                                }
                            }

                            pending_final = Some(make_response(
                                if parts.is_empty() {
                                    None
                                } else {
                                    Some(Content {
                                        role: "model".to_string(),
                                        parts,
                                    })
                                },
                                usage_metadata,
                                finish_reason,
                                false,
                                true,
                            ));
                            continue;
                        }

                        // 输出 reasoning_content → Part::Thinking
                        if let Some(reasoning) =
                            delta.get("reasoning_content").and_then(|v| v.as_str())
                        {
                            if !reasoning.is_empty() {
                                yield make_response(
                                    Some(Content {
                                        role: "model".to_string(),
                                        parts: vec![Part::Thinking {
                                            thinking: reasoning.to_string(),
                                            signature: None,
                                        }],
                                    }),
                                    None,
                                    None,
                                    true,
                                    false,
                                );
                            }
                        }

                        // ★ 使用自定义 ToolCallBuffer（加强版过滤规则）
                        // 与 ADK OpenAICompatible 的唯一区别：
                        // has_partial_prefix() 最小匹配长度从 1 提升到 3，
                        // 避免单字符 `<` / `[` 误触发缓冲导致流式内容截断。
                        if let Some(text) = delta.get("content").and_then(|v| v.as_str()) {
                            if !text.is_empty() {
                                match text_buffer.push(text) {
                                    BufferAction::Emit(parts) => {
                                        for part in parts {
                                            let is_tool = matches!(part, Part::FunctionCall { .. });
                                            yield make_response(
                                                Some(Content {
                                                    role: "model".to_string(),
                                                    parts: vec![part],
                                                }),
                                                None,
                                                None,
                                                !is_tool,  // partial: Text=true, FunctionCall=false
                                                false,     // turn_complete: always false
                                            );
                                        }
                                    }
                                    BufferAction::Buffering => { /* 累积中，不 emit */ }
                                }
                            }
                        }
                    }
                }

                // 流结束 — 先补挂 usage 并 yield finish_reason 缓冲的最终响应。
                // 必须放在 text_buffer.flush() 之前：上层 cortex_agent 在
                // finish_reason/turn_complete 帧处 break，若 flush 残余（含
                // turn_complete=true 的 FC 帧）先出，会再次提前触发 break，
                // 把这份带 usage 的最终响应丢掉——正是要修的 bug。
                if let Some(mut final_resp) = pending_final.take() {
                    if final_resp.usage_metadata.is_none() {
                        final_resp.usage_metadata = pending_usage.take();
                    }
                    yield final_resp;
                }

                // 流结束 — flush 文本缓冲区残余内容（对齐 ADK 原版逻辑）
                for part in text_buffer.flush() {
                    let is_tool = matches!(part, Part::FunctionCall { .. });
                    if is_tool { ended = true; } // FunctionCall flush 带 turn_complete=true 结束信号
                    yield make_response(
                        Some(Content {
                            role: "model".to_string(),
                            parts: vec![part],
                        }),
                        None,
                        if is_tool { Some(FinishReason::Stop) } else { None },
                        !is_tool,       // partial: Text=true, FunctionCall=false
                        is_tool,        // turn_complete: Text=false, FunctionCall=true
                    );
                }

                // 本次流若全程未发出任何结束信号（provider 未发 finish_reason，且 flush 仅有
                // 纯文本、无 FunctionCall），手动补发一个 turn_complete=true 的空结束信号，
                // 确保单次 LLM 调用语义完整。否则上层 cortex_agent 收不到 turn_complete /
                // finish_reason 会盲目 continue 重调 LLM，把重复文本喂回上下文形成死循环。
                // （原 `!emitted_final` 判据错在：只要 flush 出残余文本 emitted_final 即真，
                //  便跳过补发，而那批文本 chunk 本身并不携带任何结束信号。）
                if !ended {
                    yield make_response(
                        None,
                        None,
                        Some(FinishReason::Stop),
                        false,  // partial
                        true,   // turn_complete
                    );
                }
            };

            Ok(with_usage_tracking(Box::pin(response_stream), usage_span))
        } else {
            // ── 非流式路径 ──────────────────────────────────────────
            let response_stream = try_stream! {
                let response = execute_with_retry(&retry_config, is_retryable_model_error, || {
                    let model = model.clone();
                    let provider_name = provider_name.clone();
                    let http = http.clone();
                    let api_key = api_key.clone();
                    let base_url = base_url.clone();
                    let body = request_body.clone();
                    let organization_id = organization_id.clone();
                    async move {
                        let url = format!("{base_url}/chat/completions");
                        let http_resp =
                            send_request(&http, &url, &api_key, &organization_id, &body, &provider_name)
                                .await?;

                        let raw_json: serde_json::Value = http_resp.json().await.map_err(|e| {
                            AdkError::new(
                                ErrorComponent::Model,
                                ErrorCategory::Internal,
                                "model.openai_custom_compat.parse",
                                format!("{provider_name} response parse error: {e}"),
                            )
                            .with_provider(&provider_name)
                        })?;

                        telemetry::debug!(
                            provider = %provider_name,
                            model = %model,
                            has_reasoning = raw_json
                                .pointer("/choices/0/message/reasoning_content")
                                .is_some(),
                            "openai chat completion response"
                        );

                        Ok(raw_json)
                    }
                })
                .await?;

                // 解析完整响应（内联自 convert::from_raw_openai_response）
                let choice = response.get("choices").and_then(|c| c.get(0));

                let content = choice.map(|choice| {
                    let message = &choice["message"];
                    let mut parts = Vec::new();

                    // reasoning_content → Part::Thinking
                    if let Some(reasoning) =
                        message.get("reasoning_content").and_then(|v| v.as_str()) && !reasoning.is_empty() {
                            parts.push(Part::Thinking {
                                thinking: reasoning.to_string(),
                                signature: None,
                            });
                        };

                    // 正文文本（对齐 ADK：先尝试解析文本标签格式的工具调用）
                    if let Some(text) = message.get("content").and_then(|v| v.as_str()) {
                        if !text.is_empty() {
                            if let Some(parsed_parts) =
                                adk_rust::model::tool_call_parser::parse_text_tool_calls(text)
                            {
                                parts.extend(parsed_parts);
                            } else {
                                parts.push(Part::Text { text: text.to_string() });
                            }
                        }
                    }

                    // 结构化工具调用
                    if let Some(tool_calls) =
                        message.get("tool_calls").and_then(|v| v.as_array())
                    {
                        for tc in tool_calls {
                            let func = &tc["function"];
                            if let Some(name) = func.get("name").and_then(|n| n.as_str()) {
                                let args: serde_json::Value = func
                                    .get("arguments")
                                    .and_then(|a| a.as_str())
                                    .and_then(|a| serde_json::from_str(a).ok())
                                    .unwrap_or(serde_json::json!({}));
                                let id =
                                    tc.get("id").and_then(|i| i.as_str()).map(String::from);
                                parts.push(Part::FunctionCall {
                                    name: name.to_string(),
                                    args,
                                    id,
                                    thought_signature: None,
                                });
                            }
                        }
                    }

                    Content {
                        role: "model".to_string(),
                        parts,
                    }
                });

                // usage（对齐 adk-model，复用 parse_usage_from_chunk）
                let usage_metadata = parse_usage_from_chunk(&response);

                // finish_reason
                let finish_reason = choice
                    .and_then(|c| c.get("finish_reason"))
                    .and_then(|v| v.as_str())
                    .map(parse_finish_reason);

                yield make_response(
                    content,
                    usage_metadata,
                    finish_reason,
                    false,
                    true,
                );
            };

            Ok(with_usage_tracking(Box::pin(response_stream), usage_span))
        }
    }
}


#[cfg(test)]
mod tests;
