//! 会话预处理与超长压缩 — 清洗分页提示、LLM 摘要压缩、尾部截断降级。
//!
//! 这些方法被 FAQ 学习流程(见 [`super::faq`])在提取候选前调用：
//! 先清洗「请输入继续」等分页噪声，超长会话再压缩摘要，保证送入 LLM 的上下文可控。

use super::{KnowledgeManager, MAX_CONVERSATION_CHARS};
use adk_rust::{Content, Llm, LlmRequest};
use futures::StreamExt;
use std::sync::Arc;

impl KnowledgeManager {
    /// 会话预处理：清洗系统提示文本（如"输出过长，请输入『继续』"）、去除多余空行
    ///
    /// 这些提示是分页/截断时由前端或模型插入的，与命令知识无关，若不清洗会污染 FAQ 内容。
    pub(super) fn prepare_conversation(raw: &str) -> String {
        let mut cleaned: Vec<String> = Vec::new();
        for line in raw.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            // 清洗"输出过长，请输入「继续」/继续 以获取更多内容"这类分页提示
            if trimmed.contains("请输入")
                && (trimmed.contains("继续") || trimmed.contains("获取更多"))
            {
                continue;
            }
            if trimmed.contains("输出过长") {
                continue;
            }
            cleaned.push(line.to_string());
        }
        cleaned.join("\n")
    }

    /// 超长会话压缩：字符数超过 `MAX_CONVERSATION_CHARS` 时调用 LLM 做命令相关摘要
    ///
    /// 目的：防止上下文过长导致模型总结报错。压缩保留所有命令、参数、配置示例等技术细节，
    /// 仅去除寒暄、重复与无关内容。压缩失败时降级为尾部截断，确保流程不中断。
    pub(super) async fn compress_if_too_long(
        &self,
        conversation: &str,
        model: &Arc<dyn Llm>,
        model_name: &str,
    ) -> String {
        if conversation.chars().count() <= MAX_CONVERSATION_CHARS {
            return conversation.to_string();
        }

        tracing::info!(
            "[compress] 会话过长({} 字符)，启动 LLM 压缩摘要",
            conversation.chars().count()
        );

        let prompt = format!(
            r#"请把下面的网络设备运维对话压缩成一份**命令知识摘要**，要求：
1. 完整保留所有命令、参数、配置示例、回退命令等技术细节，不得丢失
2. 去除寒暄、重复、与命令无关的闲聊
3. 按命令主题分组整理，输出纯文本
4. 总长度控制在 {} 字以内

对话内容：
{}"#,
            MAX_CONVERSATION_CHARS, conversation
        );

        let req = LlmRequest::new(model_name, vec![Content::new("user").with_text(&prompt)])
            .with_config(adk_rust::GenerateContentConfig {
                max_output_tokens: Some(4096),
                temperature: Some(0.2),
                ..Default::default()
            });

        match model.generate_content(req, false).await {
            Ok(mut stream) => match stream.next().await {
                Some(Ok(resp)) => {
                    let text = resp
                        .content
                        .as_ref()
                        .map(|c| {
                            c.parts
                                .iter()
                                .filter_map(|p| p.text().map(|s| s.to_string()))
                                .collect::<Vec<_>>()
                                .join("")
                        })
                        .unwrap_or_default();
                    if text.trim().is_empty() {
                        tracing::warn!("[compress] LLM 返回空，降级为尾部截断");
                        Self::tail_truncate(conversation)
                    } else {
                        tracing::info!("[compress] 压缩完成，{} 字符", text.chars().count());
                        text
                    }
                }
                _ => {
                    tracing::warn!("[compress] LLM 无响应，降级为尾部截断");
                    Self::tail_truncate(conversation)
                }
            },
            Err(e) => {
                tracing::warn!("[compress] LLM 压缩失败({}), 降级为尾部截断", e);
                Self::tail_truncate(conversation)
            }
        }
    }

    /// 降级策略：保留头部 + 尾部，截断中段，尽量保留首尾的设备识别与最终结论
    fn tail_truncate(conversation: &str) -> String {
        let chars: Vec<char> = conversation.chars().collect();
        let keep = MAX_CONVERSATION_CHARS;
        if chars.len() <= keep {
            return conversation.to_string();
        }
        let head = keep * 2 / 3;
        let tail = keep - head;
        let head_s: String = chars[..head].iter().collect();
        let tail_s: String = chars[chars.len() - tail..].iter().collect();
        format!("{}\n\n……（中段已截断）……\n\n{}", head_s, tail_s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepare_conversation_strips_continue_hints() {
        let raw = "\
用户: 帮我配置静态路由
输出过长，请输入「继续」以获取更多内容。
助手: 使用 ip route-static 命令

请输入 继续 获取更多内容
用户: 谢谢";
        let out = KnowledgeManager::prepare_conversation(raw);

        // 分页/截断提示被清洗
        assert!(!out.contains("输出过长"));
        assert!(!out.contains("请输入"));
        assert!(!out.contains("继续"));
        // 正常命令内容保留
        assert!(out.contains("ip route-static"));
        assert!(out.contains("帮我配置静态路由"));
        // 多余空行被去除
        assert!(!out.contains("\n\n"));
    }

    #[test]
    fn prepare_conversation_keeps_normal_text() {
        let raw = "用户: 配置VLAN\n助手: 使用 vlan 10";
        let out = KnowledgeManager::prepare_conversation(raw);
        assert_eq!(out, raw);
    }

    #[test]
    fn tail_truncate_respects_limit() {
        let s: String = "中".repeat(MAX_CONVERSATION_CHARS + 500);
        let out = KnowledgeManager::tail_truncate(&s);
        assert!(out.chars().count() <= MAX_CONVERSATION_CHARS + 20);
        assert!(out.contains("中段已截断"));
    }

    #[test]
    fn tail_truncate_short_unchanged() {
        let s = "短文本";
        assert_eq!(KnowledgeManager::tail_truncate(s), s);
    }
}
