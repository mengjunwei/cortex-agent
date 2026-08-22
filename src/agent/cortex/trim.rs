//! 历史级裁剪——压缩前反向遍历，把超大的工具输出截短。
//!
//! 对齐 codex `trim_function_call_history_to_fit_context_window`。
//! 与 `tools/truncating.rs` 的单次工具截断互补：那是工具执行后截单条；
//! 本模块是压缩前扫整条历史，兜住「单条没超、十几次工具调用累积超了」的盲区。
//! 对齐 codex 语义：trim 只服务于压缩本身（compact_remote_request.rs 在压缩
//! 请求内部 trim 让其塞进窗口），**不存在「裁完够了就跳过压缩」的逃生门**——
//! 逃生门曾用字符估算口径判「够了」，与真实 usage 触发口径分叉，在中文/工具
//! 密集会话（估算长期低估真实占用）静默吞掉整个压缩分支且无任何日志。

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
        return (
            TrimStats {
                trimmed_outputs: 0,
                chars_removed: 0,
            },
            true,
        );
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
            return (
                TrimStats {
                    trimmed_outputs: trimmed,
                    chars_removed: removed,
                },
                true,
            );
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
    (
        TrimStats {
            trimmed_outputs: trimmed,
            chars_removed: removed,
        },
        false,
    )
}

/// 估算一条 Content 的字符量（与 mod.rs 的 token 估算同口径，按字节）。
fn content_char_len(c: &Content) -> usize {
    c.parts
        .iter()
        .map(|p| match p {
            Part::Text { text } => text.len(),
            Part::Thinking { thinking, .. } => thinking.len(),
            Part::FunctionResponse {
                function_response, ..
            } => function_response.response.to_string().len(),
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
///
/// 头尾保留、中间省略（middle-cut）：工具日志/命令输出的尾部常含最终结果与退出码，
/// 仅保头会丢掉最关键的信息。预算太小放不下头尾时回退保头。
///
/// 截断点由 middle_safe_truncate 内部的 `…` 标注；尾注（原文大小）长度**计入预算**，
/// 确保截断后整体严格 ≤ max_chars——否则裁剪契约被破坏、且与 `…` 形成双重截断标记。
fn truncate_function_response(c: &mut Content, max_chars: usize) {
    for p in c.parts.iter_mut() {
        if let Part::FunctionResponse {
            function_response, ..
        } = p
        {
            let s = function_response.response.to_string();
            if s.len() > max_chars {
                let note = format!("\n\n[上下文压缩裁断：保留首尾，原文约 {} 字节]", s.len());
                let budget = max_chars.saturating_sub(note.len());
                let cut = middle_safe_truncate(&s, budget);
                function_response.response =
                    adk_rust::serde_json::Value::String(format!("{cut}{note}"));
            }
        }
    }
}

/// UTF-8 字符边界安全的字节截断（stable `is_char_boundary`）。保头。
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

/// 把字节索引回退到最近的字符边界（不切断多字节字符）。
fn floor_char_boundary(s: &str, mut idx: usize) -> usize {
    if idx >= s.len() {
        return s.len();
    }
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

/// 头尾保留、中间省略的字符边界安全截断。预算太小（头尾会重叠）时回退 [`safe_truncate`]。
///
/// 省略号 `…`（3 字节）的长度**计入预算**，保证输出严格 ≤ max_bytes。
fn middle_safe_truncate(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    const ELLIPSIS: &str = "\u{2026}"; // 3 字节
    let body_budget = max_bytes.saturating_sub(ELLIPSIS.len());
    let head_budget = body_budget / 2;
    let tail_budget = body_budget - head_budget;
    let head_end = floor_char_boundary(s, head_budget);
    let tail_start = floor_char_boundary(s, s.len().saturating_sub(tail_budget));
    if head_end >= tail_start {
        // 预算太小，头尾会重叠 → 回退保头
        return safe_truncate(s, max_bytes);
    }
    let mut out = String::with_capacity(head_end + ELLIPSIS.len() + (s.len() - tail_start));
    out.push_str(&s[..head_end]);
    out.push_str(ELLIPSIS);
    out.push_str(&s[tail_start..]);
    out
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
            Content {
                role: "system".into(),
                parts: vec![Part::Text {
                    text: "preamble".into(),
                }],
            },
            Content {
                role: "user".into(),
                parts: vec![Part::Text { text: "hi".into() }],
            },
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
            Content {
                role: "system".into(),
                parts: vec![Part::Text { text: "p".into() }],
            },
            tool_output("function", json!({ "out": big })), // 旧：user 之后，反向遇不到
            Content {
                role: "user".into(),
                parts: vec![Part::Text { text: "ask".into() }],
            },
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

    #[test]
    fn truncate_function_response_respects_budget_and_marks() {
        // 截断后原始字符串（含尾注）必须严格 ≤ max_chars，且保留首尾 + 截断标记 + 原文大小线索
        let big = "x".repeat(20_000);
        let mut c = tool_output("function", json!({ "out": big }));
        truncate_function_response(&mut c, 8_192);
        let raw = match &c.parts[0] {
            Part::FunctionResponse {
                function_response, ..
            } => match &function_response.response {
                adk_rust::serde_json::Value::String(s) => s.clone(),
                _ => function_response.response.to_string(),
            },
            _ => unreachable!(),
        };
        assert!(
            raw.len() <= 8_192,
            "截断后原始字符串应不超过 max_chars，实际 {}",
            raw.len()
        );
        // middle-cut 内部省略号 + 尾注原文大小
        assert!(raw.contains('…'), "应有 middle-cut 截断标记: {raw}");
        assert!(raw.contains("原文约"), "尾注应含原文字节数: {raw}");
        // 应是合法 UTF-8
        assert!(String::from_utf8(raw.into_bytes()).is_ok());
    }
}
