//! fork_turns 解析与 fork 历史过滤（对齐 codex SpawnAgentArgs::fork_mode / keep_forked_rollout_item）。

use adk_rust::{Content, Part};

/// fork_turns 解析（对齐 codex SpawnAgentArgs::fork_mode，错误消息原文照抄）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ForkMode {
    /// 不 fork：子 agent 干净上下文（仅 message）
    None,
    /// 全量历史 fork（默认）
    FullHistory,
    /// 只 fork 最近 n 个 turn
    LastNTurns(usize),
}

pub(crate) fn parse_fork_turns(raw: Option<&str>) -> std::result::Result<ForkMode, String> {
    let ft = raw
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("all");
    if ft.eq_ignore_ascii_case("none") {
        return Ok(ForkMode::None);
    }
    if ft.eq_ignore_ascii_case("all") {
        return Ok(ForkMode::FullHistory);
    }
    match ft.parse::<usize>() {
        Ok(n) if n > 0 => Ok(ForkMode::LastNTurns(n)),
        _ => Err("fork_turns must be `none`, `all`, or a positive integer string".to_string()),
    }
}

/// 从父 conv（preamble 之后的历史）按 codex 过滤规则生成子 agent 初始历史。
///
/// 对齐 codex keep_forked_rollout_item：
/// - user 消息保留（codex 的 role in {system,developer,user} → cortex 的 system preamble
///   由子 agent 自行重建，这里只处理历史区）；
/// - model 消息只保留**纯文本**（对齐 codex assistant phase==FinalAnswer 保留、
///   Commentary/工具调用丢弃）；
/// - function 消息（工具结果）丢弃；
/// - ForkMode::LastNTurns(n) 先按 user 边界截最近 n 个 turn（对齐
///   truncate_rollout_to_last_n_fork_turns：边界=真实 user 消息；不足 n 个全保留）。
pub(crate) fn fork_history(
    history: &[Content],
    mode: ForkMode,
    // 当前 run 内新增的消息（本轮 conv 中 history 之后的部分）——同样参与 fork，
    // 使 spawn 时父的本轮进展也能继承（对齐 codex fork 整个 rollout）
    current_turn: &[Content],
) -> Vec<Content> {
    if matches!(mode, ForkMode::None) {
        return Vec::new();
    }
    // 按引用拼接待过滤视图（history + 本 run 增量），只对幸存条目 clone——
    // 先全量 clone 再过滤会拷贝大量注定丢弃的内容（工具重的会话保留率 <20%）。
    let all: Vec<&Content> = history.iter().chain(current_turn.iter()).collect();

    // LastNTurns：按 user 边界截断。对齐 codex truncate_rollout_to_last_n_fork_turns——
    // 边界 = 「真实用户消息」∪「NEW_TASK 信封」（对齐 is_real_user_message_boundary
    // 排除 contextual fragment + is_trigger_turn_boundary）：
    // - mailbox 信封（Message Type: 开头）中只有 NEW_TASK（新任务=新 turn）算边界，
    //   MESSAGE/FINAL_ANSWER（随轮附带）不算；
    // - 软着陆提醒/borrow/max-steps 等合成 user 模板不算边界（按固定前缀排除）。
    let all: &[&Content] = if let ForkMode::LastNTurns(n) = mode {
        let boundaries: Vec<usize> = all
            .iter()
            .enumerate()
            .filter(|(_, c)| c.role == "user" && is_real_user_turn_boundary(c))
            .map(|(i, _)| i)
            .collect();
        let keep_idx = boundaries
            .len()
            .checked_sub(n)
            .map(|pos| boundaries[pos])
            .or_else(|| boundaries.first().copied());
        match keep_idx {
            Some(idx) => &all[idx..],
            None => &all[..],
        }
    } else {
        &all[..]
    };

    // 逐条过滤（对齐 codex keep_forked_rollout_item）：
    // - user 保留（codex: role in {system,developer,user}）
    // - model 消息：含 FunctionCall 的整条丢（codex 丢 assistant 工具调用）；
    //   纯答复保留其中 Text parts、丢 Thinking（codex 丢 Reasoning、按 FinalAnswer
    //   phase 保留正文——cortex 无相位标注，以「不含工具调用」判终答）。
    // - function 消息丢弃（codex 丢 FunctionCallOutput）。
    all.iter()
        .filter_map(|c| {
            let mut c = (*c).clone();
            match c.role.as_str() {
                "user" => Some(c),
                "model" => {
                    if c.parts
                        .iter()
                        .any(|p| matches!(p, Part::FunctionCall { .. }))
                    {
                        None
                    } else {
                        c.parts.retain(|p| matches!(p, Part::Text { .. }));
                        if c.parts.is_empty() { None } else { Some(c) }
                    }
                }
                _ => None,
            }
        })
        .collect()
}

/// 判定 user 消息是否为「真实 turn 边界」（fork LastNTurns 用；对齐 codex
/// is_real_user_message_boundary + is_trigger_turn_boundary）。
/// 排除合成注入（它们附属于已有 turn，不是新 turn 的起点）：
/// - mailbox 信封（`Message Type: ` 固定首行）：仅 NEW_TASK 算边界，MESSAGE/FINAL_ANSWER 不算
/// - 软着陆提醒/borrow/max-steps 模板（对齐 codex 排除 contextual fragment）
fn is_real_user_turn_boundary(c: &Content) -> bool {
    let first_text = c.parts.iter().find_map(|p| match p {
        Part::Text { text } => Some(text.as_str()),
        _ => None,
    });
    let Some(text) = first_text else {
        return false;
    };
    let t = text.trim_start();
    // mailbox 信封：看 Message Type 行
    if let Some(rest) = t.strip_prefix("Message Type: ") {
        // NEW_TASK（新任务）= 新 turn 边界；MESSAGE / FINAL_ANSWER 随轮附带，不算
        return rest.starts_with("NEW_TASK");
    }
    // 轮次上限软降级模板（mod.rs MAX_STEPS_PROMPT 固定前缀）
    if t.starts_with("CRITICAL - MAXIMUM STEPS REACHED") {
        return false;
    }
    // 软着陆提醒（soft_landing.rs reminder_message 固定前缀）
    if t.starts_with("Your context window is nearly exhausted") {
        return false;
    }
    // 借最后一轮（soft_landing.rs borrow_message 固定前缀）
    if t.starts_with("You are in the final turn before an automatic context compaction") {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fork_turns_parsing() {
        assert_eq!(parse_fork_turns(None).unwrap(), ForkMode::FullHistory);
        assert_eq!(parse_fork_turns(Some("")).unwrap(), ForkMode::FullHistory);
        assert_eq!(parse_fork_turns(Some("none")).unwrap(), ForkMode::None);
        assert_eq!(parse_fork_turns(Some("NONE")).unwrap(), ForkMode::None);
        assert_eq!(
            parse_fork_turns(Some("all")).unwrap(),
            ForkMode::FullHistory
        );
        assert_eq!(
            parse_fork_turns(Some("3")).unwrap(),
            ForkMode::LastNTurns(3)
        );
        // 0 / 非法 → 错误（消息对齐 codex）
        assert!(parse_fork_turns(Some("0")).is_err());
        let e = parse_fork_turns(Some("abc")).unwrap_err();
        assert_eq!(
            e,
            "fork_turns must be `none`, `all`, or a positive integer string"
        );
    }

    #[test]
    fn fork_history_filters_and_truncates() {
        let mk = |role: &str, text: &str| Content {
            role: role.to_string(),
            parts: vec![Part::Text {
                text: text.to_string(),
            }],
        };
        let fc = |id: &str| Content {
            role: "model".to_string(),
            parts: vec![Part::FunctionCall {
                name: "t".into(),
                args: adk_rust::serde_json::json!({}),
                id: Some(id.into()),
                thought_signature: None,
            }],
        };
        let fr = |id: &str| Content {
            role: "function".to_string(),
            parts: vec![Part::FunctionResponse {
                function_response: adk_rust::FunctionResponseData::new("t", adk_rust::serde_json::json!({})),
                id: Some(id.into()),
                annotations: None,
            }],
        };
        let hist = vec![
            mk("user", "q1"),
            fc("c1"),
            fr("c1"),
            mk("model", "answer1"), // 纯文本 model 保留
            Content {
                // 含 FC 的 model 消息 → 整条丢弃（对齐 codex assistant 工具调用丢弃）
                role: "model".to_string(),
                parts: vec![
                    Part::Text {
                        text: "mixed".to_string(),
                    },
                    Part::FunctionCall {
                        name: "t".into(),
                        args: adk_rust::serde_json::json!({}),
                        id: Some("c2".into()),
                        thought_signature: None,
                    },
                ],
            },
            mk("user", "q2"),
            mk("model", "answer2"),
        ];
        // FullHistory：user + 纯文本 model 保留，FC/FR/混合 model 丢弃
        let out = fork_history(&hist, ForkMode::FullHistory, &[]);
        let texts: Vec<&str> = out
            .iter()
            .flat_map(|c| {
                c.parts.iter().filter_map(|p| match p {
                    Part::Text { text } => Some(text.as_str()),
                    _ => None,
                })
            })
            .collect();
        assert_eq!(texts, vec!["q1", "answer1", "q2", "answer2"]);

        // LastNTurns(1)：只保留最后一个 user 边界起（q2 + answer2）
        let out1 = fork_history(&hist, ForkMode::LastNTurns(1), &[]);
        let texts1: Vec<&str> = out1
            .iter()
            .flat_map(|c| {
                c.parts.iter().filter_map(|p| match p {
                    Part::Text { text } => Some(text.as_str()),
                    _ => None,
                })
            })
            .collect();
        assert_eq!(texts1, vec!["q2", "answer2"]);

        // None：空
        assert!(fork_history(&hist, ForkMode::None, &[]).is_empty());
    }

    #[test]
    fn fork_last_n_turns_keeps_exactly_n_turns() {
        // 回归：5 个 user turn + fork_turns=3 → 必须保留最后 3 个 turn（q3/q4/q5 及其答复），
        // 旧实现的扫描覆盖 bug 会退化成只留最后 1 个 turn。
        let mk = |role: &str, text: &str| Content {
            role: role.to_string(),
            parts: vec![Part::Text {
                text: text.to_string(),
            }],
        };
        let mut hist = Vec::new();
        for i in 1..=5 {
            hist.push(mk("user", &format!("q{i}")));
            hist.push(mk("model", &format!("a{i}")));
        }
        let out = fork_history(&hist, ForkMode::LastNTurns(3), &[]);
        let texts: Vec<&str> = out
            .iter()
            .flat_map(|c| {
                c.parts.iter().filter_map(|p| match p {
                    Part::Text { text } => Some(text.as_str()),
                    _ => None,
                })
            })
            .collect();
        assert_eq!(texts, vec!["q3", "a3", "q4", "a4", "q5", "a5"]);

        // n 超过实际 turn 数 → 全保留（对齐 codex checked_sub → first 兜底）
        let out_all = fork_history(&hist, ForkMode::LastNTurns(10), &[]);
        assert_eq!(out_all.len(), 10);
    }

    #[test]
    fn fork_keeps_text_part_of_thinking_model_messages() {
        // 回归：Thinking+Text 混合的 model 消息（thinking 模型常态）——正文 Text 必须保留、
        // Thinking 丢弃。旧 all(Text) 过滤会整条丢，子 agent 拿不到任何父答复。
        let msg = Content {
            role: "model".to_string(),
            parts: vec![
                Part::Thinking {
                    thinking: "let me think".to_string(),
                    signature: None,
                },
                Part::Text {
                    text: "the answer".to_string(),
                },
            ],
        };
        let out = fork_history(
            &[
                Content {
                    role: "user".to_string(),
                    parts: vec![Part::Text {
                        text: "q".to_string(),
                    }],
                },
                msg,
            ],
            ForkMode::FullHistory,
            &[],
        );
        let texts: Vec<&str> = out
            .iter()
            .flat_map(|c| {
                c.parts.iter().filter_map(|p| match p {
                    Part::Text { text } => Some(text.as_str()),
                    _ => None,
                })
            })
            .collect();
        assert_eq!(
            texts,
            vec!["q", "the answer"],
            "thinking 消息的正文必须保留"
        );
        // Thinking part 被剔除
        assert!(
            out.iter()
                .all(|c| c.parts.iter().all(|p| matches!(p, Part::Text { .. })))
        );
    }
}
