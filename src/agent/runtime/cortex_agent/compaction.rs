//! 上下文压缩（auto-compaction）—— 超阈值时用 LLM 把旧消息摘要成交接摘要。
//!
//! 对齐 codex `run_auto_compact` / `build_compacted_history`：旧 user 消息按预算原样保留，
//! 旧非 user 消息（model/function/thinking）摘要成一条，保护进行中的 tool 流不被截断。

use std::collections::HashMap;
use std::sync::Arc;

use adk_rust::{Content, Event, EventActions, EventCompaction, GenerateContentConfig, Llm, LlmRequest, Part};
use tokio_util::sync::CancellationToken;
use futures::StreamExt;

/// 用 LLM 总结对话历史（对齐 codex run_auto_compact）
///
/// 把旧的对话消息发给模型，让它生成一份交接摘要。
/// 摘要包含：进度、关键决策、待完成事项、关键数据。
pub(super) async fn llm_compact(
    model: &Arc<dyn Llm>,
    compact_model: Option<&Arc<dyn Llm>>,
    messages: &[Content],
    cancel_token: &CancellationToken,
) -> String {
    // 压缩优先用专用便宜模型（compact_model），未配则用主模型
    let model = compact_model.unwrap_or(model);
    let prompt_text = messages
        .iter()
        .filter_map(|c| {
            let role = c.role.clone();
            let text: String = c
                .parts
                .iter()
                .filter_map(|p| match p {
                    Part::Text { text } => Some(text.clone()),
                    Part::Thinking { thinking, .. } => Some(thinking.clone()),
                    Part::FunctionCall { name, args, .. } => {
                        Some(format!("[Tool call: {name}({args})]"))
                    }
                    Part::FunctionResponse {
                        function_response, ..
                    } => Some(format!("[Tool result: {}]", function_response.response)),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            if text.is_empty() {
                None
            } else {
                Some(format!("[{role}] {text}"))
            }
        })
        .collect::<Vec<_>>()
        .join("\n\n");

    let request = LlmRequest {
        model: model.name().to_string(),
        contents: vec![Content {
            role: "user".to_string(),
            parts: vec![Part::Text {
                text: format!("{}\n\n{}", crate::prompts::COMPACT_PROMPT, prompt_text),
            }],
        }],
        config: Some(GenerateContentConfig {
            temperature: Some(0.0),
            max_output_tokens: Some(2048),
            ..Default::default()
        }),
        tools: HashMap::new(),
        previous_response_id: None,
    };

    let failed = || {
        format!(
            "{} [Context compacted. Key tool results in recent messages.]",
            crate::prompts::COMPACT_SUMMARY_PREFIX
        )
    };

    // 建连 + 流读取都监听 cancel，避免 compaction 期间用户取消时卡住
    let mut stream = tokio::select! {
        r = model.generate_content(request, false) => match r {
            Ok(s) => s,
            Err(_) => return failed(),
        },
        _ = cancel_token.cancelled() => return failed(),
    };

    let mut summary = String::new();
    loop {
        let chunk = tokio::select! {
            r = stream.next() => match r {
                Some(Ok(c)) => c,
                _ => break,
            },
            _ = cancel_token.cancelled() => break,
        };
        if let Some(c) = &chunk.content {
            for p in &c.parts {
                if let Part::Text { text } = p {
                    summary.push_str(text);
                }
            }
        }
        if chunk.turn_complete || chunk.finish_reason.is_some() {
            break;
        }
    }

    if summary.is_empty() {
        format!(
            "{} [Summary generation failed. Key tool results are in recent messages.]",
            crate::prompts::COMPACT_SUMMARY_PREFIX
        )
    } else {
        format!("{}{}", crate::prompts::COMPACT_SUMMARY_PREFIX, summary)
    }
}

/// 判断一条消息是否是压缩摘要（连续压缩时跳过，避免「摘要的摘要」级联失真）。
///
/// 摘要均以 `COMPACT_SUMMARY_PREFIX` 开头（见 `llm_compact` 产出）；此处据此识别。
pub(super) fn is_summary_content(c: &Content) -> bool {
    c.parts.iter().any(|p| match p {
        Part::Text { text } => text.starts_with(crate::prompts::COMPACT_SUMMARY_PREFIX),
        _ => false,
    })
}

/// 构造一条压缩检查点事件（镜像 adk-agent `compaction.rs:104-111` 的 L1 schema）。
///
/// 关键约定：`llm_response.content = None`，摘要只放进 `actions.compaction.compacted_content`。
/// 这让 SSE 消费循环（按 `content` 分发）与 `collect_history_messages` 自然跳过本条，
/// 既不把摘要当 assistant 正文渲染，也不重复持久化。
///
/// 已知限制（adk 回放 boundary 机制）：框架的 `conversation_history_for_agent_impl`
/// 以 `end_timestamp` 为边界，跳过所有 `timestamp <= end_timestamp` 的事件。本条用
/// `now()` 作为 end_timestamp，因此重启后回放时，**本次 run 内保留的 retained_users/
/// tail（其持久化事件 timestamp < now）会被边界跳过**，下次 run 只看到 `[preamble, 摘要]`。
/// 即 retained_users/tail 仅在「本次 run 内连续」有效，不跨重启；重启后从摘要续接
/// （摘要已含进度/决策/待办，可接受）。彻底修复需把 retained_users/tail 作为新事件
/// 重新持久化，但会触发前端重复渲染，风险高于收益，暂不修。
pub(super) fn build_compaction_event(invocation_id: &str, summary_text: String) -> Event {
    let now = chrono::Utc::now();
    let mut event = Event::new(invocation_id);
    event.author = "system".to_string();
    event.actions = EventActions {
        compaction: Some(EventCompaction {
            start_timestamp: now,
            end_timestamp: now,
            compacted_content: Content {
                role: "model".to_string(),
                parts: vec![Part::Text { text: summary_text }],
            },
        }),
        ..Default::default()
    };
    // content 保持 None（见函数文档）
    event
}

/// 判断错误是否是「上下文超窗」（ContextWindowExceeded）。
///
/// 超窗时由 mod.rs 兜底「删最旧一条消息重试」（而非直接失败）。不同 provider 措辞不一，
/// 这里按关键词匹配（覆盖 OpenAI `context_length_exceeded` / Anthropic `prompt_too_long` 等）。
pub(super) fn is_context_window_exceeded(e: &adk_rust::AdkError) -> bool {
    let msg = e.to_string().to_lowercase();
    msg.contains("context_length")
        || msg.contains("context window")
        || msg.contains("maximum context")
        || msg.contains("reduce the length")
        || msg.contains("prompt_too_long")
}
