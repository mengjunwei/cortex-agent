# OpenAICustomCompatible — 自定义实现变更说明

## 一、背景

ADK `OpenAICompatible` 在流式模式下使用 `ToolCallBuffer` 检测文本标签格式的工具调用
（如 `<tool_call>`、`[TOOL_CALLS]`）。

其 `has_partial_prefix()` 方法从 `i=1` 开始匹配前缀，导致：
- 普通文本中的单字符 `<` 或 `[` 就会触发缓冲状态
- 后续文本被暂存，直到确认不是工具调用才输出
- 在某些场景下（如网络设备命令 `enable<cr>configure terminal`），内容到达延迟或被截断

此外，`flush_as_emit()` 和 `flush()` 使用 `text.trim().is_empty()` 判断，
会**丢弃纯空白 chunk**（空格、换行符），导致流式输出丢失格式。

## 二、与 ADK `OpenAICompatible` 的差异清单

### 2.1 有意修改（3 处）

#### 修改 1：`has_partial_prefix()` — 最小前缀匹配长度

**文件**: `src/llm/openai_custom.rs`
**位置**: L211-L224

| 项目 | ADK 原版 | 本实现 |
|---|---|---|
| 最小匹配长度 | `1`（单字符 `<` / `[` 即触发） | `3`（`<to` / `[TO` / `<\|t` 才触发） |
| 常量名 | 无（硬编码 `1`） | `MIN_PREFIX_LEN` |

**ADK 原版代码**:
```rust
fn has_partial_prefix(&self) -> bool {
    let buf = &self.buffer;
    for prefix in TOOL_CALL_PREFIXES {
        let prefix_chars: Vec<char> = prefix.chars().collect();
        for i in 1..prefix_chars.len() {  // ← i=1: 单字符即触发
            let partial: String = prefix_chars[..i].iter().collect();
            if buf.ends_with(&partial) {
                return true;
            }
        }
    }
    false
}
```

**本实现代码**:
```rust
const MIN_PREFIX_LEN: usize = 3;

fn has_partial_prefix(&self) -> bool {
    let buf = &self.buffer;
    for prefix in TOOL_CALL_PREFIXES {
        let prefix_chars: Vec<char> = prefix.chars().collect();
        for i in MIN_PREFIX_LEN..prefix_chars.len() {  // ← i=3: 至少3字符
            let partial: String = prefix_chars[..i].iter().collect();
            if buf.ends_with(&partial) {
                return true;
            }
        }
    }
    false
}
```

**影响**:
- 普通文本中的 `<`、`[`、`<t`、`[T` 等**不再触发缓冲**，直接流式输出
- 真正的工具调用前缀（最短的 `<tool_call` 前 3 字符 `<to`）仍能正确匹配
- 所有 6 种工具调用格式不受影响：
  - `<tool_call` (10 字符) → 前 3 字符 `<to`
  - `<|tool_call>` (12 字符) → 前 3 字符 `<|t`
  - `<|python_tag|` (14 字符) → 前 3 字符 `<|p`
  - `[TOOL_CALLS]` (12 字符) → 前 3 字符 `[TO`
  - `<|action_start|>` (17 字符) → 前 3 字符 `<|a`
  - `<｜tool` (DeepSeek 全宽，7 字符) → 前 3 字符 `<｜t`

#### 修改 2：`flush_as_emit()` — 保留纯空白文本

**文件**: `src/llm/openai_custom.rs`
**位置**: L250-L258

| 项目 | ADK 原版 | 本实现 |
|---|---|---|
| 空白判断 | `text.trim().is_empty()` → 丢弃 | `text.is_empty()` → 保留 |

**ADK 原版代码**:
```rust
fn flush_as_emit(&mut self) -> BufferAction {
    let text = std::mem::take(&mut self.buffer);
    self.buffering = false;
    if text.trim().is_empty() {  // ← "  \n" 会被丢弃
        BufferAction::Emit(Vec::new())
    } else {
        BufferAction::Emit(vec![Part::Text { text }])
    }
}
```

**本实现代码**:
```rust
fn flush_as_emit(&mut self) -> BufferAction {
    let text = std::mem::take(&mut self.buffer);
    self.buffering = false;
    if text.is_empty() {  // ← 只有真正空字符串才跳过
        BufferAction::Emit(Vec::new())
    } else {
        BufferAction::Emit(vec![Part::Text { text }])
    }
}
```

**影响**: 流式模式下，模型发送的纯空白 chunk（换行符 `\n`、空格 ` `）不再被丢弃，
保留了文本格式（缩进、换行、段落间距）。

#### 修改 3：`flush()` — 同上

**文件**: `src/llm/openai_custom.rs`
**位置**: L177-L198

同修改 2 的逻辑，将 `text.trim().is_empty()` 改为 `text.is_empty()`。

---

### 2.2 必要适配（1 处）

#### 适配 1：`reasoning_effort` — JSON 层面设置

**文件**: `src/llm/openai_custom.rs`
**位置**: L604-L612

| 项目 | ADK 原版 | 本实现 |
|---|---|---|
| 设置方式 | `request_builder.reasoning_effort(effort.clone())` | JSON body 直接 `insert("reasoning_effort", ...)` |

**原因**: `async_openai` 的 builder 方法 `reasoning_effort()` 要求 `ReasoningEffort: Clone`，
而通过 `adk_rust::model::openai::ReasoningEffort` re-export 的版本在 trait bound 上存在冲突。
改为 JSON 层面插入，序列化结果完全一致。

```rust
// 本实现
if let Some(effort) = reasoning_effort {
    if let Some(body_obj) = body.as_object_mut() {
        body_obj.insert(
            "reasoning_effort".to_string(),
            serde_json::to_value(effort).unwrap_or_default(),
        );
    }
}
```

---

### 2.3 已验证一致的模块（23 项）

| # | 模块 | 对应 ADK 源码位置 | 状态 |
|---|---|---|---|
| 1 | `OpenAICustomCompatibleConfig` 结构 | `openai_compatible.rs:20-56` | 一致 |
| 2 | `OpenAICustomCompatible` struct + `new()` | `openai_compatible.rs:60-150` | 一致 |
| 3 | `build_request_json`（messages/tools/parallel_tool_calls） | `openai_compatible.rs:280-375` | 一致 |
| 4 | `build_request_json`（temperature/top_p/max_tokens/response_schema） | `openai_compatible.rs:375-490` | 一致 |
| 5 | `build_request_json`（extensions 合并） | `openai_compatible.rs:490-505` | 一致 |
| 6 | `send_request`（bearer_auth/org_header） | `openai_compatible.rs:506-540` | 一致 |
| 7 | `send_request`（HTTP status → ErrorCategory 映射） | `openai_compatible.rs:540-565` | 一致 |
| 8 | `send_request`（with_provider/with_upstream_status） | `openai_compatible.rs:565-570` | 一致 |
| 9 | `parse_finish_reason` | `openai_compatible.rs:571-580` | 一致 |
| 10 | `parse_usage_from_chunk`（含 audio_tokens） | `openai_compatible.rs:581-610` | 一致 |
| 11 | `content_to_message`（user/model/system/tool 全分支） | `openai/convert.rs:21-147` | 一致 |
| 12 | `convert_tools` | `openai/convert.rs:226-257` | 一致 |
| 13 | `serialize_tool_result` | `tool_result.rs:6-14` | 一致 |
| 14 | `extract_text` | `openai/convert.rs:187-197` | 一致 |
| 15 | Retry（流式/非流式 `execute_with_retry`） | `openai_compatible.rs:620-690` | 一致 |
| 16 | Telemetry（`llm_generate_span` + `with_usage_tracking`） | `openai_compatible.rs:665-670` | 一致 |
| 17 | 流式 SSE 解析（按行 / `data:` / `[DONE]` / JSON 容错） | `openai_compatible.rs:700-724` | 一致 |
| 18 | 流式 tool_calls 累积（index/id/name/arguments） | `openai_compatible.rs:725-780` | 一致 |
| 19 | 流式 finish_reason（FunctionCall/Text 分支，partial/turn_complete） | `openai_compatible.rs:780-845` | 一致 |
| 20 | 流式 reasoning_content → Part::Thinking | `openai_compatible.rs:846-866` | 一致 |
| 21 | 流式 BufferAction Emit + flush（is_tool 区分） | `openai_compatible.rs:867-910` | 一致 |
| 22 | 非流式响应解析（含 `parse_text_tool_calls`） | `openai/convert.rs:341-450` | 一致 |
| 23 | ToolCallBuffer（push/starts/has_complete/try_parse） | `tool_call_parser.rs:398-498` | 一致 |

> **后续增量差异（本表未逐项记录）**：本仓库在上述对照之后又新增了以下改动——
> - `frequency_penalty` / `presence_penalty` 透传（L543-550），并对 `f32→f64` 序列化做精度规整（L574-592，extensions 合并后再规整一次 L625-643）；
> - 移除 `tool_choice` 字段，兼容未启用 `--enable-auto-tool-choice` 的 vLLM / 兼容端点（L578-602）；
> - 识别 vLLM 未启用 function-calling 的典型错误并给出可操作提示（L688-703）；
> - `MaxTokens` 截断时丢弃 `args` 解析失败的残缺 tool call，避免「截断 → 残缺工具 → 报错 → 重生成」死循环（L948-977）；
> - 以 `ended` 标志在流末尾补发 `turn_complete=true` 的结束信号，保证单次调用语义完整（L1099-1107）。
>
> 证据行号均相对 `src/llm/openai_custom.rs`。

## 三、文件结构

```
src/llm/
├── mod.rs                  ← make_model() / make_gen_config()（按 protocol 分发）
├── openai_custom.rs        ← OpenAICustomCompatible 完整实现
└── anthropic_custom/       ← Anthropic Messages 协议本地实现（抄自 adk-model，修了 base_url / SSE UTF-8 分包 bug）
    ├── mod.rs
    ├── client.rs           ← AnthropicClient
    ├── config.rs
    ├── convert.rs
    ├── sse_stream.rs
    ├── schema_adapter.rs
    ├── models.rs
    ├── rate_limit.rs
    ├── token_count.rs
    ├── attachment.rs
    └── error.rs
```

## 四、使用方式

在 `src/llm/mod.rs` 中：

```rust
use crate::llm::openai_custom::{OpenAICustomCompatible, OpenAICustomCompatibleConfig};

// 当前签名：供应商存储经参数注入，内部走 store.resolve_model()，
// 按 protocol 分发到 OpenAICustomCompatible 或 AnthropicClient，无硬编码供应商。
// 详见 src/llm/mod.rs：L15（use）、L28-42（make_model / make_model_by_id）、L85-93（构造）。
pub fn make_model(store: &ModelProviderStore) -> anyhow::Result<Arc<dyn Llm>> {
    make_model_by_id(store, None)
}
```
