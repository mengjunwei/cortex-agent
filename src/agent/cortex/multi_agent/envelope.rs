//! InterAgent 消息信封（对齐 codex InterAgentMessage / InterAgentCompletionMessage）。

/// 消息类型（渲染进信封首行）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InterAgentMessageType {
    /// 排队不触发 turn（send_message）
    Message,
    /// 新任务触发 turn（followup_task / spawn 初始任务）
    NewTask,
    /// 子 agent 最终答案（对齐 codex FINAL_ANSWER）
    FinalAnswer,
}

impl InterAgentMessageType {
    fn as_str(self) -> &'static str {
        match self {
            Self::Message => "MESSAGE",
            Self::NewTask => "NEW_TASK",
            Self::FinalAnswer => "FINAL_ANSWER",
        }
    }
}

/// 信封格式原文对齐 codex inter_agent_message.rs / inter_agent_completion_message.rs：
/// `Message Type: {TYPE}\nTask name: {recipient}\nSender: {author}\nPayload:\n{payload}`
pub(crate) fn render_inter_agent_message(
    msg_type: InterAgentMessageType,
    recipient: &str,
    sender: &str,
    payload: &str,
) -> String {
    format!(
        "Message Type: {}\nTask name: {}\nSender: {}\nPayload:\n{}",
        msg_type.as_str(),
        recipient,
        sender,
        payload
    )
}

/// 从消息文本判定 InterAgent 信封类型（非信封文本返回 None）。
///
/// 压缩保留策略用（对齐 codex compact_remote_v2::is_retained_for_remote_compaction_v2）：
/// NEW_TASK 原文保留、MESSAGE / FINAL_ANSWER 只进摘要器输入。
pub(crate) fn envelope_type_of(text: &str) -> Option<InterAgentMessageType> {
    let rest = text.strip_prefix("Message Type: ")?;
    match rest.split('\n').next()?.trim() {
        "NEW_TASK" => Some(InterAgentMessageType::NewTask),
        "MESSAGE" => Some(InterAgentMessageType::Message),
        "FINAL_ANSWER" => Some(InterAgentMessageType::FinalAnswer),
        _ => None,
    }
}

/// 错误 payload 截断（中间截断，对齐 codex truncate_middle_with_token_budget，
/// 近似 4 bytes/token；标记 `…{n} tokens truncated…`）。
pub(crate) fn truncate_middle_tokens(text: &str, max_tokens: usize) -> String {
    let max_bytes = max_tokens.saturating_mul(4);
    let total = text.len();
    if total <= max_bytes {
        return text.to_string();
    }
    // 按字符边界切，避免切在 UTF-8 中间
    let marker = format!("…{} tokens truncated…", (total - max_bytes) / 4);
    let half = max_bytes.saturating_sub(marker.len()) / 2;
    let head_end = floor_char_boundary(text, half);
    let tail_start = ceil_char_boundary(text, total.saturating_sub(half));
    format!("{}{}{}", &text[..head_end], marker, &text[tail_start..])
}

fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}
fn ceil_char_boundary(s: &str, mut i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_payload_truncation() {
        let long = "x".repeat(900 * 4 * 3); // ~3 倍预算
        let t = truncate_middle_tokens(&long, 900);
        assert!(t.contains("tokens truncated"));
        assert!(t.len() < long.len());
        // 短文本不截断
        assert_eq!(truncate_middle_tokens("short", 900), "short");
    }

    #[test]
    fn inter_agent_message_envelope() {
        let m = render_inter_agent_message(
            InterAgentMessageType::FinalAnswer,
            "/root",
            "/root/task_1",
            "the answer",
        );
        assert_eq!(
            m,
            "Message Type: FINAL_ANSWER\nTask name: /root\nSender: /root/task_1\nPayload:\nthe answer"
        );
    }

    #[test]
    fn envelope_type_detection() {
        assert_eq!(envelope_type_of("Message Type: NEW_TASK\nTask name: x"), Some(InterAgentMessageType::NewTask));
        assert_eq!(envelope_type_of("Message Type: MESSAGE\nTask name: x"), Some(InterAgentMessageType::Message));
        assert_eq!(envelope_type_of("Message Type: FINAL_ANSWER\nTask name: x"), Some(InterAgentMessageType::FinalAnswer));
        // 非信封文本 / 未知类型 / 前缀不匹配
        assert_eq!(envelope_type_of("just a user message"), None);
        assert_eq!(envelope_type_of("Message Type: UNKNOWN\nx"), None);
        assert_eq!(envelope_type_of("prefix Message Type: NEW_TASK\n"), None);
    }
}
