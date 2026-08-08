//! Anthropic client implementation.

use super::config::{AnthropicConfig, Effort};
use super::convert;
use super::error::AnthropicApiError;
use super::rate_limit::RateLimitInfo;
use super::schema_adapter::AnthropicSchemaAdapter;
use super::sse_stream;
use adk_anthropic::{
    Anthropic, ContentBlock, ContentBlockDelta, ContentBlockDeltaEvent, MessageStreamEvent,
    StopReason, TextDelta,
};
use adk_rust::model::retry::{
    RetryConfig, ServerRetryHint, execute_with_retry, is_retryable_model_error,
};
use adk_rust::{
    AdkError, ErrorCategory, ErrorComponent, FinishReason, Llm, LlmRequest, Part, SchemaAdapter,
    SchemaCache,
};
use async_stream::try_stream;
use async_trait::async_trait;
use futures::StreamExt;
use reqwest::header;
use std::pin::pin;
use std::sync::{Arc, LazyLock};
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::field;
use tracing::{Span, debug};

/// 流式请求专用 HTTP 客户端：不设总超时（流式响应可能很长），仅设连接超时；
/// chunk 间静默由 [`sse_stream`] 的 `CHUNK_TIMEOUT` 兜底。
static HTTP_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .pool_max_idle_per_host(10)
        .pool_idle_timeout(Duration::from_secs(90))
        .tcp_keepalive(Duration::from_secs(60))
        .build()
        .expect("failed to build anthropic_custom stream HTTP client")
});

/// Anthropic client for Claude models.
pub struct AnthropicClient {
    pub(super) client: Anthropic,
    pub(super) config: AnthropicConfig,
    pub(super) model: String,
    pub(super) max_tokens: u32,
    retry_config: RetryConfig,
    /// Latest rate-limit information from the most recent API response.
    latest_rate_limit: Arc<RwLock<RateLimitInfo>>,
}

impl AnthropicClient {
    /// Create a new Anthropic client.
    pub fn new(config: AnthropicConfig) -> Result<Self, AdkError> {
        let base_url = config.base_url.clone();
        let mut client = Anthropic::new(Some(config.api_key.clone()))
            .map_err(|e| AdkError::model(format!("Failed to create Anthropic client: {e}")))?;
        // ★ 修复 adk-model 1.0.0 的 base_url bug：原版建底层 Anthropic client 时只传 api_key，
        // 忽略 AnthropicConfig.base_url，永远打官方 api.anthropic.com。此处把数据库配置的
        // base_url 传给底层 client，使其打到中转地址。
        if let Some(url) = base_url {
            client = client.with_base_url(url)
                .map_err(|e| AdkError::model(format!("invalid base_url: {e}")))?;
        }

        Ok(Self {
            client,
            model: config.model.clone(),
            max_tokens: config.max_tokens,
            config,
            retry_config: RetryConfig::default(),
            latest_rate_limit: Arc::new(RwLock::new(RateLimitInfo::default())),
        })
    }

    /// Create a client with just an API key (uses default model).
    pub fn from_api_key(api_key: impl Into<String>) -> Result<Self, AdkError> {
        Self::new(AnthropicConfig::new(api_key, "claude-sonnet-4-6"))
    }

    /// Access the underlying `adk_anthropic::Anthropic` HTTP client.
    ///
    /// Use this for direct API access to endpoints not covered by the `Llm` trait:
    /// batches, files, skills, models, token counting, and pricing.
    ///
    /// ```rust,ignore
    /// let inner = anthropic_client.inner();
    /// let models = inner.list_models(None).await?;
    /// let batch = inner.create_batch(requests).await?;
    /// ```
    pub fn inner(&self) -> &adk_anthropic::Anthropic {
        &self.client
    }

    /// Access the current Anthropic configuration.
    pub fn anthropic_config(&self) -> &AnthropicConfig {
        &self.config
    }

    /// Set the retry configuration (builder pattern).
    #[must_use]
    pub fn with_retry_config(mut self, retry_config: RetryConfig) -> Self {
        self.retry_config = retry_config;
        self
    }

    /// Set the retry configuration (mutable reference).
    pub fn set_retry_config(&mut self, retry_config: RetryConfig) {
        self.retry_config = retry_config;
    }

    /// Returns the current retry configuration.
    pub fn retry_config(&self) -> &RetryConfig {
        &self.retry_config
    }

    /// Returns the latest rate-limit information from the most recent API response.
    ///
    /// Updated after each API call when the server provides rate-limit headers
    /// via `adk_anthropic::Error::RateLimit` or `adk_anthropic::Error::ServiceUnavailable`.
    /// Returns the default (all `None`) if no rate-limit info has been received.
    pub async fn latest_rate_limit_info(&self) -> RateLimitInfo {
        self.latest_rate_limit.read().await.clone()
    }

    pub(super) fn build_message_params(
        model: &str,
        max_tokens: u32,
        request: &LlmRequest,
        anthropic_config: &super::config::AnthropicConfig,
    ) -> Result<adk_anthropic::MessageCreateParams, AdkError> {
        let mut system_parts: Vec<String> = Vec::new();
        let mut messages = Vec::new();

        for content in &request.contents {
            if content.role == "system" {
                // Requirement 1.1: Extract system-role content text parts
                let text: String = content
                    .parts
                    .iter()
                    .filter_map(|p| match p {
                        Part::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                if !text.is_empty() {
                    system_parts.push(text);
                }
            } else {
                messages.push(convert::content_to_message(
                    content,
                    anthropic_config.prompt_caching,
                )?);
            }
        }

        // Requirement 1.2: Heuristic — re-route leading user-role text-only messages
        // to the system parameter when no explicit system-role content exists.
        // The agent layer injects instructions as role="user" before session history.
        // We detect consecutive user-only-text messages before the first assistant reply
        // and move them to the system parameter.
        if system_parts.is_empty() {
            let instruction_boundary = messages
                .iter()
                .position(|m| m.role == adk_anthropic::MessageRole::Assistant)
                .unwrap_or(0);

            if instruction_boundary > 0 {
                // Verify all leading messages are text-only user messages
                let all_text_only = messages[..instruction_boundary]
                    .iter()
                    .all(|m| m.role == adk_anthropic::MessageRole::User && is_text_only_message(m));

                if all_text_only {
                    let instruction_messages: Vec<_> =
                        messages.drain(..instruction_boundary).collect();
                    for msg in &instruction_messages {
                        if let Some(text) = extract_text_from_message(msg) {
                            if !text.is_empty() {
                                system_parts.push(text);
                            }
                        }
                    }
                }
            }
        }

        // 读 stable 边界：extensions["cortex"]["stable_system_count"]（由 CortexAgent 写入）。
        // 缺失时 split_system_segments 把全部 system 归 stable，向后兼容。
        let stable_count = request
            .config
            .as_ref()
            .and_then(|c| c.extensions.get("cortex"))
            .and_then(|v| v.get("stable_system_count"))
            .and_then(|v| v.as_u64())
            .map(|n| n as usize);
        let system_prompt = if system_parts.is_empty() {
            None
        } else {
            Some(split_system_segments(&system_parts, stable_count))
        };

        let mut tools = if request.tools.is_empty() {
            Vec::new()
        } else {
            // Requirement 19.3: When ToolSearchConfig is set, filter tools by regex pattern.
            // Requirement 19.4: When no ToolSearchConfig is set, load all tools.
            let filtered_tools = if let Some(ref tool_search) = anthropic_config.tool_search {
                request
                    .tools
                    .iter()
                    .filter(|(name, _)| tool_search.matches(name).unwrap_or(false))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect::<std::collections::HashMap<_, _>>()
            } else {
                request.tools.clone()
            };
            if filtered_tools.is_empty() {
                Vec::new()
            } else {
                use std::sync::LazyLock;
                static ADAPTER: AnthropicSchemaAdapter = AnthropicSchemaAdapter;
                static SCHEMA_CACHE: LazyLock<SchemaCache> = LazyLock::new(SchemaCache::new);
                convert::convert_tools(&filtered_tools, &ADAPTER, &SCHEMA_CACHE)?
            }
        };

        // Read extensions["anthropic"]["built_in_tools"] → append to tools array
        let config = request.config.as_ref();
        let anthropic_ext = config
            .and_then(|c| c.extensions.get("anthropic"))
            .and_then(|v| v.as_object());
        if let Some(built_in_tools) = anthropic_ext.and_then(|o| o.get("built_in_tools")) {
            if let Some(arr) = built_in_tools.as_array() {
                for (index, tool_value) in arr.iter().enumerate() {
                    let tool = serde_json::from_value::<adk_anthropic::ToolUnionParam>(
                        tool_value.clone(),
                    )
                    .map_err(|error| {
                        AdkError::new(
                            ErrorComponent::Model,
                            ErrorCategory::InvalidInput,
                            "model.anthropic.invalid_tool",
                            format!(
                                "failed to deserialize Anthropic built-in tool at index {index}: {error}"
                            ),
                        )
                        .with_provider("anthropic")
                    })?;
                    tools.push(tool);
                }
            }
        }

        // 思考级别优先取 request.config.extensions["anthropic"].effort（助手 thinking_level 透传），
        // 回退 client config 的 effort（目前始终 None）。
        let effort = request
            .config
            .as_ref()
            .and_then(|c| c.extensions.get("anthropic"))
            .and_then(|v| v.get("effort"))
            .and_then(|v| v.as_str())
            .and_then(|s| match s {
                "low" => Some(Effort::Low),
                "medium" => Some(Effort::Medium),
                "high" => Some(Effort::High),
                "xhigh" => Some(Effort::XHigh),
                "max" => Some(Effort::Max),
                _ => None,
            })
            .or(anthropic_config.effort);

        // Requirement 7.3: thinking 启用时 temperature 必须 1.0（Anthropic 规范）。
        // effort 会隐式启用 extended thinking，故 effort.is_some() 同样应锁 1.0——此前仅在
        // anthropic_config.thinking 显式设置时才锁，effort 路径下残留低温度（如 0.3）。而低温度
        // + thinking 的组合会加剧模型的确定性复读退化（低温度 = 高确定性 = 更易陷入重复循环）；
        // 锁 1.0 引入采样随机性，从源头降低陷入重复循环的概率。
        // 注意：不下发 thinking.budget_tokens——它是 legacy 字段，在 Opus 4.7 等新模型上会被拒绝
        // （见 config::ThinkingMode 文档）；thinking 深度统一由 effort（output_config.effort）控制。
        let thinking_enabled = anthropic_config.thinking.is_some() || effort.is_some();
        let temperature = if thinking_enabled {
            Some(1.0)
        } else {
            request.config.as_ref().and_then(|c| c.temperature)
        };
        let top_p = request.config.as_ref().and_then(|c| c.top_p);
        let top_k = request.config.as_ref().and_then(|c| c.top_k);
        let effective_max_tokens = request
            .config
            .as_ref()
            .and_then(|c| c.max_output_tokens)
            .map(|t| t as u32)
            .unwrap_or(max_tokens);

        // Merge consecutive messages with the same role.
        // This is critical for Anthropic parallel tool use — per the docs,
        // all tool results must be in a single user message. Without this,
        // Claude "learns to avoid parallel calls" from the conversation history.
        merge_consecutive_messages(&mut messages);

        Ok(convert::build_message_params(
            model,
            effective_max_tokens,
            messages,
            tools,
            system_prompt,
            temperature,
            top_p,
            top_k,
            anthropic_config.prompt_caching,
            anthropic_config.thinking.as_ref(),
            effort,
            anthropic_config.fast_mode,
            anthropic_config.inference_geo.as_deref(),
            anthropic_config.service_tier.as_deref(),
            anthropic_config.context_management.as_ref(),
        ))
    }

    /// 绕开 adk-anthropic `Anthropic::stream`（内部 `process_sse` 有 UTF-8 跨 chunk 丢字节
    /// bug）：仅负责「构造请求 → POST → 校验 HTTP 状态」，返回原始 [`reqwest::Response`]，
    /// 其字节流交由 [`sse_stream::parse`] 做健壮解析。错误类型保持 `adk_anthropic::Error`，
    /// 复用既有 `convert_anthropic_error` / `is_retryable_model_error` 链路，retry 语义不变。
    async fn post_stream(
        config: &AnthropicConfig,
        params: &adk_anthropic::MessageCreateParams,
    ) -> Result<reqwest::Response, adk_anthropic::Error> {
        let base = config
            .base_url
            .as_deref()
            .map(|u| u.trim_end_matches('/'))
            .unwrap_or("https://api.anthropic.com");
        let url = format!("{base}/v1/messages");

        // 复刻 adk stream() 的 anthropic-beta 拼接逻辑
        let mut betas: Vec<&str> = Vec::new();
        if params.requires_structured_outputs_beta() {
            betas.push("structured-outputs-2025-11-13");
        }
        if params.context_management.is_some() {
            betas.push("context-management-2025-06-27");
        }
        if params.speed.is_some() {
            betas.push("fast-mode-2026-02-01");
        }

        let mut req = HTTP_CLIENT
            .post(&url)
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "text/event-stream")
            .header("x-api-key", &config.api_key)
            .header("anthropic-version", "2023-06-01")
            .json(params);
        if !betas.is_empty() {
            req = req.header("anthropic-beta", betas.join(","));
        }

        let response = req.send().await.map_err(|e| {
            if e.is_timeout() {
                adk_anthropic::Error::timeout(format!("Request timed out: {e}"), Some(30.0))
            } else if e.is_connect() {
                adk_anthropic::Error::connection(
                    format!("Connection error: {e}"),
                    Some(Box::new(e)),
                )
            } else {
                adk_anthropic::Error::http_client(format!("Request failed: {e}"), Some(Box::new(e)))
            }
        })?;

        if !response.status().is_success() {
            return Err(Self::map_error_status(response).await);
        }
        Ok(response)
    }

    /// 照搬 adk `process_error_response` 的状态码 → `Error` 映射，保证 retry/backoff 语义一致。
    async fn map_error_status(response: reqwest::Response) -> adk_anthropic::Error {
        let status_code = response.status().as_u16();
        let request_id = response
            .headers()
            .get("x-request-id")
            .and_then(|v| v.to_str().ok())
            .map(String::from);
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok());

        #[derive(serde::Deserialize)]
        struct ErrorResponse {
            error: Option<ErrorDetail>,
        }
        #[derive(serde::Deserialize)]
        struct ErrorDetail {
            #[serde(rename = "type")]
            error_type: Option<String>,
            message: Option<String>,
            param: Option<String>,
        }

        let body = match response.text().await {
            Ok(b) => b,
            Err(e) => {
                return adk_anthropic::Error::http_client(
                    format!("Failed to read error response: {e}"),
                    Some(Box::new(e)),
                );
            }
        };
        let parsed = serde_json::from_str::<ErrorResponse>(&body).ok();
        let detail = parsed.as_ref().and_then(|p| p.error.as_ref());
        let error_type = detail.and_then(|d| d.error_type.clone());
        let message = detail
            .and_then(|d| d.message.clone())
            .unwrap_or_else(|| body.clone());
        let param = detail.and_then(|d| d.param.clone());

        match status_code {
            400 => adk_anthropic::Error::bad_request(message, param),
            401 => adk_anthropic::Error::authentication(message),
            403 => adk_anthropic::Error::permission(message),
            404 => adk_anthropic::Error::not_found(message, None, None),
            408 => adk_anthropic::Error::timeout(message, None),
            429 => adk_anthropic::Error::rate_limit(message, retry_after),
            500 => adk_anthropic::Error::internal_server(message, request_id),
            502..=504 => adk_anthropic::Error::service_unavailable(message, retry_after),
            529 => adk_anthropic::Error::rate_limit(message, retry_after),
            _ => adk_anthropic::Error::api(status_code, error_type, message, request_id),
        }
    }
}

/// Check if a `MessageParam` contains only text content (no tool use, tool results, images, etc.).
fn is_text_only_message(msg: &adk_anthropic::MessageParam) -> bool {
    match &msg.content {
        adk_anthropic::MessageParamContent::String(_) => true,
        adk_anthropic::MessageParamContent::Array(blocks) => {
            !blocks.is_empty()
                && blocks
                    .iter()
                    .all(|block| matches!(block, ContentBlock::Text(_)))
        }
    }
}

/// Extract concatenated text from a `MessageParam`, returning `None` if empty.
fn extract_text_from_message(msg: &adk_anthropic::MessageParam) -> Option<String> {
    match &msg.content {
        adk_anthropic::MessageParamContent::String(s) => {
            if s.is_empty() {
                None
            } else {
                Some(s.clone())
            }
        }
        adk_anthropic::MessageParamContent::Array(blocks) => {
            let parts: Vec<&str> = blocks
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text(tb) if !tb.text.is_empty() => Some(tb.text.as_str()),
                    _ => None,
                })
                .collect();
            if parts.is_empty() {
                None
            } else {
                Some(parts.join("\n"))
            }
        }
    }
}

/// Merge consecutive `MessageParam`s that share the same role into a single message.
///
/// This is required for Anthropic parallel tool use. Per the
/// [Anthropic docs](https://docs.anthropic.com/en/docs/build-with-claude/tool-use/parallel-tool-use),
/// all tool results must be in a single user message. Without merging, each tool
/// result becomes a separate user message, which "teaches Claude to avoid parallel calls."
///
/// Zero-cost when messages already alternate roles correctly.
fn merge_consecutive_messages(messages: &mut Vec<adk_anthropic::MessageParam>) {
    if messages.len() < 2 {
        return;
    }

    let mut merged = Vec::with_capacity(messages.len());
    let mut drain = messages.drain(..);

    if let Some(first) = drain.next() {
        merged.push(first);
    }

    for msg in drain {
        let last = merged.last_mut().unwrap();
        if last.role == msg.role {
            // Same role — merge content blocks into the existing message
            let blocks = match std::mem::replace(
                &mut last.content,
                adk_anthropic::MessageParamContent::Array(Vec::new()),
            ) {
                adk_anthropic::MessageParamContent::String(s) => {
                    if s.is_empty() {
                        Vec::new()
                    } else {
                        vec![ContentBlock::Text(adk_anthropic::TextBlock::new(s))]
                    }
                }
                adk_anthropic::MessageParamContent::Array(blocks) => blocks,
            };

            let new_blocks = match msg.content {
                adk_anthropic::MessageParamContent::String(s) => {
                    if s.is_empty() {
                        Vec::new()
                    } else {
                        vec![ContentBlock::Text(adk_anthropic::TextBlock::new(s))]
                    }
                }
                adk_anthropic::MessageParamContent::Array(blocks) => blocks,
            };

            let mut combined = blocks;
            combined.extend(new_blocks);
            last.content = adk_anthropic::MessageParamContent::Array(combined);
        } else {
            merged.push(msg);
        }
    }

    *messages = merged;
}

/// Convert an `adk_anthropic::Error` into an [`AnthropicApiError`], preserving
/// structured context (error type, message, status code, request ID).
///
/// The resulting `AnthropicApiError` is then converted to `AdkError` via its
/// `From` impl. The request ID, when present, is also recorded on the current
/// tracing span as `anthropic.request_id` (Requirement 4.2).
pub(super) fn convert_anthropic_error(e: adk_anthropic::Error) -> AdkError {
    let api_error = to_anthropic_api_error(&e);

    // Requirement 4.2: record request-id on the active tracing span when present
    if let Some(ref rid) = api_error.request_id {
        Span::current().record("anthropic.request_id", rid.as_str());
    }

    // Requirement 11.4: Record error type and message as a span event on failure
    tracing::error!(
        error.type_ = %api_error.error_type,
        error.message = %api_error.message,
        error.status_code = api_error.status_code,
        "anthropic api error"
    );

    api_error.into()
}

/// Build an [`AnthropicApiError`] from an `adk_anthropic::Error`, extracting the
/// error type, message, HTTP status code, and request ID from whichever
/// variant is present.
///
/// Requirements 4.1, 4.2, 4.4: Parse structured error body fields and
/// capture the request-id header value.
fn to_anthropic_api_error(e: &adk_anthropic::Error) -> AnthropicApiError {
    match e {
        adk_anthropic::Error::Api {
            status_code,
            error_type,
            message,
            request_id,
        } => AnthropicApiError {
            error_type: error_type
                .clone()
                .unwrap_or_else(|| "api_error".to_string()),
            message: message.clone(),
            status_code: *status_code,
            request_id: request_id.clone(),
        },
        adk_anthropic::Error::RateLimit {
            message,
            retry_after,
        } => {
            let msg = match retry_after {
                Some(secs) => format!("{message} (retry-after: {secs}s)"),
                None => message.clone(),
            };
            AnthropicApiError {
                error_type: "rate_limit_error".to_string(),
                message: msg,
                status_code: 429,
                request_id: None,
            }
        }
        adk_anthropic::Error::ServiceUnavailable {
            message,
            retry_after,
        } => {
            let msg = match retry_after {
                Some(secs) => format!("{message} (retry-after: {secs}s)"),
                None => message.clone(),
            };
            AnthropicApiError {
                error_type: "overloaded_error".to_string(),
                message: msg,
                status_code: 529,
                request_id: None,
            }
        }
        adk_anthropic::Error::Authentication { message } => AnthropicApiError {
            error_type: "authentication_error".to_string(),
            message: message.clone(),
            status_code: 401,
            request_id: None,
        },
        adk_anthropic::Error::Permission { message } => AnthropicApiError {
            error_type: "permission_error".to_string(),
            message: message.clone(),
            status_code: 403,
            request_id: None,
        },
        adk_anthropic::Error::NotFound { message, .. } => AnthropicApiError {
            error_type: "not_found_error".to_string(),
            message: message.clone(),
            status_code: 404,
            request_id: None,
        },
        adk_anthropic::Error::BadRequest { message, .. } => AnthropicApiError {
            error_type: "invalid_request_error".to_string(),
            message: message.clone(),
            status_code: 400,
            request_id: None,
        },
        adk_anthropic::Error::InternalServer {
            message,
            request_id,
        } => AnthropicApiError {
            error_type: "api_error".to_string(),
            message: message.clone(),
            status_code: 500,
            request_id: request_id.clone(),
        },
        // All other adk_anthropic error variants (Connection, Timeout, Serialization, etc.)
        // are client-side errors without structured API error bodies.
        other => AnthropicApiError {
            error_type: "client_error".to_string(),
            message: format!("{other}"),
            status_code: 0,
            request_id: None,
        },
    }
}

/// Extract a [`ServerRetryHint`] from an `adk_anthropic::Error`, if the error
/// contains a server-provided `retry_after` value.
#[allow(dead_code)]
fn extract_retry_hint(e: &adk_anthropic::Error) -> Option<ServerRetryHint> {
    match e {
        adk_anthropic::Error::RateLimit {
            retry_after: Some(secs),
            ..
        }
        | adk_anthropic::Error::ServiceUnavailable {
            retry_after: Some(secs),
            ..
        } => Some(ServerRetryHint {
            retry_after: Some(std::time::Duration::from_secs(*secs)),
        }),
        _ => None,
    }
}

#[async_trait]
impl Llm for AnthropicClient {
    fn name(&self) -> &str {
        &self.model
    }

    fn schema_adapter(&self) -> &dyn SchemaAdapter {
        static ADAPTER: AnthropicSchemaAdapter = AnthropicSchemaAdapter;
        &ADAPTER
    }

    #[tracing::instrument(
        skip_all,
        fields(
            anthropic.model = %self.model,
            anthropic.request_type = if stream { "stream" } else { "unary" },
            anthropic.request_id = field::Empty,
        )
    )]
    async fn generate_content(
        &self,
        request: LlmRequest,
        stream: bool,
    ) -> Result<adk_rust::LlmResponseStream, AdkError> {
        let usage_span = adk_rust::telemetry::llm_generate_span("anthropic", &self.model, stream);
        let model = self.model.clone();
        let max_tokens = self.max_tokens;
        let client = self.client.clone();
        let retry_config = self.retry_config.clone();
        let request_for_retry = request.clone();
        let anthropic_config = self.config.clone();

        let response_stream = try_stream! {
            if stream {
                // Streaming mode
                let model_ref = model.as_str();
                // ★ 绕开 adk-anthropic 1.0.0 `Anthropic::stream` 内部 `process_sse` 的
                // UTF-8 跨 chunk 丢字节 bug（TCP 分片切在中文等多字节字符中间时，valid_up_to
                // 之后的不完整尾部被 continue 丢弃 → buffer 错位 → missing newline separator
                // → 整条流中断、agent 无输出）。改为 post_stream 拿原始字节流，交给
                // sse_stream::parse 用 Vec<u8> 字节累积解析，根治分包切断。
                let response = execute_with_retry(&retry_config, is_retryable_model_error, || {
                    let request = request_for_retry.clone();
                    let cfg = &anthropic_config;
                    async move {
                        let mut params = Self::build_message_params(model_ref, max_tokens, &request, cfg)?;
                        params.stream = true;
                        let resp = Self::post_stream(cfg, &params)
                            .await
                            .map_err(convert_anthropic_error)?;
                        Ok(resp)
                    }
                })
                .await?;

                let event_stream = sse_stream::parse(response.bytes_stream());

                // Pin the stream for iteration
                let mut pinned_stream = pin!(event_stream);

                // Track tool calls being built
                let mut current_tool_calls: Vec<(String, String, String)> = Vec::new(); // (id, name, args_json)
                let mut current_tool_index: Option<usize> = None;
                let mut pending_server_parts: Vec<Part> = Vec::new();

                // Track usage from MessageStart for propagation to final MessageDelta
                let mut stream_input_tokens: i32 = 0;
                let mut stream_cache_read_tokens: Option<i32> = None;
                let mut stream_cache_creation_tokens: Option<i32> = None;

                while let Some(event_result) = pinned_stream.next().await {
                    // Requirement 3.4: Handle error events from the stream.
                    // The adk-anthropic SSE parser converts `event: error` into stream Err values
                    // with structured error info. We emit these as LlmResponse with error fields
                    // rather than propagating as AdkError.
                    let event = match event_result {
                        Ok(ev) => ev,
                        Err(ref e) => {
                            // Requirement 4.2: extract request-id from stream errors
                            let api_err = to_anthropic_api_error(e);
                            if let Some(ref rid) = api_err.request_id {
                                Span::current().record("anthropic.request_id", rid.as_str());
                            }
                            // Requirement 11.4: Record error details as a span event
                            tracing::error!(
                                error.type_ = %api_err.error_type,
                                error.message = %api_err.message,
                                error.status_code = api_err.status_code,
                                "anthropic stream error"
                            );
                            yield convert::from_stream_error(&api_err.error_type, &api_err.message);
                            continue;
                        }
                    };

                    match event {
                        MessageStreamEvent::ContentBlockStart(start_event) => {
                            // Check if this is a tool_use block
                            let index = start_event.index;
                            match start_event.content_block {
                                ContentBlock::ToolUse(tool_use) => {
                                    current_tool_index = Some(index);
                                    while current_tool_calls.len() <= index {
                                        current_tool_calls
                                            .push((String::new(), String::new(), String::new()));
                                    }
                                    current_tool_calls[index] = (
                                        tool_use.id.clone(),
                                        tool_use.name.clone(),
                                        String::new(),
                                    );
                                }
                                ContentBlock::ServerToolUse(server_tool_use) => {
                                    if let Ok(val) = serde_json::to_value(server_tool_use) {
                                        pending_server_parts
                                            .push(Part::ServerToolCall { server_tool_call: val });
                                    }
                                }
                                ContentBlock::WebSearchToolResult(web_search_result) => {
                                    if let Ok(val) = serde_json::to_value(web_search_result) {
                                        pending_server_parts.push(Part::ServerToolResponse {
                                            server_tool_response: val,
                                        });
                                    }
                                }
                                _ => {}
                            }
                        }
                        MessageStreamEvent::ContentBlockDelta(ContentBlockDeltaEvent { index, delta }) => {
                            match delta {
                                ContentBlockDelta::TextDelta(TextDelta { text }) => {
                                    if !text.is_empty() {
                                        yield convert::from_text_delta(&text);
                                    }
                                }
                                ContentBlockDelta::InputJsonDelta(json_delta) => {
                                    // Accumulate tool call arguments
                                    if let Some(idx) = current_tool_index {
                                        if idx < current_tool_calls.len() {
                                            current_tool_calls[idx].2.push_str(&json_delta.partial_json);
                                        }
                                    } else if index < current_tool_calls.len() {
                                        current_tool_calls[index].2.push_str(&json_delta.partial_json);
                                    }
                                }
                                // Requirement 3.1: Emit thinking deltas wrapped in <thinking> tags
                                ContentBlockDelta::ThinkingDelta(td) => {
                                    if !td.thinking.is_empty() {
                                        yield convert::from_thinking_delta(&td.thinking);
                                    }
                                }
                                // Requirement 3.2: Accumulate signature deltas silently
                                ContentBlockDelta::SignatureDelta(_) => {}
                                // Requirement 3.5: Log unrecognized deltas at debug level
                                ContentBlockDelta::CitationsDelta(cd) => {
                                    debug!(?cd, "citations delta received (not yet mapped)");
                                }
                            }
                        }
                        MessageStreamEvent::ContentBlockStop { .. } => {
                            current_tool_index = None;
                        }
                        MessageStreamEvent::MessageDelta(delta_event) => {
                            // Check for stop reason
                            if let Some(stop_reason) = &delta_event.delta.stop_reason {
                                let finish_reason = match stop_reason {
                                    StopReason::EndTurn => Some(FinishReason::Stop),
                                    StopReason::MaxTokens => Some(FinishReason::MaxTokens),
                                    StopReason::StopSequence => Some(FinishReason::Stop),
                                    StopReason::ToolUse => Some(FinishReason::Stop),
                                    StopReason::PauseTurn => Some(FinishReason::Stop),
                                    StopReason::Refusal => Some(FinishReason::Safety),
                                    StopReason::PauseRun => Some(FinishReason::Stop),
                                    StopReason::ModelContextWindowExceeded => Some(FinishReason::MaxTokens),
                                };

                                // If we have accumulated tool calls, emit them
                                let mut parts = std::mem::take(&mut pending_server_parts);
                                if !current_tool_calls.is_empty() {
                                    // MaxTokens 截断时，args 解析失败的 tool call 是残缺的——执行必报错，
                                    // 会触发「截断 → 残缺工具 → 报错 → 模型重生成」死循环。故 MaxTokens 时
                                    // 丢弃残缺的、仅 emit args 完整的；全残缺则上层按 MaxTokens 无 FC 正常结束。
                                    let is_truncated =
                                        matches!(finish_reason, Some(FinishReason::MaxTokens));
                                    let tool_calls = current_tool_calls
                                        .drain(..)
                                        .filter(|(id, name, _)| !id.is_empty() && !name.is_empty())
                                        .filter_map(|(id, name, args_str)| {
                                            match serde_json::from_str::<serde_json::Value>(&args_str) {
                                                Ok(args) => Some(Part::FunctionCall {
                                                    name,
                                                    args,
                                                    id: Some(id),
                                                    thought_signature: None,
                                                }),
                                                Err(_) if is_truncated => {
                                                    tracing::warn!(
                                                        "[anthropic] MaxTokens 截断：丢弃残缺 tool call (name={name})，避免执行报错触发重生成循环"
                                                    );
                                                    None
                                                }
                                                Err(_) => Some(Part::FunctionCall {
                                                    name,
                                                    args: serde_json::json!({}),
                                                    id: Some(id),
                                                    thought_signature: None,
                                                }),
                                            }
                                        })
                                        .collect::<Vec<_>>();
                                    parts.extend(tool_calls);
                                }

                                if !parts.is_empty() {
                                    yield adk_rust::LlmResponse {
                                        content: Some(adk_rust::Content {
                                            role: "model".to_string(),
                                            parts,
                                        }),
                                        usage_metadata: Some(adk_rust::UsageMetadata {
                                            prompt_token_count: stream_input_tokens,
                                            candidates_token_count: delta_event.usage.output_tokens,
                                            total_token_count: stream_input_tokens + delta_event.usage.output_tokens,
                                            cache_read_input_token_count: stream_cache_read_tokens,
                                            cache_creation_input_token_count: stream_cache_creation_tokens,
                                            ..Default::default()
                                        }),
                                        finish_reason,
                                        citation_metadata: None,
                                        partial: false,
                                        turn_complete: true,
                                        interrupted: false,
                                        error_code: None,
                                        error_message: None,
                                        provider_metadata: None,
                                        interaction_id: None,
                                    };
                                    continue;
                                }

                                // Emit final message
                                yield adk_rust::LlmResponse {
                                    content: None,
                                    usage_metadata: Some(adk_rust::UsageMetadata {
                                        prompt_token_count: stream_input_tokens,
                                        candidates_token_count: delta_event.usage.output_tokens,
                                        total_token_count: stream_input_tokens + delta_event.usage.output_tokens,
                                        cache_read_input_token_count: stream_cache_read_tokens,
                                        cache_creation_input_token_count: stream_cache_creation_tokens,
                                        ..Default::default()
                                    }),
                                    finish_reason,
                                    citation_metadata: None,
                                    partial: false,
                                    turn_complete: true,
                                    interrupted: false,
                                    error_code: None,
                                    error_message: None,
                                    provider_metadata: None,
                                    interaction_id: None,
                                };
                            }
                        }
                        MessageStreamEvent::MessageStop(_) => {
                            // Stream complete
                        }
                        // Requirement 3.3: Treat ping as keep-alive no-op
                        MessageStreamEvent::Ping => {}
                        // Requirement 3.5: Log unrecognized events at debug level
                        MessageStreamEvent::MessageStart(start_event) => {
                            debug!("message_start event received");
                            // Store input tokens for the final UsageMetadata
                            stream_input_tokens = start_event.message.usage.input_tokens;
                            // Store cache token counts for propagation to the final MessageDelta
                            stream_cache_read_tokens = start_event.message.usage.cache_read_input_tokens;
                            stream_cache_creation_tokens = start_event.message.usage.cache_creation_input_tokens;
                            // Requirement 6.3: Extract cache usage from the initial message usage
                            let cache_meta = convert::extract_cache_usage(&start_event.message.usage);
                            if !cache_meta.is_empty() {
                                debug!(
                                    cache_creation = ?start_event.message.usage.cache_creation_input_tokens,
                                    cache_read = ?start_event.message.usage.cache_read_input_tokens,
                                    "cache usage tokens received in stream"
                                );
                            }
                        }
                        // New adk-anthropic event variants — log at debug level for now
                        MessageStreamEvent::ToolInputStart { .. }
                        | MessageStreamEvent::ToolInputDelta { .. }
                        | MessageStreamEvent::CompactionEvent(_)
                        | MessageStreamEvent::StreamError { .. } => {
                            debug!("unhandled stream event variant received");
                        }
                    }
                }
            } else {
                // Non-streaming mode
                let client_ref = &client;
                let model_ref = model.as_str();
                let message = execute_with_retry(&retry_config, is_retryable_model_error, || {
                    let request = request_for_retry.clone();
                    let cfg = &anthropic_config;
                    async move {
                        let params = Self::build_message_params(model_ref, max_tokens, &request, cfg)?;
                        client_ref
                            .send(params)
                            .await
                            .map_err(convert_anthropic_error)
                    }
                })
                .await?;

                // Requirement 4.3: On success, propagate request-id to tracing span.
                // The adk-anthropic crate does not expose the raw `request-id` response
                // header on successful responses, but the message `id` field
                // (e.g. "msg_...") serves as the primary correlation identifier.
                // When adk-anthropic adds header access, this will be updated to use
                // the actual `request-id` header value.
                Span::current().record("anthropic.request_id", message.id.as_str());

                // Requirement 6.3: Extract cache usage tokens into provider metadata
                let (_response, _cache_metadata) = convert::from_anthropic_message(&message);

                yield convert::from_anthropic_message(&message).0;
            }
        };

        Ok(adk_rust::model::usage_tracking::with_usage_tracking(
            Box::pin(response_stream),
            usage_span,
        ))
    }
}

/// 按 `stable_count` 把 system 文本段分成 stable / volatile。
///
/// stable 段（前 `stable_count` 条）打 cache_control 命中缓存；volatile 段（剩余，如时间）
/// 不打、每次刷新。`stable_count` 缺失 → 全部归 stable（volatile=None），向后兼容旧行为。
fn split_system_segments(
    parts: &[String],
    stable_count: Option<usize>,
) -> convert::SystemPromptSegments {
    let n = stable_count.unwrap_or(parts.len()).min(parts.len());
    let stable = parts[..n].join("\n");
    let volatile = if n < parts.len() {
        let v = parts[n..].join("\n");
        if v.is_empty() { None } else { Some(v) }
    } else {
        None
    };
    convert::SystemPromptSegments { stable, volatile }
}

#[cfg(test)]
mod prompt_cache_tests {
    use super::*;

    #[test]
    fn split_with_stable_count_separates_segments() {
        let parts = vec!["STABLE".to_string(), "VOLATILE".to_string()];
        let seg = split_system_segments(&parts, Some(1));
        assert_eq!(seg.stable, "STABLE");
        assert_eq!(seg.volatile.as_deref(), Some("VOLATILE"));
    }

    #[test]
    fn split_missing_count_puts_all_in_stable() {
        // 向后兼容：count 缺失 → 全部归 stable，volatile=None（等价旧"整段"行为）
        let parts = vec!["A".to_string(), "B".to_string()];
        let seg = split_system_segments(&parts, None);
        assert_eq!(seg.stable, "A\nB");
        assert_eq!(seg.volatile, None);
    }

    #[test]
    fn split_count_exceeding_len_is_clamped() {
        let parts = vec!["A".to_string()];
        let seg = split_system_segments(&parts, Some(5));
        assert_eq!(seg.stable, "A");
        assert_eq!(seg.volatile, None);
    }
}
