//! 本地 SSE 流解析器：绕开 adk-anthropic 1.0.0 `process_sse` 的 UTF-8 跨 chunk 丢字节 bug。
//!
//! ## 背景
//! adk-anthropic 1.0.0 的 `process_sse` 对每个 reqwest chunk **独立**做 `String::from_utf8`。
//! 当 TCP/TLS 分片切在多字节 UTF-8 字符（如中文，3 字节/字）中间时，`valid_up_to` 之后
//! 的不完整尾部字节被 `continue` 永久丢弃，且无跨 chunk 累积 → buffer 错位 → `\n\n` 边界
//! 找错 → `extract_event` 的 `split_once('\n')` 失败 → 报 `missing newline separator in
//! event`，整条流中断、agent 无输出。中文 thinking 内容极易触发，且失败是间歇性的
//! （取决于真实分片大小是否恰好切断字符）。
//!
//! ## 修复
//! 用 `Vec<u8>` 直接累积 reqwest 的原始字节（**不丢任何尾部**），按 `b"\n\n"` 切分事件。
//! 因 `\n`（0x0A）是 ASCII，UTF-8 多字节字符绝不包含 ASCII 字节，故 `\n\n` 边界必然落在
//! 字符边界上——切出的 event 切片整体必为合法 UTF-8，从根本上消除分包切断问题。
//!
//! 产出与 `adk_anthropic::Anthropic::stream` 内部的 `process_sse` 等价（同样的
//! `MessageStreamEvent`、同样的 `adk_anthropic::Error`），下游消费与错误转换链路无需改动。

use std::time::{Duration, Instant};

use adk_anthropic::{
    CompactionMetadata, ContentBlockDeltaEvent, ContentBlockStartEvent, ContentBlockStopEvent,
    Error, MessageDeltaEvent, MessageStartEvent, MessageStopEvent, MessageStreamEvent,
};
use bytes::Bytes;
use futures::stream::{self, Stream, StreamExt};

/// chunk 间最长静默时间：超过则判定连接卡死。
const CHUNK_TIMEOUT: Duration = Duration::from_secs(60);
/// 缓冲上限（防 DoS）。
const MAX_BUFFER_SIZE: usize = 1024 * 1024;

struct State<S> {
    stream: S,
    /// 原始字节缓冲：跨 chunk 累积，根治 UTF-8 分包切断（核心修复点）。
    buffer: Vec<u8>,
    last_activity: Instant,
}

/// 把一个 reqwest 字节流解析为 `MessageStreamEvent` 流。
///
/// 对外等价于 adk-anthropic 的 `process_sse`，但用字节累积替代 per-chunk `String::from_utf8`，
/// 消除「不完整多字节尾部被丢弃」的 bug。错误类型保持 [`Error`]，下游
/// `to_anthropic_api_error` / `convert_anthropic_error` 等无需改动。
pub fn parse<S>(byte_stream: S) -> impl Stream<Item = Result<MessageStreamEvent, Error>>
where
    S: Stream<Item = reqwest::Result<Bytes>> + Unpin + 'static,
{
    // reqwest 错误 → adk_anthropic::Error，保持与上游一致的错误语义。
    let mapped = byte_stream.map(|res| {
        res.map_err(|e| Error::streaming(format!("Error in HTTP stream: {e}"), Some(Box::new(e))))
    });

    stream::unfold(
        State {
            stream: mapped,
            buffer: Vec::new(),
            last_activity: Instant::now(),
        },
        |mut state| async move {
            loop {
                // 1. 尝试从缓冲取一个完整事件
                if let Some((event, consumed)) = extract_event(&state.buffer) {
                    state.buffer.drain(..consumed);
                    return Some((event, state));
                }

                // 2. 缓冲上限保护
                if state.buffer.len() > MAX_BUFFER_SIZE {
                    return Some((
                        Err(Error::streaming(
                            format!("SSE buffer exceeded maximum limit: {MAX_BUFFER_SIZE} bytes"),
                            None,
                        )),
                        state,
                    ));
                }

                // 3. chunk 间静默超时
                if state.last_activity.elapsed() > CHUNK_TIMEOUT {
                    return Some((
                        Err(Error::timeout(
                            "SSE stream timeout: no data received within timeout period"
                                .to_string(),
                            Some(CHUNK_TIMEOUT.as_secs_f64()),
                        )),
                        state,
                    ));
                }

                // 4. 读下一个 chunk —— 字节直接累积（关键：不丢不完整尾部）
                match state.stream.next().await {
                    Some(Ok(bytes)) => {
                        state.last_activity = Instant::now();
                        state.buffer.extend_from_slice(&bytes);
                        // 继续循环，尝试解析已累积的字节
                    }
                    Some(Err(e)) => return Some((Err(e), state)),
                    // 流结束：正常 SSE 每个事件均以 \n\n 结尾，缓冲此时应为空，故直接结束。
                    None => return None,
                }
            }
        },
    )
}

/// 从字节缓冲中取出一个完整的 SSE 事件。
///
/// 返回 `(事件, 已消费字节数)`；若没有完整事件（未出现 `\n\n`）返回 `None`。
/// 切出的 `event_bytes` 必为合法 UTF-8——`\n` 是 ASCII，UTF-8 多字节字符不含 ASCII 字节，
/// 故 `\n\n` 边界必在字符边界上。
fn extract_event(buffer: &[u8]) -> Option<(Result<MessageStreamEvent, Error>, usize)> {
    let end = buffer.windows(2).position(|w| w == b"\n\n")?;
    let consumed = end + 2;
    let event_text = match std::str::from_utf8(&buffer[..end]) {
        Ok(s) => s,
        Err(_) => {
            return Some((
                Err(Error::serialization(
                    "SSE event slice is not valid UTF-8 (unexpected: \\n is an ASCII boundary)"
                        .to_string(),
                    None,
                )),
                consumed,
            ));
        }
    };
    Some((parse_event_text(event_text), consumed))
}

/// 解析单个 SSE 事件的文本（`event:` + `data:` 行），产出 [`MessageStreamEvent`]。
///
/// 相较 adk 的 `parse_event_type`，额外做的健壮化（均不中断流）：
/// - 忽略 SSE 注释行（`:` 开头）；
/// - 未知的 `event` 类型记 warn 后跳过（返回 `Ping`），而非抛错中断整条流。
fn parse_event_text(text: &str) -> Result<MessageStreamEvent, Error> {
    let mut event_type: Option<&str> = None;
    let mut data_lines: Vec<&str> = Vec::new();

    for line in text.lines() {
        if line.is_empty() {
            continue;
        }
        if line.starts_with(':') {
            continue; // SSE 注释行
        }
        if let Some(rest) = line.strip_prefix("event:") {
            event_type = Some(rest.trim());
        } else if let Some(rest) = line.strip_prefix("data:") {
            data_lines.push(rest.trim());
        }
    }

    let event_type = match event_type {
        None => return Ok(MessageStreamEvent::Ping), // 无 event 行（如纯 keepalive）
        Some("") => return Ok(MessageStreamEvent::Ping),
        Some(t) => t,
    };

    let data = data_lines.join("\n");

    match event_type {
        "ping" => Ok(MessageStreamEvent::Ping),
        "message_start" => {
            from_json::<MessageStartEvent>(&data).map(MessageStreamEvent::MessageStart)
        }
        "message_delta" => {
            from_json::<MessageDeltaEvent>(&data).map(MessageStreamEvent::MessageDelta)
        }
        "message_stop" => from_json::<MessageStopEvent>(&data).map(MessageStreamEvent::MessageStop),
        "content_block_start" => {
            from_json::<ContentBlockStartEvent>(&data).map(MessageStreamEvent::ContentBlockStart)
        }
        "content_block_delta" => {
            from_json::<ContentBlockDeltaEvent>(&data).map(MessageStreamEvent::ContentBlockDelta)
        }
        "content_block_stop" => {
            from_json::<ContentBlockStopEvent>(&data).map(MessageStreamEvent::ContentBlockStop)
        }
        "tool_input_start" => parse_tool_input(&data, false),
        "tool_input_delta" => parse_tool_input(&data, true),
        "compaction" => {
            from_json::<CompactionMetadata>(&data).map(MessageStreamEvent::CompactionEvent)
        }
        "error" => Ok(parse_error_event(&data)),
        other => {
            tracing::warn!(event_type = other, "unknown SSE event type, skipping");
            Ok(MessageStreamEvent::Ping)
        }
    }
}

fn from_json<T: serde::de::DeserializeOwned>(data: &str) -> Result<T, Error> {
    serde_json::from_str(data).map_err(|e| {
        Error::serialization(
            format!("Failed to parse SSE event data: {e}"),
            Some(Box::new(e)),
        )
    })
}

fn parse_tool_input(data: &str, delta: bool) -> Result<MessageStreamEvent, Error> {
    let v: serde_json::Value = serde_json::from_str(data).map_err(|e| {
        Error::serialization(
            format!("Failed to parse tool_input event: {e}"),
            Some(Box::new(e)),
        )
    })?;
    let get = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
    if delta {
        Ok(MessageStreamEvent::ToolInputDelta {
            tool_use_id: get("tool_use_id"),
            parameter_name: get("parameter_name"),
            value_fragment: get("value_fragment"),
        })
    } else {
        Ok(MessageStreamEvent::ToolInputStart {
            tool_use_id: get("tool_use_id"),
            parameter_name: get("parameter_name"),
        })
    }
}

fn parse_error_event(data: &str) -> MessageStreamEvent {
    let (error_type, message) = match serde_json::from_str::<serde_json::Value>(data) {
        Ok(obj) => {
            let et = obj
                .get("error")
                .and_then(|e| e.get("type"))
                .and_then(|t| t.as_str())
                .unwrap_or("stream_error")
                .to_string();
            let msg = obj
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .unwrap_or("Unknown stream error")
                .to_string();
            (et, msg)
        }
        Err(_) => ("stream_error".to_string(), data.to_string()),
    };
    MessageStreamEvent::StreamError {
        error: adk_anthropic::ApiError {
            error_type,
            message,
        },
    }
}
