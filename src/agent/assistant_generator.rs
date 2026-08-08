//! 助手草稿生成器：让 LLM 依据用户模糊需求，自动产出专业的自定义助手四字段
//!
//! 输出字段：
//! - `name`：助手名字（简洁准确，8-16 字）
//! - `description`：一句话简介（20-60 字，说明助手擅长做什么）
//! - `system_prompt`：系统提示词（专业、有清晰角色和执行规范，300-1500 字）
//! - `greeting`：开场白（欢迎用户并说明使用方式，30-100 字）
//!
//! 设计要点：
//! - 使用 JSON schema 强约束 LLM 输出结构，避免解析歧义
//! - 消费 `generate_content(_, false)` 返回的完整流拼 full_text
//! - 兼容 LLM 偶尔加代码块包裹的输出（复用 extract_json_value 思路）

use adk_rust::{Content, GenerateContentConfig, Llm, prelude::LlmRequest};
use anyhow::{Result, anyhow};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// 助手草稿：与前端 form 字段一一对应
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantDraft {
    pub name: String,
    pub description: String,
    pub system_prompt: String,
    pub greeting: String,
}

/// 生成助手草稿。
///
/// - `prompt`：用户对助手用途的模糊描述（可短可长）
/// - `model`：直接注入 LLM 实例（由调用方通过 `make_model_by_id` 解析）
pub async fn generate(model: Arc<dyn Llm>, prompt: &str) -> Result<AssistantDraft> {
    let user_input = prompt.trim();
    if user_input.is_empty() {
        return Err(anyhow!("需求描述不能为空"));
    }
    if user_input.len() > 2000 {
        return Err(anyhow!("需求描述过长（最多 2000 字），请精简"));
    }

    let schema = serde_json::json!({
        "type": "object",
        "properties": {
            "name":          { "type": "string", "description": "助手名字，8-16 个字，能一眼看出用途" },
            "description":   { "type": "string", "description": "一句话简介，20-60 字，说明助手擅长做什么" },
            "system_prompt": { "type": "string", "description": "系统提示词，300-1500 字，包含角色、能力边界、执行规范、输出格式" },
            "greeting":      { "type": "string", "description": "开场白，30-100 字，友好地欢迎并说明如何使用" }
        },
        "required": ["name", "description", "system_prompt", "greeting"]
    });

    let req_llm = LlmRequest::new(
        "",
        vec![
            Content::new("system").with_text(build_system_prompt()),
            Content::new("user").with_text(build_user_prompt(user_input)),
        ],
    )
    .with_response_schema(schema)
    .with_config(GenerateContentConfig {
        max_output_tokens: Some(4096),
        temperature: Some(0.5),
        ..Default::default()
    });

    let mut stream = model.generate_content(req_llm, false).await?;
    let mut full_text = String::new();
    while let Some(result) = stream.next().await {
        let resp = result?;
        if let Some(c) = resp.content {
            for part in c.parts {
                if let Some(text) = part.text() {
                    full_text.push_str(text);
                }
            }
        }
    }

    if full_text.trim().is_empty() {
        return Err(anyhow!("LLM 返回内容为空"));
    }

    let value = extract_json_value(&full_text)
        .ok_or_else(|| anyhow!("无法从 LLM 输出中提取 JSON: {full_text}"))?;

    let draft: AssistantDraft =
        serde_json::from_value(value).map_err(|e| anyhow!("LLM 输出结构不符合预期: {e}"))?;

    validate_draft(&draft)?;

    Ok(draft)
}

fn build_system_prompt() -> String {
    r#"你是一位资深的 AI 助手设计师，擅长把用户模糊的需求转成专业、可直接投入使用的自定义 AI 助手配置。

## 你的输出要求

请严格按照以下 JSON 结构返回（不要输出任何 JSON 之外的解释性文字）：

- name（8-16 字）：助手名字，中文优先，能一眼看出这个助手是做什么的。避免使用"XX 专家"这种老套模板，追求具体、生动。
- description（20-60 字）：一句话简介，说明助手擅长哪类任务、面向什么场景。
- system_prompt（300-1500 字）：系统提示词，是助手的"灵魂"。必须包含：
  1. **角色定位**：这个助手是谁、有什么专业背景
  2. **核心能力**：能解决哪些具体问题
  3. **执行规范**：回答问题时应该遵循的方法论 / 步骤
  4. **输出格式**：期望的回复形式（是否用 markdown、要不要举例、篇幅长短）
  5. **边界约束**：不擅长什么、遇到超纲问题如何应对
- greeting（30-100 字）：开场白，用友好的第一人称语气欢迎用户，简要说明能帮什么忙、建议如何提问。

## 撰写风格

- 用词专业、精准，避免空话套话（例如"我会尽全力帮你"这种没有信息量的话）
- system_prompt 使用第二人称"你"来指导助手，语气坚定
- greeting 使用第一人称"我"，语气亲切自然
- 中文场景优先输出中文；如果用户明确指定英文才用英文

## 示例（仅参考风格，不要照抄）

用户需求：帮我做一个能写正则表达式的助手
name: "正则表达式工程师"
description: "把自然语言需求翻译成精准的正则表达式，覆盖主流方言并附解释与测试用例。"
"#.to_string()
}

fn build_user_prompt(user_input: &str) -> String {
    format!(
        "请根据以下用户需求，设计一个专业的 AI 助手：\n\n---\n{user_input}\n---\n\n只返回符合 schema 的 JSON，不要额外解释。"
    )
}

fn validate_draft(d: &AssistantDraft) -> Result<()> {
    if d.name.trim().is_empty() {
        return Err(anyhow!("生成的 name 为空"));
    }
    if d.name.chars().count() > 32 {
        return Err(anyhow!("生成的 name 过长（{} 字）", d.name.chars().count()));
    }
    if d.system_prompt.trim().is_empty() {
        return Err(anyhow!("生成的 system_prompt 为空"));
    }
    if d.system_prompt.chars().count() > 8000 {
        return Err(anyhow!(
            "生成的 system_prompt 过长（{} 字），超出 8000 上限",
            d.system_prompt.chars().count()
        ));
    }
    Ok(())
}

/// 从 LLM 原始输出中提取 JSON 值（兼容代码块包裹、前后有解释文字等情况）
fn extract_json_value(text: &str) -> Option<serde_json::Value> {
    let trimmed = text.trim();
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
        return Some(v);
    }

    // 剥离 ```json ... ``` 或 ``` ... ``` 代码块
    let stripped = if let Some(start) = trimmed.find("```json") {
        let after = &trimmed[start + 7..];
        after
            .find("```")
            .map(|e| after[..e].trim())
            .unwrap_or(trimmed)
    } else if let Some(start) = trimmed.find("```") {
        let after = &trimmed[start + 3..];
        after
            .find("```")
            .map(|e| after[..e].trim())
            .unwrap_or(trimmed)
    } else {
        trimmed
    };
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(stripped) {
        return Some(v);
    }

    // 兜底：截取第一个 { 到最后一个 }
    let start = stripped.find('{')?;
    let end = stripped.rfind('}')?;
    if end < start {
        return None;
    }
    serde_json::from_str::<serde_json::Value>(&stripped[start..=end]).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_direct_json() {
        let raw = r#"{"name":"a","description":"b","system_prompt":"c","greeting":"d"}"#;
        let v = extract_json_value(raw).unwrap();
        assert_eq!(v["name"], "a");
    }

    #[test]
    fn extract_from_code_fence() {
        let raw = "```json\n{\"name\":\"x\",\"description\":\"y\",\"system_prompt\":\"z\",\"greeting\":\"w\"}\n```";
        let v = extract_json_value(raw).unwrap();
        assert_eq!(v["name"], "x");
    }

    #[test]
    fn extract_with_preamble() {
        let raw = "以下是生成结果：\n{\"name\":\"a\",\"description\":\"b\",\"system_prompt\":\"c\",\"greeting\":\"d\"}\n希望有帮助！";
        let v = extract_json_value(raw).unwrap();
        assert_eq!(v["name"], "a");
    }

    #[test]
    fn validate_rejects_empty_name() {
        let d = AssistantDraft {
            name: "".into(),
            description: "x".into(),
            system_prompt: "y".into(),
            greeting: "z".into(),
        };
        assert!(validate_draft(&d).is_err());
    }

    #[test]
    fn validate_rejects_oversize_system_prompt() {
        let d = AssistantDraft {
            name: "a".into(),
            description: "b".into(),
            system_prompt: "x".repeat(8001),
            greeting: "z".into(),
        };
        assert!(validate_draft(&d).is_err());
    }

    #[test]
    fn validate_accepts_normal_draft() {
        let d = AssistantDraft {
            name: "正则工程师".into(),
            description: "帮你写正则表达式".into(),
            system_prompt: "你是一位正则表达式专家...".into(),
            greeting: "你好，我可以帮你写各种正则。".into(),
        };
        assert!(validate_draft(&d).is_ok());
    }
}
