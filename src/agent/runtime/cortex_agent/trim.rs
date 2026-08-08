//! 历史级裁剪——压缩前反向遍历，把超大的工具输出截短。
//!
//! 对齐 codex `trim_function_call_history_to_fit_context_window`。
//! 与 `tools/truncating.rs` 的单次工具截断互补：那是工具执行后截单条；
//! 本模块是压缩前扫整条历史，兜住「单条没超、十几次工具调用累积超了」的盲区。
//! 裁剪后若已降到软闸以下，可跳过 LLM 摘要（省一次调用）。

use adk_rust::{Content, Part};

/// 裁剪统计。
pub(super) struct TrimStats {
    pub trimmed_outputs: usize,
    pub chars_removed: usize,
}

/// 单条工具输出在历史级裁剪时的上限（字符）。超出则截到该长度。
const MAX_TOOL_OUTPUT_CHARS: usize = 8_192;

/// 反向遍历 conv，只把超大的 `FunctionResponse`（工具输出）做硬截断，遇到 user/model
/// 文本边界停。返回 `(统计, 是否已低于 soft_gate)`。
///
/// - `preamble_len`：开头 preamble 消息数（不裁、不越过）。
/// - 只裁工具输出，不动用户消息/模型文本（保住语义）。
/// - 反向（从最新往最老）——优先牺牲最近的工具输出。
pub(super) fn trim_tool_outputs_to_fit(
    conv: &mut [Content],
    preamble_len: usize,
    soft_gate_tokens: usize,
    chars_per_token: usize,
) -> (TrimStats, bool) {
    let soft_gate_chars = soft_gate_tokens.saturating_mul(chars_per_token);
    let total_chars: usize = conv.iter().map(content_char_len).sum();
    if total_chars <= soft_gate_chars {
        return (TrimStats { trimmed_outputs: 0, chars_removed: 0 }, true);
    }

    let mut trimmed = 0usize;
    let mut removed = 0usize;
    // 反向索引遍历（避免 iter_mut 与复检的不可变借用冲突），跳过 preamble
    let mut idx = conv.len();
    while idx > preamble_len {
        idx -= 1;
        // 复检是否已降到预算内（不可变借用，语句结束即释放）
        let now_total: usize = conv.iter().map(content_char_len).sum();
        if now_total <= soft_gate_chars {
            return (TrimStats { trimmed_outputs: trimmed, chars_removed: removed }, true);
        }
        // 取这一条的可变引用做截断
        let c = &mut conv[idx];
        // 遇到非工具输出（user/model 文本）停——保住语义边界，不越过用户消息删更早的
        if !is_tool_output(c) {
            break;
        }
        let before = content_char_len(c);
        if before > MAX_TOOL_OUTPUT_CHARS {
            truncate_function_response(c, MAX_TOOL_OUTPUT_CHARS);
            let after = content_char_len(c);
            removed = removed.saturating_add(before.saturating_sub(after));
            trimmed += 1;
        }
    }
    (TrimStats { trimmed_outputs: trimmed, chars_removed: removed }, false)
}

/// 估算一条 Content 的字符量（与 mod.rs 的 token 估算同口径，按字节）。
fn content_char_len(c: &Content) -> usize {
    c.parts
        .iter()
        .map(|p| match p {
            Part::Text { text } => text.len(),
            Part::Thinking { thinking, .. } => thinking.len(),
            Part::FunctionResponse { function_response, .. } => {
                function_response.response.to_string().len()
            }
            _ => 64,
        })
        .sum()
}

fn is_tool_output(c: &Content) -> bool {
    c.role == "function"
        || c.role == "tool"
        || c.parts
            .iter()
            .any(|p| matches!(p, Part::FunctionResponse { .. }))
}

/// 把 FunctionResponse 的 response 截短到 max_chars（UTF-8 字符边界安全）。
fn truncate_function_response(c: &mut Content, max_chars: usize) {
    for p in c.parts.iter_mut() {
        if let Part::FunctionResponse { function_response, .. } = p {
            let s = function_response.response.to_string();
            if s.len() > max_chars {
                let cut = safe_truncate(&s, max_chars);
                let truncated = format!(
                    "{cut}\n\n[... 工具输出过长，已为上下文压缩裁断（原文约 {} 字节）...]",
                    s.len()
                );
                function_response.response = adk_rust::serde_json::Value::String(truncated);
            }
        }
    }
}

/// UTF-8 字符边界安全的字节截断（stable `is_char_boundary`）。
fn safe_truncate(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes.min(s.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use adk_rust::{Content, FunctionResponseData, Part};
    use serde_json::json;

    fn tool_output(role: &str, resp: serde_json::Value) -> Content {
        Content {
            role: role.to_string(),
            parts: vec![Part::FunctionResponse {
                function_response: FunctionResponseData::new("t", resp),
                id: None,
                annotations: None,
            }],
        }
    }

    #[test]
    fn reverses_and_trims_only_tool_outputs() {
        let big = "x".repeat(20_000);
        let mut conv = vec![
            Content { role: "system".into(), parts: vec![Part::Text { text: "preamble".into() }] },
            Content { role: "user".into(), parts: vec![Part::Text { text: "hi".into() }] },
            tool_output("function", json!({ "out": big })), // 唯一工具输出（最新）
        ];
        // soft_gate=16000 字符：截断后（~8192+marker）+preamble+user 可容纳
        let (stats, under) = trim_tool_outputs_to_fit(&mut conv, 1, 4_000, 4);
        assert!(stats.trimmed_outputs >= 1, "应裁剪工具输出");
        assert!(under, "截断后应低于预算");
    }

    #[test]
    fn stops_at_user_boundary() {
        let big = "x".repeat(20_000);
        let mut conv = vec![
            Content { role: "system".into(), parts: vec![Part::Text { text: "p".into() }] },
            tool_output("function", json!({ "out": big })), // 旧：user 之后，反向遇不到
            Content { role: "user".into(), parts: vec![Part::Text { text: "ask".into() }] },
            tool_output("function", json!({ "out": big })), // 最新
        ];
        let (stats, _) = trim_tool_outputs_to_fit(&mut conv, 1, 1, 4);
        // 反向从最新 tool 开始，裁它；上一条是 user → 停。只裁 1 条。
        assert!(stats.trimmed_outputs <= 1, "遇到 user 应停，不越过");
    }

    #[test]
    fn safe_truncate_respects_char_boundary() {
        let s = "你好世界".repeat(100); // 中文 3 字节/字
        let t = safe_truncate(&s, 100);
        assert!(t.len() <= 100);
        assert!(String::from_utf8(t.into_bytes()).is_ok());
    }
}
