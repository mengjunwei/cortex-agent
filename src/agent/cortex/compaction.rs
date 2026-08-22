//! 上下文压缩（auto-compaction）—— 超阈值时用 LLM 把旧消息摘要成交接摘要。
//!
//! 对齐 codex `run_auto_compact` / `build_compacted_history`：旧 user 消息按预算原样保留，
//! 旧非 user 消息（model/function/thinking）摘要成一条，保护进行中的 tool 流不被截断。

use std::collections::HashMap;
use std::sync::Arc;

use adk_rust::{
    Content, Event, EventActions, EventCompaction, GenerateContentConfig, Llm, LlmRequest, Part,
};
use futures::StreamExt;
use tokio_util::sync::CancellationToken;

use super::multi_agent::{envelope_type_of, InterAgentMessageType};

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

/// 压缩保留规划：对 older 区间逐条判定 user 消息是否原文保留，返回与 `older`
/// 等长的保留标记（未标记的 user 条目由调用方并入摘要器输入，历史不静默丢失）。
///
/// 信封保留语义（对齐 codex compact_remote_v2::is_retained_for_remote_compaction_v2
/// + 4f6d06d485「Preserve delegated tasks across remote compaction」）：
/// - MESSAGE / FINAL_ANSWER 信封：不保留（过程性/已完结内容只进摘要；最终答案
///   已由子 agent 事件流呈现给用户）；
/// - NEW_TASK 信封：保留（任务指派是后续 turn 的决策依据）；单条超
///   `envelope_cap_chars`（10k token 换算，对齐 MAX_RETAINED_AGENT_MESSAGE_TOKENS）
///   只摘要；
/// - 普通 user 消息：预算内保留；
/// - 预算从最新往最旧消耗，预算外条目**跳过继续扫**（skip 而非 stop）：旧实现的
///   break 在首条超预算处终止扫描，更早的可保留条目既不保留也无摘要，静默丢失。
///
/// 非 user / 摘要条目恒 false（非 user 由调用方按 role 直送摘要器）。
pub(super) fn plan_user_retention(
    older: &[Content],
    user_budget_chars: usize,
    envelope_cap_chars: usize,
) -> Vec<bool> {
    let msg_chars = |c: &Content| -> usize {
        c.parts
            .iter()
            .map(|p| match p {
                Part::Text { text } => text.len(),
                _ => 0,
            })
            .sum()
    };
    let envelope_kind = |c: &Content| {
        c.parts.iter().find_map(|p| match p {
            Part::Text { text } if !text.is_empty() => envelope_type_of(text),
            _ => None,
        })
    };
    let mut retained_chars = 0usize;
    let mut retain = vec![false; older.len()];
    for (i, c) in older.iter().enumerate().rev() {
        if c.role != "user" || is_summary_content(c) {
            continue;
        }
        let len = msg_chars(c);
        let keep = match envelope_kind(c) {
            // 过程性/完结信封：只摘要，不占保留预算
            Some(InterAgentMessageType::Message | InterAgentMessageType::FinalAnswer) => false,
            // 超大任务书：只摘要（对齐 codex 单条上限）
            Some(InterAgentMessageType::NewTask) if len > envelope_cap_chars => false,
            // 普通消息/合格信封：预算内保留，预算外跳过（不终止扫描）
            _ => retained_chars + len <= user_budget_chars,
        };
        if keep {
            retained_chars += len;
            retain[i] = true;
        }
    }
    retain
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

#[cfg(test)]
mod retention_tests {
    use super::*;

    fn user(text: &str) -> Content {
        Content {
            role: "user".to_string(),
            parts: vec![Part::Text {
                text: text.to_string(),
            }],
        }
    }

    fn model(text: &str) -> Content {
        Content {
            role: "model".to_string(),
            parts: vec![Part::Text {
                text: text.to_string(),
            }],
        }
    }

    fn envelope(ty: &str, payload: &str) -> Content {
        user(&format!(
            "Message Type: {ty}\nTask name: /root/x\nSender: /root\nPayload:\n{payload}"
        ))
    }

    const BUDGET: usize = 200;
    const CAP: usize = 100;

    #[test]
    fn new_task_envelopes_retained_within_cap() {
        let older = vec![
            envelope("NEW_TASK", "do the analysis"), // 合格信封 → 保留
            model("working on it"),                  // 非 user → false（走摘要）
        ];
        let retain = plan_user_retention(&older, BUDGET, CAP);
        assert_eq!(retain, vec![true, false]);
    }

    #[test]
    fn message_and_final_answer_envelopes_dropped() {
        let older = vec![
            envelope("MESSAGE", "progress note"),      // 过程性 → 只摘要
            envelope("FINAL_ANSWER", "the result"),    // 完结 → 只摘要
            envelope("NEW_TASK", "still active task"), // 任务书 → 保留
        ];
        let retain = plan_user_retention(&older, BUDGET, CAP);
        assert_eq!(retain, vec![false, false, true]);
    }

    #[test]
    fn oversized_new_task_dropped() {
        // 超过单条信封上限（CAP=100）的任务书 → 只摘要
        let big = "x".repeat(CAP + 1);
        let older = vec![envelope("NEW_TASK", &big), envelope("NEW_TASK", "small")];
        let retain = plan_user_retention(&older, BUDGET, CAP);
        assert_eq!(retain, vec![false, true]);
    }

    #[test]
    fn budget_skip_continues_scan_instead_of_stopping() {
        // 旧实现（break）：[small_early, big, small_late] 从新往旧扫，big 超预算即终止
        // → small_early 丢失（不保留也无摘要）。修复后跳过 big 继续保留 small_early。
        let budget = 100; // late(11) 装得下、big(150) 装不下、early(11) 又装得下
        let big = "y".repeat(150); // 普通user消息（无信封前缀，不走 CAP 路径）
        let older = vec![
            user("early small"), // 最旧：仍应被保留
            user(&big),          // 超预算：跳过
            user("late small"),  // 最新：先消耗预算
        ];
        let retain = plan_user_retention(&older, budget, CAP);
        assert_eq!(retain, vec![true, false, true], "跳过超预算条目后必须继续扫描更早条目");
    }

    #[test]
    fn plain_users_and_summary_content() {
        let older = vec![
            user("plain question"),          // 普通 → 保留
            user(&format!("{}stale", crate::prompts::COMPACT_SUMMARY_PREFIX)), // 摘要 → 不保留
        ];
        let retain = plan_user_retention(&older, BUDGET, CAP);
        assert_eq!(retain, vec![true, false]);
    }
}
