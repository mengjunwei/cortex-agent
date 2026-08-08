//! 工具输出处理管线：脱敏 → 语义过滤 → UTF-8 安全截断
//!
//! MCP 工具（如 browser_snapshot）可能返回超大内容（完整 DOM 快照可达 1.5MB+），
//! 直接送入 LLM 会触发模型提供商 HTTP 500 InternalServiceError。
//!
//! 本模块提供 `TruncatingToolset`，对内层 toolset 的每个工具做包装，在 `execute`
//! 返回后依次执行三步处理：
//!
//! 1. **脱敏**（[`crate::tools::redact`]）：擦除密钥/凭证/通用 PII（Email/Phone）
//! 2. **语义过滤**（[`crate::tools::filter`]）：按工具家族压缩结构化输出
//!    （表格表头保留、markdown 目录抽取、grep 行聚合）
//! 3. **截断**：分层阈值 + UTF-8 安全硬截断 + 精度兜底（过滤后不短于原文则回退）
//!
//! ## 分层截断阈值
//!
//! 不同工具按名称匹配不同阈值，避免一刀切：
//! - `browser_snapshot` 等 DOM 类工具：24KB（DOM 快照压缩）
//! - `search_kb` 等检索类工具：16KB（知识库结果）
//! - 其他工具：默认 48KB

use std::sync::Arc;

use adk_rust::tool::{Tool, Toolset};
use adk_rust::{ReadonlyContext, ToolContext};
use async_trait::async_trait;
use serde_json::Value;

use crate::infra::object_store::ObjectStore;
use crate::tools::filter::{self, FilterFamily};

/// 默认最大输出大小（字节）：48 KB
///
/// 选取 48KB 是为了给 system prompt + 历史消息 + 工具 schema 留出足够余量，
/// 避免单次工具输出占满模型上下文窗口。
const DEFAULT_MAX_OUTPUT_BYTES: usize = 48 * 1024;
/// DOM 类工具（browser_snapshot 等）输出阈值：24 KB
const DOM_TOOL_MAX_BYTES: usize = 24 * 1024;
/// 检索类工具（search_kb 等）输出阈值：16 KB
const SEARCH_TOOL_MAX_BYTES: usize = 16 * 1024;
/// 嵌套对象/数组截断时，每层预算衰减系数（1/10）
const NESTED_BUDGET_DECAY: usize = 10;
/// 数组截断时保留的最大元素个数
const MAX_ARRAY_ELEMENTS: usize = 200;

/// 工具输出截断包装 toolset
pub struct TruncatingToolset {
    inner: Arc<dyn Toolset>,
    /// 全局默认截断阈值（可通过 with_max_output_bytes 覆盖）
    max_output_bytes: usize,
    /// 对象存储：截图工具的 base64 结果会上传为 image_url，避免巨大 base64 在截断阶段
    /// 被破坏。None 时截图走普通截断流程。
    object_store: Option<Arc<ObjectStore>>,
}

impl TruncatingToolset {
    /// 创建截断包装器，使用默认阈值
    pub fn new(inner: Arc<dyn Toolset>) -> Self {
        Self {
            inner,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            object_store: None,
        }
    }

    /// 自定义截断阈值（字节）
    pub fn with_max_output_bytes(mut self, max_bytes: usize) -> Self {
        self.max_output_bytes = max_bytes;
        self
    }

    /// 设置对象存储，启用「截断前先上传」的截图保护
    pub fn with_object_store(mut self, os: Arc<ObjectStore>) -> Self {
        self.object_store = Some(os);
        self
    }
}

#[async_trait]
impl Toolset for TruncatingToolset {
    fn name(&self) -> &str {
        self.inner.name()
    }

    async fn tools(&self, ctx: Arc<dyn ReadonlyContext>) -> adk_rust::Result<Vec<Arc<dyn Tool>>> {
        let inner_tools = self.inner.tools(ctx).await?;
        let user_cap = self.max_output_bytes;
        let object_store = self.object_store.clone();
        let wrapped: Vec<Arc<dyn Tool>> = inner_tools
            .into_iter()
            .map(|tool| {
                let name = tool.name().to_string();
                // 按工具名匹配类别默认阈值，再与用户全局上限取较小值：
                // 用户通过 with_max_output_bytes 设置的值始终作为不可逾越的上限生效
                // （回归 B6：此前硬编码值会无视用户 override）。
                let tool_default = if name.contains("snapshot") || name.contains("dom") {
                    DOM_TOOL_MAX_BYTES
                } else if name.contains("search") || name.contains("retrieve") {
                    SEARCH_TOOL_MAX_BYTES
                } else {
                    DEFAULT_MAX_OUTPUT_BYTES
                };
                let max_bytes = tool_default.min(user_cap);
                Arc::new(TruncatingTool {
                    inner: tool,
                    max_output_bytes: max_bytes,
                    tool_name: name,
                    object_store: object_store.clone(),
                }) as Arc<dyn Tool>
            })
            .collect();
        Ok(wrapped)
    }
}

/// 工具输出截断包装 tool
struct TruncatingTool {
    inner: Arc<dyn Tool>,
    max_output_bytes: usize,
    tool_name: String,
    object_store: Option<Arc<ObjectStore>>,
}

#[async_trait]
impl Tool for TruncatingTool {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn description(&self) -> &str {
        self.inner.description()
    }

    fn is_long_running(&self) -> bool {
        self.inner.is_long_running()
    }

    fn is_builtin(&self) -> bool {
        self.inner.is_builtin()
    }

    fn parameters_schema(&self) -> Option<Value> {
        self.inner.parameters_schema()
    }

    fn response_schema(&self) -> Option<Value> {
        self.inner.response_schema()
    }

    fn is_read_only(&self) -> bool {
        self.inner.is_read_only()
    }

    fn is_concurrency_safe(&self) -> bool {
        self.inner.is_concurrency_safe()
    }

    fn required_scopes(&self) -> &[&str] {
        self.inner.required_scopes()
    }

    fn declaration(&self) -> Value {
        self.inner.declaration()
    }

    async fn execute(&self, ctx: Arc<dyn ToolContext>, args: Value) -> adk_rust::Result<Value> {
        // 截图按会话隔离存盘，需 session_id。inner.execute(ctx) 会 move ctx，故先取出
        // （session_id() 仅读字符串，非截图工具取了也无开销）。
        let session_id = ctx.session_id().to_string();
        let result = self.inner.execute(ctx, args).await?;
        // 管线第一步：脱敏（密钥/凭证/PII），确保敏感信息不进入 LLM 上下文
        let redacted = crate::tools::redact::redact_secrets(result);
        // 截图工具：截断前先把大 base64 落盘转 image_url。否则 base64 会在下面的
        // truncate_tool_output 里被硬截断（JSON 断裂 + 数据残缺 + 追加截断标记），
        // 既无法解码成图片、前端也无法识别。详见 super::screenshot 模块文档。
        let processed = if self.tool_name.contains("screenshot") {
            match self.object_store.as_ref() {
                Some(os) => {
                    super::screenshot::process_screenshot_response(os, &session_id, redacted).await
                }
                None => redacted,
            }
        } else {
            redacted
        };
        Ok(truncate_tool_output(
            processed,
            &self.tool_name,
            self.max_output_bytes,
        ))
    }
}

/// 检测并截断工具输出
///
/// 管线：脱敏（已在 execute 完成）→ 语义过滤 → 分层截断。
/// 针对不同工具采用分层截断策略：
/// 1. 对结构化 JSON 返回，提取关键字段而非硬截断
/// 2. 对 `{"output": "..."}` 结构做原地截断
/// 3. 兜底：对整体 JSON 序列化后截断
fn truncate_tool_output(mut value: Value, tool_name: &str, max_bytes: usize) -> Value {
    let family = filter::detect_family(tool_name);

    // 策略1：对结构化返回（含 status/data 字段），提取关键字段
    //
    // 修复 C1：黑名单策略只压缩 data，但其他大字段（如 logs/full_config）可能仍超预算。
    // 提取后必须序列化复检，超限则降级为整体硬截断，守住 max_bytes 这一核心契约
    // （本模块存在的全部意义就是"防爆 500"）。
    if let Some(obj) = value.as_object() {
        if obj.contains_key("status") && obj.contains_key("data") {
            let extracted = extract_key_fields(value, tool_name, max_bytes);
            let serialized = serde_json::to_string(&extracted).unwrap_or_default();
            if (serialized.len() as f64 * SERIALIZATION_SAFETY) as usize <= max_bytes {
                return extracted;
            }
            // 压缩后仍超预算 → 整体硬截断，确保不突破上限
            return Value::String(truncate_text(&serialized, max_bytes, family));
        }
    }

    // 策略2：优先处理 {"output": "..."} 结构（McpTool 的标准返回格式）
    if let Some(output) = value.get("output").and_then(|v| v.as_str()) {
        let output_bytes = output.len();
        if output_bytes > max_bytes {
            let truncated = truncate_text(output, max_bytes, family);
            if let Some(obj) = value.as_object_mut() {
                obj.insert("output".to_string(), Value::String(truncated));
            }
        }
        return value;
    }

    // 策略3：兜底 — 对整体 JSON 序列化后判断大小
    let serialized = serde_json::to_string(&value).unwrap_or_default();
    if (serialized.len() as f64 * SERIALIZATION_SAFETY) as usize <= max_bytes {
        return value;
    }

    // 整体过大且非上述结构，截断序列化文本
    let truncated = truncate_text(&serialized, max_bytes, family);
    Value::String(truncated)
}

/// 从结构化 JSON 中压缩冗余 payload，保留所有字段
///
/// 适用于含 `status` + `data` 的工具返回。采用**黑名单**策略：
/// 仅对已知的"重型字段"（`data`，可能含完整 DOM / 日志全文）做递归截断，
/// 其余字段（`message` / `error_code` / `trace_id` / `device_id` / `timestamp` ...）
/// **原样保留**，避免此前白名单实现静默吞掉业务字段（回归 B2）。
fn extract_key_fields(mut value: Value, _tool_name: &str, max_bytes: usize) -> Value {
    if let Some(obj) = value.as_object_mut() {
        // 仅对 data 这类已知大 payload 做截断，其他键一律不动
        if let Some(data) = obj.remove("data") {
            obj.insert("data".to_string(), truncate_nested_value(data, max_bytes));
        }
    }
    value
}

/// 递归截断嵌套 Value 中的大字符串/数组
///
/// 嵌套值脱离了工具上下文，无法判定家族，统一用 [`FilterFamily::Generic`]
/// （仅硬截断，不做语义压缩）。
fn truncate_nested_value(value: Value, max_bytes: usize) -> Value {
    match value {
        Value::String(s) if s.len() > max_bytes => {
            Value::String(truncate_text(&s, max_bytes, FilterFamily::Generic))
        }
        Value::Array(arr) => {
            // 数组元素过多时只保留前部
            if arr.len() > MAX_ARRAY_ELEMENTS {
                let truncated: Vec<Value> = arr.into_iter().take(MAX_ARRAY_ELEMENTS).collect();
                Value::Array(truncated)
            } else {
                Value::Array(
                    arr.into_iter()
                        .map(|v| truncate_nested_value(v, max_bytes / NESTED_BUDGET_DECAY))
                        .collect(),
                )
            }
        }
        Value::Object(obj) => {
            let truncated: serde_json::Map<String, Value> = obj
                .into_iter()
                .map(|(k, v)| (k, truncate_nested_value(v, max_bytes / NESTED_BUDGET_DECAY)))
                .collect();
            Value::Object(truncated)
        }
        other => other,
    }
}

/// 智能截断文本：语义过滤 → 精度兜底 → UTF-8 安全硬截断
///
/// 处理顺序：
/// 1. 文本短于 `max_bytes` 直接返回
/// 2. 调 [`filter::apply_filter`] 按家族做结构化压缩（Generic 族原样返回）
/// 3. 精度兜底（monotonicity）：若过滤结果不短于原文，回退到原文，避免反效果
/// 4. 对最终文本做 UTF-8 安全硬截断
fn truncate_text(text: &str, max_bytes: usize, family: FilterFamily) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    // 语义过滤：按 family 压缩（Generic 时 apply_filter 原样返回）
    let filtered = filter::apply_filter(text, family, max_bytes);
    // 精度兜底：过滤结果不短于原文则回退原文，保证截断单调缩减
    let source: &str = if filtered.len() < text.len() {
        &filtered
    } else {
        text
    };
    // 用 middle_truncate（头尾保留）替代仅保头：工具日志尾部常含结果/退出码，不应丢
    middle_truncate(source, max_bytes)
}

/// 序列化安全系数：截断判定时给 JSON 转义/包装开销预留 20% 余量。
///
/// 工具输出的文本后续会被 `serde_json::to_string` 再包一层（转义引号、加
/// `{"type":"function_call_output",...}` 框架），这些开销未计入文本自身字节。
/// 判定 `serialized.len() × 1.2 ≤ max_bytes` 提前收缩，避免序列化后实际超预算。
const SERIALIZATION_SAFETY: f64 = 1.2;

/// 头尾保留、中间省略的截断（对工具日志友好：头部常是命令/报错，尾部是结果/退出码）。
///
/// 预算太小（放不下 marker）时回退到 [`hard_truncate`]（保头）。
fn middle_truncate(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    const MARKER: &str = "\n\n[... 中间内容过长已截断 ...]\n\n";
    if max_bytes <= MARKER.len() + 16 {
        return hard_truncate(text, max_bytes);
    }
    let body = max_bytes - MARKER.len();
    let head_budget = body / 2;
    let tail_budget = body - head_budget;
    let head_end = filter::floor_char_boundary(text, head_budget);
    let tail_start = filter::floor_char_boundary(text, text.len().saturating_sub(tail_budget));
    if head_end >= tail_start {
        return hard_truncate(text, max_bytes);
    }
    let mut result = String::with_capacity(max_bytes);
    result.push_str(&text[..head_end]);
    result.push_str(MARKER);
    result.push_str(&text[tail_start..]);
    result
}

/// UTF-8 安全的硬截断：保留头部并附加截断提示
///
/// 借助 [`filter::floor_char_boundary`] 把预算下取到字符边界，
/// 避免切断 UTF-8 多字节字符（中文、emoji 等）。
///
/// **小预算保护**（回归 B4）：当 `max_bytes` 不足以容纳截断提示后缀时，
/// 省略后缀，仅按字符边界截断到 `max_bytes`，避免最终输出反而超出预算。
fn hard_truncate(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    const SUFFIX: &str = "\n\n[... 内容过长已截断 ...]";
    // 后缀放不下（或放下后正文所剩无几）时，省略后缀只截断正文
    if max_bytes <= SUFFIX.len() {
        let end = filter::floor_char_boundary(text, max_bytes);
        return text[..end].to_string();
    }
    let budget = max_bytes - SUFFIX.len();
    let end = filter::floor_char_boundary(text, budget);
    let mut result = String::with_capacity(end + SUFFIX.len());
    result.push_str(&text[..end]);
    result.push_str(SUFFIX);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_text_respects_byte_budget() {
        let text = "abcdefghij".repeat(1000); // 10KB
        let truncated = truncate_text(&text, 100, FilterFamily::Generic);
        assert!(truncated.len() <= 100);
        // middle_truncate：头尾保留 + 中间省略标记
        assert!(truncated.contains("已截断"));
    }

    #[test]
    fn truncate_text_preserves_short_content() {
        let text = "short content";
        let truncated = truncate_text(text, 100, FilterFamily::Generic);
        assert_eq!(truncated, "short content");
    }

    #[test]
    fn truncate_text_handles_utf8_boundary() {
        let text = "你好世界".repeat(500); // 中文字符，每个 3 字节
        let truncated = truncate_text(&text, 100, FilterFamily::Generic);
        // 不应在多字节字符中间截断
        assert!(truncated.len() <= 100);
        assert!(String::from_utf8(truncated.into_bytes()).is_ok());
    }

    #[test]
    fn truncate_text_applies_markdown_filter_for_kb() {
        // 长 markdown + MarkdownToc 族：应触发目录抽取，结果含"文档目录"
        let mut text = String::from("# 设备手册\n\n## 概述\n\n");
        text.push_str(&"正文内容。".repeat(2000));
        text.push_str("\n\n## 接口\n\n更多内容");
        let out = truncate_text(&text, 2048, FilterFamily::MarkdownToc);
        assert!(out.contains("文档目录") || out.contains("已截断"));
    }

    #[test]
    fn truncate_text_precision_fallback_generic() {
        // Generic 族不做压缩，过滤结果 == 原文，精度兜底回退原文后硬截断
        let text = "x".repeat(1000);
        let out = truncate_text(&text, 100, FilterFamily::Generic);
        assert!(out.len() <= 100);
        assert!(out.contains("已截断"));
    }

    #[test]
    fn truncate_tool_output_handles_output_field() {
        let large_output = "x".repeat(100_000);
        let value = serde_json::json!({ "output": large_output });
        let truncated = truncate_tool_output(value, "test_tool", 1000);
        let output = truncated.get("output").unwrap().as_str().unwrap();
        assert!(output.len() <= 1000);
        assert!(output.contains("已截断"));
    }

    #[test]
    fn truncate_tool_output_passes_through_small_output() {
        let value = serde_json::json!({ "output": "small" });
        let truncated = truncate_tool_output(value, "test_tool", 1000);
        assert_eq!(truncated.get("output").unwrap(), "small");
    }

    #[test]
    fn truncate_tool_output_structured_small_payload_kept_as_object() {
        // 正常路径：data 中等大小（< max_bytes），整体 ≤ 预算 → 保持 Object 结构
        // （C1 降级仅在 extract_key_fields 压缩后仍超预算时触发；本用例不触发）
        let modest_data = "x".repeat(500);
        let value = serde_json::json!({
            "status": "ok",
            "data": modest_data,
            "message": "success"
        });
        let truncated = truncate_tool_output(value, "test_tool", 2000);
        let obj = truncated.as_object().unwrap();
        assert_eq!(obj["status"], "ok");
        assert_eq!(obj["message"], "success");
        // 小 data 原样保留
        assert_eq!(obj["data"].as_str().unwrap().len(), 500);
    }

    #[test]
    fn extract_key_fields_preserves_unknown_fields() {
        // 回归 B2：黑名单策略必须保留 data 以外的业务字段，
        // 此前白名单实现会静默吞掉 error_code / trace_id 等
        let large_data = "x".repeat(50_000);
        let value = serde_json::json!({
            "status": "error",
            "data": large_data,
            "error_code": "DEVICE_TIMEOUT",
            "trace_id": "abc-123",
            "device_id": "r1-core-01",
            "timestamp": "2026-06-27T10:00:00Z",
            "retryable": true
        });
        let out = extract_key_fields(value, "device_command", 500);
        let obj = out.as_object().unwrap();
        // 未知字段全部保留
        assert_eq!(obj["error_code"], "DEVICE_TIMEOUT");
        assert_eq!(obj["trace_id"], "abc-123");
        assert_eq!(obj["device_id"], "r1-core-01");
        assert_eq!(obj["retryable"], true);
        assert!(obj.contains_key("timestamp"));
        // data 仍被截断
        assert!(obj["data"].as_str().unwrap().len() <= 500);
    }

    #[test]
    fn large_non_data_field_falls_back_to_hard_truncate() {
        // 回归 C1：非 data 字段（如 logs）很大时，extract_key_fields 黑名单只压缩 data，
        // 提取后整体仍超预算。truncate_tool_output 必须序列化复检并降级硬截断，
        // 守住 max_bytes 这一核心契约（防爆 500）。
        let huge_logs = "L".repeat(50_000); // 非已知大字段，但实际很大
        let value = serde_json::json!({
            "status": "ok",
            "data": "tiny",
            "logs": huge_logs
        });
        // max_bytes = 1000，logs 50KB 必须被压缩
        let out = truncate_tool_output(value, "device_command", 1000);
        // 降级为整体硬截断 → 结果为 Value::String
        assert!(out.is_string(), "超预算时应降级为 Value::String");
        let s = out.as_str().expect("降级后为字符串");
        // 字符串值本身 ≤ max_bytes（核心契约）。
        // 注：不测 serde_json::to_string(&out).len() —— 那会给字符串再加外层引号
        // 和转义内部引号，测量值虚高；模块契约针对"字符串值长度"。
        assert!(s.len() <= 1000, "字符串值 {} 字节超过预算 1000", s.len());
        assert!(
            s.contains("已截断"),
            "降级硬截断应附加截断提示，实际：{}",
            &s[..s.len().min(200)]
        );
    }

    #[test]
    fn redact_pipeline_strips_secrets_before_truncate() {
        // 模拟 execute 管线：先 redact_secrets 再 truncate_tool_output
        let large_secret = "AKIAIOSFODNN7EXAMPLE ".repeat(2000);
        let value = serde_json::json!({ "output": large_secret });
        let redacted = crate::tools::redact::redact_secrets(value);
        let out = truncate_tool_output(redacted, "exec_cmd", 2000);
        let s = out.get("output").unwrap().as_str().unwrap();
        assert!(s.contains("[REDACTED:AWS_AKID]"));
        assert!(!s.contains("AKIAIOSFODNN7EXAMPLE"));
    }

    #[test]
    fn tool_max_bytes_snapshot_vs_search() {
        // 验证阈值选择逻辑：类别默认值与用户上限取较小值（回归 B6）
        // 未设置用户上限时（用 DEFAULT_MAX_OUTPUT_BYTES 模拟），取类别默认
        assert_eq!(
            resolve_threshold("browser_snapshot", DEFAULT_MAX_OUTPUT_BYTES),
            DOM_TOOL_MAX_BYTES
        );
        assert_eq!(
            resolve_threshold("search_kb", DEFAULT_MAX_OUTPUT_BYTES),
            SEARCH_TOOL_MAX_BYTES
        );
        assert_eq!(
            resolve_threshold("unknown_tool", DEFAULT_MAX_OUTPUT_BYTES),
            DEFAULT_MAX_OUTPUT_BYTES
        );
    }

    #[test]
    fn user_cap_overrides_tool_default() {
        // 回归 B6：用户全局上限小于类别默认时，必须取用户上限生效
        let tight = 8 * 1024;
        assert_eq!(resolve_threshold("browser_snapshot", tight), tight);
        assert_eq!(resolve_threshold("search_kb", tight), tight);
        // 用户上限大于类别默认时，仍用类别默认（snapshot 不超过 24KB）
        let loose = 64 * 1024;
        assert_eq!(
            resolve_threshold("browser_snapshot", loose),
            DOM_TOOL_MAX_BYTES
        );
    }

    /// 镜像 TruncatingToolset::tools 中的阈值解析逻辑，便于单测
    fn resolve_threshold(tool_name: &str, user_cap: usize) -> usize {
        let tool_default = if tool_name.contains("snapshot") || tool_name.contains("dom") {
            DOM_TOOL_MAX_BYTES
        } else if tool_name.contains("search") || tool_name.contains("retrieve") {
            SEARCH_TOOL_MAX_BYTES
        } else {
            DEFAULT_MAX_OUTPUT_BYTES
        };
        tool_default.min(user_cap)
    }

    #[test]
    fn hard_truncate_tiny_budget_omits_suffix() {
        // 回归 B4：预算不足以容纳后缀时，省略后缀，结果不超预算
        let text = "abcdefghij".repeat(100);
        let out = hard_truncate(&text, 10);
        assert!(out.len() <= 10, "got len {}", out.len());
        // 小预算下不应出现截断后缀（后缀本身就有 20+ 字节）
        assert!(!out.contains("已截断"));
    }

    #[test]
    fn hard_truncate_normal_budget_keeps_suffix() {
        // 正常预算下仍附加后缀
        let text = "abcdefghij".repeat(100);
        let out = hard_truncate(&text, 100);
        assert!(out.len() <= 100);
        assert!(out.ends_with("[... 内容过长已截断 ...]"));
    }
}
