//! MCP Toolset 适配层 — 把共享连接包装成 adk `Toolset` / `Tool`。
//!
//! 从 manager.rs 拆出:`McpManager::build_toolsets` 只为每个健康连接构造
//! [`ManagedMcpToolset`],list/execute 的协议细节(命名空间前缀、参数清理、
//! 截图工具图片块内联)全部收在本文件。

use std::sync::Arc;
use std::time::Duration;

use adk_rust::tool::{Tool, Toolset};
use adk_rust::{AdkError, ReadonlyContext, Result as AdkResult, ToolContext};
use async_trait::async_trait;
use rmcp::model::CallToolRequestParams;
use serde_json::Value;

use super::SharedClient;
use crate::domain::mcp::models::namespaced_tool_name;

// ============================== ManagedMcpToolset ==============================

/// 自定义 MCP Toolset：共享 `Arc<Mutex<RunningService>>`，
/// 实现 adk `Toolset` trait 并自动给工具名加 `mcp__{slug}__` 前缀。
///
/// 由于 `RunningService` 不实现 `Clone`，无法直接使用 adk_tool 的 `McpToolset`
/// （它按值持有 `RunningService`）。本实现用 `Arc<Mutex<>>` 共享连接，
/// 允许连接池（健康探测）和 Agent（工具执行）共享同一连接。
pub struct ManagedMcpToolset {
    client: SharedClient,
    slug: String,
    /// 单次工具调用超时（来自 McpServer.tool_timeout_secs，界面可配）
    tool_timeout: Duration,
}

impl ManagedMcpToolset {
    pub fn new(client: SharedClient, slug: String, tool_timeout: Duration) -> Self {
        Self {
            client,
            slug,
            tool_timeout,
        }
    }
}

#[async_trait]
impl Toolset for ManagedMcpToolset {
    fn name(&self) -> &str {
        &self.slug
    }

    async fn tools(&self, _ctx: Arc<dyn ReadonlyContext>) -> AdkResult<Vec<Arc<dyn Tool>>> {
        let running = self.client.lock().await;
        let mcp_tools = running
            .list_all_tools()
            .await
            .map_err(|e| AdkError::tool(format!("MCP list_all_tools 失败: {e}")))?;

        let slug = self.slug.clone();
        let client = self.client.clone();
        let tools = mcp_tools
            .into_iter()
            .map(|t| {
                let tool_name = t.name.to_string();
                let namespaced = namespaced_tool_name(&slug, &tool_name);
                let description = t
                    .description
                    .as_ref()
                    .map(|d| d.to_string())
                    .unwrap_or_default();
                let input_schema = Some(Value::Object(t.input_schema.as_ref().clone()));
                Arc::new(ManagedMcpTool {
                    client: client.clone(),
                    tool_name,
                    namespaced_name: namespaced,
                    description,
                    input_schema,
                    tool_timeout: self.tool_timeout,
                }) as Arc<dyn Tool>
            })
            .collect();
        Ok(tools)
    }
}

/// 单个 MCP 工具包装：通过共享连接执行 `call_tool`
struct ManagedMcpTool {
    client: SharedClient,
    tool_name: String,
    namespaced_name: String,
    description: String,
    input_schema: Option<Value>,
    tool_timeout: Duration,
}

#[async_trait]
impl Tool for ManagedMcpTool {
    fn name(&self) -> &str {
        &self.namespaced_name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn is_builtin(&self) -> bool {
        false
    }

    fn parameters_schema(&self) -> Option<Value> {
        self.input_schema.clone()
    }

    fn declaration(&self) -> Value {
        serde_json::json!({
            "name": self.namespaced_name,
            "description": self.description,
            // 字段名必须是 "parameters"（OpenAI tool 格式），LLM client 的 convert_tools
            // 用 decl.get("parameters") 取 schema；写成 "input_schema" 会导致参数 schema
            // 取空，LLM 收到无参工具 → 调用时传空 {} → 有参工具（如 save_workbook）报
            // "missing field"。见 llm/openai/compat 的 convert_tools / anthropic convert_tools。
            "parameters": self.input_schema.clone().unwrap_or_else(|| serde_json::json!({})),
        })
    }

    async fn execute(&self, _ctx: Arc<dyn ToolContext>, args: Value) -> AdkResult<Value> {
        // 节点级摘要(每工具调用一条):名字+参数字节,不 dump 全文——参数可能巨大
        // (文档/代码内容),全文进日志既刷屏又可能带敏感数据;逐项排查开 debug。
        tracing::info!(
            "[ManagedMcpTool] 调用: {} args_bytes={}",
            self.tool_name,
            serde_json::to_string(&args).map(|s| s.len()).unwrap_or(0)
        );
        tracing::debug!("[ManagedMcpTool] 原始参数: {:?}", args);
        let cleaned_args = sanitize_tool_args(args);
        // 截图类工具：去掉 filename，强制 MCP 端回传 base64 图片块（而非只存盘），
        // 使 cortex 与 MCP 跨机器（不共享文件系统）时也能拿到图片字节内联显示。
        let cleaned_args = strip_screenshot_filename(&self.tool_name, cleaned_args);
        tracing::debug!("[ManagedMcpTool] 清理后参数: {:?}", cleaned_args);

        let mut params = CallToolRequestParams::new(self.tool_name.clone());
        match cleaned_args {
            Value::Object(ref map) if !map.is_empty() => {
                params = params.with_arguments(map.clone());
            }
            Value::Object(_) => {}
            Value::Null => {}
            _ => {
                return Err(AdkError::tool(format!(
                    "MCP tool '{}' 参数必须是 JSON 对象",
                    self.tool_name
                )));
            }
        }
        let running = self.client.lock().await;
        // 【参考 codex DEFAULT_TOOL_TIMEOUT】call_tool 加超时，防止 MCP 工具卡死
        // （如 excel-mcp-server 的 write_cells 在某些 cell 组合下挂起）导致 SSE 无限阻塞。
        // 超时返回错误，agent 可重试/换法，而不是前端永远转圈。
        let result = tokio::time::timeout(self.tool_timeout, running.call_tool(params))
            .await
            .map_err(|_| {
                AdkError::tool(format!(
                    "MCP tool '{}' 执行超时（{}s）",
                    self.tool_name,
                    self.tool_timeout.as_secs()
                ))
            })?
            .map_err(|e| AdkError::tool(format!("MCP call_tool 失败: {e}")))?;

        if result.is_error.unwrap_or(false) {
            let mut msg = format!("MCP tool '{}' 执行失败", self.tool_name);
            for content in &result.content {
                if let Some(text) = content.as_text() {
                    msg.push_str(": ");
                    msg.push_str(&text.text);
                    break;
                }
            }
            return Err(AdkError::tool(msg));
        }

        // 结果摘要:类型计数,不 dump 全文(CallToolResult 可能含 base64 图片块,
        // 单条数 MB 会灌爆日志);文本内容截断 200 字符进 debug 供排查。
        let result_text = result
            .content
            .iter()
            .find_map(|c| c.as_text().map(|t| t.text.clone()))
            .unwrap_or_default();
        tracing::info!(
            "[ManagedMcpTool] 结果: {} ok={} content_blocks={} text_chars={}",
            self.tool_name,
            !result.is_error.unwrap_or(false),
            result.content.len(),
            result_text.chars().count()
        );
        tracing::debug!(
            "[ManagedMcpTool] 结果文本(截断): {}",
            result_text.chars().take(200).collect::<String>()
        );

        // 优先返回 structured_content
        if let Some(structured) = result.structured_content {
            return Ok(serde_json::json!({ "output": structured }));
        }

        // 否则拼接文本内容。截图类工具额外保留 image content block 的 base64
        // （挂到 out.image），交由截图管线（tools::screenshot::process_screenshot_response）
        // 解码落盘 + 注入 image_url，使截图能在聊天界面内联显示。需配合 MCP 端
        // `--image-responses allow` 让工具回传图片块。门控在 screenshot 工具名上，
        // 避免非截图工具的图片块把巨大 base64 灌进 LLM 上下文。
        let is_screenshot_tool = self.tool_name.contains("screenshot");
        let mut parts: Vec<String> = Vec::new();
        let mut image_data: Option<(String, String)> = None; // (base64, mime_type)
        for content in &result.content {
            if let Some(text) = content.as_text() {
                parts.push(text.text.clone());
            } else if is_screenshot_tool {
                if let Some(img) = content.as_image() {
                    image_data.get_or_insert((img.data.clone(), img.mime_type.clone()));
                }
            }
            // 其他类型（resource 等）忽略：原占位 "[非文本内容]" 无信息量
        }
        let mut out = serde_json::json!({ "output": parts.join("\n") });
        if let Some((data, mime)) = image_data {
            if let Some(obj) = out.as_object_mut() {
                obj.insert(
                    "image".to_string(),
                    serde_json::json!({ "mime_type": mime, "data": data }),
                );
            }
        }
        Ok(out)
    }
}

/// 截图类工具去掉 `filename` 参数：Playwright 等在传入 filename 时只存盘、不回传
/// base64 图片块；去掉后改为内联回传 base64，供 cortex 解码显示。cortex 与 MCP 跨
/// 机器、不共享文件系统时尤其必需——否则 cortex 拿不到图片字节。
fn strip_screenshot_filename(tool_name: &str, mut value: Value) -> Value {
    if !tool_name.to_ascii_lowercase().contains("screenshot") {
        return value;
    }
    if let Some(obj) = value.as_object_mut() {
        if obj.remove("filename").is_some() {
            tracing::info!(
                "[ManagedMcpTool] 已去掉截图工具 `{}` 的 filename 参数，强制内联 base64",
                tool_name
            );
        }
    }
    value
}

// 清理工具参数的辅助函数。
// 递归过程不打日志:嵌套 JSON 每节点 2 条会把一次调用刷成十几条,
// 清理动作本身幂等无事件语义(去反引号包裹),不值得观测。
fn sanitize_tool_args(value: Value) -> Value {
    match value {
        Value::String(s) => Value::String(s.trim_matches('`').to_string()),
        Value::Object(mut map) => {
            for (_key, val) in map.iter_mut() {
                *val = sanitize_tool_args(val.clone());
            }
            Value::Object(map)
        }
        Value::Array(arr) => {
            let cleaned_arr: Vec<Value> = arr.into_iter().map(sanitize_tool_args).collect();
            Value::Array(cleaned_arr)
        }
        _ => value,
    }
}

